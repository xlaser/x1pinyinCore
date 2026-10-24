//! 行为回放评测。
//!
//! 把用户的真实打字记录（selections.jsonl，每条 = 一次候选提交：input + 用户选了
//! 第几个 + picked_text）逐条**重放**给当前引擎，回答一个问题：
//! 「用户当时选的词，现在的引擎能不能直接排第一？」
//!
//! 与 eval_baseline（150 句固定语料）的区别：固定语料测的是算法冷启动质量，
//! 行为回放测的是**真实使用中的引擎表现**——调行为类参数（绿词采纳门槛、
//! freq_ratio 误选守卫等）应以本工具的指标为准，固定语料测不出这些。
//!
//! 噪音过滤：真实日志混有少量非意图事件（选错→删除重打的中间态、教学
//! 连击、天马行空白造词）。过滤器三原则：
//! ① 只标记**高置信噪音**（只有一种合理解释的模式，宁可漏杀不可错杀）；
//! ② **标记分段而非删除**——全集/干净集/噪音集三栏并列，过滤改变可见；
//! ③ **审计清单全量落盘**——被标记事件逐条列出供人工否决。
//! 承认的残留误差：打了错字但没纠正的事件在数据里不可见，无法过滤；
//! 天马行空类无法可靠自动判定，不过滤。
//!
//! 运行：
//! ```sh
//! cargo test -p x1core --release eval_replay -- --nocapture
//! ```
//! 环境变量：
//! - `X1_REPLAY_LOG`：selections.jsonl 路径（默认用户数据目录：macOS =
//!   ~/Library/Application Support/x1pinyin，Windows = %APPDATA%\x1pinyin）
//! - `X1_REPLAY_DB`：回放用 DB（默认复制用户 DB 到临时目录后用副本——绝不直接打开
//!   活动 DB，避免锁冲突/迁移写入风险；**A/B 对比必须显式固定此变量**，
//!   否则活动 DB 漂移会造成假差异）
//! - `X1_REPLAY_CHAIN`：1（默认）= 链式回放（按 ts 顺序，把前序 picked_text 推进
//!   context，5 分钟间隔算新会话）；0 = 每条独立冷上下文
//!
//! 已知保真边界：日志的 context 字段恒为空（宿主端未记录）→ 上下文
//! 靠链式重建（真实打字的近似）；引擎用默认 fuzzy/correction 配置（与用户偏好
//! 可能有出入）；日志里混有极少数非拼音 input（符号提交等）会进 absent 统计。
//! 测量工具不 assert 阈值（同 eval_baseline），JSON 落盘系统临时目录
//! x1_eval_replay.json（跨平台：std::env::temp_dir）。

#[cfg(test)]
mod tests {
    use crate::engine::Engine;

    /// x1 用户数据目录（跨平台：Windows = %APPDATA%\x1pinyin，macOS =
    /// ~/Library/Application Support/x1pinyin，与宿主端解析保持一致）。
    fn x1_data_dir() -> std::path::PathBuf {
        if cfg!(windows) {
            std::env::var_os("APPDATA")
                .map(|v| std::path::PathBuf::from(v).join("x1pinyin"))
                .unwrap_or_else(|| std::env::temp_dir().join("x1pinyin"))
        } else {
            let home = std::env::var("HOME").unwrap_or_default();
            std::path::PathBuf::from(format!("{home}/Library/Application Support/x1pinyin"))
        }
    }

    /// 回放 JSON 输出路径（系统临时目录，跨平台一致）。
    fn eval_json_path() -> std::path::PathBuf {
        std::env::temp_dir().join("x1_eval_replay.json")
    }

    /// 一条选择记录（只取回放需要的 4 个字段；candidates/features 不解析——
    /// 回放重新跑 process，当时展示的候选列表只用于参考不用于判定）。
    #[derive(Debug, Clone)]
    struct ReplayEvent {
        ts: i64,
        input: String,
        picked: usize,
        picked_text: String,
    }

    /// 噪音标记类型（审计清单用）。
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum NoiseFlag {
        /// 纠正序列：同 input 连续（中间无其他输入）、间隔 ≤120s、picked_text 不同
        /// → 除最后一条（最终意图）外全是非意图中间态（选错→删除→重打）。
        CorrectionSeq,
        /// 教学连击：同 (input, picked_text) 背靠背 ≥3 次、间隔 ≤60s
        /// → 第一条保留（真实意图），其余是刻意重复（教学/测试）。
        TeachingBurst,
    }

    impl NoiseFlag {
        fn label(&self) -> &'static str {
            match self {
                NoiseFlag::CorrectionSeq => "纠正序列",
                NoiseFlag::TeachingBurst => "教学连击",
            }
        }
    }

    /// 高置信噪音标记（推断式，防线一：只标记只有一种合理解释的模式）。
    ///
    /// 两种模式的判定依据：
    /// - 纠正序列：人不可能在同一时刻对同一输入想要两个不同的词——同 input
    ///   连续事件换了词，前面那条必非最终意图（错选/改主意）。保留最后一条。
    /// - 教学连击：正常打字不会瞬间背靠背重复同一个词——只有刻意重复。
    ///   保留第一条（首次选择是真实意图），其余标记。
    ///
    /// 刻意不做的：天马行空白造词无法可靠自动判定（长句自造大多是真实意图），
    /// 不过滤；打了错字但没纠正在数据里不可见，是承认的残留误差。
    ///
    /// 返回 (事件下标, 标记)，按下标排序去重。
    fn flag_noise(events: &[ReplayEvent]) -> Vec<(usize, NoiseFlag)> {
        let n = events.len();
        let mut flags: Vec<(usize, NoiseFlag)> = Vec::new();

        // 纠正序列：同 input 连续分组（间隔 ≤120s），组内 picked_text 不同
        // → 除最后一条外全部标记
        let mut i = 0;
        while i < n {
            let mut j = i + 1;
            while j < n
                && events[j].input == events[i].input
                && events[j].ts - events[j - 1].ts <= 120
            {
                j += 1;
            }
            if j - i > 1 {
                let distinct: std::collections::HashSet<&str> = events[i..j]
                    .iter()
                    .map(|e| e.picked_text.as_str())
                    .collect();
                if distinct.len() > 1 {
                    for k in i..j - 1 {
                        flags.push((k, NoiseFlag::CorrectionSeq));
                    }
                }
            }
            i = j;
        }

        // 教学连击：同 (input, picked_text) 背靠背 ≥3（间隔 ≤60s）
        // → 第一条保留，其余标记
        let mut i = 0;
        while i < n {
            let mut j = i + 1;
            while j < n
                && events[j].input == events[i].input
                && events[j].picked_text == events[i].picked_text
                && events[j].ts - events[j - 1].ts <= 60
            {
                j += 1;
            }
            if j - i >= 3 {
                for k in i + 1..j {
                    flags.push((k, NoiseFlag::TeachingBurst));
                }
            }
            i = j;
        }

        flags.sort_by_key(|f| f.0);
        flags.dedup_by_key(|f| f.0);
        flags
    }

    /// 极简 JSON 字段提取（selections.jsonl 是宿主端手工拼接的扁平结构，
    /// 无嵌套同名键冲突：input/picked/picked_text/ts 都是顶层键，candidates/features
    /// 数组内只有 text/freq/pinyin 不冲突）。逐字段定位 + 字符串 unescape，
    /// 比引入 serde_json 依赖更轻（x1core 保持零多余依赖）。
    fn parse_event(line: &str) -> Option<ReplayEvent> {
        let ts = extract_int(line, "\"ts\":")?;
        let input = extract_str(line, "\"input\":")?;
        let picked = extract_int(line, "\"picked\":")? as usize;
        let picked_text = extract_str(line, "\"picked_text\":")?;
        if input.is_empty() || picked_text.is_empty() {
            return None;
        }
        Some(ReplayEvent {
            ts,
            input,
            picked,
            picked_text,
        })
    }

    fn extract_int(line: &str, key: &str) -> Option<i64> {
        let start = line.find(key)? + key.len();
        let rest = &line[start..];
        let end = rest.find(|c: char| !c.is_ascii_digit() && c != '-')?;
        rest[..end].parse().ok()
    }

    /// 提取 `"key":"value"` 的字符串值（处理转义：\" \\ \n \r \t \uXXXX）。
    fn extract_str(line: &str, key: &str) -> Option<String> {
        let start = line.find(key)? + key.len();
        let rest = &line[start..];
        if !rest.starts_with('"') {
            return None;
        }
        let mut out = String::new();
        let mut chars = rest[1..].chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => return Some(out),
                '\\' => match chars.next()? {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' => {
                        let hex: String = chars.by_ref().take(4).collect();
                        let code = u32::from_str_radix(&hex, 16).ok()?;
                        out.push(char::from_u32(code)?);
                    }
                    other => out.push(other),
                },
                c => out.push(c),
            }
        }
        None // 未闭合引号
    }

    /// 会话间隔：超过 5 分钟无提交视为新会话（context 重置）。
    const SESSION_GAP_SECS: i64 = 300;
    /// context 推进上限（与引擎 CONTEXT_HISTORY_SIZE 对齐）。
    const SESSION_CTX_CAP: usize = 30;

    struct PosStat {
        total: usize,
        hit1: usize,
        hit3: usize,
        /// picked_text 在当前候选列表中的位置分布
        pos_buckets: [usize; 7], // 1 / 2 / 3 / 4-5 / 6-10 / 11-20 / 21+或不在榜
    }

    impl PosStat {
        fn new() -> Self {
            Self {
                total: 0,
                hit1: 0,
                hit3: 0,
                pos_buckets: [0; 7],
            }
        }
        fn record(&mut self, pos: Option<usize>) {
            self.total += 1;
            match pos {
                Some(0) => {
                    self.hit1 += 1;
                    self.hit3 += 1;
                    self.pos_buckets[0] += 1;
                }
                Some(p) => {
                    if p <= 2 {
                        self.hit3 += 1;
                    }
                    let b = match p {
                        1 => 1,
                        2 => 2,
                        3..=4 => 3,
                        5..=9 => 4,
                        10..=19 => 5,
                        _ => 6,
                    };
                    self.pos_buckets[b] += 1;
                }
                None => self.pos_buckets[6] += 1,
            }
        }
        fn rate(&self, n: usize) -> f64 {
            if self.total == 0 {
                0.0
            } else {
                n as f64 / self.total as f64
            }
        }
    }

    fn json_escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out
    }

    fn sha256_of(path: &str) -> String {
        std::process::Command::new("shasum")
            .args(["-a", "256", path])
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .split_whitespace()
                    .next()
                    .map(|h| h.to_string())
            })
            .unwrap_or_else(|| "unknown".to_string())
    }

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

    #[test]
    fn test_parse_event_basic() {
        let line = r#"{"ts":1753800000,"input":"nihao","picked":0,"picked_text":"你好","context":[],"candidates":[{"text":"你好","freq":9000,"pinyin":"ni hao"}],"features":[]}"#;
        let e = parse_event(line).unwrap();
        assert_eq!(e.ts, 1753800000);
        assert_eq!(e.input, "nihao");
        assert_eq!(e.picked, 0);
        assert_eq!(e.picked_text, "你好");
    }

    #[test]
    fn test_parse_event_escaped() {
        let line = r#"{"ts":1,"input":"a\"b","picked":2,"picked_text":"x\\y"}"#;
        let e = parse_event(line).unwrap();
        assert_eq!(e.input, "a\"b");
        assert_eq!(e.picked, 2);
        assert_eq!(e.picked_text, "x\\y");
    }

    #[test]
    fn test_parse_event_malformed() {
        assert!(parse_event("").is_none());
        assert!(parse_event("{}").is_none());
        assert!(parse_event(r#"{"ts":1,"input":"","picked":0,"picked_text":"x"}"#).is_none());
    }

    fn ev(ts: i64, input: &str, text: &str) -> ReplayEvent {
        ReplayEvent {
            ts,
            input: input.to_string(),
            picked: 0,
            picked_text: text.to_string(),
        }
    }

    #[test]
    fn test_flag_noise_correction_seq() {
        // 同 input 连续换词：前两条标记，最后一条保留
        let events = vec![
            ev(100, "youg", "有关"),
            ev(130, "youg", "有个"),  // ≤120s 换词 → 第一条标记
            ev(500, "nihao", "你好"), // 无关事件打断
            ev(600, "youg", "有关"),  // 新序列（中间夹了其他输入）
        ];
        let flags = flag_noise(&events);
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0], (0, NoiseFlag::CorrectionSeq));
    }

    #[test]
    fn test_flag_noise_teaching_burst() {
        // 背靠背连击 ≥3：第一条保留，其余标记
        let events = vec![
            ev(100, "yingguo", "因果"),
            ev(110, "yingguo", "因果"),
            ev(120, "yingguo", "因果"),
            ev(130, "yingguo", "因果"),
        ];
        let flags = flag_noise(&events);
        assert_eq!(flags.len(), 3);
        assert!(flags.iter().all(|(_, f)| *f == NoiseFlag::TeachingBurst));
        assert_eq!(flags[0].0, 1); // 下标 0（第一条）保留
    }

    #[test]
    fn test_flag_noise_no_false_positive() {
        // 正常打字：同 input 换词但中间夹其他输入 / 间隔超窗 → 不标记
        let events = vec![
            ev(100, "de", "的"),
            ev(110, "le", "了"),
            ev(130, "de", "得"), // 中间夹了 le，且是真意图（的地得多义）
            ev(400, "de", "的"), // 间隔 >120s
        ];
        assert!(flag_noise(&events).is_empty());
    }

    /// 回放主流程（测量工具，不 assert 阈值——指标变化由人读 JSON 判断）。
    #[test]
    fn eval_replay() {
        let log_path = std::env::var("X1_REPLAY_LOG").unwrap_or_else(|_| {
            x1_data_dir()
                .join("selections.jsonl")
                .to_string_lossy()
                .into_owned()
        });
        let chain = std::env::var("X1_REPLAY_CHAIN").unwrap_or_else(|_| "1".to_string()) != "0";

        // DB：默认复制用户 DB 到临时目录用副本（绝不直接打开活动用户 DB——
        // 避免与运行中的输入法宿主进程锁冲突，也避免引擎构造期的迁移写入污染真库）。
        // A/B 对比时必须显式 export X1_REPLAY_DB 固定副本，否则活动 DB
        // 漂移会造成假差异。
        let db_path = std::env::var("X1_REPLAY_DB").unwrap_or_else(|_| {
            // 候选源：合并布局用户库 → 分离布局 dict.db（双库分离布局下
            // x1pinyin.db 不存在）→ 仓库种子。**一律复制副本再打开**——
            // Storage::open 会把种子转 WAL 并写入迁移标记，直接打开
            // 种子路径会改写源文件。
            let data_dir = x1_data_dir();
            let candidates = [
                data_dir.join("x1pinyin.db"),
                data_dir.join("dict.db"),
                std::path::PathBuf::from(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/assets/x1pinyin.db"
                )),
            ];
            // DB 副本落项目 target/test-tmp，不占系统临时目录。
            let dst = crate::test_tmp::dir().join("x1_replay_db_copy.db");
            for src in &candidates {
                if !src.exists() {
                    continue;
                }
                let _ = std::fs::remove_file(&dst);
                if std::fs::copy(src, &dst).is_ok() {
                    return dst.to_string_lossy().into_owned();
                }
            }
            // 无可用源：返回不存在的 dst，由下方 Engine::new_with_db 走「打开失败，跳过」
            dst.to_string_lossy().into_owned()
        });

        let content = match std::fs::read_to_string(&log_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[eval_replay] 日志不可读 {log_path}: {e}，跳过（非失败）");
                return;
            }
        };
        let mut events: Vec<ReplayEvent> = content.lines().filter_map(parse_event).collect();
        events.sort_by_key(|e| e.ts);
        if events.is_empty() {
            eprintln!("[eval_replay] 无有效记录，跳过");
            return;
        }

        // v2 噪音标记（推断式高置信规则；标记分段而非删除）
        let flags = flag_noise(&events);
        let flagged: std::collections::HashMap<usize, NoiseFlag> = flags.iter().copied().collect();

        let mut engine = match Engine::new_with_db(&db_path) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[eval_replay] DB 打开失败 {db_path}: {e:?}，跳过");
                return;
            }
        };
        // 智能单词识别关闭：行为回放测中文候选行为，
        // 英文候选会改变候选位次——与 eval_baseline 同口径关闭。
        engine.set_smart_word_zh_enabled(false);

        // 三栏统计：全集 / 干净集 / 噪音集（防线二：过滤改变可见）
        let mut overall = PosStat::new();
        let mut clean = PosStat::new();
        let mut noise = PosStat::new();
        // 干净集细分
        let mut seg_was_top1 = PosStat::new();
        let mut seg_corrected = PosStat::new();
        let mut len_buckets: [PosStat; 4] = [
            PosStat::new(),
            PosStat::new(),
            PosStat::new(),
            PosStat::new(),
        ]; // input 字母数 1-2 / 3-6 / 7-12 / 13+

        let mut session_ctx: Vec<String> = Vec::new();
        let mut last_ts: i64 = 0;
        let t0 = std::time::Instant::now();
        // 每条事件的回放位置（第二遍生成未修复样本时复用，不重复跑 process）
        let mut positions: Vec<Option<usize>> = Vec::with_capacity(events.len());

        for (idx, ev) in events.iter().enumerate() {
            if chain {
                if ev.ts - last_ts > SESSION_GAP_SECS {
                    session_ctx.clear();
                }
                last_ts = ev.ts;
                engine.context = session_ctx.clone();
            }
            // 回放：当前引擎 + 当前 DB +（链式）重建上下文，重跑当时输入。
            // 注意：噪音事件也推进 context——上下文要镜像真实打字过程，
            // 过滤只影响计分桶，不影响世界状态。
            let cands = engine.process(&ev.input);
            let pos = cands.iter().position(|c| c.text.as_ref() == ev.picked_text);
            positions.push(pos);

            overall.record(pos);
            match flagged.get(&idx) {
                Some(_) => noise.record(pos),
                None => {
                    clean.record(pos);
                    if ev.picked == 0 {
                        seg_was_top1.record(pos);
                    } else {
                        seg_corrected.record(pos);
                    }
                    let lb = match ev.input.len() {
                        1..=2 => 0,
                        3..=6 => 1,
                        7..=12 => 2,
                        _ => 3,
                    };
                    len_buckets[lb].record(pos);
                }
            }

            if chain {
                session_ctx.push(ev.picked_text.clone());
                if session_ctx.len() > SESSION_CTX_CAP {
                    session_ctx.remove(0);
                }
            }
        }
        let elapsed = t0.elapsed();

        // 控制台摘要
        println!(
            "\n========== 行为回放（{} 条，链式={}，噪音标记 {} 条） ==========",
            overall.total,
            chain,
            flagged.len()
        );
        println!("  耗时: {:?}", elapsed);
        let print_stat = |name: &str, s: &PosStat| {
            println!(
                "  {name:22} n={:<5} 首选命中 {:.1}%  前三命中 {:.1}%  | 位置分布 1:{} 2:{} 3:{} 4-5:{} 6-10:{} 11-20:{} 20+:{}",
                s.total,
                s.rate(s.hit1) * 100.0,
                s.rate(s.hit3) * 100.0,
                s.pos_buckets[0], s.pos_buckets[1], s.pos_buckets[2],
                s.pos_buckets[3], s.pos_buckets[4], s.pos_buckets[5], s.pos_buckets[6],
            );
        };
        print_stat("全集", &overall);
        print_stat("干净集", &clean);
        print_stat("噪音集（被标记）", &noise);
        println!("  --- 干净集细分 ---");
        print_stat("当时即首选(picked=0)", &seg_was_top1);
        print_stat("用户纠正过(picked>0)", &seg_corrected);
        for (i, name) in ["1-2字母", "3-6字母", "7-12字母", "13+字母"]
            .iter()
            .enumerate()
        {
            print_stat(name, &len_buckets[i]);
        }

        // 审计清单（防线三：被标记事件逐条可见，供人工否决）
        println!("\n  --- 审计清单（被标记为噪音的事件，前 30 条；全量在 JSON）---");
        for (idx, flag) in flags.iter().take(30) {
            let ev = &events[*idx];
            println!(
                "    [{}] ts={} {} → 「{}」",
                flag.label(),
                ev.ts,
                ev.input,
                ev.picked_text
            );
        }
        if flags.len() > 30 {
            println!("    ... 其余 {} 条见 JSON", flags.len() - 30);
        }

        // 干净集纠正事件里现在仍非首选的样本（最多 30 条，供人工归因）
        println!("\n  --- 干净集纠正事件未修复样本（picked>0 且当前仍非首选，前 30 条）---");
        let mut samples: Vec<String> = Vec::new();
        for (idx, ev) in events.iter().enumerate() {
            if flagged.contains_key(&idx) || ev.picked == 0 {
                continue;
            }
            let pos = positions[idx];
            if pos != Some(0) && samples.len() < 30 {
                let line = format!(
                    "    {} 想要「{}」（位置{}）",
                    ev.input,
                    ev.picked_text,
                    pos.map(|p| (p + 1).to_string())
                        .unwrap_or_else(|| "榜外".to_string())
                );
                println!("{line}");
                samples.push(line);
            }
        }

        // JSON 落盘（带指纹，跨次对比可追溯）
        let stat_json = |s: &PosStat| {
            format!(
            "{{\"total\": {}, \"hit1\": {}, \"hit1_rate\": {:.4}, \"hit3_rate\": {:.4}, \"pos\": {:?}}}",
            s.total, s.hit1, s.rate(s.hit1), s.rate(s.hit3), s.pos_buckets
        )
        };
        let audit_json = flags
            .iter()
            .map(|(idx, flag)| {
                let ev = &events[*idx];
                format!(
                    "{{\"flag\": \"{}\", \"ts\": {}, \"input\": \"{}\", \"picked_text\": \"{}\"}}",
                    flag.label(),
                    ev.ts,
                    json_escape(&ev.input),
                    json_escape(&ev.picked_text)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let json = format!(
            "{{\n  \"version\": 2,\n  \"git_commit\": \"{}\",\n  \"db_path\": \"{}\",\n  \"db_sha256\": \"{}\",\n  \"log_path\": \"{}\",\n  \"log_sha256\": \"{}\",\n  \"chain\": {},\n  \"elapsed_ms\": {},\n  \"overall\": {},\n  \"clean\": {},\n  \"noise\": {},\n  \"was_top1\": {},\n  \"corrected\": {},\n  \"len_1_2\": {},\n  \"len_3_6\": {},\n  \"len_7_12\": {},\n  \"len_13p\": {},\n  \"audit\": [{}],\n  \"unfixed_samples\": [{}]\n}}\n",
            git_commit(),
            json_escape(&db_path),
            sha256_of(&db_path),
            json_escape(&log_path),
            sha256_of(&log_path),
            chain,
            elapsed.as_millis(),
            stat_json(&overall),
            stat_json(&clean),
            stat_json(&noise),
            stat_json(&seg_was_top1),
            stat_json(&seg_corrected),
            stat_json(&len_buckets[0]),
            stat_json(&len_buckets[1]),
            stat_json(&len_buckets[2]),
            stat_json(&len_buckets[3]),
            audit_json,
            samples.iter().map(|s| format!("\"{}\"", json_escape(s))).collect::<Vec<_>>().join(", "),
        );
        let json_path = eval_json_path();
        let _ = std::fs::write(&json_path, json);
        println!("\nJSON 已写入 {}", json_path.display());
    }
}
