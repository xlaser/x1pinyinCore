//! 长程关联能力实测：量化上文关联的距离衰减、干扰词与边界。

use super::tests_query::{seed_copy, seed_copy_cleanup};
use super::Engine;

/// 30 个不重复的日常物名词——刻意选与「出发/处罚/触发」无词库搭配的词，
/// 保证扫描测到的是「位置衰减」而不是干扰词自身的关联证据。
const DISTRACTORS: &[&str] = &[
    "桌子",
    "椅子",
    "窗户",
    "杯子",
    "瓶子",
    "毛巾",
    "牙刷",
    "沙发",
    "地毯",
    "台灯",
    "书架",
    "冰箱",
    "洗衣机",
    "电视",
    "电脑",
    "键盘",
    "鼠标",
    "耳机",
    "充电器",
    "雨伞",
    "铅笔",
    "本子",
    "橡皮",
    "尺子",
    "水杯",
    "台历",
    "花盆",
    "窗帘",
    "拖鞋",
    "衣架",
];

/// 另一组同长度的抽象词（用于验证「干扰词类型无关、只看位置」）。
const ABSTRACT: &[&str] = &[
    "情况", "方式", "过程", "结果", "关系", "水平", "能力", "内容", "形式", "状态", "特点", "意义",
    "作用", "范围", "条件", "标准", "程度", "方面", "角度", "阶段", "方向", "目标", "任务", "责任",
    "价值", "影响", "效果", "经验", "认识", "态度",
];

fn seed_engine(tag: &str) -> (String, Engine) {
    let db = seed_copy(tag);
    let mut engine = Engine::new_with_db(&db).expect("open seed copy");
    engine.set_smart_word_zh_enabled(false);
    (db, engine)
}

fn build_ctx(engine: &mut Engine, source: &str, distance: usize, pool: &[&str]) {
    engine.context_clear_for_test();
    engine.context_push_for_test(source);
    for i in 0..distance {
        engine.context_push_for_test(pool[i % pool.len()]);
    }
}

fn bonus(engine: &Engine, cands: &[crate::dict::Candidate], text: &str, input: &str) -> u64 {
    cands
        .iter()
        .find(|c| c.text.as_ref() == text)
        .map(|c| engine.score_breakdown(input, c).bigram_bonus)
        .unwrap_or(0)
}

#[test]
fn test_longrange_pair_association_distance_sweep() {
    let (db, mut engine) = seed_engine("lr_sweep");
    println!("\n=== 候选层：种子关联 (机场→出发)=11，距离扫描 ===");
    println!("  d | bigram(出发) | bigram(处罚) | top1");
    for d in [0usize, 1, 2, 3, 5, 8, 10, 12, 15, 20, 24, 27, 28, 29, 30] {
        build_ctx(&mut engine, "机场", d, DISTRACTORS);
        let cands = engine.process("chufa");
        let top1 = cands
            .first()
            .map(|c| c.text.to_string())
            .unwrap_or_default();
        println!(
            "{:>3} | {:>12} | {:>12} | {}",
            d,
            bonus(&engine, &cands, "出发", "chufa"),
            bonus(&engine, &cands, "处罚", "chufa"),
            top1
        );
    }
    seed_copy_cleanup(&db);
}

#[test]
fn test_longrange_distractor_type_is_positional_only() {
    let (db, mut engine) = seed_engine("lr_type");
    println!("\n=== 干扰词类型无关性（同距离，物名词 vs 抽象词）===");
    for d in [1usize, 5, 10, 20] {
        build_ctx(&mut engine, "机场", d, DISTRACTORS);
        let a = engine.process("chufa");
        let object_bonus = bonus(&engine, &a, "出发", "chufa");

        build_ctx(&mut engine, "机场", d, ABSTRACT);
        let b = engine.process("chufa");
        let abstract_bonus = bonus(&engine, &b, "出发", "chufa");
        println!(
            "d={d:>2}  物名={object_bonus}  抽象={abstract_bonus}  {}",
            if object_bonus == abstract_bonus {
                "完全相同（只看位置，不看词义）"
            } else {
                "不同（存在语义/统计差异）"
            }
        );
    }
    seed_copy_cleanup(&db);
}

#[test]
fn test_longrange_window_truncation_drops_source() {
    let (db, mut engine) = seed_engine("lr_window");
    println!("\n=== 30 词窗口截断（源词被挤掉即归零）===");
    for (label, source, d) in [
        ("源在窗口内(d=28)", "机场", 28usize),
        ("源刚好被挤掉(d=29)", "机场", 29),
        ("源明显出窗(d=30)", "机场", 30),
    ] {
        build_ctx(&mut engine, source, d, DISTRACTORS);
        let cands = engine.process("chufa");
        println!(
            "{label}: ctx_len={} bigram(出发)={} top1={}",
            engine.context_vec_for_test().len(),
            bonus(&engine, &cands, "出发", "chufa"),
            cands
                .first()
                .map(|c| c.text.to_string())
                .unwrap_or_default()
        );
    }
    seed_copy_cleanup(&db);
}

#[test]
fn test_longrange_sentence_boundary_clears_evidence() {
    let (db, mut engine) = seed_engine("lr_boundary");
    build_ctx(&mut engine, "机场", 2, DISTRACTORS);
    let with = engine.process("chufa");
    engine.debug_clear_context();
    let without = engine.process("chufa");
    println!(
        "\n=== 句边界：清栈前 bigram(出发)={} top1={} / 清栈后 bigram(出发)={} top1={}",
        bonus(&engine, &with, "出发", "chufa"),
        with.first().map(|c| c.text.to_string()).unwrap_or_default(),
        bonus(&engine, &without, "出发", "chufa"),
        without
            .first()
            .map(|c| c.text.to_string())
            .unwrap_or_default()
    );
    seed_copy_cleanup(&db);
}

/// 绿词 DP 的可见范围：句首段只看 `self.context` 最后 3 个词。
/// 把「机场」放在不同距离（后面跟物名词），看 `mashangchufa` 是否还选用「出发」。
#[test]
fn test_longrange_sentence_dp_visible_range() {
    let (db, mut engine) = seed_engine("lr_dp");
    println!("\n=== 绿词 DP：句首只读最后 3 个上文词 ===");
    for d in [0usize, 1, 2, 3, 4, 5, 6, 8, 10, 12] {
        build_ctx(&mut engine, "机场", d, DISTRACTORS);
        let cands = engine.process("mashangchufa");
        let top1 = cands
            .first()
            .map(|c| c.text.to_string())
            .unwrap_or_default();
        let green_is_chufa = cands
            .iter()
            .take(3)
            .any(|c| c.is_sentence && c.text.as_ref() == "马上出发");
        println!(
            "d={d}  机场在上下文距末位 {}  top1={}  马上出发在前3={}",
            d + 1,
            top1,
            green_is_chufa
        );
    }
    // 性能诊断（不设断言）：歧义段长程扫描的每键开销。
    build_ctx(&mut engine, "机场", 3, DISTRACTORS);
    let t0 = std::time::Instant::now();
    for _ in 0..200 {
        std::hint::black_box(engine.process("mashangchufa"));
    }
    let with_ctx = t0.elapsed() / 200;
    engine.debug_clear_context();
    let t1 = std::time::Instant::now();
    for _ in 0..200 {
        std::hint::black_box(engine.process("mashangchufa"));
    }
    let without_ctx = t1.elapsed() / 200;
    // 隔离测量：同样 4 词上文、但全部无证据（桌子 vs 机场）——两者长程
    // 扫描次数相同，差值只反映 DP 打分/剪枝差异，可区分「上下文通道开销」
    // 与「长程扫描开销」。
    build_ctx(&mut engine, "桌子", 3, DISTRACTORS);
    let t2 = std::time::Instant::now();
    for _ in 0..200 {
        std::hint::black_box(engine.process("mashangchufa"));
    }
    let no_evidence_ctx = t2.elapsed() / 200;
    println!(
        "性能：4 词上文(机场) {with_ctx:?}/键，4 词上文(无证据) {no_evidence_ctx:?}/键，无上文 {without_ctx:?}/键"
    );

    seed_copy_cleanup(&db);
}

/// 长程语境验收：句中歧义词也能看见更远的上文，并带噪声门。
///
/// 实测射程：语境词与歧义词之间隔 0-6 个词（7 词窗口）仍能翻盘，
/// 隔 8 个词起失效（0.85^d 衰减到证据差不足以翻越 LM 先验）。
#[test]
fn test_p0_sentence_longrange_gates_and_range() {
    let (db, mut engine) = seed_engine("lr_p0");

    // 强证据（机场→出发=11，多字内容词）：d=0/3/6 都必须选「出发」。
    for d in [0usize, 3, 6] {
        build_ctx(&mut engine, "机场", d, DISTRACTORS);
        let cands = engine.process("mashangchufa");
        assert_eq!(
            cands
                .first()
                .map(|c| c.text.to_string())
                .unwrap_or_default(),
            "马上出发",
            "机场在 d={d} 时应影响句中歧义（长程语境目标）"
        );
    }

    // 射程边界：d=8 起衰减到不足以翻盘，回到默认「处罚」。
    build_ctx(&mut engine, "机场", 8, DISTRACTORS);
    let far = engine.process("mashangchufa");
    assert_eq!(
        far.first().map(|c| c.text.to_string()).unwrap_or_default(),
        "马上处罚",
        "d=8 已超出有效射程，不应翻盘"
    );

    // 无证据：领导语境保持默认。
    build_ctx(&mut engine, "领导", 0, DISTRACTORS);
    let none = engine.process("mashangchufa");
    assert_eq!(
        none.first().map(|c| c.text.to_string()).unwrap_or_default(),
        "马上处罚",
        "无证据语境必须保持默认"
    );

    // 低计数噪声门：结果→出发=5（跨义项，如「从结果出发」）不得翻盘。
    build_ctx(&mut engine, "结果", 0, DISTRACTORS);
    let lowcount = engine.process("mashangchufa");
    assert_eq!(
        lowcount
            .first()
            .map(|c| c.text.to_string())
            .unwrap_or_default(),
        "马上处罚",
        "低计数种子搭配不得推翻默认读法"
    );

    // 单字虚词门：在→出发=12（「在出发前」句法关联）不得翻盘。
    build_ctx(&mut engine, "在", 0, DISTRACTORS);
    let functionword = engine.process("mashangchufa");
    assert_eq!(
        functionword
            .first()
            .map(|c| c.text.to_string())
            .unwrap_or_default(),
        "马上处罚",
        "单字虚词关联不得作为长程话题证据"
    );

    seed_copy_cleanup(&db);
}

/// 读取 A/B 开关（仅测试构建存在）。
fn set_longrange(on: bool) {
    // 开关定义在 sentence.rs 的私有 mod 内，测试同 crate 可见性受限——
    // 由 Engine 暴露的测试钩子切换（见 engine/mod.rs）。
    // 本模块测「长程语境」单旋钮：测量期钉死后向词格重打分为关——
    // 后向右文证据同样会移动 A/B 的「前」基线（如 shuomingyan 在
    // ctx=[名人] 下会被后向证据提前翻正），不钉死则矩阵对比的就不是
    // 长程贡献。
    super::Engine::set_backward_enabled_for_test(false);
    super::Engine::set_longrange_enabled_for_test(on);
}

/// 在给定上文栈下跑一次 process，返回首候选文本。
fn run_with_ctx(engine: &mut Engine, ctx: &[&str], input: &str) -> String {
    engine.context_clear_for_test();
    for w in ctx {
        engine.context_push_for_test(w);
    }
    engine
        .process(input)
        .first()
        .map(|c| c.text.to_string())
        .unwrap_or_default()
}

/// 多族 A/B 对照矩阵：同一构建、同一 DB，只切换长程语境证据开关，
/// 证明「关闭时看不见句中上文，开启后能看见」。
///
/// 选例标准：种子有强证据（≥8、多字内容词）且正确读法不是组内最高频
/// ——只有这种例子才能显示「默认错 → 带语境对」。覆盖科技/工具/内容/办公/
/// 学习/口语等多个领域，歧义词位于句中第 2-4 段。
#[test]
fn test_p0_ab_matrix_other_families() {
    let (db, mut engine) = seed_engine("lr_ab");

    // (上文, 输入, 期望带语境, 期望无语境=旧行为)
    let cases: &[(&[&str], &str, &str, &str)] = &[
        (&["轮胎"], "zixingchechongqi", "自行车充气", "自行车重启"),
        (&["等待"], "xitongchaoshile", "系统超时了", "系统超市了"),
        (&["加密"], "wenjianjiemi", "文件解密", "文件揭秘"),
        (&["词频"], "moxingdafen", "魔性打分", "魔性大分"),
        (&["主要靠"], "chengjikaotianfu", "成绩考天赋", "成绩考天父"),
        (&["混合"], "zhichijianpin", "支持简拼", "支持减贫"),
        (&["市井"], "chengshiyanhuo", "城市烟火", "城市焰火"),
        (&["网红"], "tanmeishi", "谈美食", "谈没事"),
        (&["名人"], "shuomingyan", "说名言", "说明言"),
        (&["党员"], "zhaodaotongzhi", "找到同志", "找到通知"),
        (&["身边"], "womendetongshi", "我们的同事", "我们的同时"),
        (&["管制"], "shiyongdaoju", "使用刀具", "使用道具"),
    ];

    println!("\n=== 长程 A/B 对照（同一构建，仅切换长程开关）===");
    println!(
        "{:<8} {:<20} {:<14} {:<14} 结果",
        "上文", "输入", "关(默认)", "开"
    );
    let mut improved = 0;
    let mut failed: Vec<String> = Vec::new();
    for (ctx, input, want_on, want_off) in cases {
        set_longrange(false);
        let before = run_with_ctx(&mut engine, ctx, input);
        set_longrange(true);
        let after = run_with_ctx(&mut engine, ctx, input);
        let ok = before == *want_off && after == *want_on;
        if ok {
            improved += 1;
        } else {
            failed.push(format!(
                "ctx={ctx:?} in={input} 前={before} 后={after} 期望 前={want_off} 后={want_on}"
            ));
        }
        println!(
            "{:<8} {:<20} {:<14} {:<14} {}",
            ctx.join("|"),
            input,
            before,
            after,
            if ok { "翻正" } else { "未过" }
        );
    }
    println!("----- {improved}/{} 例由长程语境翻正 -----", cases.len());
    assert!(
        failed.is_empty(),
        "A/B 矩阵不符：\n  {}",
        failed.join("\n  ")
    );

    // 负例：无证据/低计数/虚词/无关上文都必须保持默认（不得误翻）。
    let negatives: &[(&[&str], &str, &str)] = &[
        (&["领导"], "zixingchechongqi", "自行车重启"),
        (&["桌子"], "zixingchechongqi", "自行车重启"),
        (&["结果"], "mashangchufa", "马上处罚"),
        (&["在"], "mashangchufa", "马上处罚"),
        (&["我们"], "mashangchufa", "马上处罚"),
    ];
    for (ctx, input, want) in negatives {
        set_longrange(true);
        let got = run_with_ctx(&mut engine, ctx, input);
        assert_eq!(got, *want, "负例被误翻：ctx={ctx:?} in={input}");
    }
    println!("负例 {} 条全部保持默认", negatives.len());

    // 距离衰减：轮胎→充气 在中间隔 0/2/5 个词时仍生效，隔 8 个词失效。
    println!("\n=== 距离衰减（轮胎→充气）===");
    for d in [0usize, 2, 5, 8] {
        set_longrange(true);
        engine.context_clear_for_test();
        engine.context_push_for_test("轮胎");
        for i in 0..d {
            engine.context_push_for_test(DISTRACTORS[i % DISTRACTORS.len()]);
        }
        let got = engine
            .process("zixingchechongqi")
            .first()
            .map(|c| c.text.to_string())
            .unwrap_or_default();
        println!("隔 {d} 个词 → {got}");
        if d <= 5 {
            assert_eq!(got, "自行车充气", "d={d} 应仍生效");
        } else {
            assert_eq!(got, "自行车重启", "d={d} 应已失效");
        }
    }

    set_longrange(true);
    seed_copy_cleanup(&db);
}

/// 长程证据的每键开销基准（诊断，不设断言）：同一构建/同一 context，
/// 仅切换 A/B 开关，隔离长程机制的增量成本。
///
/// 注意：这里量的是长程证据增量，不含既有的候选层上下文通道
/// （`context_bonus` 的 collocation/bigram 查询——那一部分与本机制无关，
/// 开关关闭时也存在）。
#[test]
fn test_longrange_perf_ab() {
    let (db, mut engine) = seed_engine("lr_perf");
    let inputs = [
        ("mashangchufa", "4 音节，1 个歧义段"),
        ("zixingchechongqi", "6 音节，1 个歧义段"),
        ("wodemudishixuexi", "7 音节，1 个歧义段（mu di）"),
        (
            "womenmingtianxiawuqukanidianying",
            "14 音节，0 个歧义段（dian ying 比 7.3 超门）",
        ),
        ("woxiangheshui", "5 音节，1 个歧义段（he shui）"),
    ];
    println!("\n=== 长程证据 A/B 计时（300 次平均，release，4 词上文）===");
    println!(
        "{:<34} {:<22} {:>9} {:>9} {:>8}",
        "输入", "形态", "开(µs)", "关(µs)", "增量(µs)"
    );
    for (input, note) in inputs {
        build_ctx(&mut engine, "机场", 3, DISTRACTORS);
        for _ in 0..50 {
            std::hint::black_box(engine.process(input));
        }
        set_longrange(true);
        let t0 = std::time::Instant::now();
        for _ in 0..300 {
            std::hint::black_box(engine.process(input));
        }
        let on_us = t0.elapsed().as_secs_f64() * 1e6 / 300.0;
        set_longrange(false);
        let t1 = std::time::Instant::now();
        for _ in 0..300 {
            std::hint::black_box(engine.process(input));
        }
        let off_us = t1.elapsed().as_secs_f64() * 1e6 / 300.0;
        println!(
            "{:<34} {:<22} {:>9.1} {:>9.1} {:>8.1}",
            input,
            note,
            on_us,
            off_us,
            on_us - off_us
        );
    }
    set_longrange(true);
    seed_copy_cleanup(&db);
}
