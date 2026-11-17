//! user_phrases 表 CRUD + 自定义短语导入导出。
//!
//! 自定义短语：触发码（任意按键序列，如 noip）→ 短语文本（中文/英文/特殊
//! 符号均可）。**无词频**、**不依赖拼音切分**（触发码按原始按键匹配），一码
//! 可对应多条短语（按添加顺序）。引擎 process 命中即置顶注入；上屏不参与
//! 任何学习（独立通道，见 engine commit_internal 的短语守卫）。
//!
//! 导入导出格式（纯文本，UTF-8，每行一条，**短语在前、触发码在后**）：
//!   `信息学省级联赛 noip`  或  `信息学省级联赛,noip`
//! 解析取**最后一个**分隔符（逗号优先，否则空白）切分——短语本身含空格/
//! 逗号不影响（触发码恒为末段单词）。`#` 开头行与空行忽略。

use super::*;

/// 触发码归一（存储与匹配同口径）：去首尾空白 + 小写 + 去隔音符号 ' 与空格。
/// 触发码是按键序列而非拼音，全拼/双拼都按实际敲键匹配。
pub fn normalize_phrase_key(s: &str) -> String {
    s.trim().to_lowercase().replace(['\'', ' '], "")
}

/// 解析一行导入文本 → (短语文本, 触发码)。短语在前、触发码在后；取最后一个
/// 分隔符（有逗号用最后一个逗号，否则用最后一个空白）切分。非法行返回 None。
pub fn parse_phrase_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    // 逗号优先（CSV 形态）；否则空白（纯文本形态）。取最后一个分隔符，
    // 保证短语内部的分隔符不被误切（触发码恒为末段）。
    let pos = line
        .rfind(',')
        .or_else(|| line.rfind(|c: char| c.is_whitespace()))?; // 无分隔符 → None
    let (text, trigger) = (&line[..pos], &line[pos + 1..]);
    let text = text.trim();
    let trigger = normalize_phrase_key(trigger);
    if text.is_empty() || trigger.is_empty() || trigger.chars().any(|c| c.is_whitespace()) {
        return None;
    }
    Some((text.to_string(), trigger))
}

/// 导出一行（逗号分隔，短语在前）：`短语,触发码`。
pub fn format_phrase_line(text: &str, trigger: &str) -> String {
    format!("{},{}", text, trigger)
}

impl crate::storage::Storage {
    /// 加载全部短语（按 id 升序 = 添加顺序；一码多条保持先后）。
    pub fn load_phrases(&self) -> rusqlite::Result<Vec<(i64, String, String)>> {
        let mut stmt = self
            .user()
            .prepare("SELECT id, trigger, text FROM user_phrases ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 新增一条短语（触发码归一存储），返回新行 id。
    pub fn insert_phrase(&mut self, trigger: &str, text: &str) -> rusqlite::Result<i64> {
        let key = normalize_phrase_key(trigger);
        self.user_mut().execute(
            "INSERT INTO user_phrases (trigger, text) VALUES (?1, ?2)",
            params![key, text],
        )?;
        Ok(self.user().last_insert_rowid())
    }

    /// 按 id 更新一条短语（触发码归一）。
    pub fn update_phrase(&mut self, id: i64, trigger: &str, text: &str) -> rusqlite::Result<()> {
        let key = normalize_phrase_key(trigger);
        self.user_mut().execute(
            "UPDATE user_phrases SET trigger = ?1, text = ?2 WHERE id = ?3",
            params![key, text, id],
        )?;
        Ok(())
    }

    /// 按 id 删除一条短语。
    pub fn delete_phrase(&mut self, id: i64) -> rusqlite::Result<()> {
        self.user_mut()
            .execute("DELETE FROM user_phrases WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// 批量删除（管理界面多选）。
    pub fn delete_phrases(&mut self, ids: &[i64]) -> rusqlite::Result<()> {
        let tx = self.user_mut().transaction()?;
        for id in ids {
            tx.execute("DELETE FROM user_phrases WHERE id = ?1", params![id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 清空全部短语（导入「替换」模式用）。
    pub fn clear_phrases(&mut self) -> rusqlite::Result<()> {
        self.user_mut().execute("DELETE FROM user_phrases", [])?;
        Ok(())
    }

    /// 短语总数（词库页规模展示）。
    pub fn phrase_count(&self) -> rusqlite::Result<usize> {
        let n: i64 = self
            .user()
            .query_row("SELECT COUNT(*) FROM user_phrases", [], |r| r.get(0))?;
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_phrase_line_space() {
        assert_eq!(
            parse_phrase_line("信息学省级联赛 noip"),
            Some(("信息学省级联赛".into(), "noip".into()))
        );
        // 短语含空格（英文短语）：取最后一个空白切分
        assert_eq!(
            parse_phrase_line("hello world hw"),
            Some(("hello world".into(), "hw".into()))
        );
    }

    #[test]
    fn test_parse_phrase_line_comma() {
        assert_eq!(
            parse_phrase_line("信息学省级联赛,noip"),
            Some(("信息学省级联赛".into(), "noip".into()))
        );
        // 短语含逗号：取最后一个逗号切分
        assert_eq!(
            parse_phrase_line("a,b,c,code"),
            Some(("a,b,c".into(), "code".into()))
        );
        // 特殊符号短语
        assert_eq!(
            parse_phrase_line("①②③★ xuhao"),
            Some(("①②③★".into(), "xuhao".into()))
        );
    }

    #[test]
    fn test_parse_phrase_line_invalid() {
        assert_eq!(parse_phrase_line(""), None);
        assert_eq!(parse_phrase_line("# 注释"), None);
        assert_eq!(parse_phrase_line("无分隔符"), None);
        assert_eq!(parse_phrase_line("短语 "), None); // 触发码空
        assert_eq!(parse_phrase_line(" noip"), None); // 短语空
    }

    #[test]
    fn test_normalize_and_format() {
        assert_eq!(normalize_phrase_key(" NO'IP "), "noip");
        assert_eq!(
            format_phrase_line("信息学省级联赛", "noip"),
            "信息学省级联赛,noip"
        );
        // 导出→解析 round-trip
        let line = format_phrase_line("hello world", "hw");
        assert_eq!(
            parse_phrase_line(&line),
            Some(("hello world".into(), "hw".into()))
        );
    }
}
