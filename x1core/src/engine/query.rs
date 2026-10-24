//! 候选查询逻辑：模糊音扩展查询、混合简拼查询、前缀词收集等。
//!
//! 这些方法都是 `&self`（共享借用），不改 Engine 状态。

use alloc::string::String;
use alloc::vec::Vec;

use crate::dict::Candidate;
use crate::pinyin::Token;

/// 紫词（猜测候选）总数上限：零精确命中才联想，且末音节
/// 延伸 + 长词延展合并 ≤3 条（base_freq 降序取前）——防同前缀猜测词刷屏
/// （如 buyao 被十几条「不要X」紫词淹没）。
const MAX_GUESS_CANDIDATES: usize = 3;

impl crate::engine::Engine {
    /// 以下一组查询包装在 dict 原函数基础上增加模糊音扩展。
    /// 保持 dict.rs 不感知模糊音，便于单独测试和维护。
    pub(crate) fn query_single_chars_fuzzy(&self, prefix: &str) -> Vec<Candidate> {
        if self.fuzzy_config.is_empty() {
            // 联想开关：关闭时滤掉紫标前缀延伸候选。简拼态
            //（输入不完整音节，如 j）源头上未标 is_guess → 天然全保留。
            let mut r = self.dict.query_single_chars(prefix);
            if !self.guess_enabled {
                r.retain(|c| !c.is_guess);
            }
            return r;
        }
        let mut result = Vec::new();
        // expand_prefix（不过滤有效音节）：prefix 是裸声母/半截音节，用
        // expand_syllable 会被有效性过滤清空（导致单字消失）。
        for s in self.fuzzy_config.expand_prefix(prefix) {
            result.extend(self.dict.query_single_chars(&s));
        }
        // 联想开关关闭时同样滤掉紫标候选（模糊音扩展路径）
        if !self.guess_enabled {
            result.retain(|c| !c.is_guess);
        }
        result.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        result.dedup_by(|a, b| a.text == b.text);
        // 不截断，确保模糊音合并后的所有精确匹配单字都能被翻页找到
        result
    }

    pub(crate) fn query_fuzzy(&self, syllables: &[&str]) -> Vec<Candidate> {
        if self.fuzzy_config.is_empty() {
            return self.dict.query(syllables);
        }
        let expanded = self.expand_syllables(syllables);
        let mut result = Vec::new();
        for seq in expanded {
            let seq_refs: Vec<&str> = seq.iter().map(|s| s.as_str()).collect();
            result.extend(self.dict.query(&seq_refs));
        }
        result.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        result.dedup_by(|a, b| a.text == b.text);
        result
    }

    pub(crate) fn query_prefix_filtered_fuzzy(
        &self,
        complete: &[&str],
        prefix: &str,
    ) -> Vec<Candidate> {
        if self.fuzzy_config.is_empty() {
            return self.dict.query_prefix_filtered(complete, prefix);
        }
        let complete_expanded = self.expand_syllables(complete);
        // expand_prefix：prefix 是尾部半截音节（如 "r"/"zho"），
        // expand_syllable 的有效性过滤会清空它（同类原因）。
        let prefix_expanded = self.fuzzy_config.expand_prefix(prefix);
        let mut result = Vec::new();
        for comp in complete_expanded {
            let comp_refs: Vec<&str> = comp.iter().map(|s| s.as_str()).collect();
            for pref in &prefix_expanded {
                result.extend(self.dict.query_prefix_filtered(&comp_refs, pref));
            }
        }
        result.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        result.dedup_by(|a, b| a.text == b.text);
        result.truncate(50);
        result
    }

    /// 长词延展查询（模糊音展开）：query_continuation 的模糊音
    /// 版本，口径与 query_fuzzy / query_prefix_filtered_fuzzy 一致。
    pub(crate) fn query_continuation_fuzzy(&self, syllables: &[&str]) -> Vec<Candidate> {
        if self.fuzzy_config.is_empty() {
            return self.dict.query_continuation(syllables);
        }
        let expanded = self.expand_syllables(syllables);
        let mut result = Vec::new();
        for seq in expanded {
            let seq_refs: Vec<&str> = seq.iter().map(|s| s.as_str()).collect();
            result.extend(self.dict.query_continuation(&seq_refs));
        }
        result.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        result.dedup_by(|a, b| a.text == b.text);
        result
    }

    pub(crate) fn query_single_char_by_initial_fuzzy(&self, initial: char) -> Vec<Candidate> {
        if self.fuzzy_config.is_empty() {
            return self.dict.query_single_char_by_initial(initial);
        }
        let mut result = Vec::new();
        // 把首字母扩展为等价首字母集合
        let initial_str = initial.to_string();
        let mut initials: Vec<String> = vec![initial_str.clone()];
        if self.fuzzy_config.enabled {
            if self.fuzzy_config.z_zh && initial == 'z' {
                initials.push("zh".to_string());
            }
            if self.fuzzy_config.z_zh && initial == 'h' {
                initials.push("z".to_string());
            }
            if self.fuzzy_config.c_ch && initial == 'c' {
                initials.push("ch".to_string());
            }
            if self.fuzzy_config.c_ch && initial == 'h' {
                initials.push("c".to_string());
            }
            if self.fuzzy_config.s_sh && initial == 's' {
                initials.push("sh".to_string());
            }
            if self.fuzzy_config.s_sh && initial == 'h' {
                initials.push("s".to_string());
            }
            if self.fuzzy_config.n_l && initial == 'n' {
                initials.push("l".to_string());
            }
            if self.fuzzy_config.n_l && initial == 'l' {
                initials.push("n".to_string());
            }
            if self.fuzzy_config.f_h && initial == 'f' {
                initials.push("h".to_string());
            }
            if self.fuzzy_config.f_h && initial == 'h' {
                initials.push("f".to_string());
            }
        }
        // 去重
        let mut seen = alloc::collections::BTreeSet::new();
        for ini in initials {
            if seen.insert(ini.clone()) {
                // 取首字符作为 query_single_char_by_initial 的参数
                if let Some(ch) = ini.chars().next() {
                    result.extend(self.dict.query_single_char_by_initial(ch));
                }
            }
        }
        result.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        result.dedup_by(|a, b| a.text == b.text);
        result.truncate(15);
        result
    }

    pub(crate) fn query_jianpin_fuzzy(&self, input: &str) -> Vec<Candidate> {
        let initials_list = self.fuzzy_config.expand_jianpin_initials(input);
        let mut result = Vec::new();
        for initials in initials_list {
            result.extend(self.dict.query_jianpin(&initials));
        }
        result.dedup_by(|a, b| a.text == b.text);
        // 截断前用 recent_score 重排，让用户学过的词（recent 高）进入前 50，
        // 而非纯按 base_freq 截断（高频首字母如 s 开头上千词条，用户词可能被挤出）。
        self.rerank_by_recent_then_truncate(&mut result, 100);
        result
    }

    /// 拍扁回退候选的同位音节前缀约束（口径：尾部全拼必须精确匹配，
    /// 见 test_hdong_trailing_full_syllable_precise）。
    ///
    /// 背景：混合简拼回退把整段拍成首字母查简拼，已完整输入的全拼段也被
    /// 降级——hdong 的 dong 拍成 d，捞出「回答(hui da)/海底(hai di)/
    /// 很多(hen duo)」这类第二音节只共首字母的噪音并霸屏。
    ///
    /// 口径：候选第 i 音节必须以输入第 i 完整音节开头（输入是候选音节的
    /// 前缀，含模糊音展开）：
    ///   · hdong 位置 1 = "dong"：只有 dong 过（da/duo/di/de 全滤）→ 尾部精确；
    ///   · bydi 位置 2 = "di"：di/ding/dian… 过（「不一定」的 ding 是续打半截）
    ///     → 尾部宽松仅限「前缀续打」；
    ///   · 简拼/不完整位（单字母、无效音节）不约束——拍扁的本义就是首字母猜测。
    ///
    /// 与「huad 尾简拼宽、hdong 尾全拼严」的用户直觉一致：打完的字要精确，
    /// 没打完的字母才宽松。候选音节数恒 ≥ 输入段数（query_jianpin 按位下钻，
    /// 词挂在深度 ≥ len 的节点），对位访问安全。
    pub(crate) fn flatten_tail_compatible(&self, syllables: &[&str], cand: &Candidate) -> bool {
        let cand_syls: Vec<&str> = cand.pinyin.split(' ').collect();
        for (i, s) in syllables.iter().enumerate() {
            // 只约束完整音节位（长度 ≥2 的有效音节）；简拼/不完整位放行
            if s.len() < 2 || !crate::pinyin::is_valid_syllable_pub(s) {
                continue;
            }
            let Some(cs) = cand_syls.get(i) else {
                return false; // 音节数不足（防御），宁滤勿放
            };
            let hit = self
                .fuzzy_config
                .expand_syllable(s)
                .iter()
                .any(|exp| cs.starts_with(exp.as_str()));
            if !hit {
                return false;
            }
        }
        true
    }

    pub(crate) fn query_mixed_fuzzy(&self, complete: &[&str], initials: &str) -> Vec<Candidate> {
        if self.fuzzy_config.is_empty() {
            return self.dict.query_mixed(complete, initials);
        }
        let complete_expanded = self.expand_syllables(complete);
        let initials_expanded = self.fuzzy_config.expand_jianpin_initials(initials);
        let mut result = Vec::new();
        for comp in complete_expanded {
            let comp_refs: Vec<&str> = comp.iter().map(|s| s.as_str()).collect();
            for init in &initials_expanded {
                result.extend(self.dict.query_mixed(&comp_refs, init));
            }
        }
        result.dedup_by(|a, b| a.text == b.text);
        // 截断前用 recent_score 重排（同 query_jianpin_fuzzy）
        self.rerank_by_recent_then_truncate(&mut result, 100);
        result
    }

    /// 按用户使用频率重排后截断。
    ///
    /// 问题：dict 层按 base_freq 排序，高频首字母（如 j+s 开头上百词条）中，
    /// 用户选过的词（如"竞赛"选过 17 次）如果 base_freq 低，会被挤出候选。
    ///
    /// 三级排序——
    /// 1. freq.counts（累计使用次数）：用户选过的词最优先保留
    /// 2. recent_score（近期使用分）：刚选过的词次优
    /// 3. base_freq：词库原始频率兜底
    pub(crate) fn rerank_by_recent_then_truncate(&self, result: &mut Vec<Candidate>, limit: usize) {
        result.sort_by(|a, b| {
            // 1. 累计使用次数（freq.counts）——用户选过的高优先
            let ca = self.freq.counts.get(a.text.as_ref()).copied().unwrap_or(0);
            let cb = self.freq.counts.get(b.text.as_ref()).copied().unwrap_or(0);
            if ca != cb {
                return cb.cmp(&ca);
            }
            // 2. 近期使用分（recent_score）——刚选过的次优
            let ra = self.freq.recent_score(&a.text);
            let rb = self.freq.recent_score(&b.text);
            if ra != rb {
                return rb.cmp(&ra);
            }
            // 3. 词库原始频率兜底
            b.base_freq.cmp(&a.base_freq)
        });
        result.truncate(limit);
    }

    /// 对一组音节做笛卡尔积扩展
    pub(crate) fn expand_syllables(&self, syllables: &[&str]) -> Vec<Vec<String>> {
        if self.fuzzy_config.is_empty() {
            return vec![syllables.iter().map(|s| s.to_string()).collect()];
        }
        let mut results: Vec<Vec<String>> = vec![Vec::new()];
        for s in syllables {
            let opts = self.fuzzy_config.expand_syllable(s);
            let mut next = Vec::new();
            for prefix in &results {
                for opt in &opts {
                    let mut cloned = prefix.clone();
                    cloned.push(opt.clone());
                    next.push(cloned);
                }
            }
            // 超限回退：模糊选项笛卡尔积逐轮翻倍，检查必须
            // 在累积过程中触发——等全部生成完再查，1400 万序列早已把主线程冻住。
            // 回退为原始音节序列（与模糊关闭路径同构），查询降级但不冻结。
            if next.len() > crate::fuzzy::FuzzyConfig::MAX_SYLLABLE_EXPANSIONS {
                return vec![syllables.iter().map(|s| s.to_string()).collect()];
            }
            results = next;
        }
        results
    }

    /// 对一组音节执行查询：精确/混合/前缀词 → 前缀回退 → 首音节单字补充
    ///
    /// 与 `word_candidates_for_syllables` 的区别：本函数会在词级候选不足时，
    /// 只补充第一个音节的单字候选，供用户自造词。
    /// 后续音节的字必须等用户选完前面的字、buffer 消费后再出现，避免
    /// 用户误选“第二个音节”的字导致自组词顺序错乱。
    pub(crate) fn candidates_for_syllables(
        &self,
        input: &str,
        syllables: &[&str],
    ) -> Vec<Candidate> {
        if syllables.is_empty() {
            return Vec::new();
        }

        let mut result = self.word_candidates_for_syllables(input, syllables);

        // 补充第一个音节的单字候选，供用户自造词/逐字选择。
        // 当末尾音节不完整（混合简拼，如 kaix / went）时，始终补充首音节单字，
        // 因为用户很可能想先选第一个音节的字再继续输入；
        // 完整拼音则保持原阈值策略，避免单字过度干扰词级候选。
        // 无论词组候选多少，都始终补充第一个音节的单字候选，让用户能逐字自造词。
        // 之前当词组候选 >= 9 时不补充单字，导致多音节输入（如 lilizi）时
        // 前缀匹配产生大量词组候选后，用户无法翻页找到需要的单字（如"锂"）。
        //
        // 去重：word_candidates_for_syllables 对单音节输入也产出
        // 单字（如 zhi → 只/之/知），query_single_chars_fuzzy 又产出同一批单字，两列表
        // 各自去重但合并后跨列表重复。在源头合并时去重，所有调用方
        // （全拼/双拼/橙词补充）统一受益，不依赖调用方各自 dedup。
        let existing: std::collections::HashSet<String> =
            result.iter().map(|c| c.text.as_ref().to_owned()).collect();
        for c in self.query_single_chars_fuzzy(syllables[0]) {
            if existing.contains(c.text.as_ref()) {
                continue;
            }
            result.push(c);
        }
        result
    }

    /// 查询词级候选：精确匹配、混合简拼、前缀回退，以及所有连续前缀词。
    ///
    /// 连续前缀词让用户可以先选短词再继续输入后面的音节，例如输入
    /// "houxuankuang" 时，可以把 "候选" 作为候选词给出。
    pub(crate) fn word_candidates_for_syllables(
        &self,
        input: &str,
        syllables: &[&str],
    ) -> Vec<Candidate> {
        if syllables.is_empty() {
            return Vec::new();
        }

        let last = syllables.last().unwrap();
        let is_last_valid = crate::pinyin::is_valid_syllable_pub(last);
        let mut result = Vec::new();

        // 混合简拼判据：
        //
        // 设计原则：区分「用户意图的首字母简拼」与「全拼音节的错误拆分」。
        //
        // 两条反拆分规则（任一触发则不算混合）：
        // 1. 连续 Initial 段（长度≥2）拼起来是有效音节 → 拆分
        //    例：[ting,s,h,u,o] 的 s+h+u+o 拼 shuo（有效）→ 是 shuo 的拆分
        // 2. 单字符 Initial 后紧跟 Full 音节，二者拼起来是有效音节 → 拆分
        //    例：[wan,m,ei,...] 的 m+ei 拼 mei（有效）→ 是 mei 的拆分
        //    （m 单独成段，但和后面 ei 能拼成 mei，说明是 mei 被拆开）
        //
        // 覆盖的所有 case：
        // - [zhong,g,r,m]   g+r+m 拼 grm（非有效）→ 简拼 ✓（zhonggrm）
        // - [zhong,g,ren,m] g 后 ren 拼 gren（非有效）；m 后无 → 简拼 ✓（zhonggrenm）
        // - [wan,m,ei,...]  m+ei 拼 mei（有效）→ 拆分 ✗（wanmeipipei 保护）
        // - [ting,s,h,u,o]  s+h+u+o 拼 shuo（有效）→ 拆分 ✗（tingshuo 保护）
        // - [ti,n,g,shuo]   n+g 拼 ng（非有效）；但 n 后 g 是单字符，g+shuo 拼 gshuo（非有效）
        //                   → 全是单字符，靠长度限制拦截（见 MAX_INITIAL_SEG_LEN）
        const MAX_INITIAL_SEG_LEN: usize = 4;
        let is_hybrid = {
            let has_full = syllables
                .iter()
                .any(|s| s.len() >= 2 && crate::pinyin::is_valid_syllable_pub(s));
            let has_initial = syllables.iter().any(|s| s.len() == 1);
            if !has_full || !has_initial {
                false
            } else {
                let mut ok = true;
                let mut current_seg: Vec<&str> = Vec::new();
                for s in syllables.iter() {
                    if s.len() == 1 {
                        current_seg.push(s);
                    } else {
                        // 遇到 Full，检查前面积累的 Initial 段
                        if !current_seg.is_empty() {
                            // 规则1：多字符段拼起来是有效音节 → 拆分
                            if current_seg.len() > MAX_INITIAL_SEG_LEN {
                                ok = false;
                                break;
                            }
                            if current_seg.len() >= 2 {
                                let joined: String = current_seg.iter().copied().collect();
                                if crate::pinyin::is_valid_syllable_pub(&joined) {
                                    ok = false;
                                    break;
                                }
                            }
                            // 规则2：单字符段后紧跟 Full，二者拼起来是有效音节 → 拆分
                            // （current_seg 末字符 + 当前 Full 音节）
                            if current_seg.len() == 1 {
                                let merged = format!("{}{}", current_seg[0], s);
                                if crate::pinyin::is_valid_syllable_pub(&merged) {
                                    ok = false;
                                    break;
                                }
                            }
                            current_seg.clear();
                        }
                    }
                }
                // 检查末尾 Initial 段（末尾单字符段后无 Full，规则2 不适用）
                if ok && !current_seg.is_empty() {
                    if current_seg.len() > MAX_INITIAL_SEG_LEN {
                        ok = false;
                    } else if current_seg.len() >= 2 {
                        let joined: String = current_seg.iter().copied().collect();
                        if crate::pinyin::is_valid_syllable_pub(&joined) {
                            ok = false;
                        }
                    }
                    // 末尾单字符段（如 [zhong,g,ren,m] 的 m）无法判断，放行（min 的 m）
                }
                ok
            }
        };

        if is_hybrid {
            // 混合简拼：走统一 query_tokens（Full/Initial/Shengmu 三类下钻 + 前缀词收集）。
            // 不按「末音节是否有效」的二元判据分流（那会把 zhonggrm 尾 m 当有效
            // 音节误走精确分支）。
            let tokens: Vec<Token> = syllables
                .iter()
                .map(|s| {
                    if s.len() == 1 {
                        Token::Initial(s.chars().next().unwrap())
                    } else if *s == "zh" || *s == "sh" || *s == "ch" {
                        Token::Shengmu(s)
                    } else {
                        Token::Full(s)
                    }
                })
                .collect();
            result.extend(self.dict.query_tokens(&tokens));
            // 延长猜测：
            //
            // 混合简拼/半截尾字母的中途态（luanqib = luan qi b）此前完全没有
            // 猜测通道——query_tokens 只收等长词与前缀词，收不到「更长词」，
            // 于是上一步看到的紫词「乱七八糟」（luanqi 时由末音节完整分支
            // 给出）在这一步整体消失，候选回退到单字「乱」；再打一个字母
            // （luanqiba）紫词又回来。逐键「出现→消失→出现」即
            // 「明明很接近却回退闪烁」。
            //
            // 规则（与末音节完整分支同一哲学）：确定缺席才猜测——本步没有
            // 等长词命中时，收集「以本输入为前缀的更长词」染紫，≤3 条按
            // base_freq 降序。等长命中存在时不出（精确优先，不抢确定结果）。
            if self.guess_enabled && tokens.len() >= 2 {
                let token_count = tokens.len();
                let has_full_exact = result
                    .iter()
                    .any(|c| c.pinyin.split_whitespace().count() == token_count);
                if !has_full_exact {
                    let mut guesses = self.dict.query_tokens_extensions(&tokens);
                    for c in &mut guesses {
                        c.is_guess = true;
                    }
                    guesses.truncate(MAX_GUESS_CANDIDATES);
                    result.extend(guesses);
                    result.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
                    result.dedup_by(|a, b| a.text == b.text);
                }
            }
        } else if is_last_valid {
            // 完整拼音（全 Full）：先精确匹配，避免过度联想
            result.extend(self.query_fuzzy(syllables));

            // 智能猜测统一门槛（口径：精确命中即完全不出）：
            // 确定缺席才猜测——输入已精确命中等长词（buyao→不要、yonghu→用户）时，
            // 一切对后续输入的猜测一律不出。否则 buyao/yonghu 会被十几条
            // 「不要X」「用户X」紫词刷屏（长词延展无门槛无上限，且紫词整层
            // 压在全部单字之上）——猜测不给确定让位，信噪比倒挂（紫词喧宾夺主）。
            // 零精确命中（hupeng）才联想：末音节延伸（yonghu→用坏 类）与长词延展
            // （hupeng→狐朋狗友 类）合并生成、全部染紫（对后续输入的猜测）、
            // 总数 ≤3（base_freq 降序取前）。1-2 条等长精确词已构成确定，
            // 猜测让位（与英文候选「拼音可解释不抢首位」同一哲学）。
            // 排序安全性不变：is_pinyin_full_match 判延展词音节数 > 输入
            // → 前缀层，恒在等长精确词之下、单字之上。
            if self.guess_enabled && result.is_empty() && syllables.len() >= 2 {
                let complete = &syllables[..syllables.len() - 1];
                let mut guesses = self.query_prefix_filtered_fuzzy(complete, last);
                guesses.extend(self.query_continuation_fuzzy(syllables));
                for c in &mut guesses {
                    c.is_guess = true;
                }
                guesses.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
                guesses.dedup_by(|a, b| a.text == b.text);
                guesses.truncate(MAX_GUESS_CANDIDATES);
                result.extend(guesses);
                result.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
                result.dedup_by(|a, b| a.text == b.text);
            }
        } else {
            // 末尾音节不完整（非单字符）：混合匹配 + 前缀回退
            // 注：单字符末尾的简拼场景已由 is_hybrid 分支处理
            let valid_end = syllables
                .iter()
                .position(|s| !crate::pinyin::is_valid_syllable_pub(s))
                .unwrap_or(syllables.len());
            let complete = &syllables[..valid_end];
            let initials: String = syllables[valid_end..].iter().copied().collect();

            if !initials.is_empty() {
                result.extend(self.query_mixed_fuzzy(complete, &initials));
            }

            if syllables.len() >= 2 {
                let prefix_last = syllables.last().unwrap();
                let is_single_char_initial = prefix_last.len() == 1;
                if !is_single_char_initial {
                    let prefix_complete = &syllables[..syllables.len() - 1];
                    let prefix_matches =
                        self.query_prefix_filtered_fuzzy(prefix_complete, prefix_last);
                    result.extend(prefix_matches);
                    result.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
                    result.dedup_by(|a, b| a.text == b.text);
                }
            }
        }

        // 收集连续前缀词：输入 hou-xuan-kuang 时，补充 hou-xuan 能组成的词。
        // 只考虑长度 >= 2 个音节的 prefix，避免把单音节词（如 biao 中的 bi）
        // 提到完整单字前面，干扰正常输入。
        for k in 2..syllables.len() {
            let prefix = &syllables[..k];
            if prefix
                .last()
                .is_some_and(|s| crate::pinyin::is_valid_syllable_pub(s))
            {
                // 安全检查：双拼路径下 syllables 是 decode_buffer 产出的
                // owned String，与 input 无关（非子切片），下面的指针算术会越界 panic。
                // 检查最后一个音节的指针是否在 input 范围内，不在则跳过 duan 保护
                // （双拼切分由 decode_buffer 唯一确定，无切分歧义需保护），直接补查前缀词。
                let last_syl = prefix.last().unwrap();
                let last_ptr = last_syl.as_ptr() as usize;
                let input_start = input.as_ptr() as usize;
                let input_end = input_start + input.len();
                let in_range = last_ptr >= input_start && last_ptr + last_syl.len() <= input_end;

                if in_range {
                    // 全拼路径：音节是 input 子切片，duan 保护指针算术有效
                    // 无显式分隔符时，禁止把一个完整音节拆开成词。
                    // 例如 "duanlian" 不应按 du+an 给出"都安"，
                    // 用户必须显式输入 du'an 才能得到"都安"。
                    let end = last_ptr - input_start + last_syl.len();
                    let raw_prefix = &input[..end];
                    if !raw_prefix.contains('\'') {
                        let joined = prefix.concat();
                        if crate::pinyin::is_valid_syllable_pub(&joined) {
                            continue;
                        }
                    }
                }
                // 双拼路径（in_range=false）：跳过 duan 保护，直接补查前缀词
                result.extend(self.query_fuzzy(prefix));
            }
        }

        result.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        result.dedup_by(|a, b| a.text == b.text);
        result
    }

    // 原标记函数（按末音节异同染紫）已随「精确命中即完全不出」统一门槛
    // 废止——零精确命中时整批猜测都是对后续输入的预测，全部染紫，
    // 无需再区分末音节异同。
}

/// 运行时统计快照（宿主偏好设置「运行信息」展示用）。
///
/// 只读、纯计数（无字符串/无分配），供管理界面展示"引擎这边有多少东西"。
/// 不含进程内存：那是宿主进程的数据，由服务侧取（GetProcessMemoryInfo）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngineRuntimeStats {
    /// 统一词索引条数 = 系统词 + 用户词（Trie 里可查的全部词条）
    pub dict_entries: usize,
    /// 用户词（自造词 + 学习固化）
    pub user_words: usize,
    /// 输入习惯映射条数（简拼/混拼选择记录，green_memo 恢复源）
    pub input_mappings: usize,
    /// 词选择次数记录条数
    pub learned_counts: usize,
    /// 用户词级共现条数（上下文学习）
    pub bigrams: usize,
    /// 种子搭配条数（只读语料预填）
    pub seed_bigram_rows: usize,
    /// 固定候选条数
    pub pins: usize,
}

impl crate::engine::Engine {
    /// 采集运行时统计快照（见 [`EngineRuntimeStats`]）。
    pub fn runtime_stats(&self) -> EngineRuntimeStats {
        EngineRuntimeStats {
            dict_entries: self.dict.entry_count(),
            user_words: self.user_words.len(),
            input_mappings: self.input_mappings.len(),
            learned_counts: self.freq.counts.len(),
            bigrams: self.freq.user_bigram.len_stats().1,
            seed_bigram_rows: self.freq.seed_bigram.rows(),
            pins: self.pin_count(),
        }
    }
}
