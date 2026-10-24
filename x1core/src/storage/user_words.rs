//! user_words 表 CRUD + 用户黑名单 user_blacklist：用户自造词与删除词管理。

use super::*;

impl crate::storage::Storage {
    // -----------------------------------------------------------------
    // 用户词频：延迟写入
    // -----------------------------------------------------------------

    /// 加载全部用户词频为 HashMap<text, user_freq>
    ///
    /// 供 `FreqModel::load_from_db()` 启动时恢复内存频率。
    /// 多读音下按 text 取 MAX：引擎快照 flush 把 text 级词频写到该词
    /// **每个**读音行上（行值相同 → MAX = 精确值，与行数无关）；导入增量（delta）
    /// 只落在匹配 (text, pinyin) 的行上 → MAX 自然反映最常用读音的累计。
    /// （不能用 SUM：快照语义下行值相同，SUM 会按读音数翻倍。）
    pub fn load_user_freq(&self) -> rusqlite::Result<HashMap<String, u32>> {
        let mut stmt = self
            .user()
            .prepare("SELECT text, MAX(user_freq) FROM user_words GROUP BY text")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1).map(|v| v.max(0) as u32)?,
            ))
        })?;
        let mut out = HashMap::new();
        for r in rows {
            let (text, freq) = r?;
            out.insert(text, freq);
        }
        Ok(out)
    }

    /// 加载全部用户词（含拼音和词频）
    ///
    /// 供 `Engine::new_with_db()` 启动时把用户自造词回填到内存 Trie。
    pub fn load_user_words(&self) -> rusqlite::Result<Vec<(String, String, u32)>> {
        let mut stmt = self
            .user()
            .prepare("SELECT text, pinyin, user_freq FROM user_words")?;
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

    /// 批量 upsert 用户词频（延迟写入 flush 调用）
    ///
    /// 对每条记录：若 text 已存在则累加 delta，否则插入新行。
    pub fn upsert_user_freq_batch(
        &mut self,
        entries: &[(String, String, u32)],
    ) -> rusqlite::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let tx = self.user_mut().transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO user_words (text, pinyin, user_freq, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(text, pinyin) DO UPDATE SET
                    user_freq = user_freq + ?3,
                    updated_at = ?4",
            )?;
            for (text, pinyin, delta) in entries {
                stmt.execute(params![text, pinyin, *delta as i64, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // 用户词库管理（供词库管理工具调用）
    // -----------------------------------------------------------------

    /// 删除一条用户词（按 text 精确匹配）
    pub fn delete_user_word(&mut self, text: &str) -> rusqlite::Result<usize> {
        self.user_mut()
            .execute("DELETE FROM user_words WHERE text = ?1", params![text])
    }

    /// 更新用户词的拼音和频率
    ///
    /// 若该词不存在则不做任何操作；调用方应先确认存在。
    /// 多读音语义：显式编辑 = 替换全部读音——先删其他读音行再 UPDATE，
    /// 防止多行收敛到同一 (text, pinyin) 触发 UNIQUE 冲突。
    pub fn update_user_word(
        &mut self,
        text: &str,
        pinyin: &str,
        freq: u32,
    ) -> rusqlite::Result<usize> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        // 事务包裹：先删其他读音再 UPDATE 两步原子——否则崩溃在两条语句之间
        // = 该词全部读音行被删而新值未写入（用户 DB 数据不可丢）。
        let tx = self.user_mut().transaction()?;
        tx.execute(
            "DELETE FROM user_words WHERE text = ?1 AND pinyin <> ?2",
            params![text, pinyin],
        )?;
        let n = tx.execute(
            "UPDATE user_words SET pinyin = ?2, user_freq = ?3, updated_at = ?4 WHERE text = ?1",
            params![text, pinyin, freq as i64, now],
        )?;
        tx.commit()?;
        Ok(n)
    }

    /// 清空全部用户词（重置学习数据）
    pub fn clear_user_words(&mut self) -> rusqlite::Result<usize> {
        self.user_mut().execute("DELETE FROM user_words", [])
    }

    /// 清空运行时统计三表（种子 DB 清洗用）。
    ///
    /// 种子 DB 是干净基线，这三张表是**运行时用户统计量**（输入习惯映射、多音字
    /// 权重、bigram 共现），不应出现在种子 DB 里；残留会污染首装体验 + 拖慢
    /// 候选排序（candidate_score 的 prefix mapping 扫描遍历整个 input_mappings）。
    ///
    /// 返回三表各自清理的条数 `(input_mappings, pinyin_counts, bigram)`。
    pub fn clear_runtime_stat_tables(&mut self) -> rusqlite::Result<(usize, usize, usize)> {
        let im = self
            .user_mut()
            .execute("DELETE FROM user_input_mappings", [])?;
        let pc = self
            .user_mut()
            .execute("DELETE FROM user_pinyin_counts", [])?;
        let bg = self
            .user_mut()
            .execute("DELETE FROM user_bigram_counts", [])?;
        Ok((im, pc, bg))
    }

    /// 导入用户词。
    ///
    /// - `merge = true`：与现有词库合并，冲突时累加频率；
    /// - `merge = false`：先清空再导入。
    pub fn import_user_words(
        &mut self,
        entries: &[(String, String, u32)],
        merge: bool,
    ) -> rusqlite::Result<usize> {
        if !merge {
            self.clear_user_words()?;
        }
        self.upsert_user_freq_batch(entries).map(|_| entries.len())
    }

    /// 全量替换 user_words 表。
    ///
    /// 内存优先架构下的落盘接口：调用方在内存中维护完整用户词状态，
    /// 只在定时 flush / 退出时把当前全量快照写回 SQLite。
    /// 实现为先 DELETE 全表，再批量 INSERT，保证幂等且不会重复累加。
    pub fn replace_user_words(
        &mut self,
        entries: &[(String, String, u32)],
    ) -> rusqlite::Result<usize> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let tx = self.user_mut().transaction()?;
        tx.execute("DELETE FROM user_words", [])?;
        let mut inserted = 0;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO user_words (text, pinyin, user_freq, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(text, pinyin) DO UPDATE SET
                    pinyin = ?2,
                    user_freq = ?3,
                    updated_at = ?4",
            )?;
            for (text, pinyin, freq) in entries {
                stmt.execute(params![text, pinyin, *freq as i64, now])?;
                inserted += 1;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// 查询 DB 完整性（测试用）。
    #[cfg(test)]
    pub fn integrity_check(&self) -> String {
        self.user()
            .query_row("PRAGMA integrity_check;", [], |r| r.get(0))
            .unwrap_or_default()
    }

    /// 查询 user_words 表中指定 text 的数量（测试用）。
    #[cfg(test)]
    pub fn user_words_count_for_text(&self, text: &str) -> i64 {
        self.user()
            .query_row(
                "SELECT count(*) FROM user_words WHERE text=?1",
                rusqlite::params![text],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    /// 批量 upsert 用户词（绝对值替换）。
    ///
    /// 与旧 `upsert_user_freq_batch`（累加 delta）不同，这里直接设置
    /// `pinyin` 和 `user_freq` 为传入值，适用于增量 flush。
    pub fn upsert_user_words_batch(
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
        let mut inserted = 0;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO user_words (text, pinyin, user_freq, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(text, pinyin) DO UPDATE SET
                    pinyin = excluded.pinyin,
                    user_freq = excluded.user_freq,
                    updated_at = excluded.updated_at",
            )?;
            for (text, pinyin, freq) in entries {
                stmt.execute(params![text, pinyin, *freq as i64, now])?;
                inserted += 1;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// 批量删除用户词。
    pub fn delete_user_words_batch(&mut self, texts: &[String]) -> rusqlite::Result<usize> {
        if texts.is_empty() {
            return Ok(0);
        }
        let tx = self.user_mut().transaction()?;
        let mut deleted = 0;
        {
            let mut stmt = tx.prepare("DELETE FROM user_words WHERE text = ?1")?;
            for text in texts {
                deleted += stmt.execute(params![text])?;
            }
        }
        tx.commit()?;
        Ok(deleted)
    }

    /// 删除某词**除保留读音外**的全部读音行（多读音合并补漏）。
    ///
    /// 语义 = `update_user_word` 的 DELETE 部分（`text = ? AND pinyin <> ?`），
    /// 但保留读音是**列表**：flush 时以内存 vec 为准（显式编辑替换为单读音、
    /// 学习追加为多读音，两种场景都正确——凡不在内存里的行都是残留）。
    /// 调用点：Engine::flush 消费 pending_reading_prune（词库管理器编辑拼音后
    /// 旧读音行残留 → 同词两条、候选冒出旧读音的根因）。
    pub fn prune_user_word_readings_except(
        &mut self,
        text: &str,
        keep: &[String],
    ) -> rusqlite::Result<usize> {
        if keep.is_empty() {
            return Ok(0);
        }
        let placeholders = vec!["?"; keep.len()].join(",");
        let sql = format!(
            "DELETE FROM user_words WHERE text = ?1 AND pinyin NOT IN ({})",
            placeholders
        );
        let mut stmt = self.user_mut().prepare(&sql)?;
        let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(keep.len() + 1);
        params_vec.push(&text);
        for k in keep {
            params_vec.push(k);
        }
        let n = stmt.execute(rusqlite::params_from_iter(params_vec))?;
        Ok(n)
    }
    /// 清理用户词库中的单字
    pub fn cleanup_single_char_user_words(&mut self) -> rusqlite::Result<usize> {
        let n = self
            .user_mut()
            .execute("DELETE FROM user_words WHERE length(text) = 1", [])?;
        Ok(n)
    }

    /// 为 `user_words` 中拼音为空的条目回填拼音。
    ///
    /// 旧版本只记录频率、没有保存系统多字词拼音，导致这些用户词无法被
    /// 拼音查询命中。本迁移从 `x1dict_words` 中查找同 `simplified` 词条，
    /// 将其拼音写回 `user_words`。
    ///
    /// 仅处理多字词；单字已由 `cleanup_single_char_user_words` 清理。
    /// 返回成功回填的条数。
    pub fn backfill_user_pinyin_from_x1dict(&mut self) -> rusqlite::Result<usize> {
        // 跨库操作（双库分离后 x1dict_words 在 dict.db、user_words 在 user.db）：
        // 合并模式两表同库，直接 UPDATE…SELECT；分离模式临时 ATTACH dict.db 只读。
        let split_dict = if self.is_split() {
            let Some(p) = self.dict_path().map(|p| p.to_path_buf()) else {
                return Ok(0);
            };
            Some(p)
        } else {
            None
        };
        let (dict_ref, attached) = match &split_dict {
            Some(p) => {
                self.user_mut().execute(
                    "ATTACH DATABASE ?1 AS backfill_dict",
                    rusqlite::params![p.to_string_lossy()],
                )?;
                ("backfill_dict.x1dict_words", true)
            }
            None => ("x1dict_words", false),
        };
        let sql = format!(
            "UPDATE user_words
             SET pinyin = (
                 SELECT {dict_ref}.pinyin
                 FROM {dict_ref}
                 WHERE {dict_ref}.simplified = user_words.text
                 LIMIT 1
             )
             WHERE pinyin = ''
               AND length(text) > 1
               AND EXISTS (
                   SELECT 1 FROM {dict_ref}
                   WHERE {dict_ref}.simplified = user_words.text
               )"
        );
        let n = self.user_mut().execute(&sql, [])?;
        if attached {
            let _ = self.user_mut().execute("DETACH DATABASE backfill_dict", []);
        }
        Ok(n)
    }

    /// 查询单个词的用户频率（调试 / 测试用）
    pub fn get_user_freq(&self, text: &str) -> rusqlite::Result<Option<u32>> {
        // MAX 在零行时返回一行 NULL（不是 QueryReturnedNoRows），必须按 Option 列取
        let freq: Option<i64> = self
            .user()
            .query_row(
                "SELECT MAX(user_freq) FROM user_words WHERE text = ?1",
                params![text],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        Ok(freq.map(|v| v.max(0) as u32))
    }

    // -------------------------------------------------------------------------
    // 用户持久化黑名单（user_blacklist）
    // -------------------------------------------------------------------------
    //
    // Command+Backspace 删除的词写入此表，跨会话/跨部署保留。
    // 部署链合并用户数据时跳过黑名单词，铺种子时清理同名词，
    // 避免「删除的词每次部署又回来」。

    /// 加载全部黑名单词文本（启动时调用，填充 Engine.removed_texts）。
    pub fn load_blacklist(&self) -> rusqlite::Result<HashSet<String>> {
        let mut stmt = self.user().prepare("SELECT text FROM user_blacklist")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = HashSet::new();
        for r in rows {
            out.insert(r?);
        }
        Ok(out)
    }

    /// 新增一条黑名单（用户删除某词时调用）。
    /// 幂等：已存在则更新 updated_at。
    pub fn insert_blacklist(&mut self, text: &str) -> rusqlite::Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        self.user_mut().execute(
            "INSERT INTO user_blacklist (text, updated_at) VALUES (?1, ?2)
             ON CONFLICT(text) DO UPDATE SET updated_at = excluded.updated_at",
            params![text, now],
        )?;
        Ok(())
    }

    /// 移除一条黑名单（用户重新选中该词时调用，复活机制）。
    pub fn remove_blacklist(&mut self, text: &str) -> rusqlite::Result<()> {
        self.user_mut()
            .execute("DELETE FROM user_blacklist WHERE text = ?1", params![text])?;
        Ok(())
    }

    // -------------------------------------------------------------------------
    // 固定候选（user_pins）
    // -------------------------------------------------------------------------
    //
    // 用户把某输入串的字/词钉死在 1-3 号候选位（Ctrl+数字），免疫一切排序学习。
    // 键 = 原始输入串（green_memo 同口径：全拼 wo / 简拼 w / 双拼原文各是各的
    // 键，互不触发）。小表（个位数到几十行），启动全量加载进内存。

    /// 加载全部固定项（Engine::new_with_db 启动调用）。返回 (input, slot, text)。
    pub fn load_pins(&self) -> rusqlite::Result<Vec<(String, u8, String)>> {
        let mut stmt = self
            .user()
            .prepare("SELECT input, slot, text FROM user_pins ORDER BY input, slot")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as u8,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 写入/覆盖一个固定位。同事务内先删同输入下同文本的其他槽（移动语义），
    /// 再 INSERT OR REPLACE 目标槽（覆盖语义）——与引擎内存态操作严格同序。
    pub fn upsert_pin(&mut self, input: &str, slot: u8, text: &str) -> rusqlite::Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let tx = self.user_mut().transaction()?;
        tx.execute(
            "DELETE FROM user_pins WHERE input = ?1 AND text = ?2",
            params![input, text],
        )?;
        tx.execute(
            "INSERT INTO user_pins (input, slot, text, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(input, slot) DO UPDATE SET text = excluded.text, updated_at = excluded.updated_at",
            params![input, slot as i64, text, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 删除一个固定位（用户在同槽位再按一次 Ctrl+N 解除，或管理界面删除）。
    pub fn delete_pin(&mut self, input: &str, slot: u8) -> rusqlite::Result<()> {
        self.user_mut().execute(
            "DELETE FROM user_pins WHERE input = ?1 AND slot = ?2",
            params![input, slot as i64],
        )?;
        Ok(())
    }
}
