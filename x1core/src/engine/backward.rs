//! 双向词格重打分：用右文（后向 bigram）对整句 N-best 路径二次打分。
//!
//! 前向 DP 的统计先验（插值 LM / collocation / trigram）只看左上文，而
//! 拼音串是打完整段才上屏，右文信息被整体丢弃；功能词歧义恰由右文决定：
//! 「认真地考虑」（地+动词）vs「认真的考虑」、「吃饭吧」（吧=句末）vs
//! 「吃饭把」、「你更看好哪个」（哪+疑问语境）。
//!
//! 机制：对同一份 seed_bigram_counts 换反向归一化视角——
//! P(left|right) = count(left,right) / next_total(right)。
//! 前向同分无法区分的同音对，后向分母可拉开判别力（如 next_total(地)
//! 远小于 next_total(的)，同一份计数、零新增语料）。
//!
//! 集成位（保守）：N-best 回溯之后、绿词排序之前——不进 DP 转移、不动
//! 切分门槛与分层排序。每对打分复用前向插值 LM 的插值归一（未命中阶
//! 不参与 → 覆盖洞优雅退化为 unigram 先验，无 ε 重罚）与 map_lm_score
//! 映射；路径级总增量经 κ_b 缩放 + 钳幅。
//!
//! 守卫：仅全拼路径（简拼保守处理：不引入任何上下文引导）；全程整数分数
//! 运算（跨平台逐位确定，eval 可复现）。

use alloc::string::String;

use crate::lm::{interpolated_logp, map_lm_score, TOTAL_FREQ, USER_EVIDENCE_K};

/// κ_b = BACKWARD_K_NUM / BACKWARD_K_DEN：路径后向总分并进路径分的比例，
/// 取 1/2（由参数扫描定标）。
pub(crate) const BACKWARD_K_NUM: i64 = 1;
pub(crate) const BACKWARD_K_DEN: i64 = 2;

/// 路径级后向总增量钳幅 ±12（≈4 个映射分 ≈1.4 bit）：
/// 足以翻正局部同音对（的/地、吧/把一类），不足以掀翻结构级差距——
/// 与「freq 差 >20× ≈ 13 分不可翻盘」的量级对齐。
pub(crate) const BACKWARD_CAP: i32 = 12;

impl crate::engine::Engine {
    /// 单对后向分（纯函数式查询，无状态变更）：右文证据相对 unigram 基线
    /// 的提升量（lift）。
    ///
    /// `lift = map(interpolated(p1, p_b, 0)) − map(interpolated(p1, 0, 0))`
    ///
    /// 为什么必须扣基线：unigram 分量在前向插值 LM 已每词计过一次，后向再计
    /// 一次 = 路径按对数累加重复词频分——切得越碎（单字链）对的数目越多、
    /// 白赚越多，DP 被系统性推向过度切分（与 lm.rs LM_MIN 的单字链压制
    /// 同族）。扣除基线后：
    /// - 对缺失（p_b=0）→ lift ≡ 0：无右文证据零贡献（覆盖洞不奖不罚，
    ///   绝无 ε 重罚）；
    /// - 支持性右文（P(left|right) > P(left)）→ 正；
    /// - 反证性右文（P(left|right) < P(left)，如 (吃饭,把)）→ 负。
    /// 这才是「右文说了什么」的纯信号。
    ///
    /// - `p1`：left 的 unigram 先验（与前向插值 LM 同式，+1 平滑）；
    /// - `p_b`：用户贝叶斯饱和 `count/(count+K)` 与种子反向条件概率取 max
    ///   （「用户证据 > 种子证据」的优先级语义，与前向的 P2 通道一致）；
    /// - P3 恒 0（后向 trigram 需要 (right2,right1) 反向键序的种子表，
    ///   暂不建，插值归一自动跳过该阶）。
    fn backward_pair_score(&self, left: &str, right: &str, left_freq: u32) -> i32 {
        let p1 = (left_freq as f64 + 1.0) / (TOTAL_FREQ + 1.0);
        // 对计数与 forward 共用同一查询（含 equiv_prev 泛化半权语义）；
        // 反向边际查 next_total(right)。
        let (raw, _) = self.freq.seed_bigram_count_and_total(left, right);
        let p_seed = if raw > 0 {
            let nt = self.freq.seed_bigram_next_total(right);
            (raw as f64 / nt.max(raw as u64) as f64).min(1.0)
        } else {
            0.0
        };
        let ucount = self.freq.user_word_bigram_count(left, right) as f64;
        let p_user = ucount / (ucount + USER_EVIDENCE_K);
        let p_b = p_user.max(p_seed);
        map_lm_score(interpolated_logp(p1, p_b, 0.0))
            - map_lm_score(interpolated_logp(p1, 0.0, 0.0))
    }

    /// 路径级后向重打分增量（已 κ_b 缩放 + 钳幅）。
    ///
    /// 对 words[0..m-1] 的每个相邻对 (w_i, w_{i+1}) 累计
    /// `backward_pair_score(w_i, w_{i+1})`（= log P(w_i | w_{i+1}) 的映射分），
    /// 末词只作为右文出现、自身不作为 left 计分（无右文）。
    /// 词数 <2 或 freqs 长度不符 → 0（防御，恒不改变现状）。
    pub(crate) fn backward_path_delta(&self, words: &[String], freqs: &[u32]) -> i32 {
        if words.len() < 2 || freqs.len() != words.len() {
            return 0;
        }
        let mut sum: i64 = 0;
        for i in 0..words.len() - 1 {
            sum += self.backward_pair_score(&words[i], &words[i + 1], freqs[i]) as i64;
        }
        ((sum * BACKWARD_K_NUM) / BACKWARD_K_DEN).clamp(
            -(BACKWARD_CAP as i64),
            BACKWARD_CAP as i64,
        ) as i32
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::tests_query::{seed_copy, seed_copy_cleanup};

    /// 真种子副本上的机制验证（数字随种子漂移，只断言方向与量级关系，
    /// 与 tests_longrange 同款纪律）：(认真,地) 与 (认真,的) 计数接近时，
    /// 后向分必须拉开差距（next_total(的) 巨大稀释 vs next_total(地) 集中）。
    /// 场景数据复刻真实种子的形态见 tests_backward.rs；本文件仅真种子
    /// 机制方向验证。
    #[cfg(test)]
    impl crate::engine::Engine {
        /// 合成数据单测入口（CI 稳定，不依赖种子内容）。
        pub(crate) fn backward_pair_score_for_test(
            &self,
            left: &str,
            right: &str,
            left_freq: u32,
        ) -> i32 {
            self.backward_pair_score(left, right, left_freq)
        }
    }

    #[test]
    fn test_backward_sharpens_de_di_on_real_seed() {
        let db = seed_copy("backward_dedi");
        let engine = crate::engine::Engine::new_with_db(&db).expect("open seed copy");
        let (raw_de, _) = engine.freq.seed_bigram_count_and_total("认真", "地");
        let (raw_di, _) = engine.freq.seed_bigram_count_and_total("认真", "的");
        if raw_de == 0 && raw_di == 0 {
            seed_copy_cleanup(&db);
            return; // 种子换代后两对都缺，跳过（不作为回归判据）
        }
        let s_de = engine.backward_pair_score("认真", "地", 3000);
        let s_di = engine.backward_pair_score("认真", "的", 3000);
        assert!(
            s_de >= s_di,
            "后向分应不偏好「的」：地={} 的={} (raw {}/{})",
            s_de,
            s_di,
            raw_de,
            raw_di
        );
        seed_copy_cleanup(&db);
    }
}
