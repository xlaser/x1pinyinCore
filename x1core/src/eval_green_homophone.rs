//! 同音绿词评测。
//!
//! 目标：同一拼音串的绿词候选里，正句是否压过同音替换负样本。
//! 用 `assets/eval/green_homophone.tsv`（single 单字口语 + mixed PFR 句），
//! 只测量不断言阈值；A/B 用 `X1_EVAL_DB` 指向无 POS 表的副本即可关掉语法层。
//!
//! 运行：
//! `cargo test -p x1core --release --lib green_homophone_eval -- --nocapture`

use crate::engine::Engine;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::sync::OnceLock;

const SEED_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
const CASES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/eval/green_homophone.tsv"
);

fn eval_db_path() -> String {
    std::env::var("X1_EVAL_DB").unwrap_or_else(|_| SEED_DB.to_string())
}

/// 只读副本（VACUUM INTO）；语义同 eval_baseline/eval_homophone。
fn isolated_db_path() -> String {
    static COPY: OnceLock<String> = OnceLock::new();
    COPY.get_or_init(|| {
        let src = eval_db_path();
        // 独立子目录（不是 test-tmp 顶层）：微型模型按「DB 同目录」发现，
        // 顶层伴随文件会被 seed_copy 的裸文件副本互见，污染其它并行测试。
        let dir = crate::test_tmp::dir().join(format!(
            "x1_green_homophone_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create eval dir");
        let dst = dir.join("eval.db");
        let dst_str = dst.to_string_lossy().into_owned();
        let copied =
            rusqlite::Connection::open_with_flags(&src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .and_then(|c| c.execute("VACUUM INTO ?1", rusqlite::params![&dst_str]))
                .is_ok();
        if copied {
            // 微型语法模型按「DB 同目录」发现；评测副本默认在 test-tmp，需要显式带过去。
            if let Ok(src_model) = std::env::var("X1_EVAL_GRAMMAR_MODEL") {
                let dst_model = dst.with_file_name("x1grammar.bin");
                if std::fs::copy(&src_model, &dst_model).is_ok() {
                    eprintln!(
                        "[green_homophone] 微型语法模型副本 → {}",
                        dst_model.display()
                    );
                }
            }
            eprintln!("[green_homophone] 源 DB 只读副本 → {dst_str}");
            dst_str
        } else {
            eprintln!("[green_homophone] 警告：副本创建失败，直接使用源 DB {src}");
            src
        }
    })
    .clone()
}

struct Case {
    subset: String,
    input: String,
    positive: String,
    negatives: Vec<String>,
}

#[derive(Default)]
struct Stats {
    n: usize,
    pos1: usize,
    pos3: usize,
    pos_any: usize,
    neg1: usize,
    neg3: usize,
    neg_any: usize,
}

impl Stats {
    fn add(
        &mut self,
        pos1: bool,
        pos3: bool,
        pos_any: bool,
        neg1: bool,
        neg3: bool,
        neg_any: bool,
    ) {
        self.n += 1;
        self.pos1 += pos1 as usize;
        self.pos3 += pos3 as usize;
        self.pos_any += pos_any as usize;
        self.neg1 += neg1 as usize;
        self.neg3 += neg3 as usize;
        self.neg_any += neg_any as usize;
    }
}

fn load_cases() -> Vec<Case> {
    let f = std::fs::File::open(CASES).expect("open green_homophone.tsv");
    let mut out = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = line.expect("read green_homophone.tsv");
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 4 {
            continue;
        }
        out.push(Case {
            subset: cols[0].to_string(),
            input: cols[1].to_string(),
            positive: cols[2].to_string(),
            negatives: cols[3].split('|').map(|s| s.to_string()).collect(),
        });
    }
    out
}

#[test]
fn green_homophone_eval() {
    let cases = load_cases();
    assert!(!cases.is_empty(), "评测集为空");
    let mut engine = match Engine::new_with_db(&isolated_db_path()) {
        Ok(e) => e,
        Err(e) => panic!("打开评测 DB 失败: {e}"),
    };
    engine.set_smart_word_zh_enabled(false);
    if std::env::var("X1_EVAL_GRAMMAR_MICRO")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        engine.set_grammar_micro_enabled(true);
        assert!(
            engine.grammar_micro_info().is_some(),
            "X1_EVAL_GRAMMAR_MICRO=1 但模型未加载（检查 X1_EVAL_GRAMMAR_MODEL）"
        );
        eprintln!("[green_homophone] 微型语法模型：开");
    }

    let mut by_subset: HashMap<String, Stats> = HashMap::new();
    let mut total = Stats::default();
    let mut shown = 0usize;
    for case in &cases {
        // 每例独立：不让上一句的上下文影响下一句的变体/排序。
        engine.clear_context();
        let cands = engine.process(&case.input);
        let top: Vec<&str> = cands.iter().take(3).map(|c| c.text.as_ref()).collect();
        let all: Vec<&str> = cands.iter().map(|c| c.text.as_ref()).collect();
        let pos1 = top.first().copied() == Some(case.positive.as_str());
        let pos3 = top.iter().any(|t| *t == case.positive);
        let pos_any = all.iter().any(|t| *t == case.positive);
        let neg1 = top
            .first()
            .map(|t| case.negatives.iter().any(|n| n == t))
            .unwrap_or(false);
        let neg3 = top.iter().any(|t| case.negatives.iter().any(|n| n == t));
        let neg_any = all.iter().any(|t| case.negatives.iter().any(|n| n == t));
        by_subset
            .entry(case.subset.clone())
            .or_default()
            .add(pos1, pos3, pos_any, neg1, neg3, neg_any);
        total.add(pos1, pos3, pos_any, neg1, neg3, neg_any);
        if !pos1 && shown < 20 {
            shown += 1;
            eprintln!(
                "[miss] {} {} 正={} 首={:?} 负={:?}",
                case.subset, case.input, case.positive, top, case.negatives
            );
        }
    }

    eprintln!("── 同音绿词评测 ──");
    for (subset, s) in &by_subset {
        eprintln!(
            "{subset:8} n={:3} 正Top1={:3}/{:<3} 正Top3={:3}/{:<3} 正Any={:3}/{:<3} 负Top1={:3} 负Top3={:3} 负Any={:3}",
            s.n, s.pos1, s.n, s.pos3, s.n, s.pos_any, s.n, s.neg1, s.neg3, s.neg_any
        );
    }
    eprintln!(
        "total    n={:3} 正Top1={:3}/{:<3} 正Top3={:3}/{:<3} 正Any={:3}/{:<3} 负Top1={:3} 负Top3={:3} 负Any={:3}",
        total.n, total.pos1, total.n, total.pos3, total.n, total.pos_any, total.n, total.neg1, total.neg3, total.neg_any
    );
}
