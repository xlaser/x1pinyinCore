//! 双库分离端到端探针。
//!
//! 用**真实种子 + 真实用户库副本**跑完整链路：
//!   种子 → dict.db 铺开 → 旧单库迁移 → open_split 加载 → 查询出词
//!   → 学习写入 → 验证落在 user.db → dict.db 换新种子 → 用户数据完好
//!
//! 用法：cargo run -p x1core --release --example probe_split_e2e -- <work_dir> [legacy_db]
//! 全程只动 work_dir 内的副本，不触碰真实 %APPDATA%。

use std::path::{Path, PathBuf};
use x1core::storage::split::ensure_dict_db;
use x1core::storage::Storage;

fn count(db: &Path, table: &str) -> i64 {
    let c = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap_or(-1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let work = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| {
        x1core::test_tmp::dir()
            .join("split_e2e")
            .to_string_lossy()
            .into_owned()
    }));
    let seed = work.join("seed.db");
    let legacy = PathBuf::from(
        args.get(2)
            .cloned()
            .unwrap_or_else(|| work.join("x1pinyin.db").to_string_lossy().into_owned()),
    );
    let dict_db = work.join("dict.db");
    let user_db = work.join("user.db");
    for p in [&dict_db, &user_db] {
        let _ = std::fs::remove_file(p);
    }

    println!("== 工作目录 {} ==", work.display());
    println!("种子 {} | 旧库 {}", seed.display(), legacy.display());
    println!(
        "旧库 x1dict={} user_words={} user_bigram={} blacklist={}",
        count(&legacy, "x1dict_words"),
        count(&legacy, "user_words"),
        count(&legacy, "user_bigram_counts"),
        count(&legacy, "user_blacklist")
    );

    // ① 系统词库铺开
    let t0 = std::time::Instant::now();
    let replaced = ensure_dict_db(&dict_db, &seed).expect("ensure_dict_db");
    println!(
        "① dict.db 铺开: replaced={} 耗时={}ms size={}KB",
        replaced,
        t0.elapsed().as_millis(),
        std::fs::metadata(&dict_db)
            .map(|m| m.len() / 1024)
            .unwrap_or(0)
    );

    // ② 旧单库迁移
    let t1 = std::time::Instant::now();
    let rep = Storage::migrate_split_user_db_if_needed(&user_db, &legacy)
        .expect("migrate")
        .expect("应执行迁移");
    let rows: i64 = rep.tables.iter().map(|(_, n)| *n).sum();
    println!(
        "② 用户库迁移: {} 表 / {} 行 / {} meta，耗时={}ms，旧库→{}",
        rep.tables.len(),
        rows,
        rep.meta_keys,
        t1.elapsed().as_millis(),
        Path::new(&rep.legacy_backup)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    );
    for (t, n) in &rep.tables {
        println!("     {t:24} {n:>8}");
    }

    // ③ 分离模式加载 + 查询
    let t2 = std::time::Instant::now();
    let engine =
        x1core::engine::Engine::open_split(dict_db.to_str().unwrap(), user_db.to_str().unwrap())
            .expect("open_split");
    println!(
        "③ 引擎加载（分离模式）: 耗时={}ms",
        t2.elapsed().as_millis()
    );
    for q in ["wohaoxiangchifan", "guangzh", "nihao", "shurufa"] {
        let cands = engine.process(q);
        let top: Vec<String> = cands.iter().take(3).map(|c| c.text.to_string()).collect();
        println!("     {q} → {}", top.join(" | "));
    }

    // ④ 学习写入 → 必须落 user.db（dict.db 不变）
    let dict_before = std::fs::metadata(&dict_db).map(|m| m.len()).unwrap_or(0);
    let mut engine = engine;
    engine.commit("ceshi", "测试词", Some("ce shi ci"), false);
    engine.flush().expect("flush");
    drop(engine);
    let words_in_user = count(&user_db, "user_words");
    let words_in_dict = {
        let c = rusqlite::Connection::open(&dict_db).unwrap();
        let has: bool = c
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='user_words')",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if has {
            count(&dict_db, "user_words")
        } else {
            -1 /* 无该表 = 正确 */
        }
    };
    println!("④ 学习写入: user.db user_words={words_in_user}（含新词应为原值+1），dict.db user_words={words_in_dict}（-1=无表，正确）");
    println!(
        "     dict.db 大小变化: {dict_before} → {}（应不变或仅元数据）",
        std::fs::metadata(&dict_db).map(|m| m.len()).unwrap_or(0)
    );

    // ⑤ dict.db 换新种子 → 用户数据必须完好
    let mut s = Storage::open(&seed).unwrap();
    s.import_x1dict_batch(&[x1core::storage::X1DictWord {
        simplified: "端到端新词".into(),
        pinyin: "duan dao duan xin ci".into(),
        freq: 600,
        is_primary: true,
    }])
    .unwrap();
    s.set_seed_meta("x1dict_sync_hash", "e2e0000000000001")
        .unwrap();
    drop(s);
    let swapped = ensure_dict_db(&dict_db, &seed).expect("ensure 2");
    let engine2 =
        x1core::engine::Engine::open_split(dict_db.to_str().unwrap(), user_db.to_str().unwrap())
            .expect("open_split 2");
    let visible = engine2
        .process("duandaoduanxinci")
        .iter()
        .take(5)
        .any(|c| c.text.as_ref() == "端到端新词");
    println!(
        "⑤ dict.db 覆盖: swapped={swapped}，新词可见={visible}，user_words 仍={}，bigram 仍={}",
        count(&user_db, "user_words"),
        count(&user_db, "user_bigram_counts")
    );
    println!(
        "     dict.db 表: x1dict={} seed_bigram={}",
        count(&dict_db, "x1dict_words"),
        count(&dict_db, "seed_bigram_counts")
    );

    println!("\n✅ 端到端链路完成");
}
