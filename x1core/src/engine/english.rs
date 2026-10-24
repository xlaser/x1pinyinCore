//! 智能单词识别·英文词表查询。
//!
//! 架构：英文词表存放于 dict.db 独立表 `english_words`
//!（word/pos/cn/freq，不与任何中文表混用），经种子分发（哈希变更触发
//! `ensure_dict_db` 重铺）；引擎不编译内嵌——为后续翻译/词典等英文
//! 功能留数据生长空间（pos/cn 列即翻译储备）。
//!
//! 查询全部经 Storage 参数绑定 + prepare_cached（按键热路径要求）；
//! 无 DB（`Engine::new` 内置模式）返回空——英文功能依赖种子铺开的 dict.db。
//!
//! 两个消费面：
//! - 中文模式：拼音输入命中词表 → 英文单词候选（`inject_english_candidates`）
//! - 英文模式：英文输入前缀 → 补全（`english_complete`，服务 EnglishComplete）

use super::Engine;
use alloc::string::String;
use alloc::vec::Vec;

/// 词形硬约束：ASCII 字母 2..=24 字符，允许混合大小写。
///
/// 存储层 word 列是规范展示词形（IBM / iPhone / Microsoft）；查询键一律
/// 归一为小写（`normalize`），因此大小写只影响展示，不影响输入匹配。
pub(crate) fn is_word_shape(s: &str) -> bool {
    s.len() >= 2 && s.len() <= 24 && s.bytes().all(|b| b.is_ascii_alphabetic())
}

/// 输入归一化：去掉拼音分隔符 `'` → 转小写（大小写不敏感匹配；用户打字不会
/// 刻意区分大小写）→ 通过词形判定才返回 Some。
/// 单字母输入（a/e/o/m 等合法拼音）不查英文表。
fn normalize(input: &str) -> Option<String> {
    if input.is_empty() {
        return None;
    }
    let compact: String = input.chars().filter(|c| *c != '\'').collect();
    let lower = compact.to_ascii_lowercase();
    if is_word_shape(&lower) {
        Some(lower)
    } else {
        None
    }
}

/// 有效词频加成（开启英文补全后英文词频随使用调整，中/英文模式皆然）：
/// 加成 = `BOOST_STEP × min(count, BOOST_CAP)`
///（4000×4 = 16000 封顶）——一次上屏 +4000（中频词一步进前几名），四次即
/// 压过任何种子词（种子谱 ≤9999）。没有时间衰减——英文侧同样只进化
/// 不退化。种子表 english_words 不写：用户数据归 user.db，且 dict.db 会被
/// 种子重铺整库覆盖（写进去也会丢）。
const BOOST_STEP: u32 = 4000;
const BOOST_CAP: u32 = 4;

impl Engine {
    /// word 的使用计数加成（0 = 未用过/未学习）。
    fn english_boost(&self, word: &str) -> u32 {
        self.user_english
            .get(word)
            .map(|c| BOOST_STEP * (*c).min(BOOST_CAP))
            .unwrap_or(0)
    }

    /// 精确命中：返回 (规范展示词形, 有效词频 = 种子 freq + 使用加成)。
    /// 输入大小写不敏感（`ibm` / `IBM` 都命中），展示用存储词形（`IBM`）。
    /// 无 DB / 未命中返回 None。
    pub(crate) fn english_lookup_exact(&self, input: &str) -> Option<(String, u32)> {
        let key = normalize(input)?;
        let storage = self.storage.as_ref()?;
        let (word, freq) = storage.english_exact(&key)?;
        let eff = freq.clamp(0, u32::MAX as i64) as u32 + self.english_boost(&key);
        Some((word, eff))
    }

    /// 前缀命中（含精确命中词），按有效词频降序、平局字典序，最多 `limit`
    /// 条。被加成的低频种子词可能不在「按种子 freq 的前 limit 行」里——
    /// 先取 `max(limit, 256)` 行再合并使用加成重排，保证用过 4 次的词必在
    /// 结果内（256 覆盖 3 字母前缀族常见规模 <100 及 Microsoft/microphone
    /// 这类同前缀品牌词；仍是索引区间扫描的轻量开销）。
    pub(crate) fn english_lookup_prefix(&self, input: &str, limit: usize) -> Vec<(String, u32)> {
        if limit == 0 {
            return Vec::new();
        }
        let Some(key) = normalize(input) else {
            return Vec::new();
        };
        let Some(storage) = self.storage.as_ref() else {
            return Vec::new();
        };
        let mut out: Vec<(String, u32)> = storage
            .english_prefix_words(&key, limit.max(256))
            .into_iter()
            .map(|(w, f)| (w, f.clamp(0, u32::MAX as i64) as u32))
            .collect();
        for (w, f) in out.iter_mut() {
            // 使用计数键恒为小写（学习路径同样归一），展示词形不参与匹配
            *f += self.english_boost(&w.to_ascii_lowercase());
        }
        // 有效 freq 降序、平局字典序（与 SQL 原排序口径一致）
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out.truncate(limit);
        out
    }

    /// 英文模式补全（服务 EnglishComplete 消费）：精确命中词置顶
    ///（必在结果内，即使 freq 不占前），其余 freq 降序，最多 `limit` 条。
    pub fn english_complete(&self, prefix: &str, limit: usize) -> Vec<(String, u32)> {
        if !self.smart_word_en_enabled {
            return Vec::new();
        }
        let mut out: Vec<(String, u32)> = Vec::with_capacity(limit);
        if let Some((word, freq)) = self.english_lookup_exact(prefix) {
            out.push((word, freq));
        }
        for (w, f) in self.english_lookup_prefix(prefix, limit) {
            if out.first().map(|(ew, _)| ew == &w).unwrap_or(false) {
                continue; // 精确词已置顶，跳过前缀结果中的重复
            }
            out.push((w, f));
        }
        out.truncate(limit);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;

    #[test]
    fn test_normalize_shape() {
        assert_eq!(normalize("do'g").as_deref(), Some("dog"));
        assert_eq!(normalize("dog").as_deref(), Some("dog"));
        // 输入大小写不敏感（打字不会刻意区分大小写）
        assert_eq!(normalize("Dog").as_deref(), Some("dog"));
        assert_eq!(normalize("IBM").as_deref(), Some("ibm"));
        assert!(normalize("d").is_none());
        assert!(normalize("dog1").is_none());
    }

    #[test]
    fn test_seed_anchors_forever() {
        // 高频副词/介词锚词守护（防止 forever 类整类漏出）——
        // 种子库（en_engine 临时副本）守护；MUST_HAVE 锚词同款。
        let e = crate::engine::tests_english::en_engine();
        assert!(
            e.english_lookup_exact("forever").is_some(),
            "forever 必须在词表"
        );
        assert!(
            e.english_lookup_exact("despite").is_some(),
            "despite 必须在词表"
        );
        assert!(
            e.english_lookup_exact("hello").is_some(),
            "hello 必须在词表"
        );
    }

    #[test]
    fn test_db_queries_exact_and_prefix() {
        // 内存库：SCHEMA_DICT 建表 → 测试辅助插词 → 验证精确/前缀/排序。
        let st = Storage::open_memory().unwrap();
        st.english_insert_for_test(&[
            ("cat", "n.", "猫", 900),
            ("catalog", "n.", "目录", 800),
            ("category", "n.", "类别", 700),
            ("cattle", "n.", "牛", 600),
            ("catch", "v.", "抓住", 500),
        ]);
        assert_eq!(st.english_exact("cat"), Some(("cat".to_string(), 900)));
        assert_eq!(st.english_exact("caz"), None);
        let pref = st.english_prefix_words("cat", 9);
        assert_eq!(pref.len(), 5);
        assert_eq!(pref[0], ("cat".to_string(), 900)); // freq 降序
        assert_eq!(pref[1], ("catalog".to_string(), 800));
        assert_eq!(pref[2], ("category".to_string(), 700));
        // 上界正确：cat 前缀族不含 "caz" 侧的词（表里没有 caz 开头的词）
        assert!(st.english_prefix_words("caz", 9).is_empty());
        // limit 收敛
        assert_eq!(st.english_prefix_words("cat", 2).len(), 2);
        // 大小写不敏感：大写输入同样命中（返回存储的规范词形）
        assert_eq!(st.english_exact("CAT"), Some(("cat".to_string(), 900)));
        assert_eq!(st.english_prefix_words("CAT", 2).len(), 2);
        // 词性/翻译列可取（后续词典功能消费点）
        assert_eq!(
            st.english_word_detail("cat"),
            Some(("n.".to_string(), "猫".to_string(), 900))
        );
        assert_eq!(
            st.english_word_detail("CAT"),
            Some(("n.".to_string(), "猫".to_string(), 900))
        );
    }
}
