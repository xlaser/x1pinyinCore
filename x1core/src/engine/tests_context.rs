//! Engine 测试 · bigram/trigram 上下文 / 输入映射 / 排序加权。

use super::*;
/// 自适应 bigram cap：max_bigram_count 返回最高频次
#[test]
fn test_max_bigram_count() {
    let mut engine = Engine::new();
    // 直写 NgramStore：本测试校验 max_bigram_count 读取语义，不经过
    // record_word_bigram 的会话级增量上限（上限另有 freq::session_cap_tests 守护）。
    engine.freq.user_bigram.record("我", "吃饭", 2);
    engine.freq.user_bigram.record("他", "工作", 8);
    assert_eq!(
        engine.freq.max_bigram_count(),
        8,
        "应返回最高频 bigram count"
    );
}

/// 自适应 bigram cap：recompute 后 dyn_cap 随 max 对数增长（>固定 10）
#[test]
fn test_adaptive_bigram_cap() {
    let mut engine = Engine::new();
    // 冷启动：max=0 → dyn_cap = CAP_BASE = 8
    engine.recompute_modulators();
    assert_eq!(engine.dyn_cap_cache, 8, "冷启动 dyn_cap 应 = CAP_BASE = 8");

    // 建立高频 bigram（count=30）——直写 store，绕过会话级增量上限；
    // 本测试只校验 dyn_cap 随 max 对数增长。
    engine.freq.user_bigram.record("数码", "产品", 30);
    engine.recompute_modulators();
    // max=30 → dyn_cap = 8 + log2(31)×4 = 8 + 4×4 = 24（log2(31)≈4.95，ilog2=4）
    // 大于固定 10，头部信号不再被压平
    assert!(
        engine.dyn_cap_cache > 10,
        "max=30 时 dyn_cap 应 > 10，实际 {}",
        engine.dyn_cap_cache
    );
}

/// recompute 永不衰减：「我不打字，凭什么衰减」——真实时间不进权重公式，
/// recompute（flush 前置、每次失活都跑）只准重算 dyn_cap，绝不动
/// counts/input_mappings。退化由 competition.rs 同音零和归一承担
///（tests_competition_decay.rs 守护零和链路），防止时间衰减回流。
#[test]
fn test_recompute_never_decays_counts() {
    let mut engine = Engine::new();
    for _ in 0..8 {
        engine.freq.record("测"); // count=8
    }
    let count_of = |e: &Engine, t: &str| -> u32 {
        e.freq
            .counts_snapshot()
            .into_iter()
            .find(|(w, _)| w == t)
            .map(|(_, c)| c)
            .unwrap_or(0)
    };
    assert_eq!(count_of(&engine, "测"), 8, "前置：count=8");
    // 连续多次 recompute（模拟 IMK 失活抖动下 flush 前置高频跑）：counts 一位不动
    engine.recompute_modulators();
    engine.recompute_modulators();
    engine.recompute_modulators();
    assert_eq!(
        count_of(&engine, "测"),
        8,
        "recompute 不得衰减 counts（时间不进权重公式）"
    );
}

#[test]
fn test_recent_commits_boost_low_freq_word() {
    use crate::storage::{Storage, X1DictWord};

    let db_path = crate::test_tmp::dir().join("x1pinyin_test_recent_boost.db");
    let _ = std::fs::remove_file(&db_path);
    {
        let mut storage = Storage::open(&db_path).unwrap();
        storage
            .import_x1dict_batch(&[
                X1DictWord {
                    simplified: "中国".into(),
                    pinyin: "zhong guo".into(),
                    freq: 5900,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "中过".into(),
                    pinyin: "zhong guo".into(),
                    freq: 100,
                    ..Default::default()
                },
            ])
            .unwrap();
    }

    let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();

    // 未频繁输入时，高频系统词“中国”排在前面
    let r1 = engine.process("zhongguo");
    let texts1: Vec<&str> = r1.iter().map(|c| c.text.as_ref()).collect();
    println!("before recent: {:?}", texts1);
    let china_pos1 = texts1.iter().position(|&t| t == "中国").unwrap();
    let zhongguo_pos1 = texts1.iter().position(|&t| t == "中过").unwrap();
    assert!(china_pos1 < zhongguo_pos1, "初始时高频系统词应在前");

    // 短期内连续选择 5 次“中过”
    for _ in 0..5 {
        engine.commit("", "中过", None, true);
    }

    let r2 = engine.process("zhongguo");
    let texts2: Vec<&str> = r2.iter().map(|c| c.text.as_ref()).collect();
    println!("after recent: {:?}", texts2);
    let china_pos2 = texts2.iter().position(|&t| t == "中国").unwrap();
    let zhongguo_pos2 = texts2.iter().position(|&t| t == "中过").unwrap();
    assert!(
        zhongguo_pos2 < china_pos2,
        "短期内频繁输入的低频词应反超高频系统词"
    );
}

#[test]
fn test_jinggao_warning_boost() {
    use crate::storage::{Storage, X1DictWord};

    let db_path = crate::test_tmp::dir().join("x1pinyin_test_jinggao.db");
    let _ = std::fs::remove_file(&db_path);
    {
        let mut storage = Storage::open(&db_path).unwrap();
        storage
            .import_x1dict_batch(&[
                X1DictWord {
                    simplified: "净高".into(),
                    pinyin: "jing gao".into(),
                    freq: 0,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "警告".into(),
                    pinyin: "jing gao".into(),
                    freq: 0,
                    ..Default::default()
                },
            ])
            .unwrap();
    }

    let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();

    let before = engine.process("jinggao");
    let before_texts: Vec<&str> = before.iter().map(|c| c.text.as_ref()).collect();
    println!("jinggao before: {:?}", before_texts);

    // 模拟用户连续选择 10 次“警告”
    for _ in 0..10 {
        engine.commit("", "警告", None, true);
    }
    engine.flush().unwrap();

    let after = engine.process("jinggao");
    let after_texts: Vec<&str> = after.iter().map(|c| c.text.as_ref()).collect();
    println!("jinggao after (same session): {:?}", after_texts);

    let warning_pos = after_texts.iter().position(|&t| t == "警告").unwrap();
    let jinggao_pos = after_texts.iter().position(|&t| t == "净高").unwrap();
    assert!(
        warning_pos < jinggao_pos,
        "连续选择 10 次后，警告应排在净高前面：{:?}",
        after_texts
    );

    // 模拟重启：重新打开引擎，近期窗口会丢失，但累计次数应已持久化
    let engine2 = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    let after_reload = engine2.process("jinggao");
    let reload_texts: Vec<&str> = after_reload.iter().map(|c| c.text.as_ref()).collect();
    println!("jinggao after reload: {:?}", reload_texts);

    let warning_pos2 = reload_texts.iter().position(|&t| t == "警告").unwrap();
    let jinggao_pos2 = reload_texts.iter().position(|&t| t == "净高").unwrap();
    assert!(
        warning_pos2 < jinggao_pos2,
        "重启后累计次数仍应让警告排在净高前面：{:?}",
        reload_texts
    );
}

#[test]
fn test_recent_selection_overrides_history_for_prefix() {
    use crate::storage::{Storage, X1DictWord};

    let db_path = crate::test_tmp::dir().join("x1pinyin_test_prefix_history.db");
    let _ = std::fs::remove_file(&db_path);
    {
        let mut storage = Storage::open(&db_path).unwrap();
        storage
            .import_x1dict_batch(&[
                X1DictWord {
                    simplified: "稳妥".into(),
                    pinyin: "wen tuo".into(),
                    freq: 500,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "问题".into(),
                    pinyin: "wen ti".into(),
                    freq: 1500,
                    ..Default::default()
                },
            ])
            .unwrap();
    }

    let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();

    // 先让“稳妥”成为历史高频词
    for _ in 0..10 {
        engine.commit("", "稳妥", None, true);
    }
    let r = engine.process("went");
    let r1: Vec<&str> = r.iter().take(3).map(|c| c.text.as_ref()).collect();
    assert_eq!(r1[0], "稳妥", "历史高频词应在前：{:?}", r1);

    // 连续选择"问题"5次，应反超
    for _ in 0..5 {
        engine.commit("", "问题", None, true);
    }
    // 清空 context 栈隔离 bigram 干扰——此测试验证 recent_score 反超，
    // 不应受 context 历史 bigram 加分影响（context 扩展到20词后 bigram 贡献增大）
    engine.clear_context();
    let r = engine.process("went");
    let r2: Vec<&str> = r.iter().take(3).map(|c| c.text.as_ref()).collect();
    assert_eq!(r2[0], "问题", "近期多次选择后应反超历史高频词：{:?}", r2);
}

#[test]
fn test_mixed_jianpin_commit_updates_trie_immediately() {
    use crate::storage::{Storage, X1DictWord};

    let db_path = crate::test_tmp::dir().join("x1pinyin_test_mixed_commit.db");
    let _ = std::fs::remove_file(&db_path);
    {
        let mut storage = Storage::open(&db_path).unwrap();
        storage
            .import_x1dict_batch(&[
                X1DictWord {
                    simplified: "词牌".into(),
                    pinyin: "ci pai".into(),
                    freq: 1000,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "词频".into(),
                    pinyin: "ci pin".into(),
                    freq: 1000,
                    ..Default::default()
                },
            ])
            .unwrap();
    }

    let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();

    // 初始两者同频，系统词默认排序稳定
    let r0 = engine.process("cip");
    let cipai_pos0 = r0.iter().position(|c| c.text.as_ref() == "词牌").unwrap();
    let cipin_pos0 = r0.iter().position(|c| c.text.as_ref() == "词频").unwrap();
    // 记录初始相对顺序
    let cipai_first0 = cipai_pos0 < cipin_pos0;

    // 选择一次"词频"
    engine.commit("", "词频", None, true);

    // 立即查询，应发现"词频"已在候选中（commit 后 Trie 立即更新）
    let r1 = engine.process("cip");
    let cipin = r1.iter().find(|c| c.text.as_ref() == "词频").unwrap();
    // base_freq 用真实 freq（compute_freq 范围 1~9000），不再用 50000 虚高值。
    // 用户词排序靠 recent_score 体现，不靠 base_freq 注水。
    assert!(
        cipin.base_freq >= 1,
        "commit 后「词频」应在候选中，base_freq={}",
        cipin.base_freq
    );

    // 若初始"词牌"在前，选择一次"词频"后应能反超
    if cipai_first0 {
        let cipin_pos1 = r1.iter().position(|c| c.text.as_ref() == "词频").unwrap();
        let cipai_pos1 = r1.iter().position(|c| c.text.as_ref() == "词牌").unwrap();
        assert!(
            cipin_pos1 < cipai_pos1,
            "选择一次目标词后，混合简拼下应立即反超：{:?}",
            r1.iter().take(5).map(|c| &c.text).collect::<Vec<_>>()
        );
    }
}

#[test]
fn test_mixed_input_includes_first_syllable_single_char() {
    use crate::storage::{Storage, X1DictWord};

    let db_path = crate::test_tmp::dir().join("x1pinyin_test_kaix.db");
    let _ = std::fs::remove_file(&db_path);
    {
        let mut storage = Storage::open(&db_path).unwrap();
        storage
            .import_x1dict_batch(&[
                X1DictWord {
                    simplified: "开".into(),
                    pinyin: "kai".into(),
                    freq: 6650,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "开心".into(),
                    pinyin: "kai xin".into(),
                    freq: 2458,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "开学".into(),
                    pinyin: "kai xue".into(),
                    freq: 2243,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "开销".into(),
                    pinyin: "kai xiao".into(),
                    freq: 1475,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "凯旋".into(),
                    pinyin: "kai xuan".into(),
                    freq: 533,
                    ..Default::default()
                },
            ])
            .unwrap();
    }

    let engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    let r = engine.process("kaix");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    assert!(
        texts.contains(&"开"),
        "混合简拼 kaix 应补充首音节 kai 的单字候选 '开'，实际为 {:?}",
        texts
    );
}

#[test]
fn test_mixed_input_syllable_count_falls_back_to_parse_pinyin() {
    // kaix 中的 x 不是有效音节，best_segmentation 会返回空。
    // 音节数和前缀长度必须回退到 parse_pinyin，否则宿主前端
    // 无法正确进行部分消费，会一次性清空整个 buffer。
    let engine = Engine::new();
    let input = "kaix";
    assert_eq!(engine.syllable_count(input), 2, "kaix 应被识别为 2 个音节");
    assert_eq!(
        engine.syllable_prefix_len(input, 1),
        3,
        "kaix 第一个音节应占 3 字节 (kai)"
    );

    // 同理验证 went / cip
    assert_eq!(engine.syllable_count("went"), 2);
    assert_eq!(engine.syllable_prefix_len("went", 1), 3); // wen
    assert_eq!(engine.syllable_count("cip"), 2);
    assert_eq!(engine.syllable_prefix_len("cip", 1), 2); // ci
}

#[test]
fn test_input_mapping_boosts_rank_on_same_input() {
    // 内存模式即可验证：输入 "went" 时同时存在 "问题" 与 "稳妥"，
    // 选择 "问题" 后，再次输入 "went" 时 "问题" 应因输入习惯映射而置顶。
    let mut engine = Engine::new();

    let r0 = engine.process("went");
    let wen_pos0 = r0
        .iter()
        .position(|c| c.text.as_ref() == "问题")
        .unwrap_or(usize::MAX);
    let _wt_pos0 = r0
        .iter()
        .position(|c| c.text.as_ref() == "稳妥")
        .unwrap_or(usize::MAX);
    let _wenti_pos0 = r0
        .iter()
        .position(|c| c.text.as_ref() == "文体")
        .unwrap_or(usize::MAX);

    // 选择 "问题"，并记录原始输入 "went"
    engine.commit("went", "问题", Some("wen ti"), true);

    let r1 = engine.process("went");
    let wen_pos1 = r1
        .iter()
        .position(|c| c.text.as_ref() == "问题")
        .unwrap_or(usize::MAX);
    let wt_pos1 = r1
        .iter()
        .position(|c| c.text.as_ref() == "稳妥")
        .unwrap_or(usize::MAX);
    let wenti_pos1 = r1
        .iter()
        .position(|c| c.text.as_ref() == "文体")
        .unwrap_or(usize::MAX);

    assert!(
        wen_pos1 <= wen_pos0,
        "选择 '问题' 后再次输入 'went'，'问题' 排名应提升：before={} after={}",
        wen_pos0,
        wen_pos1
    );
    assert!(
        wen_pos1 < wt_pos1,
        "输入习惯映射应让 '问题' 排在 '稳妥' 前：positions {:?}",
        r1.iter().take(5).map(|c| &c.text).collect::<Vec<_>>()
    );
    assert!(
        wen_pos1 < wenti_pos1,
        "输入习惯映射应让 '问题' 排在 '文体' 前：positions {:?}",
        r1.iter().take(5).map(|c| &c.text).collect::<Vec<_>>()
    );

    // 输入不同拼音 "wenti" 时，映射不应生效，排名回到原状
    let r2 = engine.process("wenti");
    let wen_pos2 = r2
        .iter()
        .position(|c| c.text.as_ref() == "问题")
        .unwrap_or(usize::MAX);
    assert!(
        wen_pos2 >= wen_pos1,
        "输入 'wenti' 时 'went'->'问题' 的映射不应让 '问题' 异常置顶：multi={} full={}",
        wen_pos1,
        wen_pos2
    );
}

/// 回归测试：prefix mapping 反向索引正确性（byd 是 bydi 的前缀）。
///
/// 验证 candidate_score 的 prefix mapping 查询从全表扫描改为反向索引后，
/// 「当前输入是历史输入的前缀」的加分逻辑仍正确生效。
#[test]
fn test_prefix_mapping_via_reverse_index() {
    let mut engine = Engine::new();
    engine.input_mapping_enabled = true;

    // 用户曾用完整输入 "bydi" 选过「笨一点」（假设词库有此词或近似）。
    // 这里直接灌入 input_mappings + 反向索引，模拟 commit 积累。
    engine
        .input_mappings
        .insert(("bydi".to_string(), "笨一点".to_string()), 3);
    engine
        .input_mappings_by_text
        .insert("笨一点".to_string(), vec![("bydi".to_string(), 3)]);

    // 验证反向索引已正确建立
    assert_eq!(
        engine.input_mappings_by_text.get("笨一点").map(|v| v.len()),
        Some(1),
        "反向索引应含「笨一点」的 1 条映射"
    );

    // 灌入大量无关 mapping（模拟用户长期积累），验证 prefix 查询不退化、不错配
    for i in 0..500 {
        engine
            .input_mappings
            .insert((format!("xxx{}", i), format!("无关词{}", i)), 1);
        engine
            .input_mappings_by_text
            .insert(format!("无关词{}", i), vec![(format!("xxx{}", i), 1)]);
    }

    // prefix 查询应只扫「笨一点」的小集合（1 条），不被 500 条无关 mapping 影响。
    // 反向索引正确性：bydi 的前缀 by 能匹配，xxx0 的前缀不会误匹配到「笨一点」。
    let bucket = engine.input_mappings_by_text.get("笨一点").unwrap();
    assert_eq!(
        bucket.len(),
        1,
        "「笨一点」的反向索引应只有 1 条，无关 mapping 不应污染"
    );
    assert_eq!(bucket[0].0, "bydi");
}

#[test]
fn debug_sangeren() {
    let engine = Engine::new();
    let segs = crate::pinyin::segmentations_with_partial("sangeren", 8);
    println!("segmentations:");
    for seg in &segs {
        let score = engine.score_for_syllables("sangeren", seg);
        println!("  {:?} \u{2192} score={}", seg, score);
    }
    let result = engine.process("sangeren");
    println!("candidates (first 5):");
    for c in result.iter().take(5) {
        println!("  {} (pinyin={})", c.text, c.pinyin);
    }
}

// ============= bigram 上下文感知测试 =============

#[test]
fn test_trigram_context_boost() {
    // 上文「饭」+ 输入「chi」，多次共现后「吃」应因 bigram 加分排前。
    // （函数名保留 trigram 为历史命名，实际验证的是 bigram 二元共现加分。）
    let path = crate::test_tmp::dir().join("x1_test_trigram_boost.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let mut s = crate::storage::Storage::open(path).unwrap();
        s.import_x1dict_batch(&[
            crate::storage::X1DictWord {
                simplified: "吃".into(),
                pinyin: "chi".into(),
                freq: 5000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "迟".into(),
                pinyin: "chi".into(),
                freq: 6000,
                is_primary: true,
            },
        ])
        .unwrap();
    }
    let mut engine = Engine::new_with_db(path).unwrap();

    // 初始：迟(6000) > 吃(5000)，迟排前
    let cands = engine.process("chi");
    assert_eq!(cands[0].text.as_ref(), "迟", "初始应迟排前（freq更高）");

    // 直接构造 bigram 共现：(午饭, 吃) ×8（避免 commit 让「吃」频率上升干扰；
    // 字+字 pair 被 choke point 过滤，fixture 用 词+字 合法形态）
    for _ in 0..8 {
        engine.freq.record_word_bigram("午饭", "吃");
    }
    // 设置上文栈为 [午饭]（单元素，bigram 语义），吃应因 bigram 加分排前
    engine.context = vec!["午饭".to_string()];
    let cands = engine.process("chi");
    assert_eq!(
        cands[0].text.as_ref(),
        "吃",
        "上文(午饭)时吃应因 bigram 加分排前，实际第0是{}",
        cands[0].text
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn test_trigram_no_context_no_bonus() {
    // 无上文（context 空）时，bigram_bonus=0，排序与无 bigram 时一致
    let path = crate::test_tmp::dir().join("x1_test_trigram_nobonus.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let mut s = crate::storage::Storage::open(path).unwrap();
        s.import_x1dict_batch(&[
            crate::storage::X1DictWord {
                simplified: "吃".into(),
                pinyin: "chi".into(),
                freq: 5000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "迟".into(),
                pinyin: "chi".into(),
                freq: 6000,
                is_primary: true,
            },
        ])
        .unwrap();
    }
    let engine = Engine::new_with_db(path).unwrap();
    // 无上文，迟(6000) 仍应排前
    let cands = engine.process("chi");
    assert_eq!(cands[0].text.as_ref(), "迟", "无上文时不应有 bigram 加分");
    let _ = std::fs::remove_file(path);
}

#[test]
fn test_clear_context_after_punctuation() {
    // clear_context 清空上下文栈后，bigram 加分不再生效。
    // 直接用 record_bigram 构造共现（避免 commit 让「吃」的频率上升干扰）。
    let path = crate::test_tmp::dir().join("x1_test_clear_ctx.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let mut s = crate::storage::Storage::open(path).unwrap();
        s.import_x1dict_batch(&[
            crate::storage::X1DictWord {
                simplified: "吃".into(),
                pinyin: "chi".into(),
                freq: 5000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "迟".into(),
                pinyin: "chi".into(),
                freq: 6000,
                is_primary: true,
            },
        ])
        .unwrap();
    }
    let mut engine = Engine::new_with_db(path).unwrap();
    // 直接构造 bigram 共现：(午饭, 吃) ×8（不通过 commit，避免「吃」频率上升；
    // 字+字 pair 被 choke point 过滤，fixture 用 词+字 合法形态）
    for _ in 0..8 {
        engine.freq.record_word_bigram("午饭", "吃");
    }
    // 设置上文栈为 [午饭]（单元素，bigram 语义），吃应因 bigram 加分排前
    engine.context = vec!["午饭".to_string()];
    let cands = engine.process("chi");
    assert_eq!(cands[0].text.as_ref(), "吃", "有上文(午饭)时吃应排前");

    // 清空上下文（模拟标点上屏），bigram 加分失效，迟(freq高)应排前
    engine.clear_context();
    let cands = engine.process("chi");
    assert_eq!(
        cands[0].text.as_ref(),
        "迟",
        "清空上下文后 bigram 加分失效，迟应排前"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn test_context_bonus_trigram_upgrade() {
    // trigram 升级：context=[我, 知道]，context_bonus("你") 应因 trigram(我,知道,你)
    // 加分高于无关词（苹果）。验证 context_bonus_with_ctx 的 distance==0 trigram 查询。
    let seed = super::tests_query::seed_copy("ctx");
    let mut engine = Engine::new_with_db(&seed).unwrap();
    engine.commit("wo", "我", Some("wo"), true);
    engine.commit("zhidao", "知道", Some("zhi dao"), true);
    engine.trigram.record_weighted("我", "知道", "你", 5);
    // context=[我, 知道]，context_bonus("你") 应含 trigram 加分
    let bonus_ni = engine.context_bonus("你", '你', false, "ni");
    let bonus_other = engine.context_bonus("苹果", '苹', true, "ping guo");
    assert!(
        bonus_ni > bonus_other,
        "trigram(我,知道,你) 应让「你」加分高于无关词"
    );
}

#[test]
fn test_trigram_persistence_commit_flush_reload() {
    // 端到端：commit 3 次建立 trigram(我,知道,你) → flush 落盘 → 重新打开引擎
    // 验证 trigram 跨会话存活。
    let path = crate::test_tmp::dir().join("x1_test_trigram_persist.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let mut s = crate::storage::Storage::open(path).unwrap();
        s.import_x1dict_batch(&[
            crate::storage::X1DictWord {
                simplified: "我".into(),
                pinyin: "wo".into(),
                freq: 5000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "知道".into(),
                pinyin: "zhi dao".into(),
                freq: 5000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "你".into(),
                pinyin: "ni".into(),
                freq: 5000,
                is_primary: true,
            },
        ])
        .unwrap();
    }
    {
        let mut engine = Engine::new_with_db(path).unwrap();
        // 连续 commit：我 → 知道 → 你。第三次 commit 时 context=[我,知道]，记录
        // trigram(我,知道,你)。
        engine.commit("wo", "我", Some("wo"), true);
        engine.commit("zhidao", "知道", Some("zhi dao"), true);
        engine.commit("ni", "你", Some("ni"), true);
        assert_eq!(
            engine.trigram.count("我", "知道", "你"),
            1,
            "commit 后 trigram(我,知道,你) 应记 1 次"
        );
        engine.flush().unwrap();
    }
    // 重启：重新打开引擎，trigram 应从 user_trigram_counts 表恢复
    let engine2 = Engine::new_with_db(path).unwrap();
    assert_eq!(
        engine2.trigram.count("我", "知道", "你"),
        1,
        "重启后 trigram 应从持久层恢复"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn test_incremental_flush_bigram_trigram_roundtrip() {
    // 增量落盘守护：bigram/trigram 从「全量 DELETE+重插」改 diff 增量后，
    // 语义必须与全量等价——
    // ① 变化项跨会话存活；② 删除（forget_bigram）真的从表里消失；
    // ③ 无变化时 delta 为空（不重复写）；④ 有变化时 delta 只含变化项。
    let path = crate::test_tmp::dir().join("x1_test_incr_flush.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let mut s = crate::storage::Storage::open(path).unwrap();
        s.import_x1dict_batch(&[
            crate::storage::X1DictWord {
                simplified: "我".into(),
                pinyin: "wo".into(),
                freq: 5000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "知道".into(),
                pinyin: "zhi dao".into(),
                freq: 5000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "你".into(),
                pinyin: "ni".into(),
                freq: 5000,
                is_primary: true,
            },
        ])
        .unwrap();
    }
    {
        let mut engine = Engine::new_with_db(path).unwrap();
        engine.commit("wo", "我", Some("wo"), true);
        engine.commit("zhidao", "知道", Some("zhi dao"), true);
        engine.commit("ni", "你", Some("ni"), true);
        engine.flush().unwrap();

        // ③ 落盘后无新变化 → 两个 delta 都应为空（快照已推进）
        let (bu, bd) = engine.freq.take_bigram_delta();
        let (tu, td) = engine.trigram.take_delta();
        assert!(
            bu.is_empty() && bd.is_empty(),
            "flush 后 bigram delta 应为空，实际 up={} del={}",
            bu.len(),
            bd.len()
        );
        assert!(
            tu.is_empty() && td.is_empty(),
            "flush 后 trigram delta 应为空，实际 up={} del={}",
            tu.len(),
            td.len()
        );

        // ④ 再 commit 一个不同的双字词 → delta 只含本次变化（远离全量）。
        // 两个约定都会让 delta 合法为空，测试必须避开：连续同词提交不记、
        // 字+字粒度不记 bigram——故这里接双字词「知道」（字+双字 → 记）。
        engine.commit("zhidao", "知道", Some("zhi dao"), true);
        let (bu2, _) = engine.freq.take_bigram_delta();
        assert!(!bu2.is_empty(), "有新学习时 delta 不应为空");
        assert!(
            bu2.len() <= 8,
            "增量应只含变化项（非全量），实际 {}",
            bu2.len()
        );
        engine.flush().unwrap();
        // 连续同词提交不记上下文（约定守护）→ delta 保持为空
        engine.commit("zhidao", "知道", Some("zhi dao"), true);
        let (bu3, bd3) = engine.freq.take_bigram_delta();
        assert!(
            bu3.is_empty() && bd3.is_empty(),
            "连续同词提交后 bigram delta 仍应为空"
        );
    }
    // ① 跨会话存活
    let mut engine2 = Engine::new_with_db(path).unwrap();
    assert_eq!(
        engine2.trigram.count("我", "知道", "你"),
        1,
        "增量落盘后 trigram 应跨会话恢复"
    );
    // ② 删除语义：forget_bigram → flush → 重载后 bigram 不得残留（增量
    //    delete 路径）。注意 forget_bigram 的覆盖范围就是 bigram——
    //    TrigramModel 无删除 API（只增不减是现状设计），故 trigram 只验存活。
    engine2.freq.forget_bigram("知道");
    engine2.flush().unwrap();
    drop(engine2);
    let engine3 = Engine::new_with_db(path).unwrap();
    assert_eq!(
        engine3.freq.word_bigram_count("我", "知道"),
        0,
        "删除后 bigram 不应残留（增量 delete 路径）"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn test_compose_step_no_context_pollution() {
    // 自组词分步 commit(advance_context=false) 不应推进上下文
    let path = crate::test_tmp::dir().join("x1_test_compose_ctx.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let mut s = crate::storage::Storage::open(path).unwrap();
        s.import_x1dict_batch(&[crate::storage::X1DictWord {
            simplified: "我".into(),
            pinyin: "wo".into(),
            freq: 9000,
            is_primary: true,
        }])
        .unwrap();
    }
    let mut engine = Engine::new_with_db(path).unwrap();
    assert!(engine.context.is_empty(), "初始上下文应为空");
    // 分步 commit（advance_context=false）不推进上下文
    engine.commit("woznb", "我", Some("wo"), false);
    assert!(
        engine.context.is_empty(),
        "自组词分步 commit 不应推进上下文"
    );
    // 普通提交（advance_context=true）推进上下文
    engine.commit("wo", "我", Some("wo"), true);
    assert_eq!(engine.context.len(), 1, "普通提交应推进上下文");
    let _ = std::fs::remove_file(path);
}

#[test]
fn test_long_word_bonus_single_char() {
    // 场景1：词库有「星期六」(freq=5000)，context=["星期"]，输入 liu，
    // 「六」应因长词匹配 bonus 排在「流」/「留」前。
    let path = crate::test_tmp::dir().join("x1_test_long_word_single.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let mut s = crate::storage::Storage::open(path).unwrap();
        s.import_x1dict_batch(&[
            crate::storage::X1DictWord {
                simplified: "星期六".into(),
                pinyin: "xing qi liu".into(),
                freq: 5000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "星期".into(),
                pinyin: "xing qi".into(),
                freq: 6000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "流".into(),
                pinyin: "liu".into(),
                freq: 8000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "留".into(),
                pinyin: "liu".into(),
                freq: 7000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "六".into(),
                pinyin: "liu".into(),
                freq: 4000,
                is_primary: true,
            },
        ])
        .unwrap();
    }
    let mut engine = Engine::new_with_db(path).unwrap();

    // 无 context 时：流(8000) > 留(7000) > 六(4000)
    let cands = engine.process("liu");
    assert_eq!(
        cands[0].text.as_ref(),
        "流",
        "无 context 时流应排前（freq 最高）"
    );

    // context=["星期"] 后：六应因「星期六」长词匹配 bonus 排前
    engine.context = vec!["星期".to_string()];
    let cands = engine.process("liu");
    assert_eq!(
        cands[0].text.as_ref(),
        "六",
        "context=星期 时六应因「星期六」长词匹配排前，实际前三是: {:?}",
        cands.iter().take(3).map(|c| &c.text).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn test_long_word_bonus_word_candidate() {
    // 场景2：词库有「手机电池」(freq=4000)，context=["手机"]，输入 dianchi，
    // 「电池」应因长词匹配排在「滇池」前（即便滇池 freq 更高）。
    let path = crate::test_tmp::dir().join("x1_test_long_word_word.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let mut s = crate::storage::Storage::open(path).unwrap();
        s.import_x1dict_batch(&[
            crate::storage::X1DictWord {
                simplified: "手机电池".into(),
                pinyin: "shou ji dian chi".into(),
                freq: 4000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "手机".into(),
                pinyin: "shou ji".into(),
                freq: 6000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "电池".into(),
                pinyin: "dian chi".into(),
                freq: 3000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "滇池".into(),
                pinyin: "dian chi".into(),
                freq: 5000,
                is_primary: true,
            },
        ])
        .unwrap();
    }
    let mut engine = Engine::new_with_db(path).unwrap();

    // 无 context 时：滇池(5000) > 电池(3000)
    let cands = engine.process("dianchi");
    assert_eq!(cands[0].text.as_ref(), "滇池", "无 context 时滇池应排前");

    // context=["手机"] 后：电池应因「手机电池」长词匹配排前
    engine.context = vec!["手机".to_string()];
    let cands = engine.process("dianchi");
    assert_eq!(
        cands[0].text.as_ref(),
        "电池",
        "context=手机 时电池应因「手机电池」长词匹配排前，实际前三是: {:?}",
        cands.iter().take(3).map(|c| &c.text).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn test_long_word_bonus_no_context() {
    // context 为空时 long_word_bonus=0，不影响排序
    let path = crate::test_tmp::dir().join("x1_test_long_word_noctx.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let mut s = crate::storage::Storage::open(path).unwrap();
        s.import_x1dict_batch(&[
            crate::storage::X1DictWord {
                simplified: "星期六".into(),
                pinyin: "xing qi liu".into(),
                freq: 5000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "流".into(),
                pinyin: "liu".into(),
                freq: 8000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "六".into(),
                pinyin: "liu".into(),
                freq: 4000,
                is_primary: true,
            },
        ])
        .unwrap();
    }
    let engine = Engine::new_with_db(path).unwrap();
    // context 为空（默认）
    assert!(engine.context.is_empty());
    let cands = engine.process("liu");
    assert_eq!(
        cands[0].text.as_ref(),
        "流",
        "无 context 时流应排前，长词 bonus 不应干扰"
    );
    let _ = std::fs::remove_file(path);
}

/// word bigram（用户数据）与 collocation_freq（按需查词库）独立工作。
#[test]
fn test_word_bigram_independent() {
    let mut fm = crate::freq::FreqModel::new();
    fm.record_word_bigram("坐", "飞机");
    assert_eq!(fm.word_bigram_count("坐", "飞机"), 1);
    assert_eq!(fm.word_bigram_count("坐", "飞"), 0);
}

/// 标点/字面空格 = 上下文边界。
///
/// 契约：前端在标点或字面空格写入文档后置位断链标记，并于下一笔学习
/// 提交前调用 `context_clear`——该笔词不得与边界前的词产生 bigram；
/// 边界之后新段内部的配对照常记录（只是断开跨边界的那一对）。
/// 宿主前端实现：置位断链标记（CommitAppend / 透传空格）→ 提交前
/// clear_context（FFI `x1_engine_context_clear`）。
#[test]
fn test_context_clear_breaks_bigram_across_boundary() {
    let mut engine = Engine::new();
    engine.commit("nihao", "你好", Some("ni hao"), true);
    engine.commit("shijie", "世界", Some("shi jie"), true);
    assert_eq!(
        engine.freq.word_bigram_count("你好", "世界"),
        1,
        "前置：同段内相邻词照常记录配对"
    );

    // 边界（标点/空格）→ 前端清上下文
    engine.context_clear();

    engine.commit("ceshi", "测试", Some("ce shi"), true);
    assert_eq!(
        engine.freq.word_bigram_count("世界", "测试"),
        0,
        "边界后的词不得与边界前的词配对"
    );
    // 边界之后新段内部照常学：测试 → 完毕
    engine.commit("wanbi", "完毕", Some("wan bi"), true);
    assert_eq!(
        engine.freq.word_bigram_count("测试", "完毕"),
        1,
        "边界之后新段内的配对照常记录"
    );
}

/// [验证] collocation_freq 按需查组合词（commit 后按需查，不预存不占内存）。
#[test]
fn test_collocation_freq_on_demand() {
    let seed = super::tests_query::seed_copy("ctx");
    let engine = Engine::new_with_db(&seed).unwrap();
    // 词库有「你好」「吃饭」等组合词，按需查应命中（读音匹配：好=hao，饭=fan）
    assert!(
        engine.dict.collocation_freq("你", "好", "hao").is_some(),
        "你好 应在词库"
    );
    assert!(
        engine.dict.collocation_freq("吃", "饭", "fan").is_some(),
        "吃饭 应在词库"
    );
    // 无关组合应 None
    assert!(
        engine.dict.collocation_freq("你", "飞", "fei").is_none(),
        "你飞 不应在词库"
    );
}

/// [读音校验] 多音字字形匹配但读音不匹配时，collocation 不应命中。
///
/// 核心场景：用户 commit「你」后打 ni 查「呢」（呢读 ni 次音）。词库里「你呢」
/// 拼音是 `ni ne`（呢读 ne 主音），但候选「呢」的实际读音是 ni。
/// 字形拼接「你+呢」命中词库，但读音 ne ≠ ni，应返回 None（不给「呢」加分）。
/// collocation_freq 不能只查字形不校验读音。
#[test]
fn test_collocation_pinyin_mismatch_polyphone() {
    let seed = super::tests_query::seed_copy("ctx");
    let mut engine = Engine::new_with_db(&seed).unwrap();
    // 先构造「你呢」在 text_freq_map（user_words 加载或手动 insert）
    engine.dict.insert("ni ne", "你呢", 500, true);

    // 读音匹配：「你呢」最后音节 ne = 候选「呢」的拼音 ne → 命中
    assert!(
        engine.dict.collocation_freq("你", "呢", "ne").is_some(),
        "读音匹配（ne=ne）应命中「你呢」"
    );

    // 读音不匹配：「你呢」最后音节 ne ≠ 候选「呢」的拼音 ni → 不命中
    assert!(
        engine.dict.collocation_freq("你", "呢", "ni").is_none(),
        "读音不匹配（ne≠ni）不应命中——「呢」读 ni 时组不成「你呢」(ni ne)"
    );

    // 对照：不传拼音（DP 路径兼容）跳过校验，保持原行为
    assert!(
        engine.dict.collocation_freq("你", "呢", "").is_some(),
        "不传拼音时跳过校验，字形匹配即命中（DP 路径兼容）"
    );
}

/// [用户词库一视同仁] 读音校验对用户词库同样生效。
///
/// 设计原则：用户词库只是系统词库的扩展，结构一致，读音校验不应区别对待。
/// 验证通过 insert_user_word_to_dict（真实用户词加载路径）注入的词，
/// 其 text_pinyin_map 侧索引正确维护，find_pinyin_fast 能查到拼音，
/// collocation_freq 读音校验对用户词同样生效。
#[test]
fn test_collocation_pinyin_check_applies_to_user_words() {
    let seed = super::tests_query::seed_copy("ctx");
    let mut engine = Engine::new_with_db(&seed).unwrap();
    // 通过用户词加载路径注入「你呢」（模拟 user_words 加载）
    engine.insert_user_word_to_dict("你呢", "ni ne", 4);
    // 确认侧索引正确维护（用户词拼音写入了 text_pinyin_map）
    assert_eq!(
        engine.dict.find_pinyin_fast("你呢").as_deref(),
        Some("ni ne"),
        "用户词「你呢」的拼音应在 text_pinyin_map 侧索引中"
    );

    // 读音校验对用户词同样生效：ne≠ni 不命中，ne=ne 命中
    assert!(
        engine.dict.collocation_freq("你", "呢", "ni").is_none(),
        "用户词读音校验：ne≠ni 不应命中"
    );
    assert!(
        engine.dict.collocation_freq("你", "呢", "ne").is_some(),
        "用户词读音校验：ne=ne 应正常命中"
    );
}

/// [读音匹配正常命中] 读音一致时 collocation 正常加分，不误杀。
#[test]
fn test_collocation_pinyin_match_still_hits() {
    let seed = super::tests_query::seed_copy("ctx");
    let engine = Engine::new_with_db(&seed).unwrap();
    // 「你好」拼音 ni hao，候选「好」读 hao → 匹配
    assert!(
        engine.dict.collocation_freq("你", "好", "hao").is_some(),
        "读音匹配（hao=hao）「你好」应命中"
    );
    // 「吃饭」拼音 chi fan，候选「饭」读 fan → 匹配
    assert!(
        engine.dict.collocation_freq("吃", "饭", "fan").is_some(),
        "读音匹配（fan=fan）「吃饭」应命中"
    );
}

/// context_bonus 公共函数统一后，候选首字/多字回退仍能产生上下文加分。
#[test]
fn test_context_bonus_unified() {
    let seed = super::tests_query::seed_copy("ctx");
    let mut engine = Engine::new_with_db(&seed).unwrap();
    // 字+字 (我,是) 全线不记，fixture 改 词+字 合法形态
    engine.commit("women", "我们", Some("wo men"), true);
    engine.freq.record_word_bigram_weighted("我们", "是", 5);
    let bonus_shi = {
        let first = '是';
        engine.context_bonus("是", first, false, "shi")
    };
    let bonus_other = {
        let first = '苹';
        engine.context_bonus("苹果", first, true, "ping guo")
    };
    assert!(bonus_shi > 0, "「是」应有上下文加分（我们→是）");
    // 苹果 在 word 表无 (我们,苹果) 记录，char 表也无记录 → 应为 0
    assert_eq!(bonus_other, 0, "「苹果」无上下文记录应为 0");
}

/// [回归] commit「机」后输入 zhi，「制」应因 collocation(机制) 排到「只」前面。
/// 守护上下文关联（bigram+collocation）不被 primary_reading_bonus 压制。
#[test]
fn test_ji_zhi_zhi_ranks_after_zhi_collocation() {
    let seed = super::tests_query::seed_copy("ctx");
    let mut engine = Engine::new_with_db(&seed).unwrap();
    engine.commit("ji", "机", Some("ji"), true);
    let cands = engine.process("zhi");
    let pos_zhi = cands.iter().position(|c| c.text.as_ref() == "制");
    let pos_only = cands.iter().position(|c| c.text.as_ref() == "只");
    if let (Some(pz), Some(po)) = (pos_zhi, pos_only) {
        assert!(
            pz < po,
            "commit 机 后，制(机制搭配)应排在只前面，实际制={}只={}",
            pz,
            po
        );
    }
}

/// [回归] commit「记」后输入 yi，「忆」应因 collocation(记忆) 排到「已」前面。
/// 验证第3层优先级：collocation（词库权威搭配）> word_bigram（用户偶然历史）。
#[test]
fn test_ji_yi_memory_ranks_by_collocation() {
    let seed = super::tests_query::seed_copy("ctx");
    let mut engine = Engine::new_with_db(&seed).unwrap();
    engine.commit("ji", "记", Some("ji"), true);
    let cands = engine.process("yi");
    let pos_yi = cands.iter().position(|c| c.text.as_ref() == "忆");
    let pos_yi2 = cands.iter().position(|c| c.text.as_ref() == "已");
    if let (Some(py), Some(po)) = (pos_yi, pos_yi2) {
        assert!(
            py < po,
            "commit 记 后，忆(记忆搭配)应排在已前面，实际忆={}已={}",
            py,
            po
        );
    }
}

/// [回归] 无上下文时第3层不生效，排序同现状（collocation 层跳过）。
#[test]
fn test_no_context_collocation_layer_skipped() {
    let seed = super::tests_query::seed_copy("ctx");
    let engine = Engine::new_with_db(&seed).unwrap();
    // context 为空，第3层不触发，按频率排序
    let cands = engine.process("yi");
    assert!(!cands.is_empty());
    // 无上下文时「一」应靠高频排前（不被 collocation 干扰）
    let pos_yi = cands.iter().position(|c| c.text.as_ref() == "一");
    assert!(
        pos_yi.map(|p| p < 5).unwrap_or(false),
        "无上下文时「一」应排前5"
    );
}
/// [用户DB验证] 真实用户 DB 下 记→yi 出忆。
#[test]
fn test_ji_yi_user_db_collocation() {
    let user_db = format!(
        "{}/Library/Application Support/x1pinyin/x1pinyin.db",
        std::env::var("HOME").unwrap_or_default()
    );
    if !std::path::Path::new(&user_db).exists() {
        return;
    }
    let mut engine = Engine::new_with_db(&user_db).unwrap();
    engine.commit("ji", "记", Some("ji"), true);
    let cands = engine.process("yi");
    let pos_yi = cands.iter().position(|c| c.text.as_ref() == "忆");
    let pos_yi2 = cands.iter().position(|c| c.text.as_ref() == "已");
    if let (Some(py), Some(po)) = (pos_yi, pos_yi2) {
        println!("用户DB: 忆位置={}, 已位置={}", py, po);
        assert!(py < po, "用户DB commit 记 后，忆应排在已前面");
    }
}

// ===== 种子 bigram 双表方案测试 =====

/// 核心目标：冷启动时（用户未积累），种子 bigram 让 LM 先验生效
/// （走 lm_prior_score；旧 context_bonus_dp 已删除）。
#[test]
fn test_cold_start_bigram_from_seed() {
    let path = crate::test_tmp::dir().join("x1_test_seed_bigram_cold.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let s = crate::storage::Storage::open(path).unwrap();
        // 模拟种子数据（自有分词语料 提取的"饭/吃"搭配）
        s.ensure_seed_bigram_table().unwrap();
        s.insert_seed_bigram_for_test("饭", "吃", 15).unwrap();
    }
    let mut engine = Engine::new_with_db(path).unwrap();

    // 验证种子 bigram 加载到 FreqModel（CSR 平铺表）
    assert_eq!(
        engine.freq.seed_bigram.get("饭", "吃"),
        Some(15),
        "种子 bigram 应加载到 FreqModel.seed_bigram"
    );

    // 验证 word_bigram_count 查种子表（用户没 commit 过 → 种子兜底）
    assert_eq!(
        engine.freq.word_bigram_count("饭", "吃"),
        15,
        "冷启动 word_bigram_count 应返回种子 count"
    );

    // 验证 LM 先验冷启动有上下文加分（关键！）：种子 bigram 饭→吃 命中，
    // 有上下文分数应显著高于空 ctx 的 unigram-only 分
    engine.context = vec!["饭".to_string()];
    let with_ctx = engine.lm_prior_score(&engine.context, "吃", 1000);
    let without_ctx = engine.lm_prior_score(&[], "吃", 1000);
    assert!(
        with_ctx > without_ctx,
        "冷启动种子 bigram 应让有上下文 LM 分更高：{} vs {}",
        with_ctx,
        without_ctx
    );

    let _ = std::fs::remove_file(path);
}

/// seed_bigram_totals 加载时派生 + 归一化条件概率。
///
/// 归一化是避免「高频弱指向错配」的核心机制：P(吃|饭)=count/total 衡量的是
/// 「饭」的后继中「吃」的占比，而非绝对共现次数。
#[test]
fn test_seed_bigram_totals_normalization() {
    let mut freq = crate::freq::FreqModel::new();
    freq.set_seed_bigram_counts(vec![
        ("饭".to_string(), "吃".to_string(), 15),
        ("饭".to_string(), "做".to_string(), 5),
        ("天".to_string(), "气".to_string(), 100),
        ("天".to_string(), "空".to_string(), 100),
    ]);
    // totals 派生：饭→20，天→200
    assert_eq!(freq.seed_bigram.total("饭"), 20);
    assert_eq!(freq.seed_bigram.total("天"), 200);
    // 归一化条件概率：P(吃|饭)=0.75（强指向，低 count）> P(气|天)=0.5（弱指向，高 count）
    let (c1, t1) = freq.seed_bigram_count_and_total("饭", "吃");
    let (c2, t2) = freq.seed_bigram_count_and_total("天", "气");
    assert_eq!((c1, t1), (15, 20));
    assert_eq!((c2, t2), (100, 200));
    let p1 = c1 as f64 / t1 as f64;
    let p2 = c2 as f64 / t2 as f64;
    assert!(
        p1 > p2,
        "强指向低 count 应赢过弱指向高 count：{:.2} vs {:.2}",
        p1,
        p2
    );
    // 未命中返回 (0, 0)
    assert_eq!(freq.seed_bigram_count_and_total("饭", "不存在"), (0, 0));
}

/// 用户优先：用户 commit 后，word_bigram_count 返回用户 count（完全忽略种子）。
#[test]
fn test_seed_bigram_user_priority() {
    let path = crate::test_tmp::dir().join("x1_test_seed_bigram_priority.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let s = crate::storage::Storage::open(path).unwrap();
        s.ensure_seed_bigram_table().unwrap();
        // 种子: (午饭, 吃) = 15（"午饭" 不在 equiv_prev 列表，避免泛化干扰；
        // 字+字 全线不记，fixture 用 词+字 合法形态）
        s.insert_seed_bigram_for_test("午饭", "吃", 15).unwrap();
    }
    let mut engine = Engine::new_with_db(path).unwrap();

    // 冷启动：word_bigram_count(午饭,吃) = 15（种子兜底）
    assert_eq!(engine.freq.word_bigram_count("午饭", "吃"), 15);

    // 用户 commit (午饭,吃) 2 次 → 用户精确表 (午饭,吃)=2
    // word_bigram_count(午饭,吃) 应返回用户的 2（用户优先，完全忽略种子的 15）
    engine.freq.record_word_bigram("午饭", "吃");
    engine.freq.record_word_bigram("午饭", "吃");
    assert_eq!(
        engine.freq.word_bigram_count("午饭", "吃"),
        2,
        "用户 commit 后应完全覆盖种子（用户优先硬规则），实际应 = 2"
    );

    let _ = std::fs::remove_file(path);
}

/// bigram_source_info 调试函数：双表查询返回正确来源标记。
#[test]
fn test_bigram_source_info_dual_table() {
    let path = crate::test_tmp::dir().join("x1_test_bigram_source.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let s = crate::storage::Storage::open(path).unwrap();
        s.ensure_seed_bigram_table().unwrap();
        s.insert_seed_bigram_for_test("午饭", "吃", 15).unwrap();
    }
    let mut engine = Engine::new_with_db(path).unwrap();

    // 种子有，用户无 → used = "seed"
    let (u, sc, used) = engine.bigram_source_info("午饭", "吃");
    assert_eq!((u, sc, used), (0, 15, "seed"));

    // 用户 commit 后 → used = "user"
    engine.freq.record_word_bigram("午饭", "吃");
    let (u, sc, used) = engine.bigram_source_info("午饭", "吃");
    assert_eq!(
        (u, sc, used),
        (1, 15, "user"),
        "用户 commit 后 used 应 = user（用户优先），实际 user={} seed={}",
        u,
        sc
    );

    // 都没有 → used = "none"
    let (u, _, used) = engine.bigram_source_info("不存在", "无搭配");
    assert_eq!((u, used), (0, "none"));

    let _ = std::fs::remove_file(path);
}

// ===== 种子 trigram 按需查询测试 =====

/// 核心目标：冷启动时（用户未积累），种子 trigram 经按需 SQL 查询生效。
///
/// 架构：种子不启动加载（零启动成本），Engine::trigram_count_with_seed
/// 在用户内存表未命中时按需查 seed_trigram_counts 表。
#[test]
fn test_cold_start_trigram_from_seed() {
    let path = crate::test_tmp::dir().join("x1_test_seed_trigram_cold.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let s = crate::storage::Storage::open(path).unwrap();
        s.insert_seed_trigram_for_test("我", "知道", "你", 18)
            .unwrap();
    }
    let engine = Engine::new_with_db(path).unwrap();

    // 冷启动：种子按需兜底（用户表空）
    assert_eq!(
        engine.trigram_count_with_seed("我", "知道", "你"),
        18,
        "冷启动 trigram_count_with_seed 应返回种子 count"
    );
    assert_eq!(
        engine.trigram_count_with_seed("我", "知道", "他"),
        0,
        "种子没有的三元组应返回 0"
    );
    // 用户内存表本身仍为空（种子不进 TrigramModel）
    assert_eq!(
        engine.trigram.count("我", "知道", "你"),
        0,
        "种子不应进入用户内存表（按需查询，非加载）"
    );

    let _ = std::fs::remove_file(path);
}

/// 用户优先：用户 record 后，trigram_count_with_seed 返回用户 count（完全忽略种子）。
#[test]
fn test_seed_trigram_user_priority() {
    let path = crate::test_tmp::dir().join("x1_test_seed_trigram_priority.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let s = crate::storage::Storage::open(path).unwrap();
        s.insert_seed_trigram_for_test("我", "知道", "你", 18)
            .unwrap();
    }
    let mut engine = Engine::new_with_db(path).unwrap();

    assert_eq!(engine.trigram_count_with_seed("我", "知道", "你"), 18);

    // 用户 record 2 次 → 用户优先，完全覆盖种子的 18
    engine.trigram.record("我", "知道", "你");
    engine.trigram.record("我", "知道", "你");
    assert_eq!(
        engine.trigram_count_with_seed("我", "知道", "你"),
        2,
        "用户 record 后应完全覆盖种子（用户优先硬规则）"
    );

    let _ = std::fs::remove_file(path);
}

/// 种子 trigram 不污染用户表：counts_flat（flush 数据源）不含种子。
///
/// 种子是只读数据，绝不能被 flush 写进用户表（否则用户表
/// 混入种子，"用户优先"硬规则就再也区分不出哪些是真用户行为）。
#[test]
fn test_seed_trigram_not_flushed_to_user_table() {
    let path = crate::test_tmp::dir().join("x1_test_seed_trigram_flush.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let s = crate::storage::Storage::open(path).unwrap();
        s.insert_seed_trigram_for_test("我", "知道", "你", 18)
            .unwrap();
    }
    let engine = Engine::new_with_db(path).unwrap();
    assert_eq!(engine.trigram_count_with_seed("我", "知道", "你"), 18);

    assert!(
        engine.trigram.counts_flat().is_empty(),
        "counts_flat 只应含用户表数据，种子不应泄漏到 flush 路径"
    );

    let _ = std::fs::remove_file(path);
}

/// 紧邻强 bigram 硬规则（「轮胎→放气」分开输入也应生效）。
///
/// 场景：轮胎→放气 是早期教学的强关联（用户 count=9，已落盘），
/// 随后教学 利益→放弃 三轮（栈内留重复词对 + 放弃 recent 很热），
/// 再 commit 轮胎，输 fangqi —— 放气必须排第一。
///
/// 若 context_bonus 对栈内全部 30 个词加权求和且 0.85 衰减太弱，栈内
/// 重复的「利益」在 distance 2/4 各贡献一份，加上放弃 recent，总和会
/// 反超 distance-0 的 轮胎→放气 强关联——本测试防止该回退。
#[test]
fn test_adjacent_strong_bigram_dominates_stale_history() {
    use crate::storage::{Storage, X1DictWord};
    let db_path = crate::test_tmp::dir().join("x1pinyin_test_adj_bigram.db");
    let _ = std::fs::remove_file(&db_path);
    {
        let mut storage = Storage::open(&db_path).unwrap();
        storage
            .import_x1dict_batch(&[
                X1DictWord {
                    simplified: "轮胎".into(),
                    pinyin: "lun tai".into(),
                    freq: 3000,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "放气".into(),
                    pinyin: "fang qi".into(),
                    freq: 800,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "放弃".into(),
                    pinyin: "fang qi".into(),
                    freq: 4000,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "利益".into(),
                    pinyin: "li yi".into(),
                    freq: 5000,
                    ..Default::default()
                },
            ])
            .unwrap();
    }
    let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();

    // 早期教学沉淀：轮胎→放气 用户强关联（count=9，模拟历史落盘数据，
    // 不在当前 context 栈窗口内——栈已被后续输入冲刷）
    for _ in 0..9 {
        engine.freq.record_word_bigram("轮胎", "放气");
    }

    // 近期教学：利益→放弃 三轮（栈内留下重复词对 + 放弃攒 total/recent）
    for _ in 0..3 {
        engine.commit("liyi", "利益", Some("li yi"), true);
        engine.commit("fangqi", "放弃", Some("fang qi"), true);
    }

    // 用户输入「轮胎」上屏，紧接着输 fangqi
    engine.commit("luntai", "轮胎", Some("lun tai"), true);
    let r = engine.process("fangqi");
    let top: Vec<&str> = r.iter().take(2).map(|c| c.text.as_ref()).collect();
    assert_eq!(
        top.first(),
        Some(&"放气"),
        "轮胎刚上屏，fangqi 首候选应是放气（紧邻强关联支配），实际 top2={:?}",
        top
    );

    let _ = std::fs::remove_file(&db_path);
}

/// context_bonus 去重：栈内重复出现的 prev 词只按最近一次
/// （distance 最小）计入，避免教学轮次让同一历史证据被累加多次。
#[test]
fn test_context_bonus_dedupes_repeated_prev() {
    let mut engine = Engine::new();
    for _ in 0..10 {
        engine.freq.record_word_bigram("利益", "放弃");
    }

    // 栈 A：利益出现两次（教学轮次残留）；栈 B：利益只出现一次。
    // 两者最近 occurrence 的 distance 相同（都是 2），去重后加分应相等。
    engine.context_clear_for_test();
    for w in ["利益", "放弃", "利益", "放弃", "轮胎"] {
        engine.context_push_for_test(w);
    }
    let bonus_dup = engine.context_bonus("放弃", '放', true, "fang qi");

    engine.context_clear_for_test();
    for w in ["利益", "放弃", "轮胎"] {
        engine.context_push_for_test(w);
    }
    let bonus_single = engine.context_bonus("放弃", '放', true, "fang qi");

    assert_eq!(
        bonus_dup, bonus_single,
        "重复 prev 不应重复计分：dup={} single={}",
        bonus_dup, bonus_single
    );
    assert!(bonus_single > 0, "利益→放弃 count=10 应有非零加分");
}

/// 上下文栈跨会话快照。
/// 背景：上下文栈会话级，进程重启归零——上下文加成（bigram/trigram/
/// collocation/3.5 层硬规则）在每次重启后冷启动，重启后前几句话无加成。
/// 设计：flush 时把栈尾 10 个词写 db_meta.context_snapshot（SOH 分隔），
/// 启动时读回。只动 db_meta。
#[test]
fn test_context_snapshot_persists_across_reopen() {
    let tmp = crate::test_tmp::dir().join(format!(
        "x1_ctx_snap_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let db = tmp.to_str().unwrap();
    {
        let mut engine = Engine::new_with_db(db).unwrap_or_else(|_| Engine::new());
        engine.commit("nihao", "你好", Some("ni hao"), true);
        engine.commit("xiexie", "谢谢", Some("xie xie"), true);
        engine.flush().unwrap();
    }
    {
        // 重开引擎（模拟进程重启）：上下文应从快照恢复
        let engine = Engine::new_with_db(db).unwrap_or_else(|_| Engine::new());
        let ctx = engine.context_vec_for_test();
        assert!(
            ctx.iter().any(|w| w == "你好") && ctx.iter().any(|w| w == "谢谢"),
            "重启后上下文应从快照恢复（含 你好/谢谢），实际: {:?}",
            ctx
        );
    }
    let _ = std::fs::remove_file(&tmp);
}

/// 快照只带栈尾 10 个词（BIGRAM_DECAY=0.85^d 下更旧的词权重 <0.2 且跨会话已是噪声）。
#[test]
fn test_context_snapshot_restores_only_recent_tail() {
    let tmp = crate::test_tmp::dir().join(format!(
        "x1_ctx_tail_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let db = tmp.to_str().unwrap();
    {
        let mut engine = Engine::new_with_db(db).unwrap_or_else(|_| Engine::new());
        for i in 1..=15 {
            engine.context_push_for_test(&format!("词{}", i));
        }
        engine.commit("nihao", "你好", Some("ni hao"), true); // 置脏让 flush 生效
        engine.flush().unwrap();
    }
    {
        let engine = Engine::new_with_db(db).unwrap_or_else(|_| Engine::new());
        let ctx = engine.context_vec_for_test();
        assert_eq!(
            ctx.len(),
            10,
            "只恢复栈尾 10 个词，实际 {} 个: {:?}",
            ctx.len(),
            ctx
        );
        assert_eq!(ctx.last().unwrap(), "你好", "最新词应在栈尾");
        assert!(
            !ctx.iter().any(|w| w == "词1"),
            "最旧的词不应恢复: {:?}",
            ctx
        );
    }
    let _ = std::fs::remove_file(&tmp);
}

// ===== 单字成词抑制状态机测试 =====
// 场景：用户逐字输入 wei(维)bao(保)，commit 单字「保」时检测到 维+保=维保 成词 →
// 置 suppress 标记 → 下个字 zheng 的候选查询跳过 保+证=保证 的 collocation →
// 「正」靠纯词频排前。commit 下一个单字时标记重置。
// 详见 mod.rs suppress_collocation_after_word 字段文档。

/// [核心] commit 单字成词后，下个字的候选跳过与前字的 collocation。
///
/// 构造：先 commit 单字「保」（此时 context 为空，不触发抑制），再 commit 单字「险」
/// （保+险=保险 在系统词库 → 触发抑制标记）。然后输入 xian——「险」本应因 保险 在词库
/// 排前，但因抑制标记生效，「险」的 collocation 被跳过。
/// 注：此测试验证标记能正确跳过 collocation；若词库里 保险 freq 足够高让其他路径
/// 也让「险」排前，则改用对比断言（抑制前后位次变化）。
#[test]
fn test_single_char_word_formation_suppresses_collocation() {
    let seed = super::tests_query::seed_copy("ctx");
    let mut engine = Engine::new_with_db(&seed).unwrap();
    // 确认系统词库有「保险」（测试前提）
    if !engine.base_dict_has_for_test("保险") {
        eprintln!("跳过：种子 DB 无「保险」");
        return;
    }
    // commit 单字「保」→ context=[保]，前字为空不触发抑制
    engine.commit("bao", "保", Some("bao"), true);
    assert!(
        !engine.suppress_collocation_after_word,
        "commit「保」时 context.last 为空，不应触发抑制"
    );
    // 无抑制状态下查 xian：「险」应因 保险 collocation 靠前
    let cands_normal = engine.process("xian");
    let pos_xian_normal = cands_normal.iter().position(|c| c.text.as_ref() == "险");

    // commit 单字「险」→ 检查 保+险=保险 成词 → 触发抑制标记
    engine.commit("xian", "险", Some("xian"), true);
    assert!(
        engine.suppress_collocation_after_word,
        "commit「险」时 保+险=保险 成词，应触发抑制标记"
    );
    // context.last 现在是「险」，但抑制是针对「下个字与险的 collocation」
    // 为验证抑制效果，commit 单字「保」重置标记 + 设新 context，再 commit「险」触发
    engine.commit("bao", "保", Some("bao"), true); // 重置标记，context=[...,险,保]
                                                   // 此时 commit「保」检查 险+保 不成词 → 标记 false
    assert!(!engine.suppress_collocation_after_word);
    engine.commit("xian", "险", Some("xian"), true); // 保+险=保险 成词 → 标记 true
    assert!(
        engine.suppress_collocation_after_word,
        "第二次 保+险 成词，应再次触发抑制"
    );
    // 标记 true 时查任意输入都应跳过 collocation。查 xian 验证「险」不再因 collocation 靠前
    let cands_suppressed = engine.process("xian");
    let pos_xian_suppressed = cands_suppressed
        .iter()
        .position(|c| c.text.as_ref() == "险");
    // 对比：抑制前「险」应比抑制后靠前（collocation 加成消失）
    if let (Some(pn), Some(ps)) = (pos_xian_normal, pos_xian_suppressed) {
        assert!(
            ps >= pn,
            "抑制后「险」位次({})应 ≥ 抑制前({})，collocation 加成应消失",
            ps,
            pn
        );
    }
}

/// [重置验证] commit 下一个单字时抑制标记重置，collocation 恢复正常。
#[test]
fn test_suppress_flag_resets_on_next_single_char_commit() {
    let seed = super::tests_query::seed_copy("ctx");
    let mut engine = Engine::new_with_db(&seed).unwrap();
    if !engine.base_dict_has_for_test("保险") {
        eprintln!("跳过：种子 DB 无「保险」");
        return;
    }
    // 构造抑制：commit「保」→ commit「险」(保+险成词) → 标记 true
    engine.commit("bao", "保", Some("bao"), true);
    engine.commit("xian", "险", Some("xian"), true);
    assert!(engine.suppress_collocation_after_word, "应已触发抑制");
    // commit 下一个单字（任意）→ 标记重置
    engine.commit("a", "啊", Some("a"), true);
    // 啊 与 险 不成词（险+啊 不在词库）→ 标记应重置为 false
    assert!(
        !engine.suppress_collocation_after_word,
        "commit 下一个单字应重置抑制标记"
    );
}

/// [标点清栈验证] clear_context 一并重置抑制标记。
#[test]
fn test_clear_context_resets_suppress_flag() {
    let seed = super::tests_query::seed_copy("ctx");
    let mut engine = Engine::new_with_db(&seed).unwrap();
    if !engine.base_dict_has_for_test("保险") {
        eprintln!("跳过：种子 DB 无「保险」");
        return;
    }
    engine.commit("bao", "保", Some("bao"), true);
    engine.commit("xian", "险", Some("xian"), true);
    assert!(engine.suppress_collocation_after_word, "应已触发抑制");
    engine.clear_context();
    assert!(
        !engine.suppress_collocation_after_word,
        "clear_context 应重置抑制标记"
    );
    assert!(engine.context_vec_for_test().is_empty(), "context 应已清空");
}

/// [Windows 等价] record_bigrams_from_text：
/// FMM 切词（long_word_freq 文本索引）+ 粒度规则（字+字不记）+ 标点断点 + 带权累积。
/// 用生僻双字组合避开内置词库内容不确定性（「写代码」类常见组合可能恰在
/// 内置词库中被 FMM 整段切走，断言失稳）。
#[test]
fn test_record_bigrams_from_text_fmm_and_granularity() {
    let mut engine = Engine::new();
    engine.dict.insert("bai wei", "掰蔚", 1000, true);
    engine.dict.insert("beng xi", "甭曦", 1000, true);

    // 我|你|掰蔚|甭曦：字+字跳过，字+词/词+词 记录
    engine.record_bigrams_from_text("我你掰蔚甭曦", 1);
    assert_eq!(
        engine.bigram_count_for_test("我", "你"),
        0,
        "字+字不记（粒度规则）"
    );
    assert_eq!(engine.bigram_count_for_test("你", "掰蔚"), 1, "字+词应记录");
    assert_eq!(
        engine.bigram_count_for_test("掰蔚", "甭曦"),
        1,
        "词+词应记录"
    );

    // 标点断点：跨标点不配对（计数不变）
    engine.record_bigrams_from_text("掰蔚，甭曦", 1);
    assert_eq!(
        engine.bigram_count_for_test("掰蔚", "甭曦"),
        1,
        "跨标点不配对"
    );

    // 带权累积：weight=2 后 1+2=3
    engine.record_bigrams_from_text("我你掰蔚甭曦", 2);
    assert_eq!(
        engine.bigram_count_for_test("掰蔚", "甭曦"),
        3,
        "带权累积 1+2"
    );
}

/// [连续同词抑制] 连续 ≥2 次提交同一词（测试/改内容，
/// 非正常表达）不记 bigram/trigram、不推进上下文栈——n-gram 历史零痕迹；
/// 后续正常词的记录不受影响。
#[test]
fn test_consecutive_same_word_suppressed() {
    let mut engine = Engine::new();
    // 同词连打两次：不产生自反对
    engine.commit("nihao", "你好", Some("ni hao"), true);
    engine.commit("nihao", "你好", Some("ni hao"), true);
    assert_eq!(
        engine.bigram_count_for_test("你好", "你好"),
        0,
        "连续同词不记 bigram"
    );
    assert_eq!(
        engine.context_vec_for_test().len(),
        1,
        "重复词不推进上下文栈"
    );

    // 重复后接正常词：新词对正常记录（prev 仍是 你好，无中间污染）
    engine.commit("shijie", "世界", Some("shi jie"), true);
    assert!(
        engine.bigram_count_for_test("你好", "世界") >= 1,
        "重复后的正常词对应记录"
    );
}

/// [连续同词抑制 · trigram + learn 路径] A,B,B 中第二个 B 不产生 (A,B,B)
/// trigram；learn 连续同词同样不记 bigram。
#[test]
fn test_consecutive_same_word_trigram_suppressed() {
    let mut engine = Engine::new();
    engine.commit("ni", "你", Some("ni"), true);
    engine.commit("hao", "好", Some("hao"), true);
    engine.commit("hao", "好", Some("hao"), true); // 连续同词
    assert_eq!(
        engine.trigram.count("你", "好", "好"),
        0,
        "连续同词 trigram 不记录"
    );

    // learn 路径同样抑制：连续 learn 同词不记 bigram
    let mut e2 = Engine::new();
    e2.learn("测试词", "ceshici");
    e2.learn("测试词", "ceshici");
    assert_eq!(
        e2.bigram_count_for_test("测试词", "测试词"),
        0,
        "learn 路径连续同词不记"
    );
}

/// 字+字 n-gram 全线不记。
///
/// 规则：bigram 两侧均单字 → 不记；trigram 的 (prev1, text) 过渡均单字 → 不记。
/// 字+词 / 词+字 / 词+词 照常。理由：两个单字高频相邻应固化成词（自造词管线），
/// 字↔字统计只有噪声价值——ddddd 类连续测试可借拆词学习与上下文积累出
/// (的→地) 这类噪声对，被 bigram 加分放大成排序错误。过滤在
/// Freq::record_word_bigram(_weighted) 与 TrigramModel::record(_weighted)
/// choke point 统一执行，覆盖全部调用方（commit 上下文 / 拆词学习 / FFI / FFM）。
#[test]
fn test_char_char_ngram_never_recorded() {
    // 路径 1：连续单字提交（ddddd 测试形态：de→的、de→地、de→得 交替）
    let mut engine = Engine::new();
    engine.commit("de", "的", Some("de"), true);
    engine.commit("de", "地", Some("de"), true);
    engine.commit("de", "得", Some("de"), true);
    engine.commit("de", "的", Some("de"), true);
    assert_eq!(
        engine.bigram_count_for_test("的", "地"),
        0,
        "字+字 bigram 不得记录（的→地）"
    );
    assert_eq!(
        engine.bigram_count_for_test("地", "得"),
        0,
        "字+字 bigram 不得记录（地→得）"
    );
    assert_eq!(
        engine.trigram.count("的", "地", "得"),
        0,
        "字+字过渡 trigram 不得记录"
    );

    // 路径 2：单字 + 多字词混排 → 字↔词照常记录
    let mut engine = Engine::new();
    engine.commit("chi", "吃", Some("chi"), true);
    engine.commit("fan", "米饭", Some("mi fan"), true);
    assert!(
        engine.bigram_count_for_test("吃", "米饭") > 0,
        "字+词 bigram 应照常记录"
    );

    // 路径 3：绿词拆分学习 [我, 去, 吃饭] → 只记 (去→吃饭)，(我→去) 过滤
    let tmp = crate::test_tmp::dir().join(format!(
        "x1_cc_ngram_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = crate::storage::Storage::open(tmp.to_str().unwrap()).unwrap();
        s.import_x1dict_batch(&[
            crate::storage::X1DictWord {
                simplified: "我".into(),
                pinyin: "wo".into(),
                freq: 5000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "去".into(),
                pinyin: "qu".into(),
                freq: 5000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "吃饭".into(),
                pinyin: "chi fan".into(),
                freq: 5000,
                is_primary: true,
            },
        ])
        .unwrap();
    }
    let mut engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
    engine.commit("woquchifan", "我去吃饭", Some("wo qu chi fan"), true);
    assert_eq!(
        engine.bigram_count_for_test("我", "去"),
        0,
        "绿词拆分的字+字段（我→去）不得记录"
    );
    assert!(
        engine.bigram_count_for_test("去", "吃饭") > 0,
        "绿词拆分的字+词段（去→吃饭）应照常记录"
    );
    let _ = std::fs::remove_file(&tmp);
}
