//! 输入统计核心（跨平台共享实现）。
//!
//! **统一标准**：本模块是输入统计的**唯一存储与聚合实现**——五张事实表 DDL、
//! 查询结果 JSON 形状、键名表、视图语义与 Windows 服务侧实现逐字对齐，产出
//! 同一个 `x1stats.db` 结构，任何一端的分析工具/UI 都能读另一端的库。
//!
//! **平台差异只在时间源**：Windows 侧用 `GetLocalTime`（服务进程持有 store）；
//! 本模块用 unix `localtime_r`（mac 输入法进程经 FFI 持有 store）。公开 API 两端
//! 同名同义，时间注入点（`*_at`）也一致——测试可显式给时间。
//!
//! **职责边界**：统计是独立的事实表，与用户词库完全隔离
//! （不同 DB 文件）——统计库损坏/清空不影响词库学习，反之亦然。
//!
//! **写入时机**：调用方（mac: Swift 采集器）在按键热路径上只做内存累加，
//! 30s / 上屏 / 失焦时调用本模块落库（懒打开，首个写入才建库建表，
//! 启动零成本）。写入失败静默降级（统计宁缺毋滥，绝不影响打字）。

use rusqlite::{params, Connection};
use std::path::PathBuf;

/// 本地「日 + 当日分钟」时间戳（写入路径的时间源；测试可注入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayMinute {
    /// 'YYYY-MM-DD'（本地时区）
    pub day: String,
    /// 当日第几分钟（0..=1439）
    pub minute: i64,
}

/// 统计库（懒打开；`path = None` = 禁用）。
pub struct StatsStore {
    path: Option<PathBuf>,
    conn: Option<Connection>,
    // 无清理循环（与 Windows 侧一致）：分钟桶只增不删。
    // 量级 ~MB/年，唯一消费者（日视图趋势）只读今天——空转的维护路径无价值。
}

/// Top 榜上限（UI 10/20/50/100 切换，一次给足 100）。
const TOP_LIMIT: i64 = 100;

/// 汇总行（13 列，顺序与 totals SQL 一致；末列 = 活跃秒，速度分母）。
type Totals = (
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
);

/// 键名表（DLL/DB/UI 三方契约；mac 侧键码映射必须产出这些名字——
/// `x1_stats_known_keys` FFI 供 Swift 启动自检漂移）。
pub const KNOWN_KEYS: [&str; 65] = [
    "a",
    "b",
    "c",
    "d",
    "e",
    "f",
    "g",
    "h",
    "i",
    "j",
    "k",
    "l",
    "m",
    "n",
    "o",
    "p",
    "q",
    "r",
    "s",
    "t",
    "u",
    "v",
    "w",
    "x",
    "y",
    "z",
    "0",
    "1",
    "2",
    "3",
    "4",
    "5",
    "6",
    "7",
    "8",
    "9",
    ";",
    "/",
    "`",
    "[",
    "\\",
    "]",
    "'",
    ",",
    ".",
    "-",
    "=",
    "space",
    "enter",
    "backspace",
    "delete",
    "tab",
    "escape",
    "left",
    "right",
    "up",
    "down",
    "pageup",
    "pagedown",
    "shift",
    "caps",
    "ctrl",
    "alt",
    "win",
    // mac 独有补充（不破坏 Windows 契约：新名字在 Windows 上不会产生，热力图
    // 未收录的名字自动不着色、不影响布局）。mac 的 Command 归入 "win" 槽位
    // （同为系统修饰键），故此处只有 fn 一个平台补充键。
    "fn",
];

impl StatsStore {
    /// 创建（懒打开——首个写入/查询时才真正开库建表）。
    pub fn new(path: Option<PathBuf>) -> Self {
        StatsStore { path, conn: None }
    }

    /// 统计是否可用（有落盘路径）。
    pub fn enabled(&self) -> bool {
        self.path.is_some()
    }

    /// 懒打开连接 + 建表。失败返回 None（调用方静默降级）。
    fn conn(&mut self) -> Option<&Connection> {
        if self.conn.is_some() {
            return self.conn.as_ref();
        }
        let path = self.path.as_ref()?;
        if let Some(dir) = path.parent() {
            if std::fs::create_dir_all(dir).is_err() {
                return None;
            }
        }
        let conn = match Connection::open(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[stats] 打开 stats.db 失败（统计禁用）: {}", e);
                return None;
            }
        };
        // WAL + NORMAL：单写者（输入法进程），WAL 只为崩溃安全与外部只读工具友好。
        // IO 调优：autocheckpoint 2000 页（降低 4MB 间隔的回拷频率）+
        // journal_size_limit 8MB（checkpoint 后截断 WAL，防长期使用无界增长）。
        // DDL 与 Windows 服务侧实现逐字一致（统一标准）——PRAGMA 调优双端同步。
        if let Err(e) = conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             PRAGMA wal_autocheckpoint=2000; PRAGMA journal_size_limit=8388608;
             CREATE TABLE IF NOT EXISTS stats_minute(
               day TEXT NOT NULL, minute INTEGER NOT NULL,
               zh_chars INTEGER NOT NULL DEFAULT 0, en_chars INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY(day, minute));
             CREATE TABLE IF NOT EXISTS stats_day(
               day TEXT PRIMARY KEY,
               zh_chars INTEGER NOT NULL DEFAULT 0, en_chars INTEGER NOT NULL DEFAULT 0,
               zh_keys INTEGER NOT NULL DEFAULT 0, backspaces INTEGER NOT NULL DEFAULT 0,
               single_cnt INTEGER NOT NULL DEFAULT 0, word_cnt INTEGER NOT NULL DEFAULT 0,
               green_cnt INTEGER NOT NULL DEFAULT 0,
               first_pick_cnt INTEGER NOT NULL DEFAULT 0, commit_cnt INTEGER NOT NULL DEFAULT 0,
               page_turns INTEGER NOT NULL DEFAULT 0,
               active_minutes INTEGER NOT NULL DEFAULT 0, best_cpm INTEGER NOT NULL DEFAULT 0,
               active_secs INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS stats_keys(
               day TEXT NOT NULL, key TEXT NOT NULL, mode INTEGER NOT NULL, cnt INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY(day, key, mode));
             CREATE TABLE IF NOT EXISTS stats_chars(
               day TEXT NOT NULL, ch TEXT NOT NULL, cnt INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY(day, ch));
             CREATE TABLE IF NOT EXISTS stats_words(
               day TEXT NOT NULL, word TEXT NOT NULL, cnt INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY(day, word));",
        ) {
            eprintln!("[stats] 建表失败（统计禁用）: {}", e);
            return None;
        }
        // 增量迁移（活跃秒列）：CREATE IF NOT EXISTS 不给已有表加列，
        // 老库（v1 无 active_secs）需要 ALTER 补列。PRAGMA 探测幂等。
        let has_active_secs = conn
            .prepare("PRAGMA table_info(stats_day)")
            .and_then(|mut s| {
                let names = s.query_map([], |r| r.get::<_, String>(1))?;
                Ok(names.flatten().any(|n| n == "active_secs"))
            })
            .unwrap_or(false);
        if !has_active_secs {
            if let Err(e) = conn.execute_batch(
                "ALTER TABLE stats_day ADD COLUMN active_secs INTEGER NOT NULL DEFAULT 0",
            ) {
                eprintln!("[stats] active_secs 迁移失败（统计禁用）: {}", e);
                return None;
            }
            eprintln!("[stats] stats_day 已迁移：+active_secs");
        }
        self.conn = Some(conn);
        self.conn.as_ref()
    }

    /// 当前本地时间（写入路径时间源；ffi 批量入口复用）。
    pub(crate) fn now() -> DayMinute {
        local_now()
    }

    // ---------------- 写入入口（系统时间包装） ----------------

    /// 上屏提交统计：text = 本步实际上屏文本（组合逐步提交的 delta）；
    /// green = 绿词（整句候选）；first_pick = 首选命中（选中第 1 项）。
    pub fn add_commit(&mut self, text: &str, green: bool, first_pick: bool) {
        let at = Self::now();
        self.add_commit_at(&at, text, green, first_pick);
    }

    /// 键级批量事件：keys = [(键名, 中文模式计数, 英文模式计数)]；标量 =
    /// 中文模式字母键（码长分子）/ 英文可见字符 / 退格 / 翻页 / 活跃毫秒。
    pub fn add_keys(
        &mut self,
        keys: &[(String, i64, i64)],
        zh_letters: i64,
        en_chars: i64,
        backspaces: i64,
        page_turns: i64,
        active_ms: i64,
    ) {
        let at = Self::now();
        self.add_keys_at(
            &at, keys, zh_letters, en_chars, backspaces, page_turns, active_ms,
        );
    }

    // ---------------- 写入核心（测试可注入时间） ----------------

    /// 批量落库（IO 优化）：keys 批 + 多条 commit **单事务合并写**。
    ///
    /// 落库若分两次 FFI（note → commit）= 两个事务，每事务把触到的页整页追加
    /// 进 WAL（keys 逐键 + chars 逐字 + 分钟桶 + 日汇总），日汇总行两次碰 =
    /// 写入翻倍。合并后单事务只碰一次，WAL 页触碰 ≈ 减半。语义与分开调用完全
    /// 一致（两组写互不依赖）；`commits` 为空 = 纯 keys 批（等价 add_keys）。
    ///
    /// 多条 commit 零丢失：Swift 采集器的 commit 节流若「限频直接丢弃」会偏少
    /// （15s 内只记首词），改为内存缓存攒批后此处收整批，同事务内 day 计数
    /// 合并一条 UPDATE、chars/words 逐文本。
    #[allow(clippy::too_many_arguments)]
    pub fn add_batch(
        &mut self,
        at: &DayMinute,
        keys: &[(String, i64, i64)],
        zh_letters: i64,
        en_chars: i64,
        backspaces: i64,
        page_turns: i64,
        active_ms: i64,
        commits: &[(&str, bool, bool)],
    ) {
        if let Err(e) = self.add_batch_inner(
            at, keys, zh_letters, en_chars, backspaces, page_turns, active_ms, commits,
        ) {
            eprintln!("[stats] batch 统计写入失败（已跳过）: {}", e);
            self.conn = None;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_batch_inner(
        &mut self,
        at: &DayMinute,
        keys: &[(String, i64, i64)],
        zh_letters: i64,
        en_chars: i64,
        backspaces: i64,
        page_turns: i64,
        active_ms: i64,
        commits: &[(&str, bool, bool)],
    ) -> rusqlite::Result<()> {
        let Some(conn) = self.conn() else {
            return Ok(());
        };
        let tx = conn.unchecked_transaction()?;
        // commit 段（批量聚合）
        Self::add_commits_in_tx(&tx, at, commits)?;
        // keys 部分（内联自 add_keys_inner）
        if !keys.is_empty()
            || zh_letters > 0
            || en_chars > 0
            || backspaces > 0
            || page_turns > 0
            || active_ms > 0
        {
            Self::upsert_day(&tx, &at.day)?;
            for (key, zh, en) in keys {
                if key.len() > 16 {
                    continue;
                }
                for (mode, cnt) in [(0i64, *zh), (1i64, *en)] {
                    if cnt <= 0 {
                        continue;
                    }
                    tx.execute(
                        "INSERT INTO stats_keys(day, key, mode, cnt) VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(day, key, mode) DO UPDATE SET cnt = cnt + ?4",
                        params![at.day, key, mode, cnt.min(100_000)],
                    )?;
                }
            }
            let active_secs = (active_ms.max(0) + 500) / 1000;
            tx.execute(
                "UPDATE stats_day SET zh_keys = zh_keys + ?2, en_chars = en_chars + ?3,
                   backspaces = backspaces + ?4, page_turns = page_turns + ?5,
                   active_secs = active_secs + ?6
                 WHERE day = ?1",
                params![
                    at.day,
                    zh_letters.max(0),
                    en_chars.max(0),
                    backspaces.max(0),
                    page_turns.max(0),
                    active_secs
                ],
            )?;
            if en_chars > 0 {
                Self::add_minute_inner(&tx, at, 0, en_chars)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 一批 commit 写入（事务内）：day 行 6 计数合并成**一条**
    /// UPDATE，chars/words 逐文本累加，分钟桶合并一次。空批 = 空转。
    fn add_commits_in_tx(
        tx: &Connection,
        at: &DayMinute,
        commits: &[(&str, bool, bool)],
    ) -> rusqlite::Result<()> {
        let mut n_total = 0i64;
        let mut commit_cnt = 0i64;
        let mut first_cnt = 0i64;
        let mut single_cnt = 0i64;
        let mut word_cnt = 0i64;
        let mut green_cnt = 0i64;
        for (text, green, first_pick) in commits {
            let cjk: Vec<char> = text.chars().filter(|&c| is_cjk(c)).collect();
            let n = cjk.len() as i64;
            if n == 0 {
                continue; // 非中文上屏（标点/英文原文）不计
            }
            let kind = if *green {
                CommitKind::Green
            } else if n == 1 {
                CommitKind::Single
            } else {
                CommitKind::Word
            };
            n_total += n;
            commit_cnt += 1;
            first_cnt += if *first_pick { 1 } else { 0 };
            single_cnt += if kind == CommitKind::Single { 1 } else { 0 };
            word_cnt += if kind == CommitKind::Word { 1 } else { 0 };
            green_cnt += if kind == CommitKind::Green { 1 } else { 0 };
            for c in cjk.iter() {
                tx.execute(
                    "INSERT INTO stats_chars(day, ch, cnt) VALUES (?1, ?2, 1)
                     ON CONFLICT(day, ch) DO UPDATE SET cnt = cnt + 1",
                    params![at.day, c.to_string()],
                )?;
            }
            if n >= 2 && n as usize == text.chars().count() {
                tx.execute(
                    "INSERT INTO stats_words(day, word, cnt) VALUES (?1, ?2, 1)
                     ON CONFLICT(day, word) DO UPDATE SET cnt = cnt + 1",
                    params![at.day, text],
                )?;
            }
        }
        if commit_cnt == 0 {
            return Ok(());
        }
        Self::upsert_day(tx, &at.day)?;
        tx.execute(
            "UPDATE stats_day SET zh_chars = zh_chars + ?2, commit_cnt = commit_cnt + ?3,
               first_pick_cnt = first_pick_cnt + ?4,
               single_cnt = single_cnt + ?5, word_cnt = word_cnt + ?6, green_cnt = green_cnt + ?7
             WHERE day = ?1",
            params![at.day, n_total, commit_cnt, first_cnt, single_cnt, word_cnt, green_cnt],
        )?;
        Self::add_minute_inner(tx, at, n_total, 0)?;
        Ok(())
    }

    fn add_commit_at(&mut self, at: &DayMinute, text: &str, green: bool, first_pick: bool) {
        if let Err(e) = self.add_commit_inner(at, text, green, first_pick) {
            eprintln!("[stats] commit 统计写入失败（已跳过）: {}", e);
            self.conn = None; // 出错后下次重开（防半开连接持续失败）
        }
    }

    fn add_commit_inner(
        &mut self,
        at: &DayMinute,
        text: &str,
        green: bool,
        first_pick: bool,
    ) -> rusqlite::Result<()> {
        let cjk: Vec<char> = text.chars().filter(|&c| is_cjk(c)).collect();
        let n = cjk.len() as i64;
        if n == 0 {
            return Ok(()); // 非中文上屏（标点/英文原文）不计
        }
        let kind = if green {
            CommitKind::Green
        } else if n == 1 {
            CommitKind::Single
        } else {
            CommitKind::Word
        };
        let Some(conn) = self.conn() else {
            return Ok(());
        };
        let tx = conn.unchecked_transaction()?;
        Self::upsert_day(&tx, &at.day)?;
        tx.execute(
            "UPDATE stats_day SET zh_chars = zh_chars + ?2, commit_cnt = commit_cnt + 1,
               first_pick_cnt = first_pick_cnt + ?3,
               single_cnt = single_cnt + ?4, word_cnt = word_cnt + ?5, green_cnt = green_cnt + ?6
             WHERE day = ?1",
            params![
                at.day,
                n,
                if first_pick { 1 } else { 0 },
                if kind == CommitKind::Single { 1 } else { 0 },
                if kind == CommitKind::Word { 1 } else { 0 },
                if kind == CommitKind::Green { 1 } else { 0 },
            ],
        )?;
        // 热字：逐 CJK 字累加
        for c in cjk.iter() {
            tx.execute(
                "INSERT INTO stats_chars(day, ch, cnt) VALUES (?1, ?2, 1)
                 ON CONFLICT(day, ch) DO UPDATE SET cnt = cnt + 1",
                params![at.day, c.to_string()],
            )?;
        }
        // 热词：纯 CJK 且 ≥2 字（混合文本不记词）
        if n >= 2 && n as usize == text.chars().count() {
            tx.execute(
                "INSERT INTO stats_words(day, word, cnt) VALUES (?1, ?2, 1)
                 ON CONFLICT(day, word) DO UPDATE SET cnt = cnt + 1",
                params![at.day, text],
            )?;
        }
        Self::add_minute_inner(&tx, at, n, 0)?;
        tx.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn add_keys_at(
        &mut self,
        at: &DayMinute,
        keys: &[(String, i64, i64)],
        zh_letters: i64,
        en_chars: i64,
        backspaces: i64,
        page_turns: i64,
        active_ms: i64,
    ) {
        if let Err(e) = self.add_keys_inner(
            at, keys, zh_letters, en_chars, backspaces, page_turns, active_ms,
        ) {
            eprintln!("[stats] keys 统计写入失败（已跳过）: {}", e);
            self.conn = None;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_keys_inner(
        &mut self,
        at: &DayMinute,
        keys: &[(String, i64, i64)],
        zh_letters: i64,
        en_chars: i64,
        backspaces: i64,
        page_turns: i64,
        active_ms: i64,
    ) -> rusqlite::Result<()> {
        let Some(conn) = self.conn() else {
            return Ok(());
        };
        let tx = conn.unchecked_transaction()?;
        Self::upsert_day(&tx, &at.day)?;
        for (key, zh, en) in keys {
            if key.len() > 16 {
                continue; // 防御：键名表是固定集合，超长即畸形
            }
            for (mode, cnt) in [(0i64, *zh), (1i64, *en)] {
                if cnt <= 0 {
                    continue;
                }
                tx.execute(
                    "INSERT INTO stats_keys(day, key, mode, cnt) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(day, key, mode) DO UPDATE SET cnt = cnt + ?4",
                    params![at.day, key, mode, cnt.min(100_000)],
                )?;
            }
        }
        // 活跃毫秒 → 秒（四舍五入；30s 批次的截断误差 <0.5s/批，可忽略）
        let active_secs = (active_ms.max(0) + 500) / 1000;
        tx.execute(
            "UPDATE stats_day SET zh_keys = zh_keys + ?2, en_chars = en_chars + ?3,
               backspaces = backspaces + ?4, page_turns = page_turns + ?5,
               active_secs = active_secs + ?6
             WHERE day = ?1",
            params![
                at.day,
                zh_letters.max(0),
                en_chars.max(0),
                backspaces.max(0),
                page_turns.max(0),
                active_secs
            ],
        )?;
        if en_chars > 0 {
            Self::add_minute_inner(&tx, at, 0, en_chars)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 确保当日行存在（所有写路径的前置）。
    fn upsert_day(conn: &Connection, day: &str) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT OR IGNORE INTO stats_day(day) VALUES (?1)",
            params![day],
        )?;
        Ok(())
    }

    /// 分钟桶累加（zh/en 可只加一边）；新桶 → active_minutes +1；
    /// zh_chars 刷新当日 best_cpm。
    fn add_minute_inner(
        conn: &Connection,
        at: &DayMinute,
        zh: i64,
        en: i64,
    ) -> rusqlite::Result<()> {
        Self::upsert_day(conn, &at.day)?;
        let created = conn.execute(
            "INSERT OR IGNORE INTO stats_minute(day, minute, zh_chars, en_chars)
             VALUES (?1, ?2, ?3, ?4)",
            params![at.day, at.minute, zh, en],
        )?;
        if created == 0 {
            conn.execute(
                "UPDATE stats_minute SET zh_chars = zh_chars + ?3, en_chars = en_chars + ?4
                 WHERE day = ?1 AND minute = ?2",
                params![at.day, at.minute, zh, en],
            )?;
        } else {
            conn.execute(
                "UPDATE stats_day SET active_minutes = active_minutes + 1 WHERE day = ?1",
                params![at.day],
            )?;
        }
        if zh > 0 {
            conn.execute(
                "UPDATE stats_day SET best_cpm = MAX(best_cpm,
                   (SELECT zh_chars FROM stats_minute WHERE day = ?1 AND minute = ?2))
                 WHERE day = ?1",
                params![at.day, at.minute],
            )?;
        }
        Ok(())
    }

    // ---------------- 查询 ----------------

    /// 视图查询：scope = "day" | "week" | "month" | "year" | "all"。
    /// 返回完整 JSON（指标卡 + 曲线序列 + 热力图 + Top 榜 + 纪录）；
    /// 不可用/未知 scope 返回 "{}"。
    pub fn query_json(&mut self, scope: &str) -> String {
        let at = Self::now();
        match self.query_inner(&at, scope) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[stats] 查询失败: {}", e);
                self.conn = None;
                "{}".to_string()
            }
        }
    }

    fn query_inner(&mut self, at: &DayMinute, scope: &str) -> rusqlite::Result<String> {
        let Some((from, to)) = scope_range(&at.day, scope) else {
            return Ok("{}".to_string()); // 未知 scope
        };
        let Some(conn) = self.conn() else {
            return Ok("{}".to_string());
        };

        // ── 汇总卡 ──
        let totals_sql = |pred: &str| {
            format!(
                "SELECT COALESCE(SUM(zh_chars),0), COALESCE(SUM(en_chars),0),
                   COALESCE(SUM(zh_keys),0), COALESCE(SUM(backspaces),0),
                   COALESCE(SUM(single_cnt),0), COALESCE(SUM(word_cnt),0), COALESCE(SUM(green_cnt),0),
                   COALESCE(SUM(first_pick_cnt),0), COALESCE(SUM(commit_cnt),0),
                   COALESCE(SUM(page_turns),0), COALESCE(SUM(active_minutes),0),
                   COALESCE(MAX(best_cpm),0), COALESCE(SUM(active_secs),0)
                 FROM stats_day {}",
                pred
            )
        };
        let totals = match (&from, &to) {
            (Some(f), Some(t)) => conn.query_row(
                &totals_sql("WHERE day BETWEEN ?1 AND ?2"),
                params![f, t],
                row13,
            )?,
            _ => conn.query_row(&totals_sql(""), params![], row13)?,
        };

        // ── 曲线序列 ──
        let series = Self::query_series(conn, at, scope, from.as_deref(), to.as_deref())?;

        // ── 按键热力图（key → [中, 英]）──
        let mut heat: Vec<(String, i64, i64)> = Vec::new();
        {
            let sql = match (&from, &to) {
                (Some(_), Some(_)) => {
                    "SELECT key, mode, COALESCE(SUM(cnt),0) FROM stats_keys
                     WHERE day BETWEEN ?1 AND ?2 GROUP BY key, mode"
                }
                _ => "SELECT key, mode, COALESCE(SUM(cnt),0) FROM stats_keys GROUP BY key, mode",
            };
            let mut stmt = conn.prepare(sql)?;
            let mut merge = |r: rusqlite::Result<(String, i64, i64)>| {
                if let Ok((k, mode, cnt)) = r {
                    match heat.iter_mut().find(|h| h.0 == k) {
                        Some(slot) => {
                            if mode == 0 {
                                slot.1 += cnt
                            } else {
                                slot.2 += cnt
                            }
                        }
                        None => {
                            if mode == 0 {
                                heat.push((k, cnt, 0))
                            } else {
                                heat.push((k, 0, cnt))
                            }
                        }
                    }
                }
            };
            match (&from, &to) {
                (Some(f), Some(t)) => {
                    for r in stmt.query_map(params![f, t], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                        ))
                    })? {
                        merge(r);
                    }
                }
                _ => {
                    for r in stmt.query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                        ))
                    })? {
                        merge(r);
                    }
                }
            }
        }

        // ── 热字 / 热词 Top100 ──
        let top_chars = Self::query_top(conn, "stats_chars", "ch", from.as_deref(), to.as_deref())?;
        let top_words =
            Self::query_top(conn, "stats_words", "word", from.as_deref(), to.as_deref())?;

        // ── 纪录（恒为全历史口径，不随视图切换）──
        let (best_cpm_ever, best_cpm_day) = conn
            .query_row(
                "SELECT best_cpm, day FROM stats_day WHERE best_cpm > 0
                 ORDER BY best_cpm DESC, day ASC LIMIT 1",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .unwrap_or((0, String::new()));
        let (best_day_chars, best_day) = conn
            .query_row(
                "SELECT zh_chars, day FROM stats_day WHERE zh_chars > 0
                 ORDER BY zh_chars DESC, day ASC LIMIT 1",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .unwrap_or((0, String::new()));
        let total_chars: i64 = conn
            .query_row("SELECT COALESCE(SUM(zh_chars),0) FROM stats_day", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        let total_days: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM stats_day WHERE zh_chars > 0 OR en_chars > 0 OR zh_keys > 0",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let streak = Self::query_streak(conn, &at.day)?;

        Ok(build_query_json(
            scope,
            &at.day,
            &totals,
            &series,
            &heat,
            &top_chars,
            &top_words,
            best_cpm_ever,
            &best_cpm_day,
            best_day_chars,
            &best_day,
            streak,
            total_chars,
            total_days,
        ))
    }

    /// 曲线序列按视图粒度：日=5 分钟桶（288 点）；周/月=逐日；年=逐月；全部=逐年。
    ///
    /// 每桶返回 `(标签, 中文字数, 英文字符, 翻页, 上屏, 绿词)`——后三项供 UI 计算
    /// **比率趋势**（翻页率 = 翻页÷中文字数、长句命中率 = 绿词÷上屏）。
    /// 日视图的分钟桶（`stats_minute`）只存字数，故后三项恒 0；比率趋势仅
    /// 周/月/年/全部可用（数据源 `stats_day` 有这些计数）。
    fn query_series(
        conn: &Connection,
        at: &DayMinute,
        scope: &str,
        from: Option<&str>,
        to: Option<&str>,
    ) -> rusqlite::Result<Vec<(String, i64, i64, i64, i64, i64)>> {
        let mut out: Vec<(String, i64, i64, i64, i64, i64)> = Vec::new();
        match scope {
            "day" => {
                let mut buckets = vec![(0i64, 0i64); 288];
                let mut stmt = conn.prepare(
                    "SELECT minute / 5, COALESCE(SUM(zh_chars),0), COALESCE(SUM(en_chars),0)
                     FROM stats_minute WHERE day = ?1 GROUP BY minute / 5",
                )?;
                let rows = stmt.query_map(params![at.day], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?;
                for r in rows.flatten() {
                    if (0..288).contains(&r.0) {
                        buckets[r.0 as usize] = (r.1, r.2);
                    }
                }
                for (i, (zh, en)) in buckets.iter().enumerate() {
                    // 分钟桶无翻页/上屏/绿词计数（stats_minute 只存字数）
                    out.push((format!("{}", i * 5), *zh, *en, 0, 0, 0));
                }
            }
            "week" | "month" => {
                // 逐日（无数据日补零，图表不断档）
                if let (Some(f), Some(t)) = (from, to) {
                    let mut map: std::collections::HashMap<String, (i64, i64, i64, i64, i64)> =
                        Default::default();
                    let mut stmt = conn.prepare(
                        "SELECT day, COALESCE(SUM(zh_chars),0), COALESCE(SUM(en_chars),0),
                                COALESCE(SUM(page_turns),0), COALESCE(SUM(commit_cnt),0),
                                COALESCE(SUM(green_cnt),0)
                         FROM stats_day WHERE day BETWEEN ?1 AND ?2 GROUP BY day",
                    )?;
                    let rows = stmt.query_map(params![f, t], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, i64>(5)?,
                        ))
                    })?;
                    for r in rows.flatten() {
                        map.insert(r.0, (r.1, r.2, r.3, r.4, r.5));
                    }
                    let mut d = f.to_string();
                    loop {
                        let (zh, en, pg, cm, gr) = map.get(&d).copied().unwrap_or((0, 0, 0, 0, 0));
                        out.push((d.clone(), zh, en, pg, cm, gr));
                        if d == *t {
                            break;
                        }
                        match shift_day_string(&d, 1) {
                            Some(n) => d = n,
                            None => break,
                        }
                    }
                }
            }
            "year" => {
                let year_prefix = &at.day[..4];
                let mut map: std::collections::HashMap<String, (i64, i64, i64, i64, i64)> =
                    Default::default();
                let mut stmt = conn.prepare(
                    "SELECT substr(day, 6, 2), COALESCE(SUM(zh_chars),0), COALESCE(SUM(en_chars),0),
                            COALESCE(SUM(page_turns),0), COALESCE(SUM(commit_cnt),0),
                            COALESCE(SUM(green_cnt),0)
                     FROM stats_day WHERE substr(day, 1, 4) = ?1 GROUP BY substr(day, 6, 2)",
                )?;
                let rows = stmt.query_map(params![year_prefix], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                })?;
                for r in rows.flatten() {
                    map.insert(r.0, (r.1, r.2, r.3, r.4, r.5));
                }
                for m in 1..=12 {
                    let key = format!("{:02}", m);
                    let (zh, en, pg, cm, gr) = map.get(&key).copied().unwrap_or((0, 0, 0, 0, 0));
                    out.push((format!("{}-{:02}", year_prefix, m), zh, en, pg, cm, gr));
                }
            }
            "all" => {
                let mut stmt = conn.prepare(
                    "SELECT substr(day, 1, 4), COALESCE(SUM(zh_chars),0), COALESCE(SUM(en_chars),0),
                            COALESCE(SUM(page_turns),0), COALESCE(SUM(commit_cnt),0),
                            COALESCE(SUM(green_cnt),0)
                     FROM stats_day GROUP BY substr(day, 1, 4) ORDER BY 1",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                })?;
                for r in rows.flatten() {
                    out.push((r.0, r.1, r.2, r.3, r.4, r.5));
                }
                if out.is_empty() {
                    out.push((at.day[..4].to_string(), 0, 0, 0, 0, 0));
                }
            }
            _ => {}
        }
        Ok(out)
    }

    fn query_top(
        conn: &Connection,
        table: &str,
        col: &str,
        from: Option<&str>,
        to: Option<&str>,
    ) -> rusqlite::Result<Vec<(String, i64)>> {
        let sql = |pred: &str| {
            format!(
                "SELECT {}, COALESCE(SUM(cnt),0) AS s FROM {} {} GROUP BY {} ORDER BY s DESC, {} ASC LIMIT {}",
                col, table, pred, col, col, TOP_LIMIT
            )
        };
        let mut out = Vec::new();
        match (from, to) {
            (Some(f), Some(t)) => {
                let mut stmt = conn.prepare(&sql("WHERE day BETWEEN ?1 AND ?2"))?;
                let rows = stmt.query_map(params![f, t], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?;
                for r in rows.flatten() {
                    out.push(r);
                }
            }
            _ => {
                let mut stmt = conn.prepare(&sql(""))?;
                let rows =
                    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                for r in rows.flatten() {
                    out.push(r);
                }
            }
        }
        Ok(out)
    }

    /// 连续打字天数（有任一活动 = 打字日）。今天还没打字时不打断连击。
    fn query_streak(conn: &Connection, today: &str) -> rusqlite::Result<i64> {
        let mut days: Vec<String> = Vec::new();
        let mut stmt = conn.prepare(
            "SELECT day FROM stats_day
             WHERE zh_chars > 0 OR en_chars > 0 OR zh_keys > 0
             ORDER BY day DESC",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for r in rows.flatten() {
            days.push(r);
        }
        let mut streak = 0i64;
        let mut expect = today.to_string();
        if days.first().map(|d| d.as_str()) != Some(today) {
            expect = match shift_day_string(today, -1) {
                Some(d) => d,
                None => return Ok(0),
            };
        }
        for d in &days {
            if *d == expect {
                streak += 1;
                expect = match shift_day_string(&expect, -1) {
                    Some(p) => p,
                    None => break,
                };
            } else if *d < expect {
                break; // 缺口
            }
        }
        Ok(streak)
    }

    // ---------------- 重置 ----------------

    /// 重置本时段：删除该时段覆盖的全部原始行（日表 + 热力 + 热字热词 + 分钟桶）。
    /// 父视图总量随查询自动减少（同一批事实表）。
    pub fn reset(&mut self, scope: &str) -> bool {
        let at = Self::now();
        match self.reset_inner(&at, scope) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[stats] 重置失败: {}", e);
                self.conn = None;
                false
            }
        }
    }

    fn reset_inner(&mut self, at: &DayMinute, scope: &str) -> rusqlite::Result<()> {
        let Some((from, to)) = scope_range(&at.day, scope) else {
            return Ok(());
        };
        let Some(conn) = self.conn() else {
            return Ok(());
        };
        let tx = conn.unchecked_transaction()?;
        for table in [
            "stats_day",
            "stats_keys",
            "stats_chars",
            "stats_words",
            "stats_minute",
        ] {
            match (&from, &to) {
                (Some(f), Some(t)) => {
                    tx.execute(
                        &format!("DELETE FROM {} WHERE day BETWEEN ?1 AND ?2", table),
                        params![f, t],
                    )?;
                }
                _ => {
                    tx.execute(&format!("DELETE FROM {}", table), [])?;
                }
            }
        }
        tx.commit()?;
        eprintln!("[stats] 已重置视图: {}", scope);
        Ok(())
    }

    // ---------------- 测试辅助（显式时间注入） ----------------

    #[cfg(test)]
    fn query_json_at(&mut self, scope: &str, today: &str) -> String {
        let at = DayMinute {
            day: today.to_string(),
            minute: 0,
        };
        self.query_inner(&at, scope)
            .unwrap_or_else(|_| "{}".to_string())
    }

    #[cfg(test)]
    fn reset_at(&mut self, scope: &str, today: &str) {
        let at = DayMinute {
            day: today.to_string(),
            minute: 0,
        };
        let _ = self.reset_inner(&at, scope);
    }
}

#[derive(PartialEq, Eq)]
enum CommitKind {
    Single,
    Word,
    Green,
}

fn row13(r: &rusqlite::Row) -> rusqlite::Result<Totals> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
        r.get(11)?,
        r.get(12)?,
    ))
}

/// CJK 表意文字判定（统一汉字 + 扩展 A + 兼容表意 + 扩展 B；不含 CJK 标点/
/// 全角符号——标点上屏不算「中文字数」）。
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF | 0x20000..=0x2A6DF)
}

// ---------------- 本地时间 ----------------

/// 本地（时区）当前日 + 当日分钟。
#[cfg(unix)]
fn local_now() -> DayMinute {
    use std::os::raw::{c_char, c_int, c_long};
    // struct tm（glibc/BSD 同布局：前 9 个 int + gmtoff/zone）
    #[repr(C)]
    struct Tm {
        tm_sec: c_int,
        tm_min: c_int,
        tm_hour: c_int,
        tm_mday: c_int,
        tm_mon: c_int,
        tm_year: c_int,
        tm_wday: c_int,
        tm_yday: c_int,
        tm_isdst: c_int,
        tm_gmtoff: c_long,
        tm_zone: *const c_char,
    }
    extern "C" {
        fn localtime_r(timep: *const i64, result: *mut Tm) -> *mut Tm;
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut tm: Tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !localtime_r(&secs, &mut tm).is_null() };
    if ok {
        DayMinute {
            day: format!(
                "{:04}-{:02}-{:02}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday
            ),
            minute: (tm.tm_hour as i64) * 60 + tm.tm_min as i64,
        }
    } else {
        utc_day_minute(secs)
    }
}

/// 非 unix 平台的兜底（UTC）。Windows 侧统计走服务端自有实现（GetLocalTime），
/// 本模块该分支不会被 mac 使用，仅为保持 x1core 跨平台可编译。
#[cfg(not(unix))]
fn local_now() -> DayMinute {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    utc_day_minute(secs)
}

fn utc_day_minute(secs: i64) -> DayMinute {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    DayMinute {
        day: format!("{:04}-{:02}-{:02}", y, m, d),
        minute: rem / 60,
    }
}

/// 当前本地日期字符串（'YYYY-MM-DD'）——供调用方展示/日志对齐时间口径。
pub fn today_string() -> String {
    local_now().day
}

// ---------------- 日期工具（纯函数，可测） ----------------

/// 解析 'YYYY-MM-DD' → 自 1970-01-01 的天数（格式非法返回 None）。
fn parse_day(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let m: i64 = s.get(5..7)?.parse().ok()?;
    let d: i64 = s.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// 日期 → 天数（Howard Hinnant civil_from_days 的逆变换）。
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 天数 → 日期（civil_from_days）。
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 'YYYY-MM-DD' 加减天数（格式非法返回 None）。
pub fn shift_day_string(day: &str, delta: i64) -> Option<String> {
    let days = parse_day(day)? + delta;
    let (y, m, d) = civil_from_days(days);
    Some(format!("{:04}-{:02}-{:02}", y, m, d))
}

/// 视图 → 覆盖日期闭区间（all = 无界）。周 = 周一至周日（中国惯例）。
/// days_from_civil(1970-01-01)=0 是周四 → wd = (days+4) mod 7，0=周日。
fn scope_range(today: &str, scope: &str) -> Option<(Option<String>, Option<String>)> {
    match scope {
        "day" => Some((Some(today.to_string()), Some(today.to_string()))),
        "week" => {
            // 滚动最近 7 日：自然周（周一~周日）窗口在
            // 周一打开时只剩当日一条实柱 + 6 条未来零柱，图表很难看；滚动窗口
            // 以今天收尾 [today-6, today]，任何一天看都是连续 7 天。
            // Windows 服务侧同步（契约统一：JSON 形状不变，仅窗口语义两端一致）。
            let from = shift_day_string(today, -6)?;
            Some((Some(from), Some(today.to_string())))
        }
        "month" => {
            let first = format!("{}-01", &today[..7]);
            let (y, m, _) = civil_from_days(parse_day(&first)?);
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            let next_month_first = format!("{:04}-{:02}-01", ny, nm);
            let last = shift_day_string(&next_month_first, -1)?;
            Some((Some(first), Some(last)))
        }
        "year" => Some((
            Some(format!("{}-01-01", &today[..4])),
            Some(format!("{}-12-31", &today[..4])),
        )),
        "all" => Some((None, None)),
        _ => None,
    }
}

// ---------------- 查询结果 JSON 拼装 ----------------
//
// 形状与 Windows 服务侧实现的 build_query_json 逐字一致
// （同一份契约：字段名、数组结构、无空白压缩形式）——统一标准，勿单方面改。

mod jsonw {
    pub enum J {
        Int(i64),
        Str(String),
        Arr(Vec<J>),
        Obj(Vec<(&'static str, J)>),
    }

    fn escape_into(s: &str, out: &mut String) {
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{08}' => out.push_str("\\b"),
                '\u{0C}' => out.push_str("\\f"),
                c if (c as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        }
    }

    impl J {
        pub fn to_json(&self) -> String {
            let mut out = String::new();
            self.write_to(&mut out);
            out
        }

        fn write_to(&self, out: &mut String) {
            match self {
                J::Int(i) => out.push_str(&i.to_string()),
                J::Str(s) => {
                    out.push('"');
                    escape_into(s, out);
                    out.push('"');
                }
                J::Arr(items) => {
                    out.push('[');
                    for (i, it) in items.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        it.write_to(out);
                    }
                    out.push(']');
                }
                J::Obj(kv) => {
                    out.push('{');
                    for (i, (k, v)) in kv.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        out.push('"');
                        out.push_str(k);
                        out.push_str("\":");
                        v.write_to(out);
                    }
                    out.push('}');
                }
            }
        }
    }
}

use jsonw::J;

#[allow(clippy::too_many_arguments)]
fn build_query_json(
    scope: &str,
    today: &str,
    t: &Totals,
    series: &[(String, i64, i64, i64, i64, i64)],
    heat: &[(String, i64, i64)],
    top_chars: &[(String, i64)],
    top_words: &[(String, i64)],
    best_cpm_ever: i64,
    best_cpm_day: &str,
    best_day_chars: i64,
    best_day: &str,
    streak: i64,
    total_chars: i64,
    total_days: i64,
) -> String {
    let totals = J::Obj(vec![
        ("zh", J::Int(t.0)),
        ("en", J::Int(t.1)),
        ("zhKeys", J::Int(t.2)),
        ("bs", J::Int(t.3)),
        ("single", J::Int(t.4)),
        ("word", J::Int(t.5)),
        ("green", J::Int(t.6)),
        ("firstPick", J::Int(t.7)),
        ("commits", J::Int(t.8)),
        ("pages", J::Int(t.9)),
        ("activeMin", J::Int(t.10)),
        ("bestCpm", J::Int(t.11)),
        ("activeSecs", J::Int(t.12)),
    ]);
    let series = J::Arr(
        series
            .iter()
            .map(|(l, zh, en, pg, cm, gr)| {
                J::Obj(vec![
                    ("l", J::Str(l.clone())),
                    ("zh", J::Int(*zh)),
                    ("en", J::Int(*en)),
                    // 加性扩展字段：比率趋势用（翻页率/长句命中率）。
                    // 日视图的分钟桶恒 0（该表只存字数）——UI 据此禁用日视图的比率曲线。
                    ("pg", J::Int(*pg)),
                    ("cm", J::Int(*cm)),
                    ("gr", J::Int(*gr)),
                ])
            })
            .collect(),
    );
    let heat = J::Arr(
        heat.iter()
            .map(|(k, zh, en)| J::Arr(vec![J::Str(k.clone()), J::Int(*zh), J::Int(*en)]))
            .collect(),
    );
    let top_chars = J::Arr(
        top_chars
            .iter()
            .map(|(c, n)| J::Arr(vec![J::Str(c.clone()), J::Int(*n)]))
            .collect(),
    );
    let top_words = J::Arr(
        top_words
            .iter()
            .map(|(w, n)| J::Arr(vec![J::Str(w.clone()), J::Int(*n)]))
            .collect(),
    );
    let records = J::Obj(vec![
        ("bestCpm", J::Int(best_cpm_ever)),
        ("bestCpmDay", J::Str(best_cpm_day.to_string())),
        ("bestDayChars", J::Int(best_day_chars)),
        ("bestDay", J::Str(best_day.to_string())),
        ("streak", J::Int(streak)),
        ("totalChars", J::Int(total_chars)),
        ("totalDays", J::Int(total_days)),
    ]);
    J::Obj(vec![
        ("scope", J::Str(scope.to_string())),
        ("today", J::Str(today.to_string())),
        ("totals", totals),
        ("series", series),
        ("heat", heat),
        ("topChars", top_chars),
        ("topWords", top_words),
        ("records", records),
    ])
    .to_json()
}

#[cfg(test)]
mod tests;
