//! 同音歧义评测。
//!
//! 目标：把「同音节合法歧义」从个案排障变成可重复的测量。评测集与
//! `assets/eval/sentences.tsv`（500 句整句基线）**物理隔离**，不改后者口径。
//!
//! 用例格式（TSV，4-5 列）：
//! `family \t context \t input \t gold \t note?`
//! - `context`：上屏词，`|` 分隔；`-` 表示无上文。
//! - `input`：本次输入的拼音串。
//! - `gold`：期望首候选文本（整句或单词）。
//!
//! 运行：
//! `cargo test -p x1core --release --lib homophone_eval -- --nocapture`
//! holdout 默认 `#[ignore]`，需要时用 `-- --ignored` 单独跑。
//!
//! 口径：
//! - 英文注入关闭（与 eval_baseline 同口径，纯中文候选）。
//! - 源 DB 只读副本（`VACUUM INTO` → `target/test-tmp`），绝不就地打开种子。
//! - 本文件只测量、不断言（基线/回归由单独的守卫测试断言）。
//!
//! **读数字的纪律**：部分用例「gold == 默认 top1」属于免费分——上下文完全
//! 不起作用也能命中。判断「语境是否真的有用」必须看学习实验
//! （`homophone_learn_once_tune`）的 before→after 增量与「gold ≠ 默认」的
//! 子集；不要用 overall top1 单指标下结论。

use crate::engine::Engine;
use std::io::{BufRead, BufReader};
use std::sync::OnceLock;

const SEED_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
const TUNE_CASES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/eval/homophone_tune.tsv"
);
const HOLDOUT_CASES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/eval/homophone_holdout.tsv"
);

fn eval_db_path() -> String {
    std::env::var("X1_EVAL_DB").unwrap_or_else(|_| SEED_DB.to_string())
}

/// 评测专用只读副本（VACUUM INTO）。语义与 `eval_baseline::isolated_db_path` 同款：
/// `Engine::new_with_db` 会 journal→WAL 并写 db_meta，源文件必须隔离。
fn isolated_db_path() -> String {
    static COPY: OnceLock<String> = OnceLock::new();
    COPY.get_or_init(|| {
        let src = eval_db_path();
        let dst = crate::test_tmp::dir().join(format!(
            "x1_homophone_eval_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let dst_str = dst.to_string_lossy().into_owned();
        let copied =
            rusqlite::Connection::open_with_flags(&src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .and_then(|c| c.execute("VACUUM INTO ?1", rusqlite::params![&dst_str]))
                .is_ok();
        if copied {
            eprintln!("[homophone_eval] 源 DB 只读副本 → {dst_str}");
            dst_str
        } else {
            eprintln!("[homophone_eval] 警告：副本创建失败，直接使用源 DB {src}");
            src
        }
    })
    .clone()
}

fn cold_engine() -> Engine {
    let mut engine = match Engine::new_with_db(&isolated_db_path()) {
        Ok(e) => e,
        Err(e) => panic!("同音歧义评测打开种子 DB 失败: {e}"),
    };
    // 与 eval_baseline 同口径：英文候选不参与，保持纯中文候选测量。
    engine.set_smart_word_zh_enabled(false);
    engine
}

#[derive(Default, Clone, Debug)]
struct FamilyStats {
    total: usize,
    hit1: usize,
    hit3: usize,
}

impl FamilyStats {
    fn record(&mut self, hit1: bool, hit3: bool) {
        self.total += 1;
        self.hit1 += hit1 as usize;
        self.hit3 += hit3 as usize;
    }
    fn top1_pct(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.hit1 as f64 * 100.0 / self.total as f64
        }
    }
    fn top3_pct(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.hit3 as f64 * 100.0 / self.total as f64
        }
    }
}

fn run_cases(path: &str, label: &str) {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[homophone_eval] 用例读取失败 {path}: {e}");
            return;
        }
    };
    let mut engine = cold_engine();
    let mut overall = FamilyStats::default();
    let mut by_family: std::collections::BTreeMap<String, FamilyStats> = Default::default();
    let mut by_note: std::collections::BTreeMap<String, FamilyStats> = Default::default();

    println!("\n===== homophone_eval {label} =====");
    for line in BufReader::new(file).lines() {
        let line = line.unwrap_or_default();
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').map(str::trim).collect();
        if cols.len() < 4 {
            continue;
        }
        let (family, ctx, input, gold) = (cols[0], cols[1], cols[2], cols[3]);
        let note = cols.get(4).copied().unwrap_or("");

        engine.context_clear_for_test();
        for w in ctx.split('|') {
            let w = w.trim();
            if !w.is_empty() && w != "-" {
                engine.context_push_for_test(w);
            }
        }
        let cands = engine.process(input);
        let top1 = cands
            .first()
            .map(|c| c.text.to_string())
            .unwrap_or_default();
        let hit1 = top1 == gold;
        let hit3 = cands.iter().take(3).any(|c| c.text.as_ref() == gold);
        let tag = if hit1 {
            "HIT "
        } else if hit3 {
            "T3  "
        } else {
            "MISS"
        };
        println!(
            "{tag} [{family}] ctx={ctx} in={input} gold={gold} top1={top1} top3={:?}",
            cands
                .iter()
                .take(3)
                .map(|c| c.text.as_ref())
                .collect::<Vec<_>>()
        );

        overall.record(hit1, hit3);
        by_family
            .entry(family.to_string())
            .or_default()
            .record(hit1, hit3);
        if !note.is_empty() {
            by_note
                .entry(note.to_string())
                .or_default()
                .record(hit1, hit3);
        }
    }

    println!(
        "----- {label} overall: top1 {}/{} ({:.1}%)  top3 {}/{} ({:.1}%) -----",
        overall.hit1,
        overall.total,
        overall.top1_pct(),
        overall.hit3,
        overall.total,
        overall.top3_pct()
    );
    for (family, s) in &by_family {
        println!(
            "  {family:<14} top1 {:>3}/{:<3} ({:>5.1}%)  top3 {:>3}/{:<3} ({:>5.1}%)",
            s.hit1,
            s.total,
            s.top1_pct(),
            s.hit3,
            s.total,
            s.top3_pct()
        );
    }
    for (note, s) in &by_note {
        println!(
            "  note {note:<24} top1 {:>3}/{:<3} ({:>5.1}%)",
            s.hit1,
            s.total,
            s.top1_pct()
        );
    }
}

#[test]
fn homophone_eval_tune() {
    run_cases(TUNE_CASES, "tune");
}

/// 可学性实验（诊断，默认忽略）：每条用例「在同一上文里教一次」，
/// 立刻复测该用例，统计 top1 翻正率。
///
/// 这是用户视角的下界证明：不要求系统冷启动猜对，只要求**教一次就记住**。
/// 逐例「教→测」而非先全教再全测，避免会话级 recent/mapping 跨用例串扰。
/// 输出附 `ctx同音` 分量，证明翻正来自本轮的上下文证据而不是偶然的 recent。
#[test]
#[ignore = "可学性诊断：逐例教一次后复测，不做 CI 门禁"]
fn homophone_learn_once_tune() {
    use crate::engine::Engine;
    let file = std::fs::File::open(TUNE_CASES).expect("open tune cases");
    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut cases: Vec<(String, Vec<String>, String, String)> = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.unwrap_or_default();
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').map(str::trim).collect();
        if cols.len() < 4 {
            continue;
        }
        // 同一条（ctx,input）只教一次（重复用例会产生重复计数的假象）。
        let key = format!("{}|{}", cols[1], cols[2]);
        if !seen.insert(key) {
            continue;
        }
        let ctx: Vec<String> = cols[1]
            .split('|')
            .map(str::trim)
            .filter(|w| !w.is_empty() && *w != "-")
            .map(str::to_string)
            .collect();
        cases.push((
            cols[0].to_string(),
            ctx,
            cols[2].to_string(),
            cols[3].to_string(),
        ));
    }

    let mut engine = cold_engine();
    let mut before_top1 = 0usize;
    let mut after_top1 = 0usize;
    let mut after_top3 = 0usize;
    let mut promoted_by_ctx = 0usize;
    println!(
        "\n===== homophone learn-once ({} unique cases) =====",
        cases.len()
    );
    for (family, ctx, input, gold) in &cases {
        let apply_ctx = |e: &mut Engine| {
            e.context_clear_for_test();
            for w in ctx {
                e.context_push_for_test(w);
            }
        };
        apply_ctx(&mut engine);
        let before = engine.process(input);
        let before_top = before
            .first()
            .map(|c| c.text.to_string())
            .unwrap_or_default();
        let hit_before = before_top == *gold;

        // 教一次：用输入的权威音节切分作为拼音（用例保证 gold 字数 == 音节数）。
        let syls: Vec<String> = engine.syllable_segmentation(input);
        let pinyin = syls.join(" ");
        engine.commit(input, gold, Some(&pinyin), true);

        apply_ctx(&mut engine);
        let after = engine.process(input);
        let after_top = after
            .first()
            .map(|c| c.text.to_string())
            .unwrap_or_default();
        let hit_after = after_top == *gold;
        let hit_after3 = after.iter().take(3).any(|c| c.text.as_ref() == gold);
        let ctx_bonus = after
            .iter()
            .find(|c| c.text.as_ref() == gold)
            .map(|c| engine.score_breakdown(input, c).ctx_homophone_bonus)
            .unwrap_or(0);

        before_top1 += hit_before as usize;
        after_top1 += hit_after as usize;
        after_top3 += hit_after3 as usize;
        promoted_by_ctx += (hit_after && !hit_before && ctx_bonus > 0) as usize;
        if !hit_before {
            println!(
                "  {} ctx={:?} in={} gold={} before={} after={} ctx同音={} after_top3={:?}",
                family,
                ctx,
                input,
                gold,
                before_top,
                after_top,
                ctx_bonus,
                after
                    .iter()
                    .take(3)
                    .map(|c| c.text.as_ref())
                    .collect::<Vec<_>>()
            );
        }
    }
    let n = cases.len();
    println!(
        "----- learn-once: before top1 {}/{} ({:.1}%) → after top1 {}/{} ({:.1}%), after top3 {}/{} ({:.1}%); 由上下文证据翻正 {} -----",
        before_top1, n, before_top1 as f64 * 100.0 / n as f64,
        after_top1, n, after_top1 as f64 * 100.0 / n as f64,
        after_top3, n, after_top3 as f64 * 100.0 / n as f64,
        promoted_by_ctx
    );
}

/// holdout 需单独跑：`cargo test ... homophone_eval_holdout -- --ignored --nocapture`
#[test]
#[ignore = "holdout 需单独运行，避免调参泄漏"]
fn homophone_eval_holdout() {
    run_cases(HOLDOUT_CASES, "holdout");
}
