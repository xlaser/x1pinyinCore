//! 同音歧义探针：冷启动引擎跑一组同音易混句，打印 Top1 与命中情况。
//!
//! 用法：
//! ```bash
//! cargo run -p x1core --release --example probe_ambig -- <db_path> <cases_tsv>
//! ```
//! 默认 DB = assets/eval/pristine_seed.db（冷启动种子），默认用例 = assets/eval/ambig_cases.tsv。
//! 用例格式：category\tinput\tpexpected（# 注释、空行跳过，与 eval/sentences.tsv 同构）。
//!
//! ⚠️ 只读评测：给它的 DB 应是副本（temp 拷贝），不要指向还要复用的库
//！——Engine::new_with_db 会把 journal 转 WAL。

use std::io::{BufRead, BufReader};
use x1core::engine::Engine;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "x1core/assets/eval/pristine_seed.db".into());
    let cases_path = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "x1core/assets/eval/ambig_cases.tsv".into());

    let engine = match Engine::new_with_db(&db) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("打开 DB 失败 {db}: {e}");
            std::process::exit(1);
        }
    };

    let f = match std::fs::File::open(&cases_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("用例读取失败 {cases_path}: {e}");
            std::process::exit(1);
        }
    };

    let mut hit = 0usize;
    let mut total = 0usize;
    let mut by_cat: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
    for line in BufReader::new(f).lines() {
        let line = line.unwrap();
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 3 {
            continue;
        }
        let (cat, input, expected) = (cols[0], cols[1], cols[2]);
        let cands = engine.process(input);
        let top1 = cands
            .first()
            .map(|c| c.text.to_string())
            .unwrap_or_default();
        let in3 = cands.iter().take(3).any(|c| c.text.as_ref() == expected);
        let is_hit = top1 == expected;
        total += 1;
        if is_hit {
            hit += 1;
        }
        let e = by_cat.entry(cat.to_string()).or_default();
        e.0 += 1;
        if is_hit {
            e.1 += 1;
        }
        let tag = if is_hit {
            "HIT "
        } else if in3 {
            "T3  "
        } else {
            "MISS"
        };
        println!(
            "{tag}\t{cat}\t{input}\t{expected}\t{top1}\t{}",
            if in3 {
                String::new()
            } else {
                cands
                    .iter()
                    .take(3)
                    .map(|c| c.text.to_string())
                    .collect::<Vec<_>>()
                    .join(" | ")
            }
        );
    }
    for (cat, (t, h)) in &by_cat {
        println!(
            "---- {cat}: {h}/{t} ({:.1}%)",
            100.0 * *h as f64 / *t as f64
        );
    }
    println!(
        "==== Top1 {hit}/{total} ({:.1}%) ====",
        100.0 * hit as f64 / total.max(1) as f64
    );
}
