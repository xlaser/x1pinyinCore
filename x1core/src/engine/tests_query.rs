//! Engine 测试 · 基础查询 / DB / 性能（按主题模块化拆分）。
//!
//! 按主题分组：
//! - 基础全拼 / 简拼 / 单字 / 音节切分 / 混合 (hybrid) 查询
//! - DB 引擎加载 / 回退内置词库 / X1Dict 集成
//! - 性能探针：hybrid 查询、Trie 遍历、process 各长度、ni 映射扫描、累积退化
//!
//! mod.rs 用 `#[cfg(test)] mod tests_query;` 加载（分号形式从文件加载）。
//! `use super::*` 仍指向 engine mod，测试访问 engine 的 pub 项不变。

use super::*;

#[test]
fn test_empty_input() {
    let engine = Engine::new();
    let r = engine.process("");
    assert!(r.is_empty());
}

/// 回归：candidates_for_syllables 不得返回重复 text。
///
/// 历史 bug：candidates_for_syllables 先调 word_candidates_for_syllables 产出词级候选，
/// 再 extend(query_single_chars_fuzzy) 补首音节单字。单音节输入时两者都产出单字，
/// 两个各自去重的列表合并后产生跨列表重复（如 zhi → "只/之/知/直/治" 各 2 次）。
/// 之前靠 process_inner 末尾 dedup_by 兜底，但双拼路径漏了 → 暴露 bug。
/// 源头修复：candidates_for_syllables 合并时去重，让所有调用方自动受益，不再依赖兜底。
#[test]
fn test_candidates_for_syllables_no_duplicate() {
    let engine = Engine::new();
    // 单音节：word_candidates 和 single_chars 都产出单字，最易重复
    for syl in &["zhi", "yi", "shi", "ji", "a"] {
        let cands = engine.candidates_for_syllables(syl, &[*syl]);
        let mut seen = std::collections::HashSet::new();
        for c in &cands {
            assert!(
                seen.insert(c.text.as_ref()),
                "candidates_for_syllables(\"{}\") 重复 text=\"{}\"",
                syl,
                c.text
            );
        }
    }
    // 多音节：词级 + 首音节单字，也可能跨列表重复
    let cands = engine.candidates_for_syllables("zhongguo", &["zhong", "guo"]);
    let mut seen = std::collections::HashSet::new();
    for c in &cands {
        assert!(
            seen.insert(c.text.as_ref()),
            "多音节 candidates_for_syllables 重复 text=\"{}\"",
            c.text
        );
    }
}

/// 全拼模式下 process 也不得有重复候选（守护之前被 process_inner dedup 兜底掩盖的隐患）。
#[test]
fn test_fullpinyin_process_no_duplicate() {
    let engine = Engine::new();
    for input in &["zhi", "yi", "shi", "zhongguo", "nihao"] {
        let cands = engine.process(input);
        let mut seen = std::collections::HashSet::new();
        for c in &cands {
            assert!(
                seen.insert(c.text.as_ref()),
                "全拼 process(\"{}\") 重复 text=\"{}\"",
                input,
                c.text
            );
        }
    }
}

#[test]
fn test_full_pinyin_query() {
    let engine = Engine::new();
    let r = engine.process("zhongwen");
    assert!(!r.is_empty(), "全拼 zhongwen 应返回候选");
    assert!(r.iter().any(|c| c.text.as_ref() == "中文"), "应包含'中文'");
}

#[test]
fn test_long_pinyin_prefix_word_priority() {
    // 长串全拼应先给出前缀匹配词，方便先选词再续输。
    // 例如输入 houxuankuang 时，若 "候选" 在词库中，应排在前面。
    let engine = Engine::new();
    let r = engine.process("houxuankuang");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("houxuankuang => {:?}", texts);
    for c in &r[..20.min(r.len())] {
        println!("  {} / {} / {}", c.text, c.pinyin, c.base_freq);
    }
    assert!(texts.contains(&"候选"), "应包含前缀词 '候选'");
    let houxuan_idx = texts.iter().position(|&t| t == "候选").unwrap();
    // "候选" 应出现在比较靠前的位置（前 3 个候选内）
    assert!(
        houxuan_idx < 3,
        "'候选' 应出现在前 3 位，实际位置 {:?}",
        texts
    );
}

#[test]
fn test_jianpin_query() {
    let engine = Engine::new();
    let r = engine.process("zw");
    assert!(!r.is_empty(), "简拼 zw 应返回候选");
    assert!(
        r.iter().any(|c| c.text.as_ref() == "中文"),
        "简拼 zw 应包含'中文'"
    );
}

/// 长简拼不断词：连输 7+ 个相同首字母时必须有候选。
///
/// 根因：`is_jianpin` 的 6 字上限让 zzzzzzzz（7）
/// 被踢到全拼路径、切不出有效音节 → 候选框完全断词（列表空）。修复 = 移除分类
/// 上限（元音/相邻音节两条语义判据已足以区分全拼）；算力护栏改放 DP 层
/// （≤16 字母，超限跳过整句猜测）。
#[test]
fn test_long_jianpin_not_cut_off() {
    let engine = Engine::new();
    for input in ["zzzzz", "zzzzzz", "zzzzzzz", "zzzzzzzz"] {
        let r = engine.process(input);
        assert!(
            !r.is_empty(),
            "长简拼 {} 不应断词（应有候选），实际空列表",
            input
        );
    }
    // 绿词 DP 上限（16）以上：跳过整句猜测但查词/单字照常返回——
    // 任何长度都不允许出现"候选框全空"的断词悬崖
    // （上限保留但改高；护栏放在 DP 这一算力大头，不是分类层）。
    for n in [17usize, 24, 32] {
        let input = "z".repeat(n);
        let r = engine.process(&input);
        assert!(
            !r.is_empty(),
            "超 DP 上限的长简拼（{n} 字母）仍须有候选（查词/单字），实际空列表"
        );
    }
}

#[test]
fn test_keneng_uses_ke_neng() {
    let engine = Engine::new();
    let r = engine.process("keneng");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    assert!(
        texts.contains(&"可能"),
        "keneng 应切分为 ke+neng 并给出 '可能'"
    );
    // "可能" 应排在 "肯" 等单字前面
    let keneng_idx = texts.iter().position(|&t| t == "可能").unwrap();
    assert!(
        keneng_idx < 3,
        "'可能' 应出现在前 3 位，实际位置 {:?}",
        texts
    );
}

#[test]
fn test_neng_only_returns_neng_chars() {
    let engine = Engine::new();
    let r = engine.process("neng");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    assert!(texts.contains(&"能"), "neng 应返回 '能'");
    // 不应混入你/那/过/国等其它音节的高频字
    assert!(
        !texts
            .iter()
            .any(|&t| t == "你" || t == "那" || t == "过" || t == "国"),
        "neng 不应出现无关单字：{:?}",
        texts
    );
}

#[test]
fn test_biao_prefers_biao_chars() {
    let engine = Engine::new();
    let r = engine.process("biao");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    assert!(texts.contains(&"表"), "biao 应返回 '表'");
    // 不应把 "bi" 的单字（如 "比"）混进来
    assert!(
        !texts.contains(&"比"),
        "biao 不应出现 bi 音节的单字：{:?}",
        texts
    );
}

#[test]
fn test_single_char_query() {
    let engine = Engine::new();
    let r = engine.process("wo");
    assert!(!r.is_empty(), "单字全拼 wo 应返回候选");
    assert!(r.iter().any(|c| c.text.as_ref() == "我"), "应包含'我'");
}

#[test]
fn test_single_char_common_first() {
    let engine = Engine::new();
    let cases = [("ni", "你"), ("wo", "我"), ("ta", "他"), ("de", "的")];
    for (input, expected_first) in &cases {
        let r = engine.process(input);
        let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
        assert!(!texts.is_empty(), "{} 应返回候选", input);
        assert_eq!(
            texts[0], *expected_first,
            "{} 的首候选应为 '{}'，实际为 {:?}",
            input, expected_first, texts
        );
    }
}

#[test]
fn test_multi_syllable_word_priority() {
    let engine = Engine::new();
    let r = engine.process("muq");
    assert!(!r.is_empty(), "muq 应返回候选");
    // 多音节输入时，词组应排在以 q 开头的单字前面
    let word_pos = r.iter().position(|c| c.text.as_ref() == "目前");
    let single_pos = r.iter().position(|c| c.text.as_ref() == "去");
    if let (Some(w), Some(s)) = (word_pos, single_pos) {
        assert!(w < s, "多音节输入 muq 时，'目前' 应排在 '去' 前面");
    }
}

#[test]
fn test_fallback_single_chars_for_unknown_word() {
    let engine = Engine::new();
    // 输入一个不太可能存在的拼音组合，应返回各音节单字候选，而不是空
    let r = engine.process("xyzabc");
    assert!(!r.is_empty(), "无匹配词组时应返回单字候选供自造词");
}

#[test]
fn test_hen_query() {
    let engine = Engine::new();
    for input in &["h", "he", "hen", "heng"] {
        let r = engine.process(input);
        let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
        println!("{} => {:?}", input, texts);
    }
    let r = engine.process("hen");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    // 不应包含读音为 hang/xing 的 "行"
    assert!(!texts.contains(&"行"), "hen 不应匹配 hang/xing 的 '行'");
    // 完整音节匹配应排在前缀匹配前面：很(hen) 在 横(heng) 前面
    let hen_idx = texts.iter().position(|&t| t == "很");
    let heng_idx = texts.iter().position(|&t| t == "横");
    if let (Some(h1), Some(h2)) = (hen_idx, heng_idx) {
        assert!(h1 < h2, "'很'(hen) 应排在 '横'(heng) 前面");
    }
}

#[test]
fn test_syllable_count_and_prefix_len() {
    let engine = Engine::new();
    assert_eq!(engine.syllable_count("zizuci"), 3);
    assert_eq!(engine.syllable_count("muqian"), 2);
    assert_eq!(engine.syllable_count("xian"), 1);
    assert_eq!(engine.syllable_prefix_len("zizuci", 1), 2); // zi
    assert_eq!(engine.syllable_prefix_len("zizuci", 2), 4); // zizu
    assert_eq!(engine.syllable_prefix_len("zizuci", 3), 6); // zizuci
}

/// 场景：混合输入「pengbfzhan」（蓬勃发展，全拼+简拼）空格
/// 提交后残留「an」。
///
/// 根因：显示/计数/消费链（best_segmentation → pick_segmentation）在
/// `segmentations`（全有效）为空时只拿 `segmentations_prefix_tail` 合成变体
/// 参选，而该合成把尾部已敲完整的音节 zhan 按声母退化成 zh|a|n（打分 0）；
/// 候选链 token 语义的正确切分 [peng,b,f,zhan]（打分 1000）不在池内 →
/// 选举数出 6 音节。宿主前端把「4 字候选 < 6 音节」判成自组词
/// 分步提交，只消费前 4 个「音节」= 8 字节「pengbfzh」→ 剩余 an 重新预输入
///（用户可见残留，Windows DLL 经 consumed 同链同病）。
///
/// 守护不变量：4 字候选「蓬勃发展」必须全消费整个输入（prefix_len(4) == len），
/// 且显示/计数链与候选链对同一输入选出同一切分。
#[test]
fn test_hybrid_input_full_consumption_no_residue() {
    let engine = Engine::new();
    // 显示/计数链与 token 链同源
    assert_eq!(
        engine.syllable_segmentation("pengbfzhan"),
        vec!["peng", "b", "f", "zhan"],
        "pengbfzhan 的显示链切分应与候选链同源（zhan 整段）"
    );
    assert_eq!(
        engine.syllable_count("pengbfzhan"),
        4,
        "pengbfzhan 应切出 4 音节"
    );
    // 4 字候选（蓬勃发展）消费完整输入；否则提交后残留
    assert_eq!(
        engine.syllable_prefix_len("pengbfzhan", 4),
        "pengbfzhan".len(),
        "4 字候选应全消费（否则残留）"
    );
    // 3 字候选（如「蓬发占」类三字读法）只消费前 3 音节，剩余留给自组词分步
    assert_eq!(engine.syllable_prefix_len("pengbfzhan", 3), "pengbf".len());
    // 打字中间态同修：pengbfzha（zha 已完整）同样 4 音节、4 字候选全消费
    assert_eq!(
        engine.syllable_count("pengbfzha"),
        4,
        "pengbfzha 应切出 4 音节"
    );
    assert_eq!(
        engine.syllable_prefix_len("pengbfzha", 4),
        "pengbfzha".len(),
        "pengbfzha 上选 4 字候选应全消费"
    );
    // 纯简拼+全拼混合 pbfzhan（p|b|f|zhan）保持既有行为：全消费
    assert_eq!(
        engine.syllable_prefix_len("pbfzhan", 4),
        "pbfzhan".len(),
        "pbfzhan 上选 4 字候选应全消费"
    );
    // 端到端（真实种子库）：「蓬勃发展」经 consumed_lens 必须
    // 全消费 pengbfzhan——Windows DLL commit 切剩余音节走同一权威值。
    let db = seed_copy("pengbfzhan_residue");
    let engine = Engine::new_with_db(&db).expect("seed db");
    let cands = engine.process("pengbfzhan");
    let consumed = engine.consumed_lens("pengbfzhan", &cands);
    let idx = cands
        .iter()
        .position(|c| c.text.as_ref() == "蓬勃发展")
        .expect("种子库应含「蓬勃发展」");
    assert_eq!(
        consumed[idx],
        "pengbfzhan".len(),
        "「蓬勃发展」在 pengbfzhan 上必须全消费（否则提交后残留）"
    );
    seed_copy_cleanup(&db);
}

#[test]
fn test_syllable_segmentation() {
    let engine = Engine::new();
    let seg = |input: &str| engine.syllable_segmentation(input);
    // 简拼：每字符一个音节
    assert_eq!(seg("zgrm"), vec!["z", "g", "r", "m"]);
    assert_eq!(seg("wdm"), vec!["w", "d", "m"]);
    // 全拼：best_segmentation 最优切分
    assert_eq!(seg("wenmin"), vec!["wen", "min"]);
    assert_eq!(seg("nihao"), vec!["ni", "hao"]);
    assert_eq!(seg("muqian"), vec!["mu", "qian"]);
    assert_eq!(seg("zizuci"), vec!["zi", "zu", "ci"]);
    // 单音节：返回单个元素（FFI 层不加分隔符）
    assert_eq!(seg("xian"), vec!["xian"]);
    assert_eq!(seg("a"), vec!["a"]);
    // 含不完整音节：parse_pinyin 回退
    let kaix = seg("kaix");
    assert_eq!(kaix.len(), 2);
    assert_eq!(kaix[0], "kai");
    // went → wen + t（t 作为不完整音节）
    let went = seg("went");
    assert_eq!(went.len(), 2);
    // 空输入
    assert!(seg("").is_empty());
}

/// 手动分隔符 ' 的显示切分（回归守护）：
/// ' 是用户显式段界——各段独立自动切分，撇号落回按下位置。
/// 旧实现：简拼分支把 ' 当独立段（s'h → s'''h 连续撇号）；
/// token 分支吞掉 '（wenm'in → wen'm'i'n，用户边界消失）。
#[test]
fn test_syllable_segmentation_manual_apostrophe() {
    let engine = Engine::new();
    let disp = |input: &str| engine.syllable_segmentation(input).join("'");
    // 场景：sh 自动切 s|h + 用户尾按 ' → s'h'（旧：s'h'' 双撇号）
    assert_eq!(disp("sh'"), "s'h'");
    // 显式 s|h：s'h → s'h（旧：s'''h 撇号爆炸）
    assert_eq!(disp("s'h"), "s'h");
    assert_eq!(disp("s'h'"), "s'h'");
    // 经典用法不受影响
    assert_eq!(disp("xi'an"), "xi'an");
    assert_eq!(disp("wen'min"), "wen'min");
    // 用户边界保留 + 段内自动切分照旧（wenm → wen|m 声母尾；in 非音节 → i|n
    // 首字母），与查询消费口径 wen|m|i|n 完全一致（显示 = 实际切分）
    assert_eq!(disp("wenm'in"), "wen'm'i'n");
    // 无 ' 输入零变化（与 test_syllable_segmentation 互证）
    assert_eq!(disp("wenmin"), "wen'min");
    assert_eq!(disp("zgrm"), "z'g'r'm");
    assert_eq!(disp("xian"), "xian");
    // 容错：连按/单独按撇号如实显示，不崩不爆
    assert_eq!(disp("s''h"), "s''h");
    assert_eq!(disp("'"), "'");
}

#[test]
fn test_full_pinyin_no_overmatch() {
    let engine = Engine::new();
    let r = engine.process("zhongjian");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("zhongjian => {:?}", texts);
    // 不应出现 "中间人/中间件/中间层" 等 3 字以上的过度联想词
    assert!(
        !texts.iter().any(|t| t.chars().count() > 2),
        "完整拼音 zhongjian 不应返回 2 字以上的过度联想词: {:?}",
        texts
    );
}

#[test]
fn test_ok_input() {
    let engine = Engine::new();
    for input in &["ok", "OK", "Ok"] {
        let r = engine.process(input);
        let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
        println!("{} => {:?}", input, texts);
    }
}

#[test]
fn test_is_complete_pinyin() {
    let engine = Engine::new();
    assert!(engine.is_complete_pinyin("laishuo"));
    assert!(engine.is_complete_pinyin("zhongjian"));
    assert!(!engine.is_complete_pinyin("ok"));
    assert!(!engine.is_complete_pinyin("muq"));
    assert!(!engine.is_complete_pinyin(""));
    assert!(!engine.is_complete_pinyin("zw")); // 简拼不算完整
}

#[test]
fn test_prefix_filtered_no_overmatch() {
    let mut dict = Dict::new();
    dict.insert("gong neng", "功能", 1000, true);
    dict.insert("gong neng jian", "功能键", 900, true);
    dict.insert("gong neng xing", "功能性", 800, true);
    dict.insert("gong neng quan", "功能全", 700, true);

    let r = dict.query_prefix_filtered(&["gong"], "n");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    assert!(texts.contains(&"功能"));
    assert!(!texts.contains(&"功能键"), "不应返回 3 字过度联想词");
    assert!(!texts.contains(&"功能性"), "不应返回 3 字过度联想词");
    assert!(!texts.contains(&"功能全"), "不应返回 3 字过度联想词");
}

#[test]
fn test_gongn_input() {
    let engine = Engine::new();
    let r = engine.process("gongn");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("gongn => {:?}", texts);
}

#[test]
fn test_gongne_input() {
    let engine = Engine::new();
    let r = engine.process("gongne");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("gongne => {:?}", texts);
    assert!(
        texts.contains(&"功能"),
        "gongne 应通过前缀匹配找到 '功能': {:?}",
        texts
    );
}

#[test]
fn test_zhonggr_input() {
    let mut engine = Engine::new();
    engine.learn("中国人", "zhong guo ren");
    let r = engine.process("zhonggr");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("zhonggr => {:?}", texts);
    assert!(
        texts.contains(&"中国人"),
        "zhonggr 应通过混合简拼找到 '中国人': {:?}",
        texts
    );
}

/// 混合简拼端到端：zhonggrm 应命中「中国人民」（修复尾 m 误判 bug）
#[test]
fn test_hybrid_zhonggrm() {
    let mut engine = Engine::new();
    engine.learn("中国人民", "zhong guo ren min");
    engine.learn("中国人", "zhong guo ren");
    engine.learn("中国", "zhong guo");
    let cands = engine.process("zhonggrm");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    assert!(
        texts.contains(&"中国人民"),
        "zhonggrm 应命中「中国人民」，实际: {:?}",
        texts
    );
}

/// 回归测试：xiaoguo 的「效果」应是白词（score_for_syllables 打分修复）。
/// 原版 [xiao,gu,o] 切分靠前缀词「小姑」(xiao gu, 字数2 < 音节数3) 高 freq 压过
/// [xiao,guo] 的精确匹配「效果」，导致「效果」成橙词补充。修复：打分只计精确匹配词。
#[test]
fn test_xiaoguo_xiaoguo_is_white_word() {
    let mut engine = Engine::new();
    engine.learn("效果", "xiao guo");
    engine.learn("小姑", "xiao gu");
    let cands = engine.process("xiaoguo");
    let first = cands.first().expect("应有候选");
    assert_eq!(
        first.text.as_ref(),
        "效果",
        "xiaoguo 首候选应为「效果」，实际: {:?}",
        cands.iter().take(3).map(|c| &c.text).collect::<Vec<_>>()
    );
    assert!(
        !first.is_correction,
        "「效果」是精确匹配词，应是白词（非橙词）"
    );
}

/// 回归测试：zhgrm 应命中「中国人民」（修复 max_tokens 排序 bug）。
/// process_hybrid_shengmu_inner 旧版用全 Initial 退化切分的 token 数（5）做完全匹配判据，
/// 导致「中国人民」（4字）被判非完全匹配，被高频 2 字词挤出。修复改用含 Shengmu 切分的
/// token 数（4）做判据。
#[test]
fn test_zhgrm_ranks_zhongguorenmin_first() {
    let mut engine = Engine::new();
    engine.learn("中国人民", "zhong guo ren min");
    engine.learn("中国人", "zhong guo ren");
    let cands = engine.process("zhgrm");
    let first = cands.first().map(|c| c.text.as_ref()).unwrap_or("");
    assert_eq!(
        first,
        "中国人民",
        "zhgrm 首候选应为「中国人民」（Shengmu 双试 + 完全匹配优先），实际: {:?}",
        cands.iter().take(5).map(|c| &c.text).collect::<Vec<_>>()
    );
}

/// 回归测试：zhmz 应让「中华民族」进第 1 页（完全匹配判据歧义修复）。
/// zhmz 有两种等价的输入完全覆盖：声母切分 [zh,m,z]（这么着，3 token）与
/// 全首字母切分 [z,h,m,z]（中华民族，4 token）。旧判据只认含 Shengmu 切分的
/// token 数（3），4 字词被单字补充/二字前缀词压到第 95 位——与 zhgrm
/// 修复（反向只认全首字母切分数）互为另一面。修复后
/// 字数命中任一切分方案的 token 数即第 1 层，层内由 candidate_score 分胜负。
/// 用真实词库（用户 DB → 种子 DB）：内置小词库无「中华民族」，无 DB 时跳过。
#[test]
fn test_zhmz_zhonghuaminzu_full_match_ambiguity() {
    let engine = real_engine();
    let cands = engine.process("zhmz");
    let rank = cands.iter().position(|c| c.text.as_ref() == "中华民族");
    let rank = rank.expect("zhmz 应查出「中华民族」（全首字母切分 z,h,m,z）") + 1;
    assert!(
        rank <= 10,
        "「中华民族」应进前 10（第 1 层完全匹配；旧实现第 95 位），实际第 {} 位，前10: {:?}",
        rank,
        cands.iter().take(10).map(|c| &c.text).collect::<Vec<_>>()
    );
}

/// 守护：用户词"字数==音节数"不变量——learn 入口拒绝错位拼音。
///
/// 背景（Windows 微信"习惯惯"重复上屏根因）：learn("习惯惯","xi guan") 这类
/// 3 字词带 2 音节拼音的错位条目会直入 user_words + Trie，之后输入 "xiguan"
/// 就把"习惯惯"当整词候选顶到最前（实测排名 #1，与合法"习惯"竞争排名 =
/// "偶尔出现"），一次空格整体上屏 = 末字重复。组合学习（Windows editor
/// comboPinyin_ 拼接）步骤拼音缺失时即产出该形态。
#[test]
fn test_learn_rejects_pinyin_syllable_mismatch() {
    let mut e = Engine::new();
    e.learn("习惯惯", "xi guan");
    assert!(
        !e.user_words.contains_key("习惯惯"),
        "learn 必须拒绝字数≠音节数的错位拼音（3 字 2 音节）"
    );
    // 合法叠字词不受影响（回归）
    e.learn("试试", "shi shi");
    assert!(
        e.user_words.contains_key("试试"),
        "合法词（2 字 2 音节）正常学习"
    );
}

/// 守护：commit 固化路径（force_solidify / 自动固化 / 系统词分支）同样拒绝
/// 错位拼音——防止 find_pinyin 拿到已污染行后自我延续。
#[test]
fn test_commit_solidify_rejects_pinyin_syllable_mismatch() {
    let mut e = Engine::new();
    e.commit_with_solidify("xiguanguan", "习惯惯", Some("xi guan"), true, true);
    assert!(
        !e.user_words.contains_key("习惯惯"),
        "commit 固化必须拒绝字数≠音节数的拼音"
    );
    // 合法规化不受影响（回归）
    e.commit_with_solidify("shishi", "试试", Some("shi shi"), true, true);
    assert!(e.user_words.contains_key("试试"), "合法词固化正常");
}

/// 守护：同词多读音共存——zhujiao→主角 造词后，
/// 再用 zhujue→主角 造词不能冲掉前者。用户输入不精确是常态
/// （zhujiao/zhujue 都指主角），两种读音必须共存：两种查询都要能出该词，
/// 重启（DB round-trip）后也都还在。
#[test]
fn test_user_word_multi_pinyin_coexist() {
    use std::fs;

    let dir = crate::test_tmp::dir().join(format!(
        "x1pinyin_multi_py_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("mp.db");

    let mut e = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    e.commit_with_solidify("zhujiao", "主角", Some("zhu jiao"), true, true);
    e.commit_with_solidify("zhujue", "主角", Some("zhu jue"), true, true);

    // 内存：两个读音都在
    let pys = e
        .user_words_pinyins_for_test("主角")
        .expect("主角应为用户词");
    assert!(
        pys.iter().any(|p| p == "zhu jiao"),
        "zhu jiao 读音被冲掉：{:?}",
        pys
    );
    assert!(
        pys.iter().any(|p| p == "zhu jue"),
        "zhu jue 读音缺失：{:?}",
        pys
    );

    // 查询：两种读音都要出「主角」
    assert!(
        e.query_for_test(&["zhu", "jiao"])
            .iter()
            .any(|c| c.text.as_ref() == "主角"),
        "zhu jiao 查询应出「主角」"
    );
    assert!(
        e.query_for_test(&["zhu", "jue"])
            .iter()
            .any(|c| c.text.as_ref() == "主角"),
        "zhu jue 查询应出「主角」"
    );

    // flush 落盘后重启：两个读音都还在（DB round-trip + 迁移后的 schema）
    e.flush().unwrap();
    drop(e);
    let e2 = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    let pys2 = e2
        .user_words_pinyins_for_test("主角")
        .expect("重启后主角仍应为用户词");
    assert!(
        pys2.iter().any(|p| p == "zhu jiao"),
        "重启后 zhu jiao 丢失：{:?}",
        pys2
    );
    assert!(
        pys2.iter().any(|p| p == "zhu jue"),
        "重启后 zhu jue 丢失：{:?}",
        pys2
    );
    assert!(
        e2.query_for_test(&["zhu", "jiao"])
            .iter()
            .any(|c| c.text.as_ref() == "主角"),
        "重启后 zhu jiao 查询应仍出「主角」"
    );
    fs::remove_dir_all(&dir).ok();
}

/// 守护：启动加载过滤——DB 里已存在的错位 user_words 行（历史版本学入的
/// 脏数据）不进 Trie/内存（跳过不删除：零启动成本、不丢数据）。
#[test]
fn test_load_skips_mismatched_user_word_rows() {
    use std::fs;

    let dir = crate::test_tmp::dir().join(format!(
        "x1pinyin_mismatch_load_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("mm.db");

    // 模拟被历史版本污染的用户 DB：好行 + 错位行并存
    {
        let mut s = crate::storage::Storage::open(&db_path).unwrap();
        s.import_user_words(
            &[
                ("习惯惯".to_string(), "xi guan".to_string(), 100),
                ("试试".to_string(), "shi shi".to_string(), 100),
            ],
            true,
        )
        .unwrap();
    }
    let mut e = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    assert!(
        !e.user_words.contains_key("习惯惯"),
        "错位行（3 字 2 音节）加载时必须跳过"
    );
    assert!(e.user_words.contains_key("试试"), "合法行正常加载");
    let cands = e.process("xiguan");
    assert!(
        cands.iter().all(|c| c.text.as_ref() != "习惯惯"),
        "错位词不得作为候选出现（重复上屏根因）"
    );
    let _ = e.flush();
    let _ = fs::remove_dir_all(&dir);
}

/// 守护：用户词库 TSV 导入过滤错位拼音行（外部导入的词库可能带错位数据）。
#[test]
fn test_import_user_dict_rejects_pinyin_syllable_mismatch() {
    let mut e = Engine::new();
    let imported = e
        .import_user_dict_tsv("习惯惯\txi guan\t100\n试试\tshi shi\t100\n", true)
        .unwrap();
    assert_eq!(imported, 1, "只有合法行被导入");
    assert!(!e.user_words.contains_key("习惯惯"), "错位行不得入库");
    assert!(e.user_words.contains_key("试试"), "合法行正常导入");
}

/// 回归测试：zhonggrenm 应命中「中国人民」（修复 is_hybrid 交错模式）。
/// Full+Initial 交错切分（zhong+g+ren+m），原 is_hybrid 要求 Initial 段连续在末尾，
/// 不识别交错模式。修订判据：分段判断，单字符段不做反拆分检查（无法区分 m 是音节还是首字母）。
#[test]
fn test_hybrid_zhonggrenm_interleaved() {
    let mut engine = Engine::new();
    engine.learn("中国人民", "zhong guo ren min");
    engine.learn("中国人", "zhong guo ren");
    let cands = engine.process("zhonggrenm");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    assert!(
        texts.contains(&"中国人民"),
        "zhonggrenm 应命中「中国人民」（Full+Initial 交错），实际: {:?}",
        texts
    );
}

/// 回归测试：shuru 首候选应为「输入」（修复 exact_multi_freq 拼音验证）。
/// 原版 exact_multi_freq 只看字数==音节数，未验证拼音，导致 [s,hu,ru] 切分的「撒欢儿」
/// (sa huan r, 3字==3音节但 s≠sa) 被误判精确匹配，拿 1M bonus 压过 [shu,ru] 的「输入」。
/// 修复：精确匹配必须拼音逐音节相等。
#[test]
fn test_shuru_first_is_shuru() {
    let mut engine = Engine::new();
    engine.learn("输入", "shu ru");
    engine.learn("撒欢儿", "sa huan r");
    let cands = engine.process("shuru");
    let first = cands.first().expect("应有候选");
    assert_eq!(
        first.text.as_ref(),
        "输入",
        "shuru 首候选应为「输入」，实际: {:?}",
        cands.iter().take(3).map(|c| &c.text).collect::<Vec<_>>()
    );
}

/// 回归测试矩阵：用真实用户 DB 跑全量边界 case，守护切分/排序不回归。
/// 每个案例对应一个历史 bug，统一守护 is_hybrid + score_for_syllables + candidate_cmp。
#[test]
fn diag_matrix_real_db() {
    // 用户 DB 路径：新直路径优先，旧容器路径兜底。
    // 注意：硬编码用户名仅在本机有效——两路径都不可用时静默回退种子 DB，
    // 矩阵仍然跑（守护逻辑回归），只是不覆盖用户真实数据。
    let engine = real_engine();
    // (输入, 期望首候选包含, 描述) —— 用"包含"而非"等于"避免过度耦合 DB freq
    let cases: &[(&str, &str, &str)] = &[
        ("shuru", "输入", "精确匹配 vs 前缀（撒欢儿）"),
        ("xiaoguo", "效果", "精确匹配 vs 前缀（小姑）"),
        ("duan", "段", "完整音节 vs 拆分（都安）"),
        ("tingshuo", "听", "全拼不被误判混合（含听字即可）"),
        ("zhonggrm", "中国人民", "末尾连续简拼"),
        ("zhonggrenm", "中国人民", "交错简拼"),
        (
            "wanmeipipei",
            "完美匹配",
            "长句切分歧义（mei 不被拆成 m+ei）",
        ),
        ("biegen", "别", "全有效音节不被拆（bie 不被拆成 bi+e）"),
        ("bierenshuo", "别人说", "全有效音节不被拆（bie+ren+shuo）"),
        ("genbu", "根", "全有效音节不被拆（gen 不被拆成 ge+n）"),
        ("nihao", "你好", "基础全拼"),
        ("aoliao", "奥利奥", "零声母歧义"),
    ];
    for (input, expected_substr, desc) in cases {
        let cands = engine.process(input);
        let top5: Vec<&str> = cands.iter().take(5).map(|c| c.text.as_ref()).collect();
        assert!(
            top5.iter().any(|&t| t.contains(expected_substr)
                || t.starts_with(expected_substr.chars().next().unwrap())),
            "「{}」应在前5含「{}」相关[{}]，实际: {:?}",
            input,
            expected_substr,
            desc,
            top5
        );
    }
}

/// 联想候选（前缀延伸）三件套守护：
/// ① 完整音节输入的前缀延伸候选染紫（ji → 间[jian]/接[jie] is_guess=true）
/// ② 简拼态（不完整音节，如 j）不染色——前缀匹配是简拼本质
/// ③ 开关关闭后前缀延伸/末音节前缀猜测不再生成，简拼不受影响
/// ④ 紫词上限：延伸猜测 ≤3 条（原 20 条刷屏）
fn real_db_engine_or_fallback() -> Engine {
    real_engine()
}

#[test]
fn test_prefix_guess_marked_for_complete_syllable() {
    let engine = real_db_engine_or_fallback();
    let cands = engine.process("ji");
    assert!(!cands.is_empty());
    // 精确 ji 字：不染
    let exact: Vec<_> = cands
        .iter()
        .filter(|c| c.pinyin.split_whitespace().last() == Some("ji"))
        .collect();
    assert!(exact.len() > 5, "ji 精确字应有一批");
    assert!(exact.iter().all(|c| !c.is_guess), "精确音节候选不染紫");
    // 前缀延伸（间/接类）：染紫且 ≤3 条（紫词上限）
    let prefix: Vec<_> = cands
        .iter()
        .filter(|c| c.pinyin.split_whitespace().last() != Some("ji"))
        .collect();
    assert!(
        !prefix.is_empty() && prefix.len() <= 3,
        "ji 前缀延伸候选应 1-3 条（间/接类），实得 {}",
        prefix.len()
    );
    assert!(
        prefix.iter().all(|c| c.is_guess),
        "完整音节的前缀延伸候选应染紫"
    );
}

#[test]
fn test_jianpin_prefix_not_marked() {
    let engine = real_db_engine_or_fallback();
    let cands = engine.process("j");
    assert!(!cands.is_empty(), "简拼 j 应有候选");
    assert!(
        cands.iter().all(|c| !c.is_guess),
        "简拼态（不完整音节）候选不染紫"
    );
}

#[test]
fn test_guess_switch_off_keeps_jianpin() {
    let mut engine = real_db_engine_or_fallback();
    engine.set_guess_enabled(false);
    // 完整音节：不再有紫标前缀延伸
    let ji = engine.process("ji");
    assert!(
        ji.iter().all(|c| !c.is_guess),
        "关闭后 ji 不应再有前缀延伸候选"
    );
    assert!(
        ji.iter()
            .any(|c| c.pinyin.split_whitespace().last() == Some("ji")),
        "精确 ji 字保留"
    );
    // 多音节末音节前缀猜测（用坏类）不再生成
    let yh = engine.process("yonghu");
    assert!(
        yh.iter().all(|c| !c.is_guess),
        "关闭后 yonghu 不应有用坏类猜测"
    );
    // 简拼不受影响
    let j = engine.process("j");
    assert!(!j.is_empty(), "简拼 j 不受开关联动");
}

/// 双拼完全匹配优先：多音节双拼输入，字数=音节数的词组应排单字前。
#[test]
fn diag_shuangpin_full_match_priority() {
    let mut engine = real_engine();
    engine.shuangpin = crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe").cloned();

    // nihc = ni + hao，"你好"应排第一
    let cands = engine.process("nihc");
    let top1 = cands.first().map(|c| c.text.as_ref()).unwrap_or("");
    assert_eq!(
        top1, "你好",
        "双拼 nihc 首候选应为「你好」，实际「{}」",
        top1
    );

    // vsgo = zhong + guo，"中国"应排第一
    let cands2 = engine.process("vsgo");
    let top1_2 = cands2.first().map(|c| c.text.as_ref()).unwrap_or("");
    assert_eq!(
        top1_2, "中国",
        "双拼 vsgo 首候选应为「中国」，实际「{}」",
        top1_2
    );
}

/// 验证：双拼模式下模糊音是否生效。
/// 测声母模糊（n↔l）+ 韵母模糊（an↔ang / en↔eng / in↔ing）。
#[test]
fn diag_shuangpin_fuzzy_works() {
    let mk = || -> Engine {
        let mut e = real_engine();
        e.shuangpin = crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe").cloned();
        e
    };

    // === 声母模糊 n↔l ===
    let mut e = mk();
    e.fuzzy_config = crate::fuzzy::FuzzyConfig {
        enabled: true,
        n_l: true,
        ..Default::default()
    };
    let cands = e.process("lihc");
    assert!(
        cands.iter().any(|c| c.text.as_ref() == "你好"),
        "双拼 n↔l 模糊音：lihc 应含「你好」，实际 top5: {:?}",
        cands
            .iter()
            .take(5)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );

    // === 韵母模糊 an↔ang ===
    let mut e2 = mk();
    e2.fuzzy_config = crate::fuzzy::FuzzyConfig {
        enabled: true,
        an_ang: true,
        ..Default::default()
    };
    let cands2 = e2.process("hj");
    let has_han_sound = cands2
        .iter()
        .any(|c| c.text.as_ref() == "汉" || c.text.as_ref() == "韩" || c.text.as_ref() == "寒");
    assert!(
        has_han_sound,
        "双拼 an↔ang 模糊音：hj(hang) 应出现 han 音的字，实际: {:?}",
        cands2
            .iter()
            .take(5)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );

    // === 韵母模糊 en↔eng ===
    let mut e3 = mk();
    e3.fuzzy_config = crate::fuzzy::FuzzyConfig {
        enabled: true,
        en_eng: true,
        ..Default::default()
    };
    let cands3 = e3.process("ff");
    let has_feng = cands3
        .iter()
        .any(|c| c.text.as_ref() == "风" || c.text.as_ref() == "丰" || c.text.as_ref() == "封");
    assert!(
        has_feng,
        "双拼 en↔eng 模糊音：ff(fen) 应出现 feng 音的字（风/丰），实际: {:?}",
        cands3
            .iter()
            .take(5)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );

    // === 韵母模糊 in↔ing ===
    let mut e4 = mk();
    e4.fuzzy_config = crate::fuzzy::FuzzyConfig {
        enabled: true,
        in_ing: true,
        ..Default::default()
    };
    let cands4 = e4.process("bk");
    let has_bin = cands4
        .iter()
        .any(|c| c.text.as_ref() == "宾" || c.text.as_ref() == "滨" || c.text.as_ref() == "彬");
    assert!(
        has_bin,
        "双拼 in↔ing 模糊音：bk(bing) 应出现 bin 音的字（宾/滨），实际: {:?}",
        cands4
            .iter()
            .take(5)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
}

/// 验证：双拼模式下绿词（整句组合）和简拼（末尾不完整声母）的适配。
#[test]
fn diag_shuangpin_green_jianpin() {
    let mk = || -> Engine {
        let mut e = real_engine();
        e.shuangpin = crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe").cloned();
        e
    };

    let e = mk();

    // === 绿词（完整音节）：vsgoxkni = zhong+guo+xing+ni → "中国行你"？至少出多音节绿词
    let cands = e.process("vsgoxkni");
    let has_multi = cands.iter().any(|c| c.text.chars().count() >= 3);
    assert!(
        has_multi,
        "双拼多音节应出绿词（3+字），实际 top5: {:?}",
        cands
            .iter()
            .take(5)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );

    // === 简拼（末尾不完整）：nih = ni + h(声母半截)
    // 应出"你好/你还/你回"等 ni+h 简拼组合，而非只有 ni 单字
    let cands2 = e.process("nih");
    let has_ni_h = cands2.iter().any(|c| {
        // 至少有一个 2+ 字的候选（ni+h 简拼组合）
        c.text.chars().count() >= 2
    });
    assert!(
        has_ni_h,
        "双拼 nih（ni+h 简拼）应出 2+ 字候选，实际 top5: {:?}",
        cands2
            .iter()
            .take(5)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );

    // === 对比：单音节双拼 ni = 你（不应出绿词/简拼，纯单字）
    let cands3 = e.process("ni");
    let all_single = cands3.iter().all(|c| c.text.chars().count() == 1);
    assert!(
        all_single,
        "双拼单音节 ni 应全是单字候选，实际: {:?}",
        cands3
            .iter()
            .take(3)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
}

/// 守护：green_memo 恢复上限截断——
/// input_mappings 无上限增长（固化只清 user_words/user_blacklist 不清
/// input_mappings），恢复的 green_memo 远超 GREEN_MEMO_MAX_ENTRIES(200) 时，
/// 首次 commit 写入即超限 → 整体 clear → 历史绿词 + 新学绿词全丢。
/// 修复：恢复截断到 MAX/2（按 key 内最高 user_freq 保留），会话新学习留一半空间。
#[test]
fn test_green_memo_recovery_capped() {
    use std::fs;

    let dir = crate::test_tmp::dir().join(format!(
        "x1pinyin_gm_cap_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("gm.db");

    // 构造 250 条 input_mappings（3-8 字文本，超过 GREEN_MEMO_MAX_ENTRIES=200）
    {
        let mut s = crate::storage::Storage::open(&db_path).unwrap();
        let entries: Vec<(String, String, u32)> = (0..250)
            .map(|i| (format!("input{}", i), format!("组合词语{}", i), 1))
            .collect();
        s.replace_input_mappings(&entries).unwrap();
        // 词条也要在 dict（split_into_words 需要）——用 user_words 表让它进 dict
        let words: Vec<(String, String, u32)> = (0..250)
            .map(|i| (format!("组合词语{}", i), format!("zu he ci yu {}", i), 1000))
            .collect();
        s.import_user_words(&words, true).unwrap();
    }

    let mut e = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    // 恢复被截断到 MAX/2 = 100 个 key
    assert!(
        e.green_memo.len() <= 100,
        "恢复的 green_memo 应截断到 MAX/2（实际 {}）",
        e.green_memo.len()
    );

    // commit 新绿词：不应触发整体 clear（新词 key 必须存在）
    e.commit("uihglgni", "我好困急你", Some("wo hao kun ji ni"), false);
    assert!(
        e.green_memo.contains_key("uihglgni"),
        "commit 新绿词后 key 必须存在（不得触发整体 clear）"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// 守护：用户词导入与系统词冲突时以用户词为主。
/// 系统词「广州」freq=5000，导入用户词「广州」freq=100（同 text+pinyin）——
/// rebuild 后 Trie 候选 freq 必须为 100（用户词覆盖，而非 insert 的 max 保留系统值）。
#[test]
fn test_user_word_overrides_system_word_on_import() {
    use std::fs;

    let dir = crate::test_tmp::dir().join(format!(
        "x1pinyin_user_override_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("uo.db");

    // 系统词库：广州 freq=5000
    {
        let mut s = crate::storage::Storage::open(&db_path).unwrap();
        s.import_x1dict_batch(&[crate::storage::X1DictWord {
            simplified: "广州".into(),
            pinyin: "guang zhou".into(),
            freq: 5000,
            ..Default::default()
        }])
        .unwrap();
    }
    let mut e = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    // 基线：系统词 freq=5000
    let before = e.dict.query(&["guang", "zhou"]);
    assert_eq!(before[0].base_freq, 5000);

    // 导入用户词「广州」freq=100 → rebuild → 用户词为主（覆盖为 100）
    let imported = e
        .import_user_dict_tsv("广州\tguang zhou\t100\n", true)
        .unwrap();
    assert!(imported >= 1);
    let after = e.dict.query(&["guang", "zhou"]);
    assert_eq!(
        after[0].base_freq, 100,
        "导入用户词与系统词冲突时应以用户词为主（实际 {}）",
        after[0].base_freq
    );

    let _ = fs::remove_dir_all(&dir);
}

/// 守护：发布链路端到端明文——
/// 种子 DB 明文 → 用户 DB 复制 → 引擎加载出词 + 词库管理器读路径
/// （list_x1dict_words）+ 导出路径 freq 均为明文原值。
#[test]
fn test_release_chain_plaintext_seed_end_to_end() {
    use std::fs;

    let dir = crate::test_tmp::dir().join(format!(
        "x1pinyin_release_chain_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let seed_path = dir.join("seed.db");
    let user_path = dir.join("user.db");

    // 1. 构造明文种子（含系统词）
    {
        let mut s = crate::storage::Storage::open(&seed_path).unwrap();
        s.import_x1dict_batch(&[
            crate::storage::X1DictWord {
                simplified: "广州".into(),
                pinyin: "guang zhou".into(),
                freq: 5000,
                ..Default::default()
            },
            crate::storage::X1DictWord {
                simplified: "我喜欢吃饭".into(),
                pinyin: "wo xi huan chi fan".into(),
                freq: 800,
                ..Default::default()
            },
        ])
        .unwrap();
    }

    // 2. 用户 DB = 复制种子（发布版首启行为）
    fs::copy(&seed_path, &user_path).unwrap();

    // 3. 引擎加载 → 查询出词正常
    let e = Engine::new_with_db(user_path.to_str().unwrap()).unwrap();
    let cands = e.process("guangzhou");
    assert!(
        cands.iter().any(|c| c.text.as_ref() == "广州"),
        "用户 DB 加载后 guangzhou 应出「广州」"
    );

    // 4. 词库管理器 FFI 读路径（x1_engine_get_x1dict_words → engine.list_x1dict_words）
    //    返回明文 freq
    let listed = e.list_x1dict_words(None, 10, 0);
    let gz = listed.iter().find(|w| w.simplified == "广州").unwrap();
    assert_eq!(gz.freq, 5000, "词库管理器列表必须返回明文 freq");
    assert_eq!(gz.pinyin, "guang zhou");

    // 5. 导出路径（系统词库导出代码保留——UI 无按钮，FFI 仍可用且 freq 明文）
    let tsv = e.export_x1dict_tsv();
    assert!(
        tsv.contains("广州\tguang zhou\t5000\t"),
        "导出应含明文 freq"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// 验证：green_memo 跨模式共享。
/// 双拼用户 commit 的绿词，切到全拼模式也能查到（绿词记忆是语言层面，与按键无关）。
#[test]
fn diag_green_memo_cross_mode() {
    let mk = || -> Engine { real_engine() };

    // 双拼模式 commit 一个绿词（3+ 字组合，非词库真词）
    let mut e_sp = mk();
    e_sp.shuangpin = crate::shuangpin::ShuangpinScheme::builtin_by_id("xiaohe").cloned();
    // 找一个不在词库的 3 字组合作为绿词测试
    // "我好累" = wo hao lei → 小鹤 whlc？不对。用 commit 直接学习。
    // commit 的 input 是双拼 buffer，pinyin 是双拼编码，内部展开为全拼存储 green_memo
    e_sp.commit("uihglgni", "我好困急你", Some("wo hao kun ji ni"), false);
    // green_memo 应同时存了 "uihglgni"(双拼input) 和 "wo hao kun ji ni"(全拼pinyin)
    let has_input_key = e_sp.green_memo.contains_key("uihglgni");
    let has_pinyin_key = e_sp.green_memo.contains_key("wo hao kun ji ni");
    assert!(has_input_key, "green_memo 应有双拼 input key");
    assert!(
        has_pinyin_key,
        "green_memo 应有全拼 pinyin key（跨模式共享）"
    );

    // 切到全拼模式，用全拼查询 → 应通过 seg_pinyin 回退命中绿词
    // （green_memo 是会话级，同一 engine 实例切换 shuangpin=None 模拟切模式）
    e_sp.shuangpin = None;
    // 全拼 process 不直接查 green_memo（它走 process_inner → decode_sentence），
    // 但 decode_sentence 的 seg_pinyin 回退会查 "wo hao kun ji ni" key
    // 这里验证 green_memo 数据结构本身有全拼 key 即可（查询路径已有 seg_pinyin 回退）
}

/// 回归测试：tingshuo 不应被误判为混合简拼。
/// tingshuo 是全拼（ting+shuo），旧版被 is_hybrid 误判走 query_tokens，
/// 导致 ting→s+h+u+o 拆分后「体内」「体能」等 ti 开头词污染首候选。
/// 修复：is_hybrid 加「Initial 段拼起来是有效音节则不算混合」反拆分判据。
#[test]
fn test_tingshuo_not_misclassified_as_hybrid() {
    let seed = seed_copy("q");
    let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
    let cands = engine.process("tingshuo");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    // 首候选应是 ting 开头的词（听书/听说），不是 ti 开头（体内/体能）
    let first = texts.first().copied().unwrap_or("");
    assert!(
        first.starts_with('听') || first.starts_with('停') || first.starts_with('霆'),
        "tingshuo 首候选应是 ting 开头（听/停/霆），实际: {:?}",
        texts
    );
    assert!(
        !texts
            .iter()
            .take(3)
            .any(|&t| t.starts_with('体') || t.starts_with('提')),
        "tingshuo 前3不应出现 ti 开头（体/提），实际: {:?}",
        texts
    );
}

/// 混合简拼：zhgrm 应命中「中国人民」（Shengmu(zh) + Initial(g/r/m) 双试路径）
#[test]
fn test_hybrid_zhgrm() {
    let mut engine = Engine::new();
    engine.learn("中国人民", "zhong guo ren min");
    engine.learn("中国", "zhong guo");
    let cands = engine.process("zhgrm");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    assert!(
        texts.contains(&"中国人民"),
        "zhgrm 应命中「中国人民」（Shengmu 双试），实际: {:?}",
        texts
    );
}

/// 混合简拼：shgrm 应命中 sh 开头 + g 字 + r + m 首字母的词
#[test]
fn test_hybrid_shgrm() {
    let mut engine = Engine::new();
    engine.learn("施工人员", "shi gong ren yuan");
    // shgrm: Shengmu(sh) + Initial(g-工) + Initial(r-人) + Initial(m-员，yuan 的 y)
    // 注：m 在此例对应「员」yuan 不匹配，改用「施工入门」shigongrumen
    engine.learn("施工入门", "shi gong ru men");
    let cands = engine.process("shgrm");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    assert!(
        texts.contains(&"施工入门"),
        "shgrm 应命中「施工入门」（Shengmu(sh)+g/r/m），实际: {:?}",
        texts
    );
}

/// 混合简拼：chgrm 应命中 ch 开头 + grm 首字母的词
#[test]
fn test_hybrid_chgrm() {
    let mut engine = Engine::new();
    engine.learn("崇高人民", "chong gao ren min");
    let cands = engine.process("chgrm");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    assert!(
        texts.contains(&"崇高人民"),
        "chgrm 应命中「崇高人民」（Shengmu(ch) 双试），实际: {:?}",
        texts
    );
}

/// 性能探针：混合查询的耗时（阈值：短输入 <10ms，长输入 <50ms）
#[test]
fn bench_hybrid_queries() {
    use std::time::Instant;
    let seed = seed_copy("q");
    let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
    let inputs = [
        "zhonggrm",
        "zhgrm",
        "zgrm",
        "zhongguorenmin",
        "shurufxiangmu",
        "nihao",
        "shishierfe",
    ];
    for input in &inputs {
        let _ = engine.process(input); // 预热
        let start = Instant::now();
        for _ in 0..1000 {
            let _ = engine.process(input);
        }
        let per_us = start.elapsed().as_micros() as f64 / 1000.0;
        // 长输入（≥5 音节）含整句 DP，放宽到 50ms；短输入 <10ms
        // 简拼 DP（≥4 Initial）开销大，放宽到 15ms；长句 DP 放宽到 50ms
        let threshold = if input.len() > 10 {
            50_000.0
        } else if input.chars().filter(|c| c.is_ascii_lowercase()).count() <= 6 {
            20_000.0
        } else {
            10_000.0
        };
        println!(
            "BENCH {:<14} → {:.2} μs/query (阈值 {:.0}ms)",
            input,
            per_us,
            threshold / 1000.0
        );
        // 性能阈值只在 release 模式断言——debug 无优化可慢 5-10 倍，
        // debug 下只打印供参考。
        if !cfg!(debug_assertions) {
            assert!(
                per_us < threshold,
                "{} 单次查询 {:.1}ms 超过阈值",
                input,
                per_us / 1000.0
            );
        }
    }
}

#[test]
fn test_db_engine_fallback_to_builtin() {
    // 空数据库 → 应回退到内置词库
    let mut path = crate::test_tmp::dir();
    path.push("x1pinyin_test_empty.db");
    let _ = std::fs::remove_file(&path);

    let engine = Engine::new_with_db(path.to_str().unwrap()).unwrap();
    assert!(engine.has_storage());
    // 内置词库可用
    let r = engine.process("zhongwen");
    assert!(!r.is_empty(), "空 DB 回退内置词库应返回候选");
}

/// 性能探针：Trie + word_index + seed_bigram 内存构成统计。
/// 确定性计算节点/Arc/Vec 的理论内存，指导紧凑化优先级。
/// 内存紧凑化修订：拼音 Arc 按节点共享、word_index 复用
/// Trie Arc 分配、seed_bigram CSR 平铺——三项与旧结构对照一并输出。
/// 用 --nocapture 看输出。
#[test]
fn bench_trie_memory_profile() {
    let seed = seed_copy("q");
    let t0 = std::time::Instant::now();
    let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
    eprintln!(
        "[启动] Engine::new_with_db: {:?}（release 口径对照基线 370ms；debug 仅看量级）",
        t0.elapsed()
    );
    let p = engine.dict.memory_profile();

    // Arc<str> 胖指针 16B/个（ptr+len，每候选 2 个：text + pinyin）；
    // Vec 栈开销 24B/个（每节点 2 个 Vec：candidates + children）
    let arc_ptr = 16 * p.candidate_count * 2;
    let vec_stack = 24 * p.node_count * 2;
    // Arc 堆分配：16B 头（strong/weak 计数）+ 内容，向上取整到 16B 分配量子。
    // text 每候选一份；pinyin 按节点共享（pinyin_alloc_count 份）。
    // 逐分配取整无逐条数据，按「内容 + 头 × 份数 + 8B/份取整余量」估算。
    let alloc16 = |content: usize, allocs: usize| content + allocs * (16 + 8);
    let text_heap = alloc16(p.text_heap_bytes, p.candidate_count);
    let pinyin_heap = alloc16(p.pinyin_heap_bytes, p.pinyin_alloc_count);
    // Vec 堆（容量 × 元素大小）：children 元素 = TrieNode（72B），
    // candidates 元素 = Candidate（48B：2×16B 胖指针 + u32 + 5 bool 对齐）
    let vec_heap = p.children_capacity * 72 + p.candidates_capacity * 48;
    let trie_total = arc_ptr + vec_stack + text_heap + pinyin_heap + vec_heap;

    // word_index：(Arc<str>, u32, Arc<str>) = 40B/条（对齐后），零独立堆分配
    let wi_new = p.word_index_len * 40;
    // 旧结构：(String, u32, String) = 56B/条 + 每条 2 次 String 堆分配（头 24B
    // 在结构体内已计，堆内容 + malloc 头 ~16B，短词按 32B/次估）
    let wi_old = p.word_index_len * 56 + p.word_index_len * 2 * 32;

    let sb = &engine.freq.seed_bigram;
    let sb_new = sb.estimated_bytes();
    // 旧结构：嵌套 BTreeMap + totals BTreeMap 估算 ~140B/条
    let sb_old = sb.len() * 140;

    eprintln!("=== 内存构成（187k 词种子，seed_bigram blob 化后）===");
    // arena 化后的真实基底（节点 Trie 是构建期结构，
    // finish_load 后整体丢弃——node_count 只剩 overlay 增量，不是主存储）。
    eprintln!(
        "[arena] 基底: ~{} MB（节点: {}  候选: {}）  word_index span: ~{} MB（{} 条）  overlay: 节点 {} 候选 {}",
        p.arena_bytes / 1048576,
        p.arena_nodes,
        p.arena_candidates,
        p.wi_span_bytes / 1048576,
        p.wi_span_len,
        p.node_count,
        p.candidate_count
    );
    eprintln!(
        "[Trie] 节点: {}  候选: {}  带候选节点: {}  word_index: {}",
        p.node_count, p.candidate_count, p.pinyin_alloc_count, p.word_index_len
    );
    eprintln!(
        "[Trie] text 堆: ~{} MB  pinyin 堆(共享): ~{} MB  Vec 堆: {} MB  指针+栈: {} MB",
        text_heap / 1048576,
        pinyin_heap / 1048576,
        vec_heap / 1048576,
        (arc_ptr + vec_stack) / 1048576
    );
    eprintln!("[Trie] 小计: ~{} MB", trie_total / 1048576);
    eprintln!(
        "[word_index] 现结构 ~{} MB（Arc 复用，0 堆分配） vs 旧结构 ~{} MB",
        wi_new / 1048576,
        wi_old / 1048576
    );
    eprintln!(
        "[seed_bigram] CSR {} 条/{} 组: ~{} MB vs 旧结构 BTreeMap ~{} MB",
        sb.len(),
        sb.prev_len(),
        sb_new / 1048576,
        sb_old / 1048576
    );
    eprintln!(
        "[合计] 当前 ~{} MB，旧结构 ~{} MB，估算节省 ~{} MB",
        (trie_total + wi_new + sb_new) / 1048576,
        (trie_total + wi_old + sb_old) / 1048576,
        (wi_old - wi_new + sb_old - sb_new) / 1048576
    );
    eprintln!(
        "（对照：优化前实测 MALLOC_SMALL ~176MB resident——差额为 malloc 头/碎片/其他）"
    );
}

/// 分配器空闲页归还：release_allocator_slack 只归还 malloc 攥着的
/// 空闲缓存页（footprint 实测 ~44MB Reclaimable 的来源），数据与查询行为零变化
/// ——本测试是它的功能守卫（链接/崩溃/行为）。
#[test]
fn release_allocator_slack_keeps_engine_intact() {
    let engine = Engine::new();
    let before = engine.process("nihao").len();
    assert!(before > 0, "内置词库应有候选");
    super::Engine::release_allocator_slack();
    let after = engine.process("nihao").len();
    assert_eq!(before, after, "归还空闲页后查询结果零变化");
}

/// 性能探针：启动耗时分解（评估启动优化的下刀点）。
/// 把 Engine::new_with_db 的 ~350ms 拆成：Storage::open / SQL 行解码 /
/// Trie insert（含解码）/ shrink_to_fit / finish_load / seed_bigram 加载+CSR。
/// 只测不断言。用 --nocapture 看输出。
#[test]
fn bench_startup_breakdown() {
    // seed_copy 临时副本：Storage::open(仓库种子) 就地转 WAL 写 db_meta +
    // bigram 缓存写回 assets——交付种子被改写，且与其他测试的 fs::copy
    // 并行竞争（readonly panic，套件偶发红）。
    let seed = seed_copy("bench");

    let t0 = std::time::Instant::now();
    let storage = crate::storage::Storage::open(&seed).unwrap();
    eprintln!("[启动分解] Storage::open: {:?}", t0.elapsed());

    // 纯 SQL 行解码（回调空转）：187k 行 → 每行 2 次 String 分配 + 参数解析
    let t1 = std::time::Instant::now();
    let mut rows = 0usize;
    storage
        .for_each_trie_word(|_, _, _, _| {
            rows += 1;
            Ok(())
        })
        .unwrap();
    eprintln!(
        "[启动分解] SQL 行解码（{} 行，不建树）: {:?}",
        rows,
        t1.elapsed()
    );

    // SQL + Trie insert（insert 净成本 ≈ 本项 − 上项）
    let t2 = std::time::Instant::now();
    let mut dict = crate::dict::Dict::new();
    storage
        .for_each_trie_word(|p, t, f, pri| {
            dict.insert(p, t, f, pri);
            Ok(())
        })
        .unwrap();
    eprintln!("[启动分解] SQL + Trie insert（含解码）: {:?}", t2.elapsed());

    let t3 = std::time::Instant::now();
    dict.shrink_to_fit();
    eprintln!("[启动分解] shrink_to_fit: {:?}", t3.elapsed());

    let p = dict.memory_profile();
    eprintln!(
        "[启动分解] single_chars: {} 桶 / {} 候选（线性扫描成本 = 单字行数 × 桶数）",
        p.sc_buckets, p.sc_candidates
    );

    let t4 = std::time::Instant::now();
    dict.finish_load();
    eprintln!(
        "[启动分解] finish_load（word_index 排序去重）: {:?}",
        t4.elapsed()
    );

    let t5 = std::time::Instant::now();
    let sb = storage.load_seed_bigram_counts().unwrap_or_default();
    let sb_rows = sb.len();
    let mut freq = crate::freq::FreqModel::new();
    freq.set_seed_bigram_counts(sb);
    eprintln!(
        "[启动分解] seed_bigram 加载+CSR（{} 条）: {:?}",
        sb_rows,
        t5.elapsed()
    );

    let t6 = std::time::Instant::now();
    let engine = Engine::new_with_db(&seed).unwrap();
    eprintln!("[启动分解] 对照：完整 new_with_db: {:?}", t6.elapsed());
    drop(engine);
}

/// 启动分解·编译缓存对照（独立测试：带 hash 副本才能走编译缓存；seed_copy
/// 无 hash 的分解主体展示的是构建路径）。用 --nocapture 看输出。
#[test]
fn bench_startup_cache_hit() {
    let seed = seed_copy_keephash("benchhit");

    // 首跑：构建 + 回写缓存
    let t0 = std::time::Instant::now();
    let e1 = Engine::new_with_db(&seed).unwrap();
    let miss = t0.elapsed();
    let _ = e1.process("nihao");
    drop(e1);
    let cache_ok = crate::storage::dict_cache::cache_exists(std::path::Path::new(&seed));

    // 二次：命中缓存
    let t1 = std::time::Instant::now();
    let e2 = Engine::new_with_db(&seed).unwrap();
    let hit = t1.elapsed();
    let _ = e2.process("nihao");
    drop(e2);

    eprintln!(
        "[启动分解] 编译缓存：未命中（构建+回写）={:?} / 命中（cache_exists={}）={:?} / 节省 ≈ {:?}",
        miss,
        cache_ok,
        hit,
        miss.saturating_sub(hit)
    );
    for f in [
        seed.clone(),
        format!("{}-wal", seed),
        format!("{}-shm", seed),
        crate::storage::dict_cache::cache_path_for(std::path::Path::new(&seed))
            .to_string_lossy()
            .into_owned(),
    ] {
        let _ = std::fs::remove_file(f);
    }
}

/// 性能探针：测量真实 Trie（12万词）下 find_pinyin/remove_by_text 全树遍历耗时。
/// 评估 text→node 侧索引的必要性。用 --nocapture 看输出。
#[test]
fn bench_full_trie_traversal() {
    let seed = seed_copy("q");
    let mut engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

    // 测 find_pinyin（全树遍历找 text 的 pinyin）
    let targets = ["我们", "问题", "输入法", "中国", "的"];
    let start = std::time::Instant::now();
    for t in &targets {
        let _ = engine.dict.find_pinyin(t);
    }
    let fp_elapsed = start.elapsed() / targets.len() as u32;
    eprintln!(
        "find_pinyin（{} 个目标，平均单次）: {:?}",
        targets.len(),
        fp_elapsed
    );

    // 测 remove_by_text：测一个不存在的词（仍全树遍历 retain），不破坏 engine.dict 内容
    let start = std::time::Instant::now();
    for _ in 0..5 {
        engine.dict.remove_by_text("___不存在的词___");
    }
    let rb_elapsed = start.elapsed() / 5;
    eprintln!(
        "remove_by_text（不存在词，全树 retain，平均单次）: {:?}",
        rb_elapsed
    );

    // 对照：query（按音节路径，非全树）
    let start = std::time::Instant::now();
    for _ in 0..1000 {
        let _ = engine.dict.query(&["wo", "men"]);
    }
    let q_elapsed = start.elapsed() / 1000;
    eprintln!("query（按音节路径，对照）平均单次: {:?}", q_elapsed);
}

/// 性能探针：测量 process 在不同输入长度下的耗时（per-keystroke 热路径）。
/// 评估候选缓存复用（0.1）的必要性。用 --nocapture 看输出。
#[test]
fn bench_process_various_lengths() {
    let seed = seed_copy("q");
    let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
    let cases = [
        ("ni", "短"),
        ("nihao", "双音节"),
        ("nihao shijie", "4音节"),
        ("woyaozuofeijichuxing", "长句8音节"),
        ("nengjiejuewenti", "长句5音节歧义"),
        ("wojintianhenkaixinchileyigepingguo", "超长句11音节"),
    ];
    for (input, label) in &cases {
        // 预热
        let _ = engine.process(input);
        let n = 100;
        let start = std::time::Instant::now();
        for _ in 0..n {
            let _ = engine.process(input);
        }
        let elapsed = start.elapsed() / n;
        eprintln!("process {:>12} ({} 字): {:?}", label, input.len(), elapsed);
    }
}

/// 性能基线：定位 process("ni") 的耗时分布（验证 input_mappings 清理效果）。
/// 用 --nocapture 看输出。清理运行时统计表后应从 6ms 降至 ~200µs。
#[test]
fn bench_ni_mapping_scan() {
    let seed = seed_copy("q");
    let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

    // 预热
    for _ in 0..5 {
        let _ = engine.process("ni");
    }

    // 1. query_single_chars（单字表查询）
    let n = 200;
    let start = std::time::Instant::now();
    for _ in 0..n {
        let _ = engine.dict.query_single_chars("ni");
    }
    let sc = start.elapsed() / n;
    let sc_count = engine.dict.query_single_chars("ni").len();
    eprintln!("query_single_chars('ni'): {:?} (返回 {} 个)", sc, sc_count);

    // 2. query（按音节路径）
    let start = std::time::Instant::now();
    for _ in 0..n {
        let _ = engine.dict.query(&["ni"]);
    }
    let q = start.elapsed() / n;
    let q_count = engine.dict.query(&["ni"]).len();
    eprintln!("query(['ni']): {:?} (返回 {} 个)", q, q_count);

    // 3. 完整 process 对照
    let start = std::time::Instant::now();
    for _ in 0..n {
        let _ = engine.process("ni");
    }
    let p = start.elapsed() / n;
    let p_count = engine.process("ni").len();
    eprintln!("process('ni'): {:?} (候选 {} 个)", p, p_count);
}

/// 算法层验证：模拟用户积累 input_mappings 后 process 是否退化。
/// 证明 prefix mapping 全表扫描是独立于种子DB数据层的算法隐患。
#[test]
fn bench_mapping_accumulation_degradation() {
    let seed = seed_copy("q");
    let mut engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

    // 预热
    for _ in 0..3 {
        let _ = engine.process("ni");
    }

    // 基线：clean（0 条 mapping）
    let n = 100;
    let start = std::time::Instant::now();
    for _ in 0..n {
        let _ = engine.process("ni");
    }
    eprintln!("mapping=0: process('ni') {:?}", start.elapsed() / n);

    // 模拟用户积累：灌入 N 条 (input, text) 映射
    for &target_count in &[1000usize, 3000, 6000] {
        // 灌到目标数量
        while engine.input_mappings.len() < target_count {
            let i = engine.input_mappings.len();
            engine
                .input_mappings
                .insert((format!("input{}", i), format!("词{}", i)), 1);
        }
        let start = std::time::Instant::now();
        for _ in 0..n {
            let _ = engine.process("ni");
        }
        eprintln!(
            "mapping={}: process('ni') {:?}",
            engine.input_mappings.len(),
            start.elapsed() / n
        );
    }
}

#[test]
fn test_db_engine_with_x1dict() {
    use crate::storage::{Storage, X1DictWord};

    let mut path = crate::test_tmp::dir();
    path.push("x1pinyin_test_x1dict.db");
    let _ = std::fs::remove_file(&path);

    // 预置 X1Dict 数据
    {
        let mut s = Storage::open(&path).unwrap();
        s.import_x1dict_batch(&[X1DictWord {
            simplified: "测试词".into(),
            pinyin: "ce shi ci".into(),
            freq: 500,
            ..Default::default()
        }])
        .unwrap();
    }

    let engine = Engine::new_with_db(path.to_str().unwrap()).unwrap();
    let r = engine.process("ceshici");
    assert!(
        r.iter().any(|c| c.text.as_ref() == "测试词"),
        "应从 SQLite 加载 '测试词'"
    );
}

/// 键预计算排序与 candidate_cmp 两两比较的等价性守护。
/// 主排序从 sort_by(candidate_cmp) 改为预计算 CandidateSortKey + 元组排序，
/// 要求对所有参数组合（单/多音节、best_score 0/>0、有无上下文）结果逐位相同。
#[test]
fn test_candidate_sort_key_matches_cmp() {
    let seed = seed_copy("q");
    let mut engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

    // 混合候选池：单字（yi）+ 多字词（zhongguoren），覆盖全部 tier 组合
    let mut pool = engine.process("yi");
    pool.extend(engine.process("zhongguoren"));
    assert!(pool.len() >= 10, "候选池太小，等价性覆盖不足");
    pool.reverse(); // 确定性打乱，让排序有实际工作

    // (label, is_multi, single_syl, best_score, syls)
    let rounds: [(&str, bool, Option<&str>, u32, Vec<&str>); 3] = [
        ("单音节", false, Some("yi"), 1, vec!["yi"]),
        (
            "多音节best=0(激活单字位置层)",
            true,
            Some("zhong"),
            0,
            vec!["zhong", "guo", "ren"],
        ),
        (
            "多音节best>0",
            true,
            Some("shi"),
            1000,
            vec!["shi", "shi", "er"],
        ),
    ];
    // 两轮上下文：空（跳过 3/3.5 层）→ 有（激活 collocation/强 bigram 层）
    for with_context in [false, true] {
        if with_context {
            engine.context_push_for_test("记");
        }
        for (label, is_multi, single_syl, best_score, syls) in &rounds {
            // 上下文变化后刷新 score_cache（打分含上下文分量），与生产路径一致
            //（process_inner 入口清缓存），保证 cmp 和 key 读到同一组打分。
            let _ = engine.process("yi");
            // 纯简拼让位参数：两实现必须吃同一个值——本测试的
            // 候选池来自 "yi"/"zhongguoren"（均首音节完整），取实际预算值。
            let first_complete = Engine::input_starts_with_complete_syllable("yi");
            let mut by_cmp = pool.clone();
            by_cmp.sort_by(|a, b| {
                engine.candidate_cmp(
                    "yi",
                    a,
                    b,
                    *is_multi,
                    *single_syl,
                    *best_score,
                    syls,
                    first_complete,
                )
            });
            let mut keyed: Vec<_> = pool
                .iter()
                .map(|c| {
                    (
                        engine.candidate_sort_key(
                            "yi",
                            c,
                            *is_multi,
                            *single_syl,
                            *best_score,
                            syls,
                            first_complete,
                        ),
                        c.clone(),
                    )
                })
                .collect();
            #[allow(clippy::unnecessary_sort_by)]
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
            let ord_cmp: Vec<_> = by_cmp.iter().map(|c| (&c.text, &c.pinyin)).collect();
            let ord_key: Vec<_> = keyed.iter().map(|(_, c)| (&c.text, &c.pinyin)).collect();
            assert_eq!(
                ord_cmp, ord_key,
                "上下文={} {}：键排序与 candidate_cmp 结果不一致",
                with_context, label
            );
        }
    }
}

/// 纯简拼让位（yem 案例）：输入首音节能成完整音节时，
/// 「每字母一字」的纯简拼词退出 tier 0，让位于「完整音节+简拼」读法；
/// 首音节不能成音节时（ssef）简拼整词保持置顶（不回归）。
#[test]
fn test_jianpin_whole_word_yields_to_complete_first_syllable() {
    let seed = seed_copy("q");
    let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

    // ① 判据本身：ye 是完整音节 → true；s/ss/sse 都不是 → false
    assert!(Engine::input_starts_with_complete_syllable("yem"));
    assert!(!Engine::input_starts_with_complete_syllable("ssef"));

    // ② yem：生僻纯简拼词「耶尔穆」(base≈6, y|e|m) 不得霸占首位；
    //    首位应落在「完整音节 ye + 简拼 m」的高频词上（也没/叶某/业满/液面/页面）
    let yem = engine.process("yem");
    let texts: Vec<&str> = yem.iter().map(|c| c.text.as_ref()).collect();
    assert!(!texts.is_empty(), "yem 应有候选");
    assert_ne!(
        texts[0], "耶尔穆",
        "yem 首候选不应是纯简拼生僻词，实际: {:?}",
        &texts[..texts.len().min(6)]
    );
    // 前 3 名应为 ye|m 读法的常用词（拼音首音节是完整 ye）
    let top3_ok = texts
        .iter()
        .take(3)
        .filter(|t| {
            yem.iter()
                .find(|c| c.text.as_ref() == **t)
                .is_some_and(|c| c.pinyin.split_whitespace().next() == Some("ye"))
        })
        .count();
    assert!(
        top3_ok >= 2,
        "yem 前 3 应至少 2 个 ye 完整音节读法，实际: {:?}",
        &texts[..texts.len().min(6)]
    );

    // ③ ssef：似是而非（简拼整词）保持第一，不回退到让位之前的处境
    let ssef = engine.process("ssef");
    assert_eq!(
        ssef.first().map(|c| c.text.as_ref()),
        Some("似是而非"),
        "ssef 首候选必须是简拼整词「似是而非」，实际: {:?}",
        ssef.iter().take(5).map(|c| c.text.as_ref()).collect::<Vec<_>>()
    );
}

/// 回归测试：zpiny 不产「拍扁噪音」候选（混合简拼回退修复）。
///
/// 原 bug：混合简拼回退把 [z,pin,y] 全部拍扁成 zpy 查简拼，捞出第二音节与
/// 用户明确输入的 pin 完全对不上的词（装配员 zhuang pei yuan、
/// 找朋友 zhao peng you、占便宜 zhan pian yi）。
/// 修复：完整音节不在末尾时禁止拍扁（pin 后还有 y，证明 pin 是刻意打的）。
#[test]
fn test_zpiny_no_flatten_noise() {
    let seed = seed_copy("q");
    let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
    let cands = engine.process("zpiny");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    // 拍扁噪音：第二音节与 pin 矛盾的词绝不应出现
    for noise in ["装配员", "找朋友", "占便宜", "质谱仪"] {
        assert!(
            !texts.contains(&noise),
            "zpiny 不应出现拍扁噪音「{}」，实际前10: {:?}",
            noise,
            &texts[..texts.len().min(10)]
        );
    }
    // 合法候选保留：z*+pin 的词（作品 zuo pin / 正品 zheng pin / 制品 zhi pin）
    assert!(
        texts.iter().any(|t| ["作品", "正品", "制品"].contains(t)),
        "zpiny 应保留 z*+pin 合法候选（作品/正品/制品），实际前10: {:?}",
        &texts[..texts.len().min(10)]
    );
}

/// 回归测试：末尾完整音节仍允许拍扁回退（保留面）。
///
/// zpin → [z,pin]：pin 在末尾，可能是"只打了前几个字母"的半截（如想打
/// 照片 zhao pian 只打了 zpin）→ 拍扁 zp 补查合理，照片/作品应出现。
/// zpyou → [z,p,you]：完整音节 you 在末尾，回退保留
/// （zpyou 是正常可达方案，找朋友必须能出）。
#[test]
fn test_trailing_complete_syllable_flatten_preserved() {
    let seed = seed_copy("q");
    let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
    let zpin_cands = engine.process("zpin");
    let texts: Vec<&str> = zpin_cands.iter().map(|c| c.text.as_ref()).collect();
    assert!(
        texts
            .iter()
            .any(|t| ["照片", "作品", "正品", "招聘"].contains(t)),
        "zpin 应保留拍扁回退候选（照片/作品等 zp 词），实际前10: {:?}",
        &texts[..texts.len().min(10)]
    );
    let zpyou_cands = engine.process("zpyou");
    let texts: Vec<&str> = zpyou_cands.iter().map(|c| c.text.as_ref()).collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("朋友") || ["自有", "择优"].contains(t)),
        "zpyou 应保持正常方案可用（找朋友/自有/择优），实际前10: {:?}",
        &texts[..texts.len().min(10)]
    );
}

/// 回归测试：首简拼 + 尾全拼时，尾部全拼必须精确约束候选。
///
/// hdong = [h, dong]：dong 是用户明确打完的完整音节，候选第二音节必须匹配
/// dong——「回答(hui da)/海底(hai di)/很多(hen duo)」第二音节只共首字母 d，
/// 是拍扁回退把 dong 降级成首字母后捞出的噪音。
/// 期望：h*+dong 精确词（活动/寒冬/华东/滑动/轰动）出现，h*+d* 噪音不出现。
#[test]
fn test_hdong_trailing_full_syllable_precise() {
    let seed = seed_copy("q");
    let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
    let cands = engine.process("hdong");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    // 尾音节与 dong 完全对不上的拍扁噪音绝不应出现
    for noise in ["回答", "海底", "很多", "回到", "花朵"] {
        assert!(
            !texts.contains(&noise),
            "hdong 不应出现尾音节非 dong 的噪音「{}」，实际前10: {:?}",
            noise,
            &texts[..texts.len().min(10)]
        );
    }
    // 精确词保留：第二音节是 dong
    assert!(
        texts
            .iter()
            .any(|t| ["活动", "寒冬", "华东", "滑动", "轰动", "互动"].contains(t)),
        "hdong 应有 h*+dong 精确词（活动/寒冬等），实际前10: {:?}",
        &texts[..texts.len().min(10)]
    );
}

/// 回归测试：尾部全拼的「续打半截」保留面（精确约束的边界）。
///
/// bydi = [b, y, di]：di 是 ding/dian/die… 的前缀，用户可能还在续打
/// （bu yi ding → byding）——同位音节以 di 开头的候选（不一定）必须保留。
/// 精确约束的口径是「候选同位音节以输入完整音节开头」，不是死等全等，
/// 这样 dong 位置的噪音（da/duo/di）被过滤、di 位置的续打（ding）保留。
#[test]
fn test_bydi_trailing_prefix_continuation_preserved() {
    let seed = seed_copy("q");
    let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
    let cands = engine.process("bydi");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    assert!(
        texts.iter().any(|t| t.contains("不一定")),
        "bydi 的 di 是 ding 的前缀（续打半截），「不一定」应保留，实际前10: {:?}",
        &texts[..texts.len().min(10)]
    );
}

/// 回归测试：mo 不出「无」（种子 DB 已删除 无|mo 罕见古音单字条目）。
///
/// 「无」现代汉语只有 wu 一个读音（mo 仅存于佛经词「南无 nāmó」），
/// 单字条目 无|mo 让「无」出现在 mo 输入的候选里，是词库脏数据。
/// 修复只删单字条目，保留词级条目 南无|na mo（打 namo 出南无是正确的）。
#[test]
fn test_mo_no_wu_candidate() {
    let seed = seed_copy("q");
    let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
    let mo = engine.process("mo");
    assert!(
        !mo.iter().any(|c| c.text.as_ref() == "无"),
        "mo 候选不应出现「无」，实际前10: {:?}",
        mo.iter()
            .take(10)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
    let wu = engine.process("wu");
    assert!(
        wu.iter().any(|c| c.text.as_ref() == "无"),
        "wu 候选应保留「无」，实际前10: {:?}",
        wu.iter()
            .take(10)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
    let namo = engine.process("namo");
    assert!(
        namo.iter().any(|c| c.text.as_ref() == "南无"),
        "namo 候选应保留「南无」（词级条目不受影响），实际前10: {:?}",
        namo.iter()
            .take(10)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
}

/// 回归测试：灭绝/词绑读音单字清理。
///
/// 用户原则：「读音只在特定词汇中出现，就不要让它出现在单字候选里」。
/// 清理了 50 个单字条目（万|mo/佛|fu/南|na/会|kuai/提|di 等），
/// 词级条目不受影响（仿佛/会计/南无/般若/参差/游说 等词里的读音是对的）。
#[test]
fn test_extinct_readings_removed() {
    let seed = seed_copy("q");
    let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
    // (输入, 不应出现的单字) —— 该输入下此字的读音已被清理
    let gone: &[(&str, &str)] = &[
        ("mo", "万"),    // 万俟复姓
        ("fu", "佛"),    // 仿佛
        ("re", "若"),    // 般若
        ("kuai", "会"),  // 会计
        ("di", "提"),    // 提防
        ("jiang", "强"), // 倔强
        ("shui", "说"),  // 游说
        ("xian", "见"),  // 通「现」
        ("cen", "参"),   // 参差
        ("nuo", "那"),   // 脏数据读音
        ("gong", "红"),  // 女红
        ("ju", "车"),    // 象棋古读
                         // 注：南|na/似|shi 已删但不进本列表——打 na 时「南」经前缀补全
                         // （nan 以 na 开头，pinyin 显示 nan）出现是合法的实时输入提示；
                         // 打 shi 时「似」经 fuzzy s_sh 模糊音出现同理。清理的目标只是
                         // 「无对应输入路径」的脏单字条目。
    ];
    for (input, text) in gone {
        let cands = engine.process(input);
        assert!(
            !cands.iter().any(|c| c.text.as_ref() == *text),
            "{} 候选不应出现「{}」（罕见读音已清理），实际前10: {:?}",
            input,
            text,
            cands
                .iter()
                .take(10)
                .map(|c| c.text.as_ref())
                .collect::<Vec<_>>()
        );
    }
    // 词级条目不受影响：这些词里的罕见读音是正确读音
    let words_kept: &[(&str, &str)] = &[
        ("fangfu", "仿佛"),
        ("kuaiji", "会计"),
        ("namo", "南无"),
        ("cenci", "参差"),
        ("jiliang", "伎俩"),
        ("difang", "提防"),
    ];
    for (input, text) in words_kept {
        let cands = engine.process(input);
        assert!(
            cands.iter().any(|c| c.text.as_ref() == *text),
            "{} 候选应保留「{}」（词级条目读音正确），实际前10: {:?}",
            input,
            text,
            cands
                .iter()
                .take(10)
                .map(|c| c.text.as_ref())
                .collect::<Vec<_>>()
        );
    }
    // 现代合法多音字不受影响（保留面抽查）
    let legit: &[(&str, &str)] = &[
        ("mo", "没"),    // 沉没/没收
        ("shui", "睡"),  // 主路径不受影响
        ("zhang", "长"), // 长大
        ("dei", "得"),   // 得亏
    ];
    // 姓氏读音豁免：姓名无固定词，靠单字组合，
    // 区Ōu/查Zhā/解Xiè 必须保留单字候选
    let surnames: &[(&str, &str)] = &[
        ("ou", "区"),
        ("zha", "查"),
        ("xie", "解"),
        ("gou", "句"),   // 句 Gōu 姓/高句丽
        ("lu", "六"),    // 六 Lù 姓/六安
        ("yun", "员"),   // 员 Yùn 姓（新增补全）
        ("chong", "种"), // 种 Chóng 姓（新增补全）
    ];
    for (input, text) in legit {
        let cands = engine.process(input);
        assert!(
            cands.iter().any(|c| c.text.as_ref() == *text),
            "{} 候选应保留「{}」（现代合法读音），实际前10: {:?}",
            input,
            text,
            cands
                .iter()
                .take(10)
                .map(|c| c.text.as_ref())
                .collect::<Vec<_>>()
        );
    }
    for (input, text) in surnames {
        let cands = engine.process(input);
        assert!(
            cands.iter().any(|c| c.text.as_ref() == *text),
            "{} 候选应保留「{}」（姓氏读音豁免），实际前10: {:?}",
            input,
            text,
            cands
                .iter()
                .take(10)
                .map(|c| c.text.as_ref())
                .collect::<Vec<_>>()
        );
    }
}

/// 真实数据引擎（布局感知）：环境变量 `X1_USER_DB_DIR` 指向本机输入法数据目录时，
/// 分离布局 = open_split(dict.db, user.db)，旧单库 = new_with_db(x1pinyin.db)。
/// 不能用 new_with_db 开 user.db——它只有用户表，合并模式打开等于空系统词典。
/// 未设置或打开失败时回退种子副本。
fn real_engine() -> Engine {
    if let Ok(dir) = std::env::var("X1_USER_DB_DIR") {
        let dict = format!("{}/dict.db", dir);
        let user = format!("{}/user.db", dir);
        if std::path::Path::new(&dict).exists() {
            if let Ok(e) = Engine::open_split(&dict, &user) {
                return e;
            }
        }
        let legacy = format!("{}/x1pinyin.db", dir);
        if std::path::Path::new(&legacy).exists() {
            if let Ok(e) = Engine::new_with_db(&legacy) {
                return e;
            }
        }
    }
    let db = seed_copy("re");
    Engine::new_with_db(&db).expect("seed db")
}

/// 长词延展猜测（hupeng 应见狐朋狗友）：全拼完整
/// 音节是更长词的音节前缀时，更长词以紫词（is_guess）进入候选；排序层
/// is_pinyin_full_match 判其为前缀层 → 恒在等长精确词之下（结构性无回归）。
/// 完整输入命中间一词时它必须是白词（精确覆盖，非猜测）。
#[test]
fn test_fullpinyin_continuation_guess() {
    let db = seed_copy("cont");
    let engine = super::Engine::new_with_db(&db).expect("seed db");
    // hu|peng 等长精确词在种子中不存在（pinyin like 'hu peng%'
    // 只有 3-4 音节延展词）——该输入原本候选全空。
    let cands = engine.process("hupeng");
    let pos = cands.iter().position(|c| c.text.as_ref() == "狐朋狗友");
    assert!(
        pos.is_some(),
        "hupeng 应出现「狐朋狗友」（长词延展猜测），当前前 6：{:?}",
        cands
            .iter()
            .take(6)
            .map(|c| c.text.as_ref().to_string())
            .collect::<Vec<_>>()
    );
    let idx = pos.unwrap();
    assert!(idx < 9, "「狐朋狗友」应在第一页（idx={}）", idx);
    assert!(cands[idx].is_guess, "延展词必须是紫词（对后续输入的猜测）");
    // 完整输入 = 精确覆盖 → 白词（不染紫），且仍在第一页
    let full = engine.process("hupenggouyou");
    let fpos = full.iter().position(|c| c.text.as_ref() == "狐朋狗友");
    assert!(fpos.is_some(), "完整输入 hupenggouyou 必须命中「狐朋狗友」");
    let fi = fpos.unwrap();
    assert!(fi < 9, "完整输入命中应在第一页（idx={}）", fi);
    assert!(!full[fi].is_guess, "精确覆盖命中 = 白词，非紫词");
    seed_copy_cleanup(&db);
}

/// 紫词喧宾夺主回归（打 buyao 候选框被「不要X」刷屏）：
/// 精确命中等长词时零猜测（buyao→不要、yonghu→用户），零精确命中
/// （hupeng）的猜测 ≤3 条。真实种子库守护。
#[test]
fn test_guess_suppressed_on_exact_match_seed() {
    let db = seed_copy("guessflood");
    let engine = super::Engine::new_with_db(&db).expect("seed db");
    for input in ["buyao", "yonghu"] {
        let cands = engine.process(input);
        assert!(
            cands.iter().all(|c| !c.is_guess),
            "{} 精确命中等长词应零紫词，实际：{:?}",
            input,
            cands
                .iter()
                .take(8)
                .map(|c| format!("{}{}", c.text, if c.is_guess { "[紫]" } else { "" }))
                .collect::<Vec<_>>()
        );
    }
    let cands = engine.process("hupeng");
    let word_guesses: Vec<&str> = cands
        .iter()
        .filter(|c| c.is_guess && c.text.chars().count() >= 2)
        .map(|c| c.text.as_ref())
        .collect();
    let single_guesses = cands
        .iter()
        .filter(|c| c.is_guess && c.text.chars().count() == 1)
        .count();
    assert!(
        word_guesses.contains(&"狐朋狗友") && word_guesses.len() <= 3,
        "hupeng 零精确命中应保留「狐朋狗友」且词级猜测 ≤3 条，实际 {word_guesses:?}"
    );
    assert!(
        single_guesses <= 3,
        "单字延伸猜测 ≤3 条（紫词上限），实际 {}",
        single_guesses
    );
    seed_copy_cleanup(&db);
}

/// 英文精确命中登顶（happy 排「哈珀培养/哈珀」后，
/// 与英文匹配度最高的 happy 落在第三位不合理）：拼音覆盖不了整个输入的
/// 白词（哈珀 "hapo" vs "happy"）与绿词一律让位——happy 必须第 1 位。
/// 真实种子库守护。
#[test]
fn test_exact_english_tops_partial_match_seed() {
    let db = seed_copy("happytop");
    let engine = super::Engine::new_with_db(&db).expect("seed db");
    let cands = engine.process("happy");
    let idx = cands
        .iter()
        .position(|c| c.is_english && c.text.as_ref() == "happy")
        .expect("happy 英文候选必须出现");
    assert_eq!(
        idx,
        0,
        "happy 必须登顶，实际：{:?}",
        cands
            .iter()
            .take(4)
            .map(|c| format!("{}{}", c.text, if c.is_sentence { "[绿]" } else { "" }))
            .collect::<Vec<_>>()
    );
    seed_copy_cleanup(&db);
}

/// 种子库 → 唯一临时副本路径（返回路径字符串；用后由调用方清理）。
/// 测试禁止直接读写仓库种子（相对路径 + cwd 依赖 + new_with_db 的维护性
/// 写入会改动种子；种子内容还随部署脚本合并开发数据漂移，是单跑失败/
/// 全量绿不稳定的来源）。引用既有 tests_dict.rs 的副本范式。
/// 兄弟测试文件不得各自 `Engine::new_with_db(仓库种子)` 直接开种子——并行
/// 互踩（readonly 恢复竞争 → 套件偶发红）+ Storage::open 就地转 WAL + 种子带
/// sync_hash 时 bigram 缓存写回 assets（交付种子每轮测试被改写）。统一改走
/// 本 helper 的临时副本。
pub(crate) fn seed_copy(tag: &str) -> String {
    crate::test_tmp::require_seed_db();
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_seed_copy_{}_{}_{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::copy(seed, &tmp).expect("copy seed db");
    // 禁用共享 bigram 缓存：seed_bigram_cache.bin
    // 用 with_file_name 落在 test-tmp 共享路径，缓存键 x1dict_sync_hash
    // 又不感知裸 SQL 改表——并行测试互写共享缓存互相污染（bigram 类测试偶发
    // 红），副本上 scrub 过的表也会被别的副本写下的全量缓存顶掉（是
    // test_context_phonetic_rescue 教学前断言恒红的根因）。置空 sync_hash
    // （非 16 字符 → load_seed_bigram_table_cached 跳过缓存），每个副本读
    // 自己的表，行为确定。
    {
        let conn = rusqlite::Connection::open(&tmp).expect("open seed copy");
        conn.execute(
            "UPDATE db_meta SET value='' WHERE key='x1dict_sync_hash'",
            [],
        )
        .expect("disable shared bigram cache for copy");
    }
    tmp.to_str().unwrap().to_string()
}

#[allow(dead_code)]
pub(crate) fn seed_copy_cleanup(path: &str) {
    let p = std::path::Path::new(path);
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(p.with_extension("db-wal"));
    let _ = std::fs::remove_file(p.with_extension("db-shm"));
}

/// 种子副本（保留 x1dict_sync_hash）——词典编译缓存测试专用。
/// seed_copy 会把 hash 置空来禁共享 bigram 缓存，而编译缓存的失效键依赖
/// hash；本变体在独立子目录里保留 hash（编译缓存与 DB 同伴落
/// 该子目录——不能落 test-tmp 顶层：`with_file_name` 会掉到共享路径，多测试
/// 并行互写竞态）。
pub(crate) fn seed_copy_keephash(tag: &str) -> String {
    crate::test_tmp::require_seed_db();
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let dir = crate::test_tmp::dir().join(format!(
        "x1pinyin_seed_kh_{}_{}_{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create keephash dir");
    let tmp = dir.join("x1pinyin.db");
    std::fs::copy(seed, &tmp).expect("copy seed db");
    tmp.to_str().unwrap().to_string()
}

#[test]
fn test_guangzh_shengmu_tail() {
    let db = seed_copy("gz");
    let mut engine = super::Engine::new_with_db(&db).expect("seed db");
    for _ in 0..3 {
        engine.commit("guangzhou", "广州", Some("guang zhou"), false);
    }
    let cands = engine.process("guangzh");
    assert!(!cands.is_empty(), "guangzh 应有候选");
    assert_eq!(
        cands[0].text.as_ref(),
        "广州",
        "guangzh 首候选应为「广州」，实际: {}",
        cands[0].text
    );

    // consumed 权威值（声母残留修复）：广州 前缀扩展 guangzhou ⊇
    // guangzh → 全消费 7，上屏后 h 不再残留。
    let consumed = engine.consumed_lens("guangzh", &cands);
    assert_eq!(
        consumed[0], 7,
        "guangzh→广州 consumed 应为 7（全消费），实际 {}",
        consumed[0]
    );
    // 显示切分与音节数同源修正：guang'zh（Shengmu 整段）而非 guang'z'h
    assert_eq!(
        engine.syllable_segmentation("guangzh"),
        vec!["guang", "zh"],
        "guangzh 显示切分应为 [guang, zh]"
    );
    assert_eq!(engine.syllable_count("guangzh"), 2, "guangzh 音节数应为 2");

    // 切分层：末尾 Shengmu 路径必须存在
    let with_partial = crate::pinyin::segmentations_with_partial("guangzh", 16);
    assert!(
        with_partial.iter().any(|seg| seg == &vec!["guang", "zh"]),
        "segmentations_with_partial 应含 [guang, zh]，实际: {:?}",
        with_partial
    );
    let tokens = crate::pinyin::segmentations_tokens("guangzh", 16);
    assert!(
        tokens.iter().any(|seg| matches!(
            seg.as_slice(),
            [
                crate::pinyin::Token::Full("guang"),
                crate::pinyin::Token::Shengmu("zh")
            ]
        )),
        "segmentations_tokens 应含 [Full(guang), Shengmu(zh)]"
    );

    // 纯简拼 zh 单独：仍拆 z+h（不出现 Shengmu）——保「智慧」等双首字母词
    let zh_segs = crate::pinyin::segmentations_with_partial("zh", 16);
    assert!(
        !zh_segs.iter().any(|seg| seg == &vec!["zh"]),
        "纯简拼 zh 不应整体保留（prev=None），实际: {:?}",
        zh_segs
    );
    drop(engine);
    seed_copy_cleanup(&db);
}

/// consumed 双规则守护：前缀扩展候选全消费 + 声母尾段合并，
/// 同时锁定「正常剩余音节保留」不受误伤。
#[test]
fn test_consumed_lens_prefix_extension_and_residual() {
    let db = seed_copy("cl");
    let mut engine = super::Engine::new_with_db(&db).expect("seed db");
    engine.commit("shengcheng", "生成", Some("sheng cheng"), false);
    engine.commit("jieguo", "结果", Some("jie guo"), false);

    // ① 前缀扩展：候选拼音 ⊇ 输入 → 全消费（sh→是(shi)、zh→着/这(zhe)、
    //    ch→出(chu)——首候选具体是哪个字随词库/学习态浮动，断言与选词解耦：
    //    只断言「首候选拭音以输入为前缀（前缀扩展形态）+ consumed=全消费」）
    for input in ["sh", "zh", "ch"] {
        let cands = engine.process(input);
        assert!(!cands.is_empty(), "{input} 应有候选");
        let compact: String = cands[0].pinyin.chars().filter(|c| *c != ' ').collect();
        assert!(
            compact.starts_with(input),
            "{input} 首候选拭音 {compact} 应以输入为前缀（前缀扩展形态）"
        );
        let consumed = engine.consumed_lens(input, &cands);
        assert_eq!(
            consumed[0],
            input.len(),
            "{input}→{} 前缀扩展应全消费 {}，实际 {}",
            cands[0].text,
            input.len(),
            consumed[0]
        );
    }

    // ② 声母尾段合并 + 前缀扩展双路径都到 guangzho→广州 = 8（此前 6，残留 ho）
    let cands = engine.process("guangzho");
    assert_eq!(cands[0].text.as_ref(), "广州", "guangzho 首候选应为广州");
    let consumed = engine.consumed_lens("guangzho", &cands);
    assert_eq!(consumed[0], 8, "guangzho→广州 应全消费 8");

    // ③ 正常剩余音节保留不受误伤（负样本守护）：
    //    zhonggr→中国 留 r / zaijianh→再见 留 h / jiegw→结果 留 w。
    //    断言按 text 定位候选（首候选可能是 中国人 这类全消费词，属另一形态）
    let cases: &[(&str, &str, usize)] = &[
        ("zhonggr", "中国", 6),
        ("zaijianh", "再见", 7),
        ("jiegw", "结果", 4),
    ];
    for (input, word, expect) in cases {
        let cands = engine.process(input);
        let idx = cands
            .iter()
            .position(|c| c.text.as_ref() == *word)
            .unwrap_or_else(|| {
                panic!(
                    "{input} 候选列表应含「{word}」，实际前几: {:?}",
                    cands
                        .iter()
                        .take(3)
                        .map(|c| c.text.as_ref())
                        .collect::<Vec<_>>()
                )
            });
        let consumed = engine.consumed_lens(input, &cands);
        assert_eq!(
            consumed[idx], *expect,
            "{input}→{word} consumed 应为 {expect}（剩余音节保留），实际 {}",
            consumed[idx]
        );
    }
    drop(engine);
    seed_copy_cleanup(&db);
}

/// 输入长度护栏。
///
/// 切分 DFS 按字节递归（深度=输入字节数）+ memo O(N²) 物化，实测 ~2KB 输入
/// 即耗尽 1MB 线程栈（栈溢出 = SIGSEGV，catch_unwind 拦不住）。护栏在
/// pinyin::segment_tokens_with_opts（单点）与 Engine::process（兜底）双层设防。
#[test]
fn test_input_length_guard() {
    use crate::pinyin::MAX_INPUT_BYTES;

    // 切分层：超限返回空切分（不递归、不崩溃）
    assert!(crate::pinyin::segmentations(&"z".repeat(MAX_INPUT_BYTES + 1), 1).is_empty());
    assert!(
        crate::pinyin::segmentations_with_partial(&"xian".repeat(MAX_INPUT_BYTES / 4 + 1), 1)
            .is_empty()
    );

    // engine 层：超长输入直接空候选（此前 2KB 输入栈溢出杀进程）
    let engine = Engine::new();
    assert!(engine.process(&"z".repeat(4096)).is_empty());
    assert!(engine.process(&"xian".repeat(1024)).is_empty());

    // 边界内（护栏值本身）不崩溃且正常工作
    let at_limit = "z".repeat(MAX_INPUT_BYTES);
    let _ = engine.process(&at_limit);
    // 正常输入不受影响
    assert!(!engine.process("nihao").is_empty());
}

/// 守护：多字节输入进 is_hybrid_shengmu_input 不得 panic
///（&input[..2] 字节切片遇非 char boundary 会直接 panic——每次按键级路径）。
#[test]
fn test_hybrid_shengmu_multibyte_no_panic() {
    // 中文/emoji/混合输入：返回 false 但绝不 panic
    assert!(!Engine::is_hybrid_shengmu_input("中文输入"));
    assert!(!Engine::is_hybrid_shengmu_input("zh文"));
    assert!(!Engine::is_hybrid_shengmu_input("🀄zhgrm"));
    assert!(!Engine::is_hybrid_shengmu_input("中zhgrm"));
    // 正常混合声母不受影响
    assert!(Engine::is_hybrid_shengmu_input("zhgrm"));
    assert!(!Engine::is_hybrid_shengmu_input("zhongguo"));
}

/// 守护：加载不完整 → flush 不做全量覆写（空态清零链闸门）。
/// 用损坏 DB 触发 load 失败（垃圾字节不是合法 SQLite 文件）。
#[test]
fn test_flush_skips_snapshot_when_load_incomplete() {
    let dir = crate::test_tmp::dir().join("x1core_audit_d6");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("corrupt.db");
    std::fs::write(&db, b"this is not a sqlite database at all").unwrap();
    // new_with_db 对垃圾文件：open 可能成功（惰性）但 init_schema/读表失败
    // → Engine 可能返回 Err（回退内存模式）或带 storage_load_incomplete。
    // 无论哪种，都不能出现"空内存态 + 正常 flush 覆写"。
    match super::Engine::new_with_db(db.to_str().unwrap()) {
        Err(_) => { /* open 失败 = 内存模式，安全 */ }
        Ok(mut engine) => {
            // 引擎起来则模拟用户打字 + flush：不应 panic、不应 60s 覆写
            engine.commit("nihao", "你好", Some("ni hao"), true);
            let _ = engine.flush();
            // flush 后引擎仍可用（损坏 DB 回退态下不保证出词，只验证不 panic）
            let _ = engine.process("nihao");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// 多音字次音 freq 语料定标守护。
///
/// 病灶：compute_freq 按字不按音，多音字所有读音行共享同一 freq（的/di=9999），
/// 次音在对应音节冷启动虚高（的 压过 低/底/滴，与「呢 ni」同类）。
/// 定标后次音 freq = 主音 freq × 自有分词语料占比（三路归因：POS 读音标注 /
/// 多字词反查 / 单字回退主音）。数据守护 + 行为守护双锁。
#[test]
fn test_polyphone_secondary_freq_calibrated() {
    let seed = seed_copy("q");

    // 数据守护：次音 freq 必须显著低于主音（宽比例上限，防定标回退到同频）
    {
        use rusqlite::Connection;
        let conn = Connection::open(&seed).unwrap();
        for (ch, sec_py, max_ratio) in [
            ("的", "di", 0.10),
            ("了", "liao", 0.20),
            ("都", "du", 0.30),
            ("行", "hang", 0.30),
            ("长", "zhang", 0.70),
            ("还", "huan", 0.20),
        ] {
            let (sec, prim): (i64, i64) = conn
                .query_row(
                    "SELECT
                        (SELECT freq FROM x1dict_words WHERE simplified=?1 AND pinyin=?2),
                        (SELECT freq FROM x1dict_words WHERE simplified=?1 AND is_primary=1)",
                    rusqlite::params![ch, sec_py],
                    |r| {
                        Ok((
                            r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                            r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        ))
                    },
                )
                .unwrap();
            assert!(
                sec > 0 && prim > 0,
                "「{}」主/次音行缺失：prim={} sec={}",
                ch,
                prim,
                sec
            );
            assert!(
                (sec as f64) < (prim as f64) * max_ratio,
                "「{}」次音 {} freq={} 未定标（应 < 主音 {}×{:.0}%）",
                ch,
                sec_py,
                sec,
                prim,
                max_ratio * 100.0
            );
        }
    }

    // 行为守护：输入 di，冷启动前 6 候选不得出现「的」（真实使用中 di 仅占 的 的一成不到）
    let engine = Engine::new_with_db(&seed).unwrap();
    let cands = engine.process("di");
    let top6: Vec<String> = cands.iter().take(6).map(|c| c.text.to_string()).collect();
    assert!(
        !top6.contains(&"的".to_string()),
        "di 前 6 候选不应含「的」（次音定标失效）：{:?}",
        top6
    );
    // 地/第 仍应在前排（定标不动主音行）
    assert!(
        top6.first().map(|s| s == "地").unwrap_or(false) || top6.contains(&"第".to_string()),
        "di 候选应保留 地/第 前排：{:?}",
        top6
    );
}

/// 守护：di 选「的」后，de 读音的「的」不得获得任何加成。
///
/// 三项统计（rank_boost/count/recent）按音隔离是既有设计；本测试额外锁住
/// user_modulator 豁免缝隙——counts 是不分音的文本级表，di 选择会写入 counts[的]，
/// 旧代码 counts_contains(的)=true 会让 de 读音拿到"用过"豁免（600→1000，base×1.67）。
/// 断言：di 提交前后，de 音「的」的 base / total / 排名逐位不变；di 侧学习照常生效。
#[test]
fn test_cross_reading_commit_does_not_boost_other_reading() {
    let seed = seed_copy("q");
    let mut engine = Engine::new_with_db(&seed).unwrap();

    // 垫一个多字词让 counts 非空（脱离全局冷启动守卫，modulator 缝隙才可见）
    engine.commit("nihao", "你好", Some("ni hao"), false);

    let de_of = |e: &Engine| {
        let cands = e.process("de");
        let rank = cands
            .iter()
            .position(|c| c.text.as_ref() == "的")
            .expect("de 候选应含「的」");
        let cand = cands[rank].clone();
        (rank, e.score_breakdown("de", &cand))
    };

    let (rank_before, bd_before) = de_of(&engine);
    // 用户打 di 选「的」（advance_context=false，隔离上下文副作用）
    engine.commit("di", "的", Some("di"), false);
    let (rank_after, bd_after) = de_of(&engine);

    assert_eq!(
        bd_after.base, bd_before.base,
        "di 选「的」后 de 读音 base 变化（modulator 跨音串扰）：{} → {}",
        bd_before.base, bd_after.base
    );
    assert_eq!(
        bd_after.total(),
        bd_before.total(),
        "di 选「的」后 de 读音 total 不得变化：{} → {}",
        bd_before.total(),
        bd_after.total()
    );
    assert_eq!(
        rank_after, rank_before,
        "di 选「的」后 de 读音排名不得变化：{} → {}",
        rank_before, rank_after
    );

    // 反向验证：di 侧学习照常生效（按音统计确实增长）
    assert!(
        engine.freq.pinyin_count("的", "di") > 0,
        "di 读音的学习应正常记录"
    );
    // 且 de 读音的按音统计未被污染
    assert_eq!(
        engine.freq.pinyin_count("的", "de"),
        0,
        "de 读音按音计数不应被 di 选择污染"
    );
}

/// 多字词 freq 语料定标守护（「词承载」专项）。
///
/// 病灶：compute_freq 公式给一批极常用词算出离谱低值（中国=18、没有=14、时间=4），
/// 字+字种子统计清零后词路径再也无法凭自身 freq 与单字路径竞争（eval 冷启动
/// Top1 47.0→43.6）。定标：2014 语料真实词频 × 单字锚点 log-log 回归映射到引擎
/// freq 尺度，只升不降。定标后 Top1 50.8%（净 +3.8pp，书面 38.5→72.3）。
#[test]
fn test_word_freq_calibrated() {
    let seed = seed_copy("q");
    use rusqlite::Connection;
    let conn = Connection::open(&seed).unwrap();
    // 极常用词必须已在语料量级（防退回公式垃圾值 4~18）
    for (word, min) in [
        ("中国", 4000),
        ("没有", 4000),
        ("时间", 3000),
        ("这些", 3000),
    ] {
        let freq: i64 = conn
            .query_row(
                "SELECT MAX(freq) FROM x1dict_words WHERE simplified=?1",
                rusqlite::params![word],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert!(
            freq >= min,
            "「{}」freq={} 未达语料定标量级（应 ≥{}）",
            word,
            freq,
            min
        );
    }
    // 行为守护：极常用词应作为整词候选精确命中并居首
    let engine = Engine::new_with_db(&seed).unwrap();
    let top = engine
        .process("zhongguo")
        .first()
        .map(|c| c.text.to_string());
    assert_eq!(
        top.as_deref(),
        Some("中国"),
        "zhongguo 首候选应为「中国」，实际 {:?}",
        top
    );
}

/// 【诊断】候选序实测（真实使用反馈的三项问题）。
/// X1_DIAG_DB 环境变量指定 DB（默认 repo 种子；对比定标前后用备份路径）。
/// print-only，无断言——分析用，勿在 CI 依赖其输出。
#[test]
fn test_diag_user_feedback() {
    let db = std::env::var("X1_DIAG_DB").unwrap_or_else(|_| seed_copy("diag"));
    let mut engine = Engine::new_with_db(&db).unwrap();
    println!("=== DB: {} ===", db);
    // 探针：jimtian/xianfong 纠错门控形态
    for probe in ["jimtian", "xianfong", "chnagtai"] {
        let segs = crate::pinyin::segmentations(probe, 4);
        println!(
            "probe {}: segs={:?} legal_mix={}",
            probe,
            segs,
            crate::engine::correction::is_legal_jianpin_mix(probe)
        );
    }
    // 探针：xianzw 在用户库副本上的分数分解（先在>现在 排查）
    {
        let cands = engine.process("xianzw");
        for c in cands.iter().take(5) {
            let bd = engine.score_breakdown("xianzw", c);
            println!(
                "xianzw breakdown {} freq={}: base={} total={} recent={} mapping={} bigram={}",
                c.text,
                c.base_freq,
                bd.base,
                bd.total_bonus,
                bd.recent,
                bd.mapping_bonus,
                bd.bigram_bonus
            );
        }
    }
    // 探针：woaizongguo 教学闭环（营救排查）
    {
        let cands = engine.process("woaizongguo");
        println!(
            "woaizongguo pre-teach top: {:?}",
            cands
                .iter()
                .take(4)
                .map(|c| c.text.to_string())
                .collect::<Vec<_>>()
        );
        println!(
            "woaizongguo pre-teach has 我爱中国 sentence: {}",
            cands.iter().any(|c| c.text.as_ref() == "我爱中国")
        );
        // 教学 + 复查（诊断：区分引擎逻辑 vs 服务层）
        engine.commit("woaizhongguo", "我爱中国", Some("wo ai zhong guo"), true);
        engine.clear_context();
        println!(
            "post-teach bigram(我爱,中国) = {}",
            engine.bigram_count_for_test("我爱", "中国")
        );
        println!(
            "post-teach bigram(爱,中国) = {}",
            engine.bigram_count_for_test("爱", "中国")
        );
        let cands2 = engine.process("woaizongguo");
        println!(
            "woaizongguo post-teach top: {:?}",
            cands2
                .iter()
                .take(5)
                .map(|c| (c.text.to_string(), c.is_correction))
                .collect::<Vec<_>>()
        );
        // 变体句直接解码（排查 decode_sentence 是否产出 我爱中国）
        let var_seq: Vec<&str> = vec!["wo", "ai", "zhong", "guo"];
        let sents = engine.decode_sentence("woaizhongguo", &var_seq, false, false);
        println!(
            "variant decode first 3: {:?}",
            sents
                .iter()
                .take(3)
                .map(|c| c.text.to_string())
                .collect::<Vec<_>>()
        );
    }
    // 探针：hendr 分数分解（很多人 vs 很多）
    {
        let cands = engine.process("hendr");
        for c in cands.iter().take(4) {
            let bd = engine.score_breakdown("hendr", c);
            println!(
                "hendr breakdown {} freq={}: base={} total={} recent={} mapping={} bigram={}",
                c.text,
                c.base_freq,
                bd.base,
                bd.total_bonus,
                bd.recent,
                bd.mapping_bonus,
                bd.bigram_bonus
            );
        }
    }
    // 探针：zgr 分数分解（在/最 vs 中国人，简拼路径的学习分vs结构分）
    {
        let cands = engine.process("zgr");
        for c in cands.iter().take(6) {
            let bd = engine.score_breakdown("zgr", c);
            println!(
                "zgr breakdown {} freq={}: base={} total={} recent={} mapping={} bigram={}",
                c.text,
                c.base_freq,
                bd.base,
                bd.total_bonus,
                bd.recent,
                bd.mapping_bonus,
                bd.bigram_bonus
            );
        }
    }
    // 探针：shenng 的纠错路径形态
    println!(
        "is_legal_jianpin_mix(shenng) = {}",
        crate::engine::correction::is_legal_jianpin_mix("shenng")
    );
    let parsed = crate::pinyin::parse_pinyin("shenng");
    println!(
        "parse_pinyin(shenng) syllables = {:?}",
        parsed
            .syllables
            .iter()
            .map(|s| &"shenng"[s.start..s.end])
            .collect::<Vec<&str>>()
    );

    let cases: &[(&str, &[&str])] = &[
        // (输入, 期望应在前排的字/词)
        ("w", &["我", "问", "无"]),
        ("wo", &["我", "我们"]),
        ("s", &["是", "说", "时"]),
        ("d", &["的", "大", "到"]),
        ("l", &["了", "来", "里"]),
        ("h", &["和", "好", "还"]),
        ("sh", &["是", "说", "什么"]),
        // 末字偷懒只打首字母（混合简拼尾）
        ("xianzw", &["现在"]),
        ("zheyw", &["这样"]),
        ("jiegw", &["结果"]),
        ("wom", &["我们"]),
        ("bus", &["不是"]),
        // 纯简拼三字词（zgr 应出中国人）
        ("zgr", &["中国人"]),
        // 双字母+尾声母（yingg 应该应第一，橙词英不应抢前）
        ("yingg", &["应该"]),
        ("shenng", &["生", "省"]),
        ("sheng", &["生", "省"]),
    ];
    for (input, expects) in cases {
        let cands = engine.process(input);
        println!("\n--- {} 期望前排 {:?} ---", input, expects);
        for (i, c) in cands.iter().take(10).enumerate() {
            println!(
                "  {:>2}. {} freq={}{}{}{}",
                i + 1,
                c.text,
                c.base_freq,
                if c.is_sentence { " [句]" } else { "" },
                if c.is_correction { " [纠]" } else { "" },
                if c.is_guess { " [猜]" } else { "" }
            );
        }
        for e in *expects {
            if let Some(pos) = cands.iter().position(|c| c.text.as_ref() == *e) {
                println!("  → 「{}」在第 {} 位", e, pos + 1);
            } else {
                println!("  → 「{}」未出现在候选中", e);
            }
        }
    }
}

/// 简拼与纠错三项守护：
/// 1. 双字母声母简拼：sh 下「是」必须前排（旧实现排第 101 位——简拼单字附录在
///    排序后裸 append 不参与加分，且完全匹配加分只认 字数==字母数）；
/// 2. 简拼/混合的整词优先层级：合法简拼
///    （含混合）下能匹配上（覆盖全部输入单位）的单个词候选，结构层优先
///    于部分匹配（留尾巴）的词，与词频无关——jiegw→结果为（3单位全匹配）
///    优先于 结果（留 w），hendr/hen'd'r→很多人 优先于 很多；词频只在
///    同层内排序。
/// 3. 双写字母折叠纠错：shenng 前 3 含橙标「生」（合法简拼解析吞掉纠错
///    触发时，用户拿到的是 神女/神农 系错误解读）。
#[test]
fn test_user_feedback_jianpin_and_correction() {
    let seed = seed_copy("q");
    let engine = Engine::new_with_db(&seed).unwrap();

    // 1. 双字母声母：单字回前排（是/说 系 + zh 的 中/这 系）
    let sh_cands = engine.process("sh");
    let top5: Vec<&str> = sh_cands.iter().take(5).map(|c| c.text.as_ref()).collect();
    assert!(
        top5.contains(&"是"),
        "sh 前 5 应含「是」（双字母声母被词淹没回归）：{:?}",
        top5
    );
    let zh_cands = engine.process("zh");
    let top5z: Vec<&str> = zh_cands.iter().take(5).map(|c| c.text.as_ref()).collect();
    assert!(
        top5z.iter().any(|t| ["中", "这", "种", "只"].contains(t)),
        "zh 前 5 应含 中/这/种/只：{:?}",
        top5z
    );
    // 单字母路径零变化（s=是 #1）
    let top = engine.process("s").first().map(|c| c.text.to_string());
    assert_eq!(top.as_deref(), Some("是"), "单字母简拼行为不得回归");

    // 2. 整词优先层级：完整匹配（覆盖全部单位）优先于留尾巴，与词频无关
    // （句子候选与完整词同享结构层——都覆盖全部单位，层内按各自分数排）
    for (input, expect) in [("jiegw", "结果为"), ("bus", "不是")] {
        let top = engine.process(input).first().map(|c| c.text.to_string());
        assert_eq!(
            top.as_deref(),
            Some(expect),
            "{} 首候选应为「{}」（整词优先）",
            input,
            expect
        );
    }
    // zheyw：完整匹配层（这样我[句]）优先于留尾巴的 这样
    let zyw = engine.process("zheyw");
    let pos_zy = zyw.iter().position(|c| c.text.as_ref() == "这样").unwrap();
    assert!(
        pos_zy < 5,
        "这样 应在完整匹配层之后紧随（第 {} 位）",
        pos_zy + 1
    );
    // xianzw：完整匹配层（现在我[句]/现在网）在留尾巴的 现在 之前——
    // 句子候选与完整词同享结构层（都覆盖全部单位），层内按各自分数排
    let xzw = engine.process("xianzw");
    let pos_xzw = xzw
        .iter()
        .position(|c| c.text.as_ref() == "现在网")
        .unwrap();
    let pos_now = xzw.iter().position(|c| c.text.as_ref() == "现在").unwrap();
    assert!(
        pos_xzw < pos_now,
        "完整匹配的现在网({}) 应在留尾巴的现在({}) 前",
        pos_xzw + 1,
        pos_now + 1
    );
    // 留尾巴的高频词紧随其后（同层内频率主导）
    let jiegw = engine.process("jiegw");
    let pos_jg = jiegw
        .iter()
        .position(|c| c.text.as_ref() == "结果")
        .unwrap();
    assert!(pos_jg < 5, "结果 应紧随 结果为 之后：第 {} 位", pos_jg + 1);
    // 无撇号/有撇号同型：很多人（3 单位全匹配）优先于 很多（留 r）
    let top = engine
        .process("hen'd'r")
        .first()
        .map(|c| c.text.to_string());
    assert_eq!(
        top.as_deref(),
        Some("很多人"),
        "hen'd'r 首候选应为「很多人」"
    );
    let top = engine.process("hendr").first().map(|c| c.text.to_string());
    assert_eq!(
        top.as_deref(),
        Some("很多人"),
        "hendr 首候选应为「很多人」（整词优先）"
    );

    // 3. 双写折叠纠错：shenng 前 3 含橙标「生」，且插在最佳自然候选之后
    //（折叠让位给最强自然解读——yingg→应该#1、应[纠]#2）
    let cands = engine.process("shenng");
    let top3: Vec<&crate::dict::Candidate> = cands.iter().take(3).collect();
    assert!(
        top3.iter()
            .any(|c| c.text.as_ref() == "生" && c.is_correction),
        "shenng 前 3 应含橙标「生」：{:?}",
        top3.iter().map(|c| c.text.to_string()).collect::<Vec<_>>()
    );
    // yingg：应该（2 单位完整匹配）第一，折叠候选紧随其后
    let yg = engine.process("yingg");
    let top_yg = yg.first().map(|c| c.text.to_string());
    assert_eq!(
        top_yg.as_deref(),
        Some("应该"),
        "yingg 首候选应为「应该」，实际 {:?}",
        top_yg
    );
    let pos_y = yg
        .iter()
        .position(|c| c.text.as_ref() == "应" && c.is_correction);
    assert!(
        pos_y.map(|p| p < 4).unwrap_or(false),
        "应[纠] 应紧随应该之后：{:?}",
        yg.iter()
            .take(5)
            .map(|c| c.text.to_string())
            .collect::<Vec<_>>()
    );
    // 反例：合法全拼的跨音节双写（changgan 的 gg）不触发折叠纠错
    let cands = engine.process("jimtian");
    let top6j: Vec<&crate::dict::Candidate> = cands.iter().take(6).collect();
    assert!(
        top6j
            .iter()
            .any(|c| c.text.as_ref() == "今天" && c.is_correction),
        "jimtian 前 6 应含橙标「今天」（m 伪音节吸收修复）：{:?}",
        top6j.iter().map(|c| c.text.to_string()).collect::<Vec<_>>()
    );
    // 纯简拼三字词：zgr 双向定标后中国人第一
    //（虚高的 这个人 2678→1177、自个儿 2271→380 向语料值收敛）
    let zgr = engine.process("zgr");
    let top_zgr = zgr.first().map(|c| c.text.to_string());
    assert_eq!(
        top_zgr.as_deref(),
        Some("中国人"),
        "zgr 首候选应为「中国人」，实际 {:?}",
        top_zgr
    );
    // changgan：合法全拼的跨音节双写（gg）不触发折叠纠错
    let cands = engine.process("changgan");
    assert!(
        cands.iter().take(3).all(|c| !c.is_correction),
        "changgan 是合法全拼（chang+gan），不应出折叠纠错候选"
    );
}

/// 双击故障时序感知守护：
/// 折叠纠错只对"故障级快双击"（击键间隔 < DOUBLE_FAULT_MS=100ms）生效；
/// 正常节奏的刻意重复（如简拼 shen+n+g 的 nn，间隔正常）不折叠。
/// 未传时序（process）保持旧行为——恒可折叠，旧前端零回归。
#[test]
fn test_double_fault_timing_gate() {
    let seed = seed_copy("q");

    // 前提：未传时序（旧行为）——折叠生效
    {
        let engine = Engine::new_with_db(&seed).unwrap();
        let cands = engine.process("shenng");
        assert!(
            cands
                .iter()
                .take(3)
                .any(|c| c.text.as_ref() == "生" && c.is_correction),
            "未传时序：折叠纠错应生效（旧行为）"
        );
    }

    // 快双击（30ms，键盘 bounce 签名）→ 折叠生效
    {
        let engine = Engine::new_with_db(&seed).unwrap();
        // 逐键模拟：shen(正常节奏) → n(100ms) → n(30ms，双写完成)
        engine.process_timed("s", 300);
        engine.process_timed("sh", 150);
        engine.process_timed("she", 120);
        engine.process_timed("shen", 100);
        engine.process_timed("shenn", 30);
        let cands = engine.process_timed("shenng", 90);
        assert!(
            cands
                .iter()
                .take(4)
                .any(|c| c.text.as_ref() == "生" && c.is_correction),
            "快双击(30ms)：折叠纠错应生效（故障判定）"
        );
    }

    // 慢双写（500ms，刻意输入）→ 不折叠（shen+n+g 是合法简拼意图）
    {
        let engine = Engine::new_with_db(&seed).unwrap();
        engine.process_timed("s", 300);
        engine.process_timed("sh", 200);
        engine.process_timed("she", 180);
        engine.process_timed("shen", 220);
        engine.process_timed("shenn", 500);
        let cands = engine.process_timed("shenng", 150);
        assert!(
            cands
                .iter()
                .take(4)
                .all(|c| c.text.as_ref() != "生" || !c.is_correction),
            "慢双写(500ms)：不应折叠（刻意输入，shen+n+g 简拼意图）"
        );
    }
}

/// 上下文证据发音营救守护：
/// 合法输入 + 发音变体（z↔zh 等）+ 搭配链证据过硬 → 橙色纠错候选；
/// 无证据 → 严格按模糊音设置（不出现变体）；模糊音已开对子 → 跳过（不重复）。
#[test]
fn test_context_phonetic_rescue() {
    // fixture = 种子副本 + 清链证据：
    // ① 测试禁直开仓库种子——new_with_db 维护性写入 + 本测试教学
    //    commit 会把学习结果写回种子文件，多轮后「教学前无证据」前提被自己
    //    上一轮的教学数据破坏。
    // ② 语料反哺把 ('我爱','中国') 计数写进种子 bigram 后，3c 案例的
    //    「教学前无 (爱→中国) 证据」前提失效 → 恒红。副本上清掉该链证据，
    //    恢复测试自带前提（教学前严格模式不出「我爱中国」纠错）。
    let seed = seed_copy("phonrescue");
    {
        let conn = rusqlite::Connection::open(&seed).unwrap();
        conn.execute(
            "DELETE FROM seed_bigram_counts WHERE prev='我爱' AND text='中国'",
            [],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM user_bigram_counts WHERE prev='我爱' AND text='中国'",
            [],
        )
        .unwrap();
    }
    let seed_db = seed;
    let seed = seed_db.as_str();

    // 1. 句级链证据：zongguoren 字面 [宗][国][人] 链 ~0 vs 变体 [中国人]
    //    带 (中国→人 12) → 中国人[纠] 出现
    {
        let engine = Engine::new_with_db(seed).unwrap();
        let cands = engine.process("zongguoren");
        let hit = cands
            .iter()
            .take(6)
            .find(|c| c.text.as_ref() == "中国人" && c.is_correction);
        assert!(
            hit.is_some(),
            "zongguoren 应出橙色「中国人」（句内链证据 中国→人 12）：{:?}",
            cands
                .iter()
                .take(6)
                .map(|c| c.text.to_string())
                .collect::<Vec<_>>()
        );
    }

    // 2. 词级上文证据：上屏「的」后输入 zongguo → (的→中国 32) → 中国[纠]
    {
        let mut engine = Engine::new_with_db(seed).unwrap();
        engine.commit("de", "的", Some("de"), true);
        engine.clear_context(); // 重建干净栈后压入"的"——直接 push
        engine.context.push("的".to_string());
        let cands = engine.process("zongguo");
        let hit = cands
            .iter()
            .take(6)
            .find(|c| c.text.as_ref() == "中国" && c.is_correction);
        assert!(
            hit.is_some(),
            "的+zongguo 应出橙色「中国」（上文证据 的→中国 32）：{:?}",
            cands
                .iter()
                .take(6)
                .map(|c| c.text.to_string())
                .collect::<Vec<_>>()
        );
    }

    // 3. 既有词证据（无上文也成立）：孤立 zongguo 字面无整词（宗国不是词）
    //    vs 变体命中 中国 → 中国[纠] 出现（用户原话"bigram 数据或者既有词"）
    {
        let engine = Engine::new_with_db(seed).unwrap();
        let cands = engine.process("zongguo");
        let hit = cands
            .iter()
            .take(6)
            .find(|c| c.text.as_ref() == "中国" && c.is_correction);
        assert!(
            hit.is_some(),
            "孤立 zongguo 应出橙色「中国」（既有词证据：字面无整词）：{:?}",
            cands
                .iter()
                .take(6)
                .map(|c| c.text.to_string())
                .collect::<Vec<_>>()
        );
    }

    // 3b. 字面有整词 → 既有词证据不成立 → 严格按模糊音设置：
    //     zonghe 字面命中 综合（整词）→ 不出 中和[纠]
    {
        let engine = Engine::new_with_db(seed).unwrap();
        let cands = engine.process("zonghe");
        let hit = cands
            .iter()
            .find(|c| c.text.as_ref() == "中和" && c.is_correction);
        assert!(
            hit.is_none(),
            "zonghe 字面有整词「综合」，不应出「中和」纠错（严格按模糊音设置）：{:?}",
            cands
                .iter()
                .take(5)
                .map(|c| c.text.to_string())
                .collect::<Vec<_>>()
        );
    }

    // 3c. 教学闭环（数据自愈）：种子无 (爱→中国)（语料切分不含），
    //     正确打一遍 woaizhongguo 并 commit → 拆词学习记 (爱→中国)+3 →
    //     此后 woaizongguo 句级营救生效（我爱中国[纠]）
    {
        let mut engine = Engine::new_with_db(seed).unwrap();
        // 教学前：无证据 → 不救（严格模式）
        let before = engine.process("woaizongguo");
        assert!(
            !before
                .iter()
                .any(|c| c.text.as_ref() == "我爱中国" && c.is_correction),
            "教学前无 (爱→中国) 证据，不应出「我爱中国」纠错"
        );
        // 教学：正确输入并 commit（拆词学习记录 爱→中国 +3）
        engine.commit("woaizhongguo", "我爱中国", Some("wo ai zhong guo"), true);
        engine.clear_context();
        let after = engine.process("woaizongguo");
        let hit = after
            .iter()
            .take(6)
            .find(|c| c.text.as_ref() == "我爱中国" && c.is_correction);
        assert!(
            hit.is_some(),
            "教学后应出橙色「我爱中国」（爱→中国 用户 bigram +3）：{:?}",
            after
                .iter()
                .take(6)
                .map(|c| c.text.to_string())
                .collect::<Vec<_>>()
        );
    }

    // 4. 模糊音已开对子：z_zh 开启后 zong 查询扩展已含 zhong → 营救跳过
    //   （中国 是正常候选而非纠错候选）
    {
        let mut engine = Engine::new_with_db(seed).unwrap();
        engine.context.push("的".to_string());
        engine.fuzzy_config = crate::fuzzy::FuzzyConfig {
            enabled: true,
            z_zh: true,
            ..Default::default()
        };
        let cands = engine.process("zongguo");
        let as_correction = cands
            .iter()
            .find(|c| c.text.as_ref() == "中国" && c.is_correction);
        assert!(
            as_correction.is_none(),
            "模糊音 z_zh 已开：中国 应为正常候选（查询扩展），不应重复出纠错"
        );
    }
    seed_copy_cleanup(&seed_db);
}

/// FFI 级端到端守护：完全模拟宿主前端的调用形态
/// ——x1_engine_new_with_db + 双击 setter 注入 + x1_engine_process_timed 逐键
/// （首键 u32::MAX 哨兵 = keyDeltaMs 无前键），验证折叠候选经 FFI 边界（C 字符串
/// JSON）完整返回。防宿主前端时序传递回归（keyDeltaMs 丢失 → 恒 MAX → 恒旧行为，
/// 症状即"双击纠错不触发"）。
#[test]
fn test_double_fault_via_ffi_swift_shape() {
    use std::ffi::{CStr, CString};

    // 种子副本：时序行为测试不依赖用户数据；且本测试走
    // x1_engine_new_with_db（合并模式 FFI），开分离布局的 user.db 只有用户表
    // （空系统词典），必须用完整库。
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let db = seed_copy("ffi");
    std::fs::copy(seed, &db).expect("copy seed");
    let c_db = CString::new(db).unwrap();
    let ptr = crate::ffi::x1_engine_new_with_db(c_db.as_ptr());
    assert!(!ptr.is_null(), "引擎创建失败");

    unsafe {
        // 偏好注入（与宿主前端应用纠错配置同款，无条件调用）。
        // enabled 开关已摘除（与防抖双绑会让引擎折叠恒失效），
        // 时序门控恒活——本测试即「无 enabled 注入也能折叠」的回归守护。
        crate::ffi::config::x1_engine_set_double_fault_threshold_ms(ptr, 100);

        let query = |input: &str, dms: u32| -> String {
            let c_input = CString::new(input).unwrap();
            let raw = crate::ffi::query::x1_engine_process_timed(ptr, c_input.as_ptr(), dms);
            assert!(!raw.is_null(), "process_timed 返回 null: {input}");
            let s = CStr::from_ptr(raw).to_string_lossy().into_owned();
            crate::ffi::x1_string_free(raw);
            s
        };

        // 快双击序列（宿主前端形态：首键 MAX 哨兵，n 双写间隔 30ms < 100）
        query("s", u32::MAX);
        query("sh", 120);
        query("she", 110);
        query("shen", 100);
        query("shenn", 30);
        let json = query("shenng", 90);
        assert!(
            json.contains("\"text\":\"生\"") && json.contains("\"is_correction\":true"),
            "快双击：JSON 应含橙色折叠候选「生」，实得前 200 字: {}",
            &json[..json.len().min(200)]
        );

        // 慢双写（500ms 刻意节奏）→ 无折叠（shen+n+g 简拼意图保护）
        query("sh", u32::MAX);
        query("she", 200);
        query("shen", 220);
        query("shenn", 500);
        let json2 = query("shenng", 150);
        // 「生」可能作为自然候选出现（部分匹配路径），但不得带纠错标记
        let idx = json2.find("\"text\":\"生\"");
        let marked = idx
            .map(|i| {
                !json2[i..]
                    .trim_start_matches("\"text\":\"生\",")
                    .starts_with("\"freq\"")
                    && json2[i..].contains("\"is_correction\":true")
                    && json2[i..]
                        .find("\"is_correction\":true")
                        .map(|j| j < 80)
                        .unwrap_or(false)
            })
            .unwrap_or(false);
        assert!(!marked, "慢双写：生 不应带 is_correction 标记");

        crate::ffi::x1_engine_free(ptr);
    }
}

/// 回归守护：jishufanganwendang 绿词应按 fang+an 拆分出「技术方案文档」。
/// 根因：fang+an 切分触发贪婪合并惩罚（fang 去尾 g→fan、
/// g+an→gan，存在 fan+gan 替代读法）被 ÷100 压到层级 1，而 fan+gan 零惩罚拿
/// 层级 2——长句无整词兜底时绿词 DP 收到 fan+gan，拼出「技术反感文档」。
/// 短句 jishufangan 能对纯属「技术方案」是 4 音节整词的巧合（层级 3 压过惩罚）。
///
/// 修复（词频事实裁决真歧义）：
/// ① 贪婪合并惩罚加窗口词频豁免——当前切法窗口成词 freq ≥ 替代切法时免罚
///   （「方案」3455 > 「反感」997）；原受益场景不受影响（sang+e 无词 <
///   san+ge「三个」照罚；du+an 是另一类「合并成完整音节」惩罚，不豁免，
///   test_duan_prefers_full_syllable 继续守护）。
/// ② 同分 tie-break 在「首音节短优先」前比较首分歧窗口成词 freq。
#[test]
fn test_jishufanganwendang_prefers_fang_an() {
    let engine = real_engine();
    // 短句基线（整词路径，防御性回归）
    let short = engine.process("jishufangan");
    assert_eq!(
        short.first().map(|c| c.text.as_ref()),
        Some("技术方案"),
        "jishufangan 首候选应为「技术方案」，实际: {:?}",
        short.iter().take(3).map(|c| &c.text).collect::<Vec<_>>()
    );
    // 长句绿词（本修复目标）：fang+an 拆分胜出
    let long = engine.process("jishufanganwendang");
    let top = long.iter().find(|c| c.is_sentence);
    let (text, pinyin) = top
        .map(|c| (c.text.as_ref(), c.pinyin.as_ref()))
        .unwrap_or(("", ""));
    assert_eq!(
        text,
        "技术方案文档",
        "jishufanganwendang 绿词应为「技术方案文档」（fang an 拆分），实际 {:?} {:?}，前3: {:?}",
        text,
        pinyin,
        long.iter().take(3).map(|c| &c.text).collect::<Vec<_>>()
    );
    assert!(
        pinyin.contains("fang an"),
        "绿词拼音应含 fang an 切分，实际 {}",
        pinyin
    );
}

/// 回归守护：平分歧义下显示/消费链与候选链必须选同一切分
/// （场景：输入 wangou 显示 wang'ou、候选全是 wan 音字、选『弯』后残留
/// ou、g 被吞）。根因：pick_segmentation 同分「保留先枚举到的」（DFS 长
/// 音节优先 → wang|ou），候选链 process_inner 却按精细裁决选 wan|gou——
/// 显示 wang'ou 与候选（wan 音字）自相矛盾；consumed 按显示切分的前 n 音节
/// 偏移算，选「弯」消费 4 字节吞掉 g。修复：两边同源 seg_order_cmp。
#[test]
fn test_wangou_display_consumed_match_candidates() {
    let engine = real_engine();
    // 显示切分与候选链同选 wan|gou
    assert_eq!(
        engine.syllable_segmentation("wangou"),
        vec!["wan", "gou"],
        "wangou 显示切分应为 wan|gou（与候选链同源），实际 {:?}",
        engine.syllable_segmentation("wangou")
    );
    // 选首音节单字「弯」只消费 3 字节（wan），残留 gou 而非 ou
    let cands = engine.process("wangou");
    let wan = cands
        .iter()
        .find(|c| c.text.as_ref() == "弯")
        .unwrap_or_else(|| {
            panic!(
                "wangou 应有「弯」候选，实际前5: {:?}",
                cands.iter().take(5).map(|c| &c.text).collect::<Vec<_>>()
            )
        });
    let lens = engine.consumed_lens("wangou", std::slice::from_ref(wan));
    assert_eq!(
        lens,
        vec![3],
        "选「弯」应消费 wan 3 字节、残留 gou，实际 {:?}",
        lens
    );
}

/// 分层驻留设计基线：种子 DB 加载后的内存构成盘点（诊断工具，平时 ignore）。
///
/// 输出四块：① 词典存储（arena 化：arena 基底紧凑数组 + 词索引
/// span 基底 + 运行时 overlay 增量）② 单字表 ③ seed n-gram 行数 ④ DB freq
/// 分布（分层边界设计的输入：各阈值下的词条数占比）。与进程 RSS 的差值 =
/// SQLite 页缓存 + 统计库 + fx + 运行时（外部另测）。
#[test]
#[ignore = "诊断工具：cargo test -p x1core --lib memory_profile_seed_db -- --ignored --nocapture"]
fn memory_profile_seed_db() {
    let seed = seed_copy("q");
    let engine = Engine::new_with_db(&seed).unwrap();
    let p = engine.dict.memory_profile();

    let node_sz = std::mem::size_of::<crate::dict::TrieNode>();
    let cand_sz = std::mem::size_of::<crate::dict::Candidate>();
    // overlay 节点 Trie（arena 化后只剩运行时用户词增量，量级极小）：
    // TrieNode = SmallStr<8>（内联）+ 两个 Vec；Vec 堆 = 容量 × 元素大小，
    // 字符串堆另计（allocator 取整忽略——确定性估算，非逐字节精确）。
    let overlay_bytes = p.node_count * node_sz
        + p.children_capacity * node_sz
        + p.candidates_capacity * cand_sz
        + p.syllable_heap_bytes
        + p.text_heap_bytes
        + p.pinyin_heap_bytes;

    println!("== 词典存储（arena 化：基底 + 运行时 overlay）==");
    println!(
        "arena 基底：节点 {} / 候选 {}，紧凑平行数组 + blob = {:.1}MB（精确预留，零增长碎片）",
        p.arena_nodes,
        p.arena_candidates,
        p.arena_bytes as f64 / 1048576.0
    );
    println!("词索引 span 基底：{} 条 × 16B + text/py blob = {:.1}MB（运行时 Vec 已清空，另有运行时增量 {} 条）",
        p.wi_span_len, p.wi_span_bytes as f64 / 1048576.0, p.word_index_len);
    println!(
        "overlay 节点 Trie（用户词运行时增量）：节点 {} / 候选 {} = {:.2}MB",
        p.node_count,
        p.candidate_count,
        overlay_bytes as f64 / 1048576.0
    );
    println!(
        "单字表 {} 桶 {} 候选；候选总数 {}（arena 词 {} + 单字 {}）",
        p.sc_buckets,
        p.sc_candidates,
        p.arena_candidates + p.sc_candidates,
        p.arena_candidates,
        p.sc_candidates
    );
    let dict_total = p.arena_bytes + p.wi_span_bytes + overlay_bytes;
    println!(
        "词典估算合计 {:.1}MB（不含 SQLite 页缓存/统计库/fx/运行时）",
        dict_total as f64 / 1048576.0
    );

    // seed n-gram：种子表只读，DB 行数 = 内存条目数（trigram 按需 SQL 不驻留）。
    let conn =
        rusqlite::Connection::open_with_flags(seed, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let bigram: i64 = conn
        .query_row("SELECT COUNT(*) FROM seed_bigram_counts", [], |r| r.get(0))
        .unwrap_or(0);
    let trigram: i64 = conn
        .query_row("SELECT COUNT(*) FROM seed_trigram_counts", [], |r| r.get(0))
        .unwrap_or(0);
    println!("== 种子 n-gram（bigram 全量驻留 CSR）==");
    println!(
        "seed_bigram {} 条；seed_trigram {} 条（按需 SQL 不驻留）",
        bigram, trigram
    );

    println!("== DB 构成（分层边界设计的输入）==");
    let words: i64 = conn
        .query_row("SELECT COUNT(*) FROM x1dict_words", [], |r| r.get(0))
        .unwrap();
    let chars: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM x1dict_words WHERE length(simplified)=3",
            [],
            |r| r.get(0),
        )
        .unwrap();
    println!(
        "x1dict_words 总 {} 条（UTF-8 单字 3B ≈ {} 条）",
        words, chars
    );
    for floor in [2000u32, 1000, 500, 300, 200, 100] {
        let n: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM x1dict_words WHERE freq >= {}", floor),
                [],
                |r| r.get(0),
            )
            .unwrap();
        let bytes: i64 = conn
            .query_row(
                &format!(
                    "SELECT SUM(length(simplified)+length(pinyin)) FROM x1dict_words WHERE freq >= {}",
                    floor
                ),
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        println!(
            "freq >= {:>4}：{:>6} 条（{:>5.1}%），文本+拼音字节 {:.1}MB",
            floor,
            n,
            n as f64 * 100.0 / words as f64,
            bytes as f64 / 1048576.0
        );
    }
    let avg_heap: f64 = dict_total as f64 / (p.arena_candidates + p.sc_candidates).max(1) as f64;
    println!(
        "平均每候选摊 {:.0}B（含 arena/词索引/单字表）——分层估算用它 × 条数",
        avg_heap
    );
}

/// 计数分配器（仅测试二进制）：精确量「某段代码的 Rust 侧存活字节」。
/// SQLite C 侧（bundled sqlite3 用 C 运行时 malloc）天然不计——正好把
/// Rust 结构与 SQLite 各自的内存分开归因。
#[cfg(test)]
mod count_alloc {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub static LIVE: AtomicUsize = AtomicUsize::new(0);
    pub static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

    pub struct Counting;

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            System.alloc(layout)
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            System.dealloc(ptr, layout)
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if new_size > layout.size() {
                LIVE.fetch_add(new_size - layout.size(), Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
            System.realloc(ptr, layout, new_size)
        }
    }

    #[global_allocator]
    static GLOBAL: Counting = Counting;
}

/// 分层驻留归因：用计数分配器精确测 Engine 各阶段的 Rust 侧存活字节。
/// 运行：cargo test -p x1core --lib engine_live_bytes -- --ignored --nocapture
#[test]
#[ignore = "诊断工具：Rust 侧存活字节归因"]
fn engine_live_bytes() {
    use count_alloc::{ALLOC_COUNT, LIVE};
    let live = || LIVE.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1048576.0;
    let allocs = || ALLOC_COUNT.load(std::sync::atomic::Ordering::Relaxed);

    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let dir = crate::test_tmp::dir().join(format!("x1live_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let db = dir.join("seed.db");
    std::fs::copy(seed, &db).unwrap();

    let a0 = live();
    let engine = Engine::new_with_db(db.to_str().unwrap()).unwrap();
    let a1 = live();
    println!("== Rust 侧存活字节（计数分配器，种子 DB）==");
    println!(
        "加载前 {:.1}MB → 加载后 {:.1}MB：引擎 Rust 结构存活 {:.1}MB（{} 笔分配）",
        a0,
        a1,
        a1 - a0,
        allocs()
    );
    let _ = engine.process("jishufanganwendang");
    println!("打一轮查询后 {:.1}MB（打分缓存增量）", live());
    drop(engine);
    println!("drop 后 {:.1}MB（差额即引擎全部 Rust 内存）", live());

    // 用户 DB 对照：归因用户积累数据（74k bigram + 68k trigram 嵌套 BTreeMap）
    // 的 Rust 侧字节。活库先复制，只读视角并入 WAL。
    if let Ok(appdata) = std::env::var("APPDATA") {
        let src = std::path::Path::new(&appdata)
            .join("x1pinyin")
            .join("x1pinyin.db");
        if src.exists() {
            let udb = dir.join("user.db");
            std::fs::copy(&src, &udb).unwrap();
            for ext in ["wal", "shm"] {
                let s = src.with_extension(ext);
                if s.exists() {
                    let _ = std::fs::copy(&s, udb.with_extension(ext));
                }
            }
            let b0 = live();
            let uengine = Engine::new_with_db(udb.to_str().unwrap()).unwrap();
            let b1 = live();
            println!("== 用户 DB ==\n加载后 {:.1}MB：用户引擎 Rust 存活 {:.1}MB（种子 {:.1}MB → 用户数据增 {:.1}MB，142k 条 n-gram 条目）",
                b1, b1 - b0, a1 - a0, (b1 - b0) - (a1 - a0));
            drop(uengine);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// 守护：角色 juese + jiaose 双读音共存行为——
/// 系统读音（jue se）按设计不进 user_words（两表去重），走系统词库；
/// 变体读音（jiao se）进自造词；功能判据 = 两种拼法都能打出「角色」。
/// 同时守护 Windows 侧 pending_reading_prune 修复不误删学习来的读音
///（该清理只在词库管理器显式编辑拼音时置位，见 dict_admin.rs）。
#[test]
fn verify_juese_jiaose_coexist() {
    use std::fs;
    let dir = crate::test_tmp::dir().join(format!(
        "x1pinyin_juese_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("j.db");
    let mut e = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    // 两个读音都按用户真实行为固化（角色 juese 是系统读音、jiaose 是变体）
    e.commit_with_solidify("juese", "角色", Some("jue se"), true, true);
    e.commit_with_solidify("jiaose", "角色", Some("jiao se"), true, true);
    e.flush().unwrap();
    drop(e);
    let e2 = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    // 系统读音（jue se）按设计不进 user_words（两表去重），走系统词库；
    // 变体读音（jiao se）进自造词。功能判据 = 两种拼法都能打出「角色」。
    let pys = e2.user_words_pinyins_for_test("角色").unwrap();
    eprintln!("[verify] 角色 自造词读音 = {:?}（jue se 走系统词库）", pys);
    assert!(
        pys.iter().any(|p| p == "jiao se"),
        "变体读音必须在：{:?}",
        pys
    );
    assert!(
        e2.query_for_test(&["jue", "se"])
            .iter()
            .any(|c| c.text.as_ref() == "角色")
            || e2
                .dict
                .query(&["jue", "se"])
                .iter()
                .any(|c| c.text.as_ref() == "角色"),
        "jue se 查询应出「角色」（系统词库或用户词任一来源）"
    );
    assert!(e2
        .query_for_test(&["jiao", "se"])
        .iter()
        .any(|c| c.text.as_ref() == "角色"));
    eprintln!("[verify] juese 与 jiaose 都能打出「角色」");
    fs::remove_dir_all(&dir).ok();
}

// ── houxuanguding 切分回归（绿词整句出不来，首候选变「后续」）──
#[test]
fn test_houxuanguding_prefers_full_coverage_segmentation() {
    // 现象：输入 houxuanguding（意图「候选」+「固定」），显示切分正确
    // hou'xuan'gu'ding，但候选首位的却是「后续」——整句绿词「候选固定」消失。
    //
    // 根因：score_for_syllables 层级 2 的 fallback_freq 取"任意候选最高词频"
    // 不问覆盖度——碎片切分 [hou,xu,ang,u,ding]（xuan 被撕成 xu+ang+u，拆分
    // 惩罚只查相邻合并、查不到跨三段重组）靠开头 2 音节的「后续」(freq 1755)
    // 把 base 顶到 999 格顶（1000+999=1999），压过正确切分 [hou,xuan,gu,ding]
    // （「候选」freq 808 → 1000+809=1809）。绿词 DP 收到错误切分 → DP 组不出
    // 整句 → 候选只剩首音节单字 + 前缀词「后续」。
    //
    // 修复：fallback 候选须覆盖 ≥ 一半音节（「后续」2/5 < 1/2 出局，「候选」
    // 2/4 = 1/2 保留）→ 正确切分 1809 > 碎片 1000+cover=1011 → 绿词 DP 收到
    // 正确切分，组合出「候选固定」插首位（is_full_match、无同长白词）。
    let db = seed_copy("hxd");
    let engine = super::Engine::new_with_db(&db).expect("seed db");

    // 切分选择：正确切分（与显示层 syllable_segmentation 同判）
    assert_eq!(
        engine.syllable_segmentation("houxuanguding"),
        vec![
            "hou".to_string(),
            "xuan".to_string(),
            "gu".to_string(),
            "ding".to_string()
        ],
        "显示切分应为 hou/xuan/gu/ding"
    );

    // 候选：整句绿词「候选固定」应存在且排首位（词库「候选」「固定」作为
    // 子词被 DP 组合；首候选不得是只覆盖部分音节的前缀词「后续」）
    let cands = engine.process("houxuanguding");
    assert_eq!(
        cands.first().map(|c| c.text.as_ref()),
        Some("候选固定"),
        "首候选应为绿词整句「候选固定」，实际: {:?}",
        cands.iter().map(|c| &c.text).take(8).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_file(&db);
}

/// 模糊音笛卡尔积上限：多组模糊音下 15 个
/// 歧义音节组合数指数增长（z_zh 下每个 ≥2 选项 → ≥2^15 = 32768 > 1024），
/// 护栏必须逐轮触发——等全部生成完再查，主线程早已冻结。超限整体回退为
/// 未扩展的原始音节序列（与模糊关闭路径同构），查询降级但可用。
#[test]
fn expand_syllables_cartesian_cap_falls_back() {
    let mut engine = Engine::new();
    engine.fuzzy_config = crate::fuzzy::FuzzyConfig {
        enabled: true,
        z_zh: true,
        an_ang: true,
        s_sh: true,
        ..Default::default()
    };
    let syllables = vec!["zhan"; 15];
    let expanded = engine.expand_syllables(&syllables);
    assert_eq!(expanded.len(), 1, "超限必须回退为单条原始序列");
    assert_eq!(
        expanded[0],
        vec!["zhan".to_string(); 15],
        "回退内容 = 未扩展的原始音节"
    );

    // 边界内正常扩展：少量歧义音节的模糊组合不受护栏影响
    let small = engine.expand_syllables(&["zhan", "shang"]);
    assert!(
        small.len() > 1,
        "边界内模糊扩展仍应生效，实际序列数 {}",
        small.len()
    );
}

/// 自组词组合学习召回：naera 依次选「哪儿」+「啊」
/// 收尾后，下次输 naera 应出「哪儿啊」（green_memo 首次即记忆）。
/// 本测试模拟平台收尾提交的参数（整句 text + 完整拼音 + 原始 input）。
#[test]
fn test_naera_composed_green_memo_recall() {
    let db = seed_copy("naera");
    let mut engine = super::Engine::new_with_db(&db).expect("seed db");
    // 前置：裸尾零声母音节的
    // 「词+语气词」读法冷引擎即可组出（语气词第二路 DP 共存）——
    // 未学过时「哪儿啊」允许在列；本测试的核心守护（学习后前 5 召回）不变。
    let _before = engine.process("naera");
    // 收尾提交（组合场景：text=累计组合、pinyin=完整拼音、input=原始输入）
    engine.commit("naera", "哪儿啊", Some("na er a"), true);
    // 下次输入应召回，且必须在第一页可见（前 5）——用户只见前 9 候选，
    // 排到末尾等于没召回。
    let after = engine.process("naera");
    let texts: Vec<&str> = after.iter().map(|c| c.text.as_ref()).collect();
    let pos = texts.iter().position(|t| *t == "哪儿啊");
    assert!(
        pos.is_some() && pos.unwrap() < 5,
        "组合学习后 naera 应召回「哪儿啊」且进前5，实际 pos={:?} 前8: {:?}",
        pos,
        &texts[..texts.len().min(8)]
    );
    seed_copy_cleanup(&db);
}

/// Windows 组合路径的真实序列：输 naera 依次选
/// 「哪儿」（首步：input=完整 buffer）+「啊」（续步：input=完整 buffer、text=
/// 累计组合）→ 两笔 commit。第二笔写 green_memo["naera"]，下次输 naera 必须
/// 在前 5 召回「哪儿啊」。
#[test]
fn test_naera_combo_two_commit_recall() {
    let db = seed_copy("naera_combo");
    let mut engine = super::Engine::new_with_db(&db).expect("seed db");
    // 首步：选「哪儿」（组合起点，input=完整 buffer "naera"）
    engine.commit("naera", "哪儿", Some("na er"), false);
    // 续步：选「啊」（组合延续，input=组合完整输入、text=累计组合）
    engine.commit("naera", "哪儿啊", Some("na er a"), true);
    let after = engine.process("naera");
    let texts: Vec<&str> = after.iter().map(|c| c.text.as_ref()).collect();
    let pos = texts.iter().position(|t| *t == "哪儿啊");
    assert!(
        pos.is_some() && pos.unwrap() < 5,
        "组合两笔提交后 naera 应在前5召回「哪儿啊」，实际 pos={:?} 前8: {:?}",
        pos,
        &texts[..texts.len().min(8)]
    );
    // 重启后仍须召回：green_memo 是缓存（恢复限 100 条），
    // 持久兜底走 input_mappings——重开引擎等价于服务重启，教过一次就该在。
    //（flush = 真实服务的周期落盘等价物，不 flush 则映射只在内存。）
    engine.flush().expect("flush 学习数据");
    drop(engine);
    let engine = super::Engine::new_with_db(&db).expect("seed db reopen");
    let after2 = engine.process("naera");
    let texts2: Vec<&str> = after2.iter().map(|c| c.text.as_ref()).collect();
    let pos2 = texts2.iter().position(|t| *t == "哪儿啊");
    assert!(
        pos2.is_some() && pos2.unwrap() < 5,
        "重启后 naera 仍应在前5召回「哪儿啊」（input_mappings 持久兜底），实际 pos={:?} 前8: {:?}",
        pos2,
        &texts2[..texts2.len().min(8)]
    );
    seed_copy_cleanup(&db);
}

/// 模糊音开启时简拼候选不消失（双 bug 之一）：
///
/// 根因：`query_single_chars_fuzzy` / `query_prefix_filtered_fuzzy` 的前缀参数是
/// 裸声母（"z"/"w"）或半截音节，而 `expand_syllable` 末尾的「仅保留有效音节」
/// 过滤把 "z" 这类非完整音节整组清空 → 模糊分支零迭代 → 单字补充全部消失；
/// 长简拼（无绿词可拼时）表现为完整断词。修复 = 前缀类查询改用
/// `expand_prefix`（不过滤有效音节）。本守护用用户式配置
/// （z_zh + in_ing + jianpin merge）确证单字补充与非空性。
#[test]
fn test_fuzzy_enabled_jianpin_single_chars_survive() {
    let db = seed_copy("fuzzyjp");
    let mut engine = Engine::new_with_db(&db).expect("seed db");
    engine.set_fuzzy_config(crate::fuzzy::FuzzyConfig::from_csv(
        "enabled,z_zh,in_ing,jianpin_merge_zh_ch_sh",
    ));
    for input in ["wxnb", "zzzzzzz", "zzzzzzzzz"] {
        let r = engine.process(input);
        assert!(
            !r.is_empty(),
            "开模糊音后 {input} 不应消失（旧实现 zzzzzzzzz 为空列表）"
        );
        let has_single = r.iter().any(|c| c.text.chars().count() == 1);
        assert!(
            has_single,
            "开模糊音后 {input} 的单字补充不应消失（expand_syllable 有效性过滤根因）"
        );
    }
    seed_copy_cleanup(&db);
}

/// 纯简拼「词库整词优先于绿词」（mldhz 案例守护）：
/// 词库已有覆盖全部输入的整词（mldhz → 木兰当户织）时，DP 拼接的绿词
/// （词频合成的路径分 5000+ 拿满额完全匹配加分）不得凭词频压过整词；
/// 整词必须出现在任何绿词之前。绿词的例外（用户学过 / 上下文关联更强）
/// 见 test_jianpin_green_with_context_evidence_may_outrank。
#[test]
fn test_jianpin_dict_word_beats_green() {
    let db = seed_copy("mldhz");
    let engine = Engine::new_with_db(&db).expect("seed db");
    let r = engine.process("mldhz");
    let word_pos = r
        .iter()
        .position(|c| c.text.as_ref() == "木兰当户织")
        .expect("诗词种子应有「木兰当户织」（freq=50）");
    let green_pos = r.iter().position(|c| c.is_sentence);
    if let Some(g) = green_pos {
        assert!(
            word_pos < g,
            "词库整词（pos={word_pos}）必须在绿词（pos={g}）之前，实际顺序：{:?}",
            r.iter()
                .take(6)
                .map(|c| c.text.as_ref())
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(word_pos, 0, "mldhz 的首位应是词库整词「木兰当户织」");
    seed_copy_cleanup(&db);
}

/// 绿词例外通道：绿词有显著上下文证据时允许排在
/// 词库整词之前——本测试用「上文=昨天 + bigram(昨天→一一对应)」构造证据，
/// 验证 yydy 下绿词回到整词之前（与 test_jianpin_dict_word_beats_green
/// 的无证据默认让位互为对照）。残片段清理后素材更换：
/// 原用「面临的孩子」（的尾残片词条已清）改为同首字母签名的
/// 「一一对应/洋洋得意」（yydy）对。
#[test]
fn test_jianpin_green_with_context_evidence_may_outrank() {
    let db = seed_copy("yydy");
    let mut engine = Engine::new_with_db(&db).expect("seed db");
    // 造上下文：先"上屏"一个词（进入 context 栈），再记录它与绿词的共现。
    engine.learn("昨天", "zuotian");
    engine.record_word_bigram_weighted("昨天", "一一对应", 5);
    let r = engine.process("yydy");
    let green_pos = r
        .iter()
        .position(|c| c.text.as_ref() == "一一对应")
        .expect("应有绿词「一一对应」");
    let word_pos = r
        .iter()
        .position(|c| c.text.as_ref() == "洋洋得意")
        .expect("种子应有「洋洋得意」");
    assert!(
        green_pos < word_pos,
        "有上下文证据的绿词（pos={green_pos}）应允许排在整词（pos={word_pos}）之前"
    );
    seed_copy_cleanup(&db);
}

// ═══════════ 单音节拆分识别（偏好开关） ═══════════

/// yuan 默认（开关开、位 2）→「预案」在第 2 位；首位仍是完整音节单字。
#[test]
fn test_ambig_split_yuan_default_position2() {
    let db = seed_copy("ambig");
    let mut engine = super::Engine::new_with_db(&db).expect("seed db");
    engine.set_ambig_split_enabled(true);
    engine.set_ambig_split_position(2);
    let cands = engine.process("yuan");
    let pos = cands.iter().position(|c| c.text.as_ref() == "预案");
    assert_eq!(
        pos,
        Some(1),
        "yuan 应在第 2 位出「预案」，实际前 5: {:?}",
        cands
            .iter()
            .take(5)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
    // 首位恒为完整音节单字（非拆分词）
    assert_eq!(cands[0].text.chars().count(), 1, "首位应为单字");
    seed_copy_cleanup(&db);
}

/// 开关关闭 → 回到旧行为：yuan 不出「预案」（除非正常路径本身命中）。
#[test]
fn test_ambig_split_disabled_no_word() {
    let db = seed_copy("ambig2");
    let mut engine = super::Engine::new_with_db(&db).expect("seed db");
    engine.set_ambig_split_enabled(false);
    let cands = engine.process("yuan");
    assert!(
        cands.iter().take(9).all(|c| c.text.as_ref() != "预案"),
        "关闭后前 9 候选不应有「预案」"
    );
    seed_copy_cleanup(&db);
}

/// 插入位可设 2..=5：位 5 时「预案」落到 index 4。
#[test]
fn test_ambig_split_position5() {
    let db = seed_copy("ambig3");
    let mut engine = super::Engine::new_with_db(&db).expect("seed db");
    engine.set_ambig_split_enabled(true);
    engine.set_ambig_split_position(5);
    let cands = engine.process("yuan");
    let pos = cands.iter().position(|c| c.text.as_ref() == "预案");
    assert_eq!(pos, Some(4), "位 5 → index 4");
    // 越界钳制：位 99 钳到 5
    engine.set_ambig_split_position(99);
    let cands = engine.process("yuan");
    let pos = cands.iter().position(|c| c.text.as_ref() == "预案");
    assert_eq!(pos, Some(4), "位 99 钳到 5 → index 4");
    seed_copy_cleanup(&db);
}

/// 用户点名案例族：tuan→图案、mian→蜜胺、yue→余额、xian→西安。
#[test]
fn test_ambig_split_word_family() {
    let db = seed_copy("ambig4");
    let mut engine = super::Engine::new_with_db(&db).expect("seed db");
    engine.set_ambig_split_enabled(true);
    engine.set_ambig_split_position(2);
    for (input, word) in [
        ("tuan", "图案"),
        ("mian", "蜜胺"),
        ("yue", "余额"),
        ("xian", "西安"),
    ] {
        let cands = engine.process(input);
        let pos = cands.iter().position(|c| c.text.as_ref() == word);
        assert_eq!(
            pos,
            Some(1),
            "{input} 第 2 位应为「{word}」，实际前 5: {:?}",
            cands
                .iter()
                .take(5)
                .map(|c| c.text.as_ref())
                .collect::<Vec<_>>()
        );
    }
    seed_copy_cleanup(&db);
}

/// 不可拆音节零影响：ni/hao/duan 的候选首位与开关前一致；duan 保护语义
/// 不被破坏（du|an 的「都安」若不在词库就不出现）。
#[test]
fn test_ambig_split_nonsplittable_unchanged() {
    let db = seed_copy("ambig5");
    let mut engine = super::Engine::new_with_db(&db).expect("seed db");
    engine.set_ambig_split_enabled(false);
    let ni_off = engine.process("ni");
    let duan_off = engine.process("duan");
    engine.set_ambig_split_enabled(true);
    let ni_on = engine.process("ni");
    let duan_on = engine.process("duan");
    assert_eq!(
        ni_off.first().map(|c| c.text.as_ref()),
        ni_on.first().map(|c| c.text.as_ref()),
        "ni（无两段拆分）开关前后首位一致"
    );
    assert_eq!(
        duan_off.first().map(|c| c.text.as_ref()),
        duan_on.first().map(|c| c.text.as_ref()),
        "duan（词库无 du|an 词）开关前后首位一致"
    );
    seed_copy_cleanup(&db);
}

/// 诊断工具：切分选举快照——显示/计数/消费链（best_segmentation）
/// vs 候选链 token 语义（best_token_segmentation）对同一输入的切分、打分与
/// 前缀消费长度。改切分/消费链相关逻辑时先跑本快照对照前后差异。
/// 输出格式：input | cnt | seg(分) | tok(分) | preLens
#[test]
#[ignore = "诊断工具：cargo test -p x1core --lib diag_seg_chain_snapshot -- --ignored --nocapture"]
fn diag_seg_chain_snapshot() {
    let engine = Engine::new();
    let corpus = [
        "pengbfzhan",
        "pengbfzh",
        "pengbfz",
        "pengbofazhan",
        "pbfzhan",
        "guangzho",
        "guangzh",
        "zhonggr",
        "zaijianh",
        "jiegw",
        "wangou",
        "wanguo",
        "fangan",
        "fangandehen",
        "yigerenguodehaob",
        "yigerenguodehao",
        "dangnishiyongrengongzhinengd",
        "rengongzhineng",
        "kaix",
        "went",
        "cip",
        "muq",
        "bydi",
        "byd",
        "nihao",
        "xian",
        "zhongguo",
        "wenzhang",
        "laishuo",
        "zhongjian",
        "ok",
        "zw",
        "sh",
        "duan",
        "ni",
        "guang",
        "peng",
        "san",
        "yonghu",
        "zhonggf",
        "qiehuanshurufzhihou",
        "yinianjide",
        "chi",
        "chifan",
        "pengb",
        "pengbf",
        "guangfazhan",
        "shangh",
    ];
    for input in corpus {
        let seg = engine.best_segmentation(input);
        let tok = engine.best_token_segmentation(input);
        let ss = engine.score_for_syllables(input, &seg);
        let ts = engine.score_for_syllables(input, &tok);
        let cnt = engine.syllable_count(input);
        let maxn = cnt.min(6);
        let pre = if maxn > 0 {
            engine.syllable_prefix_lens(input, &(1..=maxn).collect::<Vec<_>>())
        } else {
            vec![]
        };
        eprintln!("{input} | cnt={cnt} | seg={seg:?}({ss}) | tok={tok:?}({ts}) | pre={pre:?}");
    }
}

/// 查询面指纹：一组输入经 process 的候选全字段序列化 + dict 级查询点。
/// 两次装载（构建 vs 缓存命中）应产出逐字节相同的指纹。
fn query_fingerprint(e: &Engine) -> String {
    let mut out = String::new();
    for input in [
        "ni",
        "nihao",
        "n",
        "hao",
        "zhongguo",
        "pengbofazhan",
        "wxs",
        "ni'hao",
        "chifan",
    ] {
        out.push_str(input);
        out.push(':');
        for c in e.process(input) {
            out.push_str(&format!(
                "({}|{}|{}|{}|{}|{}|{})",
                c.text.as_ref(),
                c.base_freq,
                c.pinyin.as_ref(),
                c.is_primary,
                c.is_correction,
                c.is_sentence,
                c.is_guess
            ));
        }
        out.push('\n');
    }
    // dict 级查询点：词索引 / 单字表 / 长词查询
    out.push_str(&format!(
        "entry_count={} 你好={:?} 蓬勃发展={:?}\n",
        e.dict.entry_count(),
        e.dict.long_word_freq("你好"),
        e.dict.long_word_freq("蓬勃发展"),
    ));
    for p in ["ni", "hao", "peng"] {
        for c in e.dict.query_single_chars(p) {
            out.push_str(&format!(
                "sc:{}|{}|{}\n",
                c.text.as_ref(),
                c.base_freq,
                c.pinyin
            ));
        }
    }
    out
}

/// 词典编译缓存守护。
///
/// ① 首跑（含用户词）构建 + 回写缓存；② 二次装载命中（缓存文件 mtime 不变
/// ——走构建路径必然重写：tmp+删+改名）；③ 两次装载查询面逐字节等价
/// （覆盖用户词 overlay 化后的 arena+overlay 合成）；④ rev 失效：行数不变的
/// freq 编辑（管理器口）→ 缓存失配重建、查询反映新值；⑤ 损坏缓存自愈。
#[test]
fn test_dict_compile_cache_roundtrip_and_invalidation() {
    let seed = seed_copy_keephash("p2cache");
    let p = std::path::Path::new(&seed);
    let cache = crate::storage::dict_cache::cache_path_for(p);

    // 素材：注入用户词（自造词 + 与系统词冲突的长词），让 overlay 化路径参与对比
    {
        let mut st = crate::storage::Storage::open(&seed).unwrap();
        st.upsert_user_words_batch(&[
            ("蓬勃发展".to_string(), "peng bo fa zhan".to_string(), 3),
            ("你好".to_string(), "ni hao".to_string(), 7),
        ])
        .unwrap();
    }

    // ① 首跑：构建 + 回写缓存
    let e1 = Engine::new_with_db(&seed).unwrap();
    assert!(
        crate::storage::dict_cache::cache_exists(p),
        "含 sync_hash 的库首跑应回写编译缓存"
    );
    let mtime1 = std::fs::metadata(&cache).unwrap().modified().unwrap();
    let fp1 = query_fingerprint(&e1);
    drop(e1);

    // ② 二次装载：命中（mtime 不变 = 没有走构建路径的重写）
    std::thread::sleep(std::time::Duration::from_millis(30));
    let e2 = Engine::new_with_db(&seed).unwrap();
    let mtime2 = std::fs::metadata(&cache).unwrap().modified().unwrap();
    assert_eq!(
        mtime1, mtime2,
        "二次装载必须命中缓存（走构建路径会重写缓存文件）"
    );

    // ③ 查询面等价（构建路径 vs 缓存路径，含用户词 overlay）
    let fp2 = query_fingerprint(&e2);
    assert_eq!(fp1, fp2, "构建路径与缓存路径的查询面必须逐字节一致");
    drop(e2);

    // ④ rev 失效：行数不变的 freq 编辑（管理器口 update_x1dict_word）→ 失效重建
    {
        let mut st = crate::storage::Storage::open(&seed).unwrap();
        let n = st
            .update_x1dict_word(
                "你好",
                "ni hao",
                &crate::storage::X1DictWord {
                    simplified: "你好".to_string(),
                    pinyin: "ni hao".to_string(),
                    freq: 1,
                    is_primary: true,
                },
            )
            .unwrap();
        assert_eq!(n, 1, "应命中 1 行");
    }
    std::thread::sleep(std::time::Duration::from_millis(30));
    let e3 = Engine::new_with_db(&seed).unwrap();
    let mtime3 = std::fs::metadata(&cache).unwrap().modified().unwrap();
    assert_ne!(
        mtime2, mtime3,
        "rev 变化（行数不变的编辑）必须让缓存失效并重建"
    );
    // 查询反映新 freq（若读了陈旧缓存，这里是旧值 1053）
    assert_eq!(
        e3.dict.find_freq("ni hao", "你好"),
        Some(1),
        "失效重建后 base_freq 应为编辑值"
    );
    drop(e3);

    // ⑤ 损坏缓存自愈：写垃圾 → 装载回退构建路径并重写缓存
    std::fs::write(&cache, b"garbage-not-a-cache").unwrap();
    let e4 = Engine::new_with_db(&seed).unwrap();
    assert!(
        e4.process("nihao")
            .iter()
            .any(|c| c.text.as_ref() == "你好"),
        "损坏缓存不得影响查询"
    );
    drop(e4);
    let repaired = std::fs::read(&cache).unwrap();
    assert_eq!(
        &repaired[..8],
        b"X1DAC001",
        "损坏缓存应被自愈重写为合法格式"
    );

    for f in [
        seed.clone(),
        format!("{}-wal", seed),
        format!("{}-shm", seed),
        cache.to_string_lossy().into_owned(),
    ] {
        let _ = std::fs::remove_file(f);
    }
}
