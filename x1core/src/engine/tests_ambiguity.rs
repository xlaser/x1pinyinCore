//! 同音歧义学习链守护：上下文证据重排、槽级选择记忆与整句提交学习的行为测试。

use super::tests_query::{seed_copy, seed_copy_cleanup};
use super::Engine;

fn seed_engine(tag: &str) -> (String, Engine) {
    let db = seed_copy(tag);
    let mut engine = Engine::new_with_db(&db).expect("open seed copy");
    engine.set_smart_word_zh_enabled(false);
    (db, engine)
}

fn green_texts(cands: &[crate::dict::Candidate]) -> Vec<&str> {
    cands
        .iter()
        .filter(|c| c.is_sentence)
        .map(|c| c.text.as_ref())
        .collect()
}

/// 守护：整句已精确解出时不得注入同拼音猜测绿词。
///
/// 这些输入最容易诱发出垃圾读法绿词（别生漆 / 握在路上 / 我快刀乐 / 明皂碱…），
/// 必须只有 1 条绿词。
#[test]
fn test_no_blind_homophone_green_injection() {
    let (db, engine) = seed_engine("ambig_no_blind_inject");
    let noisy = [
        "woxiangheshui", // 我想喝水
        "bieshengqi",    // 别生气
        "wozailushang",  // 我在路上
        "wokuaidaole",   // 我快到了
        "mingzaojian",   // 明早见
        "mashangchufa",  // 马上处罚（旗舰同音族）
    ];
    for input in noisy {
        let cands = engine.process(input);
        let greens = green_texts(&cands);
        assert!(
            greens.len() <= 1,
            "{input} 不应注入猜测绿词（≤1 条绿词），实际 {greens:?}"
        );
    }
    seed_copy_cleanup(&db);
}

/// 词级 PinCtx：同一语境学一次即翻盘，其他语境不受影响。
#[test]
fn test_context_evidence_flips_only_in_learned_context() {
    let (db, mut engine) = seed_engine("ambig_ctx_scoped");

    engine.context_push_for_test("灵感");
    let before = engine.process("chufa");
    assert_eq!(before[0].text.as_ref(), "处罚", "学习前应保持词频默认");
    engine.context_clear_for_test();

    // 提交「触发」（chufa 整词）——PinCtx 上下文证据 + 全局 Pin 各记一次。
    engine.context_push_for_test("灵感");
    engine.commit("chufa", "触发", Some("chu fa"), true);
    engine.context_clear_for_test();

    engine.context_push_for_test("灵感");
    let learned = engine.process("chufa");
    assert_eq!(
        learned[0].text.as_ref(),
        "触发",
        "同一语境学一次后应翻盘，实际: {:?}",
        learned
            .iter()
            .take(5)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
    let trigger = learned
        .iter()
        .find(|c| c.text.as_ref() == "触发")
        .expect("触发 候选存在");
    assert!(
        engine.score_breakdown("chufa", trigger).ctx_homophone_bonus > 0,
        "学过的语境应计入上下文同音证据分"
    );
    // 证据本身的作用域：同一词在未学语境为零。
    assert_eq!(
        engine.pin_ctx_boost("chu fa", "触发", "领导"),
        0,
        "上下文证据不得扩散到未学过的语境"
    );
    engine.context_clear_for_test();

    engine.context_push_for_test("领导");
    let other = engine.process("chufa");
    let trigger_other = other
        .iter()
        .find(|c| c.text.as_ref() == "触发")
        .expect("触发 候选存在");
    assert_eq!(
        engine
            .score_breakdown("chufa", trigger_other)
            .ctx_homophone_bonus,
        0,
        "其他语境的上下文证据分必须为 0"
    );
    seed_copy_cleanup(&db);
}

/// 整句提交也要把**歧义子词**的上下文证据记到句前上文词上，
/// 供之后同语境的单词输入使用。
#[test]
fn test_sentence_commit_records_context_evidence_for_slot() {
    let (db, mut engine) = seed_engine("ambig_ctx_sentence");
    engine.context_push_for_test("灵感");
    engine.commit("mashangchufa", "马上触发", Some("ma shang chu fa"), true);
    engine.context_clear_for_test();

    let total = engine
        .competition
        .total_of(&crate::competition::CtxKey::PinCtx {
            pinyin: "chu fa".to_string(),
            prev: "灵感".to_string(),
        });
    assert!(
        total >= 1,
        "整句提交应记录槽级上下文证据，实际 total={total}"
    );

    engine.context_push_for_test("灵感");
    let cands = engine.process("chufa");
    assert_eq!(cands[0].text.as_ref(), "触发", "同语境单词输入应受益");
    engine.context_clear_for_test();

    engine.context_push_for_test("领导");
    let other = engine.process("chufa");
    assert_eq!(other[0].text.as_ref(), "处罚", "其他语境不受影响");
    seed_copy_cleanup(&db);
}

/// 选过一次后同输入 DP 学习翻转（既有子词 bigram 链，不依赖任何句级注入）。
#[test]
fn test_commit_learns_bigram_and_flips_next_query() {
    let (db, mut engine) = seed_engine("ambig_dp_learn");
    let before = engine.process("mashangchufa");
    assert_eq!(before[0].text.as_ref(), "马上处罚", "基线默认");

    engine.commit("mashangchufa", "马上出发", Some("ma shang chu fa"), true);
    engine.context_clear_for_test();

    assert!(
        engine.freq.word_bigram_count("马上", "出发") > 0,
        "选择后应记录子词 bigram (马上→出发)"
    );
    let again = engine.process("mashangchufa");
    assert_eq!(
        again[0].text.as_ref(),
        "马上出发",
        "选过一次后同输入首候选应翻转，实际: {:?}",
        again
            .iter()
            .take(5)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
    seed_copy_cleanup(&db);
}

/// 上下文证据经 flush → 重启后仍生效（competition layer 4 持久化）。
#[test]
fn test_context_evidence_survives_restart() {
    let (db, mut engine) = seed_engine("ambig_ctx_persist");
    engine.context_push_for_test("灵感");
    engine.commit("chufa", "触发", Some("chu fa"), true);
    engine.flush().expect("flush competition");
    drop(engine);

    let mut engine2 = Engine::new_with_db(&db).expect("reopen seed copy");
    engine2.set_smart_word_zh_enabled(false);
    engine2.context_push_for_test("灵感");
    let cands = engine2.process("chufa");
    assert_eq!(
        cands[0].text.as_ref(),
        "触发",
        "上下文证据应跨重启保留，实际: {:?}",
        cands
            .iter()
            .take(5)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
    seed_copy_cleanup(&db);
}
