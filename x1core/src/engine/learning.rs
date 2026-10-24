//! 学习/commit（从 engine/mod.rs 抽出）。

use alloc::string::String;
use alloc::vec::Vec;

#[allow(unused_imports)]
use super::{is_valid_chinese_text, normalize_pinyin_for_storage};

impl crate::engine::Engine {
    /// 用户选择了某个候选，记录到频率模型与输入习惯映射。
    ///
    /// `input` 是用户实际输入的原始拼音串（如 "zhongwen" / "zw" / "went"）。
    /// `pinyin` 是候选的拼音（空格分隔的音节）。对于多字系统词，若用户词库中
    /// 还没有该词，会尝试从基础词库找到拼音并写入 `user_words`，避免生成
    /// `pinyin` 为空的记录。单字候选不进入用户词库。
    ///
    /// `advance_context`：是否推进 bigram 上下文栈。普通提交传 `true`（记录上文共现 +
    /// 重置上文栈为当前词）；自组词分步提交传 `false`（避免把整词拆成单字污染 bigram），
    /// 自组词收尾走 `learn` 由其内部记整词上文。
    pub fn commit(&mut self, input: &str, text: &str, pinyin: Option<&str>, advance_context: bool) {
        self.commit_internal(input, text, pinyin, advance_context, false, false);
    }

    /// 用户选择候选并指定是否固化整句为白词。
    ///
    /// 与 `commit` 区别：`force_solidify=true` 时，整句（≤8字）会固化进 user_words。
    /// 用于 Shift+数字 手动固化快捷键——用户明确想把这个绿词"固定"成白词时才固化。
    /// 普通数字键选词走 `commit`（force_solidify=false），只拆词+bigram 学习。
    pub fn commit_with_solidify(
        &mut self,
        input: &str,
        text: &str,
        pinyin: Option<&str>,
        advance_context: bool,
        force_solidify: bool,
    ) {
        self.commit_internal(input, text, pinyin, advance_context, force_solidify, false);
    }

    /// 橙词纠错候选的提交。
    ///
    /// 与 `commit_with_solidify` 的唯一区别：is_correction=true → 不写
    /// 错串→词的 input_mapping。错串映射会让 nihso 这类拼错串下次白词
    /// 直达，与纠错「纯只读」哲学矛盾，且误选一次永久留痕（纠错撤销不回滚
    /// mapping）；纠错路径本身已能稳定复纠，错串直达属冗余。freq/bigram/
    /// 竞争事件照常学习——那些学的是正确词，正是用户意图。
    pub fn commit_correction(
        &mut self,
        input: &str,
        text: &str,
        pinyin: Option<&str>,
        advance_context: bool,
        force_solidify: bool,
    ) {
        self.commit_internal(input, text, pinyin, advance_context, force_solidify, true);
    }

    pub(crate) fn commit_internal(
        &mut self,
        input: &str,
        text: &str,
        pinyin: Option<&str>,
        advance_context: bool,
        force_solidify: bool,
        is_correction: bool,
    ) {
        // 智能单词识别：英文单词上屏不做中文学习——
        // 含 ASCII 字母的 text 一律跳过 freq/input_mappings/user_words/green_memo/
        // 搭配上下文，防污染中文排序与 Ranker 训练数据。
        //
        // 英文词频动态调整（开了英文补全则英文词频随使用调整，
        // 中/英文模式皆然）：英文词自身的使用计数在此记录（独立通道，不碰
        // 中文任何表）。词形约束复用 is_word_shape（ASCII 字母 2..=24，允许
        // 规范大小写；计数键归一小写）；门控 =
        // smartWordZh/En 任一开（补全都关 = 无消费面，不学）。归一小写：中文
        // 模式英文候选恒小写，英文模式直打可能带大写（句首），同一词计数合并。
        // 学习只影响 english_complete/inject_english_candidates 的有效词频
        // （english.rs eff_freq）；eval（关英文注入口径）零影响。
        if text.chars().any(|c| c.is_ascii_alphabetic()) {
            if self.smart_word_en_enabled || self.smart_word_zh_enabled {
                let w = text.to_ascii_lowercase();
                if crate::engine::english::is_word_shape(&w) {
                    *self.user_english.entry(w.clone()).or_insert(0) += 1;
                    *self.english_delta.entry(w).or_insert(0) += 1;
                }
            }
            return;
        }
        // 双拼展开：双拼模式下宿主前端传入的 pinyin 是双拼编码，
        // 必须 expand 成全拼再记学习，否则学习数据被污染（与全拼用户不互通）。
        // scheme=None 时 no-op：decode_to_pinyin_string 不调用，pinyin_owned 持原值副本。
        // 见 spec §4.4。commit / commit_with_solidify 都走本函数，统一在此展开。
        let pinyin_owned: Option<String> = if let Some(scheme) = &self.shuangpin {
            pinyin.map(|p| {
                crate::shuangpin::decode_to_pinyin_string(scheme, p)
                    .unwrap_or_else(|| p.to_string())
            })
        } else {
            pinyin.map(|p| p.to_string())
        };
        // 阴影原 pinyin（Option<&str>）为 Option<&str> 指向 owned 副本，下游零改动。
        let pinyin: Option<&str> = pinyin_owned.as_deref();

        self.freq.record(text);

        // 用户主动选字上屏 = 改主意了。若该词在黑名单（remove 所删），复活它：
        // 人的想法会变，主动选择说明又想要这个词，不应再屏蔽整句解码。
        // 同时清除持久化黑名单（user_blacklist 表），让下次部署不再清理该词。
        self.removed_texts.remove(text);
        if let Some(storage) = &mut self.storage {
            let _ = storage.remove_blacklist(text);
        }

        // 记录拼音级选择次数（多音字主音/次音动态权重）。
        // 按 (text, pinyin) 维度统计，避免跨音串扰：用户在 de 选「地」
        // 只累加 (地, de)，不影响 di 查询的「地」。
        //
        // 只对单字记录：多音字权重仅在单字排序（primary_reading_bonus）中生效，
        // 多字词的主音恒为 true，记录它们是纯冗余（会让 user_pinyin_counts 表
        // 充斥多字词，清理时需一次性删上千条）。
        // 若调用方未传 pinyin，从 input 推断首音节（兼容旧接口）。
        if text.chars().count() == 1 {
            let pinyin_for_record = pinyin
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(|p| p.to_string())
                .or_else(|| {
                    if !input.is_empty() {
                        // 从用户输入推断首音节（如 input="ni" → "ni"）
                        crate::pinyin::segmentations(input, 4)
                            .into_iter()
                            .next()
                            .and_then(|segs| segs.into_iter().next().map(|s| s.to_string()))
                    } else {
                        None
                    }
                });
            if let Some(p) = pinyin_for_record {
                self.freq.record_with_pinyin(text, &p);
            }
        }

        // 竞争事件（「退化」通道）：
        // 被选词记一次「选中」——按输入形态分两层写：
        //   全拼/双拼 → 同音层（word 的完整拼音 ctx；同音竞争的本来语义）；
        //   简拼/混拼（输入 token 非完整音节）→ 整句层（原始输入串 ctx，
        //   与 scoring.rs 的 jianpin 分支 key 对齐——sh 下 是/说/上/社会
        //   同场；绝不把单字错切进它自己的音节语境——test_user_feedback 的
        //   sh 案例守护此点）。
        // 组词层（Bigram）写侧复用 user_bigram_counts（advance_context 段），
        // 不在此重复记。Input 的 \u{1} 前缀是简拼整句层 key 的命名空间约定
        // （与拼音形态的全拼 key 隔开，先验计算据此分流，见 pin_priors_full）。
        {
            let jp_input = crate::pinyin::is_jianpin(input)
                || (input.len() == 2 && matches!(input, "zh" | "ch" | "sh"));
            if jp_input {
                self.competition.record_choice(
                    &[crate::competition::CtxKey::Input(format!(
                        "\u{1}{}",
                        input.trim_matches('\'')
                    ))],
                    text,
                );
            } else {
                let pinyin_full: Option<String> = pinyin
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(|p| p.to_string())
                    .or_else(|| {
                        if !input.is_empty() {
                            crate::pinyin::segmentations(input, 4)
                                .into_iter()
                                .next()
                                .map(|segs| segs.join(" "))
                        } else {
                            None
                        }
                    })
                    .or_else(|| self.dict.find_pinyin(text));
                if let Some(py) = pinyin_full {
                    // 上下文同音证据：记住「在 prev 之后，
                    // 该拼音槽用户选了 text」。只记录词库整词 + 真同音歧义组
                    // （≥2 词、gap≤5×），整句提交由 commit_sentence_split 按子词
                    // 记录（避免把整句拼音当槽键）。
                    let syls: Vec<&str> = py.split_whitespace().collect();
                    let is_whole_dict_word = !syls.is_empty()
                        && self
                            .dict
                            .query(&syls)
                            .iter()
                            .any(|c| c.text.as_ref() == text);
                    if is_whole_dict_word && self.is_ambiguous_pinyin(&syls) {
                        // advance_context=false = 自组词中间步，不是独立意图；
                        // repeated（与上文同词）= 测试/修改序列，不进语境证据。
                        if advance_context {
                            if let Some(prev) = self.context.last() {
                                if prev.as_str() != text {
                                    self.competition.record_choice(
                                        &[crate::competition::CtxKey::PinCtx {
                                            pinyin: py.clone(),
                                            prev: prev.clone(),
                                        }],
                                        text,
                                    );
                                }
                            }
                        }
                    }
                    self.competition
                        .record_choice(&[crate::competition::CtxKey::Pin(py)], text);
                }
            }
        }

        // 记录原始输入到选择文本的映射，用于强化个人简拼/混合输入习惯。
        // 经 record_input_mapping 同步反向索引（加速 candidate_score 的 prefix 查询）。
        // 橙词纠错提交不写错串映射——input 是拼错
        // 的串，持久化成 mapping 会让错串下次白词直达（纠错路径已能稳定复纠，
        // 直达属冗余；误选则永久留痕且纠错撤销不回滚 mapping）。
        if self.input_mapping_enabled && !input.is_empty() && !is_correction {
            self.record_input_mapping(input, text);
            self.dirty = true;
        }

        // 本次提交是否走了整句拆词学习分支（绿词确认）。外层 advance_context 块
        // 据此跳过 (末子词→整词) 的伪 bigram/trigram 记录（见该块内注释）。
        let mut sentence_split_commit = false;

        // 只有多字词需要写 user_words 表；单字由系统词库提供，不写入。
        // 防御：拒绝含 ASCII 字母的候选入库（如整句解码 bug 产出的「系w」「文T」，
        // 或早期词表来源的字母脏词条），避免脏候选持久化污染 user_words。
        //
        // 整句学习（关键）：如果 text 是整句（多词组合，如「你说你知不知道」），
        // 不能把整句存为单个 user_word——那样永远记不完所有句子，且整句作为候选
        // 会污染查询。正确做法：用 decode_sentence 把整句拆成子词（你说/你/知道），
        // 只学习子词 + 词间 bigram 关联，整句本身不进 user_words。
        if text.chars().count() > 1 && is_valid_chinese_text(text) {
            // 规范化拼音：把无空格的连续拼音（如 "tongzhi"）切成带空格的音节序列
            // （"tong zhi"），防御调用方传入未分隔音节导致 Trie 节点错误（查询时
            // 按 "tong zhi" 两音节匹配，会找不到单音节 "tongzhi" 节点）。
            // 与 learn() 的规范化逻辑一致。
            let pinyin_normalized = pinyin
                .filter(|p| !p.trim().is_empty())
                .map(normalize_pinyin_for_storage);
            let pinyin_to_store = pinyin_normalized.or_else(|| self.dict.find_pinyin(text));

            // 整句学习策略（选择性固化 + 拆词学习双保险）：
            //
            // 设计原则：拆词学习 + bigram 关联是始终执行的（让连续输入越用越准）。
            // 整句固化（≤8字进 user_words 成为白词）只在用户明确要求时才发生：
            // - Shift+数字键选词（force_solidify=true）：用户明确想"固定"这个绿词成白词
            // - 普通数字键选词（force_solidify=false）：只拆词+bigram，绿词保持绿词身份，
            //   靠 bigram 积累自然提升，不污染白词排序
            //
            // 双轨学习：
            // 1. 拆词学习（commit_sentence_split）：子词间 bigram 加权学习(+3)，
            //    让连续输入的上下文关联越用越准。这是绿词"智能"的来源。
            // 2. 整句固化（≤8 字 + force_solidify）：整句进 user_words + dict，
            //    下次同输入作为白词精确命中排首位。
            //
            // 字数限制（膨胀控制）：
            // - ≤8 字（MAX_FRESH_WORD_LEN）：可固化（需 force_solidify）。
            // - >8 字：无论是否 force_solidify 都不固化（长句无限多，固化收益低膨胀风险高）。
            let is_sentence = self.is_sentence_input(text, pinyin_to_store.as_deref());
            if is_sentence {
                // 绿词采纳率数据采集：
                // 用户 commit 的 text 走绿词路径（is_sentence=true）→ 选中了一个绿词候选。
                // 此处只累加 total_green_picks（采集总数）。
                // second_green_picks（选中第二位绿词）暂未采集——需要记录 process 返回候选里
                // 绿词的位置，复杂度高，留待未来实现。采纳率微调通道在 second 采集
                // 补齐前自然不生效，纯用动态阈值，安全无副作用。
                // TODO：采集 second_green_picks 以激活按采纳率微调动态门槛。
                self.green_accept.total_green_picks =
                    self.green_accept.total_green_picks.saturating_add(1);

                // 拆词学习（始终执行：子词 bigram 让连续输入更顺，这是绿词智能的核心）
                self.commit_sentence_split(text, pinyin_to_store.as_deref(), advance_context);
                sentence_split_commit = true;

                // 自组词绿词记忆：
                // 用户自组的整句（如"各显神通"）存到 green_memo。
                // key 用原始 input（如"gexianshentong"），而非拼音——避免 DP 切分
                // 不一致导致 miss（拼音 key 要求"mei fen zhong"切分完全对齐）。
                // 不入 user_words/dict（不固化白词、不干扰 is_sentence_input 判定）。
                // 会话级有效——重启后从 input_mappings 恢复（count>=1，首次即记忆）。
                let char_count = text.chars().count();
                if (3..=8).contains(&char_count) && !input.is_empty() {
                    let base_freq = crate::char_freq::compute_freq(text).max(1);
                    let commit_count = self.freq.counts.get(text).copied().unwrap_or(0);
                    let user_freq = if commit_count > 0 {
                        (1000 + commit_count * 1000).min(10000)
                    } else {
                        0
                    };
                    let computed = base_freq.max(user_freq);
                    // key = 原始 input（如"meifenzhong"），查询端用 seg.concat() 或整句 input 查。
                    // 第三元 = 学习时真实读音：提升/召回构造候选的 pinyin
                    // 及预编辑显示跟随首选都靠它（否则候选携带当选切分读音，文本自相矛盾）。
                    let entries = self.green_memo.entry(input.to_string()).or_default();
                    if let Some(slot) = entries.iter_mut().find(|(t, _, _)| t == text) {
                        slot.1 = computed;
                        slot.2 = pinyin_to_store.clone();
                    } else {
                        entries.push((text.to_string(), computed, pinyin_to_store.clone()));
                    }
                    // 双拼模式下额外用全拼拼音存一份（跨模式共享）：
                    // green_memo 记录的是"拼音组合→整句"语言层面的关联，与按键方式无关。
                    // 双拼用户和全拼用户选的是同一个词，绿词记忆应跨模式共享。
                    // 查询端整句查询已有 seg_pinyin 回退查询（.or_else 查 seg.join(" ")），
                    // 此处用全拼 pinyin_to_store 存一份后，双拼/全拼查询都能命中。
                    if let Some(full_pinyin) = &pinyin_to_store {
                        // 全拼 input 本身就等于拼音前缀（去空格），避免重复存储
                        let is_same_as_input =
                            input.replace(' ', "") == full_pinyin.replace(' ', "");
                        if !is_same_as_input {
                            let pinyin_entries =
                                self.green_memo.entry(full_pinyin.clone()).or_default();
                            if let Some(slot) =
                                pinyin_entries.iter_mut().find(|(t, _, _)| t == text)
                            {
                                slot.1 = computed;
                                slot.2 = Some(full_pinyin.clone());
                            } else {
                                pinyin_entries.push((
                                    text.to_string(),
                                    computed,
                                    Some(full_pinyin.clone()),
                                ));
                            }
                        }
                    }
                    if self.green_memo.len() > Self::GREEN_MEMO_MAX_ENTRIES {
                        self.green_memo.clear();
                    }
                }

                // 整句固化（≤8 字）：
                // - force_solidify=true（Shift+数字键）：用户明确要求固化，立即固化
                // - 自动固化：用户对同一 (input, text) 选过 auto_solidify_threshold
                //   次（默认 2 次，可通过偏好设置调整）后自动固化。解决「蛇鼠一窝」「输入法项目」
                //   等固定搭配/专有名词选几十次都记不住的问题。
                const MAX_FRESH_WORD_LEN: usize = 8;
                let char_count = text.chars().count();
                // 查 input_mappings 的累计次数（record_input_mapping 刚累加过）
                let mapping_count = if !input.is_empty() {
                    self.input_mappings
                        .get(&(input.to_string(), text.to_string()))
                        .copied()
                        .unwrap_or(0)
                } else {
                    0
                };
                let should_solidify = (force_solidify
                    || (self.auto_solidify_threshold > 0
                        && mapping_count >= self.auto_solidify_threshold))
                    && char_count <= MAX_FRESH_WORD_LEN;
                if should_solidify {
                    if let Some(p) = pinyin_to_store.as_ref() {
                        // 字数==音节数守卫：错位拼音不入 user_words/Trie——
                        // "习惯惯"末字重复上屏的自我延续源头。
                        if super::user_word_pinyin_matches(text, p) {
                            self.record_user_word_pronunciation(text, p);
                            self.pending_upserts.insert(text.to_string());
                        } else if force_solidify {
                            // 守卫拒绝显式固化也大声报（可观测性原则）
                            eprintln!(
                                "[solidify] 拼音与文本字数错位，守卫拒绝固化：{} <- {:?}",
                                text, p
                            );
                        }
                    } else if force_solidify {
                        // 可观测性：用户明确要求固化（Shift+数字）却拿不到
                        // 拼音 → 静默跳过会让空拼音问题隐身（「程序基础」
                        // 无法固化绿词的根因）。大声报出来。
                        eprintln!(
                            "[solidify] force_solidify 但无拼音可用，跳过固化：{}（调用方需传候选拼音）",
                            text
                        );
                    }
                }
            } else if let Some(p) = pinyin_to_store {
                // 系统词去重：已是 dict 系统词的不写 user_words，
                // 避免 x1dict_words + user_words 两张表重复（如"奥利奥"既是系统词又是用户词）。
                // 系统词被选择只更新 freq/rank_boost（走 freq.record），不需要进 user_words。
                //
                // 多音字修正：用 find_freq 检查 (text, pinyin) 精确组合。
                // 如果 dict 里"浅表"只有 jian biao，用户输入 qian biao，find_freq 返回 None
                // → 不是"该音的系统词" → 写入 user_words 用正确拼音。
                //
                // 字数==音节数守卫：错位拼音不入
                // user_words/Trie（同上方 solidify 分支）。
                let is_system_word = self.dict.find_freq(p.as_str(), text).is_some();
                if !is_system_word && super::user_word_pinyin_matches(text, &p) {
                    self.record_user_word_pronunciation(text, &p);
                    self.pending_upserts.insert(text.to_string());
                }
            } else if self.user_words.contains_key(text) {
                // 已是用户词但本次没找到拼音（理论上不会发生），仅更新频率
                self.pending_upserts.insert(text.to_string());
                let count = self.freq.counts.get(text).copied().unwrap_or(1);
                let pys = self.user_words.get(text).cloned().unwrap_or_default();
                self.dict.remove_by_text(text);
                for p in &pys {
                    self.insert_user_word_to_dict(text, p, count);
                }
            }
        }

        self.dirty = true;

        // 推进 bigram 上下文栈（仅普通提交；自组词分步传 false）。
        // 先用栈顶（最近 1 个上屏词）记录共现 (prev, text)，再把 text 压入栈。
        // 栈保留最近 CONTEXT_HISTORY_SIZE 个上屏词：bigram 只看 (最近1词, 当前词)，
        // trigram 额外看倒数第二个词。
        if advance_context {
            // 每次普通提交先清空可撤销快照；只有真正记录 n-gram 时才重建
            // （纠错撤销：前端提交后立即退格可回滚本次跨词学习）。
            self.last_commit_ngrams = None;
            // 连续同词抑制：连续 ≥2 次提交同一词通常是
            // 测试或修改内容，不是正常表达——不记 bigram/trigram，也不推进
            // 上下文栈（n-gram 历史零痕迹；词频/词典学习不受影响）。真实语言
            // 的叠词表达（看看/对对对）是单次提交内的分词产物，走
            // record_bigrams_from_text 路径，不受本规则影响。
            let repeated = self.context.last().map(|s| s.as_str()) == Some(text);
            // 整句（绿词）提交跳过本处 bigram/trigram 记录：此时栈顶是
            // commit_sentence_split 刚压入的末子词，照常记录会产出 (末子词→整词) 伪搭配
            // （如确认「输入法项目」记 (项目→输入法项目)+1）——查询侧要求"上文恰为末
            // 子词且候选恰为整词"，真实输入几乎不会命中，属死数据。拆词层已记真实的
            // 子词间搭配；整词照常压栈，(整词→下一词) 的下文衔接不受影响。
            if !repeated && !sentence_split_commit {
                let ctx_len = self.context.len();
                let mut snap = crate::engine::LastCommitNgrams {
                    bigram: None,
                    trigram: None,
                };
                // 词级 bigram（字+字提交对——如连续打 de 选 的/地——被
                // Freq choke point 过滤不记）。
                // record_* 返回实际写入量（会话上限可能截断），用于撤销快照。
                if let Some(prev1) = self.context.last().cloned() {
                    let w = self.record_word_bigram_weighted(&prev1, text, 1);
                    if w > 0 {
                        snap.bigram = Some((prev1, text.to_string(), w));
                    }
                }
                // trigram：context>=2 时记录 (prev2, prev1, text)。
                // trigram 不做 equiv_prev 泛化（三阶稀疏性高，泛化放大噪声）。
                // prev1+text 均单字的过渡被 TrigramModel choke point 过滤。
                if ctx_len >= 2 {
                    let prev2 = self.context[ctx_len - 2].clone();
                    let prev1 = self.context[ctx_len - 1].clone();
                    let w = self.record_trigram_weighted(&prev2, &prev1, text, 1);
                    if w > 0 {
                        snap.trigram = Some((prev2, prev1, text.to_string(), w));
                    }
                }
                if snap.bigram.is_some() || snap.trigram.is_some() {
                    self.last_commit_ngrams = Some(snap);
                }
            }

            // 单字成词抑制状态机：
            // commit 单字 X 时，先重置标记（上一次抑制结束），再检查 X 与 context.last()
            // 能否成词（系统词库 collocation_freq 或 user_words）→ 成词则置标记 true，
            // 下次候选查询跳过 collocation。详见 mod.rs 字段文档。
            //
            // 时序：commit「保」(维+保=维保成词) → 标记 true → process zheng 跳过 collocation
            // → commit「正」→ 先重置 false → 检查 保+正 不成词 → 保持 false。
            let text_is_single = text.chars().count() == 1;
            self.suppress_collocation_after_word = false; // 先重置（上一次抑制结束）
            if text_is_single {
                if let Some(prev) = self.context.last() {
                    let prev_is_single = prev.chars().count() == 1;
                    if prev_is_single {
                        // 检查 prev+text 是否成词：系统词库（collocation_freq 带读音校验）或 user_words。
                        // 读音校验：commit 的 pinyin 传给 collocation_freq，
                        // 避免 维+呢 这种字形拼接但读音不匹配的误判。
                        let commit_pinyin = pinyin.unwrap_or("");
                        let forms_word = self
                            .dict
                            .collocation_freq(prev, text, commit_pinyin)
                            .is_some()
                            || self.user_words.contains_key(&*format!("{}{}", prev, text));
                        if forms_word {
                            self.suppress_collocation_after_word = true;
                        }
                    }
                }
            }

            // 连续同词不推进栈（见上方 repeated 注释）
            if !repeated {
                self.context.push(text.to_string());
                // 截断到最大长度（保留最近 CONTEXT_HISTORY_SIZE=30 个，旧的自然淘汰）
                if self.context.len() > Self::CONTEXT_HISTORY_SIZE {
                    let excess = self.context.len() - Self::CONTEXT_HISTORY_SIZE;
                    self.context.drain(0..excess);
                }
            }
        }
    }

    /// 判断是否为多词组合（应拆词学习，而非存为单个长词）。
    ///
    /// 核心判据：文本能被切成 ≥ 2 个子词。
    ///
    /// 这能精确区分：
    /// - 「我在家」→ 切成 [我, 在家]（2 段）→ 多词组合 → 拆词
    /// - 「不知道」→ 切成 [不知道]（1 段）→ 单个词 → 整体学习
    /// - 「我不知道」→ 切成 [我, 不知道] 或 [我, 不知, 到]（≥2 段）→ 拆词
    ///
    /// 不要求 recon 对齐：split 切分可能选错字（如「不知道」
    /// 被切成「不知」+「到」），recon≠原文，但对齐检查会因此误判为"非多词组合"，
    /// 导致整句被存为整体。「能切成 ≥2 段」这个事实本身就说明是多词组合，
    /// 即使切分不完美，也应走拆词学习（学习的是子词 + bigram 关联，切错字影响小）。
    ///
    /// 不用 find_pinyin 判定：历史 user_words 渗透进 Trie，
    /// 「我在家」等被存为整词，find_pinyin 返回 Some 误判为真词。
    /// （统一查 dict。当前靠音节数≥3 + 拆词
    /// 策略兜底，用户学过的整句不拆词是可接受的行为，符合"学过即真词"语义。）
    pub(crate) fn is_sentence_input(&self, text: &str, pinyin: Option<&str>) -> bool {
        let pinyin = match pinyin {
            Some(p) if !p.trim().is_empty() => p,
            _ => return false,
        };
        let syllables: Vec<&str> = pinyin.split_whitespace().collect();
        // 音节数 ≥ 3 才考虑拆词（3+ 字可能是多词组合）。
        // 2 音节及以下当真词处理（符合用户"2词组合固化可接受"的要求）。
        if syllables.len() < 3 {
            return false;
        }
        // 真词保护：如果 text 作为整体在 dict 里精确匹配（音节完全对应），
        // 它是单个词不拆（如「不知道」bu zhi dao 是词库真词）。
        // 「我在家」wo zai jia 在词库查不到（不是词库词）→ 可拆。
        //
        // 统一查 dict：绿词不进 dict（见 commit_internal 绿词只进 green_memo），
        // 无污染前提；dict 含启动时加载的用户词，精确匹配到的按真词处理。
        let exact = self.dict.query(&syllables);
        if exact.iter().any(|c| c.text.as_ref() == text) {
            return false; // 精确匹配到的真词，不拆
        }
        // 3 音节保守策略：3 音节短词（如 不知道/工程师/研究员）更可能是固定词组，
        // 而非真正的多词组合（真正的多词组合通常 ≥4 音节，如 我喜欢你/我喜欢吃饭）。
        // 即使 dict 没有该词（如早期词库不含「不知道」），也整体存储为
        // user_word，不拆词。这与 4+ 音节的拆词策略形成清晰边界。
        //
        // 历史 bug：之前 3 音节也走 split 拆词，导致「不知道」被切成「不+知道」拆词学习，
        // 测试期望它整体存为 user_word。修复：3 音节门槛整体存储。
        // 3 音节策略：3 音节不再整体存储为白词，
        // 走拆词+bigram 学习。只有 2 音节及以下整体存储（2 字词组合固化可接受）。
        // 原则：「不要自动固化白词，两个字可以固化成白词即可」。
        // 3 字以上的组合（如 不知道/工程师）靠 bigram 举一反三，不固化。
        //
        // 核心条件：切分子词数 ≥ 2（能拆就是多词组合）
        let segments = self.split_into_words(&syllables);
        segments.len() >= 2
    }

    /// 整句拆词学习：把整句切成子词，只学习子词 + 词间 bigram，不存整句。
    ///
    /// 这是正确的整句学习方式（杜绝「整句存为单个长词」的愚蠢做法）：
    /// - 「你说你知不知道」→ 拆成「你说」「你」「知道」等子词
    /// - 每个子词 learn（进 user_words，单字除外）
    /// - 子词间记 bigram（你说→你、你→知道...），让下次连续输入更顺
    /// - 整句本身绝不进 user_words（句子无限，记不完；且整句候选会污染查询）
    ///
    /// `advance_context`：与 commit 一致，推进整句首词的上下文（句首衔接上文）。
    pub(crate) fn commit_sentence_split(
        &mut self,
        text: &str,
        pinyin: Option<&str>,
        advance_context: bool,
    ) {
        let pinyin = match pinyin {
            Some(p) if !p.trim().is_empty() => p,
            _ => return,
        };
        let syllables: Vec<&str> = pinyin.split_whitespace().collect();
        // 用用户传入的 text 作为切分锚定（关键）：
        // 完全靠 decode_sentence 重新猜切分会因词频失真选错字
        // （如 shu ru fa xiang mu 切成 [输入法, 香木] 而非用户明确传入的 [输入法, 项目]）。
        // 用户 commit 时传入的 text 是「正确答案」，应直接用它对齐音节切分。
        // 这尊重用户的明确教学，避免词库词频失真污染学习。
        let segments = self.split_text_anchored(text, &syllables);
        if segments.len() < 2 {
            // 切不出多个子词，退化为整词学习（虽然不该走到这，防御）
            return;
        }

        // 逐词记录 bigram（词间搭配关系），但不再 learn_single_word 子词入 user_words。
        //
        // 拆词学习不把子词 learn_single_word 进 user_words，
        // 否则恶性循环——用户选了错误绿词（如「输入发想目」），拆词把错误子词「发想」
        // 学进 user_words 并 +50 加权，下次 DP 看到高频「发想」又切错，又学一次。
        //
        // 设计：拆词只记 bigram（子词间共现关系），不污染 user_words。
        // 子词学习交给用户单独输入时的 commit（那时用户意图明确）。
        // 整句本身通过 ≤8 字固化进 user_words（commit 路径已处理）。
        //
        // bigram 加权学习：子词间 bigram 用 record_bigram_weighted(+3)，
        // 让用户一次 commit 就能建立足够强的跨词引导信号（种子最高 8，用户 commit +3，
        // 几次即超过种子）。这对 N-best DP 纠正切分至关重要——如用户 commit「坐飞机出行」，
        // (坐,飞)(飞,机)(机,出)(出,行) 各 +3，下次同输入 DP 能靠 bigram 选对切分。
        // 上下文同音证据：整句提交时，各歧义子词按
        // 句子上屏前的上文词记录（跨输入语境学习——如「机场 → 马上出发」
        // 记住 (chu fa, 机场)→出发，下次同语境 chufa 直接受益）。capture 必须在
        // 推进上下文（下方 push 末子词）之前。
        let outer_prev = self.context.last().cloned();
        let mut prev_word: Option<String> = None;
        let mut prev2_word: Option<String> = None; // 倒数第2段子词（trigram 用）
        for (word_text, word_pinyin) in &segments {
            // 只记录词间 bigram（让下次连续输入时子词衔接更顺）
            // bigram 不污染 user_words，只是候选打分时的上下文加分
            // 字+字不记（过滤在 Freq::record_word_bigram_weighted
            // choke point 统一执行）：两个单字高频相邻应固化成词，字↔字统计只有
            // 噪声价值（连续测试输入会借本路径 +3 积累 (的→地) 噪声对）。
            // 字+词 / 词+字 照常（如 [我,去,吃饭] 只记 (去→吃饭)）。
            if let Some(prev) = &prev_word {
                self.record_word_bigram_weighted(prev, word_text, 3);
            }
            // trigram：有 prev2 + prev1 时记录
            // (prev2, prev1, word) 三元共现，与 bigram 同样 +3 加权（建立跨词三阶框架）。
            // prev1+word 均单字的过渡同样被 TrigramModel choke point 过滤。
            if let (Some(p2), Some(p1)) = (&prev2_word, &prev_word) {
                self.record_trigram_weighted(p2, p1, word_text, 3);
            }
            // 记录子词的频率（recent_score，让子词本次会话优先，但不入 user_words）
            self.freq.record(word_text);
            // 只对真同音歧义子词记录上下文证据（非歧义词不进竞争表）。
            // advance_context=false（自组词中间步）不记——那不是独立上屏意图。
            if advance_context {
                if let Some(prev) = &outer_prev {
                    let wsyls: Vec<&str> = word_pinyin.split_whitespace().collect();
                    if self.is_ambiguous_pinyin(&wsyls) {
                        self.competition.record_choice(
                            &[crate::competition::CtxKey::PinCtx {
                                pinyin: word_pinyin.clone(),
                                prev: prev.clone(),
                            }],
                            word_text,
                        );
                    }
                }
            }
            // 滚动双状态：prev1 升级为 prev2，当前 word 成为新 prev1
            prev2_word = prev_word.clone();
            prev_word = Some(word_text.clone());
        }

        // 推进上下文：整句的最后一个子词作为上文（衔接后续输入）
        if advance_context {
            if let Some(last) = segments.last().map(|(t, _)| t.clone()) {
                self.context.push(last);
                if self.context.len() > Self::CONTEXT_HISTORY_SIZE {
                    let excess = self.context.len() - Self::CONTEXT_HISTORY_SIZE;
                    self.context.drain(0..excess);
                }
            }
        }
    }

    /// 用用户传入的整句文本作为锚定，把音节序列切成 (子词文本, 子词拼音) 列表。
    ///
    /// 核心思想：用户 commit 时传入的 text 是「正确答案」（如「输入法项目」），
    /// 直接用它对齐音节切分，不依赖 decode_sentence 的词频选择（会因词库 freq 失真
    /// 把「项目」猜成「香木」）。
    ///
    /// 算法（贪心最长匹配 + 文本对齐）：
    /// - 从 text 和 syllables 同步左到右扫描
    /// - 每步尝试 k=4..1 音节段，查 dict，要求候选文本与 text 当前位置精确匹配
    /// - 找到匹配的最长 k 切为一段（让多字词优先成段，避免单字堆砌）
    /// - 若词库无匹配（用户自造词），按「1 音节对应 1 字」切单字
    ///
    /// 这样切分保证：子词文本与用户 text 完全对齐，recon 一致，bigram 学习正确。
    pub(crate) fn split_text_anchored(
        &self,
        text: &str,
        syllables: &[&str],
    ) -> Vec<(String, String)> {
        let text_chars: Vec<char> = text.chars().collect();
        let n = syllables.len();
        let mut result = Vec::new();
        let mut syl_pos = 0;
        let mut char_pos = 0;

        while syl_pos < n && char_pos < text_chars.len() {
            let max_k = 4usize.min(n - syl_pos);
            let mut found = false;
            // 从长到短尝试（贪心最长匹配，让多字词优先成段）
            for k in (1..=max_k).rev() {
                let seg = &syllables[syl_pos..syl_pos + k];
                // 纯首字母段（简拼/混合尾）：
                // 单字母段不是有效音节，若整段跳过 → 简拼上屏的整句（AI 红词
                // 尤其典型——其 commit 拼音就是单字母串）退化成「1 音节 1 字」逐字
                // 学习，子词级 freq.record 全部丢失 → 下次同输入 DP 的 cand_order
                // 拿不到证据、整句构造不出来（「得先用全拼教一遍」问题的根因）。
                // 现在：全单字母段改走首字母查询（query_tokens）——与 DP 简拼
                // 路径同一查询口径——再用 text 对应位置锚定筛选（用户 text 是正确答案，
                // 尊重教学）。char 数 == 段长约束与 query_seg 的全消费守卫同口径。
                let all_single = seg.iter().all(|s| s.len() == 1);
                let cands = if all_single {
                    let tokens: Vec<crate::pinyin::Token> = seg
                        .iter()
                        .map(|s| crate::pinyin::Token::Initial(s.chars().next().unwrap()))
                        .collect();
                    self.dict.query_tokens(&tokens)
                } else {
                    if seg.iter().any(|s| !crate::pinyin::is_valid_syllable_pub(s)) {
                        continue;
                    }
                    self.dict.query(seg)
                };
                // 中文输入法基本规律：1 音节对应 1 字，k 音节对应 k 字
                if char_pos + k > text_chars.len() {
                    continue;
                }
                let expected: String = text_chars[char_pos..char_pos + k].iter().collect();
                // 查词结果要求候选文本精确匹配 text 的对应位置（字数 == 段长）
                let matched = cands
                    .iter()
                    .find(|c| c.text.as_ref() == expected && c.text.chars().count() == k);
                if let Some(c) = matched {
                    result.push((c.text.as_ref().to_owned(), seg.join(" ")));
                    syl_pos += k;
                    char_pos += k;
                    found = true;
                    break;
                }
            }
            if !found {
                // 词库无匹配（用户自造词，或词库缺词）：按「1 音节 1 字」切单字
                let seg = &syllables[syl_pos..syl_pos + 1];
                if char_pos < text_chars.len() {
                    let ch = text_chars[char_pos].to_string();
                    result.push((ch, seg.join(" ")));
                    syl_pos += 1;
                    char_pos += 1;
                } else {
                    break;
                }
            }
        }
        result
    }

    /// 用整句解码的 DP 把音节序列切成 (词文本, 词拼音) 列表。
    ///
    /// 复用 decode_sentence 的切分结果（关键）：
    /// 独立维护 DP 会与 decode_sentence 不一致——缺失
    /// freq_score +6 floor、path_single_penalty、bigram 双轨修正三层防御，导致
    /// `shu ru fa xiang mu` 错切成 [输入, 发想, 目] 而非 [输入法, 项目]，
    /// 让用户的拆词学习（commit_sentence_split）记错子词，整句学习链路彻底失灵。
    ///
    /// 现在统一调 decode_sentence 得到最优切分，从其返回的 Candidate 的 pinyin
    /// 字段（空格分隔音节序列）反推切分点，保证 split 与绿词生成的切分完全一致。
    pub(crate) fn split_into_words(&self, syllables: &[&str]) -> Vec<(String, String)> {
        let n = syllables.len();
        if n < 2 {
            return Vec::new();
        }
        // 复用 decode_sentence 的前向 DP（带三层防御）。
        // decode_sentence 内部门槛要求 ≥3 音节 + 切出 ≥2 段，对短输入返回空 Vec。
        // 对空 Vec 情况，回退到最简切分（每个有效音节单独查词）。
        let input_dummy = syllables.join("");
        let sentences = self.decode_sentence(&input_dummy, syllables, false, false);
        if let Some(sentence) = sentences.into_iter().next() {
            // decode_sentence 已经回溯出 words 和 pinyin_parts，但 Candidate 只有
            // 拼接后的 text 和 pinyin。我们需要切分点——用 pinyin 的空格切分音节，
            // 然后用每个音节段去 dict 查出对应的子词文本。
            //
            // 注意：decode_sentence 内部 DP 的切分粒度（k=1..4）保留在 best_len 里，
            // 但 Candidate 不暴露。这里我们用「按拼音段查询」反推：
            // pinyin_parts 是 "shu ru|fa|xiang mu" 这种用空格分隔音节、无分段标记的字符串。
            // 但 decode_sentence 的 pinyin 是各段拼音用空格 join，段内音节也用空格分隔，
            // 无法从 join 后的字符串区分段边界。
            //
            // 因此最稳的做法是：重新跑一次 decode_sentence 的 DP（但暴露切分点）。
            // 但避免代码重复，这里用「贪心最长匹配 + 词频验证」从 sentence.text 反推：
            // sentence.text 是子词拼接（如「输入法项目」），用音节序列对齐切。
            return self.recover_segments_from_sentence(&sentence.text, syllables);
        }
        // 回退：decode_sentence 返回 None（音节 <3 或切不出 ≥2 段多字骨架）
        // 用最简策略——每个音节单独查，能成词的合并，否则单字。
        self.fallback_split(syllables)
    }

    /// 从 decode_sentence 拼出的整句文本 + 音节序列，反推子词切分。
    ///
    /// 算法（贪心 + 词库验证）：
    /// 从左到右扫描音节，尝试 k=4..1 长度的段，查 dict：
    /// - 若该段在词库有匹配且文本对齐 sentence.text 对应位置 → 切为一段
    /// - 否则继续尝试更短 k
    /// 这样保证切分与 decode_sentence 选词一致。
    pub(crate) fn recover_segments_from_sentence(
        &self,
        sentence: &str,
        syllables: &[&str],
    ) -> Vec<(String, String)> {
        let sentence_chars: Vec<char> = sentence.chars().collect();
        let n = syllables.len();
        let mut result = Vec::new();
        let mut syl_pos = 0; // 当前音节位置
        let mut char_pos = 0; // 当前字符位置

        while syl_pos < n {
            let max_k = 4usize.min(n - syl_pos);
            let mut found = false;
            // 从长到短尝试（贪心最长匹配，让多字词优先成段）
            for k in (1..=max_k).rev() {
                let seg = &syllables[syl_pos..syl_pos + k];
                if seg.iter().any(|s| !crate::pinyin::is_valid_syllable_pub(s)) {
                    continue;
                }
                let cands = self.dict.query(seg);
                if cands.is_empty() {
                    continue;
                }
                // 选与 sentence.text 当前位置对齐的候选
                let k_chars = k; // 每音节对应 1 字（中文输入法基本规律）
                if char_pos + k_chars > sentence_chars.len() {
                    continue;
                }
                let expected_text: String = sentence_chars[char_pos..char_pos + k_chars]
                    .iter()
                    .collect();
                // 优先精确匹配 sentence 对应位置的文本
                let matched = cands
                    .iter()
                    .find(|c| c.text.as_ref() == expected_text)
                    .or_else(|| {
                        cands.iter().max_by(|a, b| {
                            let ra = self.freq.recent_score(&a.text);
                            let rb = self.freq.recent_score(&b.text);
                            ra.cmp(&rb).then_with(|| a.base_freq.cmp(&b.base_freq))
                        })
                    });
                if let Some(c) = matched {
                    result.push((c.text.as_ref().to_owned(), seg.join(" ")));
                    syl_pos += k;
                    char_pos += k_chars;
                    found = true;
                    break;
                }
            }
            if !found {
                // 兜底：单音节查不到词，用首候选或空
                let seg = &syllables[syl_pos..syl_pos + 1];
                let cands = self.dict.query(seg);
                let text = cands
                    .first()
                    .map(|c| c.text.as_ref().to_owned())
                    .unwrap_or_else(|| {
                        sentence_chars
                            .get(char_pos)
                            .map(|c| c.to_string())
                            .unwrap_or_default()
                    });
                result.push((text, seg.join(" ")));
                syl_pos += 1;
                char_pos += 1;
            }
        }
        result
    }

    /// 回退切分：decode_sentence 不可用时（短输入），用贪心最长匹配切分。
    pub(crate) fn fallback_split(&self, syllables: &[&str]) -> Vec<(String, String)> {
        let n = syllables.len();
        let mut result = Vec::new();
        let mut pos = 0;
        while pos < n {
            let max_k = 4usize.min(n - pos);
            let mut found = false;
            for k in (1..=max_k).rev() {
                let seg = &syllables[pos..pos + k];
                if seg.iter().any(|s| !crate::pinyin::is_valid_syllable_pub(s)) {
                    continue;
                }
                let cands = self.dict.query(seg);
                if !cands.is_empty() {
                    let pick = cands
                        .iter()
                        .max_by(|a, b| {
                            let ra = self.freq.recent_score(&a.text);
                            let rb = self.freq.recent_score(&b.text);
                            ra.cmp(&rb).then_with(|| a.base_freq.cmp(&b.base_freq))
                        })
                        .unwrap();
                    result.push((pick.text.as_ref().to_owned(), seg.join(" ")));
                    pos += k;
                    found = true;
                    break;
                }
            }
            if !found {
                let seg = &syllables[pos..pos + 1];
                let cands = self.dict.query(seg);
                let text = cands
                    .first()
                    .map(|c| c.text.as_ref().to_owned())
                    .unwrap_or_default();
                result.push((text, seg.join(" ")));
                pos += 1;
            }
        }
        result
    }

    /// 从词库中删除一个词（用户不喜欢的候选）
    ///
    /// 彻底删除策略：
    /// Command+Backspace 删除时，从所有数据源清除该词：
    /// 1. 内存 dict Trie（候选查询用的主 Trie，唯一 Trie）
    /// 2. user_words 内存 + DB（用户自造词）
    /// 3. x1dict_words DB（系统词库真词，如「发想」是早期词表收录词条）
    /// 5. 频率模型（freq counts / recent_score / bigram）
    /// 6. 持久化黑名单 user_blacklist（跨部署保留删除意图，部署脚本据此清理种子 DB）
    ///
    /// 这样 DP 查 dict 根本查不到该词，不会再被选来拼绿词。
    /// 黑名单仍保留作为额外保险（万一 DB 删除未持久化，黑名单兜底）。
    ///
    /// 复活机制：用户 commit 同文本时清除黑名单。但 x1dict_words 已删的词不会自动恢复
    /// （需要重新导入词库）。这是用户明确删除的代价——删了就是真的没了。
    pub fn remove(&mut self, text: &str) {
        // 1. 内存 Trie 删除（dict 是唯一查询 Trie，删了就查不到）
        self.dict.remove_by_text(text);
        self.seg_cache_clear();
        // 2. 频率模型清除
        self.freq.forget(text);
        self.freq.forget_pinyin(text);
        self.freq.forget_bigram(text);
        // trigram 连带清理：删词后不残留 (词,*,*) / (*,词,*) / (*,*,词) 边
        self.trigram.forget_text(text);
        // 3. 会话级黑名单（绿词降级 + DP pick filter）
        self.removed_texts.insert(text.to_string());

        // 4. input_mappings 清理：
        // 用户删词后，关联的 (input, text) 输入习惯映射必须同步清理，
        // 否则被删的词会通过 mapping_bonus 重新排到前面。
        // （如 panduan→判读安 删词后，mapping 残留让判读安仍出现）
        self.input_mappings.retain(|(_, t), _| t != text);
        self.input_mappings_by_text.remove(text);

        if let Some(storage) = &mut self.storage {
            // 5. 持久化黑名单（跨部署保留，部署脚本据此清理种子 DB）。
            // 落盘失败不吞错（黑名单是删词防复活
            // 的唯一持久化通道，失败即失效）——进 pending 由 flush 重试。
            if storage.insert_blacklist(text).is_err() {
                self.pending_blacklist.push(text.to_string());
            }
            // 6. user_words DB 删除（用户自造词）
            // 7. x1dict_words DB 删除（系统词库真词，如「发想」）——失败同样
            // 进 pending_blacklist 重试（同批黑名单语义：防复活）
            if storage.delete_x1dict_word_by_text(text).is_err() {
                self.pending_blacklist.push(text.to_string());
            }
            // 8. input_mappings DB 删除（同步内存清理；失败可容忍——mapping
            // 残留只影响排序权重，不导致删词复活）
            let _ = storage.delete_input_mappings_by_text(text);
        }

        // user_words 内存删除 + 标记 pending_deletions
        if self.user_words.remove(text).is_some() {
            self.pending_upserts.remove(text);
            self.pending_deletions.insert(text.to_string());
            self.dirty = true;
        }
    }

    /// 手动造词（字+字组合成词的备用接口）
    ///
    /// 整体固化到 user_words，不做拆词判定。
    /// 当前宿主前端自组词已统一走 commit（拆词学习），learn 保留供：
    /// - 外部工具/脚本主动造词
    /// - 未来词库管理器的"强制固化某词"功能
    /// - 特定长词整体固化（如用户指定的成语/诗句）
    ///
    /// `input` 是用户实际输入的拼音串（如 "laishuo"），会在内部解析成
    /// 带空格的音节序列 "lai shuo" 后再更新内存状态。
    /// 失败不抛错，只静默忽略（避免输入卡顿）。
    pub fn learn(&mut self, text: &str, input: &str) {
        // 用户词库只保留多字词
        if text.chars().count() <= 1 {
            return;
        }
        // 防御：拒绝含 ASCII 字母的候选入库（同 commit，防脏候选持久化）
        if !is_valid_chinese_text(text) {
            return;
        }
        // 提示：4+字手动造词会被整体固化。用户如果想让系统学习词间搭配关系，
        // 建议用连续输入（commit 自动拆词学习子词+bigram），而非手动造长词。
        // 此处只记录日志，不阻断——尊重用户明确想固化这个词的意图。
        if text.chars().count() >= 4 {
            eprintln!("[learn] 用户手动造词「{}」({}字)将整体固化。如需学习词间搭配，建议用连续输入让系统自动拆词。", text, text.chars().count());
        }

        // 规范化输入：把多个连续空格压缩成一个，保留显式音节分隔。
        // 这样无论调用方传 "lai shuo" 还是 "laishuo"，引擎都得到一致的 pinyin。
        let pinyin = normalize_pinyin_for_storage(input);
        if pinyin.is_empty() || pinyin.split_whitespace().next().is_none() {
            return;
        }
        // 字数==音节数守卫：拒绝错位拼音（如
        // learn("习惯惯","xi guan")——组合学习步骤拼音缺失的产物）。错位词入
        // Trie 后挂在短音节键下，输入 "xiguan" 会把 3 字"习惯惯"当整词候选顶到
        // 最前，一次空格整体上屏 = 微信"习惯惯"末字重复上屏的根因。
        if !super::user_word_pinyin_matches(text, &pinyin) {
            eprintln!(
                "[learn] 拒绝字数≠音节数的错位拼音：「{}」({}字) pinyin={:?}——疑似组合学习步骤拼音缺失",
                text,
                text.chars().count(),
                pinyin
            );
            return;
        }

        // 更新内存用户词库和 Trie；频率 +1 表示用户主动学过/用过。
        // 多读音共存：新读音追加，全部读音重建（不冲掉旧读音）。
        self.record_user_word_pronunciation(text, &pinyin);
        self.freq.record(text);
        self.pending_deletions.remove(text);
        self.pending_upserts.insert(text.to_string());
        // 用户重新学习了该词，移除会话级黑名单（不再屏蔽整句解码拼出此文本）
        self.removed_texts.remove(text);
        self.dirty = true;

        // 自组词收尾：记整词上文。自组词分步 commit 传 advance_context=false，
        // 此处统一以整词推进上下文栈，避免把「我真牛逼」拆成单字污染 bigram。
        // 连续同词抑制（同 commit 路径）：不记 bigram、
        // 不推进栈（测试痕迹零 n-gram 记录）。
        if self.context.last().map(|s| s.as_str()) != Some(text) {
            if let Some(prev) = self.context.last() {
                self.freq.record_word_bigram(prev, text);
            }
            self.context.push(text.to_string());
            if self.context.len() > Self::CONTEXT_HISTORY_SIZE {
                let excess = self.context.len() - Self::CONTEXT_HISTORY_SIZE;
                self.context.drain(0..excess);
            }
        }
    }

    /// 记录一条带权重的词级 bigram 共现（FFI 暴露，供宿主前端分词后调用）。
    pub fn record_word_bigram_weighted(&mut self, prev: &str, text: &str, weight: u32) -> u32 {
        let applied = self.freq.record_word_bigram_weighted(prev, text, weight);
        if self.learning_stats_enabled {
            if applied > 0 {
                self.learning_stats.bigrams_recorded += 1;
                if applied < weight {
                    self.learning_stats.bigrams_capped += 1;
                }
            } else {
                self.learning_stats.bigrams_skipped += 1;
            }
        }
        applied
    }

    /// 记录一条带权重的 trigram（含学习治理计数）。返回实际写入量。
    pub fn record_trigram_weighted(
        &mut self,
        prev2: &str,
        prev1: &str,
        text: &str,
        weight: u32,
    ) -> u32 {
        let applied = self.trigram.record_weighted(prev2, prev1, text, weight);
        if self.learning_stats_enabled {
            if applied > 0 {
                self.learning_stats.trigrams_recorded += 1;
                if applied < weight {
                    self.learning_stats.trigrams_capped += 1;
                }
            } else {
                self.learning_stats.trigrams_skipped += 1;
            }
        }
        applied
    }

    /// 把上屏文本按词库做正向最大匹配（FMM）
    /// 切词，相邻词对写入带权 bigram——宿主前端（mac 侧）用系统分词器切词后
    /// 逐对调 `record_word_bigram_weighted`，
    /// 引擎服务侧在完整提交（!solidify）且多字时调用本方法。
    ///
    /// 切词查内存 `long_word_freq`（text_freq_map：词库 + 用户词合一，零新依赖、
    /// 零 SQL）；未匹配段退化为单字；非基本区汉字（标点/西文/数字）是断点，
    /// 跨断点不配对（同系统分词器词边界语义）。
    ///
    /// 粒度约束沿用挖掘规范：只记
    /// 词+词 / 词+字 / 字+词，字+字不记——未收录新词会碎成单字，跳过防噪声
    /// （新词进词库后自动被切出，自愈）。
    ///
    /// 两段式（每游程先切后写）：切词只借 `&self.dict`、写入走 `&mut self`——
    /// 同一函数内先后进行，无借用冲突；Commit 低频路径，FMM 成本 O(长度×12) 查表。
    pub fn record_bigrams_from_text(&mut self, text: &str, weight: u32) {
        /// FMM 尝试的最长词字数（词库最长词上界；超长查表 miss 无副作用，成本 O(log N)）
        const MAX_WORD_CHARS: usize = 12;

        let chars: Vec<char> = text.chars().collect();
        // 1) 按断点（非基本区汉字）切游程
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut run_start: Option<usize> = None;
        for (idx, &c) in chars.iter().enumerate() {
            let cjk = ('\u{4E00}'..='\u{9FFF}').contains(&c);
            if cjk {
                if run_start.is_none() {
                    run_start = Some(idx);
                }
            } else if let Some(s) = run_start.take() {
                runs.push((s, idx));
            }
        }
        if let Some(s) = run_start {
            runs.push((s, chars.len()));
        }

        // 2) 每游程独立 FMM + 游程内相邻对写 bigram（跨游程 = 跨标点，不配对）
        for &(start, end) in &runs {
            let mut tokens: Vec<String> = Vec::new();
            let mut i = start;
            while i < end {
                let max_len = MAX_WORD_CHARS.min(end - i);
                let mut take = 1;
                for l in (2..=max_len).rev() {
                    let piece: String = chars[i..i + l].iter().collect();
                    if self.dict.long_word_freq(&piece).is_some() {
                        take = l;
                        break;
                    }
                }
                tokens.push(chars[i..i + take].iter().collect());
                i += take;
            }
            for pair in tokens.windows(2) {
                let (a, b) = (&pair[0], &pair[1]);
                if a.chars().count() == 1 && b.chars().count() == 1 {
                    continue; // 字+字不记（粒度约束）
                }
                self.record_word_bigram_weighted(a, b, weight);
            }
        }
    }

    /// 注入学习型排序器权重（离线训练后调用，运行时生效）。
    /// 6 维权重（candidate_score 共 6 分量），
    /// 顺序与 ScoreBreakdown 对齐：base/total_bonus/recent/mapping_bonus/bigram_bonus/long_word_bonus。
    /// 全 0 视为未训练，回退手调公式。
    pub fn set_ranker_weights(&mut self, weights: [f64; 6]) {
        self.ranker_weights = Some(weights);
    }

    // 注：set_semantic_bonuses 方法不存在——无 semantic_bonuses 注入通道。

    /// 仅重算 dyn_cap_cache（不衰减），用于启动初始化。
    ///
    /// 与 recompute_modulators 分开：recompute_modulators 既算 dyn_cap 又做
    /// 周期衰减，new_with_db 启动时调用会让刚从 DB 加载的低频词（如 count=2）立即被
    /// 衰减，违背"持久化的累计次数应保留"语义（test_commit_system_word_stores_pinyin
    /// 守护）。启动只需用加载的 bigram 数据初始化 dyn_cap，衰减是运行时 60s 周期行为。
    pub(crate) fn init_dyn_cap(&mut self) {
        let max_count = self.freq.max_bigram_count();
        self.dyn_cap_cache =
            Self::BIGRAM_CAP_BASE + (max_count + 1).ilog2().saturating_mul(Self::BIGRAM_CAP_K);
    }

    /// 自适应排序：重算动态参数（后台 60s 周期调用，复用 flushTimer）。
    /// 扫描 bigram 表算 max_count → 更新 dyn_cap_cache（对数增长 cap）。
    /// O(n) 扫描，仅在后台 .background qos 调用，不阻塞热路径。
    pub fn recompute_modulators(&mut self) {
        let max_count = self.freq.max_bigram_count();
        // dyn_cap = CAP_BASE + log2(max+1) × CAP_K（max=0 时 log2(1)=0，cap=CAP_BASE）
        self.dyn_cap_cache =
            Self::BIGRAM_CAP_BASE + (max_count + 1).ilog2().saturating_mul(Self::BIGRAM_CAP_K);

        // 时间衰减已废除（时间不进权重公式）：decay_counts /
        // decay_input_mappings 调用整体移除——真实时间不该进权重公式。
        // 退化（「选得少则衰、选得多则提」）
        // 由 competition.rs 的同音零和归一承担：commit 时 total(ctx)+1、
        // selects(ctx,chosen)+1，未选中者分子不动被分母稀释（无时间参数，
        // 不用电脑期间权重逐位冻结——competition.rs 头注）。时间衰减还会把
        // engine 自弄脏（decay 无条件 dirty=true）绕过 FlushScheduler 的
        // is_dirty 短路，致输入法失活抖动下每次失活全量落盘（同秒连发
        // "user freq flushed"）。
    }

    /// 同音层先验（懒缓存）：`prior(w) = base_freq(w) / Σ_w' base_freq(w')`，
    /// 分母 = 该拼音（单字 = 首音节、多字 = 词完整拼音，与 CtxKey::Pin 同口径）
    /// 下 dict 精确命中的全部候选的 base_freq 和。只在读取平滑频率时按需计算
    ///（dict.query 一次，热路径复用——不得在 candidate_score 内重复现算）。
    /// priors_full：竞争 boost 的「先验份额项」需要每个候选的精确先验
    ///（含零——无份额者动态项可从零起涨，见 scoring.rs rank_boost 注释）。
    ///
    /// `pin_key` 首字符为 `\u{1}` 时是简拼整句层 key（scoring.rs 的
    /// jianpin 分支约定）：先验取该输入 token 映射的全集（简拼按首字母
    /// 展开的近似映射——单字母/双字声母输入取 query_single_char_by_initial
    /// 的单字全集归一；长串简拼按 query_jianpin 的近似集合归一）。
    pub(crate) fn pin_priors_full(&self, pin_key: &str) -> Vec<(String, f32)> {
        let mut cache = self.competition_priors.borrow_mut();
        let entries = cache.entry(pin_key.to_string()).or_insert_with(|| {
            if let Some(jp) = pin_key.strip_prefix('\u{1}') {
                // 简拼整句层先验：输入 token 映射全集按词长分层
                // 归一（单字桶/多字桶各自一份——单字候选与词级候选的竞争是两类
                // 不同的实体，共一份先验会被巨大多字桶稀释掉单字的份额；
                // 单字桶=「是」这类简拼单字竞争的主战场，它必须占有足量先验）。
                // 数据近似：单字母/双字声母输入用 query_single_char_by_initial
                //（首字母单字全集）；长串简拼用 query_jianpin（按位下钻的词级候选）。
                let (single_cands, word_cands) = if jp.len() <= 2 {
                    let singles: Vec<crate::dict::Candidate> = jp
                        .chars()
                        .next()
                        .map(|ch| self.dict.query_single_char_by_initial(ch))
                        .unwrap_or_default();
                    (singles, Vec::new())
                } else {
                    let all = self.dict.query_jianpin(jp);
                    let (singles, words): (Vec<_>, Vec<_>) =
                        all.into_iter().partition(|c| c.text.chars().count() == 1);
                    (singles, words)
                };
                let norm = |v: Vec<crate::dict::Candidate>| -> Vec<(String, f32)> {
                    let total: u64 = v.iter().map(|c| c.base_freq as u64).sum::<u64>().max(1);
                    v.into_iter()
                        .map(|c| (c.text.to_string(), c.base_freq as f32 / total as f32))
                        .collect()
                };
                // 单字桶与多字桶等权合成（各 0.5——词长是硬分层，不该由数据量定比）。
                let mut out = norm(single_cands)
                    .into_iter()
                    .map(|(w, p)| (w, p * 0.5))
                    .collect::<Vec<_>>();
                out.extend(norm(word_cands).into_iter().map(|(w, p)| (w, p * 0.5)));
                return out;
            }
            let syls: Vec<&str> = pin_key.split_whitespace().collect();
            let cands = self.dict.query(&syls);
            let total: u64 = cands.iter().map(|c| c.base_freq as u64).sum::<u64>().max(1);
            cands
                .into_iter()
                .map(|c| (c.text.to_string(), c.base_freq as f32 / total as f32))
                .collect()
        });
        entries.clone()
    }

    /// 单候选先验（便捷封装；零份额返回 0.0，不塞回 1.0——零即「无先验份额」）。
    #[allow(dead_code)]
    pub(crate) fn pin_prior(&self, pinyin_key: &str, word: &str) -> f32 {
        self.pin_priors_full(pinyin_key)
            .into_iter()
            .find(|(w, _)| w == word)
            .map(|(_, p)| p)
            .unwrap_or(0.0)
    }

    /// 平滑竞争频率（同音层）：
    /// `(selects + α_pin·prior) / (total + α_pin)` ∈ [0,1]。
    /// 零证据（total=0）时恒等 prior（回落种子基准）。
    pub(crate) fn pin_comp_freq(&self, pinyin_key: &str, word: &str) -> f32 {
        let prior = self.pin_prior(pinyin_key, word);
        self.competition.smoothed(
            &crate::competition::CtxKey::Pin(pinyin_key.to_string()),
            word,
            crate::competition::ALPHA_PIN,
            prior,
        )
    }
}
