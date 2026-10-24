//! Engine 测试 · 词库 CRUD / TSV 导入导出 / 持久化。
//!
//! 覆盖：
//! - TSV 导入导出：用户词典 export/import、跳过单字、清理既有单字
//! - 系统词 CRUD / 确定性顺序 / log_freq 不压制系统词 / X1Dict TSV 重载 Trie
//! - 用户词 vs 系统词排序（同长 / 长前缀）
//! - 持久化：commit 权重跨重启、DB 健康数据启动保留、黑名单跨重启、commit 落库存拼音
//! - commit 规范化未分拼 / flush 用户频次 / 单字偏好覆盖常用度
//!
//! mod.rs 用 `#[cfg(test)] mod tests_dict;` 加载（分号形式从文件加载）。
//! `use super::*` 仍指向 engine mod，测试访问 engine 的 pub 项不变。

use super::*;

#[test]
fn test_commit_normalizes_unspaced_pinyin() {
    // 8feb0ea 后「通知」是系统词不写 user_words，但 process 应能命中系统词。
    let mut engine = Engine::new();
    engine.commit("tongzhi", "通知", Some("tongzhi"), true);
    assert!(
        engine
            .process("tongzhi")
            .iter()
            .any(|c| c.text.as_ref() == "通知"),
        "commit 无空格拼音应规范化，全拼查询仍能命中「通知」"
    );
    assert!(
        engine.freq_counts_contains_for_test("通知"),
        "commit「通知」后 freq.counts 应有记录"
    );
}

/// 系统词不应重复写入 user_words（"奥利奥"重复 bug 回归）。
/// 已是 base_dict 系统词的，commit 后只更新 freq，不进 user_words。
#[test]
fn test_system_word_not_duplicated_in_user_words() {
    let seed = super::tests_query::seed_copy("dict");
    let mut engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
    // 找一个种子 DB 里已有的系统词（如"不知道"bu zhi dao，3音节真词）
    let cands = engine.process("buzhidao");
    let has_buzhidao = cands.iter().any(|c| c.text.as_ref() == "不知道");
    if !has_buzhidao {
        // 种子 DB 可能没有，跳过
        eprintln!("[跳过] 种子 DB 无'不知道'，无法测系统词去重");
        return;
    }
    // commit 该系统词
    engine.commit("buzhidao", "不知道", Some("bu zhi dao"), true);
    // 系统词不应进 user_words
    assert!(
        !engine.user_words.contains_key("不知道"),
        "系统词'不知道'不应写入 user_words（避免与 x1dict_words 重复）"
    );
}

/// 探针：模拟用户 DB 已被污染（旧版本 commit 学错成「香木」），
/// 验证新版本能否正确覆盖：commit「输入法项目」后再次查询，已学词应主导。
#[test]
fn test_commit_word_weights_survive_restart() {
    crate::test_tmp::require_seed_db();
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let tmp_path = crate::test_tmp::dir().join("x1pinyin_test_restart.db");
    let _ = std::fs::remove_file(&tmp_path);
    std::fs::copy(seed, &tmp_path).expect("copy seed db");

    // 第一个 engine：在副本上 commit_with_solidify「输入法项目」并 flush。
    // 4-8 字整句需 force_solidify 才固化进 user_words（固化规则 §4.3）。
    {
        let mut e1 =
            Engine::new_with_db(tmp_path.to_str().unwrap()).unwrap_or_else(|_| Engine::new());
        e1.commit_with_solidify(
            "shurufaxiangmu",
            "输入法项目",
            Some("shu ru fa xiang mu"),
            true,
            true,
        );
        e1.flush().ok();
    }

    // 模拟重启：新 engine 从同一 DB 加载
    let engine_restart =
        Engine::new_with_db(tmp_path.to_str().unwrap()).unwrap_or_else(|_| Engine::new());
    // 整句「输入法项目」(5字 ≤8) 经 force_solidify 应固化到 user_words，跨重启保留
    assert!(
        engine_restart.user_words.contains_key("输入法项目"),
        "整句「输入法项目」应跨重启保留（≤8字 + force_solidify 固化）。实际 user_words: {:?}",
        engine_restart.user_words.keys().take(5).collect::<Vec<_>>()
    );

    // 重启后查询：首候选必须是「输入法项目」（白词固化后直接命中）
    let cands = engine_restart.process("shurufaxiangmu");
    let first = cands.first().map(|c| c.text.as_ref());
    assert_eq!(
        first,
        Some("输入法项目"),
        "重启后 shurufaxiangmu 首候选必须仍是「输入法项目」（白词固化跨会话生效），实际：{:?}",
        first
    );

    let _ = std::fs::remove_file(&tmp_path);
}

/// 回归测试：DB 健康时 Storage::open 不应误伤数据（启动秒开，不自愈）。
///
/// 设计原则：启动时**不做任何 DB 完整性检查/修复**——守住启动秒开的性能要求。
/// DB 维护（备份 + REINDEX + VACUUM + 脏数据清理）统一在部署脚本于部署期完成。
/// 此测试守护「健康 DB 不被启动流程误伤」。
#[test]
fn test_db_open_preserves_healthy_data() {
    use crate::storage::Storage;
    let tmp = crate::test_tmp::dir().join("x1pinyin_test_open.db");
    let _ = std::fs::remove_file(&tmp);

    // 写入测试数据
    {
        let mut storage = Storage::open(&tmp).unwrap();
        storage
            .upsert_user_words_batch(&[("测试词".to_string(), "ce shi ci".to_string(), 5)])
            .unwrap();
        assert_eq!(storage.integrity_check(), "ok");
        assert_eq!(storage.user_words_count_for_text("测试词"), 1);
    }

    // 重新打开（启动流程），数据必须完整保留
    let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap_or_else(|_| Engine::new());
    assert!(
        engine.user_words.contains_key("测试词"),
        "启动加载不应丢失有效用户词"
    );

    let _ = std::fs::remove_file(&tmp);
}

/// 回归测试：用户删除的词持久化到 user_blacklist，跨重启/跨部署保留。
///
/// 若 removed_texts 只是内存 HashSet，重启即丢；部署脚本合并用户数据时
/// 会把种子 DB 里的脏词带回来，删除过的垃圾词每次部署又出现。
///
/// 处理：新增 user_blacklist 表，remove() 写入持久化黑名单，
/// new_with_db 启动时加载，部署合并时跳过黑名单词。
#[test]
fn test_blacklist_persists_across_restart() {
    crate::test_tmp::require_seed_db();
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let tmp = crate::test_tmp::dir().join("x1pinyin_test_blacklist.db");
    // 必须连 WAL/SHM 一起删：种子 DB 是 WAL 模式，若上次测试崩溃残留 -wal，
    // 只覆盖主文件会让 SQLite 打开时回放旧 WAL → "database disk image is malformed"。
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
    let _ = std::fs::remove_file(tmp.with_extension("db-shm"));
    std::fs::copy(seed, &tmp).expect("copy seed db");

    // 第一个 engine：learn 一个词，然后 remove 它
    {
        let mut e1 = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
        e1.learn("测试脏词", "ce shi zang ci");
        assert!(e1.user_words.contains_key("测试脏词"));
        e1.remove("测试脏词");
        assert!(
            !e1.user_words.contains_key("测试脏词"),
            "remove 后应从 user_words 删除"
        );
        assert!(e1.removed_texts.contains("测试脏词"), "应进会话级黑名单");
        e1.flush().ok();
    }

    // 重启：黑名单应保留
    let engine2 = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
    assert!(
        engine2.removed_texts.contains("测试脏词"),
        "持久化黑名单应跨重启保留（user_blacklist 表）"
    );
    assert!(
        !engine2.user_words.contains_key("测试脏词"),
        "删除的词不应在 user_words 里"
    );

    // commit 复活：清除黑名单
    let mut engine3 = engine2;
    engine3.commit("ceshizangci", "测试脏词", Some("ce shi zang ci"), true);
    assert!(
        !engine3.removed_texts.contains("测试脏词"),
        "commit 后应清除黑名单（复活机制）"
    );

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn test_flush_user_freq_to_db() {
    let mut path = crate::test_tmp::dir();
    path.push("x1pinyin_test_flush.db");
    let _ = std::fs::remove_file(&path);

    let mut engine = Engine::new_with_db(path.to_str().unwrap()).unwrap();
    engine.commit("", "你好", None, true);
    engine.commit("", "你好", None, true);
    assert!(engine.is_user_dict_dirty());

    engine.flush().unwrap();
    assert!(!engine.is_user_dict_dirty());

    // 重新打开验证
    let engine2 = Engine::new_with_db(path.to_str().unwrap()).unwrap();
    assert!(engine2.user_freq("你好") > 1.0);
}

#[test]
fn test_export_import_user_dict_tsv() {
    let db_path = crate::test_tmp::dir().join("x1pinyin_test_export_import.db");
    let _ = std::fs::remove_file(&db_path);
    let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();

    engine.learn("候选框", "houxuankuang");
    engine.commit("", "候选框", None, true);
    engine.commit("", "候选框", None, true);

    let tsv = engine.export_user_dict_tsv();
    assert!(tsv.contains("候选框"));
    assert!(tsv.contains("hou xuan kuang"));

    // 合并导入：频率累加
    let imported = engine
        .import_user_dict_tsv("候选框\thou xuan kuang\t3\n", true)
        .unwrap();
    assert_eq!(imported, 1);
    assert!(engine.user_freq("候选框") > 1.0);

    // 覆盖导入：只剩新数据
    engine
        .import_user_dict_tsv("自造词\tzi zao ci\t5\n", false)
        .unwrap();
    assert!(engine
        .process("zizaoci")
        .iter()
        .any(|c| c.text.as_ref() == "自造词"));
    assert!(!engine
        .process("houxuankuang")
        .iter()
        .any(|c| c.text.as_ref() == "候选框"));
}

#[test]
fn test_user_dict_skips_single_char() {
    let db_path = crate::test_tmp::dir().join("x1pinyin_test_single_char.db");
    let _ = std::fs::remove_file(&db_path);
    let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();

    // 学习单字应被忽略
    engine.learn("我", "wo");
    assert!(
        !engine
            .process("wo")
            .iter()
            .any(|c| c.text.as_ref() == "我" && c.base_freq > 50000),
        "单字不应以用户词高频插入"
    );

    // commit 单字不应写入 user_words
    engine.commit("", "我", None, true);
    engine.commit("", "我", None, true);
    engine.flush().unwrap();

    let engine2 = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    assert!(
        engine2
            .process("wo")
            .iter()
            .any(|c| c.text.as_ref() == "我"),
        "单字仍应从系统词库出现"
    );
    // 用户词库不应有我
    let words = engine2.get_user_words();
    assert!(!words.iter().any(|(t, _, _)| t == "我"));
}

#[test]
fn test_cleanup_existing_single_char_user_words() {
    use crate::storage::Storage;

    let db_path = crate::test_tmp::dir().join("x1pinyin_test_cleanup_single.db");
    let _ = std::fs::remove_file(&db_path);
    {
        let mut storage = Storage::open(&db_path).unwrap();
        storage
            .upsert_user_freq_batch(&[
                ("我".to_string(), "wo".to_string(), 5u32),
                ("他们".to_string(), "ta men".to_string(), 2u32),
            ])
            .unwrap();
    }

    let engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    let words = engine.get_user_words();
    assert_eq!(words.len(), 1);
    assert_eq!(words[0].0, "他们");
}

#[test]
fn test_system_word_crud() {
    use crate::storage::Storage;

    let db_path = crate::test_tmp::dir().join("x1pinyin_test_system_word.db");
    let _ = std::fs::remove_file(&db_path);
    {
        let mut storage = Storage::open(&db_path).unwrap();
        storage
            .import_x1dict_batch(&[X1DictWord {
                simplified: "中国".into(),
                pinyin: "zhong guo".into(),
                freq: 5000,
                ..Default::default()
            }])
            .unwrap();
    }

    let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    assert!(engine
        .process("zhongguo")
        .iter()
        .any(|c| c.text.as_ref() == "中国"));

    // 添加新系统词
    let word = X1DictWord {
        simplified: "中國人".into(),
        pinyin: "zhong guo ren".into(),
        freq: 3000,
        ..Default::default()
    };
    assert!(engine.add_system_word(&word));
    assert!(engine
        .process("zhongguoren")
        .iter()
        .any(|c| c.text.as_ref() == "中國人"));

    // 更新词频
    let updated = X1DictWord {
        freq: 9999,
        ..word.clone()
    };
    assert!(engine.update_system_word("中國人", "zhong guo ren", &updated));

    // 删除
    assert!(engine.delete_system_word("中国", "zhong guo"));
    assert!(!engine
        .process("zhongguo")
        .iter()
        .any(|c| c.text.as_ref() == "中国"));
}

#[test]
fn test_process_deterministic_order() {
    let engine = Engine::new();
    let r1: Vec<String> = engine
        .process("ni")
        .iter()
        .map(|c| c.text.as_ref().to_owned())
        .collect();
    let r2: Vec<String> = engine
        .process("ni")
        .iter()
        .map(|c| c.text.as_ref().to_owned())
        .collect();
    let r3: Vec<String> = engine
        .process("zhongwen")
        .iter()
        .map(|c| c.text.as_ref().to_owned())
        .collect();
    assert_eq!(r1, r2, "同一输入多次 process 候选顺序应完全一致");
    assert!(!r3.is_empty(), "zhongwen 应返回候选");
}

#[test]
fn test_log_freq_does_not_dominate_system_words() {
    let db_path = crate::test_tmp::dir().join("x1pinyin_test_log_freq.db");
    let _ = std::fs::remove_file(&db_path);
    let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();

    // 学习一个低频自造词很多次
    for _ in 0..1000 {
        engine.commit("", "泥嚎", None, true);
    }
    // 泥嚎是低频组合，即使学了 1000 次，对数模型下也不应压过系统高频词「你好」
    let r = engine.process("nihao");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    if let (Some(nihao_pos), Some(niha_pos)) = (
        texts.iter().position(|&t| t == "你好"),
        texts.iter().position(|&t| t == "泥嚎"),
    ) {
        assert!(
            nihao_pos < niha_pos,
            "系统高频词应排在过度学习的低频自造词前面：{:?}",
            texts
        );
    }
}

#[test]
fn test_import_x1dict_tsv_reloads_trie() {
    use crate::storage::Storage;

    let db_path = crate::test_tmp::dir().join("x1pinyin_test_x1dict_tsv.db");
    let _ = std::fs::remove_file(&db_path);
    Storage::open(&db_path).unwrap();

    let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    engine
        .import_x1dict_tsv(
            "系统词\t系統詞\txi tong ci\txi4 tong3 ci2\tsystem word\t1000\n",
            true,
        )
        .unwrap();
    assert!(engine
        .process("xitongci")
        .iter()
        .any(|c| c.text.as_ref() == "系统词"));
}

#[test]
fn test_user_word_ranks_above_system_word_same_length() {
    use crate::storage::{Storage, X1DictWord};

    let db_path = crate::test_tmp::dir().join("x1pinyin_test_user_rank.db");
    let _ = std::fs::remove_file(&db_path);
    {
        let mut storage = Storage::open(&db_path).unwrap();
        storage
            .import_x1dict_batch(&[X1DictWord {
                simplified: "中国".into(),
                pinyin: "zhong guo".into(),
                freq: 5900,
                ..Default::default()
            }])
            .unwrap();
    }

    let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    engine.learn("中过", "zhong guo");

    let r = engine.process("zhongguo");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("zhongguo => {:?}", texts);
    let user_pos = texts.iter().position(|&t| t == "中过");
    let sys_pos = texts.iter().position(|&t| t == "中国");
    if let (Some(u), Some(s)) = (user_pos, sys_pos) {
        assert!(
            u < s,
            "用户词应排在系统词前面：user={:?}, system={:?}",
            u,
            s
        );
    }
}

#[test]
fn test_user_word_ranks_above_longer_system_phrase() {
    use crate::storage::{Storage, X1DictWord};

    let db_path = crate::test_tmp::dir().join("x1pinyin_test_user_rank2.db");
    let _ = std::fs::remove_file(&db_path);
    {
        let mut storage = Storage::open(&db_path).unwrap();
        storage
            .import_x1dict_batch(&[X1DictWord {
                simplified: "中国人".into(),
                pinyin: "zhong guo ren".into(),
                freq: 2500,
                ..Default::default()
            }])
            .unwrap();
    }

    let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    engine.learn("中国", "zhong guo");

    let r = engine.process("zhongguoren");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("zhongguoren => {:?}", texts);
    let user_pos = texts.iter().position(|&t| t == "中国");
    let sys_pos = texts.iter().position(|&t| t == "中国人");
    if let (Some(u), Some(s)) = (user_pos, sys_pos) {
        // 完整匹配优先：输入 zhongguoren(3音节) 时，「中国人」(3字完整匹配)
        // 应排在「中国」(2字前缀匹配) 前面。用户输入完整拼音，期望得到完整匹配，
        // 前缀短词应让位于完整长词（如 beijingkaoya → 北京烤鸭 优先于 北京）。
        assert!(
            s < u,
            "完整匹配「中国人」应排在前缀「中国」前面：system={:?}, user={:?}",
            s,
            u
        );
    }
}

#[test]
fn test_commit_system_word_stores_pinyin() {
    use std::fs;

    let dir = crate::test_tmp::dir().join("x1pinyin_commit_pinyin_test");
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::create_dir_all(&dir);
    let db_path = dir.join("user.db");

    {
        let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
        engine.commit("", "你好", None, true);
        engine.commit("", "你好", None, true);
        engine.flush().unwrap();
    }

    // 8feb0ea 后系统词不写 user_words，但 user_counts 表应持久化累计选择次数。
    let engine2 = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    let count = engine2.freq_count_for_test("你好");
    assert!(
        count > 1,
        "'你好' 累计选择次数应通过 user_counts 持久化（>1），实际 {}",
        count
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_single_char_user_preference_overrides_commonness() {
    use crate::storage::{Storage, X1DictWord};

    let db_path = crate::test_tmp::dir().join("x1pinyin_test_single_char_pref.db");
    let _ = std::fs::remove_file(&db_path);
    {
        let mut storage = Storage::open(&db_path).unwrap();
        storage
            .import_x1dict_batch(&[
                X1DictWord {
                    simplified: "你".into(),
                    pinyin: "ni".into(),
                    freq: 9000,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "泥".into(),
                    pinyin: "ni".into(),
                    freq: 1000,
                    ..Default::default()
                },
            ])
            .unwrap();
    }

    let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    let r = engine.process("ni");
    let before: Vec<&str> = r.iter().take(2).map(|c| c.text.as_ref()).collect();
    assert_eq!(before[0], "你", "无历史时高频单字应在前");

    // 连续选择"泥"5次，应上升到首位
    // 注意：单字按音隔离后，commit 必须传 pinyin（用户是在 ni 查询下选的泥）
    for _ in 0..5 {
        engine.commit("ni", "泥", Some("ni"), true);
    }
    let r = engine.process("ni");
    let after: Vec<&str> = r.iter().take(2).map(|c| c.text.as_ref()).collect();
    assert_eq!(
        after[0], "泥",
        "多次选择后低频单字应反超高频单字：{:?}",
        after
    );
}

/// 回归测试：sync_x1dict_from_builtin 后内存状态必须立即重建。
/// 缺陷场景：部署更新后 sync 只写 DB 不 reload，运行引擎整个会话用旧词库——
/// 用户 DB 磁盘已含新词「我马上」，但 live 引擎内存 Trie 还是 sync 前的旧词库，
/// womashangdao 出「我妈伤道」（旧词库无「我马上」，DP 只能拼 我妈+伤+道）。
/// sync 全程主线程执行，无后台线程竞争，可安全 reload。
#[test]
fn test_sync_x1dict_reloads_trie_and_seed_bigram() {
    use crate::storage::{Storage, X1DictWord};
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let user_db =
        crate::test_tmp::dir().join(format!("x1_sync_user_{}_{}.db", std::process::id(), ts));
    let builtin_db =
        crate::test_tmp::dir().join(format!("x1_sync_builtin_{}_{}.db", std::process::id(), ts));
    let base_words = || {
        vec![
            X1DictWord {
                simplified: "我".into(),
                pinyin: "wo".into(),
                freq: 9990,
                ..Default::default()
            },
            X1DictWord {
                simplified: "妈".into(),
                pinyin: "ma".into(),
                freq: 5000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "伤".into(),
                pinyin: "shang".into(),
                freq: 1000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "道".into(),
                pinyin: "dao".into(),
                freq: 8840,
                ..Default::default()
            },
            X1DictWord {
                simplified: "马上".into(),
                pinyin: "ma shang".into(),
                freq: 2526,
                ..Default::default()
            },
            X1DictWord {
                simplified: "剑".into(),
                pinyin: "jian".into(),
                freq: 3000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "到".into(),
                pinyin: "dao".into(),
                freq: 9830,
                ..Default::default()
            },
        ]
    };
    {
        // 用户 DB：旧词库状态（无「我马上」，无种子 bigram）
        let mut s = Storage::open(&user_db).unwrap();
        s.import_x1dict_batch(&base_words()).unwrap();
    }
    {
        // 内置 DB：新词库状态（新增「我马上」+ 种子 bigram (我,马上)）
        let mut s = Storage::open(&builtin_db).unwrap();
        let mut words = base_words();
        words.push(X1DictWord {
            simplified: "剑道".into(),
            pinyin: "jian dao".into(),
            freq: 100,
            ..Default::default()
        });
        s.import_x1dict_batch(&words).unwrap();
        s.ensure_seed_bigram_table().unwrap();
        s.insert_seed_bigram_for_test("我", "马上", 20).unwrap();
    }
    let mut engine = Engine::new_with_db(user_db.to_str().unwrap()).unwrap();
    // sync 前：无「剑道」候选（2 音节不产绿词，白词只能来自 Trie），无种子 bigram
    assert!(
        engine
            .process("jiandao")
            .iter()
            .all(|c| c.text.as_ref() != "剑道"),
        "前置条件：旧词库不应有「剑道」"
    );
    assert_eq!(
        engine.bigram_count_for_test("我", "马上"),
        0,
        "前置条件：无种子 bigram"
    );
    // sync
    assert!(engine.sync_x1dict_from_builtin(builtin_db.to_str().unwrap()));
    // sync 后（不重启引擎）：新词 + 种子 bigram 立即可用
    assert!(
        engine
            .process("jiandao")
            .iter()
            .any(|c| c.text.as_ref() == "剑道" && !c.is_sentence),
        "sync 后应立即能查到新词「剑道」（内存 Trie 重建）"
    );
    assert_eq!(
        engine.bigram_count_for_test("我", "马上"),
        20,
        "sync 后种子 bigram 应立即生效（内存表重载）"
    );
    let _ = std::fs::remove_file(&user_db);
    let _ = std::fs::remove_file(&builtin_db);
}

/// sync 的 freq 修正通道：INSERT OR IGNORE 只补缺词不更新已有词
/// freq——存量用户库的 中国=18 类异常值（compute_freq 公式给出过低值）必须经
/// restore_x1dict_freq_from 刷进用户库。守护：sync 后已有词 freq 被种子值
/// 覆盖、排序立即改善、用户数据表不受影响。
#[test]
fn test_sync_updates_existing_word_freq() {
    use crate::storage::{Storage, X1DictWord};
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let user_db =
        crate::test_tmp::dir().join(format!("x1_syncfreq_user_{}_{}.db", std::process::id(), ts));
    let builtin_db = crate::test_tmp::dir().join(format!(
        "x1_syncfreq_builtin_{}_{}.db",
        std::process::id(),
        ts
    ));
    {
        // 用户 DB：旧状态——「中国」freq=18（compute_freq 公式病灶形态）
        let mut s = Storage::open(&user_db).unwrap();
        s.import_x1dict_batch(&[
            X1DictWord {
                simplified: "中".into(),
                pinyin: "zhong".into(),
                freq: 9000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "国".into(),
                pinyin: "guo".into(),
                freq: 8000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "中国".into(),
                pinyin: "zhong guo".into(),
                freq: 18,
                ..Default::default()
            },
        ])
        .unwrap();
        s.import_user_words(&[("我的自造词".into(), "wo de zi zao ci".into(), 5)], true)
            .unwrap();
    }
    {
        // 内置 DB：定标后——「中国」freq=8534
        let mut s = Storage::open(&builtin_db).unwrap();
        s.import_x1dict_batch(&[
            X1DictWord {
                simplified: "中".into(),
                pinyin: "zhong".into(),
                freq: 9000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "国".into(),
                pinyin: "guo".into(),
                freq: 8000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "中国".into(),
                pinyin: "zhong guo".into(),
                freq: 8534,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    let mut engine = Engine::new_with_db(user_db.to_str().unwrap()).unwrap();
    assert!(engine.sync_x1dict_from_builtin(builtin_db.to_str().unwrap()));
    // 已有词 freq 被种子值覆盖（restore 通道）
    {
        use rusqlite::Connection;
        let conn = Connection::open(user_db.to_str().unwrap()).unwrap();
        let freq: i64 = conn
            .query_row(
                "SELECT freq FROM x1dict_words WHERE simplified='中国' AND pinyin='zhong guo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            freq, 8534,
            "sync 应把种子侧 freq 定标刷进已有词（INSERT OR IGNORE 不会）"
        );
        let user_word: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM user_words WHERE text='我的自造词'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(user_word, 1, "restore 只动 x1dict_words，用户词必须保留");
    }
    // 排序立即改善（内存 Trie 重建后）
    let top = engine
        .process("zhongguo")
        .first()
        .map(|c| c.text.to_string());
    assert_eq!(
        top.as_deref(),
        Some("中国"),
        "freq 修正后 zhongguo 首候选应为「中国」"
    );
    let _ = std::fs::remove_file(&user_db);
    let _ = std::fs::remove_file(&builtin_db);
}
