//! 打分方法（从 engine/mod.rs 抽出）。

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::dict::Candidate;
use crate::dict::SmallStr;
use crate::engine::ScoreBreakdown;

#[allow(unused_imports)]
use super::char_commonness;

/// 候选排序键（键预计算排序）。
///
/// 字段声明顺序即比较优先级（派生 Ord 按字段序逐层比较，全升序；
/// 降序字段用 Reverse 包装）。与 candidate_cmp 的判定层一一对应：
/// tier（词/字 + 完整/前缀分层）→ pos（单字 fallback 音节位置）→
/// 第3层 collocation → 第3.5层强 bigram → 打分（含主音加成）→
/// 单字常用度 → 字数 → 拼音/文本确定性全序。
/// 语义详见 Engine::candidate_sort_key 注释。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CandidateSortKey {
    tier: u8,
    pos: usize,
    no_collocation: bool,
    no_strong_bigram: bool,
    score: core::cmp::Reverse<u64>,
    sentence_score: core::cmp::Reverse<i32>,
    commonness: core::cmp::Reverse<u32>,
    len_key: i64,
    pinyin: SmallStr<48>,
    text: SmallStr<24>,
}

impl crate::engine::Engine {
    /// 窗口成词 freq（惩罚豁免/tie-break 事实核查用）：
    /// 给定一段音节序列，查词典（含用户词）返回最高 base_freq 的多字词频；
    /// 无词命中返回 0。只看词级查询（单字由 query 自然返回），调用方用它比较
    /// 「同一字母窗口在两种切法下的成词能力」。
    /// 单次查询内按段序列 key 缓存（同 score_cache 生命周期，见字段注释）——
    /// 同一歧义窗口在最多 16 个切分的打分里重复出现。
    pub(crate) fn window_word_freq(&self, seg: &[&str]) -> u32 {
        // key 必须含段边界（join 空格）：concat 会让 "fang"+"an" 与 "fan"+"gan"
        // 碰撞成同一 key "fangan"，两种读法互相污染缓存 → 豁免比较失效
        //（test_jishufanganwendang_prefers_fang_an 守护抓到的真实回归）。
        let key = seg.join(" ");
        if let Some(v) = self.window_freq_cache.borrow().get(key.as_str()) {
            return *v;
        }
        let v = self
            .dict
            .query(seg)
            .iter()
            .map(|c| c.base_freq)
            .max()
            .unwrap_or(0);
        self.window_freq_cache.borrow_mut().insert(key, v);
        v
    }

    /// 同分同长切分的逐侧成词强度键（替代成对
    /// first_divergence_window_cmp）：该切分自身的所有连续 2..=3 音节跨度中，
    /// 词库（含用户词）能命中的最高 base_freq（复用 window_word_freq 的
    /// 逐查询缓存）。
    ///
    /// 语义保持：成对窗口比较在首分歧窗口比两侧成词频次——
    /// fang+an(方案 3455) vs fan+gan(反感 997)、sang+e 无词 vs san+ge 有词
    /// 的判例结果都由本键复现（公共前后缀跨度两侧同值相消，分歧跨度分高者
    /// 胜）。改逐侧键的动因：成对比较的窗口随对变化（A/B、B/C、A/C 各自首分歧窗口
    /// 不同），三切分可成环 → sort 全序违规 panic（172k 语料 learn 实测，
    /// 生产等价用户积累统计后打字崩服务）；本键为纯逐侧函数，严格全序。
    /// k=1 单字跨度不参与：单字频次是结构噪音（防/反这类单字高频会淹没
    /// 方案/反感的词级分歧），与原窗口 ≥2 音节的主流形态一致。
    pub(crate) fn max_span_word_freq(&self, seg: &[&str]) -> u32 {
        // 逐段 memo（"sp:" 前缀隔离键空间，与 window_freq_cache 同生命周期）：
        // 比较器在退化枚举（混合输入 cap 扩到 512 段）里被调 O(m·log m) 次，
        // 不 memo 时每次现算 3n 个窗口查表 → shishierfe 实测 17ms/查询超
        // bench 时限；memo 后每段每查询只算一次。
        let mut key = String::with_capacity(seg.len() * 6 + 3);
        key.push_str("sp:");
        for (i, s) in seg.iter().enumerate() {
            if i > 0 {
                key.push(' ');
            }
            key.push_str(s);
        }
        if let Some(v) = self.window_freq_cache.borrow().get(key.as_str()) {
            return *v;
        }
        let mut best = 0u32;
        for k in 2..=3usize {
            if seg.len() < k {
                break;
            }
            for i in 0..=seg.len() - k {
                let f = self.window_word_freq(&seg[i..i + k]);
                if f > best {
                    best = f;
                }
            }
        }
        self.window_freq_cache.borrow_mut().insert(key, best);
        best
    }

    /// 给一个音节切分方案打分，用于 `best_segmentation` 选最优切分。
    ///
    /// 打分由两部分取较大者，再施加拆分惩罚：
    /// 1. 整词命中分：词级候选（含连续前缀词）中的最高 base_freq。
    ///    单字补充不计分，避免 "ken+eng" 靠单字频率反超 "ke+neng"。
    /// 2. 可分词质量分（`segmentation_coverage_score`）：
    ///    评估切分能被词典多字词覆盖的程度。应对长句切分歧义——
    ///    `neng|jie|jue|wen|ti` 整序列无词命中（得 0），但 jie|jue 命中「解决」，
    ///    可分词分让正确长句切分能被选中，交给 `decode_sentence` DP 拆词。
    ///
    /// 额外施加「完整音节拆分惩罚」：无显式分隔符时，若相邻两音节能合并成
    /// 一个更长的有效音节（如 du+an → duan），说明该切分把一个完整音节拆开，
    /// 应大幅降权，避免 duan 被拆成 du+an 而优先给出「都安」。
    ///
    /// pub 供离线调试工具的切分选举诊断（seg 子命令）：与引擎内部同一打分
    /// 口径复现「枚举 → 打分 → 选举」全链路，免真机打字复现切分类问题。
    /// 切分的「词跨度数」：序列中能组成多字词（字数≥2）的连续 2~3 音节段个数
    /// （含重叠）。
    ///
    /// 用途两处（单一来源，防口径漂移）：
    /// 1. `score_for_syllables` 的层内小额加分（平局裁决）；
    /// 2. 整句 DP 的仲裁触发（当选读法 1 个跨度
    ///    「首钢」vs 备选 2 个跨度「手感/给力」——一个高频词压过更自然的词组链，
    ///    此时才值得让 DP 用真实语言证据裁决）。
    pub(crate) fn word_span_count(&self, syllables: &[&str]) -> u32 {
        let mut count: u32 = 0;
        for k in 2..=3usize {
            for i in 0..syllables.len().saturating_sub(k - 1) {
                let seg = &syllables[i..i + k];
                if seg.iter().any(|s| !crate::pinyin::is_valid_syllable_pub(s)) {
                    continue;
                }
                if self
                    .dict
                    .query(seg)
                    .iter()
                    .any(|c| c.text.chars().count() >= 2)
                {
                    count += 1;
                }
            }
        }
        count
    }

    pub fn score_for_syllables(&self, input: &str, syllables: &[&str]) -> u32 {
        // 全是不完整音节（单字符）的切分返回 0 分，
        // 避免它们靠单字频率胜过含完整音节的切分。
        let has_any_complete = syllables
            .iter()
            .any(|s| crate::pinyin::is_valid_syllable_pub(s));
        if !has_any_complete {
            return 0;
        }

        // 打分用候选：
        // 优先取「字数==音节数」的精确匹配词 freq，不计前缀词（字数<音节数）。
        // 防止 xiaoguo 的 [xiao,gu,o] 切分靠前缀匹配的「小姑」(xiao gu, 字数2 < 音节数3)
        // 高 freq 压过 [xiao,guo] 的精确匹配「效果」(字数2==音节数2)。
        let cands = self.word_candidates_for_syllables(input, syllables);
        let syl_count = syllables.len();
        // 精确匹配词（字数==音节数 且 拼音逐音节精确等于输入音节，含单字词）。
        // 必须验证拼音——只看字数会把 [s,hu,ru] 切分的「撒欢儿」(sa huan r, 3字==3音节)
        // 误判精确匹配，但 s≠sa/hu≠huan/ru≠r，实际是前缀匹配——
        // 因此必须逐音节验证拼音，不能只 filter 字数==音节数。
        let exact_multi_freq = cands
            .iter()
            .filter(|c| {
                if c.text.chars().count() != syl_count {
                    return false;
                }
                // 拼音逐音节比较（候选拼音 vs 输入音节）
                let cand_syls: Vec<&str> = c.pinyin.split_whitespace().collect();
                if cand_syls.len() != syllables.len() {
                    return false;
                }
                cand_syls
                    .iter()
                    .zip(syllables.iter())
                    .all(|(cs, is)| *cs == *is)
            })
            .map(|c| c.base_freq)
            .max();
        // 兜底：单字（字数==1）+ 前缀词（字数>=2 且 < 音节数）的最高 freq。
        // 前缀词参与兜底是为了 nihsou 等拼错输入的切分能有非零分（依赖前缀匹配的候选）。
        // 但当存在精确匹配词时，优先用精确匹配（不让前缀词的高 freq 干扰）。
        //
        // 覆盖门槛：前缀词覆盖的音节数必须
        // ≥ 一半，否则不参与兜底。原缺陷：[hou,xu,ang,u,ding] 碎片切分（xuan 被
        // 撕成 xu+ang+u，拆分惩罚只查相邻合并查不到跨三段重组）靠开头 2 音节的
        // 「后续」(freq 1755) 把 base 顶到 999 格顶（层级 2 = 1000+999 = 1999），
        // 压过覆盖全部输入的正确切分 [hou,xuan,gu,ding]（「候选」freq 808 →
        // 1809）——绿词 DP 收到错误切分，整句出不来，首候选变「后续」。
        // 低覆盖前缀词代表不了整条切分的质量，覆盖 < 一半不兜底；
        // exact_multi_freq（字数==音节数的精确匹配）不受影响。
        let fallback_freq = cands
            .iter()
            .filter(|c| c.text.chars().count() * 2 >= syl_count)
            .map(|c| c.base_freq)
            .max();
        let max_freq = exact_multi_freq.or(fallback_freq).unwrap_or(0);

        // 可分词质量分（应对 nengjiejuewenti 类长句切分歧义）：
        //
        // 背景：旧版只用「整序列是否构成单个词」打分。但长句切分（如 neng|jie|jue|wen|ti）
        // 根本没有 5 音节的整词，导致正确切分得 0 分，被前缀命中整词的错误切分
        // （neng|ji|e|jue 反推「能级」）压过——最终绿词 DP 收到错误切分，拼出「能继而决问题」。
        //
        // 修复：评估切分能被词典覆盖的程度——相邻 2~3 音节段能否构成多字词。
        // 正确切分 neng|jie|jue|... 里 jie|jue 命中「解决」(2266)，可分词分显著；
        // 错误切分 neng|ji|e|jue|... 里 ji|e/e|jue 都查不到词，可分词分低。
        // 这让 best_segmentation 能选中正确切分，再交给 decode_sentence DP 拆词。
        let cover_score = self.segmentation_coverage_score(syllables);

        // 全有效音节基础分：
        // 切分全是有效音节（无单字符拆分）时，即使没词命中也给予基础分，
        // 让它胜过有拆分但靠前缀词得分的切分。
        // 例：biegen 的 [bie,gen]（全有效，无词命中）应胜过 [bi,e,gen]（拆 bie，靠「避恶」得分）。
        // genbu 的 [gen,bu]（全有效，无词命中）应胜过 [ge,n,bu]（拆 gen，靠「歌女」得分）。
        // all_valid_syllables 不再单独使用（层级化打分改用 penalty_count==0）

        if max_freq == 0 && cover_score == 0 {
            // 无词命中且无可分词覆盖：后面 penalty 计算后判定层级
            // 暂时记下，在 penalty 计算后统一处理
        }

        let mut penalty_count = 0u32;
        for i in 0..syllables.len().saturating_sub(1) {
            // 显式守卫：syllables[i] 正常是 input 的子切片，
            // 但双拼等 owned 音节场景指针不在 input 分配内，差值无意义。
            // 先验证在 input 范围内再算偏移；否则 end 置 usize::MAX 让下方
            // .get() 必然返回 None（= 无分隔符信息，走正常惩罚判定）。
            let s_addr = syllables[i].as_ptr() as usize;
            let in_addr = input.as_ptr() as usize;
            let in_range =
                s_addr >= in_addr && s_addr + syllables[i].len() <= in_addr + input.len();
            let end = if in_range {
                s_addr - in_addr + syllables[i].len()
            } else {
                usize::MAX
            };
            // 显式分隔符 ' 允许拆分，不惩罚
            if input.as_bytes().get(end) == Some(&b'\'') {
                continue;
            }
            let joined = format!("{}{}", syllables[i], syllables[i + 1]);
            if crate::pinyin::is_valid_syllable_pub(&joined) {
                // 拆分惩罚：当前音节+下一音节能合并成完整音节（如 li+ao→liao、
                // du+an→duan），应惩罚这种"拆开完整音节"的切分。
                // 不豁免：完整音节被拆开几乎总是
                // 错（用户想打 duan，拆 du+an 只能靠罕见词/用户词续命）——守护
                // 测试 test_duan_prefers_full_syllable 锁定此语义；词频豁免仅适用
                // 下方的贪婪合并惩罚（两种读法都是拆分形态的真歧义）。
                penalty_count += 1;
            }
            // 贪婪合并惩罚：当前音节去掉末尾字符后仍是有效音节，
            // 且末尾字符+下一音节也构成有效音节，说明存在更短的切分。
            // 例如 sang+e：sang 去掉 g → san(有效)，g+e → ge(有效)，
            // 存在更短切分 san+ge，应惩罚 sang+e 让 san+ge 优先。
            // 同款窗口词频豁免：fang+an 触发本惩罚（fan+gan 替代），
            // 但「方案」(3455) > 「反感」(997)——长句无整词兜底时被 ÷100 压死，
            // 绿词 DP 收到 fan+gan 拼出「技术反感文档」。词频高的一侧免罚；
            // sang+e 无词 < san+ge「三个」，照罚（原行为保持）。
            if syllables[i].len() > 1 {
                let split_pos = syllables[i].len() - 1;
                let shorter = &syllables[i][..split_pos];
                let last_char = &syllables[i][split_pos..];
                if crate::pinyin::is_valid_syllable_pub(shorter) {
                    let alt_next = format!("{}{}", last_char, syllables[i + 1]);
                    if crate::pinyin::is_valid_syllable_pub(&alt_next)
                        && self.window_word_freq(&[syllables[i], syllables[i + 1]])
                            < self.window_word_freq(&[shorter, alt_next.as_str()])
                    {
                        penalty_count += 1;
                    }
                }
            }
        }
        // 跨 token 重建惩罚（jiaoleng→「浇了能够」垃圾绿词，
        // 与 bushijiaolengshui「不是交了那个谁」同族）：既有拆分惩罚只查
        // 「相邻两 token 能合并」（du+an→duan 的 k=2 情形）——但碎片切分可以
        // 把一个合法音节拆成 3+ 段（leng→le|n|g），单看相邻对 le+n/n+g 都
        // 拼不回，惩罚漏网。漏网碎链的双重恶果：① 借 交了/浇了 的前缀词频
        // 把选举分顶过正确全音节读法（jiaoleng 实测 1999:1295）；② 把 2 音节
        // 输入吹成 4 token，跨过整句 DP 的 ≥3 音节门槛，I(n)+I(g) 被当简拼
        // 首字母命中「能够」→ 拼出「浇了能够」。
        //
        // 判定：对每个「单字符且非合法音节」的游离首字母位，以其为中点向两侧
        // 扩窗（窗口 ≤3 token、拼接 ≤6 字节 = 最长音节 chuang），若窗口内
        // token 拼接恰好是一个完整合法音节 → 该碎片拆掉了用户没打算拆的音节，
        // 每重建出一个音节罚一次（并入 100^penalty_count 同一除数体系）。
        // 合法简拼意图零误伤：nixianggm 的 g/m、zhonggrm 的 g/r/m 扩窗拼不回
        // 任何合法音节，不产生惩罚；k=2 相邻合并已由上方循环处理，这里只查
        // ≥3 token 的窗口，两规则不重复计数。
        {
            let n_tok = syllables.len();
            let mut reconstructed: std::collections::HashSet<(usize, usize)> =
                std::collections::HashSet::new();
            for (i, s) in syllables.iter().enumerate() {
                if s.len() != 1 || crate::pinyin::is_valid_syllable_pub(s) {
                    continue; // 只看游离首字母位（a/e/o 等单字符合法音节不算）
                }
                let lo = i.saturating_sub(2);
                let hi = (i + 2).min(n_tok - 1);
                for st in lo..=i {
                    for en in (i + 1)..=hi {
                        if en - st + 1 < 3 {
                            continue; // 相邻对已由上方既有惩罚覆盖
                        }
                        let joined: String = syllables[st..=en].concat();
                        if joined.len() > 6 {
                            break; // 音节最长 6 字母（chuang），en 再扩只会更长
                        }
                        if crate::pinyin::is_valid_syllable_pub(&joined)
                            && reconstructed.insert((st, en))
                        {
                            penalty_count += 1;
                        }
                    }
                }
            }
        }
        // 中位游离首字母惩罚（zhenbena→「真笨啊」场景）：单字符
        // 非音节 token 前后都存在完整音节（≥2 字母有效）= 碎片化标志——
        // [zhen,b,e,na] 的 b 夹在 zhen 与 na 之间，靠纠错候选词频（真版→层2
        // 999）压过正解 [zhen,ben,a]（真笨 仅 100），整句绿词无从谈起。
        // 位置判据天然豁免三类合法形态：简拼头（zgrmjfjxa 的 z/g/r/m 前方无
        // 完整音节）、简拼尾（zhonggr 的 g/r 后方无完整音节）、单字符合法
        // 音节（a/e/o 本身有效不适用本规则）。与跨 token 重建惩罚互补：
        // 重建规则治「拼得回音节」的碎链（le|n|g），本规则治「拼不回但
        // 位置暴露」的碎字母（b|e 拼不成 be）。
        {
            let has_full_before = |idx: usize| {
                syllables[..idx]
                    .iter()
                    .any(|t| t.len() >= 2 && crate::pinyin::is_valid_syllable_pub(t))
            };
            let has_full_after = |idx: usize| {
                syllables[idx + 1..]
                    .iter()
                    .any(|t| t.len() >= 2 && crate::pinyin::is_valid_syllable_pub(t))
            };
            for (i, s) in syllables.iter().enumerate() {
                if s.len() == 1
                    && !crate::pinyin::is_valid_syllable_pub(s)
                    && has_full_before(i)
                    && has_full_after(i)
                {
                    penalty_count += 1;
                }
            }
        }
        // 惩罚计算：
        //
        // 设计目标（两全）：
        // 1. 拆分惩罚要强：保护 duan 完整音节不被 du+an 反超（du+an 有用户词「都安」）
        // 2. 不能归零低频词：「奥利奥」freq=916 因 li+ao→liao 触发惩罚，旧版 /1000 归零
        //
        // 旧版：divisor = 1000^penalty_count，分数 = max_freq / divisor。
        // 问题：916/1000=0，与查不到词的切分（0分）同分，tie-break 丢失「奥利奥」。
        //
        // 新版：分数 = (max_freq + 1) / 100^penalty_count。
        // - max_freq+1 保证有词命中的切分至少得 1 分（永远 > 查不到词的 0 分）
        // - 100^penalty_count 仍是强惩罚（1惩罚/100，2惩罚/10000），保护 duan 不被 du+an 反超
        //   （「都安」典型 freq~50，(50+1)/100=0 < duan 单字分，稳定保护）
        // - 低频词「奥利奥」(916+1)/100=9，远 > 0，ao|li|ao 稳定胜出 ao|liao
        //
        // 长句可分词分（cover_score）：
        // max_freq 为 0（整序列无词命中）但 cover_score > 0（能拆成子词）时，
        // 用 cover_score 作为基础分，让正确长句切分（如 neng|jie|jue|wen|ti）能被选中，
        // 交给 decode_sentence DP 进一步拆词。两者取较大者再施加同样的拆分惩罚。
        // 词跨度丰富度（平局裁决，nizaiganmane→你在干慢而 案例）：
        // 统计切分序列中「能成词的连续 2~3 音节跨度」个数（含重叠）。词频平手时
        // 它是决定性信号：[ma,ne] 侧 你在/干嘛/干嘛呢/嘛呢 4 个词跨度 vs
        // [man,e] 侧 你在/满额 2 个——错误切分在词典里几乎组不出词，而对的切分
        // 词跨度丰富（语言真实统计）。裸 ganmane 靠整词精确匹配胜出（层级 3），
        // 但带前缀后精确证据降级为子串、两侧词频平手（干嘛呢 253 == 满额 253），
        // 只剩词跨度能裁决。作为层内小额加分（不跨层级），历史案例（duan/
        // xiaoguo/shuru/houxuanguding/奥利奥 等）由层级/拆分惩罚裁决，不受影响。
        let word_span_count = self.word_span_count(syllables);
        let divisor = 100u32.saturating_pow(penalty_count.min(4)).max(1);
        let base = if max_freq > 0 { max_freq + 1 } else { 0 };
        // 可分词分归一到与 max_freq 同量级（cover_score 内部已用对数压缩，直接 +1 防归零）
        let cover = if cover_score > 0 { cover_score + 1 } else { 0 };
        let combined = base.max(cover);
        let base_score = combined / divisor + word_span_count.min(999); // 层级 1：0-999 范围

        // 层级化打分（替代 500K/1M 大数）：
        // 层级 3（精确匹配）：2000 + base_score（原 1M+）
        // 层级 2（无拆分惩罚）：1000 + base_score（原 500K+）
        // 层级 1（有拆分）：base_score（原 freq 量级）
        //
        // 层级 2 覆盖"无词命中但无拆分"的早期 return 场景（原 500K 基础分）
        if exact_multi_freq.is_some() {
            2000 + base_score.min(999) // 层级 3：精确匹配优先
        } else if penalty_count == 0 {
            1000 + base_score.min(999) // 层级 2：无拆分次优
        } else {
            // 无词命中且有拆分 → 0（早期 return 的等效处理）
            if base_score == 0 {
                0
            } else {
                base_score.min(999)
            } // 层级 1
        }
    }

    /// 评估一个音节切分能被词典多字词覆盖的程度（可分词质量分）。
    ///
    /// 用贪心扫描：从位置 i 开始，尝试最长的 2~3 音节段能否在词典构成多字词（字数≥2）。
    /// 命中则该段得 log2(freq) 分（对数压缩，避免高频段碾压），跳过该段继续。
    /// 不命中则单音节前进（不加分）。
    ///
    /// 这是 `decode_sentence` DP 的轻量近似——DP 会做完整的多词切分重排，
    /// 但 `score_for_syllables` 只需快速判断「这个切分值不值得送进 DP」，
    /// 不必精确求最优路径，故用贪心 + 对数分即可。
    ///
    /// 返回值：所有可成词段的对数分之和（典型 0~40）。
    pub(crate) fn segmentation_coverage_score(&self, syllables: &[&str]) -> u32 {
        let n = syllables.len();
        if n < 2 {
            return 0;
        }
        let mut i = 0;
        let mut total: u32 = 0;
        while i < n {
            // 优先尝试 3 音节段，再 2 音节段（贪心取最长可成词段）
            let mut matched_k = 0;
            for k in (2..=3).rev() {
                if i + k > n {
                    continue;
                }
                let seg = &syllables[i..i + k];
                // 段内必须全是有效音节，否则 query 会返回单字噪音
                if seg.iter().any(|s| !crate::pinyin::is_valid_syllable_pub(s)) {
                    continue;
                }
                let cands = self.dict.query(seg);
                // 取该段最高频的多字词（字数≥2）作为成词证据
                if let Some(best_freq) = cands
                    .iter()
                    .filter(|c| c.text.chars().count() >= 2)
                    .map(|c| c.base_freq)
                    .max()
                {
                    // 对数压缩：freq 100→7，1000→10，10000→13。高频段不碾压低频段。
                    let log_score = (best_freq as f64).log2().round() as u32;
                    total = total.saturating_add(log_score);
                    matched_k = k;
                    break;
                }
            }
            i += if matched_k > 0 { matched_k } else { 1 };
        }
        total
    }

    /// 结合词典为输入选择最优音节切分
    pub(crate) fn best_segmentation<'a>(&self, input: &'a str) -> Vec<&'a str> {
        if crate::pinyin::is_jianpin(input) {
            // 简拼每个字符视为一个音节
            return input
                .char_indices()
                .map(|(i, c)| &input[i..i + c.len_utf8()])
                .collect();
        }
        let (best, _) = self.pick_segmentation(input, &[8, 128, 512], false);
        best
    }

    /// 切分择优比较器——候选链与显示/消费链同源共用。
    ///
    /// 语义 = 原 process_inner best_idx 的 max_by 比较器逐字节搬移：
    /// 分数高优先 → 同分全有效音节切分优先 → 同分同音节数时逐侧成词强度
    /// 裁决（`max_span_word_freq`，fang+an vs fan+gan 类真歧义）→ 音节数少
    /// 优先 → 首音节短优先。
    ///
    /// 全序保证（172k 语料 learn 实测 sort panic 修复）：
    /// 同分分支的每一层都是纯逐侧函数（all_valid / 证据键 / 成词强度键），
    /// 尾部为固定结构链（分→长度→首音节长）——整个比较器严格全序。历史上
    /// 两层成对裁决（选择史 20× 守卫、首分歧窗口）可三元成环，真实用户
    /// 积累选择史后打字即可触发全序违规 panic（服务进程崩溃）。
    ///
    /// 背景（wangou 案例）：显示/消费链（`pick_segmentation`）此前同分时
    /// 「保留先枚举到的」——DFS 长音节优先把 wang|ou 排在 wan|gou 前，平分
    /// 即定；候选链却走上述精细裁决选 wan|gou。两边切分不同 → 预编辑显示
    /// wang'ou、候选却是「wan」音字；选「弯」后 consumed 按显示切分的前 n
    /// 音节偏移吞掉 g、残留 ou。本比较器让 pick_segmentation 与 process_inner
    /// 裁决同源，显示/消费/候选三者对同一输入恒选同一切分。
    pub(crate) fn seg_order_cmp(
        &self,
        input: &str,
        a: &[&str],
        sa: u32,
        b: &[&str],
        sb: u32,
    ) -> Ordering {
        if sa == sb {
            // 同分时：全有效音节切分优先于含不完整音节的切分
            let a_all_valid = a.iter().all(|s| crate::pinyin::is_valid_syllable_pub(s));
            let b_all_valid = b.iter().all(|s| crate::pinyin::is_valid_syllable_pub(s));
            if a_all_valid != b_all_valid {
                return if a_all_valid {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            // 两层裁决只对双方全有效音节的真歧义开放（判例全是全音节场景：
            // fangan / jishufanganwendang / kanta 大致）。悬尾/单字母混合段
            //（shishierfe 类 125 候选热路径）免付两层查表成本——旧实现在
            // 这些段上两层也几乎恒 None（initial 段成词查询必 miss），行为等价
            //（perf 约束：该输入基线 9.8ms 贴 10ms 线）。
            if a_all_valid && b_all_valid {
                // 读法偏好裁决：上文 bigram 证据 >
                // 用户选择史。无证据/打平返回 None，成词强度裁决接管。
                if let Some(ord) = self.reading_preference_cmp(input, a, b) {
                    return ord;
                }
                if a.len() == b.len() {
                    // 同分同长度时：逐侧成词强度键裁决（词频裁决真歧义）。
                    // 成对首分歧窗口比较的窗口随对
                    // 变化可成环 → 改为纯逐侧键，见
                    // max_span_word_freq 注释。
                    let fa = self.max_span_word_freq(a);
                    let fb = self.max_span_word_freq(b);
                    if fa != fb {
                        return fa.cmp(&fb);
                    }
                }
            }
        }
        sa.cmp(&sb)
            .then_with(|| a.len().cmp(&b.len()).reverse())
            .then_with(|| {
                // 之后才轮到旧的贪婪合并 tie-break（第一个音节更短者优先）
                let a0 = a.first().map_or(0, |s| s.len());
                let b0 = b.first().map_or(0, |s| s.len());
                b0.cmp(&a0)
            })
    }

    /// 读法偏好裁决（含全序修复）：
    /// 同分真歧义时按证据选读法，两层证据按优先级：
    ///
    /// ① 上文 bigram（commit「十分」后输入 fangan 应读 fan'gan）：
    ///    比较「上下文末词 × 各读法首词」的泛化关联强度
    ///    （word_bigram_count_generalized，含等价类成员行聚合——种子只有成员行
    ///    很→反感=10、类键行从未物化），有证据且不等 → 证据强的读法胜。
    /// ② 用户选择史（选过 反感 30 次后打 fangan 仍出方案——
    ///    选举 2999:2999 平局、tie-break 只看词库存频）：比较 input_mappings
    ///    里「本输入 × 各读法首词」的选择次数。谱地板门控：词自身
    ///    freq≥500（谱中位）才计选择史（个人噪音不翻盘结构性高频）；
    ///    首次即生效（与 green_memo「首次即记忆」同哲学）、改选另一读法
    ///    一次即拉平（自愈对称）。
    ///
    /// 全序修复（172k 语料 learn 实测 panic）：早期实现②用
    /// 成对 20× 词频守卫（A/B、B/C、A/C 三对各自判定），叠加①②层间条件
    /// 激活——三切分可成环（A>B>C>A），sort_by 检测全序违规直接 panic；真实
    /// 用户积累 input_mappings 后打字即可触发（服务进程崩溃）。改为逐侧
    /// 绝对谱地板门控，证据键 = 纯逐侧函数 (bigram, counts)，严格全序；
    /// 两轮裁决案例（反感/方案 同谱段，两侧都过地板）行为不变。
    ///
    /// 首词 = 该切分从位置 0 起、字数==音节数的最长可成词 span（k≤3，span 须
    /// 全为完整音节，简拼/悬尾位不参与）。例：fangan 的 [fang,an]→方案 vs
    /// [fan,gan]→反感；两个方向都由证据说话，不是单边偏置。
    ///
    /// 调用面：seg_order_cmp 的同分分支——显示（预编辑）/消费/候选三链同源
    /// 生效（切分同源不变量），且只在选举打平的真歧义上介入。
    /// pub(crate)：tests_seg_chain 全序回归测试直调。
    pub(crate) fn reading_preference_cmp(
        &self,
        input: &str,
        a: &[&str],
        b: &[&str],
    ) -> Option<Ordering> {
        // 首个词级代表：最长 span 优先（k=3→1），要求 span 全为完整音节
        let first_word = |seg: &[&str]| -> Option<String> {
            let max_k = 3.min(seg.len());
            for k in (1..=max_k).rev() {
                if !seg[..k]
                    .iter()
                    .all(|s| s.len() >= 2 && crate::pinyin::is_valid_syllable_pub(s))
                {
                    continue;
                }
                if let Some(c) = self
                    .dict
                    .query(&seg[..k])
                    .iter()
                    .filter(|c| c.text.chars().count() == k)
                    .max_by_key(|c| c.base_freq)
                {
                    return Some(c.text.to_string());
                }
            }
            None
        };
        /// 选择史证据的词频谱地板（低于此谱位的
        /// 个人选择噪音不计证据；500 ≈ 种子谱中位，常用内容词均在此上）。
        const READING_PREF_FREQ_FLOOR: u32 = 500;
        let evidence_key = |seg: &[&str]| -> Option<(u32, u32)> {
            let w = first_word(seg)?;
            let ev = match self.context.last() {
                Some(prev) if !prev.is_empty() => {
                    self.freq.word_bigram_count_generalized(prev, &w)
                }
                _ => 0,
            };
            // ct=0（冷启动常态）时跳过 freq 查表——对齐旧代码的早退结构，
            // 比较器热路径（shishierfe 类 125 候选）零增费（perf 约束）。
            let ct = match self.input_choice_count(input, &w) {
                0 => 0,
                n => {
                    if self.dict.freq_for_text(&w).unwrap_or(0) >= READING_PREF_FREQ_FLOOR {
                        n
                    } else {
                        0
                    }
                }
            };
            Some((ev, ct))
        };
        let ka = evidence_key(a)?;
        let kb = evidence_key(b)?;
        if ka == kb {
            return None;
        }
        Some(ka.cmp(&kb))
    }

    /// 本输入下该词的用户选择次数（input_mappings 精确键；无则 0）。
    /// 键与 learning.rs record_input_mapping 写入侧完全一致（原始输入串）。
    fn input_choice_count(&self, input: &str, text: &str) -> u32 {
        self.input_mappings
            .get(&(input.to_string(), text.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// 切分枚举 + 打分择优（自适应扩枚举），`tokens` 选择枚举语义。
    ///
    /// 自适应扩枚举：
    /// DFS 先序枚举按「长音节优先」产出，名额（cap）会被退化子树占满——
    /// 实测 `yigerenguodehaob` 在 pos=4 的 reng 分支（ren+g 误并成 reng，
    /// 后续 u/o 靠单字符兜底续命）子树 ≥16 叶，把 cap 吃光，正解
    /// `ren|guo|de|hao|b`（手工打分 1381，垃圾态全 0 分）从未被枚举 →
    /// 候选层只出前缀词、绿词整句崩塌（显示 reng'u'o 乱切）。
    /// 策略：按 [小, 中, 大] 逐级扩大枚举上限，某级出现正分即停——正常输入
    /// 第一级即有正分（零额外开销）；仅「全 0 分退化态」付扩大成本，且大
    /// cap 只对短输入启用（防长输入 DFS 容积膨胀）。
    fn pick_segmentation<'a>(
        &self,
        input: &'a str,
        caps: &[usize],
        tokens: bool,
    ) -> (Vec<&'a str>, u32) {
        let mut best: Vec<&str> = Vec::new();
        let mut best_score = 0u32;
        // 打字进行中形态保底切分：「完整音节前缀 +
        // 尾部未拼完字母」合成变体先入局——含尾枚举的 DFS 名额会被贪长退化子树
        // 挤满（reng|o|n|g 一族把 ren|gong 正解挤出 cap），FULL_ONLY 对含尾输入
        // 又整体返回空，正解不入局则 best 全是垃圾切分（绿词消失根因）。合成变体
        // 与枚举变体同台打分竞争，正解分高自然胜出；与枚举变体重合时等价于无操作。
        for seg in crate::pinyin::segmentations_prefix_tail(input) {
            let score = self.score_for_syllables(input, &seg);
            // 择优用 seg_order_cmp（与候选链 process_inner 同源，见比较器注释）
            if best.is_empty()
                || self.seg_order_cmp(input, &seg, score, &best, best_score) == Ordering::Greater
            {
                best_score = score;
                best = seg;
            }
        }
        for &cap in caps {
            if cap > 16 && input.len() > 64 {
                break; // 长输入的扩大枚举成本不可控（全 0 分退化态罕见且本就无解）
            }
            let segs: Vec<Vec<&str>> = if tokens {
                crate::pinyin::segmentations_tokens(input, cap)
                    .into_iter()
                    .map(|seg| crate::pinyin::tokens_to_strs(input, &seg))
                    .collect()
            } else {
                crate::pinyin::segmentations(input, cap)
            };
            for seg in segs {
                let score = self.score_for_syllables(input, &seg);
                // 择优用 seg_order_cmp（与候选链 process_inner 同源，见比较器注释）
                if best.is_empty()
                    || self.seg_order_cmp(input, &seg, score, &best, best_score)
                        == Ordering::Greater
                {
                    best_score = score;
                    best = seg;
                }
            }
            if best_score > 0 {
                break;
            }
        }
        (best, best_score)
    }

    /// 混合语义最优切分（guangzh 声母残留场景）。
    ///
    /// `best_segmentation`（FULL_ONLY）对含声母尾段的输入（guangzh）返回空——
    /// "zh" 不是合法全音节；但候选生成路径用 token 语义（`segmentations_tokens`，
    /// Shengmu(zh) 整段）能切出 [guang, zh] 命中「广州」。两条链路语义不一致
    /// 是残留根因：consumed/显示/音节数按 FULL_ONLY→parse_pinyin 贪心切出
    /// [guang, z, h]，广州 只消费 6 字母，h 成残留上屏后滞留 buffer。
    ///
    /// 本方法给偏移类消费者（syllable_count / syllable_segmentation /
    /// syllable_prefix_lens）提供与候选查询同源的切分：FULL_ONLY 有结果时
    /// 调用方不会走到这里（行为零变化）；为空时枚举 token 切分打分择优。
    /// 注意「末音节是否完整」判定（is_valid 消费者）不适用本方法——
    /// 那个问题的答案不应被 Initial/Shengmu 段回答成 true（如 guangzho 的尾段 o）。
    pub(crate) fn best_token_segmentation<'a>(&self, input: &'a str) -> Vec<&'a str> {
        let (best, _) = self.pick_segmentation(input, &[8, 128, 512], true);
        best
    }

    /// 近期提交加分：指数衰减的近期分，让“刚选过的词”快速置顶，
    /// 久未使用的词自然下降，避免历史高频词长期霸占首位。
    /// `recent_score` 是固定点值（RECENT_SCALE = 1000 代表 1.0）。
    /// 权重 5000：让单次 recent ≈ base 量级，
    /// 不"夸张反超"高频常用字。用户连续选 N 次后 recent 衰减叠加仍能反超。
    pub(crate) const RECENT_COMMIT_BONUS: u64 = 5_000;
    pub(crate) const RECENT_SCALE: u64 = 1_000;
    /// 累计选择上屏加分：每次历史上屏提供固定分数加成，且持久化后仍生效。
    /// 上限 15 次：对齐同音词候选规模（一般十来个），
    /// 避免重度使用词拿到 25000 碾压高频系统词。
    pub(crate) const TOTAL_COMMIT_BONUS: u64 = 500;
    pub(crate) const TOTAL_COMMIT_BONUS_CAP: u32 = 15;

    /// 原始输入习惯映射加分：每次选择上屏增加固定分数，上限避免霸屏。
    pub(crate) const INPUT_MAPPING_BONUS: u64 = 1_000;
    pub(crate) const INPUT_MAPPING_BONUS_CAP: u32 = 20;
    /// 前缀映射加分：当前输入是历史输入的前缀时（如 byd 是 bydi 的前缀），
    /// 也给予一定加分，让"越用越准"的效果延伸到简拼。
    pub(crate) const PREFIX_MAPPING_BONUS: u64 = 600;
    pub(crate) const PREFIX_MAPPING_BONUS_CAP: u32 = 20;
    /// bigram 上下文加分：候选与历史栈中上屏词构成高频共现时加分。
    ///
    /// `candidate_score` 遍历 `context` 栈（最多 30 个词），对每个上屏词查
    /// `bigram_count(prev, cand)`，按距离指数衰减加权（`BIGRAM_DECAY^distance`）后求和，
    /// 单词贡献 = `min(count, BIGRAM_BONUS_CAP) * BIGRAM_DECAY^distance * BIGRAM_BONUS`。
    /// 满血（最近词 count≥10）贡献上限 15000：
    /// - 与 INPUT_MAPPING(1000) 同量级但更高，因为上下文信号更强（真实上文共现）
    /// - 满血 15000 < FULL_MATCH_BONUS(30000)，不破坏字数精确匹配优先级
    /// - 满血 15000 < base_freq 主导项（高频系统词），不会顶下高频词
    /// - 零记录即零分，不惩罚无上下文历史的候选（纯 additive 奖励）
    #[allow(dead_code)] // 旧版固定 bigram 加分常量，已被 BIGRAM_BONUS_K 自适应版本替代
    pub(crate) const BIGRAM_BONUS: u64 = 1_500;
    #[allow(dead_code)]
    pub(crate) const BIGRAM_BONUS_CAP: u32 = 10;
    /// 纠错候选词频地板（低频词占黄金位的防线——
    /// 崔颢(483) 经非邻接替换[已移除]、拿他(100) 经删除变形）。
    /// 纠错本身是低置信假设，被救回的词必须足够常见才配得上这个假设
    /// （贝叶斯直觉：低概率事件需要强证据）。
    /// 阈值 300：崔颢案的生成路径（非邻接替换）已整体移除，不再需 500 来挡；
    /// 真实种子谱
    /// 取证——≤200 档是 拿他/人名地名 类真噪音（地板要挡的），300-499 档是
    /// 一刹那(491)/传染性(491)/运行时(355) 类合法常用词（500 会误杀，
    /// 文档旗舰例 yunxongshi→运行时 因此在真实库上失效）。300 同时仍挡住
    /// 拿他(100) 类删除变形噪音（守护：test_correction_freq_floor_real_seed）。
    pub(crate) const CORRECTION_MIN_WORD_FREQ: u32 = 300;
    /// 上下文证据发音营救门槛：变体解读的搭配链证据
    /// （bigram/collocation，不含原始词频）至少这么强才触发——"明确用户可能
    /// 存在发音错误"。锚点：种子侧最低条目 = 挖掘 min-count 5 的压缩值 8
    ///（全部 ≥8，3 不会引起种子误触发）；用户侧单次教学 +3（拆词学习强意图
    /// 信号）恰好达标——教一遍 woaizhongguo，之后 woaizongguo 即可救。
    /// GAP 是句级分支变体须超出字面解读的余量，防弱翻转。
    pub(crate) const CONTEXT_RESCUE_MIN_LINK: u32 = 3;
    pub(crate) const CONTEXT_RESCUE_GAP: u32 = 3;
    // 双击故障判定阈值已改为引擎可配字段 double_fault_threshold_ms
    ///（默认 100，偏好设置滑块 30..1000，set_double_fault_threshold_ms 钳制），
    /// 见 mod.rs 字段文档与 process_timed。
    /// 自适应 bigram cap：对数增长，替代固定 CAP。
    /// 公式：dyn_cap = CAP_BASE + log2(max_user_bigram + 1) × CAP_K
    /// 冷启动（max=0）→ cap = CAP_BASE = 8，接近原 CAP=10 行为。
    /// max=97 → cap = 8 + log2(98)×4 ≈ 34，头部信号不再被压平。
    /// 与 rank_boost 同构（对数增长），项目一致性强。
    pub(crate) const BIGRAM_CAP_BASE: u32 = 8;
    pub(crate) const BIGRAM_CAP_K: u32 = 4;
    /// 对数压缩后的 bigram 加分系数。
    /// 旧：min(count,10) × 1500，count=10 满血 15000。
    /// 新：log2(min(count,cap)+1) × K，count=10 → log2(11)×8000 ≈ 27500。
    /// 8000（而非 3000）的由来：让上下文搭配（collocation_freq 命中的组合词，
    /// 如 机→制 的「机制」）能压过高频字的 total+primary 加成。实测 3000 时
    /// bigram_bonus=6000 不足以让「制」超过「只」(total+primary≈13000)。
    pub(crate) const BIGRAM_BONUS_K: u64 = 8_000;
    /// 全局用户调制器：没用过的预设词 effective freq 衰减系数（千分制）。
    /// 用过的词（freq.counts 含）= 1000（1.0 倍，不变）；没用过 = 600（0.6 倍，温和让权）。
    /// 冷启动（counts 为空）全部 = 1000，不衰减（行为同现状）。
    pub(crate) const UNUSED_FREQ_DECAY: u64 = 600;
    /// 上下文历史栈最大长度（最近30个上屏词，加权衰减参与 bigram 重排）。
    ///
    /// 30（而非 20）的由来：标点/切输入法不再清空 context（引擎运行期间持续
    /// 生效），更长窗口让跨句连续输入也能享受 bigram 引导。第30个词权重 ≈0.85^30≈0.008，
    /// 自然衰减到可忽略，不会引入噪音。
    pub(crate) const CONTEXT_HISTORY_SIZE: usize = 30;
    /// 上下文跨会话快照恢复的词数。
    /// 只带栈尾 10 个：BIGRAM_DECAY=0.85^d 下第 11 个词权重 ≈0.20 且跨会话的旧话题
    /// 本就是噪声，30 全存收益小隐患大。flush 时写 db_meta.context_snapshot，启动读回。
    pub(crate) const CONTEXT_SNAPSHOT_SIZE: usize = 10;
    /// bigram 衰减系数（每远一个词，权重 ×0.85，近期主导远处补充）
    pub(crate) const BIGRAM_DECAY: f64 = 0.85;

    /// 紧邻强 bigram 硬规则的最小用户共现次数（candidate_cmp 第3.5层）。
    /// 3 = 用户明确重复建立过的关联（1-2 次可能是误触/偶然，走软加分即可）。
    /// 与项目「用户重复 3 次算稳定意图」的惯例一致。
    pub(crate) const ADJACENT_BIGRAM_HARD_MIN_COUNT: u32 = 3;

    /// 最终候选数量保险丝。排序+整句插入完成后截断到此数。
    /// 一页 7-9 个候选，300 ≈ 30+ 页（无人翻到），用户可见结果零损失；
    /// 防模糊音+长输入病理场景（500+ 候选）的排序/内存/FFI 传输膨胀。
    pub(crate) const MAX_FINAL_CANDIDATES: usize = 300;

    /// 长词匹配 bonus 的缩放系数（千分制）。
    ///
    /// 当 `context_last + candidate_text` 拼成的新文本在词库中存在时（如
    /// "星期"+"六"="星期六"），按该长词的 base_freq 给候选加分：
    ///
    /// 系数 1500（×1.5）让 bonus 略高于长词自身的 base_freq 主项，
    /// 确保「星期六」(freq≈5000) 的「六」加分约 7500，能压过「流」/「留」
    /// 的高 base_freq 差距；冷僻长词 freq 低加分也低，不至于误压高频词。
    pub(crate) const LONG_WORD_BONUS_SCALE: u64 = 1500;
    /// green_memo 不同拼音的容量上界。
    /// 超过时整体 clear（会话级缓存，重启后靠 bigram 举一反三重建）。
    /// 200 个不同拼音覆盖日常自组词场景；重度写作用户偶尔触发 clear，bigram 引导可恢复。
    pub(crate) const GREEN_MEMO_MAX_ENTRIES: usize = 200;

    /// 计算候选的整数综合分数（确定性排序键的一部分）
    ///
    /// score = base_freq * rank_boost / 1000
    ///       + min(total_count, 50) * 500
    ///       + recent_score(text) / 1000 * 12_000
    ///       + min(mapping_count, 20) * 1_000   （当前输入与该候选的历史映射）
    pub(crate) fn candidate_score(&self, input: &str, c: &Candidate) -> u64 {
        // 单查询缓存：candidate_cmp 在 sort_by 比较器中对每对候选各调
        // 一次本函数，123 候选 ≈1700+ 次调用 × ~7.5µs → 17ms 尖峰（shishierfe 卡顿）。
        // 缓存后每候选只算一次（~123 次），尖峰消除。生命周期 = 单次 process_inner
        //（入口清空），同一次查询内打分确定。
        if let Some(&v) = self.score_cache.borrow().get(c.text.as_ref()) {
            return v;
        }
        let bd = self.score_breakdown(input, c);
        // 学习型排序器：仅当运行时显式注入权重（离线训练后 set_ranker_weights）
        // 才走点积路径。编译期 ranker_weights() 不自动启用——
        // 实测点积路径与 primary_reading_bonus(5000)、context_bonus(6000) 等手工加成
        // 不在同一量级（点积后 bigram_bonus=6000 缩水到 ~16），导致上下文关联被稀释
        // （机→zhi 场景诊断）。默认走手调公式，bigram 等分量是绝对贡献。
        if let Some(w) = self.ranker_weights {
            if w.iter().any(|&v| v != 0.0) {
                let feats = [
                    bd.base,
                    bd.total_bonus,
                    bd.recent,
                    bd.mapping_bonus,
                    bd.bigram_bonus,
                    bd.long_word_bonus,
                ];
                let dot: f64 = feats
                    .iter()
                    .zip(w.iter())
                    .map(|(f, wt)| *f as f64 * wt)
                    .sum();
                // 上下文同音证据不是 6 维特征之一（契约不变），但它是用户
                // 显式选择证据，注入 ranker 权重时也要生效——点积后绝对分叠加。
                let v = dot.max(0.0) as u64 + bd.ctx_homophone_bonus;
                self.score_cache.borrow_mut().insert(c.text.clone(), v);
                return v;
            }
        }
        // 默认（未注入 ranker_weights）：手调公式求和，各分量绝对值相加。
        let v = bd.total();
        self.score_cache.borrow_mut().insert(c.text.clone(), v);
        v
    }

    /// 计算候选打分的各分量明细（供 candidate_score 求和 + 学习型排序器训练特征）。
    ///
    /// 拆分自 candidate_score，行为完全一致，只是返回结构化的分量而非标量总分。
    /// pub 供离线调试工具（`--breakdown` / `repl :b`）：单候选分值逐分量展开，
    /// 排序问题定位（哪一层/哪个信号在起决定作用）免写临时测试。
    pub fn score_breakdown(&self, input: &str, c: &Candidate) -> ScoreBreakdown {
        // 单字按音隔离频率统计：用 (text, pinyin) 维度的数据，
        // 避免 di 选「地」让 de 的「地」获得 rank_boost/recent 加成（跨音串扰）。
        // 多字词不存在多音歧义（如「你好」只有 ni hao），继续用不分音的 counts。
        let is_single = c.text.chars().count() == 1;
        // 取候选拼音的首音节作为隔离维度（单字各项统计共用）
        let pinyin_syl = c.pinyin.split_whitespace().next().unwrap_or(&c.pinyin);
        // 竞争频率（「退化」通道）：只作用在「候选拼音与输入
        // 音节完全一致」的候选（同音竞争的本来语义——de 下 的/得/地 都
        // 拼音恰为 de，才是同一场竞争）。前缀/简拼候选完全走现状模型——
        // 它们拼音与输入不一致，不属同音竞争（按候选拼音评会把 sh 的单字
        // 错切进各自的音节语境；test_user_feedback/test_cross_reading 两案
        // 即此教训）。recent_score 不动（短期通道）。
        //
        // 单字的竞争 ctx 必须按音隔离（首音节 pinyin_syl，与既有
        // pinyin_rank_boost/recent 的隔离维度一致——test_cross_reading 的
        // 守护点：di 选「的」不得让 de 读音的「的」获任何加成）；多字词拼音
        // 与输入不一致时不评（其 boost 走现状）。
        let pin_key: String = if is_single {
            pinyin_syl.to_string()
        } else {
            c.pinyin.trim().to_string()
        };
        let input_chars: Vec<char> = input.chars().filter(|c| *c != '\'').collect();
        let pin_chars: Vec<char> = pin_key.replace(' ', "").chars().collect();
        let is_precise = pin_chars == input_chars;
        // 上下文同音证据：绝对分 bonus（正增量才计入）。
        // 零证据 = 0；只在「用户在该语境实际选过某读法」时非零。
        let mut ctx_homophone_bonus: u64 = 0;
        let (rank_boost, total_count) = if is_precise {
            // 同音精确阶梯：静态保持现状（按音隔离的 ilog2 计数——
            // pinyin_count(text, syl) 的 ilog2，与旧 pinyin_rank_boost 同构，
            // 隔离守护（test_cross_reading）：di 选「的」不影响 de 的「的」的
            // 静态 boost）+ 动态竞争增量（±480，竞争证据饱和 total≥α_pin
            // 后生效）——用过则涨、停用则跌，dynamic override 依旧。
            let priors = self.pin_priors_full(&pin_key);
            let prior_w = priors
                .iter()
                .find(|(w, _)| w.as_str() == c.text.as_ref())
                .map(|(_, p)| *p)
                .unwrap_or(0.0);
            let smoothed_w = self.pin_comp_freq(&pin_key, &c.text);
            let dynamic = smoothed_w - prior_w;
            let total = self
                .competition
                .total_of(&crate::competition::CtxKey::Pin(pin_key.clone()));
            let dynamic_term = if total >= crate::competition::ALPHA_PIN {
                (480.0 * dynamic) as i64
            } else {
                0
            };
            // 单字静态：按音隔离的计数（pinyin_count，不含跨音文本级 counts）；
            // 多字词静态：文本级 counts（与旧一致）。
            let static_count = if is_single {
                self.freq.pinyin_count(&c.text, pinyin_syl)
            } else {
                self.freq.counts.get(c.text.as_ref()).copied().unwrap_or(0)
            };
            let static_boost = 1000 + (static_count + 1).ilog2() * 150;
            // 上下文同音证据：`(拼音, 上一上屏词) → 选择` 的
            // 平滑份额增量，只在用户在该语境实际选过时非零（零证据 = 0，
            // 排序逐字节保持现状）。与全局 dynamic_term 叠加：上下文证据是更
            // 具体的意图，量级更高（±600 vs ±480）。
            let ctx_term = self
                .context
                .last()
                .map(|prev| self.pin_ctx_boost_with_prior(&pin_key, &c.text, prev, prior_w))
                .unwrap_or(0);
            ctx_homophone_bonus = ctx_term.max(0) as u64;
            let rb = ((static_boost as i64) + dynamic_term).max(200) as u32;
            (rb, (smoothed_w * 50.0) as u32)
        } else {
            // 非精确候选（前缀词/简拼）：完全现状（ilog2 raw count + raw count cap）。
            (
                self.freq.rank_boost(&c.text),
                self.freq.counts.get(c.text.as_ref()).copied().unwrap_or(0),
            )
        };
        let recent_score = if is_single {
            self.freq.pinyin_recent_score(&c.text, pinyin_syl)
        } else {
            self.freq.recent_score(&c.text)
        };
        // 自适应 user_modulator：没用过的预设词温和衰减，让出主导权。
        // 用过的词 ×1.0 不变；没用过 ×UNUSED_FREQ_DECAY（0.6）。
        // 冷启动（counts 为空）全部 ×1.0，行为同现状。
        //
        // 「用过」的豁免必须按 (text, pinyin) 维度隔离：
        //
        // counts 是不分音的文本级表——di 选「的」会写 counts[的]，若沿用
        // counts_contains 会让 de 读音的「的」拿到"用过"豁免（600→1000，
        // test_cross_reading 的守护点）。改查既有 `freq.pinyin_count(text, pinyin_syl)`
        // ——di 的选择只写 pinyin_count(的, di)，不影响 pinyin_count(的, de)。
        // 全局冷启动守卫（counts_is_empty）保持不分音。多字词沿用 counts_contains
        //（跨词豁免是其设计语义）。
        let used = if is_single {
            self.freq.pinyin_count(&c.text, pinyin_syl) > 0
        } else {
            self.freq.counts_contains(&c.text)
        };
        let user_modulator = if self.freq.counts_is_empty() || used {
            1000
        } else {
            Self::UNUSED_FREQ_DECAY
        };
        // base 分量：词库 freq × 用户使用加成。
        //
        // base 话语权下调（/1M → /5M）：
        // 原公式 base = freq × rank_boost × user_modulator / 1_000_000，满血时
        // base ≈ freq（如「的」9999），与 total_bonus 满血（7500）同量级——
        // 高频字即使没用过也能压过用户重度使用的低频词，freq 主导排序。
        //
        // 用户论点：freq 唯一用途是「同音节内候选间相对排序」，不该主导总分。
        // 主导权应交给 bigram（上下文关联）+ total/recent/mapping（用户学习）。
        //
        // 改为 /5_000_000 后：
        // - 「的」base 9999→2000，「吃」6800→1360，「持」4500→900（区分度保留）
        // - base 满血 2000 << total_bonus 满血 7500，用户信号主导
        // - 翅葡萄 bug 不回归（吃 1360 > 持 900 仍成立）
        //
        // 为什么不拉低 MAX_FREQ：clamp 会丢失高频字间区分度（吃6800 vs 持4500
        // 都 clamp 到 3000 → 翅葡萄回归）。调除数保留 freq 相对顺序，更优雅。
        let base = c.base_freq as u64 * rank_boost as u64 * user_modulator / 5_000_000;
        let total_bonus =
            (total_count.min(Self::TOTAL_COMMIT_BONUS_CAP)) as u64 * Self::TOTAL_COMMIT_BONUS;
        let recent = recent_score as u64 * Self::RECENT_COMMIT_BONUS / Self::RECENT_SCALE;
        let mapping_bonus = if self.input_mapping_enabled {
            // 精确映射计数（exact_bonus 不设：与 total_bonus/recent 高度重叠，
            // 选过该词已经 counts+1/recent 加分，再加 exact 多余）。
            // mapping_count 仍保留，仅用于下面 prefix 查询的跳过优化（exact 命中时不查 prefix）。
            let mapping_count = self
                .input_mappings
                .get(&(input.to_string(), c.text.to_string()))
                .copied()
                .unwrap_or(0);

            // 前缀映射：当前输入是某个历史输入的前缀（如 byd 是 bydi 的前缀），
            // 说明用户之前用更长的输入选过这个词，现在用简写也应该优先。
            // 用反向索引 input_mappings_by_text 按 text 取小集合（通常 ≤ 几条），
            // 避免遍历整个 input_mappings（用户积累后可达数千条，致 process 线性退化）。
            let mut prefix_count = 0u32;
            if mapping_count == 0 {
                if let Some(bucket) = self.input_mappings_by_text.get(c.text.as_ref()) {
                    for (hist_input, cnt) in bucket {
                        if hist_input.starts_with(input) && hist_input.len() > input.len() {
                            prefix_count = prefix_count.max(*cnt);
                        }
                    }
                }
            }
            let prefix_bonus = (prefix_count.min(Self::PREFIX_MAPPING_BONUS_CAP)) as u64
                * Self::PREFIX_MAPPING_BONUS;

            // 首字母简拼映射（如 js→竞赛）：
            // 当前输入是纯简拼（如 js），历史全拼输入（如 jingsai）的首字母提取（j+s=js）
            // 与当前输入匹配时，说明用户用全拼选过这个词，简拼也应优先。
            // 与 prefix_bonus 叠加（不互斥）——prefix 匹配"jsai→js"（部分前缀），
            // jianpin 匹配"jingsai→js"（首字母提取），两者维度不同。
            let jianpin_bonus = if crate::pinyin::is_jianpin(input) {
                let mut jp_count = 0u32;
                if let Some(bucket) = self.input_mappings_by_text.get(c.text.as_ref()) {
                    for (hist_input, cnt) in bucket {
                        let hist_initials: String = crate::pinyin::parse_pinyin(hist_input)
                            .syllables
                            .iter()
                            .filter_map(|s| hist_input.get(s.start..s.start + 1))
                            .collect();
                        if hist_initials == input {
                            jp_count = jp_count.max(*cnt);
                        }
                    }
                }
                // 用 INPUT_MAPPING_BONUS（1000）——用户用全拼选过是强意图信号
                (jp_count.min(Self::INPUT_MAPPING_BONUS_CAP)) as u64 * Self::INPUT_MAPPING_BONUS
            } else {
                0
            };

            prefix_bonus + jianpin_bonus
        } else {
            0
        };
        // bigram 上下文加分：遍历最近 CONTEXT_HISTORY_SIZE(=30) 个上屏词，加权衰减求和。
        // 基于马尔可夫链"近期依赖最强"思想：最近词 weight=0.85^0=1.0，越远 weight 越低
        // （×0.85 指数衰减），让近期上下文主导、远处补充。
        // 比标准一阶马尔可夫（只看1个词）能看到更长的搭配规律。
        //
        // 公式抽成公共函数 `context_bonus`（engine/mod.rs），与 DP 共用，消除两层公式不一致。
        // 双轨查询（bigram 表已拆为 word + char 两张独立表）：
        // 1. 先查词级 word_bigram_count(prev, 整个候选 text)
        // 2. 若词级为 0 且候选是多字词，回退字级 char_bigram_count(prev, 候选首字)
        let bigram_bonus = if !c.text.is_empty() {
            let first_char = c.text.chars().next().unwrap_or('\0');
            let is_multichar = c.text.chars().count() > 1;
            self.context_bonus(&c.text, first_char, is_multichar, &c.pinyin)
        } else {
            0
        };

        // 长词匹配 bonus：context 最近 N 词 + 候选 text 拼成的新文本若在词库中存在
        // （如 "星期"+"六"="星期六"、"手机"+"电池"="手机电池"），按该长词的
        // base_freq 给候选加分。词频越高加分越多——让词库已有的长词优先补全，
        // 解决「星期后输入 liu，六应优先于流/留」的上下文感知需求。
        //
        // 链式扩展：原版只看 context.last()（单词拼接），
        // 现在遍历 context 最近 3 词作为拼接起点，捕捉「星期一」+「上午」=「星期一上午」这类
        // 跨多词长词匹配。取所有起点中的最大 bonus（不叠加），且按链长加权（seg_count）：
        // 链得越长（覆盖更多上文词），证明这条候选与历史衔接越紧密，bonus 越大。
        //
        // 仅对非纠错候选生效（纠错候选本就是猜测，不参与长词匹配）。
        // 注意：逐字上屏时 context 是单字（如"期"），"期"+"六"="期六"通常不在
        // 词库 → bonus=0，符合直觉（逐字输入无长词先验）。
        let long_word_bonus = if !c.is_correction {
            let mut best: u64 = 0;
            let ctx_len = self.context.len();
            // start 为拼接起点在 context 中的下标；取最近 3 个起点（含 last）。
            // .rev() 让更长的链（start 更小）先尝试，但 best.max 保证最终取最大。
            for start in (ctx_len.saturating_sub(3)..ctx_len).rev() {
                let mut combined = String::new();
                for w in &self.context[start..ctx_len] {
                    combined.push_str(w);
                }
                combined.push_str(&c.text);
                if let Some(f) = self.dict.long_word_freq(&combined) {
                    // seg_count = 链覆盖的 context 词数 + 1（候选本身），作为链长权重。
                    let seg_count = (ctx_len - start + 1) as u64;
                    let bonus = f as u64 * Self::LONG_WORD_BONUS_SCALE * seg_count / 1000;
                    best = best.max(bonus);
                }
            }
            best
        } else {
            0
        };

        // 上下文搭配命中（candidate_cmp 第3层判定用）。
        // collocation_freq 查「上文词 + 候选」是否构成词库词（如「记忆」）。
        // 仅看 context.last()（紧邻上文），冷启动 context 空 = false。
        // 读音校验：传候选 pinyin，避免「你+呢(ni)」误匹配词库「你呢(ni ne)」。
        let has_collocation = if let Some(prev) = self.context.last() {
            self.dict
                .collocation_freq(prev, &c.text, &c.pinyin)
                .is_some()
        } else {
            false
        };

        ScoreBreakdown {
            base,
            total_bonus,
            recent,
            mapping_bonus,
            bigram_bonus,
            long_word_bonus,
            has_collocation,
            ctx_homophone_bonus,
        }
    }

    /// 抽取给定 input 下所有候选的打分特征明细（供学习型排序器训练数据记录）。
    ///
    /// 重新跑一次 process 取候选列表，对每个候选用 score_breakdown 计算各分量。
    /// 仅在 commit 时（低频）由 FFI 调用，不影响 per-keystroke 热路径。
    /// 返回 (text, ScoreBreakdown) 列表，调用方据此 + 用户选择索引写训练样本。
    pub fn extract_features(&self, input: &str) -> Vec<(String, ScoreBreakdown)> {
        let candidates = self.process(input);
        candidates
            .into_iter()
            .map(|c| {
                let bd = self.score_breakdown(input, &c);
                (c.text.as_ref().to_owned(), bd)
            })
            .collect()
    }

    /// extract_features + 序列化一步到位（FFI x1_engine_extract_features 与
    /// 引擎服务侧 selections 记录共用，见下方 features_to_json）。
    pub fn extract_features_json(&self, input: &str) -> String {
        features_to_json(&self.extract_features(input))
    }

    /// 完全匹配加分：多音节输入时，候选字数等于当前输入音节数视为"完全匹配"。
    ///
    /// 这个加分用于让 "zhongguoren → 中国人" 排在 "中国" 前面，
    /// 同时又不会让低频长词压过高频系统词。
    /// 注意：完整 vs 前缀匹配的绝对优先级在 candidate_cmp 开头已判定，
    /// 此处是分数层面的额外加权。
    pub(crate) const FULL_MATCH_BONUS: u64 = 30_000;

    /// 简拼完全匹配加分的词频缩放系数。
    ///
    /// 简拼路径（process_jianpin_inner）的完全匹配加分按 `min(FULL_MATCH_BONUS, base_freq × 本系数)`
    /// 计算，让加分与词频正相关：高频成语仍拿高分排首位，冷僻成语加分显著降低，
    /// 避免冷僻词（如「蜗行牛步」freq=1040）靠固定加分压过高频单字（如「我」freq=9950）。
    /// 系数 8：1040×8=8320（蜗行牛步被压下），1976×8=15808（中国人民仍首位），见 §3.x。
    pub(crate) const FULL_MATCH_BONUS_SCALE: u64 = 8;

    /// 整句自动固化默认阈值（0=禁用，用户可通过偏好设置调整为 1-5）。
    /// 默认 0（禁用）：绿词靠 bigram 学习"举一反三"，不固化整句（句子造不完，固化无意义）。
    /// 用户想固化特定词时用 Shift+数字键手动触发。
    /// 用户可在偏好设置开启（1-5）。
    pub(crate) const DEFAULT_AUTO_SOLIDIFY_THRESHOLD: u32 = 0;

    /// 纠错候选数量上限默认值（用户可通过偏好设置调整为 0-10）。
    /// 5 是兼顾"看得见的纠错能力"和"不挤占正常候选位"的经验值。
    /// 0 = 禁用纠错补充（区别于 smart_correction_enabled 总闸）。
    pub(crate) const DEFAULT_MAX_CORRECTIONS: usize = 5;

    /// 多音字主音加成：精确匹配当前音节时，主音字比次音字额外高的分数。
    ///
    /// 1000（而非 5000，随 base 分量 /5M 同步缩 5 倍）：
    /// base 公式从 /1M 改为 /5M 后，base_freq 主导项缩小 5 倍（德的 base
    /// 从 5230→1046）。primary_reading_bonus 也需同步缩 5 倍保持相对关系：
    /// - 德(主音) base 1046 + primary 1000 = 2046 > 地(次音) base 1756 ✓ 主音优先
    /// - 底(主音 freq=50) base 10 + primary 1000 = 1010 < 地 base 1756 ✓ 不压高频次音
    ///
    /// 历史：原 5000 设计依据是 base /1M（德 5230+5000=10230 > 地 8780）。
    /// 用户实际输入习惯可通过 `primary_reading_bonus` 动态调整该加成
    pub(crate) const PRIMARY_READING_BONUS: u64 = 1_000;

    /// 多音字次音反超主音的偏好由 `primary_reading_bonus` 静态化，
    /// 统一走用户频率信号，「选择次数差反超」的动态补偿不再使用
    ///（禁止重新引入补偿）。
    /// 计算某候选在「主音优先 + 动态权重」下的净加成分数。
    ///
    /// 仅当候选精确匹配当前查询音节时调用（由调用方判断）。
    ///
    /// 规则：
    /// - 主音字：基础加成 `PRIMARY_READING_BONUS`
    /// - 次音字：基础加成 0
    /// - 动态调整：用户在某 (字, 音) 下反复选择（pinyin_count 高），
    ///   说明偏好该读音，按次数比例补偿加成。每多选一次，补偿
    ///   `PRIMARY_READING_BONUS / OVERRUN_THRESHOLD` 分，使次音字
    ///   在用户选够阈值次后能反超主音字（动态尊重用户输入习惯）。
    pub(crate) fn primary_reading_bonus(&self, c: &Candidate) -> u64 {
        // 主音加成：仅返回基础常量（主音字 +1000，次音字 0）。
        // 不做 user_count 动态补偿——补偿会让 primary_reading_bonus
        // 膨胀到 40000+（user_count×1666），远超 bigram/context_bonus，导致上下文关联
        // 被压制（机→zhi 场景「制」排不到「只」前）。candidate_score 的 base/total/recent
        // 已反映用户偏好，primary 不需重复叠加 user_count。
        if c.is_primary {
            Self::PRIMARY_READING_BONUS
        } else {
            0
        }
    }

    /// 比较两个候选的排序顺序。
    ///
    /// 排序规则：
    /// 1. 多音节输入时词组优先于单字；单音节输入时单字优先于词组
    /// 2. 单音节输入时，精确匹配当前音节的字优先于仅前缀匹配的字
    /// 3. 上下文优先级分层：
    ///    第 3 层 collocation（词库权威搭配命中 > 无搭配）；
    ///    第 3.5 层紧邻用户强 bigram（context.last() 与候选的用户共现 ≥3 → 优先，
    ///    见 ADJACENT_BIGRAM_HARD_MIN_COUNT）；
    ///    然后按综合整数分数降序：
    ///    score = base_freq × rank_boost / 1000
    ///          + min(total_count, 50) × 500
    ///          + recent_score(text) / 1000 × 12_000
    ///          + min(mapping_count, 20) × 1_000
    ///          + min(prefix_mapping_count, 20) × 600
    ///          + bigram_bonus（§3.6，见 `candidate_score`）
    ///          + long_word_bonus（§3.8，见 `candidate_score`）
    ///    多音节输入且字数等于音节数时额外加 FULL_MATCH_BONUS；
    ///    精确匹配当前音节的主音单字额外加 PRIMARY_READING_BONUS（§3.4）。
    ///    （score 完整计算见 `candidate_score`，本注释只列主要项。）
    /// 4. 同分单字按常用度降序（无用户历史时保证高频单字在前）
    /// 5. 分数相同且为多音节输入时，字数多的词优先；单音节输入时字数少的词优先
    /// 6. 上述全部相等时，按 pinyin、text 字符串升序比较，确保全序确定性
    /// 判断候选是否「完整匹配」输入音节：候选拼音的每个音节都精确等于对应输入音节。
    ///
    /// 区分完整匹配 vs 前缀匹配的关键（fanghu 案例）：
    /// - 防护 拼音 "fang hu" → split → ["fang","hu"]，逐个 == 输入 ["fang","hu"] → 完整匹配 ✓
    /// - 放还 拼音 "fang huan" → split → ["fang","huan"]，第二音节 "huan" != 输入 "hu" → 前缀匹配 ✗
    ///
    /// 候选拼音列存的是字典里的完整拼音（如 huan），不是用户输入的 hu，
    /// 所以逐音节比较能准确区分。这比旧版「只看字数==音节数」准确——
    /// 旧版放还/防护字数都=2、音节数都=2，无法区分，导致高 freq 放还压过防护。
    pub(crate) fn is_pinyin_full_match(candidate: &Candidate, input_syllables: &[&str]) -> bool {
        let cand_syls: Vec<&str> = candidate.pinyin.split_whitespace().collect();
        // 音节数不同（如 fang hu fu 是 3 音节，输入 2 音节）→ 前缀词，非完整匹配
        if cand_syls.len() != input_syllables.len() {
            return false;
        }
        // 混合输入判定：输入含单字符音节（如 zhonggrm 的 g/r/m）时，
        // 用「字数==音节数」作为完全匹配判据（候选字数已在 cand_syls.len() 体现）。
        // 单字符音节是首字母简拼，不要求候选拼音逐音节相等。
        let has_single_char_syl = input_syllables.iter().any(|s| s.len() == 1);
        if has_single_char_syl {
            return true; // 字数==音节数即为完全匹配（混合简拼场景）
        }
        // 全拼场景：逐音节比较，任一不等就是前缀匹配（如 huan != hu）
        cand_syls
            .iter()
            .zip(input_syllables.iter())
            .all(|(c, i)| c == i)
    }

    /// 两两比较器（主排序已改用 candidate_sort_key 键预计算，
    /// 见 engine/mod.rs 主排序处注释）。本函数保留为参考实现：
    /// 分层判定的可读语义都在这里，且 test_candidate_sort_key_matches_cmp
    /// 用它逐位验证键排序等价性。生产路径不再调用（dead_code 是预期的）。
    #[allow(dead_code)]
    // 8 个参数是分层判定本身的上下文（input / 双方候选 / 音节数 / 首音节 /
    // 窗口分 / 切分），打包成结构体只是把参数搬家；本函数是 candidate_sort_key
    // 的语义参考实现（生产排序已改用键预计算）。
    #[allow(clippy::too_many_arguments)]
    /// 简拼整词判据：候选字数 == 输入
    /// 字母数，且候选拼音各音节首字母拼接 == 输入（全小写）。
    ///
    /// 语义：纯首字母简拼的「每字母一字」读法——4 字母输入对应 4 字词
    /// （ssef → 似是而非 si shi er fei）。这是最强对位结构证据：候选恰好
    /// 解释整条输入、不多不少一个字母一个字，应排在普通词频候选之前（微软式）。
    /// 只对纯小写无撇号输入生效；输入侧补充处另有长度 3..=8 与去重约束
    /// （见 process_inner 简拼整词补充段），本函数是排序层的同源判据。
    pub(crate) fn is_jianpin_whole_word(input: &str, c: &Candidate) -> bool {
        if input.is_empty()
            || !input.chars().all(|ch| ch.is_ascii_lowercase())
            || c.text.chars().count() != input.len()
        {
            return false;
        }
        let initials: String = c
            .pinyin
            .split_whitespace()
            .filter_map(|s| s.chars().next())
            .collect();
        initials.len() == input.len() && initials.eq_ignore_ascii_case(input)
    }

    /// 输入首个音节能否成完整音节（「纯简拼让位」判据）。
    ///
    /// 语义：input 的某个前缀是合法完整音节（a/o/e 这类单字母音节也算——
    /// 它们本就是合法音节）。用于区分两类「每字母一字」输入：
    /// - `yem`：前缀 ye 是完整音节 → 用户更可能想打「完整音节+简拼」
    ///   （页面 ye|m / 也没 ye|m），全简拼读法（耶尔穆 y|e|m）应让位；
    /// - `ssef`：前缀 s/ss/sse 都不是完整音节（首个完整音节 se 在第二位，
    ///   只能靠 s|se|f 读到）→ 全简拼是唯一读法，简拼整词（似是而非）保持
    ///   最优先。
    ///
    /// 判据只看输入本身的字面结构：不看候选、不调词频参数——结构层判据，
    /// 与分层排序同哲学（否则同类现象换一批词又复发）。
    pub(crate) fn input_starts_with_complete_syllable(input: &str) -> bool {
        // 完整音节最长 6 字母（zhuang/chuang）；1 起步含 a/o/e 单字母音节
        let max = input.len().min(6);
        (1..=max).rev().any(|len| {
            input
                .get(..len)
                .is_some_and(crate::pinyin::is_valid_syllable_pub)
        })
    }

    /// 「有效简拼整词」= 每字母一字且输入首音节不能完整成音节。
    /// 排序层唯一入口——「纯简拼让位」判定收敛在此，
    /// candidate_cmp / candidate_sort_key 两条路径共用，避免条件漂移。
    ///
    /// `input_first_syl_complete` 由调用方在排序前算一次
    /// （[`Self::input_starts_with_complete_syllable`]）传进来——判据只与
    /// input 有关、与候选无关，放进每候选的键计算就是重复劳动（候选数百、
    /// 按键热路径预算敏感）。
    ///
    /// 背景（yem 场景）：旧版 is_jianpin_whole_word 无条件置顶 tier 0，
    /// 使 seed 里 base=6 的生僻词「耶尔穆」(ye er mu) 压过 base=248 的「也没」
    /// 与「页面」(ye mian)——纯简拼词霸屏。原则：「ye 能组成完整音节时
    /// 纯简拼应让位」；实现为整类机制：首音节完整时简拼整词退出 tier 0，
    /// 回到常规分层（完整匹配词 > 前缀词），与词频无关。
    #[inline]
    pub(crate) fn is_jianpin_whole_word_eligible(
        input: &str,
        c: &Candidate,
        input_first_syl_complete: bool,
    ) -> bool {
        !input_first_syl_complete && Self::is_jianpin_whole_word(input, c)
    }

    /// 排序比较器参考实现（矩阵守护）：生产排序已迁移 sort_by_key +
    /// candidate_sort_key（本文件顶部），本函数仅测试直调守护（tests_query
    /// 的分层序断言）——防误删保留 #[allow]。
    #[cfg_attr(not(test), allow(dead_code))]
    // 参数即分层判定的完整上下文（输入/双方候选/音节数/首音节/窗口分/切分），
    // 打包结构体只是把参数搬家
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn candidate_cmp(
        &self,
        input: &str,
        a: &Candidate,
        b: &Candidate,
        is_multi_syllable: bool,
        single_syllable_input: Option<&str>,
        best_score: u32,
        best_syllables: &[&str],
        // 纯简拼让位：input 首音节能否完整成音节（调用方预算一次）
        input_first_syl_complete: bool,
    ) -> Ordering {
        let a_len = a.text.chars().count();
        let b_len = b.text.chars().count();
        let a_is_single = a_len == 1;
        let b_is_single = b_len == 1;

        if is_multi_syllable {
            match (!a_is_single, !b_is_single) {
                (true, false) => return Ordering::Less,
                (false, true) => return Ordering::Greater,
                _ => {}
            }
            // 简拼整词（每字母一字）＝ 最强对位结构证据，先于完整/前缀分层
            //（ssef 的 似是而非 会被 2 字高频词按词频压到 rank 11）。
            //
            // 「纯简拼让位」：仅当输入首音节不能完整成音节时成立。
            // yem 的 ye 是完整音节 → 简拼整词（耶尔穆）退出本层、按常规分层；
            // ssef 的首音节 s 只能是简拼 → 简拼整词（似是而非）保持置顶。
            let a_jp = Self::is_jianpin_whole_word_eligible(input, a, input_first_syl_complete);
            let b_jp = Self::is_jianpin_whole_word_eligible(input, b, input_first_syl_complete);
            if a_jp != b_jp {
                return if a_jp {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            // 多音节输入下，完整匹配（候选的拼音音节精确等于输入音节）的多字词
            // 绝对优先于前缀匹配（候选拼音音节多于输入音节）的多字词。
            //
            // 关键（fanghu 案例）：只看「字数==音节数」判断完整匹配是错的——
            // 但放还(fang huan, 2字) 和 防护(fang hu, 2字) 字数都等于输入音节数(2)，
            // 会被误判为都完整匹配，导致高 freq 的放还(2400) 压过防护(1050)。
            // 正确判据：候选的拼音音节数必须等于输入音节数（拼音真的精确匹配，
            // 而非 fang hu 作为 fang huan 的前缀）。
            //
            // 简拼/混合输入同样适用（结构层原则，
            // 取代早期多轮频率类补丁——seg_all_complete 门控/撇号例外/freq 门槛资格）：
            // 输入是合法简拼（含混合）时，能匹配上（覆盖全部
            // 输入单位）的单个词候选，结构层优先于部分匹配（留尾巴）的词。
            // 这是层级判断而非词频判断——很多人(3单位全匹配) 优先于 很多(留 r)，
            // 现在网 优先于 现在，与词频无关；词频只在同层（都匹配上/都没
            // 匹配上）内排序。实测依据：hendr 最佳匹配是「很多人」但排在
            // 「很多」后面——"多匹配一个单位"就是更强的结构证据。
            let a_full = Self::is_pinyin_full_match(a, best_syllables);
            let b_full = Self::is_pinyin_full_match(b, best_syllables);
            let a_prefix = !a_is_single && !a_full; // 多字词但拼音非完整匹配（前缀匹配）
            let b_prefix = !b_is_single && !b_full;
            if a_full && b_prefix {
                return Ordering::Less;
            }
            if b_full && a_prefix {
                return Ordering::Greater;
            }
            // 多音节输入的单字候选之间：精确匹配第一音节的字优先于前缀匹配的字。
            // 例如 lilizi 中，li 的精确单字（里/力/理/锂...）应排在 liang/lian/liu
            // 等前缀匹配的字前面，否则用户翻页看到的全是无关的前缀字。
            if a_is_single && b_is_single {
                if let Some(syl) = best_syllables.first() {
                    let a_exact = a.pinyin.split_whitespace().next() == Some(*syl);
                    let b_exact = b.pinyin.split_whitespace().next() == Some(*syl);
                    if a_exact != b_exact {
                        return if a_exact {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        };
                    }
                }
            }
        } else {
            match (a_is_single, b_is_single) {
                (true, false) => return Ordering::Less,
                (false, true) => return Ordering::Greater,
                _ => {}
            }

            if let Some(syl) = single_syllable_input {
                let a_exact = a.pinyin.split_whitespace().next() == Some(syl);
                let b_exact = b.pinyin.split_whitespace().next() == Some(syl);
                if a_exact != b_exact {
                    return if a_exact {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    };
                }
            }
        }

        if a_is_single && b_is_single {
            // 多音节无词匹配时的单字 fallback：先按音节位置排序，
            // 避免把后续音节的高频单字排到前面。
            if best_score == 0 && best_syllables.len() > 1 {
                let first_a = a.pinyin.split_whitespace().next().unwrap_or("");
                let first_b = b.pinyin.split_whitespace().next().unwrap_or("");
                let pos_a = best_syllables
                    .iter()
                    .position(|&s| s == first_a)
                    .unwrap_or(usize::MAX);
                let pos_b = best_syllables
                    .iter()
                    .position(|&s| s == first_b)
                    .unwrap_or(usize::MAX);
                if pos_a != pos_b {
                    return pos_a.cmp(&pos_b);
                }
            }
        }

        // 第3层：上下文搭配优先。
        // 候选与上文 context.last() 构成词库词（collocation 命中）的优先于无搭配的。
        // 例：上文「记」+「忆」=「记忆」在词库 → 优先于「已」（无「记已」）。
        // 双方都命中或都不命中 → 进第4层比分数。冷启动 context 空 → 都不命中，行为同现状。
        // 轻量判定：直接调 collocation_freq（O(logN)），不调完整 score_breakdown。
        if let Some(prev) = self.context.last() {
            let a_collocation = self
                .dict
                .collocation_freq(prev, &a.text, &a.pinyin)
                .is_some();
            let b_collocation = self
                .dict
                .collocation_freq(prev, &b.text, &b.pinyin)
                .is_some();
            if a_collocation != b_collocation {
                return if a_collocation {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }

            // 第3.5层：紧邻用户强 bigram 硬规则。
            // context.last() 与候选的用户 bigram count ≥ 阈值 → 优先。
            // 场景（轮胎→放气）：教学「轮胎→放气」(count=9) 后，输轮胎再输
            // fangqi，首候选仍是放弃——栈内重复历史（利益→放弃多轮教学残留）在
            // context_bonus 里被 0.85 弱衰减累加多份，叠加放弃的 recent_score，
            // 总和反超 distance-0 的强关联。刚上屏词建立的紧邻关联应有支配权，
            // 与第3层 collocation（词库权威搭配）同构：这是「用户行为权威搭配」。
            // 只查用户表（user_word_bigram_count，含 equiv 泛化，不含种子回退），
            // 避免种子语料高频搭配升级为硬规则、大范围颠覆既有排序。
            let a_strong = self.freq.user_word_bigram_count(prev, &a.text)
                >= Self::ADJACENT_BIGRAM_HARD_MIN_COUNT;
            let b_strong = self.freq.user_word_bigram_count(prev, &b.text)
                >= Self::ADJACENT_BIGRAM_HARD_MIN_COUNT;
            if a_strong != b_strong {
                return if a_strong {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
        }

        let mut score_a = self.candidate_score(input, a);
        let mut score_b = self.candidate_score(input, b);

        // 注：FULL_MATCH_BONUS 加分块已删除（dead code）。
        // 完整 vs 前缀的绝对优先级已在 candidate_cmp 开头判定（上方
        // 的 a_full/b_full 比较），到此处要么双方都 full 要么都 prefix，
        // 加分到两边净零效果。FULL_MATCH_BONUS 常量保留（简拼路径仍真实使用）。
        // 混合简拼尾的结构优先同样走 tier（见上方 tier 块与
        // mixed_full_eligible），不在分数层做。

        // 主音优先加成：精确匹配当前音节的单字，主音字额外加分。
        // 让「德」(主音 de) 排在「地」(次音 de) 前面，但用户偏好可动态反超。
        // 仅对精确匹配当前首音节的单字生效，避免误伤前缀匹配/多字词。
        let cur_syl = if is_multi_syllable {
            best_syllables.first().copied()
        } else {
            single_syllable_input
        };
        if let Some(syl) = cur_syl {
            if a_is_single && a.pinyin.split_whitespace().next() == Some(syl) {
                score_a += self.primary_reading_bonus(a);
            }
            if b_is_single && b.pinyin.split_whitespace().next() == Some(syl) {
                score_b += self.primary_reading_bonus(b);
            }
        }

        let ord = score_b.cmp(&score_a);
        if ord != Ordering::Equal {
            return ord;
        }

        // 同分绿词按 DP 路径分降序（与 candidate_sort_key
        // 同层序）：绿词 base_freq 取切分最高词频（同构绿词恒同分）、pinyin 是同一串
        // 简拼字母（恒相同），旧 tie-break 落到 text 字典序——汉字 Unicode 序无语义
        //（「性爱」U+6027 压「西安」U+897F），DP 路径分更优的切分反而排后。
        // 非绿词 sentence_score 恒 0（相等自动跳过，行为不变）。
        if a.sentence_score != b.sentence_score {
            return b.sentence_score.cmp(&a.sentence_score);
        }

        // 同分单字按常用度降序（无用户历史时保证“你”排在“泥”前面）
        if a_is_single && b_is_single {
            let common_a = char_commonness(&a.text);
            let common_b = char_commonness(&b.text);
            if common_a != common_b {
                return common_b.cmp(&common_a);
            }
        }

        // 分数相同，按字数做次优区分
        let len_ord = if is_multi_syllable {
            b_len.cmp(&a_len)
        } else {
            a_len.cmp(&b_len)
        };
        if len_ord != Ordering::Equal {
            return len_ord;
        }

        // 最终按 pinyin、text 升序确保确定性全序
        a.pinyin.cmp(&b.pinyin).then_with(|| a.text.cmp(&b.text))
    }

    /// 计算候选的排序键（键预计算排序）。
    ///
    /// candidate_cmp 的每一层判定（词/字 tier、full-match、音节位置、collocation、
    /// 强 bigram、打分、常用度、字数、拼音）都是候选自身属性，不依赖被比较的
    /// 另一方。因此可以把 sort_by 的 O(n log n) 次两两比较（每次比较含 collocation
    /// Trie 查询 + user bigram hash 查询，上下文非空时是排序主成本）改为：
    /// 每候选预计算一次键（各查询只做 1 次）+ 元组排序。
    /// 排序结果与 candidate_cmp 两两比较逐位相同——
    /// 由 test_candidate_sort_key_matches_cmp 守护等价性。
    // 参数与 candidate_cmp 同源（键预计算版本的同一组上下文）
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn candidate_sort_key(
        &self,
        input: &str,
        c: &Candidate,
        is_multi_syllable: bool,
        single_syllable_input: Option<&str>,
        best_score: u32,
        best_syllables: &[&str],
        // 纯简拼让位：input 首音节能否完整成音节（调用方预算一次，
        // 与 candidate_cmp 同参数同语义——两实现在此维度必须一致）
        input_first_syl_complete: bool,
    ) -> CandidateSortKey {
        let len = c.text.chars().count();
        let is_single = len == 1;

        // tier 编码 candidate_cmp 开头的分层（值小者排前）：
        // 多音节：0=简拼整词 1=完整匹配词 2=前缀词 3=精确单字 4=前缀单字
        //        （词恒在字前；简拼整词=每字母一字，为最前层）
        // 单音节：0=精确单字 1=前缀单字 2=词（字恒在词前）
        //
        // 简拼/混合输入同样分层（与 candidate_cmp
        // 同步，test_candidate_sort_key_matches_cmp 守护）：合法简拼（含混合）
        // 下能匹配上（覆盖全部输入单位）的单个词候选，结构层优先于部分匹配
        // （留尾巴）的词——很多人 > 很多（留 r），与词频无关；词频只在同层内
        // 排序。此前的多轮频率类补丁（seg_all_complete 门控/撇号例外/freq 资格
        // 门槛）已全部废弃。
        let tier = if is_multi_syllable {
            // 有效简拼整词才置顶（纯简拼让位，见其函数注释）
            if Self::is_jianpin_whole_word_eligible(input, c, input_first_syl_complete) {
                0
            } else if !is_single {
                if Self::is_pinyin_full_match(c, best_syllables) {
                    1
                } else {
                    2
                }
            } else {
                let exact = best_syllables
                    .first()
                    .is_some_and(|syl| c.pinyin.split_whitespace().next() == Some(*syl));
                if exact {
                    3
                } else {
                    4
                }
            }
        } else if is_single {
            let exact = single_syllable_input
                .is_some_and(|syl| c.pinyin.split_whitespace().next() == Some(syl));
            if exact {
                0
            } else {
                1
            }
        } else {
            2
        };

        // 单字 fallback 音节位置（仅单字参与；词恒 0，跨类别对被 tier 隔开不会到此字段）
        let pos = if is_single && best_score == 0 && best_syllables.len() > 1 {
            let first = c.pinyin.split_whitespace().next().unwrap_or("");
            best_syllables
                .iter()
                .position(|&s| s == first)
                .unwrap_or(usize::MAX)
        } else {
            0
        };

        // 第3层 collocation + 第3.5层强 bigram（true=无搭配/弱关联，排后）。
        // context 空时两者恒 true（所有候选相等，相当于跳过这两层，与 cmp 行为一致）。
        //
        // 单字成词抑制：suppress_collocation_after_word=true 时，跳过
        // collocation_freq 查询（no_collocation 恒 true）。详见 context_bonus_with_ctx
        // 的同源改动与「单字成词抑制状态机」设计。
        let (no_collocation, no_strong_bigram) = if let Some(prev) = self.context.last() {
            let collocation_hit = if self.suppress_collocation_after_word {
                false
            } else {
                self.dict
                    .collocation_freq(prev, &c.text, &c.pinyin)
                    .is_some()
            };
            (
                !collocation_hit,
                self.freq.user_word_bigram_count(prev, &c.text)
                    < Self::ADJACENT_BIGRAM_HARD_MIN_COUNT,
            )
        } else {
            (true, true)
        };

        // 打分（含主音优先加成，规则同 candidate_cmp）
        let mut score = self.candidate_score(input, c);
        let cur_syl = if is_multi_syllable {
            best_syllables.first().copied()
        } else {
            single_syllable_input
        };
        if let Some(syl) = cur_syl {
            if is_single && c.pinyin.split_whitespace().next() == Some(syl) {
                score += self.primary_reading_bonus(c);
            }
        }

        // 常用度仅单字参与（词恒 0，词-词比较时相等自动跳过，同 cmp）
        let commonness = if is_single {
            char_commonness(&c.text)
        } else {
            0
        };
        // 多音节长词优先（降序 → 取负），单音节短词优先（升序）
        let len_key = if is_multi_syllable {
            -(len as i64)
        } else {
            len as i64
        };

        CandidateSortKey {
            tier,
            pos,
            no_collocation,
            no_strong_bigram,
            score: core::cmp::Reverse(score),
            // 绿词 DP 路径分同分 tie 层（与 candidate_cmp 同层序）：
            // 非绿词恒 0（相等自动跳过）
            sentence_score: core::cmp::Reverse(c.sentence_score),
            commonness: core::cmp::Reverse(commonness),
            len_key,
            // SmallStr 内联免堆分配（排序语义与 String 完全一致）
            pinyin: SmallStr::from_str(&c.pinyin),
            text: SmallStr::from_str(&c.text),
        }
    }
}

/// 把 (text, ScoreBreakdown) 列表序列化为 JSON（学习型排序器训练数据记录格式）。
///
/// 格式：`[{"text":"...","features":{"base":N,"total_bonus":N,...,"total":N}}, ...]`
/// 字段名是跨进程契约：宿主前端与引擎服务侧
/// （selections.rs）都按此格式写 selections.jsonl，离线训练工具
/// 按名解析——多端共用本实现（杜绝漂移）。
pub fn features_to_json(features: &[(String, super::ScoreBreakdown)]) -> String {
    let mut json = String::from("[");
    for (i, (text, bd)) in features.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push_str("{\"text\":\"");
        json.push_str(&text.replace('\\', "\\\\").replace('"', "\\\""));
        json.push_str("\",\"features\":{\"base\":");
        json.push_str(&bd.base.to_string());
        json.push_str(",\"total_bonus\":");
        json.push_str(&bd.total_bonus.to_string());
        json.push_str(",\"recent\":");
        json.push_str(&bd.recent.to_string());
        json.push_str(",\"mapping_bonus\":");
        json.push_str(&bd.mapping_bonus.to_string());
        json.push_str(",\"bigram_bonus\":");
        json.push_str(&bd.bigram_bonus.to_string());
        json.push_str(",\"long_word_bonus\":");
        json.push_str(&bd.long_word_bonus.to_string());
        json.push_str(",\"total\":");
        json.push_str(&bd.total().to_string());
        json.push_str("}}");
    }
    json.push(']');
    json
}
