//! 候选装配探针：打印指定输入的前 N 个候选（文本/拼音/标志/分数），
//! 诊断「前缀候选压整句」类装配层排序问题。
//!
//! 用法：cargo run -p x1core --release --example probe_cands -- [db] [n] [input...]
//! 默认 DB = assets/eval/pristine_seed.db（冷启动种子），n 默认 12。
//!
//! 只读评测：给它的 DB 应是副本（temp 拷贝）——
//! Engine::new_with_db 会把 journal 转 WAL。

use x1core::engine::Engine;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "x1core/assets/eval/pristine_seed.db".into());
    let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(12);
    let inputs: Vec<String> = if args.len() > 3 {
        args[3..].to_vec()
    } else {
        vec!["wohaoxiangchifan".into()]
    };
    let engine = match Engine::new_with_db(&db) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("打开 DB 失败 {db}: {e}");
            std::process::exit(1);
        }
    };
    for input in &inputs {
        let cands = engine.process(input);
        println!("== {input}（{} 个候选）==", cands.len());
        for (i, c) in cands.iter().enumerate().take(n) {
            let flags = format!(
                "{}{}{}{}{}",
                if c.is_sentence { "句" } else { "-" },
                if c.is_guess { "猜" } else { "-" },
                if c.is_correction { "纠" } else { "-" },
                if c.is_shortcut { "快" } else { "-" },
                if c.is_primary { "主" } else { "-" },
            );
            let score = engine.candidate_score_for_test(input, c);
            println!(
                "{:>2}. {} | py={} | freq={} | {} | score={}",
                i + 1,
                c.text,
                c.pinyin,
                c.base_freq,
                flags,
                score
            );
        }
    }
}
