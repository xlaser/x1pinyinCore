//! Engine 测试 · 快捷输入（/fh 触发的符号候选，单一触发词版）。
//!
//! 触发场景：用户输入 `/fh`，Swift 端把 `/fh` 送进 buffer，引擎 `try_quick_input`
//! 拦截后查 symbols 模块的 ALL_SYMBOLS（所有符号合并为一组）产生候选。
//! 候选标记 is_shortcut=true，蓝色显示。
//!
//! **单一触发词设计**：所有符号合并到 /fh 一个入口，
//! 不再用 /sx /jt /lm 等分类触发词（避免记忆负担）。翻页选取即可。
//!
//! 注：日期时间（/date /time /week）由 Swift 端 Date+DateFormatter 生成，
//! 不走 Rust 拦截，故本文件不测试日期时间。
//!
//! mod.rs 用 `#[cfg(test)] mod tests_quick_input;` 加载（分号形式从文件加载）。
//! `use super::*` 仍指向 engine mod，测试访问 engine 的 pub 项不变。

use super::*;

/// 用真实种子 DB 构造 engine（快捷输入不依赖词库内容，用种子 DB 即可）。
/// seed_copy 临时副本：直接开仓库种子会被 Storage::open 就地转 WAL +
/// bigram 缓存写回 assets（交付资产被改写）。
fn make_engine_with_seed() -> Engine {
    let seed = super::tests_query::seed_copy("qi");
    Engine::new_with_db(&seed).unwrap()
}

/// 回归：/fh 触发符号候选，is_shortcut=true。
#[test]
fn test_quick_input_symbols() {
    let engine = make_engine_with_seed();
    let cands = engine.process("/fh");
    assert!(!cands.is_empty(), "/fh 应返回符号候选");
    assert!(
        cands.iter().all(|c| c.is_shortcut),
        "所有符号候选应标记 is_shortcut=true"
    );
    // 应包含高频标点（前几个）
    assert!(
        cands.iter().any(|c| c.text.as_ref() == "、"),
        "符号候选应含「、」"
    );
    assert!(
        cands.iter().any(|c| c.text.as_ref() == "，"),
        "符号候选应含「，」"
    );
}

/// 回归：/fh 包含各类符号（合并后所有类别都在 /fh 里）。
#[test]
fn test_quick_input_fh_contains_all_categories() {
    let engine = make_engine_with_seed();
    let cands = engine.process("/fh");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    // 标点
    assert!(texts.contains(&"。"), "应含标点「。」");
    // 数学
    assert!(texts.contains(&"×"), "应含数学「×」");
    // 箭头
    assert!(texts.contains(&"→"), "应含箭头「→」");
    // 罗马数字
    assert!(texts.contains(&"Ⅰ"), "应含罗马数字「Ⅰ」");
    // 圆圈数字
    assert!(texts.contains(&"①"), "应含圆圈数字「①」");
    // 希腊字母
    assert!(texts.contains(&"α"), "应含希腊字母「α」");
    // 货币
    assert!(texts.contains(&"￥"), "应含货币「￥」");
    // 制表符
    assert!(texts.contains(&"┌"), "应含制表符「┌」");
}

/// 回归：旧的分类触发词（/sx /jt /lm）已废弃，返回空。
#[test]
fn test_quick_input_old_category_triggers_removed() {
    let engine = make_engine_with_seed();
    // 这些旧触发词现在查不到（合并到 /fh），走拼音路径返回空
    assert!(
        engine.process("/sx").is_empty(),
        "/sx 应已废弃（合并到 /fh）"
    );
    assert!(engine.process("/jt").is_empty(), "/jt 应已废弃");
    assert!(engine.process("/lm").is_empty(), "/lm 应已废弃");
    assert!(engine.process("/hb").is_empty(), "/hb 应已废弃");
}

/// 回归：未知快捷词（/xyz）返回空。
#[test]
fn test_quick_input_unknown_returns_empty() {
    let engine = make_engine_with_seed();
    let cands = engine.process("/xyz");
    assert!(cands.is_empty(), "未知快捷词 /xyz 不应产生候选");
}

/// 回归：裸 /（只有引导符无内容）不触发快捷，返回空。
#[test]
fn test_quick_input_bare_slash() {
    let engine = make_engine_with_seed();
    let cands = engine.process("/");
    assert!(cands.is_empty(), "裸 / 不应触发快捷输入");
}

/// 回归：正常拼音输入不受快捷拦截影响。
#[test]
fn test_quick_input_does_not_affect_normal_pinyin() {
    let engine = make_engine_with_seed();
    let cands = engine.process("nihao");
    assert!(!cands.is_empty(), "正常拼音 nihao 应有候选");
    assert!(
        cands.iter().all(|c| !c.is_shortcut),
        "正常拼音候选不应标记 is_shortcut"
    );
}

/// 回归：/fh 候选数量充足（合并后应 ≥ 300）。
#[test]
fn test_quick_input_fh_count_sufficient() {
    let engine = make_engine_with_seed();
    let cands = engine.process("/fh");
    assert!(
        cands.len() >= 300,
        "/fh 候选数应 ≥ 300（合并所有类别），实际 {}",
        cands.len()
    );
}
