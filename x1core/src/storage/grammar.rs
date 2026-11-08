//! 中文词性/语法资产（种子侧表）。
//!
//! 三张表从 PFR 2014 语料挖掘，随种子分发：
//! - `x1dict_pos`：词 → 词性位图 + 主词性 + 分布；
//! - `x1pos_trans`：粗类词性转移计数（将来语法模型用）；
//! - `x1func_ctx`：的/地/得 上下文计数（函数词消歧的第一消费面）。
//!
//! 与 `x1dict_words` 解耦：不改变 arena/词典编译缓存布局；用户库同步走
//! `sync_x1dict_from_builtin`（只读种子表覆盖，幂等）。

use super::Storage;

impl Storage {
    /// 确保词性侧表存在（老用户库/老种子库升级用）。
    pub(crate) fn ensure_grammar_tables(&self) -> rusqlite::Result<()> {
        self.dict.execute_batch(
            "CREATE TABLE IF NOT EXISTS x1dict_pos (
                 word         TEXT PRIMARY KEY,
                 pos_mask     INTEGER NOT NULL,
                 pos_dominant TEXT NOT NULL DEFAULT '',
                 pos_counts   TEXT NOT NULL DEFAULT '',
                 source       TEXT NOT NULL DEFAULT 'pfr2014'
             );
             CREATE TABLE IF NOT EXISTS x1pos_seq (
                 seq   TEXT NOT NULL PRIMARY KEY,
                 count INTEGER NOT NULL
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS x1grammar_patterns (
                 seq    TEXT NOT NULL PRIMARY KEY,
                 len    INTEGER NOT NULL,
                 count  INTEGER NOT NULL,
                 source TEXT NOT NULL DEFAULT 'pfr2014'
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS x1pos_trans (
                 prev_pos TEXT NOT NULL,
                 next_pos TEXT NOT NULL,
                 count    INTEGER NOT NULL,
                 PRIMARY KEY (prev_pos, next_pos)
             );
             CREATE TABLE IF NOT EXISTS x1func_ctx (
                 func     TEXT NOT NULL,
                 prev_pos TEXT NOT NULL,
                 next_pos TEXT NOT NULL,
                 count    INTEGER NOT NULL,
                 PRIMARY KEY (func, prev_pos, next_pos)
             );
             CREATE TABLE IF NOT EXISTS x1func_word_ctx (
                 func  TEXT NOT NULL,
                 grp   TEXT NOT NULL,
                 prev  TEXT NOT NULL,
                 next  TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 PRIMARY KEY (func, grp, prev, next)
             );
             CREATE TABLE IF NOT EXISTS x1func_word_global (
                 func  TEXT PRIMARY KEY,
                 grp   TEXT NOT NULL,
                 total INTEGER NOT NULL
             );",
        )
    }

    /// 词性表行数（缺表 = 0）。调用方据此一次性预留紧凑表容量，避免增长
    /// 过程中产生可回收但 malloc 仍占用的旧缓冲。
    pub(crate) fn pos_entry_count(&self) -> rusqlite::Result<usize> {
        let exists: bool = self.dict.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='x1dict_pos')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(0);
        }
        let n: i64 = self
            .dict
            .query_row("SELECT COUNT(*) FROM x1dict_pos", [], |row| row.get(0))?;
        Ok(n.max(0) as usize)
    }

    /// 流式遍历词性表：`(word, pos_mask, pos_dominant)`，按 `word` 升序。
    ///
    /// 不物化中间 `Vec<(String, ...)>`（81k 行会制造 16 万个短字符串，释放后
    /// 仍被 malloc 攥成空闲页）；调用方直接构建紧凑表。
    /// 刻意不读 `pos_counts`（81k 行的长字符串，启动解码/分配大头；
    /// 当前运行时只用 dominant 粗类，分布留作将来语法模型从 DB 重读）。
    pub(crate) fn for_each_pos_entry<F>(&self, mut f: F) -> rusqlite::Result<()>
    where
        F: FnMut(&str, u16, &str),
    {
        let exists: bool = self.dict.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='x1dict_pos')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(());
        }
        let mut stmt = self
            .dict
            .prepare("SELECT word, pos_mask, pos_dominant FROM x1dict_pos ORDER BY word")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let word = row.get_ref(0)?.as_str()?;
            let mask = row.get::<_, i64>(1)?.clamp(0, u16::MAX as i64) as u16;
            let dominant = row.get_ref(2)?.as_str()?;
            f(word, mask, dominant);
        }
        Ok(())
    }

    /// 粗类多词性词的细分布：只取 `pos_mask` 多位的词（粗类真正多类，
    /// 当前约 55 个），避免全量 81k `pos_counts` 长字符串进启动内存。
    pub(crate) fn load_pos_multi_counts(&self) -> rusqlite::Result<Vec<(String, String)>> {
        let exists: bool = self.dict.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='x1dict_pos')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
        let mut stmt = self.dict.prepare(
            "SELECT word, pos_counts FROM x1dict_pos
             WHERE (pos_mask & (pos_mask - 1)) <> 0 ORDER BY word",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 整句语法模板：`(seq, len, count)`，seq 形如 `"r v n"`（2–6 词）。
    pub(crate) fn load_grammar_patterns(&self) -> rusqlite::Result<Vec<(String, usize, u32)>> {
        let exists: bool = self.dict.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='x1grammar_patterns')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .dict
            .prepare("SELECT seq, len, count FROM x1grammar_patterns ORDER BY seq")?;
        let rows = stmt.query_map([], |row| {
            let len: i64 = row.get(1)?;
            let c: i64 = row.get(2)?;
            Ok((
                row.get::<_, String>(0)?,
                len.clamp(0, i32::MAX as i64) as usize,
                c.clamp(0, u32::MAX as i64) as u32,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 粗类三元词性组合：`(seq, count)`，seq 形如 `"r v n"`。
    pub(crate) fn load_pos_seq(&self) -> rusqlite::Result<Vec<(String, u32)>> {
        let exists: bool = self.dict.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='x1pos_seq')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .dict
            .prepare("SELECT seq, count FROM x1pos_seq ORDER BY seq")?;
        let rows = stmt.query_map([], |row| {
            let c: i64 = row.get(1)?;
            Ok((row.get::<_, String>(0)?, c.clamp(0, u32::MAX as i64) as u32))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 粗类词性转移：`(prev_pos, next_pos, count)`。
    pub(crate) fn load_pos_trans(&self) -> rusqlite::Result<Vec<(String, String, u32)>> {
        let exists: bool = self.dict.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='x1pos_trans')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
        let mut stmt = self.dict.prepare(
            "SELECT prev_pos, next_pos, count FROM x1pos_trans ORDER BY prev_pos, next_pos",
        )?;
        let rows = stmt.query_map([], |row| {
            let c: i64 = row.get(2)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                c.clamp(0, u32::MAX as i64) as u32,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 函数词上下文：`(func, prev_pos, next_pos, count)`。
    pub(crate) fn load_func_ctx(&self) -> rusqlite::Result<Vec<(String, String, String, u32)>> {
        let exists: bool = self.dict.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='x1func_ctx')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
        let mut stmt = self.dict.prepare(
            "SELECT func, prev_pos, next_pos, count FROM x1func_ctx
             ORDER BY func, prev_pos, next_pos",
        )?;
        let rows = stmt.query_map([], |row| {
            let c: i64 = row.get(3)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                c.clamp(0, u32::MAX as i64) as u32,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 同音歧义词级上下文：`(func, grp, prev, next, count)`。
    pub(crate) fn load_func_word_ctx(
        &self,
    ) -> rusqlite::Result<Vec<(String, String, String, String, u32)>> {
        let exists: bool = self.dict.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='x1func_word_ctx')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
        let mut stmt = self.dict.prepare(
            "SELECT func, grp, prev, next, count FROM x1func_word_ctx
             ORDER BY func, grp, prev, next",
        )?;
        let rows = stmt.query_map([], |row| {
            let c: i64 = row.get(4)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                c.clamp(0, u32::MAX as i64) as u32,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 同音歧义词全局计数：`(func, grp, total)`。
    pub(crate) fn load_func_word_global(&self) -> rusqlite::Result<Vec<(String, String, u32)>> {
        let exists: bool = self.dict.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='x1func_word_global')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .dict
            .prepare("SELECT func, grp, total FROM x1func_word_global ORDER BY func")?;
        let rows = stmt.query_map([], |row| {
            let c: i64 = row.get(2)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                c.clamp(0, u32::MAX as i64) as u32,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 口语搭配全量：`(prev, curr, count)`；表缺失返回空。
    pub(crate) fn load_colloc(&self) -> rusqlite::Result<Vec<(String, String, u32)>> {
        let exists: bool = self.dict.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='x1colloc')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .dict
            .prepare("SELECT prev, curr, count FROM x1colloc ORDER BY prev, curr")?;
        let rows = stmt.query_map([], |row| {
            let c: i64 = row.get(2)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                c.clamp(0, u32::MAX as i64) as u32,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 清空词性侧表（种子重建脚本用；不碰用户表）。
    #[allow(dead_code)]
    pub(crate) fn clear_grammar_tables(&self) -> rusqlite::Result<()> {
        self.ensure_grammar_tables()?;
        self.dict.execute_batch(
            "DELETE FROM x1dict_pos;
             DELETE FROM x1grammar_patterns;
             DELETE FROM x1pos_seq;
             DELETE FROM x1pos_trans;
             DELETE FROM x1func_ctx;
             DELETE FROM x1func_word_ctx;
             DELETE FROM x1func_word_global;",
        )?;
        Ok(())
    }
}
