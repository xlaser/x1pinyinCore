//! 三阶上下文模型：(prev2, prev1) → (text → count)
//! 比 bigram 多一阶，捕捉"我·知道·你"三词框架。
//! 稀疏性控制：只在 bigram 命中时才升级查询 trigram（见 context_bonus）。
//! 不做 equiv_prev 泛化（trigram 稀疏性高，泛化放大噪声）。
//!
//! 种子 trigram 不在本结构内。语料预填的种子 trigram
//! 存于 seed_trigram_counts 表，由 `Engine::trigram_count_with_seed` 按需 SQL 查询
//! （用户内存表优先，种子兜底），避免启动全量加载拖慢启动（详见
//! `Storage::seed_trigram_count` 注释）。本结构只存用户运行时积累。

use crate::freq::NgramStore;
use alloc::collections::BTreeMap;

/// 会话级 trigram 增量上限（学习噪声治理，同 bigram 的会话上限；
/// trigram 更稀疏，上限更低）。
const SESSION_TRIGRAM_CAP: u32 = 3;

pub struct TrigramModel {
    /// 用户 trigram 当前值（原嵌套 BTreeMap 68k 条
    /// ~16MB → CSR 基底 ~2MB + overlay；外层 key = (prev2, prev1) 用 "\x00"
    /// 拼接防碰撞，NgramStore 不做键变换。语义等价守护见 freq.rs
    /// `ngram_store_equivalence`——trigram 无 forget 路径，墓碑不被触发）。
    counts: NgramStore,
    /// 自上次成功落盘以来变化的 trigram（dirty 追踪，语义同
    /// FreqModel::bigram_dirty；全量快照 diff 在 6.6 万
    /// 条规模下要几十 ms，改为写入点记录变化 → 落盘 O(变化量)）。
    dirty_entries: BTreeMap<String, BTreeMap<String, u32>>,
    /// 自上次落盘以来删除的 (key, text)。
    dirty_deletes: alloc::collections::BTreeSet<(String, String)>,
    /// 本会话内每个 `(prev2,prev1,text)` 已写入的增量（上限 SESSION_TRIGRAM_CAP）。
    /// `Engine::context_clear` 清空（切换应用/上下文不连续 = 新会话段）。
    session_inc: BTreeMap<(String, String, String), u32>,
    dirty: bool,
}

impl TrigramModel {
    pub fn new() -> Self {
        Self {
            counts: NgramStore::new(),
            dirty_entries: BTreeMap::new(),
            dirty_deletes: alloc::collections::BTreeSet::new(),
            session_inc: BTreeMap::new(),
            dirty: false,
        }
    }

    /// trigram 记录。字+字过渡不记（与 Freq::record_word_bigram
    /// 同源）：prev1 与 text 均为单字时跳过——两个单字高频相邻应由自造词承载，
    /// 字↔字统计只有噪声价值。
    /// prev2 的字数不限制：(中国,的,地) 这类噪声已由 prev1+text 均单字的判据覆盖。
    ///
    /// 生产路径走 `record_weighted`（带学习治理计数）；本方法保留等权便捷入口，
    /// 供测试与外部调用。
    #[allow(dead_code)]
    pub fn record(&mut self, prev2: &str, prev1: &str, text: &str) -> u32 {
        self.record_weighted(prev2, prev1, text, 1)
    }

    /// 带权重记录（拆词学习 +3 用）。字+字过渡同样不记（同 `record`）。
    /// 返回实际写入的增量（会话上限可能截断；0 = 未写）。
    pub fn record_weighted(&mut self, prev2: &str, prev1: &str, text: &str, weight: u32) -> u32 {
        if prev2.is_empty() || prev1.is_empty() || text.is_empty() || weight == 0 {
            return 0;
        }
        if prev1.chars().count() == 1 && text.chars().count() == 1 {
            return 0;
        }
        // 会话级上限（噪音治理）：同一三元组在本会话累计不超过
        // SESSION_TRIGRAM_CAP；context_clear（切 app/上下文不连续）清零。
        let sess_key = (prev2.to_string(), prev1.to_string(), text.to_string());
        let used = self.session_inc.get(&sess_key).copied().unwrap_or(0);
        let weight = weight.min(SESSION_TRIGRAM_CAP.saturating_sub(used));
        if weight == 0 {
            return 0;
        }
        *self.session_inc.entry(sess_key).or_insert(0) += weight;
        let key = format!("{}\x00{}", prev2, prev1);
        let cur = self.counts.record(&key, text, weight);
        self.dirty_entries
            .entry(key)
            .or_default()
            .insert(text.to_string(), cur);
        self.dirty = true;
        weight
    }

    /// 回滚一次 trigram 增量（纠错撤销），语义同 bigram revert：
    /// store 与会话增量同步减，减到 0 记 dirty delete。
    pub(crate) fn revert_weighted(
        &mut self,
        prev2: &str,
        prev1: &str,
        text: &str,
        weight: u32,
    ) -> bool {
        if prev2.is_empty() || prev1.is_empty() || text.is_empty() || weight == 0 {
            return false;
        }
        if prev1.chars().count() == 1 && text.chars().count() == 1 {
            return false;
        }
        let sess_key = (prev2.to_string(), prev1.to_string(), text.to_string());
        if let Some(used) = self.session_inc.get(&sess_key).copied() {
            let remaining = used.saturating_sub(weight);
            if remaining == 0 {
                self.session_inc.remove(&sess_key);
            } else {
                self.session_inc.insert(sess_key.clone(), remaining);
            }
        }
        let key = format!("{}\x00{}", prev2, prev1);
        let next = self.counts.decrement(&key, text, weight);
        if next == 0 {
            self.dirty_deletes.insert((key.clone(), text.to_string()));
            if let Some(m) = self.dirty_entries.get_mut(&key) {
                m.remove(text);
            }
            if self
                .dirty_entries
                .get(&key)
                .map(|m| m.is_empty())
                .unwrap_or(false)
            {
                self.dirty_entries.remove(&key);
            }
        } else {
            self.dirty_deletes.remove(&(key.clone(), text.to_string()));
            self.dirty_entries
                .entry(key)
                .or_default()
                .insert(text.to_string(), next);
        }
        self.dirty = true;
        true
    }

    /// 重置会话级 trigram 增量计数（切换应用 / 上下文不连续时调用）。
    pub(crate) fn reset_session_trigram_inc(&mut self) {
        self.session_inc.clear();
    }

    /// 删除某词参与的全部 trigram（作为 prev2/prev1/text）——删词连带清理。
    /// CSR 基底不可变，逐 key 写墓碑并记 dirty delete。
    pub(crate) fn forget_text(&mut self, text: &str) -> Vec<(String, String)> {
        let to_delete: Vec<(String, String)> = self
            .counts
            .entries_flat()
            .into_iter()
            .filter(|(outer, t, c)| *c > 0 && (t == text || outer.split('\x00').any(|p| p == text)))
            .map(|(outer, t, _)| (outer, t))
            .collect();
        for (key, t) in &to_delete {
            self.counts.forget_pair(key, t);
            self.dirty_deletes.insert((key.clone(), t.clone()));
            if let Some(m) = self.dirty_entries.get_mut(key) {
                m.remove(t);
            }
        }
        for (key, _) in &to_delete {
            if self
                .dirty_entries
                .get(key)
                .map(|m| m.is_empty())
                .unwrap_or(false)
            {
                self.dirty_entries.remove(key);
            }
        }
        self.session_inc
            .retain(|(p2, p1, t), _| p2 != text && p1 != text && t != text);
        if !to_delete.is_empty() {
            self.dirty = true;
        }
        to_delete
    }

    /// 删除单个 trigram（审计/清理用）：当前存在才删；写 dirty delete。
    pub(crate) fn forget_pair(&mut self, prev2: &str, prev1: &str, text: &str) -> bool {
        if prev2.is_empty() || prev1.is_empty() || text.is_empty() {
            return false;
        }
        let key = format!("{}\x00{}", prev2, prev1);
        if self.counts.get(&key, text).is_none() {
            return false;
        }
        self.counts.forget_pair(&key, text);
        self.session_inc
            .remove(&(prev2.to_string(), prev1.to_string(), text.to_string()));
        self.dirty_deletes.insert((key.clone(), text.to_string()));
        if let Some(m) = self.dirty_entries.get_mut(&key) {
            m.remove(text);
        }
        if self
            .dirty_entries
            .get(&key)
            .map(|m| m.is_empty())
            .unwrap_or(false)
        {
            self.dirty_entries.remove(&key);
        }
        self.dirty = true;
        true
    }

    /// 查询用户表的 (prev2, prev1, text) 共现次数（不含种子，种子由
    /// `Engine::trigram_count_with_seed` 按需 SQL 兜底）。
    pub fn count(&self, prev2: &str, prev1: &str, text: &str) -> u32 {
        if prev2.is_empty() || prev1.is_empty() {
            return 0;
        }
        let key = format!("{}\x00{}", prev2, prev1);
        self.counts.get(&key, text).unwrap_or(0)
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    pub fn set_counts<I>(&mut self, entries: I)
    where
        I: IntoIterator<Item = (String, String, String, u32)>,
    {
        self.counts = NgramStore::from_entries(
            entries
                .into_iter()
                .map(|(p2, p1, t, c)| (format!("{}\x00{}", p2, p1), t, c)),
        );
        self.dirty_entries.clear(); // 加载即磁盘状态（dirty 清空）
        self.dirty_deletes.clear();
        self.dirty = true;
    }

    /// 取出自上次落盘以来的 trigram 增量（take 语义；失败需 `restore_delta` 回填）。
    pub fn take_delta(
        &mut self,
    ) -> (
        Vec<(String, String, String, u32)>,
        Vec<(String, String, String)>,
    ) {
        let ups: Vec<(String, String, String, u32)> = std::mem::take(&mut self.dirty_entries)
            .into_iter()
            .filter(|(_, m)| !m.is_empty())
            .flat_map(|(key, m)| {
                let parts = split_key(&key);
                m.into_iter()
                    .map(move |(t, c)| {
                        let (p2, p1) = parts.clone().unwrap_or_default();
                        (p2, p1, t, c)
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let del: Vec<(String, String, String)> = std::mem::take(&mut self.dirty_deletes)
            .into_iter()
            .filter_map(|(key, text)| split_key(&key).map(|(p2, p1)| (p2, p1, text)))
            .collect();
        (ups, del)
    }

    /// 写盘失败回填（保持"未落盘"状态）。
    pub fn restore_delta(
        &mut self,
        upserts: &[(String, String, String, u32)],
        deletes: &[(String, String, String)],
    ) {
        for (p2, p1, text, count) in upserts {
            let key = format!("{}\x00{}", p2, p1);
            self.dirty_entries
                .entry(key)
                .or_default()
                .insert(text.clone(), *count);
        }
        for (p2, p1, text) in deletes {
            let key = format!("{}\x00{}", p2, p1);
            self.dirty_deletes.insert((key, text.clone()));
        }
    }

    /// 扁平快照（测试断言 / 全量重建工具；flush 已改用 `delta`）。
    #[allow(dead_code)]
    pub fn counts_flat(&self) -> Vec<(String, String, String, u32)> {
        self.counts
            .entries_flat()
            .into_iter()
            .map(|(key, text, c)| {
                let (p2, p1) = split_key(&key).unwrap_or_default();
                (p2, p1, text, c)
            })
            .collect()
    }
}

impl Default for TrigramModel {
    fn default() -> Self {
        Self::new()
    }
}

/// 拆 ("prev2\x00prev1") → (prev2, prev1)；格式异常返回 None（防御空 key）。
fn split_key(key: &str) -> Option<(String, String)> {
    let mut it = key.splitn(2, '\x00');
    let p2 = it.next()?.to_string();
    let p1 = it.next()?.to_string();
    Some((p2, p1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trigram_record_and_count() {
        let mut tm = TrigramModel::new();
        tm.record("我", "知道", "你");
        assert_eq!(tm.count("我", "知道", "你"), 1);
        assert_eq!(tm.count("我", "知道", "他"), 0);
        assert_eq!(tm.count("你", "知道", "你"), 0);
    }

    #[test]
    fn test_trigram_weighted() {
        let mut tm = TrigramModel::new();
        tm.record_weighted("我", "知道", "你", 3);
        assert_eq!(tm.count("我", "知道", "你"), 3);
    }

    #[test]
    fn test_trigram_key_no_collision() {
        let mut tm = TrigramModel::new();
        // prev1+text 均单字被过滤，fixture 保证过渡段不全是单字
        tm.record("ab", "c", "XY");
        tm.record("a", "bc", "Y");
        assert_eq!(tm.count("ab", "c", "XY"), 1);
        assert_eq!(tm.count("a", "bc", "Y"), 1);
        assert_eq!(tm.count("ab", "c", "Y"), 0);
    }

    #[test]
    fn test_trigram_flat_roundtrip() {
        let mut tm = TrigramModel::new();
        tm.record("我", "知道", "你");
        tm.record("知道", "你", "的吗");
        let flat = tm.counts_flat();
        let mut tm2 = TrigramModel::new();
        tm2.set_counts(flat);
        assert_eq!(tm2.count("我", "知道", "你"), 1);
        assert_eq!(tm2.count("知道", "你", "的吗"), 1);
    }
}

#[cfg(test)]
mod session_cap_tests {
    use super::*;

    /// 学习噪声治理：同一会话同一三元组增量上限 3；
    /// `reset_session_trigram_inc`（切换应用/上下文不连续）后重新计数。
    #[test]
    fn session_trigram_increment_cap_and_reset() {
        let mut tm = TrigramModel::new();
        for _ in 0..10 {
            tm.record("我们", "一起", "测试");
        }
        assert_eq!(
            tm.count("我们", "一起", "测试"),
            SESSION_TRIGRAM_CAP,
            "同一会话重复记录应封顶"
        );
        tm.reset_session_trigram_inc();
        for _ in 0..2 {
            tm.record("我们", "一起", "测试");
        }
        assert_eq!(
            tm.count("我们", "一起", "测试"),
            SESSION_TRIGRAM_CAP + 2,
            "新会话段应能继续累计"
        );
    }
}
