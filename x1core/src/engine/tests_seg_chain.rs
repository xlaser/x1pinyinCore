//! 切分选举误判守护：高频单词不得压过更自然的词组链。

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

/// 核心：有了高频「首钢」词，`shougangeili` 仍必须解出「手感给力」。
///
/// 机制：若只按选举定序会被 首钢(1355) 拉向 gang|ei，DP 只看到该切分 →
/// 首钢诶里；备选 gan|gei 的词跨度更多 → 触发仲裁 → DP 按语言证据选出
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

/// 反例：当选读法词组链不劣于备选时不触发仲裁，既有结果不回归。
///
/// - `nizaiganmane`：当选 [ni,zai,gan,ma,ne] 词跨度 6 > 备选 3 → 不触发
///   （既定期望「你在干嘛呢」）。
/// - `wananle`：当选 [wan,an,le] 跨度 2 > 备选 [wa,nan,le] 1 → 不触发
///   （既定期望「晚安了」）。
#[test]
fn test_arbitration_not_triggered_when_elected_chain_is_better() {
    let (db, engine) = seed_copy_engine("seg_chain_negative");

    assert_eq!(
        top1(&engine, "nizaiganmane"),
        "你在干嘛呢",
        "nizaiganmane 既有结果不回归（当选链更优，不仲裁）"
    );
    let wan = top1(&engine, "wananle");
    assert!(
        wan.starts_with("晚安"),
        "wananle 既有结果不回归（当选链更优，不仲裁），实际 {wan}"
    );

    seed_copy_cleanup(&db);
}

fn seed_copy_engine(tag: &str) -> (String, Engine) {
    let db = seed_copy(tag);
    let mut engine = Engine::new_with_db(&db).expect("open seed copy");
    engine.set_smart_word_zh_enabled(false);
    (db, engine)
}

/// 记忆相对质量门：误选留下的记忆条目（链路更差）不应压过更自然的整句。
///
/// 直接注入 green_memo（不经 commit，隔离掉「commit 会同时学习子词 bigram」
/// 这个副作用——那是另一条设计语义）：条目「首钢诶里」词组链只有 1 个多字段，
/// 而 DP 最优句「手感给力」有 2 个 → 条目不抢首位，但保留可达（末尾仍在）。
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

/// 同长白词在场时绿词不展示。
///
/// 场景：`haowuyc` → 「毫无用处（白）+ 好物因此（绿）」，绿词扎眼。
/// 120 个「4 字整词 + 尾段简拼」构造样本实测：有同长白词时 74 例绿词被压到
/// 第 2 位，74/74 的 top1 白词都是该输入对应的真实词，绿词全是
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

/// reading_preference_cmp 全序回归守护。
///
/// 若选择史层带成对 20× 词频守卫，三切分下 A/B、B/C、A/C 各自判定可成环
/// （sort_by 全序违规 panic，等价于用户积累 input_mappings 后打字崩溃）。
/// 证据键 = 纯逐侧函数（bigram, 谱地板门控 counts），本测试钉住两个
/// 不变量：①裁决对称（A<B ⇔ B>A）；②等键（无证据）返回 None 回退
/// 窗口/结构层。谱地板（freq≥500 才计选择史）的逐侧门控同时保住
/// 反感 997/方案 3455 同谱段的选择史照常生效。
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

/// consumed_lens 规则③：备选切分的词级候选
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
    assert_eq!(find("笨蛋"), Some(6), "笨蛋=ben dan 应消费 bendan 6 字节留 a");
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
        c4.iter().take(6).map(|c| c.text.as_ref()).collect::<Vec<_>>()
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

/// zhenbena 语气词整句绿词（不加撇号也要能组出「真笨啊」）：
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
        j.iter().take(4).map(|c| c.text.as_ref()).collect::<Vec<_>>()
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

/// ssef 简拼整词（「似是而非」应可达）：is_jianpin 判非简拼
/// （se 是音节）→ 混合读法绿词压过真词。补充规则：每字母一字的简拼整词作为
/// 白词竞争读法并入 + 消费走规则④（首字母串==输入 → 全消费）。
#[test]
fn test_ssef_jianpin_whole_word() {
    let (db, engine) = seed_copy_engine("ssef_jp");
    // 诊断：query_tokens 全 Initial 能否命中
    let toks: Vec<crate::pinyin::Token> = "ssef"
        .chars()
        .map(crate::pinyin::Token::Initial)
        .collect();
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
