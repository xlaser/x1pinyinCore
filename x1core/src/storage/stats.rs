//! 运行时统计三表：user_input_mappings / user_pinyin_counts / user_bigram_counts / user_counts。
//!
//! 内存优先架构下的 flush 落盘接口。
//! 从 storage.rs 拆出（mod.rs 模块化重构）。

use super::*;

impl crate::storage::Storage {
    // -----------------------------------------------------------------
    // 输入映射（input → text）：记录用户在特定原始输入下的选择习惯
    // -----------------------------------------------------------------

    /// 加载全部输入映射为 HashMap<(input, text), count>
    pub fn load_input_mappings(&self) -> rusqlite::Result<HashMap<(String, String), u32>> {
        let mut stmt = self
            .user()
            .prepare("SELECT input, text, count FROM user_input_mappings")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2).map(|v| v.max(0) as u32)?,
            ))
        })?;
        let mut out = HashMap::new();
        for r in rows {
            let (input, text, count) = r?;
            out.insert((input, text), count);
        }
        Ok(out)
    }

    /// 全量替换输入映射表。
    ///
    /// 内存优先架构下的落盘接口：调用方在内存中维护完整映射，
    /// 只在 flush 时把当前快照写回 SQLite。
    pub fn replace_input_mappings(
        &mut self,
        entries: &[(String, String, u32)],
    ) -> rusqlite::Result<usize> {
        // diff 更新（IO 优化）：原 DELETE 全表 + 重插（万行级 =
        // 每次 flush 1-2MB WAL，失活 flush 无节流时「几秒几个 MB」实测），
        // 改写前 diff——只 upsert 变化行、只删消失行。语义保持全量快照
        // （多余行删除 = 自愈性保留），写入量降到真实变化量（典型几十行）。
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let mut existing: std::collections::HashMap<(String, String), i64> =
            std::collections::HashMap::new();
        {
            let mut stmt = self
                .user()
                .prepare("SELECT input, text, count FROM user_input_mappings")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for r in rows {
                let (i, t, c) = r?;
                existing.insert((i, t), c);
            }
        }
        let keep: std::collections::HashSet<(&str, &str)> = entries
            .iter()
            .map(|(i, t, _)| (i.as_str(), t.as_str()))
            .collect();
        let tx = self.user_mut().transaction()?;
        let mut changed = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO user_input_mappings (input, text, count, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(input, text) DO UPDATE SET
                    count = excluded.count,
                    updated_at = excluded.updated_at",
            )?;
            for (input, text, count) in entries {
                let c = *count as i64;
                if existing.get(&(input.clone(), text.clone())) != Some(&c) {
                    stmt.execute(params![input, text, c, now])?;
                    changed += 1;
                }
            }
        }
        {
            let mut stmt =
                tx.prepare("DELETE FROM user_input_mappings WHERE input = ?1 AND text = ?2")?;
            for (i, t) in existing.keys() {
                if !keep.contains(&(i.as_str(), t.as_str())) {
                    stmt.execute(params![i, t])?;
                    changed += 1;
                }
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// 删除某 text 对应的所有 input_mapping 记录（用户删词时清理关联输入习惯）。
    ///
    /// 删除时必须连带清理：remove 只清 user_words，input_mappings 残留
    /// 会让被删的词通过 mapping_bonus 重新排到前面（如 panduan→判读安 删词后仍出现）。
    pub fn delete_input_mappings_by_text(&self, text: &str) -> rusqlite::Result<usize> {
        self.user().execute(
            "DELETE FROM user_input_mappings WHERE text = ?1",
            params![text],
        )
    }

    // -----------------------------------------------------------------
    // 用户拼音级频率（user_pinyin_counts 表）
    // -----------------------------------------------------------------
    //
    // 记录用户在 (text, pinyin) 维度的选择次数，用于多音字主音/次音的动态权重。
    // 与 FreqModel.counts（按 text 不分音）平行存在，避免跨音串扰。
    // 例如用户在 de 查询下选「地」，只累加 (地, de) 的计数，不影响 di 查询。

    /// 加载全部拼音级频率为 HashMap<(text, pinyin), count>
    pub fn load_pinyin_counts(&self) -> rusqlite::Result<HashMap<(String, String), u32>> {
        let mut stmt = self
            .user()
            .prepare("SELECT text, pinyin, count FROM user_pinyin_counts")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2).map(|v| v.max(0) as u32)?,
            ))
        })?;
        let mut out = HashMap::new();
        for r in rows {
            let (text, pinyin, count) = r?;
            out.insert((text, pinyin), count);
        }
        Ok(out)
    }

    /// 全量替换拼音级频率表（内存优先架构下的 flush 接口）
    pub fn replace_pinyin_counts(
        &mut self,
        entries: &[(String, String, u32)],
    ) -> rusqlite::Result<usize> {
        // diff 更新（同 replace_input_mappings 注）。
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let mut existing: std::collections::HashMap<(String, String), i64> =
            std::collections::HashMap::new();
        {
            let mut stmt = self
                .user()
                .prepare("SELECT text, pinyin, count FROM user_pinyin_counts")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for r in rows {
                let (t, p, c) = r?;
                existing.insert((t, p), c);
            }
        }
        let keep: std::collections::HashSet<(&str, &str)> = entries
            .iter()
            .map(|(t, p, _)| (t.as_str(), p.as_str()))
            .collect();
        let tx = self.user_mut().transaction()?;
        let mut changed = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO user_pinyin_counts (text, pinyin, count, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(text, pinyin) DO UPDATE SET
                    count = excluded.count,
                    updated_at = excluded.updated_at",
            )?;
            for (text, pinyin, count) in entries {
                let c = *count as i64;
                if existing.get(&(text.clone(), pinyin.clone())) != Some(&c) {
                    stmt.execute(params![text, pinyin, c, now])?;
                    changed += 1;
                }
            }
        }
        {
            let mut stmt =
                tx.prepare("DELETE FROM user_pinyin_counts WHERE text = ?1 AND pinyin = ?2")?;
            for (t, p) in existing.keys() {
                if !keep.contains(&(t.as_str(), p.as_str())) {
                    stmt.execute(params![t, p])?;
                    changed += 1;
                }
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    // -----------------------------------------------------------------
    // bigram 共现统计（user_bigram_counts 表）
    // -----------------------------------------------------------------
    //
    // 记录 (prev, text) → count，用于上下文感知排序（双轨：词库种子+用户积累）。
    // 上下文栈本身不持久化（会话级），只有共现统计落盘。
    // idx_bigram_prev(prev) 索引支持按上文前缀查询。

    /// 加载全部 bigram 共现为 Vec<(prev, text, count)>
    pub fn load_bigram_counts(&self) -> rusqlite::Result<Vec<(String, String, u32)>> {
        let mut stmt = self
            .user()
            .prepare("SELECT prev, text, count FROM user_bigram_counts")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2).map(|v| v.max(0) as u32)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 加载种子 bigram（seed_bigram_counts 表，双表方案）。
    ///
    /// 与 load_bigram_counts 区别：读 seed_bigram_counts 表（语料预填，只读）。
    /// 表不存在时返回空 Vec（兼容老用户 DB 未建表场景，由 ensure_seed_bigram_table 处理）。
    pub fn load_seed_bigram_counts(&self) -> rusqlite::Result<Vec<(String, String, u32)>> {
        // 先检查表是否存在（老用户 DB 可能没有此表）
        let exists: bool = self.dict.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='seed_bigram_counts')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .dict
            .prepare("SELECT prev, text, count FROM seed_bigram_counts")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (prev, text, raw) = r?;
            out.push((prev, text, raw.max(0) as u32));
        }
        Ok(out)
    }

    /// 加载种子 bigram 为构建好的 CSR 表（优先磁盘缓存，启动优化）。
    ///
    /// 启动原路径：SQL 全表扫描（174k 行 × 2 String 分配）+ 排序构建 ≈ 67ms。
    /// 缓存路径：一次顺序读 + O(n) 校验 ≈ 5ms。缓存键 = 种子侧 db_meta 的
    /// `x1dict_sync_hash`（内容哈希覆盖 seed_bigram_counts，sync 是该表的唯一
    /// 写者且同步更新 hash）——种子表更新后旧缓存自动失效重建
    /// （自愈）。缓存文件与系统词库 DB 同目录（`seed_bigram_cache.bin`；
    /// 双库分离后跟 dict.db 走，与用户库无关）。
    ///
    /// 不缓存的情形（直接走 SQL 慢路径）：DB 无 sync_hash（开发种子 DB / 全新
    /// DB——也防止测试在 repo assets 目录写缓存文件污染工作树）、缓存读/校验
    /// 失败、缓存写失败。任何失败都静默回退，不影响正确性。
    pub(crate) fn load_seed_bigram_table_cached(
        &self,
    ) -> rusqlite::Result<crate::freq::BigramCsrTable> {
        let hash = self
            .get_seed_meta("x1dict_sync_hash")?
            .filter(|h| h.len() == 16);
        let cache_path = self
            .dict_path()
            .map(|d| d.with_file_name("seed_bigram_cache.bin"));
        if let (Some(h), Some(p)) = (hash.as_deref(), cache_path.as_ref()) {
            if let Ok(bytes) = std::fs::read(p) {
                if let Some(t) = crate::freq::BigramCsrTable::from_cache_bytes(&bytes, h) {
                    return Ok(t);
                }
            }
        }
        // 慢路径：SQL 全量 + CSR 构建；成功后尽力写缓存（失败静默，下次启动再试）
        let rows = self.load_seed_bigram_counts()?;
        let t = crate::freq::BigramCsrTable::from_entries(rows);
        if let (Some(h), Some(p)) = (hash, cache_path) {
            let tmp = p.with_extension("bin.tmp");
            if std::fs::write(&tmp, t.to_cache_bytes(&h)).is_ok() {
                // Windows 的 rename 不覆盖已存在目标：先删后改名（缓存可重建，
                // 崩在删后改名前只是下次启动重建，无正确性影响）。
                let _ = std::fs::remove_file(&p);
                let _ = std::fs::rename(&tmp, &p);
            } else {
                let _ = std::fs::remove_file(&tmp);
            }
        }
        Ok(t)
    }

    /// 确保 seed_bigram_counts 表存在（启动迁移）。
    ///
    /// 老用户 DB（此表引入前创建）没有此表，启动时建空表。
    /// 表结构为 WITHOUT ROWID 紧凑形态（数据只存主键 B 树一份，
    /// 无 updated_at / 无 PK 前缀冗余索引——旧 rowid 形态三份存储把 50 万行
    /// n-gram 撑到 37MB）。老库旧形态由 ensure_dict_db 内容哈希（含结构版本
    /// 常量）触发重铺换代；回退合并路径写入方按 table_info 双形态兼容。
    /// CREATE TABLE IF NOT EXISTS 是 O(1) 元数据操作，不影响启动秒开。
    pub fn ensure_seed_bigram_table(&self) -> rusqlite::Result<()> {
        self.dict.execute_batch(
            "CREATE TABLE IF NOT EXISTS seed_bigram_counts (
                prev  TEXT NOT NULL,
                text  TEXT NOT NULL,
                count INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY (prev, text)
            ) WITHOUT ROWID;",
        )?;
        Ok(())
    }

    /// 测试辅助：直接插入一条种子 bigram（绕过 FreqModel，用于冷启动测试 setup）。
    #[cfg(test)]
    pub fn insert_seed_bigram_for_test(
        &self,
        prev: &str,
        text: &str,
        count: u32,
    ) -> rusqlite::Result<()> {
        self.dict.execute(
            "INSERT OR REPLACE INTO seed_bigram_counts (prev, text, count) VALUES (?1, ?2, ?3)",
            rusqlite::params![prev, text, count],
        )?;
        Ok(())
    }

    /// 全量替换 bigram 共现表（内存优先架构下的 flush 接口）
    pub fn replace_bigram_counts(
        &mut self,
        entries: &[(String, String, u32)],
    ) -> rusqlite::Result<usize> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let tx = self.user_mut().transaction()?;
        tx.execute("DELETE FROM user_bigram_counts", [])?;
        let mut inserted = 0;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO user_bigram_counts (prev, text, count, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(prev, text) DO UPDATE SET
                    count = excluded.count,
                    updated_at = excluded.updated_at",
            )?;
            for (prev, text, count) in entries {
                stmt.execute(params![prev, text, *count as i64, now])?;
                inserted += 1;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// 英文单词使用计数（user_english_counts，英文补全词频动态调整）：
    /// 启动全量加载（表小——用户实际用过的英文词，量级数百行）。
    pub fn load_english_counts(&self) -> rusqlite::Result<Vec<(String, u32)>> {
        let mut out = Vec::new();
        let Ok(mut stmt) = self
            .user()
            .prepare("SELECT word, count FROM user_english_counts")
        else {
            return Ok(out);
        };
        let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u32))
        }) else {
            return Ok(out);
        };
        for r in rows.flatten() {
            out.push(r);
        }
        Ok(out)
    }

    /// 增量 upsert 英文使用计数：`count = count + delta`（追加语义，与引擎内存
    /// 计数一致地收敛；词频进化通道）。
    pub fn add_english_counts(&mut self, deltas: &[(String, u32)]) -> rusqlite::Result<usize> {
        if deltas.is_empty() {
            return Ok(0);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let tx = self.user_mut().transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO user_english_counts (word, count, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(word) DO UPDATE SET
                    count = count + excluded.count,
                    updated_at = excluded.updated_at",
            )?;
            for (word, d) in deltas {
                stmt.execute(rusqlite::params![word, *d as i64, now])?;
            }
        }
        tx.commit()?;
        Ok(deltas.len())
    }

    /// 增量 upsert bigram（避免上屏卡顿）。
    /// 背景：`replace_bigram_counts` 每次落盘 DELETE 全表 + 重插（实测用户库
    /// 7.1 万行 ≈ 200ms），而落盘挂在 Commit 请求路径上（服务单线程）——每 60s
    /// 一次的某次上屏被拖慢 200ms = 用户感知"偶尔卡一下"。
    /// 本方法只写自上次落盘以来真正变化的 (prev, text) 对（典型 60s 内数
    /// 十行 → 个位数 ms），最终状态与全量 replace 等价（内存为准 + 配对删除）。
    pub fn upsert_bigram_counts(
        &mut self,
        entries: &[(String, String, u32)],
    ) -> rusqlite::Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let tx = self.user_mut().transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO user_bigram_counts (prev, text, count, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(prev, text) DO UPDATE SET
                    count = excluded.count,
                    updated_at = excluded.updated_at",
            )?;
            for (prev, text, count) in entries {
                stmt.execute(params![prev, text, *count as i64, now])?;
            }
        }
        tx.commit()?;
        Ok(entries.len())
    }

    /// 增量删除 bigram（内存中已消失的 (prev, text) 对，forget_bigram 语义）。
    pub fn delete_bigram_counts(&mut self, keys: &[(String, String)]) -> rusqlite::Result<usize> {
        if keys.is_empty() {
            return Ok(0);
        }
        let tx = self.user_mut().transaction()?;
        {
            let mut stmt =
                tx.prepare("DELETE FROM user_bigram_counts WHERE prev = ?1 AND text = ?2")?;
            for (prev, text) in keys {
                stmt.execute(params![prev, text])?;
            }
        }
        tx.commit()?;
        Ok(keys.len())
    }

    // -----------------------------------------------------------------
    // 三阶上下文共现统计（user_trigram_counts 表）
    // -----------------------------------------------------------------
    //
    // 与 user_bigram_counts（词×词二元）解耦，记录 (prev2, prev1) → text 三元共现，
    // 供 context_bonus_with_ctx（candidate_score）/ lm_prior_score（DP）的
    // trigram 升级查询用。
    // 冷启动为空，完全靠用户积累。idx_trigram_prev(prev2, prev1) 索引支持前缀查询。

    /// 加载全部 trigram 共现为 Vec<(prev2, prev1, text, count)>
    pub fn load_trigram_counts(&self) -> rusqlite::Result<Vec<(String, String, String, u32)>> {
        let mut stmt = self
            .user()
            .prepare("SELECT prev2, prev1, text, count FROM user_trigram_counts")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3).map(|v| v.max(0) as u32)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 全量替换 trigram 共现表（内存优先架构下的 flush 接口）
    pub fn replace_trigram_counts(
        &mut self,
        entries: &[(String, String, String, u32)],
    ) -> rusqlite::Result<usize> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let tx = self.user_mut().transaction()?;
        tx.execute("DELETE FROM user_trigram_counts", [])?;
        let mut inserted = 0;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO user_trigram_counts (prev2, prev1, text, count, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(prev2, prev1, text) DO UPDATE SET
                    count = excluded.count,
                    updated_at = excluded.updated_at",
            )?;
            for (prev2, prev1, text, count) in entries {
                stmt.execute(params![prev2, prev1, text, *count as i64, now])?;
                inserted += 1;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// 增量 upsert trigram（同 upsert_bigram_counts 的理由：
    /// 用户库 6.6 万行全量重写是落盘 200ms 的另一半）。
    pub fn upsert_trigram_counts(
        &mut self,
        entries: &[(String, String, String, u32)],
    ) -> rusqlite::Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let tx = self.user_mut().transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO user_trigram_counts (prev2, prev1, text, count, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(prev2, prev1, text) DO UPDATE SET
                    count = excluded.count,
                    updated_at = excluded.updated_at",
            )?;
            for (prev2, prev1, text, count) in entries {
                stmt.execute(params![prev2, prev1, text, *count as i64, now])?;
            }
        }
        tx.commit()?;
        Ok(entries.len())
    }

    /// 增量删除 trigram（内存中已消失的三元组）。
    pub fn delete_trigram_counts(
        &mut self,
        keys: &[(String, String, String)],
    ) -> rusqlite::Result<usize> {
        if keys.is_empty() {
            return Ok(0);
        }
        let tx = self.user_mut().transaction()?;
        {
            let mut stmt = tx.prepare(
                "DELETE FROM user_trigram_counts WHERE prev2 = ?1 AND prev1 = ?2 AND text = ?3",
            )?;
            for (prev2, prev1, text) in keys {
                stmt.execute(params![prev2, prev1, text])?;
            }
        }
        tx.commit()?;
        Ok(keys.len())
    }

    // -----------------------------------------------------------------
    // 种子 trigram（seed_trigram_counts 表）
    // -----------------------------------------------------------------
    //
    // 镜像 seed_bigram_counts 的双表方案：种子 trigram 由离线导入工具预填到
    // 种子 DB，随 app bundle 流转到用户 DB。
    // 表只读——用户 commit 只写 user_trigram_counts，flush 不碰此表。
    // 查询用户优先（Engine::trigram_count_with_seed：用户内存表优先，种子 SQL 按需兜底）。

    /// 确保 seed_trigram_counts 表存在（启动迁移）。
    ///
    /// 老用户 DB（此表引入前创建）没有此表，启动时建空表。
    /// WITHOUT ROWID 紧凑形态（同 seed_bigram_counts，见其注释）。
    /// CREATE TABLE IF NOT EXISTS 是 O(1) 元数据操作，不影响启动秒开。
    pub fn ensure_seed_trigram_table(&self) -> rusqlite::Result<()> {
        self.dict.execute_batch(
            "CREATE TABLE IF NOT EXISTS seed_trigram_counts (
                prev2 TEXT NOT NULL,
                prev1 TEXT NOT NULL,
                text  TEXT NOT NULL,
                count INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY (prev2, prev1, text)
            ) WITHOUT ROWID;",
        )?;
        Ok(())
    }

    /// 按需查询单条种子 trigram 的 count（按需查询而非启动加载）。
    ///
    /// 为什么是按需而非启动加载：100k 种子 trigram 启动全量读入 BTreeMap 实测
    /// +30~60ms（拖慢启动），而 trigram 升级
    /// 只在「distance==0 且 bigram/collocation 低分」的缝隙生效（调用频率低）。
    /// 按需查询把启动成本降到 0，代价是命中路径一次 prepare_cached + 索引查找
    /// （约 2-5µs），与 `dict.collocation_freq` 的按需模式一致。
    ///
    /// 表不存在（老用户 DB）或查询失败时返回 0（种子是兜底，失败安全降级）。
    pub fn seed_trigram_count(&self, prev2: &str, prev1: &str, text: &str) -> u32 {
        self.dict
            .prepare_cached(
                "SELECT count FROM seed_trigram_counts WHERE prev2 = ?1 AND prev1 = ?2 AND text = ?3",
            )
            .and_then(|mut stmt| {
                stmt.query_row(rusqlite::params![prev2, prev1, text], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .map(|raw| raw.max(0) as u32)
            .unwrap_or(0)
    }

    /// 一次拉取 (prev2, prev1) 下整桶种子 trigram 的 (text, count)（桶级缓存用）。
    ///
    /// 为什么加桶查询：冷启动用户 trigram 表为空时，候选打分 + DP 转移 + LM
    /// 先验三处调用点每按键合计 400-600 次单条 `seed_trigram_count` SQL（每次 2-5µs，
    /// 与输入长度/候选数线性增长）。同一按键的 (prev2, prev1) 组合通常 ≤2-3 个，
    /// 一次 SQL 拉整桶（走 idx_seed_trigram_prev 索引）+ 内存 O(1) 查 count，
    /// 把每按键 SQL 次数降到 2-3 次。仍是按需语义（不启动加载）。
    ///
    /// 表不存在或查询失败返回空桶（种子是兜底，失败安全降级）。
    pub fn seed_trigram_bucket(&self, prev2: &str, prev1: &str) -> HashMap<String, u32> {
        let mut out = HashMap::new();
        let Ok(mut stmt) = self.dict.prepare_cached(
            "SELECT text, count FROM seed_trigram_counts WHERE prev2 = ?1 AND prev1 = ?2",
        ) else {
            return out;
        };
        let Ok(rows) = stmt.query_map(rusqlite::params![prev2, prev1], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        }) else {
            return out;
        };
        for r in rows.flatten() {
            out.insert(r.0, r.1.max(0) as u32);
        }
        out
    }

    /// 按需查询 (prev2, prev1) 上下文下的种子 trigram 总次数（插值 LM 的分母）。
    /// 走 idx_seed_trigram_prev 索引的 SUM 聚合，与 seed_trigram_count
    /// 同款按需策略（不启动加载）。表不存在或查询失败返回 0（失败安全降级）。
    pub fn seed_trigram_context_total(&self, prev2: &str, prev1: &str) -> u32 {
        // 明文 count：SQL SUM 聚合即可（走 idx_seed_trigram_prev 索引，
        // 空集返回 NULL → 0）。
        let total = self
            .dict
            .prepare_cached(
                "SELECT SUM(MAX(count, 0)) FROM seed_trigram_counts WHERE prev2 = ?1 AND prev1 = ?2",
            )
            .and_then(|mut stmt| {
                stmt.query_row(rusqlite::params![prev2, prev1], |row| {
                    row.get::<_, Option<i64>>(0).map(|v| v.unwrap_or(0) as u64)
                })
            })
            .unwrap_or(0);
        total.min(u32::MAX as u64) as u32
    }

    /// 测试辅助：直接插入一条种子 trigram（用于冷启动测试 setup）。
    #[cfg(test)]
    pub fn insert_seed_trigram_for_test(
        &self,
        prev2: &str,
        prev1: &str,
        text: &str,
        count: u32,
    ) -> rusqlite::Result<()> {
        self.ensure_seed_trigram_table()?;
        self.dict.execute(
            "INSERT OR REPLACE INTO seed_trigram_counts (prev2, prev1, text, count) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![prev2, prev1, text, count],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // 用户选择次数（user_counts 表）
    // -----------------------------------------------------------------

    /// 加载全部用户选择次数为 Vec<(text, count)>
    pub fn load_counts(&self) -> rusqlite::Result<Vec<(String, u32)>> {
        let mut stmt = self.user().prepare("SELECT text, count FROM user_counts")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1).map(|v| v.max(0) as u32)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (text, count) = r?;
            out.push((text, count));
        }
        Ok(out)
    }

    /// 全量替换用户选择次数表（内存优先架构下的 flush 接口）
    /// diff 更新（同 replace_input_mappings 注）。
    pub fn replace_counts(&mut self, entries: &[(String, u32)]) -> rusqlite::Result<usize> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let mut existing: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        {
            let mut stmt = self.user().prepare("SELECT text, count FROM user_counts")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            for r in rows {
                let (t, c) = r?;
                existing.insert(t, c);
            }
        }
        let keep: std::collections::HashSet<&str> =
            entries.iter().map(|(t, _)| t.as_str()).collect();
        let tx = self.user_mut().transaction()?;
        let mut changed = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO user_counts (text, count, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(text) DO UPDATE SET
                    count = excluded.count,
                    updated_at = excluded.updated_at",
            )?;
            for (text, count) in entries {
                let c = *count as i64;
                if existing.get(text) != Some(&c) {
                    stmt.execute(params![text, c, now])?;
                    changed += 1;
                }
            }
        }
        {
            let mut stmt = tx.prepare("DELETE FROM user_counts WHERE text = ?1")?;
            for t in existing.keys() {
                if !keep.contains(t.as_str()) {
                    stmt.execute(params![t])?;
                    changed += 1;
                }
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    // -----------------------------------------------------------------
    // 竞争频率（competition / competition_totals 表，「退化」通道）
    // -----------------------------------------------------------------

    /// 加载竞争分子：Vec<(layer, ctx, word, selects)>
    pub fn load_competition_selects(&self) -> rusqlite::Result<Vec<(i64, String, String, u32)>> {
        let mut stmt = self
            .user()
            .prepare("SELECT layer, ctx, word, selects FROM competition")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3).map(|v| v.max(0) as u32)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 加载竞争分母：Vec<(layer, ctx, total)>
    pub fn load_competition_totals(&self) -> rusqlite::Result<Vec<(i64, String, u32)>> {
        let mut stmt = self
            .user()
            .prepare("SELECT layer, ctx, total FROM competition_totals")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2).map(|v| v.max(0) as u32)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 全量替换 competition（内存优先；与 replace_pinyin_counts 同模式）
    /// diff 更新（同 replace_input_mappings 注）。
    pub fn replace_competition(
        &mut self,
        entries: &[(i64, String, String, u32)],
    ) -> rusqlite::Result<usize> {
        let mut existing: std::collections::HashMap<(i64, String, String), i64> =
            std::collections::HashMap::new();
        {
            let mut stmt = self
                .user()
                .prepare("SELECT layer, ctx, word, selects FROM competition")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?;
            for r in rows {
                let (l, c, w, s) = r?;
                existing.insert((l, c, w), s);
            }
        }
        let keep: std::collections::HashSet<(i64, &str, &str)> = entries
            .iter()
            .map(|(l, c, w, _)| (*l, c.as_str(), w.as_str()))
            .collect();
        let tx = self.user_mut().transaction()?;
        let mut changed = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO competition (layer, ctx, word, selects)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(layer, ctx, word) DO UPDATE SET selects = excluded.selects",
            )?;
            for (layer, ctx, word, s) in entries {
                let v = *s as i64;
                if existing.get(&(*layer, ctx.clone(), word.clone())) != Some(&v) {
                    stmt.execute(params![layer, ctx, word, v])?;
                    changed += 1;
                }
            }
        }
        {
            let mut stmt =
                tx.prepare("DELETE FROM competition WHERE layer = ?1 AND ctx = ?2 AND word = ?3")?;
            for (l, c, w) in existing.keys() {
                if !keep.contains(&(*l, c.as_str(), w.as_str())) {
                    stmt.execute(params![l, c, w])?;
                    changed += 1;
                }
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// 全量替换 competition_totals（diff 更新）
    pub fn replace_competition_totals(
        &mut self,
        entries: &[(i64, String, u32)],
    ) -> rusqlite::Result<usize> {
        let mut existing: std::collections::HashMap<(i64, String), i64> =
            std::collections::HashMap::new();
        {
            let mut stmt = self
                .user()
                .prepare("SELECT layer, ctx, total FROM competition_totals")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for r in rows {
                let (l, c, t) = r?;
                existing.insert((l, c), t);
            }
        }
        let keep: std::collections::HashSet<(i64, &str)> =
            entries.iter().map(|(l, c, _)| (*l, c.as_str())).collect();
        let tx = self.user_mut().transaction()?;
        let mut changed = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO competition_totals (layer, ctx, total)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(layer, ctx) DO UPDATE SET total = excluded.total",
            )?;
            for (layer, ctx, t) in entries {
                let v = *t as i64;
                if existing.get(&(*layer, ctx.clone())) != Some(&v) {
                    stmt.execute(params![layer, ctx, v])?;
                    changed += 1;
                }
            }
        }
        {
            let mut stmt =
                tx.prepare("DELETE FROM competition_totals WHERE layer = ?1 AND ctx = ?2")?;
            for (l, c) in existing.keys() {
                if !keep.contains(&(*l, c.as_str())) {
                    stmt.execute(params![l, c])?;
                    changed += 1;
                }
            }
        }
        tx.commit()?;
        Ok(changed)
    }
}
