//! 同音歧义判定 + 上下文证据分。
//!
//! # 为什么这里只有「判定」，没有「句级备选」
//!
//! 句级备选（给 3-4 音节绿词注入同拼音 `[句]` 候选，如「马上处罚 / 马上出发 /
//! 马上触发」）已移除：整句评测实测盲式同音扩句会注入大量垃圾读法——等于
//! 「已经精确解出整句了还在猜」，违反既有原则「确定缺席才猜测、猜测 ≤3 条」。
//! 本模块只保留两件事：
//!
//! 1. `is_ambiguous_pinyin`：commit 时判断该拼音是否**真同音歧义组**
//!    （≥2 个多字词、频率比 ≤5×、地板 100）——只有真歧义组的选择才值得
//!    记进上下文证据表，避免污染。
//! 2. `pin_ctx_boost*`：`(同音槽拼音, 上一上屏词) → 选择` 的平滑份额增量，
//!    供候选列表打分（`ScoreBreakdown.ctx_homophone_bonus`）使用。
//!
//! 句级长程语义（句中位置也能看见更远上文）是后续方向。

use std::collections::HashSet;

use super::Engine;

/// 槽内候选最低词频（低于此值视为生僻词，不参与同音判定）。
const AMBIG_FREQ_FLOOR: u32 = 100;
/// 组内最高/次高频率比上限：超过 5× 说明有明确赢家，不算真歧义。
const AMBIG_MAX_GAP_RATIO: f32 = 5.0;
/// 上下文同音证据的分数倍率与钳制（`ALPHA_PIN_CTX=2`，一次选择即生效）。
///
/// 量级推导（种子 chu fa 组）：处罚 base≈574、出发≈461、触发≈148
/// （freq×1000/5M）。α=2 时一次「机场→出发」——注意这里「机场」是**上一上屏词**，
/// 不是长程语境——选择让出发份额 delta≈0.217 → +391 分（翻越 113 分差）；
/// 「灵感→触发」delta≈0.296 → +533 分（翻越 426 分差）。
/// 钳制 ±600：可以翻越 2-4× 词频差，但不至于压过数量级差距。
///
/// 必须按**绝对分**累加，不能并进 `rank_boost`（base_freq 乘数）：触发 738 的
/// +533 乘数只值 ~226 绝对分，压不过处罚 2868 的 573（实测）。
pub(crate) const PIN_CTX_K: f64 = 1800.0;
pub(crate) const PIN_CTX_MIN: i64 = -600;
pub(crate) const PIN_CTX_MAX: i64 = 600;
/// 长程语境证据（整句 DP）的换算系数与上限：证据差（bit 量级）→ DP 分。
///
/// 量级：d=1 强共现（count=11）≈3.0 bit；d=5 ≈1.6 bit。K=8 时 3.0 bit → +24 分，
/// 与 `word_adjust`（双字词 +5、三字整词 +15）和 LM（−16..+32）同量级——
/// 足以翻越同音歧义段（几千词频差折算到 DP 只有个位数分），又不足以推翻
/// 结构性证据（整词/切分惩罚）。上限 24 与 LM 上限 32 对齐。
pub(crate) const LONG_RANGE_K: f64 = 8.0;
pub(crate) const LONG_RANGE_CAP: f64 = 24.0;
/// 长程证据只作用于「真同音歧义段」：组内最高/次高频比 ≤5×（与 is_ambiguous_pinyin 判定同口径）。
pub(crate) const LONG_RANGE_AMBIG_RATIO: f32 = 5.0;
/// 证据差门槛（bit）：低于此值视为噪声，不加分。
pub(crate) const LONG_RANGE_MIN_DELTA: f32 = 0.5;
/// 种子共现计数下限（压缩值，`round(log2(raw+1)*3)`）：≈ 原始 5 次。
/// 低于此值的种子搭配多为跨义项共现（「从结果出发」），不足以推翻默认读法；
/// 用户亲自打过的搭配不受此限（`user_cnt > 0` 直接采信）。
pub(crate) const LONG_RANGE_SEED_MIN_COUNT: u32 = 8;

impl Engine {
    /// 同拼音多字词组成员（freq≥地板，按频率降序，已去重）。
    fn same_pinyin_members(&self, syls: &[&str]) -> Vec<(String, u32)> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut members: Vec<(String, u32)> = Vec::new();
        for c in self.dict.query(syls) {
            let cand_syls: Vec<&str> = c.pinyin.split_whitespace().collect();
            if cand_syls != syls {
                continue;
            }
            let n = c.text.chars().count();
            if n < 2 || n != syls.len() || c.base_freq < AMBIG_FREQ_FLOOR {
                continue;
            }
            if !seen.insert(c.text.to_string()) {
                continue;
            }
            members.push((c.text.to_string(), c.base_freq));
        }
        members.sort_by_key(|(_, freq)| std::cmp::Reverse(*freq));
        members
    }

    /// 该音节序列是否构成**真同音歧义组**（≥2 个多字词、gap≤5×、地板 100）。
    ///
    /// 用途：commit 时决定是否记录上下文证据——只有真歧义组才值得记住
    /// 「用户在什么语境下选了哪个」，非歧义词记录只会污染竞争表。
    pub(crate) fn is_ambiguous_pinyin(&self, syls: &[&str]) -> bool {
        if syls.len() < 2
            || syls
                .iter()
                .any(|s| s.len() < 2 || !crate::pinyin::is_valid_syllable_pub(s))
        {
            return false;
        }
        let members = self.same_pinyin_members(syls);
        if members.len() < 2 {
            return false;
        }
        members[0].1 as f32 / members[1].1.max(1) as f32 <= AMBIG_MAX_GAP_RATIO
    }

    /// 上下文同音证据：`(pinyin, prev) → word` 的平滑份额增量 × K。
    ///
    /// 保留便捷入口供测试/后续句级使用（当前热路径走 `_with_prior` 版本）。
    #[allow(dead_code)]
    ///
    /// 零证据返回 0（调用方逐字节保持现状）。正负都可能——用户在该语境改选
    /// 其他读法时，旧词被分母稀释（上下文级退化，与全局竞争层同语义）。
    pub(crate) fn pin_ctx_boost(&self, pinyin_key: &str, word: &str, prev: &str) -> i64 {
        let prior = self.pin_prior(pinyin_key, word);
        self.pin_ctx_boost_with_prior(pinyin_key, word, prev, prior)
    }

    /// 长程语境证据：`context` 里更远的上文词与候选的共现，
    /// 按距离衰减求和（log2 压缩，单位≈bit）。
    ///
    /// 与 `context_bonus`（候选层，30 词 × 8000 量级）同源但**不共用**：
    /// 这里只服务整句 DP，且调用方用「真同音歧义段」门控——非歧义段零开销。
    /// `skip_last`：句首段跳过 LM 已消费的最近 2 词，避免重复计数；
    /// 句中段 LM 只看 DP 路径 prev1/prev2，与本函数的 `self.context` 不重叠，
    /// 因此 `skip_last=0`（把紧邻上屏词也算进来）。
    pub(crate) fn longrange_evidence(&self, text: &str, skip_last: usize) -> f32 {
        /// 扫描窗口：最近 8 个上文词（跳过 skip_last 后）。
        const WINDOW: usize = 8;
        /// 距离衰减：与候选层同款 0.85^d（d 从 skip_last 起算）。
        const DECAY: f64 = 0.85;
        if self.context.is_empty() {
            return 0.0;
        }
        let mut sum = 0.0f64;
        for (distance, prev) in self.context.iter().rev().enumerate().skip(skip_last) {
            if distance >= skip_last + WINDOW {
                break;
            }
            let weight = DECAY.powi(distance as i32);
            if weight < 0.01 {
                break;
            }
            // **单字虚词跳过**：中文单字上文多为虚词/介词，
            // 与候选的共现是句法驱动而非话题驱动——「在→出发=12」来自
            // 「在出发前」，「的→出发=10」来自「…的出发」，都不能说明
            // 「马上出发」该选出发。长程证据只采信多字内容词。
            if prev.chars().count() < 2 {
                continue;
            }
            // **不用 collocation 兜底**：`collocation_freq(prev,text)`
            // 的语义是「prev+text 能组成词库词」，与两词在句中的**实际间隔无关**；
            // 用在 d≥2 的长程上会凭空把「机场出发」当证据。长程只采信语料共现。
            let user_cnt = self.freq.user_word_bigram_count(prev, text);
            let cnt = self.freq.word_bigram_count(prev, text);
            if cnt == 0 {
                continue;
            }
            // **种子低计数门**：种子计数是 log2 压缩值，
            // 5 ≈ 原始 2-3 次，多为跨义项搭配（「从结果出发」= result 作起点，
            // 与「马上出发」= 启程无关）。只有用户亲自打过（任何计数）或
            // 种子证据足够强（≥8 ≈ 原始 5 次以上）才允许推翻默认读法。
            if user_cnt == 0 && cnt < LONG_RANGE_SEED_MIN_COUNT {
                continue;
            }
            let capped = cnt.min(self.dyn_cap_cache);
            sum += (capped + 1).ilog2() as f64 * weight;
        }
        sum as f32
    }

    /// 同上，但复用调用方已算好的先验份额（scoring 热路径避免重复
    /// `pin_priors_full`：该函数每次 pin_key 变更都要查一次字典）。
    ///
    /// **热路径守卫**：先查 `has_pin_ctx()`（O(1) 布尔），
    /// 零上下文证据时直接返回 0——避免每候选构造两次 String 的 CtxKey。
    pub(crate) fn pin_ctx_boost_with_prior(
        &self,
        pinyin_key: &str,
        word: &str,
        prev: &str,
        prior: f32,
    ) -> i64 {
        if !self.competition.has_pin_ctx() {
            return 0;
        }
        let ctx = crate::competition::CtxKey::PinCtx {
            pinyin: pinyin_key.to_string(),
            prev: prev.to_string(),
        };
        if self.competition.total_of(&ctx) == 0 {
            return 0;
        }
        let smoothed =
            self.competition
                .smoothed(&ctx, word, crate::competition::ALPHA_PIN_CTX, prior);
        ((smoothed - prior) as f64 * PIN_CTX_K).clamp(PIN_CTX_MIN as f64, PIN_CTX_MAX as f64) as i64
    }
}
