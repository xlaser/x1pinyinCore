//! 中文词性/语法模型。
//!
//! 第一版消费面：的/地/得 上下文裁决。
//! - 词性侧表（`x1dict_pos`）给每个词一个粗类与主词性；
//! - `x1func_ctx` 给 (前词粗类, 后词粗类) 下 的/地/得 的语料计数；
//! - 用 **lift（语境份额 / 全局份额）** 而不是原始计数：的全局占比 ~96%，
//!   原始计数会让任何语境都偏向的；lift 才能表达“该语境下地显著更可能”。
//!
//! 全部数据来自 PFR 2014（CC0），离线挖掘。

use std::collections::HashMap;
use std::sync::Arc;

use crate::storage::Storage;

/// PFR 细标签 → 粗类。`ad` 归 a（副词性形容词），`vn/vi/...` 归 v。
/// （运行时当前只用 dominant 粗类。）
pub(crate) fn coarse_class(tag: &str) -> &'static str {
    if tag.starts_with('n') {
        "n"
    } else if tag.starts_with('v') {
        "v"
    } else if tag.starts_with('a') {
        "a"
    } else if tag.starts_with('d') {
        "d"
    } else if tag.starts_with('r') {
        "r"
    } else if tag.starts_with('m') || tag.starts_with('q') {
        "m"
    } else if tag.starts_with('t') {
        "t"
    } else if tag.starts_with('f') {
        "f"
    } else if tag.starts_with('p') {
        "p"
    } else if tag.starts_with('c') {
        "c"
    } else if tag.starts_with('u') {
        "u"
    } else if tag.starts_with('w') {
        "w"
    } else {
        "x"
    }
}

/// 口语搭配键：FNV-1a(prev\0curr)。
pub(crate) fn colloc_key(prev: &str, curr: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in prev
        .as_bytes()
        .iter()
        .copied()
        .chain([0u8])
        .chain(curr.as_bytes().iter().copied())
    {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// OOV 构词后缀 → 粗类（短词/自造词没有 PFR 词性时的兜底）。
/// 只做保守的单字后缀，避免把生词硬塞进错误词性。
fn suffix_coarse(word: &str) -> Option<&'static str> {
    const SUFFIXES: &[(char, &str)] = &[
        ('性', "n"),
        ('者', "n"),
        ('员', "n"),
        ('家', "n"),
        ('机', "n"),
        ('器', "n"),
        ('度', "n"),
        ('力', "n"),
        ('感', "n"),
        ('心', "n"),
        ('头', "n"),
        ('子', "n"),
        ('儿', "n"),
        ('们', "n"),
        ('化', "v"),
        ('然', "d"),
    ];
    let last = word.chars().last()?;
    SUFFIXES.iter().find(|(c, _)| *c == last).map(|(_, p)| *p)
}

/// 粗类名 → 紧凑 id（顺序即 POS_CLASSES；未知归 x）。
fn class_id(c: &str) -> u8 {
    POS_CLASSES
        .iter()
        .position(|(_, n)| *n == c)
        .unwrap_or(POS_CLASSES.len() - 1) as u8
}

/// 粗类代号 → 中文名（诊断输出用；模型内部仍是紧凑 id）。
pub(crate) fn coarse_pos_zh(c: &str) -> &'static str {
    match c {
        "n" => "名词",
        "v" => "动词",
        "a" => "形容词",
        "d" => "副词",
        "r" => "代词",
        "m" => "数词",
        "t" => "时间词",
        "f" => "方位词",
        "p" => "介词",
        "c" => "连词",
        "u" => "助词",
        "w" => "标点",
        _ => "其他",
    }
}

/// 粗类 id 序列 → 紧凑 key（每类 4bit + 1；2–6 词最多 24bit）。
fn pattern_key(ids: &[u8]) -> u64 {
    ids.iter()
        .fold(0u64, |k, id| (k << 4) | ((*id as u64 & 0xf) + 1))
}

/// 位图 → 粗类（磁盘表 `pos_mask` 的位序定义）。
const POS_CLASSES: &[(u16, &str)] = &[
    (1, "n"),
    (2, "v"),
    (4, "a"),
    (8, "d"),
    (16, "r"),
    (32, "m"),
    (64, "t"),
    (128, "f"),
    (256, "p"),
    (512, "c"),
    (1024, "u"),
    (2048, "w"),
    (4096, "x"),
];

/// 全部合法粗类位；由 `POS_CLASSES` 派生，新增第 14 类时自动跟上。
const KNOWN_POS_MASK: u16 = {
    let mut mask = 0u16;
    let mut i = 0;
    while i < POS_CLASSES.len() {
        mask |= POS_CLASSES[i].0;
        i += 1;
    }
    mask
};

/// 词性表的紧凑行：mask + 粗类 id，不再为每个词保留独立 String。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PosEntry {
    mask: u16,
    coarse: u8,
}

/// 词性只读表（紧凑内存表示）。
///
/// 旧表示 `HashMap<Arc<str>, PosInfo>` 为 81,399 个词各分配一个 HashMap 桶、
/// 一份词字符串和一份 `dominant: String`；内容不足 1MB，运行时对象头与重复
/// 字符串却吃掉约 10MB。新表示：
/// - `words`：所有词按字典序拼成单一 blob；
/// - `spans`：`(offset, len)` 平行索引，二分查找；
/// - `entries`：mask + 粗类 id 平行数组；
/// - `weights`：仅 55 个粗类多词性词有分布，稀疏 HashMap 保存。
///
/// 数据真相仍是 `x1dict_pos`（外部编辑入口不变）；这里只是启动期派生的
/// 只读表示，**不落盘、不参与失效协议**，改词性后按既有语义重启生效。
#[derive(Debug, Default)]
struct PosTable {
    words: String,
    spans: Vec<(u32, u32)>,
    entries: Vec<PosEntry>,
    weights: HashMap<u32, Arc<[(&'static str, u32)]>>,
}

impl PosTable {
    fn new() -> Self {
        Self::default()
    }

    fn reserve(&mut self, rows: usize) {
        self.spans.reserve(rows);
        self.entries.reserve(rows);
    }

    /// 追加一行；输入必须按词字典序。同词后写覆盖（与旧 HashMap insert 相同）。
    fn push_sorted(&mut self, word: &str, mask: u16, coarse: u8) {
        if let Some(&(off, len)) = self.spans.last() {
            let prev = &self.words[off as usize..(off + len) as usize];
            debug_assert!(prev <= word, "PosTable 输入必须按词字典序: {prev} > {word}");
            if prev == word {
                if let Some(entry) = self.entries.last_mut() {
                    *entry = PosEntry { mask, coarse };
                }
                return;
            }
        }
        let off = self.words.len() as u32;
        self.words.push_str(word);
        self.spans.push((off, word.len() as u32));
        self.entries.push(PosEntry { mask, coarse });
    }

    fn finish(mut self) -> Self {
        self.words.shrink_to_fit();
        self.spans.shrink_to_fit();
        self.entries.shrink_to_fit();
        self
    }

    #[cfg(test)]
    fn from_rows<I>(rows: I) -> Self
    where
        I: IntoIterator<Item = (String, u16, u8)>,
    {
        let mut rows: Vec<(String, u16, u8)> = rows.into_iter().collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        let mut table = Self::new();
        table.reserve(rows.len());
        for (word, mask, coarse) in rows {
            table.push_sorted(&word, mask, coarse);
        }
        table.finish()
    }

    fn index(&self, word: &str) -> Option<usize> {
        self.spans
            .binary_search_by(|&(off, len)| {
                self.words[off as usize..(off + len) as usize].cmp(word)
            })
            .ok()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.spans.len()
    }

    fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    fn contains_key(&self, word: &str) -> bool {
        self.index(word).is_some()
    }

    fn mask(&self, word: &str) -> Option<u16> {
        self.index(word).map(|i| self.entries[i].mask)
    }

    fn coarse_id(&self, word: &str) -> Option<u8> {
        self.index(word).map(|i| self.entries[i].coarse)
    }

    fn coarse(&self, word: &str) -> Option<&'static str> {
        self.coarse_id(word).map(coarse_name)
    }

    fn weights(&self, word: &str) -> Option<&[(&'static str, u32)]> {
        let i = self.index(word)?;
        self.weights.get(&(i as u32)).map(|w| w.as_ref())
    }

    fn set_weights(&mut self, word: &str, weights: Arc<[(&'static str, u32)]>) {
        if let Some(i) = self.index(word) {
            self.weights.insert(i as u32, weights);
        }
    }

    #[cfg(test)]
    fn upsert(&mut self, word: &str, mask: u16, coarse: u8) {
        let mut rows: Vec<(String, u16, u8)> = self
            .spans
            .iter()
            .zip(&self.entries)
            .map(|(&(off, len), entry)| {
                (
                    self.words[off as usize..(off + len) as usize].to_string(),
                    entry.mask,
                    entry.coarse,
                )
            })
            .collect();
        if let Some(row) = rows.iter_mut().find(|(w, _, _)| w == word) {
            *row = (word.to_string(), mask, coarse);
        } else {
            rows.push((word.to_string(), mask, coarse));
        }
        *self = Self::from_rows(rows);
    }

    #[cfg(test)]
    fn remove(&mut self, word: &str) {
        let rows: Vec<(String, u16, u8)> = self
            .spans
            .iter()
            .zip(&self.entries)
            .filter_map(|(&(off, len), entry)| {
                let w = &self.words[off as usize..(off + len) as usize];
                (w != word).then(|| (w.to_string(), entry.mask, entry.coarse))
            })
            .collect();
        *self = Self::from_rows(rows);
    }
}

/// 粗类 id（`POS_CLASSES` 下标）→ 粗类名。数组顺序即 `class_id` 的映射基准。
fn coarse_name(id: u8) -> &'static str {
    POS_CLASSES
        .get(id as usize)
        .map(|(_, name)| *name)
        .unwrap_or("x")
}

/// 词的可能粗类迭代器：mask 位序 = `POS_CLASSES` 顺序；OOV 回退为单类。
/// 栈上结构，不分配 Vec（`classes_for` 在候选打分热路径调用）。
struct PosClasses {
    mask: u16,
    fallback: Option<&'static str>,
    next: usize,
}

impl Iterator for PosClasses {
    type Item = &'static str;

    fn next(&mut self) -> Option<Self::Item> {
        while self.mask != 0 && self.next < POS_CLASSES.len() {
            let (bit, name) = POS_CLASSES[self.next];
            self.next += 1;
            if self.mask & bit != 0 {
                return Some(name);
            }
        }
        self.fallback.take()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining_mask = POS_CLASSES[self.next..]
            .iter()
            .filter(|(bit, _)| self.mask & bit != 0)
            .count();
        let n = remaining_mask + usize::from(self.fallback.is_some());
        (n, Some(n))
    }
}

impl ExactSizeIterator for PosClasses {}

/// 一个 (prev_pos, next_pos) 语境下的 的/地/得 计数。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FuncCounts {
    pub(crate) de: u32,
    pub(crate) di: u32,
    pub(crate) de2: u32,
}

impl FuncCounts {
    fn total(&self) -> u64 {
        self.de as u64 + self.di as u64 + self.de2 as u64
    }
}

/// 词性/语法模型。空模型 = 无词性资产（老库/内置模式），所有消费点返回 0。
#[derive(Default)]
pub(crate) struct GrammarModel {
    pos: PosTable,
    /// prev_pos → next_pos → 的/地/得计数（键是粗类，静态字符串，查询零分配）。
    func_ctx: HashMap<&'static str, HashMap<&'static str, FuncCounts>>,
    total_de: u64,
    total_di: u64,
    total_de2: u64,
    /// 整句语法模板：粗类 id 序列压成 u64 key（每类 4bit，2–6 词）→ count。
    patterns: HashMap<u64, u32>,
    pattern_totals: [u64; 7],
    pattern_kinds: [u32; 7],
    /// 粗类三元词性组合计数（seq id → count）与 (a,b) 上下文分母。
    pos_seq: HashMap<(u8, u8, u8), u32>,
    pos_seq_ctx: HashMap<(u8, u8), u32>,
    pos_seq_class: HashMap<u8, u32>,
    pos_seq_total: u64,
    /// 粗类词性转移计数。
    pos_trans: HashMap<(&'static str, &'static str), u32>,
    /// 每个粗类作为前词的出度总次数（P(b|a) 分母 / unigram 代理）。
    pos_prev_totals: HashMap<&'static str, u64>,
    pos_total_all: u64,
    /// 同音歧义词级上下文：func → (prev_word,next_word) → count。
    word_ctx: HashMap<String, HashMap<(String, String), u32>>,
    /// func → (group, PFR 全局出现次数)。
    func_global: HashMap<String, (String, u64)>,
    /// group → (prev,next) → 组内总次数（P(context|group) 分母）。
    group_ctx: HashMap<String, HashMap<(String, String), u32>>,
    group_totals: HashMap<String, u64>,
    /// group → 组内成员词。
    group_members: HashMap<String, Vec<String>>,
    /// 口语搭配：FNV(prev\0curr) → 共现次数（只服务 2 字词拆分变体）。
    colloc: HashMap<u64, u32>,
}

impl GrammarModel {
    pub(crate) fn load(storage: &Storage) -> rusqlite::Result<Self> {
        let mut model = GrammarModel::default();

        // 流式构建紧凑表：不物化 81k 个 (word, dominant) String。SQL 已按
        // 字典序返回，push_sorted 直接追加；同词后写覆盖语义保持不变。
        let pos_rows = storage.pos_entry_count()?;
        let mut pos_table = PosTable::new();
        pos_table.reserve(pos_rows);
        storage.for_each_pos_entry(|word, mask, dominant| {
            let coarse = class_id(coarse_class(dominant));
            pos_table.push_sorted(word, mask, coarse);
        })?;
        model.pos = pos_table.finish();

        // 多词性细分布：只覆盖粗类真正多类（55 个）的词；单类词不加载。
        for (word, counts) in storage.load_pos_multi_counts()? {
            let mut by_class: HashMap<&'static str, u32> = HashMap::new();
            for part in counts.split(',') {
                let Some((tag, n)) = part.split_once(':') else {
                    continue;
                };
                let Ok(n) = n.parse::<u32>() else {
                    continue;
                };
                *by_class.entry(coarse_class(tag)).or_default() += n;
            }
            if by_class.len() > 1 {
                let mut v: Vec<(&'static str, u32)> = by_class.into_iter().collect();
                v.sort_unstable_by_key(|(_, n)| std::cmp::Reverse(*n));
                model
                    .pos
                    .set_weights(&word, Arc::from(v.into_boxed_slice()));
            }
        }

        for (func, prev, next, count) in storage.load_func_ctx()? {
            let p = coarse_class(&prev);
            let n = coarse_class(&next);
            let entry = model.func_ctx.entry(p).or_default().entry(n).or_default();
            match func.as_str() {
                "的" => entry.de = entry.de.saturating_add(count),
                "地" => entry.di = entry.di.saturating_add(count),
                "得" => entry.de2 = entry.de2.saturating_add(count),
                _ => {}
            }
            match func.as_str() {
                "的" => model.total_de = model.total_de.saturating_add(count as u64),
                "地" => model.total_di = model.total_di.saturating_add(count as u64),
                "得" => model.total_de2 = model.total_de2.saturating_add(count as u64),
                _ => {}
            }
        }

        for (seq, len, count) in storage.load_grammar_patterns()? {
            if !(2..=6).contains(&len) {
                continue;
            }
            let ids: Vec<u8> = seq.split_whitespace().map(class_id).collect();
            if ids.len() != len {
                continue;
            }
            *model.patterns.entry(pattern_key(&ids)).or_default() += count;
            model.pattern_totals[len] = model.pattern_totals[len].saturating_add(count as u64);
            model.pattern_kinds[len] += 1;
        }

        for (seq, count) in storage.load_pos_seq()? {
            let ids: Vec<u8> = seq.split_whitespace().map(class_id).collect();
            if ids.len() != 3 {
                continue;
            }
            let key = (ids[0], ids[1], ids[2]);
            *model.pos_seq.entry(key).or_default() += count;
            *model.pos_seq_ctx.entry((ids[0], ids[1])).or_default() += count;
            *model.pos_seq_class.entry(ids[2]).or_default() += count;
            model.pos_seq_total = model.pos_seq_total.saturating_add(count as u64);
        }

        for (prev, next, count) in storage.load_pos_trans()? {
            let p = coarse_class(&prev);
            let n = coarse_class(&next);
            model.pos_trans.insert((p, n), count);
            *model.pos_prev_totals.entry(p).or_default() += count as u64;
            model.pos_total_all = model.pos_total_all.saturating_add(count as u64);
        }

        for (func, grp, total) in storage.load_func_word_global()? {
            model
                .func_global
                .insert(func.clone(), (grp.clone(), total as u64));
            model
                .group_totals
                .entry(grp.clone())
                .and_modify(|v| *v = v.saturating_add(total as u64))
                .or_insert(total as u64);
            model.group_members.entry(grp).or_default().push(func);
        }
        for (func, grp, prev, next, count) in storage.load_func_word_ctx()? {
            model
                .word_ctx
                .entry(func)
                .or_default()
                .insert((prev.clone(), next.clone()), count);
            *model
                .group_ctx
                .entry(grp)
                .or_default()
                .entry((prev, next))
                .or_default() += count;
        }

        for (prev, curr, count) in storage.load_colloc()? {
            model.colloc.insert(colloc_key(&prev, &curr), count);
        }

        Ok(model)
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.pos.is_empty() && !self.func_ctx.is_empty()
    }

    /// 口语搭配表是否可用（拆分变体总闸）。
    pub(crate) fn has_colloc(&self) -> bool {
        !self.colloc.is_empty()
    }

    /// 相邻词口语共现次数（0 = 无证据）。
    pub(crate) fn colloc_count(&self, prev: &str, curr: &str) -> u32 {
        if prev.is_empty() || curr.is_empty() || prev == curr {
            return 0;
        }
        self.colloc
            .get(&colloc_key(prev, curr))
            .copied()
            .unwrap_or(0)
    }

    /// 整条路径的相邻搭配总分。
    pub(crate) fn path_colloc_score(&self, words: &[String]) -> u32 {
        let mut sum = 0u32;
        for w in words.windows(2) {
            sum = sum.saturating_add(self.colloc_count(&w[0], &w[1]));
        }
        sum
    }

    fn pos_active(&self) -> bool {
        !self.pos.is_empty() && !self.pos_trans.is_empty()
    }

    /// 测试构造入口：向紧凑词性表 upsert 一行（生产数据始终来自 `x1dict_pos`）。
    #[cfg(test)]
    fn insert_pos(&mut self, word: &str, mask: u16, dominant: &str) {
        let coarse = class_id(coarse_class(dominant));
        self.pos.upsert(word, mask, coarse);
    }

    #[cfg(test)]
    fn remove_pos(&mut self, word: &str) {
        self.pos.remove(word);
    }

    #[cfg(test)]
    fn pos_len(&self) -> usize {
        self.pos.len()
    }

    #[cfg(test)]
    fn pos_mask(&self, word: &str) -> Option<u16> {
        self.pos.mask(word)
    }

    #[cfg(test)]
    fn coarse_for(&self, word: &str) -> Option<&'static str> {
        self.pos.coarse(word)
    }

    /// 词的可能粗类（mask 全展开；mask=0 时回退 dominant 粗类）。
    /// 返回栈上迭代器，热路径不分配 Vec。
    fn classes_for(&self, word: &str) -> PosClasses {
        let Some(mask) = self.pos.mask(word) else {
            // OOV / 自造词 / 短词：用末字词性、首字词性或构词后缀回退。
            // 只参与排序（path_pos_delta），不生成候选文本。
            let fallback = if word.chars().count() >= 2 {
                // 不物化 Vec<char>，也不为单字构造临时 String（热路径零分配）。
                let mut buf = [0u8; 4];
                let mut fallback = None;
                if let Some(ch) = word.chars().last() {
                    fallback = self.pos.coarse(ch.encode_utf8(&mut buf));
                }
                if fallback.is_none() {
                    if let Some(ch) = word.chars().next() {
                        fallback = self.pos.coarse(ch.encode_utf8(&mut buf));
                    }
                }
                fallback.or_else(|| suffix_coarse(word))
            } else {
                None
            };
            return PosClasses {
                mask: 0,
                fallback,
                next: 0,
            };
        };
        // 与旧实现等价：仅当没有任何已知粗类位（含非法高位）时才回退 dominant。
        let fallback = if mask & KNOWN_POS_MASK == 0 {
            self.pos.coarse(word)
        } else {
            None
        };
        PosClasses {
            mask,
            fallback,
            next: 0,
        }
    }

    /// 多词性先验：log2(该粗类占比)×0.5，钳 ±1。只对加载了 weights 的
    /// 55 个粗类多词性词生效；单类词恒 0。
    fn pos_prior(&self, word: &str, c: &str) -> f64 {
        let Some(weights) = self.pos.weights(word) else {
            return 0.0;
        };
        let total: u32 = weights.iter().map(|(_, n)| *n).sum();
        if total == 0 {
            return 0.0;
        }
        let Some((_, n)) = weights.iter().find(|(k, _)| *k == c) else {
            return 0.0;
        };
        ((*n as f64 / total as f64).log2() * 0.5).clamp(-1.0, 1.0)
    }

    /// 粗类转移 PMI：`log2(P(b|a)/P(b))`（加 0.1 平滑）。
    fn pos_pmi(&self, a: &str, b: &str) -> f64 {
        let cab = self.pos_trans.get(&(a, b)).copied().unwrap_or(0) as f64;
        let ta = self.pos_prev_totals.get(a).copied().unwrap_or(0) as f64;
        let tb = self.pos_prev_totals.get(b).copied().unwrap_or(0) as f64;
        let total = self.pos_total_all as f64;
        if ta == 0.0 || total == 0.0 {
            return 0.0;
        }
        let k = POS_CLASSES.len() as f64;
        let p_ba = (cab + 0.1) / (ta + 0.1 * k);
        let p_b = (tb + 0.1) / (total + 0.1 * k);
        (p_ba / p_b).log2()
    }

    /// 词的主导粗类（已知词用 dominant，OOV 用回退的第一类）。
    fn dominant_class(&self, word: &str) -> Option<&'static str> {
        if let Some(coarse) = self.pos.coarse(word) {
            return Some(coarse);
        }
        self.classes_for(word).next()
    }

    /// 模板命中次数（按粗类 id 序列；不套用阈值/OOV 门控）。
    fn pattern_count_ids(&self, ids: &[u8]) -> Option<u32> {
        if self.patterns.is_empty() || !(2..=6).contains(&ids.len()) {
            return None;
        }
        self.patterns.get(&pattern_key(ids)).copied()
    }

    /// 整句模板命中次数（不套用阈值/OOV 门控；诊断用）。
    pub(crate) fn pattern_count(&self, words: &[String]) -> Option<u32> {
        if self.patterns.is_empty() || !(2..=6).contains(&words.len()) {
            return None;
        }
        let ids: Vec<u8> = words
            .iter()
            .map(|w| self.dominant_class(w).map(class_id).unwrap_or(0))
            .collect();
        if ids.len() != words.len() {
            return None;
        }
        self.pattern_count_ids(&ids)
    }

    /// 整句语法模板的排序增量。
    ///
    /// 短路径（≤6 词）查完整模板；中长路径（7–24 词）用 4–6 词滑动窗口
    /// 评估语法块覆盖——支持多词组合，不把长句硬塞进稀疏全句模板。
    /// 只奖励高频、全词有 PFR 词性的组合；未命中不罚。
    pub(crate) fn pattern_delta(&self, words: &[String]) -> i32 {
        if self.patterns.is_empty() || words.len() < 2 {
            return 0;
        }
        if words.iter().any(|w| !self.pos.contains_key(w.as_str())) {
            return 0;
        }
        let ids: Vec<u8> = words
            .iter()
            .map(|w| self.dominant_class(w).map(class_id).unwrap_or(0))
            .collect();
        if ids.len() != words.len() {
            return 0;
        }
        if ids.len() <= 6 {
            let Some(count) = self.pattern_count_ids(&ids) else {
                return 0;
            };
            // 短路径沿用整模板直分（0..=3）；滑动窗口才用 0..=2 的块分。
            let total = self.pattern_totals[ids.len()];
            let kinds = self.pattern_kinds[ids.len()];
            if total == 0 || kinds == 0 {
                return 0;
            }
            let avg = total as f64 / kinds as f64;
            if avg <= 0.0 {
                return 0;
            }
            return ((1.0 * (count as f64 / avg).log2()).round() as i32).clamp(0, 3);
        }
        if ids.len() > 24 {
            return 0;
        }
        let mut scores: Vec<i32> = Vec::new();
        for size in 4..=6 {
            for win in ids.windows(size) {
                let Some(count) = self.pattern_count_ids(win) else {
                    continue;
                };
                let score = self.pattern_count_score(count, size);
                if score > 0 {
                    scores.push(score);
                }
            }
        }
        if scores.is_empty() {
            return 0;
        }
        scores.sort_unstable_by(|a, b| b.cmp(a));
        let sum: i32 = scores.iter().take(3).sum();
        // 单个语法块也应有轻量信号（整数除法会把 1 分抹成 0）。
        ((sum as f64 / 2.0).ceil() as i32).clamp(0, 3)
    }

    /// 单个模板次数 → 0..=2 的轻量分（低频不参与）。
    fn pattern_count_score(&self, count: u32, len: usize) -> i32 {
        if count < 100 {
            return 0;
        }
        let total = self.pattern_totals[len];
        let kinds = self.pattern_kinds[len];
        if total == 0 || kinds == 0 {
            return 0;
        }
        let avg = total as f64 / kinds as f64;
        if avg <= 0.0 {
            return 0;
        }
        ((1.0 * (count as f64 / avg).log2()).round() as i32).clamp(0, 2)
    }

    /// 三元词性组合的排序增量：对路径的主导粗类串，按 P(c|a,b) 相对
    /// P(c) 的 lift 累加。只对 ≥3 词路径启用，钳 ±8；不做生成。
    pub(crate) fn path_pos_trigram_delta(&self, words: &[String]) -> i32 {
        if self.pos_seq.is_empty() || words.len() < 3 {
            return 0;
        }
        let ids: Vec<u8> = words
            .iter()
            .map(|w| self.dominant_class(w).map(class_id).unwrap_or(0))
            .collect();
        if ids.len() != words.len() {
            return 0;
        }
        let total = self.pos_seq_total as f64;
        if total == 0.0 {
            return 0;
        }
        let k = POS_CLASSES.len() as f64;
        let mut sum = 0.0f64;
        for i in 0..ids.len() - 2 {
            let key = (ids[i], ids[i + 1], ids[i + 2]);
            let count = self.pos_seq.get(&key).copied().unwrap_or(0) as f64;
            let ctx = self
                .pos_seq_ctx
                .get(&(ids[i], ids[i + 1]))
                .copied()
                .unwrap_or(0) as f64;
            let cls = self.pos_seq_class.get(&ids[i + 2]).copied().unwrap_or(0) as f64;
            if ctx == 0.0 || cls == 0.0 {
                continue;
            }
            let p_cond = (count + 0.1) / (ctx + 0.1 * k);
            let p_cls = (cls + 0.1) / (total + 0.1 * k);
            sum += (p_cond / p_cls).log2();
        }
        // 轻量三元先验：权重刻意低（0.5、钳 ±2），避免局部词性组合压过
        // 后文长程语境；本层保持中性，作为词性组合记录的第一版特征保留。
        ((sum * 0.5).round() as i32).clamp(-2, 2)
    }

    /// 短路径（≤6 词）的 POS 转移增量（Viterbi 选每词最合适的粗类）。
    ///
    /// 中长句 A/B 实测：相邻 Viterbi 在长句会误推错误同音句，
    /// 因此 >6 词交给三元/滑动模板与长程语境。只参与排序、不生成文本；
    /// 返回 PMI 和×1.5，钳 ±12。做/作、那/哪 的 POS 类相同，靠本层无法区分
    /// （留给词级上下文）。
    pub(crate) fn path_pos_delta(&self, words: &[String]) -> i32 {
        if !self.pos_active() || words.len() < 2 || words.len() > 6 {
            return 0;
        }
        // 词性 + 词性组合只参与排序，不生成文本。
        // OOV 的粗类由 classes_for 的末字/首字/后缀回退提供；
        // 生成缺口由绿词层单独观察。
        let first = self.classes_for(&words[0]);
        if first.len() == 0 {
            return 0;
        }
        let mut prev: HashMap<&'static str, f64> =
            first.map(|c| (c, self.pos_prior(&words[0], c))).collect();
        for word in &words[1..] {
            let cur = self.classes_for(word);
            if cur.len() == 0 {
                return 0;
            }
            let mut next: HashMap<&'static str, f64> = HashMap::new();
            for c in cur {
                let prior = self.pos_prior(word, c);
                let best = prev
                    .iter()
                    .map(|(p, s)| s + self.pos_pmi(p, c) + prior)
                    .fold(f64::NEG_INFINITY, f64::max);
                if best.is_finite() {
                    next.insert(c, best);
                }
            }
            if next.is_empty() {
                return 0;
            }
            prev = next;
        }
        let best = prev.values().cloned().fold(f64::NEG_INFINITY, f64::max);
        if !best.is_finite() {
            return 0;
        }
        ((best * 1.5).round() as i32).clamp(-12, 12)
    }

    /// 同音歧义词的上下文 lift：`P(context|func)/P(context|group)`。
    fn word_ctx_delta(&self, func: &str, prev: &str, next: &str) -> i32 {
        let Some((grp, total_func)) = self.func_global.get(func) else {
            return 0;
        };
        let total_func = *total_func;
        let group_total = self.group_totals.get(grp).copied().unwrap_or(0);
        if total_func == 0 || group_total == 0 {
            return 0;
        }
        let key = (prev.to_string(), next.to_string());
        let count = self
            .word_ctx
            .get(func)
            .and_then(|m| m.get(&key))
            .copied()
            .unwrap_or(0);
        let ctx_sum = self
            .group_ctx
            .get(grp)
            .and_then(|m| m.get(&key))
            .copied()
            .unwrap_or(0);
        if ctx_sum < 2 {
            return 0;
        }
        if count == 0 {
            return -12;
        }
        let share = count as f64 / total_func as f64;
        let base = ctx_sum as f64 / group_total as f64;
        if share <= 0.0 || base <= 0.0 {
            return 0;
        }
        (((share / base).log2() * 4.0).round() as i32).clamp(-12, 12)
    }

    /// 整条路径上同音歧义词的词级上下文增量。
    pub(crate) fn path_word_ctx_delta(&self, words: &[String]) -> i32 {
        if words.is_empty() || self.word_ctx.is_empty() {
            return 0;
        }
        let mut sum = 0i32;
        for i in 0..words.len() {
            if !self.func_global.contains_key(words[i].as_str()) {
                continue;
            }
            let prev = if i == 0 { "<s>" } else { words[i - 1].as_str() };
            let next = if i + 1 == words.len() {
                "</s>"
            } else {
                words[i + 1].as_str()
            };
            sum += self.word_ctx_delta(&words[i], prev, next);
        }
        sum.clamp(-24, 24)
    }

    /// 若路径里某个同音词在词级上下文下显著应换成组内另一词，返回 (下标, 替换词)。
    pub(crate) fn word_ctx_variant(&self, words: &[String]) -> Option<(usize, String)> {
        if words.len() < 2 || self.word_ctx.is_empty() {
            return None;
        }
        let mut best: Option<(usize, String, i32)> = None;
        for i in 0..words.len() {
            let w = words[i].as_str();
            let Some((grp, _)) = self.func_global.get(w) else {
                continue;
            };
            let prev = if i == 0 { "<s>" } else { words[i - 1].as_str() };
            let next = if i + 1 == words.len() {
                "</s>"
            } else {
                words[i + 1].as_str()
            };
            let cur = self.word_ctx_delta(w, prev, next);
            let Some(members) = self.group_members.get(grp) else {
                continue;
            };
            for alt in members {
                if alt == w {
                    continue;
                }
                let d = self.word_ctx_delta(alt, prev, next);
                if d >= cur + 4 && best.as_ref().is_none_or(|(_, _, bd)| d > *bd) {
                    best = Some((i, alt.clone(), d));
                }
            }
        }
        best.map(|(i, alt, _)| (i, alt))
    }

    /// 词内目标字替换变体：`我在` + 后词 `去` → `我再`。
    ///
    /// 在词内找单字目标（在/再/做/作/那/哪……），用组内同音字替换后，
    /// 用**词内前字 / 后字（或路径邻词）**作为上下文比较词级 lift。
    /// 返回替换后的整词；词典存在性由调用方校验。
    pub(crate) fn word_char_variant(
        &self,
        word: &str,
        prev: &str,
        next: &str,
    ) -> Option<(String, i32)> {
        if self.word_ctx.is_empty() {
            return None;
        }
        let chars: Vec<char> = word.chars().collect();
        let mut best: Option<(String, i32)> = None;
        for j in 0..chars.len() {
            let c = chars[j].to_string();
            let Some((grp, _)) = self.func_global.get(&c) else {
                continue;
            };
            let p = if j > 0 {
                chars[j - 1].to_string()
            } else {
                prev.to_string()
            };
            let n = if j + 1 < chars.len() {
                chars[j + 1].to_string()
            } else {
                next.to_string()
            };
            let cur = self.word_ctx_delta(&c, &p, &n);
            let Some(members) = self.group_members.get(grp) else {
                continue;
            };
            for alt in members {
                if alt == &c || alt.chars().count() != 1 {
                    continue;
                }
                let d = self.word_ctx_delta(alt, &p, &n);
                if d >= cur + 4 && best.as_ref().is_none_or(|(_, bd)| d > *bd) {
                    let mut out = chars.clone();
                    out[j] = alt.chars().next()?;
                    best = Some((out.into_iter().collect(), d));
                }
            }
        }
        best
    }

    /// 2 字词末字 的/地/得 变体（干的 → 干得）。
    ///
    /// DP 常把「干的」当一个词，单字层的 `di_variant_index` 覆盖不到词内
    /// 末字；用词首字（句首时）作前词上下文，按 `func_ctx` lift 选末字。
    /// 只在增益 ≥4 时生成，避免把正确的「的」改坏。
    pub(crate) fn de_word_variant(
        &self,
        word: &str,
        prev: &str,
        next: &str,
    ) -> Option<(String, i32)> {
        if !self.is_active() {
            return None;
        }
        let chars: Vec<char> = word.chars().collect();
        if chars.len() < 2 {
            return None;
        }
        let last = *chars.last()?;
        let last_str = last.to_string();
        if !matches!(last, '的' | '地' | '得') {
            return None;
        }
        // 句首没有前词时，用该词首字作粗类上下文（干的：干/v + 漂亮/a）。
        let ctx_prev = if prev == "<s>" {
            chars[0].to_string()
        } else {
            prev.to_string()
        };
        let c = self.de_choice(&ctx_prev, next)?;
        let cur = self.delta_for(&last_str, &c);
        let mut best: Option<(String, i32)> = None;
        for alt in ["的", "地", "得"] {
            if alt == last_str {
                continue;
            }
            let d = self.delta_for(alt, &c);
            if d < cur + 4 {
                continue;
            }
            let mut out = chars.clone();
            *out.last_mut()? = alt.chars().next()?;
            let text: String = out.into_iter().collect();
            if best.as_ref().is_none_or(|(_, bd)| d - cur > *bd) {
                best = Some((text, d - cur));
            }
        }
        best
    }

    fn de_choice(&self, prev: &str, next: &str) -> Option<FuncCounts> {
        let p = self.pos.coarse(prev)?;
        let n = self.pos.coarse(next)?;
        self.func_ctx.get(p).and_then(|m| m.get(n)).copied()
    }

    /// 语境 lift → 分数增量。
    ///
    /// `lift = P(func | context) / P(func)`；log2(lift)×4，钳 ±12。
    /// 的全局占比 ~96%，所以必须用 lift，否则任何语境都偏向的。
    fn delta_for(&self, func: &str, c: &FuncCounts) -> i32 {
        let ctx_total = c.total();
        if ctx_total < 4 {
            return 0;
        }
        let total_all = self.total_de + self.total_di + self.total_de2;
        if total_all == 0 {
            return 0;
        }
        let (count, base_total) = match func {
            "的" => (c.de as u64, self.total_de),
            "地" => (c.di as u64, self.total_di),
            "得" => (c.de2 as u64, self.total_de2),
            _ => return 0,
        };
        if base_total == 0 {
            return 0;
        }
        if count == 0 {
            return -12;
        }
        let share = count as f64 / ctx_total as f64;
        let base = base_total as f64 / total_all as f64;
        if share <= 0.0 || base <= 0.0 {
            return 0;
        }
        let lift = share / base;
        ((lift.log2() * 4.0).round() as i32).clamp(-12, 12)
    }

    /// 整条路径上单字 的/地/得 的累计语法增量。
    pub(crate) fn path_delta(&self, words: &[String]) -> i32 {
        if !self.is_active() || words.len() < 3 {
            return 0;
        }
        let mut sum = 0i32;
        for i in 1..words.len() - 1 {
            let w = words[i].as_str();
            if !matches!(w, "的" | "地" | "得") {
                continue;
            }
            if let Some(c) = self.de_choice(&words[i - 1], &words[i + 1]) {
                sum += self.delta_for(w, &c);
            }
        }
        sum.clamp(-24, 24)
    }

    /// 若路径里某个“的”在语境下显著应作“地”，返回该下标。
    ///
    /// 判定用 lift 差：`delta(地) - delta(的) >= 4` 且 地 至少有 2 次语料
    /// 证据。调用方负责替换文本/更新词频，避免 GrammarModel 依赖 Dict。
    pub(crate) fn di_variant_index(&self, words: &[String]) -> Option<usize> {
        if !self.is_active() || words.len() < 3 {
            return None;
        }
        let mut best: Option<(usize, i32)> = None;
        for i in 1..words.len() - 1 {
            if words[i] != "的" {
                continue;
            }
            // 生成“地”变体只走典型状中结构：前词形容词/副词（a/d）+
            // 后词动词/形容词/副词（v/a/d）。避免在名词前、代词后等
            // 语境滥用 lift 造出错误变体（原“的”候选仍保留）。
            let Some(prev_coarse) = self.pos.coarse(words[i - 1].as_str()) else {
                continue;
            };
            if !matches!(prev_coarse, "a" | "d") {
                continue;
            }
            let Some(next_coarse) = self.pos.coarse(words[i + 1].as_str()) else {
                continue;
            };
            if !matches!(next_coarse, "v" | "a" | "d") {
                continue;
            }
            let Some(c) = self.de_choice(&words[i - 1], &words[i + 1]) else {
                continue;
            };
            if c.di < 2 {
                continue;
            }
            let diff = self.delta_for("地", &c) - self.delta_for("的", &c);
            if diff >= 4 && best.is_none_or(|(_, d)| diff > d) {
                best = Some((i, diff));
            }
        }
        best.map(|(i, _)| i)
    }
}

impl GrammarModel {
    /// 中长句 4–6 词滑动模板覆盖（命中窗口数 / 总窗口数；诊断用）。
    pub(crate) fn pattern_window_stats(&self, words: &[String]) -> (usize, usize) {
        if words.len() <= 6 || words.len() > 24 {
            return (0, 0);
        }
        if words.iter().any(|w| !self.pos.contains_key(w.as_str())) {
            return (0, 0);
        }
        let ids: Vec<u8> = words
            .iter()
            .map(|w| self.dominant_class(w).map(class_id).unwrap_or(0))
            .collect();
        let mut hits = 0usize;
        let mut total = 0usize;
        for size in 4..=6 {
            for win in ids.windows(size) {
                total += 1;
                if self.pattern_count_ids(win).is_some_and(|c| c >= 100) {
                    hits += 1;
                }
            }
        }
        (hits, total)
    }
}

impl crate::engine::Engine {
    /// 绿词排序微型模型开关：开启后在 ≤5 词口语短链上给 legacy
    /// 词性三项叠加正向微型分（见 [`Self::grammar_ab_pos_delta`]）；关闭或模型
    /// 缺失 = 逐字节保持 legacy。**默认开**（扩展口语语料 + 正分融合验证通过）。
    pub fn set_grammar_micro_enabled(&self, on: bool) {
        self.grammar_micro_enabled.set(on);
    }

    /// 当前微型语法分是否参与排序（调试 CLI 诊断展示生产默认态）。
    pub fn grammar_micro_enabled(&self) -> bool {
        self.grammar_micro_enabled.get()
    }

    /// 已加载微型模型的元信息（资产缺失/解码失败降级态 = None）。调试 CLI 诊断消费。
    pub fn grammar_micro_info(&self) -> Option<crate::grammar_micro::GrammarMicroInfo> {
        let info = self.grammar_micro.info();
        info.path.is_some().then_some(info)
    }

    /// 词 → 微型模型粗类 id（`classes_for` 首类映射，多词性词 = mask 展开
    /// 首位；OOV 走末字/首字/构词后缀回退，仍未知 → 类 0，与 legacy
    /// `pattern_count` 的 `unwrap_or(0)` 同款，模型不惩罚未命中组合）。
    /// 排序（`grammar_micro_delta`）与只读观测（`grammar_micro_path_components`）
    /// 共用同一实现，词→类映射全工程仅此一份（消费侧诊断不复刻映射）。
    fn micro_classes(&self, words: &[String]) -> Vec<u8> {
        words
            .iter()
            .map(|w| {
                self.grammar
                    .classes_for(w)
                    .next()
                    .map(class_id)
                    .unwrap_or(0)
            })
            .collect()
    }

    /// 微型词性搭配模型的路径分（绿词排序 A/B 的「开」臂内部实现）。
    ///
    /// - 门控：开关关 / 词数不在 2..=24 → 0（与 `GrammarMicro::path_score`
    ///   的路径上限同口径）；
    /// - 词性映射：[`Self::micro_classes`]（多词性词 = mask 展开首位；
    ///   OOV 回退链，仍未知 → 类 0）；
    /// - 上下文让位：路径已有强语境证据（上文栈非空 / 任一相邻词对
    ///   bigram ≥3）时归零——与 `grammar_pattern_delta` 的“让位”语义一致，
    ///   防局部句法模型压过长程/右文证据。
    ///
    /// 放在 grammar.rs 实现：`classes_for` / `class_id` 是本模块私有项，
    /// 不为接入暴露新的可见性。
    pub(crate) fn grammar_micro_delta(&self, words: &[String]) -> i32 {
        if !self.grammar_micro_enabled.get() || words.len() < 2 || words.len() > 24 {
            return 0;
        }
        let classes = self.micro_classes(words);
        let raw = self.grammar_micro.path_score(&classes);
        let strong_context = !self.context.is_empty()
            || words
                .windows(2)
                .any(|w| self.freq.word_bigram_count(&w[0], &w[1]) >= 3);
        // 强上下文（上文栈/路径强词对）时完全让位：正分融合实验中 raw/4
        // 仍会翻转 moxingdafen/womendetongshi 长程语境。
        if strong_context {
            0
        } else {
            raw
        }
    }

    /// 微型语法模型的路径分诊断（只读观测；调试 CLI
    /// `grammar` 命令消费——词→粗类映射与逐层分量不在消费侧复刻，
    /// 避免双份逻辑漂移）。
    ///
    /// - **不受总闸开关影响**：任何开关状态都能拿到分量（观测 ≠ 参与排序，
    ///   调试 CLI 的「关闭（仅观测）」行仍由旗标渲染）；
    /// - 词→粗类映射与 `grammar_micro_delta` 共用 [`Self::micro_classes`]；
    /// - 逐层分量来自 `GrammarMicro::path_breakdown`（与排序 `path_score`
    ///   单实现，`(bigram+trigram+block).clamp(-24,24)` 恒等于 `raw_total`）；
    /// - 模型未加载（资产缺失/解码失败降级态）→ `loaded=false`、分量全 0；
    /// - 词数 ∉ 2..=24 → 分量全 0（与打分门控同口径）。
    ///
    /// 只读：不触碰开关/上下文/缓存等任何可变引擎状态。
    pub fn grammar_micro_path_components(
        &self,
        words: &[String],
    ) -> crate::grammar_micro::GrammarMicroPathDiagnosis {
        let loaded = self.grammar_micro.is_loaded();
        if !loaded || !(2..=24).contains(&words.len()) {
            return crate::grammar_micro::GrammarMicroPathDiagnosis {
                loaded,
                ..Default::default()
            };
        }
        let classes = self.micro_classes(words);
        let b = self.grammar_micro.path_breakdown(&classes);
        crate::grammar_micro::GrammarMicroPathDiagnosis {
            loaded,
            bigram: b.bigram,
            trigram: b.trigram,
            block: b.block,
            raw_total: b.total,
        }
    }

    /// 生产默认开关：默认开；`X1_GRAMMAR_MICRO=0` 逃生口（模型缺失时本就
    /// 降级回 legacy，该环境变量只用于显式对照/回退）。
    pub(crate) fn grammar_micro_default_enabled() -> bool {
        !std::env::var("X1_GRAMMAR_MICRO")
            .map(|v| v == "0")
            .unwrap_or(false)
    }

    /// 绿词语法分「词性组合层」的出口（sentence.rs 语法打分块唯一消费点）：
    /// 微型模型开启、已加载且路径 ≤5 词时，在 legacy 三项之上叠加
    /// **正向微型分的四分之一**（`max(micro,0)/4`）。只叠正分的原因：
    /// PFR/口语混合统计仍有域偏差，负分惩罚会误压口语短链；正向奖励只把
    /// 「更常见词性搭配」往上抬。其余情况与旧组合逐字节一致（关臂 = 基线）。
    ///
    /// 降级回退（纵深防御）：开关开但资产缺失/
    /// 解码失败（空模型，`is_loaded()==false`）时**回退 legacy 三项**而非给 0
    /// ——「开」臂只在真实模型在场时才叠微型正分，防退化安装静默丢掉整层
    /// 词性信号、也防未来 A/B 处理臂混入「无词性分」的污染样本。
    /// `path_delta`（的/地/得语境）与 `path_word_ctx_delta`（词级上下文）两层
    /// 不受本开关影响，两种臂均保留。
    pub(crate) fn grammar_ab_pos_delta(&self, words: &[String]) -> i32 {
        if self.grammar_micro_enabled.get() && self.grammar_micro.is_loaded() && words.len() <= 5 {
            let micro = self.grammar_micro_delta(words).max(0);
            let legacy = self.grammar.path_pos_delta(words)
                + self.grammar.path_pos_trigram_delta(words)
                + self.grammar_pattern_delta(words);
            legacy + micro / 4
        } else {
            self.grammar.path_pos_delta(words)
                + self.grammar.path_pos_trigram_delta(words)
                + self.grammar_pattern_delta(words)
        }
    }

    /// 绿词语法模板分（上下文让位版）：路径内部已有强词对证据
    ///（用户 bigram ≥3）时只保留 1/4 权重，避免局部句法模板压过
    /// 长程/右文证据。只排序，不生成文本。
    ///
    /// 三档证据门（quanliyifua 案例衍生）：
    /// ① 强证据（context 非空或路径内任一词对 bigram ≥3）→ raw/4：词元证据
    ///    在场，模板退居辅助（现状）；
    /// ② 有边际证据（任一词对 bigram ≥1）→ raw 全量：词性序列 + 词元关联
    ///    双重佐证（现状）；
    /// ③ **零关联**（所有相邻词对 bigram=0 且无 context）→ raw/4：模板命中
    ///    只证明「词性序列像中文」（名词接名词在自然语料极常见——主谓宾结构
    ///    的碎片），不证明「这两个词有搭配关系」。「权力|衣服」类碰巧同音的
    ///    碎词拼接拿全量模板分 = 语法层给碎拼背书，整类压制。
    /// 判据用 [`FreqModel::word_bigram_count`]（用户+种子+等价类三层），与
    /// DP 的 bigram 证据同源，不引新表。
    pub(crate) fn grammar_pattern_delta(&self, words: &[String]) -> i32 {
        let raw = self.grammar.pattern_delta(words);
        if raw <= 0 {
            return 0;
        }
        let best_link = words
            .windows(2)
            .map(|w| self.freq.word_bigram_count(&w[0], &w[1]))
            .max()
            .unwrap_or(0);
        let has_context = !self.context.is_empty();
        if !pattern_evidence_ab::enabled() {
            // A/B 关闭态 = 旧两档行为（复现「A 之前」，评测对照用）
            let strong_context = has_context || best_link >= 3;
            return if strong_context { raw / 4 } else { raw };
        }
        if has_context || best_link >= 3 {
            raw / 4
        } else if best_link >= 1 {
            raw
        } else {
            raw / 4
        }
    }

    /// 语法诊断（只观测）：输出中文词性、模板命中/让位、二元/三元分。
    /// 与排序使用同一批方法，但不改变任何候选分数。
    pub fn grammar_diagnosis(&self, words: &[String]) -> crate::engine::GrammarDiagnosis {
        let pos_known: Vec<bool> = words
            .iter()
            .map(|w| self.grammar.pos.contains_key(w.as_str()))
            .collect();
        let pos: Vec<String> = words
            .iter()
            .zip(pos_known.iter())
            .map(|(w, known)| {
                let label = self
                    .grammar
                    .dominant_class(w)
                    .map(coarse_pos_zh)
                    .unwrap_or("未知");
                if *known {
                    label.to_string()
                } else {
                    format!("{label}（推测）")
                }
            })
            .collect();
        let all_known_pos = pos_known.iter().all(|k| *k);
        let pattern_count = self.grammar.pattern_count(words).unwrap_or(0);
        let pattern_hit = pattern_count > 0;
        let pattern_raw = self.grammar.pattern_delta(words);
        let (window_hits, window_total) = self.grammar.pattern_window_stats(words);
        let strong_context = !self.context.is_empty()
            || words
                .windows(2)
                .any(|w| self.freq.word_bigram_count(&w[0], &w[1]) >= 3);
        let pattern_effective = if strong_context {
            pattern_raw / 4
        } else {
            pattern_raw
        };
        crate::engine::GrammarDiagnosis {
            words: words.to_vec(),
            pos,
            pos_known,
            all_known_pos,
            pattern_hit,
            pattern_count,
            pattern_raw,
            pattern_effective,
            window_hits,
            window_total,
            context_yield: strong_context,
            pair_score: self.grammar.path_pos_delta(words),
            triple_score: self.grammar.path_pos_trigram_delta(words),
        }
    }

    /// 2 字词同音拆分变体（我心 → 我/信）。
    ///
    /// DP 常把「我心」当一个整词，路径级同音替换够不到第二个字；这里把
    /// 2 字词拆成两个单字，逐字取同音候选，用口语搭配（prev|a + a|b）
    /// 选最优组合。只对 ≤4 音节、搭配陌生的短链启用；返回
    /// `(word_index, [char0, char1], delta)`。
    pub(crate) fn split_variant(
        &self,
        words: &[String],
        pinyin_parts: &[String],
    ) -> Option<(usize, Vec<String>, i32)> {
        if !self.grammar.has_colloc()
            || !self.context.is_empty()
            || words.len() < 2
            || words.len() > 4
            || pinyin_parts
                .iter()
                .map(|p| p.split_whitespace().count())
                .sum::<usize>()
                > 4
        {
            return None;
        }
        if self.grammar.path_colloc_score(words) >= 16 {
            return None;
        }
        let mut best: Option<(usize, Vec<String>, i32)> = None;
        for i in 0..words.len() {
            let w = &words[i];
            if w.chars().count() != 2 {
                continue;
            }
            // 只拆“生僻/低置信”2 字词（我心 355）；常用词（配合 2659）
            // 不拆，避免把正确整词拆坏。
            if self.dict.freq_for_text(w).unwrap_or(0) > 500 {
                continue;
            }
            // 只拆“人称代词 + 单字”型低置信词（我心/我姓）；其他 2 字词
            // （了结/小德）保持整词，避免拆坏。
            if !matches!(w.chars().next(), Some('我' | '你' | '他' | '她' | '它')) {
                continue;
            }
            let syls: Vec<&str> = pinyin_parts[i].split_whitespace().collect();
            if syls.len() != 2 {
                continue;
            }
            let prev = if i == 0 { "<s>" } else { words[i - 1].as_str() };
            let next = if i + 1 == words.len() {
                "</s>"
            } else {
                words[i + 1].as_str()
            };
            let base_local =
                self.grammar.colloc_count(prev, w) + self.grammar.colloc_count(w, next);
            let mut firsts: Vec<String> = Vec::new();
            for c in self.dict.query(&[syls[0]]).iter().take(12) {
                if c.text.chars().count() == 1
                    && !c.is_sentence
                    && !c.is_guess
                    && !c.text.chars().any(|ch| ch.is_ascii_alphabetic())
                {
                    firsts.push(c.text.to_string());
                }
            }
            let mut seconds: Vec<String> = Vec::new();
            for c in self.dict.query(&[syls[1]]).iter().take(12) {
                if c.text.chars().count() == 1
                    && !c.is_sentence
                    && !c.is_guess
                    && !c.text.chars().any(|ch| ch.is_ascii_alphabetic())
                {
                    seconds.push(c.text.to_string());
                }
            }
            let orig: Vec<char> = w.chars().collect();
            for a in &firsts {
                for b in &seconds {
                    if a.starts_with(orig[0]) && b.starts_with(orig[1]) {
                        continue;
                    }
                    let score = self.grammar.colloc_count(prev, a)
                        + self.grammar.colloc_count(a, b)
                        + self.grammar.colloc_count(b, next);
                    if score < 3 || score <= base_local.saturating_add(2) {
                        continue;
                    }
                    let delta = (score - base_local).min(12) as i32 + 4;
                    if best.as_ref().is_none_or(|(_, _, d)| delta > *d) {
                        best = Some((i, vec![a.clone(), b.clone()], delta));
                    }
                }
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> GrammarModel {
        let mut m = GrammarModel::default();
        for (word, mask, dominant) in [
            ("认真", 4u16, "ad"),
            ("思考", 2, "vn"),
            ("态度", 1, "n"),
            ("用力", 4, "ad"),
            ("推", 2, "v"),
        ] {
            m.insert_pos(word, mask, dominant);
        }
        let mut av = HashMap::new();
        av.insert(
            "v",
            FuncCounts {
                de: 12771,
                di: 3948,
                de2: 163,
            },
        );
        av.insert(
            "n",
            FuncCounts {
                de: 38327,
                di: 333,
                de2: 61,
            },
        );
        m.func_ctx.insert("a", av);
        m.total_de = 554844;
        m.total_di = 15870;
        m.total_de2 = 6537;
        m
    }

    fn words(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn test_pattern_delta_requires_known_pos() {
        let mut m = GrammarModel::default();
        for (word, mask, dominant) in [("我", 16u16, "r"), ("信", 2, "v"), ("你", 16, "r")] {
            m.insert_pos(word, mask, dominant);
        }
        let seq: Vec<u8> = ["r", "v", "r"].iter().map(|c| class_id(c)).collect();
        m.patterns.insert(pattern_key(&seq), 4000);
        m.patterns.insert(
            pattern_key(
                &["n", "n", "n"]
                    .iter()
                    .map(|c| class_id(c))
                    .collect::<Vec<u8>>(),
            ),
            1000,
        );
        m.pattern_totals[3] = 5000;
        m.pattern_kinds[3] = 2;
        assert!(m.pattern_delta(&words(&["我", "信", "你"])) > 0);
        // OOV 回退是猜测，不能叠加句式奖励。
        m.remove_pos("信");
        assert_eq!(m.pattern_delta(&words(&["我", "信", "你"])), 0);
    }

    #[test]
    fn test_long_path_uses_sliding_patterns() {
        let mut m = GrammarModel::default();
        for (word, mask, dominant) in [
            ("我", 16u16, "r"),
            ("信", 2, "v"),
            ("你", 16, "r"),
            ("的", 1024, "u"),
            ("猫", 1, "n"),
            ("很", 8, "d"),
            ("好", 4, "a"),
        ] {
            m.insert_pos(word, mask, dominant);
        }
        let key: Vec<u8> = ["r", "v", "r", "u"].iter().map(|c| class_id(c)).collect();
        m.patterns.insert(pattern_key(&key), 4000);
        m.patterns.insert(
            pattern_key(
                &["n", "n", "n", "n"]
                    .iter()
                    .map(|c| class_id(c))
                    .collect::<Vec<u8>>(),
            ),
            1000,
        );
        m.pattern_totals[4] = 5000;
        m.pattern_kinds[4] = 2;
        let words = words(&["我", "信", "你", "的", "猫", "很", "好"]);
        assert!(m.pattern_delta(&words) > 0, "7 词路径应走滑动模板");
        assert_eq!(m.pattern_window_stats(&words), (1, 9));
        m.remove_pos("猫");
        assert_eq!(m.pattern_delta(&words), 0, "含 OOV 时不叠加模板奖励");
        assert_eq!(m.pattern_window_stats(&words), (0, 0));
    }

    #[test]
    fn test_lift_prefers_di_before_verb() {
        let m = model();
        let di = m.path_delta(&words(&["认真", "地", "思考"]));
        let de = m.path_delta(&words(&["认真", "的", "思考"]));
        assert!(di > de, "地 before verb 应高于的：di={di} de={de}");
        assert_eq!(m.di_variant_index(&words(&["认真", "的", "思考"])), Some(1));
    }

    #[test]
    fn test_lift_prefers_de_before_noun() {
        let m = model();
        let de = m.path_delta(&words(&["认真", "的", "态度"]));
        let di = m.path_delta(&words(&["认真", "地", "态度"]));
        assert!(de > di, "的 before noun 应高于地：de={de} di={di}");
        assert_eq!(m.di_variant_index(&words(&["认真", "的", "态度"])), None);
    }

    #[test]
    fn test_oov_pos_fallback_uses_char_or_suffix() {
        let mut m = GrammarModel::default();
        for (word, mask, dominant) in [("推", 2u16, "v"), ("心", 1, "n")] {
            m.insert_pos(word, mask, dominant);
        }
        // 末字已知 → 末字；末字未知 → 首字；两字都未知 → 构词后缀。
        assert_eq!(m.classes_for("推心").collect::<Vec<_>>(), vec!["n"]);
        assert_eq!(m.classes_for("心化").collect::<Vec<_>>(), vec!["n"]);
        assert_eq!(m.classes_for("囧性").collect::<Vec<_>>(), vec!["n"]);
        assert_eq!(m.classes_for("囧化").collect::<Vec<_>>(), vec!["v"]);
        // 单字 OOV 不硬猜。
        assert!(m.classes_for("囧").next().is_none());
    }

    #[test]
    fn test_pos_table_reloads_after_external_edit() {
        use rusqlite::Connection;

        let db = crate::test_tmp::path("grammar_pos_external_reload.db");
        let _ = std::fs::remove_file(&db);
        {
            let storage = Storage::open(&db).unwrap();
            storage.ensure_grammar_tables().unwrap();
        }
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute(
                "INSERT INTO x1dict_pos(word, pos_mask, pos_dominant, pos_counts, source)
                 VALUES('测试', 1, 'n', 'n:1', 'pfr2014')",
                [],
            )
            .unwrap();
        }
        {
            let storage = Storage::open(&db).unwrap();
            let m = GrammarModel::load(&storage).unwrap();
            assert_eq!(m.coarse_for("测试"), Some("n"));
        }
        // 等价于外部编辑器保存后 x1dict_pos 的生效值（override 元数据由编辑侧维护，
        // 引擎运行期只读生效表）。
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute(
                "UPDATE x1dict_pos SET pos_mask=2, pos_dominant='v', pos_counts='v:1',
                 source='tlv2' WHERE word='测试'",
                [],
            )
            .unwrap();
        }
        {
            // 新启动周期重新加载：不依赖任何持久化词性缓存即可读到新值。
            let storage = Storage::open(&db).unwrap();
            let m = GrammarModel::load(&storage).unwrap();
            assert_eq!(m.coarse_for("测试"), Some("v"));
            assert_eq!(m.pos_mask("测试"), Some(2));
        }
    }

    #[test]
    fn test_compact_pos_table_lookup_upsert_remove() {
        let mut m = GrammarModel::default();
        m.insert_pos("认真", 4, "ad");
        m.insert_pos("态度", 1, "n");
        assert_eq!(m.pos_len(), 2);
        assert_eq!(m.pos_mask("认真"), Some(4));
        assert_eq!(m.coarse_for("认真"), Some("a"));
        assert_eq!(m.coarse_for("态度"), Some("n"));
        assert_eq!(m.pos_mask("不存在"), None);

        // 后写覆盖同词条，保持与旧 HashMap insert 相同的最后写入语义。
        m.insert_pos("认真", 2, "vn");
        assert_eq!(m.pos_len(), 2);
        assert_eq!(m.pos_mask("认真"), Some(2));
        assert_eq!(m.coarse_for("认真"), Some("v"));

        m.remove_pos("认真");
        assert_eq!(m.pos_len(), 1);
        assert_eq!(m.pos_mask("认真"), None);
        assert_eq!(m.coarse_for("态度"), Some("n"));

        // 多词性按 POS_CLASSES 位序展开；mask=0/仅非法高位时回退 dominant。
        m.insert_pos("多性", 1 | 2, "v");
        assert_eq!(m.classes_for("多性").collect::<Vec<_>>(), vec!["n", "v"]);
        m.insert_pos("零位", 0, "v");
        assert_eq!(m.classes_for("零位").collect::<Vec<_>>(), vec!["v"]);
        m.insert_pos("高位", 1 << 13, "a");
        assert_eq!(m.classes_for("高位").collect::<Vec<_>>(), vec!["a"]);
    }

    #[test]
    fn test_empty_model_inactive() {
        let m = GrammarModel::default();
        assert!(!m.is_active());
        assert_eq!(m.path_delta(&words(&["认真", "的", "思考"])), 0);
        assert_eq!(m.di_variant_index(&words(&["认真", "的", "思考"])), None);
    }
}

#[cfg(test)]
mod word_ctx_tests {
    use super::*;

    fn zai_model() -> GrammarModel {
        let mut m = GrammarModel::default();
        m.func_global.insert("在".into(), ("zai".into(), 148246));
        m.func_global.insert("再".into(), ("zai".into(), 7169));
        m.group_totals.insert("zai".into(), 155415);
        m.group_members
            .insert("zai".into(), vec!["在".into(), "再".into()]);
        m.word_ctx.insert(
            "再".into(),
            HashMap::from([(("我".to_string(), "去".to_string()), 5)]),
        );
        m.group_ctx.insert(
            "zai".into(),
            HashMap::from([(("我".to_string(), "去".to_string()), 5)]),
        );
        m
    }

    #[test]
    fn test_word_ctx_prefers_zai() {
        let m = zai_model();
        let zai = m.path_word_ctx_delta(&["我".into(), "在".into(), "去".into()]);
        let zai2 = m.path_word_ctx_delta(&["我".into(), "再".into(), "去".into()]);
        assert!(zai2 > zai, "我再去 应高于 我在去：zai={zai} zai2={zai2}");
        assert_eq!(
            m.word_ctx_variant(&["我".into(), "在".into(), "去".into()]),
            Some((1, "再".to_string()))
        );
    }
}

// ── A/B 对照开关（「模板分需关联佐证」策略）────────────────
// 与 sentence.rs 的 backward_ab/longrange_ab 同款：线程局部（cargo test 并行
// 安全），测试构建可关闭复现「A 之前」的两档行为，生产恒开。
#[cfg(test)]
pub(crate) mod pattern_evidence_ab {
    use std::cell::Cell;
    thread_local! {
        static ENABLED: Cell<bool> = const { Cell::new(true) };
    }
    pub(crate) fn enabled() -> bool {
        ENABLED.with(|c| c.get())
    }
    pub(crate) fn set(on: bool) {
        ENABLED.with(|c| c.set(on));
    }
}
#[cfg(not(test))]
pub(crate) mod pattern_evidence_ab {
    #[inline]
    pub(crate) fn enabled() -> bool {
        true
    }
}

/// 测试钩子：切换模板证据门开关（A/B 对照）。生产构建无此函数。
#[cfg(test)]
pub(crate) fn set_pattern_evidence_for_test(on: bool) {
    pattern_evidence_ab::set(on);
}

#[cfg(test)]
mod pattern_evidence_tests {
    use super::*;

    /// 零关联碎拼的模板分压制（核心守护，quanliyifua 案例）：
    /// 种子副本上「权力|衣服|啊」——词对 bigram 全 0（种子无此搭配）、名词+
    /// 名词+语气词命中高频模板（raw=3）——三档门下应得 raw/4=0；关闭 A/B
    /// （旧两档）时全量 3。同一 Engine 两个状态，只动开关。
    #[test]
    fn zero_link_pattern_score_gated() {
        let db = crate::engine::tests_query::seed_copy("pattern_gate");
        let engine = crate::engine::Engine::new_with_db(&db).expect("seed db");
        let _ = std::fs::remove_file(&db);
        let words: Vec<String> = ["权力", "衣服", "啊"].map(String::from).to_vec();

        // 旧两档（A/B 关）：零关联落在「无强证据」档 → 全量
        set_pattern_evidence_for_test(false);
        let legacy = engine.grammar_pattern_delta(&words);
        // 开（默认）：零关联档 → raw/4
        set_pattern_evidence_for_test(true);
        let gated = engine.grammar_pattern_delta(&words);
        // 幂等恢复（防污染其它测试线程视图——thread_local 各线程独立，同线程恢复）
        assert!(
            gated * 4 <= legacy,
            "零关联碎拼模板分应被压制：legacy={legacy} gated={gated}"
        );
    }
}
