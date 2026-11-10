//! 切分选举误判守护（shougangeili 案例）。
//!
//! 病灶：`shougangeili` 在用户活库上出「首钢诶里」而不是「手感给力」——
//! 切分选举用**最高单词频**定序，用户词「首钢」(1355) 压过「手感 767 +
//! 给力 1028」这条两个词的链：
//! - 当选 [shou,gang,ei,li]：1 个词跨度（首钢），选举分 1999
//! - 备选 [shou,gan,gei,li]：2 个词跨度（手感/给力），选举分 1770
//! DP 原本只在「含简拼位」或「解码为空」时仲裁，此场景两者都不满足 →
//! DP 永远看不到 gan|gei 读法。
//!
//! 修复分两层（本文件守护）：
//! 1. **仲裁触发**：当选读法的词跨度数 < 备选读法的词跨度数时也让 DP 裁决
//!    （一个高频词压过更自然的词组链 = 选举不可信的签名）。对比基准：
//!    nizaiganmane（当选 6 > 备选 3）与 wananle（2 > 1）都**不触发**，
//!    保住既有定稿。
//! 2. **记忆相对质量门**：green_memo / input_mappings 的记忆条目若词组链比
//!    当前最优句更差，降级到候选末尾（保可达），不再无条件抢首位——一次
//!    误选不该永久霸屏。

use super::tests_query::{seed_copy, seed_copy_cleanup};
use super::Engine;

/// 构造「用户学过 首钢」的场景：把 首钢(shou gang, freq 高) 插进 dict，
/// 使切分选举像活库一样偏向 gang|ei 读法。
fn engine_with_shougang(tag: &str) -> (String, Engine) {
    let db = seed_copy(tag);
    {
        let conn = rusqlite::Connection::open(&db).expect("open seed copy");
        conn.execute(
            "INSERT OR REPLACE INTO x1dict_words (simplified, pinyin, freq, is_primary)
             VALUES ('首钢', 'shou gang', 1355, 1)",
            [],
        )
        .expect("insert 首钢");
    }
    let mut engine = Engine::new_with_db(&db).expect("open engine");
    engine.set_smart_word_zh_enabled(false);
    (db, engine)
}

fn top1(engine: &Engine, input: &str) -> String {
    engine
        .process(input)
        .first()
        .map(|c| c.text.to_string())
        .unwrap_or_default()
}

/// **核心**：有了高频「首钢」词，`shougangeili` 仍必须解出「手感给力」。
///
/// 修复前：选举被 首钢(1355) 拉向 gang|ei，DP 只看到该切分 → 首钢诶里。
/// 修复后：备选 gan|gei 的词跨度更多 → 触发仲裁 → DP 按语言证据选出
/// 手感给力（手感/给力两个词）。
#[test]
fn test_high_freq_word_does_not_beat_better_word_chain() {
    let (db, engine) = engine_with_shougang("seg_chain_core");

    // 先确认构造生效：单独打 shougang 应出「首钢」
    assert_eq!(
        top1(&engine, "shougang"),
        "首钢",
        "构造前提：shougang 应命中首钢"
    );
    // 核心断言
    let got = top1(&engine, "shougangeili");
    assert_eq!(
        got, "手感给力",
        "高频单词不能压过两词链（实际首候选 {got}）"
    );

    seed_copy_cleanup(&db);
}

/// 反例：当选读法词组链**不劣于**备选时不触发仲裁，既有定稿不回归。
///
/// - `nizaiganmane`：当选 [ni,zai,gan,ma,ne] 词跨度 6 > 备选 3 → 不触发
///   （平局裁决定稿「你在干嘛呢」）。
/// - `wananle`：当选 [wan,an,le] 跨度 2 > 备选 [wa,nan,le] 1 → 不触发
///   （仲裁收紧定稿「晚安了」）。
#[test]
fn test_arbitration_not_triggered_when_elected_chain_is_better() {
    let (db, engine) = seed_copy_engine("seg_chain_negative");

    assert_eq!(
        top1(&engine, "nizaiganmane"),
        "你在干嘛呢",
        "nizaiganmane 定稿不回归（当选链更优，不仲裁）"
    );
    let wan = top1(&engine, "wananle");
    assert!(
        wan.starts_with("晚安"),
        "wananle 定稿不回归（当选链更优，不仲裁），实际 {wan}"
    );

    seed_copy_cleanup(&db);
}

fn seed_copy_engine(tag: &str) -> (String, Engine) {
    let db = seed_copy(tag);
    let mut engine = Engine::new_with_db(&db).expect("open seed copy");
    engine.set_smart_word_zh_enabled(false);
    (db, engine)
}

/// **记忆相对质量门**：误选留下的记忆条目（链路更差）不应压过更自然的整句。
///
/// 直接注入 green_memo（不经 commit，隔离掉「commit 会同时学习子词 bigram」
/// 这个副作用——那是另一条设计语义）：条目「首钢诶里」词组链只有 1 个多字段，
/// 而 DP 最优句「手感给力」有 2 个 → 条目不抢首位，但**保留可达**（末尾仍在）。
#[test]
fn test_memo_with_weaker_chain_does_not_pin_bad_sentence() {
    let (db, mut engine) = engine_with_shougang("seg_chain_memo");
    let input = "shougangeili";
    // 复刻活库形态：记忆里有「首钢诶里」，user_freq 2000（首次组装值），
    // counts ≥1（lift 门槛）。
    engine.green_memo.insert(
        input.to_string(),
        vec![(
            "首钢诶里".to_string(),
            2000,
            Some("shou gang ei li".to_string()),
        )],
    );
    engine.freq.record("首钢诶里");
    engine.context_clear_for_test();

    let cands = engine.process(input);
    let first = cands
        .first()
        .map(|c| c.text.to_string())
        .unwrap_or_default();
    assert_eq!(
        first, "手感给力",
        "链路更差的记忆条目不得霸屏（实际首候选 {first}）"
    );
    assert!(
        cands.iter().any(|c| c.text.as_ref() == "首钢诶里"),
        "记忆条目仍应可达（降级但不丢弃）: {:?}",
        cands
            .iter()
            .take(8)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );

    seed_copy_cleanup(&db);
}

/// 正常记忆提升不变：链路不劣的首次学习仍应立即置顶。
#[test]
fn test_legit_memo_still_lifts_to_front() {
    let (db, mut engine) = seed_copy_engine("seg_chain_legit_memo");
    engine.commit("xuqiuyanj", "需求研究", Some("xu qiu yan jiu"), true);
    engine.context_clear_for_test();

    assert_eq!(
        top1(&engine, "xuqiuyanj"),
        "需求研究",
        "正常自组词的首次记忆仍应置顶"
    );
    seed_copy_cleanup(&db);
}

/// **同长白词在场时绿词不展示**。
///
/// 场景：`haowuyc` → 「毫无用处（白）+ 好物因此（绿）」，绿词扎眼。
/// 120 个「4 字整词 + 尾段简拼」构造样本实测：有同长白词时 74 例绿词被压到
/// 第 2 位，**74/74 的 top1 白词都是该输入对应的真实词**，绿词全是
/// 「是一政策 / 网络电视 / 农家出租」这类碎片拼凑。判据：同长白词在场 =
/// 这条绿词没有额外信息量（同一串音节必然也能被拆成「词+虚字」的碎片链）。
///
/// 豁免：用户亲手教过的绿词仍展示（见下一条测试）。
#[test]
fn test_green_hidden_when_same_length_white_exists() {
    let (db, engine) = seed_copy_engine("green_hide_same_len");

    let cands = engine.process("haowuyc");
    assert_eq!(
        cands.first().map(|c| c.text.as_ref()),
        Some("毫无用处"),
        "整词白词应占首位"
    );
    assert!(
        !cands.iter().any(|c| c.is_sentence),
        "同长白词在场时不应出现绿词: {:?}",
        cands
            .iter()
            .take(8)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );

    // 对照：无同长白词时绿词照常出现（wohaoxiangchifan 5 字，白词都更短）
    let c2 = engine.process("wohaoxiangchifan");
    assert!(
        c2.iter().any(|c| c.is_sentence),
        "无同长白词时绿词必须照常: {:?}",
        c2.iter()
            .take(5)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );

    seed_copy_cleanup(&db);
}

/// 用户教过的绿词即便同长白词在场也仍可选中（不被静默吞掉）。
#[test]
fn test_learned_green_still_reachable_despite_same_length_white() {
    let (db, mut engine) = seed_copy_engine("green_memo_exempt");
    engine.commit("haowuyc", "好物因此", Some("hao wu yin ci"), true);
    engine.context_clear_for_test();

    let cands = engine.process("haowuyc");
    let pos = cands
        .iter()
        .position(|c| c.is_sentence && c.text.as_ref() == "好物因此");
    assert!(
        pos.is_some(),
        "学过的绿词必须仍可达: {:?}",
        cands
            .iter()
            .take(8)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
    seed_copy_cleanup(&db);
}

/// reading_preference_cmp 全序回归守护（172k 语料 learn panic 修复）。
///
/// 原实现的选择史层带**成对** 20× 词频守卫，三切分下 A/B、B/C、A/C 各自判定
/// 可成环（sort_by 全序违规 panic，用户积累 input_mappings 后打字即触发）。
/// 修复后证据键 = 纯逐侧函数（bigram, 谱地板门控 counts），本测试
/// 钉住两个不变量：①裁决对称（A<B ⇔ B>A）；②等键（无证据）返回 None 回退
/// 窗口/结构层。谱地板（freq≥500 才计选择史）的逐侧门控同时保住
/// 裁决案例（反感 997/方案 3455 同谱段，选择史照常生效）。
#[test]
fn test_reading_preference_symmetry_and_none_on_equal_evidence() {
    let (db, mut engine) = seed_copy_engine("reading_pref_totality");
    engine.input_mapping_enabled = true;
    // 反感(997)/方案(3455) 都在谱地板（≥500）上：选择史证据生效
    engine
        .input_mappings
        .insert(("fangan".to_string(), "反感".to_string()), 9);
    engine
        .input_mappings
        .insert(("fangan".to_string(), "方案".to_string()), 1);
    let a: [&str; 2] = ["fang", "an"]; // 首词 方案
    let b: [&str; 2] = ["fan", "gan"]; // 首词 反感
    let ab = engine.reading_preference_cmp("fangan", &a, &b);
    let ba = engine.reading_preference_cmp("fangan", &b, &a);
    assert_eq!(
        ab,
        Some(core::cmp::Ordering::Less),
        "方案 ct=1 应输 反感 ct=9（实际 {ab:?}）"
    );
    assert_eq!(ba, Some(core::cmp::Ordering::Greater), "裁决必须对称");
    // 无选择史证据（等键）→ None，回退窗口词频/结构层
    engine.input_mappings.clear();
    assert_eq!(
        engine.reading_preference_cmp("fangan", &a, &b),
        None,
        "等证据键应返回 None 回退后续层"
    );
    seed_copy_cleanup(&db);
}

/// 预上屏显示跟随**焦点候选**的读音切分。
///
/// 场景：duandian 的候选里混有子前缀词（毒案 du|an 只吃 duan 4 字母、
/// 短笛 duan|di 只吃 duandi 6 字母）。选举切分显示 duan'dian 让人以为
/// 整个输入都归这个候选；把焦点移到它时，预上屏应显示「已吃/未吃」边界，
/// 与选中后的实际消费自洽。
///
/// 规则：仅当候选拼音是输入**真前缀**时接管（其余候选回退选举切分，
/// 避免候选拼音与输入不对齐时乱拆）；候选拼音 == 输入时用候选音节序列。
#[test]
fn test_segmentation_follows_focused_candidate() {
    let (db, engine) = seed_copy_engine("seg_focus_candidate");

    // 子前缀候选：已吃部分按候选读音拆，剩余部分回退选举切分
    assert_eq!(
        engine
            .syllable_segmentation_for_candidate("duandian", "du an")
            .join("'"),
        "du'an'dian",
        "毒案 du|an 只吃 duan，剩余 dian 独立成段"
    );
    assert_eq!(
        engine
            .syllable_segmentation_for_candidate("duandian", "duan di")
            .join("'"),
        "duan'di'an",
        "短笛 duan|di 吃 duandi，剩余 an 独立成段"
    );
    // 全覆盖候选：与选举切分一致（视觉无跳变）
    assert_eq!(
        engine
            .syllable_segmentation_for_candidate("duandian", "duan dian")
            .join("'"),
        "duan'dian",
        "断点覆盖整条输入，显示与选举一致"
    );
    // 非真前缀候选（拼音与输入不对齐）→ 回退选举切分，不接管
    assert_eq!(
        engine
            .syllable_segmentation_for_candidate("duandian", "zhong guo")
            .join("'"),
        engine.syllable_segmentation("duandian").join("'"),
        "非前缀候选回退选举切分"
    );
    // 空拼音/空输入 → 回退
    assert_eq!(
        engine
            .syllable_segmentation_for_candidate("duandian", "")
            .join("'"),
        engine.syllable_segmentation("duandian").join("'"),
        "空候选拼音回退选举切分"
    );

    seed_copy_cleanup(&db);
}

/// 简拼整词、绿词等既有显示优先级不被焦点候选接管破坏。
#[test]
fn test_segmentation_focus_keeps_existing_priorities() {
    let (db, engine) = seed_copy_engine("seg_focus_priority");

    // ssef 简拼整词：整词显示 s's'e'f（既有 override）
    engine.process("ssef");
    assert_eq!(
        engine.syllable_segmentation("ssef").join("'"),
        "s's'e'f",
        "简拼整词显示保持逐字母"
    );
    // 对它应用焦点候选（拼音 s se f）也不得破坏整词形态之外的既有覆盖语义
    let focused = engine.syllable_segmentation_for_candidate("ssef", "s se f");
    assert_eq!(
        focused.join("'"),
        "s'se'f",
        "s se f 覆盖整条输入时按候选音节拆"
    );

    seed_copy_cleanup(&db);
}

/// 单候选统一消费口径覆盖子前缀形态。
///
/// 病灶：duandian 输入下候选中混入子切分词「毒案」(du an)、「短笛」(duan di)；
/// 选中后**消费了全部 8 个字母**。根因：前端提交路径只调
/// `jianpin_candidate_consumed_len`，它的门禁要求「输入是简拼/混合声母」——
/// duandian 不满足 → 返回 0 → 前端退回旧逻辑「字数 < 音节数」，
/// 毒案 2 字 vs duandian 2 音节 → 判全消费。实际「毒案」的 du|an 只覆盖
/// 前 4 字母，应留 "dian" 继续组词。
///
/// 修复：新增与 `consumed_lens` 同口径的单候选入口，覆盖
/// ①输入 ⊂ 候选拼音 ②候选拼音 ⊂ 输入 ③简拼/混合 ④纠错 ⑤简拼整词，
/// 无法判定才返回 None 让前端退回旧逻辑。
#[test]
fn test_candidate_consumed_len_covers_sub_prefix() {
    let (db, engine) = seed_copy_engine("consumed_unified");

    let cands = engine.process("duandian");
    let find = |t: &str| -> Option<usize> {
        cands
            .iter()
            .position(|c| c.text.as_ref() == t)
            .and_then(|i| engine.candidate_consumed_len_unified("duandian", &cands[i]))
    };
    // 子前缀候选：只吃自己读音覆盖的字母，剩余拼音留作继续组词
    assert_eq!(
        find("毒案"),
        Some(4),
        "毒案=du an 应消费 duan 4 字节留 dian"
    );
    assert_eq!(
        find("短笛"),
        Some(6),
        "短笛=duan di 应消费 duandi 6 字节留 an"
    );
    // 完整词仍全消费
    assert_eq!(find("断点"), Some(8), "断点=duan dian 全消费 8");
    assert_eq!(find("断电"), Some(8), "断电=duan dian 全消费 8");

    // 既有回归锚（同族判例）：jisuan + 急速 只吃 jisu 4 留 an
    let c4 = engine.process("jisuan");
    let fast = c4.iter().find(|c| c.text.as_ref() == "急速");
    assert_eq!(
        fast.and_then(|c| engine.candidate_consumed_len_unified("jisuan", c)),
        Some(4),
        "jisuan + 急速 应消费 4 留 an"
    );
    // 简拼/混合形态的既有路径不变：zhonggr→中国 由前端旧逻辑 + syllable_prefix_len
    // 处理（本方法返回 None 即「不接管」，与改动前 FFI 返回 0 等价）
    let c2 = engine.process("zhonggr");
    let cn = c2.iter().find(|c| c.text.as_ref() == "中国");
    assert_eq!(
        cn.and_then(|c| engine.candidate_consumed_len_unified("zhonggr", c)),
        None,
        "zhonggr 不由统一口径接管（返 None，前端走 syllable_prefix_len 旧路径）"
    );
    // 无法对齐时返回 None（前端退回自组词旧逻辑）
    assert_eq!(
        engine.candidate_consumed_len_unified("", &cands[0]),
        None,
        "空输入应返回 None"
    );

    seed_copy_cleanup(&db);
}

/// consumed_lens 规则③（bendana 案例）：备选切分的词级候选
/// （笨蛋 = ben dan ⊂ bendana，橙词「词+零声母语气词」模式）必须按自身
/// 读音消费 6 字节、留 "a" 继续组词——不得被选举切分边界（ben+da=5）截短
/// 成残留 "na"。规则②既有行为由 max() 构造保证零变化（只延长不缩短），
/// 锚点：zhonggr→中国=6（留 r）。
#[test]
fn test_consumed_prefix_cross_segment_word() {
    let (db, engine) = seed_copy_engine("consumed_bendana");

    let cands = engine.process("bendana");
    let lens = engine.consumed_lens("bendana", &cands);
    let find = |t: &str| -> Option<usize> {
        cands
            .iter()
            .position(|c| c.text.as_ref() == t)
            .map(|i| lens[i])
    };
    assert_eq!(
        find("笨蛋"),
        Some(6),
        "笨蛋=ben dan 应消费 bendan 6 字节留 a"
    );
    assert_eq!(find("本单"), Some(6), "本单=ben dan 同款跨段词");
    assert_eq!(find("笨"), Some(3), "单字按选举边界不变");

    // 反向：输入比候选长——jisuan 选「急速」(ji su)
    // 只应消费 jisu 4 字节留 an；选举边界 ji+suan=6 不得吞掉剩余输入。
    let c4 = engine.process("jisuan");
    let l4 = engine.consumed_lens("jisuan", &c4);
    let i4 = c4.iter().position(|c| c.text.as_ref() == "急速");
    assert_eq!(
        i4.map(|i| l4[i]),
        Some(4),
        "jisuan + 急速 应消费 jisu 4 字节留 an（实际候选: {:?}）",
        c4.iter()
            .take(6)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
    // 同输入下完整词仍全消费
    let i4b = c4.iter().position(|c| c.text.as_ref() == "计算");
    assert_eq!(i4b.map(|i| l4[i]), Some(6), "jisuan + 计算 全消费 6");

    let c2 = engine.process("zhonggr");
    let l2 = engine.consumed_lens("zhonggr", &c2);
    let i2 = c2.iter().position(|c| c.text.as_ref() == "中国");
    assert_eq!(i2.map(|i| l2[i]), Some(6), "规则②锚：zhonggr→中国 6 留 r");
    seed_copy_cleanup(&db);
}

/// zhenbena 语气词整句绿词（「不加撇号组不出 真笨啊」案例）：
/// 裸尾单字母零声母音节两可（半截长音节 jisua→an / 语气词 真笨+啊）——
/// 主路补字预览不动，第二路 DP 共存合并必须产出全消费整句（珍本啊/真笨啊
/// 平分 100:100，任意一条在列即过；选一次即由个人统计定序）。jisua 的
/// 预览路（补字句）不回归。
#[test]
fn test_zhenbena_particle_green() {
    let (db, engine) = seed_copy_engine("zhenbena_particle");
    let cands = engine.process("zhenbena");
    let greens: Vec<&str> = cands
        .iter()
        .filter(|c| c.is_sentence)
        .map(|c| c.text.as_ref())
        .collect();
    assert!(
        greens.contains(&"真笨啊") || greens.contains(&"珍本啊"),
        "zhenbena 应有语气词整句绿词，实际: {:?}",
        greens
    );
    let j = engine.process("jisua");
    assert!(
        !j.iter().any(|c| c.is_sentence),
        "jisua 有延长词证据（计算）→ 统一规则②a 半截语义，不出绿词: {:?}",
        j.iter()
            .take(4)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
    seed_copy_cleanup(&db);
}

/// 预上屏显示跟随首选读音：首选是延长预测时
/// （jisua + 计算 ji'suan），显示必须把末尾字母并入未打完的音节
/// （ji'sua），不能显示成独立音节 ji'su'a——否则用户以为「计算」只消费
/// ji'su、选完还剩 a。完整输入/语气词态维持原切分。
#[test]
fn test_display_segmentation_tracks_extension_word() {
    let (db, engine) = seed_copy_engine("display_extension");

    // 延长态：jisua → 计算(ji suan) 首选 → 显示 ji'sua（a 并入 suan）
    let c = engine.process("jisua");
    assert_eq!(
        c.first().map(|x| x.text.as_ref()),
        Some("计算"),
        "前置：jisua 首选应为延长词 计算"
    );
    assert_eq!(
        engine.syllable_segmentation("jisua").join("'"),
        "ji'sua",
        "延长预测显示须为 ji'sua（末尾字母并入未打完音节）"
    );
    // 完成态不受影响
    engine.process("jisuan");
    assert_eq!(
        engine.syllable_segmentation("jisuan").join("'"),
        "ji'suan",
        "完成态显示不变"
    );
    // 语气词态（无延长词）显示保持逐音节
    engine.process("zhenbena");
    assert_eq!(
        engine.syllable_segmentation("zhenbena").join("'"),
        "zhen'ben'a",
        "语气词态显示保持 zhen'ben'a"
    );
    seed_copy_cleanup(&db);
}

/// ssef 简拼整词（「似是而非」不可达案例）：is_jianpin 判非简拼
/// （se 是音节）→ 混合读法绿词压过真词。补充规则：每字母一字的简拼整词作为
/// 白词竞争读法并入 + 消费走规则④（首字母串==输入 → 全消费）。
#[test]
fn test_ssef_jianpin_whole_word() {
    let (db, engine) = seed_copy_engine("ssef_jp");
    // 诊断：query_tokens 全 Initial 能否命中
    let toks: Vec<crate::pinyin::Token> =
        "ssef".chars().map(crate::pinyin::Token::Initial).collect();
    let jp = engine.dict.query_tokens(&toks);
    eprintln!(
        "[probe] query_tokens(s,s,e,f) 4字命中: {:?}",
        jp.iter()
            .filter(|c| c.text.chars().count() == 4)
            .map(|c| (c.text.as_ref(), c.base_freq))
            .collect::<Vec<_>>()
    );
    let cands = engine.process("ssef");
    let texts: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
    let pos = texts.iter().position(|t| *t == "似是而非");
    assert!(
        pos.is_some(),
        "ssef 应含词库白词「似是而非」，实际前10: {:?}",
        &texts[..texts.len().min(10)]
    );
    let lens = engine.consumed_lens("ssef", &cands);
    assert_eq!(pos.map(|i| lens[i]), Some(4), "简拼整词应全消费 4 字母");
    seed_copy_cleanup(&db);
}

/// 混合声母显示/计数/消费同源（zhgrm 选「中国人民」残留 m 案例）。
///
/// 候选链把 zh 当一个 Shengmu 单位（zh|g|r|m = 4），显示/计数/消费若仍按
/// is_jianpin 的逐字符语义（z|h|g|r|m = 5），4 字候选就会被误判为分步，
/// 只消费 zhgr、残留 m。本测试钉死四条链的 4 单位语义。
#[test]
fn test_hybrid_shengmu_display_count_consume_aligned() {
    let engine = Engine::new();
    let cand = |text: &str, pinyin: &str| crate::dict::Candidate {
        text: text.into(),
        base_freq: 1,
        pinyin: pinyin.into(),
        is_primary: true,
        is_correction: false,
        is_sentence: false,
        is_guess: false,
        is_shortcut: false,
        is_english: false,
        sentence_score: 0,
    };

    // 显示/计数：zh 是一个声母单位（zhgrm = 4，不是 z|h|g|r|m = 5）
    assert_eq!(engine.syllable_count("zhgrm"), 4);
    assert_eq!(
        engine.syllable_segmentation("zhgrm"),
        vec!["zh", "g", "r", "m"]
    );
    // 前缀偏移保持逐字符语义：部分选择（如「在」）只吃 z，不吃 zh。
    assert_eq!(
        engine.syllable_prefix_lens("zhgrm", &[1, 2, 3, 4, 5]),
        vec![1, 2, 3, 4, 5]
    );

    // 完整候选：候选自身覆盖整条输入 → 全消费 5（修复前 4 → 残留 m）
    assert_eq!(
        engine.consumed_lens("zhgrm", &[cand("中国人民", "zhong guo ren min")]),
        vec![5]
    );
    // 部分候选：只吃首字母 z（保留 hgrm 继续组词）
    assert_eq!(engine.consumed_lens("zhgrm", &[cand("在", "zai")]), vec![1]);
    // 部分多字候选按候选自身读音：中国人 = zh,g,r → 4（留 m）；中国 = zh,g → 3（留 rm）
    assert_eq!(
        engine.consumed_lens("zhgrm", &[cand("中国人", "zhong guo ren")]),
        vec![4]
    );
    assert_eq!(
        engine.consumed_lens("zhgrm", &[cand("中国", "zhong guo")]),
        vec![3]
    );

    // sh/ch 同形态；zhmz 是 zh / z+h 真歧义：
    // 完整候选（中华民族）按候选自身读音全消费 4；
    // 部分候选（中华）保持逐字符前缀 2（z|h），不被 zh 多吞一个字母。
    assert_eq!(engine.syllable_count("shgrm"), 4);
    assert_eq!(engine.syllable_count("chgrm"), 4);
    assert_eq!(engine.syllable_count("zhmz"), 3);
    assert_eq!(
        engine.consumed_lens("zhmz", &[cand("中华民族", "zhong hua min zu")]),
        vec![4]
    );
    assert_eq!(
        engine.consumed_lens("zhmz", &[cand("中华", "zhong hua")]),
        vec![2]
    );
}

/// 非 ASCII 输入不得 panic（process("中") 曾在
/// segmentations_prefix_tail 的字节切片处崩）。
#[test]
fn test_non_ascii_input_no_panic() {
    let engine = Engine::new();
    assert!(engine.process("中").is_empty());
    // 中英混合也不得崩（结果不强制为空——前面可能有拼音候选）
    let _ = engine.process("ni中");
    let _ = engine.process("你a");
    assert!(crate::pinyin::segmentations_prefix_tail("中").is_empty());
    let _ = engine.syllable_count("中");
    let _ = engine.syllable_segmentation("中");
}

/// 纯简拼撇号：查询、计数、消费三链一致。
#[test]
fn test_jianpin_apostrophe_white_words_and_consumption() {
    let mut engine = Engine::new();
    engine.learn("你好", "ni hao");
    engine.learn("社会", "she hui");
    engine.learn("中国人民", "zhong guo ren min");
    engine.learn("在韩国人民", "zai han guo ren min");
    engine.learn("是", "shi");
    engine.learn("三", "san");

    let has = |cands: &[crate::dict::Candidate], text: &str| {
        cands.iter().any(|c| c.text.as_ref() == text)
    };
    let nh = engine.process("nh");
    let n_h = engine.process("n'h");
    assert!(has(&nh, "你好"), "nh 应含 你好");
    assert!(
        has(&n_h, "你好"),
        "n'h 应含 你好（撇号不得让白词查询整串失败）"
    );
    assert!(
        has(&engine.process("z'g'rm"), "中国人民"),
        "z'g'rm 应含 中国人民"
    );
    // zh'g'rm 的撇号显式断开 zh：按 5 个首字母解释（z,h,g,r,m），
    // 4 字「中国人民」不是它的候选；5 字「在韩国人民」才是。
    assert!(
        has(&engine.process("zh'g'rm"), "在韩国人民"),
        "zh'g'rm 应按 5 个首字母命中 在韩国人民"
    );

    assert_eq!(engine.syllable_count("n'h"), 2);
    assert_eq!(engine.syllable_segmentation("n'h"), vec!["n", "h"]);
    assert_eq!(engine.syllable_prefix_lens("n'h", &[1, 2]), vec![2, 3]);

    let cand = |text: &str, pinyin: &str| crate::dict::Candidate {
        text: text.into(),
        base_freq: 1,
        pinyin: pinyin.into(),
        is_primary: true,
        is_correction: false,
        is_sentence: false,
        is_guess: false,
        is_shortcut: false,
        is_english: false,
        sentence_score: 0,
    };

    // 撇号显式边界：n'h 选 你好 全消费 3 字节；选 你 只吃 n'（2 字节，留 h）
    assert_eq!(
        engine.consumed_lens("n'h", &[cand("你好", "ni hao")]),
        vec![3]
    );
    assert_eq!(engine.consumed_lens("n'h", &[cand("你", "ni")]), vec![2]);

    // 无撇号 shengmu：sh 选「是」= sh 一个声母，全消费 2；
    // 选「三」= s 首字母，只吃 1（留 h）。
    assert_eq!(engine.consumed_lens("sh", &[cand("是", "shi")]), vec![2]);
    assert_eq!(engine.consumed_lens("sh", &[cand("三", "san")]), vec![1]);

    // z'g'rm 的 4 单位候选全消费 6；zh'g'rm 按 5 个首字母全消费 7
    assert_eq!(
        engine.consumed_lens("z'g'rm", &[cand("中国人民", "zhong guo ren min")]),
        vec![6]
    );
    assert_eq!(
        engine.consumed_lens("zh'g'rm", &[cand("在韩国人民", "zai han guo ren min")]),
        vec![7]
    );
}

/// 词性语法模型：的/地/得 上下文裁决（PFR 侧表）。
///
/// 种子随包带 `x1dict_pos` / `x1func_ctx`。lift 口径：`(a,v)` 语境下
/// 地 的 lift 远高于的（PFR：地 3948 / 的 12771，但的全局占比 ~96%），
/// 所以“认真 地 思考 / 用力 地 推”应翻到首位；“认真 的 态度”保持的。
#[test]
fn test_grammar_de_di_on_seed() {
    let db = seed_copy("grammar_de");
    if crate::test_tmp::skip_without_grammar_tables() {
        return;
    }
    let engine = Engine::new_with_db(&db).expect("open seed copy");
    assert_eq!(top1(&engine, "renzhendesikao"), "认真地思考");
    assert_eq!(top1(&engine, "renzhendekaolv"), "认真地考虑");
    assert_eq!(top1(&engine, "yonglidetui"), "用力地推");
    assert_eq!(top1(&engine, "renzhendetaidu"), "认真的态度");
    seed_copy_cleanup(&db);
}

/// 在/再 词级上下文（DP 用整词“我在/我再”时也要能翻）。
#[test]
fn test_grammar_zai_qu_on_seed() {
    let db = seed_copy("grammar_zai");
    if crate::test_tmp::skip_without_grammar_tables() {
        return;
    }
    let engine = Engine::new_with_db(&db).expect("open seed copy");
    assert_eq!(top1(&engine, "wozaiqu"), "我再去");
    assert_eq!(top1(&engine, "wozaijia"), "我在家");
    assert_eq!(top1(&engine, "quguo"), "去过");
    seed_copy_cleanup(&db);
}

/// 简拼串豁免中位游离首字母惩罚（quxiaolzggnma 案例，裸输入形态）。
///
/// 病灶：连续打 quxiaolzggn → 切分 qu'xiao'l'z'g'g'n（首选「取消了这个功能」
/// 正常），追加 ma → quxiaolzggnma 切分崩成 qu'xi'ao'l'z'g'g'n'ma，首选错成
/// 「去西澳了这个功能吗」。根因：中位游离首字母惩罚（zhenbena 案例引入）
/// 只看「前后曾有完整音节」，把 l/z/g/g/n 这些**互为邻居的简拼首字母**判成
/// 碎片化，penalty_count=5 → 正确切分得分被压到 1，错误切分 qu|xi|ao|... 以
/// 2 分抢走选举。
///
/// 修复：紧邻位置也是「非音节单字母」时视为简拼串，豁免惩罚；孤立单字母
/// （zhen|b|e|na 的 b，右邻居 e 是有效音节）惩罚照旧。
#[test]
fn test_initial_run_exempts_medial_penalty() {
    let db = seed_copy("initial_run_exempt");
    let mut engine = Engine::new_with_db(&db).expect("open seed copy");

    // ① 裸输入追加 ma 后切分必须保持 qu|xiao（不得重拆成 qu|xi|ao）
    let seg = engine.syllable_segmentation("quxiaolzggnma").join("|");
    assert!(
        seg.starts_with("qu|xiao|"),
        "裸输入追加 ma 后切分须保持 qu|xiao，实际: {}",
        seg
    );

    // ② 追加前后「取消」都必须存活为候选（不得被错误路径挤出）
    for input in ["quxiaolzggn", "quxiaolzggnma"] {
        let cands = engine.process(input);
        assert!(
            cands.iter().take(5).any(|c| c.text.starts_with("取消")),
            "{} 前 5 候选中须有取消路径，实际: {:?}",
            input,
            cands
                .iter()
                .take(5)
                .map(|c| c.text.as_ref())
                .collect::<Vec<_>>()
        );
    }

    // ③ 用户真实场景（学过「这个」「功能」）下首选必须是完整正确整句
    engine.commit("z'g", "这个", None, false);
    engine.commit("g'n", "功能", None, false);
    engine.commit("z'g", "这个", None, false);
    engine.commit("g'n", "功能", None, false);
    let cands = engine.process("quxiaolzggnma");
    assert_eq!(
        cands.first().map(|c| c.text.as_ref()),
        Some("取消了这个功能吗"),
        "学过 这个/功能 后首选应为 取消了这个功能吗，实际: {:?}",
        cands
            .iter()
            .take(4)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );

    // ④ 孤立中位单字母（zhen|b|e|na）的惩罚仍生效：不得因豁免破坏语气词整句
    let zhen = engine.process("zhenbena");
    assert!(
        zhen.iter().any(|c| c.is_sentence),
        "zhenbena 仍应有语气词整句绿词（孤立 b 不在简拼串内）"
    );

    // ⑤ 显式分隔符输入同样受益（用户手动切分时也走同一打分口径）
    let seg_explicit = engine
        .syllable_segmentation("qu'xiao'l'z'g'g'n'ma")
        .join("|");
    assert!(
        seg_explicit.starts_with("qu|xiao|"),
        "显式分隔符输入切分须保持 qu|xiao，实际: {}",
        seg_explicit
    );

    seed_copy_cleanup(&db);
}
