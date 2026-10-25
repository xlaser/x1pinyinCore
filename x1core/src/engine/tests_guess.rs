//! Engine 测试 · 紫词（前缀补全猜测词：末音节已完整时对后续输入的预测候选）。

use super::*;

/// 构造式测试 DB：保证 yong 节点下挂 hu（精确匹配）和 huai（前缀补全才拉到）。
/// 避免依赖种子 DB 的真实数据状态。
fn make_guess_test_db() -> std::path::PathBuf {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_guess_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        s.import_x1dict_batch(&[
            // 精确匹配 [yong, hu] 命中的词（白词）
            X1DictWord {
                simplified: "用户".into(),
                pinyin: "yong hu".into(),
                freq: 100,
                ..Default::default()
            },
            X1DictWord {
                simplified: "拥护".into(),
                pinyin: "yong hu".into(),
                freq: 90,
                ..Default::default()
            },
            // 末音节更长（yong huai），需由前缀补全拉来（紫词）
            X1DictWord {
                simplified: "用坏".into(),
                pinyin: "yong huai".into(),
                freq: 500,
                ..Default::default()
            },
            // 又一个末音节更长（yong huang），也是紫词
            X1DictWord {
                simplified: "用谎".into(),
                pinyin: "yong huang".into(),
                freq: 50,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    tmp
}

/// 精确命中即不出猜测：输入已精确命中等长词（用户/拥护）时，一切猜测
/// （末音节延伸 用坏/用谎 + 长词延展）一律不出——紫词不给确定让位
/// （否则精确命中会被十几条「不要X」/「用户X」类紫词刷屏）。
#[test]
fn test_exact_match_silences_guesses() {
    let db = make_guess_test_db();
    let engine = Engine::new_with_db(db.to_str().unwrap()).unwrap();
    let cands = engine.process("yonghu");

    assert!(
        cands.iter().any(|c| c.text.as_ref() == "用户"),
        "应包含「用户」"
    );
    assert!(
        cands.iter().any(|c| c.text.as_ref() == "拥护"),
        "应包含「拥护」"
    );
    for c in &cands {
        assert!(
            !c.is_guess,
            "精确命中时不应有紫词：{} pinyin={} 频={}",
            c.text, c.pinyin, c.base_freq
        );
    }
    assert!(
        !cands.iter().any(|c| c.text.as_ref() == "用坏"),
        "精确命中时「用坏」（末音节延伸猜测）不出"
    );
}

/// 零精确命中才联想：无等长精确词时猜测照常生成、全部染紫、≤3 条。
#[test]
fn test_guess_when_no_exact_match() {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_guess_zero_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        s.import_x1dict_batch(&[
            // 只有 yong huai / yong huang（无 yong hu 精确词）
            X1DictWord {
                simplified: "用坏".into(),
                pinyin: "yong huai".into(),
                freq: 500,
                ..Default::default()
            },
            X1DictWord {
                simplified: "用谎".into(),
                pinyin: "yong huang".into(),
                freq: 50,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
    let cands = engine.process("yonghu");

    let guesses: Vec<&str> = cands
        .iter()
        .filter(|c| c.is_guess)
        .map(|c| c.text.as_ref())
        .collect();
    assert!(
        guesses.contains(&"用坏"),
        "零精确命中应保留「用坏」猜测，实际 {guesses:?}"
    );
    assert!(
        guesses.contains(&"用谎"),
        "零精确命中应保留「用谎」猜测，实际 {guesses:?}"
    );
    assert!(guesses.len() <= 3, "猜测总数 ≤3，实际 {}", guesses.len());
    let _ = std::fs::remove_file(&tmp);
}

/// 猜测数量上限：零精确命中的猜测词无论来多少条，
/// 合并后总数 ≤3（base_freq 降序取前）——防同前缀猜测刷屏。
#[test]
fn test_guess_capped_at_three() {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_guess_cap_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        // 5 个 kai shi X 延展词（无 kai shi 精确词）→ 猜测应被砍到 3
        let words = [
            ("开始工作", 500),
            ("开始上课", 400),
            ("开始出现", 300),
            ("开始行动", 200),
            ("开始实施", 100),
        ];
        let batch: Vec<X1DictWord> = words
            .iter()
            .map(|(t, f)| X1DictWord {
                simplified: (*t).into(),
                pinyin: format!(
                    "kai shi {}",
                    match *t {
                        "开始工作" => "gong zuo",
                        "开始上课" => "shang ke",
                        "开始出现" => "chu xian",
                        "开始行动" => "xing dong",
                        _ => "shi shi",
                    }
                ),
                freq: *f,
                ..Default::default()
            })
            .collect();
        s.import_x1dict_batch(&batch).unwrap();
    }
    let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
    let cands = engine.process("kaishi");
    let guesses: Vec<&str> = cands
        .iter()
        .filter(|c| c.is_guess)
        .map(|c| c.text.as_ref())
        .collect();
    assert!(
        !guesses.is_empty() && guesses.len() <= 3,
        "零精确命中猜测应生成且 ≤3 条，实际 {guesses:?}"
    );
    // freq 降序取前：最高频的 3 条在，第 4 名（开始行动 200）被砍
    assert_eq!(guesses, vec!["开始工作", "开始上课", "开始出现"]);
    let _ = std::fs::remove_file(&tmp);
}

/// 单字前缀延伸猜测上限（「猜测 ≤3 条」统一上限）：
/// ji→间/接/今/京/九 类延伸染紫且 ≤3 条（freq 降序取前）；精确单字不受限。
#[test]
fn test_single_char_guess_capped() {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_guess_schar_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        let words = [
            ("机", "ji", 900),
            ("间", "jian", 500),
            ("接", "jie", 400),
            ("今", "jin", 300),
            ("京", "jing", 200),
            ("九", "jiu", 100),
        ];
        let batch: Vec<X1DictWord> = words
            .iter()
            .map(|(t, p, f)| X1DictWord {
                simplified: (*t).into(),
                pinyin: (*p).into(),
                freq: *f,
                ..Default::default()
            })
            .collect();
        s.import_x1dict_batch(&batch).unwrap();
    }
    let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
    let cands = engine.process("ji");
    let purple: Vec<&str> = cands
        .iter()
        .filter(|c| c.is_guess)
        .map(|c| c.text.as_ref())
        .collect();
    assert_eq!(purple, vec!["间", "接", "今"], "延伸染紫且 freq 降序取前 3");
    assert!(
        cands.iter().any(|c| c.text.as_ref() == "机" && !c.is_guess),
        "精确单字「机」不受限且不染紫"
    );
    let _ = std::fs::remove_file(&tmp);
}

/// 精确匹配充足时不应触发猜测（门槛为「精确命中即不出」，3 条精确的
/// 场景顺带覆盖）。
#[test]
fn test_no_guess_when_exact_match_enough() {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_guess_enough_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        s.import_x1dict_batch(&[
            // 精确 [yong, hu] 命中 ≥3 个 → 不触发猜测
            X1DictWord {
                simplified: "用户".into(),
                pinyin: "yong hu".into(),
                freq: 100,
                ..Default::default()
            },
            X1DictWord {
                simplified: "拥护".into(),
                pinyin: "yong hu".into(),
                freq: 90,
                ..Default::default()
            },
            X1DictWord {
                simplified: "佣户".into(),
                pinyin: "yong hu".into(),
                freq: 80,
                ..Default::default()
            },
            // 这两个虽然 hu starts_with "hu"，但 result.len() >= 3 不触发前缀补全
            X1DictWord {
                simplified: "用坏".into(),
                pinyin: "yong huai".into(),
                freq: 500,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
    let cands = engine.process("yonghu");

    for c in &cands {
        assert!(
            !c.is_guess,
            "精确匹配结果 ≥3 时不应有紫词，但 {} 被标记 is_guess=true",
            c.text
        );
    }
}

/// 末音节不完整（分支 B，如 yongh）不应染紫：那是输入实时提示的合法前缀补全。
#[test]
fn test_no_guess_for_partial_last_syllable() {
    let db = make_guess_test_db();
    let engine = Engine::new_with_db(db.to_str().unwrap()).unwrap();
    // 输入 yongh：末音节 h 不完整（非单字符完整音节场景）
    let cands = engine.process("yongh");

    for c in &cands {
        // 分支 B 不调 mark_guess_candidates，所有候选都不应染紫
        assert!(
            !c.is_guess,
            "分支 B（末音节不完整）是输入实时提示，不应染紫：{} pinyin={}",
            c.text, c.pinyin
        );
    }
}

/// 构造「长词 + 组成单字」DB：用于延长预测的逐键稳定性守护。
fn make_extension_test_db(tag: &str) -> std::path::PathBuf {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_ext_{}_{}_{}.db",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        s.import_x1dict_batch(&[
            X1DictWord {
                simplified: "乱".into(),
                pinyin: "luan".into(),
                freq: 4100,
                ..Default::default()
            },
            X1DictWord {
                simplified: "七".into(),
                pinyin: "qi".into(),
                freq: 3000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "八".into(),
                pinyin: "ba".into(),
                freq: 2000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "糟".into(),
                pinyin: "zao".into(),
                freq: 1000,
                ..Default::default()
            },
            X1DictWord {
                simplified: "乱七八糟".into(),
                pinyin: "luan qi ba zao".into(),
                freq: 715,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    tmp
}

/// 逐键稳定性：luanqi → luanqib → luanqiba 全程「乱七八糟」必须可见，
/// 且中途态必须是紫词（对未敲完输入的预测）。否则 luanqib（混合简拼分支）
/// 无猜测通道 → 紫词消失、候选回退到单字「乱」→ 再打一个字母又出现（闪烁）。
#[test]
fn test_extension_prediction_stable_across_every_key() {
    let db = make_extension_test_db("stable");
    let engine = Engine::new_with_db(db.to_str().unwrap()).unwrap();
    for (input, label) in [
        ("luanqi", "末音节完整"),
        ("luanqib", "混合简拼单字母尾"),
        ("luanqiba", "三音节完整"),
    ] {
        let cands = engine.process(input);
        let pos = cands
            .iter()
            .position(|c| c.text.as_ref() == "乱七八糟")
            .unwrap_or_else(|| {
                panic!(
                    "{label}（{input}）必须能看到延长预测「乱七八糟」，实际：{:?}",
                    cands
                        .iter()
                        .take(5)
                        .map(|c| c.text.as_ref())
                        .collect::<Vec<_>>()
                )
            });
        assert!(
            cands[pos].is_guess,
            "{label}（{input}）中途态的延长预测必须染紫（对后续输入的猜测）"
        );
    }
    // 全拼完：精确命中转白词且排首位
    let full = engine.process("luanqibazao");
    assert_eq!(full[0].text.as_ref(), "乱七八糟", "全拼完应精确命中首位");
    assert!(!full[0].is_guess, "精确命中不再是紫词");
}

/// 碎片绿词让位延长预测：luanqiba 时 DP 会拼出碎片绿词「乱七八」（is_sentence）
/// 并因 is_full_match 抢 pos 0，把真正的延长预测挤到 #2——用户每多打一个
/// 音节就「回退」一次。规则：绿词文本是紫词延长预测的严格前缀时，排到该
/// 紫词之后（只降位不隐藏）。
///
/// 用真实种子副本（seed_copy 复制到 test-tmp 再开）：碎片绿词由 DP + 字符
/// 频率共同产生，构造式小库（仅 5 个词）产不出来——本测试的前提（绿词在场）
/// 必须实证，不能靠假定。
#[test]
fn test_fragment_green_yields_to_prefix_extension() {
    let seed = super::tests_query::seed_copy("green_ext");
    let engine = Engine::new_with_db(&seed).unwrap();
    let cands = engine.process("luanqiba");
    let word_pos = cands
        .iter()
        .position(|c| c.text.as_ref() == "乱七八糟")
        .expect("延长预测应在列");
    let green_pos = cands.iter().position(|c| c.text.as_ref() == "乱七八");
    assert!(
        green_pos.is_some(),
        "前置：真实种子下应拼出碎片绿词「乱七八」（否则本测试无意义），实际：{:?}",
        cands
            .iter()
            .take(6)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        word_pos,
        0,
        "延长预测应在首位（碎片绿词让位），实际列表：{:?}",
        cands
            .iter()
            .take(5)
            .map(|c| c.text.as_ref())
            .collect::<Vec<_>>()
    );
    let gp = green_pos.unwrap();
    assert!(
        gp > word_pos,
        "碎片绿词「乱七八」必须排在「乱七八糟」之后（只降位不隐藏）"
    );
    assert!(cands[gp].is_sentence, "「乱七八」应仍是绿词");
}

/// 全输入覆盖判据的匹配口径：
/// - 单字母单位 = 简拼首字母/半截声母 → 候选音节以该字母开头；
/// - 完整音节单位 → 候选音节**精确相等**（fang hu ≠ fang huan，fanghu 判据不变）；
/// - 输入是候选拼音的严格前缀（长词补全）同样算全覆盖。
#[test]
fn test_covers_all_input_semantics() {
    fn cand(text: &str, pinyin: &str) -> Candidate {
        Candidate {
            text: text.into(),
            base_freq: 0,
            pinyin: pinyin.into(),
            is_primary: true,
            is_correction: false,
            is_sentence: false,
            is_guess: false,
            is_shortcut: false,
            is_english: false,
            sentence_score: 0,
        }
    }
    let army = cand("中国人民解放军", "zhong guo ren min jie fang jun");

    // 长词补全：6 个单位全对齐，候选多出尾部 jun
    let six = ["zhong", "guo", "ren", "min", "jie", "fang"];
    assert_eq!(Engine::pinyin_input_coverage(&army, &six), 6);
    assert!(Engine::covers_all_input(&army, &six));

    // 半截声母尾（zhongguorenminjief）：f 按首字母对齐
    let half = ["zhong", "guo", "ren", "min", "jie", "f"];
    assert_eq!(Engine::pinyin_input_coverage(&army, &half), 6);
    assert!(Engine::covers_all_input(&army, &half));

    // 完整音节 fa/fan 不等于 fang → 覆盖停在第五位（不是补全）
    for last in ["fa", "fan"] {
        let units = ["zhong", "guo", "ren", "min", "jie", last];
        assert_eq!(Engine::pinyin_input_coverage(&army, &units), 5, "{last}");
        assert!(!Engine::covers_all_input(&army, &units), "{last}");
    }

    // fanghu 判据回归：放还(fang huan) 不是 ["fang","hu"] 的全覆盖，防护是
    let fanghu = ["fang", "hu"];
    assert_eq!(
        Engine::pinyin_input_coverage(&cand("放还", "fang huan"), &fanghu),
        1
    );
    assert!(!Engine::covers_all_input(
        &cand("放还", "fang huan"),
        &fanghu
    ));
    assert!(Engine::covers_all_input(&cand("防护", "fang hu"), &fanghu));

    // 短词只覆盖前几位 → 非全覆盖
    let six_units = ["zhong", "guo", "ren", "min", "jie", "fang"];
    assert_eq!(
        Engine::pinyin_input_coverage(&cand("中国", "zhong guo"), &six_units),
        2
    );
    assert!(!Engine::covers_all_input(
        &cand("中国", "zhong guo"),
        &six_units
    ));
}

/// **长词补全压过部分覆盖词**（逐键回退家族）：输入是词库长词的严格前缀时
/// （缺末音节），长词覆盖整条输入，结构层必须优先于「中国」这类只覆盖前
/// 几位的词频短词——此前长词掉进前缀层，被 中国(8534) 按词频压到深位，
/// 逐键输入时表现为「无限接近却回退」。
///
/// 三条覆盖形态：全拼前缀（zhongguorenminjiefang）、简拼+全拼混合
/// （zgrmjief）、显式撇号形态（z'g'r'm'jie'f）。补上末音节 jun 后
/// 精确命中转白词仍居首；有等长精确词时补全不抢位（中国人民）。
#[test]
fn test_long_word_completion_outranks_partial_words() {
    let db = super::tests_query::seed_copy("completion_rank");
    let engine = Engine::new_with_db(&db).expect("seed db");

    for input in ["zhongguorenminjiefang", "zgrmjief", "z'g'r'm'jie'f"] {
        let cands = engine.process(input);
        let top = cands.first().map(|c| c.text.as_ref()).unwrap_or("");
        assert_eq!(
            top,
            "中国人民解放军",
            "{}：长词补全应排首位，实际前 6：{:?}",
            input,
            cands
                .iter()
                .take(6)
                .map(|c| c.text.as_ref())
                .collect::<Vec<_>>()
        );
        assert!(
            cands[0].is_guess,
            "{}：缺末音节的前缀态长词是延长预测（紫词）",
            input
        );
        // 部分覆盖的高频短词必须让位
        let cn = cands
            .iter()
            .position(|c| c.text.as_ref() == "中国")
            .expect("中国应在列");
        assert!(cn > 0, "{}：「中国」（覆盖 2/6）必须在长词之后", input);
        // 碎片绿词（中国人民解放）仍排在长词之后（只降位不隐藏）
        let green = cands
            .iter()
            .position(|c| c.is_sentence && c.text.as_ref() == "中国人民解放")
            .expect("中国人民解放 绿词应在列");
        assert!(
            green > 0 && green < cn,
            "{}：碎片绿词应在长词之后、短词之前（实际 green={} cn={}）",
            input,
            green,
            cn
        );
    }

    // 补上末音节 jun 的首字母 → 精确命中转白词，仍居首（单调收敛）
    let full = engine.process("zhongguorenminjiefangj");
    assert_eq!(full[0].text.as_ref(), "中国人民解放军");
    assert!(!full[0].is_guess, "精确命中不再是紫词");

    // 有等长精确词时补全不抢位：zhongguorenmin → 中国人民（tier 1）
    let exact = engine.process("zhongguorenmin");
    assert_eq!(
        exact[0].text.as_ref(),
        "中国人民",
        "等长精确词必须压过长词补全"
    );

    super::tests_query::seed_copy_cleanup(&db);
}
