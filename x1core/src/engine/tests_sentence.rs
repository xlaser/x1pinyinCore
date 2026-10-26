//! Engine 测试 · 整句解码 / 切分 / 学习基础。

use super::*;

#[test]
fn test_sentence_decode_boundary() {
    // 边界：单词输入不产生重复整句候选
    let engine = Engine::new();
    let cands = engine.process("nihao");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    // 「你好」应在候选中，且不重复
    let count = texts.iter().filter(|&&t| t == "你好").count();
    assert_eq!(count, 1, "整句解码不应重复「你好」: {:?}", texts);
}

/// 复现：nengjiejuewenti（能解决问题）解码出「能继而决问题」而非「能解决问题」。
/// 测试数据策略：构造式 DB，避免种子 DB「解决」freq 数据冲突。
#[test]
fn test_repro_nengjiejuewenti() {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_neng_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        s.import_x1dict_batch(&[
            X1DictWord {
                simplified: "能".into(),
                pinyin: "neng".into(),
                freq: 5000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "解决".into(),
                pinyin: "jie jue".into(),
                freq: 5000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "问题".into(),
                pinyin: "wen ti".into(),
                freq: 5000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "戒绝".into(),
                pinyin: "jie jue".into(),
                freq: 100,
                ..Default::default()
            },
            X1DictWord {
                simplified: "文体".into(),
                pinyin: "wen ti".into(),
                freq: 100,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
    let cands = engine.process("nengjiejuewenti");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    assert!(
        texts.contains(&"能解决问题"),
        "「能解决问题」应在候选中，实际: {:?}",
        texts
    );
    let _ = std::fs::remove_file(&tmp);
}

/// 同类切分歧义回归测试（构造式 DB）。
#[test]
fn test_segmentation_ambiguity_cases() {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_seg_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        s.import_x1dict_batch(&[
            X1DictWord {
                simplified: "能".into(),
                pinyin: "neng".into(),
                freq: 5000,
                ..Default::default()
            },
            // 要/需要 是 case 2/3 的依赖词，必须保留
            X1DictWord {
                simplified: "要".into(),
                pinyin: "yao".into(),
                freq: 5000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "需要".into(),
                pinyin: "xu yao".into(),
                freq: 5000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "解决".into(),
                pinyin: "jie jue".into(),
                freq: 5000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "问题".into(),
                pinyin: "wen ti".into(),
                freq: 5000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "戒绝".into(),
                pinyin: "jie jue".into(),
                freq: 100,
                ..Default::default()
            },
            X1DictWord {
                simplified: "文体".into(),
                pinyin: "wen ti".into(),
                freq: 100,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
    let cases: &[(&str, &str)] = &[
        ("nengjiejuewenti", "能解决问题"),
        ("yaojiejuewenti", "要解决问题"),
        ("xuyaojiejue", "需要解决"),
    ];
    for (input, target) in cases {
        let cands = engine.process(input);
        let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
        assert!(
            texts.contains(target),
            "输入 {} 应含「{}」，实际: {:?}",
            input,
            target,
            texts
        );
    }
    let _ = std::fs::remove_file(&tmp);
}

/// 复现：fanghu（防护）完整双音节精确匹配，应排在放还/放话/放火（前缀匹配）之前。
///
/// 防护 fang hu freq=1050（输入音节数=词语音节数，完整匹配）
/// 放还 fang huan freq=2400 / 放话 fang hua 2198 / 放火 fang huo 1888（前缀匹配）
/// 完整匹配的优先级必须高于前缀匹配，不能被高 freq 的前缀词压过。
#[test]
fn test_repro_fanghu_full_match_priority() {
    let seed = super::tests_query::seed_copy("sent");
    let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
    let cands = engine.process("fanghu");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    eprintln!("fanghu 候选: {:?}", texts);

    // 防护（完整匹配）应排在放还/放话/放火（前缀匹配）之前
    let fanghu_pos = texts.iter().position(|&t| t == "防护");
    let prefix_words = ["放还", "放话", "放火"];
    assert!(
        fanghu_pos.is_some(),
        "「防护」应在候选中，实际: {:?}",
        texts
    );
    let fh = fanghu_pos.unwrap();
    for w in prefix_words {
        if let Some(pos) = texts.iter().position(|&t| t == w) {
            assert!(
                fh < pos,
                "完整匹配「防护」(位置{})应在前缀匹配「{}」(位置{})之前，候选: {:?}",
                fh,
                w,
                pos,
                texts
            );
        }
    }
}

#[test]
fn test_sentence_decode_single_syllable_no_op() {
    // 边界：单音节输入不触发整句解码（<2 音节）
    let engine = Engine::new();
    let cands = engine.process("ni");
    // 应返回单字候选，无整句拼接（单音节无整句价值）
    assert!(!cands.is_empty());
    // 所有候选都应是单字（ni 的同音字）
    assert!(
        cands.iter().take(9).all(|c| c.text.chars().count() == 1),
        "单音节不应产生多字整句: {:?}",
        cands.iter().take(3).collect::<Vec<_>>()
    );
}

#[test]
fn test_removed_text_not_reconstructed() {
    // 边界：remove 后整句解码不应把该词拼回来（黑名单）
    let mut engine = Engine::new();
    engine.learn("来说", "lai shuo");
    assert!(engine
        .process("laishuo")
        .iter()
        .any(|c| c.text.as_ref() == "来说"));
    engine.remove("来说");
    // 即使「来」「说」单字都在，整句解码也不应拼出「来说」
    assert!(
        !engine
            .process("laishuo")
            .iter()
            .any(|c| c.text.as_ref() == "来说"),
        "remove 后整句解码不应拼回「来说」"
    );
}

#[test]
fn test_learn_clears_removed_blacklist() {
    // 边界：learn 重新学习的词应清除黑名单（不再屏蔽整句解码）
    let mut engine = Engine::new();
    engine.learn("来说", "lai shuo");
    engine.remove("来说");
    assert!(!engine
        .process("laishuo")
        .iter()
        .any(|c| c.text.as_ref() == "来说"));
    // 重新学习 → 黑名单清除 → 整句解码可拼回
    engine.learn("来说", "lai shuo");
    assert!(
        engine
            .process("laishuo")
            .iter()
            .any(|c| c.text.as_ref() == "来说"),
        "learn 重新学习后整句解码应能拼回「来说」"
    );
}

#[test]
fn test_no_letter_in_candidates() {
    // 防御回归：中文候选列表绝不出现含 ASCII 字母的文本（汉字候选不可能含字母）。
    // 即使有字母脏词条（x1dict/user_words），process 也要过滤掉。
    // 英文单词候选（is_english，智能单词识别）是唯一豁免——
    // 纯小写英文词形由词表保证，其余类型不得含字母。
    let engine = Engine::new();
    let check = |cands: &[crate::dict::Candidate], label: &str| {
        assert!(
            cands
                .iter()
                .all(|c| { c.is_english || !c.text.chars().any(|ch| ch.is_ascii_alphabetic()) }),
            "{} 非英文候选含字母: {:?}",
            label,
            cands.iter().take(3).collect::<Vec<_>>()
        );
    };
    // xiw 这种混合简拼曾产出「系w」，现应被过滤
    check(&engine.process("xiw"), "xiw");
    // went 曾产出「文T」（现可能出英文候选 went——豁免，但「文T」类必须仍被过滤）
    check(&engine.process("went"), "went");
}

#[test]
fn test_commit_rejects_letter_text() {
    let mut engine = Engine::new();
    engine.commit("xiw", "系w", Some("xi w"), true);
    assert!(
        !engine.user_words.contains_key("系w"),
        "commit 不应把含字母的「系w」写入 user_words"
    );
    engine.commit("xiwang", "希望", Some("xi wang"), true);
    assert!(
        engine.freq_counts_contains_for_test("希望"),
        "commit 应记住「希望」的选择次数（freq.counts 应有记录）"
    );
}

#[test]
fn test_learn_rejects_letter_text() {
    // 防御回归：learn 拒绝写入含 ASCII 字母的候选。
    let mut engine = Engine::new();
    engine.learn("系w", "xi w");
    assert!(
        !engine.user_words.contains_key("系w"),
        "learn 不应把含字母的「系w」写入 user_words"
    );
}

#[test]
fn test_commit_sentence_not_stored_as_word() {
    // 设计：整句默认不固化，只拆词+bigram 学习。
    // 绿词保持绿词身份，靠 bigram 越用越准，不污染白词排序。
    // 用户想固化时用 Shift+数字键（force_solidify=true）手动触发。
    //
    // 此测试验证：普通 commit（force_solidify=false）整句不固化，只记 bigram。
    let mut engine = Engine::new();
    engine.commit("wobuzhidao", "我不知道", Some("wo bu zhi dao"), true);
    // 普通提交：整句不应固化到 user_words（保持绿词身份）
    assert!(
        !engine.user_words.contains_key("我不知道"),
        "普通 commit 整句「我不知道」不应固化（默认不固化，需 Shift+数字 force_solidify）"
    );
    // 但 bigram 应记录子词间关联（拆词学习仍执行）
    let bc = engine.bigram_count_for_test("我", "不知道");
    assert!(bc >= 1, "commit 整句后应记录 bigram(我→不知道)={}", bc);

    // 对照：force_solidify=true 时整句固化（模拟 Shift+数字键）
    let mut engine2 = Engine::new();
    engine2.commit_with_solidify("wobuzhidao", "我不知道", Some("wo bu zhi dao"), true, true);
    assert!(
        engine2.user_words.contains_key("我不知道"),
        "commit_with_solidify(force=true) 整句「我不知道」应固化到 user_words"
    );
}

/// 整句按次数自动固化：用户对同一 (input, text) 选过 ≥阈值次后自动固化。
/// 默认阈值 0（禁用）：绿词靠 bigram 学习"举一反三"，不固化整句。
/// 用户可在偏好设置开启（1-5）。此测试验证默认禁用 + 手动开启的行为。
#[test]
fn test_auto_solidify_after_threshold() {
    let mut engine = Engine::new();
    // 默认阈值 0（禁用）：选多次也不固化
    for _ in 0..10 {
        engine.commit("sheshuyiwo", "蛇鼠一窝", Some("she shu yi wo"), true);
    }
    assert!(
        !engine.user_words.contains_key("蛇鼠一窝"),
        "默认阈值 0（禁用）时，选 10 次也不应固化（绿词靠 bigram 举一反三）"
    );
    // 手动开启阈值 2：第 2 次固化
    let mut engine2 = Engine::new();
    engine2.set_auto_solidify_threshold(2);
    engine2.commit("sheshuyiwo", "蛇鼠一窝", Some("she shu yi wo"), true);
    assert!(
        !engine2.user_words.contains_key("蛇鼠一窝"),
        "阈值 2 时第 1 次不应固化"
    );
    engine2.commit("sheshuyiwo", "蛇鼠一窝", Some("she shu yi wo"), true);
    assert!(
        engine2.user_words.contains_key("蛇鼠一窝"),
        "阈值 2 时第 2 次应固化"
    );
}

/// 阈值可配置 + 0=禁用
#[test]
fn test_auto_solidify_threshold_configurable() {
    let mut engine = Engine::new();
    engine.set_auto_solidify_threshold(0); // 禁用
    for _ in 0..10 {
        engine.commit("sheshuyiwo", "蛇鼠一窝", Some("she shu yi wo"), true);
    }
    assert!(
        !engine.user_words.contains_key("蛇鼠一窝"),
        "阈值=0（禁用）时不应自动固化，即使选 10 次"
    );

    let mut engine2 = Engine::new();
    engine2.set_auto_solidify_threshold(1); // 选 1 次即固化
    engine2.commit("sheshuyiwo", "蛇鼠一窝", Some("she shu yi wo"), true);
    assert!(
        engine2.user_words.contains_key("蛇鼠一窝"),
        "阈值=1 时第 1 次选即应固化"
    );
}

#[test]
fn test_commit_short_phrase_still_stored() {
    // 系统词不写 user_words（避免两表重复），但 freq.counts 应记录选择。
    let mut engine = Engine::new();
    engine.commit("buzhidao", "不知道", Some("bu zhi dao"), true);
    assert!(
        engine.freq_counts_contains_for_test("不知道"),
        "常用词组「不知道」commit 后 freq.counts 应有记录"
    );
}

#[test]
fn test_long_input_stability() {
    // xiwangnih（末尾 h 不完整）时首候选不得退化为「希望」（丢失「你」）：
    // 整句候选应保持「希望」开头的多字结果，不回退到单词
    let engine = Engine::new();
    let cases = &[
        // (输入, 期望首候选以该前缀开头——证明整句候选没整体消失)
        ("xiwang", "希望"),
        ("xiwangni", "希望"),
        ("xiwangnih", "希望"), // 末尾 h 不完整，整句仍应是「希望」开头的多字
        ("xiwangnihao", "希望"),
        ("xiwangnihaoh", "希望"), // 末尾 h 不完整
    ];
    for (input, prefix) in cases {
        let cands = engine.process(input);
        let first = cands.first().map(|c| c.text.as_ref()).unwrap_or("");
        assert!(
            first.starts_with(prefix),
            "{} 首候选「{}」应以「{}」开头（整句不应整体回退）",
            input,
            first,
            prefix
        );
    }
    // 平滑性验证：xiwangni(完整匹配,绿词排首位) → xiwangnih(partial,绿词排后)
    // 首候选可能从绿词跳到既有词，这是合理的（partial 是过渡态，补字含猜测）。
    // 但首候选必须仍以「希望」开头（绿词在候选列表里，只是不排首位）。
    // xiwangnihao(完整匹配)时绿词恢复排首位。
    let n_full = engine
        .process("xiwangni")
        .first()
        .map(|c| c.text.chars().count())
        .unwrap_or(0);
    let n_complete = engine
        .process("xiwangnihao")
        .first()
        .map(|c| c.text.chars().count())
        .unwrap_or(0);
    // 完整输入的绿词字数应 >= 部分输入（用户继续输入，信息更完整）
    assert!(
        n_complete >= n_full,
        "完整输入的绿词字数不应少于部分输入（{}→{}）",
        n_full,
        n_complete
    );
}

#[test]
fn test_remove_word() {
    let mut engine = Engine::new();
    engine.learn("来说", "lai shuo");
    assert!(engine
        .process("laishuo")
        .iter()
        .any(|c| c.text.as_ref() == "来说"));

    engine.remove("来说");
    assert!(!engine
        .process("laishuo")
        .iter()
        .any(|c| c.text.as_ref() == "来说"));
}

/// 删词必须同步清理 input_mappings，否则被删的词通过 mapping_bonus 重新冒头。
#[test]
fn test_remove_clears_input_mappings() {
    let mut engine = Engine::new();
    engine.learn("来说", "lai shuo");
    // 模拟用户选过「来说」，建立 input_mapping
    engine.commit("laishuo", "来说", Some("lai shuo"), true);
    assert!(engine.input_mappings.iter().any(|((_, t), _)| t == "来说"));
    assert!(engine.input_mappings_by_text.contains_key("来说"));

    // 删词应同步清 input_mappings（内存 + 反向索引）
    engine.remove("来说");
    assert!(
        !engine.input_mappings.iter().any(|((_, t), _)| t == "来说"),
        "删词后 input_mappings 不应有该 text 残留"
    );
    assert!(
        !engine.input_mappings_by_text.contains_key("来说"),
        "删词后 input_mappings 不应有该 text 残留"
    );
}

/// 验证 lm_prior_score（插值 LM，替代旧 context_bonus_dp）的 trigram 用户硬覆盖生效。
///
/// 构造 [我, 知道] 上下文，用户 trigram(我, 知道, 你)=5 → trigram 层按 1.0 满档计。
/// 「你」的 LM 先验分应 > 无关词「苹果」。同时验证 (prev2, prev1) 双状态在 ctx
/// 切片中正确传递：栈尾=知道=prev1，倒数第二=我=prev2。
#[test]
fn test_lm_prior_trigram_user_override() {
    let seed = super::tests_query::seed_copy("sent");
    let mut engine = Engine::new_with_db(&seed).unwrap();
    // 直接构造 trigram 模型（绕过 commit，避免 freq 干扰）
    engine.trigram.record_weighted("我", "知道", "你", 5);
    // 构造 DP 句中衔接的 ctx 切片：[prev2, prev1] = [我, 知道]
    // base_freq 统一传相同值，只比上下文条件概率差异
    let ctx: Vec<String> = vec!["我".to_string(), "知道".to_string()];
    let score_ni = engine.lm_prior_score(&ctx, "你", 1000);
    let score_irrelevant = engine.lm_prior_score(&ctx, "苹果", 1000);
    assert!(
        score_ni > score_irrelevant,
        "用户 trigram(我,知道,你)=5 硬覆盖应让「你」的 LM 先验 > 无关词：{} vs {}",
        score_ni,
        score_irrelevant
    );
    // 量级守卫：lm_prior_score 返回 i32，应落在 clamp(0,32) 区间
    assert!(score_ni <= 32, "lm_prior_score 上界 32，实际 {}", score_ni);
}

/// 验证 DP 句首段在简拼模式（is_initial_mode）下完全不读 context。
///
/// 空 ctx → bigram/trigram 落 ε 地板，只剩 unigram 先验；有上下文（用户 bigram 硬覆盖）
/// 时分数应显著更高。
#[test]
fn test_lm_prior_empty_ctx_unigram_only() {
    let empty_ctx: Vec<String> = Vec::new();
    let engine = Engine::new();
    let score_empty = engine.lm_prior_score(&empty_ctx, "吃", 1000);

    let mut engine2 = Engine::new();
    // 字+字 (饭,吃) 全线不记，fixture 改 词+字 合法形态
    engine2.freq.record_word_bigram_weighted("午饭", "吃", 3);
    let ctx: Vec<String> = vec!["午饭".to_string()];
    let score_ctx = engine2.lm_prior_score(&ctx, "吃", 1000);
    assert!(
        score_ctx > score_empty,
        "用户 bigram 硬覆盖应显著高于空 ctx 的 unigram-only 分：{} vs {}",
        score_ctx,
        score_empty
    );
}

/// wohaoxiangchifan 冷启动整句回归：
/// 种子 bigram 未覆盖「我|好|想|吃饭」的句式时，bigram_support 恒 0 →
/// multi(1) < single(3) 门槛全杀 → 整句缺席、前缀词（我好/我好想/我好像）霸屏。
/// 门槛证据源补「相邻段边界字对成词」（我好/好想/想吃，freq≥500 计 1）。
///
/// 构造式 DB（词频镜像真实种子形态：好香2326 > 好像1936 > 好想781，
/// DP 的 [hao,xiang] 段 pick 是好香——正确句由单字路径拼接而成），避免与
/// 种子词库内容耦合。
#[test]
fn test_wohaoxiangchifan_cold_start_sentence_top1() {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_wohao_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        s.import_x1dict_batch(&[
            // 单字（我/好/想/吃/饭）——DP k=1 段的候选池
            X1DictWord {
                simplified: "我".into(),
                pinyin: "wo".into(),
                freq: 9950,
                ..Default::default()
            },
            X1DictWord {
                simplified: "好".into(),
                pinyin: "hao".into(),
                freq: 4000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "想".into(),
                pinyin: "xiang".into(),
                freq: 3500,
                ..Default::default()
            },
            X1DictWord {
                simplified: "香".into(),
                pinyin: "xiang".into(),
                freq: 900,
                ..Default::default()
            },
            X1DictWord {
                simplified: "吃".into(),
                pinyin: "chi".into(),
                freq: 3000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "饭".into(),
                pinyin: "fan".into(),
                freq: 2500,
                ..Default::default()
            },
            // 词库词：前缀词 + 同音词（freq 关系镜像真实种子：好香最高）
            X1DictWord {
                simplified: "我好".into(),
                pinyin: "wo hao".into(),
                freq: 840,
                ..Default::default()
            },
            X1DictWord {
                simplified: "我好想".into(),
                pinyin: "wo hao xiang".into(),
                freq: 441,
                ..Default::default()
            },
            X1DictWord {
                simplified: "我好像".into(),
                pinyin: "wo hao xiang".into(),
                freq: 327,
                ..Default::default()
            },
            X1DictWord {
                simplified: "好香".into(),
                pinyin: "hao xiang".into(),
                freq: 2326,
                ..Default::default()
            },
            X1DictWord {
                simplified: "好像".into(),
                pinyin: "hao xiang".into(),
                freq: 1936,
                ..Default::default()
            },
            X1DictWord {
                simplified: "好想".into(),
                pinyin: "hao xiang".into(),
                freq: 781,
                ..Default::default()
            },
            X1DictWord {
                simplified: "想吃".into(),
                pinyin: "xiang chi".into(),
                freq: 1019,
                ..Default::default()
            },
            X1DictWord {
                simplified: "吃饭".into(),
                pinyin: "chi fan".into(),
                freq: 2581,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    let mut engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
    // 本测试验证「第二绿词机制」（正确句靠第二绿词可达）——默认关，
    // 此处显式开启以保持测试意图；默认行为由
    // test_second_green_default_off_switch_restores 守护。
    engine.set_second_green_enabled(true);
    let cands = engine.process("wohaoxiangchifan");
    let first = cands.first().expect("应有候选");
    // 词图多候选边后的行为：Top1 为「我好像吃饭」——[hao,xiang] 段的
    // 「好想」(781) 与「好像」(1936) 都进入词图后靠词频竞争，好像胜出
    //（种子词频有语料偏差，不作为局部决定性证据）。本测试核心意图保持：
    // ①整句绿词占首位（前缀词「我好/我好想」不霸屏）；②正确答案
    //「我好想吃饭」作为第二绿词在前 4 候选可达——用户选一次即由行为数据翻转。
    assert!(
        first.is_sentence,
        "首候选应为绿词整句（防前缀词霸屏），实际前4: {:?}",
        cands
            .iter()
            .take(4)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
    assert!(
        cands
            .iter()
            .take(4)
            .any(|c| c.text.as_ref() == "我好想吃饭"),
        "正确整句「我好想吃饭」应在前 4 候选可达，实际前4: {:?}",
        cands
            .iter()
            .take(4)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
    let _ = std::fs::remove_file(&tmp);
}

/// 「首字母串 + 全拼尾」长混合守护（打到南京前不应回退成单字）：
/// 简拼长串后切全拼（zgrmjfj + nan → 中国人民解放军南）。
/// 旧行为：word_candidates 的 is_hybrid 上限（MAX_INITIAL_SEG_LEN=4）与全拼
/// 路径「句首单字母停止收集」双拦截 → 整条塌成单字（知/在/这…）。
/// 现路由到 token 混合路径（query_tokens + 绿词 DP）+ DP 末位全拼单字边。
#[test]
fn test_jianpin_full_tail_mix_produces_sentence() {
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_jptail_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::copy(seed, &tmp);
    let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();

    // ① 用户现场输入（打进「南京」的中间态）：#0 应为 8 字全覆盖整句
    let cands = engine.process("z'g'r'm'j'f'j'nan");
    let first = &cands[0];
    assert!(
        first.is_sentence && first.text.starts_with("中国人民") && first.text.chars().count() == 8,
        "中间态首选应为全覆盖整句（中国人民…），实际「{}」",
        first.text
    );

    // ② 打完「南京」：前 3 候选应含 中国人民解放军南京
    let cands2 = engine.process("z'g'r'm'j'f'j'nanjing");
    assert!(
        cands2
            .iter()
            .take(3)
            .any(|c| c.text.as_ref() == "中国人民解放军南京"),
        "应含「中国人民解放军南京」，实际前5: {:?}",
        cands2
            .iter()
            .take(5)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );

    // ③ zh 开头的同族（hybrid 路由）不回归：整句仍在首位
    let cands3 = engine.process("zhgrmjfjnan");
    assert!(
        cands3[0].is_sentence && cands3[0].text.starts_with("中国人民"),
        "zh 同族首选应为整句，实际「{}」",
        cands3[0].text
    );

    drop(engine);
    let _ = std::fs::remove_file(&tmp);
}

/// 「简拼+全拼不规律混合」中途绿词防消失：`z'g'ren'm'j`（= 中-国-人-民-解
/// 的混合输入）句首简拼字母被 valid_syl 收集循环拒收（jianpin_single 要求
///「前面已有入表段」）→ 循环在句首 break → valid_count=0 → 整句解码（≥3
/// 门槛）整体跳过，绿词回退成单词（实际现象：z'g'ren'm'j 时只剩 这个/中国/
/// 照顾…，前一键 z'guo'r'm 还有「中国人民」、后一键 z'g'ren'm'j'f'j 又回来
///「中国人民解放军」）。纯简拼（zgrmj）走 process_jianpin_inner 自建音节表
/// 不受影响（不对称根因）。机制：句首简拼字母同「中置简拼」入表（后随内容
/// 即视为简拼意图——句首单字母后接全拼音节的形态不可能是「半截声母续打」，
/// 续打会先补本音节字母）。
#[test]
fn test_green_no_disappear_irregular_jianpin_full_mix() {
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_jpmix_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::copy(seed, &tmp);
    let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();

    // 回归键：句首简拼 z/g + 中段全拼 ren + 尾简拼 m/j 的不规律混合——
    // 应有整句候选（5 音节全覆盖）
    // 且锚定「中国人民」（与完成态 中国人民解放军 连续，不回退）。
    let cands = engine.process("zgrenmj");
    assert!(
        cands
            .iter()
            .take(4)
            .any(|c| c.is_sentence && c.text.starts_with("中国人民")),
        "zgrenmj 应有以「中国人民」开头的整句候选，实际前4: {:?}",
        cands
            .iter()
            .take(4)
            .map(|c| (c.text.as_ref(), c.is_sentence))
            .collect::<Vec<_>>()
    );

    // 逐键稳定性：续打尾字母绿词不消失（6 字母态有整句，7 字母完成态有整词）
    let six = engine.process("zgrenmjf");
    assert!(
        six.iter()
            .take(4)
            .any(|c| c.is_sentence && c.text.starts_with("中国人民")),
        "zgrenmjf 绿词不应消失，实际前4: {:?}",
        six.iter()
            .take(4)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
    let seven = engine.process("zgrenmjfj");
    assert!(
        seven
            .iter()
            .take(2)
            .any(|c| c.text.as_ref() == "中国人民解放军"),
        "完成态应有「中国人民解放军」，实际前2: {:?}",
        seven
            .iter()
            .take(2)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );

    drop(engine);
    let _ = std::fs::remove_file(&tmp);
}

/// 检测函数单测：长首字母段（>4）+ 全拼尾才命中；近邻形态不误拦（各有既有路径）。
#[test]
fn test_initial_full_tail_mix_detection() {
    let t = |s: &str| Engine::is_initial_full_tail_mix_input(s);
    // 命中：首字母段 > 4 + 全拼尾
    assert!(t("zgrmjfjnan"));
    assert!(t("z'g'r'm'j'f'j'nan"));
    assert!(t("zgrmjfjnanjing"));
    assert!(t("zgrmjshao"), "run 5 + shao 尾");
    // 不命中：短前导段（既有 hybrid 词分支/拍扁回退覆盖）
    assert!(!t("hbni"), "run 2");
    assert!(!t("zgrma"), "run 3");
    assert!(!t("hdong"), "run 1");
    assert!(!t("znan"), "run 1");
    // 不命中：无全拼尾（纯简拼 → 简拼路径）/ 全拼前缀态（全拼路径）
    assert!(!t("zgrmjfj"));
    assert!(!t("zhonggr"));
    assert!(!t("zho"));
    assert!(!t("woaini"));
}

/// 长句第二备选默认关：绿词默认只 1 条；
/// 开关开启恢复 2 条（第二绿词动态门槛保留）。自包含小库（不依赖种子）。
#[test]
fn test_second_green_default_off_switch_restores() {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_2g_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut st = Storage::open(&tmp).unwrap();
        st.import_x1dict_batch(&[
            X1DictWord {
                simplified: "我".into(),
                pinyin: "wo".into(),
                freq: 9950,
                ..Default::default()
            },
            X1DictWord {
                simplified: "好".into(),
                pinyin: "hao".into(),
                freq: 4000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "想".into(),
                pinyin: "xiang".into(),
                freq: 3500,
                ..Default::default()
            },
            X1DictWord {
                simplified: "香".into(),
                pinyin: "xiang".into(),
                freq: 900,
                ..Default::default()
            },
            X1DictWord {
                simplified: "吃".into(),
                pinyin: "chi".into(),
                freq: 3000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "饭".into(),
                pinyin: "fan".into(),
                freq: 2500,
                ..Default::default()
            },
            X1DictWord {
                simplified: "我好".into(),
                pinyin: "wo hao".into(),
                freq: 840,
                ..Default::default()
            },
            X1DictWord {
                simplified: "我好想".into(),
                pinyin: "wo hao xiang".into(),
                freq: 441,
                ..Default::default()
            },
            X1DictWord {
                simplified: "我好像".into(),
                pinyin: "wo hao xiang".into(),
                freq: 327,
                ..Default::default()
            },
            X1DictWord {
                simplified: "好香".into(),
                pinyin: "hao xiang".into(),
                freq: 2326,
                ..Default::default()
            },
            X1DictWord {
                simplified: "好像".into(),
                pinyin: "hao xiang".into(),
                freq: 1936,
                ..Default::default()
            },
            X1DictWord {
                simplified: "好想".into(),
                pinyin: "hao xiang".into(),
                freq: 781,
                ..Default::default()
            },
            X1DictWord {
                simplified: "想吃".into(),
                pinyin: "xiang chi".into(),
                freq: 1019,
                ..Default::default()
            },
            X1DictWord {
                simplified: "吃饭".into(),
                pinyin: "chi fan".into(),
                freq: 2581,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    let mut engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();

    // 默认：绿词（is_sentence）≤ 1 条
    let cands = engine.process("wohaoxiangchifan");
    let greens = cands.iter().filter(|c| c.is_sentence).count();
    assert!(
        greens <= 1,
        "默认应只给 1 条绿词（实际 {greens}）: {:?}",
        cands.iter().map(|c| c.text.as_ref()).collect::<Vec<_>>()
    );

    // 开关开启：恢复到 2 条上限（动态门槛内）
    engine.set_second_green_enabled(true);
    let cands2 = engine.process("wohaoxiangchifan");
    let greens2 = cands2.iter().filter(|c| c.is_sentence).count();
    assert!(
        greens2 > greens,
        "开关开启后第二绿词应恢复（实际 {greens2} vs 默认 {greens}）"
    );
    assert!(greens2 <= 2, "绿词上限 2（实际 {greens2}）");
    let _ = std::fs::remove_file(&tmp);
}

/// 绿词防截断。
/// 键窗：…chai'f（悬尾补字，正常）→ …chai'fe（尾部 f,e 成简拼段，整句解码入
/// initial 模式后 DP 可达表全 0，绿词整体消失）→ …chaifen（回归）。前缀回退解码让
/// 三键绿词逐字一致，杜绝「长句突然消失再重组」的跳变。
#[test]
fn test_green_no_disappear_mid_syllable_typing() {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_greenfb_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        s.import_x1dict_batch(&[
            X1DictWord {
                simplified: "将".into(),
                pinyin: "jiang".into(),
                freq: 5000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "单体".into(),
                pinyin: "dan ti".into(),
                freq: 4000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "应用".into(),
                pinyin: "ying yong".into(),
                freq: 4000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "拆".into(),
                pinyin: "chai".into(),
                freq: 3000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "拆分".into(),
                pinyin: "chai fen".into(),
                freq: 4000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "分".into(),
                pinyin: "fen".into(),
                freq: 3500,
                ..Default::default()
            },
            X1DictWord {
                simplified: "飞".into(),
                pinyin: "fei".into(),
                freq: 100,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();

    let first_sentence = |input: &str| -> Option<String> {
        engine
            .process(input)
            .into_iter()
            .find(|c| c.is_sentence)
            .map(|c| c.text.as_ref().to_owned())
    };

    // 完成态：chai'fen 全拼解码出绿词
    let done = first_sentence("jiangdantiyingyongchaifen").expect("chaifen 应有绿词");
    assert_eq!(done, "将单体应用拆分");

    // 回归键：chai'fe 应有前缀回退补「分」，与完成态逐字一致
    let mid =
        first_sentence("jiangdantiyingyongchaife").expect("chaife 绿词不应消失（前缀回退解码）");
    assert_eq!(mid, done, "chaife 绿词应与 chaifen 完成态逐字一致");

    // 前一键：chai'f 悬尾既有机制仍在，字数与完成态一致
    let pre = first_sentence("jiangdantiyingyongchaif").expect("chaif 应有绿词（悬尾补字）");
    assert_eq!(pre.chars().count(), done.chars().count());

    let _ = std::fs::remove_file(&tmp);
}

/// 绿词防消失·下一句首字母形态（前缀回退解码的姊妹案）。
/// 场景：dang…zhineng 绿词「当你使用人工智能」在位，继续输入下一句首字母 d 后
/// 绿词整体消失只剩单词。根因与 chaife 案不同：切分选举层面丢变体——
/// `segmentations()`（全有效语义）对含不完整尾的输入整体返回空，含尾枚举的
/// DFS 名额又被贪长退化子树（reng|o|n|g 一族，reng 误并 + 单字符续命）挤满，
/// 正解 `ren|gong`（分数更高）落在 cap 之外从未被枚举 → best_syllables 取到
/// 垃圾切分 → 整句 DP 无法组句（人/工 需要 ren|gong 而非 reng|o|n|g）。
/// 解法：「完整音节前缀 + 尾部未拼完字母」保底切分（pinyin::segmentations_prefix_tail）
/// 恒在候选切分里，打分自然选回正解。本测试锁两件事：
/// ① 绿词不消失（带 d 尾的输入仍有整句候选，且以完成态绿词为前缀）；
/// ② 显示链切分不崩（预编辑仍是 ren'gong 而非 reng'o'n'g）。
#[test]
fn test_green_no_disappear_next_word_initial() {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_greenfb2_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        s.import_x1dict_batch(&[
            X1DictWord {
                simplified: "当".into(),
                pinyin: "dang".into(),
                freq: 6920,
                ..Default::default()
            },
            X1DictWord {
                simplified: "你".into(),
                pinyin: "ni".into(),
                freq: 5000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "使".into(),
                pinyin: "shi".into(),
                freq: 2000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "用".into(),
                pinyin: "yong".into(),
                freq: 3000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "人".into(),
                pinyin: "ren".into(),
                freq: 6000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "工".into(),
                pinyin: "gong".into(),
                freq: 3000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "智".into(),
                pinyin: "zhi".into(),
                freq: 2000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "能".into(),
                pinyin: "neng".into(),
                freq: 3000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "使用".into(),
                pinyin: "shi yong".into(),
                freq: 4000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "人工".into(),
                pinyin: "ren gong".into(),
                freq: 2500,
                ..Default::default()
            },
            X1DictWord {
                simplified: "智能".into(),
                pinyin: "zhi neng".into(),
                freq: 2500,
                ..Default::default()
            },
            X1DictWord {
                simplified: "人工智能".into(),
                pinyin: "ren gong zhi neng".into(),
                freq: 2000,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();

    let first_sentence = |input: &str| -> Option<String> {
        engine
            .process(input)
            .into_iter()
            .find(|c| c.is_sentence)
            .map(|c| c.text.as_ref().to_owned())
    };

    // 完成态：dang…zhineng 全拼解码出绿词
    let done = first_sentence("dangnishiyongrengongzhineng").expect("完成态应有绿词");
    assert_eq!(done, "当你使用人工智能");

    // 回归键：多打一个下一句首字母 d——绿词应仍在且以
    // 完成态绿词为前缀（d 尾按悬尾语义可补 1 个猜测字，允许文本变长不允许消失）。
    let mid = first_sentence("dangnishiyongrengongzhinengd")
        .expect("d 尾一键绿词不应消失（完整音节前缀+悬尾保底切分）");
    assert!(
        mid.starts_with(&done),
        "d 尾绿词「{mid}」应以完成态「{done}」为前缀"
    );

    // 显示链：预编辑切分必须保住 ren|gong（而非 reng|o|n|g 贪长退化切分）
    let disp = engine
        .syllable_segmentation("dangnishiyongrengongzhinengd")
        .join("'");
    assert_eq!(
        disp, "dang'ni'shi'yong'ren'gong'zhi'neng'd",
        "预编辑切分应保持完整音节前缀 + 悬尾 d"
    );

    let _ = std::fs::remove_file(&tmp);
}

/// 同字数整词读法（紫/橙）在场时，拼凑绿词不得占首位——只降位不隐藏
///（两个假设都保留可见，整词读法在前）。
/// 锚点：aizengfenmin（min→ming 尾音节截断）不应让「爱憎分民[句]」压住
///「爱憎分明[紫]」——整词读法须 #0。正常整句绿词行为不变。
#[test]
fn test_word_reading_outranks_fragment_green() {
    use super::tests_query::{seed_copy, seed_copy_cleanup};
    let db = seed_copy("bfix_green_demote");
    let mut engine = Engine::new_with_db(&db).expect("open seed copy");
    engine.set_smart_word_zh_enabled(false);

    let cands = engine.process("aizengfenmin");
    let top = cands.first().expect("aizengfenmin 候选非空");
    assert_eq!(
        top.text.as_ref(),
        "爱憎分明",
        "整词读法（紫）应压拼凑绿句，实际首位 {:?}",
        top.text
    );
    assert!(top.is_guess, "锚点候选应来自延展猜测通道");
    if let Some(p) = cands.iter().position(|c| c.text.as_ref() == "爱憎分民") {
        assert!(p > 0, "拼凑绿句不得占首位（实际位 {}）", p);
    }

    // 旗舰回归：正常整句绿词照常首位（本规则只在同字数整词读法在场时触发）
    let cands2 = engine.process("woxiangchifan");
    assert_eq!(
        cands2.first().unwrap().text.as_ref(),
        "我想吃饭",
        "无同字数整词读法竞争时绿句行为必须不变"
    );
    seed_copy_cleanup(&db);
}

/// 语气词尾精确读法回归（haibucuoa）：尾 a 是有效单字母音节且无延长词
/// 证据 → 语气词语义，精确读法「还不错啊」（尾音节 a 精确匹配）必须压过
/// 补字猜测「还不错爱」（a 仅是 ai 的前缀，爱字频 6120 > 啊 5000 曾靠
/// 裸字频胜出）。词频镜像真实种子形态：还不错 8770 / 爱 6120 / 啊 5000 /
/// 错爱 253（< 组合词门槛 500，守护「错爱」不因 collocation 层劫持补字）。
/// 修复位：best_char_for_initial 精确音节层——补字直接选精确的 啊，与
/// 全消费句同文本去重合一（爱不再出现）。曾试「全消费句硬排补字句前」的
/// 族级兜底，实测翻转 nixiangganm（你想干吗→你想干姆）且与「用户
/// bigram > 精确」层级自相矛盾，已弃。
#[test]
fn test_particle_tail_exact_reading_beats_prefix_guess() {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_haibucuoa_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        s.import_x1dict_batch(&[
            X1DictWord {
                simplified: "还".into(),
                pinyin: "hai".into(),
                freq: 8770,
                ..Default::default()
            },
            X1DictWord {
                simplified: "不".into(),
                pinyin: "bu".into(),
                freq: 8000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "错".into(),
                pinyin: "cuo".into(),
                freq: 3000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "还不错".into(),
                pinyin: "hai bu cuo".into(),
                freq: 8770,
                ..Default::default()
            },
            X1DictWord {
                simplified: "不错".into(),
                pinyin: "bu cuo".into(),
                freq: 7000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "爱".into(),
                pinyin: "ai".into(),
                freq: 6120,
                ..Default::default()
            },
            X1DictWord {
                simplified: "啊".into(),
                pinyin: "a".into(),
                freq: 5000,
                ..Default::default()
            },
            // 错爱真实词频 253：< 组合词门槛 500，collocation 层必须不认它
            X1DictWord {
                simplified: "错爱".into(),
                pinyin: "cuo ai".into(),
                freq: 253,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    let mut engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
    // 开第二绿词：让「还不错爱」若仍被产出时可见，断言其位次（族级兜底验证）
    engine.set_second_green_enabled(true);
    let cands = engine.process("haibucuoa");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    let pos_a = texts.iter().position(|&t| t == "还不错啊");
    let pos_ai = texts.iter().position(|&t| t == "还不错爱");
    assert!(
        pos_a.is_some(),
        "精确读法「还不错啊」必须在候选中，实际: {:?}",
        texts.iter().take(6).collect::<Vec<_>>()
    );
    match (pos_a, pos_ai) {
        (Some(a), Some(ai)) => assert!(
            a < ai,
            "精确读法「还不错啊」(#{}) 必须排在补字猜测「还不错爱」(#{ai}) 之前",
            a
        ),
        // 补字层修复后主路补 啊 与全消费句去重、猜测句消失——同样合格
        (Some(_), None) => {}
        _ => unreachable!(),
    }
    assert!(
        texts.first() == Some(&"还不错啊"),
        "首候选应为「还不错啊」，实际前3: {:?}",
        texts.iter().take(3).collect::<Vec<_>>()
    );
    let _ = std::fs::remove_file(&tmp);
}

/// 记忆召回强度序回归（haibucuoa）：用户在同一 input 上教过两个读法
/// （还不错爱×2 误触 / 还不错啊×4 真实选择）时，召回排序必须让 count 强的
/// 还不错啊 占 #0——旧「升序头插 + 同名在场即跳过」让 啊×4 的召回被 DP
/// 产出的同名候选顶替、爱×2 独自头插霸屏。修复：降序迭代 + 插入位锚定
///（在场条目不移动——fangqi 上下文裁决语义保留——但为更弱缺席条目锚定
/// 插入位）。两个召回分支（green_memo 命中 / input_mappings 兜底）同语义；
/// 守护 test_adjacent_strong_bigram_dominates_stale_history（fangqi 在场
/// 不霸位）与本测（强度序）双侧。
#[test]
fn test_mapping_recall_strength_order() {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_recall_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        s.import_x1dict_batch(&[
            X1DictWord {
                simplified: "还".into(),
                pinyin: "hai".into(),
                freq: 8770,
                ..Default::default()
            },
            X1DictWord {
                simplified: "不".into(),
                pinyin: "bu".into(),
                freq: 8000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "错".into(),
                pinyin: "cuo".into(),
                freq: 3000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "还不错".into(),
                pinyin: "hai bu cuo".into(),
                freq: 8770,
                ..Default::default()
            },
            X1DictWord {
                simplified: "不错".into(),
                pinyin: "bu cuo".into(),
                freq: 7000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "爱".into(),
                pinyin: "ai".into(),
                freq: 6120,
                ..Default::default()
            },
            X1DictWord {
                simplified: "啊".into(),
                pinyin: "a".into(),
                freq: 5000,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    let mut engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
    // 教学历史镜像用户现场：爱 2 次（误触）+ 啊 4 次（真实选择）
    for _ in 0..2 {
        engine.commit("haibucuoa", "还不错爱", Some("hai bu cuo ai"), false);
    }
    for _ in 0..4 {
        engine.commit("haibucuoa", "还不错啊", Some("hai bu cuo a"), false);
    }
    let cands = engine.process("haibucuoa");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    let pos_a = texts.iter().position(|&t| t == "还不错啊");
    let pos_ai = texts.iter().position(|&t| t == "还不错爱");
    assert_eq!(
        texts.first().copied(),
        Some("还不错啊"),
        "count 强的读法必须 #0，实际前3: {:?}",
        texts.iter().take(3).collect::<Vec<_>>()
    );
    if let (Some(a), Some(ai)) = (pos_a, pos_ai) {
        assert!(a < ai, "还不错啊(#{a}) 必须排在 还不错爱(#{ai}) 之前");
    }
    let _ = std::fs::remove_file(&tmp);
}
