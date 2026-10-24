//! Engine 测试 · 智能单词识别（英文单词候选）与英文模式补全查询。

use super::*;

/// 种子库引擎（词表在 dict.db english_words 表，内置模式 Engine::new()
/// 无英文词）。全文件共享一份临时种子副本（测试禁直读仓库种子；
/// 多引擎同库只读共享，避免 16 份 × 57MB 撑爆 TEMP）。
pub(crate) fn en_engine() -> Engine {
    static DB: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    // 并行测试同时打开共享副本会撞 SQLITE_BUSY（打开序列含短暂写锁：
    // pragma/引导），复制+打开串行化（打开为毫秒级，串行不拖慢）。
    static OPEN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = OPEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = DB.get_or_init(|| {
        crate::test_tmp::require_seed_db();
        let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
        let p = crate::test_tmp::dir().join(format!("x1pinyin_en_{}.db", std::process::id()));
        std::fs::copy(seed, &p).expect("copy seed");
        p.to_str().unwrap().to_string()
    });
    let mut e = Engine::new_with_db(db).expect("seed db");
    drop(_guard);
    // 英文模式补全默认关——本文件的测试均为「功能语义」测试，显式开启
    // 英文模式开关；「默认关」本身由 test_smart_word_en_defaults_off 守护。
    e.set_smart_word_en_enabled(true);
    e
}

/// 默认值守护：新部署/新引擎实例的英文模式单词补全默认关闭（默认开启
/// 会对很多人造成困扰）。中文模式开关 smartWordZh 维持默认开。
#[test]
fn test_smart_word_en_defaults_off() {
    // 不直读仓库种子——复制到 test-tmp 再开。
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let tmp = crate::test_tmp::dir().join(format!(
        "x1pinyin_en_default_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::copy(seed, &tmp);
    let e = Engine::new_with_db(tmp.to_str().unwrap()).expect("seed db");
    assert!(
        !e.smart_word_en_enabled(),
        "英文模式单词补全必须默认关闭"
    );
    assert!(
        e.english_complete("cat", 9).is_empty(),
        "默认关时英文补全必须返回空"
    );
    assert!(e.smart_word_zh_enabled(), "中文模式英文候选维持默认开");
    let _ = std::fs::remove_file(&tmp);
}

/// 排位不变量：英文候选之前只允许「拼音覆盖整个输入的
/// 白色候选」（consumed_lens 规则①判据：bushi 覆盖 bus、women 覆盖 women）；
/// 绿/紫/橙与覆盖不了输入的部分匹配白词（哈珀 "hapo" vs "happy"）都必须在其后。
fn assert_en_placement(cands: &[Candidate], input: &str, en: &str) {
    let idx = cands
        .iter()
        .position(|c| c.is_english && c.text.as_ref() == en)
        .unwrap_or_else(|| panic!("{} 英文候选必须出现", en));
    let key: String = input.chars().filter(|c| *c != '\'').collect();
    for c in &cands[..idx] {
        let compact: String = c
            .pinyin
            .chars()
            .filter(|ch| *ch != ' ' && *ch != '\'')
            .collect();
        let full = !c.is_english
            && !c.is_correction
            && !c.is_guess
            && !c.is_sentence
            && compact.starts_with(key.as_str());
        assert!(
            full,
            "英文候选 {} 之前的 [{}] 拼音未覆盖整个输入（pinyin={} input={}）",
            en, c.text, c.pinyin, input
        );
    }
}

#[test]
fn test_dog_exact_tops_or_follows_first_word_level() {
    let e = en_engine();
    let cands = e.process("dog");
    assert_en_placement(&cands, "dog", "dog");
    let idx = cands
        .iter()
        .position(|c| c.is_english && c.text.as_ref() == "dog")
        .unwrap();
    // 英文候选特征：pinyin 恒空
    assert!(cands[idx].pinyin.is_empty());
}

#[test]
fn test_happy_exact_tops_partial_match_and_green() {
    // 「哈珀培养」（绿）/「哈珀」（ha po 部分匹配）都不应压住完整精确
    // 匹配的 happy——happy 必须登顶。
    let e = en_engine();
    let mut cands = vec![
        Candidate {
            text: "哈珀培养".into(),
            base_freq: 100,
            pinyin: "ha po pei yang".into(),
            is_primary: true,
            is_correction: false,
            is_sentence: true,
            is_guess: false,
            is_shortcut: false,
            is_english: false,
            sentence_score: 0,
        },
        Candidate {
            text: "哈珀".into(),
            base_freq: 100,
            pinyin: "ha po".into(),
            is_primary: true,
            is_correction: false,
            is_sentence: false,
            is_guess: false,
            is_shortcut: false,
            is_english: false,
            sentence_score: 0,
        },
    ];
    e.inject_english_candidates("happy", &mut cands);
    assert_eq!(
        cands[0].text.as_ref(),
        "happy",
        "happy 必须登顶（绿/部分匹配让位）"
    );
    assert!(cands[1].is_sentence, "绿词仍在（只是让位）");
    assert_eq!(cands[2].text.as_ref(), "哈珀");
}

#[test]
fn test_full_match_white_anchors_english() {
    // 打拼音出中文的基本盘：拼音与输入完全相等的白色词/单字仍在英文前。
    let e = en_engine();
    let zh = |text: &str, pinyin: &str| Candidate {
        text: text.into(),
        base_freq: 100,
        pinyin: pinyin.into(),
        is_primary: true,
        is_correction: false,
        is_sentence: false,
        is_guess: false,
        is_shortcut: false,
        is_english: false,
        sentence_score: 0,
    };
    // man = 满/慢 的完整拼音 → 英文排其后
    let mut c1 = vec![zh("满", "man"), zh("慢", "man")];
    e.inject_english_candidates("man", &mut c1);
    assert_eq!(c1[0].text.as_ref(), "满");
    assert_eq!(c1[1].text.as_ref(), "man");
    assert!(c1[1].is_english);
    assert_en_placement(&c1, "man", "man");
}

#[test]
fn test_hello_everyday_words_present() {
    // 日常寒暄词（hello 等）词表覆盖回归：build --check 的必备词锚点 +
    // 本用例双守护。
    let e = en_engine();
    let cands = e.process("hello");
    assert!(
        cands
            .iter()
            .any(|c| c.is_english && c.text.as_ref() == "hello"),
        "hello 必须出英文候选（日常词缺口回归）"
    );
}

#[test]
fn test_morning_exact_no_correction_noise() {
    let e = en_engine();
    let cands = e.process("morning");
    // 纠错短路：精确命中英文词表 → 不出橙色纠错噪音
    assert!(
        cands.iter().all(|c| !c.is_correction),
        "morning 命中英文词表后不得产生纠错候选"
    );
    assert_en_placement(&cands, "morning", "morning");
}

#[test]
fn test_exact_hit_suppresses_prefix_completions() {
    let e = en_engine();
    let cands = e.process("dog");
    // 精确命中时不出前缀补全（dog 不出 dogma/doggy 等）
    let english: Vec<&str> = cands
        .iter()
        .filter(|c| c.is_english)
        .map(|c| c.text.as_ref())
        .collect();
    assert_eq!(english, vec!["dog"], "精确命中时英文候选有且仅有一条");
}

#[test]
fn test_mornin_prefix_completion() {
    let e = en_engine();
    let cands = e.process("mornin");
    let english: Vec<&str> = cands
        .iter()
        .filter(|c| c.is_english)
        .map(|c| c.text.as_ref())
        .collect();
    assert!(
        english.contains(&"morning"),
        "mornin 前缀补全必须出 morning，实际 {english:?}"
    );
    assert!(!english.is_empty() && english.len() <= 3);
    assert!(english.iter().all(|w| w.starts_with("mornin")));
}

#[test]
fn test_toggle_off_injects_nothing() {
    let mut e = en_engine();
    e.set_smart_word_zh_enabled(false);
    let cands = e.process("dog");
    assert!(
        cands.iter().all(|c| !c.is_english),
        "开关关闭时不得注入英文候选"
    );
    // 英文模式开关独立：english_complete 不受影响
    assert!(!e.english_complete("cat", 3).is_empty());
}

#[test]
fn test_prefix_subswitch_off_exact_only() {
    // 子开关 smartWordZhPrefix 关：中文模式只出精确命中（mornin 不出补全）
    let mut e = en_engine();
    e.set_smart_word_zh_prefix_enabled(false);
    let cands = e.process("mornin");
    assert!(
        cands.iter().all(|c| !c.is_english),
        "前缀补全关时 mornin 不得出英文补全"
    );
    // 精确命中不受子开关影响
    let cands = e.process("dog");
    assert!(
        cands
            .iter()
            .any(|c| c.is_english && c.text.as_ref() == "dog"),
        "前缀补全关时精确命中照出"
    );
}

#[test]
fn test_priority_subswitch_off_reverts_placement() {
    // 子开关 smartWordZhPriority 关：回退旧排位——英文跟在首个多字候选之后
    //（绿词也算锚）。happy 压不过绿词。
    let mut e = en_engine();
    e.set_smart_word_zh_priority_enabled(false);
    let mut cands = vec![{
        Candidate {
            text: "哈珀培养".into(),
            base_freq: 100,
            pinyin: "x".into(),
            is_primary: true,
            is_correction: false,
            is_sentence: true,
            is_guess: false,
            is_shortcut: false,
            is_english: false,
            sentence_score: 0,
        }
    }];
    e.inject_english_candidates("happy", &mut cands);
    assert_eq!(
        cands[0].text.as_ref(),
        "哈珀培养",
        "英文优先关时绿词保持在前"
    );
    assert_eq!(cands[1].text.as_ref(), "happy", "英文跟在首个多字候选后");
}

#[test]
fn test_shuangpin_mode_also_injects() {
    let mut e = en_engine();
    let scheme = crate::shuangpin::ShuangpinScheme::builtins()[0].clone();
    e.set_shuangpin_scheme(Some(scheme));
    // 注入在 process() 出口统一执行，双拼分支同样生效
    let cands = e.process("dog");
    assert!(
        cands
            .iter()
            .any(|c| c.is_english && c.text.as_ref() == "dog"),
        "双拼模式也必须出英文候选"
    );
}

#[test]
fn test_commit_english_skips_learning() {
    let mut e = en_engine();
    e.commit("dog", "dog", None, true);
    assert!(!e.freq.counts.contains_key("dog"), "英文词不得进 freq 计数");
    assert!(!e.green_memo.contains_key("dog"), "英文词不得进 green_memo");
    assert!(
        !e.user_words_contains_for_test("dog"),
        "英文词不得进 user_words"
    );
}

#[test]
fn test_english_complete_exact_first() {
    let e = en_engine();
    let out = e.english_complete("cat", 9);
    assert!(!out.is_empty());
    assert_eq!(out[0].0, "cat", "精确命中词置顶");
    assert!(out.iter().all(|(w, _)| w.starts_with("cat")));
    // 其余（index 1 起）按 freq 降序；精确置顶允许打破 freq 序
    for pair in out[1..].windows(2) {
        assert!(pair[0].1 >= pair[1].1, "其余按 freq 降序");
    }
    // limit 收敛 + 开关切断
    assert!(e.english_complete("cat", 3).len() <= 3);
    let mut e2 = en_engine();
    e2.set_smart_word_en_enabled(false);
    assert!(e2.english_complete("cat", 9).is_empty());
}

#[test]
fn test_placement_rule_direct() {
    // 直接验证排位规则（构造式候选列表，与 DB 状态无关；
    // 唯一让位者 = 拼音完全匹配的白色候选）
    let e = en_engine();
    fn zh(text: &str, pinyin: &str) -> Candidate {
        Candidate {
            text: text.into(),
            base_freq: 100,
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
    // 场景 1：混合简拼部分匹配白词不作锚（哈珀 "ha po" ≠ "happy"）——英文登顶
    let mut c1 = vec![zh("末日", "mo ri"), zh("摸", "mo")];
    e.inject_english_candidates("dog", &mut c1);
    assert_eq!(c1[0].text.as_ref(), "dog", "部分匹配白词不作锚");
    assert!(c1[0].is_english);
    // 场景 2：单字不作锚 → index 0
    let mut c2 = vec![zh("大", "da"), zh("的", "de")];
    e.inject_english_candidates("dog", &mut c2);
    assert_eq!(c2[0].text.as_ref(), "dog");
    // 场景 3：纠错不作锚（英文仍登顶）
    let mut c3 = vec![zh("打个嗝", "da ge ge")];
    c3[0].is_correction = true;
    e.inject_english_candidates("dog", &mut c3);
    assert_eq!(c3[0].text.as_ref(), "dog");
    // 场景 4：拼音完全匹配白词作锚（man = 满/慢）——英文排其后，
    // space 必须继续提交首个中文候选（打拼音出中文的基本盘）
    let mut c4 = vec![zh("满", "man"), zh("慢", "man")];
    e.inject_english_candidates("man", &mut c4);
    assert_eq!(c4[0].text.as_ref(), "满");
    assert_eq!(c4[1].text.as_ref(), "man");
    assert!(c4[1].is_english);
    // 场景 5：拼音可解释输入不出前缀补全（chi 不出 child）
    let mut c5 = vec![zh("吃", "chi"), zh("迟", "chi")];
    e.inject_english_candidates("chi", &mut c5);
    assert!(c5.iter().all(|c| !c.is_english), "chi 不得注入英文候选");
    // 场景 6：绿词 + 部分匹配白词全部让位——
    // happy 必须登顶（哈珀培养/哈珀 都不配压住完整精确匹配）
    let mut c6 = vec![zh("哈珀培养", "ha po pei yang"), zh("哈珀", "ha po")];
    c6[0].is_sentence = true;
    e.inject_english_candidates("happy", &mut c6);
    assert_eq!(c6[0].text.as_ref(), "happy", "英文精确命中必须登顶");
    assert!(c6[1].is_sentence, "绿词仍在列表（只是让位）");
    // 场景 7：完整匹配锚点即使排在绿词之后也生效（绿让位、白锚保留）
    let mut c7 = vec![zh("门缝", "men feng"), zh("们", "men")];
    c7[0].is_sentence = true;
    e.inject_english_candidates("men", &mut c7);
    assert_eq!(c7[0].text.as_ref(), "门缝");
    assert_eq!(c7[1].text.as_ref(), "们", "拼音完全匹配白词仍锚定英文");
    assert_eq!(c7[2].text.as_ref(), "men");
}

// ── 词频动态调整守护：开了英文补全则英文词频随使用调整，中/英文模式皆然 ──

#[test]
fn english_freq_learns_and_reranks() {
    let mut e = en_engine(); // smartWordEn 开
    let before = e.english_lookup_exact("forever").expect("forever 在词表");
    // 上屏 4 次 → 加成 4000×4 = 16000（封顶；种子谱 ≤9999 必登顶）
    for _ in 0..4 {
        e.commit("forever", "forever", None, true);
    }
    let after = e.english_lookup_exact("forever").unwrap();
    assert_eq!(after.1, before.1 + 16000, "4 次使用加成 = 4000×4");
    // 第 5 次起封顶（不再增长）
    e.commit("forever", "forever", None, true);
    assert_eq!(
        e.english_lookup_exact("forever").unwrap().1,
        before.1 + 16000
    );
    // 低频词用过 4 次后在补全列表登顶（选 ab 前缀的末位词——种子 freq 最低；
    // 前缀须 ≥2 字母：normalize 按设计拒绝单字母查询）
    let list = e.english_complete("ab", 9);
    let last = list.last().unwrap().clone();
    for _ in 0..4 {
        e.commit(&last.0, &last.0, None, true);
    }
    let after_list = e.english_complete("ab", 9);
    assert_eq!(
        after_list[0].0, last.0,
        "用过 4 次的词必须登顶（加成 16000 > 任何种子 freq）"
    );
}

#[test]
fn english_freq_learning_gated_by_smart_word_switches() {
    let mut e = en_engine();
    // 双闸全关（补全没开就不学）
    e.set_smart_word_en_enabled(false);
    e.set_smart_word_zh_enabled(false);
    let before = e.english_lookup_exact("forever").unwrap();
    e.commit("forever", "forever", None, true);
    assert_eq!(
        e.english_lookup_exact("forever").unwrap().1,
        before.1,
        "补全双关时不学习"
    );
    // 仅中文模式开（zh 模式英文候选也是补全面）→ 学习
    e.set_smart_word_zh_enabled(true);
    e.commit("forever", "forever", None, true);
    assert_eq!(
        e.english_lookup_exact("forever").unwrap().1,
        before.1 + 4000,
        "smartWordZh 开（中文模式英文候选）也计入使用"
    );
}

#[test]
fn english_learning_not_polluted_by_chinese_commit() {
    let mut e = en_engine();
    e.commit("nihao", "你好", Some("ni hao"), true);
    assert!(
        e.user_english.is_empty(),
        "中文上屏不得进英文计数（反向同样隔离）"
    );
}

/// 独立临时库（持久化链路：flush 增量落盘 + 重启加载）。
#[test]
fn english_freq_persists_across_restart() {
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let dir = crate::test_tmp::dir().join(format!("x1pinyin_en_persist_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("x1pinyin.db");
    std::fs::copy(seed, &db).unwrap();
    let db_str = db.to_str().unwrap().to_string();

    let seed_freq;
    {
        let mut e = Engine::new_with_db(&db_str).unwrap();
        e.set_smart_word_en_enabled(true);
        seed_freq = e.english_lookup_exact("forever").unwrap().1;
        e.commit("forever", "forever", None, true);
        e.commit("forever", "forever", None, true);
        e.flush().expect("flush 落盘英文计数增量");
    }
    {
        let e2 = Engine::new_with_db(&db_str).unwrap();
        assert_eq!(
            e2.english_lookup_exact("forever").unwrap().1,
            seed_freq + 8000,
            "重启后使用计数从 user_english_counts 恢复（加成随次数）"
        );
        assert!(e2.english_delta.is_empty(), "启动时无待落盘增量");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
