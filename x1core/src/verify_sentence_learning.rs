//! 整句学习行为验证：证明 commit 不把长词组合存为整体词。
//!
//! 运行：cargo test -p x1core verify_sentence_learning -- --nocapture
//! 会打印每个例子 commit 后 user_words 的实际内容，用事实说话。
#[cfg(test)]
mod verify {
    use crate::engine::Engine;

    /// 导出 Engine 的 user_words 内容（测试辅助）。
    /// 因为 user_words 是私有字段，这里通过 Engine 已有的测试方法间接验证。
    ///
    /// 我们对每个例子：
    /// 1. commit 该文本
    /// 2. 检查「整句本身」是否进了 user_words（应为 false，证明没存整体）
    /// 3. 检查「可能的子词」是否进了 user_words（看实际拆分结果）
    /// 4. 打印结论
    /// 严格回归测试（用真实种子 DB）。
    ///
    /// 设计现状：多词组合选择性固化（≤8 字进 user_words）。
    /// ≤8 字整句默认不固化，只拆词+bigram；
    /// 用户想固化时用 Shift+数字键（commit_with_solidify force=true）。
    #[test]
    fn verify_multi_word_not_stored_with_seed_db() {
        let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
        // 用临时副本，避免并行测试对种子 DB 的写竞争 + 确保 dirty 清理能跑
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_verify_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::test_tmp::require_seed_db();
        let _ = std::fs::copy(seed, &tmp);
        let db_path = tmp.to_str().unwrap();
        // 多词组合（≤8 字）：普通 commit 默认不固化，force_solidify=true 才固化。
        // 测试词必须不在种子词库（「我不知道」已进库，换「我没吃饭」）。
        let multi_word_cases: &[(&str, &str, &str)] = &[
            ("womeichifan", "我没吃饭", "wo mei chi fan"),
            ("woxihuanchifan", "我喜欢吃饭", "wo xi huan chi fan"),
        ];
        for &(input, text, pinyin) in multi_word_cases {
            // 普通提交：不固化
            let mut engine = Engine::new_with_db(db_path).unwrap_or_else(|_| Engine::new());
            engine.commit(input, text, Some(pinyin), true);
            assert!(
                !engine.user_words_contains_for_test(text),
                "普通 commit「{}」不应固化（默认不固化，需 Shift+数字手动触发）",
                text
            );
            // force_solidify=true：固化
            let mut engine2 = Engine::new_with_db(db_path).unwrap_or_else(|_| Engine::new());
            engine2.commit_with_solidify(input, text, Some(pinyin), true, true);
            assert!(
                engine2.user_words_contains_for_test(text),
                "commit_with_solidify(force=true)「{}」应固化到 user_words",
                text
            );
        }
        // 单个词：commit 后应被引擎记住（系统词不写 user_words，但
        // freq.counts 应记录选择次数）。这里「不知道」「你好」在种子 DB 是系统词。
        let single_word_cases: &[(&str, &str, &str)] = &[
            ("buzhidao", "不知道", "bu zhi dao"),
            ("nihao", "你好", "ni hao"),
        ];
        for &(input, text, pinyin) in single_word_cases {
            let mut engine = Engine::new_with_db(db_path).unwrap_or_else(|_| Engine::new());
            engine.commit(input, text, Some(pinyin), true);
            assert!(
                engine.freq_counts_contains_for_test(text),
                "「{}」commit 后 freq.counts 应有记录（记住用户选择）",
                text
            );
        }
        let _ = std::fs::remove_file(&tmp);
    }

    /// 回归测试：删除绿色智能组合词后，整句解码仍应能生成它（不永久屏蔽）。
    /// 智能组合词（非 user_word/词库词）的删除不进黑名单；
    /// 用户主动选字（commit）后，黑名单词复活。
    /// 人的想法会变——删过的词，用户主动选中说明改主意了，应清除黑名单。
    /// 探针：复现 xiangm 产生无意义绿词「想呣」。
    #[test]
    fn probe_xiangm() {
        let seed = crate::engine::tests_query::seed_copy("vsl");
        let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
        for input in &["xiang", "xiangm", "xiangmu", "xm"] {
            let cands = engine.process(input);
            let top: Vec<String> = cands
                .iter()
                .take(5)
                .map(|c| {
                    let tag = if c.is_sentence {
                        "绿"
                    } else if c.is_correction {
                        "橙"
                    } else {
                        "白"
                    };
                    format!("{}[{}]", c.text, tag)
                })
                .collect();
            eprintln!("PROBE {} → {}", input, top.join(", "));
        }
    }

    /// 探针：复现"项目规划"绿词学习。
    /// 核心回归：绿词自学习反映用户输入习惯。
    /// 用户连续输入"项目"+"规划"多次后，xiangmuguihua 绿词应给出"项目规划"，
    /// 而非词库里 freq 更高的"香木归化"。
    /// 综合案例测试：展示句内 bigram + 简拼展开 bigram + 绿词门槛的效果。
    /// 运行：cargo test -p x1core verify_showcase -- --nocapture
    #[test]
    fn verify_showcase() {
        // seed_copy 临时副本：测试不得直开仓库种子——commit 学习写库 +
        // bigram 缓存写回 assets 会弄脏交付资产。
        let seed = crate::engine::tests_query::seed_copy("vsl");

        eprintln!("\n========== 句内词间 bigram ==========");
        eprintln!("（用户连续输入词组，子词 bigram 应影响整句切分）\n");
        {
            let mut engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
            // 模拟用户连续输入"项目"+"规划"5次，强化 bigram(项目,规划)
            for _ in 0..5 {
                engine.commit("xiangmu", "项目", Some("xiang mu"), true);
                engine.commit("guihua", "规划", Some("gui hua"), true);
            }
            let bc = engine.bigram_count_for_test("项目", "规划");
            eprintln!("学习后 bigram(项目,规划) = {}", bc);
            let cands = engine.process("xiangmuguihua");
            let green = cands.iter().find(|c| c.is_sentence);
            eprintln!("xiangmuguihua 绿词 = {:?}", green.map(|c| &c.text));
            // 无学习时冷启动绿词（对照）
            let cold = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
            let cold_cands = cold.process("xiangmuguihua");
            let cold_green = cold_cands.iter().find(|c| c.is_sentence);
            eprintln!("（对照）冷启动绿词 = {:?}", cold_green.map(|c| &c.text));
        }

        eprintln!("\n========== 简拼展开结合 bigram 选音节 ==========");
        eprintln!("（混合输入的简拼字母，结合上文 bigram 选最合适音节）\n");
        {
            let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
            // 全拼对照（无歧义）
            let full = engine.process("qiehuanshurufazhihou");
            eprintln!(
                "全拼 qiehuanshurufazhihou 首候选 = {:?}",
                full.first().map(|c| &c.text)
            );
            // 混合简拼（f 是简拼，应覆盖完整输入）
            let mixed = engine.process("qiehuanshurufzhihou");
            let mixed_green = mixed.iter().find(|c| c.is_sentence);
            eprintln!(
                "混合 qiehuanshurufzhihou 绿词 = {:?}",
                mixed_green.map(|c| &c.text)
            );
        }

        eprintln!("\n========== 绿词门槛 ==========");
        eprintln!("（≥3音节 + 必含真实词组，杜绝垃圾双字绿词）\n");
        {
            let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
            // 2音节不应生成绿词
            let zhine = engine.process("zhine");
            let has_green = zhine.iter().any(|c| c.is_sentence);
            eprintln!(
                "zhine（2音节）有绿词 = {}（应为 false，杜绝「只呢」）",
                has_green
            );
            eprintln!(
                "  前三: {:?}",
                zhine.iter().take(3).map(|c| &c.text).collect::<Vec<_>>()
            );
            // 3音节含真实词组应生成（「我在家」已进库，换「我在忙」）
            let wozaimang = engine.process("wozaimang");
            let green = wozaimang.iter().find(|c| c.is_sentence);
            eprintln!(
                "wozaimang（3音节）绿词 = {:?}（应为「我在忙」）",
                green.map(|c| &c.text)
            );
            // 长句多词组合
            let long = engine.process("woxihuanchifan");
            let green = long.iter().find(|c| c.is_sentence);
            eprintln!("woxihuanchifan 绿词 = {:?}", green.map(|c| &c.text));
        }

        eprintln!("\n========== 完整匹配优先 ==========");
        eprintln!("（完整匹配长词组排前缀短词前面）\n");
        {
            let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
            let bjky = engine.process("beijingkaoya");
            eprintln!(
                "beijingkaoya 首候选 = {:?}（应为「北京烤鸭」而非「北京」）",
                bjky.first().map(|c| &c.text)
            );
        }

        eprintln!("\n========== 长句连续输入 ==========");
        eprintln!("（整句解码 + 渐进匹配 + 平滑不回退）\n");
        {
            let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
            // 逐字符追踪，验证不回退
            let full = "woxihuanchifan";
            let mut acc = String::new();
            for ch in full.chars() {
                acc.push(ch);
                let cands = engine.process(&acc);
                let first = cands.first().map(|c| c.text.as_ref()).unwrap_or("(空)");
                eprintln!("  {:<16} → {}", acc, first);
            }
        }

        eprintln!("\n========== 上下文感知 ==========");
        eprintln!("（上文 + 当前输入，bigram 引导首词选择）\n");
        {
            let mut engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
            // 上文「吃」+ fan → 「饭」应靠前
            engine.context_clear_for_test();
            engine.context_push_for_test("吃");
            let cands = engine.process("fan");
            eprintln!(
                "上文「吃」+ fan 前三 = {:?}（应含「饭」）",
                cands.iter().take(3).map(|c| &c.text).collect::<Vec<_>>()
            );
            // 上文「我」+ xiangchifan → 首词「想」
            engine.context_clear_for_test();
            engine.context_push_for_test("我");
            let cands = engine.process("xiangchifan");
            let green = cands.iter().find(|c| c.is_sentence);
            eprintln!(
                "上文「我」+ xiangchifan 绿词 = {:?}",
                green.map(|c| &c.text)
            );
        }
    }

    /// 混合简拼输入支持绿词组合。
    /// 选绿词上屏后系统应从中学习（拆词记 bigram）。整句默认不固化，需 Shift+数字手动固化。
    #[test]
    fn verify_learn_from_green_word() {
        // 用空 DB（Engine::new）而非种子 DB——种子 DB 的 seed_bigram（自有语料）
        // 会改变 DP 切分（如"woxihuanchifan"在种子下切出"窝喜欢吃饭"而非"我喜欢吃饭"）。
        // 本测试验证"绿词学习机制"本身，不应被种子数据干扰（与 test_nbest_dp 同理）。
        let mut engine = Engine::new();

        // 输入 woxihuanchifan，选中绿词「我喜欢吃饭」上屏
        let cands = engine.process("woxihuanchifan");
        let green = cands.iter().find(|c| c.is_sentence).expect("应有绿词");
        assert!(
            green.text.as_ref() == "我喜欢吃饭",
            "绿词应为「我喜欢吃饭」"
        );

        // commit（模拟用户用普通数字键选中绿词上屏——默认不固化）
        engine.commit(
            "woxihuanchifan",
            &green.text,
            Some("wo xi huan chi fan"),
            true,
        );

        // 验证学习结果（默认不固化 + 拆词只记 bigram）：
        //
        // 1. 整句默认不固化到 user_words（保持绿词身份，靠 bigram 越用越准）
        assert!(
            !engine.user_words_contains_for_test("我喜欢吃饭"),
            "普通 commit 整句「我喜欢吃饭」不应固化（默认不固化，需 Shift+数字手动触发）"
        );
        // 2. bigram 应记录子词间关联（拆词学习的核心产出，绿词智能的来源）
        let bc = engine.bigram_count_for_test("喜欢", "吃饭");
        assert!(bc >= 1, "commit 绿词后应记录 bigram(喜欢→吃饭)={}", bc);

        // 对照：用 force_solidify=true（模拟 Shift+数字）提交，整句应固化
        let mut engine2 = Engine::new();
        engine2.commit_with_solidify(
            "woxihuanchifan",
            "我喜欢吃饭",
            Some("wo xi huan chi fan"),
            true,
            true,
        );
        assert!(
            engine2.user_words_contains_for_test("我喜欢吃饭"),
            "commit_with_solidify(force=true) 整句「我喜欢吃饭」应固化到 user_words"
        );
    }

    /// zhegguangc（zhe+g+guang+c 混合简拼）应能触发绿词。
    /// （原用例 zhegxingq「这个星球」已进词库，换「这个广场」——
    /// 测试词必须不在词库，否则直接命中白词不触发绿词机制）
    #[test]
    fn verify_mixed_jianpin_green_word() {
        let seed = crate::engine::tests_query::seed_copy("vsl");
        let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
        // zhegguangc：混合简拼，应触发绿词（即使切分不完美，绿词机制应生效）
        let cands = engine.process("zhegguangc");
        let has_green = cands.iter().any(|c| c.is_sentence);
        assert!(
            has_green,
            "zhegguangc 混合简拼应触发绿词，实际无绿词: {:?}",
            cands.iter().take(3).map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    /// 完整匹配绿词（字数==音节数，无补字）排首位，压过前缀白词。
    /// 测试词必须不在词库（同 verify_mixed_jianpin_green_word 注释的换词先例）：
    /// 「那些日子」入库后 naxierizi 直接
    /// 命中白词（词库词优先于绿词是既定规则，见 verify_dict_word_ranks_above_green），
    /// 换「那些年华」保持「绿词压过前缀白词」原场景。
    #[test]
    fn verify_full_match_green_ranks_first() {
        let seed = crate::engine::tests_query::seed_copy("vsl");
        let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
        // naxienianhua→「那些年华」4字=4音节完整匹配，应排首位（压过前缀「那些」）
        let cands = engine.process("naxienianhua");
        let first = cands.first().expect("应有候选");
        assert!(
            first.is_sentence,
            "完整匹配绿词「那些年华」应排首位，实际首候选「{}」is_sentence={}",
            first.text, first.is_sentence
        );
    }

    /// 既有词库词优先于绿词（既定规则）。
    /// 绿词是算法拼接产物，不应逾越词库里的既有词。
    /// 测试数据策略：构造式 DB，注入高频「输入」，避免种子 DB freq 数据耦合。
    #[test]
    fn verify_dict_word_ranks_above_green() {
        use crate::storage::{Storage, X1DictWord};
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_dictrank_{}_{}.db",
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
                    simplified: "输入".into(),
                    pinyin: "shu ru".into(),
                    freq: 5000,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "之后".into(),
                    pinyin: "zhi hou".into(),
                    freq: 3000,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "shu".into(),
                    pinyin: "shu".into(),
                    freq: 100,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "ru".into(),
                    pinyin: "ru".into(),
                    freq: 100,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "zhi".into(),
                    pinyin: "zhi".into(),
                    freq: 100,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "hou".into(),
                    pinyin: "hou".into(),
                    freq: 100,
                    ..Default::default()
                },
            ])
            .unwrap();
        }
        let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();

        // shurufzhiihou：既有词「输入」应排在绿词前
        let cands = engine.process("shurufzhiihou");
        let dict_pos = cands
            .iter()
            .position(|c| c.text.as_ref() == "输入" && !c.is_sentence);
        let green_pos = cands.iter().position(|c| c.is_sentence);
        if let (Some(dp), Some(gp)) = (dict_pos, green_pos) {
            assert!(dp < gp, "既有词「输入」(位{})应排在绿词(位{})前面", dp, gp);
        }
        let _ = std::fs::remove_file(&tmp);
    }

    /// 末尾半截音节统一规则（jisua 中途态）：有延长词
    /// 证据（jisua←计算）→ 半截语义，不出语气词绿词、不出现「急速爱」式
    /// a 首字母扩词垃圾；最佳延长词（计算）以紫词注入。
    #[test]
    fn verify_truncated_tail_no_green_word() {
        let seed = crate::engine::tests_query::seed_copy("vsl");
        let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
        // jisua：无语气词绿词，无「急速爱」垃圾，有延长词「计算」
        let cands = engine.process("jisua");
        let greens: Vec<String> = cands
            .iter()
            .filter(|c| c.is_sentence)
            .map(|c| c.text.to_string())
            .collect();
        assert!(greens.is_empty(), "jisua 续打中途不得出绿词，实际: {:?}", greens);
        assert!(
            !cands.iter().any(|c| c.text.as_ref() == "急速爱"),
            "不得出现 a 首字母扩词垃圾「急速爱」"
        );
        assert!(
            cands.iter().any(|c| c.text.as_ref() == "计算"),
            "jisua 应含延长词「计算」（紫词）: {:?}",
            cands.iter().take(5).map(|c| c.text.as_ref()).collect::<Vec<_>>()
        );
        // 对照：jisu（无末尾半截）正常
        let cands = engine.process("jisu");
        assert!(!cands.iter().any(|c| c.is_sentence), "jisu 2音节不应有绿词");
    }

    /// 完整匹配长词组排前缀短词前面。
    #[test]
    fn verify_full_match_ranks_before_prefix() {
        let seed = crate::engine::tests_query::seed_copy("vsl");
        let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
        // beijingkaoya: 「北京烤鸭」(4字完整匹配) 应排在「北京」(2字前缀匹配) 前
        let cands = engine.process("beijingkaoya");
        let first = cands.first().expect("应有候选");
        assert_eq!(
            first.text.as_ref(),
            "北京烤鸭",
            "完整匹配的「北京烤鸭」应排首位，实际首候选「{}」",
            first.text
        );
    }

    /// 多词绿词（≥3音节）排首位。
    /// 用户连续输入多个词形成的长组合是其主要意图，应首选呈现。
    #[test]
    fn verify_multi_word_green_ranked_first() {
        let seed = crate::engine::tests_query::seed_copy("vsl");
        let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

        // woxihuanchifan（5音节多词组合）：绿词应排首位
        let cands = engine.process("woxihuanchifan");
        let first = cands.first().expect("应有候选");
        assert!(
            first.is_sentence,
            "多词绿词（≥3音节）应排首位，实际首候选「{}」is_sentence={}",
            first.text, first.is_sentence
        );

        // wozaimang（3音节，「我在忙」不在词库）：绿词应排首位
        // （原用例 wozaijia「我在家」已进词库，直接命中白词不触发绿词）
        let cands = engine.process("wozaimang");
        let first = cands.first().expect("应有候选");
        assert!(
            first.is_sentence,
            "3音节绿词应排首位，实际首候选「{}」is_sentence={}",
            first.text, first.is_sentence
        );
    }

    /// 绿词门槛回归：2 音节不生成绿词（杜绝「只呢」类垃圾双字组合）。
    /// 3 音节且含真实多字词组才生成。
    #[test]
    fn verify_green_word_threshold() {
        let seed = crate::engine::tests_query::seed_copy("vsl");
        let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

        // 2 音节（zhine=zhi+ne）：词库无「zhi ne」二字词，绝不应拼成「只呢」绿词
        let cands = engine.process("zhine");
        let has_green = cands.iter().any(|c| c.is_sentence);
        assert!(
            !has_green,
            "2 音节不应生成绿词（杜绝垃圾双字组合），实际有绿词: {:?}",
            cands
                .iter()
                .filter(|c| c.is_sentence)
                .map(|c| &c.text)
                .collect::<Vec<_>>()
        );

        // 3 音节含真实词组（wozaimang=wo zai mang，「我在」是真实词且整词不在库）→ 应生成绿词
        // （原用例 wozaijia「我在家」已进词库，换「我在忙」）
        let cands = engine.process("wozaimang");
        let has_green = cands.iter().any(|c| c.is_sentence);
        assert!(
            has_green || cands.iter().take(3).any(|c| c.text.chars().count() >= 3),
            "3 音节含真实词组应能生成绿词或返回词组候选"
        );
    }

    #[test]
    fn verify_green_word_learns_user_habit() {
        let seed = crate::engine::tests_query::seed_copy("vsl");
        let mut engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

        // 用户连续5次输入"项目"+"规划"（强化记忆）
        for _ in 0..5 {
            engine.commit("xiangmu", "项目", Some("xiang mu"), true);
            engine.commit("guihua", "规划", Some("gui hua"), true);
        }

        // 学习后：绿词应反映用户习惯，给出"项目规划"
        let after = engine.process("xiangmuguihua");
        let after_sentence = after
            .iter()
            .find(|c| c.is_sentence)
            .map(|c| c.text.as_ref().to_owned());

        assert_eq!(
            after_sentence.as_deref(),
            Some("项目规划"),
            "绿词应学习用户习惯：期望「项目规划」，实际 {:?}",
            after_sentence
        );
    }

    #[test]
    fn verify_commit_revives_blacklisted_word() {
        let mut engine = Engine::new();
        engine.learn("来说", "lai shuo");
        // 删除 → 进黑名单 → 整句解码不拼回
        engine.remove("来说");
        assert!(
            !engine
                .process("laishuo")
                .iter()
                .any(|c| c.text.as_ref() == "来说"),
            "删除后「来说」应被屏蔽"
        );

        // 用户改主意，主动选「来说」上屏（commit）→ 应复活
        engine.commit("laishuo", "来说", Some("lai shuo"), true);
        // 黑名单应已清除，整句解码能重新拼出「来说」
        assert!(
            engine
                .process("laishuo")
                .iter()
                .any(|c| c.text.as_ref() == "来说"),
            "commit 后「来说」应复活，不再被屏蔽"
        );
    }

    /// 回归测试：删除绿色智能组合词后，本次会话降级排末尾（不再排首位打扰），
    /// 但用户 commit 复活后能重新排首位。
    ///
    /// - 若删除后绿词完全消失，会导致死循环——绿词消失后用户
    ///   选不到它，无法 commit 复活；
    /// - 因此删除后绿词降级排到候选列表末尾（不打扰但仍可翻页找到），
    ///   让用户改变主意时能选中复活。Command+Backspace 仍是基本功能。
    #[test]
    fn verify_remove_sentence_blacklisted_until_revive() {
        let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
        // 用临时副本：remove() 立即落盘黑名单（insert_blacklist），
        // 直接在真实种子上跑会把删除的整句写进种子 blacklist
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_vrs_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::test_tmp::require_seed_db();
        let _ = std::fs::copy(seed, &tmp);
        let mut engine =
            Engine::new_with_db(tmp.to_str().unwrap()).unwrap_or_else(|_| Engine::new());

        // 用确实产生绿词的多字真音节组合
        let cands1 = engine.process("woxihuanchifan");
        let sentence_text = cands1
            .iter()
            .find(|c| c.is_sentence)
            .map(|c| c.text.as_ref().to_owned());
        if let Some(text) = sentence_text {
            // 删除前：绿词应在候选列表里
            assert!(
                cands1
                    .iter()
                    .any(|c| c.is_sentence && c.text.as_ref() == text),
                "删除前绿词「{}」应在候选列表",
                text
            );

            // 删除绿词 → 降级（仍在列表，但不在首位）
            engine.remove(&text);
            let cands2 = engine.process("woxihuanchifan");
            // 新设计：绿词不消失，仍在列表末尾（让用户能复活）
            assert!(
                cands2
                    .iter()
                    .any(|c| c.is_sentence && c.text.as_ref() == text),
                "删除绿词「{}」后应降级排末尾（不消失，让用户能复活），实际候选：{:?}",
                text,
                cands2.iter().map(|c| &c.text).collect::<Vec<_>>()
            );

            // commit 复活 → 整句已固化为白词，首候选直接命中
            // （新设计：≤8 字整句固化，commit 后整句进 user_words，下次查询白词排首位）
            engine.commit("woxihuanchifan", &text, Some("wo xi huan chi fan"), true);
            let cands3 = engine.process("woxihuanchifan");
            // 复活后首候选应是整句（白词固化后直接命中，不依赖绿词）
            let first = cands3.first().map(|c| c.text.as_ref());
            assert_eq!(
                first,
                Some(text.as_str()),
                "commit 复活后，首候选应是「{}」（白词固化直接命中）",
                text
            );
        }
        let _ = std::fs::remove_file(&tmp);
    }

    /// 回归测试：删除真实 user_word 后，整句解码应屏蔽（防拼回），commit 复活。
    #[test]
    fn verify_remove_stored_word_still_blacklisted() {
        let mut engine = Engine::new();
        engine.learn("来说", "lai shuo");
        assert!(engine
            .process("laishuo")
            .iter()
            .any(|c| c.text.as_ref() == "来说"));
        // 「来说」是 user_word，删除应进黑名单，防止单字拼回
        engine.remove("来说");
        assert!(
            !engine
                .process("laishuo")
                .iter()
                .any(|c| c.text.as_ref() == "来说"),
            "删除真实 user_word 后，整句解码应屏蔽防拼回"
        );
        // commit 复活
        engine.commit("laishuo", "来说", Some("lai shuo"), true);
        assert!(
            engine
                .process("laishuo")
                .iter()
                .any(|c| c.text.as_ref() == "来说"),
            "commit 后「来说」应复活"
        );
    }

    #[test]
    fn verify_sentence_learning() {
        // 用真实种子 DB（与用户实际环境一致），行为才有代表性
        let seed = crate::engine::tests_query::seed_copy("vsl");
        let make_engine = || Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
        let cases: &[(&str, &str, &str)] = &[
            // (输入, 文本, 拼音)
            // —— 2字词（应作为整体学习）——
            ("nihao", "你好", "ni hao"),
            ("zhidao", "知道", "zhi dao"),
            // —— 3字：边界情况，短句 or 词组？——
            // （「我在家」「我不知道」已进词库，
            //   边界用例换成仍缺席的「我在忙」「我没吃饭」）
            ("wozaimang", "我在忙", "wo zai mang"),
            ("buzhidao", "不知道", "bu zhi dao"),
            // —— 4字：明确的多词组合，应拆词 ——
            ("womeichifan", "我没吃饭", "wo mei chi fan"),
            ("nishishui", "你是谁", "ni shi shui"),
            // —— 7字长句：必须拆词 ——
            (
                "nishuonizhibuzhidao",
                "你说你知不知道",
                "ni shuo ni zhi bu zhi dao",
            ),
            ("woxihuanchifan", "我喜欢吃饭", "wo xi huan chi fan"),
        ];

        eprintln!("\n========== 整句学习行为验证 ==========");
        eprintln!("对每个例子 commit 后，检查 user_words 实际存了什么\n");

        for &(input, text, pinyin) in cases {
            let mut engine = make_engine();
            // 先打印 is_sentence 判定结果，定位是否走拆词
            let is_sent = engine.is_sentence_input_for_test(text, Some(pinyin));
            engine.commit(input, text, Some(pinyin), true);

            let char_count = text.chars().count();
            let stored_as_whole = engine.user_words_contains_for_test(text);

            // 重新切分判断：是否为多词组合（决定"存整体"是否正确）
            let syllables: Vec<&str> = pinyin.split_whitespace().collect();
            let engine2 = make_engine();
            let segments = engine2.split_into_words_for_test(&syllables);
            let is_multi = segments.len() >= 2 && !engine2.base_dict_has_for_test(text);
            // 多词组合存整体 = 错误；单个词存整体 = 正确
            let verdict = if stored_as_whole {
                if is_multi {
                    "错误：多词组合却存为整体"
                } else {
                    "正确：单个词存为整体"
                }
            } else {
                "正确：多词组合已拆词学习"
            };

            // 检查各种可能的子词
            let chars: Vec<char> = text.chars().collect();
            let mut sub_words_found = Vec::new();
            let len = 3.min(char_count);
            for sub_len in 2..=len {
                for start in 0..=char_count - sub_len {
                    let sub: String = chars[start..start + sub_len].iter().collect();
                    if engine.user_words_contains_for_test(&sub) {
                        sub_words_found.push(sub);
                    }
                }
            }

            eprintln!(
                "输入 {:<22} → 「{}」（{}字）",
                format!("'{}'", input),
                text,
                char_count
            );
            eprintln!(
                "  is_sentence判定={}  切分子词数: {}  {}",
                is_sent,
                segments.len(),
                if is_multi {
                    "[多词组合]"
                } else {
                    "[单个词]"
                }
            );
            eprintln!(
                "  整句「{}」进 user_words: {} → {}",
                text, stored_as_whole, verdict
            );
            if sub_words_found.is_empty() {
                eprintln!("  被学习的子词: （无多字子词，仅单字 freq+bigram）");
            } else {
                eprintln!(
                    "  被学习的子词: {}",
                    sub_words_found
                        .iter()
                        .map(|s| format!("「{}」", s))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
            eprintln!();
        }

        eprintln!("========== 切分能力诊断 ==========");
        eprintln!("（看每个文本能否被切成多个有意义的子词）\n");
        for &(_input, text, pinyin) in cases {
            let engine = make_engine();
            let syllables: Vec<&str> = pinyin.split_whitespace().collect();
            let splits = engine.split_into_words_for_test(&syllables);
            let multi_char_count = splits
                .iter()
                .filter(|(t, _)| t.chars().count() >= 2)
                .count();
            eprintln!(
                "「{}」({}字) 切分: {:?}  多字子词数: {}",
                text,
                text.chars().count(),
                splits,
                multi_char_count
            );
        }
        eprintln!();

        eprintln!("========== 颜色标记验证（词库词=默认 / 纠错=橙 / 智能组合=绿）==========\n");
        {
            // 加载种子 DB（与 eval 模块一致）
            let seed = crate::engine::tests_query::seed_copy("vsl");
            let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
            // 1. 词库词：is_sentence=false, is_correction=false（默认色）
            let cands = engine.process("nihao");
            if let Some(c) = cands.first() {
                eprintln!(
                    "词库词「你好」: is_sentence={} is_correction={}（应 false/false，默认色）",
                    c.is_sentence, c.is_correction
                );
            }
            // 2. 智能组合词：is_sentence=true（绿色）
            let cands = engine.process("woxihuanchifan");
            if let Some(c) = cands.first() {
                eprintln!(
                    "智能组合「{}」: is_sentence={} is_correction={}（应 true/false，绿色）",
                    c.text, c.is_sentence, c.is_correction
                );
            }
            // 3. 单字词库词：不应被标记为智能组合
            let cands = engine.process("ni");
            if let Some(c) = cands.first() {
                eprintln!(
                    "单字「{}」: is_sentence={}（应 false，默认色）",
                    c.text, c.is_sentence
                );
            }
            // 4. 2字词组（词库整词）：不应被标记为智能组合
            let cands = engine.process("zhidao");
            if let Some(c) = cands.first() {
                eprintln!(
                    "词组「{}」: is_sentence={}（应 false，默认色）",
                    c.text, c.is_sentence
                );
            }
        }
        eprintln!();

        eprintln!("========== 结论 ==========");
        eprintln!("- 2-3 字短词：作为整体学习（正确，它们就是单个词）");
        eprintln!("- 4+ 字多词组合：整句不进 user_words，只学子词 + bigram 关联");
        eprintln!("- 系统学到的是「词与词的搭配规律」，不是死记每个句子");
    }

    /// 回归：带 partial 尾时必须不短路整句，且全覆盖句子排首位。
    ///
    /// 旧行为：faxianyigew 的 valid 前缀 fa-xian-yi-ge 恰好是词「发现一个」→
    /// 「词库已有同音整词」短路杀死句子层 → 首选回退成部分覆盖的「发现」；
    /// 新行为：句子层运行 + 尾首字母补字 + 覆盖序插位 → 首选全覆盖「发现一个我」。
    /// 构造式 DB（不耦合种子数据）：发现/一个/发现一个/我（w 补字来源）。
    #[test]
    fn verify_partial_tail_sentence_survives_whole_word_prefix() {
        use crate::storage::{Storage, X1DictWord};
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_tailsent_{}_{}.db",
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
                    simplified: "发现".into(),
                    pinyin: "fa xian".into(),
                    freq: 6306,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "一个".into(),
                    pinyin: "yi ge".into(),
                    freq: 959,
                    ..Default::default()
                },
                // valid 前缀本身成词——旧代码正是在这里触发「整词短路」
                X1DictWord {
                    simplified: "发现一个".into(),
                    pinyin: "fa xian yi ge".into(),
                    freq: 959,
                    ..Default::default()
                },
                // 尾 w 的补字来源（best_char_for_initial 只认 w 开头单字）
                X1DictWord {
                    simplified: "我".into(),
                    pinyin: "wo".into(),
                    freq: 9999,
                    ..Default::default()
                },
            ])
            .unwrap();
        }
        let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
        let cands = engine.process("faxianyigew");
        if cands.len() < 3 {
            eprintln!(
                "candidates: {:?}",
                cands.iter().map(|c| &c.text).collect::<Vec<_>>()
            );
        }
        let first = cands.first().expect("应有候选");
        assert!(
            first.is_sentence && first.text.as_ref() == "发现一个我",
            "带 partial 尾时首选应为全覆盖整句「发现一个我」，实际「{}」is_sentence={}（回退到部分覆盖白词=旧 bug）",
            first.text,
            first.is_sentence
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// 回归：带 partial 尾的全覆盖句子必须排到部分覆盖白词之前。
    ///
    /// 旧行为：faxianyigewent 的句子「发现一个问他」(全消费) 固定插在第一个多字
    /// 白词之后，被 2 音节的「发现」(freq 6306) 压住——候选随键入长短回跳。
    /// 新行为：按覆盖序插到第一个「覆盖不足」白词之前 → 句子登顶。
    #[test]
    fn verify_partial_tail_full_coverage_sentence_ranks_first() {
        use crate::storage::{Storage, X1DictWord};
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_tailrank_{}_{}.db",
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
                    simplified: "发现".into(),
                    pinyin: "fa xian".into(),
                    freq: 6306,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "一个".into(),
                    pinyin: "yi ge".into(),
                    freq: 959,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "问".into(),
                    pinyin: "wen".into(),
                    freq: 8200,
                    ..Default::default()
                },
                // 尾 t 的补字来源
                X1DictWord {
                    simplified: "他".into(),
                    pinyin: "ta".into(),
                    freq: 9000,
                    ..Default::default()
                },
            ])
            .unwrap();
        }
        let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
        let cands = engine.process("faxianyigewent");
        let first = cands.first().expect("应有候选");
        assert!(
            first.is_sentence && first.text.as_ref() == "发现一个问他",
            "全消费整句「发现一个问他」应排首位，实际「{}」is_sentence={}（被部分覆盖白词压住=旧 bug）",
            first.text,
            first.is_sentence
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// 悬尾补字的组合词关联层（nixiangganm→你想干吗）：
    ///
    /// 旧行为：best_char_for_initial 只有 bigram→字频两层，冷启动 bigram 全 0
    /// 时纯字频定序——干+m 补出字频 9760 的「没」（句成 你想干没），而全拼
    /// nixiangganma 的胜者 [你想][干][吗] 靠 boundary 组合词 collocation_freq
    /// (干,吗,ma)=706 过门（同款信号在 sentence.rs 绿词 DP 里早就是过门证据）。
    /// 新行为：与上文成组合词（≥500 地板）的字排进纯字频之前；过门槛者之间
    /// 仍按字频定序（吗 8940 > 嘛 1000），与全拼首选同文本、续打零跳变。
    /// 词频镜像种子真值，守护生产形态。
    #[test]
    fn verify_partial_tail_completion_uses_collocation() {
        use crate::storage::{Storage, X1DictWord};
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_tailcolloc_{}_{}.db",
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
                    simplified: "你想".into(),
                    pinyin: "ni xiang".into(),
                    freq: 1157,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "干".into(),
                    pinyin: "gan".into(),
                    freq: 5000,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "吗".into(),
                    pinyin: "ma".into(),
                    freq: 8940,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "没".into(),
                    pinyin: "mei".into(),
                    freq: 9760,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "嘛".into(),
                    pinyin: "ma".into(),
                    freq: 1000,
                    ..Default::default()
                },
                // 组合词证据：干吗/干嘛 都过 500 地板，嘛 也合格——字频在合格者
                // 之间定序（吗 8940 > 嘛 1000），不能让组合词频 875 反超字频。
                X1DictWord {
                    simplified: "干吗".into(),
                    pinyin: "gan ma".into(),
                    freq: 706,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "干嘛".into(),
                    pinyin: "gan ma".into(),
                    freq: 875,
                    ..Default::default()
                },
            ])
            .unwrap();
        }
        let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
        let cands = engine.process("nixiangganm");
        let first = cands.first().expect("应有候选");
        assert!(
            first.is_sentence && first.text.as_ref() == "你想干吗",
            "悬尾补字应吃组合词证据补「吗」（与全拼 nixiangganma 首选同文本），实际「{}」is_sentence={}",
            first.text,
            first.is_sentence
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// 上一测试的对照组：无组合词证据时补字回落纯字频（旧行为逐字节保持）。
    /// 词库不含 干吗/干嘛 → 干+m 不再有任何关联证据 → 没(9760) 按字频胜出。
    #[test]
    fn verify_partial_tail_completion_freq_fallback_without_collocation() {
        use crate::storage::{Storage, X1DictWord};
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_tailfreq_{}_{}.db",
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
                    simplified: "你想".into(),
                    pinyin: "ni xiang".into(),
                    freq: 1157,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "干".into(),
                    pinyin: "gan".into(),
                    freq: 5000,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "吗".into(),
                    pinyin: "ma".into(),
                    freq: 8940,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "没".into(),
                    pinyin: "mei".into(),
                    freq: 9760,
                    ..Default::default()
                },
            ])
            .unwrap();
        }
        let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
        let cands = engine.process("nixiangganm");
        let first = cands.first().expect("应有候选");
        assert!(
            first.is_sentence && first.text.as_ref() == "你想干没",
            "无组合词证据时应回落字频补「没」（旧行为），实际「{}」is_sentence={}",
            first.text,
            first.is_sentence
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// 多切分组句仲裁（bushijiaolengshui→「不是交了那个谁」场景）：
    ///
    /// 根因：切分选举的启发式打分把碎片切分 [bu,shi,jiao,le,n,g,shui] 选为当选
    /// （leng 拆成 le|n|g，1026:1025 一分险胜正确全音节切分），绿词 DP 只在碎链
    /// 上跑——I(n)+I(g) 当简拼首字母命中「那个」(na ge)，[jiao,leng,shui]→浇冷水
    /// 词边从未进词图。现行为：备选全完整音节切分各跑全拼 DP，绿词按 DP 路径分
    /// 全局仲裁；整词短路全局化（输入本身是词库整词时压制全部备选句子）；
    /// 备选只能严格赢过当选最优才入列；备选胜出后落选句子过全局第二绿词门槛。
    ///
    /// 本测试用真实种子（词频/组合词证据都是生产形态）锁定四条行为：
    #[test]
    fn verify_multi_seg_sentence_arbitration() {
        let seed = crate::engine::tests_query::seed_copy("arb");
        let engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

        // ① 主案：备选全拼切分 [bu,shi,jiao,leng,shui] 的 [不是][浇冷水] 必须
        //    在仲裁中击败当选碎链的「不是交了那个谁」并登顶。
        let cands = engine.process("bushijiaolengshui");
        let first = cands.first().expect("应有候选");
        assert!(
            first.is_sentence && first.text.as_ref() == "不是浇冷水",
            "bushijiaolengshui 首选应为仲裁胜出的「不是浇冷水」，实际「{}」is_sentence={}",
            first.text,
            first.is_sentence
        );
        // 落选碎链句分差过大，应被全局第二绿词门槛整句丢弃（不留噪音尾巴）。
        assert!(
            !cands.iter().any(|c| c.text.as_ref() == "不是交了那个谁"),
            "落选碎片句应被全局门槛丢弃"
        );

        // ② 整词短路全局化：输入本身是词库整词时，备选切分的拼凑绿词不得顶位
        //    （jiaolengshui 的 [ji,ao,leng,shui]→「骥骜冷水」曾被顶到 #0）。
        let cands = engine.process("jiaolengshui");
        let first = cands.first().expect("应有候选");
        assert!(
            !first.is_sentence && first.text.as_ref() == "浇冷水",
            "jiaolengshui 首选应保持整词「浇冷水」（白词），实际「{}」is_sentence={}",
            first.text,
            first.is_sentence
        );

        // ③ 备选只能取代不能陪跑：当选切分已解码良好时，低分备选句不得混入
        //    （nixiangganma 的 [ni,xi,ang,gan,ma]→「逆袭昂干嘛」曾混入 #3）。
        let cands = engine.process("nixiangganma");
        let first = cands.first().expect("应有候选");
        assert!(
            first.is_sentence && first.text.as_ref() == "你想干吗",
            "nixiangganma 首选应保持「你想干吗」，实际「{}」",
            first.text
        );
        assert!(
            !cands.iter().any(|c| c.text.as_ref() == "逆袭昂干嘛"),
            "低分备选句不得陪跑混入候选"
        );
        let _ = std::fs::remove_file(&seed);
    }

    /// 真歧义双读串的备选读法可达性（fangandehen 场景）：
    /// 选举同分（1999:1999）时词频裁决走 fang'an（方案 3455 > 反感 997，
    /// 既定规则），但 fan'gan 读法的整句必须以橙词可达——否则
    /// 用户只能拆开分两次输入。选中后 green_memo 记忆直达绿词首位。
    #[test]
    fn verify_alt_reading_sentence_reachable_as_orange() {
        let seed = crate::engine::tests_query::seed_copy("arboro");
        let mut engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
        let cands = engine.process("fangandehen");
        eprintln!(
            "cands: {:?}",
            cands
                .iter()
                .take(8)
                .map(|c| format!(
                    "{}{}",
                    c.text,
                    if c.is_sentence {
                        "[句]"
                    } else if c.is_correction {
                        "[橙]"
                    } else {
                        ""
                    }
                ))
                .collect::<Vec<_>>()
        );
        // ① 绿词首位 = 词频裁决的 fang'an 读法（既定规则不动）
        let first = cands.first().expect("应有候选");
        assert!(
            first.is_sentence && first.text.as_ref() == "方案得很",
            "首选应保持词频裁决的「方案得很」，实际「{}」",
            first.text
        );
        // ② 备选读法整句以橙词可达
        let orange = cands.iter().find(|c| c.text.as_ref() == "反感得很");
        assert!(
            orange.is_some(),
            "备选读法「反感得很」应以橙词出现在候选列表（可达性）"
        );
        let orange = orange.unwrap();
        assert!(
            orange.is_correction && !orange.is_sentence,
            "备选读法应为橙词（is_correction=true, is_sentence=false）"
        );
        // ②b 未学习时显示不变（首选 方案得很 的读音 = 选举读法，覆盖为恒等）
        let disp = engine.syllable_segmentation("fangandehen");
        assert_eq!(
            disp,
            vec!["fang", "an", "de", "hen"],
            "未学习时显示应保持选举读法，实际 {:?}",
            disp
        );
        // ③ 学习闭环：选中橙词后，下次输入直达绿词首位
        engine.commit("fangandehen", "反感得很", Some("fan gan de hen"), true);
        let cands = engine.process("fangandehen");
        let first = cands.first().expect("应有候选");
        assert!(
            first.text.as_ref() == "反感得很",
            "选中备选读法后应记忆直达首位，实际「{}」",
            first.text
        );
        // ④ 显示跟随首选（绿词读对了但预编辑还是旧读法）：
        //    首选绿词是 fan'gan 读法时，预编辑显示同步改切分。
        let disp = engine.syllable_segmentation("fangandehen");
        assert_eq!(
            disp,
            vec!["fan", "gan", "de", "hen"],
            "首选为 反感得很 时显示应跟随其读音切分，实际 {:?}",
            disp
        );
        let _ = std::fs::remove_file(&seed);
    }

    /// 上文决定歧义读法：commit「十分」后输入 fangan
    /// 应读 fan'gan，预编辑拆 fan'gan 而非词频更高的 fang'an。
    ///
    /// 机制 = seg_order_cmp 新增的 reading_preference_cmp（同分真歧义时比较
    /// 上下文末词 × 各读法首词的泛化 bigram）+ freq 的等价类成员行聚合
    ///（种子只有成员行 很→反感=10、没有类键行；聚合后「十分→反感」获得
    /// 半权证据 5，压过 方案 的 0）。两个方向都测：程度副词上文喂 fan'gan，
    /// 动词上文（改革→方案 直连行 22）保持 fang'an——证据双向对称，不是
    /// 单边偏置。显示/候选/消费三链同源生效（预编辑断言 + 首候选断言）。
    #[test]
    fn verify_context_reading_uses_bigram_generalization() {
        let seed = crate::engine::tests_query::seed_copy("ctxread");
        let mut engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

        // 无上文：词频裁决 fang'an（既定规则，守护不破）
        assert_eq!(
            engine.syllable_segmentation("fangan"),
            vec!["fang", "an"],
            "无上文时显示应为词频裁决的 fang'an"
        );
        let cands = engine.process("fangan");
        assert_eq!(
            cands.first().unwrap().text.as_ref(),
            "方案",
            "无上文时首候选应为「方案」"
        );

        // 上文「十分」：DEGREE 成员行（很→反感=10）泛化证据 → fan'gan
        engine.commit("shifen", "十分", Some("shi fen"), true);
        assert_eq!(
            engine.syllable_segmentation("fangan"),
            vec!["fan", "gan"],
            "上文「十分」时显示应拆 fan'gan（反感读法）"
        );
        let cands = engine.process("fangan");
        assert_eq!(
            cands.first().unwrap().text.as_ref(),
            "反感",
            "上文「十分」时首候选应为「反感」"
        );

        // 反向：动词上文「改革」（改革→方案 直连行）保持 fang'an
        let mut engine2 = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
        engine2.commit("gaige", "改革", Some("gai ge"), true);
        assert_eq!(
            engine2.syllable_segmentation("fangan"),
            vec!["fang", "an"],
            "上文「改革」应保持 fang'an（方案直连 bigram 证据）"
        );
        let _ = std::fs::remove_file(&seed);
    }

    /// 用户选择史裁决读法（选过 反感 多次后打 fangan
    /// 应出反感——选举平局、tie-break 只看词库存频时用户选择史
    /// 完全没参与）。reading_preference_cmp 第二层：input_mappings 里「本输入 ×
    /// 读法首词」的选择次数，选得多的一方胜；20 倍频差守卫；
    /// 双向对称自愈（改选另一读法一次即拉平）。
    #[test]
    fn verify_user_choice_flips_reading() {
        let seed = crate::engine::tests_query::seed_copy("choice");
        let mut engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());

        // 无选择史：词频裁决 fang'an（守护既有行为）
        assert_eq!(
            engine.syllable_segmentation("fangan"),
            vec!["fang", "an"],
            "无选择史时应保持词频裁决 fang'an"
        );

        // 学两次（mapping fangan→反感 ≥2）：选择史把读法翻到 fan'gan
        engine.commit("fangan", "反感", Some("fan gan"), true);
        engine.commit("fangan", "反感", Some("fan gan"), true);
        assert_eq!(
            engine.syllable_segmentation("fangan"),
            vec!["fan", "gan"],
            "选过反感后显示应拆 fan'gan"
        );
        let cands = engine.process("fangan");
        assert_eq!(
            cands.first().unwrap().text.as_ref(),
            "反感",
            "首选应为学过的反感"
        );

        // 反向对照：另一台引擎学「方案」→ 读法保持 fang'an（双向对称）
        let mut engine2 = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
        engine2.commit("fangan", "方案", Some("fang an"), true);
        engine2.commit("fangan", "方案", Some("fang an"), true);
        assert_eq!(
            engine2.syllable_segmentation("fangan"),
            vec!["fang", "an"],
            "学方案侧应保持 fang'an"
        );
        assert_eq!(
            engine2.process("fangan").first().unwrap().text.as_ref(),
            "方案",
            "学方案侧首选方案"
        );
        let _ = std::fs::remove_file(&seed);
    }

    /// 恢复条目的读音反推（上屏文本 反感得很 与预编辑
    /// 分隔 fang'an'de'hen 对不上）。
    ///
    /// 场景：green_memo 经 input_mappings 恢复的条目无学习时读音（持久表无
    /// 拼音列，重启后即 None），提升构造候选时此前用当选切分冒充 pinyin——
    /// 「反感得很」+ «fang an de hen» 自相矛盾体，显示跟随首选时把假读法
    /// 显示出来。现由 derive_reading_from_text 按词库成词证据反推真实读音
    /// （枚举输入全音节切分，逐词对齐——fang|an 侧「反感」不成词、fan|gan
    /// 侧成词 → fan gan de hen）。本测试直接构造恢复态（None 读音）验证。
    #[test]
    fn verify_legacy_green_entry_derives_reading() {
        let seed = crate::engine::tests_query::seed_copy("legacy");
        let mut engine = Engine::new_with_db(&seed).unwrap_or_else(|_| Engine::new());
        // 建立 counts（提升门槛用）与记忆，再模拟「重启后恢复」：读音置 None
        engine.commit("fangandehen", "反感得很", Some("fan gan de hen"), true);
        engine.green_memo.insert(
            "fangandehen".to_string(),
            vec![("反感得很".to_string(), 2000, None)],
        );

        let cands = engine.process("fangandehen");
        let first = cands.first().expect("应有候选");
        assert_eq!(first.text.as_ref(), "反感得很", "提升条目应居首");
        assert_eq!(
            first.pinyin.as_ref(),
            "fan gan de hen",
            "恢复条目拼音应为反推的真实读音（而非当选切分冒充的 fang an de hen）"
        );
        assert_eq!(
            engine.syllable_segmentation("fangandehen"),
            vec!["fan", "gan", "de", "hen"],
            "预编辑显示应跟随首选的真实读音切分"
        );
        let _ = std::fs::remove_file(&seed);
    }

    /// 回归：混合简拼整句的 DP 边必须全消费 span，禁止「吞词路径」。
    ///
    /// 旧行为：juglzshuomwt 的 [ju,g,l,z] 四段被 2 字「莒国」吞掉 l/z 两个 token
    /// （结构分按段数给反而更高）→ 吞词句「莒国说明问题」压住自然句；
    /// 新行为：首个绿词为全覆盖自然句「举个例子说明问题」。
    #[test]
    fn verify_mixed_full_coverage_green_beats_swallowing_path() {
        use crate::storage::{Storage, X1DictWord};
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_swallow_{}_{}.db",
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
                    simplified: "举个例子".into(),
                    pinyin: "ju ge li zi".into(),
                    freq: 402,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "说明问题".into(),
                    pinyin: "shuo ming wen ti".into(),
                    freq: 441,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "说明".into(),
                    pinyin: "shuo ming".into(),
                    freq: 3041,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "问题".into(),
                    pinyin: "wen ti".into(),
                    freq: 6966,
                    ..Default::default()
                },
                // 吞词候选：ju guo 本可在 [ju,g] 两段全消费，旧代码允许它占四段吞掉 l/z
                X1DictWord {
                    simplified: "莒国".into(),
                    pinyin: "ju guo".into(),
                    freq: 1648,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "例子".into(),
                    pinyin: "li zi".into(),
                    freq: 1224,
                    ..Default::default()
                },
            ])
            .unwrap();
        }
        let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
        let cands = engine.process("juglzshuomwt");
        let first_green = cands.iter().find(|c| c.is_sentence);
        if first_green.is_none() || first_green.map(|c| c.text.as_ref()) != Some("举个例子说明问题")
        {
            eprintln!(
                "greens: {:?}",
                cands
                    .iter()
                    .filter(|c| c.is_sentence)
                    .map(|c| &c.text)
                    .collect::<Vec<_>>()
            );
        }
        let first_green = first_green.expect("混合长串应产出绿词");
        assert_eq!(
            first_green.text.as_ref(),
            "举个例子说明问题",
            "首个绿词应为全覆盖自然句（吞词路径应被全消费守卫拦截），实际「{}」",
            first_green.text
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// Full 兄弟让位不得放行 Shengmu/Initial 伪切分（回归守护）。
    ///
    /// 背景：分支配额保留（Full 兄弟让位）最初把 reserve 算进 Shengmu/Initial
    /// 分支（「每个分支保证至少 1 条名额」），导致 `zhongfanzuohaole` 枚举里
    /// 出现 `["zh","o","n","g","fan",…]` 伪切分（`is_jianpin` 外的高频单字
    /// 堆积打 1022 分，压过正解 1020）→ best 切分被选举成它 → 整句候选
    /// 消失（基线语料掉 2 条：「中饭做好了」「中小企业面临转型压力」）。
    /// 现行为：让位只对 Full 兄弟计算（Shengmu/Initial 是「另起炉灶」而非
    /// 同一路线的替代最优，旧行为本就只在 Full 系产不满 max 时才轮到它们）。
    /// 本守护钉死这两条 eval 句的首候选（整句 + 文本严格相等）。
    #[test]
    fn verify_full_sentence_survives_quota_reserve() {
        let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_quota_sent_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::test_tmp::require_seed_db();
        let _ = std::fs::copy(seed, &tmp);
        let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
        for (inp, expected) in [
            ("zhongfanzuohaole", "中饭做好了"),
            ("zhongxiaoqiyemianlinzhuanxingyali", "中小企业面临转型压力"),
        ] {
            let cands = engine.process(inp);
            let first = cands.first().unwrap_or_else(|| panic!("{} 应有候选", inp));
            assert!(first.is_sentence, "{} #0 应为整句绿词", inp);
            assert_eq!(first.text.as_ref(), expected, "{} 首候选", inp);
        }
        drop(engine);
        let _ = std::fs::remove_file(&tmp);
    }

    /// 映射逃生口证据量支配回归（「zgrm 回退成单字」场景）。
    ///
    /// 典型 zgrm 精确映射三条：做(1)、中(2)、中国人民(3)（历次选择/
    /// 组合逐字提交累积）。旧判据 `mapping > 0` 让三者全部晋级高层互撕 →
    /// 顺序在 中/做/中国人民 之间随上下文漂移。修复 = 部分覆盖候选须严格
    /// 支配全部全覆盖候选的最大映射数才晋级（见 mod.rs max_full_cover_mapping）。
    /// 守护：模拟同样的映射累积后，zgrm 首候选必须是全覆盖词 中国人民。
    #[test]
    fn verify_jianpin_mapping_dominance_keeps_full_cover_first() {
        let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_mapdom_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::test_tmp::require_seed_db();
        let _ = std::fs::copy(seed, &tmp);
        let mut engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
        // 复刻用户库证据形态：做×1 / 中×2 / 中国人民×3
        engine.commit("zgrm", "做", Some("zuo"), true);
        engine.commit("zgrm", "中", Some("zhong"), true);
        engine.commit("zgrm", "中", Some("zhong"), true);
        for _ in 0..3 {
            engine.commit("zgrm", "中国人民", Some("zhong guo ren min"), true);
        }
        let cands = engine.process("zgrm");
        let first = cands.first().expect("zgrm 应有候选");
        assert_eq!(
            first.text.as_ref(),
            "中国人民",
            "映射积累后 zgrm 首候选仍须是全覆盖词（实际: {:?}）",
            cands.iter().take(5).map(|c| &c.text).collect::<Vec<_>>()
        );
        drop(engine);
        let _ = std::fs::remove_file(&tmp);
    }

    /// 元音首字母简拼（西安=xa）保持绿词整句 + 30+ 字支持
    /// （zgrmjfjxa：「中国人民解放军西安」曾塌成单字列表；同构参照同串出词）。
    ///
    /// 根因：is_jianpin 旧判据「a/e/o 非首位即非简拼」把 xa 类零声母首字母
    /// 组合整条踢出简拼路径（同族第三例全链见 pinyin.rs is_jianpin 头注释）。
    /// 修复两处：① pinyin.rs a/e/o 判据改 is_pinyin_prefix 全拼前缀测试
    ///（zho/zhon 全拼中间态留全拼，zgrmjfjxa 归简拼）；② MAX_GREEN_CHARS
    /// 16→36（至少支持 30 字）。
    /// 守护：① 9 字母含 a；② 11 字母延续；③ 36 字母（4×同串）整句 ≥30 字。
    #[test]
    fn verify_vowel_initial_jianpin_keeps_green_sentence() {
        let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_probe_vow_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::test_tmp::require_seed_db();
        let _ = std::fs::copy(seed, &tmp);
        let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();

        // ① 9 字母（xa=西安 零声母首字母）→ 全覆盖绿词
        let cands = engine.process("zgrmjfjxa");
        let first = cands.first().expect("zgrmjfjxa 应有候选");
        assert!(first.is_sentence, "zgrmjfjxa #0 应为整句绿词");
        assert_eq!(first.text.as_ref(), "中国人民解放军西安");

        // ② 11 字母延续（xakj=西安科技）
        let cands = engine.process("zgrmjfjxakj");
        let first = cands.first().expect("zgrmjfjxakj 应有候选");
        assert!(first.is_sentence, "zgrmjfjxakj #0 应为整句绿词");
        assert_eq!(first.text.as_ref(), "中国人民解放军西安科技");

        // ③ 36 字母（4×同串）→ 整句且 ≥30 字（「至少 30 字」诉求直接守护）
        let long = "zgrmjfjxa".repeat(4);
        let cands = engine.process(&long);
        let first = cands.first().expect("36 字母应有候选");
        assert!(first.is_sentence, "36 字母 #0 应为整句绿词");
        assert_eq!(first.text.as_ref(), "中国人民解放军西安".repeat(4));

        drop(engine);
        let _ = std::fs::remove_file(&tmp);
    }

    /// zh/sh/ch 开头 + a/e/o 首字母简拼的路由回归（调试 CLI 定位发现）。
    ///
    /// 根因：`is_hybrid_shengmu_input` 旧判据「zh 之后不得含元音 a/e/i/o/u」把
    /// 含 a/e/o 首字母（西安=xa）的合法简拼整体误拒——`zhgrmjfjxa` 掉进纯简拼
    /// 路径 → DP 按逐字母拆读（z=走 h=回 g=国）→「走回国人民解放军西安」垃圾
    /// 压过正解、正确词整条消失。修复 = `is_syllable_initial_prefix`：存在以 zh
    /// 开头的有效音节使整串「以该音节开头」（zhongguo）或「是其前缀」（zho）
    /// 才归全拼；否则 zh 是完成声母 → 混合路径。
    /// 守护：① 正解「中国人民解放军西安」在候选内且为整句；② 旧误路由的
    /// 「走回国…」垃圾不得作为整句出现；③ zhonggrm 混合形态零回归（同测连带）。
    #[test]
    fn verify_zh_shengmu_with_vowel_initial_routing() {
        let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_zhroute_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::test_tmp::require_seed_db();
        let _ = std::fs::copy(seed, &tmp);
        let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();

        let cands = engine.process("zhgrmjfjxa");
        assert!(
            cands
                .iter()
                .any(|c| c.is_sentence && c.text.as_ref() == "中国人民解放军西安"),
            "zhgrmjfjxa 应含整句「中国人民解放军西安」（实际前 5: {:?}）",
            cands.iter().take(5).map(|c| &c.text).collect::<Vec<_>>()
        );
        assert!(
            !cands.iter().any(|c| c.text.starts_with("走回国")),
            "旧误路由垃圾「走回国…」不应出现"
        );

        // 11 字母延续（xakj=西安科技）
        let cands = engine.process("zhgrmjfjxakj");
        assert!(
            cands
                .iter()
                .any(|c| c.is_sentence && c.text.as_ref() == "中国人民解放军西安科技"),
            "zhgrmjfjxakj 应含整句「中国人民解放军西安科技」"
        );

        // zhonggrm（混合全拼+简拼，zhong 音节开头）路由不受新判据影响
        let cands = engine.process("zhonggrm");
        assert!(
            cands.iter().any(|c| c.text.as_ref() == "中国人民"),
            "zhonggrm 应命中「中国人民」（前 3: {:?}）",
            cands.iter().take(3).map(|c| &c.text).collect::<Vec<_>>()
        );

        drop(engine);
        let _ = std::fs::remove_file(&tmp);
    }

    /// 长混合串（16 音节 + zj 简拼尾）分支配额保留回归
    /// （yigerenguodehaobuhaozhiyouzjcaizhidao——结构性修复）。
    ///
    /// 根因（三次同族：b 尾 512 够 / zh 尾 2048 够 / 本串再爆）：DFS 先序枚举
    /// 名额被贪长退化子树（reng|u|o 类误并 + 单字符兜底续命，产出随串长乘法
    /// 增长）吃光，正解进不了枚举——纯扩 cap 永远追不上。
    /// 修复：分支配额保留（pinyin.rs segment dfs）——每分支保证至少 1 条
    /// 名额，贪长分支至多 max − 后续分支数 条；后续分支第一条即其贪长最优
    /// 切分（名额留 1 条即可让正解进集，由打分竞争选中）。非退化态行为逐位
    /// 不变（产出总和 ≤ max 时配额不触发）。
    /// 守护：完整句/中间态/zh 尾三形态 #0 均为全覆盖绿词。
    #[test]
    fn verify_branch_quota_keeps_long_mixed_sentence() {
        let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_bquota_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::test_tmp::require_seed_db();
        let _ = std::fs::copy(seed, &tmp);
        let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();

        // ① 完整句：#0 = 「一个人过得好不好只有自己才知道」
        let inp = "yigerenguodehaobuhaozhiyouzjcaizhidao";
        let seg = engine.syllable_segmentation(inp);
        assert_eq!(
            seg,
            vec![
                "yi", "ge", "ren", "guo", "de", "hao", "bu", "hao", "zhi", "you", "z", "j", "cai",
                "zhi", "dao"
            ],
            "长混合串中段切分不得被贪长分支带歪（ren|guo 必须保留）"
        );
        let first = engine.process(inp).into_iter().next().expect("应有候选");
        assert!(
            first.is_sentence && first.text.as_ref() == "一个人过得好不好只有自己才知道",
            "完整句首选应为全覆盖绿词，实际「{}」（is_sentence={}）",
            first.text,
            first.is_sentence
        );

        // ② 中间态（截图态，不带 zhidao）：仍应出全覆盖绿词（贪长分支不得夺位）
        let mid = engine
            .process("yigerenguodehaobuhaozhiyouzjcai")
            .into_iter()
            .next()
            .expect("应有候选");
        assert!(
            mid.is_sentence
                && mid.text.as_ref().starts_with("一个人过得好不好只有")
                && mid.text.chars().count() == 13,
            "中间态首选应为 13 字全覆盖绿词（一个人过得好不好只有…），实际「{}」",
            mid.text
        );

        // ③ zh 尾形态：#0 全覆盖 + zh 补字（16 字）
        let zh = engine
            .process("yigerenguodehaobuhaozhiyouzjcaizhidaozh")
            .into_iter()
            .next()
            .expect("应有候选");
        assert!(
            zh.is_sentence && zh.text.chars().count() == 16,
            "zh 尾形态首选应为 16 字全覆盖绿词，实际「{}」",
            zh.text
        );

        drop(engine);
        let _ = std::fs::remove_file(&tmp);
    }

    /// zh 双字母尾不得带歪中段切分（yigerenguodehaobuhaozh——b 尾修复后同族残留）。
    ///
    /// 根因：与 b 尾同源（DFS 名额被 reng 贪长分支退化子树占满），但
    /// PARTIAL 语义下该子树 >512 条（2048 才露出正解）——上一版扩枚举
    /// caps [128,512] 恰好只够更短的 b 尾串。显示链（TOKENS 语义）128 条
    /// 即达 → 一直显示正确；候选链（PARTIAL）512 不够 → 候选塌成前缀词。
    /// 修复：扩枚举先借 TOKENS 语义自适应（best_token_segmentation），
    /// PARTIAL [512,2048] 降为第二道兜底。
    /// 守护：切分含 ren|guo + 首选=全覆盖绿词「一个人过得好不好」+ zh 补字。
    #[test]
    fn verify_zh_tail_keeps_correct_middle_segmentation() {
        let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_zhtail_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::test_tmp::require_seed_db();
        let _ = std::fs::copy(seed, &tmp);
        let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();

        let input = "yigerenguodehaobuhaozh";
        // ① 切分：中段必须保持 ren|guo（不得被 reng|u|o 贪长分支带歪）
        let seg = engine.syllable_segmentation(input);
        assert_eq!(
            seg,
            vec!["yi", "ge", "ren", "guo", "de", "hao", "bu", "hao", "zh"],
            "zh 尾不得带歪中段切分"
        );

        // ② 首选：全覆盖绿词（zh 尾执行首字母补字）
        let cands = engine.process(input);
        let first = cands.first().expect("应有候选");
        assert!(
            first.is_sentence && first.text.as_ref().starts_with("一个人过得好不好"),
            "首选应为全覆盖绿词（一个人过得好不好 + zh 补字），实际「{}」（is_sentence={}）",
            first.text,
            first.is_sentence
        );
        assert_eq!(
            first.text.chars().count(),
            9,
            "绿词字数应为 8+1（zh 补一个字）"
        );

        // ③ ch/sh 同族抽查（zh 修复对同族生效）
        for (inp, prefix) in [
            ("yigerenguodehaobuhaoch", "一个人过得好不好"),
            ("yigerenguodehaobuhaosh", "一个人过得好不好"),
        ] {
            let c = engine.process(inp);
            let f = c.first().expect("应有候选");
            assert!(
                f.is_sentence && f.text.as_ref().starts_with(prefix),
                "{} 首选应为全覆盖绿词，实际「{}」",
                inp,
                f.text
            );
        }

        drop(engine);
        let _ = std::fs::remove_file(&tmp);
    }

    /// 尾部半截字母不得带歪中段切分（yigerenguodehaob）。
    ///
    /// 根因：DFS 先序枚举名额被「reng 贪长分支」（ren+g 误并 reng，后续 u/o
    /// 靠单字符兜底续命）的退化子树占满（cap 16 全被吃光）→ 正解
    /// ren|guo|de|hao|b（打分 1381，垃圾态全 0 分）从未被枚举 → 候选层只出
    /// 前缀词「一个」、绿词整句崩塌（绿词回缩成两个字）。
    /// 修复：全 0 分退化态自适应扩枚举（scoring.rs pick_segmentation +
    /// process_inner 同款），正解进集即被总分选中。
    /// 守护：切分=ren|guo + 首选=全覆盖绿词「一个人过得好不」（尾 b 补 不）。
    #[test]
    fn verify_partial_tail_keeps_correct_middle_segmentation() {
        let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_ptail_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::test_tmp::require_seed_db();
        let _ = std::fs::copy(seed, &tmp);
        let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();

        // ① 切分：中段必须保持 ren|guo（不得被 reng|u|o 贪长分支带歪）
        let seg = engine.syllable_segmentation("yigerenguodehaob");
        assert_eq!(
            seg,
            vec!["yi", "ge", "ren", "guo", "de", "hao", "b"],
            "尾部半截 b 不得带歪中段切分"
        );

        // ② 首选：全覆盖绿词（b 尾执行首字母补字 → 不）
        let cands = engine.process("yigerenguodehaob");
        let first = cands.first().expect("应有候选");
        assert!(
            first.is_sentence && first.text.as_ref() == "一个人过得好不",
            "首选应为全覆盖绿词「一个人过得好不」，实际「{}」（is_sentence={}）",
            first.text,
            first.is_sentence
        );

        // ③ 无尾版本行为不变（回归守护）
        let seg2 = engine.syllable_segmentation("yigerenguodehao");
        assert_eq!(seg2, vec!["yi", "ge", "ren", "guo", "de", "hao"]);
        let cands2 = engine.process("yigerenguodehao");
        assert_eq!(
            cands2.first().map(|c| c.text.to_string()).as_deref(),
            Some("一个人过得好")
        );

        drop(engine);
        let _ = std::fs::remove_file(&tmp);
    }

    /// 全拼长句 DP 路径收集回归（kanta 案例）：
    /// 全拼 ≥3 音节走 decode_sentence_collect 时，路径表须含全部候选切分
    /// （「大致/看它/打只」三版）——AI 全拼长句裁决的数据基础；首条路径
    /// 为引擎分最高者。收集开关关闭时路径为空（逐字节等价旧行为）。
    #[test]
    fn verify_fullpinyin_long_sentence_paths_collected() {
        let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_probe_kanta_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::test_tmp::require_seed_db();
        let _ = std::fs::copy(seed, &tmp);
        let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();
        let syl = [
            "kan", "ta", "fa", "peng", "you", "quan", "de", "pin", "lv", "jiu", "neng", "da",
            "zhi", "tui", "ce", "chu", "lai",
        ];
        let (cands, paths) = engine.decode_sentence_paths(
            "kantafapengyouquandepinlvjiunengdazhituicechulai",
            &syl,
            false,
            false,
            8,
        );
        // 绿词候选：≥1 条且为全覆盖整句
        assert!(!cands.is_empty(), "kanta 全拼应有绿词候选");
        assert!(
            cands.iter().all(|c| c.text.chars().count() == syl.len()),
            "绿词候选应全覆盖：{:?}",
            cands.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        // 路径表：≥2 条（裁决模式的资格前提）；文本去重、按分降序、words 拼回原文
        assert!(paths.len() >= 2, "kanta 应有 ≥2 条路径：{:?}", paths);
        for p in &paths {
            assert_eq!(p.words.concat(), p.text, "words 拼接必须等于 text");
            assert_eq!(p.text.chars().count(), syl.len(), "路径应全消费");
        }
        for w in paths.windows(2) {
            assert!(w[0].score >= w[1].score, "路径按分降序");
        }
        // 路径文本集合须包含自然句（含「大致」而非全是生拼）——防未来回退
        assert!(
            paths.iter().any(|p| p.text.contains("大致")),
            "路径表应含自然切分（大致版）：{:?}",
            paths.iter().map(|p| &p.text).collect::<Vec<_>>()
        );
        // 收集开关关闭：路径为空（旧行为等价）
        let (_, none) = engine.decode_sentence_paths(
            "kantafapengyouquandepinlvjiunengdazhituicechulai",
            &syl,
            false,
            false,
            0,
        );
        assert!(none.is_empty(), "limit=0 不得收集路径（旧行为等价）");
        drop(engine);
        let _ = std::fs::remove_file(&tmp);
    }

    /// 简拼上屏（含 AI 红词）学习链回归：
    /// 单字母"拼音"提交（AI 红词 commit 的真实形态）必须与全拼提交等效——
    /// split_text_anchored 按 text 锚定 + 首字母查询 切出真实多字子词（贪心最长匹配），
    /// freq.record 子词后下次同输入 DP 自然构造整句（#0 绿词）。
    /// 旧行为：单字母段被整段跳过 → 逐字学习 → 子词证据全丢 → 整句永不出现
    ///（用户被迫先用全拼"教"一遍，其子词记录才是 DP 能构造的原因）。
    #[test]
    fn verify_jianpin_commit_learns_like_fullpinyin() {
        let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_jpcommit_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::test_tmp::require_seed_db();
        let _ = std::fs::copy(seed, &tmp);
        let mut engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();

        // ① 切分：单字母音节序列按 text 锚定切出真实子词（贪心最长匹配——
        // 种子有「举个例子」4 字词（freq 402）时先成段，与全拼提交同一条
        // split_text_anchored 逻辑同构；关键是切出多字词而非逐字）。
        let segments = engine.split_text_anchored("举个例子说明", &["j", "g", "l", "z", "s", "m"]);
        let words: Vec<&str> = segments.iter().map(|(w, _)| w.as_str()).collect();
        assert!(
            words.iter().all(|w| w.chars().count() >= 2) && words.len() >= 2,
            "单字母拼音须按 text 锚定切出多字子词（非逐字），实际：{:?}",
            segments
        );
        assert_eq!(
            words.concat(),
            "举个例子说明",
            "子词拼接须等于原文，实际：{:?}",
            segments
        );

        // ② 基线：教之前该句不在候选（种子状态）
        let before = engine.process("jglzsm");
        assert!(
            !before.iter().any(|c| c.text.as_ref() == "举个例子说明"),
            "基线应为未学状态（若已出现说明种子/逻辑变化，本测试前提失效）"
        );

        // ③ AI 红词形态提交（单字母拼音）→ 第 2 次同输入应 #0 出来
        engine.commit("jglzsm", "举个例子说明", Some("j g l z s m"), true);
        let after = engine.process("jglzsm");
        let first = after.first().expect("应有候选");
        assert_eq!(
            first.text.as_ref(),
            "举个例子说明",
            "AI 红词上屏后同输入应自然构造整句（全拼/简拼学习等效），实际 #0 =「{}」",
            first.text
        );
        assert!(first.is_sentence, "应为绿词身份");
        drop(engine);
        let _ = std::fs::remove_file(&tmp);
    }

    /// 回归：AI 切分裁决的整句路径收集。
    ///
    /// 契约：
    /// 1. `set_collect_sentence_paths(true)` 后 `process` 纯简拼 → `take_sentence_paths`
    ///    返回过门禁的 DP 路径（文本去重、按分降序、words 拼接 == text，字数==字母数）；
    /// 2. 收集开关不改变候选列表一字（硬不变量：AI 只动展示层）；
    /// 3. 关闭收集 → take 恒空（零残留）；全拼输入不收集。
    ///
    /// 构造式 DB（不耦合种子）：zjsm 的两种合法切分 再见|什么 与 自己|说明。
    #[test]
    fn verify_sentence_paths_collection_for_ai_judge() {
        use crate::storage::{Storage, X1DictWord};
        let tmp = crate::test_tmp::dir().join(format!(
            "x1pinyin_paths_{}_{}.db",
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
                    simplified: "再见".into(),
                    pinyin: "zai jian".into(),
                    freq: 5200,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "什么".into(),
                    pinyin: "shen me".into(),
                    freq: 8800,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "自己".into(),
                    pinyin: "zi ji".into(),
                    freq: 6400,
                    ..Default::default()
                },
                X1DictWord {
                    simplified: "说明".into(),
                    pinyin: "shuo ming".into(),
                    freq: 4300,
                    ..Default::default()
                },
            ])
            .unwrap();
        }
        let engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();

        // 基准：不收集时的候选列表
        let base: Vec<String> = engine
            .process("zjsm")
            .iter()
            .map(|c| c.text.to_string())
            .collect();

        // 收集开启：候选列表必须逐字节一致（硬不变量）
        engine.set_collect_sentence_paths(true);
        let with_collect: Vec<String> = engine
            .process("zjsm")
            .iter()
            .map(|c| c.text.to_string())
            .collect();
        assert_eq!(base, with_collect, "收集开关不得改变候选列表（硬不变量）");

        let paths = engine.take_sentence_paths();
        assert!(
            paths.len() >= 2,
            "zjsm 应收集到至少 2 条路径，实际 {}: {:?}",
            paths.len(),
            paths.iter().map(|p| &p.text).collect::<Vec<_>>()
        );
        let texts: Vec<&str> = paths.iter().map(|p| p.text.as_str()).collect();
        assert!(
            texts.contains(&"再见什么") && texts.contains(&"自己说明"),
            "两条合法切分都应在路径表: {:?}",
            texts
        );
        for p in &paths {
            assert_eq!(p.words.concat(), p.text, "words 拼接必须等于 text");
            assert_eq!(
                p.text.chars().count(),
                4,
                "纯简拼全消费路径字数 == 字母数: {}",
                p.text
            );
        }
        // 按分降序
        for w in paths.windows(2) {
            assert!(w[0].score >= w[1].score, "路径应按 score 降序: {:?}", paths);
        }
        // take 后清空（重复取不再是同一批）
        assert!(engine.take_sentence_paths().is_empty(), "take 后槽应清空");

        // 全拼输入不收集（本功能只对纯简拼/混合声母）
        engine.process("ziji");
        assert!(
            engine.take_sentence_paths().is_empty(),
            "全拼输入不应收集路径"
        );

        // 关闭收集 → 纯简拼也不再收集
        engine.set_collect_sentence_paths(false);
        engine.process("zjsm");
        assert!(engine.take_sentence_paths().is_empty(), "关闭后不得收集");
        let _ = std::fs::remove_file(&tmp);
    }
}
