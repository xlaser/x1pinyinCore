//! DB 结构版本验证（自定义数据目录配套：迁移来的库可能来自不同版本，
//! 老结构带新代码会出问题；降级安装遇新结构必须隔离）。
//!
//! 设计：
//! - `db_meta.schema_version` 标记（缺省视为 1 = 本机制引入前的全部历史库）。
//! - 校验只在部署期引导链（服务 bootstrap / 迁移验证），不进热路径
//!   （启动速度优先——Storage::open 零检查不变）。
//! - 三态：`Ok`（等于期望，原样用）/ `Legacy`（旧版或无标记 → 补标记即可，
//!   缺表由 Storage::open 的 CREATE IF NOT EXISTS 幂等补齐）/ `Newer`
//!   （未知更新版本 = 降级安装：不静默动数据——调用方 quarantine 改名
//!   保留 + 重建，杜绝老代码读新结构带出问题）。
//!
//! 表清单核对：user 库按 split.rs 的 USER_TABLES 口径 + 运行时自建表
//!（competition 系列为双端统计契约表）；dict 库按词库三表。缺表不在这里
//! 建（open 会建），只报告——bootstrap 据此打日志。

use rusqlite::Connection;
use std::path::Path;

/// 当前期望的 user.db 结构版本。
pub const USER_SCHEMA_VERSION: u32 = 1;
/// 当前期望的 dict.db 结构版本。
pub const DICT_SCHEMA_VERSION: u32 = 1;

/// user.db 应存在的表（USER_TABLES 全量 + 运行时表；db_meta 恒有）。
pub const USER_EXPECTED_TABLES: &[&str] = &[
    "user_words",
    "user_input_mappings",
    "user_pinyin_counts",
    "user_bigram_counts",
    "user_trigram_counts",
    "user_counts",
    "user_blacklist",
    "user_pins",
    "user_english_counts",
    "competition",
    "competition_totals",
];

/// dict.db 应存在的表。
pub const DICT_EXPECTED_TABLES: &[&str] = &["x1dict_words", "db_meta"];

/// 校验结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaState {
    /// 与期望一致（或已按期望标记）。
    Ok,
    /// 旧版/无标记：合法升级路径（缺标记补写即可）。
    Legacy,
    /// 比二进制期望更新（降级安装）：调用方必须 quarantine。
    Newer(u32),
    /// 库打不开/读不出（损坏走既有 corrupt 链，不归本模块）。
    Unreadable(String),
}

/// 读 db_meta.schema_version（只读，不写）。
fn read_schema_version(conn: &Connection) -> Option<u32> {
    conn.query_row(
        "SELECT value FROM db_meta WHERE key='schema_version'",
        [],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|v| v.parse().ok())
}

/// 校验单个库的结构版本 + 表清单（只读）。
///
/// 返回 (版本态, 缺失表清单)。缺表非致命（Storage::open 幂等补建），
/// 仅供日志；`Newer` 才是必须处置的态。
pub fn verify(
    conn: &Connection,
    expected_version: u32,
    expected_tables: &[&'static str],
) -> (SchemaState, Vec<&'static str>) {
    let state = match read_schema_version(conn) {
        None => SchemaState::Legacy,
        Some(v) if v == expected_version => SchemaState::Ok,
        Some(v) if v < expected_version => SchemaState::Legacy,
        Some(v) => SchemaState::Newer(v),
    };
    let mut missing = Vec::new();
    for t in expected_tables {
        let ok: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                [t],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if !ok {
            missing.push(*t);
        }
    }
    (state, missing)
}

/// 补写 schema_version 标记（bootstrap 期一次性写；幂等）。
pub fn stamp(conn: &Connection, version: u32) -> rusqlite::Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS db_meta (key TEXT PRIMARY KEY, value TEXT)",
        [],
    )?;
    conn.execute(
        "INSERT INTO db_meta(key,value) VALUES('schema_version',?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [version.to_string()],
    )?;
    Ok(())
}

/// 引导链单库预检：文件不存在 = 全新（Ok 空态）；存在则开短连接校验。
/// 打不开 = Unreadable（走既有 corrupt 改名链，调用方处置）。
pub fn preflight(
    path: &Path,
    expected_version: u32,
    expected_tables: &[&'static str],
) -> (SchemaState, Vec<&'static str>) {
    if !path.exists() {
        return (SchemaState::Ok, Vec::new());
    }
    let conn = match Connection::open(path) {
        Ok(c) => c,
        Err(e) => return (SchemaState::Unreadable(e.to_string()), Vec::new()),
    };
    verify(&conn, expected_version, expected_tables)
}

/// 快速完整性校验（PRAGMA quick_check）：迁移验证用（部署期，不进热路径）。
/// 文件不存在 = false；打开失败/结果非 ok = false。
pub fn quick_check(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let Ok(conn) = Connection::open(path) else {
        return false;
    };
    conn.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
        .map(|v| v.eq_ignore_ascii_case("ok"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db(tag: &str) -> String {
        let p = crate::test_tmp::dir().join(format!("schema_{tag}_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p.to_str().unwrap().to_string()
    }

    #[test]
    fn fresh_db_is_ok() {
        let p = tmp_db("fresh");
        let (state, missing) = preflight(Path::new(&p), USER_SCHEMA_VERSION, USER_EXPECTED_TABLES);
        assert_eq!(state, SchemaState::Ok);
        assert!(missing.is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn legacy_no_marker_upgrades_by_stamp() {
        let p = tmp_db("legacy");
        {
            let conn = Connection::open(&p).unwrap();
            conn.execute(
                "CREATE TABLE db_meta (key TEXT PRIMARY KEY, value TEXT)",
                [],
            )
            .unwrap();
            conn.execute("CREATE TABLE user_words (id INTEGER PRIMARY KEY)", [])
                .unwrap();
        }
        // 无标记 → Legacy；补标记 → Ok
        let (state, missing) = preflight(Path::new(&p), USER_SCHEMA_VERSION, USER_EXPECTED_TABLES);
        assert_eq!(state, SchemaState::Legacy);
        assert!(missing.contains(&"user_input_mappings"));
        {
            let conn = Connection::open(&p).unwrap();
            stamp(&conn, USER_SCHEMA_VERSION).unwrap();
        }
        let (state, _) = preflight(Path::new(&p), USER_SCHEMA_VERSION, USER_EXPECTED_TABLES);
        assert_eq!(state, SchemaState::Ok);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn newer_version_flagged() {
        let p = tmp_db("newer");
        {
            let conn = Connection::open(&p).unwrap();
            stamp(&conn, USER_SCHEMA_VERSION + 5).unwrap();
        }
        let (state, _) = preflight(Path::new(&p), USER_SCHEMA_VERSION, USER_EXPECTED_TABLES);
        assert_eq!(state, SchemaState::Newer(USER_SCHEMA_VERSION + 5));
        let _ = std::fs::remove_file(&p);
    }
}
