//! 双库分离的运行期支撑：用户库迁移 + 系统词库覆盖更新。
//!
//! - **dict.db**（系统词库）= 种子母版的可覆盖副本，更新即整体替换
//! - **user.db**（用户库）= 全部自学习数据，更新永不触碰；卸载默认保留
//!
//! 本模块只做「文件级编排 + 一次性迁移」，不含任何查询/排序语义。

use rusqlite::Connection;
use std::path::Path;

/// 用户库携带的全部表（迁移逐表拷贝；顺序无关，均为独立表）。
pub(crate) const USER_TABLES: &[&str] = &[
    "user_words",
    "user_input_mappings",
    "user_pinyin_counts",
    "user_bigram_counts",
    "user_trigram_counts",
    "user_counts",
    "user_blacklist",
    "user_pins",
];

/// 属于用户侧的 db_meta 键（迁移时从旧库带走；种子侧键留在 dict.db）。
pub(crate) const USER_META_KEYS: &[&str] = &[
    "context_snapshot",
    "green_total_picks",
    "user_words_letters_cleaned",
    "user_words_multi_pinyin_v1",
    "bigram_migrated",
];

/// 迁移结果（供日志与测试断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitMigrationReport {
    /// 逐表迁移行数 (表名, 行数)。
    pub tables: Vec<(&'static str, i64)>,
    /// 迁移的用户侧 db_meta 键数。
    pub meta_keys: i64,
    /// 旧库改名后的备份路径。
    pub legacy_backup: String,
}

impl crate::storage::Storage {
    /// 旧单库（种子表 + 用户表混装）→ 新用户库的一次性迁移。
    ///
    /// **零搬运**：只把 `user_*` 表 + 用户侧 db_meta 拷进新 user.db，
    /// 旧文件**改名保留**（用户数据不可丢，确认健康前不删）。
    /// 系统词表由调用方从种子铺 dict.db（本函数不涉及）。
    ///
    /// 幂等/可重入：新用户库里已有 user_words 行时视为已迁移，直接返回 None
    /// （避免重复执行覆盖用户期间新产生的数据）。
    ///
    /// 返回 `Ok(None)` = 无需迁移（旧库不存在 / 已迁移 / 旧库不可读）。
    pub fn migrate_split_user_db_if_needed(
        user_db: &Path,
        legacy_db: &Path,
    ) -> rusqlite::Result<Option<SplitMigrationReport>> {
        if !legacy_db.exists() {
            return Ok(None);
        }
        // 已迁移判定：新用户库存在且已有 user_words 数据（或迁移标记）。
        if user_db.exists() {
            if let Ok(conn) =
                Connection::open_with_flags(user_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            {
                let done: bool = conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='user_words')",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap_or(false);
                if done {
                    let n: i64 = conn
                        .query_row("SELECT COUNT(*) FROM user_words", [], |r| r.get(0))
                        .unwrap_or(0);
                    let marked: bool = conn
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM db_meta WHERE key='split_migrated')",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap_or(false);
                    if n > 0 || marked {
                        return Ok(None);
                    }
                }
            }
        }

        // 建新用户库：**只建用户角色表**（分离后单文件只含自己角色的表）
        let user_conn = Connection::open(user_db)?;
        user_conn.busy_timeout(std::time::Duration::from_millis(3000))?;
        let mut user = crate::storage::Storage {
            dict: user_conn,
            user: None,
            dict_path: None,
        };
        user.init_user_schema_only()?;

        let mut report = SplitMigrationReport {
            tables: Vec::new(),
            meta_keys: 0,
            legacy_backup: String::new(),
        };

        // ATTACH 旧库（只读），SQL 级逐表拷贝——比逐行读回再写快一个量级。
        {
            let conn = user.user_mut();
            conn.execute(
                "ATTACH DATABASE ?1 AS legacy",
                rusqlite::params![legacy_db.to_string_lossy()],
            )?;
            // 事务包裹：中途失败整体回滚，不留半成品用户库
            let tx = conn.transaction()?;
            for table in USER_TABLES {
                let exists: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM legacy.sqlite_master WHERE type='table' AND name=?1)",
                        rusqlite::params![table],
                        |r| r.get(0),
                    )
                    .unwrap_or(false);
                if !exists {
                    continue;
                }
                let n = tx.execute(
                    &format!("INSERT INTO main.{table} SELECT * FROM legacy.{table}"),
                    [],
                )?;
                report.tables.push((table, n as i64));
            }
            // 用户侧 db_meta 键
            let mut meta_n = 0i64;
            for key in USER_META_KEYS {
                meta_n += tx.execute(
                    "INSERT OR REPLACE INTO main.db_meta(key, value)
                     SELECT key, value FROM legacy.db_meta WHERE key = ?1",
                    rusqlite::params![key],
                )? as i64;
            }
            report.meta_keys = meta_n;
            tx.execute(
                "INSERT OR REPLACE INTO main.db_meta(key, value) VALUES('split_migrated', ?1)",
                rusqlite::params![now_unix().to_string()],
            )?;
            tx.commit()?;
            conn.execute("DETACH DATABASE legacy", [])?;
        }

        // 完整性校验：不达标则不改名旧库（保留现场供排查）
        let ok: String = user
            .user()
            .query_row("PRAGMA integrity_check;", [], |r| r.get(0))
            .unwrap_or_default();
        drop(user);
        if ok != "ok" {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                Some(format!("用户库迁移后完整性校验失败: {ok}")),
            ));
        }

        // 旧库改名保留（不删——用户数据不可丢）
        let stamp = now_unix();
        let backup = legacy_db.with_extension(format!("db.migrated-{stamp}.bak"));
        rename_or_err(legacy_db, &backup)?;
        // WAL/SHM 残骸一并挪走（否则新库/旧副本混用会读错）
        for ext in ["db-wal", "db-shm"] {
            let p = legacy_db.with_extension(ext);
            if p.exists() {
                let _ = rename_or_err(&p, &backup.with_extension(ext));
            }
        }
        report.legacy_backup = backup.to_string_lossy().into_owned();
        Ok(Some(report))
    }

    /// 只建用户库表（不含系统词表）——迁移与分离模式引导用。
    pub(crate) fn init_user_schema_only(&mut self) -> rusqlite::Result<()> {
        self.user_mut().execute_batch(Self::SCHEMA_USER)?;
        self.run_user_migrations()?;
        Ok(())
    }
}

/// 本地修正标记键（写 dict.db 的修正通道写入后打标）。
///
/// 文本学习 / 词库管理器对系统词库的修正直接落 dict.db（dict.db 不是只读副本）。
/// 更新覆盖时据此**先备份**，杜绝修正被种子静默吞掉。
pub const LOCAL_CORRECTIONS_KEY: &str = "local_corrections";

/// 打本地修正标记（修正通道写完 dict.db 后调用；幂等）。
pub fn mark_dict_corrected(dict_db: &Path) -> rusqlite::Result<()> {
    let conn = Connection::open(dict_db)?;
    conn.execute(
        "INSERT OR REPLACE INTO db_meta(key, value) VALUES(?1, ?2)",
        rusqlite::params![LOCAL_CORRECTIONS_KEY, now_unix().to_string()],
    )?;
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
    Ok(())
}

/// 确保系统词库 DB 与种子母版一致（更新链核心）。
///
/// 判据：种子自带 `x1dict_sync_hash`（维护期 `freq_rebuild --hash-only` 写入）
/// 与 dict.db 记录的哈希不等 → 原子替换（tmp + fsync + rename）。
/// 相等 → 直接返回 false（微秒级，无全表扫描）。
///
/// **修正保护**：dict.db 带 `local_corrections` 标记（写过本地修正）时，
/// 覆盖前先把现存 dict.db 备份为 `dict.db.corrected-<时间戳>.bak`——修正要跨更新
/// 持久需走 promote 进种子流程，备份是防静默丢失的兜底。
///
/// 返回 `Ok(true)` = 执行了替换（调用方需重载内存态）。
pub fn ensure_dict_db(dict_db: &Path, seed_db: &Path) -> std::io::Result<bool> {
    // 首次铺开无条件执行：
    // 哈希门控只保护「已存在的 dict.db 不被未标定种子打回原形」，绝不能挡在
    // 缺失检查之前——安装目录只读（Program Files / mac 签名 bundle）时 WAL
    // 形态种子的 read_meta 会因 -wal/-shm 边车建不出而**静默失败返回 None**，
    // 门控在前会连首次复制都跳过：dict.db 空壳 → x1dict 空 → 引擎回退内置小词库。
    if !dict_db.exists() {
        // 孤儿边车清理：dict.db 不在但 -wal/-shm 残留（崩溃/手工删库）时，
        // 新库继承旧 WAL 同样会 malformed（见下方注释）
        drop_db_sidecars(dict_db);
        atomic_copy(seed_db, dict_db)?;
        strip_user_tables(dict_db);
        return Ok(true);
    }
    let seed_hash = read_meta(seed_db, "x1dict_sync_hash");
    if seed_hash.is_none() {
        // 种子无哈希（开发用未标定库）或不可读（只读目录 + WAL 边车建不出）
        // → 不替换，保留现有 dict.db（手工调优不打回原形）。
        return Ok(false);
    }
    let cur = read_meta(dict_db, "x1dict_sync_hash");
    if cur == seed_hash {
        return Ok(false);
    }
    // ① 折叠旧库 WAL：旧 dict.db 可能带未 checkpoint 的
    //    WAL（上次运行未正常关闭/被替换过）。先把已提交内容回写进**旧库自身**
    //    ——否则既丢内容（备份拿不到），又让残留 WAL 污染下一步。
    checkpoint_db_wal(dict_db);
    // 修正保护：带本地修正的 dict.db 先备份再覆盖（此时备份已含 WAL 内容）
    if let Some(stamp) = read_meta(dict_db, LOCAL_CORRECTIONS_KEY) {
        let bak = dict_db.with_extension(format!("db.corrected-{stamp}.bak"));
        if !bak.exists() {
            if let Err(e) = std::fs::copy(dict_db, &bak) {
                // 备份失败 = 覆盖会丢修正 → 中止本次替换（下次启动重试）
                return Err(std::io::Error::other(format!(
                    "dict.db 本地修正备份失败（{e}），中止覆盖以免丢失修正"
                )));
            }
        }
    }
    // ② 删除旧库边车（-wal/-shm）——**替换前必做**：atomic_copy 只换主文件，
    //    残留 WAL 的页属于旧代文件，新库配上它会被 SQLite 判定
    //    "database disk image is malformed"（页头/页数错位），或更隐蔽地
    //    被 strip_user_tables 的 wal_checkpoint 折叠进新库（旧代内容静默混入）。
    drop_db_sidecars(dict_db);
    atomic_copy(seed_db, dict_db)?;
    strip_user_tables(dict_db);
    Ok(true)
}

/// 把库的 WAL 已提交内容折叠回主文件（TRUNCATE checkpoint）。失败静默
/// （库不可读时后续替换照常进行，备份是原始字节拷贝仍可用）。
fn checkpoint_db_wal(db: &Path) {
    if let Ok(conn) = Connection::open(db) {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        // conn 在此 drop：非 persistent-WAL 模式下关闭即删空 WAL
    }
}

/// 删除库的 -wal / -shm 边车文件（替换/新建前的清理，见 ensure_dict_db 注释）。
fn drop_db_sidecars(db: &Path) {
    for ext in ["db-wal", "db-shm"] {
        let _ = std::fs::remove_file(db.with_extension(ext));
    }
}

/// 把库的边车文件挪到一旁（`<db>-wal.stale-<stamp>`）并返回是否挪动过——
/// 供分离引导**自愈**用（open_split 失败时若残留边车，挪开重试一次；
/// 挪动而非删除：数据可人工恢复，符合「用户数据不可丢」原则）。
pub fn quarantine_db_sidecars(db: &Path) -> bool {
    let stamp = now_unix();
    let mut moved = false;
    for ext in ["db-wal", "db-shm"] {
        let p = db.with_extension(ext);
        if p.exists() {
            let dst = db.with_extension(format!("{ext}.stale-{stamp}"));
            if std::fs::rename(&p, &dst).is_ok() {
                moved = true;
            }
        }
    }
    moved
}

/// 剥离 dict.db 里的用户表（复制自种子时的副产品）。
///
/// 种子母版历史上是「用户库的一次快照」，可能带空 `user_*` 表；分离后
/// dict.db 只该有系统表。这些空表对引擎完全无害（用户表一律经 user 连接读写），
/// 剥离只为「单文件只含自己角色的表」这一形态整洁 + 防误用（有人误开 dict.db
/// 当用户库时不会静默接受写入）。幂等：表不存在则跳过。
fn strip_user_tables(dict_db: &Path) {
    let Ok(conn) = Connection::open(dict_db) else {
        return;
    };
    for table in USER_TABLES {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                rusqlite::params![table],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if exists {
            let _ = conn.execute_batch(&format!("DROP TABLE IF EXISTS {table}"));
        }
    }
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
}

/// 读 db_meta 单键（只读打开，任何错误 → None）。
fn read_meta(db: &Path, key: &str) -> Option<String> {
    let conn = Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.query_row(
        "SELECT value FROM db_meta WHERE key = ?1",
        rusqlite::params![key],
        |r| r.get(0),
    )
    .ok()
}

/// 原子复制（tmp + fsync + 同卷 rename）——中断不产生半截目标文件。
fn atomic_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    let tmp = dst.with_extension("tmp-copy");
    std::fs::copy(src, &tmp)?;
    if let Ok(f) = std::fs::File::open(&tmp) {
        let _ = f.sync_all();
    }
    let _ = std::fs::remove_file(dst); // Windows rename 不覆盖已存在目标
    std::fs::rename(&tmp, dst)?;
    Ok(())
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// io::Error → rusqlite::Error（迁移函数的错误类型统一）。
fn rename_or_err(from: &Path, to: &Path) -> rusqlite::Result<()> {
    std::fs::rename(from, to).map_err(|e| {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
            Some(format!(
                "改名失败 {} -> {}: {e}",
                from.display(),
                to.display()
            )),
        )
    })
}
