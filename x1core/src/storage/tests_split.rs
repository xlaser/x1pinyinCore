//! 双库分离专项测试。
//!
//! 覆盖三件必须成立的事：
//! 1. **读写归属**：种子表只进 dict.db、用户表只进 user.db（分离后单文件只含自己角色的表）
//! 2. **迁移零丢失**：旧单库（混装）→ 用户库逐表行数相等，旧库改名保留
//! 3. **更新安全**：dict.db 被种子覆盖后，用户库的自学习数据/自造词/黑名单完好

use crate::storage::{Storage, X1DictWord};
use std::fs;
use std::path::Path;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = crate::test_tmp::dir().join(format!("x1_split_{}_{}", tag, std::process::id()));
    fs::remove_dir_all(&d).ok();
    fs::create_dir_all(&d).unwrap();
    d
}

fn table_exists(db: &Path, table: &str) -> bool {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        rusqlite::params![table],
        |r| r.get(0),
    )
    .unwrap()
}

fn count(db: &Path, table: &str) -> i64 {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// ① 分离模式：两库各自只含自己角色的表 + 读写落到正确文件。
#[test]
fn test_split_connection_roles() {
    let dir = temp_dir("roles");
    let dict_db = dir.join("dict.db");
    let user_db = dir.join("user.db");

    let mut s = Storage::open_split(&dict_db, &user_db).unwrap();
    assert!(s.is_split());

    // 种子侧写入（词条 + 种子 bigram）
    s.import_x1dict_batch(&[X1DictWord {
        simplified: "广州".into(),
        pinyin: "guang zhou".into(),
        freq: 2950,
        is_primary: true,
    }])
    .unwrap();
    s.ensure_seed_bigram_table().unwrap();
    s.insert_seed_bigram_for_test("我好想", "吃饭", 3).unwrap();

    // 用户侧写入（自造词 + 黑名单 + 用户 bigram + 用户 meta）
    s.upsert_user_words_batch(&[("测试词".to_string(), "ce shi ci".to_string(), 5)])
        .unwrap();
    s.insert_blacklist("删掉的词").unwrap();
    s.upsert_bigram_counts(&[("我".to_string(), "喜欢".to_string(), 7)])
        .unwrap();
    s.set_user_meta("context_snapshot", "尾巴").unwrap();
    s.set_seed_meta("x1dict_sync_hash", "0123456789abcdef")
        .unwrap();
    drop(s);

    // dict.db：有 x1dict_words + seed_bigram，无 user 表
    assert!(table_exists(&dict_db, "x1dict_words"));
    assert!(table_exists(&dict_db, "seed_bigram_counts"));
    assert!(
        !table_exists(&dict_db, "user_words"),
        "dict.db 不应有 user_words"
    );
    assert!(
        !table_exists(&dict_db, "user_bigram_counts"),
        "dict.db 不应有用户 bigram"
    );
    assert_eq!(count(&dict_db, "x1dict_words"), 1);
    assert_eq!(count(&dict_db, "seed_bigram_counts"), 1);

    // user.db：有 user 表，无系统词表
    assert!(table_exists(&user_db, "user_words"));
    assert!(table_exists(&user_db, "user_blacklist"));
    assert!(
        !table_exists(&user_db, "x1dict_words"),
        "user.db 不应有 x1dict_words"
    );
    assert!(
        !table_exists(&user_db, "seed_bigram_counts"),
        "user.db 不应有种子表"
    );
    assert_eq!(count(&user_db, "user_words"), 1);
    assert_eq!(count(&user_db, "user_bigram_counts"), 1);

    // meta 分流：用户键在 user.db、种子键在 dict.db
    let uc = rusqlite::Connection::open(&user_db).unwrap();
    let dc = rusqlite::Connection::open(&dict_db).unwrap();
    let user_has: i64 = uc
        .query_row(
            "SELECT COUNT(*) FROM db_meta WHERE key='context_snapshot'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let seed_has: i64 = dc
        .query_row(
            "SELECT COUNT(*) FROM db_meta WHERE key='x1dict_sync_hash'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(user_has, 1, "用户侧 meta 应在 user.db");
    assert_eq!(seed_has, 1, "种子侧 meta 应在 dict.db");
    let cross: i64 = uc
        .query_row(
            "SELECT COUNT(*) FROM db_meta WHERE key='x1dict_sync_hash'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cross, 0, "种子侧 meta 不应出现在 user.db");

    fs::remove_dir_all(&dir).ok();
}

/// ② 旧单库 → 用户库迁移：用户表逐表零丢失 + 旧库改名保留 + 幂等。
#[test]
fn test_split_migration_zero_loss() {
    let dir = temp_dir("migrate");
    let legacy = dir.join("x1pinyin.db");
    let user_db = dir.join("user.db");

    // 造一个混装旧库（含系统词 + 各类用户数据）
    {
        let mut old = Storage::open(&legacy).unwrap();
        old.import_x1dict_batch(&[X1DictWord {
            simplified: "广州".into(),
            pinyin: "guang zhou".into(),
            freq: 2950,
            is_primary: true,
        }])
        .unwrap();
        old.upsert_user_words_batch(&[
            ("自造词甲".to_string(), "zi zao ci jia".to_string(), 4),
            ("自造词乙".to_string(), "zi zao ci yi".to_string(), 2),
        ])
        .unwrap();
        old.insert_blacklist("拉黑词").unwrap();
        old.upsert_bigram_counts(&[("我".to_string(), "喜欢".to_string(), 9)])
            .unwrap();
        old.replace_input_mappings(&[("nihao".to_string(), "你好".to_string(), 3)])
            .unwrap();
        old.set_user_meta("context_snapshot", "旧快照").unwrap();
        old.set_seed_meta("x1dict_sync_hash", "aaaaaaaaaaaaaaaa")
            .unwrap();
    }

    let rep = Storage::migrate_split_user_db_if_needed(&user_db, &legacy)
        .unwrap()
        .expect("应执行迁移");

    // 逐表行数相等
    let expect = [
        ("user_words", 2i64),
        ("user_blacklist", 1),
        ("user_bigram_counts", 1),
        ("user_input_mappings", 1),
    ];
    for (table, n) in expect {
        assert_eq!(count(&user_db, table), n, "{table} 迁移行数不符");
    }
    assert!(rep
        .tables
        .iter()
        .any(|(t, n)| *t == "user_words" && *n == 2));

    // 用户侧 meta 带走、种子侧 meta 不带
    let uc = rusqlite::Connection::open(&user_db).unwrap();
    let snap: String = uc
        .query_row(
            "SELECT value FROM db_meta WHERE key='context_snapshot'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(snap, "旧快照");
    let seed_key: i64 = uc
        .query_row(
            "SELECT COUNT(*) FROM db_meta WHERE key='x1dict_sync_hash'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(seed_key, 0, "种子侧 meta 应留在 dict.db 侧，不随用户库迁移");

    // 旧库改名保留（不删）+ 迁移标记写入
    assert!(!legacy.exists(), "旧库应已改名");
    assert!(
        Path::new(&rep.legacy_backup).exists(),
        "旧库备份应存在: {}",
        rep.legacy_backup
    );
    let marked: i64 = uc
        .query_row(
            "SELECT COUNT(*) FROM db_meta WHERE key='split_migrated'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(marked, 1, "应写入迁移标记");
    drop(uc);

    // 幂等：再次调用 → None（不覆盖用户库）
    let again = Storage::migrate_split_user_db_if_needed(&user_db, &legacy).unwrap();
    assert!(again.is_none(), "重复迁移应短路");

    fs::remove_dir_all(&dir).ok();
}

/// ③ 更新安全：dict.db 被新种子整体覆盖后，用户库数据完好且新词生效。
#[test]
fn test_split_dict_update_keeps_user_data() {
    let dir = temp_dir("update");
    let dict_db = dir.join("dict.db");
    let user_db = dir.join("user.db");
    let seed = dir.join("seed.db");

    // 旧种子 → 铺 dict.db
    {
        let mut s = Storage::open(&seed).unwrap();
        s.import_x1dict_batch(&[X1DictWord {
            simplified: "广州".into(),
            pinyin: "guang zhou".into(),
            freq: 2950,
            is_primary: true,
        }])
        .unwrap();
        s.set_seed_meta("x1dict_sync_hash", "1111111111111111")
            .unwrap();
    }
    assert!(
        crate::storage::split::ensure_dict_db(&dict_db, &seed).unwrap(),
        "首铺应执行"
    );
    assert!(
        !crate::storage::split::ensure_dict_db(&dict_db, &seed).unwrap(),
        "同版本应跳过"
    );

    // 用户库：写入学习数据
    {
        let mut u = Storage::open(&user_db).unwrap();
        u.upsert_user_words_batch(&[("我的词".to_string(), "wo de ci".to_string(), 6)])
            .unwrap();
        u.insert_blacklist("拉黑词").unwrap();
        u.upsert_bigram_counts(&[("我".to_string(), "喜欢".to_string(), 11)])
            .unwrap();
    }

    // 新种子（加一个词 + 换哈希）→ 覆盖 dict.db
    {
        let mut s = Storage::open(&seed).unwrap();
        s.import_x1dict_batch(&[
            X1DictWord {
                simplified: "广州".into(),
                pinyin: "guang zhou".into(),
                freq: 2950,
                is_primary: true,
            },
            X1DictWord {
                simplified: "新词条".into(),
                pinyin: "xin ci tiao".into(),
                freq: 800,
                is_primary: true,
            },
        ])
        .unwrap();
        s.set_seed_meta("x1dict_sync_hash", "2222222222222222")
            .unwrap();
    }
    assert!(
        crate::storage::split::ensure_dict_db(&dict_db, &seed).unwrap(),
        "哈希变化应触发覆盖"
    );

    // 系统词库更新生效（新词在）
    assert_eq!(
        count(&dict_db, "x1dict_words"),
        2,
        "覆盖后应含新种子全部词条"
    );

    // 用户库一个字节不动
    let uc = rusqlite::Connection::open(&user_db).unwrap();
    let words: i64 = uc
        .query_row("SELECT COUNT(*) FROM user_words", [], |r| r.get(0))
        .unwrap();
    let bl: i64 = uc
        .query_row("SELECT COUNT(*) FROM user_blacklist", [], |r| r.get(0))
        .unwrap();
    let bg: i64 = uc
        .query_row(
            "SELECT count FROM user_bigram_counts WHERE prev='我' AND text='喜欢'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(words, 1, "自造词应完好");
    assert_eq!(bl, 1, "黑名单应完好");
    assert_eq!(bg, 11, "用户 bigram 应完好");
    drop(uc);

    // 引擎可用：分离模式加载后两库数据都在（系统词 + 用户词）
    let engine =
        crate::engine::Engine::open_split(dict_db.to_str().unwrap(), user_db.to_str().unwrap())
            .unwrap();
    assert!(
        engine.dict.contains_text("新词条"),
        "分离模式应加载到系统词库新词"
    );

    fs::remove_dir_all(&dir).ok();
}

/// ④ dict.db 保持可写 + 本地修正保护：
/// 文本学习/管理器对系统词库的修正写 dict.db，种子更新覆盖前必须先备份。
#[test]
fn test_split_dict_stays_writable_and_corrections_protected() {
    let dir = temp_dir("correct");
    let dict_db = dir.join("dict.db");
    let _user_db = dir.join("user.db");
    let seed = dir.join("seed.db");

    // 铺 dict.db（旧种子）
    {
        let mut s = Storage::open(&seed).unwrap();
        s.import_x1dict_batch(&[X1DictWord {
            simplified: "广州".into(),
            pinyin: "guang zhou".into(),
            freq: 2950,
            is_primary: true,
        }])
        .unwrap();
        s.set_seed_meta("x1dict_sync_hash", "aaaa000000000001")
            .unwrap();
    }
    crate::storage::split::ensure_dict_db(&dict_db, &seed).unwrap();

    // ① dict.db 可写：模拟本地修正（改 freq + 加词）+ 打修正标记
    {
        let mut d = Storage::open(&dict_db).unwrap();
        d.update_x1dict_word(
            "广州",
            "guang zhou",
            &X1DictWord {
                simplified: "广州".into(),
                pinyin: "guang zhou".into(),
                freq: 4200, // 学到的修正频次
                is_primary: true,
            },
        )
        .unwrap();
        d.import_x1dict_batch(&[X1DictWord {
            simplified: "修正新词".into(),
            pinyin: "xiu zheng xin ci".into(),
            freq: 700,
            is_primary: true,
        }])
        .unwrap();
    }
    crate::storage::split::mark_dict_corrected(&dict_db).unwrap();
    {
        let c = rusqlite::Connection::open(&dict_db).unwrap();
        let f: i64 = c
            .query_row(
                "SELECT freq FROM x1dict_words WHERE simplified='广州'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(f, 4200, "dict.db 应可写（本地修正生效在库内）");
    }

    // ② 新种子到达 → 覆盖前必须留下修正备份
    {
        let mut s = Storage::open(&seed).unwrap();
        s.set_seed_meta("x1dict_sync_hash", "bbbb000000000002")
            .unwrap();
    }
    assert!(
        crate::storage::split::ensure_dict_db(&dict_db, &seed).unwrap(),
        "哈希变化应触发覆盖"
    );
    let baks: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("corrected-"))
        .collect();
    assert_eq!(
        baks.len(),
        1,
        "带修正的 dict.db 覆盖前应留下备份，实际: {baks:?}"
    );

    // 备份内容保留修正（freq=4200 与修正新词）
    let bak = dir.join(&baks[0]);
    let bc = rusqlite::Connection::open(&bak).unwrap();
    let f: i64 = bc
        .query_row(
            "SELECT freq FROM x1dict_words WHERE simplified='广州'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let n: i64 = bc
        .query_row(
            "SELECT COUNT(*) FROM x1dict_words WHERE simplified='修正新词'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(f, 4200, "备份应保留修正后的频次");
    assert_eq!(n, 1, "备份应保留修正新增的词");
    drop(bc);

    // ③ 无修正标记的 DB 覆盖时不产生多余备份
    let dict2 = dir.join("dict2.db");
    crate::storage::split::ensure_dict_db(&dict2, &seed).unwrap();
    {
        let mut s = Storage::open(&seed).unwrap();
        s.set_seed_meta("x1dict_sync_hash", "cccc000000000003")
            .unwrap();
    }
    crate::storage::split::ensure_dict_db(&dict2, &seed).unwrap();
    let extra: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("corrected-"))
        .collect();
    assert_eq!(extra.len(), 1, "无修正标记不应新增备份");

    fs::remove_dir_all(&dir).ok();
}

/// dict.db 替换必须清理旧 WAL/SHM（回归测试）。
///
/// 风险链：ensure_dict_db 用 atomic_copy 覆盖 dict.db 后，**上一代 dict.db 的
/// -wal/-shm 残留**——陈旧 WAL 的页属于旧文件，SQLite 打开新库时应用旧页 →
/// "database disk image is malformed" → x1_engine_new_split 返回 NULL →
/// 调用方静默回退**合并模式**（x1pinyin.db），分离布局永不恢复。
#[test]
fn replace_dict_db_clears_stale_wal() {
    let dir = temp_dir("stale_wal");
    let seed = dir.join("seed.db");
    fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db"),
        &seed,
    )
    .unwrap();
    let dict_db = dir.join("dict.db");
    fs::copy(&seed, &dict_db).unwrap();

    // ① 让旧 dict.db 与种子**真正换代**（否则陈旧 WAL 恰好与同内容新库匹配，
    //    测不出问题）：正常 WAL 模式写入 +
    //    关闭（关闭即 checkpoint 回写主文件、WAL 清空）→ 主文件内容已不同于种子
    {
        use rusqlite::Connection;
        let conn = Connection::open(&dict_db).unwrap();
        conn.execute_batch(
            "INSERT OR REPLACE INTO db_meta(key, value) VALUES('stale_gen_marker', '1');
             INSERT OR REPLACE INTO db_meta(key, value) VALUES('local_corrections', '1');",
        )
        .unwrap();
    }
    assert!(
        !dict_db.with_extension("db-wal").exists(),
        "关闭后 WAL 应已清空"
    );

    // ② 制造"陈旧 WAL 残留"：手工落无效头的 wal/shm 边车（真实场景 = 崩溃进程
    //    留下、句柄已随进程消亡）。**不能**用泄漏连接模拟——Windows 上被 forget
    //    的连接持有文件句柄，替换时删 dict.db 会"拒绝访问"（平台差异）。
    //    SQLite 对无效头 WAL 按"空 WAL"处理，checkpoint_db_wal / open 的容错
    //    路径不受影响。
    fs::write(
        dict_db.with_extension("db-wal"),
        b"stale-wal-not-a-real-wal",
    )
    .unwrap();
    fs::write(dict_db.with_extension("db-shm"), b"stale-shm").unwrap();
    assert!(
        dict_db.with_extension("db-wal").exists(),
        "前置条件：WAL 已生成"
    );

    // ③ 让 seed 哈希与 dict.db 不同 → 触发替换路径
    {
        use rusqlite::Connection;
        let conn = Connection::open(&seed).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO db_meta(key, value) VALUES('x1dict_sync_hash', 'deadbeefdeadbeef')",
            [],
        ).unwrap();
    }

    let replaced = crate::storage::split::ensure_dict_db(&dict_db, &seed).expect("ensure_dict_db");
    assert!(replaced, "哈希不同 → 应执行替换");

    // 核心断言：替换后不得残留旧 WAL/SHM
    assert!(
        !dict_db.with_extension("db-wal").exists(),
        "替换后必须无 dict.db-wal 残留"
    );
    assert!(
        !dict_db.with_extension("db-shm").exists(),
        "替换后必须无 dict.db-shm 残留"
    );

    // 更本质的断言：陈旧 WAL 的**内容不得被折叠进新库**。
    // strip_user_tables 的 `PRAGMA wal_checkpoint(TRUNCATE)` 会把残留 WAL 的页
    // 回写进刚复制的新文件——旧代页混入新库 = 页头/页数错位，真实数据差异大时
    // open_split 直接 "database disk image is malformed"。新库应是种子的纯净
    // 副本：旧代标记（WAL 里的 local_corrections / 主文件的
    // stale_gen_marker）都不得出现。
    {
        use rusqlite::Connection;
        let conn = Connection::open(&dict_db).unwrap();
        let leaked: i64 = conn.query_row(
            "SELECT COUNT(*) FROM db_meta WHERE key IN ('local_corrections', 'stale_gen_marker')",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(leaked, 0, "旧代 WAL/主文件内容不得混入替换后的 dict.db");
    }

    // 端到端：替换后的 dict.db 必须能被 open_split 打开
    let user_db = dir.join("user.db");
    let engine = crate::engine::Engine::open_split(
        dict_db.to_string_lossy().as_ref(),
        user_db.to_string_lossy().as_ref(),
    );
    assert!(
        engine.is_ok(),
        "替换后的 dict.db 必须可打开: {:?}",
        engine.err()
    );
    let _ = fs::remove_dir_all(&dir);
}

/// 迁移中途崩溃的恢复：崩溃残留的 x1dict_words_new 不得让重跑报
/// "table already exists" 导致 Storage::open 永久失败——事务 + 前置
/// DROP IF EXISTS 后，残留现场应被清理并完成重建。
#[test]
fn migrate_survives_leftover_new_table() {
    let dir = temp_dir("poison_migrate");
    fs::create_dir_all(&dir).unwrap();
    let db = dir.join("poison.db");
    {
        use rusqlite::Connection;
        let conn = Connection::open(&db).unwrap();
        // 旧形态（含翻译三列，触发迁移检测）+ 模拟上次迁移崩溃的残留新表
        conn.execute_batch(
            "CREATE TABLE x1dict_words (
                id INTEGER PRIMARY KEY,
                simplified TEXT NOT NULL,
                pinyin TEXT NOT NULL,
                freq INTEGER DEFAULT 0,
                traditional TEXT, pinyin_tones TEXT, english TEXT
            );
            INSERT INTO x1dict_words (simplified, pinyin, freq) VALUES ('测', 'ce', 100);
            CREATE TABLE x1dict_words_new (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
    }
    // 重跑 CREATE TABLE x1dict_words_new 不得报 "table already exists"
    let storage = Storage::open(&db).expect("残留现场必须可恢复打开");
    // 新形态验证：翻译列已删、数据保留、无残留表
    let has_old: bool = storage
        .dict
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('x1dict_words')
             WHERE name IN ('traditional', 'pinyin_tones', 'english')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!has_old, "翻译三列应已随迁移删除");
    let leftover: i64 = storage
        .dict
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'x1dict_words_new'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leftover, 0, "迁移后不得残留 x1dict_words_new");
    let freq: i64 = storage
        .dict
        .query_row(
            "SELECT freq FROM x1dict_words WHERE simplified = '测' AND pinyin = 'ce'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(freq, 100, "迁移必须保留既有数据");
    drop(storage);
    let _ = fs::remove_dir_all(&dir);
}
