//! TSV 智能列识别 + 系统词库 TSV 导入/导出。

use super::*;

impl crate::storage::Storage {
    /// 导出全部系统词为 TSV
    ///
    /// 格式：`simplified\tpinyin\tfreq\tis_primary`（4 列）
    ///
    /// is_primary 列 1=主音, 0=次音。已移除翻译字段，不再导出繁体/声调/释义。
    pub fn export_x1dict_tsv(&self) -> rusqlite::Result<String> {
        let mut stmt = self.dict.prepare(
            "SELECT simplified, pinyin, freq, is_primary
             FROM x1dict_words
             ORDER BY freq DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3).unwrap_or(1),
            ))
        })?;
        let mut lines = Vec::new();
        for r in rows {
            let (s, p, f, ip) = r?;
            lines.push(format!("{}\t{}\t{}\t{}", s, p, f, ip));
        }
        Ok(lines.join("\n"))
    }

    /// 从 TSV 导入系统词（智能列识别）
    ///
    /// 自动识别列格式，兼容新旧两种导出：
    /// - **新格式 4 列**：`simplified pinyin freq is_primary`
    /// - **旧格式 7/6 列**：`simplified traditional pinyin pinyin_tones english freq [is_primary]`
    /// - **简式 3/2 列**：`simplified pinyin [freq]`（freq 缺失按字符频率计算）
    ///
    /// 识别策略（[`detect_tsv_column_map`]）：先尝试表头行匹配（首行含列名），
    /// 失败则用数据启发式（拼音含空格+小写、freq 纯数字、繁体含 CJK 扩展等）。
    /// 旧格式的翻译三列（traditional/pinyin_tones/english）会被识别并跳过，仅取
    /// simplified/pinyin/freq/is_primary。is_primary 缺失时按字音主音推断。
    /// `replace = true` 时清空表后导入（清空与导入同一事务）。
    pub fn import_x1dict_tsv(&mut self, tsv: &str, replace: bool) -> rusqlite::Result<usize> {
        let tx = self.dict.transaction()?;
        if replace {
            // 清空必须在导入同一事务内——若在事务外先清空，导入中途失败
            // （磁盘满/IO 错误）回滚时清空已持久化，系统词库被清空且新词未导入。
            tx.execute("DELETE FROM x1dict_words", [])?;
        }
        let mut delete_stmt =
            tx.prepare("DELETE FROM x1dict_words WHERE simplified = ?1 AND pinyin = ?2")?;
        let mut insert_stmt = tx.prepare(
            "INSERT INTO x1dict_words
                (simplified, pinyin, freq, is_primary)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        let mut count = 0usize;
        // 智能列识别：解析表头或数据启发式，得到列下标映射。
        let (col_map, skip_first) = detect_tsv_column_map(tsv);
        for (i, line) in tsv.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if skip_first && i == 0 {
                continue; // 表头行已用于列识别，跳过
            }
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }
            let simplified = parts
                .get(col_map.simplified)
                .copied()
                .unwrap_or("")
                .to_string();
            let pinyin = parts.get(col_map.pinyin).copied().unwrap_or("").to_string();
            if simplified.is_empty() || pinyin.is_empty() {
                continue;
            }
            let freq = col_map
                .freq
                .and_then(|idx| parts.get(idx).and_then(|s| s.parse::<i64>().ok()))
                .filter(|v| *v > 0)
                .unwrap_or_else(|| crate::char_freq::compute_freq(&simplified) as i64);
            // is_primary：有对应列则读取，否则按字音主音推断
            let is_primary = match col_map.is_primary {
                Some(idx) => parts
                    .get(idx)
                    .and_then(|s| s.parse::<i64>().ok())
                    .map(|v| v != 0)
                    .unwrap_or(true),
                None => simplified
                    .chars()
                    .next()
                    .map(|c| crate::char_freq::is_primary_reading(c, &pinyin))
                    .unwrap_or(true),
            };
            delete_stmt.execute(params![simplified, pinyin])?;
            insert_stmt.execute(params![simplified, pinyin, freq, is_primary as i64])?;
            count += 1;
        }
        drop(delete_stmt);
        drop(insert_stmt);
        tx.commit()?;
        Ok(count)
    }
}

// -----------------------------------------------------------------------------
// TSV 智能列识别（import_x1dict_tsv 的辅助逻辑）
// -----------------------------------------------------------------------------

/// TSV 列下标映射。
///
/// `simplified` 和 `pinyin` 必有；`freq` 和 `is_primary` 可选（缺失时由
/// 字符频率计算 / 字音主音推断）。
pub(super) struct ColumnMap {
    pub(super) simplified: usize,
    pub(super) pinyin: usize,
    pub(super) freq: Option<usize>,
    pub(super) is_primary: Option<usize>,
}

/// 识别 TSV 的列归属，兼容新旧两种导出格式。
///
/// 返回 `(ColumnMap, skip_first)`：`skip_first=true` 表示首行是表头需跳过。
///
/// **识别顺序**：
/// 1. **表头匹配**：首行若含字面列名（simplified/pinyin/freq/is_primary/
///    traditional/pinyin_tones/english 等），按列名建映射并标记跳过表头。
/// 2. **数据启发式**：无表头时，取前若干行样本，对每列按特征投票：
///    - 拼音列：含空格或全小写 ASCII 拼音段（`ni hao` / `wo`）→ pinyin
///    - 数字列：全部可解析为整数 → freq / is_primary（值域 0/1 → is_primary）
///    - 其余 CJK 列取第一个 → simplified
///    - 翻译列（traditional/pinyin_tones/english）即使存在也被忽略
///
/// 启发式无法区分时回退到位置假设（第 0 列 simplified、第 1 列 pinyin）。
pub(super) fn detect_tsv_column_map(tsv: &str) -> (ColumnMap, bool) {
    let mut lines = tsv.lines().filter(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with('#')
    });
    let first = match lines.next() {
        Some(l) => l,
        None => {
            return (
                ColumnMap {
                    simplified: 0,
                    pinyin: 1,
                    freq: None,
                    is_primary: None,
                },
                false,
            )
        }
    };
    let first_parts: Vec<&str> = first.split('\t').collect();

    // 1) 表头匹配：首行含任一已知列名即视为表头
    let header_names = [
        "simplified",
        "traditional",
        "pinyin",
        "pinyin_tones",
        "english",
        "freq",
        "is_primary",
    ];
    let header_hits: usize = first_parts
        .iter()
        .filter(|p| header_names.contains(&p.trim().to_lowercase().as_str()))
        .count();
    if header_hits >= 2 {
        let mut simplified = 0usize;
        let mut pinyin = 1usize;
        let mut freq = None;
        let mut is_primary = None;
        for (i, p) in first_parts.iter().enumerate() {
            match p.trim().to_lowercase().as_str() {
                "simplified" => simplified = i,
                "pinyin" => pinyin = i,
                "freq" => freq = Some(i),
                "is_primary" => is_primary = Some(i),
                _ => {} // traditional/pinyin_tones/english 忽略
            }
        }
        return (
            ColumnMap {
                simplified,
                pinyin,
                freq,
                is_primary,
            },
            true,
        );
    }

    // 2) 数据启发式：取首行样本逐列分类
    let ncols = first_parts.len();
    if ncols < 2 {
        return (
            ColumnMap {
                simplified: 0,
                pinyin: 1,
                freq: None,
                is_primary: None,
            },
            false,
        );
    }

    // 找拼音列：含空格，或纯小写 ASCII（单音节如 "wo"）。优先选含空格的列，
    // 否则选第一个纯小写 ASCII 列。注意要避开 english（含字母但通常多词无 CJK）。
    let is_pinyin_like = |s: &str| -> bool {
        let s = s.trim();
        if s.is_empty() {
            return false;
        }
        // 含空格且每段都是小写 ASCII → 多音节拼音（ni hao）
        if s.contains(' ') {
            return s
                .split_whitespace()
                .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_lowercase()));
        }
        // 单段小写 ASCII → 单音节拼音（wo），但也可能是 english 单词。
        // 启发式：长度 ≤6 视为拼音音节，更长视为 english。
        s.len() <= 6 && s.chars().all(|c| c.is_ascii_lowercase())
    };
    let is_int = |s: &str| s.trim().parse::<i64>().is_ok();

    let mut pinyin_col = None;
    for (i, p) in first_parts.iter().enumerate() {
        if is_pinyin_like(p) {
            pinyin_col = Some(i);
            break;
        }
    }
    let pinyin_col = pinyin_col.unwrap_or(1);

    // 数字列：扫所有候选列，区分 freq（大数值）和 is_primary（仅 0/1）
    let mut freq_col = None;
    let mut is_primary_col = None;
    for (i, p) in first_parts.iter().enumerate() {
        if i == pinyin_col || !is_int(p) {
            continue;
        }
        let v: i64 = p.trim().parse().unwrap();
        if v == 0 || v == 1 {
            // 值域 0/1 → is_primary（除非另有更大的数字列，否则也可能是 freq）
            if is_primary_col.is_none() {
                is_primary_col = Some(i);
            }
        } else {
            freq_col = Some(i);
        }
    }
    // 若只找到一个数字列且值域 0/1，无法确定是 freq 还是 is_primary：
    // 保守视为 freq（更常见），is_primary 留空由字音推断
    if freq_col.is_none() && is_primary_col.is_some() && ncols <= 4 {
        freq_col = is_primary_col;
        is_primary_col = None;
    }

    // simplified：第一个非拼音、非数字的 CJK 列
    let mut simplified_col = 0usize;
    for (i, p) in first_parts.iter().enumerate() {
        if i == pinyin_col || i == freq_col.unwrap_or(usize::MAX) {
            continue;
        }
        if p.trim().chars().any(|c| c as u32 >= 0x4E00) {
            // CJK 统一表意文字范围
            simplified_col = i;
            break;
        }
    }

    (
        ColumnMap {
            simplified: simplified_col,
            pinyin: pinyin_col,
            freq: freq_col,
            is_primary: is_primary_col,
        },
        false,
    )
}
