//! 条件 DP 统计先验：三元插值语言模型。
//!
//! 替代 DP 转移打分里的手调加法先验（freq_score + context_bonus_dp），
//! 用归一化条件概率衡量「上文对候选的选择性」而非绝对共现次数——
//! 修复 raw-count 框架下高频弱指向搭配错配（有则/着个/台热了）的结构性缺陷。
//!
//! 分层：
//! - `interpolated_logp` / `map_lm_score`：纯函数，无 Engine 依赖，独立单测。
//! - `Engine::lm_prior_score`：查询编排（用户硬覆盖 → 种子条件概率 → ε 地板）。

use alloc::string::String;

/// unigram 分母：种子 x1dict_words 的 SUM(freq) 快照（约 186,581 词）。
/// 近似值即可——常数偏差对 logP 是整体平移，被 LM_FLOOR 定标吸收，
/// 词库扩容后无需同步更新（若规模翻倍以上再刷新）。
pub(crate) const TOTAL_FREQ: f64 = 159_574_919.0;

/// 插值权重（λ1 unigram / λ2 bigram / λ3 trigram）。
/// 离线定标结论：λ1↑λ3↓ 方向单调改善——trigram 表约 100k 条较稀疏（λ3 高 →
/// 噪声），unigram 是冷启动最稳的先验；λ1 过高则退回 freq 主导（同音字尾
/// 切分错误回归），0.6/0.3/0.1 为峰值。
pub(crate) const LM_LAMBDA1: f64 = 0.6;
pub(crate) const LM_LAMBDA2: f64 = 0.3;
pub(crate) const LM_LAMBDA3: f64 = 0.1;

/// 平滑地板：未命中的阶按 ε 计（log2(ε) = -20），防止 log2(0) 未定义。
/// 与 context_bonus 系的 log2(c+1) 平移不同——插值 LM 需要「未命中也要有负贡献」，
/// 否则未命中阶 log2(0+1)=0 反而变成满分。
pub(crate) const LM_EPS: f64 = 1.0 / 1_048_576.0; // 2^-20

/// logP → DP seg 分映射参数（离线定标：K=3.0、FLOOR=-14 为评测峰值，
/// 过低 LM 对比度丢失，过高 churn 放大）。
pub(crate) const LM_FLOOR: f64 = -14.0;
pub(crate) const LM_K: f64 = 3.0;
/// 下界 -16：**允许负分**。联合概率 P(path)=ΠP(seg|ctx)
/// 的对数可加性要求弱段扣分——多切一段就多一份负贡献，自然惩罚过度切分。
/// 若把弱段 clamp 成 0 会丢失该性质：单字（freq 高、bigram 强）的 LM 分
/// 系统性压过多字词，DP 最优路径退化成单字链。
/// 上界 32 保持与旧 freq_score(0-16)+context_bonus(0-16) 之和同量级。
pub(crate) const LM_MIN: i32 = -16;
pub(crate) const LM_MAX: i32 = 32;

/// 三元插值 log 概率（按可用阶归一化）。
///
/// **为什么归一化而非固定 λ + ε 地板**：trigram 表只有 100k 条，绝大多数转移
/// P3 未命中。固定 λ 下未命中阶贡献 λ3×log2(ε)=-10 的**平坦常数**——所有段一起
/// 被压到 FLOOR 附近，区分度被常数吃掉。
/// 归一化（deleted interpolation 思想）：未命中的阶不参与，权重按命中阶重新分配，
/// logP = Σλᵢ·log2(Pᵢ) / Σλᵢ（仅 Pᵢ>0 的阶）。P1 unigram 恒可用（base_freq+1>0），
/// 分母永不为 0（防御仍保留）。
pub(crate) fn interpolated_logp(p1: f64, p2: f64, p3: f64) -> f64 {
    let mut num = 0.0;
    let mut den = 0.0;
    if p3 > 0.0 {
        num += LM_LAMBDA3 * p3.max(LM_EPS).log2();
        den += LM_LAMBDA3;
    }
    if p2 > 0.0 {
        num += LM_LAMBDA2 * p2.max(LM_EPS).log2();
        den += LM_LAMBDA2;
    }
    if p1 > 0.0 {
        num += LM_LAMBDA1 * p1.max(LM_EPS).log2();
        den += LM_LAMBDA1;
    }
    if den == 0.0 {
        return LM_EPS.log2();
    }
    num / den
}

/// logP → DP seg 分：clamp(round(K × (logP − FLOOR)), LM_MIN, LM_MAX)。
/// 负分有效（见 LM_MIN 注释）：弱先验段扣分是联合概率切分惩罚的来源。
pub(crate) fn map_lm_score(logp: f64) -> i32 {
    ((logp - LM_FLOOR) * LM_K)
        .round()
        .clamp(LM_MIN as f64, LM_MAX as f64) as i32
}

/// collocation 伪 count 的分母下限。
///
/// 种子 bigram 未命中时回退 dict.collocation_freq 伪 count（clamp 1..8）。
/// 若 prev 不在种子表（total=0），直接相除会得到伪 count/1 = 虚高条件概率，
/// 用 64 作为「典型出度总质量」下限压住（种子 bigram 平均 count ≈ 12.6，
/// 出度≥5 的 prev total 普遍 >60）。
pub(crate) const COLLOCATION_TOTAL_FLOOR: u64 = 64;

/// 用户证据饱和常数（贝叶斯融合）：
/// 用户 bigram/trigram 命中时的概率按 Beta-Bernoulli 后验均值
/// count/(count+K) 计（与种子概率取 max），替代满概率 1.0 硬覆盖。
/// 数学性质：count=0 零影响；count=1 → 0.25（温和，单次误选不瞬间翻转
/// 排序）；K=3 次过半；9 次 0.75；饱和逼近 1 但保留种子强统计的保底
///（P_seed 高时不被拉低）。K=3 偏小是刻意取值——用户词库词频
/// 虽不高但都是真人打的，具备高价值，话语权爬升要快。
pub(crate) const USER_EVIDENCE_K: f64 = 3.0;

impl crate::engine::Engine {
    /// DP 转移的 LM 先验分（替代旧 freq_score + context_bonus_dp）。
    ///
    /// `ctx`：上文切片，最后一个元素 = prev1（紧邻上文），倒数第二 = prev2。
    /// 空 ctx（句首段/简拼句首）→ P2=P3=ε，退化纯 unigram 先验。
    ///
    /// **用户硬覆盖**（「用户 > 种子」语义）：任一阶用户表命中
    /// （user word_bigram / user trigram count>0）→ 该阶概率按 1.0 计
    /// （log2(1)=0，该阶贡献拉满），完全不看种子。
    ///
    /// **Markov 假设**：只看紧邻 prev1/prev2，不遍历整个 context 栈
    /// （旧 context_bonus_dp 的 0.85 距离衰减多 prev 累加被插值替代）。
    /// 句首段的跨句衔接由调用方把 engine.context 作 ctx 传入保留。
    pub(crate) fn lm_prior_score(&self, ctx: &[String], text: &str, base_freq: u32) -> i32 {
        // P1 unigram 先验：词库 freq 占比（+1 平滑，freq=0 也有小正值）
        let p1 = (base_freq as f64 + 1.0) / (TOTAL_FREQ + 1.0);

        let prev1 = ctx.last().map(|s| s.as_str());
        let prev2 = if ctx.len() >= 2 {
            ctx.get(ctx.len() - 2).map(|s| s.as_str())
        } else {
            None
        };

        // P2 一阶条件概率
        let p2 = match prev1 {
            None => 0.0,
            Some(p1word) => {
                // 种子侧概率（bigram → collocation 伪 count → 0）
                let p_seed = {
                    let (raw, total) = self.freq.seed_bigram_count_and_total(p1word, text);
                    if raw > 0 {
                        (raw as f64 / total.max(raw as u64) as f64).min(1.0)
                    } else {
                        // 种子未命中 → collocation 伪 count 保底（上文+候选成词库词）
                        // 注：DP 路径暂不传候选拼音（curr_pinyin=""），跳过读音校验。
                        // 整句解码的候选由音节切分产生，读音与切分基本一致，误匹配风险低。
                        match self.dict.collocation_freq(p1word, text, "") {
                            Some(f) => {
                                let pseudo =
                                    (((f as f64).log2().max(0.0) / 2.0).round() as u64).clamp(1, 8);
                                pseudo as f64 / COLLOCATION_TOTAL_FLOOR as f64
                            }
                            None => 0.0,
                        }
                    }
                };
                // 用户证据贝叶斯融合（替代满概率 1.0 硬覆盖）：Beta 后验饱和
                // count/(count+USER_EVIDENCE_K) 与种子概率取 max——count=0 时
                // p_user=0，max 退回纯种子（行为不变）；单次行为温和提升不瞬间
                // 翻转；种子强统计不被拉低。
                // TODO：P3（trigram）通道同款融合——表稀疏大多 miss，暂不动。
                let ucount = self.freq.user_word_bigram_count(p1word, text) as f64;
                let p_user = ucount / (ucount + USER_EVIDENCE_K);
                p_user.max(p_seed)
            }
        };

        // P3 二阶条件概率（仅 prev2 存在时查询，与旧 trigram 升级守卫一致）
        let p3 = match (prev2, prev1) {
            (Some(p2word), Some(p1word)) => {
                if self.trigram.count(p2word, p1word, text) > 0 {
                    // 用户硬覆盖（trigram_count_with_seed 的用户部分）
                    1.0
                } else {
                    // 走 trigram_count_with_seed 吃桶级会话缓存（避免每 DP 转移
                    // 一条 SQL）。else 分支里用户表必为 0，内部的用户优先分支
                    // 不会命中，返回值与直查种子等价。
                    let t = self.trigram_count_with_seed(p2word, p1word, text);
                    if t > 0 {
                        let total = self.trigram_context_total_cached(p2word, p1word);
                        (t as f64 / total.max(t as u64) as f64).min(1.0)
                    } else {
                        0.0
                    }
                }
            }
            _ => 0.0,
        };

        map_lm_score(interpolated_logp(p1, p2, p3))
    }

    /// trigram 上下文总次数（P3 分母），带会话级缓存防同句重复 SQL。
    ///
    /// 缓存策略与 green_memo 同款：HashMap + 512 上限，满即整体 clear
    /// （会话级缓存，clear 后 SQL 按需重建，无正确性影响）。
    /// RefCell 内部可变性（engine 只在主线程使用）。
    fn trigram_context_total_cached(&self, prev2: &str, prev1: &str) -> u64 {
        let key = (prev2.to_string(), prev1.to_string());
        if let Some(&v) = self.trigram_total_cache.borrow().get(&key) {
            return v;
        }
        let v = self
            .storage
            .as_ref()
            .map(|s| s.seed_trigram_context_total(prev2, prev1) as u64)
            .unwrap_or(0);
        let mut cache = self.trigram_total_cache.borrow_mut();
        if cache.len() >= 512 {
            cache.clear();
        }
        cache.insert(key, v);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interpolated_logp_full_hit() {
        // 三阶全满概率 → logP = 0（log2(1)=0）
        assert_eq!(interpolated_logp(1.0, 1.0, 1.0), 0.0);
    }

    #[test]
    fn test_interpolated_logp_eps_floor() {
        // 全未命中 → λ 加权的 log2(ε) = -20（λ 和为 1）
        let logp = interpolated_logp(0.0, 0.0, 0.0);
        assert!(
            (logp - (-20.0)).abs() < 1e-9,
            "全 miss 应 = -20，实际 {}",
            logp
        );
    }

    #[test]
    fn test_interpolated_logp_higher_order_sharper() {
        // 同概率下，命中高阶（λ3 大）比只命中低阶 logP 更高
        let tri = interpolated_logp(0.001, 0.01, 0.5);
        let uni_only = interpolated_logp(0.001, 0.0, 0.0);
        assert!(tri > uni_only);
    }

    #[test]
    fn test_map_lm_score_bounds() {
        assert_eq!(map_lm_score(0.0), LM_MAX, "logP=0（满概率）应顶格");
        assert_eq!(
            map_lm_score(-100.0),
            LM_MIN,
            "极弱先验应压到下界（负分有效）"
        );
        assert_eq!(map_lm_score(LM_FLOOR), 0, "FLOOR 处恰好 0 分");
        // 单调性：logP 越高分越高
        assert!(map_lm_score(-4.0) > map_lm_score(-12.0));
        // 弱于 FLOOR 的先验得负分（联合概率切分惩罚的来源）
        assert!(map_lm_score(-20.0) < 0);
    }

    #[test]
    fn test_lm_score_typical_sentence_word() {
        // 典型句中词：unigram 中频（freq≈1000 → P1≈6e-6 → log2≈-17.3），
        // bigram 强条件（P2=0.1 → -3.3），trigram 未命中（ε → -20）。
        // logP = 0.5×(-20) + 0.3×(-3.3) + 0.2×(-17.3) ≈ -14.5 → 分 ≈ 6
        let logp = interpolated_logp(1000.0 / TOTAL_FREQ, 0.1, 0.0);
        let score = map_lm_score(logp);
        assert!((0..=LM_MAX).contains(&score));
        assert!(score > 0, "有 bigram 条件支撑的词应有正分，实际 {}", score);
    }
}
