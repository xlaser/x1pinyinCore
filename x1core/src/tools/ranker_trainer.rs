//! 学习型排序器离线训练工具（pairwise logistic 回归）
//!
//! 读 ~/Library/Application Support/x1pinyin/selections.jsonl（选择日志），
//! 训练一个线性 ranker，替代 candidate_score 的手调权重。
//!
//! 模型：pairwise ranknet。
//! - 对每条日志，把「用户选的候选」与「其他每个候选」配对。
//! - 特征向量 = picked 候选特征 - 另一候选特征（差值，标准 pairwise 做法）。
//! - label = 1（picked 应排前）。学一个 logistic 权重 w，使 sigmoid(w·Δfeat) → 1。
//! - 梯度下降：w += lr * (1 - sigmoid(w·Δfeat)) * Δfeat。
//!
//! 输出：assets/ranker_weights.rs（编译期常量数组），Rust 推理直接 include!，零运行时加载。
//! 若权重文件已存在则覆盖。冷启动（无文件）回退 candidate_score 手调公式。
//!
//! 用法：
//! ```sh
//! cargo run -p x1core --bin ranker_trainer -- <selections.jsonl 路径> [输出.rs 路径]
//! ```
//! 默认输入 ~/Library/Application Support/x1pinyin/selections.jsonl
//! 默认输出 x1core/src/ranker_weights.rs

use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

/// 特征维度（与 ScoreBreakdown 的 6 个分量一致，embedding_bonus 已随后台架构移除）
/// 顺序：base, total_bonus, recent, mapping_bonus, bigram_bonus, long_word_bonus
const FEAT_DIM: usize = 6;
const FEAT_NAMES: [&str; FEAT_DIM] = [
    "base",
    "total_bonus",
    "recent",
    "mapping_bonus",
    "bigram_bonus",
    "long_word_bonus",
];

/// 单条 selection 日志解析结果
struct SelectionRecord {
    picked: usize,
    /// 每个候选的 6 维特征（按 candidates 顺序对齐 features）
    features: Vec<[f64; FEAT_DIM]>,
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let home = dirs_home();
    let input_path = if args.len() >= 2 {
        PathBuf::from(&args[1])
    } else {
        home.join("Library/Application Support/x1pinyin/selections.jsonl")
    };
    let output_path = if args.len() >= 3 {
        PathBuf::from(&args[2])
    } else {
        // 默认输出到 x1core/src/ranker_weights.rs（相对于 CARGO_MANIFEST_DIR）
        let manifest = env!("CARGO_MANIFEST_DIR");
        PathBuf::from(manifest).join("src/ranker_weights.rs")
    };

    eprintln!("输入: {}", input_path.display());
    eprintln!("输出: {}", output_path.display());

    if !input_path.exists() {
        eprintln!("❌ 输入文件不存在。请先用输入法积累 selection 日志。");
        std::process::exit(1);
    }

    // 1. 解析 jsonl
    let records = parse_jsonl(&input_path);
    eprintln!("✅ 解析 {} 条选择记录", records.len());
    if records.is_empty() {
        eprintln!("❌ 无有效记录，退出");
        std::process::exit(1);
    }

    // 2. 构造 pairwise 样本
    let samples = build_pairwise(&records);
    eprintln!("✅ 构造 {} 条 pairwise 样本", samples.len());
    if samples.is_empty() {
        eprintln!("❌ 无 pairwise 样本（可能每条都只有1个候选），退出");
        std::process::exit(1);
    }

    // 3. 训练 logistic 回归（梯度下降）
    let weights = train_logistic(&samples);
    eprintln!("✅ 训练完成，权重:");
    for (i, w) in weights.iter().enumerate() {
        eprintln!("   {:>14}: {:.6}", FEAT_NAMES[i], w);
    }

    // 4. 评估：top-1 准确率（按学到的权重重排，看 picked 是否排第一）
    let acc = evaluate_top1(&records, &weights);
    eprintln!("✅ top-1 准确率（学到的权重）: {:.1}%", acc * 100.0);

    // 对照：当前手调公式（各分量直接求和）的 top-1 准确率
    let acc_baseline = evaluate_top1_baseline(&records);
    eprintln!("   对照（手调公式求和）: {:.1}%", acc_baseline * 100.0);

    // 5. 输出 Rust 常量
    let src = format_rust_weights(&weights);
    fs::write(&output_path, src).expect("写输出文件");
    eprintln!("✅ 已输出: {}", output_path.display());
}

/// 简易手写 JSON 提取：从 jsonl 每行解析 picked 和 features。
/// 避免引入 serde 依赖（x1core 只依赖 rusqlite）。
fn parse_jsonl(path: &PathBuf) -> Vec<SelectionRecord> {
    let file = fs::File::open(path).expect("打开 jsonl");
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rec) = parse_one(line) {
            records.push(rec);
        }
    }
    records
}

/// 解析单行 JSON：提取 picked（int）和 features 数组里每个候选的 6 维特征。
fn parse_one(line: &str) -> Option<SelectionRecord> {
    let picked = extract_int(line, "\"picked\":")?;
    // features 是 [{text, features:{base:N,...}}, ...]，提取每个候选的 6 个数
    let features = extract_features_array(line);
    if features.is_empty() {
        return None;
    }
    Some(SelectionRecord { picked, features })
}

/// 从 json 文本里找 "key": 后面的整数
fn extract_int(s: &str, key: &str) -> Option<usize> {
    let idx = s.find(key)?;
    let rest = &s[idx + key.len()..];
    let num_str: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    num_str.parse().ok()
}

/// 提取所有候选的特征数组：扫描 "base":N,"total_bonus":N,... 序列
fn extract_features_array(s: &str) -> Vec<[f64; FEAT_DIM]> {
    let mut result = Vec::new();
    // 用 features 键内每个 base 出现位置定位一个候选
    let mut search_from = 0;
    while let Some(base_idx) = s[search_from..].find("\"base\":") {
        let abs = search_from + base_idx;
        let mut feats = [0.0f64; FEAT_DIM];
        for (i, name) in FEAT_NAMES.iter().enumerate() {
            let key = format!("\"{}\":", name);
            if let Some(v) = extract_num_near(s, abs, &key) {
                feats[i] = v;
            }
        }
        result.push(feats);
        search_from = abs + 6;
    }
    result
}

/// 从 start 附近向后找 key 对应的数值（容差窗口内）
///
/// 窗口 400 字符：6 维特征在最坏情况下（每个 u64 值 20 位）
/// 单候选 JSON 可达 ~220 字符，200 窗口会截断最后的字段导致静默填 0。
fn extract_num_near(s: &str, start: usize, key: &str) -> Option<f64> {
    let window = &s[start..s.len().min(start + 400)];
    let idx = window.find(key)?;
    let rest = &window[idx + key.len()..];
    let num_str: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    num_str.parse().ok()
}

/// 构造 pairwise 样本：(Δfeat, label=1)，Δfeat = picked_feat - other_feat
fn build_pairwise(records: &[SelectionRecord]) -> Vec<([f64; FEAT_DIM], f64)> {
    let mut samples = Vec::new();
    for rec in records {
        if rec.picked >= rec.features.len() {
            continue;
        }
        let picked_feat = rec.features[rec.picked];
        for (i, other_feat) in rec.features.iter().enumerate() {
            if i == rec.picked {
                continue;
            }
            let mut delta = [0.0f64; FEAT_DIM];
            for d in 0..FEAT_DIM {
                delta[d] = picked_feat[d] - other_feat[d];
            }
            samples.push((delta, 1.0));
        }
    }
    samples
}

/// logistic 回归梯度下降
#[allow(unused_assignments, unused_variables)]
fn train_logistic(samples: &[([f64; FEAT_DIM], f64)]) -> [f64; FEAT_DIM] {
    let mut w = [0.0f64; FEAT_DIM];
    let lr = 0.000001; // 学习率（特征值量级大，用小 lr）
    let epochs = 200;
    for _epoch in 0..epochs {
        let mut grad = [0.0f64; FEAT_DIM];
        let mut loss = 0.0;
        for (delta, label) in samples {
            let dot = (0..FEAT_DIM).map(|d| w[d] * delta[d]).sum::<f64>();
            let pred = sigmoid(dot);
            let err = pred - label;
            for d in 0..FEAT_DIM {
                grad[d] += err * delta[d];
            }
            // cross-entropy loss（监控用）
            loss -= label * pred.ln().max(-50.0) + (1.0 - label) * (1.0 - pred).ln().max(-50.0);
        }
        for d in 0..FEAT_DIM {
            w[d] -= lr * grad[d] / samples.len() as f64;
        }
    }
    w
}

fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// 用学到的权重评估 top-1 准确率：按 w·feat 重排，picked 是否排第一
fn evaluate_top1(records: &[SelectionRecord], w: &[f64; FEAT_DIM]) -> f64 {
    let mut correct = 0usize;
    let mut total = 0usize;
    for rec in records {
        if rec.picked >= rec.features.len() || rec.features.len() < 2 {
            continue;
        }
        total += 1;
        let mut best_score = f64::MIN;
        let mut best_idx = 0;
        for (i, feat) in rec.features.iter().enumerate() {
            let score = (0..FEAT_DIM).map(|d| w[d] * feat[d]).sum::<f64>();
            if score > best_score {
                best_score = score;
                best_idx = i;
            }
        }
        if best_idx == rec.picked {
            correct += 1;
        }
    }
    if total == 0 {
        0.0
    } else {
        correct as f64 / total as f64
    }
}

/// 对照：当前手调公式（直接求和）的 top-1 准确率
fn evaluate_top1_baseline(records: &[SelectionRecord]) -> f64 {
    let mut correct = 0usize;
    let mut total = 0usize;
    for rec in records {
        if rec.picked >= rec.features.len() || rec.features.len() < 2 {
            continue;
        }
        total += 1;
        let mut best_score = f64::MIN;
        let mut best_idx = 0;
        for (i, feat) in rec.features.iter().enumerate() {
            let score: f64 = feat.iter().sum();
            if score > best_score {
                best_score = score;
                best_idx = i;
            }
        }
        if best_idx == rec.picked {
            correct += 1;
        }
    }
    if total == 0 {
        0.0
    } else {
        correct as f64 / total as f64
    }
}

/// 格式化为 Rust 函数源码（明文字面量数组）。
fn format_rust_weights(w: &[f64; FEAT_DIM]) -> String {
    let mut s = String::from("// 自动生成（ranker_trainer）。勿手动编辑。\n");
    s.push_str(
        "// 学习型排序器权重：candidate_score 用 dot(features, ranker_weights()) 替代手调求和。\n",
    );
    s.push_str("// 冷启动（无此文件或权重全0）时回退 candidate_score 手调公式。\n\n");
    s.push_str("pub fn ranker_weights() -> [f64; 6] {\n");
    s.push_str("    [\n");
    for (i, name) in FEAT_NAMES.iter().enumerate() {
        s.push_str(&format!("        {:.8},  // {}\n", w[i], name));
    }
    s.push_str("    ]\n");
    s.push_str("}\n");
    s
}

fn dirs_home() -> PathBuf {
    if let Ok(h) = env::var("HOME") {
        return PathBuf::from(h);
    }
    PathBuf::from(".")
}
