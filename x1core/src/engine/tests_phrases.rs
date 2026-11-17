//! Engine 测试 · 自定义短语（user_phrases）。
//!
//! 覆盖：
//! - 精确触发码命中 → 短语置顶（第一候选位，is_shortcut）
//! - 一码多条按添加顺序全注入
//! - 精确匹配（前缀不触发）
//! - 英文/特殊符号短语文本不被 ASCII 过滤
//! - 上屏不参与学习（freq/input_mappings 零污染）
//! - 持久化（重启引擎仍在）+ 触发码大小写归一

use super::*;

/// 真实词库引擎（种子 DB 复制到临时文件，写路径互不影响）。
fn make_engine(tag: &str) -> Engine {
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let tmp = crate::test_tmp::dir().join(format!("x1pinyin_test_phrases_{}.db", tag));
    let _ = std::fs::remove_file(&tmp);
    std::fs::copy(seed, &tmp).expect("copy seed db");
    Engine::new_with_db(tmp.to_str().unwrap()).expect("open engine")
}

/// 直写 storage 新增短语 + reload（模拟词库管理器改库后 notify_reload）。
fn add_phrase(e: &mut Engine, trigger: &str, text: &str) {
    e.storage
        .as_mut()
        .unwrap()
        .insert_phrase(trigger, text)
        .unwrap();
    e.reload_user_state().unwrap();
}

#[test]
fn test_phrase_first_position() {
    let mut e = make_engine("first");
    add_phrase(&mut e, "noip", "信息学省级联赛");
    let cands = e.process("noip");
    assert!(!cands.is_empty());
    assert_eq!(
        cands[0].text.as_ref(),
        "信息学省级联赛",
        "短语必须第一候选位"
    );
    assert!(cands[0].is_shortcut, "短语带快捷标记");
}

#[test]
fn test_phrase_multiple_per_trigger() {
    let mut e = make_engine("multi");
    add_phrase(&mut e, "noip", "信息学省级联赛");
    add_phrase(&mut e, "noip", "全国青少年信息学奥林匹克联赛");
    let cands = e.process("noip");
    assert_eq!(cands[0].text.as_ref(), "信息学省级联赛", "先添加的在前");
    assert_eq!(cands[1].text.as_ref(), "全国青少年信息学奥林匹克联赛");
}

#[test]
fn test_phrase_exact_match_only() {
    let mut e = make_engine("exact");
    add_phrase(&mut e, "noip", "信息学省级联赛");
    // 前缀不触发（精确匹配语义）
    let prefix = e.process("noi");
    assert!(
        !prefix.iter().any(|c| c.text.as_ref() == "信息学省级联赛"),
        "前缀 noi 不应触发 noip 短语"
    );
    // 更长输入也不触发
    let longer = e.process("noipx");
    assert!(!longer.iter().any(|c| c.text.as_ref() == "信息学省级联赛"));
}

#[test]
fn test_phrase_english_and_symbols() {
    let mut e = make_engine("symbol");
    add_phrase(&mut e, "hw", "hello world");
    add_phrase(&mut e, "xuhao", "①②③★");
    add_phrase(&mut e, "mail", "user@example.com");
    let hw = e.process("hw");
    assert_eq!(
        hw[0].text.as_ref(),
        "hello world",
        "英文短语不被 ASCII 过滤"
    );
    let xh = e.process("xuhao");
    assert_eq!(xh[0].text.as_ref(), "①②③★", "特殊符号短语保留");
    let mail = e.process("mail");
    assert_eq!(mail[0].text.as_ref(), "user@example.com");
}

#[test]
fn test_phrase_no_learning() {
    let mut e = make_engine("nolearn");
    add_phrase(&mut e, "noip", "信息学省级联赛");
    // 短语上屏（commit）不进任何学习面
    e.commit("noip", "信息学省级联赛", None, true);
    assert!(
        !e.input_mappings
            .contains_key(&("noip".to_string(), "信息学省级联赛".to_string())),
        "短语上屏不写 input_mappings"
    );
    assert_eq!(
        e.freq.counts.get("信息学省级联赛").copied().unwrap_or(0),
        0,
        "短语上屏不增词频"
    );
    // 再次查询短语仍置顶（未被学习流程改写）
    assert_eq!(e.process("noip")[0].text.as_ref(), "信息学省级联赛");
}

/// 关联通道：短语上屏 = opaque 整词 token 参与关联——
/// 推进上下文 + 双向 user bigram；词频/造词/绿词/映射仍全隔离。
#[test]
fn test_phrase_association_channel_open() {
    let mut e = make_engine("assoc");
    add_phrase(&mut e, "noip", "信息学省级联赛");
    e.commit("nihao", "你好", Some("ni hao"), true);
    e.commit("noip", "信息学省级联赛", None, true);
    // 上下文推进：栈顶 = 短语（下文衔接不断链）
    assert_eq!(
        e.context.last().map(|s| s.as_str()),
        Some("信息学省级联赛"),
        "短语上屏应推进上下文栈"
    );
    // 双向 bigram：(你好→短语) 与 (短语→比赛) 都进 user 表
    e.commit("bisai", "比赛", Some("bi sai"), true);
    let (_, _, used_fwd) = e.bigram_source_info("你好", "信息学省级联赛");
    assert_eq!(used_fwd, "user", "(前词→短语) 应记 user bigram");
    let (_, _, used_bwd) = e.bigram_source_info("信息学省级联赛", "比赛");
    assert_eq!(used_bwd, "user", "(短语→后词) 应记 user bigram");
    // 隔离保持：词频/映射/造词/绿词零污染
    assert_eq!(e.freq.counts.get("信息学省级联赛").copied().unwrap_or(0), 0);
    assert!(!e
        .input_mappings
        .contains_key(&("noip".to_string(), "信息学省级联赛".to_string())));
    assert!(!e.user_words.contains_key("信息学省级联赛"));
}

/// advance_context=false（自组词分步语义）不推进不记；撤销快照可回滚短语关联。
#[test]
fn test_phrase_association_gates_and_revert() {
    let mut e = make_engine("assoc_gate");
    add_phrase(&mut e, "noip", "信息学省级联赛");
    e.commit("nihao", "你好", Some("ni hao"), true);
    e.commit("noip", "信息学省级联赛", None, false);
    assert_eq!(
        e.context.last().map(|s| s.as_str()),
        Some("你好"),
        "advance_context=false 不推进栈"
    );
    let (_, _, used) = e.bigram_source_info("你好", "信息学省级联赛");
    assert_ne!(used, "user", "advance_context=false 不记 bigram");
    // 正常推进后撤销 = 回滚本次短语跨词学习（快照同普通提交口径）
    e.commit("noip", "信息学省级联赛", None, true);
    let (_, _, used2) = e.bigram_source_info("你好", "信息学省级联赛");
    assert_eq!(used2, "user");
    e.revert_last_commit();
    let (_, _, used3) = e.bigram_source_info("你好", "信息学省级联赛");
    assert_ne!(used3, "user", "撤销应回滚短语 bigram");
}

#[test]
fn test_phrase_persistence_and_case() {
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let tmp = crate::test_tmp::dir().join("x1pinyin_test_phrases_persist.db");
    let _ = std::fs::remove_file(&tmp);
    std::fs::copy(seed, &tmp).expect("copy seed db");
    let path = tmp.to_str().unwrap().to_string();
    {
        let mut e = Engine::new_with_db(&path).expect("open");
        // 大写触发码归一存储
        add_phrase(&mut e, "NOIP", "信息学省级联赛");
    } // engine dropped
    let e2 = Engine::new_with_db(&path).expect("reopen");
    let cands = e2.process("noip");
    assert_eq!(
        cands[0].text.as_ref(),
        "信息学省级联赛",
        "重启后短语仍在 + 大写触发码归一匹配小写输入"
    );
    let _ = std::fs::remove_file(&tmp);
}
