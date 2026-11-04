//! Engine 测试 · x1grammar.bin 微型词性搭配模型接入 A/B。
//!
//! 覆盖三点（模型信息存在 / 微型打分路径确实执行）：
//! 1. 发现：资产随词库 DB 同目录分发 → `grammar_micro_info()` 有值（版本/行数）；
//! 2. 降级：资产缺失 → 空模型（info None），开启开关不 panic、增量恒 0；
//! 3. A/B：同一输入开/关微型模型，候选数量不变（候选生成不受影响），且开启后
//!    至少一条常见词路径拿到非 0 微型语法分（打分路径确实消费了模型）。
//!
//! 副本一律落 test-tmp 独立子目录（绝不直接开仓库种子；模型发现按
//! 「词库 DB 同目录」约定，伴随文件不能落 test-tmp 共享顶层——并行副本互见，
//! seed_copy_keephash 同款隔离范式）。

use super::*;

/// 种子 DB 副本 + 同目录伴随 `x1grammar.bin`（返回副本 DB 路径）。
fn seed_copy_with_model(tag: &str) -> String {
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let model = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1grammar.bin");
    // 进程内序号：并行测试的纳秒时钟可能同值（seed_copy 注释），tag+pid+nanos+seq 才唯一。
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = crate::test_tmp::dir().join(format!(
        "x1gm_seed_{}_{}_{}_{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        seq
    ));
    std::fs::create_dir_all(&dir).expect("create model seed dir");
    let db = dir.join("x1pinyin.db");
    std::fs::copy(seed, &db).expect("copy seed db");
    std::fs::copy(model, dir.join("x1grammar.bin")).expect("copy x1grammar.bin");
    db.to_str().unwrap().to_string()
}

/// 种子 DB 副本（**不带**模型资产——降级路径用），同样落独立子目录。
fn seed_copy_without_model(tag: &str) -> String {
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = crate::test_tmp::dir().join(format!(
        "x1gm_bare_{}_{}_{}_{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        seq
    ));
    std::fs::create_dir_all(&dir).expect("create bare seed dir");
    let db = dir.join("x1pinyin.db");
    std::fs::copy(seed, &db).expect("copy seed db");
    db.to_str().unwrap().to_string()
}

/// 清理副本所在子目录（DB + wal/shm + 伴随模型 + 引擎可能写的伴随缓存一并带走；
/// 残留也会被 test-tmp 的 24h 自愈清扫回收）。
fn cleanup_copy(db: &str) {
    let p = std::path::Path::new(db);
    if let Some(dir) = p.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// 探针词路径（常见句式：代词+动词、状中「的/地」、动宾等，覆盖二元/三元层
/// 最常见的粗类转移；词性映射由 classes_for 完成，含 OOV 回退）。
fn probe_paths() -> Vec<Vec<String>> {
    [
        vec!["我", "爱", "你"],
        vec!["干", "的", "漂亮"],
        vec!["认真", "地", "思考"],
        vec!["他", "在", "家"],
        vec!["一", "个", "人"],
        vec!["我们", "一起", "吃饭"],
        vec!["中国", "人"],
        vec!["今天", "天气", "很好"],
    ]
    .iter()
    .map(|ws| ws.iter().map(|w| w.to_string()).collect())
    .collect()
}

/// legacy 词性组合三项直算（A/B 出口「关/降级」臂的期望值，逐字节同组合）。
fn legacy_pos_delta(engine: &Engine, ws: &[String]) -> i32 {
    engine.grammar.path_pos_delta(ws)
        + engine.grammar.path_pos_trigram_delta(ws)
        + engine.grammar_pattern_delta(ws)
}

/// 模型发现：同目录伴随资产必须被 `Engine::from_storage` 加载，元信息可用，
/// 且微型语法生产默认**开**（扩展口语语料 + 正分融合口径）。
#[test]
fn grammar_micro_engine_discovers_model_beside_db() {
    if crate::test_tmp::skip_without_grammar_model() {
        return;
    }
    let db = seed_copy_with_model("discover");
    let engine = Engine::new_with_db(&db).expect("seed db with model");
    let info = engine
        .grammar_micro_info()
        .expect("同目录 x1grammar.bin 必须被发现");
    assert_eq!(info.version, 1);
    assert!(info.path.as_deref().unwrap().contains("x1grammar.bin"));
    assert!(info.bigram_rows > 0, "二元层非空：{}", info.bigram_rows);
    assert!(info.trigram_rows > 0, "三元层非空：{}", info.trigram_rows);
    assert!(info.block_rows > 0, "滑动块层非空：{}", info.block_rows);
    // 默认开；测试进程可用 X1_GRAMMAR_MICRO=0 显式关。
    assert!(
        engine.grammar_micro_enabled.get(),
        "扩展口语语料 + 正分融合后生产默认开"
    );
    cleanup_copy(&db);
}

/// 降级：资产缺失（老部署 / 只拷了 DB）→ 空模型（info None），开启开关也不
/// panic、正常出候选——加载失败绝不能阻塞启动。
/// 评审修复：开关开 + 模型未加载时 A/B 出口必须
/// **回退 legacy 三项**（不是 0）——不静默丢掉整层词性信号、不污染处理臂。
#[test]
fn grammar_micro_engine_degrades_to_empty_when_asset_missing() {
    let db = seed_copy_without_model("degrade");
    if crate::test_tmp::skip_without_grammar_tables() {
        return;
    }
    let engine = Engine::new_with_db(&db).expect("seed db without model");
    assert!(
        engine.grammar_micro_info().is_none(),
        "资产缺失 → 空模型降级"
    );
    // 空模型 + 开开关：微型层本身恒 0（所有层查空表）；不 panic。
    engine.set_grammar_micro_enabled(true);
    let words: Vec<String> = ["干", "的", "漂亮"].iter().map(|w| w.to_string()).collect();
    assert_eq!(engine.grammar_micro_delta(&words), 0);
    // 降级回退：开臂 + 空模型 → grammar_ab_pos_delta 逐字节等于 legacy 三项。
    let mut any_legacy = false;
    for ws in probe_paths() {
        let legacy = legacy_pos_delta(&engine, &ws);
        assert_eq!(
            engine.grammar_ab_pos_delta(&ws),
            legacy,
            "降级态必须回退 legacy 词性分（而非 0 抑制）: {ws:?}"
        );
        any_legacy |= legacy != 0;
    }
    assert!(
        any_legacy,
        "降级回退必须保留非 0 的 legacy 词性分（防退化安装静默丢层）"
    );
    let cands = engine.process("gandepiaoliang");
    assert!(!cands.is_empty(), "降级态正常出候选");
    cleanup_copy(&db);
}

/// A/B（同一输入开/关微型模型 + 候选生成不变断言）：
/// 候选数量必须一致——语法 A/B 只允许改绿词内部排序，不允许产生/消灭候选。
#[test]
fn grammar_micro_engine_ab_preserves_candidate_count() {
    if crate::test_tmp::skip_without_grammar_model() {
        return;
    }
    let db = seed_copy_with_model("ab");
    let engine = Engine::new_with_db(&db).expect("seed db with model");

    let input = "gandepiaoliang";
    engine.set_collect_sentence_paths(true);
    let before = engine.process(input);
    let _ = engine.take_sentence_paths(); // 清空收集槽（跨次不留旧路径）
    engine.set_collect_sentence_paths(false);

    engine.set_grammar_micro_enabled(true);
    engine.set_collect_sentence_paths(true);
    let after = engine.process(input);
    let _ = engine.take_sentence_paths();
    engine.set_collect_sentence_paths(false);

    assert!(!before.is_empty());
    assert_eq!(
        before.len(),
        after.len(),
        "开/关微型模型不得改变候选生成数量"
    );
    // 模型信息在 A/B 两侧都存在。
    assert!(engine.grammar_micro_info().is_some());
    cleanup_copy(&db);
}

/// 微型打分路径确实执行：关臂恒 0；开臂在常见词路径上至少一条拿到非 0 分
///（若模型未接线，所有路径恒 0，本测试必红）；长度门控与钳位区间同口径。
#[test]
fn grammar_micro_engine_delta_executes_model_scoring() {
    if crate::test_tmp::skip_without_grammar_model() {
        return;
    }
    let db = seed_copy_with_model("delta");
    let engine = Engine::new_with_db(&db).expect("seed db with model");

    let probes = probe_paths();
    engine.set_grammar_micro_enabled(false);
    // 关臂：开关关闭 → 恒 0（不消费微型模型，走 legacy 三项）。
    for ws in &probes {
        assert_eq!(engine.grammar_micro_delta(ws), 0, "关臂恒 0: {ws:?}");
    }

    engine.set_grammar_micro_enabled(true);
    // 开臂：至少一条非 0 —— 证明模型查询结果真的进了引擎路径分。
    let any_nonzero = probes.iter().any(|ws| engine.grammar_micro_delta(ws) != 0);
    assert!(
        any_nonzero,
        "开臂至少一条常见词路径拿到非 0 微型语法分（模型确实被消费）"
    );
    // 钳位区间（path_score 钳 ±24；强语境归零只会缩小不会放大）。
    for ws in &probes {
        let d = engine.grammar_micro_delta(ws);
        assert!((-24..=24).contains(&d), "微型分在钳位区间内: {ws:?} → {d}");
    }
    // 长度门控：<2 / >24 恒 0（与 GrammarMicro::path_score 的 2..=24 同口径）。
    assert_eq!(engine.grammar_micro_delta(&probes[0][..1]), 0);
    let long: Vec<String> = std::iter::repeat_n("我".to_string(), 25).collect();
    assert_eq!(engine.grammar_micro_delta(&long), 0);
    cleanup_copy(&db);
}

/// 臂选择（评审聚焦测试）：模型已加载时，开 = legacy 三项 + 正向微型分
/// 的四分之一，关 = legacy 三项（逐字节）；「开 + 已加载」必须真的走微型评分
/// 路径（A/B 处理臂未被降级回退劫走）。
#[test]
fn grammar_micro_engine_loaded_model_uses_micro_arm() {
    if crate::test_tmp::skip_without_grammar_model() {
        return;
    }
    let db = seed_copy_with_model("arm");
    let engine = Engine::new_with_db(&db).expect("seed db with model");
    let probes = probe_paths();

    // 关臂：= legacy 三项（基线逐字节）。测试不依赖默认值，两档各自显式设置。
    engine.set_grammar_micro_enabled(false);
    for ws in &probes {
        let legacy = legacy_pos_delta(&engine, ws);
        assert_eq!(
            engine.grammar_ab_pos_delta(ws),
            legacy,
            "关臂 = legacy 三项: {ws:?}"
        );
    }

    // 开臂（模型已加载）：legacy + max(微型分,0)/2（只叠正向奖励）。
    engine.set_grammar_micro_enabled(true);
    let mut any_micro = false;
    for ws in &probes {
        let micro = engine.grammar_micro_delta(ws).max(0);
        // 正分融合只服务 ≤5 词口语短链；更长路径保持 legacy，避免新闻长句被
        // 口语语料带偏。
        let expected = if ws.len() <= 5 {
            legacy_pos_delta(&engine, ws) + micro / 4
        } else {
            legacy_pos_delta(&engine, ws)
        };
        assert_eq!(
            engine.grammar_ab_pos_delta(ws),
            expected,
            "开臂 + 已加载模型：短链叠加正向微型分/4，长链保持 legacy: {ws:?}"
        );
        any_micro |= ws.len() <= 5 && micro != 0;
    }
    assert!(any_micro, "开臂至少一条非 0 微型分（微型臂确实生效）");
    cleanup_copy(&db);
}

/// 只读诊断 API：观测不受总闸
/// 影响、分量与引擎开臂逐分对账（含强语境让位谓词）、降级/长度门控全 0。
#[test]
fn grammar_micro_path_components_diagnosis_matches_scoring() {
    if crate::test_tmp::skip_without_grammar_model() {
        return;
    }
    let db = seed_copy_with_model("diag");
    let mut engine = Engine::new_with_db(&db).expect("seed db with model");
    let probes = probe_paths();

    // 观测不受总闸影响：开关关也必须能拿到分量（「关闭（仅观测）」行）。
    engine.set_grammar_micro_enabled(false);
    let mut any_nonzero = false;
    for ws in &probes {
        let d = engine.grammar_micro_path_components(ws);
        assert!(d.loaded, "模型在场");
        // 单实现对账：分量之和钳位 == raw_total（GrammarMicro::path_breakdown）。
        assert_eq!(
            (d.bigram + d.trigram + d.block).clamp(-24, 24),
            d.raw_total,
            "分量构成: {ws:?}"
        );
        any_nonzero |= d.raw_total != 0;
    }
    assert!(any_nonzero, "常见词路径至少一条非 0（模型确实被查询）");

    // 与引擎开臂逐分对账：grammar_micro_delta == effective(强语境谓词)。
    // 谓词与引擎同款（上文栈非空 / 任一相邻词对 bigram ≥3），测试可直取
    // crate 内私有状态。
    engine.set_grammar_micro_enabled(true);
    for ws in &probes {
        engine.clear_context();
        let strong = ws
            .windows(2)
            .any(|w| engine.freq.word_bigram_count(&w[0], &w[1]) >= 3);
        let d = engine.grammar_micro_path_components(ws);
        assert_eq!(
            engine.grammar_micro_delta(ws),
            d.effective(strong),
            "观测口径与排序口径逐分对账: {ws:?}"
        );
    }

    // 降级：无模型 → loaded=false、分量全 0。
    let bare = seed_copy_without_model("diagbare");
    let e2 = Engine::new_with_db(&bare).expect("bare seed");
    let d2 = e2.grammar_micro_path_components(&probes[0]);
    assert!(!d2.loaded);
    assert_eq!(
        (d2.bigram, d2.trigram, d2.block, d2.raw_total),
        (0, 0, 0, 0)
    );
    cleanup_copy(&bare);

    // 长度门控：单词 / 25 词分量全 0（与打分门控同口径）。
    assert_eq!(
        engine
            .grammar_micro_path_components(&probes[0][..1])
            .raw_total,
        0
    );
    let long: Vec<String> = vec!["我".to_string(); 25];
    let dl = engine.grammar_micro_path_components(&long);
    assert_eq!(
        (dl.bigram, dl.trigram, dl.block, dl.raw_total),
        (0, 0, 0, 0)
    );
    cleanup_copy(&db);
}
