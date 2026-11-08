//! freq 重算工具（一次性 DB 维护）
//!
//! 用途：种子 DB 的 freq 失真（多字词频极低，如「我们」=2、「中文」=1，
//! 而 compute_freq 应给 9500/5800）。本工具用 importer 的 compute_freq
//! 权威逻辑对全部 x1dict_words 重算 freq，并清理拼音为空的脏词条。
//!
//! 安全性：
//! - 只动 x1dict_words.freq，绝不碰 user_words 等用户表
//! - compute_freq 是纯函数，仅依赖编译期常量表（char_freq/word_freq），无网络
//! - 幂等：重复执行结果一致
//!
//! 用法：
//! ```sh
//! cargo run -p x1core --bin freq_rebuild <db_path>
//! ```
//! 或先在副本上验证：
//! ```sh
//! cp x1core/assets/x1pinyin.db /tmp/test.db
//! cargo run -p x1core --bin freq_rebuild /tmp/test.db
//! ```

use std::env;
use x1core::char_freq::compute_freq;
use x1core::storage::Storage;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("用法: freq_rebuild <db_path> [--flatten-only | --skip-flatten | --hash-only | --normalize-journal]");
        eprintln!("  db_path       要重算的 x1pinyin.db 路径（建议先在副本上验证）");
        eprintln!("  --flatten-only  跳过 compute_freq 重算，只压扁到 4 档（保留原始 freq 分级）");
        eprintln!("  --skip-flatten  重算 compute_freq 但不压扁，freq 保持真实值（50-9999，默认）");
        eprintln!(
            "  --hash-only     只更新 x1dict_sync_hash，不动 freq/用户表（外部数据变更场景用："
        );
        eprintln!("                  外部词库 freq 是映射值，compute_freq 无其字频数据会打成默认值，不能全量重算）");
        eprintln!("  --normalize-journal  交付形态归一：WAL→DELETE + 清 -wal/-shm + 校验文件头");
        eprintln!("                  只读安装目录（Program Files/签名 bundle）里 WAL 库开库即失败（边车建不出）");
        std::process::exit(1);
    }
    // db_path = 首个非旗标参数（`--normalize-journal <path>` 与
    // `<path> --normalize-journal` 两种次序都认）。缺失路径时报用法退出
    //（否则旗标串被当文件名在 cwd 建出垃圾库且退出码 0，门禁会误判成功）。
    let db_path = match args.iter().skip(1).find(|a| !a.starts_with("--")) {
        Some(p) => p,
        None => {
            eprintln!("错误：缺少 db_path 参数（旗标不能单独使用，且无默认路径）");
            std::process::exit(1);
        }
    };
    let flatten_only = args.iter().any(|a| a == "--flatten-only");
    let skip_flatten = args.iter().any(|a| a == "--skip-flatten");
    let hash_only = args.iter().any(|a| a == "--hash-only");
    let normalize_journal = args.iter().any(|a| a == "--normalize-journal");

    // --normalize-journal：种子交付形态归一——① wal_checkpoint(TRUNCATE) 把
    // WAL 已提交内容折进主文件（发货只带主文件，漏折 = 发货缺内容）
    // ② journal_mode=DELETE（只读目录读 DELETE 库零副作用；WAL 库开库要建
    // -wal/-shm 边车，建不出 = 开库失败 → 铺开被跳过 → x1dict 空 → 内置小词库）
    // ③ 清边车 ④ 校验文件头 write version==1，非 1 退出码 1（打包门禁用）。
    // 必须在 Storage::open 之前跑（Storage::open 会把库翻回 WAL——本工具
    // 其他模式跑完种子都会变 WAL，收尾链须补跑本模式）。
    if normalize_journal {
        use std::io::{Read, Seek, SeekFrom};
        match rusqlite::Connection::open(db_path) {
            Ok(conn) => {
                if let Err(e) = conn
                    .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
                {
                    eprintln!("❌ --normalize-journal: 归一 SQL 失败: {}", e);
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("❌ --normalize-journal: 打开失败: {}", e);
                std::process::exit(1);
            }
        }
        let _ = std::fs::remove_file(format!("{db_path}-wal"));
        let _ = std::fs::remove_file(format!("{db_path}-shm"));
        let mut b = [0u8; 1];
        let check = std::fs::File::open(db_path).and_then(|mut f| {
            f.seek(SeekFrom::Start(18))?;
            f.read_exact(&mut b)
        });
        match check {
            Ok(()) if b[0] == 1 => {
                eprintln!(
                    "✅ --normalize-journal：{} 已归一 journal_mode=DELETE，边车已清",
                    db_path
                );
            }
            _ => {
                eprintln!(
                    "❌ --normalize-journal：{} 仍是 WAL 形态（header[18]={}）——禁止交付",
                    db_path, b[0]
                );
                std::process::exit(1);
            }
        }
        return;
    }

    eprintln!("打开数据库: {}", db_path);
    let mut storage = match Storage::open(db_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("❌ 打开数据库失败: {}", e);
            std::process::exit(1);
        }
    };

    // --hash-only 模式：只更新 x1dict_sync_hash 后退出。
    // 外部数据变更场景：freq 是映射值不能重算，但 hash 必须更新
    // 以触发用户 DB 的 sync_x1dict_from_builtin。
    if hash_only {
        let hash = storage.x1dict_content_hash().unwrap_or_default();
        let _ = storage.set_seed_meta("x1dict_sync_hash", &hash);
        eprintln!(
            "✅ --hash-only：更新 x1dict_sync_hash: {}（freq/用户表未动）",
            hash
        );
        return;
    }

    // 清理 + 重算（--flatten-only 模式跳过，保留原始 freq 分级）
    let (dirty, updated) = if flatten_only {
        eprintln!("⏭️  --flatten-only 模式：跳过清理和 compute_freq 重算（保留原始 freq 分级）");
        (0, 0)
    } else {
        // 清理种子 DB 的 user_words 表（回归干净基线）。
        // user_words 只应存运行时增量，残留的个人高频词会污染测试基线与首装体验。
        let user_cleaned = match storage.clear_user_words() {
            Ok(n) => n,
            Err(e) => {
                eprintln!("❌ 清理 user_words 失败: {}", e);
                std::process::exit(1);
            }
        };
        eprintln!("✅ 清理 user_words 残留: {} 条", user_cleaned);

        // 清理运行时统计三表（input_mappings / pinyin_counts / bigram）。
        // 这三表是运行时用户统计量，不应出现在种子 DB；残留会拖慢候选排序
        //（candidate_score 的 prefix mapping 扫描遍历整个 input_mappings）。
        let (im, pc, bg) = match storage.clear_runtime_stat_tables() {
            Ok(n) => n,
            Err(e) => {
                eprintln!("❌ 清理运行时统计表失败: {}", e);
                std::process::exit(1);
            }
        };
        eprintln!(
            "✅ 清理运行时统计残留: input_mappings {} 条, pinyin_counts {} 条, bigram {} 条",
            im, pc, bg
        );

        // 用 compute_freq 重算全部 freq + 清理拼音空脏词条。
        match storage.rebuild_x1dict_freq(compute_freq) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("❌ 重算失败: {}", e);
                std::process::exit(1);
            }
        }
    };
    if !flatten_only {
        eprintln!("✅ 清理拼音为空脏词条: {} 条", dirty);
        eprintln!("✅ freq 重算完成: {} 条", updated);
    }

    // **freq 压扁（已取消为默认）**：
    // 压扁把 freq 收敛到 4 档让 base 分量不碾压 user 学习信号，但压扁机制的
    // 幂等缺陷会把已压扁 DB 再压一次（全变 100）。现默认 --skip-flatten，
    // freq 保持 compute_freq 真实值（50-9999，200 倍差距）；default_pick
    // freq_ratio 守卫阈值同步放宽。保留 --flatten-only 供实验/回退使用。
    let flattened = if skip_flatten {
        eprintln!("⏭️  --skip-flatten 模式：保留 compute_freq 真实值（50-9999），不压扁");
        0
    } else {
        match storage.flatten_freq_to(&[
            (9000, 500), // 超高频档：的/是/不/我/你/吃 等 40 个最常用字
            (3000, 300), // 高频档：常用字、高频多字词
            (1000, 200), // 中频档：一般多字词、中频字
            (0, 100),    // 低频档：兜底字（10388个）、冷僻词
        ]) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("❌ freq 压扁失败: {}", e);
                std::process::exit(1);
            }
        }
    };
    if !skip_flatten {
        eprintln!("✅ freq 压扁到 4 档（100/200/300/500）: {} 条", flattened);
    }

    // 统计重算后的分布，便于人工核对。
    let stats = storage.x1dict_freq_stats().unwrap_or_default();
    eprintln!("\n重算后分布：");
    eprintln!(
        "  {:<8} {:>8} {:>10} {:>8} {:>8}",
        "字数", "词条数", "平均freq", "最小", "最大"
    );
    for (wlen, n, avg, min_f, max_f) in &stats {
        eprintln!(
            "  {:<8} {:>8} {:>10.1} {:>8} {:>8}",
            wlen, n, avg, min_f, max_f
        );
    }

    // 重写 x1dict_sync_hash（freq 全表变了，哈希必须更新，
    // 否则启动时引擎会判定需要全量 sync，拖慢秒开）。
    let hash = storage.x1dict_content_hash().unwrap_or_default();
    let _ = storage.set_seed_meta("x1dict_sync_hash", &hash);
    eprintln!("✅ 更新 x1dict_sync_hash: {}", hash);
    // **不再写 freq_flattened_v1 标记**：
    // 压扁机制（--skip-flatten 默认 + ensure_freq_flattened no-op）已废弃，
    // 这个标记不再有任何代码消费。继续写入只会混淆未来读者。
    // 若未来重启压扁机制，再恢复此写入。
    eprintln!("\n完成。建议运行 cargo test -p x1core 验证测试转绿。");
}
