//! x1dict_words 表 CRUD：系统词库的增删改查与批量导入。
//!
//! 从 storage.rs 拆出（mod.rs 模块化重构）。

use super::*;

impl crate::storage::Storage {
    // -----------------------------------------------------------------
    // X1Dict 基础词库导入
    // -----------------------------------------------------------------

    /// X1Dict 表是否为空（用于判断是否需要首次导入）
    pub fn x1dict_is_empty(&self) -> rusqlite::Result<bool> {
        let count: i64 = self
            .dict
            .query_row("SELECT COUNT(*) FROM x1dict_words", [], |row| row.get(0))?;
        Ok(count == 0)
    }

    /// 批量导入 X1Dict 词条（事务包裹 + prepared statement）
    ///
    /// 若已有同 `simplified` 词条则跳过（X1Dict 是只读参考表，不做 upsert）。
    pub fn import_x1dict_batch(&mut self, words: &[X1DictWord]) -> rusqlite::Result<usize> {
        if words.is_empty() {
            return Ok(0);
        }
        let tx = self.dict.transaction()?;
        let mut inserted = 0;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO x1dict_words
                    (simplified, pinyin, freq, is_primary)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for w in words {
                let n =
                    stmt.execute(params![w.simplified, w.pinyin, w.freq, w.is_primary as i64,])?;
                inserted += n;
            }
        }
        tx.commit()?;
        if inserted > 0 {
            self.bump_x1dict_rev()?; // 缓存失效键
        }
        Ok(inserted)
    }

    /// 清空 X1Dict 表（重新导入前调用）
    pub fn clear_x1dict(&mut self) -> rusqlite::Result<()> {
        self.dict.execute("DELETE FROM x1dict_words", [])?;
        self.bump_x1dict_rev()?; // 缓存失效键
        Ok(())
    }

    /// 用 compute_freq 重算全部 x1dict_words 的 freq，并清理拼音为空的脏词条。
    ///
    /// 触发场景：种子 DB 的 freq 因历史操作被破坏（多字词频极低，如「我们」=2、
    /// 「中文」=1，而 compute_freq 应给 9500/5800）。
    ///
    /// 安全性：只动 x1dict_words.freq，绝不碰 user_words 等用户表。
    /// 幂等：重复执行结果一致。
    ///
    /// 返回 `(清理脏词条数, 重算词条数)`。
    pub fn rebuild_x1dict_freq<F>(&mut self, compute: F) -> rusqlite::Result<(usize, usize)>
    where
        F: Fn(&str) -> u32,
    {
        // 第一步：清理拼音为空的脏词条（如「一定||1」）。
        let dirty = self.dict.execute(
            "DELETE FROM x1dict_words WHERE pinyin = '' OR pinyin IS NULL",
            [],
        )?;

        // 第二步：读取全部 (id, simplified)，逐条重算 freq。
        let rows: Vec<(i64, String)> = {
            let mut stmt = self
                .dict
                .prepare("SELECT id, simplified FROM x1dict_words")?;
            let iter = stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            iter.filter_map(|r| r.ok()).collect()
        };

        // 第三步：批量 UPDATE（事务内，compute_freq 是纯函数无副作用）。
        let tx = self.dict.transaction()?;
        let mut updated = 0usize;
        {
            let mut stmt = tx.prepare("UPDATE x1dict_words SET freq = ?2 WHERE id = ?1")?;
            for (id, simplified) in &rows {
                let new_freq = compute(simplified) as i64;
                stmt.execute(params![id, new_freq])?;
                updated += 1;
            }
        }
        tx.commit()?;
        self.bump_x1dict_rev()?; // 缓存失效键
        Ok((dirty, updated))
    }

    /// 把 x1dict_words.freq 压扁到 N 档。
    ///
    /// `tiers` 按 (原 freq 下限, 目标 freq) 升序排列，最后一条用 (0, ...) 兜底。
    /// 例：`&[(5000, 300), (1000, 200), (0, 100)]` 表示：
    /// - freq >= 5000 → 300（高频档：的/是/不/吃/我们）
    /// - 1000 <= freq < 5000 → 200（中频档：一般多字词）
    /// - freq < 1000 → 100（低频档：兜底字、冷僻词）
    ///
    /// 设计目的：把原 freq 的 200 倍差距（9999 vs 50）压扁到 3 倍（300 vs 100），
    /// 让 candidate_score 里 base 分量不再碾压 user 学习信号（recent/counts/bigram），
    /// 用户行为成为排序主导。同时保留 3 档区分度（default_pick
    /// 的 freq_ratio 守卫仍能在跨档场景生效）。
    ///
    /// 幂等性：再次执行按当前 freq 重新映射。建议只在 rebuild_x1dict_freq 之后
    /// 立即调用（重算出真实 freq 后压扁），不要单独对已压扁的 DB 再跑（会失去分级）。
    pub fn flatten_freq_to(&mut self, tiers: &[(u32, u32)]) -> rusqlite::Result<usize> {
        let rows: Vec<(i64, i64)> = {
            let mut stmt = self.dict.prepare("SELECT id, freq FROM x1dict_words")?;
            let iter =
                stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
            iter.filter_map(|r| r.ok()).collect()
        };
        let tx = self.dict.transaction()?;
        let mut updated = 0usize;
        {
            let mut stmt = tx.prepare("UPDATE x1dict_words SET freq = ?2 WHERE id = ?1")?;
            for (id, raw) in &rows {
                let f = (*raw).max(0) as u32;
                // 找到 lo <= f 的所有档位里 lo 最大的那个（即"最高匹配档位"）。
                // tiers 升序排列如 [(0,100),(1000,200),(5000,300)]，
                // 对 f=9999 应匹配 (5000,300)。
                let new_f = tiers
                    .iter()
                    .filter(|(lo, _)| f >= *lo)
                    .max_by_key(|(lo, _)| *lo)
                    .map(|(_, target)| *target)
                    .unwrap_or(1);
                stmt.execute(params![id, new_f as i64])?;
                updated += 1;
            }
        }
        tx.commit()?;
        if updated > 0 {
            self.bump_x1dict_rev()?; // 缓存失效键
        }
        Ok(updated)
    }

    /// 查询 X1Dict 表条目数
    pub fn x1dict_count(&self) -> rusqlite::Result<i64> {
        let count: i64 = self
            .dict
            .query_row("SELECT COUNT(*) FROM x1dict_words", [], |row| row.get(0))?;
        Ok(count)
    }

    /// 查询 x1dict_words 的 freq 分布统计（按字数分组）。
    ///
    /// 返回 `Vec<(字数组名, 词条数, 平均freq, 最小, 最大)>`，用于 freq 重算后核对。
    pub fn x1dict_freq_stats(&self) -> rusqlite::Result<Vec<(String, i64, f64, i64, i64)>> {
        let mut stmt = self.dict.prepare(
            "SELECT
                CASE
                    WHEN LENGTH(simplified) = 1 THEN '1单字'
                    WHEN LENGTH(simplified) = 2 THEN '2双字'
                    WHEN LENGTH(simplified) = 3 THEN '3三字'
                    ELSE '4+多字' END AS wlen,
                COUNT(*), CAST(AVG(freq) AS REAL), MIN(freq), MAX(freq)
             FROM x1dict_words GROUP BY wlen ORDER BY wlen",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        // Result<Vec<_>, _> 的 collect 会短路：遇错返回 Err，否则收集全部行。
        rows.collect()
    }

    /// 从内置词库同步 x1dict_words，保留 user_words 不变。
    ///
    /// 用于沙盒环境下避免调用外部 sqlite3 子进程。
    /// 调用方应保证本库已初始化 schema（open 时会自动创建）。
    pub fn sync_x1dict_from_builtin(&mut self, builtin_path: &str) -> rusqlite::Result<()> {
        // 使用 INSERT OR IGNORE 而非 DELETE + INSERT，只补充内置词库中有
        // 但用户 DB 中缺失的词，保留用户对已有系统词的修改（频率、删除等）。
        // 这样即使哈希判断失效或 db_meta 表异常，也不会破坏用户数据。
        //
        // 注意：不搬内置 DB 的 id（曾导致严重 bug）：内置 DB 的 id 是 importer
        // 按导入顺序分配的（谁 shui id=121842），但用户 DB 的 id 分配完全不同
        //（用户 DB 的 121842 可能是「龙阳」）。若 SELECT id 直接插入用户 DB，
        // 主键冲突会被 OR IGNORE 静默跳过，导致新词（如补全的「谁 shui」多音字）
        // 永远同步不进用户 DB，用户打 shui 打不出「谁」。
        //
        // 正确做法：不指定 id，让用户 DB 自动分配新 id；依赖 (simplified, pinyin)
        // 唯一约束（见 ensure_x1dict_unique_constraint）让 OR IGNORE 按业务键去重。
        //
        // 必须只读打开 builtin DB：原实现用 ATTACH DATABASE 打开 builtin（可读写），
        // SQLite WAL 会在 builtin DB 同目录生成 .db-wal/.db-shm 文件。builtin DB 在
        // bundle 的 Resources 目录里，这些 wal/shm 文件加进 bundle 后破坏 codesign
        // 密封（verify 报 "file added: x1pinyin.db-wal"），导致系统拒绝加载输入法。
        // 改用 open_readonly 打开 builtin，读到内存再批量插入用户 DB，builtin
        // 永远不被写，不生成 wal/shm。
        let builtin_storage = Storage::open_readonly(builtin_path)?;
        // 读出 builtin 全部 x1dict_words 到内存（只读连接，不写 builtin）。
        let builtin_words: Vec<(String, String, i64, i64)> = {
            let mut stmt = builtin_storage.conn().prepare(
                "SELECT simplified, pinyin, freq, COALESCE(is_primary, 0) FROM x1dict_words",
            )?;
            let rows = stmt.query_map([], |row| {
                let simplified: String = row.get(0)?;
                let pinyin: String = row.get(1)?;
                let mut freq: i64 = row.get(2)?;
                let is_primary: i64 = row.get(3)?;
                // defense-in-depth：任何 freq > MAX_FREQ（10000）视为异常，clamp 到 100（低频档）
                if freq > crate::char_freq::MAX_FREQ as i64 {
                    freq = 100;
                }
                Ok((simplified, pinyin, freq, is_primary))
            })?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            v
        };
        // 同步 seed_bigram_counts（必须在 drop builtin_storage 前读）：
        // 种子 bigram 表与 x1dict_words 同属"种子级权威数据"，需从 builtin 同步到用户 DB。
        // 用户 DB 的 seed_bigram_counts 是冷启动兜底数据源，必须随 app bundle 流转。
        // 只读 (prev,text,count)——新旧表形态通用（新形态无 updated_at）。
        let builtin_seed_bigram: Vec<(String, String, i64)> = {
            let exists: bool = builtin_storage
                .dict
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='seed_bigram_counts')",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if !exists {
                Vec::new()
            } else {
                let mut stmt = builtin_storage.conn().prepare(
                    "SELECT prev, text, count FROM seed_bigram_counts",
                )?;
                let rows = stmt.query_map([], |row| {
                    let prev: String = row.get(0)?;
                    let text: String = row.get(1)?;
                    let count: i64 = row.get(2)?;
                    Ok((prev, text, count))
                })?;
                let mut v = Vec::new();
                for r in rows {
                    v.push(r?);
                }
                v
            }
        };
        // 同步 seed_trigram_counts（镜像 seed_bigram 同步）：
        // 种子 trigram 表与 seed_bigram_counts 同属"种子级权威数据"，
        // 必须随 app bundle 从 builtin 同步到用户 DB（冷启动兜底数据源）。
        // 只读四列——新旧表形态通用（新形态无 updated_at）。
        let builtin_seed_trigram: Vec<(String, String, String, i64)> = {
            let exists: bool = builtin_storage
                .dict
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='seed_trigram_counts')",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if !exists {
                Vec::new()
            } else {
                let mut stmt = builtin_storage.conn().prepare(
                    "SELECT prev2, prev1, text, count FROM seed_trigram_counts"
                )?;
                let rows = stmt.query_map([], |row| {
                    let prev2: String = row.get(0)?;
                    let prev1: String = row.get(1)?;
                    let text: String = row.get(2)?;
                    let count: i64 = row.get(3)?;
                    Ok((prev2, prev1, text, count))
                })?;
                let mut v = Vec::new();
                for r in rows {
                    v.push(r?);
                }
                v
            }
        };
        // builtin_storage 在此处 drop（关闭只读连接，不生成 wal/shm）

        // 批量 INSERT OR IGNORE 到用户 DB（主连接）
        let tx = self.dict.transaction()?;
        {
            let mut ins = tx.prepare(
                "INSERT OR IGNORE INTO x1dict_words (simplified, pinyin, freq, is_primary)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (simplified, pinyin, freq, is_primary) in &builtin_words {
                ins.execute(rusqlite::params![simplified, pinyin, freq, is_primary])?;
            }
        }
        tx.commit()?;

        // 批量同步 seed_bigram_counts 到用户 DB（builtin 数据已在上面读到 builtin_seed_bigram）。
        // INSERT OR REPLACE 覆盖（让新版部署能更新种子搭配）；先 DELETE 保证完全一致。
        // 与 x1dict_words 的 INSERT OR IGNORE 不同——x1dict 尊重用户对词频的修改，
        // 但 seed_bigram 是只读种子（用户运行时不修改），直接覆盖更合理。幂等。
        // 目标库可能是旧 rowid 形态（updated_at NOT NULL 无默认值，
        // 见 ensure_seed_bigram_table 注释）——按 table_info 双形态选 INSERT。
        if !builtin_seed_bigram.is_empty() {
            self.ensure_seed_bigram_table()?;
            let legacy_shape = self.table_has_column("seed_bigram_counts", "updated_at")?;
            let tx2 = self.dict.transaction()?;
            {
                tx2.execute("DELETE FROM seed_bigram_counts", [])?;
                let mut ins = tx2.prepare(if legacy_shape {
                    "INSERT OR REPLACE INTO seed_bigram_counts (prev, text, count, updated_at)
                     VALUES (?1, ?2, ?3, 0.0)"
                } else {
                    "INSERT OR REPLACE INTO seed_bigram_counts (prev, text, count)
                     VALUES (?1, ?2, ?3)"
                })?;
                for (prev, text, count) in &builtin_seed_bigram {
                    ins.execute(rusqlite::params![prev, text, count])?;
                }
            }
            tx2.commit()?;
        }

        // 批量同步 seed_trigram_counts 到用户 DB（语义同上：只读种子，DELETE+全量覆盖，幂等；
        // 双形态兼容同上）。
        if !builtin_seed_trigram.is_empty() {
            self.ensure_seed_trigram_table()?;
            let legacy_shape = self.table_has_column("seed_trigram_counts", "updated_at")?;
            let tx3 = self.dict.transaction()?;
            {
                tx3.execute("DELETE FROM seed_trigram_counts", [])?;
                let mut ins = tx3.prepare(if legacy_shape {
                    "INSERT OR REPLACE INTO seed_trigram_counts (prev2, prev1, text, count, updated_at)
                     VALUES (?1, ?2, ?3, ?4, 0.0)"
                } else {
                    "INSERT OR REPLACE INTO seed_trigram_counts (prev2, prev1, text, count)
                     VALUES (?1, ?2, ?3, ?4)"
                })?;
                for (prev2, prev1, text, count) in &builtin_seed_trigram {
                    ins.execute(rusqlite::params![prev2, prev1, text, count])?;
                }
            }
            tx3.commit()?;
        }

        Ok(())
    }

    /// 用源 DB（种子）的 x1dict_words 频率覆盖当前 DB（用户）的 x1dict_words 频率
    /// （取消 freq 压扁后的数据修复迁移）。
    ///
    /// 背景：`ensure_freq_flattened` 的幂等性问题把用户 DB 的 freq 全压成 100，
    /// base 分量完全失效。此方法用种子 DB 的正确 freq（compute_freq 真实值 50-9999）
    /// 覆盖用户 DB 的 x1dict_words.freq，修复失真的排序基线。
    ///
    /// 与 sync_x1dict_from_builtin 的区别：
    /// - sync 用 INSERT OR IGNORE，只补缺失词，不更新已有词 freq（保护用户修改）
    /// - restore 用 UPDATE，强制更新已有词 freq（修复被破坏的数据）
    ///
    /// 安全保证：
    /// - 只动 x1dict_words.freq 和 is_primary，不碰 user_words/user_counts/
    ///   input_mappings/bigram/trigram 等用户数据表
    /// - 用户自造词（user_words）不在 x1dict_words，不受影响
    /// - 用户学习统计（counts/recent/bigram）独立于 freq，不受影响
    ///
    pub fn restore_x1dict_freq_from(&mut self, source_path: &str) -> rusqlite::Result<usize> {
        // 只读打开源 DB（避免在 bundle Resources 目录生成 wal/shm 破坏 codesign）
        let source_storage = Storage::open_readonly(source_path)?;
        // 读出源 DB 全部 x1dict_words 到内存
        let source_words: Vec<(String, String, i64, i64)> = {
            let mut stmt = source_storage.conn().prepare(
                "SELECT simplified, pinyin, freq, COALESCE(is_primary, 0) FROM x1dict_words",
            )?;
            let rows = stmt.query_map([], |row| {
                let simplified: String = row.get(0)?;
                let pinyin: String = row.get(1)?;
                let mut freq: i64 = row.get(2)?;
                let is_primary: i64 = row.get(3)?;
                if freq > crate::char_freq::MAX_FREQ as i64 {
                    freq = 100;
                }
                Ok((simplified, pinyin, freq, is_primary))
            })?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            v
        };
        // source_storage 在此处 drop

        // 批量 UPDATE 当前 DB 的 x1dict_words（按 simplified+pinyin 业务键匹配）
        let tx = self.dict.transaction()?;
        let updated = {
            let mut upd = tx.prepare(
                "UPDATE x1dict_words SET freq = ?3, is_primary = ?4
                 WHERE simplified = ?1 AND pinyin = ?2",
            )?;
            let mut count = 0usize;
            for (simplified, pinyin, freq, is_primary) in &source_words {
                let rows_affected =
                    upd.execute(rusqlite::params![simplified, pinyin, freq, is_primary])?;
                if rows_affected > 0 {
                    count += 1;
                }
            }
            count
        };
        tx.commit()?;
        if updated > 0 {
            self.bump_x1dict_rev()?; // 缓存失效键
        }
        Ok(updated)
    }

    /// 检测并执行 freq 压扁迁移。
    ///
    /// 老用户 DB（freq_flattened_v1 标记不存在）启动时调用一次，把所有 freq 压扁
    /// 到 3 档（100/200/300），与新版种子 DB 一致。压扁后写入 freq_flattened_v1
    /// 标记，后续启动直接跳过。
    ///
    /// 绕过 sync_x1dict_from_builtin 的 INSERT OR IGNORE 限制：sync 只补缺失词，
    /// 不更新已有词的 freq，所以老用户的高 freq 不会被自动压扁。本方法独立执行
    /// 压扁，确保老用户升级后行为与新用户一致。
    ///
    /// 只在真实用户 DB 执行：通过 db_meta 里是否有 x1dict_sync_hash 判断。
    /// 测试临时 DB（Storage::open 新建，未经过 sync）没有 sync_hash，跳过压扁，
    /// 避免破坏测试 setup 里精心构造的 freq 值（如"吃"=6800）。
    ///
    /// 幂等：已标记的 DB 直接返回 Ok(0)，不重复压扁。
    pub fn ensure_freq_flattened(&mut self) -> rusqlite::Result<usize> {
        // 已压扁过 → 跳过
        if let Some(v) = self.get_seed_meta("freq_flattened_v1")? {
            if v == "1" {
                return Ok(0);
            }
        }
        // 只对真实用户 DB 执行（有 x1dict_sync_hash 标记说明经过 sync 流程）
        // 测试临时 DB 没这个标记，跳过避免破坏 setup
        if self.get_seed_meta("x1dict_sync_hash")?.is_none() {
            return Ok(0);
        }
        // 压扁到 4 档（与 freq_rebuild --flatten-only 一致的映射）
        let updated = self.flatten_freq_to(&[
            (9000, 500), // 超高频档：的/是/不/我/你 等 40 个最常用字
            (3000, 300), // 高频档：常用字、高频多字词
            (1000, 200), // 中频档：一般多字词
            (0, 100),    // 低频档：兜底字、冷僻词
        ])?;
        // 写入标记，后续启动跳过
        self.set_seed_meta("freq_flattened_v1", "1")?;
        Ok(updated)
    }

    // -----------------------------------------------------------------
    // DB 元数据（db_meta 表）：用于追踪系统词库同步版本
    // -----------------------------------------------------------------

    /// 读取种子侧 db_meta 键值（双库分离后按角色分流）。
    ///
    /// 种子侧键：`x1dict_sync_hash`（本 dict.db 来自哪个种子版本）、
    /// `freq_*` / `x1dict_*` / `polyphone_migrated` 等词库维护标记。
    /// 用户侧键（`context_snapshot` / `green_total_picks` / `user_words_*` /
    /// `bigram_migrated`）走 `get_user_meta`。写错库会导致键在更新后丢失，
    /// 因此调用点必须显式选择正确的一侧。
    pub fn get_seed_meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
        let value: Option<String> = self
            .dict
            .query_row(
                "SELECT value FROM db_meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value)
    }

    /// 写入种子侧 db_meta 键值。
    pub fn set_seed_meta(&mut self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.dict.execute(
            "INSERT INTO db_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// 词条编辑版本（缓存失效键）：所有 `x1dict_words` 变更
    /// API 尾部自增。为什么不复用 `x1dict_sync_hash`：那个键的语义是
    /// 「与种子内容的比对基线」，拿它当编辑标记会让管理器删词被误判成
    /// 「种子变了」→ 下次启动 sync 把删掉的词恢复。
    /// 裸改 DB（绕过本模块 API）由缓存头的行数哨兵兜底（同级暴露面）。
    pub(crate) fn bump_x1dict_rev(&mut self) -> rusqlite::Result<()> {
        let cur = self
            .get_seed_meta("x1dict_rev")?
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        self.set_seed_meta("x1dict_rev", &(cur + 1).to_string())
    }

    /// 读取用户侧 db_meta 键值（用户库连接；合并模式即同一文件）。
    ///
    /// 用户侧键：`context_snapshot`（跨会话上下文栈）、`green_total_picks`、
    /// `user_words_letters_cleaned`、`user_words_multi_pinyin_v1`、`bigram_migrated`。
    pub fn get_user_meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
        let value: Option<String> = self
            .user()
            .query_row(
                "SELECT value FROM db_meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value)
    }

    /// 写入用户侧 db_meta 键值。
    pub fn set_user_meta(&mut self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.user_mut().execute(
            "INSERT INTO db_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// 表是否含指定列（种子 n-gram 表 WITHOUT ROWID 换代的双形态兼容判据：
    /// 老 rowid 形态有 updated_at，新形态没有）。
    fn table_has_column(&self, table: &str, column: &str) -> rusqlite::Result<bool> {
        // 表名/列名均为本文件内常量，无注入面；低频调用不进 prepare_cached。
        let sql = format!(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name = '{column}')"
        );
        self.dict.query_row(&sql, [], |row| row.get(0)).or(Ok(false))
    }

    /// 计算内置 DB 的内容哈希（x1dict_words + seed_bigram_counts + seed_trigram_counts）。
    ///
    /// 对所有行的 `(simplified, pinyin, freq)` + seed_bigram_counts 的
    /// `(prev, text, count)` + seed_trigram_counts 的 `(prev2, prev1, text, count)`
    /// 做确定性聚合哈希，用于检测内置 DB 是否发生变化。
    ///
    /// 修订：原版只哈希 x1dict_words，导致 seed_bigram_counts
    /// 变化时不触发 sync（用户 DB 的 seed_bigram_counts 永远空）。修订后把
    /// seed_bigram_counts 也纳入哈希，让种子 bigram 更新生效；
    /// seed_trigram_counts 同理纳入哈希（否则种子 trigram 更新不同步到用户 DB）。
    ///
    /// 返回十六进制字符串（16 字符，64 位）。
    pub fn x1dict_content_hash(&self) -> rusqlite::Result<String> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        // 第零部分：种子结构版本（n-gram 表 WITHOUT ROWID 换代）——
        // 结构变更不改变内容行 → 内容哈希不动 → 装机 dict.db 不重铺，旧形态
        // 表会让紧凑形态的写入方违约。结构版本进哈希 = 结构升级强制一次全量
        // 重铺（机制同后续 english_words/zh_en_words 入哈希）。
        "NGRAM_SCHEMA_V2_WOR".hash(&mut hasher);
        // 第一部分：x1dict_words
        let mut stmt = self.dict.prepare(
            "SELECT simplified, pinyin, freq FROM x1dict_words ORDER BY simplified, pinyin",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut count: u64 = 0;
        for r in rows {
            let (s, p, f) = r?;
            s.hash(&mut hasher);
            p.hash(&mut hasher);
            f.hash(&mut hasher);
            count += 1;
        }
        count.hash(&mut hasher);

        // 第二部分：seed_bigram_counts（如果表存在）
        // 表不存在（老 DB）跳过，哈希仅基于 x1dict_words（兼容老 DB）
        let seed_exists: bool = self.dict.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='seed_bigram_counts')",
            [],
            |row| row.get(0),
        ).unwrap_or(false);
        if seed_exists {
            let mut seed_stmt = self
                .dict
                .prepare("SELECT prev, text, count FROM seed_bigram_counts ORDER BY prev, text")?;
            let seed_rows = seed_stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            let mut seed_count: u64 = 0;
            for r in seed_rows {
                let (p, t, c) = r?;
                p.hash(&mut hasher);
                t.hash(&mut hasher);
                c.hash(&mut hasher);
                seed_count += 1;
            }
            seed_count.hash(&mut hasher);
        }

        // 第三部分：seed_trigram_counts（如果表存在）。
        // 与第二部分同理——种子 trigram 更新必须触发用户 DB sync，否则
        // 用户 DB 的 seed_trigram_counts 永远停留在旧版（或空）。
        let seed_tri_exists: bool = self.dict.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='seed_trigram_counts')",
            [],
            |row| row.get(0),
        ).unwrap_or(false);
        if seed_tri_exists {
            let mut tri_stmt = self.dict.prepare(
                "SELECT prev2, prev1, text, count FROM seed_trigram_counts ORDER BY prev2, prev1, text"
            )?;
            let tri_rows = tri_stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?;
            let mut tri_count: u64 = 0;
            for r in tri_rows {
                let (p2, p1, t, c) = r?;
                p2.hash(&mut hasher);
                p1.hash(&mut hasher);
                t.hash(&mut hasher);
                c.hash(&mut hasher);
                tri_count += 1;
            }
            tri_count.hash(&mut hasher);
        }

        // 第四部分：english_words：英文词表
        // 变更必须触发 dict.db 重铺（ensure_dict_db 哈希门控），否则装机侧
        // 永远停留在旧表。与第二/三部分同理——表不存在（老 DB）跳过。
        let en_exists: bool = self.dict.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='english_words')",
            [],
            |row| row.get(0),
        ).unwrap_or(false);
        if en_exists {
            let mut en_stmt = self
                .dict
                .prepare("SELECT word, pos, cn, freq FROM english_words ORDER BY word")?;
            let en_rows = en_stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?;
            let mut en_count: u64 = 0;
            for r in en_rows {
                let (w, pos, cn, f) = r?;
                w.hash(&mut hasher);
                pos.hash(&mut hasher);
                cn.hash(&mut hasher);
                f.hash(&mut hasher);
                en_count += 1;
            }
            en_count.hash(&mut hasher);
        }

        // 第五部分：zh_en_words：与第四部分
        // 同理——词表变更必须触发 dict.db 重铺（ensure_dict_db 哈希门控）。
        // 表不存在（老 DB）跳过。
        let zh_en_exists: bool = self.dict.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='zh_en_words')",
            [],
            |row| row.get(0),
        ).unwrap_or(false);
        if zh_en_exists {
            let mut zh_en_stmt = self
                .dict
                .prepare("SELECT zh, en, pos, freq FROM zh_en_words ORDER BY zh, en")?;
            let zh_en_rows = zh_en_stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?;
            let mut zh_en_count: u64 = 0;
            for r in zh_en_rows {
                let (zh, en, pos, f) = r?;
                zh.hash(&mut hasher);
                en.hash(&mut hasher);
                pos.hash(&mut hasher);
                f.hash(&mut hasher);
                zh_en_count += 1;
            }
            zh_en_count.hash(&mut hasher);
        }

        Ok(alloc::format!("{:016x}", hasher.finish()))
    }

    /// 判断是否需要从内置 DB 同步 x1dict_words。
    ///
    /// 比较内置 DB 的内容哈希与用户 DB 中记录的 `x1dict_sync_hash`：
    /// - 哈希不同（内置词库已更新）→ 返回 `true`
    /// - 哈希相同 → 返回 `false`（用户增删改系统词不会触发同步）
    /// - 无法读取内置 DB → 返回 `false`（保守不覆盖）
    /// - 用户 DB 无记录（首次安装）→ 返回 `true`
    pub fn x1dict_sync_needed(&self, builtin_path: &str) -> rusqlite::Result<bool> {
        // 读取内置 DB 的内容哈希（只读打开）
        let builtin_hash = match Storage::open_readonly(builtin_path) {
            Ok(builtin_storage) => builtin_storage.x1dict_content_hash()?,
            Err(_) => return Ok(false),
        };

        // 比较用户 DB 中记录的上次同步哈希
        match self.get_seed_meta("x1dict_sync_hash")? {
            None => Ok(true), // 首次安装或旧版本无记录
            Some(stored) => Ok(stored != builtin_hash),
        }
    }

    /// 记录当前内置 DB 的内容哈希到 `db_meta`，并附加 `x1dict_count`。
    ///
    /// 应在 `sync_x1dict_from_builtin` 成功后调用。
    pub fn record_x1dict_sync(&mut self, builtin_path: &str) -> rusqlite::Result<()> {
        let builtin_hash = match Storage::open_readonly(builtin_path) {
            Ok(builtin_storage) => builtin_storage.x1dict_content_hash()?,
            Err(_) => return Ok(()),
        };
        self.set_seed_meta("x1dict_sync_hash", &builtin_hash)?;
        self.bump_x1dict_rev()?; // sync 改写了 x1dict_words（INSERT OR IGNORE）→ 缓存失效
        Ok(())
    }

    // -----------------------------------------------------------------
    // 系统词库管理（供词库管理工具调用）
    // -----------------------------------------------------------------

    /// 分页查询 X1Dict 词条，支持按词语、拼音过滤
    pub fn list_x1dict_words(
        &self,
        filter: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> rusqlite::Result<Vec<X1DictWord>> {
        let mut words = Vec::new();
        if let Some(f) = filter {
            let escaped = f
                .replace("\\", "\\\\")
                .replace("%", "\\%")
                .replace("_", "\\_");
            let pattern = format!("%{}%", escaped);
            // 音节感知排序（「搜 de 看不到 地」）：REPLACE(pinyin,' ','')
            // LIKE '%de%' 的子串匹配会命中 deng/记得/觉得 等拼音内含 de 的词（实测
            // 命中 7168 行），高频次音字（地/de）被排到 200+ 位看不见。四级排序：
            // 0=拼音精确同音（的 de）→ 1=音节前缀（德国 de guo；LIKE 'de %' 只在
            // 音节边界匹配，不误中 deng）→ 2=词文本含 → 3=拼音内部含（记得）。
            // 同级按 freq 降序（原行为）。WHERE 保持宽匹配（文本搜索/兜底不变）。
            // prepare_cached：连续翻页/改页码每键一次 prepare ~0.5ms
            // 累积可感；缓存后复用编译好的语句。
            let syllable = escaped.trim().to_string();
            let mut stmt = self.dict.prepare_cached(
                "SELECT simplified, pinyin, freq, is_primary
                 FROM x1dict_words
                 WHERE simplified LIKE ?1 ESCAPE '\\'
                    OR REPLACE(pinyin, ' ', '') LIKE ?1 ESCAPE '\\'
                 ORDER BY CASE
                    WHEN pinyin = ?4 THEN 0
                    WHEN pinyin LIKE ?4 || ' %' ESCAPE '\\' THEN 1
                    WHEN simplified LIKE ?1 ESCAPE '\\' THEN 2
                    ELSE 3
                 END, freq DESC
                 LIMIT ?2 OFFSET ?3",
            )?;
            let rows = stmt.query_map(
                params![pattern, limit as i64, offset as i64, syllable],
                |row| {
                    Ok(X1DictWord {
                        simplified: row.get(0)?,
                        pinyin: row.get(1)?,
                        freq: row.get::<_, i64>(2)?,
                        is_primary: row.get::<_, i64>(3).map(|v| v != 0).unwrap_or(true),
                    })
                },
            )?;
            for r in rows {
                words.push(r?);
            }
        } else {
            // idx_x1dict_freq 使 ORDER BY freq DESC 走索引免排序——深 OFFSET
            // 从 308ms/页降到 8ms（索引见 storage/mod.rs schema）
            let mut stmt = self.dict.prepare_cached(
                "SELECT simplified, pinyin, freq, is_primary
                 FROM x1dict_words
                 ORDER BY freq DESC
                 LIMIT ?1 OFFSET ?2",
            )?;
            let rows = stmt.query_map(params![limit as i64, offset as i64], |row| {
                Ok(X1DictWord {
                    simplified: row.get(0)?,
                    pinyin: row.get(1)?,
                    freq: row.get::<_, i64>(2)?,
                    is_primary: row.get::<_, i64>(3).map(|v| v != 0).unwrap_or(true),
                })
            })?;
            for r in rows {
                words.push(r?);
            }
        }
        Ok(words)
    }

    /// 带过滤的 X1Dict 词条总数
    pub fn x1dict_count_filtered(&self, filter: Option<&str>) -> rusqlite::Result<i64> {
        if let Some(f) = filter {
            let escaped = f
                .replace("\\", "\\\\")
                .replace("%", "\\%")
                .replace("_", "\\_");
            let pattern = format!("%{}%", escaped);
            self.dict.query_row(
                "SELECT COUNT(*) FROM x1dict_words
                 WHERE simplified LIKE ?1 ESCAPE '\\'
                    OR REPLACE(pinyin, ' ', '') LIKE ?1 ESCAPE '\\'",
                params![pattern],
                |row| row.get(0),
            )
        } else {
            self.x1dict_count()
        }
    }

    /// 添加或替换一条系统词（以 simplified+pinyin 为业务键，先删除旧记录避免重复）。
    ///
    /// 本地修正标记：这是宿主前端 / 词库管理器修正系统词库的通道之一，写入后打
    /// `local_corrections` 标（更新覆盖 dict.db 前据此先备份，见 `split.rs`）。
    pub fn add_x1dict_word(&mut self, word: &X1DictWord) -> rusqlite::Result<usize> {
        // 事务包裹：DELETE+INSERT 原子——否则崩溃在中间 =
        // 旧词条已删、新词条未写，系统词静默丢失。
        let tx = self.dict.transaction()?;
        tx.execute(
            "DELETE FROM x1dict_words WHERE simplified = ?1 AND pinyin = ?2",
            params![word.simplified, word.pinyin],
        )?;
        let n = tx.execute(
            "INSERT INTO x1dict_words
                (simplified, pinyin, freq, is_primary)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                word.simplified,
                word.pinyin,
                word.freq,
                word.is_primary as i64,
            ],
        )?;
        tx.commit()?;
        self.mark_local_correction();
        self.bump_x1dict_rev()?; // 缓存失效键（管理器编辑口）
        Ok(n)
    }

    /// 打本地修正标记（幂等：已存在则不重复写）。
    ///
    /// 标记语义 =「本 dict.db 含有尚未 promote 进种子的本地修正」；更新覆盖 dict.db 时
    /// 据此先备份（`split.rs::ensure_dict_db`）。已存在则跳过，省一次写事务。
    /// 查询失败（db_meta 缺失等）静默不写，绝不干扰主流程。
    pub(crate) fn mark_local_correction(&self) {
        let exists: bool = self
            .dict
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM db_meta WHERE key = ?1)",
                params![crate::storage::split::LOCAL_CORRECTIONS_KEY],
                |r| r.get(0),
            )
            .unwrap_or(true);
        if exists {
            return;
        }
        let _ = self.dict.execute(
            "INSERT OR REPLACE INTO db_meta(key, value) VALUES(?1, ?2)",
            params![
                crate::storage::split::LOCAL_CORRECTIONS_KEY,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
                    .to_string()
            ],
        );
    }

    /// 更新一条系统词（按旧主键定位）。写入后打本地修正标记（同 add）。
    pub fn update_x1dict_word(
        &mut self,
        old_text: &str,
        old_pinyin: &str,
        word: &X1DictWord,
    ) -> rusqlite::Result<usize> {
        let n = self.dict.execute(
            "UPDATE x1dict_words
             SET simplified = ?1,
                 pinyin = ?2,
                 freq = ?3,
                 is_primary = ?4
             WHERE simplified = ?5 AND pinyin = ?6",
            params![
                word.simplified,
                word.pinyin,
                word.freq,
                word.is_primary as i64,
                old_text,
                old_pinyin,
            ],
        )?;
        self.mark_local_correction();
        self.bump_x1dict_rev()?; // 缓存失效键
        Ok(n)
    }

    /// 删除一条系统词。写入后打本地修正标记（同 add）。
    pub fn delete_x1dict_word(&mut self, text: &str, pinyin: &str) -> rusqlite::Result<usize> {
        let n = self.dict.execute(
            "DELETE FROM x1dict_words WHERE simplified = ?1 AND pinyin = ?2",
            params![text, pinyin],
        )?;
        self.mark_local_correction();
        self.bump_x1dict_rev()?; // 缓存失效键
        Ok(n)
    }

    /// 按 text 删除 x1dict_words 里所有同名词（不区分读音）。
    ///
    /// 用户 Command+Backspace 删除某词时调用，彻底从系统词库清除。
    /// 这样 DP 查 dict（含 x1dict_words 加载的 Trie）查不到该词，不再被选来拼绿词。
    /// 返回删除的行数。写入后打本地修正标记（同 add）。
    pub fn delete_x1dict_word_by_text(&mut self, text: &str) -> rusqlite::Result<usize> {
        let n = self.dict.execute(
            "DELETE FROM x1dict_words WHERE simplified = ?1",
            params![text],
        )?;
        self.mark_local_correction();
        self.bump_x1dict_rev()?; // 缓存失效键
        Ok(n)
    }

    // -----------------------------------------------------------------
    // 内存 Trie 填充：一次性加载全表
    // -----------------------------------------------------------------

    /// 流式加载全部 X1Dict 词条，逐行回调（内存优化）。
    ///
    /// 供 `Engine::new()` 启动时填充 Trie。替代 load_all_for_trie 的全量物化：
    /// 原实现先收集 `Vec<(String, String, u32, bool)>`（187k × 2 String 堆分配）
    /// 再插 Trie，启动期间峰值内存 ≈ 数据 3 份（SQLite 页缓存 + Vec 物化 + Trie）。
    /// 本方法在 SQL 迭代中逐行回调（闭包内直接 insert 进 Trie），峰值减约
    /// 60-80MB，且行错误即时传播（不积累到末尾）。
    ///
    /// 保留 `ORDER BY freq DESC`（load_all_for_trie 的返回顺序契约，tests.rs
    /// 有断言；Dict::insert 内部对节点候选重新排序，加载顺序不影响 Trie 结果）。
    /// `is_primary` 表示该 (字, 拼音) 组合的拼音是否为该字的主音，
    /// 用于排序时主音字优先于次音字。
    pub fn for_each_trie_word(
        &self,
        mut f: impl FnMut(&str, &str, u32, bool) -> rusqlite::Result<()>,
    ) -> rusqlite::Result<()> {
        let mut stmt = self.dict.prepare(
            "SELECT pinyin, simplified, freq, is_primary FROM x1dict_words ORDER BY freq DESC",
        )?;
        // 借用解码（启动优化）：`get_ref().as_str()` 借用 SQLite 行缓冲
        // （有效期到下一次 rows.next()），消除每行 2 次 String 分配（187k 行 =
        // 374k 次堆分配，实测启动省 ~15-20ms）。借用仅在回调内有效，回调不跨行
        // 存活，安全。as_str() 内部做 UTF-8 校验（非法字节报错走 ? 传播）。
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let pinyin = row.get_ref(0)?.as_str()?;
            let simplified = row.get_ref(1)?.as_str()?;
            let raw_freq: i64 = row.get(2)?;
            let is_primary: bool = row.get::<_, i64>(3).map(|v| v != 0).unwrap_or(true);
            // defense-in-depth clamp：任何 freq > MAX_FREQ（10000）视为异常，
            // clamp 到 100（低频档）。防止异常值进 Trie 导致候选排序错乱。
            let freq = if raw_freq.max(0) as u32 > crate::char_freq::MAX_FREQ {
                100
            } else {
                raw_freq.max(0) as u32
            };
            f(pinyin, simplified, freq, is_primary)?;
        }
        Ok(())
    }

    /// 加载全部 X1Dict 词条为 (pinyin, text, freq, is_primary) 四元组列表
    ///
    /// 供 `Engine::new()` 启动时填充 Trie。返回数量可达 ~12 万。
    /// `is_primary` 表示该 (字, 拼音) 组合的拼音是否为该字的主音，
    /// 用于排序时主音字优先于次音字。
    ///
    /// 实现包装 `for_each_trie_word`（流式），
    /// 不再全量物化（峰值内存 -60-80MB）；返回顺序契约不变（ORDER BY freq DESC）。
    /// 新代码优先用 `for_each_trie_word`（避免 Vec 物化），本方法保留供
    /// 测试/工具使用。
    pub fn load_all_for_trie(&self) -> rusqlite::Result<Vec<(String, String, u32, bool)>> {
        let mut out = Vec::new();
        self.for_each_trie_word(|pinyin, text, freq, is_primary| {
            out.push((pinyin.to_string(), text.to_string(), freq, is_primary));
            Ok(())
        })?;
        Ok(out)
    }

    /// 旧加密 DB 一次性迁移（加密/混淆已从引擎整体移除）。
    ///
    /// 历史发布版的用户 DB 三表（x1dict_words.freq /
    /// seed_bigram_counts.count / seed_trigram_counts.count）是 XOR 密文，
    /// db_meta 有 `freq_encryption_version=v1` 标记。加密代码已删除（无密钥可解），
    /// 迁移策略是丢弃密文数据：用明文种子覆盖 freq（restore_x1dict_freq_from）、
    /// 清空种子搭配表（下一次 sync_x1dict_from_builtin 从明文种子重建）、清除标记。
    /// user_words / input_mappings 等用户数据表不受影响。
    ///
    /// 返回 true=执行了迁移（调用方必须 reload 内存状态），false=无需迁移（无标记）。
    pub fn migrate_legacy_encrypted_db_if_needed(
        &mut self,
        builtin_path: &str,
    ) -> rusqlite::Result<bool> {
        // 无加密标记 → 明文 DB，无需迁移
        if self.get_seed_meta("freq_encryption_version")?.is_none() {
            return Ok(false);
        }
        // 1) 用 bundle 明文种子的 freq 覆盖密文垃圾（只动 x1dict_words.freq/is_primary）
        self.restore_x1dict_freq_from(builtin_path)?;
        // 2) 清空密文种子搭配表（下一次 sync 会从明文种子 DELETE+全量重建，幂等）
        let _ = self.dict.execute("DELETE FROM seed_bigram_counts", []);
        let _ = self.dict.execute("DELETE FROM seed_trigram_counts", []);
        // 3) 清除加密标记
        self.dict.execute(
            "DELETE FROM db_meta WHERE key = 'freq_encryption_version'",
            [],
        )?;
        Ok(true)
    }

    // ── 英文单词表查询（智能单词识别）──
    // 全部参数绑定 + prepare_cached（按键热路径；不拼接 SQL）。
    // word 主键 B 树：精确查 = 点查；前缀 = 字典序区间扫描（upper = 前缀末字节
    // +1），前缀族仅数十行，ORDER BY freq DESC 无需额外索引。
    // 无 DB（Engine::new 内置模式）由引擎侧判 None 短路，不进这里。

    /// 精确查词：返回 (pos, cn, freq)。翻译/词典功能后续直接消费 pos/cn。
    ///
    /// 大小写不敏感：word 列存规范展示词形（IBM / iPhone），
    /// 查询按 lower(word) 命中（表达式索引 idx_english_words_lower）。
    pub fn english_word_detail(&self, word: &str) -> Option<(String, String, i64)> {
        let key = word.to_ascii_lowercase();
        let stmt = self
            .dict
            .prepare_cached("SELECT pos, cn, freq FROM english_words WHERE lower(word) = ?1");
        let Ok(mut stmt) = stmt else {
            return None;
        };
        stmt.query_row(rusqlite::params![key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .ok()
    }

    /// 精确查词的轻量版：返回 (规范展示词形, freq)。
    ///
    /// 查询键 = lower(input)（输入不区分大小写），返回值是存储的
    /// 规范词形（打 `ibm` → 候选 `IBM`）。候选注入/纠错短路热路径。
    pub fn english_exact(&self, word: &str) -> Option<(String, i64)> {
        let key = word.to_ascii_lowercase();
        let stmt = self
            .dict
            .prepare_cached("SELECT word, freq FROM english_words WHERE lower(word) = ?1");
        let Ok(mut stmt) = stmt else {
            return None;
        };
        stmt.query_row(rusqlite::params![key], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .ok()
    }

    /// 前缀查词：`lower(word) >= lower(prefix) AND < upper`，freq 降序、
    /// 平局字典序，limit 条。返回规范展示词形（IBM / iPhone）。
    pub fn english_prefix_words(&self, prefix: &str, limit: usize) -> Vec<(String, i64)> {
        let key = prefix.to_ascii_lowercase();
        let Some(upper) = english_prefix_upper(&key) else {
            return Vec::new();
        };
        let stmt = self.dict.prepare_cached(
            "SELECT word, freq FROM english_words \
             WHERE lower(word) >= ?1 AND lower(word) < ?2 \
             ORDER BY freq DESC, word LIMIT ?3",
        );
        let Ok(mut stmt) = stmt else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map(rusqlite::params![key, upper, limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        }) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for r in rows {
            match r {
                Ok(pair) => out.push(pair),
                Err(_) => break,
            }
        }
        out
    }

    /// 中文释义反查（候选翻译副栏）：`cn` 包含 `term` 的英文词，
    /// 按种子 freq 降序取前 `limit` 行；引擎层只保留整条义项精确命中
    /// （子串提及不算对译，SQL 只负责缩小候选集合）。
    /// term 的 LIKE 通配符已转义（`%` / `_` / `\`）。
    ///
    /// 10k 行全表 LIKE 实测 ~0.3ms（选中项变化时才查，非按键热路径）；
    /// 结果只读、不参与学习。
    pub fn english_reverse_cn(
        &self,
        term: &str,
        limit: usize,
    ) -> Vec<(String, String, String, i64)> {
        let escaped = term
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let stmt = self.dict.prepare_cached(
            "SELECT word, pos, cn, freq FROM english_words \
             WHERE cn LIKE ?1 ESCAPE '\\' ORDER BY freq DESC LIMIT ?2",
        );
        let Ok(mut stmt) = stmt else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map(rusqlite::params![pattern, limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        }) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for r in rows {
            match r {
                Ok(pair) => out.push(pair),
                Err(_) => break,
            }
        }
        out
    }

    // ── 中译英词表查询（候选翻译副栏·中→英精确词典）──
    // 精确词级命中（PK (zh, en) 的 zh 前缀列 B 树点查）；参数绑定 +
    // prepare_cached。只读、不参与学习。

    /// 中→英精确查词：返回 (en, pos, freq) 多译列表，freq 降序、平局 en 字典序
    /// （确定性），最多 `limit` 条。空串/未命中返回空。
    pub fn zh_en_exact(&self, zh: &str, limit: usize) -> Vec<(String, String, i64)> {
        if zh.is_empty() {
            return Vec::new();
        }
        let stmt = self.dict.prepare_cached(
            "SELECT en, pos, freq FROM zh_en_words \
             WHERE zh = ?1 ORDER BY freq DESC, en LIMIT ?2",
        );
        let Ok(mut stmt) = stmt else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map(rusqlite::params![zh, limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        }) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for r in rows {
            match r {
                Ok(t) => out.push(t),
                Err(_) => break,
            }
        }
        out
    }

    // ── 英文词条 CRUD（词库管理器·英文词库）──
    // 写路径同 x1dict 写操作：标 local_corrections（种子重铺前自动备份，
    // split.rs::ensure_dict_db）。英文查询走 SQL 热路径，改完即时生效
    // （无内存 Trie，无需 reload）。

    /// 分页列表（filter = word 前缀或 cn 包含；默认 freq 降序）。
    /// 过滤大小写不敏感（lower(word)）。
    pub fn english_list_filtered(
        &self,
        filter: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Vec<EnglishWord> {
        let stmt = self.dict.prepare_cached(
            "SELECT word, pos, cn, freq FROM english_words \
             WHERE (?1 IS NULL OR lower(word) LIKE ?1 || '%' OR cn LIKE '%' || ?1 || '%') \
             ORDER BY freq DESC, word LIMIT ?2 OFFSET ?3",
        );
        let Ok(mut stmt) = stmt else {
            return Vec::new();
        };
        let f = filter
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase());
        let Ok(rows) = stmt.query_map(rusqlite::params![f, limit as i64, offset as i64], |row| {
            Ok(EnglishWord {
                word: row.get(0)?,
                pos: row.get(1)?,
                cn: row.get(2)?,
                freq: row.get(3)?,
            })
        }) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for r in rows {
            match r {
                Ok(w) => out.push(w),
                Err(_) => break,
            }
        }
        out
    }

    /// 过滤后总数（大小写不敏感）。
    pub fn english_count_filtered(&self, filter: Option<&str>) -> i64 {
        let stmt = self.dict.prepare_cached(
            "SELECT COUNT(*) FROM english_words \
                 WHERE (?1 IS NULL OR lower(word) LIKE ?1 || '%' OR cn LIKE '%' || ?1 || '%')",
        );
        let Ok(mut stmt) = stmt else {
            return -1;
        };
        let f = filter
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase());
        stmt.query_row(rusqlite::params![f], |r| r.get(0))
            .unwrap_or(-1)
    }

    /// 新增/覆盖一条英文词（大小写变体视为同一条：按 lower(word) 先删旧行）。
    ///
    /// word 列存规范展示词形（IBM / iPhone），不是查询键。`is_word_shape`
    /// 已在引擎层校验（纯字母 2..=24，允许混合大小写）。
    pub fn english_upsert(&self, w: &EnglishWord) -> bool {
        let key = w.word.to_ascii_lowercase();
        let del = self
            .dict
            .prepare_cached("DELETE FROM english_words WHERE lower(word) = ?1");
        let Ok(mut del) = del else {
            return false;
        };
        if del.execute(rusqlite::params![key]).is_err() {
            return false;
        }
        let stmt = self.dict.prepare_cached(
            "INSERT INTO english_words(word, pos, cn, freq) VALUES(?1, ?2, ?3, ?4)",
        );
        let Ok(mut stmt) = stmt else {
            return false;
        };
        if stmt
            .execute(rusqlite::params![w.word, w.pos, w.cn, w.freq])
            .is_err()
        {
            return false;
        }
        self.mark_local_correction();
        true
    }

    /// 更新（可改 word 主键：旧词删除 + 新词写入，两步非事务——同 x1dict
    /// 更新路径的宽松语义）。old/new 均按 lower 匹配，支持只改大小写
    ///（ibm → IBM 不会留下两行）。
    pub fn english_update(&self, old_word: &str, w: &EnglishWord) -> bool {
        let del = self
            .dict
            .prepare_cached("DELETE FROM english_words WHERE lower(word) = lower(?1)");
        let Ok(mut del) = del else {
            return false;
        };
        let _ = del.execute(rusqlite::params![old_word]);
        self.english_upsert(w)
    }

    /// 删除一条英文词（幂等：不存在返回 false；大小写不敏感）。
    pub fn english_delete(&self, word: &str) -> bool {
        let stmt = self
            .dict
            .prepare_cached("DELETE FROM english_words WHERE lower(word) = lower(?1)");
        let Ok(mut stmt) = stmt else {
            return false;
        };
        let n = stmt.execute(rusqlite::params![word]).unwrap_or(0);
        if n == 0 {
            return false;
        }
        self.mark_local_correction();
        true
    }
}

/// 前缀的字典序开区间上界：末字节 +1（"cat" → "cau"；"cuz" → "cu{"，
/// ASCII 序仍正确）。全 0xFF（理论不可达，防御）返回 None。
fn english_prefix_upper(prefix: &str) -> Option<String> {
    let mut bytes = prefix.as_bytes().to_vec();
    for i in (0..bytes.len()).rev() {
        if bytes[i] < 0xFF {
            bytes[i] += 1;
            bytes.truncate(i + 1);
            return Some(String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    None
}

#[cfg(test)]
impl Storage {
    /// 测试辅助：批量插英文词（engine::english 单测用；同 crate 可见）。
    pub(crate) fn english_insert_for_test(&self, rows: &[(&str, &str, &str, i64)]) {
        let mut stmt = self
            .dict
            .prepare(
                "INSERT OR REPLACE INTO english_words(word, pos, cn, freq) VALUES(?1, ?2, ?3, ?4)",
            )
            .unwrap();
        for (w, pos, cn, f) in rows {
            stmt.execute(rusqlite::params![w, pos, cn, f]).unwrap();
        }
    }
}

/// 英文词条（管理器表格行：word, pos, cn, freq）。crate::storage 顶层 re-export。
#[derive(Debug, Clone, Default)]
pub struct EnglishWord {
    pub word: String,
    pub pos: String,
    pub cn: String,
    pub freq: i64,
}

#[cfg(test)]
mod english_crud_tests {
    use super::*;

    #[test]
    fn english_crud_roundtrip() {
        let st = Storage::open_memory().unwrap();
        // 新增（参数绑定）
        assert!(st.english_upsert(&EnglishWord {
            word: "dog".into(),
            pos: "n.".into(),
            cn: "狗".into(),
            freq: 900,
        }));
        // 搜索命中（word 前缀 / cn 包含）
        assert_eq!(st.english_count_filtered(Some("do")), 1);
        assert_eq!(st.english_count_filtered(Some("狗")), 1);
        assert_eq!(st.english_count_filtered(None), 1);
        let page = st.english_list_filtered(Some("do"), 10, 0);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].word, "dog");
        assert_eq!(page[0].cn, "狗");
        // 更新（主键替换语义：旧词删除 + 新词写入，word 可改名）
        assert!(st.english_update(
            "dog",
            &EnglishWord {
                word: "doggo".into(),
                pos: "n.".into(),
                cn: "狗（口语）".into(),
                freq: 800,
            }
        ));
        assert_eq!(st.english_count_filtered(None), 1);
        assert_eq!(st.english_exact("dog"), None);
        assert_eq!(st.english_exact("doggo"), Some(("doggo".to_string(), 800)));
        // 删除
        assert!(st.english_delete("doggo"));
        assert_eq!(st.english_count_filtered(None), 0);
        assert!(!st.english_delete("doggo")); // 幂等：再删 false
    }

    /// 大小写规范形：word 存规范展示词形，查询不区分大小写；
    /// 大小写变体按 lower 视为同一条（upsert 先删旧、update 可只改大小写）。
    #[test]
    fn english_case_insensitive_with_display_form() {
        let st = Storage::open_memory().unwrap();
        assert!(st.english_upsert(&EnglishWord {
            word: "IBM".into(),
            pos: "n.".into(),
            cn: "国际商业机器公司".into(),
            freq: 1423,
        }));
        // 小写输入命中，返回规范词形
        assert_eq!(st.english_exact("ibm"), Some(("IBM".to_string(), 1423)));
        assert_eq!(st.english_exact("IBM"), Some(("IBM".to_string(), 1423)));
        assert_eq!(
            st.english_prefix_words("ib", 10),
            vec![("IBM".to_string(), 1423)]
        );
        assert_eq!(st.english_count_filtered(Some("IB")), 1);
        assert_eq!(st.english_list_filtered(Some("ibm"), 1, 0)[0].word, "IBM");

        // 大小写变体 = 同一条：upsert 小写会替换规范形（而不是新增第二行）
        assert!(st.english_upsert(&EnglishWord {
            word: "ibm".into(),
            pos: "n.".into(),
            cn: "国际商业机器公司".into(),
            freq: 1423,
        }));
        assert_eq!(st.english_count_filtered(None), 1);
        assert_eq!(st.english_exact("ibm"), Some(("ibm".to_string(), 1423)));

        // update 只改大小写：不残留第二行
        assert!(st.english_update(
            "ibm",
            &EnglishWord {
                word: "IBM".into(),
                pos: "n.".into(),
                cn: "国际商业机器公司".into(),
                freq: 1423,
            }
        ));
        assert_eq!(st.english_count_filtered(None), 1);
        assert_eq!(st.english_exact("ibm"), Some(("IBM".to_string(), 1423)));
    }
}
