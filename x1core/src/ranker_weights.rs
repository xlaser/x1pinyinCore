// 学习型排序器权重（由 tools/ranker_trainer 离线训练固化）。
// 当权重非全 0 时，candidate_score 用 dot(features, ranker_weights()) 替代手调求和。
// 6 维（与 candidate_score 的 6 分量一一对应）。

pub fn ranker_weights() -> [f64; 6] {
    [
        0.00010595,  // base
        -0.00004267, // total_bonus
        0.00198046,  // recent
        0.00155019,  // mapping_bonus
        0.00275236,  // bigram_bonus
        0.00002504,  // long_word_bonus
    ]
}
