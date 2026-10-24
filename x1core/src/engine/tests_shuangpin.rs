use super::*;

#[test]
fn test_shuangpin_decode_fail_falls_back_to_jianpin() {
    // 双拼解码失败落回全拼路径：zq 在小鹤
    // z×q键(iu)=ziu 无效 → decode_buffer None → 落回 process_inner →
    // is_jianpin("zq")=true → 简拼查询。守护「双拼模式简拼可用」。
    let mut engine = Engine::new();
    let scheme = crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe")
        .unwrap()
        .clone();
    engine.set_shuangpin_scheme(Some(scheme));
    engine.learn("争取", "zheng qu");
    let r = engine.process("zq");
    assert!(
        r.iter().any(|c| c.text.as_ref() == "争取"),
        "双拼下 zq 应落回简拼查到「争取」: {:?}",
        r.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
}

#[test]
fn test_shuangpin_merge_falls_back_path() {
    // merge zh/ch/sh 开关在双拼落回路径生效。
    // 用紫光方案：h 键=[iang,uang]，zh 组 z×h=ziang/zuang 无效 → decode None
    // → 落回 → is_jianpin("zhw")=true（长度 3 < is_hybrid_shengmu_input 的 4）
    // → 简拼模糊查询 → merge 把 zh 合并为 z → zw → 「中文」可达。
    // 注意小鹤/微软 h 键=ang（zang 有效）decode 成功不落回——行为依赖方案键位。
    let mut engine = Engine::new();
    let scheme = crate::shuangpin::ShuangpinScheme::builtin_by_id("ziguang")
        .unwrap()
        .clone();
    engine.set_shuangpin_scheme(Some(scheme));
    engine.set_fuzzy_config(crate::fuzzy::FuzzyConfig::from_csv(
        "jianpin_merge_zh_ch_sh",
    ));
    let r = engine.process("zhw");
    assert!(
        r.iter().any(|c| c.text.as_ref() == "中文"),
        "双拼(紫光)下 zhw 应经 merge 落回查到「中文」: {:?}",
        r.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
}

#[test]
fn test_shuangpin_sorting_matches_fullpinyin() {
    // 双拼与全拼候选排序一致性守护：
    // 双拼排序接入 candidate_sort_key 分层后，同一词的双拼输入（vsgo）
    // 与全拼输入（zhongguo）首候选应一致（都是「中国」）。
    let mut engine = Engine::new();
    let scheme = crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe")
        .unwrap()
        .clone();
    engine.set_shuangpin_scheme(Some(scheme));
    let sp = engine.process("vsgo");
    engine.set_shuangpin_scheme(None);
    let fp = engine.process("zhongguo");
    assert_eq!(
        sp.first().map(|c| c.text.as_ref()),
        fp.first().map(|c| c.text.as_ref()),
        "双拼 vsgo 与全拼 zhongguo 首候选应一致\nsp={:?}\nfp={:?}",
        sp.iter().map(|c| &c.text).collect::<Vec<_>>(),
        fp.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
}

#[test]
fn test_shuangpin_off_no_regression() {
    // scheme=None 时双拼分支不触发，行为与全拼一致
    let engine = Engine::new();
    assert!(engine.shuangpin.is_none());
    let r = engine.process("zhongguo");
    assert!(r.iter().any(|c| c.text.contains("中")));
}

#[test]
fn test_shuangpin_xiaohe_process() {
    let mut engine = Engine::new();
    let scheme = crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe")
        .unwrap()
        .clone();
    engine.set_shuangpin_scheme(Some(scheme));
    // vsgo = zhongguo（小鹤 v=zh s=iong/ong g=eng o=uo/o → zh+ong=zhong, g+uo=guo）
    let r = engine.process("vsgo");
    assert!(
        r.iter().any(|c| c.text.contains("中")),
        "应有「中」候选: {:?}",
        r.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
}

#[test]
fn test_shuangpin_commit_learns_full_pinyin() {
    // 双拼 commit vsgo→中国 后，切到全拼应能查到（学习数据是全拼）
    // 这是 commit 展开垫片的端到端验证：双拼 pinyin 必须展开成全拼再记学习，
    // 否则学习数据被双拼编码污染，全拼用户无法共享。
    let mut engine = Engine::new();
    let scheme = crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe")
        .unwrap()
        .clone();
    engine.set_shuangpin_scheme(Some(scheme));
    // commit(input, text, pinyin, advance_context)
    // pinyin 传双拼编码 "vsgo"，commit_internal 应展开成 "zhongguo" 记 user_words
    engine.commit("vsgo", "中国", Some("vsgo"), true);
    // 切到全拼
    engine.set_shuangpin_scheme(None);
    let r = engine.process("zhongguo");
    // 学习后「中国」应出现（可能不是第一，但应在候选里）
    assert!(
        r.iter().any(|c| c.text.as_ref() == "中国"),
        "全拼模式应查到学习过的「中国」: {:?}",
        r.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
}

#[test]
fn test_shuangpin_incomplete_trailing() {
    let mut engine = Engine::new();
    let scheme = crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe")
        .unwrap()
        .clone();
    engine.set_shuangpin_scheme(Some(scheme));
    // vsg = zhong + g(不完整声母) → 应有候选
    let r = engine.process("vsg");
    assert!(!r.is_empty(), "vsg 应有候选");
}

#[test]
fn test_custom_scheme_end_to_end() {
    let mut engine = Engine::new();
    // 构造一个 x→zh 的自定义方案（验证非默认声母重映射）
    let json = r#"{"id":"custom-test","displayName":"测试","finalMap":{"s":["iong","ong"],"o":["uo","o"]},"initialRemap":{"x":"zh"},"zeroInitialStyle":"literalVowel"}"#;
    let validated = crate::shuangpin::ShuangpinScheme::from_json(json).unwrap();
    engine.set_shuangpin_scheme(Some(validated.scheme));
    // xso: x=zh, s=ong → zhong; o=不完整声母 o
    let r = engine.process("xso");
    // 至少不崩溃且有候选（zhong 开头的字 + o 声母的字）
    assert!(!r.is_empty(), "自定义方案 xso 应有候选");
}

#[test]
fn test_custom_invalid_json_does_not_break() {
    // 坏 JSON 不应 panic，from_json 返回 Err
    let bad = crate::shuangpin::ShuangpinScheme::from_json("not json");
    assert!(bad.is_err());
    // 引擎层：FFI 注入坏 JSON 时 set_shuangpin_scheme(None) 由 FFI 负责（见 config.rs）
    // 这里只验证 from_json 的错误处理不崩溃
}

#[test]
fn test_shuangpin_green_memo_lift() {
    // 双拼 commit 绿词 → 双拼查询命中并提升到首位。
    // green_memo 查询与 lift 都必须以双拼原文为 key（is_full_segment =
    // seg.concat()==input，"zhongguohao" != "vsgohc"），且双拼路径同样要有
    // lift 块（首次即记忆 + 优先提升），否则 commit 写入的
    // "vsgohc" key 是死数据。因此在排序后加 lift（key=双拼原文对齐）。
    let mut engine = Engine::new();
    let scheme = crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe")
        .unwrap()
        .clone();
    engine.set_shuangpin_scheme(Some(scheme));
    // vsgohc = zhong+guo+hao（小鹤），「中国好」非词库整词，commit 组装为绿词
    engine.commit("vsgohc", "中国好", Some("vsgohc"), true);
    let r = engine.process("vsgohc");
    assert!(
        r.first().map(|c| c.text.as_ref()) == Some("中国好"),
        "双拼查询 vsgohc 首候选应为记忆过的「中国好」: {:?}",
        r.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
    // 对照：未 commit 过的输入不触发提升（绿词走正常 DP）
    let r2 = engine.process("vsgo");
    assert!(
        r2.first().map(|c| c.text.as_ref()) == Some("中国"),
        "vsgo 首候选应为词库「中国」: {:?}",
        r2.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
}

#[test]
fn test_shuangpin_no_duplicate_candidates() {
    // 回归：搜狗/微软双拼输入 vi（之）候选框出现重复字。
    // 根因：candidates_for_syllables 内部 extend(query_single_chars_fuzzy)
    // 与 word_candidates_for_syllables 的产出跨列表重复（单音节输入时两者都产出单字）。
    // 处理：candidates_for_syllables 合并时去重，让所有调用方自动受益。
    // 此测试守护：任意双拼输入的候选不得有重复 text。
    let mut engine = Engine::new();
    let sogou = crate::shuangpin::ShuangpinScheme::builtin_by_id("sogou")
        .unwrap()
        .clone();
    engine.set_shuangpin_scheme(Some(sogou));
    // vi = zhi（搜狗 v=zh i=i）
    let cands = engine.process("vi");
    let mut seen = std::collections::HashSet::new();
    for c in &cands {
        assert!(
            seen.insert(c.text.as_ref()),
            "候选出现重复 text=\"{}\"",
            c.text
        );
    }
    // 同样验证多音节（vsgo = zhongguo）
    let cands2 = engine.process("vsgo");
    let mut seen2 = std::collections::HashSet::new();
    for c in &cands2 {
        assert!(
            seen2.insert(c.text.as_ref()),
            "多音节候选出现重复 text=\"{}\"",
            c.text
        );
    }
}

#[test]
fn test_shuangpin_multisyllable_no_panic() {
    // 回归：双拼模式 + ≥3 音节输入会触发 duan 保护指针越界 panic。
    // 根因：word_candidates_for_syllables 的 duan 保护用 syllable.as_ptr() - input.as_ptr()
    // 算偏移，假设音节是 input 的子切片。双拼路径的音节是 decode_buffer 产出的 owned
    // String，与 input 无关 → UB → 越界 panic。处理：指针范围检查，不在范围内跳过保护。
    let mut engine = Engine::new();
    let scheme = crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe")
        .unwrap()
        .clone();
    engine.set_shuangpin_scheme(Some(scheme));
    // 这些输入产出 ≥3 音节，触发 duan 保护循环 for k in 2..syllables.len()
    for input in &["nihaoma", "vsgogo", "woaixcn", "nihao"] {
        let cands = engine.process(input); // 不应 panic
        assert!(
            !cands.is_empty() || input == &"woaixcn",
            "process(\"{}\") 应有候选（或至少不 panic）",
            input
        );
    }
}

#[test]
fn test_force_fullpinyin_bypasses_shuangpin() {
    // force_fullpinyin=true 时，即使 shuangpin scheme 启用，也走全拼路径。
    // 验证：双拼模式下 nihao 默认会被 decode_buffer 解成音节（ni+ha+o 等，错误候选），
    // 但 force_fullpinyin=true 后应走全拼 DP 枚举出"你好"。
    let mut engine = Engine::new();
    let scheme = crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe")
        .unwrap()
        .clone();
    engine.set_shuangpin_scheme(Some(scheme));

    // force_fullpinyin=true，nihao 走全拼 → 应有"你好"
    engine.set_force_fullpinyin(true);
    let fp_cands = engine.process("nihao");
    assert!(
        fp_cands.iter().any(|c| c.text.as_ref() == "你好"),
        "force_fullpinyin=true 时 nihao 应出「你好」（全拼路径）: {:?}",
        fp_cands.iter().map(|c| &c.text).take(5).collect::<Vec<_>>()
    );
}

#[test]
fn test_force_fullpinyin_default_false_no_regression() {
    // 默认 force_fullpinyin=false，双拼行为不变
    let mut engine = Engine::new();
    assert!(!engine.force_fullpinyin);
    let scheme = crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe")
        .unwrap()
        .clone();
    engine.set_shuangpin_scheme(Some(scheme));
    // vsgo 应出"中国"（双拼正常工作）
    let cands = engine.process("vsgo");
    assert!(cands.iter().any(|c| c.text.contains("中")));
}

#[test]
fn test_force_fullpinyin_display_layer_uses_fullpinyin() {
    // 回归：force_fullpinyin=true 时，显示/工具层（segment/count/prefix_len）
    // 必须走全拼，不能走双拼。否则 pre-edit 显示按双拼 2 字符分组（与候选的全拼不一致）、
    // 自组词切 buffer 按双拼算偏移导致残留。
    // 根因：这些方法的 `if let Some(scheme) = &self.shuangpin` 漏了 force_fullpinyin 守卫。
    let mut engine = Engine::new();
    let scheme = crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe")
        .unwrap()
        .clone();
    engine.set_shuangpin_scheme(Some(scheme));
    engine.set_force_fullpinyin(true);

    // syllable_segmentation: nihao 应切 ["ni","hao"]（全拼），不是 ["ni","ha","o"]（双拼 2 字符分组）
    let seg = engine.syllable_segmentation("nihao");
    assert_eq!(
        seg,
        vec!["ni".to_string(), "hao".to_string()],
        "force_fullpinyin 时 segment 应走全拼切分，got {:?}",
        seg
    );

    // syllable_count: nihao 全拼 = 2 音节
    assert_eq!(
        engine.syllable_count("nihao"),
        2,
        "force_fullpinyin 时 syllable_count 应按全拼算"
    );

    // syllable_prefix_len: nihao 前 1 音节 = "ni" = 2 字节
    assert_eq!(
        engine.syllable_prefix_len("nihao", 1),
        2,
        "force_fullpinyin 时 syllable_prefix_len 应按全拼算"
    );

    // is_complete_pinyin: nihao 全拼完整 = true
    assert!(
        engine.is_complete_pinyin("nihao"),
        "force_fullpinyin 时 is_complete_pinyin 应按全拼判"
    );
}

// ── 单键不完整声母回归 ──

/// 种子库 → 临时副本（tests_query.rs seed_copy 同范式：不直接读写仓库种子）。
fn seed_copy_sp(tag: &str) -> String {
    crate::test_tmp::require_seed_db();
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_seed_sp_{}_{}_{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::copy(seed, &tmp).expect("copy seed db");
    tmp.to_str().unwrap().to_string()
}

#[test]
fn test_shuangpin_single_initial_matches_fullpinyin() {
    // 回归：双拼（小鹤）单键 d = 不完整声母，候选应与全拼 d 同源
    // ——「的」必须出现且排首位。
    //
    // 缺陷：process_shuangpin_inner 把「整个输入只有 1 个不完整声母」也当成
    // 「多音节末尾不完整」送 query_tokens 纯 Initial 子树洪泛——收集按 Trie
    // 存储序进行且带 2000 上限，de 节点（的/得/德，全语料最高频音节之一）
    // 从未被访问 →「的」完全消失；幸存的 ~1999 个候选再被截成 300 个按
    // 音节存储序排列的列表（对/多/度/读…），频次序全毁。
    //
    // 现在：单声母退化（normalized 恰 1 项且非有效音节）改走
    // candidates_for_syllables——与「v」→zh（2 字符声母，原本就正确）及
    // 全拼用户输入 "d"（segmentations_with_partial → candidates_for_syllables）
    // 完全同一条查询路径，天然获得频次序单字 + 前缀词。
    let db = seed_copy_sp("d1");
    let mut engine = Engine::new_with_db(&db).expect("seed db");
    let xiaohe = crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe")
        .unwrap()
        .clone();

    engine.set_shuangpin_scheme(Some(xiaohe.clone()));
    let sp = engine.process("d");
    assert!(
        sp.first().map(|c| c.text.as_ref()) == Some("的"),
        "双拼(小鹤)单键 d 首候选应为「的」，实际: {:?}",
        sp.iter().map(|c| &c.text).take(10).collect::<Vec<_>>()
    );

    // 泛化守护：n（另一非重映射单声母）首候选应与全拼 n 一致
    let spn = engine.process("n");
    engine.set_shuangpin_scheme(None);
    let fpn = engine.process("n");
    assert_eq!(
        spn.first().map(|c| c.text.as_ref()),
        fpn.first().map(|c| c.text.as_ref()),
        "双拼/全拼单键 n 首候选应一致\nsp={:?}\nfp={:?}",
        spn.iter().map(|c| &c.text).take(5).collect::<Vec<_>>(),
        fpn.iter().map(|c| &c.text).take(5).collect::<Vec<_>>()
    );

    // 不回归：重映射键 v→zh 原本就正确（走着同一条路径）
    engine.set_shuangpin_scheme(Some(xiaohe));
    let spv = engine.process("v");
    assert!(
        spv.iter().take(3).any(|c| c.text.as_ref() == "着"),
        "双拼(小鹤) v→zh 候选应含「着」: {:?}",
        spv.iter().map(|c| &c.text).take(5).collect::<Vec<_>>()
    );
    // 不回归：多音节末尾不完整（vsg = zhong+g）行为不变
    let sp3 = engine.process("vsg");
    assert!(
        sp3.first().map(|c| c.text.as_ref()) == Some("中国"),
        "vsg 首候选应仍为「中国」: {:?}",
        sp3.iter().map(|c| &c.text).take(5).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_file(&db);
}

/// 双拼模式 + 手动 ' 分段的显示切分（交互守护）：
/// syllable_segmentation 的手动 ' 分段分支位于双拼分支之前——双拼用户按 '
/// 也是显式段界：各段按双拼 2 键分组，撇号落回按下位置，不爆炸不吞没。
#[test]
fn test_shuangpin_segmentation_manual_apostrophe() {
    let mut engine = Engine::new();
    let scheme = crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe")
        .unwrap()
        .clone();
    engine.set_shuangpin_scheme(Some(scheme));
    let disp = |input: &str| engine.syllable_segmentation(input).join("'");
    // 段界保留 + 各段双拼 2 键分组
    assert_eq!(disp("vs'go"), "vs'go");
    // 尾撇号有显示反馈
    assert_eq!(disp("vsgo'"), "vs'go'");
    // 无 ' 输入零变化（双拼分支照旧）
    assert_eq!(disp("vsgo"), "vs'go");
    // 奇数尾段（半音节）如实呈现
    assert_eq!(disp("vs'g"), "vs'g");
}
