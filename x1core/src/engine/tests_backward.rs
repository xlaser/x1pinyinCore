//! 后向 bigram 词格重打分守护：合成数据机制验证 + 真种子 A/B 集成冒烟。

use super::backward::BACKWARD_CAP;
use super::tests_query::{seed_copy, seed_copy_cleanup};
use super::Engine;

/// 最小合成引擎：临时 DB + 少量词 + 直接安装合成种子 bigram 表。
fn synthetic_engine(tag: &str, words: &[(&str, &str, u32)]) -> (String, Engine) {
    let db = crate::test_tmp::dir().join(format!("backward_{}_{}.db", tag, std::process::id()));
    let db_s = db.display().to_string();
    {
        let mut s = crate::storage::Storage::open(&db_s).unwrap();
        let rows: Vec<crate::storage::X1DictWord> = words
            .iter()
            .map(|(t, p, f)| crate::storage::X1DictWord {
                simplified: (*t).into(),
                pinyin: (*p).into(),
                freq: *f as i64,
                ..Default::default()
            })
            .collect();
        s.import_x1dict_batch(&rows).unwrap();
    }
    let mut engine = Engine::new_with_db(&db_s).unwrap();
    engine.set_smart_word_zh_enabled(false);
    (db_s, engine)
}

/// 场景数据复刻真实种子的形态：
/// (认真,地)=17 与 (认真,的)=17 前向同分；next_total(的) 因海量前词巨大、
/// next_total(地) 集中——后向视角必须给「认真+地」更高分。
#[test]
fn test_backward_pair_prefers_concentrated_next_marginal() {
    let (db, mut engine) = synthetic_engine(
        "pair",
        &[
            ("认真", "ren zhen", 3000),
            ("地", "de", 4000),
            ("的", "de", 9999),
        ],
    );
    // 同计数对；「的」再造大量前词 → next_total(的) 巨大（真实分布形态）。
    engine.freq.set_seed_bigram_counts(vec![
        ("认真".into(), "地".into(), 17),
        ("认真".into(), "的".into(), 17),
        // 撑大 next_total(地) 少量、next_total(的) 大量
        ("慢慢".into(), "地".into(), 5),
        ("突然".into(), "地".into(), 5),
        ("我们".into(), "的".into(), 900),
        ("问题".into(), "的".into(), 900),
        ("目标".into(), "的".into(), 900),
        ("结果".into(), "的".into(), 900),
        ("过程".into(), "的".into(), 900),
        ("重要".into(), "的".into(), 900),
    ]);
    let s_de = engine.backward_pair_score_for_test("认真", "地", 3000);
    let s_di = engine.backward_pair_score_for_test("认真", "的", 3000);
    assert!(
        s_de > s_di,
        "后向分必须拉开同计数对：地={} 的={}",
        s_de,
        s_di
    );
    std::fs::remove_file(&db).ok();
}

/// 覆盖洞退化（lift 语义）：(问题,吧) 不在种子表 → lift 恒 0（右文证据
/// 相对 unigram 基线零提升）；命中对 > 0。既无 ε 重罚也无白赚——单字链
/// 按对数偷词频分的通道被结构性封死。
#[test]
fn test_backward_pair_missing_lift_is_zero() {
    let (db, mut engine) =
        synthetic_engine("miss", &[("问题", "wen ti", 8000), ("吧", "ba", 8600)]);
    engine
        .freq
        .set_seed_bigram_counts(vec![("吃饭".into(), "吧".into(), 6)]);
    let s_miss = engine.backward_pair_score_for_test("问题", "吧", 8000);
    let s_hit = engine.backward_pair_score_for_test("吃饭", "吧", 8000);
    assert_eq!(s_miss, 0, "缺失对 lift 必须恒 0：{}", s_miss);
    assert!(s_hit > 0, "独占后向边际的命中对应为正：{}", s_hit);
    std::fs::remove_file(&db).ok();
}

/// κ_b 缩放 + 路径级钳幅 + 防御。
#[test]
fn test_backward_delta_cap_and_guards() {
    let (db, mut engine) = synthetic_engine(
        "cap",
        &[
            ("甲", "jia", 5000),
            ("乙", "yi", 5000),
            ("丙", "bing", 5000),
        ],
    );
    // 每对 p_b=1（独占后向边际）→ 单对映射分 12（freq 5000 的 unigram 插值），
    // 两对 sum=24 → ×κ_b(1/2)=12 恰触 CAP；更多对也只到 CAP。
    engine.freq.set_seed_bigram_counts(vec![
        ("甲".into(), "乙".into(), 65535),
        ("乙".into(), "丙".into(), 65535),
    ]);
    let words3: Vec<String> = vec!["甲".into(), "乙".into(), "丙".into()];
    let freqs3 = vec![5000u32, 5000, 5000];
    assert_eq!(
        engine.backward_path_delta(&words3, &freqs3),
        BACKWARD_CAP,
        "双对满证据应触 CAP"
    );
    // 防御：单词路径 / freqs 长度不符 → 0（恒不变现状）。
    assert_eq!(engine.backward_path_delta(&words3[..1], &freqs3[..1]), 0);
    assert_eq!(engine.backward_path_delta(&words3, &freqs3[..1]), 0);
    std::fs::remove_file(&db).ok();
}

/// A/B 集成冒烟：真种子上开关两态整句解码均不 panic、候选非空、
/// 全部为合法中文候选（is_valid_chinese_text 防御字母泄漏的老不变量）。
#[test]
fn test_backward_ab_smoke_on_real_seed() {
    let db = seed_copy("backward_ab_smoke");
    let mut engine = Engine::new_with_db(&db).expect("open seed copy");
    engine.set_smart_word_zh_enabled(false);
    for input in [
        "woxihuanchifan",
        "womenyiqiquchifanba",
        "tamenzhengzaikaihui",
    ] {
        Engine::set_backward_enabled_for_test(true);
        let on = engine.process(input);
        Engine::set_backward_enabled_for_test(false);
        let off = engine.process(input);
        Engine::set_backward_enabled_for_test(true);
        assert!(!on.is_empty(), "{} 开 backward 候选非空", input);
        assert!(!off.is_empty(), "{} 关 backward 候选非空", input);
        for c in on.iter().chain(off.iter()) {
            if c.is_sentence {
                assert!(
                    super::is_valid_chinese_text(&c.text),
                    "{} 整句候选必须纯中文：{}",
                    input,
                    c.text
                );
            }
        }
    }
    seed_copy_cleanup(&db);
}
