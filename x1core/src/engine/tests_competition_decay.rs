//! 「退化」通道的 engine 级守护测试。
//!
//! 与 competition.rs 的单元测试（纯 store 层）互补：这里验证 **commit →
//! competition → 排序** 全链行为——用过（竞争胜出）的候选在同音精确阶梯
//! 上**进化升权**、停用（对手胜出）的候选**退化降权**；且全程无时间参数
//! （无事件即冻结，与真实时间解耦）。
//!
//! 守护判据（每条都必须可人工复算）：
//! 1. 冷启动 = 纯种子排序（dynamic_term=0，先验主导）；
//! 2. 用过（竞争胜出 ≥ α_pin）→ dynamic_term>0 → 该候选 base 上升；
//! 3. 停用后对手胜出 → dynamic_term<0 → 该候选 base 下降（被动稀释）；
//! 4. 「的」锚点（高频种子）在冷启动不因任何学习跌出首候选（客观基准不动）。

use super::*;
use crate::competition::{CtxKey, ALPHA_PIN};

fn seed_engine(tag: &str) -> (Engine, String) {
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_comp_{}_{}_{}.db",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::copy(seed, &tmp);
    let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
    (engine, tmp.to_string_lossy().into_owned())
}

fn de_bd(e: &Engine, word: &str) -> (u64, u64, u64) {
    let cands = e.process("de");
    let idx = cands
        .iter()
        .position(|c| c.text.as_ref() == word)
        .unwrap_or_else(|| panic!("de 候选应含「{word}」"));
    let bd = e.score_breakdown("de", &cands[idx].clone());
    (bd.base, bd.total(), idx as u64)
}

/// 冷启动：纯种子排序（dynamic_term=0）——「的/得/地」按种子先验降序。
#[test]
fn test_cold_start_pure_seed_order() {
    let (engine, tmp) = seed_engine("cold");
    let (b_de, _, _) = de_bd(&engine, "的");
    let (b_de2, _, _) = de_bd(&engine, "得");
    let (b_di, _, _) = de_bd(&engine, "地");
    assert!(
        b_de > b_de2 && b_de2 > b_di,
        "冷启动应按种子先验降序：的{b_de} > 得{b_de2} > 地{b_di}"
    );
    // 竞争表应为空（无任何学习）
    assert_eq!(
        engine.competition.total_of(&CtxKey::Pin("de".into())),
        0,
        "冷启动 competition 应为空"
    );
    let _ = std::fs::remove_file(&tmp);
}

/// 用过（竞争胜出 ≥α_pin）→ dynamic_term>0 → base 上升（进化）。
#[test]
fn test_used_word_evolves_up() {
    let (mut engine, tmp) = seed_engine("evolve");
    let (b_before, _, _) = de_bd(&engine, "得");
    // 在 de 语境反复选「得」（同音竞争胜出；α_pin=4 次成型）
    for _ in 0..(ALPHA_PIN + 1) {
        engine.commit("de", "得", Some("de"), true);
    }
    let (b_after, _, _) = de_bd(&engine, "得");
    assert!(
        b_after > b_before,
        "用过（竞争胜出）后 base 应上升（进化）：{b_before} → {b_after}"
    );
    let _ = std::fs::remove_file(&tmp);
}

/// 停用后对手胜出 → dynamic_term<0 → base 下降（被动稀释，退化）。
///
/// 设计注：「的」**不该**先积累自己的选择——那会同时抬高它的静态 ilog2 计数，
/// 掩盖稀释信号。正确对照 = 纯冷启动的「的」 vs 对手「得」饱和胜出后的
/// 「的」——从未选过「的」，但它在同音竞争里被动稀释（这是「退化」的精确定义）。
#[test]
fn test_stale_word_decays_down() {
    let (mut engine, tmp) = seed_engine("decay");
    let (b_before, _, _) = de_bd(&engine, "的");
    // 对手（得）在同语境饱和胜出——「的」零选择，被动稀释
    for _ in 0..(ALPHA_PIN * 3) {
        engine.commit("de", "得", Some("de"), true);
    }
    let (b_after, _, _) = de_bd(&engine, "的");
    assert!(
        b_after < b_before,
        "对手饱和胜出后「的」应被被动稀释（退化）：{b_before} → {b_after}"
    );
    let _ = std::fs::remove_file(&tmp);
}

/// 饱和门（§5 参数 α_pin 的行为契约）：竞争动态项**只在证据饱和后生效**
/// ——该语境学习次数 < α_pin 时 dynamic_term=0（先验主导），刚达到
/// α_pin 时门槛打开。这保证「单次/少量选择」不触发竞争升降（近期短选
/// 的提升由既有 recent/total 通道表达，竞争只负责**习惯成型后**的
/// 升降——两条通道职责分离，无重叠叠加）。
///
/// 注：单次 commit 本身就会通过既有 recent_score(+5000)/total_bonus 提升
/// 词序（短期通道，设计如此）——本测试锁的是**竞争记账的门槛**，不锁
/// 词序（词序由两条通道共同决定）。
#[test]
fn test_saturation_gate() {
    let (mut engine, tmp) = seed_engine("gate");
    let ctx = CtxKey::Pin("de".into());
    for i in 1..ALPHA_PIN {
        engine.commit("de", "得", Some("de"), true);
        assert_eq!(
            engine.competition.total_of(&ctx),
            i,
            "竞争分母应逐次累计（第 {i} 次）"
        );
        assert!(
            engine.competition.total_of(&ctx) < ALPHA_PIN,
            "饱和门内侧：total < α_pin"
        );
    }
    // 第 α_pin 次：门槛打开（total == α_pin → 后续查询 dynamic_term 生效）
    engine.commit("de", "得", Some("de"), true);
    assert_eq!(
        engine.competition.total_of(&ctx),
        ALPHA_PIN,
        "饱和门应恰在 α_pin 次打开"
    );
    let _ = std::fs::remove_file(&tmp);
}
