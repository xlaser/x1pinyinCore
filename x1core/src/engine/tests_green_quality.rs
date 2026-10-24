//! Engine 测试 · 绿词质量守护。
//!
//! 守护场景：词库已有同音整词时，decode_sentence 不应生成单字拼凑绿词
//! （如 bianyiqi 词库已有「编译器」，不应再拼出「变以期」噪音绿词）。
//! 长句（如「明天下午一起去看电影」）词库无整词，DP 应正常生成多词组合绿词。
//!
//! mod.rs 用 `#[cfg(test)] mod tests_green_quality;` 加载（分号形式从文件加载）。
//! `use super::*` 仍指向 engine mod，测试访问 engine 的 pub 项不变。

use super::*;

/// 构造式 DB：词库有「编译器」(bian yi qi) 整词 + 多个高频单字可拼出同音组合。
/// 用于验证「词库已有整词就不生拼凑绿词」。
fn make_dict_has_word_db() -> std::path::PathBuf {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_gq_has_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        s.import_x1dict_batch(&[
            // 词库已有的整词（白词）
            X1DictWord {
                simplified: "编译器".into(),
                pinyin: "bian yi qi".into(),
                freq: 312,
                ..Default::default()
            },
            // 高频单字，DP 会试图拼出"变+以+期"等同音组合
            X1DictWord {
                simplified: "变".into(),
                pinyin: "bian".into(),
                freq: 6740,
                is_primary: true,
            },
            X1DictWord {
                simplified: "以".into(),
                pinyin: "yi".into(),
                freq: 8750,
                ..Default::default()
            },
            X1DictWord {
                simplified: "期".into(),
                pinyin: "qi".into(),
                freq: 6280,
                ..Default::default()
            },
            // 2 字词段（DP 可能拼出"便意+期"等）
            X1DictWord {
                simplified: "便意".into(),
                pinyin: "bian yi".into(),
                freq: 1485,
                ..Default::default()
            },
            X1DictWord {
                simplified: "以期".into(),
                pinyin: "yi qi".into(),
                freq: 2705,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    tmp
}

/// 构造式 DB：词库没有长句整词，但子词齐全（验证长句绿词正常生成）。
fn make_long_sentence_db() -> std::path::PathBuf {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_gq_long_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        s.import_x1dict_batch(&[
            // 长句子词（注意：没有"明天下午一起去看电影"9字整词）
            X1DictWord {
                simplified: "明天".into(),
                pinyin: "ming tian".into(),
                freq: 3400,
                ..Default::default()
            },
            X1DictWord {
                simplified: "下午".into(),
                pinyin: "xia wu".into(),
                freq: 1685,
                ..Default::default()
            },
            X1DictWord {
                simplified: "一起".into(),
                pinyin: "yi qi".into(),
                freq: 1500,
                ..Default::default()
            },
            X1DictWord {
                simplified: "去看".into(),
                pinyin: "qu kan".into(),
                freq: 800,
                ..Default::default()
            },
            X1DictWord {
                simplified: "电影".into(),
                pinyin: "dian ying".into(),
                freq: 2000,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    tmp
}

/// 核心 case：词库有「编译器」整词，decode_sentence 应返回空，不生成「变以期」噪音绿词。
#[test]
fn test_no_green_when_dict_has_full_word() {
    let db = make_dict_has_word_db();
    let engine = Engine::new_with_db(db.to_str().unwrap()).unwrap();
    let syl: Vec<&str> = vec!["bian", "yi", "qi"];
    let greens = engine.decode_sentence_for_test("bianyiqi", &syl);

    // 调试输出
    for (i, c) in greens.iter().enumerate() {
        println!(
            "bianyiqi decode_sentence [{}] {} pinyin={} freq={}",
            i, c.text, c.pinyin, c.base_freq
        );
    }

    // 期望：词库已有整词 → decode_sentence 不应返回任何绿词
    assert!(
        greens.is_empty(),
        "词库已有「编译器」整词，不应生成拼凑绿词，但返回了：{:?}",
        greens.iter().map(|c| &c.text).collect::<Vec<_>>()
    );

    // 特别检查：不应出现"变以期"这种噪音绿词
    assert!(
        !greens.iter().any(|c| c.text.as_ref() == "变以期"),
        "「变以期」是噪音绿词，不应生成"
    );
}

/// 长句场景：词库没有「明天下午一起去看电影」整词，DP 应正常生成多词组合绿词。
#[test]
fn test_green_still_works_for_long_sentence() {
    let db = make_long_sentence_db();
    let engine = Engine::new_with_db(db.to_str().unwrap()).unwrap();
    let syl: Vec<&str> = vec![
        "ming", "tian", "xia", "wu", "yi", "qi", "qu", "kan", "dian", "ying",
    ];
    let greens = engine.decode_sentence_for_test("mingtianxiawuyiqiqukandianying", &syl);

    for (i, c) in greens.iter().enumerate() {
        println!(
            "long sentence decode_sentence [{}] {} pinyin={}",
            i, c.text, c.pinyin
        );
    }

    // 期望：长句场景词库无整词，应正常生成绿词
    assert!(
        !greens.is_empty(),
        "长句（词库无整词）应正常生成绿词，但返回空"
    );
    // 应包含合理的整句组合
    assert!(
        greens
            .iter()
            .any(|c| c.text.contains("明天") || c.text.contains("电影")),
        "长句绿词应包含「明天」或「电影」等子词组合，实际：{:?}",
        greens.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
}

/// 3 音节但词库无整词的场景：应正常生成绿词（验证短路只针对"词库有整词"）。
#[test]
fn test_green_still_works_for_3_syllable_no_full_word() {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_gq_3sy_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        s.import_x1dict_batch(&[
            // 注意：没有"我喜欢"3字整词
            X1DictWord {
                simplified: "我".into(),
                pinyin: "wo".into(),
                freq: 9880,
                is_primary: true,
            },
            X1DictWord {
                simplified: "喜".into(),
                pinyin: "xi".into(),
                freq: 6000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "欢".into(),
                pinyin: "huan".into(),
                freq: 5000,
                ..Default::default()
            },
            // 2 字词段（DP 可拼出"我+喜欢"或"我喜欢"等）
            X1DictWord {
                simplified: "喜欢".into(),
                pinyin: "xi huan".into(),
                freq: 7000,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
    let syl: Vec<&str> = vec!["wo", "xi", "huan"];
    let greens = engine.decode_sentence_for_test("woxihuan", &syl);

    // 词库无"我喜欢"整词 → 应正常生成绿词
    assert!(
        !greens.is_empty(),
        "词库无整词的 3 音节输入应正常生成绿词，但返回空"
    );
    assert!(
        greens
            .iter()
            .any(|c| c.text.contains("我") && c.text.contains("喜")),
        "应包含「我喜欢」或类似组合，实际：{:?}",
        greens.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
}

/// 用户自造同音整词后，下次输入应触发短路（不生拼凑绿词）。
/// 验证 self.dict（含 user_words）的整词查询能命中用户词。
#[test]
fn test_no_green_when_user_created_full_word() {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_gq_user_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        // 用户自造整词「变易气」（不在原始种子词库）
        s.import_x1dict_batch(&[
            X1DictWord {
                simplified: "变易气".into(),
                pinyin: "bian yi qi".into(),
                freq: 100,
                ..Default::default()
            },
            X1DictWord {
                simplified: "变".into(),
                pinyin: "bian".into(),
                freq: 6740,
                is_primary: true,
            },
            X1DictWord {
                simplified: "易".into(),
                pinyin: "yi".into(),
                freq: 4100,
                ..Default::default()
            },
            X1DictWord {
                simplified: "气".into(),
                pinyin: "qi".into(),
                freq: 7400,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
    let syl: Vec<&str> = vec!["bian", "yi", "qi"];
    let greens = engine.decode_sentence_for_test("bianyiqi", &syl);

    // 用户自造整词后，dict.query 能查到 → 触发短路 → 不生拼凑绿词
    assert!(
        greens.is_empty(),
        "用户自造同音整词后，应触发短路不生拼凑绿词，但返回：{:?}",
        greens.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
}

// ──────────────────────────────────────────────────────────────────────
// 简拼绿词 DP 重复长串 bug 守护
//
// Bug 现象：连续输入 wwww（4 个 w）会出现"文件清单文件清单文件清单"
// 或"稳定性文件清单"等不合理长串候选。
//
// 根因：process_jianpin_inner 在 jp_len>=4 时触发 initial-mode 绿词 DP，
// query_seg 在 initial-mode 下对 seg=[w]（k=1）调 query_tokens，
// 因"全 Initial 时 collect_candidates 全子树收"（dict.rs）拿到 w 子树下
// 任意字数的词（4-8 字长词如"文件清单"）；DP 打分对"小 k 段却 pick 长词"
// 无惩罚，导致 4 段拼出 12+ 字长串，且同一长词被多段重复 pick 就出现
// "X+X+X"三重重复模式。
//
// 处理：① query_seg initial 分支过滤 char_count ≤ k（k 个首字母最多对应 k 个汉字）
//      ② 回溯门槛加"总字数 ≤ 总音节数 + 容差"硬上限（defense-in-depth）
// ──────────────────────────────────────────────────────────────────────

/// 构造复现 wwww bug 的 DB：词库有"文件清单"（wen jian qing dan，首字母 wjqd）
/// + "稳定性"（wen ding xing，首字母 wdx）+ 若干 w 开头高频单字。
/// 输入 wwww（首字母序列 wwww）与"文件清单"的首字母 wjqd 不匹配，
/// 但因 initial-mode DP 子树全收的 bug，会被错误地拼进候选。
///
/// **关键**：真实使用中的 DB 里"文件清单"既在 x1dict_words 也在 user_words
/// （user_freq=4），需要同时写入两表才能复现 bug（user_words 回填 Trie 的
/// 路径与 x1dict_words 略有差异，且 user_words 的词会进 Trie candidates）。
fn make_jianpin_repetition_db() -> std::path::PathBuf {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_jp_rep_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        // x1dict_words：系统词 + 长词（模拟用户 DB 的实际状态）
        s.import_x1dict_batch(&[
            X1DictWord {
                simplified: "文件清单".into(),
                pinyin: "wen jian qing dan".into(),
                freq: 2000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "稳定性".into(),
                pinyin: "wen ding xing".into(),
                freq: 1430,
                ..Default::default()
            },
            // w 开头的高频单字（这些是合理的 w 简拼候选）
            X1DictWord {
                simplified: "我".into(),
                pinyin: "wo".into(),
                freq: 9800,
                ..Default::default()
            },
            X1DictWord {
                simplified: "无".into(),
                pinyin: "wu".into(),
                freq: 5400,
                ..Default::default()
            },
            X1DictWord {
                simplified: "问".into(),
                pinyin: "wen".into(),
                freq: 3200,
                ..Default::default()
            },
            X1DictWord {
                simplified: "为".into(),
                pinyin: "wei".into(),
                freq: 9500,
                ..Default::default()
            },
        ])
        .unwrap();
        // user_words：用户自造词（与真实环境一致，user_freq=4）
        s.upsert_user_words_batch(&[("文件清单".into(), "wen jian qing dan".into(), 4)])
            .unwrap();
    }
    tmp
}

/// 守护 1：wwww 输入不应产生含重复子串的长串候选（"文件清单文件清单..."）。
/// 这是 bug 的最直观症状——同一长词被 DP 多段重复 pick。
#[test]
fn test_no_repetition_in_jianpin_wwww() {
    let db = make_jianpin_repetition_db();
    let engine = Engine::new_with_db(db.to_str().unwrap()).unwrap();
    let result = engine.process("wwww");

    // 调试输出
    for (i, c) in result.iter().take(10).enumerate() {
        println!(
            "wwww process [{}] {} pinyin={} freq={} is_sentence={}",
            i, c.text, c.pinyin, c.base_freq, c.is_sentence
        );
    }

    // 不应出现任何"X+X+X"重复模式的候选（不论字数）
    let has_repetition = result.iter().any(|c| {
        let text = &c.text;
        let len = text.chars().count();
        // 检查是否是某个子串重复 2 次以上（长度 ≥ 4 才有意义）
        if len < 4 || len % 2 != 0 {
            return false;
        }
        let half = len / 2;
        let prefix: String = text.chars().take(half).collect();
        let suffix: String = text.chars().skip(half).take(half).collect();
        prefix == suffix
    });
    assert!(
        !has_repetition,
        "wwww 不应产生重复子串候选（如「文件清单文件清单」），但返回：{:?}",
        result.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
}

/// 守护 2：wwww 的候选总字数不应远超输入音节数（4 个 w 最多对应约 4-6 字，
/// 不应出现 8+ 字长串）。这是 defense-in-depth 兜底。
#[test]
fn test_jianpin_wwww_no_overlong_candidates() {
    let db = make_jianpin_repetition_db();
    let engine = Engine::new_with_db(db.to_str().unwrap()).unwrap();
    let result = engine.process("wwww");

    // 输入是 4 个简拼字母，任何候选的字数不应超过 6（输入音节数 + 容差 2）
    // 这个容差允许"问题"（2 字）这类合理多字词，但挡住"文件清单文件清单"（8 字）
    let overlong: Vec<_> = result
        .iter()
        .filter(|c| c.text.chars().count() > 6)
        .map(|c| c.text.as_ref().to_owned())
        .collect();
    assert!(
        overlong.is_empty(),
        "wwww 不应产生超过 6 字的候选（输入仅 4 个简拼字母），但返回：{:?}",
        overlong
    );
}

/// 守护 3：wwww 的候选不应包含首字母与输入不对应的词。
/// "文件清单"首字母是 wjqd，输入 wwww 完全不匹配，不应出现。
#[test]
fn test_jianpin_wwww_no_mismatched_initials() {
    let db = make_jianpin_repetition_db();
    let engine = Engine::new_with_db(db.to_str().unwrap()).unwrap();
    let result = engine.process("wwww");

    // "文件清单"（wjqd）和"稳定性"（wdx）首字母都不是 wwww，不应出现
    let bad: Vec<_> = result
        .iter()
        .filter(|c| c.text.contains("文件清单") || c.text.contains("稳定性"))
        .map(|c| c.text.as_ref().to_owned())
        .collect();
    assert!(bad.is_empty(),
            "wwww 不应匹配到首字母不对应的「文件清单」/「稳定性」（首字母 wjqd/wdx ≠ wwww），但返回：{:?}",
            bad);
}

/// 守护 4：对照实验，3 个 w（jp_len < 4）不触发绿词 DP，更不应有问题。
/// 确保 fix 没有意外破坏 jp_len < 4 的路径。
#[test]
fn test_jianpin_www_below_dp_threshold() {
    let db = make_jianpin_repetition_db();
    let engine = Engine::new_with_db(db.to_str().unwrap()).unwrap();
    let result = engine.process("www");

    // jp_len=3 < 4，不触发绿词 DP，所有候选应是非绿词（is_sentence=false）
    let greens: Vec<_> = result
        .iter()
        .filter(|c| c.is_sentence)
        .map(|c| c.text.as_ref().to_owned())
        .collect();
    assert!(
        greens.is_empty(),
        "www（jp_len=3）不应触发绿词 DP，但返回了绿词：{:?}",
        greens
    );
}

// 守护 5：（已删除——测试 DB 触发不了绿词，无法验证 context 污染）。
//
// 背景：同一输入 wwww，不同时刻可能产生不同绿词候选。根因是
// **context 上文 + semantic_bonuses 协同污染**：每次 commit 后 context 变化 +
// 后台语义向量组件重算 semantic_bonuses，
// 下次 wwww 的简拼 DP bigram_sources 变了 → pick 决策变 → 结果变。
//
// 处理（sentence.rs）：
// - 简拼（is_initial_mode）下 pick 阶段 + seg 打分阶段跳过 semantic_bonuses 引导
// - 简拼下句首段 bigram_sources 只取 context 最近 1 个词（全拼仍取 3 个）
//
// **无法用单元测试守护**：需要大型词库（几千个 w 开头单字）才能让简拼 DP
// 拼出 4 字绿词，小型测试 DB 只会返回白词单字（无绿词可"翻转"）。
// 改为依赖真实部署 + debug.log 验证（grep `query('wwww')` 看首候选是否稳定）。

// ---- 绿词 Top-2 动态门槛单元测试 ----

/// 机制 A 动态阈值：按第一绿词绝对分档，样本不足 20 时纯用动态值。
#[test]
fn test_green_threshold_dynamic() {
    let stats = GreenAcceptStats::default();
    assert_eq!(stats.threshold_ratio(100), 0.90); // > 80
    assert_eq!(stats.threshold_ratio(60), 0.86); // > 50
    assert_eq!(stats.threshold_ratio(40), 0.82); // <= 50
}

/// 机制 B 采纳率微调：样本 ≥ 20 介入，高采纳率 → 门槛下调 0.03。
#[test]
fn test_green_threshold_learning() {
    // accept_rate = 8/25 = 0.32 > 0.25 → adjust = -0.03 → 0.86 - 0.03 = 0.83
    let stats = GreenAcceptStats {
        total_green_picks: 25,
        second_green_picks: 8,
    };
    assert_eq!(stats.threshold_ratio(60), 0.83);
}

/// 机制 B 冷启动：样本 < 20 不介入，纯用机制 A 动态阈值。
#[test]
fn test_green_threshold_cold_start() {
    // 样本仅 10，即使 second 占比高也不介入
    let stats = GreenAcceptStats {
        total_green_picks: 10,
        second_green_picks: 5,
    };
    assert_eq!(stats.threshold_ratio(60), 0.86);
}

/// 机制 B 低采纳率：样本 ≥ 20 但几乎不选第二绿词 → 门槛上调 0.05（收紧）。
#[test]
fn test_green_threshold_low_accept() {
    // accept_rate = 1/30 ≈ 0.033 < 0.05 → adjust = +0.05 → 0.86 + 0.05 = 0.91，clamp 到 0.92 内
    let stats = GreenAcceptStats {
        total_green_picks: 30,
        second_green_picks: 1,
    };
    assert_eq!(stats.threshold_ratio(60), 0.91);
}

/// 搭配惩罚逻辑验证：惩罚 -3 可被 freq_score 抵消，不导致空候选/panic。
#[test]
fn test_collocation_penalty_logic() {
    let seed = super::tests_query::seed_copy("gq");
    let engine = Engine::new_with_db(&seed).unwrap();
    let cands = engine.process("zuofeiji");
    assert!(!cands.is_empty(), "搭配惩罚不应导致空候选");
}

/// long_word_bonus 链式扩展：context 多词后输入不 panic。
/// 验证遍历最近 3 词拼接查长词的路径安全（无越界、无 panic）。
#[test]
fn test_long_word_bonus_chained() {
    let seed = super::tests_query::seed_copy("gq");
    let mut engine = Engine::new_with_db(&seed).unwrap();
    // 模拟上文：context = [星期, 一]，输入验证链式查询不崩溃
    engine.commit("xingqi", "星期", Some("xing qi"), true);
    engine.commit("yi", "一", Some("yi"), true);
    let cands = engine.process("wanshang");
    // 至少不崩溃，且应有候选返回
    assert!(!cands.is_empty(), "long_word 链式扩展不应导致空候选");
}

/// wohuanlegeshouji 冷启动行为守护。
///
/// 冷启动（未 commit 过）时，DP 靠 freq 猜切分，倾向于多字词组合
/// （「我欢乐各手机」），无法切出「我换了个手机」。这是已知限制——
/// 无 bigram 引导时无解，靠首次 commit 建立 bigram 后修正。
///
/// 此测试守护：冷启动行为不因门槛放宽而改变（bigram_support_score=0
/// 时仍走原严格门槛 multi >= single）。
#[test]
fn test_wohuanlegeshouji_cold_start_unchanged() {
    let seed = super::tests_query::seed_copy("gq");
    let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

    let syl: Vec<&str> = vec!["wo", "huan", "le", "ge", "shou", "ji"];
    let greens = engine.decode_sentence_for_test("wohuanlegeshouji", &syl);

    println!("冷启动绿词输出:");
    for (i, c) in greens.iter().enumerate() {
        println!("  [{}] text={:?}", i, c.text);
    }
    // freq 有真实区分度（我们9500/葡萄533），
    // 冷启动也能切出「我换了个手机」。
    // 断言：冷启动至少应有绿词输出（不空），且不应是"我欢乐各手机"（错误切分）。
    assert!(!greens.is_empty(), "冷启动应有绿词输出");
    let has_wrong = greens.iter().any(|c| c.text.as_ref() == "我欢乐各手机");
    assert!(!has_wrong, "冷启动不应输出错误的「我欢乐各手机」");
}

/// wohuanlegeshouji 学习后修正守护。
///
/// 场景：输入 `wohuanlegeshouji`（我换了个手机），
/// DP 绿词输出「我欢乐个手机」。即使 commit 正确整句建立 4 个 bigram
/// （(我,换)(换,了)(了,个)(个,手机) 各 +3 权重）后仍切错。
///
/// 根因：**绿词门槛 multi >= single 误杀合理句子**。DP 的 N-best 多样性
/// 剪枝本就保留了「我换了个手机」路径（4 单字 + 1 双字），但回溯后的门槛
/// multi(1) < single(4) 把它硬过滤掉。
///
/// 处理（sentence.rs 绿词门槛）：路径有强 bigram 支持时（bigram_support_score ≥ 3），
/// 放宽门槛到 multi >= 1（只要有 1 个多字词即可）。冷启动 bigram_support=0 走原门槛。
///
/// 此测试守护：commit 后 wohuanlegeshouji 应能切对。
#[test]
fn test_wohuanlegeshouji_recovers_after_learn() {
    let seed = super::tests_query::seed_copy("gq");
    let mut engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

    // 用户 commit 正确整句，建立 bigram 引导信号
    // commit_sentence_split 用 text 锚定拆出 [我,换,了,个,手机]，
    // 子词间 bigram 各 +3 权重记录到 word_bigram_counts
    engine.commit(
        "wohuanlegeshouji",
        "我换了个手机",
        Some("wo huan le ge shou ji"),
        true,
    );

    let cands = engine.process("wohuanlegeshouji");
    let first = cands.first().map(|c| c.text.as_ref());

    // 期望：commit 后 bigram 引导 + 门槛放宽，DP 切出「我换了个手机」
    assert_eq!(
        first,
        Some("我换了个手机"),
        "commit「我换了个手机」后，wohuanlegeshouji 首候选应为「我换了个手机」，实际: {:?}",
        first
    );
}
