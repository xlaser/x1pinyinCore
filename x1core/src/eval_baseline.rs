//! 整句正确率评测基线。
//!
//! 纯测量模块，不改任何引擎逻辑。基于固定语料 `assets/eval/sentences.tsv`，
//! 在冷启动引擎（种子 DB + 无用户历史）上跑整句解码，输出：
//! - Top1 命中率（首候选严格相等）
//! - Top3 命中率（期望句出现在前 3）
//! - 字级准确率（首候选与期望句的字符 LCS Dice 系数，部分正确的量化）
//! - 错误归因（rank_miss / dict_missing / partial / seg_fail）
//!
//! 运行：
//! ```bash
//! cargo test -p x1core --release eval_baseline -- --nocapture
//! # 真冷启动（纯净种子，无用户运行时数据）——用 X1_EVAL_DB 指向纯净种子 DB：
//! X1_EVAL_DB=<seed.db> cargo test -p x1core --release --lib eval_baseline -- --nocapture
//! ```
//! 人类可读摘要走 stderr，机器可读 JSON 固定写 `/tmp/x1_eval_baseline.json`。
//! JSON 记录 git commit + DB 路径 / DB / 语料 sha256，确保跨轮对比时
//! 知道语料与词库版本是否一致（改了 DB 或语料后旧基线不可直接比）。
//!
//! 注意：repo 种子若混入过运行时数据（user_words/user_counts 等），直接
//! 测得到的是"冷启动 + 个人语言模型"的混合结果——真冷启动必须用纯净
//! 种子（X1_EVAL_DB 指向干净 DB）。种子 freq 意外损坏时单测可能全绿，
//! 只有本评测的 Top1 大幅波动能将其暴露。
//!
//! 本测试不 assert 任何阈值——基线是测量工具不是守门员，
//! 任何排序/词库改动后都应能跑出报告（变好或变差都是有效信息）。

#[cfg(test)]
pub(crate) mod tests {
    use crate::engine::Engine;
    use std::collections::BTreeMap;
    use std::fmt::Write as _;

    /// 语料路径（相对 crate 根，不打包进二进制，测试期从文件系统读）。
    const CORPUS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/eval/sentences.tsv");
    /// 种子 DB 路径（与 eval.rs cold_engine 一致）。
    const SEED_DB_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    /// JSON 输出路径（跨平台：系统临时目录，便于对比脚本读取）。
    fn json_out() -> String {
        std::env::temp_dir()
            .join("x1_eval_baseline.json")
            .display()
            .to_string()
    }

    /// 一条评测用例。
    struct Case {
        category: String,
        input: String,
        expected: String,
    }

    /// 单条用例的评测结果。
    struct CaseResult {
        category: String,
        input: String,
        expected: String,
        top1: String,
        hit1: bool,
        hit3: bool,
        char_acc: f64,
        /// 错误归因：hit / rank_miss / dict_missing / partial / seg_fail。
        cause: &'static str,
    }

    /// 解析 TSV 语料（# 开头注释、空行跳过；列：category\tinput\texpected\tnote?）。
    fn load_corpus() -> Vec<Case> {
        let text = std::fs::read_to_string(CORPUS_PATH)
            .unwrap_or_else(|e| panic!("语料读取失败 {}: {}", CORPUS_PATH, e));
        let mut cases = Vec::new();
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let cols: Vec<&str> = line.split('\t').collect();
            assert!(
                cols.len() >= 3,
                "语料第 {} 行列数不足（需 category/input/expected）: {:?}",
                lineno + 1,
                line
            );
            cases.push(Case {
                category: cols[0].to_string(),
                input: cols[1].to_string(),
                expected: cols[2].to_string(),
            });
        }
        cases
    }

    /// 字符级 LCS 长度（句子短，O(n×m) DP 足够）。
    fn lcs_len(a: &[char], b: &[char]) -> usize {
        let mut dp = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                dp[i][j] = if a[i - 1] == b[j - 1] {
                    dp[i - 1][j - 1] + 1
                } else {
                    dp[i - 1][j].max(dp[i][j - 1])
                };
            }
        }
        dp[a.len()][b.len()]
    }

    /// 字级准确率：Dice 系数 2·LCS/(|a|+|b|)。对「大部分对、错一两个字」
    /// 的整句给出 0~1 的连续分，比 0/1 命中率更敏感地反映渐进改善。
    fn char_accuracy(expected: &str, actual: &str) -> f64 {
        let a: Vec<char> = expected.chars().collect();
        let b: Vec<char> = actual.chars().collect();
        if a.is_empty() && b.is_empty() {
            return 1.0;
        }
        let l = lcs_len(&a, &b) as f64;
        2.0 * l / (a.len() + b.len()) as f64
    }

    /// 期望句的「多字词覆盖率」：贪心最长匹配（窗口 ≤4）把期望句切成词库词条，
    /// 返回被多字词（len≥2）覆盖的字符比例。单字兜底不计入覆盖。
    ///
    /// 用于 dict_missing 归因：覆盖率低说明期望句的内容词词库根本没有
    /// （如「社恐」「内卷」），再好的切分排序也拼不出来（词库扩容的直接目标）。
    /// 反之覆盖率高但引擎没拼对，问题在切分/排序而非词库。
    ///
    /// 这比「相邻 2-gram 命中率」准确：2-gram 把「太棒了」的「然了」这类
    /// 跨词边界窗口也算作缺失，短句误报率极高。
    fn dict_word_coverage(engine: &Engine, expected: &str) -> f64 {
        let chars: Vec<char> = expected.chars().collect();
        if chars.is_empty() {
            return 1.0;
        }
        let mut covered = 0usize;
        let mut i = 0usize;
        while i < chars.len() {
            let mut matched = 0usize;
            let max_len = (chars.len() - i).min(4);
            for len in (2..=max_len).rev() {
                let word: String = chars[i..i + len].iter().collect();
                if engine.dict.contains_text(&word) {
                    matched = len;
                    break;
                }
            }
            if matched > 0 {
                covered += matched;
                i += matched;
            } else {
                i += 1; // 单字兜底，不计覆盖
            }
        }
        covered as f64 / chars.len() as f64
    }

    /// 错误归因（只对 Top1 miss 分类），优先级从上到下：
    /// 1. rank_miss：期望句在完整候选列表里但不在 Top3 → 排序问题
    /// 2. dict_missing：期望句多字词覆盖率 < 0.5 → 词库缺失
    /// 3. partial：字级准确率 ≥ 0.7 → 接近正确，切分/选词小错
    /// 4. seg_fail：其余 → 切分/整句组合失败
    fn classify(
        engine: &Engine,
        all_texts: &[String],
        expected: &str,
        char_acc: f64,
    ) -> &'static str {
        if all_texts.iter().any(|t| t == expected) {
            return "rank_miss";
        }
        if dict_word_coverage(engine, expected) < 0.5 {
            return "dict_missing";
        }
        if char_acc >= 0.7 {
            return "partial";
        }
        "seg_fail"
    }

    /// 冷启动引擎（与 eval.rs cold_engine 相同语义：种子 DB + 无用户历史）。
    ///
    /// DB 路径可用环境变量 `X1_EVAL_DB` 覆盖（指向纯净种子 DB 测真冷启动，
    /// 或指向变体 DB）。默认仍用 repo 种子，行为与之前一致。
    fn eval_db_path() -> String {
        std::env::var("X1_EVAL_DB").unwrap_or_else(|_| SEED_DB_PATH.to_string())
    }

    /// **评测专用 DB 副本路径**：评测全程只读副本，绝不碰源 DB。
    ///
    /// 为什么必须复制：
    /// - `Engine::new_with_db` 打开 DB 即做 journal→WAL 转换并写 db_meta，
    ///   传入的**源文件被就地修改**——用纯净种子跑评测会改写它，
    ///   同一 commit 跑两次结果不同。
    /// - 评测语义是"冷启动"：任何写回都在给下一次评测偷偷预热，
    ///   `seed_db_sha256` 记录也会随运行漂移，无法定位差异来源。
    ///
    /// 实现：`VACUUM INTO`（SQLite 原生整库快照，自带一致性，无需处理 -wal/-shm
    /// 边车）复制到临时目录，进程内只做一次（`OnceLock`）。失败时退回原路径并
    /// 打印警告——评测仍可跑，只是失去隔离保证。
    fn isolated_db_path() -> String {
        use std::sync::OnceLock;
        static COPY: OnceLock<String> = OnceLock::new();
        COPY.get_or_init(|| {
            let src = eval_db_path();
            // DB 副本（53MB 级）落项目 target/test-tmp（测试产物不进系统
            // 临时目录）。JSON 结果文件仍走 temp_dir——跨平台一致的输出路径。
            let dst = crate::test_tmp::dir().join(format!(
                "x1_eval_copy_{}_{}.db",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            let dst_str = dst.to_string_lossy().into_owned();
            let copied = rusqlite::Connection::open_with_flags(
                &src,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .and_then(|c| c.execute("VACUUM INTO ?1", rusqlite::params![&dst_str]))
            .is_ok();
            if copied {
                eprintln!("[eval_baseline] 源 DB 只读副本 → {}", dst_str);
                dst_str
            } else {
                eprintln!(
                    "[eval_baseline] 警告：DB 副本创建失败，直接使用源 DB {src}（源文件可能被改写）"
                );
                src
            }
        })
        .clone()
    }

    fn cold_engine() -> Engine {
        let mut engine = match Engine::new_with_db(&isolated_db_path()) {
            Ok(e) => e,
            Err(_) => {
                eprintln!("[eval_baseline] 警告：种子 DB 不可用，回退内置词库");
                Engine::new()
            }
        };
        // 智能单词识别关闭：英文候选会挤占 Top1 位次，评测保持
        // 「纯中文候选」口径——基线连续可比。
        engine.set_smart_word_zh_enabled(false);
        engine
    }

    /// 文件 sha256（纯 Rust 实现，跨平台一致；读取失败返回 "unknown"，不影响评测本身）。
    ///
    /// 原实现 shell out 系统 `shasum`：Windows 没有该可执行文件（Git Bash 里的
    /// 是 perl 脚本，`Command` 无法 spawn），静默失败后本字段恒为 "unknown"，
    /// 「跨轮对比前先确认词库/语料版本一致」的溯源在 Windows 开发机上从未生效。
    fn sha256_of(path: &str) -> String {
        crate::sha256::sha256_of_file(path).unwrap_or_else(|| "unknown".to_string())
    }

    /// 当前 git 短 commit（失败返回 "unknown"）。
    fn git_commit() -> String {
        std::process::Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// JSON 字符串转义（UTF-8 原样输出，只转义引号/反斜杠/控制字符）。
    fn json_escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    let _ = write!(out, "\\u{:04x}", c as u32);
                }
                c => out.push(c),
            }
        }
        out
    }

    /// 分类统计聚合。
    #[derive(Default)]
    struct CatStat {
        total: usize,
        hit1: usize,
        hit3: usize,
        acc_sum: f64,
    }

    /// 整句正确率基线评测主入口。
    ///
    /// 跑全部语料，输出 stderr 摘要 + /tmp/x1_eval_baseline.json。
    /// 不 assert 阈值（见模块头注释），只保证「报告生成成功」。
    #[test]
    fn eval_baseline() {
        let cases = load_corpus();
        let engine = cold_engine();
        let started = std::time::Instant::now();

        let mut results: Vec<CaseResult> = Vec::with_capacity(cases.len());
        for case in &cases {
            let cands = engine.process(&case.input);
            let all_texts: Vec<String> = cands.iter().map(|c| c.text.as_ref().to_owned()).collect();
            let top1 = all_texts.first().cloned().unwrap_or_default();
            let hit1 = top1 == case.expected;
            let hit3 = all_texts.iter().take(3).any(|t| *t == case.expected);
            let acc = char_accuracy(&case.expected, &top1);
            let cause = if hit1 {
                "hit"
            } else {
                classify(&engine, &all_texts, &case.expected, acc)
            };
            results.push(CaseResult {
                category: case.category.clone(),
                input: case.input.clone(),
                expected: case.expected.clone(),
                top1,
                hit1,
                hit3,
                char_acc: acc,
                cause,
            });
        }

        // ---- 聚合 ----
        let total = results.len();
        let hit1 = results.iter().filter(|r| r.hit1).count();
        let hit3 = results.iter().filter(|r| r.hit3).count();
        let acc_avg = results.iter().map(|r| r.char_acc).sum::<f64>() / total.max(1) as f64;

        let mut by_cat: BTreeMap<String, CatStat> = BTreeMap::new();
        let mut by_cause: BTreeMap<&str, usize> = BTreeMap::new();
        for r in &results {
            let s = by_cat.entry(r.category.clone()).or_default();
            s.total += 1;
            s.hit1 += r.hit1 as usize;
            s.hit3 += r.hit3 as usize;
            s.acc_sum += r.char_acc;
            if !r.hit1 {
                *by_cause.entry(r.cause).or_insert(0) += 1;
            }
        }

        // ---- stderr 人类可读摘要 ----
        eprintln!("\n========== 整句正确率基线 ==========");
        eprintln!(
            "Top1: {}/{} ({:.1}%)   Top3: {}/{} ({:.1}%)   字级准确率均值: {:.1}%",
            hit1,
            total,
            hit1 as f64 * 100.0 / total as f64,
            hit3,
            total,
            hit3 as f64 * 100.0 / total as f64,
            acc_avg * 100.0
        );
        eprintln!("---------- 分类明细 ----------");
        for (cat, s) in &by_cat {
            eprintln!(
                "  {:<6} Top1 {:>3}/{:<3} ({:>5.1}%)  Top3 {:>3}/{:<3} ({:>5.1}%)  字准 {:.1}%",
                cat,
                s.hit1,
                s.total,
                s.hit1 as f64 * 100.0 / s.total as f64,
                s.hit3,
                s.total,
                s.hit3 as f64 * 100.0 / s.total as f64,
                s.acc_sum * 100.0 / s.total as f64
            );
        }
        eprintln!(
            "---------- 错误归因（Top1 miss {} 条） ----------",
            total - hit1
        );
        for (cause, n) in &by_cause {
            eprintln!("  {:<12} {}", cause, n);
        }
        eprintln!("---------- Miss 明细（最多 30 条） ----------");
        for (shown, r) in results.iter().filter(|r| !r.hit1).enumerate() {
            if shown >= 30 {
                eprintln!("  ... 其余见 JSON");
                break;
            }
            eprintln!(
                "  [{}][{}] {} → 期望「{}」实际「{}」字准{:.0}%",
                r.category,
                r.cause,
                r.input,
                r.expected,
                r.top1,
                r.char_acc * 100.0
            );
        }
        eprintln!("耗时: {:?}", started.elapsed());

        // ---- JSON 落盘 ----
        // 头部字段同样过 json_escape：db_path 在 Windows 上是 `F:\Projects\...`，
        // 裸写进去反斜杠不是合法 JSON 转义（`\P`），整个报告在 Windows 上
        // 无法被任何标准 JSON 解析器读取——评测的机器可读契约在 Windows 上
        // 是坏的（Linux/macOS 路径用 `/` 所以一直没暴露）。
        let mut json = String::with_capacity(64 * 1024);
        let _ = writeln!(
            json,
            "{{\n  \"version\": 1,\n  \"git_commit\": \"{}\",\n  \"db_path\": \"{}\",\n  \"seed_db_sha256\": \"{}\",\n  \"corpus_sha256\": \"{}\",\n  \"elapsed_ms\": {},",
            json_escape(&git_commit()),
            json_escape(&eval_db_path()),
            json_escape(&sha256_of(&eval_db_path())),
            json_escape(&sha256_of(CORPUS_PATH)),
            started.elapsed().as_millis()
        );
        let _ = writeln!(
            json,
            "  \"total\": {},\n  \"top1\": {{\"hits\": {}, \"rate\": {:.4}}},\n  \"top3\": {{\"hits\": {}, \"rate\": {:.4}}},\n  \"char_accuracy_avg\": {:.4},",
            total,
            hit1,
            hit1 as f64 / total as f64,
            hit3,
            hit3 as f64 / total as f64,
            acc_avg
        );
        json.push_str("  \"by_category\": {");
        for (i, (cat, s)) in by_cat.iter().enumerate() {
            let _ = write!(
                json,
                "{}\"{}\": {{\"total\": {}, \"top1\": {}, \"top3\": {}, \"top1_rate\": {:.4}, \"top3_rate\": {:.4}, \"char_acc\": {:.4}}}",
                if i == 0 { "" } else { ", " },
                json_escape(cat),
                s.total,
                s.hit1,
                s.hit3,
                s.hit1 as f64 / s.total as f64,
                s.hit3 as f64 / s.total as f64,
                s.acc_sum / s.total as f64
            );
        }
        json.push_str("},\n  \"attribution\": {");
        for (i, (cause, n)) in by_cause.iter().enumerate() {
            let _ = write!(
                json,
                "{}\"{}\": {}",
                if i == 0 { "" } else { ", " },
                cause,
                n
            );
        }
        json.push_str("},\n  \"cases\": [\n");
        for (i, r) in results.iter().enumerate() {
            let _ = writeln!(
                json,
                "    {{\"category\": \"{}\", \"input\": \"{}\", \"expected\": \"{}\", \"top1\": \"{}\", \"hit1\": {}, \"hit3\": {}, \"char_acc\": {:.4}, \"cause\": \"{}\"}}{}",
                json_escape(&r.category),
                json_escape(&r.input),
                json_escape(&r.expected),
                json_escape(&r.top1),
                r.hit1,
                r.hit3,
                r.char_acc,
                r.cause,
                if i + 1 == results.len() { "" } else { "," }
            );
        }
        json.push_str("  ]\n}\n");

        std::fs::write(json_out(), &json)
            .unwrap_or_else(|e| panic!("JSON 写入失败 {}: {}", json_out(), e));
        eprintln!("JSON 已写入 {}", json_out());
    }

    /// 语料纪律守护。
    ///
    /// ① 预期句不得是词库真词：扩词库后原句"白送"（词库直出而非整句拼接），
    ///    评测数字虚高且不稳定；② 语料条目（input+expected）不得重复。
    #[test]
    fn test_corpus_sentences_not_in_dict() {
        let cases = load_corpus();
        let conn = rusqlite::Connection::open_with_flags(
            SEED_DB_PATH,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap_or_else(|e| panic!("打开种子 DB 失败 {}: {}", SEED_DB_PATH, e));
        let mut offenders = Vec::new();
        for c in &cases {
            let hit: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM x1dict_words WHERE simplified = ?1",
                    rusqlite::params![c.expected],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            if hit > 0 {
                offenders.push(format!("「{}」({})", c.expected, c.input));
            }
        }
        assert!(
            offenders.is_empty(),
            "以下 {} 条语料预期句是词库真词（句子进库会让评测虚高，请改写句子）:\n{}",
            offenders.len(),
            offenders.join("\n")
        );
    }

    /// 语料条目不得重复（input+expected 唯一）。
    #[test]
    fn test_corpus_no_duplicates() {
        let cases = load_corpus();
        let mut seen: std::collections::HashSet<(&str, &str)> = std::collections::HashSet::new();
        let mut dups = Vec::new();
        for c in &cases {
            if !seen.insert((c.input.as_str(), c.expected.as_str())) {
                dups.push(format!("「{}」({})", c.expected, c.input));
            }
        }
        assert!(dups.is_empty(), "以下语料条目重复:\n{}", dups.join("\n"));
    }

    /// 语料自洽性守护。
    ///
    /// **不变量**：每条语料的 input 必须能由 expected 按词库切分拼出——
    /// 即存在一种切分，把 expected 切成若干词库词条（含单字），各词条拼音
    /// 连写恰好等于 input。违反此不变量的条目**任何算法都无法答对**（要么
    /// 输入漏了音节，要么期望句多字/错字），计入 Top1 分母只会压低评测数字、
    /// 掩盖真实改进——与语料纪律守护同属评测可信度守护。
    ///
    /// 修正一律落在 input 侧（期望句是标注真值）。
    ///
    /// 该检查是**冷启动视角**（只查种子词库，不含用户词）——语料本身按
    /// 冷启动设计，用户词只在个人评测回放里出现。
    #[test]
    fn test_corpus_input_derivable_from_expected() {
        let cases = load_corpus();
        let conn = rusqlite::Connection::open_with_flags(
            SEED_DB_PATH,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap_or_else(|e| panic!("打开种子 DB 失败 {}: {}", SEED_DB_PATH, e));
        // 词条（≤4 字，整句切分用词上限）→ 全部读音（去空格），供 DP 逐段匹配。
        let mut stmt = conn
            .prepare("SELECT simplified, pinyin FROM x1dict_words")
            .unwrap();
        let mut entries: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        for row in rows {
            let (text, pinyin) = row.unwrap();
            if text.chars().count() <= 4 {
                entries
                    .entry(text)
                    .or_default()
                    .push(pinyin.chars().filter(|c| *c != ' ').collect());
            }
        }

        let mut offenders = Vec::new();
        for c in &cases {
            let chars: Vec<char> = c.expected.chars().collect();
            // DP：句子位置 + 输入位置 的可达集合；每步吃掉 1..=4 个汉字的一个词条。
            let mut reach: std::collections::HashSet<(usize, usize)> =
                std::collections::HashSet::new();
            reach.insert((0, 0));
            for i in 0..chars.len() {
                let states: Vec<usize> = reach
                    .iter()
                    .filter(|(ci, _)| *ci == i)
                    .map(|(_, j)| *j)
                    .collect();
                for j in states {
                    for len in 1..=4usize.min(chars.len() - i) {
                        let tile: String = chars[i..i + len].iter().collect();
                        if let Some(readings) = entries.get(&tile) {
                            for py in readings {
                                if c.input[j..].starts_with(py.as_str()) {
                                    reach.insert((i + len, j + py.len()));
                                }
                            }
                        }
                    }
                }
            }
            if !reach.contains(&(chars.len(), c.input.len())) {
                offenders.push(format!("「{}」({})", c.expected, c.input));
            }
        }
        assert!(
            offenders.is_empty(),
            "以下 {} 条语料的输入无法由期望句拼出（任何算法都答不对，请修正 input 侧）:\n{}",
            offenders.len(),
            offenders.join("\n")
        );
    }
}
