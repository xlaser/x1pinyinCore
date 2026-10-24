//! stats 模块测试。
//!
//! 部分用例与 Windows 宿主侧实现共用同一行为契约：两端的聚合结果与
//! JSON 形状必须一致。另含 DDL 契约测试（五表 + active_secs 列）与本地时间健全性测试。

use super::*;

fn store_in(dir: &std::path::Path) -> StatsStore {
    StatsStore::new(Some(dir.join("x1stats.db")))
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = crate::test_tmp::dir().join(format!("x1stats_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn at(day: &str, hour: i64, min: i64) -> DayMinute {
    DayMinute {
        day: day.to_string(),
        minute: hour * 60 + min,
    }
}

/// 取对象/数组段（`"key":` 后第一个 `{`/`[` 到配对闭合）。测试用最小实现。
fn section<'a>(json: &'a str, key: &str) -> &'a str {
    let pat = format!("\"{}\":", key);
    let start = match json.find(&pat) {
        Some(i) => i + pat.len(),
        None => return "",
    };
    let bytes = json.as_bytes();
    let open = bytes[start];
    let (o, c) = match open {
        b'{' => (b'{', b'}'),
        b'[' => (b'[', b']'),
        _ => return &json[start..start + 1],
    };
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for i in start..bytes.len() {
        let b = bytes[i];
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        if b == b'"' {
            in_str = true;
        } else if b == o {
            depth += 1;
        } else if b == c {
            depth -= 1;
            if depth == 0 {
                return &json[start..=i];
            }
        }
    }
    &json[start..]
}

/// 段内取整数（`"key":123`）。
fn num(seg: &str, key: &str) -> i64 {
    let pat = format!("\"{}\":", key);
    let i = seg
        .find(&pat)
        .unwrap_or_else(|| panic!("key {} 不在 {}", key, seg));
    let rest = &seg[i + pat.len()..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
        .unwrap_or(rest.len());
    rest[..end]
        .parse()
        .unwrap_or_else(|_| panic!("{} 不是整数: {}", key, &rest[..end]))
}

// ---------------- 日期纯函数 ----------------

#[test]
fn civil_math_roundtrip() {
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    // 2026-09-08 = 20704 天（跨端统一基准）
    assert_eq!(days_from_civil(2026, 9, 8), 20704);
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(20704), (2026, 9, 8));
    assert_eq!(shift_day_string("2024-02-28", 1).unwrap(), "2024-02-29");
    assert_eq!(shift_day_string("2025-12-31", 1).unwrap(), "2026-01-01");
    assert_eq!(shift_day_string("2026-01-01", -1).unwrap(), "2025-12-31");
    assert!(
        shift_day_string("2026-9-8", 1).is_none(),
        "非零填充格式拒绝"
    );
}

#[test]
fn scope_ranges() {
    // 2026-09-08 是周二
    let (f, t) = scope_range("2026-09-08", "day").unwrap();
    assert_eq!(
        (f.unwrap(), t.unwrap()),
        ("2026-09-08".into(), "2026-09-08".into())
    );
    // 滚动周窗口：[today-6, today] 恒连续 7 天，以今天收尾
    let (f, t) = scope_range("2026-09-08", "week").unwrap();
    assert_eq!(
        (f.unwrap(), t.unwrap()),
        ("2026-09-02".into(), "2026-09-08".into())
    );
    // 周日打开：起点是上周一（与旧自然周起点恰好重合，但语义是"今天往前 6 天"）
    let (f, t) = scope_range("2026-09-13", "week").unwrap();
    assert_eq!(
        (f.unwrap(), t.unwrap()),
        ("2026-09-07".into(), "2026-09-13".into())
    );
    // 周一打开（旧自然周在此只剩 1 条实柱——本修复的主场景）：起点跨到上周二
    let (f, t) = scope_range("2026-09-14", "week").unwrap();
    assert_eq!(
        (f.unwrap(), t.unwrap()),
        ("2026-09-08".into(), "2026-09-14".into())
    );
    let (f, t) = scope_range("2026-09-08", "month").unwrap();
    assert_eq!(
        (f.unwrap(), t.unwrap()),
        ("2026-09-01".into(), "2026-09-30".into())
    );
    let (f, t) = scope_range("2026-12-15", "month").unwrap();
    assert_eq!(
        (f.unwrap(), t.unwrap()),
        ("2026-12-01".into(), "2026-12-31".into())
    );
    let (f, t) = scope_range("2026-09-08", "year").unwrap();
    assert_eq!(
        (f.unwrap(), t.unwrap()),
        ("2026-01-01".into(), "2026-12-31".into())
    );
    assert_eq!(scope_range("2026-09-08", "all").unwrap(), (None, None));
    assert!(scope_range("2026-09-08", "bogus").is_none());
}

/// 本地时间源健全性（unix localtime_r 路径）：格式与范围。
#[test]
fn local_now_is_well_formed() {
    let dm = local_now();
    assert_eq!(dm.day.len(), 10, "日期格式 YYYY-MM-DD: {}", dm.day);
    assert!(parse_day(&dm.day).is_some(), "日期可解析: {}", dm.day);
    assert!(
        (0..1440).contains(&dm.minute),
        "分钟在当日范围内: {}",
        dm.minute
    );
    // 与 UTC 兜底路径的一致性（仅验证兜底分支本身可用）
    let utc = utc_day_minute(0);
    assert_eq!(utc.day, "1970-01-01");
    assert_eq!(utc.minute, 0);
}

// ---------------- 存储契约（统一标准） ----------------

/// 五张事实表 + stats_day 全列（含 active_secs）——与 Windows 侧 DDL 同一标准。
/// 任一列改名/遗漏，双端 DB 就不再互通。
#[test]
fn ddl_contract_matches_shared_standard() {
    let dir = temp_dir("ddl");
    let mut s = store_in(&dir);
    // 触发建表
    s.add_keys_at(&at("2026-09-17", 9, 0), &[], 0, 0, 0, 0, 0);
    let conn = s.conn().expect("conn");
    let mut tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .flatten()
        .collect();
    tables.retain(|t| !t.starts_with("sqlite_"));
    assert_eq!(
        tables,
        vec![
            "stats_chars",
            "stats_day",
            "stats_keys",
            "stats_minute",
            "stats_words"
        ],
        "五张事实表（统一标准）"
    );
    let cols = |t: &str| -> Vec<String> {
        conn.prepare(&format!("PRAGMA table_info({})", t))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .flatten()
            .collect()
    };
    assert_eq!(
        cols("stats_day"),
        vec![
            "day",
            "zh_chars",
            "en_chars",
            "zh_keys",
            "backspaces",
            "single_cnt",
            "word_cnt",
            "green_cnt",
            "first_pick_cnt",
            "commit_cnt",
            "page_turns",
            "active_minutes",
            "best_cpm",
            "active_secs"
        ],
        "stats_day 列序与 Windows 契约一致"
    );
    assert_eq!(cols("stats_keys"), vec!["day", "key", "mode", "cnt"]);
    assert_eq!(
        cols("stats_minute"),
        vec!["day", "minute", "zh_chars", "en_chars"]
    );
    assert_eq!(cols("stats_chars"), vec!["day", "ch", "cnt"]);
    assert_eq!(cols("stats_words"), vec!["day", "word", "cnt"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 键名表（三方契约）自检：唯一、长度 ≤16（DB/IPC 钳制上限）、且覆盖
/// 键盘布局用到的全部名字（chart.rs HROWS 的 name 集）。
#[test]
fn known_keys_contract() {
    let mut seen = std::collections::HashSet::new();
    for k in KNOWN_KEYS {
        assert!(k.len() <= 16, "键名超长: {}", k);
        assert!(seen.insert(k), "键名重复: {}", k);
    }
    // 26 字母 + 10 数字
    for c in b'a'..=b'z' {
        assert!(
            seen.contains(&(c as char).to_string().as_str()),
            "缺字母键 {}",
            c as char
        );
    }
    for d in 0..=9 {
        assert!(seen.contains(&d.to_string().as_str()), "缺数字键 {}", d);
    }
    for k in [
        "space",
        "enter",
        "backspace",
        "tab",
        "escape",
        "shift",
        "caps",
        "ctrl",
        "alt",
        "win",
    ] {
        assert!(seen.contains(k), "缺功能键 {}", k);
    }
}

#[test]
fn migrate_v1_adds_active_secs() {
    let dir = temp_dir("migrate");
    let db = dir.join("x1stats.db");
    // 手工建 v1 形态（无 active_secs 列）+ 一行旧数据
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE stats_day(
               day TEXT PRIMARY KEY,
               zh_chars INTEGER NOT NULL DEFAULT 0, en_chars INTEGER NOT NULL DEFAULT 0,
               zh_keys INTEGER NOT NULL DEFAULT 0, backspaces INTEGER NOT NULL DEFAULT 0,
               single_cnt INTEGER NOT NULL DEFAULT 0, word_cnt INTEGER NOT NULL DEFAULT 0,
               green_cnt INTEGER NOT NULL DEFAULT 0,
               first_pick_cnt INTEGER NOT NULL DEFAULT 0, commit_cnt INTEGER NOT NULL DEFAULT 0,
               page_turns INTEGER NOT NULL DEFAULT 0,
               active_minutes INTEGER NOT NULL DEFAULT 0, best_cpm INTEGER NOT NULL DEFAULT 0);
             INSERT INTO stats_day(day, zh_chars) VALUES ('2026-09-01', 123);",
        )
        .unwrap();
    }
    let mut s = store_in(&dir);
    let v = s.query_json_at("all", "2026-09-17");
    let totals = section(&v, "totals");
    assert_eq!(num(totals, "zh"), 123, "旧数据保留");
    assert_eq!(num(totals, "activeSecs"), 0, "新列默认 0");
    assert!(db.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------- 写入 + 查询 ----------------

#[test]
fn commit_counts_chinese_composition() {
    let dir = temp_dir("commit");
    let mut s = store_in(&dir);
    s.add_commit_at(&at("2026-09-08", 10, 0), "我", false, true);
    s.add_commit_at(&at("2026-09-08", 10, 1), "我们", false, false);
    s.add_commit_at(&at("2026-09-08", 10, 2), "我们今天去", true, false);
    s.add_commit_at(&at("2026-09-08", 10, 3), "hello", false, false); // 非 CJK 不计

    let json = s.query_json_at("day", "2026-09-08");
    let totals = section(&json, "totals");
    assert_eq!(num(totals, "zh"), 1 + 2 + 5);
    assert_eq!(num(totals, "single"), 1);
    assert_eq!(num(totals, "word"), 1);
    assert_eq!(num(totals, "green"), 1);
    assert_eq!(num(totals, "firstPick"), 1);
    assert_eq!(num(totals, "commits"), 3);
    assert_eq!(num(totals, "activeMin"), 3);
    assert_eq!(num(totals, "bestCpm"), 5);

    // 热字「我」在三个词里各一次 = 3
    assert!(
        json.contains(r#"["我",3]"#),
        "热字：{}",
        section(&json, "topChars")
    );
    // 热词：只有「我们」「我们今天去」（纯 CJK ≥2 字）
    let words = section(&json, "topWords");
    assert!(
        words.contains(r#"["我们",1]"#) && words.contains(r#"["我们今天去",1]"#),
        "{}",
        words
    );
    assert_eq!(
        words, r#"[["我们",1],["我们今天去",1]]"#,
        "混合文本/单字不计词"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn keys_heatmap_and_scalars() {
    let dir = temp_dir("keys");
    let mut s = store_in(&dir);
    s.add_keys_at(
        &at("2026-09-08", 9, 30),
        &[
            ("a".into(), 10, 3),
            ("space".into(), 5, 0),
            ("enter".into(), 0, 2),
        ],
        40,
        12,
        3,
        2,
        65000,
    );
    s.add_keys_at(
        &at("2026-09-08", 9, 45),
        &[("a".into(), 2, 0)],
        2,
        0,
        1,
        0,
        499,
    );

    let v = s.query_json_at("day", "2026-09-08");
    let totals = section(&v, "totals");
    assert_eq!(num(totals, "zhKeys"), 42);
    assert_eq!(num(totals, "en"), 12);
    assert_eq!(num(totals, "bs"), 4);
    assert_eq!(num(totals, "pages"), 2);
    // 活跃毫秒→秒：分两批四舍五入 = 65 + 0
    assert_eq!(num(totals, "activeSecs"), 65);

    let heat = section(&v, "heat");
    assert!(heat.contains(r#"["a",12,3]"#), "a 键中/英累计: {}", heat);
    assert!(heat.contains(r#"["space",5,0]"#), "space: {}", heat);
    assert!(heat.contains(r#"["enter",0,2]"#), "enter: {}", heat);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 视图窗口：同一批事实表上 day/week/month/year/all 各自汇总（父视图 ⊇ 子视图）。
#[test]
fn views_aggregate_same_facts() {
    let dir = temp_dir("views");
    let mut s = store_in(&dir);
    // 2026-09-07(周一) / 09-08 / 09-20 / 2025-12-31
    s.add_commit_at(&at("2026-09-07", 9, 0), "周一", false, true);
    s.add_commit_at(&at("2026-09-08", 9, 0), "周二", false, true);
    s.add_commit_at(&at("2026-09-20", 9, 0), "月末", false, true);
    s.add_commit_at(&at("2025-12-31", 9, 0), "去年", false, true);

    let mut zh = |scope: &str| {
        num(
            section(&s.query_json_at(scope, "2026-09-08"), "totals"),
            "zh",
        )
    };
    assert_eq!(zh("day"), 2, "今天");
    assert_eq!(zh("week"), 4, "本周（周一+周二）");
    assert_eq!(zh("month"), 6, "本月（+09-20）");
    assert_eq!(zh("year"), 6, "本年（不含 2025）");
    assert_eq!(zh("all"), 8, "全部");
    // 序列点数：日 288 桶 / 周 7 天 / 月 30 天 / 年 12 月
    let mut series_len = |scope: &str| {
        section(&s.query_json_at(scope, "2026-09-08"), "series")
            .matches("\"l\":")
            .count()
    };
    assert_eq!(series_len("day"), 288);
    assert_eq!(series_len("week"), 7);
    assert_eq!(series_len("month"), 30);
    assert_eq!(series_len("year"), 12);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 重置级联：删该时段的原始行，子视图空、父视图相应减少；纪录仍为全历史口径。
#[test]
fn reset_cascades() {
    let dir = temp_dir("reset");
    let mut s = store_in(&dir);
    // 09-08 打出单分钟 5 字（= 当日最快手速纪录来源），09-20 只打 2 字
    s.add_commit_at(&at("2026-09-08", 9, 0), "今天写的", false, true);
    s.add_commit_at(&at("2026-09-20", 9, 0), "别的", false, true);
    s.add_keys_at(
        &at("2026-09-08", 9, 1),
        &[("a".into(), 100, 0)],
        100,
        0,
        0,
        0,
        600000,
    );

    let before = s.query_json_at("day", "2026-09-08");
    assert_eq!(
        num(section(&before, "records"), "bestCpm"),
        4,
        "重置前：今天单分钟 4 字"
    );

    s.reset_at("day", "2026-09-08");
    let day = s.query_json_at("day", "2026-09-08");
    assert_eq!(num(section(&day, "totals"), "zh"), 0, "今天已清空");
    assert_eq!(section(&day, "heat"), "[]", "热力图空");
    let month = s.query_json_at("month", "2026-09-08");
    assert_eq!(
        num(section(&month, "totals"), "zh"),
        2,
        "本月保留 09-20 那天（别的=2 字）"
    );
    // 纪录口径 = 全部历史行（不随视图切换变），但重置会删除所覆盖时段的
    // 原始行——若纪录正是该时段创下的，纪录随之消失。
    assert_eq!(
        num(section(&month, "records"), "bestCpm"),
        2,
        "重置删掉了创纪录那天的行"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 连击 / 纪录：连续天数（今天未打不打断）、最快速度、单日最多、累计。
#[test]
fn streak_and_records() {
    let dir = temp_dir("streak");
    let mut s = store_in(&dir);
    // 09-06 / 09-07 / 09-08 连续三天，今天 = 09-09（未打字）
    s.add_commit_at(&at("2026-09-06", 9, 0), "一", false, true);
    s.add_commit_at(&at("2026-09-07", 9, 0), "二二", false, true);
    s.add_commit_at(&at("2026-09-08", 9, 0), "三三三三三", false, true);
    let v = s.query_json_at("day", "2026-09-09");
    let rec = section(&v, "records");
    assert_eq!(num(rec, "streak"), 3, "今天没打字不打断连击");
    assert_eq!(num(rec, "bestDayChars"), 5);
    assert!(section(&v, "records").contains("\"bestDay\":\"2026-09-08\""));
    assert_eq!(num(rec, "totalChars"), 1 + 2 + 5);
    assert_eq!(num(rec, "totalDays"), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// JSON 形状契约：顶层字段名与 Windows 端 build_query_json 一致。
#[test]
fn json_shape_contract() {
    let dir = temp_dir("shape");
    let mut s = store_in(&dir);
    s.add_commit_at(&at("2026-09-17", 9, 0), "你好", false, true);
    let j = s.query_json_at("day", "2026-09-17");
    for k in [
        "scope", "today", "totals", "series", "heat", "topChars", "topWords", "records",
    ] {
        assert!(j.contains(&format!("\"{}\":", k)), "缺顶层字段 {}", k);
    }
    for k in [
        "zh",
        "en",
        "zhKeys",
        "bs",
        "single",
        "word",
        "green",
        "firstPick",
        "commits",
        "pages",
        "activeMin",
        "bestCpm",
        "activeSecs",
    ] {
        assert!(
            section(&j, "totals").contains(&format!("\"{}\":", k)),
            "缺 totals.{}",
            k
        );
    }
    for k in [
        "bestCpm",
        "bestCpmDay",
        "bestDayChars",
        "bestDay",
        "streak",
        "totalChars",
        "totalDays",
    ] {
        assert!(
            section(&j, "records").contains(&format!("\"{}\":", k)),
            "缺 records.{}",
            k
        );
    }
    // 序列元素形状 {"l":..,"zh":..,"en":..}
    let one = section(&j, "series");
    let first = section(one, "l");
    assert!(!first.is_empty());
    // 未知 scope / 未初始化路径 → "{}"
    assert_eq!(s.query_json_at("bogus", "2026-09-17"), "{}");
    let mut off = StatsStore::new(None);
    assert!(!off.enabled());
    assert_eq!(off.query_json("day"), "{}", "无路径 = 统计禁用（静默降级）");
    off.add_commit("你好", false, true); // 不得 panic
    let _ = std::fs::remove_dir_all(&dir);
}

/// 比率趋势数据源：周/月/年/全部的序列桶必须带翻页/上屏/绿词
/// 计数（翻页率 = 翻页÷中文字数，长句命中率 = 绿词÷上屏）；日视图的分钟桶
/// 只存字数，三项恒 0（UI 据此禁用日视图比率曲线）。
#[test]
fn series_carries_rate_counters() {
    let dir = temp_dir("rate_series");
    let mut s = store_in(&dir);
    s.add_commit_at(&at("2026-09-07", 9, 0), "今天天气不错", false, true);
    s.add_commit_at(&at("2026-09-08", 9, 0), "我们", true, false);
    s.add_keys_at(
        &at("2026-09-08", 9, 5),
        &[("a".into(), 10, 0)],
        10,
        0,
        0,
        3,
        1000,
    );

    let week = s.query_json_at("week", "2026-09-08");
    let series = section(&week, "series");
    // 周一（09-07）桶：1 次上屏、0 绿词、0 翻页
    assert!(
        series.contains(r#"{"l":"2026-09-07","zh":6,"en":0,"pg":0,"cm":1,"gr":0}"#),
        "周序列桶: {}",
        series
    );
    // 周二（09-08）桶：1 次上屏（绿词）、3 次翻页
    assert!(
        series.contains(r#"{"l":"2026-09-08","zh":2,"en":0,"pg":3,"cm":1,"gr":1}"#),
        "周序列桶: {}",
        series
    );

    // 日视图：分钟桶三项恒 0（无这些计数）
    let day = s.query_json_at("day", "2026-09-08");
    let dseries = section(&day, "series");
    assert!(
        dseries.contains(r#""pg":0,"cm":0,"gr":0"#),
        "日序列桶应无速率计数: {}",
        &dseries[..200.min(dseries.len())]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// IO 写入量实测（统计库调优）：模拟 20 分钟持续打字，量 db+wal
/// 总字节三组对比。手动跑：
///   cargo test -p x1core stats_io_profile -- --ignored --nocapture
///
/// 三组 = 宿主采集器三种落库策略（同 PRAGMA，差异只在事务节奏）：
///   A 旧节奏：commit 每 3s 一落 + keys 每 30s 一落（两事务/次）
///   B 新节奏：commit 每 15s 一落 + keys 每 30s 一落（两事务/次）
///   C 合并版：每 15s 单事务（keys+commit 合一），走 x1_stats_flush_all
#[test]
#[ignore]
fn stats_io_profile() {
    let words = [
        "我们", "知道", "中国", "北京", "学习", "工作", "时间", "朋友", "问题", "发展",
    ];
    let keys: Vec<(String, i64, i64)> = [
        "a",
        "e",
        "i",
        "o",
        "u",
        "space",
        "enter",
        "backspace",
        "n",
        "m",
    ]
    .iter()
    .map(|k| (String::from(*k), 3, 1))
    .collect();
    for (tag, commit_interval, merged) in [
        ("A_old_3s_two_tx", 3i64, false),
        ("B_new_15s_two_tx", 15, false),
        ("C_new_15s_merged_tx", 15, true),
        ("D_60s_merged_tx", 60, true),
    ] {
        let dir = temp_dir(tag);
        let mut st = store_in(&dir);
        let start = at("2026-10-12", 9, 0); // 09:00，minute = 540
        let mut end_m = start.clone();
        for sec in 0..(20 * 60) {
            let m = DayMinute {
                day: start.day.clone(),
                minute: start.minute + sec / 60,
            };
            end_m = m.clone();
            if sec % commit_interval == 0 {
                let w = words[(sec as usize / commit_interval as usize) % words.len()];
                if merged {
                    // 单事务：keys 批也并进来（mac persist(snapshot) 语义）
                    st.add_batch(&m, &keys, 8, 4, 1, 0, 3000, &[(w, false, false)]);
                } else {
                    st.add_commit_at(&m, w, false, false);
                }
            }
            if !merged && sec % 30 == 0 {
                st.add_keys_at(&m, &keys, 8, 4, 1, 0, 30000);
            }
        }
        // 收尾落库（模拟 flushNow）后量总字节
        if merged {
            st.add_batch(&end_m, &[], 0, 0, 0, 0, 0, &[]);
        }
        let mut total: u64 = 0;
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            total += entry.metadata().unwrap().len();
        }
        println!("[stats_io_profile] {tag}: db+wal = {total} bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// 批量 commit 零丢失守护：一次 add_batch 收 N 条 commit
/// 必须全记（zh_chars/commit_cnt/热字/热词逐条累计）。钉住宿主侧
/// pendingCommits 缓存累加语义的落库端——防回退到「节流限频直接丢弃」旧缺陷
///（该缺陷曾使 15s 节流窗口内只记首词，热字/热词/字数明显偏少）。
#[test]
fn batch_commits_no_loss() {
    let dir = temp_dir("no_loss");
    let mut s = store_in(&dir);
    let at = at("2026-10-12", 10, 0);
    // 5 条 commit：4 词 + 1 单字（含 1 条绿词、1 条首选）
    let commits = [
        ("我们", true, true),
        ("知道", false, true),
        ("中国", false, false),
        ("学习", false, false),
        ("好", false, true),
    ];
    s.add_batch(&at, &[], 0, 0, 0, 0, 0, &commits);

    let day = s.query_json_at("day", "2026-10-12");
    let totals = section(&day, "totals");
    // zh = 2+2+2+2+1 = 9（全部累计，不许丢）
    assert!(
        totals.contains(r#""zh":9"#),
        "zh_chars 应为 9（5 条 commit 全记）: {}",
        totals
    );
    // commit_cnt = 5、first = 3、single = 1、word = 3（"我们"是绿词不算 word）、green = 1
    assert!(
        totals.contains(r#""commits":5"#),
        "commit 计数应为 5: {}",
        totals
    );
    assert!(
        totals.contains(r#""firstPick":3"#) && totals.contains(r#""green":1"#),
        "firstPick/green 计数不符: {}",
        totals
    );
    // 热字：我们知道中国学习好 全部逐字累计（"我"1 "们"1 "知"1 "道"1 "中"1 "国"1 "学"1 "习"1 "好"1）
    let top = section(&day, "topChars");
    for ch in ["我", "们", "知", "道", "中", "国", "学", "习", "好"] {
        assert!(
            top.contains(&format!(r#""{ch}""#)),
            "热字 {ch} 丢失: {}",
            top
        );
    }
    // 热词：4 条纯多字词（"好"单字不入词表）
    let words = section(&day, "topWords");
    for w in ["我们", "知道", "中国", "学习"] {
        assert!(
            words.contains(&format!(r#""{w}""#)),
            "热词 {w} 丢失: {}",
            words
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
