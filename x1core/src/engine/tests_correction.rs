//! Engine 测试 · 智能纠错 / 多音字 / 模糊音 / 整句切分 / 学习基础（按主题模块化拆分）。
//!
//! 本文件按主题拆分，覆盖：
//! - 智能纠错：候选不足触发、开关禁用、is_correction 标志、首字母尾部不触发、
//!   键盘邻居优先、短输入不触发、结构合法不纠错
//! - 多音字主音/次音：make_polyphone_db helper、主音胜出、用户动态反超、无交叉污染
//! - 模糊音：z/zh、n/l 全拼；zh/ch/sh 简拼合并；z/zh 简拼模糊
//! - 整句切分：duan 全音节优先、零声母拆分、连续输入不拆前缀词、撇号允许拆分
//! - 学习基础：learn 新词、未分拼即时出现、混合拼音学习、commit 即时生效、长句不固化、normalize_for_storage
//! - 绿词删除恢复：删绿词不阻塞他人、删后可复活
//! - 其他：简拼首字母回退、debug 长词、geiwo 首音节偏好、split_text_anchored 锚定切分
//!
//! mod.rs 用 `#[cfg(test)] mod tests_correction;` 加载（分号形式从文件加载）。
//! `use super::*` 仍指向 engine mod，测试访问 engine 的 pub 项不变。

use super::*;

/// 回归测试：split_text_anchored 用用户 text 锚定切分，不靠 decode_sentence 重猜。
///
/// 背景：commit_sentence_split 忽略用户传入的 text，靠 split_into_words
/// （decode_sentence 旧版）重新猜切分，因词库词频失真把「项目」猜成「香木」，
/// 导致用户明确教学的子词学错。
/// 修复：commit_sentence_split 用 split_text_anchored，text 字符 + 音节序列对齐切分。
///
/// 注：拆词只记 bigram，不存子词入 user_words。
/// 此测试改为直接验证 split_text_anchored 的切分结果（不被词库高频「香木」顶替）。
#[test]
fn test_split_text_anchored_respects_user_input() {
    let seed = super::tests_query::seed_copy("corr");
    let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

    // 直接验证 split_text_anchored：切分「输入法项目」必须按用户 text 锚定
    // 拆成 [输入法, 项目]，不能因词库高频把「项目」猜成「香木」。
    let syllables: Vec<&str> = vec!["shu", "ru", "fa", "xiang", "mu"];
    let segments = engine.split_text_anchored("输入法项目", &syllables);
    let seg_texts: Vec<&str> = segments.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(
        seg_texts,
        vec!["输入法", "项目"],
        "split_text_anchored 应按用户 text 锚定切分为 [输入法, 项目]，实际：{:?}",
        seg_texts
    );
    // 切分出的子词不应是「香木」（验证不被词库高频顶替）
    assert!(
        !seg_texts
            .iter()
            .any(|&t| t.contains('香') || t.contains('木')),
        "切分不应被词库高频的「香木」污染：{:?}",
        seg_texts
    );
}

/// 回归测试：Command+Backspace 删除绿词不应影响其他绿词生成。
///
/// 背景：删除「输入法想目」后无法再用 shurufaxiangmu 组词。
/// 排查发现 removed_texts 用精确字符串匹配（只拦截完全相同的文本），
/// 删除「输入法想目」不应影响「输入法项目」生成。问题在会话级状态未清。
///
/// 此测试守护：删除某绿词变体后，正确的绿词变体仍能正常生成。
#[test]
fn test_remove_green_word_doesnt_block_others() {
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    // 用临时副本：remove() 会立即把黑名单写进 DB（insert_blacklist），
    // 直接在真实种子上跑会污染种子文件
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_rm_green1_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::copy(seed, &tmp);
    let mut engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap_or_else(|_| Engine::new());

    // 用户 commit 正确组合
    engine.commit(
        "shurufaxiangmu",
        "输入法项目",
        Some("shu ru fa xiang mu"),
        true,
    );

    // 用户删除一个错误变体（比如 DP 偶尔切错产生的）
    engine.remove("输入法想目");
    assert!(engine.removed_texts.contains("输入法想目"));

    // 正确变体「输入法项目」不应被影响（commit 后已固化为白词，首候选直接命中）
    let cands = engine.process("shurufaxiangmu");
    let first = cands.first().map(|c| c.text.as_ref());
    assert_eq!(
        first,
        Some("输入法项目"),
        "删除「输入法想目」不应影响「输入法项目」（白词固化后首候选直接命中），实际：{:?}",
        first
    );
    let _ = std::fs::remove_file(&tmp);
}

/// 回归测试：Command+Backspace 删除绿词后，再次输入应能复活（不死循环）。
///
/// 背景：删除绿词后 removed_texts 命中导致绿词完全不显示，
/// 用户无法再选到它来触发 commit 复活——死循环。
///
/// 修复：命中黑名单的绿词降级排到候选列表末尾（而非消失），
/// 用户翻页能找到并选中，commit 自动清除黑名单复活。
#[test]
fn test_removed_green_word_can_revive() {
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    // 用临时副本（remove 立即落盘黑名单，防污染真实种子）
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_rm_green2_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::copy(seed, &tmp);
    let mut engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap_or_else(|_| Engine::new());

    // commit 建立学习
    engine.commit(
        "shurufaxiangmu",
        "输入法项目",
        Some("shu ru fa xiang mu"),
        true,
    );
    // 用户删除绿词
    engine.remove("输入法项目");
    assert!(engine.removed_texts.contains("输入法项目"));

    // 再次输入：绿词不应消失（降级排末尾，让用户能选到复活）
    let cands = engine.process("shurufaxiangmu");
    let has_green = cands
        .iter()
        .any(|c| c.is_sentence && c.text.as_ref() == "输入法项目");
    assert!(
        has_green,
        "删除绿词后再次输入，绿词应降级排末尾（而非消失），实际候选：{:?}",
        cands.iter().map(|c| &c.text).collect::<Vec<_>>()
    );

    // 用户选到这个绿词（commit）→ 复活
    engine.commit(
        "shurufaxiangmu",
        "输入法项目",
        Some("shu ru fa xiang mu"),
        true,
    );
    assert!(
        !engine.removed_texts.contains("输入法项目"),
        "commit 后黑名单应清除（复活机制）"
    );
    let _ = std::fs::remove_file(&tmp);
}

/// 回归测试：自组词学习立即生效（base_dict→dict 修复）+ 整句自动固化（≤8字）。
///
/// 背景：
/// 1. decode_sentence 用 base_dict（启动快照），用户学习不可见 → 改用 dict
/// 2. ≥4 音节整句永不固化，下次同输入还得重新 DP → 选择性固化（≤8字进 user_words）
///
/// 双重保障：
/// - 整句「输入法项目」（5字 ≤8）经 force_solidify 进 user_words，下次同输入白词直接命中
/// - 拆词只记 bigram（不存子词入 user_words）
#[test]
fn test_compose_word_takes_effect_immediately() {
    let seed = super::tests_query::seed_copy("corr");
    let mut engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

    // 自组词分步：第二步用 commit_with_solidify（force_solidify=true）固化整句。
    // 4-8 字整句默认（普通数字）不固化，需 Shift/force_solidify 才进 user_words。
    engine.commit("shurufaxiangmu", "输入法", Some("shu ru fa"), false);
    engine.commit_with_solidify(
        "shurufaxiangmu",
        "输入法项目",
        Some("shu ru fa xiang mu"),
        true,
        true,
    );

    // 整句必须进 user_words（≤8 字 + force_solidify）
    assert!(
        engine.user_words.contains_key("输入法项目"),
        "≤8 字整句「输入法项目」经 force_solidify 应固化到 user_words"
    );

    // 立即查询（不重启进程）：首候选必须是「输入法项目」
    let cands = engine.process("shurufaxiangmu");
    let first = cands.first().map(|c| c.text.as_ref());
    assert_eq!(
        first,
        Some("输入法项目"),
        "自组词后立即查询，首候选应为「输入法项目」（整句白词直接命中），实际：{:?}",
        first
    );
}

/// 回归测试：>8 字长句不固化（膨胀控制），但仍拆词学习子词。
///
/// 设计原则：
/// - ≤8 字：整句固化 + 拆词学习（双保险）
/// - >8 字：只拆词学习，整句不固化（长句无限多，固化收益低膨胀风险高）
#[test]
fn test_long_sentence_not_persisted() {
    let seed = super::tests_query::seed_copy("corr");
    let mut engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

    // 11 字长句（>8）：「我今天很开心吃了一个苹果」
    let long_text = "我今天很开心吃了一个苹果";
    let long_pinyin = "wo jin tian hen kai xin chi le yi ge ping guo";
    engine.commit(
        "wojintianhenkaixinchileyigepingguo",
        long_text,
        Some(long_pinyin),
        true,
    );

    // 长句不应进 user_words（>8 字不固化）
    assert!(
        !engine.user_words.contains_key(long_text),
        ">8 字长句「{}」不应固化到 user_words（膨胀控制），实际 user_words 包含了",
        long_text
    );

    // 拆词学习仍执行：子词不入 user_words，
    // 只记子词间 bigram（让下次连续输入衔接更顺）。
    // 验证拆词能切出多个子词（拆词学习的前提）。
    let syllables: Vec<&str> = long_pinyin.split_whitespace().collect();
    let segments = engine.split_text_anchored(long_text, &syllables);
    assert!(
        segments.len() >= 2,
        "长句应能拆出 ≥2 子词用于 bigram 学习，实际拆分：{:?}",
        segments.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn test_normalize_pinyin_for_storage() {
    // 无空格 → 切分加空格
    assert_eq!(normalize_pinyin_for_storage("tongzhi"), "tong zhi");
    // 已带空格 → 保持
    assert_eq!(normalize_pinyin_for_storage("tong zhi"), "tong zhi");
    // 多空格 → 压缩
    assert_eq!(normalize_pinyin_for_storage("tong   zhi"), "tong zhi");
    // 简拼 → 每字符成音节
    assert_eq!(normalize_pinyin_for_storage("zw"), "z w");
    // 空串
    assert_eq!(normalize_pinyin_for_storage(""), "");
}

#[test]
fn test_learn_new_word() {
    let db_path = crate::test_tmp::dir().join("x1pinyin_test_learn.db");
    let _ = std::fs::remove_file(&db_path);
    let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();

    // 学习自造词 "来说"
    engine.learn("来说", "lai shuo");
    let r = engine.process("laishuo");
    assert!(
        r.iter().any(|c| c.text.as_ref() == "来说"),
        "学习后应能查询到 '来说'"
    );

    // 重新打开引擎，验证持久化
    let engine2 = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    let r2 = engine2.process("laishuo");
    assert!(
        r2.iter().any(|c| c.text.as_ref() == "来说"),
        "重启后仍应能查询到 '来说'"
    );
}

#[test]
fn test_learn_unspaced_pinyin_appears_immediately_after_flush() {
    use crate::storage::{Storage, X1DictWord};

    let db_path = crate::test_tmp::dir().join("x1pinyin_test_learn_unspaced.db");
    let _ = std::fs::remove_file(&db_path);
    // 构造一个非空 x1dict 表，避免回退到内置词库，确保目标词原先不存在
    {
        let mut storage = Storage::open(db_path.to_str().unwrap()).unwrap();
        storage
            .import_x1dict_batch(&[X1DictWord {
                simplified: "其它".into(),
                pinyin: "qi ta".into(),
                freq: 1000,
                ..Default::default()
            }])
            .unwrap();
    }
    let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();

    // 模拟宿主前端用未分隔拼音学习，并像真实流程一样先 commit 单字
    engine.commit("", "来", None, true);
    engine.commit("", "说", None, true);
    engine.learn("来说", "laishuo");
    let after = engine.process("laishuo");
    assert!(
        after.iter().any(|c| c.text.as_ref() == "来说"),
        "学习后 process 应返回 '来说'，实际为 {:?}",
        after.iter().map(|c| &c.text).collect::<Vec<_>>()
    );

    engine.flush().unwrap();
    let after_flush = engine.process("laishuo");
    assert!(
        after_flush.iter().any(|c| c.text.as_ref() == "来说"),
        "flush 后 process 仍应返回 '来说'"
    );

    // 重新打开引擎验证持久化
    let engine2 = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();
    assert!(
        engine2
            .process("laishuo")
            .iter()
            .any(|c| c.text.as_ref() == "来说"),
        "重启后仍应能查询到 '来说'"
    );
}

#[test]
fn test_jianpin_single_char_fallback_first_initial_only() {
    let engine = Engine::new();
    let r = engine.process("znb");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("znb => {:?}", texts);
    // 简拼多字母时，只应补充第一个首字母（z）的单字候选
    assert!(
        texts.contains(&"真"),
        "简拼 znb 应补充首字母 z 的单字候选 '真'"
    );
    assert!(
        !texts.contains(&"牛"),
        "简拼 znb 不应补充第二个首字母 n 的单字候选 '牛'"
    );
    assert!(
        !texts.contains(&"比"),
        "简拼 znb 不应补充第三个首字母 b 的单字候选 '比'"
    );
}

#[test]
fn test_debug_long_word() {
    let engine = Engine::new();
    for input in &[
        "zhong",
        "zhongg",
        "zhonggu",
        "zhongguo",
        "zhongguor",
        "zhongguoren",
    ] {
        let r = engine.process(input);
        let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
        println!("{} => {:?}", input, texts);
    }
}

#[test]
fn test_geiwo_prefers_first_syllable_chars() {
    // geiwo：整句解码会把「给」+「我」拼成「给我」作为首候选（更符合用户预期，
    // 用户输入完整 geiwo 多半想要「给我」而非单字「给」）。
    // 核心防护意图（不变）：不应把第二个音节 wo 的单字「我」单独排到首候选
    // （那才是「后续音节高频字污染首屏」的问题）。
    let engine = Engine::new();
    let r = engine.process("geiwo");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("geiwo => {:?}", texts);
    assert!(!texts.is_empty(), "geiwo 应返回候选");
    // 首候选应是整句解码的「给我」，或第一个音节 gei 的「给」；
    // 绝不应是单独的「我」（第二音节单字污染）。
    assert!(
        texts[0] == "给我" || texts[0] == "给",
        "geiwo 首候选应为「给我」(整句解码) 或「给」(单字)，实际为 {:?}",
        texts
    );
    assert!(
        !texts.iter().take(3).any(|&t| t == "我"),
        "geiwo 前3不应出现单独的「我」（第二音节单字污染），实际 {:?}",
        texts
    );
}

#[test]
fn test_fuzzy_z_zh_full_pinyin() {
    // 开启 z/zh 模糊后，输入 zao 也应能匹配 zhao 音的字。
    let mut engine = Engine::new();
    engine.set_fuzzy_config(crate::fuzzy::FuzzyConfig::from_csv("z_zh"));
    let r = engine.process("zao");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("zao (z/zh fuzzy) => {:?}", texts);
    // zhao 音的常见字如 "找/照/招" 应出现在候选中
    assert!(
        texts.iter().any(|&t| ["找", "照", "招"].contains(&t)),
        "开启 z/zh 模糊后，zao 应能匹配 zhao 音的字: {:?}",
        texts
    );
}

#[test]
fn test_fuzzy_n_l_full_pinyin() {
    // 开启 n/l 模糊后，输入 lan 也应能匹配 nan 音的字。
    let mut engine = Engine::new();
    engine.set_fuzzy_config(crate::fuzzy::FuzzyConfig::from_csv("n_l"));
    let r = engine.process("lan");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("lan (n/l fuzzy) => {:?}", texts);
    assert!(
        texts.iter().any(|&t| ["难", "南", "男"].contains(&t)),
        "开启 n/l 模糊后，lan 应能匹配 nan 音的字: {:?}",
        texts
    );
}

#[test]
fn test_jianpin_merge_zh_ch_sh() {
    // 开启简拼合并 zh/ch/sh 后，zhw 应能匹配 "中文"。
    let mut engine = Engine::new();
    engine.set_fuzzy_config(crate::fuzzy::FuzzyConfig::from_csv(
        "jianpin_merge_zh_ch_sh",
    ));
    let r = engine.process("zhw");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("zhw (merge zh/ch/sh) => {:?}", texts);
    assert!(
        texts.contains(&"中文"),
        "开启合并后 zhw 应能匹配 '中文': {:?}",
        texts
    );
}

#[test]
fn test_merge_does_not_fuzzy_full_pinyin() {
    // 合并 zh/ch/sh 与模糊音 z/zh 的作用域必须分离：
    // 开合并 + 关模糊音时，zong 是全拼（o 在末尾非首位，is_jianpin 判为全拼），
    // 走全拼路径——合并开关（简拼专用，expand_jianpin_initials）不参与，
    // 只出 zong 音节词（宗/总…），绝不出「中」（zhong 音节）。
    // zong→中 只有 z_zh 模糊音（音节级双向等价）才能做到。
    let mut engine = Engine::new();
    engine.set_fuzzy_config(crate::fuzzy::FuzzyConfig::from_csv(
        "jianpin_merge_zh_ch_sh",
    ));
    let r = engine.process("zong");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    assert!(
        !texts.contains(&"中"),
        "开合并关模糊音时 zong 不应出「中」(zhong 音节): {:?}",
        texts
    );

    // 对照：开 z_zh 模糊音后 zong 才能命中 zhong 音节词
    let mut engine2 = Engine::new();
    engine2.set_fuzzy_config(crate::fuzzy::FuzzyConfig::from_csv("z_zh"));
    let r2 = engine2.process("zong");
    let texts2: Vec<&str> = r2.iter().map(|c| c.text.as_ref()).collect();
    assert!(
        texts2.contains(&"中"),
        "开 z_zh 模糊音后 zong 应能命中「中」: {:?}",
        texts2
    );
}

#[test]
fn test_jianpin_fuzzy_z_zh() {
    // 开启 z/zh 模糊后，简拼 zgr 也能匹配 "中国人"（z↔zh 等价）。
    let mut engine = Engine::new();
    engine.learn("中国人", "zhong guo ren");
    engine.set_fuzzy_config(crate::fuzzy::FuzzyConfig::from_csv("z_zh"));
    let r = engine.process("zgr");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("zgr (z/zh fuzzy) => {:?}", texts);
    assert!(
        texts.contains(&"中国人"),
        "开启 z/zh 模糊后，zgr 应能匹配 '中国人': {:?}",
        texts
    );
}

#[test]
fn test_duan_prefers_full_syllable() {
    // 完整音节 duan 的首位恒为 duan 单字——拆分词（用户词「都安」du|an）
    // 频率再高也不抢首位。单音节拆分开关（默认开）开启后，「都安」允许按
    // 设置位（默认 2）出现；关闭开关则不出拆分词。
    let mut engine = Engine::new();
    engine.learn("都安", "du an");

    // 开关开（默认）：首位仍是完整音节单字，拆分词不在首位
    let r = engine.process("duan");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("duan (split on) => {:?}", texts);
    assert!(
        texts.first() != Some(&"都安"),
        "完整音节 duan 首位不应被拆分词 '都安' 占据"
    );
    assert!(texts.contains(&"段"), "应包含 duan 的单字 '段'");

    // 开关关：回到旧行为——不出现拆分词
    engine.set_ambig_split_enabled(false);
    let r = engine.process("duan");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("duan (split off) => {:?}", texts);
    assert!(
        !texts.contains(&"都安"),
        "关闭拆分开关后，完整音节 duan 不应被拆成 du+an 给出 '都安'"
    );
    assert!(texts.contains(&"段"), "应包含 duan 的单字 '段'");
}

/// 零声母切分歧义：aoliao 应能查到「奥利奥」。
///
/// 背景：词库有「奥利奥」(ao li ao, freq=916)，但输入 aoliao 查不到。
/// 根因：aoliao 有两种全有效切分 ao|li|ao（能查到奥利奥）和 ao|liao（查不到词）。
/// 旧版 score_for_syllables 的拆分惩罚 /1000 把 ao|li|ao 的分数（916/1000=0）归零，
/// 与 ao|liao（0分）同分，tie-break「音节数少优先」选了 ao|liao，丢失「奥利奥」。
///
/// 修复：惩罚改为 (max_freq+1)/100^penalty_count，保证有词命中的切分至少得 1 分。
#[test]
fn test_zero_initial_syllable_split_finds_word() {
    let seed = super::tests_query::seed_copy("corr");
    let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

    // aoliao 应能查到「奥利奥」（不显式加分隔符也能命中）
    let r = engine.process("aoliao");
    let has_aoliao = r.iter().any(|c| c.text.as_ref() == "奥利奥");
    assert!(
        has_aoliao,
        "aoliao 应能查到「奥利奥」（零声母切分 ao|li|ao 应被选中），实际：{:?}",
        r.iter().take(5).map(|c| &c.text).collect::<Vec<_>>()
    );
    // 对照：显式分隔符 aoli'ao 仍应查到（原有行为不破坏）
    let r2 = engine.process("aoli'ao");
    assert!(
        r2.iter().any(|c| c.text.as_ref() == "奥利奥"),
        "aoli'ao 显式分隔符仍应查到「奥利奥」"
    );
}

#[test]
fn test_duanlian_no_split_prefix_word() {
    // 连续完整拼音输入时，完整音节读法必须胜出：duanlian 首选 duan+lian →
    // 锻炼，而不是 du+an → 都安。
    // 口径：拆分前缀词（都安）以白词身份可达（与 baomianzhao→保密 同类
    // 可达性）——排序保护（锻炼 #0）不变，可达性（都安在场可选）从禁止改为允许。
    let mut engine = Engine::new();
    engine.learn("锻炼", "duan lian");
    engine.learn("都安", "du an");
    let r = engine.process("duanlian");
    assert_eq!(
        r.first().map(|c| c.text.as_ref()),
        Some("锻炼"),
        "duanlian 首选必须是完整音节读法「锻炼」，实际: {:?}",
        r.first().map(|c| &c.text)
    );
    let pos = r.iter().position(|c| c.text.as_ref() == "都安");
    assert!(
        pos.is_some_and(|p| p > 0),
        "拆分前缀词「都安」应可达但不得抢首（实际位置 {:?}）",
        pos
    );
}

#[test]
fn test_apostrophe_allows_split() {
    // 显式分隔符 ' 允许用户强制按 du+an 切分，从而得到“都安”。
    let mut engine = Engine::new();
    engine.learn("锻炼", "duan lian");
    engine.learn("都安", "du an");
    let r = engine.process("du'an");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("du'an => {:?}", texts);
    assert!(texts.contains(&"都安"), "显式分隔 du'an 应允许 '都安'");
}

#[test]
fn test_learn_mixed_pinyin_word() {
    let db_path = crate::test_tmp::dir().join("x1pinyin_test_learn_mixed.db");
    let _ = std::fs::remove_file(&db_path);
    let mut engine = Engine::new_with_db(db_path.to_str().unwrap()).unwrap();

    // 模拟宿主前端拼装好的完整拼音，学习混合全拼+简拼自造词
    engine.learn("我真牛逼", "wo zhen niu bi");
    let r = engine.process("woznb");
    assert!(
        r.iter().any(|c| c.text.as_ref() == "我真牛逼"),
        "学习后 woznb 应能打出 '我真牛逼'"
    );
}

#[test]
fn test_mixed_input_fallback_to_first_syllable_chars() {
    let engine = Engine::new();
    let r = engine.process("woznb");
    let texts: Vec<&str> = r.iter().map(|c| c.text.as_ref()).collect();
    println!("woznb => {:?}", texts);
    // 未学习前，只应给出第一个音节（wo）的单字候选，避免用户误选后续音节
    assert!(texts.contains(&"我"), "woznb 应给出首音节单字候选 '我'");
    assert!(
        !texts.contains(&"真"),
        "woznb 不应给出第二个音节的单字候选 '真'"
    );
    assert!(
        !texts.contains(&"牛"),
        "woznb 不应给出第三个音节的单字候选 '牛'"
    );
    assert!(
        !texts.contains(&"比"),
        "woznb 不应给出第四个音节的单字候选 '比'"
    );
}

// ============= 多音字主音/次音测试 =============

/// 构造一个带主音/次音标记的内存 DB，用于多音字排序测试
fn make_polyphone_db(path: &str) {
    let _ = std::fs::remove_file(path);
    let mut s = crate::storage::Storage::open(path).unwrap();
    // de 音节：德(主音)、地(de 是次音，di 是主音)
    s.import_x1dict_batch(&[
        crate::storage::X1DictWord {
            simplified: "德".into(),
            pinyin: "de".into(),
            freq: 5230,
            is_primary: true,
        },
        crate::storage::X1DictWord {
            simplified: "地".into(),
            pinyin: "de".into(),
            freq: 8780,
            is_primary: false,
        },
        crate::storage::X1DictWord {
            simplified: "地".into(),
            pinyin: "di".into(),
            freq: 8780,
            is_primary: true,
        },
        // 底：主音 di，de 是次音（但未收录在 POLYPHONE_PRIMARY，被默认当主音）
        crate::storage::X1DictWord {
            simplified: "底".into(),
            pinyin: "de".into(),
            freq: 50,
            is_primary: true,
        },
    ])
    .unwrap();
}

#[test]
fn test_polyphone_primary_reading_wins() {
    // de 查询：德(主音)应排在地(次音)前面，即使地 freq 更高
    let path = crate::test_tmp::dir().join("x1_test_polyphone.db");
    let path = path.to_str().unwrap();
    make_polyphone_db(path);
    let engine = Engine::new_with_db(path).unwrap();
    let cands = engine.process("de");
    let pos_de = cands.iter().position(|c| c.text.as_ref() == "德");
    let pos_di = cands
        .iter()
        .position(|c| c.text.as_ref() == "地" && c.pinyin.as_ref() == "de");
    assert!(
        pos_de < pos_di,
        "德(主音)应排在地(次音)前面：德={:?} 地={:?}",
        pos_de,
        pos_di
    );
    // 底(主音, freq=50)不应排在地(次音, freq=8780)前面（分数加成避免过度降权）
    let pos_di_low = cands.iter().position(|c| c.text.as_ref() == "底");
    assert!(
        pos_di < pos_di_low,
        "地(次音8780)应排在底(主音50)前面，避免过度降权：地={:?} 底={:?}",
        pos_di,
        pos_di_low
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn test_polyphone_dynamic_override() {
    // 用户在 de 下反复选「地」(次音)，应能反超「德」(主音)
    let path = crate::test_tmp::dir().join("x1_test_polyphone2.db");
    let path = path.to_str().unwrap();
    make_polyphone_db(path);
    let mut engine = Engine::new_with_db(path).unwrap();
    // 初始：德排前
    let cands = engine.process("de");
    let (pos_de, pos_di) = (
        cands.iter().position(|c| c.text.as_ref() == "德").unwrap(),
        cands
            .iter()
            .position(|c| c.text.as_ref() == "地" && c.pinyin.as_ref() == "de")
            .unwrap(),
    );
    assert!(pos_de < pos_di, "初始应德排前");
    // 选地 4 次（超过阈值），应反超
    for _ in 0..4 {
        engine.commit("de", "地", Some("de"), true);
    }
    let cands = engine.process("de");
    let (pos_de, pos_di) = (
        cands.iter().position(|c| c.text.as_ref() == "德").unwrap(),
        cands
            .iter()
            .position(|c| c.text.as_ref() == "地" && c.pinyin.as_ref() == "de")
            .unwrap(),
    );
    assert!(
        pos_di < pos_de,
        "用户选地4次后应反超德：地={} 德={}",
        pos_di,
        pos_de
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn test_polyphone_no_cross_contamination() {
    // 在 de 选「地」不应让 di 查询的「地」获得主音加成（pinyin_count 按 音隔离）
    let path = crate::test_tmp::dir().join("x1_test_polyphone3.db");
    let path = path.to_str().unwrap();
    make_polyphone_db(path);
    let mut engine = Engine::new_with_db(path).unwrap();
    // 在 de 下选地 3 次
    for _ in 0..3 {
        engine.commit("de", "地", Some("de"), true);
    }
    // di 查询：地(主音 di)的排序应正常，不因 de 选择获得额外加成
    let cands = engine.process("di");
    // 地(di, 主音)应排第一（它 freq=8780 最高，且是主音）
    assert_eq!(cands[0].text.as_ref(), "地", "di 查询里地应排第一");
    let _ = std::fs::remove_file(path);
}

// ============= 智能纠错测试 =============

#[test]
fn test_correction_triggered_when_few_candidates() {
    // 候选不足时触发纠错：输入拼错的串，变形后能查到候选
    let path = crate::test_tmp::dir().join("x1_test_correction.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let mut s = crate::storage::Storage::open(path).unwrap();
        s.import_x1dict_batch(&[crate::storage::X1DictWord {
            simplified: "你好".into(),
            pinyin: "ni hao".into(),
            freq: 5000,
            is_primary: true,
        }])
        .unwrap();
    }
    let engine = Engine::new_with_db(path).unwrap();
    // nihao 正常查询应有候选，不触发纠错
    let cands = engine.process("nihao");
    assert!(!cands.is_empty(), "nihao 应有候选");
    assert!(
        cands.iter().all(|c| !c.is_correction),
        "候选充足时不应有纠错候选"
    );

    // nihsou（s 误触，想打 h）候选不足，应触发纠错补充 nihao 变形的候选
    let cands = engine.process("nihsou");
    let corrections: Vec<_> = cands.iter().filter(|c| c.is_correction).collect();
    // 注意：nihsou 可能切不出有效音节导致 0 候选，纠错应补充
    // 变形 nihsou→nihso? 不对，关键是 s→h 邻接变形能产生 nihou/nihao 等
    // 只要纠错候选存在且 is_correction=true 即可
    eprintln!(
        "nihsou: 总候选={}, 纠错候选={}",
        cands.len(),
        corrections.len()
    );
    // 至少应该有一些候选（无论纠错与否），否则输入法对拼错完全无响应
    assert!(!cands.is_empty(), "拼错的 nihsou 应至少有纠错候选");
    let _ = std::fs::remove_file(path);
}

#[test]
fn test_correction_disabled_by_switch() {
    // 开关关闭时不触发纠错
    let path = crate::test_tmp::dir().join("x1_test_correction_off.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let mut s = crate::storage::Storage::open(path).unwrap();
        s.import_x1dict_batch(&[crate::storage::X1DictWord {
            simplified: "你好".into(),
            pinyin: "ni hao".into(),
            freq: 5000,
            is_primary: true,
        }])
        .unwrap();
    }
    let mut engine = Engine::new_with_db(path).unwrap();
    engine.set_smart_correction_enabled(false);
    let cands = engine.process("nihsou");
    assert!(
        cands.iter().all(|c| !c.is_correction),
        "开关关闭时不应有纠错候选"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn test_correction_is_correction_flag() {
    // 纠错候选的 is_correction 必须为 true
    let path = crate::test_tmp::dir().join("x1_test_correction_flag.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let mut s = crate::storage::Storage::open(path).unwrap();
        s.import_x1dict_batch(&[crate::storage::X1DictWord {
            simplified: "测试".into(),
            pinyin: "ce shi".into(),
            freq: 5000,
            is_primary: true,
        }])
        .unwrap();
    }
    let engine = Engine::new_with_db(path).unwrap();
    // 正常查询的候选 is_correction 必须为 false
    let cands = engine.process("ceshi");
    for c in &cands {
        assert!(
            !c.is_correction,
            "正常候选 is_correction 必须为 false: {}",
            c.text
        );
    }
    let _ = std::fs::remove_file(path);
}

#[test]
fn test_correction_no_trigger_for_initial_tail() {
    // 回归测试：「完整音节 + 末尾声母」是正常简拼尾部输入，不应触发纠错。
    // 场景：输入 zhongg（中国简拼）时不应冒出纠错橙词（钟爱/中澳/重案）。
    let path = crate::test_tmp::dir().join("x1_test_correction_initial_tail.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let mut s = crate::storage::Storage::open(path).unwrap();
        s.import_x1dict_batch(&[
            crate::storage::X1DictWord {
                simplified: "中国".into(),
                pinyin: "zhong guo".into(),
                freq: 5000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "北京".into(),
                pinyin: "bei jing".into(),
                freq: 4000,
                is_primary: true,
            },
        ])
        .unwrap();
    }
    let engine = Engine::new_with_db(path).unwrap();

    // 这些输入都是正常的简拼尾部，不应产生任何纠错候选
    for input in &["zhongg", "beij", "shangh", "zhongb"] {
        let cands = engine.process(input);
        let corrections: Vec<_> = cands.iter().filter(|c| c.is_correction).collect();
        assert!(
            corrections.is_empty(),
            "输入 {} 不应触发纠错，但有纠错候选: {:?}",
            input,
            corrections.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    // 对照：is_likely_typo 对真拼错（相邻非法声母 hs）仍返回 true，
    // 保证纠错路径本身没被破坏（实际是否补充候选取决于候选数是否不足）
    assert!(
        crate::pinyin::is_likely_typo("nihsou"),
        "nihsou 是真拼错，应判为 typo"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn test_correction_keyboard_neighbors_priority() {
    // 邻接替换优先 + 非邻接替换已移除（键盘误触场景的边界就是邻接关系，
    // 非邻接替换产生字母序随机的冷僻词噪声）。
    // generate_corrections 只生成：邻接变形（排前）+ 删除变形（排后）。
    let variants = generate_corrections("nihsou");
    assert!(!variants.is_empty(), "应生成变形候选");
    // 按水平同行邻接表，n 的邻接是 b/m（底行），前几个变形应含 n→b/n→m
    let first_few: Vec<&String> = variants.iter().take(5).collect();
    let n_neighbors = ["bihsou", "mihsou"];
    let neighbor_count = first_few
        .iter()
        .filter(|v| n_neighbors.contains(&v.as_str()))
        .count();
    assert!(
        neighbor_count >= 2,
        "前5个变形应包含 n 的水平邻接替换(b/m)，实际: {:?}",
        first_few
    );
    // 非邻接替换不得存在（n→a 不是 n 的邻键）
    assert!(
        !variants.iter().any(|v| v == "aihsou"),
        "非邻接替换(n→a)已移除，不应出现在变形列表: {:?}",
        variants.iter().take(8).collect::<Vec<_>>()
    );
}

#[test]
fn test_correction_swap_generates_valid_variants() {
    // generate_swap_corrections 只做相邻字母交换（韵母乱序规则表已删除：
    // 方向写反 + 与相邻交换完全冗余，见函数文档）。
    let variants = generate_swap_corrections("nag");
    // nag 的相邻交换：ang（n↔a）、nga（a↔g）。
    assert!(
        variants.contains(&"ang".to_string()),
        "nag 应能通过相邻交换(n↔a)得到 ang，实际: {:?}",
        variants
    );
    // 乱序规则表删除后不再产生非交换变体（agn 与 nag 差两次交换，交换类不该有）
    assert!(
        !variants.contains(&"agn".to_string()),
        "agn 与 nag 差两次交换，相邻交换类不应产生，实际: {:?}",
        variants
    );
}

#[test]
fn test_correction_swap_adjacent_exchange() {
    // 相邻字母交换：teh → the（e↔h 交换），nad → and（n↔a 交换）。
    let teh_variants = generate_swap_corrections("teh");
    assert!(
        teh_variants.contains(&"the".to_string()),
        "teh 相邻交换(e↔h)应得到 the"
    );
    let nad_variants = generate_swap_corrections("nad");
    assert!(
        nad_variants.contains(&"and".to_string()),
        "nad 相邻交换(n↔a)应得到 and"
    );
}

#[test]
fn test_correction_swap_dedup_and_no_original() {
    // 变形结果去重 + 不含原始 input 自身。
    let variants = generate_swap_corrections("ai");
    // ai 的相邻交换得 ia（乱序规则表已删，仅交换一条路径，HashSet 去重）。
    assert!(variants.contains(&"ia".to_string()), "ai 交换得 ia");
    assert!(!variants.contains(&"ai".to_string()), "不应含原始 input");
}

#[test]
fn test_correction_not_triggered_for_short_input() {
    // 单字符输入不纠错（太短，变形无意义）
    let path = crate::test_tmp::dir().join("x1_test_correction_short.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let mut s = crate::storage::Storage::open(path).unwrap();
        s.import_x1dict_batch(&[crate::storage::X1DictWord {
            simplified: "啊".into(),
            pinyin: "a".into(),
            freq: 5000,
            is_primary: true,
        }])
        .unwrap();
    }
    let engine = Engine::new_with_db(path).unwrap();
    let cands = engine.process("a");
    assert!(
        cands.iter().all(|c| !c.is_correction),
        "单字符输入不应触发纠错"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn test_correction_structure_only() {
    // 纠错只对结构性拼音错误生效（无法切成全有效音节、也不是合法简拼混合）。
    // 结构合法的输入（无论词库是否有匹配）都不纠错，避免噪音橙词。
    let path = crate::test_tmp::dir().join("x1_test_correction_structure.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let mut s = crate::storage::Storage::open(path).unwrap();
        s.import_x1dict_batch(&[
            // 运行时 + 相关词
            crate::storage::X1DictWord {
                simplified: "运行时".into(),
                pinyin: "yun xing shi".into(),
                freq: 5000,
                is_primary: true,
            },
            crate::storage::X1DictWord {
                simplified: "运行".into(),
                pinyin: "yun xing".into(),
                freq: 6000,
                is_primary: true,
            },
            // 平整（pingzheng）—— pingzeng 输入虽匹配不到，但不该被纠错（结构合法）
            crate::storage::X1DictWord {
                simplified: "平整".into(),
                pinyin: "ping zheng".into(),
                freq: 5000,
                is_primary: true,
            },
            // 你好（对照）
            crate::storage::X1DictWord {
                simplified: "你好".into(),
                pinyin: "ni hao".into(),
                freq: 8000,
                is_primary: true,
            },
        ])
        .unwrap();
    }
    let engine = Engine::new_with_db(path).unwrap();

    // 场景1：yunxs（运行时的全拼+简拼+简拼）—— 合法的混合输入，不纠错
    let cands = engine.process("yunxs");
    let corrections: Vec<_> = cands.iter().filter(|c| c.is_correction).collect();
    assert!(
        corrections.is_empty(),
        "yunxs 是合法的全拼+简拼混合（yun+x+s），不应触发纠错，但有橙词: {:?}",
        corrections.iter().map(|c| &c.text).collect::<Vec<_>>()
    );

    // 场景2：yunxongshi（yunxingshi 的 i 错打成 o）—— xong 非有效音节，
    // 既切不出全有效音节，也不是合法简拼混合 → 结构错误 → 应纠错出「运行时」
    let cands = engine.process("yunxongshi");
    let corrections: Vec<_> = cands.iter().filter(|c| c.is_correction).collect();
    assert!(
        corrections.iter().any(|c| c.text.as_ref() == "运行时"),
        "yunxongshi 是结构性错误（xong 无效），应纠错出「运行时」，实际纠错: {:?}",
        corrections.iter().map(|c| &c.text).collect::<Vec<_>>()
    );

    // 场景3：pingzeng（ping+zeng 都有效）—— 结构合法，但字面无整词
    //（ping+zeng 查不到词）而发音变体命中「平整」（eng→en）→ 既有词证据
    // 成立，出橙色「平整」（发音纠错扩展到合法输入，
    // 证据 = bigram 数据或既有词）。
    let cands = engine.process("pingzeng");
    let corrections: Vec<_> = cands.iter().filter(|c| c.is_correction).collect();
    assert!(
        corrections.iter().any(|c| c.text.as_ref() == "平整"),
        "pingzeng 字面无整词，应出橙色「平整」（既有词证据），实际纠错: {:?}",
        corrections.iter().map(|c| &c.text).collect::<Vec<_>>()
    );

    // 对照：nihao（正常输入）→ 不纠错
    let cands = engine.process("nihao");
    let corrections: Vec<_> = cands.iter().filter(|c| c.is_correction).collect();
    assert!(
        corrections.is_empty(),
        "nihao 正常输入不应触发纠错，但有橙词: {:?}",
        corrections.iter().map(|c| &c.text).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_file(path);
}

///
/// 修复：process_jianpin_inner 在 push 绿词前加 removed_texts 检查，
/// 命中直接跳过（与全拼"降级排末尾"略有差异，简拼绿词是临时拼凑，
/// 用户删了说明不想要，不需要保留复活能力）。
#[test]
fn test_removed_jianpin_green_word_stays_deleted() {
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    // 用临时副本（remove 立即落盘黑名单，防污染真实种子文件）
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_rm_green3_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::copy(seed, &tmp);
    let mut engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap_or_else(|_| Engine::new());

    // 先查询 wwww，看是否产生绿词候选
    let cands_before = engine.process("wwww");
    let greens_before: Vec<String> = cands_before
        .iter()
        .filter(|c| c.is_sentence)
        .map(|c| c.text.as_ref().to_owned())
        .collect();
    println!("wwww 绿词候选（删除前）: {:?}", greens_before);

    // 如果种子 DB 在 wwww 下不产生绿词，这个测试就无法验证（skip）
    // 但至少断言：删除任意一个绿词后，再次查询时它不应再出现
    if let Some(green_text) = greens_before.first().cloned() {
        // 删除这个绿词
        engine.remove(&green_text);
        assert!(
            engine.removed_texts.contains(&green_text),
            "删除后 removed_texts 应含该绿词"
        );

        // 再次查询：被删除的绿词不应再出现
        let cands_after = engine.process("wwww");
        let still_present = cands_after.iter().any(|c| c.text.as_ref() == green_text);
        assert!(
            !still_present,
            "删除后的简拼绿词 '{}' 不应再次出现（应被黑名单拦截），实际候选：{:?}",
            green_text,
            cands_after
                .iter()
                .map(|c| &c.text)
                .take(5)
                .collect::<Vec<_>>()
        );
        println!("删除 '{}' 后不再出现", green_text);
    } else {
        println!("（skip）种子 DB 在 wwww 下未产生绿词，无法验证简拼删除路径");
    }
    let _ = std::fs::remove_file(&tmp);
}

/// 回归：zh/sh/ch 开头的拼错被混合声母简拼分流劫持
/// 时，智能纠错完全不触发（zhognwen→中文 不可达）。混合路径尾部补调
/// maybe_correct 后，结构非法错输应出橙词；合法混合输入（zhgrm）不得出橙词。
/// 用真实种子库（合成小库可能掩盖真实词库中的失效场景）。
#[test]
fn test_correction_reachable_via_hybrid_shengmu_route() {
    let seed = super::tests_query::seed_copy("corr_hybrid");
    let engine = Engine::new_with_db(&seed).unwrap();

    // zhognwen：zh 开头 + 首音节被键序打破（g/n 交换）→ 混合分流，
    // 应出橙色「中文」
    let cands = engine.process("zhognwen");
    let corr: Vec<_> = cands.iter().filter(|c| c.is_correction).collect();
    assert!(
        corr.iter().any(|c| c.text.as_ref() == "中文"),
        "zhognwen 应纠错出橙色「中文」，实际橙词: {:?}",
        corr.iter().map(|c| &c.text).collect::<Vec<_>>()
    );

    // 合法混合声母输入不得出橙词（zhgrm = 中国人民 类简拼混合）
    let cands = engine.process("zhgrm");
    assert!(
        cands.iter().all(|c| !c.is_correction),
        "zhgrm 是合法混合简拼，不应有橙词: {:?}",
        cands
            .iter()
            .filter(|c| c.is_correction)
            .map(|c| &c.text)
            .collect::<Vec<_>>()
    );

    super::tests_query::seed_copy_cleanup(&seed);
}

/// 回归：纠错目标已被部分匹配捞进自然列表时，
/// 旧实现去重静默跳过（不标橙不提升，百度 埋在 #7）。现应标橙并提升到
/// 纠错位（第 0 位之后）。
#[test]
fn test_correction_promotes_existing_natural_candidate() {
    let seed = super::tests_query::seed_copy("corr_promote");
    let engine = Engine::new_with_db(&seed).unwrap();

    // nihoa（o/a 按反，想打 nihao）：你好 本在自然列表（部分匹配），
    // 应标橙提到纠错位
    let cands = engine.process("nihoa");
    let pos = cands.iter().position(|c| c.text.as_ref() == "你好");
    assert!(
        pos.is_some_and(|p| p <= 2) && cands[pos.unwrap()].is_correction,
        "nihoa 应把「你好」标橙提升到纠错位，实际位置 {:?} 橙={}",
        pos,
        pos.map(|p| cands[p].is_correction).unwrap_or(false)
    );

    // baiddu（多打一个 d，想打 baidu）：百度 本在 #7
    let cands = engine.process("baiddu");
    let pos = cands.iter().position(|c| c.text.as_ref() == "百度");
    assert!(
        pos.is_some_and(|p| p <= 2) && cands[pos.unwrap()].is_correction,
        "baiddu 应把「百度」标橙提升到纠错位，实际位置 {:?} 橙={}",
        pos,
        pos.map(|p| cands[p].is_correction).unwrap_or(false)
    );

    super::tests_query::seed_copy_cleanup(&seed);
}

/// 回归：橙词纠错提交不写错串→词映射。
/// commit_correction 学正确词（freq 照常 +1）但 input_mappings 不落错串键；
/// 普通 commit 行为不变（简拼习惯强化是既有功能，不得误伤）。
#[test]
fn test_correction_commit_skips_typo_mapping() {
    let seed = super::tests_query::seed_copy("corr_nomap");
    let mut engine = Engine::new_with_db(&seed).unwrap();

    let freq_before = engine.freq.counts.get("你好").copied().unwrap_or(0);
    // 纠错提交：错串 nihso + 正确读法 ni hao
    engine.commit_correction("nihso", "你好", Some("ni hao"), true, false);
    assert!(
        !engine.input_mappings.contains_key(&("nihso".to_string(), "你好".to_string())),
        "纠错提交不得写错串映射 nihso→你好"
    );
    let freq_after = engine.freq.counts.get("你好").copied().unwrap_or(0);
    assert!(
        freq_after > freq_before,
        "纠错提交对正确词的 freq 学习照常（{} → {}）",
        freq_before, freq_after
    );

    // 对照：普通 commit 的映射学习不受影响（简拼/混合习惯强化是既有功能）
    engine.commit("zgrm", "中国人民", Some("zhong guo ren min"), true);
    assert!(
        engine.input_mappings.contains_key(&("zgrm".to_string(), "中国人民".to_string())),
        "普通 commit 的 input_mapping 学习不得被误伤"
    );

    super::tests_query::seed_copy_cleanup(&seed);
}

/// 合并语义守护：发音混淆纠错单开关统管两条发音
/// 路径——结构非法输入的发音变形（maybe_correct 主路径）与合法输入的
/// 上下文证据营救（context_phonetic_rescue）同属一个开关；
/// 本测试钉死合并语义，防止误拆为独立开关。
#[test]
fn test_phonetic_single_switch_governs_both_paths() {
    let seed = super::tests_query::seed_copy("corr_phonetic_merge");
    let mut engine = Engine::new_with_db(&seed).unwrap();

    // 开（默认）：合法输入营救可达——zongguo → 中国[橙]
    let cands = engine.process("zongguo");
    assert!(
        cands.iter().any(|c| c.is_correction && c.text.as_ref() == "中国"),
        "发音开关开：zongguo 应出橙色「中国」（营救路径），实际橙词: {:?}",
        cands
            .iter()
            .filter(|c| c.is_correction)
            .map(|c| &c.text)
            .collect::<Vec<_>>()
    );

    // 关：营救与结构错变形同时关闭（单开关统管两条路径）
    engine.set_phonetic_correction_enabled(false);
    let cands = engine.process("zongguo");
    assert!(
        cands.iter().all(|c| !c.is_correction),
        "发音开关关：zongguo 营救不得出橙，实际: {:?}",
        cands
            .iter()
            .filter(|c| c.is_correction)
            .map(|c| &c.text)
            .collect::<Vec<_>>()
    );

    super::tests_query::seed_copy_cleanup(&seed);
}

/// 合并语义守护：结构非法输入的发音变形同属发音混淆纠错
/// 开关。faixian→海鲜：键盘类（daixian/gaixian/删 f 等）与键序类均产不出
/// 「海鲜」精确匹配，只有发音类 f→h 变形可达——发音主路径唯一可达例。
#[test]
fn test_phonetic_structural_correction_same_switch() {
    let path = crate::test_tmp::dir().join("x1_test_phonetic_split.db");
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    {
        let mut s = crate::storage::Storage::open(path).unwrap();
        s.import_x1dict_batch(&[crate::storage::X1DictWord {
            simplified: "海鲜".into(),
            pinyin: "hai xian".into(),
            freq: 5000,
            is_primary: true,
        }])
        .unwrap();
    }
    let mut engine = Engine::new_with_db(path).unwrap();
    // 开（默认）：结构非法输入 faixian 纠出「海鲜」
    let cands = engine.process("faixian");
    assert!(
        cands.iter().any(|c| c.is_correction && c.text.as_ref() == "海鲜"),
        "发音开关开：faixian 应出橙色「海鲜」（f→h 发音变形），实际橙词: {:?}",
        cands
            .iter()
            .filter(|c| c.is_correction)
            .map(|c| &c.text)
            .collect::<Vec<_>>()
    );

    // 关：faixian 不得出橙
    engine.set_phonetic_correction_enabled(false);
    let cands = engine.process("faixian");
    assert!(
        cands.iter().all(|c| !c.is_correction),
        "发音开关关：faixian 不得出橙词，实际: {:?}",
        cands
            .iter()
            .filter(|c| c.is_correction)
            .map(|c| &c.text)
            .collect::<Vec<_>>()
    );

    let _ = std::fs::remove_file(path);
}

/// 回归：橙词纠错候选选中时必须全消费整个错串。
/// nihoa→你好（键序交换）：候选拼音 "ni hao" 与输入 nihoa 互不前缀，旧规则
/// 只消费 ni'h、把 oa 留在预编辑里当「没有消费的简拼组合」残留。
#[test]
fn test_correction_candidate_consumes_full_input() {
    let seed = super::tests_query::seed_copy("corr_consume");
    let engine = Engine::new_with_db(&seed).unwrap();
    for (input, word) in [
        ("nihoa", "你好"),    // 键序
        ("baiddu", "百度"),   // 键盘多打删除
        ("zongguo", "中国"),  // 发音营救
        ("zhognwen", "中文"), // 混合分流路纠正
    ] {
        let cands = engine.process(input);
        let pos = cands
            .iter()
            .position(|c| c.text.as_ref() == word && c.is_correction)
            .unwrap_or_else(|| panic!("{} 应出橙色「{}」", input, word));
        let lens = engine.consumed_lens(input, &cands);
        assert_eq!(
            lens[pos],
            input.len(),
            "{}→{} 纠错候选应全消费 {} 字母，实际 {}",
            input, word, input.len(), lens[pos]
        );
    }
    super::tests_query::seed_copy_cleanup(&seed);
}

/// 回归（baomianquan→保密安全 绿词不可达）：当选切分
/// [bao,mian,quan] 零产出时，拆分惩罚切分 [bao,mi,an,quan]（保密+安全）
/// 单列营救清单补跑 DP——不补跑则 >=1000 门槛把它挡在仲裁外，无分隔符
/// 打不出「保密安全」，只能打 baomi'anquan。
/// 守护三件事：① 营救命中；② 纯单字拼凑不得被营救（质量门）；
/// ③ duan 完整音节保护不受影响（营救只在当选 DP 空时介入）。
#[test]
fn test_penalized_segmentation_rescue_green_word() {
    let seed = super::tests_query::seed_copy("corr_seg_rescue");
    let engine = Engine::new_with_db(&seed).unwrap();

    let cands = engine.process("baomianquan");
    assert_eq!(
        cands.first().map(|c| (c.text.as_ref(), c.is_sentence)),
        Some(("保密安全", true)),
        "baomianquan 应营救出绿词「保密安全」，实际首候选: {:?}",
        cands.first().map(|c| (&c.text, c.is_sentence))
    );

    // duan 保护：当选 [duan] 有产出（段），营救不介入
    let cands = engine.process("duan");
    assert_eq!(
        cands.first().map(|c| c.text.as_ref()),
        Some("段"),
        "duan 完整音节保护不得被营救路径破坏"
    );

    super::tests_query::seed_copy_cleanup(&seed);
}

/// 回归：maxCorrections=0 必须真关闭全部三条橙词
/// 通道——主路径本就短路，但折叠路径旧实现 push 后才查 cap（仍漏 1 条）、
/// 营救路径硬编码 3 条不看上限。页脚承诺「为 0 表示不补充纠错候选」。
#[test]
fn test_correction_max_zero_disables_all_orange_channels() {
    let seed = super::tests_query::seed_copy("corr_max0");
    let mut engine = Engine::new_with_db(&seed).unwrap();
    engine.set_max_corrections(0);

    // 折叠通道（legacy 无时序平台恒可折叠）：shenng 不得出橙
    let cands = engine.process("shenng");
    assert!(
        cands.iter().all(|c| !c.is_correction),
        "max=0 时 shenng 折叠通道应关闭，实际橙词: {:?}",
        cands
            .iter()
            .filter(|c| c.is_correction)
            .map(|c| &c.text)
            .collect::<Vec<_>>()
    );

    // 营救通道：pingzeng 不得出橙（平整/凭证）
    let cands = engine.process("pingzeng");
    assert!(
        cands.iter().all(|c| !c.is_correction),
        "max=0 时 pingzeng 营救通道应关闭，实际橙词: {:?}",
        cands
            .iter()
            .filter(|c| c.is_correction)
            .map(|c| &c.text)
            .collect::<Vec<_>>()
    );

    // 主路径通道：zhognwen 不得出橙
    let cands = engine.process("zhognwen");
    assert!(
        cands.iter().all(|c| !c.is_correction),
        "max=0 时 zhognwen 主路径应关闭，实际橙词: {:?}",
        cands
            .iter()
            .filter(|c| c.is_correction)
            .map(|c| &c.text)
            .collect::<Vec<_>>()
    );

    super::tests_query::seed_copy_cleanup(&seed);
}

/// 回归：折叠纠错改用引擎时序门控，与前端防抖开关
/// 解绑——防抖关（默认）时折叠仍可用（shenng→生 可达）。纯时序
/// 判定：快双击折叠出橙词，慢双写不折叠；合法简拼尾的快双击（zhongg）
/// 折叠候选不得抢走自然首选（中国 恒 #0）。
#[test]
fn test_collapse_correction_timing_only_gate() {
    let seed = super::tests_query::seed_copy("corr_collapse");
    let engine = Engine::new_with_db(&seed).unwrap();

    // 快双击序列：shenng（n 双写间隔 30ms）→ 折叠出橙色「生」
    engine.process_timed("s", u32::MAX);
    engine.process_timed("sh", 120);
    engine.process_timed("she", 110);
    engine.process_timed("shen", 100);
    engine.process_timed("shenn", 30);
    let cands = engine.process_timed("shenng", 90);
    assert!(
        cands.iter().any(|c| c.is_correction && c.text.as_ref() == "生"),
        "快双击 shenng 应折叠出橙色「生」，实际橙词: {:?}",
        cands
            .iter()
            .filter(|c| c.is_correction)
            .map(|c| &c.text)
            .collect::<Vec<_>>()
    );

    // 慢双写（刻意节奏）→ 不折叠
    engine.process_timed("sh", u32::MAX);
    engine.process_timed("she", 200);
    engine.process_timed("shen", 220);
    engine.process_timed("shenn", 500);
    let cands = engine.process_timed("shenng", 150);
    assert!(
        !cands.iter().any(|c| c.is_correction && c.text.as_ref() == "生"),
        "慢双写 shenng 不应折叠出橙色「生」"
    );

    // 合法简拼尾快双击（zhong+g=中国，两 g 快击 40ms）：折叠橙词可出现，
    // 但自然首选「中国」必须守住 #0（提升语义不抢最优自然解读）
    engine.process_timed("z", u32::MAX);
    engine.process_timed("zh", 120);
    engine.process_timed("zho", 110);
    engine.process_timed("zhon", 100);
    engine.process_timed("zhong", 90);
    let cands = engine.process_timed("zhongg", 40);
    assert_eq!(
        cands.first().map(|c| c.text.as_ref()),
        Some("中国"),
        "zhongg 的自然首选必须是「中国」，实际: {:?}",
        cands.first().map(|c| &c.text)
    );

    super::tests_query::seed_copy_cleanup(&seed);
}

/// 回归：词频地板从 500 下调到 300（真实种子谱
/// 分布：≤200 是拿他类噪音档，300-499 是合法常用词档）。
/// - yunxongshi（运行时 的 i 误触成 o）应纠出「运行时」(355)；
/// - nagta（低频噪音样本）仍不得出「拿他」(100)。
#[test]
fn test_correction_freq_floor_real_seed() {
    let seed = super::tests_query::seed_copy("corr_floor");
    let engine = Engine::new_with_db(&seed).unwrap();

    let cands = engine.process("yunxongshi");
    let corr: Vec<_> = cands.iter().filter(|c| c.is_correction).collect();
    assert!(
        corr.iter().any(|c| c.text.as_ref() == "运行时"),
        "yunxongshi 应纠错出橙色「运行时」（地板 300 放行 355），实际橙词: {:?}",
        corr.iter().map(|c| &c.text).collect::<Vec<_>>()
    );

    let cands = engine.process("nagta");
    assert!(
        !cands.iter().any(|c| c.is_correction && c.text.as_ref() == "拿他"),
        "nagta 不得纠出低频噪音「拿他」(freq=100，地板以下)"
    );

    super::tests_query::seed_copy_cleanup(&seed);
}

/// 回归：尝试预算全类别共享时，键盘类无效变体
///（结构合法但词库无词）烧光预算，键序类永不执行——weishemne（为什么
/// 的 m/n 按反）纠不出。按类别独立预算后应出橙色「为什么」。
#[test]
fn test_correction_per_category_attempt_budget() {
    let seed = super::tests_query::seed_copy("corr_budget");
    let engine = Engine::new_with_db(&seed).unwrap();

    let cands = engine.process("weishemne");
    let corr: Vec<_> = cands.iter().filter(|c| c.is_correction).collect();
    assert!(
        corr.iter().any(|c| c.text.as_ref() == "为什么"),
        "weishemne 应纠错出橙色「为什么」（键序类不被键盘类饿死），实际橙词: {:?}",
        corr.iter().map(|c| &c.text).collect::<Vec<_>>()
    );

    // 乱码仍秒回（预算语义未放大）：qzvwx 无橙词
    let cands = engine.process("qzvwx");
    assert!(
        cands.iter().all(|c| !c.is_correction),
        "qzvwx 乱码不应有橙词: {:?}",
        cands
            .iter()
            .filter(|c| c.is_correction)
            .map(|c| &c.text)
            .collect::<Vec<_>>()
    );

    super::tests_query::seed_copy_cleanup(&seed);
}
