//! 末尾单辅音按末音节缩写参与整句 DP 的守护测试。

use super::tests_query::{seed_copy, seed_copy_cleanup};
use super::Engine;

fn seed_engine(tag: &str) -> (String, Engine) {
    let db = seed_copy(tag);
    let mut engine = Engine::new_with_db(&db).expect("open seed copy");
    engine.set_smart_word_zh_enabled(false);
    (db, engine)
}

fn top1(engine: &Engine, input: &str) -> String {
    engine
        .process(input)
        .first()
        .map(|c| c.text.to_string())
        .unwrap_or_default()
}

fn has_sentence(engine: &Engine, input: &str, text: &str) -> bool {
    engine
        .process(input)
        .iter()
        .any(|c| c.is_sentence && c.text.as_ref() == text)
}

/// 核心回归：两个典型输入都必须解出「需求研究」。
///
/// 关键是冷启动种子（没有任何 commit/green_memo）——证明靠的是引擎
/// 组词能力，不是记忆命中。
#[test]
fn test_trailing_consonant_completes_abbreviated_last_syllable() {
    let (db, engine) = seed_engine("tail_initial_core");

    // 3 全拼 + 1 简拼
    assert_eq!(
        top1(&engine, "xuqiuyanj"),
        "需求研究",
        "xuqiuyanj 必须解出「需求研究」（末尾 j 是「究」的首字母）"
    );
    assert_eq!(
        top1(&engine, "womendeyanj"),
        "我们的研究",
        "womendeyanj 必须解出「我们的研究」（中段「的」需要单字边）"
    );
    // 全拼形态不回归
    assert_eq!(top1(&engine, "xuqiuyanjiu"), "需求研究");

    seed_copy_cleanup(&db);
}

/// 单调细化：同一个词的输入从粗到细，正确候选不得消失。
///
/// 设计目标：「更精确的输入不该比更粗略的输入更差」。
///
/// 阶梯只取真正处于缩写态的形态：
/// - `xuqiuyanj`（3 全拼 + 1 简拼）→ 本机制的核心场景；
/// - `xuqiuyanjiu`（全拼）→ 不回归。
///
/// 刻意排除两种形态（它们在物理上没有「更精确」语义）：
/// - `xuqiuyj`：2 全拼 + 2 简拼时 `y|j` 的组词候选很多（需求已经/需求研究…），
///   LM 按语料选择「需求已经」是正确行为；「需求研究」依赖 green_memo 学习
///   （见 test_learned_word_reachable_across_input_forms）。
/// - `xuqiuyanji`：末音节 `ji` 本身就是完整合法音节（对应 机/极/几），
///   引擎必须按完整音节解释，不能当 `jiu` 的缩写——否则那些字打不出来。
#[test]
fn test_refinement_is_monotonic() {
    let (db, engine) = seed_engine("tail_initial_monotonic");
    let ladder = [
        "xuqiuyanj",   // 3 全拼 + 1 简拼（末音节 j 缩写）
        "xuqiuyanjiu", // 全拼
    ];
    let mut missing = Vec::new();
    for input in ladder {
        let pos = engine
            .process(input)
            .iter()
            .position(|c| c.is_sentence && c.text.as_ref() == "需求研究");
        match pos {
            Some(0) => {}
            other => missing.push(format!("{input} → {other:?}")),
        }
    }
    assert!(
        missing.is_empty(),
        "细化阶梯上「需求研究」必须始终是首候选，实际断点: {missing:?}"
    );
    seed_copy_cleanup(&db);
}

/// 统一规则②a：裸尾单字母零声母 + 有延长词证据（jisua←计算 ji'suan）
/// → 半截语义：不出语气词绿词（续打 jisuan 中途不闪「激素啊」）；最佳延长
/// 词以紫词注入且 freq 达标置顶（jisua→计算 #1，选中全消费）。
/// 对照分支：显式撇号 jisu'a 恒语气词语义（激素啊 可组）。
#[test]
fn test_trailing_vowel_syllable_keeps_partial_tail_behavior() {
    let (db, engine) = seed_engine("tail_initial_vowel");
    let cands = engine.process("jisua");
    let first = cands
        .first()
        .map(|c| c.text.to_string())
        .unwrap_or_default();
    assert_eq!(first, "计算", "jisua 应出延长词「计算」置顶（2253>激素1503）");
    assert!(
        !cands.iter().any(|c| c.is_sentence),
        "jisua 续打中途不得出语气词绿词（激素啊），实际: {:?}",
        cands
            .iter()
            .filter(|c| c.is_sentence)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
    // 显式撇号：用户显式断音节 → 恒语气词语义（统一规则③）
    let apos = engine.process("jisu'a");
    assert!(
        apos.iter().any(|c| c.text.as_ref() == "激素啊" && c.is_sentence),
        "jisu'a 显式撇号应可组「激素啊」，实际: {:?}",
        apos.iter().take(5).map(|c| c.text.as_ref()).collect::<Vec<_>>()
    );
    seed_copy_cleanup(&db);
}

/// 学过的词在所有输入形态下都可达（用户真实路径：先学 xuqiuyj，再细化）。
#[test]
fn test_learned_word_reachable_across_input_forms() {
    let (db, mut engine) = seed_engine("tail_initial_learned");
    engine.commit("xuqiuyj", "需求研究", Some("xu qiu yan jiu"), true);
    engine.context_clear_for_test();

    for input in ["xuqiuyj", "xuqiuyanj", "xuqiuyanjiu"] {
        assert!(
            has_sentence(&engine, input, "需求研究"),
            "{input} 应仍能取到学过的「需求研究」（可达性，绿词候选）"
        );
        assert_eq!(
            top1(&engine, input),
            "需求研究",
            "{input} 学过后应是首候选（细化不回退）"
        );
    }
    seed_copy_cleanup(&db);
}

/// 平滑性：partial 过渡态不得整体消失（既有行为不回归）。
#[test]
fn test_partial_tail_still_produces_sentence_for_consonant_tail() {
    let (db, engine) = seed_engine("tail_initial_smooth");
    // 末尾 h（希望你会/你和…）—— 首候选必须以「希望」开头，不得退回单词
    for input in ["xiwangnih", "xiwangnihaoh"] {
        let first = top1(&engine, input);
        assert!(
            first.starts_with("希望"),
            "{input} 首候选「{first}」应以「希望」开头（整句不应整体回退）"
        );
    }
    seed_copy_cleanup(&db);
}

/// 回归：buheshiyid（不合时宜 + d）候选整窗消失。
/// 根因是句段候选排序的成对守卫不满足全序 → Rust sort panic → FFI 返回空。
/// 必须持续给出「不合时宜…」候选（尾 d 视作末音节缩写继续组词）。
#[test]
fn test_tail_d_after_buheshiyi_keeps_candidates() {
    let (db, engine) = seed_engine("tail_initial_buheshiyid");
    let cands = engine.process("buheshiyid");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    assert!(
        !cands.is_empty(),
        "buheshiyid 不应返回空候选（排序全序违规 panic 回归）"
    );
    assert!(
        texts.iter().any(|t| t.starts_with("不合时宜")),
        "应继续给出「不合时宜…」候选: {texts:?}"
    );
    seed_copy_cleanup(&db);
}

/// 纠错撤销：提交后回滚上一次跨词 n-gram（bigram + trigram）；
/// 候选内部词对走 revert_bigram。清到 0 后 store 应为 None（墓碑）。
#[test]
fn test_revert_last_commit_ngrams() {
    let (db, mut engine) = seed_engine("revert_last_commit");
    engine.context_clear();
    engine.commit("nihao", "你好", Some("ni hao"), true);
    engine.commit("shijie", "世界", Some("shi jie"), true);
    engine.commit("ceshi", "测试", Some("ce shi"), true);
    assert_eq!(engine.freq.user_bigram.get("世界", "测试"), Some(1));
    assert_eq!(engine.trigram.count("你好", "世界", "测试"), 1);
    assert!(engine.revert_last_commit(), "应回滚上一次提交的 n-gram");
    assert_eq!(
        engine.freq.user_bigram.get("世界", "测试"),
        None,
        "跨词 bigram 应回滚到不存在"
    );
    assert_eq!(
        engine.trigram.count("你好", "世界", "测试"),
        0,
        "trigram 应回滚到不存在"
    );
    assert!(!engine.revert_last_commit(), "快照消费后不可重复撤销");

    // 候选内部词对（前端记录）逐对回滚
    engine.freq.record_word_bigram_weighted("今天", "测试", 1);
    assert_eq!(engine.freq.user_bigram.get("今天", "测试"), Some(1));
    assert!(engine.revert_bigram("今天", "测试", 1));
    assert_eq!(engine.freq.user_bigram.get("今天", "测试"), None);
    seed_copy_cleanup(&db);
}

/// 删词连带清 trigram（不只 bigram）、pair 级清理、学习观测量。
#[test]
fn test_trigram_forget_and_learning_stats() {
    let (db, mut engine) = seed_engine("trigram_forget");
    engine.context_clear();
    engine.commit("nihao", "你好", Some("ni hao"), true);
    engine.commit("shijie", "世界", Some("shi jie"), true);
    engine.commit("ceshi", "测试", Some("ce shi"), true);
    assert_eq!(engine.trigram.count("你好", "世界", "测试"), 1);
    assert_eq!(engine.freq.user_bigram.get("你好", "世界"), Some(1));

    // 删词：bigram + trigram 双层连带清理（此前 trigram 会残留）
    engine.remove("世界");
    assert_eq!(
        engine.trigram.count("你好", "世界", "测试"),
        0,
        "删词应清 trigram 边"
    );
    assert_eq!(
        engine.freq.user_bigram.get("你好", "世界"),
        None,
        "删词应清 bigram 边"
    );

    // pair 级清理（审计/维护入口）；幂等
    engine.context_clear();
    engine.freq.record_word_bigram_weighted("今天", "测试", 1);
    engine.trigram.record_weighted("今天", "天气", "测试", 1);
    assert!(engine.forget_bigram_pair("今天", "测试"));
    assert_eq!(engine.freq.user_bigram.get("今天", "测试"), None);
    assert!(!engine.forget_bigram_pair("今天", "测试"));
    assert!(engine.forget_trigram_pair("今天", "天气", "测试"));
    assert_eq!(engine.trigram.count("今天", "天气", "测试"), 0);
    assert!(!engine.forget_trigram_pair("今天", "天气", "测试"));

    // 观测计数：3 次 commit 中后两次记录 bigram、第三次记录 trigram；
    // 开局 + 中间各一次 context_clear。
    let stats = engine.learning_stats_json();
    assert!(stats.contains("\"bigrams_recorded\":2"), "stats={stats}");
    assert!(stats.contains("\"trigrams_recorded\":1"), "stats={stats}");
    assert!(stats.contains("\"context_clears\":2"), "stats={stats}");
    seed_copy_cleanup(&db);
}

/// 噪声治理观测计数开关门控：关闭后学习行为不变（bigram/trigram 照记），
/// 仅观测计数停走；重开恢复计数。
/// 守护 FFI x1_engine_set_learning_stats_enabled 背后的引擎侧语义。
#[test]
fn test_learning_stats_toggle_gates_counters_only() {
    let db = seed_copy("stats_gate");
    let mut engine = super::Engine::new_with_db(&db).expect("seed db");

    // 关闭：commit/context_clear 的学习照常，计数全零
    engine.set_learning_stats_enabled(false);
    engine.commit("nihao", "你好", Some("ni hao"), true);
    engine.commit("shijie", "世界", Some("shi jie"), true);
    engine.context_clear();
    let stats = engine.learning_stats_json();
    assert!(stats.contains("\"bigrams_recorded\":0"), "stats={stats}");
    assert!(stats.contains("\"trigrams_recorded\":0"), "stats={stats}");
    assert!(stats.contains("\"context_clears\":0"), "stats={stats}");
    assert_eq!(
        engine.freq.user_bigram.get("你好", "世界"),
        Some(1),
        "关闭的只是观测计数，bigram 学习本体必须照记"
    );

    // 重开：context_clear 已清空上下文栈，重建三连后新学习恢复计数
    engine.set_learning_stats_enabled(true);
    engine.commit("nihao", "你好", Some("ni hao"), true);
    engine.commit("shijie", "世界", Some("shi jie"), true);
    engine.commit("ceshi", "测试", Some("ce shi"), true);
    let stats = engine.learning_stats_json();
    assert!(stats.contains("\"bigrams_recorded\":2"), "stats={stats}");
    assert!(stats.contains("\"trigrams_recorded\":1"), "stats={stats}");
    seed_copy_cleanup(&db);
}

/// 学习噪声治理：上下文 tail 查询与清理（前端提交前文档连通性
/// 校验 / 切应用时使用）。清理后 tail 必须为空，后续提交从干净上下文开始。
#[test]
fn test_context_tail_and_clear() {
    let (db, mut engine) = seed_engine("ctx_tail_clear");
    engine.context_clear();
    engine.commit("nihao", "你好", Some("ni hao"), true);
    engine.commit("shijie", "世界", Some("shi jie"), true);
    assert_eq!(engine.context_tail(2), vec!["你好", "世界"]);
    engine.context_clear();
    assert!(engine.context_tail(2).is_empty());
    seed_copy_cleanup(&db);
}
