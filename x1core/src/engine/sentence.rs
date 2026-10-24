//! 整句解码：N-best DP 把长拼音串切成最优词组序列，拼成整句候选。

use alloc::string::String;
use alloc::vec::Vec;

use crate::dict::Candidate;

use super::SentencePath;

#[allow(unused_imports)]
use super::is_valid_chinese_text;

impl crate::engine::Engine {
    /// 整句解码（兼容入口）：只取绿词候选，不收集路径。
    pub(crate) fn decode_sentence(
        &self,
        input: &str,
        syllables: &[&str],
        is_initial_mode: bool,
        has_partial_tail: bool,
    ) -> Vec<Candidate> {
        self.decode_sentence_paths(input, syllables, is_initial_mode, has_partial_tail, 0)
            .0
    }

    /// 整句解码：用动态规划把长拼音串切成最优词组序列，拼成完整句子候选。
    ///
    /// 这是长句连续输入的核心能力。当前候选机制只返回词库里存在的单个词条
    /// （如「我喜欢」「吃饭」），但用户输入 `woxihuanchifan` 期望直接得到整句
    /// 「我喜欢吃饭」作为首候选，而非逐词选择。
    ///
    /// 算法（Viterbi 风格 DP，从右向左）：
    /// 1. 把输入切成音节序列 `s[0..n]`
    /// 2. `best[i]` = 从位置 i 到末尾的最优切分（累计分数 + 回溯指针）
    /// 3. 对每个 i，枚举词长 k=1..=MAX_WORD_SYLLABLES，查 `s[i..i+k]` 的最高频词
    /// 4. 分数 = 词频（对数压缩）+ best[i+k].score；单字给惩罚（鼓励多字词切分）
    /// 5. 回溯得到词组序列，拼接成整句文本
    ///
    /// 触发条件（保守，避免短输入和简拼误触发）：
    /// - 音节数 >= 3（至少能切成 2 个词）
    /// - 最优切分产生 >= 2 个词（单词组无需解码，已有候选）
    ///
    /// 返回 `Some(Candidate)` 表示生成了整句候选，`None` 表示无需/无法解码。
    /// 整句候选的 base_freq 取切分中最高频词的 freq，pinyin 为完整音节序列。
    /// 按首字母匹配最合适的汉字（用于末尾不完整音节补全）。
    ///
    /// 长句输入中，末尾不完整音节（如 xiwangnih 的 h）无法整句解码，
    /// 用首字母匹配字拼到整句末尾，让整句候选平滑增长不闪烁。
    ///
    /// 选字策略（让补的字更自然）：
    /// 1. 候选集 = 首字母开头的最高频字
    /// 2. 若有上文（整句末字），用 bigram 加分优先选与上文搭配的字
    ///    （上文「你」+ h →「好」比首字母最高频「和」更自然）
    ///
    /// 安全性：只返回汉字（is_valid_chinese_text 防御字母泄漏）。
    /// 简拼首字母展开为音节（用于混合输入绿词）。
    ///
    /// 结合上文 bigram 选最合适的音节，而非固定最高频。
    /// 如混合输入 qiehuanshurufzhihou（f 是简拼），上文「输入」+ f：
    /// - 仅按最高频：固定取 f→fang「方」，绿词拼成「切换输入方知后」（错字）
    /// - 遍历 f 开头所有音节（fa/fan/fang/fei/...），用 bigram(上文, 音节首字)
    ///   选最强的 → 「法」(fa) 而非「方」(fang)，绿词拼成「切换输入法之后」
    ///
    /// `prev_char`：前一个有效音节的首字（如 "ru"→'r'）。无上文时回退最高频。
    /// 简拼首字母展开为音节（用于混合输入绿词）。
    ///
    /// 基于成词选择音节：
    /// 仅靠 bigram(prev字, 候选字) 在冷启动几乎失效，回退选
    /// 最高频单字（f→fang「方」），导致 shurufxiangmu 的 f 展开成 fang 拼出「乳房」
    /// 而非「输入法」的 fa。
    ///
    /// 三层优先级：
    /// 1. user_words 成词：尝试 initial 开头的所有音节，拼接 prev_syllables 后
    ///    查 user_words（用户学过的词）。命中则用该音节。如 ru+fa 查到「输入法」→fa。
    /// 2. 词库多字词：同上但查 base_dict 多字词。如 ru+fang→「乳房」（词库真词）。
    /// 3. 回退最高频：上述都不成词，取该首字母最高频单字的音节。
    #[allow(dead_code)] // 简拼音节展开，当前 DP 路径不再调用
    pub(crate) fn expand_initial_to_syllable(
        &self,
        initial: char,
        prev_syllables: &[&str],
        next_initials: &[char],
    ) -> Option<String> {
        // 收集 initial 开头的所有有效音节（f→fa/fan/fang/fei/fen/feng/fo/fou/fu）
        let mut candidate_syllables: Vec<String> = Vec::new();
        // 用 self.dict（含 user_words）收集首字母音节，让用户学习立即影响简拼展开。
        let single_cands = self.dict.query_single_char_by_initial(initial);
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for c in &single_cands {
            if let Some(first_syl) = c.pinyin.split_whitespace().next() {
                if seen.insert(first_syl.to_string()) {
                    candidate_syllables.push(first_syl.to_string());
                }
            }
        }
        if candidate_syllables.is_empty() {
            return None;
        }

        // 优先级 0：当前音节 + 后续首字母展开 能拼成词。
        // 如 t + j?，tiao + jian→条件（词库词），选 tiao 而非最高频的 ta。
        // 这是"前瞻后续简拼"——解决 jiafentj 的 t 展开成 ta 而非 tiao 的问题。
        if !next_initials.is_empty() {
            let mut best_lookahead: Option<(String, u32)> = None;
            for syl in &candidate_syllables {
                // 对后续每个首字母，收集它们开头的音节
                for &ni in next_initials {
                    let next_syls: Vec<String> = self
                        .dict
                        .query_single_char_by_initial(ni)
                        .iter()
                        .filter_map(|c| c.pinyin.split_whitespace().next().map(|s| s.to_string()))
                        .collect::<std::collections::HashSet<_>>()
                        .into_iter()
                        .collect();
                    for ns in &next_syls {
                        // 查 syl + ns 能否拼成词（如 tiao + jian → 条件）
                        let seg = [syl.as_str(), ns.as_str()];
                        let cands = self.dict.query(&seg);
                        for c in &cands {
                            if c.text.chars().count() >= 2 {
                                // 用用户选择次数（counts）优先，而非词库 base_freq。
                                // 用户选过"条件"多次（counts 高），但 base_freq=1（低），
                                // 不应被"甜酒"(base_freq=1618 但用户可能没选过) 压制。
                                let user_count =
                                    self.freq.counts.get(c.text.as_ref()).copied().unwrap_or(0);
                                let is_learned =
                                    self.user_words.contains_key(c.text.as_ref()) || user_count > 0;
                                // score = 用户选择次数 × 10000 + 词库 freq（用户习惯主导）
                                let score = user_count * 10000 + c.base_freq;
                                if is_learned
                                    && best_lookahead.as_ref().is_none_or(|(_, s)| score > *s)
                                {
                                    best_lookahead = Some((syl.clone(), score));
                                }
                            }
                        }
                    }
                }
            }
            if let Some((syl, _)) = best_lookahead {
                return Some(syl);
            }
        }

        // 优先级 1：拼接 prev_syllables 后能匹配 user_words 已学词
        // 如 prev=[shu, ru], initial='f'，尝试 shu ru fa / shu ru fan / shu ru fang...
        // 查 user_words 是否有「输入法」对应 shu ru fa → 命中则返回 fa
        if !prev_syllables.is_empty() {
            let mut best_user: Option<(String, u32)> = None; // (音节, 用户频率)
            for syl in &candidate_syllables {
                let mut full_seg: Vec<&str> = prev_syllables.to_vec();
                full_seg.push(syl);
                // 查 user_words：在 dict（含用户词）里精确音节匹配
                let cands = self.dict.query(&full_seg);
                for c in &cands {
                    // 用户选过的词（user_words 入库 或 freq.counts 有累积）。
                    // 系统词不进 user_words，但 freq.counts 记录选择次数，
                    // 双轨判断让系统词也能引导简拼展开（如「输入法」选过即生效）。
                    let is_learned = self.user_words.contains_key(c.text.as_ref())
                        || self.freq.counts.contains_key(c.text.as_ref());
                    if is_learned {
                        // 用户学过这个词，优先级最高
                        let freq = self.freq.counts.get(c.text.as_ref()).copied().unwrap_or(1);
                        if best_user.as_ref().is_none_or(|(_, f)| freq > *f) {
                            best_user = Some((syl.clone(), freq));
                        }
                    }
                }
            }
            if let Some((syl, _)) = best_user {
                return Some(syl);
            }
        }

        // 优先级 2：拼接 prev_syllables 后能匹配词库多字词（非单字）
        // 如 prev=[ru], initial='f'，ru fang→乳房（词库真词）
        if !prev_syllables.is_empty() {
            let mut best_word: Option<(String, u32)> = None; // (音节, 词频)
            for syl in &candidate_syllables {
                let mut full_seg: Vec<&str> = prev_syllables.to_vec();
                full_seg.push(syl);
                let cands = self.dict.query(&full_seg);
                for c in &cands {
                    if c.text.chars().count() >= 2 {
                        // 词库多字词，按 base_freq 选最强的
                        if best_word.as_ref().is_none_or(|(_, f)| c.base_freq > *f) {
                            best_word = Some((syl.clone(), c.base_freq));
                        }
                    }
                }
            }
            if let Some((syl, _)) = best_word {
                return Some(syl);
            }
        }

        // 优先级 3：回退最高频单字的音节（原逻辑）
        candidate_syllables.into_iter().next()
    }

    pub(crate) fn best_char_for_initial(
        &self,
        partial: &str,
        prev_char: Option<&str>,
    ) -> Option<String> {
        let initial = partial.chars().next()?;
        let mut cands = self.dict.query_single_char_by_initial(initial);
        // 完整 partial 前缀过滤（含 zh/ch/sh 扩展）：悬尾可能是双字
        // 声母（zh/ch/sh，双拼 v/i/u 键尾与全拼 zh 半截）——只认拼音以完整
        // partial 开头的字，防「zh」尾被补成首字母同为 z 但非 zh 系的字（实测
        // 全拼 faxianyigezh 补出「发现一个在[句]」——在=zai 是 z 系）；单字母
        // partial 与原行为逐字等价（query_single_char_by_initial 本就按音节
        // starts_with(首字母) 取候选，本过滤对单字母是恒真条件）。
        cands.retain(|c| {
            c.pinyin
                .split_whitespace()
                .next()
                .is_some_and(|p| p.starts_with(partial))
        });
        // bigram 加分：与上文搭配的字优先。
        //
        // 三层排序：bigram（用户/种子词级
        // 邻接证据）> 组合词关联（collocation）> 字频。
        //
        // 组合词层：悬尾补字若只有 bigram→字频两层，冷启动 bigram 全 0 时
        // 纯字频定序——干+m 补出 9760 的「没」而非 8940 的「吗」，而全拼
        // nixiangganma 的胜者 [你想][干][吗] 恰恰靠 boundary 组合词证据
        // collocation_freq(干,吗,ma)=706 过门（见本文件 BOUNDARY_
        // COLLOCATION_FREQ_FLOOR）。因此把同一证据接入补字：与上文成组合词
        //（prev+字 在词库成词且末音节一致）的字排进纯字频之前。过门槛者
        // 之间仍按字频定序——吗(8940) 压 嘛(1000)，与全拼首选同文本，续打
        // 到全拼时零跳变。
        /// 组合词证据门槛：与下方 DP 边界对 BOUNDARY_COLLOCATION_FREQ_FLOOR 同值
        ///（<500 的生僻组合词不算佐证）。
        const COMPLETION_COLOCATION_FLOOR: u32 = 500;
        if let Some(prev) = prev_char {
            let colloc_ok = |c: &Candidate| -> bool {
                c.pinyin
                    .split_whitespace()
                    .next()
                    .and_then(|syl| self.dict.collocation_freq(prev, &c.text, syl))
                    .is_some_and(|f| f >= COMPLETION_COLOCATION_FLOOR)
            };
            cands.sort_by(|a, b| {
                let ba = self.freq.word_bigram_count(prev, &a.text);
                let bb = self.freq.word_bigram_count(prev, &b.text);
                let ca = colloc_ok(a) as u8;
                let cb = colloc_ok(b) as u8;
                // bigram 高的优先；同 bigram 组合词关联优先；再同则 base_freq 兜底
                bb.cmp(&ba)
                    .then_with(|| cb.cmp(&ca))
                    .then_with(|| b.base_freq.cmp(&a.base_freq))
            });
        }
        cands
            .into_iter()
            .find(|c| is_valid_chinese_text(&c.text))
            .map(|c| c.text.as_ref().to_owned())
    }

    /// `paths_limit > 0` 时在绿词截断/门槛之前收集
    /// 过门禁的 top-K 路径（按 DP 分降序、文本去重）供 AI 切分裁决；
    /// = 0 时不收集（收集逻辑整段被 `paths_limit > 0` 门控，主路径行为不变）。
    pub(crate) fn decode_sentence_paths(
        &self,
        input: &str,
        syllables: &[&str],
        is_initial_mode: bool,
        has_partial_tail: bool,
        paths_limit: usize,
    ) -> (Vec<Candidate>, Vec<SentencePath>) {
        // ≥3 音节才触发整句解码。2 音节要么是词库已有的词（查询已返回，不需绿词），
        // 要么是两个独立单字的临时组合（如「只呢」），不构成词，是垃圾。
        // 真正的"多词组合"至少 3 音节（如「我在家」「不知道」），才有整句解码价值。
        const MIN_SYLLABLES: usize = 3;
        const MAX_WORD_SYLLABLES: usize = 4; // 单词最长 4 音节（如「中华人民共和国」分段）
        if syllables.len() < MIN_SYLLABLES {
            return (Vec::new(), Vec::new());
        }

        // 词库已有同音整词时不生成拼凑绿词（防"变以期"类噪音绿词）。
        //
        // 触发场景：bianyiqi 输入时词库已有「编译器」(bian yi qi) 整词，但 DP 仍会
        // 拼出「变+以期」之类单字组合绿词——整词路径 word_count=1 被 < 2 判据丢弃，
        // 导致次优的拼凑路径（word_count≥2）胜出，生成无意义的"变以期"绿词。
        //
        // 判据：整段音节 syllables 在词库（含 user_words）查得到整词 → 直接返回空。
        // 长句场景（如「明天下午一起去看电影」9 音节）词库没有整词，查询返回空，
        // 不触发短路，DP 正常运行生成多词组合绿词。
        //
        // 仅全拼模式生效（!is_initial_mode）：简拼模式（首字母 Token）的"整词"概念
        // 模糊（jpsr 可匹配大量首字母组合），不适合用整词判据短路，保持原行为。
        //
        // 带 partial 尾时不短路：调用方传的 syllables 是 valid
        // 前缀，句子随后还会用尾首字母补字（见 mod.rs 整句插入段）——它比前缀词多
        // 覆盖尾巴，不存在与整词重复的可能。若拿前缀直接查整词短路，会把
        // faxianyigew（valid=fa-xian-yi-ge 恰好是词「发现一个」）的句子层整体杀死，
        // partial 尾补字防闪烁机制失效（追加声母后首选回退「发现」；
        // xiwangnih→「希望」同型）。
        //
        // 手动调整不受影响：用户逐字/选子词自组词走另一条路径；commit 后写入
        // green_memo，下次走 mod.rs 的 green_memo 优先提升路径（在本入口之后执行，
        // 不被本短路拦截）。
        //
        // 安全性：只读查询（self.dict 含 user_words），不修改任何状态。
        if !is_initial_mode && !has_partial_tail && !self.dict.query(syllables).is_empty() {
            return (Vec::new(), Vec::new());
        }

        let n = syllables.len();

        // A/B 对照开关：仅测试构建可关闭长程语境证据，
        // 用于在同一份代码/同一 DB 上复现关闭态的行为（生产恒 true）。
        // 定义在模块级（见文件顶部 `longrange_ab`），测试通过
        // `set_longrange_enabled_for_test` 置位。

        // 查询辅助（简拼绿词支持）：
        // 全拼模式用 dict.query（精确音节）；简拼模式用 query_tokens（首字母子树）。
        //
        // 段缓存：dict 级查询（query_tokens /
        // query + 末段全拼单字边补充 + 全消费字数 retain）收敛到
        // Engine::seg_query_cached 跨按键复用——打长句时稳定前缀段的简拼子树
        // 收集（单字母段收集整棵子树，实测占 DP 耗时 78%，n=15 时 11.3/14.6ms）
        // 不每键重算，新键只增量计算尾部段。counts 排序 / truncate(50) /
        // green_memo 补充每键新鲜应用（选择学习与整句记忆即时生效）——
        // 缓存只改性能不改结果。失效仅挂 dict 变异点（seg_cache_clear），
        // 本层不依赖 counts / green_memo / context。
        let query_seg = |seg: &[&str], is_last: bool| -> Vec<Candidate> {
            if is_initial_mode {
                // 缓存层语义（末位单字边 / 全消费守卫的完整说明见
                // seg_query_cached）：tokens 构造 + query_tokens 子树收集 +
                // 末段单字边 + 字数 == 段长 retain。
                let mut cands = self.seg_query_cached(seg, true, is_last);
                // 性能保护 + 用户词优先：counts 优先排序（用户选
                // 过的词不被 base_freq 截断丢弃）后截 50。每键新鲜执行。
                cands.sort_by(|a, b| {
                    let ca = self.freq.counts.get(a.text.as_ref()).copied().unwrap_or(0);
                    let cb = self.freq.counts.get(b.text.as_ref()).copied().unwrap_or(0);
                    cb.cmp(&ca).then_with(|| b.base_freq.cmp(&a.base_freq))
                });
                cands.truncate(50);
                cands
            } else {
                let mut cands = self.seg_query_cached(seg, false, is_last);
                // green_memo 回退：查不到时尝试 green_memo 里的整句记忆
                // 二级索引 O(1) 查找
                // green_memo 查询：
                // 不管 dict 是否有候选，都额外查 green_memo（整段时用 input key）。
                // 这确保绿词即使 dict 有部分匹配也能出现。（每键新鲜执行，不缓存）
                {
                    let seg_pinyin = seg.join(" ");
                    let seg_input = seg.concat();
                    let is_full_segment = seg_input == input;
                    if is_full_segment {
                        if let Some(entries) = self
                            .green_memo
                            .get(input)
                            .or_else(|| self.green_memo.get(&seg_pinyin))
                        {
                            for (gt, gf, gp) in entries {
                                // 去重：dict 已返回同 text 的不重复加
                                if !cands
                                    .iter()
                                    .any(|c: &Candidate| c.text.as_ref() == gt.as_str())
                                {
                                    cands.push(Candidate {
                                        text: gt.as_str().into(),
                                        base_freq: *gf,
                                        // 学习时真实读音 > 文本反推 > 段切分（三级）
                                        pinyin: gp
                                            .clone()
                                            .or_else(|| self.derive_reading_from_text(input, gt))
                                            .unwrap_or_else(|| seg_pinyin.clone())
                                            .into(),
                                        is_primary: true,
                                        is_correction: false,
                                        is_sentence: false,
                                        is_guess: false,
                                        is_shortcut: false,
                                        is_english: false,
                                        sentence_score: 0,
                                    });
                                }
                            }
                        }
                    }
                }
                cands
            }
        };

        // N-best 前向 DP（解决贪心 DP 不可回溯缺陷）。
        //
        // 单一最优版 best[i] 只存一个最优末词：当 best[3] 被「作废」k=2 词锁定后，
        // 全局更优的 [坐]单字+[飞机] 路径彻底不可达——贪心不会为"后续能有飞机骨架"
        // 而牺牲当前位置。
        //
        // 因此 best[i] 保留 Top-N 个候选（每个携带 back_ptr 指向前驱），
        // 让次优末词（如「坐」单字）存活以支撑后续更优路径。回溯时对 N 条完整路径
        // 全局重评，选最优。N=3 在精度和噪音间取得平衡。
        //
        // 复杂度：O(N²·n·k_max)，N=3, n≤20, k_max=4 → 最坏 720 次转移，<1ms。
        // TOP_N 随多候选边同步扩容：每段边数 ×3 后 best[i] 若仍只留
        // 3 条路径会挤掉原本存活的中间节点（wohaoxiangchifan 的 [我,好想,吃饭]
        // 被剪枝淘汰即此症状）。N=3→5，转移数 ×1.67，仍在毫秒级。
        //
        // 路径收集模式放宽保留宽度：paths_limit>0
        //（AI 长句裁决）时 TOP_N 5→12——把被剪枝的尾部路径（如「…汉字+词句」
        // 这类未进 top-5 的合法切分）也暴露给路径收集，供 AI 字格。收集模式
        // 由调用方双跑隔离（主候选仍走 paths_limit=0 的原参数，AI 开/关候选
        // 逐字节一致的不变量不破，见 engine::decode_sentence_collect）。
        let top_n: usize = if paths_limit > 0 { 12 } else { 5 };

        // 每段进入词图的平行词边数：Top-M 词各自
        // 作为 DP 边参与转移，替代「每段只留一个代表词」。M 增大精度升、
        // 转移数 ×M——M=3 时最坏转移 ~2160 次（n=20,k=4,N=3），仍在毫秒级。
        //（词段（k>=2）边宽度恒 3 不动：M 再放大（如 6）会扰动路径竞争、把
        // kanta 守护的「大致」路径挤出，收集模式只放宽 top_n。）
        let top_m: usize = 3;
        // 单字段（k=1）边配额独立放宽到 4：失败归因显示
        // 整句死亡几乎全部卡在单字边：常用功能字（事/之/市/件/机/由）
        // 在音节内频次排第 4-8 名，3 条边进不了词格，gold 路径直接死亡
        //（「单字配额∞/词配额 3」可构造 100% 失败样本，反向「词∞/单字 3」
        // 仅 18%）。词段恒 3 不动；单字配额再放大（如 6）会扰动路径竞争、
        // 挤出 kanta 守护的「大致」自然切分
        //（verify_fullpinyin_long_sentence_paths 守护），4 是扩容收益与竞争
        // 扰动的平衡档位。
        let top_m_single: usize = 4;

        // DP 节点：best[i] 列表中的一个候选，记录从 0 到 i 的一条切分路径的末段信息。
        struct DPNode {
            score: i32,                // 从 0 到 i 的累计分（含本段）
            last_k: usize,             // 到达 i 的最后一段词长（音节数）
            last_word: Option<String>, // 最后一段末词文本（None 仅哨兵）
            // 倒数第2段末词文本（trigram 用）。
            // None = 句首段（无前驱）/ 哨兵。与 last_word 配合构造 [prev2, prev1] 上下文。
            prev_word: Option<String>,
            last_freq: u32,  // 最后一段末词的 base_freq（回溯用，避免重复 query_seg）
            back_pos: usize, // 前驱位置（i - last_k）
            back_idx: usize, // 前驱在 best[back_pos] 列表中的下标
        }

        // best[i] = 从位置 0 到 i 的 Top-N 候选列表
        let mut best: Vec<Vec<DPNode>> = (0..=n).map(|_| Vec::new()).collect();
        best[0] = vec![DPNode {
            score: 0,
            last_k: 0,
            last_word: None,
            prev_word: None,
            last_freq: 0,
            back_pos: 0,
            back_idx: 0,
        }];

        for i in 0..n {
            if best[i].is_empty() {
                continue; // 位置 i 不可达
            }
            let max_k = MAX_WORD_SYLLABLES.min(n - i);
            // 克隆 best[i] 快照，释放对 best 的借用，后续才能可变借用 best[next]
            // 四元组：(idx, last_word=prev1, prev_word=prev2, score)
            let preds_i: Vec<(usize, Option<String>, Option<String>, i32)> = best[i]
                .iter()
                .enumerate()
                .map(|(idx, p)| (idx, p.last_word.clone(), p.prev_word.clone(), p.score))
                .collect();
            for k in (1..=max_k).rev() {
                let seg = &syllables[i..i + k];
                // 简拼模式旁路 syllable 校验（首字母不是有效音节）
                if !is_initial_mode && seg.iter().any(|s| !crate::pinyin::is_valid_syllable_pub(s))
                {
                    continue;
                }
                // 用 self.dict（含 user_words）而非 base_dict（启动快照）。
                // 用户 commit 学到的子词进 base_dict 有延迟，用 dict 让学习立即生效。
                let cands = query_seg(seg, i + k == n);
                if cands.is_empty() {
                    continue;
                }
                // 选词优先级（bigram 引导 pick 决策）：
                // 先用默认优先级（user_words > recent > counts > base_freq）选 pick，
                // 然后在前驱循环里，如果 bigram(prev, pick) 弱但 bigram(prev, other) 强，
                // 用 bigram 最强的候选替代 pick（bigram 引导覆盖）。
                // 这让用户积累 bigram(加分,条件) 后，DP 在 fen 段选"加分"而非"加粉"。
                // 选词排序键：default_pick 的
                // max_by 与多候选边的排序共用同一比较键，保证首位行为严格不变。
                // 键序：user_words > recent(差距守卫) > counts(守卫) > base_freq。
                let cand_order = |a: &Candidate, b: &Candidate| -> core::cmp::Ordering {
                    let a_user = self.user_words.contains_key(a.text.as_ref()) as u32;
                    let b_user = self.user_words.contains_key(b.text.as_ref()) as u32;
                    if a_user != b_user {
                        return a_user.cmp(&b_user);
                    }
                    let ra = self.freq.recent_score(&a.text);
                    let rb = self.freq.recent_score(&b.text);
                    if ra != rb {
                        // freq 差距守卫：
                        // freq 是 compute_freq 真实值（50-9999，200 倍差距）。
                        // 阈值 20 意味着：base_freq 差距 >20 倍时（如 我9999 vs 窝500），
                        // recent_score/counts 不能翻盘，直接比 base_freq。
                        // 差距 ≤20 倍时（如 吃6800 vs 持4500=1.5x，我9999 vs 吃6800=1.47x），
                        // recent_score/counts 正常生效，用户偏好可主导。
                        let freq_ratio = if a.base_freq > 0 && b.base_freq > 0 {
                            a.base_freq.max(b.base_freq) / a.base_freq.min(b.base_freq)
                        } else {
                            1
                        };
                        if freq_ratio <= 20 {
                            return ra.cmp(&rb);
                        }
                        // 差距过大：跳过 recent_score，fall through 到 counts/base_freq 比较
                    }
                    let ca = self.freq.counts.get(a.text.as_ref()).copied().unwrap_or(0);
                    let cb = self.freq.counts.get(b.text.as_ref()).copied().unwrap_or(0);
                    if ca != cb {
                        // counts 守卫（同 recent_score，阈值 20）
                        let freq_ratio2 = if a.base_freq > 0 && b.base_freq > 0 {
                            a.base_freq.max(b.base_freq) / a.base_freq.min(b.base_freq)
                        } else {
                            1
                        };
                        if freq_ratio2 <= 20 {
                            return ca.cmp(&cb);
                        }
                    }
                    a.base_freq.cmp(&b.base_freq)
                };
                let default_pick = cands
                    .iter()
                    .filter(|c| !self.removed_texts.contains(c.text.as_ref()))
                    .filter(|c| c.text.chars().count() >= 2 || c.is_primary)
                    .max_by(|a, b| cand_order(a, b))
                    .or_else(|| {
                        cands.iter().max_by(|a, b| {
                            let ra = self.freq.recent_score(&a.text);
                            let rb = self.freq.recent_score(&b.text);
                            ra.cmp(&rb).then_with(|| a.base_freq.cmp(&b.base_freq))
                        })
                    })
                    .unwrap();

                // bigram 引导覆盖 + 多候选边：
                // 每段只留一个 pick 进词图的话，同段其余候选全部丢弃——Viterbi DP 的
                // 最优性保证建立在「所有候选词都作为图的边」之上，砍边后求的不再是
                // 全局最优切分（正确句子可能因中间段的代表词选错而永远拼不出）。
                // 现保留 Top-M 词作为平行边：首位严格等于原 pick 行为（bigram 覆盖
                // 优先，无覆盖用 default_pick），按 cand_order 同键排序补位，
                // bigram 覆盖词若被挤出仍追加保活（最多 +1）。
                let picks: Vec<&Candidate> = {
                    let valid_indices: Vec<usize> = cands
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| !self.removed_texts.contains(c.text.as_ref()))
                        .filter(|(_, c)| c.text.chars().count() >= 2 || c.is_primary)
                        .map(|(i, _)| i)
                        .collect();
                    // bigram 来源：DP 前驱 + context 上文栈（句首段无前驱时用 context）
                    let has_dp_prev = preds_i.iter().any(|(_, pw, _, _)| pw.is_some());
                    let bigram_sources: Vec<&str> = if has_dp_prev {
                        preds_i
                            .iter()
                            .filter_map(|(_, pw, _, _)| pw.as_deref())
                            .collect()
                    } else if is_initial_mode {
                        // 简拼模式（is_initial_mode）下句首段完全不读 context：
                        // 简拼输入（如 wwww）用户的真实意图是"逐字自造词"或"找首字母开头的词"，
                        // 与上文的语义/共现关联极弱。任何 context 引导都是噪音——
                        // 用户刚选过"忘我无为"（w 开头绿词）→ context 含它 → 下次 wwww 的
                        // DP 把 w 单字里和"忘我无为"bigram 高的字拼出来，结果随 context 翻转。
                        // 实证：上文变结果就变，同上文结果才稳定。
                        // 简拼下 bigram_sources 留空，DP pick 纯靠 freq/recent 稳态信号。
                        Vec::new()
                    } else {
                        // 全拼句首段：用 context 上文栈最近 3 个词（保留跨句衔接能力）
                        self.context
                            .iter()
                            .rev()
                            .take(3)
                            .map(|s| s.as_str())
                            .collect()
                    };
                    let default_bigram_sum: u32 = bigram_sources
                        .iter()
                        .map(|&pw| self.freq.word_bigram_count(pw, &default_pick.text))
                        .sum();
                    let mut best_idx = None;
                    let mut best_bigram_sum = 0u32;
                    for &idx in &valid_indices {
                        let bc_sum: u32 = bigram_sources
                            .iter()
                            .map(|&pw| self.freq.word_bigram_count(pw, &cands[idx].text))
                            .sum();
                        if bc_sum > best_bigram_sum && bc_sum > default_bigram_sum {
                            best_bigram_sum = bc_sum;
                            best_idx = Some(idx);
                        }
                    }
                    // 首位 = 默认 pick（bigram 覆盖优先，行为不变）
                    let lead: &Candidate = match best_idx {
                        Some(idx) => &cands[idx],
                        // 注：不做 semantic 引导覆盖。
                        // bigram 无法区分候选时（best_idx=None），直接用 default_pick
                        // （按 freq/counts 排序的稳态结果）。semantic 引导在简拼下
                        // 会让结果随 context 翻转、全拼下不可靠，故不采用。
                        None => default_pick,
                    };
                    let mut picks: Vec<&Candidate> = vec![lead];
                    // 同键排序补位到 TOP_M（去重）
                    let mut ranked: Vec<&Candidate> = cands
                        .iter()
                        .filter(|c| !self.removed_texts.contains(c.text.as_ref()))
                        .filter(|c| c.text.chars().count() >= 2 || c.is_primary)
                        .collect();
                    // `cand_order` 的「freq 差 >20× 跳过 recent/counts」是成对守卫，不满足
                    // 全序（A/B 比 recent、B/C 比 counts、A/C 比 freq 可成环），
                    // 不能直接交给排序器（全序违规会 panic，FFI 捕获后返回空候选，
                    // 表现为候选框整窗消失、补打字母才恢复）。
                    // 排序改用按组内最高词频归一的固定键：离最高频 >20× 的
                    // 候选跳过 recent/counts（保留 freq 差距守卫意图），组内按
                    // user_words → recent → counts → base_freq；lead 仍由上面的
                    // cand_order/default_pick 决定，首位行为不变。
                    let max_base = ranked.iter().map(|c| c.base_freq).max().unwrap_or(0);
                    let sort_key = |c: &Candidate| -> (u8, u32, u32, u32) {
                        let within_20x =
                            max_base == 0 || c.base_freq.saturating_mul(20) >= max_base;
                        (
                            self.user_words.contains_key(c.text.as_ref()) as u8,
                            if within_20x {
                                self.freq.recent_score(&c.text)
                            } else {
                                0
                            },
                            if within_20x {
                                self.freq.counts.get(c.text.as_ref()).copied().unwrap_or(0)
                            } else {
                                0
                            },
                            c.base_freq,
                        )
                    };
                    ranked.sort_by_key(|c| std::cmp::Reverse(sort_key(c)));
                    // 段配额：单字段（k=1）用 top_m_single，词段恒 top_m=3
                    //（见上方单字段配额注释；lead/首位行为不受影响）
                    let seg_quota = if k == 1 { top_m_single } else { top_m };
                    for c in ranked {
                        if picks.len() >= seg_quota {
                            break;
                        }
                        if picks.iter().all(|p| p.text != c.text) {
                            picks.push(c);
                        }
                    }
                    // bigram 覆盖词保活：若被 TOP_M 挤出仍追加（强上下文证据不丢）
                    if let Some(idx) = best_idx {
                        let bc = &cands[idx];
                        if picks.iter().all(|p| p.text != bc.text) {
                            picks.push(bc);
                        }
                    }
                    picks
                };

                // ── 长程语境证据 ──────────────────────────────
                //
                // 缺口：绿词 DP 若只在句首段读 `self.context`，
                // 句中段只有 DP 路径上的 prev1/prev2（Markov-2）。所以
                // 「机场 → 马上出发」这类「语境词在前、歧义词在句中」的关系，
                // DP 完全看不见——15 个不同机场类上下文对 mashangchufa 给出
                // 完全相同的结果（tests_longrange 实证）。
                //
                // 本段让每个转移段都评估一次「self.context 更远上文 → 候选」
                // 的衰减证据，但严格门控：
                // ① 仅全拼（简拼句首不读 context 是既有约束）；
                // ② 仅真同音歧义段（组内 ≥2 个多字同拼音词、频率比 ≤5×）——
                //    歧义段才是缺少裁决的地方；非歧义段是确定性命中，加了只会
                //    让上下文推翻词频/结构证据（散弹效应的反面）；
                // ③ 只加正增量（该读法明显强于最强对手时才加分），有上限；
                // ④ 零证据 = 空表 = 逐字节保持现状。
                let mut longrange: Vec<(String, f32)> = Vec::new();
                if !is_initial_mode && longrange_ab::enabled() {
                    let mut members: Vec<&Candidate> = cands
                        .iter()
                        .filter(|c| {
                            c.text.chars().count() >= 2
                                && c.pinyin.split_whitespace().eq(seg.iter().copied())
                        })
                        .collect();
                    members.sort_by_key(|c| std::cmp::Reverse(c.base_freq));
                    if members.len() >= 2 {
                        let top = members[0].base_freq.max(1);
                        let second = members[1].base_freq.max(1);
                        if top as f32 / second as f32 <= super::ambiguity::LONG_RANGE_AMBIG_RATIO {
                            // 句首段：跳过 LM 已消费的最近 2 词；句中段：全部计入。
                            let skip = if i == 0 { 2 } else { 0 };
                            for m in members.iter().take(6) {
                                let text = m.text.to_string();
                                if longrange.iter().any(|(t, _)| *t == text) {
                                    continue;
                                }
                                let ev = self.longrange_evidence(&text, skip);
                                if ev > 0.0 {
                                    longrange.push((text, ev));
                                }
                            }
                        }
                    }
                }

                let next = i + k;

                // 多候选边：Top-M 词各自作为平行边
                // 进入转移，每边独立计结构/行为分量（LM 先验与搭配惩罚在下面的
                // 前驱循环里按各前驱分别打分——概率按条件分开算，不跨路径混合）。
                for pick in &picks {
                    let char_count = pick.text.chars().count();
                    let is_word = char_count >= 2;
                    // 统计先验（词频 + 上下文）已由插值 LM 在前驱循环里统一打分
                    // （lm_prior_score），此处只保留结构性/用户行为分量。
                    let recent_score_val = self.freq.recent_score(&pick.text);
                    let recent_bonus =
                        ((recent_score_val as f64 / 160.0).round() as i32).clamp(0, 6);
                    let exact_match = char_count == k;
                    // 词完整性加分：k≥3 整词精确
                    // 匹配边加档。碎拼链靠「单字高 unigram + 后续词条件加成」双击点
                    // 得分（大|游戏 4 分压过整词 打游戏 -4；同文本整词路径还被同文本
                    // 高分碎拼路径去重吞掉）——整词是更诚实的切分，加分让词完整性
                    // 浮上来。k=2 双字词不动（最普遍形态，防扰动既有调优）。
                    // *12 决定性档（*8 恰好抹平成 14:14 平局、靠平局顺序险胜——
                    // 太脆，用户数据一变就翻）。
                    let word_adjust = if is_word {
                        (k as i32 - 1) * 2
                            + if exact_match {
                                3 + if k >= 3 { (k as i32 - 2) * 12 } else { 0 }
                            } else {
                                0
                            }
                    } else {
                        -4
                    };

                    // 路径级单字惩罚（每增加一个单字段，整路径额外扣 3 分）。
                    let path_single_penalty = if !is_word { 3 } else { 0 };

                    // 对 best[i] 的每个前驱候选，计算转移到 next 的分数
                    // 四元组：(pred_idx, prev1=last_word, prev2=prev_word, score)
                    for (pred_idx, prev1_word, prev2_word, prev_score) in preds_i.iter() {
                        // 统计先验（插值 LM）：三元插值条件概率
                        // λ3·log2(P3)+λ2·log2(P2)+λ1·log2(P1) 归一化衡量上文选择性，
                        // 避免 raw-count 高频弱指向的搭配错配（有则/着个）。
                        //
                        // 上下文构造（与 context_bonus 口径一致）：
                        // - 句中段（有 prev1）：[prev2?, prev1]，prev1 紧邻上文。
                        // - 句首段（无 prev1）：简拼 → 空 ctx（约束：简拼句首不读 context）；
                        //   全拼 → self.context（跨句衔接，lm_prior_score 取栈尾两词）。
                        //
                        // 量级：lm_prior_score 返回 clamp(0, 32)（对应 freq_score 0-16 +
                        // context 0-16 之和的量级），与保留分量 word_adjust(-4..11)、
                        // recent_bonus(0-6)、collocation_penalty(-3) 同量级平衡。
                        let ctx_buf: Vec<String>;
                        let ctx_slice: &[String] = if let Some(prev1) = prev1_word {
                            ctx_buf = match prev2_word {
                                Some(p2) => vec![p2.clone(), prev1.clone()],
                                None => vec![prev1.clone()],
                            };
                            &ctx_buf
                        } else if is_initial_mode {
                            // 简拼句首段：完全不读 context：简拼输入
                            // 用户意图是逐字自造词，与上文语义关联极弱，任何 context
                            // 引导都是噪音（结果随 context 翻转）。
                            &[]
                        } else {
                            // 全拼句首段：用 engine.context 作上文（跨句衔接）
                            &self.context
                        };
                        let lm_score = self.lm_prior_score(ctx_slice, &pick.text, pick.base_freq);

                        // 搭配惩罚：
                        // 相邻两词既无词级 bigram、又无字级 bigram、又无 trigram → 判定不合理，
                        // 轻惩罚 -3（可被 freq_score/recent_bonus 抵消，不误杀新组合）。
                        // 用本地统计判定（weak_collocation_penalty 思路）。句首段（无 prev1）不判。
                        //
                        // 注：循环里 prev1_word 是紧邻 pick 的上文词（distance==0），
                        //     prev2_word 是更早一词（distance==1，trigram 第三元）。
                        let collocation_penalty: i32 = if let Some(prev1) = &prev1_word {
                            let w_bc = self.freq.word_bigram_count(prev1, &pick.text);
                            // 按需查组合词（上文+候选是否构成词库词）
                            // 注：DP 路径传 pick.pinyin 做读音校验（pick 是切分段，pinyin 与切分一致）
                            let c_bc = if w_bc == 0 {
                                self.dict
                                    .collocation_freq(prev1, &pick.text, &pick.pinyin)
                                    .map(|f| {
                                        (((f as f64).log2().max(0.0) / 2.0).round() as u32)
                                            .clamp(1, 8)
                                    })
                                    .unwrap_or(0)
                            } else {
                                0
                            };
                            let t_bc = if let Some(prev2) = &prev2_word {
                                self.trigram_count_with_seed(prev2, prev1, &pick.text)
                            } else {
                                0
                            };
                            if w_bc == 0 && c_bc == 0 && t_bc == 0 {
                                -3
                            } else {
                                0
                            }
                        } else {
                            0
                        };

                        // 长程语境证据（仅真歧义段非空）：本边读法必须明显强于
                        // 最强对手才算数（正增量、有上限），零证据时恒 0。
                        let longrange_bonus = if longrange.is_empty() {
                            0
                        } else {
                            match longrange.iter().find(|(t, _)| t == pick.text.as_ref()) {
                                Some((_, mine)) => {
                                    let rival = longrange
                                        .iter()
                                        .filter(|(t, _)| t != pick.text.as_ref())
                                        .map(|(_, s)| *s)
                                        .fold(0.0f32, f32::max);
                                    let delta = mine - rival;
                                    if delta >= super::ambiguity::LONG_RANGE_MIN_DELTA {
                                        ((delta as f64) * super::ambiguity::LONG_RANGE_K)
                                            .min(super::ambiguity::LONG_RANGE_CAP)
                                            as i32
                                    } else {
                                        0
                                    }
                                }
                                None => 0,
                            }
                        };

                        let seg_score = lm_score + recent_bonus + word_adjust - path_single_penalty
                            + collocation_penalty
                            + longrange_bonus;
                        let total = *prev_score + seg_score;

                        // prev_word（下段的 prev2）= 当前段的 prev1（last_word）。
                        // 即：本段成为历史后，它的末词对下一段来说是 prev2。
                        best[next].push(DPNode {
                            score: total,
                            last_k: k,
                            last_word: Some(pick.text.as_ref().to_owned()),
                            prev_word: prev1_word.clone(),
                            last_freq: pick.base_freq,
                            back_pos: i,
                            back_idx: *pred_idx,
                        });
                    }
                }

                // 剪枝（多候选边后统一在段末截：该 (i,k) 的全部边转移完成再剪，
                // 避免先到的边挤掉后到的更强边）：best[next] 保留 Top-N。按 score
                // 降序，同分时保留多样性
                //（不同 last_k 优先），避免 Top-N 全是同一词的变体。
                if best[next].len() > top_n {
                    // 主排序：score 降序
                    best[next].sort_by_key(|x| std::cmp::Reverse(x.score));
                    // 多样性保留：在 Top-N 范围内，同分时优先保留 last_k 未出现过者
                    let mut kept: Vec<DPNode> = Vec::new();
                    let mut seen_k: std::collections::HashSet<usize> =
                        std::collections::HashSet::new();
                    let mut same_score_fillers: Vec<DPNode> = Vec::new();
                    for node in best[next].drain(..) {
                        if kept.len() >= top_n {
                            break;
                        }
                        if !seen_k.contains(&node.last_k) {
                            seen_k.insert(node.last_k);
                            kept.push(node);
                        } else {
                            same_score_fillers.push(node);
                        }
                    }
                    // 补齐到 TOP_N（同分填充器按原 score 降序）
                    same_score_fillers.sort_by_key(|x| std::cmp::Reverse(x.score));
                    for node in same_score_fillers {
                        if kept.len() >= top_n {
                            break;
                        }
                        kept.push(node);
                    }
                    best[next] = kept;
                }
            }
        }

        // 回溯阶段：从 best[n] 的 Top-N 候选分别回溯，得 N 条完整路径。
        // 保留 Top-K 条（K=3）作为绿词候选，让用户在候选列表看到多个选项。
        if best[n].is_empty() {
            return (Vec::new(), Vec::new());
        }

        // 先收集终点的 (idx, score) 快照
        let end_candidates: Vec<(usize, i32)> = best[n]
            .iter()
            .enumerate()
            .map(|(idx, node)| (idx, node.score))
            .collect();
        // 收集所有合法路径，按 score 降序排，取 Top-K
        // 元组：(words, pinyin_parts, word_count, max_freq, path_score, word_freqs)
        // word_freqs = 各段 base_freq（双向词格后向重打分的 unigram 先验用；
        // 回溯零查询——DPNode.last_freq 已存，不重复 query_seg）。
        let mut all_paths: Vec<(Vec<String>, Vec<String>, usize, u32, i32, Vec<u32>)> = Vec::new();
        for (idx, path_score) in end_candidates {
            // 内联回溯单条路径
            let mut words: Vec<String> = Vec::new();
            let mut pinyin_parts: Vec<String> = Vec::new();
            let mut word_freqs: Vec<u32> = Vec::new();
            let mut max_freq = 0u32;
            let mut pos = n;
            let mut cur_idx = idx;
            let mut ok = true;
            while pos > 0 {
                let path_node = match best[pos].get(cur_idx) {
                    Some(pn) => pn,
                    None => {
                        ok = false;
                        break;
                    }
                };
                let k = path_node.last_k;
                if k == 0 {
                    ok = false;
                    break;
                }
                let seg = &syllables[pos - k..pos];
                let seg_pinyin = seg.join(" ");
                // 回溯零查询：last_word 和 last_freq 在 forward DP 时已存入
                // DPNode，回溯直接读，不再重复 query_seg。
                let word_text = path_node.last_word.clone().unwrap_or_default();
                let word_freq = path_node.last_freq;
                words.push(word_text);
                word_freqs.push(word_freq);
                max_freq = max_freq.max(word_freq);
                pinyin_parts.push(seg_pinyin);
                let next_pos = path_node.back_pos;
                let next_idx = path_node.back_idx;
                pos = next_pos;
                cur_idx = next_idx;
            }
            if !ok {
                continue;
            }
            words.reverse();
            pinyin_parts.reverse();
            word_freqs.reverse();
            let word_count = words.len();

            if word_count < 2 {
                continue;
            }
            // 总字数上限守卫（防 wwww 类重复长串）：
            // initial-mode 下输入 N 个简拼字母，候选总字数最多 N + 容差 2（允许"问题"等
            // 2 字词略多于单字母段）。全拼模式不受此限制（全拼下 k 个音节对应 k 字是常态，
            // 长词如"明天下午"4 字 = 4 音节是合理的）。
            // 这是 query_seg 字数过滤之后的兜底——即使某条新路径绕过前一层过滤，
            // 这里仍能挡住超长串候选。
            if is_initial_mode {
                let total_chars: usize = words.iter().map(|w| w.chars().count()).sum();
                let max_allowed = syllables.len() + 2;
                if total_chars > max_allowed {
                    continue;
                }
            }
            // 智能门槛（三层防御）：
            let multi_word_segments: usize =
                words.iter().filter(|w| w.chars().count() >= 2).count();
            let single_segments = word_count - multi_word_segments;
            // 长输入放宽：≥4 个完整音节（含全拼+简拼尾的
            // 混合输入）时用户是"整句连打"意图，完整覆盖本身即强证据——形状门槛
            //（全单字链 / multi<single）在此让位于 DP 分数竞争，否则「天快黑了」
            // (全单字链)、「我去买瓶水」(multi<single) 这类自然句子永远进不了候选
            // （冷启动 Top1 损失 2pt）。
            // 判据刻意用「多字母有效音节数」而非音节总数：纯简拼输入（wwww 的 4 个
            // 单字母段）不算长输入，那类输入的语义是逐字自造词，整句拼凑是噪音
            // ——test_no_repetition_in_jianpin_wwww 守护。
            let long_input = syllables
                .iter()
                .filter(|s| s.len() >= 2 && crate::pinyin::is_valid_syllable_pub(s))
                .count()
                >= 4;
            if multi_word_segments == 0 && !long_input {
                continue;
            }
            // bigram 支持度：累加路径上所有相邻词对
            // 的 word_bigram_count。用户 commit 过相关整句后此值 > 0，表示这条切分与
            // 用户真实历史一致。
            //
            // 用于放宽下方「multi >= single」门槛：合理中文句子常有「功能字为主 + 1 名词」
            // 结构（如「我换了个手机」= 4 单字 + 1 双字），原门槛 multi(1) < single(4)
            // 会误杀。当支持度足够强（≥3，即至少 1 个强 bigram 或多个弱 bigram），
            // 放宽到「multi >= 1」（只要有 1 个多字词即可）。
            //
            // 边界成词佐证：冷启动种子
            // bigram 未覆盖的常见句式（我|好|想|吃饭），word_bigram_count 恒 0 → 门槛
            // 全杀 → 整句候选缺席，前缀词（我好/我好想/我好像）霸屏。补充第二个证据源：
            // 相邻段边界字对成词——前段末字 + 后段首字拼成的二字词在词库且 freq≥500
            //（我好840/好想781/想吃1019，三对即达阈值；吃反类噪音对不成词不计）。
            // 这是「该字符序列拼出来是自然中文词链」的词库级证据，与用户 bigram（行为级
            // 证据）互补。只影响门槛判定，不改路径分数与排序——存活路径仍按 score 竞争，
            // green_accept 动态门槛继续过滤次优绿词。
            // wohuanlegeshouji 不受影响：边界对我换✗/换了✓/了个✓/个手✗ 只凑 2 个，
            // 门槛保持关闭（该场景已由 bigram 通道覆盖，行为零变化）。
            let bigram_support: u32 = words
                .windows(2)
                .map(|w| self.freq.word_bigram_count(&w[0], &w[1]))
                .sum();
            /// 边界成词证据的词频地板：低于此值的生僻组合词不算佐证（防噪音对）。
            const BOUNDARY_COLLOCATION_FREQ_FLOOR: u32 = 500;
            let boundary_support: u32 = (0..words.len() - 1)
                .filter(|&i| {
                    // 前段末字 + 后段首字（如 [..,想] + [吃饭,..] → 想吃），
                    // 读音校验用后段首音节（collocation_freq 校验组合词末音节一致）。
                    let prev_last = words[i].chars().last();
                    let curr_first = words[i + 1].chars().next();
                    let curr_first_syl = pinyin_parts[i + 1].split_whitespace().next();
                    match (prev_last, curr_first, curr_first_syl) {
                        (Some(a), Some(b), Some(syl)) => {
                            let pair_a = a.to_string();
                            let pair_b = b.to_string();
                            matches!(
                                self.dict.collocation_freq(&pair_a, &pair_b, syl),
                                Some(f) if f >= BOUNDARY_COLLOCATION_FREQ_FLOOR
                            )
                        }
                        _ => false,
                    }
                })
                .count() as u32;
            const BIGRAM_SUPPORT_THRESHOLD: u32 = 3;
            let has_strong_bigram_support =
                bigram_support + boundary_support >= BIGRAM_SUPPORT_THRESHOLD;
            if !long_input && !has_strong_bigram_support && multi_word_segments < single_segments {
                continue;
            }
            all_paths.push((words, pinyin_parts, word_count, max_freq, path_score, word_freqs));
        }

        // 双向词格重打分：N-best 路径的后向（右文）
        // 证据并进路径分——同一份 seed_bigram 计数换反向归一化视角，
        // P(left|right)=count/next_total(right)，详见 engine/backward.rs。
        // 只重排已收集路径：不进 DP、不动门槛与分层排序（守护面不变）；
        // 仅全拼（简拼保守处理）；A/B 开关供评测对照。
        if !is_initial_mode && backward_ab::enabled() {
            for path in &mut all_paths {
                let delta = self.backward_path_delta(&path.0, &path.5);
                path.4 += delta;
            }
        }

        // 按 score 降序排，取 Top-K 绿词候选，去重文本。
        // 设计哲学：
        // - Top-1 思路：一个准确的首绿词优于多个噪音候选。
        // - Top-2 + 动态门槛：输出 2 个绿词候选（让用户在候选框看到备选），
        //   但第二绿词须过动态门槛（`threshold_ratio`），否则只保留第一绿词。
        //   门槛比例由 `green_accept.threshold_ratio(first_score)` 决定：
        //     动态阈值（按第一绿词绝对分档）+ 采纳率微调（样本 ≥ 20 才介入）。
        //   这让第二绿词「值得时才出现」——避免无脑 Top-2 噪音，又给合理备选机会。
        // DP 内部仍用 TOP_N=3 保证精度，最终输出截断到 1-2 个。
        all_paths.sort_by_key(|x| std::cmp::Reverse(x.4));
        // AI 切分裁决路径收集——在绿词截断（green_candidate_count）
        // 与第二绿词动态门槛之前取 top-K。门槛外被截掉的第 3-8 名正是 AI 可能
        // 提升的对象（纯简拼长串 top-2 可能是垃圾切分、正确句排在第 5 位）。
        // 文本去重保最高分（同文本不同分段保留首个 = 分最高者）。paths_limit==0
        // 时整段跳过（收集只用 & 借用，不影响下方消费）。
        let mut paths: Vec<SentencePath> = Vec::new();
        if paths_limit > 0 {
            let mut seen_pt: std::collections::HashSet<String> = std::collections::HashSet::new();
            for (words, _p, _wc, _mf, score, _fs) in &all_paths {
                if paths.len() >= paths_limit {
                    break;
                }
                let text = words.concat();
                if text.is_empty() || !seen_pt.insert(text.clone()) {
                    continue;
                }
                paths.push(SentencePath {
                    text,
                    words: words.clone(),
                    score: *score,
                });
            }
        }
        // 长句绿词条数（green_candidate_count 1..=5）：第 2 条起受第二绿词动态门槛约束
        //（下方循环内 threshold 判定，分差超容忍即 break，不逐条放宽）。
        // AI 候选（玫红）独立一位、不受此限（有 aiEnabled 总闸）。
        let max_green: usize = self.green_candidate_count as usize;
        let mut seen_texts: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut result: Vec<Candidate> = Vec::new();
        // 第一绿词的路径分（用于第二绿词门槛判定，按下文循环首次 push 后填入）。
        let mut first_path_score: Option<i32> = None;
        for (words, pinyin_parts, _wc, max_freq, path_score, _fs) in all_paths {
            if result.len() >= max_green {
                break;
            }
            // 第二绿词门槛（动态阈值 + 采纳率学习）：
            // 第二绿词与第一绿词的分数差距超容忍度 → 视为噪音丢弃（后续更差，全 break）。
            // 容忍度 = |first| × (1 - ratio)，最低 4 分：
            // 直接用 threshold = first × ratio 只对正 first 有「容忍 14% 差距」语义，
            // 负 first 时 -1×0.86=-0.86 > -1，门槛比第一名还高——第二绿词永远不可达
            //（wohaoxiangchifan：-1 vs -3 的合理双候选会被硬筛）。最低 4 分 ≈
            // 1.3 bit ≈ 词频差 2.5 倍——弱先验场景两个都合理的句子应同时可见，
            // 用户选一次即由行为数据收敛。
            if !result.is_empty() {
                if let Some(first) = first_path_score {
                    let ratio = self.green_accept.threshold_ratio(first);
                    let tolerance = ((first.unsigned_abs() as f64 * (1.0 - ratio)) as i32).max(4);
                    if path_score < first - tolerance {
                        break;
                    }
                }
            }
            let sentence = words.concat();
            if sentence.is_empty() || seen_texts.contains(&sentence) {
                continue;
            }
            seen_texts.insert(sentence.clone());
            if result.is_empty() {
                first_path_score = Some(path_score);
            }
            result.push(Candidate {
                text: sentence.into(),
                base_freq: max_freq,
                pinyin: pinyin_parts.join(" ").into(),
                is_primary: true,
                is_correction: false,
                is_sentence: true,
                is_guess: false,
                is_shortcut: false,
                is_english: false,
                // DP 路径分进候选层：同分绿词按此降序
                sentence_score: path_score,
            });
        }
        (result, paths)
    }
}

/// 测试钩子：切换长程语境证据开关（A/B 对照）。生产构建无此函数。
#[cfg(test)]
pub(crate) fn set_longrange_enabled_for_test(on: bool) {
    longrange_ab::set(on);
}

/// 测试钩子：切换双向词格重打分开关（A/B 对照）。生产构建无此函数。
#[cfg(test)]
pub(crate) fn set_backward_enabled_for_test(on: bool) {
    backward_ab::set(on);
}

/// A/B 对照开关：仅测试构建可关闭长程语境证据，
/// 用于在同一份代码/同一 DB 上复现关闭态的行为。生产恒 true。
///
/// 线程局部而非全局原子：cargo test 默认并行跑测试，若用全局标志，
/// A/B 测试置 false 的瞬间会让其它测试线程看到「关闭态」→ 偶发假红。
/// 线程局部让每个测试线程各自持有开关，默认 true，互不可见。
#[cfg(test)]
mod longrange_ab {
    use std::cell::Cell;
    thread_local! {
        static ENABLED: Cell<bool> = const { Cell::new(true) };
    }
    pub(super) fn enabled() -> bool {
        ENABLED.with(|c| c.get())
    }
    pub(super) fn set(on: bool) {
        ENABLED.with(|c| c.set(on));
    }
}
#[cfg(not(test))]
mod longrange_ab {
    #[inline]
    pub(super) fn enabled() -> bool {
        true
    }
}

/// A/B 对照开关（双向词格）：与 longrange_ab 同款线程局部
/// 开关——测试构建可关闭后向重打分复现关闭态的行为，生产恒 true。
#[cfg(test)]
mod backward_ab {
    use std::cell::Cell;
    thread_local! {
        static ENABLED: Cell<bool> = const { Cell::new(true) };
    }
    pub(super) fn enabled() -> bool {
        ENABLED.with(|c| c.get())
    }
    pub(super) fn set(on: bool) {
        ENABLED.with(|c| c.set(on));
    }
}
#[cfg(not(test))]
mod backward_ab {
    #[inline]
    pub(super) fn enabled() -> bool {
        true
    }
}
