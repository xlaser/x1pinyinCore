use super::*;

#[test]
fn test_schema_init() {
    let storage = Storage::open_memory().unwrap();
    assert!(storage.x1dict_is_empty().unwrap());
}

#[test]
fn test_x1dict_import_and_load() {
    let mut storage = Storage::open_memory().unwrap();
    let words = vec![
        X1DictWord {
            simplified: "你好".into(),
            pinyin: "ni hao".into(),
            freq: 1000,
            ..Default::default()
        },
        X1DictWord {
            simplified: "中文".into(),
            pinyin: "zhong wen".into(),
            freq: 2000,
            ..Default::default()
        },
    ];
    let n = storage.import_x1dict_batch(&words).unwrap();
    assert_eq!(n, 2);
    assert!(!storage.x1dict_is_empty().unwrap());

    let all = storage.load_all_for_trie().unwrap();
    assert_eq!(all.len(), 2);
    // freq DESC 排序，中文在前
    assert_eq!(all[0].1, "中文");
    assert_eq!(all[1].1, "你好");
}

#[test]
fn test_user_freq_upsert() {
    let mut storage = Storage::open_memory().unwrap();
    let entries = vec![
        ("你好".to_string(), "ni hao".to_string(), 3u32),
        ("中文".to_string(), "zhong wen".to_string(), 1u32),
    ];
    storage.upsert_user_freq_batch(&entries).unwrap();

    assert_eq!(storage.get_user_freq("你好").unwrap(), Some(3));
    assert_eq!(storage.get_user_freq("中文").unwrap(), Some(1));
    assert_eq!(storage.get_user_freq("不存在").unwrap(), None);

    // 再次 upsert 累加
    let entries2 = vec![("你好".to_string(), "ni hao".to_string(), 2u32)];
    storage.upsert_user_freq_batch(&entries2).unwrap();
    assert_eq!(storage.get_user_freq("你好").unwrap(), Some(5));
}

#[test]
fn test_load_user_freq() {
    let mut storage = Storage::open_memory().unwrap();
    let entries = vec![
        ("你好".to_string(), "ni hao".to_string(), 3u32),
        ("中文".to_string(), "zhong wen".to_string(), 1u32),
    ];
    storage.upsert_user_freq_batch(&entries).unwrap();

    let map = storage.load_user_freq().unwrap();
    assert_eq!(map.get("你好"), Some(&3));
    assert_eq!(map.get("中文"), Some(&1));
}

#[test]
fn test_user_word_crud() {
    let mut storage = Storage::open_memory().unwrap();
    storage
        .upsert_user_freq_batch(&[("来说".to_string(), "lai shuo".to_string(), 2u32)])
        .unwrap();

    // 更新拼音和频率
    storage.update_user_word("来说", "lai shuo", 10).unwrap();
    let words = storage.load_user_words().unwrap();
    assert_eq!(words.len(), 1);
    assert_eq!(words[0].2, 10);

    // 删除
    storage.delete_user_word("来说").unwrap();
    assert!(storage.load_user_words().unwrap().is_empty());

    // 清空
    storage
        .upsert_user_freq_batch(&[
            ("你好".to_string(), "ni hao".to_string(), 1u32),
            ("中文".to_string(), "zhong wen".to_string(), 1u32),
        ])
        .unwrap();
    storage.clear_user_words().unwrap();
    assert!(storage.load_user_words().unwrap().is_empty());
}

#[test]
fn test_import_user_words() {
    let mut storage = Storage::open_memory().unwrap();
    storage
        .upsert_user_freq_batch(&[("你好".to_string(), "ni hao".to_string(), 3u32)])
        .unwrap();

    // 合并导入：频率累加
    storage
        .import_user_words(&[("你好".to_string(), "ni hao".to_string(), 2u32)], true)
        .unwrap();
    assert_eq!(storage.get_user_freq("你好").unwrap(), Some(5));

    // 覆盖导入：清空后只剩新数据
    storage
        .import_user_words(
            &[("中文".to_string(), "zhong wen".to_string(), 4u32)],
            false,
        )
        .unwrap();
    assert_eq!(storage.get_user_freq("中文").unwrap(), Some(4));
    assert_eq!(storage.get_user_freq("你好").unwrap(), None);
}

#[test]
fn test_sync_x1dict_from_builtin_preserves_user_words() {
    use std::fs;

    let dir = crate::test_tmp::dir().join("x1pinyin_sync_test");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let builtin_path = dir.join("builtin.db");
    let user_path = dir.join("user.db");

    // 创建内置词库
    let mut builtin = Storage::open(&builtin_path).unwrap();
    builtin
        .import_x1dict_batch(&[
            X1DictWord {
                simplified: "中国".into(),
                pinyin: "zhong guo".into(),
                freq: 5000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "中文".into(),
                pinyin: "zhong wen".into(),
                freq: 4000,
                ..Default::default()
            },
        ])
        .unwrap();
    drop(builtin);

    // 创建用户词库，包含自造词
    let mut user = Storage::open(&user_path).unwrap();
    user.upsert_user_freq_batch(&[("候选框".to_string(), "hou xuan kuang".to_string(), 5u32)])
        .unwrap();
    drop(user);

    // 重新打开用户词库并同步内置词库
    let mut user = Storage::open(&user_path).unwrap();
    assert!(user.x1dict_is_empty().unwrap());
    user.sync_x1dict_from_builtin(builtin_path.to_str().unwrap())
        .unwrap();

    // x1dict_words 应与内置词库一致
    let words = user.load_all_for_trie().unwrap();
    assert_eq!(words.len(), 2);

    // user_words 应被保留
    let user_words = user.load_user_words().unwrap();
    assert_eq!(user_words.len(), 1);
    assert_eq!(user_words[0].0, "候选框");
    assert_eq!(user_words[0].2, 5);

    // 再次同步：应仍然保留 user_words
    user.sync_x1dict_from_builtin(builtin_path.to_str().unwrap())
        .unwrap();
    assert_eq!(user.load_user_words().unwrap().len(), 1);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_sync_x1dict_adds_new_polyphonic_reading() {
    // 回归测试：sync_x1dict_from_builtin 必须能把内置 DB 新增的多音字读音
    //（如「谁 shui」）同步进用户 DB，即使该字的其他读音（「谁 shei」）已存在。
    //
    // 旧实现：sync 用 SELECT id 直接插入用户 DB，内置「谁 shui」的 id
    // 在用户 DB 已被别的词占用 → OR IGNORE 静默跳过 → 「谁 shui」永远
    // 同步不进去 → 用户打 shui 打不出「谁」。
    use std::fs;

    let dir = crate::test_tmp::dir().join("x1pinyin_sync_polyphonic_test");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let builtin_path = dir.join("builtin.db");
    let user_path = dir.join("user.db");

    // 内置 DB：「谁」有 shei + shui 两个读音（重建后状态）
    let mut builtin = Storage::open(&builtin_path).unwrap();
    builtin
        .import_x1dict_batch(&[
            X1DictWord {
                simplified: "谁".into(),
                pinyin: "shei".into(),
                freq: 5000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "谁".into(),
                pinyin: "shui".into(),
                freq: 5000,
                ..Default::default()
            },
        ])
        .unwrap();
    drop(builtin);

    // 用户 DB：先有「谁 shei」（旧状态，缺 shui）
    // 关键：用户 DB 的 id 分配与内置不同，制造 id 占位（插别的词撑大 id）
    let mut user = Storage::open(&user_path).unwrap();
    // 插入大量词，让用户 DB 的自增 id 远超内置「谁 shui」的 id
    for i in 0..200 {
        user.import_x1dict_batch(&[X1DictWord {
            simplified: format!("占位{}", i),
            pinyin: "zhan wei".into(),
            freq: 50,
            ..Default::default()
        }])
        .unwrap();
    }
    user.import_x1dict_batch(&[X1DictWord {
        simplified: "谁".into(),
        pinyin: "shei".into(),
        freq: 5000,
        ..Default::default()
    }])
    .unwrap();
    // 确认同步前用户 DB 只有「谁 shei」，无 shui
    let before: Vec<String> = user
        .conn()
        .prepare("SELECT pinyin FROM x1dict_words WHERE simplified='谁' ORDER BY pinyin")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(before, vec!["shei".to_string()], "同步前应只有 shei");
    drop(user);

    // 同步
    let mut user = Storage::open(&user_path).unwrap();
    user.sync_x1dict_from_builtin(builtin_path.to_str().unwrap())
        .unwrap();

    // 核心断言：同步后用户 DB 必须同时有「谁 shei」和「谁 shui」
    let after: Vec<String> = user
        .conn()
        .prepare("SELECT pinyin FROM x1dict_words WHERE simplified='谁' ORDER BY pinyin")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        after,
        vec!["shei".to_string(), "shui".to_string()],
        "同步后必须同时有 shei 和 shui"
    );
    // 不应有重复
    let dup_count: i64 = user
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM x1dict_words WHERE simplified='谁' AND pinyin='shei'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dup_count, 1, "shei 不应有重复条目");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_x1dict_unique_constraint_dedup() {
    // 回归测试：(simplified, pinyin) 唯一约束迁移必须正确去重存量重复条目。
    // 模拟旧 DB：先建无约束的表 + 插入重复，再 Storage::open 触发迁移。
    use rusqlite::Connection;
    use std::fs;

    let dir = crate::test_tmp::dir().join("x1pinyin_unique_test");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("u.db");

    // 用裸 Connection 造一个「旧式」DB：有 x1dict_words 表但无唯一约束，且含重复
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
                "CREATE TABLE x1dict_words (
                    id INTEGER PRIMARY KEY, simplified TEXT NOT NULL,
                    pinyin TEXT NOT NULL, freq INTEGER DEFAULT 0,
                    is_primary INTEGER DEFAULT 1);
                 CREATE TABLE db_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO x1dict_words (simplified, pinyin, freq, is_primary) VALUES ('谁','shei',5000,1);
                 INSERT INTO x1dict_words (simplified, pinyin, freq, is_primary) VALUES ('谁','shei',3000,1);
                 INSERT INTO x1dict_words (simplified, pinyin, freq, is_primary) VALUES ('谁','shei',4000,1);"
            ).unwrap();
    }

    // Storage::open 触发迁移：去重 + 建唯一约束
    let s = Storage::open(&path).unwrap();

    // 验证：重复已去重，只剩 1 条（保留 freq 最大 5000）
    let cnt: i64 = s
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM x1dict_words WHERE simplified='谁' AND pinyin='shei'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cnt, 1, "重复条目应被去重");
    let max_freq: i64 = s
        .conn()
        .query_row(
            "SELECT freq FROM x1dict_words WHERE simplified='谁' AND pinyin='shei'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(max_freq, 5000, "去重应保留 freq 最大的一条");

    // 验证唯一约束确实生效：再插重复应失败
    let result = s.conn().execute(
            "INSERT INTO x1dict_words (simplified, pinyin, freq, is_primary) VALUES ('谁','shei',9999,1)",
            [],
        );
    assert!(result.is_err(), "唯一约束建立后，重复插入应被拒绝");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_x1dict_crud_and_search() {
    let mut storage = Storage::open_memory().unwrap();
    let word = X1DictWord {
        simplified: "中国人".into(),
        pinyin: "zhong guo ren".into(),
        freq: 1500,
        ..Default::default()
    };
    storage.add_x1dict_word(&word).unwrap();
    assert_eq!(storage.x1dict_count_filtered(None).unwrap(), 1);

    // 搜索（按拼音片段，英文释义搜索已移除）
    let found = storage.list_x1dict_words(Some("zhong"), 10, 0).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].simplified, "中国人");

    // 更新
    let updated = X1DictWord {
        freq: 9999,
        ..word.clone()
    };
    storage
        .update_x1dict_word("中国人", "zhong guo ren", &updated)
        .unwrap();
    let list = storage.list_x1dict_words(None, 10, 0).unwrap();
    assert_eq!(list[0].freq, 9999);

    // 删除
    storage
        .delete_x1dict_word("中国人", "zhong guo ren")
        .unwrap();
    assert!(storage.x1dict_is_empty().unwrap());
}

/// 搜索音节感知排序（「搜 de 看不到 地」场景）：
/// 子串匹配命中 deng/记得 等拼音内含 de 的词，高频次音字被排到 200+ 位。
/// 四级排序：精确同音 → 音节前缀 → 词文本含 → 拼音内部含，同级 freq 降序。
#[test]
fn test_list_x1dict_words_syllable_priority() {
    let dir = crate::test_tmp::dir().join(format!(
        "x1_list_syl_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("t.db");
    let mut storage = Storage::open(db.to_str().unwrap()).unwrap();
    storage
        .import_x1dict_batch(&[
            X1DictWord {
                simplified: "的".into(),
                pinyin: "de".into(),
                freq: 9999,
                ..Default::default()
            },
            X1DictWord {
                simplified: "德".into(),
                pinyin: "de".into(),
                freq: 5230,
                ..Default::default()
            },
            X1DictWord {
                simplified: "地".into(),
                pinyin: "de".into(),
                freq: 2178,
                is_primary: false,
            },
            X1DictWord {
                simplified: "德国".into(),
                pinyin: "de guo".into(),
                freq: 6000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "得到".into(),
                pinyin: "de dao".into(),
                freq: 4000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "记得".into(),
                pinyin: "ji de".into(),
                freq: 5000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "等".into(),
                pinyin: "deng".into(),
                freq: 3000,
                ..Default::default()
            },
        ])
        .unwrap();

    let found = storage.list_x1dict_words(Some("de"), 10, 0).unwrap();
    let order: Vec<&str> = found.iter().map(|w| w.simplified.as_str()).collect();
    // 0 级（pinyin='de'，freq 降序）→ 1 级（de 开头音节词）→ 3 级（记得 freq 5000 先于 等 3000）
    assert_eq!(
        order,
        vec!["的", "德", "地", "德国", "得到", "记得", "等"],
        "搜索 de 应按 音节精确→音节前缀→内部含 排序，实际：{:?}",
        order
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_x1dict_tsv_import_export() {
    let mut storage = Storage::open_memory().unwrap();
    // 旧 7 列格式（含翻译三字段）——智能列识别应能解析并跳过翻译列
    let tsv = "你好\t你好\tni hao\tni3 hao3\thello/hi\t1000\n中国\t中國\tzhong guo\tzhong1 guo2\tChina\t2000";
    let n = storage.import_x1dict_tsv(tsv, true).unwrap();
    assert_eq!(n, 2);
    assert_eq!(storage.x1dict_count_filtered(None).unwrap(), 2);

    // 导出为新 4 列格式（simplified\tpinyin\tfreq\tis_primary）
    let exported = storage.export_x1dict_tsv().unwrap();
    assert!(exported.contains("中国\tzhong guo\t2000\t"));
    assert!(!exported.contains("China")); // 翻译字段不再导出
    assert!(!exported.contains("zhong1 guo2")); // 声调字段不再导出

    // 简化三列导入（simplified pinyin freq）
    let mut storage2 = Storage::open_memory().unwrap();
    let n2 = storage2
        .import_x1dict_tsv("来说\tlai shuo\t5", true)
        .unwrap();
    assert_eq!(n2, 1);
    let all = storage2.load_all_for_trie().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].1, "来说");

    // 新 4 列格式导入往返一致
    let mut storage3 = Storage::open_memory().unwrap();
    let n3 = storage3
        .import_x1dict_tsv("你好\tni hao\t1000\t1\n世界\tshi jie\t2000\t1", true)
        .unwrap();
    assert_eq!(n3, 2);
    let re = storage3.export_x1dict_tsv().unwrap();
    assert!(re.contains("你好\tni hao\t1000\t1"));
    assert!(re.contains("世界\tshi jie\t2000\t1"));
}

#[test]
fn test_migrate_x1dict_drop_translate_columns() {
    // 模拟老库：手动建一个含 traditional/pinyin_tones/english 三列的旧表，
    // 写入带翻译数据的行，再通过 Storage::open 触发迁移。
    let db_path = crate::test_tmp::dir().join("x1pinyin_migrate_test.db");
    let _ = std::fs::remove_file(&db_path);

    // 先用裸 SQL 建旧 schema 并插数据（绕过 init_schema，直接造老库）
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
                "CREATE TABLE x1dict_words (
                    id INTEGER PRIMARY KEY,
                    simplified TEXT NOT NULL,
                    traditional TEXT,
                    pinyin TEXT NOT NULL,
                    pinyin_tones TEXT,
                    english TEXT,
                    freq INTEGER DEFAULT 0,
                    is_primary INTEGER DEFAULT 1
                );
                INSERT INTO x1dict_words (simplified, traditional, pinyin, pinyin_tones, english, freq, is_primary)
                VALUES
                    ('你好', '你好', 'ni hao', 'ni3 hao3', 'hello', 1000, 1),
                    ('中国', '中國', 'zhong guo', 'zhong1 guo2', 'China', 2000, 1);
                CREATE TABLE IF NOT EXISTS db_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
            )
            .unwrap();
    }

    // 打开 —— init_schema 应触发 migrate_x1dict_drop_translate_columns
    let storage = Storage::open(&db_path).unwrap();

    // 验证：三列已不存在
    let cols: Vec<String> = storage
        .conn()
        .prepare("SELECT name FROM pragma_table_info('x1dict_words')")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(
        !cols.contains(&"traditional".to_string()),
        "traditional 未删除: {:?}",
        cols
    );
    assert!(
        !cols.contains(&"pinyin_tones".to_string()),
        "pinyin_tones 未删除"
    );
    assert!(!cols.contains(&"english".to_string()), "english 未删除");
    // 必留列仍在
    assert!(cols.contains(&"simplified".to_string()));
    assert!(cols.contains(&"pinyin".to_string()));
    assert!(cols.contains(&"freq".to_string()));
    assert!(cols.contains(&"is_primary".to_string()));

    // 验证：数据保留（id/simplified/pinyin/freq 不丢）
    let count = storage.x1dict_count().unwrap();
    assert_eq!(count, 2);
    let words = storage.list_x1dict_words(None, 10, 0).unwrap();
    assert!(words
        .iter()
        .any(|w| w.simplified == "中国" && w.freq == 2000));

    // 验证：db_meta 已标记，重复打开不重复迁移
    drop(storage);
    let storage2 = Storage::open(&db_path).unwrap();
    let count2 = storage2.x1dict_count().unwrap();
    assert_eq!(count2, 2);

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_detect_tsv_column_map() {
    // 表头识别（新格式 4 列）
    let tsv = "simplified\tpinyin\tfreq\tis_primary\n你好\tni hao\t1000\t1";
    let (m, skip) = detect_tsv_column_map(tsv);
    assert_eq!(m.simplified, 0);
    assert_eq!(m.pinyin, 1);
    assert_eq!(m.freq, Some(2));
    assert_eq!(m.is_primary, Some(3));
    assert!(skip);

    // 表头识别（旧格式 7 列，翻译列应被忽略）
    let tsv_old = "simplified\ttraditional\tpinyin\tpinyin_tones\tenglish\tfreq\tis_primary\n中国\t中國\tzhong guo\tzhong1 guo2\tChina\t2000\t1";
    let (m2, skip2) = detect_tsv_column_map(tsv_old);
    assert_eq!(m2.simplified, 0);
    assert_eq!(m2.pinyin, 2); // traditional 在 1，pinyin 在 2
    assert_eq!(m2.freq, Some(5));
    assert_eq!(m2.is_primary, Some(6));
    assert!(skip2);

    // 无表头（新格式 4 列数据）——启发式
    let tsv_data = "你好\tni hao\t1000\t1\n世界\tshi jie\t2000\t1";
    let (m3, skip3) = detect_tsv_column_map(tsv_data);
    assert!(!skip3);
    assert_eq!(m3.simplified, 0);
    assert_eq!(m3.pinyin, 1);

    // 无表头（旧格式 7 列数据）——启发式应跳过翻译列定位 pinyin
    let tsv_old_data = "中国\t中國\tzhong guo\tzhong1 guo2\tChina\t2000\t1";
    let (m4, _skip4) = detect_tsv_column_map(tsv_old_data);
    // pinyin 应定位到列 2（含空格的拼音），而非列 4（english）
    assert_eq!(m4.pinyin, 2);
}

#[test]
fn test_cleanup_single_char_user_words() {
    let mut storage = Storage::open_memory().unwrap();
    storage
        .upsert_user_freq_batch(&[
            ("我".to_string(), "wo".to_string(), 3u32),
            ("你好".to_string(), "ni hao".to_string(), 2u32),
        ])
        .unwrap();
    let n = storage.cleanup_single_char_user_words().unwrap();
    assert_eq!(n, 1);
    let words = storage.load_user_words().unwrap();
    assert_eq!(words.len(), 1);
    assert_eq!(words[0].0, "你好");
}

#[test]
fn test_backfill_user_pinyin_from_x1dict() {
    let mut storage = Storage::open_memory().unwrap();
    // 导入 X1Dict 词条
    storage
        .import_x1dict_batch(&[
            X1DictWord {
                simplified: "你好".into(),
                pinyin: "ni hao".into(),
                freq: 1000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "再见".into(),
                pinyin: "zai jian".into(),
                freq: 1000,
                ..Default::default()
            },
        ])
        .unwrap();

    // 模拟旧版本用户词：有频率但无拼音
    storage
        .upsert_user_freq_batch(&[
            ("你好".to_string(), "".to_string(), 5u32),
            ("再见".to_string(), "".to_string(), 3u32),
            ("自造词".to_string(), "".to_string(), 2u32),
        ])
        .unwrap();

    let n = storage.backfill_user_pinyin_from_x1dict().unwrap();
    assert_eq!(n, 2);

    let words: std::collections::HashMap<String, String> = storage
        .load_user_words()
        .unwrap()
        .into_iter()
        .map(|(t, p, _)| (t, p))
        .collect();
    assert_eq!(words.get("你好").unwrap(), "ni hao");
    assert_eq!(words.get("再见").unwrap(), "zai jian");
    // 不在 X1Dict 中的自造词保持空拼音，等待后续 learn/commit 回填
    assert_eq!(words.get("自造词").unwrap(), "");
}

/// 防回归测试：用户修改系统词后，重启不应触发同步覆盖。
///
/// 模拟场景：
/// 1. 内置 DB 有 2 个词
/// 2. 用户 DB 同步后记录哈希
/// 3. 用户在管理器中删除一个系统词
/// 4. 重启时 `x1dict_sync_needed` 应返回 false（内置 DB 没变）
#[test]
fn test_x1dict_sync_not_needed_after_user_modification() {
    use std::fs;

    let dir = crate::test_tmp::dir().join("x1pinyin_hash_sync_test");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let builtin_path = dir.join("builtin.db");
    let user_path = dir.join("user.db");

    // 创建"内置" DB（模拟 bundle 中的 x1pinyin.db）
    let mut builtin = Storage::open(&builtin_path).unwrap();
    builtin
        .import_x1dict_batch(&[
            X1DictWord {
                simplified: "你好".into(),
                pinyin: "ni hao".into(),
                freq: 1000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "中文".into(),
                pinyin: "zhong wen".into(),
                freq: 2000,
                ..Default::default()
            },
        ])
        .unwrap();
    drop(builtin);

    // 创建"用户" DB
    let mut user = Storage::open(&user_path).unwrap();

    // 首次同步
    let bp = builtin_path.to_str().unwrap();
    assert!(user.x1dict_sync_needed(bp).unwrap());
    user.sync_x1dict_from_builtin(bp).unwrap();
    user.record_x1dict_sync(bp).unwrap();

    // 同步后再检查：哈希已记录，不应再需要同步
    assert!(!user.x1dict_sync_needed(bp).unwrap());

    // 用户在管理器中删除一个系统词（模拟用户自定义操作）
    user.delete_x1dict_word("中文", "zhong wen").unwrap();

    // 此时用户 DB 只有 1 个词，内置 DB 有 2 个，但哈希相同 → 不应同步
    // 旧逻辑用行数比较会错误地触发同步！
    assert!(
        !user.x1dict_sync_needed(bp).unwrap(),
        "用户修改系统词后不应触发同步"
    );

    // 内置 DB 更新（新增一个词）→ 哈希变化 → 应触发同步
    let mut builtin2 = Storage::open(&builtin_path).unwrap();
    builtin2
        .import_x1dict_batch(&[X1DictWord {
            simplified: "世界".into(),
            pinyin: "shi jie".into(),
            freq: 1500,
            ..Default::default()
        }])
        .unwrap();
    drop(builtin2);

    assert!(
        user.x1dict_sync_needed(bp).unwrap(),
        "内置 DB 更新后应触发同步"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// 测试内容哈希的确定性
#[test]
fn test_x1dict_content_hash_deterministic() {
    let mut storage1 = Storage::open_memory().unwrap();
    let mut storage2 = Storage::open_memory().unwrap();

    let words = vec![
        X1DictWord {
            simplified: "你好".into(),
            pinyin: "ni hao".into(),
            freq: 1000,
            ..Default::default()
        },
        X1DictWord {
            simplified: "中文".into(),
            pinyin: "zhong wen".into(),
            freq: 2000,
            ..Default::default()
        },
    ];

    storage1.import_x1dict_batch(&words).unwrap();
    storage2.import_x1dict_batch(&words).unwrap();

    let hash1 = storage1.x1dict_content_hash().unwrap();
    let hash2 = storage2.x1dict_content_hash().unwrap();
    assert_eq!(hash1, hash2, "相同内容应产生相同哈希");
    assert_eq!(hash1.len(), 16, "哈希应为 16 字符十六进制");
}

/// 守护：旧加密 DB 一次性迁移（加密/混淆已从引擎移除）。
///
/// 历史发布版用户 DB 三表是 XOR 密文 + freq_encryption_version=v1 标记。
/// 加密代码已删除（无密钥可解），迁移策略 = 丢弃密文数据：用明文种子覆盖
/// freq、清空种子搭配表（下次 sync 重建）、清标记。用户数据表不受影响。
#[test]
fn test_migrate_legacy_encrypted_db_discards_ciphertext() {
    use std::fs;

    let dir = crate::test_tmp::dir().join(format!(
        "x1pinyin_migrate_legacy_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();

    let builtin_path = dir.join("builtin_plain.db");
    let user_path = dir.join("user_legacy.db");

    // 明文种子：正确 freq + 种子搭配
    let mut builtin = Storage::open(&builtin_path).unwrap();
    builtin
        .import_x1dict_batch(&[
            X1DictWord {
                simplified: "省队".into(),
                pinyin: "sheng dui".into(),
                freq: 3,
                ..Default::default()
            },
            X1DictWord {
                simplified: "中国".into(),
                pinyin: "zhong guo".into(),
                freq: 500,
                ..Default::default()
            },
        ])
        .unwrap();
    builtin.ensure_seed_bigram_table().unwrap();
    builtin
        .insert_seed_bigram_for_test("个", "全椒", 10)
        .unwrap();
    builtin.ensure_seed_trigram_table().unwrap();
    builtin
        .insert_seed_trigram_for_test("我", "不", "知道", 5)
        .unwrap();
    drop(builtin);

    // 模拟旧加密用户 DB：freq/种子 count 是密文垃圾 + v1 标记
    let mut user = Storage::open(&user_path).unwrap();
    user.import_x1dict_batch(&[
        X1DictWord {
            simplified: "省队".into(),
            pinyin: "sheng dui".into(),
            freq: 3447936034,
            ..Default::default()
        },
        X1DictWord {
            simplified: "中国".into(),
            pinyin: "zhong guo".into(),
            freq: 3447936034,
            ..Default::default()
        },
    ])
    .unwrap();
    user.ensure_seed_bigram_table().unwrap();
    user.insert_seed_bigram_for_test("个", "全椒", 3447936034)
        .unwrap();
    user.ensure_seed_trigram_table().unwrap();
    user.insert_seed_trigram_for_test("我", "不", "知道", 3447936034)
        .unwrap();
    user.set_seed_meta("freq_encryption_version", "v1").unwrap();

    // ① 迁移：密文数据被明文种子覆盖/清空 + 清标记
    let migrated = user
        .migrate_legacy_encrypted_db_if_needed(builtin_path.to_str().unwrap())
        .unwrap();
    assert!(migrated, "带 v1 标记的 DB 应执行迁移");
    assert!(
        user.get_seed_meta("freq_encryption_version")
            .unwrap()
            .is_none(),
        "迁移后应清标记"
    );

    let words = user.load_all_for_trie().unwrap();
    let freq_map: std::collections::HashMap<String, u32> =
        words.iter().map(|(_, s, f, _)| (s.clone(), *f)).collect();
    assert_eq!(
        *freq_map.get("省队").unwrap(),
        3,
        "密文 freq 应被种子明文覆盖"
    );
    assert_eq!(*freq_map.get("中国").unwrap(), 500);

    assert!(
        user.load_seed_bigram_counts().unwrap().is_empty(),
        "密文种子 bigram 应被清空（等待 sync 重建）"
    );
    assert_eq!(
        user.seed_trigram_count("我", "不", "知道"),
        0,
        "密文种子 trigram 应被清空"
    );

    // ② 幂等：标记已清，再次调用跳过
    assert!(
        !user
            .migrate_legacy_encrypted_db_if_needed(builtin_path.to_str().unwrap())
            .unwrap(),
        "无标记应跳过迁移"
    );

    // ③ 明文 DB（无标记）不受影响
    let mut plain = Storage::open(dir.join("plain.db")).unwrap();
    plain
        .import_x1dict_batch(&[X1DictWord {
            simplified: "测试".into(),
            pinyin: "ce shi".into(),
            freq: 100,
            ..Default::default()
        }])
        .unwrap();
    assert!(!plain
        .migrate_legacy_encrypted_db_if_needed(builtin_path.to_str().unwrap())
        .unwrap());
    let raw: i64 = plain
        .conn()
        .query_row(
            "SELECT freq FROM x1dict_words WHERE simplified = '测试'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(raw, 100, "明文 DB 迁移应无副作用");

    let _ = fs::remove_dir_all(&dir);
}

/// 守护：词库管理器/导入导出路径的明文 round-trip（加密移除后）。
/// list_x1dict_words 按 freq DESC 分页、export_x1dict_tsv 降序导出、
/// import_x1dict_tsv 明文写入并可按列表读回原值。
#[test]
fn test_dict_manager_paths_plaintext_roundtrip() {
    use std::fs;

    let dir = crate::test_tmp::dir().join(format!(
        "x1pinyin_dm_paths_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("dm.db");

    let mut s = Storage::open(&db_path).unwrap();
    s.import_x1dict_batch(&[
        X1DictWord {
            simplified: "低频词".into(),
            pinyin: "di pin ci".into(),
            freq: 50,
            ..Default::default()
        },
        X1DictWord {
            simplified: "中国".into(),
            pinyin: "zhong guo".into(),
            freq: 5000,
            ..Default::default()
        },
        X1DictWord {
            simplified: "人民".into(),
            pinyin: "ren min".into(),
            freq: 3000,
            ..Default::default()
        },
        X1DictWord {
            simplified: "我".into(),
            pinyin: "wo".into(),
            freq: 9999,
            ..Default::default()
        },
    ])
    .unwrap();

    // 列表：freq DESC 排序 + 分页
    let list = s.list_x1dict_words(None, 100, 0).unwrap();
    assert_eq!(list[0].simplified, "我");
    assert_eq!(list[0].freq, 9999);
    let page2 = s.list_x1dict_words(None, 2, 2).unwrap();
    assert_eq!(page2[0].simplified, "人民");
    assert_eq!(page2[1].simplified, "低频词");

    // 过滤路径
    let filtered = s.list_x1dict_words(Some("中国"), 100, 0).unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].freq, 5000);

    // 导出：freq 明文 + 降序
    let tsv = s.export_x1dict_tsv().unwrap();
    let first_line = tsv.lines().next().unwrap();
    assert!(
        first_line.starts_with("我\two\t9999\t"),
        "导出首行应为最高频词: {}",
        first_line
    );

    // 导入：freq 明文 round-trip
    s.import_x1dict_tsv("导入词\tdao ru ci\t777\t1\n", false)
        .unwrap();
    let after = s.list_x1dict_words(Some("导入词"), 10, 0).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].freq, 777);

    let _ = fs::remove_dir_all(&dir);
}

/// 守护：seed_bigram/trigram 明文读路径（加密移除后）。
/// load_seed_bigram_counts / seed_trigram_count / seed_trigram_bucket /
/// seed_trigram_context_total 直读明文原值。
#[test]
fn test_seed_tables_plaintext_read_paths() {
    use std::fs;

    let dir = crate::test_tmp::dir().join(format!(
        "x1pinyin_seed_plain_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("seed_plain.db");

    let mut storage = Storage::open(&db_path).unwrap();
    storage
        .import_x1dict_batch(&[X1DictWord {
            simplified: "省队".into(),
            pinyin: "sheng dui".into(),
            freq: 3,
            ..Default::default()
        }])
        .unwrap();
    storage.ensure_seed_bigram_table().unwrap();
    storage
        .insert_seed_bigram_for_test("个", "全椒", 10)
        .unwrap();
    storage
        .insert_seed_bigram_for_test("我们", "的", 200)
        .unwrap();
    storage.ensure_seed_trigram_table().unwrap();
    storage
        .insert_seed_trigram_for_test("我", "不", "知道", 5)
        .unwrap();

    assert_eq!(
        storage.load_seed_bigram_counts().unwrap(),
        vec![
            ("个".to_string(), "全椒".to_string(), 10),
            ("我们".to_string(), "的".to_string(), 200)
        ]
    );
    assert_eq!(storage.seed_trigram_count("我", "不", "知道"), 5);
    assert_eq!(
        storage.seed_trigram_bucket("我", "不").get("知道"),
        Some(&5)
    );
    assert_eq!(storage.seed_trigram_context_total("我", "不"), 5);

    let _ = fs::remove_dir_all(&dir);
}

/// 守护：load_all_for_trie 必须 clamp 异常 freq。
///
/// 即使异常值漏进 DB（导入异常/历史密文残留），load_all_for_trie 加载到 Trie 时
/// 必须 clamp：freq > MAX_FREQ（10000）视为异常，clamp 到 100。这是最后一道防线。
#[test]
fn test_load_all_for_trie_clamps_anomalous_freq() {
    use std::fs;

    let dir = crate::test_tmp::dir().join(format!(
        "x1pinyin_clamp_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();

    let db_path = dir.join("anomaly.db");
    let mut storage = Storage::open(&db_path).unwrap();
    storage
        .import_x1dict_batch(&[
            X1DictWord {
                simplified: "正常词".into(),
                pinyin: "zheng chang ci".into(),
                freq: 200,
                ..Default::default()
            },
            X1DictWord {
                simplified: "异常词".into(),
                pinyin: "yi chang ci".into(),
                freq: 3447936034,
                ..Default::default()
            },
        ])
        .unwrap();

    // load_all_for_trie 应该 clamp 异常值
    let words = storage.load_all_for_trie().unwrap();
    let freq_map: std::collections::HashMap<String, u32> =
        words.iter().map(|(_, s, f, _)| (s.clone(), *f)).collect();

    let normal = *freq_map.get("正常词").unwrap();
    let anomaly = *freq_map.get("异常词").unwrap();

    assert_eq!(normal, 200, "正常词 freq 应保持 200");
    assert_eq!(
        anomaly, 100,
        "异常词 freq 应被 clamp 到 100，实际：{}",
        anomaly
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_trigram_counts_roundtrip() {
    // trigram 表 load/replace 全量往返
    let mut storage = Storage::open_memory().unwrap();
    let entries = vec![
        ("我".to_string(), "知道".to_string(), "你".to_string(), 5u32),
        ("知道".to_string(), "你".to_string(), "的".to_string(), 2u32),
    ];
    storage.replace_trigram_counts(&entries).unwrap();
    let loaded = storage.load_trigram_counts().unwrap();
    // 顺序可能不同，按内容比较
    assert_eq!(loaded.len(), 2, "trigram 表应加载 2 条");
    let find = |p2: &str, p1: &str, t: &str| -> u32 {
        loaded
            .iter()
            .find(|(a, b, c, _)| a == p2 && b == p1 && c == t)
            .map(|(_, _, _, c)| *c)
            .unwrap_or(0)
    };
    assert_eq!(find("我", "知道", "你"), 5);
    assert_eq!(find("知道", "你", "的"), 2);
    assert_eq!(
        find("我", "知道", "他"),
        0,
        "未记录的组合应为 0（不在此表）"
    );

    // 全量替换应清空旧数据
    storage
        .replace_trigram_counts(&[("他".to_string(), "说".to_string(), "了".to_string(), 1u32)])
        .unwrap();
    let loaded2 = storage.load_trigram_counts().unwrap();
    assert_eq!(loaded2.len(), 1, "全量替换后只剩新数据");
    assert_eq!(loaded2[0].0, "他");
}

/// 种子 bigram 磁盘缓存生命周期守护（启动优化）。
///
/// 契约：无 sync_hash 不写缓存（防开发种子/临时目录污染）→ 有 hash 首次
/// 构建并写缓存 → 二次加载命中缓存（表已清空仍返回缓存数据，证明缓存路径）
/// → hash 变化缓存失效回退 SQL（读到空表）。
#[test]
fn test_seed_bigram_cache_lifecycle() {
    let dir = crate::test_tmp::dir().join(format!("x1core_sbcache_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("user.db");
    let cache = dir.join("seed_bigram_cache.bin");

    let mut s = Storage::open(&db).unwrap();
    s.ensure_seed_bigram_table().unwrap();
    s.insert_seed_bigram_for_test("饭", "吃", 15).unwrap();
    s.insert_seed_bigram_for_test("天", "气", 100).unwrap();

    // 1. 无 sync_hash：走 SQL 慢路径，不写缓存文件
    let t = s.load_seed_bigram_table_cached().unwrap();
    assert_eq!(t.get("饭", "吃"), Some(15));
    assert!(!cache.exists(), "无 sync_hash 不应写缓存文件");

    // 2. 写入 sync_hash：首次构建 + 写缓存文件
    s.set_seed_meta("x1dict_sync_hash", "0123456789abcdef")
        .unwrap();
    let t = s.load_seed_bigram_table_cached().unwrap();
    assert_eq!(t.get("饭", "吃"), Some(15));
    assert!(cache.exists(), "有 sync_hash 应写缓存文件");

    // 3. 表清空但 hash 不变：命中缓存（仍返回数据 = 证明走的是缓存路径）
    s.conn()
        .execute("DELETE FROM seed_bigram_counts", [])
        .unwrap();
    let t = s.load_seed_bigram_table_cached().unwrap();
    assert_eq!(t.get("饭", "吃"), Some(15), "hash 未变应命中缓存");
    assert_eq!(t.get("天", "气"), Some(100));

    // 4. hash 变化（sync 更新种子表的场景）：缓存失效，回退 SQL 读到空表
    s.set_seed_meta("x1dict_sync_hash", "fedcba9876543210")
        .unwrap();
    let t = s.load_seed_bigram_table_cached().unwrap();
    assert_eq!(t.get("饭", "吃"), None, "hash 变化应缓存失效回退 SQL");

    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_wal_tuning_pragmas_applied() {
    // WAL 调优（与统计库同标准）：词库/user 库连接 autocheckpoint
    // 2000 页 + WAL 截断上限 8MB——周期 flush 的 diff 事务回拷频率减半、WAL
    // 不随会话时长无界增长。open_raw 双端共享，本测试钉住参数不回退。
    use std::fs;
    let dir = crate::test_tmp::dir().join(format!("x1pinyin_walp_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let conn = Storage::open_raw(&dir.join("wal.db")).unwrap();
    let num = |p: &str| -> i64 {
        conn.query_row(&format!("PRAGMA {}", p), [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(num("wal_autocheckpoint"), 2000);
    assert_eq!(num("journal_size_limit"), 8_388_608);
    let jm: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(jm, "wal");
    drop(conn);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_user_words_multi_pinyin_migration() {
    // 旧 schema（列级 UNIQUE(text)）→ 新 schema（UNIQUE(text, pinyin)）迁移
    //（同词多读音共存）：存量行无损、同词第二读音可插入
    //（旧 schema 必撞 UNIQUE(text)）、词频按 text 聚合（MAX）、重开幂等。
    use std::fs;
    let dir = crate::test_tmp::dir().join(format!(
        "x1pinyin_uw_migrate_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let db = dir.join("migrate.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE user_words (
                    id         INTEGER PRIMARY KEY AUTOINCREMENT,
                    text       TEXT NOT NULL UNIQUE,
                    pinyin     TEXT NOT NULL,
                    user_freq  INTEGER DEFAULT 1,
                    created_at REAL NOT NULL,
                    updated_at REAL NOT NULL
                );
                INSERT INTO user_words (text, pinyin, user_freq, created_at, updated_at)
                VALUES ('主角', 'zhu jiao', 3, 1, 1), ('习惯', 'xi guan', 5, 1, 1);",
        )
        .unwrap();
    }
    // Storage::open 跑迁移
    let mut s = Storage::open(&db).unwrap();
    // 存量无损
    assert_eq!(s.user_words_count_for_text("主角"), 1);
    assert_eq!(s.user_words_count_for_text("习惯"), 1);
    // 同词第二读音可写入（旧 schema 这里会 UNIQUE(text) 失败）
    s.upsert_user_words_batch(&[("主角".to_string(), "zhu jue".to_string(), 3)])
        .unwrap();
    assert_eq!(s.user_words_count_for_text("主角"), 2, "同词两读音应共存");
    // 词频按 text 聚合（MAX 语义：快照行值相同）
    let freqs = s.load_user_freq().unwrap();
    assert_eq!(freqs.get("主角"), Some(&3));
    assert_eq!(freqs.get("习惯"), Some(&5));
    // 重开幂等（迁移标记生效，数据仍在）
    drop(s);
    let s2 = Storage::open(&db).unwrap();
    assert_eq!(s2.user_words_count_for_text("主角"), 2);
    fs::remove_dir_all(&dir).ok();
}

/// 种子写保护守护自检：测试构建下可写打开仓库种子必须 panic。
///
/// 守护的理由见 `Storage::guard_repo_seed_open` 注释——套件曾偶发把交付种子
/// 写脏（db_meta.x1dict_sync_hash 改写 + assets/seed_bigram_cache.bin 重写 +
/// assets/*.arena_cache.bin 生成）。本测试守护守护本身：panic 发生在任何 I/O
/// 之前，不会污染种子。
#[test]
#[should_panic(expected = "测试禁止可写打开仓库种子")]
fn test_repo_seed_writable_open_blocked() {
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let _ = Storage::open(seed);
}
