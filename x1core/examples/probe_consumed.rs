//! 混合拼（全拼+简拼声母）候选消费探针：对齐服务层口径
//! （ns = 候选字数 → Engine::syllable_prefix_lens 权威值），
//! 诊断「声母尾段拆分导致残留」类问题（guangzh → 广州 应全消费 7）。
//!
//! 用法：cargo run -p x1core --release --example probe_consumed -- [db] [input...]

use x1core::engine::Engine;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "x1core/assets/eval/pristine_seed.db".into());
    let inputs: Vec<String> = if args.len() > 2 {
        args[2..].to_vec()
    } else {
        vec![
            "guangzh", "zhonggr", "sh", "ch", "zh", "xiazh", "jiegw", "sjh", "zaijianh", "guangzho",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    };
    let engine = Engine::new_with_db(&db).expect("open db");
    for input in &inputs {
        let cands = engine.process(input);
        let consumed = engine.consumed_lens(input, &cands);
        let display = engine.syllable_segmentation(input).join("'");
        println!("== {input} (len {}) | 显示切分 [{display}] ==", input.len());
        for (i, c) in cands.iter().enumerate().take(5) {
            let mark = if consumed[i] == input.len() {
                ""
            } else {
                " <-- 残留"
            };
            println!(
                "  {} | py={} | consumed={}{}",
                c.text, c.pinyin, consumed[i], mark
            );
        }
    }
}
