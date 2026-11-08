//! Engine 测试 · 固定候选（user_pins）。
//!
//! 覆盖（语义定稿见 pins.rs 头注）：
//! - 槽位精确（1/2/3 号位 = index 0/1/2，靠前空槽用普通候选填补）
//! - 移动语义（同文本改槽不占两位）/ 覆盖语义（槽位被占直接顶替）
//! - 键 = 原始输入串：pin("wo") 不影响 process("women")（不做音节级扩散）
//! - 固定文本不在候选列表 = 静默失效（不凭空造候选、不 panic）
//! - 入参校验（slot 1-3 / 键只许小写字母+' / 文本禁 ASCII 字母）
//! - 持久化：临时 DB 写入 → 重启引擎 → 固定仍生效
//!
//! mod.rs 用 `#[cfg(test)] mod tests_pins;` 加载（分号形式从文件加载）。

use super::*;

/// 真实词库引擎（种子 DB 复制到临时文件，写路径互不影响）。
fn make_real_engine(tag: &str) -> Engine {
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let tmp = crate::test_tmp::dir().join(format!("x1pinyin_test_pins_{}.db", tag));
    let _ = std::fs::remove_file(&tmp);
    std::fs::copy(seed, &tmp).expect("copy seed db");
    Engine::new_with_db(tmp.to_str().unwrap()).expect("open engine")
}

#[test]
fn test_pin_applies_at_slot1() {
    let mut e = make_real_engine("slot1");
    let before = e.process("wo");
    assert!(before.len() >= 3, "wo 候选至少 3 个才能测固定");
    let target = before[2].text.to_string();
    e.pin_candidate("wo", 1, &target).unwrap();
    let after = e.process("wo");
    assert_eq!(
        after[0].text.as_ref(),
        target.as_str(),
        "固定项必须顶到 1 号位"
    );
    // 普通候选顺延不丢：原前两名应紧随其后
    assert_eq!(after[1].text, before[0].text);
    assert_eq!(after[2].text, before[1].text);
}

#[test]
fn test_pin_slot3_exact_position() {
    let mut e = make_real_engine("slot3");
    let before = e.process("zao");
    assert!(before.len() >= 4, "zao 候选至少 4 个才能测 3 号位");
    let target = before[3].text.to_string();
    e.pin_candidate("zao", 3, &target).unwrap();
    let after = e.process("zao");
    assert_eq!(
        after[2].text.as_ref(),
        target.as_str(),
        "3 号位固定项必须落在 index 2"
    );
    // 靠前空槽用普通候选填补：原 1/2 名占 1/2 号位
    assert_eq!(after[0].text, before[0].text);
    assert_eq!(after[1].text, before[1].text);
}

#[test]
fn test_pin_move_and_overwrite() {
    let mut e = make_real_engine("move");
    let cands = e.process("wo");
    let a = cands[0].text.to_string();
    let b = cands[1].text.to_string();
    // 移动：A 从 1 号位改到 2 号位 → 只占 2 号位
    e.pin_candidate("wo", 1, &a).unwrap();
    e.pin_candidate("wo", 2, &a).unwrap();
    let after = e.process("wo");
    assert_eq!(after[1].text.as_ref(), a.as_str(), "移动后 A 应在 2 号位");
    assert_eq!(
        after
            .iter()
            .filter(|c| c.text.as_ref() == a.as_str())
            .count(),
        1,
        "同文本不允许占两个槽"
    );
    // 覆盖：B 固定到 2 号位 → A 被顶替回普通排序
    e.pin_candidate("wo", 2, &b).unwrap();
    let after2 = e.process("wo");
    assert_eq!(after2[1].text.as_ref(), b.as_str(), "覆盖后 B 应在 2 号位");
    let slots = e.pins_for("wo").unwrap();
    assert!(slots[0].is_none(), "1 号位的 A 早被移动清掉");
    assert_eq!(slots[1].as_deref(), Some(b.as_str()));
}

#[test]
fn test_unpin_restores() {
    let mut e = make_real_engine("unpin");
    let before = e.process("wo");
    let target = before[2].text.to_string();
    e.pin_candidate("wo", 1, &target).unwrap();
    assert_eq!(e.process("wo")[0].text.as_ref(), target.as_str());
    e.unpin_candidate("wo", 1).unwrap();
    let after = e.process("wo");
    assert_eq!(after[0].text, before[0].text, "解除后排序应复原");
    assert!(e.pins_for("wo").is_none(), "空壳输入键应被清理");
}

#[test]
fn test_pin_key_is_exact_input() {
    let mut e = make_real_engine("exact");
    let cands = e.process("wo");
    let target = cands[2].text.to_string();
    e.pin_candidate("wo", 1, &target).unwrap();
    // 关键守护：pin("wo") 不得影响 process("women")（不做音节级扩散）
    let wm = e.process("women");
    assert!(
        wm.first().map(|c| c.text.as_ref()) != Some(target.as_str()),
        "pin(wo) 不允许波及 women 的候选序"
    );
    // 简拼键独立：pin("w") 与 pin("wo") 各是各的
    let w = e.process("w");
    if w.len() >= 3 {
        let wtarget = w[2].text.to_string();
        e.pin_candidate("w", 1, &wtarget).unwrap();
        assert_eq!(e.process("w")[0].text.as_ref(), wtarget.as_str());
        // wo 的固定不受 w 影响
        assert_eq!(e.process("wo")[0].text.as_ref(), target.as_str());
    }
}

#[test]
fn test_pin_text_not_in_candidates_is_inert() {
    let mut e = make_real_engine("inert");
    let before = e.process("wo");
    // 「提醒」不是 wo 的候选——固定后对 wo 查询零影响
    e.pin_candidate("wo", 1, "提醒").unwrap();
    let after = e.process("wo");
    assert_eq!(
        before.iter().map(|c| &c.text).collect::<Vec<_>>(),
        after.iter().map(|c| &c.text).collect::<Vec<_>>(),
        "固定文本不在候选列表时必须完全惰性"
    );
}

#[test]
fn test_pin_validation() {
    let mut e = Engine::new();
    assert!(e.pin_candidate("wo", 0, "我").is_err(), "slot 0 非法");
    assert!(e.pin_candidate("wo", 4, "我").is_err(), "slot 4 非法");
    assert!(e.pin_candidate("", 1, "我").is_err(), "空键非法");
    assert!(e.pin_candidate("Wo", 1, "我").is_err(), "大写键非法");
    assert!(e.pin_candidate("wo2", 1, "我").is_err(), "数字键非法");
    assert!(e.pin_candidate("wo", 1, "abc").is_err(), "ASCII 文本非法");
    assert!(e.unpin_candidate("wo", 0).is_err());
    assert!(e.unpin_candidate("wo", 5).is_err());
}

#[test]
fn test_pin_survives_restart() {
    let tag = "restart";
    let input = "wo";
    let target = {
        let mut e = make_real_engine(tag);
        let cands = e.process(input);
        let t = cands[2].text.to_string();
        e.pin_candidate(input, 1, &t).unwrap();
        t
    };
    // 重启（同一临时 DB 重新打开）
    let tmp = crate::test_tmp::dir().join(format!("x1pinyin_test_pins_{}.db", tag));
    let e2 = Engine::new_with_db(tmp.to_str().unwrap()).expect("reopen engine");
    let after = e2.process(input);
    assert_eq!(
        after[0].text.as_ref(),
        target.as_str(),
        "固定必须跨重启存活"
    );
    assert_eq!(e2.list_pins().len(), 1);
    assert_eq!(e2.list_pins()[0].0, input);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn test_pin_beats_green_memo_lift() {
    // green_memo 提升发生在整句插入阶段，pin 注入在其后——pin 必须赢。
    // 场景：输入串有 green 记忆词时，固定项仍占 1 号位，绿词顺延。
    let mut e = make_real_engine("green");
    // 造一条 green_memo：commit 一个三音节组合两次让 green_memo 记住
    let input = "wobuzhidao";
    let cands = e.process(input);
    if cands.len() < 2 {
        eprintln!("[跳过] wobuzhidao 候选不足");
        return;
    }
    let green_text = cands[0].text.to_string();
    e.commit(input, &green_text, None, true);
    // 固定另一个候选到 1 号位
    let pin_text = cands[1].text.to_string();
    e.pin_candidate(input, 1, &pin_text).unwrap();
    let after = e.process(input);
    assert_eq!(
        after[0].text.as_ref(),
        pin_text.as_str(),
        "固定项必须压过 green_memo 提升"
    );
}

// ── 双拼写域守护：pin 键校验必须覆盖双拼 ';' ing 键 ──
// 微软/搜狗/紫光双拼的 x; = xing——键域白名单漏 ';' 时 Cmd/Ctrl+数字固定
// 被引擎静默拒绝（Err 非法固定键），这些方案下固定功能整体失效。
#[test]
fn test_pin_shuangpin_key_domain() {
    let mut engine = super::Engine::new();
    // 小鹤 "vs" = zhong（纯字母键）：pin/查询/槽位注入应全通
    engine.set_shuangpin_scheme(Some(
        crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe")
            .unwrap()
            .clone(),
    ));
    let before = engine.process("vs");
    let target = before.first().unwrap().text.to_string();
    engine
        .pin_candidate("vs", 1, &target)
        .expect("小鹤 vs pin 应成功");
    let after = engine.process("vs");
    assert_eq!(
        after.first().map(|c| c.text.as_ref()),
        Some(target.as_str()),
        "vs 固定后应首位"
    );
    assert_eq!(
        engine.pins_for("vs").unwrap()[0].as_deref(),
        Some(target.as_str())
    );

    // 微软双拼 "x;" = xing：';' 是 ing 键——pin 键校验若只认字母+' 则被拒
    engine.set_shuangpin_scheme(Some(
        crate::shuangpin::ShuangpinScheme::builtin_by_id("microsoft")
            .unwrap()
            .clone(),
    ));
    let cands = engine.process("x;");
    assert!(!cands.is_empty(), "x; 应有候选");
    let target2 = cands.first().unwrap().text.to_string();
    let r = engine.pin_candidate("x;", 1, &target2);
    assert!(r.is_ok(), "双拼 ';' 键（ing）的 buffer 必须可固定: {:?}", r);
    let after2 = engine.process("x;");
    assert_eq!(
        after2.first().map(|c| c.text.as_ref()),
        Some(target2.as_str()),
        "x; 固定后应首位"
    );
}
