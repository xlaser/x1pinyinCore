//! 最小种子构建工具：从仓库自带文本资产自举一个结构完整的词库 DB。
//!
//! 内置单字表（pinyin_dict.txt）+ 英文词表 + 中译英词表 → 新建 SQLite：
//! schema 由 `Storage::open` 幂等建齐，随后写入词条、`x1dict_sync_hash`
//! 与 `schema_version` 标记。产出可直接被 `Engine::new_with_db` 打开。
//!
//! 覆盖面以单字为主（完整种子约 12 万词条，本工具只产出内置资产量级），
//! 供无完整种子环境下的开发 / 调试 / CI 自举；日常输入体验请用完整种子
//! （见 README「词库说明」）。
//!
//! 用法：
//! ```sh
//! cargo run -p x1core --bin seed_init -- <output.db> [--force] [--normalize]
//! ```
//! `--normalize` 收敛 WAL（主文件自包含，适合作为分发形态）；
//! `--dict/--english/--zh-en` 可指定替代资产（默认本 crate `assets/` 下）。

use std::path::{Path, PathBuf};
use x1core::engine::Engine;
use x1core::storage::{EnglishWord, Storage, X1DictWord};

fn asset(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join(name)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!(
            "用法: seed_init <output.db> [--force] [--normalize] \
             [--dict <pinyin_dict.txt>] [--english <english_words.tsv>] [--zh-en <zh_en_words.tsv>]"
        );
        std::process::exit(if args.len() < 2 { 1 } else { 0 });
    }
    let out = PathBuf::from(&args[1]);
    let force = args.iter().any(|a| a == "--force");
    let normalize = args.iter().any(|a| a == "--normalize");
    let opt = |flag: &str, default: &str| -> PathBuf {
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if a == flag {
                if let Some(v) = it.next() {
                    return PathBuf::from(v);
                }
            }
        }
        asset(default)
    };
    let dict_path = opt("--dict", "pinyin_dict.txt");
    let en_path = opt("--english", "english_words.tsv");
    let zhen_path = opt("--zh-en", "zh_en_words.tsv");

    if out.exists() && !force {
        eprintln!("❌ 输出已存在：{}（--force 覆盖）", out.display());
        std::process::exit(1);
    }
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(out.with_extension("db-wal"));
    let _ = std::fs::remove_file(out.with_extension("db-shm"));

    // 1. 建库：Storage::open 幂等建全套表（含 english_words / zh_en_words / db_meta）
    let mut storage = Storage::open(&out).unwrap_or_else(|e| {
        eprintln!("❌ 建库失败: {e}");
        std::process::exit(1);
    });

    // 2. 中文词条：pinyin(TAB)text(TAB)freq[(TAB)is_primary]；不足 2 列的行跳过（含 # 头）
    let text = std::fs::read_to_string(&dict_path).unwrap_or_else(|e| {
        eprintln!("❌ 读 {}: {e}", dict_path.display());
        std::process::exit(1);
    });
    let mut words = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 2 {
            continue;
        }
        let freq = parts
            .get(2)
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(100);
        let is_primary = parts
            .get(3)
            .and_then(|s| s.parse::<i64>().ok())
            .map(|v| v != 0)
            .unwrap_or(true);
        words.push(X1DictWord {
            simplified: parts[1].to_string(),
            pinyin: parts[0].to_string(),
            freq,
            is_primary,
        });
    }
    let n = storage.import_x1dict_batch(&words).unwrap_or_else(|e| {
        eprintln!("❌ 词条导入失败: {e}");
        std::process::exit(1);
    });
    eprintln!("✅ x1dict_words: {n} 条（源 {}）", dict_path.display());

    // 3. 英文词表：word(TAB)pos(TAB)cn(TAB)freq；# 头跳过
    let en_text = std::fs::read_to_string(&en_path).unwrap_or_else(|e| {
        eprintln!("❌ 读 {}: {e}", en_path.display());
        std::process::exit(1);
    });
    let mut en_n = 0usize;
    for line in en_text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let p: Vec<&str> = line.split('\t').collect();
        if p.len() < 4 {
            continue;
        }
        let freq = p[3].parse::<i64>().unwrap_or(50);
        let ok = storage.english_upsert(&EnglishWord {
            word: p[0].to_string(),
            pos: p[1].to_string(),
            cn: p[2].to_string(),
            freq,
        });
        if ok {
            en_n += 1;
        }
    }
    eprintln!("✅ english_words: {en_n} 条");

    // 4. zh_en 词表：zh(TAB)en(TAB)pos(TAB)freq（无专用批量 API，短连接直写；
    //    表已由 Storage::open 建好）
    let zhen_text = std::fs::read_to_string(&zhen_path).unwrap_or_else(|e| {
        eprintln!("❌ 读 {}: {e}", zhen_path.display());
        std::process::exit(1);
    });
    let mut zhen_rows: Vec<(String, String, String, i64)> = Vec::new();
    for line in zhen_text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let p: Vec<&str> = line.split('\t').collect();
        if p.len() < 4 {
            continue;
        }
        let freq = p[3].parse::<i64>().unwrap_or(50);
        zhen_rows.push((p[0].to_string(), p[1].to_string(), p[2].to_string(), freq));
    }
    let zhen_n = zhen_rows.len();
    drop(storage);
    {
        let conn = rusqlite::Connection::open(&out).expect("open for zh_en");
        let tx = conn.unchecked_transaction().expect("tx");
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO zh_en_words(zh, en, pos, freq) \
                     VALUES (?1, ?2, ?3, ?4)",
                )
                .expect("prepare zh_en");
            for (zh, en, pos, freq) in &zhen_rows {
                stmt.execute(rusqlite::params![zh, en, pos, freq])
                    .expect("insert zh_en");
            }
        }
        // 结构版本标记（部署期引导链用；见 storage::schema）
        tx.execute(
            "INSERT OR REPLACE INTO db_meta(key, value) VALUES('schema_version','1')",
            [],
        )
        .expect("stamp schema_version");
        tx.commit().expect("commit zh_en");
    }
    eprintln!("✅ zh_en_words: {zhen_n} 条");

    // 5. 内容哈希 + 元信息（须在全部写入后计算；hash 是用户 DB 侧增量同步的比对键）
    let mut storage = Storage::open(&out).expect("reopen for hash");
    let hash = storage.x1dict_content_hash().unwrap_or_default();
    let _ = storage.set_seed_meta("x1dict_sync_hash", &hash);
    eprintln!("✅ x1dict_sync_hash: {}", hash.get(..12).unwrap_or(&hash));
    drop(storage);

    // 6. 可选：收敛 WAL（发货/入库形态：主文件自包含，无 -wal/-shm 边车）
    if normalize {
        let conn = rusqlite::Connection::open(&out).expect("open normalize");
        let _: String = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
            .unwrap_or_default();
        let _: String = conn
            .query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))
            .unwrap_or_default();
        eprintln!("✅ journal=DELETE（主文件自包含）");
    }

    // 7. 自验证：引擎能开、基础查询有候选
    let engine = Engine::new_with_db(out.to_str().unwrap())
        .unwrap_or_else(|e| {
            eprintln!("❌ 自验证失败（引擎打不开产物）: {e}");
            std::process::exit(1);
        });
    for input in ["ni", "hao", "zhong"] {
        let cands = engine.process(input);
        if cands.is_empty() {
            eprintln!("❌ 自验证失败：{input} 无候选");
            std::process::exit(1);
        }
        let top: Vec<&str> = cands.iter().take(3).map(|c| c.text.as_ref()).collect();
        eprintln!("自验证 {input} → {:?}", top);
    }
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    eprintln!("\n完成：{}（{:.1} MB）", out.display(), size as f64 / 1e6);
}
