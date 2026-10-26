//! 用户频率模型
//!
//! 设计：
//! - 记录（词，累计使用次数）对
//! - 近期提交使用指数衰减模型（lazy epoch decay）：每次新选择只给当前词
//!   +1 分并推进全局 epoch，不扫全表；读取时按 (当前 epoch - 词条 epoch)
//!   懒算衰减后值。这样"刚选过的词"会快速上升，而久未使用的词会自然下降，
//!   避免某个历史高频词长期霸占首位。
//! - 排序时：score = base_freq * rank_boost / 1000
//!                  + min(total_count, cap) * TOTAL_BONUS
//!                  + recent_score(text) * RECENT_BONUS
//!   其中 rank_boost = 1000 + log2(user_count + 1) * 150
//! - 不学习位置上下文，不学习语法，不做神经网络
//!
//! 存储：
//! - 启动时从 SQLite `user_words` 表加载到内存 HashMap
//! - 用户选词时只更新内存 + 标记 dirty（延迟写入）
//! - 定时器 / 应用退出时 flush 到 SQLite（批量 upsert）
//! - 近期分数是会话级，不持久化，符合"短期"语义

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::string::ToString;

/// prev 位置的语法等价类成员表（单一来源：
/// `equiv_prev` 与成员行聚合查询 `word_bigram_count_generalized` 同表派生，
/// 增删成员只改一处）。
const PRON_MEMBERS: &[&str] = &[
    "我", "你", "他", "她", "它", "我们", "你们", "他们", "她们", "它们", "咱们",
];
/// 程度副词（修饰词前）：很好/非常好/特别好/十分好 泛化。
const DEGREE_MEMBERS: &[&str] = &[
    "很", "非常", "特别", "挺", "蛮", "相当", "十分", "极其", "比较", "最", "更", "太", "好",
];
/// 能愿动词（动词前）：能做/会做/可以做 泛化。
const AUX_MEMBERS: &[&str] = &["能", "会", "可以", "应该", "要", "想", "敢", "肯", "需要"];

/// 等价类 sentinel key → 成员表（非类键返回空表）。
fn equiv_members(class: &str) -> &'static [&'static str] {
    match class {
        "PRON" => PRON_MEMBERS,
        "DEGREE" => DEGREE_MEMBERS,
        "AUX" => AUX_MEMBERS,
        _ => &[],
    }
}

/// prev 位置的语法等价类归一化（语法泛化）。
///
/// 把功能等价的词（代词/程度副词/能愿动词）映射到统一 sentinel key，
/// 让 bigram 举一反三。
///
/// 只归一化 prev（上文位置），text 保留原样。O(成员数) contains，热路径安全
///（单表 ≤13 项）。
fn equiv_prev(s: &str) -> &str {
    if PRON_MEMBERS.contains(&s) {
        "PRON"
    } else if DEGREE_MEMBERS.contains(&s) {
        "DEGREE"
    } else if AUX_MEMBERS.contains(&s) {
        "AUX"
    } else {
        s
    }
}

/// 近期分数衰减系数（固定点：实际值 = stored / RECENT_SCALE）。
///
/// 懒衰减语义下：每次新选择推进 epoch，当前词 +RECENT_SCALE 并打上当前
/// epoch 戳；读取时按 `(17/20)^(now - stored_epoch)` 算衰减后值。衰减系数
/// 0.85 意味着大约 5 次连续选择新词就能让旧词的近期分降到原来的一半以下。
const RECENT_DECAY_NUM: u64 = 17;
const RECENT_DECAY_DEN: u64 = 20;
const RECENT_SCALE: u64 = 1000;

/// 衰减归零阈值。`(17/20)^41 ≈ 0.000713`，固定点下 <1，直接判 0。
/// 读时 delta ≥ 此值即可短路返回 0，避免无意义循环。
const RECENT_DECAY_ZERO_DELTA: u64 = 41;

/// 会话级 bigram 增量上限（学习噪声治理）。
///
/// 同一进程内同一 `(prev,text)` 最多累计 5 次；`Engine::context_clear`
/// （切换应用 / 提交时文档上下文不连续）清零。跨会话继续累积——抑制
/// 「反复测试同一句」在单会话内把搭配计数刷高，同时保留长期学习。
const SESSION_BIGRAM_CAP: u32 = 5;

/// 近期分词条：存原始分数 + 最后一次更新时的 epoch 戳。
///
/// 读时按 `decay(score, current_epoch - epoch)` 算衰减后值（懒衰减），
/// 让 record 从 O(词表规模) 全表扫描降为 O(1)。
#[derive(Clone, Copy, Default)]
struct RecentEntry {
    score: u32,
    epoch: u64,
}

/// bigram CSR 平铺表（内存优化产物；复用为用户 n-gram 基底）。
///
/// 替代嵌套 `BTreeMap<String, BTreeMap<String, u32>>` + totals `BTreeMap`：
/// 20 万条语料从 ~140B/条（两层树节点开销 + String 头 + 堆内容）降到 ~22B/条。
/// 查询语义零变化：O(log) 查找，cache 局部性优于树遍历。
/// 使用方：种子 bigram（`FreqModel::seed_bigram`，整表只读替换）+
/// 用户 n-gram 基底（`NgramStore::base`，见下方 NgramStore 文档）。
///
/// 布局（CSR 压缩稀疏行 + blob 字符串）：
/// - `prev_blob`/`text_blob` 把全部字符串拼接进单一分配，`prev_spans`/
///   `text_spans` 以 (offset, len) 索引——消除 19 万次 Box<str> 独立堆分配
///   （每条 16B 胖指针 + 16B 堆头 + 分配取整余量），估算再省 ~3MB。
/// - blob 布局可直接落盘：启动时优先读磁盘缓存（一次顺序 read + 校验，
///   ~5ms）替代 SQL 全表扫描 + 排序构建（~67ms），见 `to_cache_bytes`/
///   `from_cache_bytes` 与 `Storage::load_seed_bigram_table_cached`。
/// - `prev_spans` 按 prev 字典序（二分查找）；`starts[i]..starts[i+1]` 是该 prev
///   的 text 组区间（末尾哨兵 = 总条数）；`text_spans` 组内按字典序排序，
///   与 `counts` 平行；`totals` 与 `prev_spans` 平行（插值 LM 的分母）。
/// - 构建后不可变（种子表只读不增量，`set_seed_bigram_counts` 整表替换）。
///
/// 构建语义与旧 BTreeMap 版严格一致：
/// - 同 (prev, text) 重复条目：count 后写覆盖（旧 inner.insert）；
///   totals 仍累加全部条目（旧 `entry().or_insert(0) += count`）。
/// 种子 bigram 磁盘缓存魔数+版本（布局变更时 bump，旧缓存校验失败自动重建）。
const SEED_BIGRAM_CACHE_MAGIC: &[u8; 8] = b"X1SBC001";
/// 头部长度：magic(8) + 内容哈希(16 ASCII) + 4×u32 计数
const SEED_BIGRAM_CACHE_HEADER_LEN: usize = 8 + 16 + 16;

/// 缓存哈希定长化（x1dict_content_hash 恒为 16 字符 hex；防御性补 '0'/截断）。
fn norm_cache_hash16(h: &str) -> [u8; 16] {
    let mut out = [b'0'; 16];
    let hb = h.as_bytes();
    let n = hb.len().min(16);
    out[..n].copy_from_slice(&hb[..n]);
    out
}

pub(crate) struct BigramCsrTable {
    prev_blob: Box<str>,
    /// (offset, len) 索引 prev_blob，按 prev 字典序（二分查找前提）
    prev_spans: Vec<(u32, u32)>,
    starts: Vec<u32>,
    text_blob: Box<str>,
    /// (offset, len) 索引 text_blob，与 counts 平行；组内按 text 字典序
    text_spans: Vec<(u32, u32)>,
    counts: Vec<u32>,
    totals: Vec<u64>,
    /// 反向边际（双向词格）：text → Σcount（该词作为「后词」的
    /// 入度总量），(offset, len, total) 按 text 字节序的二分索引，span 引用
    /// text_blob。纯派生数据——不进磁盘缓存序列化，from_entries /
    /// from_cache_bytes 两条构建路径统一在末尾重算（缓存格式 X1SBC001 不变）。
    /// 消费方：`next_total` = 后向条件概率 P(left|right) 的归一分母。
    next_totals: Vec<(u32, u32, u64)>,
}

impl BigramCsrTable {
    fn new() -> Self {
        Self {
            prev_blob: Box::from(""),
            prev_spans: Vec::new(),
            starts: Vec::new(),
            text_blob: Box::from(""),
            text_spans: Vec::new(),
            counts: Vec::new(),
            totals: Vec::new(),
            next_totals: Vec::new(),
        }
    }

    /// 取第 i 个 prev 的 &str。span 的界内性与字符边界由构造方保证
    /// （from_entries 来自合法 &str；from_cache_bytes 全量校验后才放行）。
    #[inline]
    fn prev_at(&self, i: usize) -> &str {
        let (o, l) = self.prev_spans[i];
        &self.prev_blob[o as usize..(o + l) as usize]
    }

    /// 取第 i 个 text 的 &str（安全性同 prev_at）。
    #[inline]
    fn text_at(&self, i: usize) -> &str {
        let (o, l) = self.text_spans[i];
        &self.text_blob[o as usize..(o + l) as usize]
    }

    /// 从 (prev, text, count) 迭代器构建 CSR。
    ///
    /// 精确预分配（碎片治理）：blob/平行数组先按两遍扫描的
    /// 精确总长 with_capacity，构建期零 realloc——旧版的摊倍增会让旧缓冲
    /// （可达最终 blob 的 2 倍）在构建中途 free，与存活分配交错成不可归还的
    /// 碎片（HeapCompact 无效的那种）。第二遍跳过重复条目后容量略超实际
    /// （重复条目在 74k/269k 规模 ≤ 个位数），误差远小于一次翻倍。
    pub(crate) fn from_entries<I: IntoIterator<Item = (String, String, u32)>>(entries: I) -> Self {
        let mut rows: Vec<(String, String, u32)> = entries.into_iter().collect();
        // 稳定排序（同 (prev, text) 重复条目保持输入顺序相邻，供后写覆盖判定）
        rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));

        // 第一遍（排序后）：精确容量——组数 = distinct prev、条目数 = 去重后的
        // (prev, text)、blob 字节 = 各自的去重累计。与第二遍的写入逻辑同构，
        // 容量分毫不差（重复条目两遍都跳过）。
        let mut prev_bytes = 0usize;
        let mut text_bytes = 0usize;
        let mut n_texts = 0usize;
        let mut n_prevs = 0usize;
        {
            let mut last: Option<(&str, &str)> = None;
            let mut current_prev: Option<&str> = None;
            for (prev, text, _) in &rows {
                let pk = prev.as_str();
                if current_prev != Some(pk) {
                    n_prevs += 1;
                    prev_bytes += pk.len();
                    current_prev = Some(pk);
                }
                let key = (pk, text.as_str());
                if last != Some(key) {
                    text_bytes += text.len();
                    n_texts += 1;
                    last = Some(key);
                }
                // 重复条目：两遍都跳过（容量与写入分毫不差）
            }
        }
        let mut t = Self::new();
        t.prev_spans = Vec::with_capacity(n_prevs);
        t.starts = Vec::with_capacity(n_prevs + 1);
        t.text_spans = Vec::with_capacity(n_texts);
        t.counts = Vec::with_capacity(n_texts);
        t.totals = Vec::with_capacity(n_prevs);
        let mut prev_blob = String::with_capacity(prev_bytes);
        let mut text_blob = String::with_capacity(text_bytes);

        let mut i = 0;
        while i < rows.len() {
            // 一个 prev 分组（rows 已按 (prev, text) 排序，组内 text 相邻）
            let group_start = i;
            while i < rows.len() && rows[i].0 == rows[group_start].0 {
                i += 1;
            }
            let group = &rows[group_start..i];

            let poff = prev_blob.len() as u32;
            prev_blob.push_str(&group[0].0);
            t.prev_spans.push((poff, group[0].0.len() as u32));
            t.starts.push(t.text_spans.len() as u32);
            // totals 累加组内全部条目（含即将被覆盖的重复条目，旧版 += 语义）
            let mut total: u64 = 0;
            // 同 (prev, text) 相邻重复：保留最后一条（旧 inner.insert 后写覆盖）
            let mut prev_text: Option<&str> = None;
            for (_, text, count) in group {
                total += *count as u64;
                let ts = text.as_str();
                if prev_text == Some(ts) {
                    *t.counts.last_mut().unwrap() = *count;
                } else {
                    let toff = text_blob.len() as u32;
                    text_blob.push_str(ts);
                    t.text_spans.push((toff, ts.len() as u32));
                    t.counts.push(*count);
                    prev_text = Some(ts);
                }
            }
            t.totals.push(total);
        }
        t.starts.push(t.text_spans.len() as u32); // 末尾哨兵
        t.prev_blob = prev_blob.into_boxed_str();
        t.text_blob = text_blob.into_boxed_str();
        t.build_next_totals();
        t
    }

    /// 从 (text_spans, counts) 派生反向边际索引（双向词格）。
    ///
    /// 一次性 O(n log n)，两条构建路径（from_entries / from_cache_bytes）末尾
    /// 统一调用。同一 text 跨多个 prev 组出现时合并累加（如「的」作为大量
    /// 不同前词的后词——这正是反向边际的判别力来源：小而集中的分母）。
    fn build_next_totals(&mut self) {
        let mut raw: Vec<(u32, u32, u64)> = self
            .text_spans
            .iter()
            .zip(self.counts.iter())
            .map(|(&(o, l), &c)| (o, l, c as u64))
            .collect();
        raw.sort_unstable_by(|a, b| {
            self.text_blob[a.0 as usize..(a.0 + a.1) as usize]
                .cmp(&self.text_blob[b.0 as usize..(b.0 + b.1) as usize])
        });
        let mut out: Vec<(u32, u32, u64)> = Vec::with_capacity(raw.len());
        for (o, l, c) in raw {
            let same_text = out.last().is_some_and(|&(po, pl, _)| {
                self.text_blob[po as usize..(po + pl) as usize]
                    == self.text_blob[o as usize..(o + l) as usize]
            });
            if same_text {
                out.last_mut().unwrap().2 += c;
            } else {
                out.push((o, l, c));
            }
        }
        self.next_totals = out;
    }

    /// text 作为「后词」的入度总量 Σcount；不在表 → 0。
    /// 后向条件概率分母：P(left|right) = count(left,right) / next_total(right)。
    pub(crate) fn next_total(&self, text: &str) -> u64 {
        let idx = self
            .next_totals
            .partition_point(|&(o, l, _)| &self.text_blob[o as usize..(o + l) as usize] < text);
        match self.next_totals.get(idx) {
            Some(&(o, l, t)) if &self.text_blob[o as usize..(o + l) as usize] == text => t,
            _ => 0,
        }
    }

    /// 精确查询 (prev, text)，未命中返回 None。
    pub(crate) fn get(&self, prev: &str, text: &str) -> Option<u32> {
        self.get_and_total(prev, text).map(|(n, _)| n)
    }

    /// 精确查询 + 匹配 prev 桶的出度总次数（LM 的分子分母一次取齐）。
    pub(crate) fn get_and_total(&self, prev: &str, text: &str) -> Option<(u32, u64)> {
        let idx = self
            .prev_spans
            .binary_search_by(|&(o, l)| self.prev_blob[o as usize..(o + l) as usize].cmp(prev))
            .ok()?;
        let (start, end) = (self.starts[idx] as usize, self.starts[idx + 1] as usize);
        let pos = self.text_spans[start..end]
            .binary_search_by(|&(o, l)| self.text_blob[o as usize..(o + l) as usize].cmp(text))
            .ok()?;
        Some((self.counts[start + pos], self.totals[idx]))
    }

    /// prev 桶的出度总次数（未命中 0，诊断/测试用）。
    #[cfg(test)]
    pub(crate) fn total(&self, prev: &str) -> u64 {
        self.prev_spans
            .binary_search_by(|&(o, l)| self.prev_blob[o as usize..(o + l) as usize].cmp(prev))
            .ok()
            .map(|i| self.totals[i])
            .unwrap_or(0)
    }

    /// 种子搭配条目总数（运行信息展示用；与 test 版 len 同源）。
    pub(crate) fn rows(&self) -> usize {
        self.text_spans.len()
    }

    /// 条目总数（诊断用）。
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.text_spans.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.text_spans.is_empty()
    }

    /// prev 分组数（诊断用）。
    #[cfg(test)]
    pub(crate) fn prev_len(&self) -> usize {
        self.prev_spans.len()
    }

    /// 扁平化全部条目 (outer, text, count)，组序 = outer 字典序、组内 text 序
    /// （诊断/全量导出；同旧嵌套 BTreeMap 的遍历序）。
    pub(crate) fn entries_flat(&self) -> Vec<(String, String, u32)> {
        let mut out = Vec::with_capacity(self.text_spans.len());
        for g in 0..self.prev_spans.len() {
            let prev = self.prev_at(g).to_string();
            for i in self.starts[g] as usize..self.starts[g + 1] as usize {
                out.push((prev.clone(), self.text_at(i).to_string(), self.counts[i]));
            }
        }
        out
    }

    /// 含某 text（作为组内键）的全部 outer 键（forget 扫描用，罕见路径；
    /// 组内已按 text 排序 → 二分）。
    pub(crate) fn outers_containing(&self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for g in 0..self.prev_spans.len() {
            let (start, end) = (self.starts[g] as usize, self.starts[g + 1] as usize);
            if self.text_spans[start..end]
                .binary_search_by(|&(o, l)| self.text_blob[o as usize..(o + l) as usize].cmp(text))
                .is_ok()
            {
                out.push(self.prev_at(g).to_string());
            }
        }
        out
    }

    /// 无分配全表遍历 (outer, text, count)，组序 = outer 字典序。
    pub(crate) fn for_each_entry(&self, mut f: impl FnMut(&str, &str, u32)) {
        for g in 0..self.prev_spans.len() {
            let prev = self.prev_at(g);
            for i in self.starts[g] as usize..self.starts[g + 1] as usize {
                f(prev, self.text_at(i), self.counts[i]);
            }
        }
    }

    /// 某 outer 组的全部 (text, count)（forget 整桶删除用；组不存在返回空）。
    pub(crate) fn group_entries(&self, outer: &str) -> Vec<(String, u32)> {
        let idx = match self
            .prev_spans
            .binary_search_by(|&(o, l)| self.prev_blob[o as usize..(o + l) as usize].cmp(outer))
        {
            Ok(i) => i,
            Err(_) => return Vec::new(),
        };
        (self.starts[idx] as usize..self.starts[idx + 1] as usize)
            .map(|i| (self.text_at(i).to_string(), self.counts[i]))
            .collect()
    }

    /// 估算内存占用（字节，诊断/内存 profile 用）：
    /// blob 内容 + span 8B/条 + 平行数组。
    #[cfg(test)]
    pub(crate) fn estimated_bytes(&self) -> usize {
        self.prev_blob.len()
            + self.prev_spans.len() * 8
            + self.text_blob.len()
            + self.text_spans.len() * 8
            + self.starts.len() * 4
            + self.counts.len() * 4
            + self.totals.len() * 8
            + self.next_totals.len() * 16
    }

    /// 序列化为磁盘缓存字节（布局同内存 blob 排布，LE 编码）。
    ///
    /// `content_hash` = 用户 DB 的 `x1dict_sync_hash`（16 字符 hex），写入头部
    /// 作新鲜度键：种子表更新必经 sync（表只读），sync 同步更新 hash →
    /// 旧缓存自动失效。格式校验见 from_cache_bytes。
    pub(crate) fn to_cache_bytes(&self, content_hash: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            SEED_BIGRAM_CACHE_HEADER_LEN
                + self.prev_spans.len() * 8
                + self.starts.len() * 4
                + self.totals.len() * 8
                + self.prev_blob.len()
                + self.text_spans.len() * 8
                + self.counts.len() * 4
                + self.text_blob.len(),
        );
        out.extend_from_slice(SEED_BIGRAM_CACHE_MAGIC);
        out.extend_from_slice(&norm_cache_hash16(content_hash));
        // 自由 fn 而非闭包：避免闭包持有 out 的可变借用与后续直接 extend 冲突
        fn w32(out: &mut Vec<u8>, v: u32) {
            out.extend_from_slice(&v.to_le_bytes());
        }
        w32(&mut out, self.prev_spans.len() as u32);
        w32(&mut out, self.text_spans.len() as u32);
        w32(&mut out, self.prev_blob.len() as u32);
        w32(&mut out, self.text_blob.len() as u32);
        for &(o, l) in &self.prev_spans {
            w32(&mut out, o);
            w32(&mut out, l);
        }
        for &s in &self.starts {
            w32(&mut out, s);
        }
        for &t in &self.totals {
            out.extend_from_slice(&t.to_le_bytes());
        }
        out.extend_from_slice(self.prev_blob.as_bytes());
        for &(o, l) in &self.text_spans {
            w32(&mut out, o);
            w32(&mut out, l);
        }
        for &c in &self.counts {
            w32(&mut out, c);
        }
        out.extend_from_slice(self.text_blob.as_bytes());
        out
    }

    /// 从磁盘缓存字节反序列化；任何不一致返回 None（调用方回退 SQL 重建
    /// 并重写缓存，自愈）。校验项（全部 O(n) 整数/边界比较，~1-2ms）：
    /// 魔数+哈希匹配、总长度与四计数精确一致、blob 整体合法 UTF-8、
    /// 每个 span 界内且落在字符边界、prev 严格升序（二分查找前提）、
    /// starts 单调且末值 = text 数、组内 text 严格升序。
    /// 校验通过后查询路径的 blob 直接索引不会 panic。
    pub(crate) fn from_cache_bytes(bytes: &[u8], content_hash: &str) -> Option<Self> {
        fn rd_u32(b: &[u8], off: &mut usize) -> Option<u32> {
            let s = b.get(*off..*off + 4)?;
            *off += 4;
            Some(u32::from_le_bytes(s.try_into().ok()?))
        }
        if bytes.len() < SEED_BIGRAM_CACHE_HEADER_LEN {
            return None;
        }
        if bytes[..8] != SEED_BIGRAM_CACHE_MAGIC[..] {
            return None;
        }
        if bytes[8..24] != norm_cache_hash16(content_hash)[..] {
            return None;
        }
        let mut off = 24;
        let n_prevs = rd_u32(bytes, &mut off)? as usize;
        let n_texts = rd_u32(bytes, &mut off)? as usize;
        let prev_blob_len = rd_u32(bytes, &mut off)? as usize;
        let text_blob_len = rd_u32(bytes, &mut off)? as usize;
        // 总长度必须精确一致（截断/追加/字段错位都拒收）
        let expected = SEED_BIGRAM_CACHE_HEADER_LEN
            .checked_add(n_prevs.checked_mul(8)?)?
            .checked_add((n_prevs.checked_add(1)?).checked_mul(4)?)?
            .checked_add(n_prevs.checked_mul(8)?)?
            .checked_add(prev_blob_len)?
            .checked_add(n_texts.checked_mul(8)?)?
            .checked_add(n_texts.checked_mul(4)?)?
            .checked_add(text_blob_len)?;
        if bytes.len() != expected {
            return None;
        }

        let mut prev_spans = Vec::with_capacity(n_prevs);
        for _ in 0..n_prevs {
            prev_spans.push((rd_u32(bytes, &mut off)?, rd_u32(bytes, &mut off)?));
        }
        let mut starts = Vec::with_capacity(n_prevs + 1);
        for _ in 0..=n_prevs {
            starts.push(rd_u32(bytes, &mut off)?);
        }
        let mut totals = Vec::with_capacity(n_prevs);
        for _ in 0..n_prevs {
            let s = bytes.get(off..off + 8)?;
            off += 8;
            totals.push(u64::from_le_bytes(s.try_into().ok()?));
        }
        let prev_blob_bytes = bytes.get(off..off + prev_blob_len)?;
        off += prev_blob_len;
        let mut text_spans = Vec::with_capacity(n_texts);
        for _ in 0..n_texts {
            text_spans.push((rd_u32(bytes, &mut off)?, rd_u32(bytes, &mut off)?));
        }
        let mut counts = Vec::with_capacity(n_texts);
        for _ in 0..n_texts {
            counts.push(rd_u32(bytes, &mut off)?);
        }
        let text_blob_bytes = bytes.get(off..off + text_blob_len)?;

        // blob 整体 UTF-8 校验（to_vec 一次拷贝成自有缓冲）
        let prev_blob = String::from_utf8(prev_blob_bytes.to_vec())
            .ok()?
            .into_boxed_str();
        let text_blob = String::from_utf8(text_blob_bytes.to_vec())
            .ok()?
            .into_boxed_str();

        let mut t = Self {
            prev_blob,
            prev_spans,
            starts,
            text_blob,
            text_spans,
            counts,
            totals,
            next_totals: Vec::new(),
        };
        // span 界内 + 字符边界（is_char_boundary 对 str 是 O(1)）
        let span_ok = |blob: &str, o: u32, l: u32| {
            let (o, l) = (o as usize, l as usize);
            o.checked_add(l).filter(|&e| e <= blob.len()).is_some()
                && blob.is_char_boundary(o)
                && blob.is_char_boundary(o + l)
        };
        for &(o, l) in &t.prev_spans {
            if !span_ok(&t.prev_blob, o, l) {
                return None;
            }
        }
        for &(o, l) in &t.text_spans {
            if !span_ok(&t.text_blob, o, l) {
                return None;
            }
        }
        // prev 严格升序（二分查找正确性前提）
        for w in 1..t.prev_spans.len() {
            if t.prev_at(w - 1) >= t.prev_at(w) {
                return None;
            }
        }
        // starts：len=n_prevs+1、首 0、单调不减、末值 = text 数
        if t.starts.len() != n_prevs + 1
            || t.starts.first().copied() != Some(0)
            || t.starts.last().copied() != Some(n_texts as u32)
            || t.starts.windows(2).any(|w| w[0] > w[1])
        {
            return None;
        }
        // 组内 text 严格升序（组内二分查找前提）
        for g in 0..n_prevs {
            let (s, e) = (t.starts[g] as usize, t.starts[g + 1] as usize);
            for j in s + 1..e {
                if t.text_at(j - 1) >= t.text_at(j) {
                    return None;
                }
            }
        }
        // 反向边际是派生数据（不进序列化），两条构建路径在此统一重算。
        t.build_next_totals();
        Some(t)
    }
}

impl Default for BigramCsrTable {
    fn default() -> Self {
        Self::new()
    }
}

/// 用户 n-gram 存储：CSR 不可变基底 + 可变 overlay。
///
/// 替代用户 bigram/trigram 的嵌套 `BTreeMap<String, BTreeMap<String, u32>>`
/// （实测 74k bigram + 68k trigram = 33.3MB ≈ 235B/条；CSR ~22B/条 + overlay
/// 趋零，预计省 ~27MB；由内存诊断测试测得）。
///
/// 语义（与旧嵌套 BTreeMap 严格等价，守护测试 `ngram_store_equivalence`）：
/// - `base`：启动时从 DB 快照构建（`from_entries`），会话内不可变——
///   运行中绝不重建（142k 条重建要几十 ms，服务单线程会阻塞 Commit）；
///   重启自然重整（overlay 归零，DB 是唯一持久层）。
/// - `overlay`：会话内全部增量/删除，键值即当前权威值：
///   - 值 ≥ 1：正常条目（遮蔽 base 同键旧值——record 的读改写以
///     base∪overlay 为当前值）；
///   - 值 = 0：墓碑（该键已删，遮蔽 base 旧值；活值恒 ≥1，
///     record 只加不减，0 不可能是正常计数——set_counts 折叠后亦然）。
/// - dirty 增量落盘（`bigram_dirty`/`dirty_entries`）与本结构
///   正交：dirty 记录"自上次 flush 的变化"（flush 后清空），overlay 记录
///   "自加载的全部变化"（持久遮蔽 base）——生命周期不同，两份并存，
///   flush 逻辑零改动。
///
/// 外层键由调用方给出（bigram = equiv_prev 归一化后的 prev；trigram =
/// "prev2\NULprev1" 拼接），本结构不做键变换。
pub(crate) struct NgramStore {
    base: BigramCsrTable,
    overlay: BTreeMap<String, BTreeMap<String, u32>>,
}

impl NgramStore {
    pub(crate) fn from_entries<I: IntoIterator<Item = (String, String, u32)>>(entries: I) -> Self {
        Self {
            base: BigramCsrTable::from_entries(entries),
            overlay: BTreeMap::new(),
        }
    }

    pub(crate) fn new() -> Self {
        Self {
            base: BigramCsrTable::new(),
            overlay: BTreeMap::new(),
        }
    }

    /// 读当前值（overlay 优先，墓碑=已删 → None）。语义同旧
    /// `counts.get(prev)?.get(text).copied()`（entry 不存在 = None）。
    #[inline]
    pub(crate) fn get(&self, outer: &str, text: &str) -> Option<u32> {
        match self.overlay.get(outer).and_then(|m| m.get(text)) {
            Some(&v) if v > 0 => Some(v),
            Some(_) => None, // 墓碑：已删，不落回 base
            None => self.base.get(outer, text),
        }
    }

    /// 读改写：当前值（墓碑按 0 算——被删键重新记录从 0 起算，与旧
    /// BTreeMap remove 后重插入一致，绝不落回 base 被遮蔽的旧值）+ weight，
    /// 写回 overlay，返回新值。
    #[inline]
    pub(crate) fn record(&mut self, outer: &str, text: &str, weight: u32) -> u32 {
        let cur = match self.overlay.get(outer).and_then(|m| m.get(text)) {
            Some(&v) => v, // overlay 存在即权威（活值或墓碑=0）
            None => self.base.get(outer, text).unwrap_or(0),
        };
        let next = cur + weight;
        self.overlay
            .entry(outer.to_string())
            .or_default()
            .insert(text.to_string(), next);
        next
    }

    /// 减值（纠错撤销）：当前值 - weight，下限 0；0 写墓碑
    /// （遮蔽 base 旧值）。返回新值（0 = 已删）。读改写语义同 `record`。
    pub(crate) fn decrement(&mut self, outer: &str, text: &str, weight: u32) -> u32 {
        let cur = match self.overlay.get(outer).and_then(|m| m.get(text)) {
            Some(&v) => v,
            None => self.base.get(outer, text).unwrap_or(0),
        };
        let next = cur.saturating_sub(weight);
        self.overlay
            .entry(outer.to_string())
            .or_default()
            .insert(text.to_string(), next);
        next
    }

    /// 删除单个 `(outer,text)`（pair 级清理/审计用）：写墓碑遮蔽 base。
    pub(crate) fn forget_pair(&mut self, outer: &str, text: &str) {
        self.overlay
            .entry(outer.to_string())
            .or_default()
            .insert(text.to_string(), 0);
    }

    /// 删除某 text 参与的全部条目（作为组内 text + 作为 outer 整桶），
    /// 返回被删的 (outer, text) 键集（调用方转 dirty deletes）。
    /// 语义同旧 forget_bigram 的两层扫描 + 空桶清理（墓碑天然覆盖"空桶"语义，
    /// base 组不物理删除——罕见路径，内存代价可忽略）。
    pub(crate) fn forget_text(&mut self, text: &str) -> Vec<(String, String)> {
        let mut deleted: Vec<(String, String)> = Vec::new();
        // base：作为组内 text（组内已排序 → 各组二分）
        for prev in self.base.outers_containing(text) {
            if self.get(&prev, text).is_some() {
                // 未被墓碑遮蔽的才算"当前存在"
                self.overlay
                    .entry(prev.clone())
                    .or_default()
                    .insert(text.to_string(), 0);
                deleted.push((prev, text.to_string()));
            }
        }
        // base：作为 outer 整桶
        for (t, _) in self.base.group_entries(text) {
            if self.get(text, &t).is_some() {
                self.overlay
                    .entry(text.to_string())
                    .or_default()
                    .insert(t.clone(), 0);
                deleted.push((text.to_string(), t));
            }
        }
        // overlay：活值条目里含 text 的（base 之外的会话内新增）
        let mut overlay_deleted: Vec<(String, String)> = Vec::new();
        for (outer, inner) in self.overlay.iter_mut() {
            if let Some(&v) = inner.get(text) {
                if v > 0 {
                    overlay_deleted.push((outer.clone(), text.to_string()));
                }
            }
        }
        for (outer, t) in overlay_deleted.drain(..) {
            self.overlay.get_mut(&outer).unwrap().insert(t.clone(), 0);
            deleted.push((outer, t));
        }
        // overlay：作为 outer 的活值整桶
        if let Some(bucket) = self.overlay.get(text).cloned() {
            for (t, v) in bucket {
                if v > 0 {
                    deleted.push((text.to_string(), t.clone()));
                }
            }
        }
        if let Some(bucket) = self.overlay.get_mut(text) {
            for v in bucket.values_mut() {
                *v = 0; // 整桶墓碑化
            }
        }
        deleted
    }

    /// 全表最高 count（max_bigram_count 用；有效条目口径——被 overlay 遮蔽/
    /// 墓碑的 base 条目不计，同旧实现的"删了就不在表里"语义。O(base+overlay)，
    /// 后台 60s 周期调用，不在热路径）。
    pub(crate) fn max_count(&self) -> u32 {
        let NgramStore { base, overlay } = self;
        let mut max = 0u32;
        base.for_each_entry(|o, t, c| {
            let shadowed = overlay.get(o).map(|m| m.contains_key(t)).unwrap_or(false);
            if !shadowed && c > max {
                max = c;
            }
        });
        for m in overlay.values() {
            for &v in m.values() {
                if v > max {
                    max = v;
                }
            }
        }
        max
    }

    /// 扁平快照（诊断/全量重建工具；= base 有效条目 ∪ overlay 活值条目，
    /// 墓碑与被遮蔽的 base 旧值都不出现——同旧嵌套 BTreeMap 的遍历内容）。
    pub(crate) fn entries_flat(&self) -> Vec<(String, String, u32)> {
        let mut out: Vec<(String, String, u32)> = self
            .base
            .entries_flat()
            .into_iter()
            .filter(|(outer, text, _)| {
                // 被 overlay 遮蔽（含墓碑）的 base 条目以 overlay 为准
                !self
                    .overlay
                    .get(outer)
                    .map(|m| m.contains_key(text))
                    .unwrap_or(false)
            })
            .collect();
        for (outer, inner) in &self.overlay {
            for (text, &v) in inner {
                if v > 0 {
                    out.push((outer.clone(), text.clone(), v));
                }
            }
        }
        out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        out
    }

    /// 诊断统计：(外层组数, 内层条目总数)，语义同旧
    /// `map.len()` / `values().map(len).sum()`（有效条目口径，墓碑不计）。
    pub(crate) fn len_stats(&self) -> (usize, usize) {
        let mut outer_set = std::collections::BTreeSet::new();
        let mut inner_total = 0usize;
        for (outer, text) in self.base.entries_flat().into_iter().map(|(o, t, _)| (o, t)) {
            if !self
                .overlay
                .get(&outer)
                .map(|m| m.contains_key(&text))
                .unwrap_or(false)
            {
                outer_set.insert(outer);
                inner_total += 1;
            }
        }
        for (outer, inner) in &self.overlay {
            let live = inner.values().filter(|&&v| v > 0).count();
            if live > 0 {
                outer_set.insert(outer.clone());
                inner_total += live;
            }
        }
        (outer_set.len(), inner_total)
    }
}

/// 用户频率数据库（纯内存模型）
///
/// 持久化由 Engine::flush() 统一负责，避免在热路径中直接访问 SQLite。
/// 近期分数是会话级，不持久化。
pub struct FreqModel {
    /// 词 → 累计使用次数
    pub(crate) counts: BTreeMap<String, u32>,
    /// 词 → 近期提交分数（固定点，RECENT_SCALE 代表 1.0）+ 最后更新 epoch
    ///
    /// lazy epoch decay：record 只推进 epoch + 更新当前词条目，不扫全表；
    /// 读 recent_score 时按 `decay(score, current_epoch - epoch)` 懒算。
    recent_scores: BTreeMap<String, RecentEntry>,
    /// (词, 拼音) → 累计选择次数（多音字主音/次音动态权重用）
    ///
    /// 与 `counts` 平行存在，但按 (text, pinyin) 维度统计，避免跨音串扰。
    /// 例如用户在 de 查询下选「地」，只累加 (地, de)，不影响 di 查询的「地」。
    /// 单字候选的排序完全使用按音隔离的频率数据（见 candidate_score），
    /// 让用户在某音下的选择只影响该音的排序。
    pub(crate) pinyin_counts: BTreeMap<(String, String), u32>,
    /// (词, 拼音) → 近期提交分数（固定点，按音隔离）+ 最后更新 epoch
    ///
    /// 与 `recent_scores` 平行，但按 (text, pinyin) 维度衰减，且有独立的
    /// `pinyin_epoch` 计数——record(text) 衰减词级，record_with_pinyin
    /// 衰减拼音级，两条互不干扰。单字排序用此值，避免 di 选「地」让 de
    /// 的「地」获得近期分。
    pinyin_recent_scores: BTreeMap<(String, String), RecentEntry>,
    /// 词级 bigram（词×词，用户积累 + equiv_prev 泛化）。
    ///
    /// CSR 基底 + overlay（原嵌套 BTreeMap 74k 条 33.3MB
    /// → ~2MB，语义等价守护测试 `ngram_store_equivalence`）：查询 O(log)，
    /// 详见 `NgramStore` 文档。持久化到 user_bigram_counts 表（dirty 增量）；
    /// 上下文栈本身不持久化（会话级）。
    pub(crate) user_bigram: NgramStore,
    /// 种子 bigram（语料预填，冷启动兜底，只读不增量）。
    ///
    /// 双表方案：与 user_bigram 分离，查询时用户优先。
    /// 用户有 (prev, text) commit 记录 → 完全用用户的（不看种子）；
    /// 用户没有 → 才查种子表（冷启动兜底）。种子 count 不与用户 count 累加。
    ///
    /// CSR 平铺表：替代嵌套 BTreeMap + totals
    /// BTreeMap 两份树结构（20 万条 ~28MB → ~11MB），查询语义零变化。
    pub(crate) seed_bigram: BigramCsrTable,
    /// 词级近期分的全局 epoch 计数器（record 推进）
    current_epoch: u64,
    /// 拼音级近期分的全局 epoch 计数器（record_with_pinyin 推进，独立计数）
    pinyin_epoch: u64,
    /// 自上次成功落盘以来变化的 bigram（值 = 当前最新值）。
    ///
    /// dirty 追踪而非全量快照：全量快照方案的
    /// `delta()` 要遍历 7 万条 + BTreeMap 查找 = 49ms/次，加载时建快照还要
    /// 深拷贝 79ms（拖慢启动，这是引入增量机制的原因）。
    /// 改为在写入点记录变化：落盘 O(变化量)（典型几十条 <1ms），加载零额外
    /// 成本，且不占冗余内存（不存全量副本）。
    bigram_dirty: BTreeMap<String, BTreeMap<String, u32>>,
    /// 自上次落盘以来被删除的 (prev, text)（forget_bigram 语义）。
    bigram_dirty_deletes: alloc::collections::BTreeSet<(String, String)>,
    /// 本会话内每个 `(prev,text)` 已写入的增量（上限 SESSION_BIGRAM_CAP）。
    /// `Engine::context_clear` 清空（切换应用/上下文不连续 = 新会话段）。
    session_bigram_inc: BTreeMap<(String, String), u32>,
    /// 是否脏（需要保存）
    dirty: bool,
}

impl FreqModel {
    pub fn new() -> Self {
        Self {
            counts: BTreeMap::new(),
            recent_scores: BTreeMap::new(),
            pinyin_counts: BTreeMap::new(),
            pinyin_recent_scores: BTreeMap::new(),
            user_bigram: NgramStore::new(),
            seed_bigram: BigramCsrTable::new(),
            current_epoch: 0,
            pinyin_epoch: 0,
            bigram_dirty: BTreeMap::new(),
            bigram_dirty_deletes: alloc::collections::BTreeSet::new(),
            session_bigram_inc: BTreeMap::new(),
            dirty: false,
        }
    }

    /// 懒衰减核心：算 `stored_score × (DECAY_NUM/DECAY_DEN)^delta`（固定点）。
    ///
    /// delta=0 返回原值；delta≥RECENT_DECAY_ZERO_DELTA 直接判 0（衰减殆尽）；
    /// 中间值循环乘除。读路径调用，O(delta) 但 delta 通常 <40。
    fn decay_score(stored: u32, delta: u64) -> u32 {
        if delta == 0 {
            return stored;
        }
        if delta >= RECENT_DECAY_ZERO_DELTA {
            return 0;
        }
        let mut score = stored as u64;
        for _ in 0..delta {
            score = score * RECENT_DECAY_NUM / RECENT_DECAY_DEN;
        }
        score as u32
    }

    /// 从文本内容加载（兼容旧 FFI 接口 `x1_engine_load_user_dict`）
    ///
    /// 格式：每行 `text\tcount`，count 为十进制整数。
    pub fn load_from_text(&mut self, text: &str) {
        self.counts.clear();
        self.recent_scores.clear();
        for line in text.lines() {
            let mut parts = line.split('\t');
            let t = parts.next().unwrap_or("").trim();
            let count = parts
                .next()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0);
            if !t.is_empty() && count > 0 {
                self.counts.insert(t.to_string(), count);
            }
        }
        self.dirty = false;
    }

    /// 导出为文本内容（兼容旧 FFI 接口 `x1_engine_save_user_dict`）
    pub fn save_to_text(&self) -> String {
        let mut content = String::new();
        for (text, count) in &self.counts {
            content.push_str(text);
            content.push('\t');
            content.push_str(&format!("{}\n", count));
        }
        content
    }

    /// 记录一次选择（次数 +1），同时以指数衰减更新近期分数。
    ///
    /// lazy epoch decay 实现：只推进全局 epoch + 更新当前词条目，不扫全表。
    /// 其他词的衰减在读 `recent_score` 时按 `(now_epoch - stored_epoch)` 懒算。
    /// 复杂度 O(1)（旧版全表扫描 O(词表规模)）。
    pub fn record(&mut self, text: &str) {
        // saturating_add：外部灌入 u32::MAX 后再
        // +1 普通加法回绕 0（计数清零比饱和停驻更糟）。
        let c = self.counts.entry(text.to_string()).or_insert(0);
        *c = c.saturating_add(1);

        // 推进全局 epoch：等价于"所有词的近期分都乘了一次衰减系数"
        self.current_epoch = self.current_epoch.wrapping_add(1);
        // 当前词：先把存的旧分懒衰减到当前 epoch，再 +RECENT_SCALE，戳上当前 epoch
        let entry = self
            .recent_scores
            .entry(text.to_string())
            .or_insert(RecentEntry {
                score: 0,
                epoch: self.current_epoch,
            });
        let decayed =
            Self::decay_score(entry.score, self.current_epoch.saturating_sub(entry.epoch));
        entry.score = decayed.saturating_add(RECENT_SCALE as u32);
        entry.epoch = self.current_epoch;

        self.dirty = true;
    }

    /// 直接设置某个词的选择次数（从 DB 加载时使用）
    pub fn set_count(&mut self, text: &str, count: u32) {
        self.counts.insert(text.to_string(), count);
        self.dirty = true;
    }

    /// 遗忘一个词（删除其用户频率记录）
    pub fn forget(&mut self, text: &str) {
        self.counts.remove(text);
        self.recent_scores.remove(text);
        self.dirty = true;
    }

    /// 获取某个词的近期分数（固定点，需除以 RECENT_SCALE 才是实际值）。
    ///
    /// 读时懒算衰减：按 `(current_epoch - stored_epoch)` 算 decay 后值。
    /// 返回值与旧版 eager 衰减等价（定点舍入容差内）。
    pub fn recent_score(&self, text: &str) -> u32 {
        match self.recent_scores.get(text) {
            Some(entry) => {
                Self::decay_score(entry.score, self.current_epoch.saturating_sub(entry.epoch))
            }
            None => 0,
        }
    }

    /// 清除 dirty 标记（写入成功后由 Engine 调用）
    pub(crate) fn mark_clean(&mut self) {
        self.dirty = false;
    }

    /// 获取用户频率加成（对数模型）
    ///
    /// 无记录时返回 1.0；有记录时返回 `1.0 + ln(count + 1)`。
    /// 对数增长避免用户学多了的词长期霸屏。
    /// 生产侧已改由竞争频率驱动（competition.rs，「退化」通道）；
    /// 本接口保留供测试/审计（Engine::user_freq 测试包装）。
    #[allow(dead_code)]
    pub fn user_freq(&self, text: &str) -> f32 {
        match self.counts.get(text) {
            // saturating_add：count 来自外部 DB，u32::MAX 时
            // 普通加法 debug panic / release 回绕 0 → ln(0) = -inf 污染排序。
            Some(&count) => 1.0 + (count.saturating_add(1) as f32).ln(),
            None => 1.0,
        }
    }

    /// 获取整数排名加成
    ///
    /// 基线 1000（代表 1.0 倍），每多一个 `log2(count + 1)` 增加 150。
    /// 用于候选排序的确定性整数计算。
    /// 生产侧已改由竞争频率驱动（同 user_freq 注）。
    #[allow(dead_code)]
    pub fn rank_boost(&self, text: &str) -> u32 {
        let count = self.counts.get(text).copied().unwrap_or(0);
        // saturating_add：回绕 0 时 ilog2(0) 在 release 也 panic，
        // 整条排序路径 unwind——外部 DB 的极端值是可触发输入。
        1000 + count.saturating_add(1).ilog2() * 150
    }

    /// 是否已脏
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// 加载后同步内存状态为干净
    pub(crate) fn mark_loaded(&mut self) {
        self.dirty = false;
    }

    /// 内存诊断：返回各 BTreeMap 的 entry 数量（内存排查用）。
    ///
    /// 封装 private 字段（recent_scores / pinyin_recent_scores）的访问，
    /// 供 FFI x1_engine_memory_stats 用。纯只读诊断，不影响任何运行时行为。
    /// 返回 (counts, recent_scores, pinyin_counts, pinyin_recent_scores,
    /// word_bigram_outer, word_bigram_inner_total)。
    pub(crate) fn memory_lens(&self) -> (usize, usize, usize, usize, usize, usize) {
        let (bigram_outer, bigram_inner_total) = self.user_bigram.len_stats();
        (
            self.counts.len(),
            self.recent_scores.len(),
            self.pinyin_counts.len(),
            self.pinyin_recent_scores.len(),
            bigram_outer,
            bigram_inner_total,
        )
    }

    // -----------------------------------------------------------------
    // 拼音级频率（多音字主音/次音动态权重）
    // -----------------------------------------------------------------

    /// 记录某字在某音下被选择一次（用于动态调整主音/次音优先级）。
    ///
    /// 与 `record(text)` 不同，本方法按 (text, pinyin) 维度统计，不会跨音串扰。
    /// 同时维护按音的近期分（指数衰减），让单字排序完全按音隔离：
    /// 在 di 选「地」只影响 di 的「地」近期分，不影响 de 的「地」。
    ///
    /// lazy epoch decay：用独立的 `pinyin_epoch` 计数（与 `current_epoch` 互不干扰），
    /// 只推进拼音 epoch + 更新当前条目，不扫 pinyin_recent_scores 全表。
    pub fn record_with_pinyin(&mut self, text: &str, pinyin: &str) {
        if text.is_empty() || pinyin.is_empty() {
            return;
        }
        let key = (text.to_string(), pinyin.to_string());
        *self.pinyin_counts.entry(key.clone()).or_insert(0) += 1;

        // 按音近期分：与 record() 同样的指数衰减，但只衰减 pinyin_recent_scores
        // 这张表（不动 recent_scores 表），两条衰减链完全独立。
        self.pinyin_epoch = self.pinyin_epoch.wrapping_add(1);
        let entry = self.pinyin_recent_scores.entry(key).or_insert(RecentEntry {
            score: 0,
            epoch: self.pinyin_epoch,
        });
        let decayed = Self::decay_score(entry.score, self.pinyin_epoch.saturating_sub(entry.epoch));
        entry.score = decayed.saturating_add(RECENT_SCALE as u32);
        entry.epoch = self.pinyin_epoch;

        self.dirty = true;
    }

    /// 查询某字某音的累计选择次数（用于排序时动态权重判断）
    pub fn pinyin_count(&self, text: &str, pinyin: &str) -> u32 {
        self.pinyin_counts
            .get(&(text.to_string(), pinyin.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// 查询某字某音的近期分（固定点，按音隔离）。
    ///
    /// 读时懒算衰减：按 `(pinyin_epoch - stored_epoch)` 算 decay 后值。
    pub fn pinyin_recent_score(&self, text: &str, pinyin: &str) -> u32 {
        match self
            .pinyin_recent_scores
            .get(&(text.to_string(), pinyin.to_string()))
        {
            Some(entry) => {
                Self::decay_score(entry.score, self.pinyin_epoch.saturating_sub(entry.epoch))
            }
            None => 0,
        }
    }

    /// 按音的排名加成（对数模型，与 rank_boost 同构）
    ///
    /// 用于单字排序：避免 di 选「地」让 de 的「地」获得 rank_boost 上升。
    /// 生产侧已改由竞争频率驱动（同 user_freq 注）。
    #[allow(dead_code)]
    pub fn pinyin_rank_boost(&self, text: &str, pinyin: &str) -> u32 {
        let count = self.pinyin_count(text, pinyin);
        1000 + (count + 1).ilog2() * 150
    }

    /// 批量设置拼音级频率（从 DB 加载时使用）
    ///
    /// 接收任意迭代器（兼容 storage 层的 HashMap 与测试的 BTreeMap）。
    pub fn set_pinyin_counts<I>(&mut self, entries: I)
    where
        I: IntoIterator<Item = ((String, String), u32)>,
    {
        self.pinyin_counts = entries.into_iter().collect();
    }

    /// 获取拼音级频率的快照（flush 持久化时使用）
    pub fn pinyin_counts_snapshot(&self) -> &BTreeMap<(String, String), u32> {
        &self.pinyin_counts
    }

    /// 批量设置累计使用次数（从 DB user_counts 表加载时使用）
    pub fn set_counts<I>(&mut self, entries: I)
    where
        I: IntoIterator<Item = (String, u32)>,
    {
        self.counts = entries.into_iter().collect();
    }

    /// 获取累计使用次数的快照（flush 持久化到 user_counts 表时使用）
    pub fn counts_snapshot(&self) -> Vec<(String, u32)> {
        self.counts.iter().map(|(k, &v)| (k.clone(), v)).collect()
    }

    /// 遗忘某字的所有拼音级记录（删除用户词时清理）
    pub fn forget_pinyin(&mut self, text: &str) {
        let keys_to_remove: Vec<(String, String)> = self
            .pinyin_counts
            .keys()
            .filter(|(t, _)| t == text)
            .cloned()
            .collect();
        let changed = !keys_to_remove.is_empty();
        for key in &keys_to_remove {
            self.pinyin_counts.remove(key);
            self.pinyin_recent_scores.remove(key);
        }
        if changed {
            self.dirty = true;
        }
    }

    // -----------------------------------------------------------------
    // bigram 共现统计（上下文感知，双轨：词库种子 + 用户积累）
    // -----------------------------------------------------------------
    //
    // 记录 (上文词, 当前词) → 共现次数，用于输入时上下文重排。
    // 例如上文「吃」+ 输入「fan」，若 (吃, 饭) 共现过多次，「饭」加分排前。
    // 双轨数据：冷启动种子（词库多字词反推）+ 用户积累（commit 记录）。
    // 上下文栈（最近 1 个上屏词）由 Engine 持有，本结构只管共现统计与持久化。

    /// 记录一次词级 bigram 共现：(prev, text) 次数 +1（用户动态积累）。
    ///
    /// 嵌套 entry：先取 prev 桶，再在桶内给 text 累加。
    /// 不做指数衰减：bigram 共现次数是累计统计，靠候选加分时的 min(cap) 自然限幅。
    /// 含 equiv_prev 泛化（代词/程度副词/能愿动词映射到统一 sentinel key）。
    ///
    /// 单字+单字一律不记（唯一 choke point 覆盖全部调用方：
    /// commit 上下文块 / 拆词学习 / 宿主端切词记录 / FFI）：
    /// 两个单字经常连在一起应固化成词（自造词管线已覆盖），字↔字 bigram 只有
    /// 噪声价值。字+词 / 词+字 / 词+词照常记录。
    pub fn record_word_bigram(&mut self, prev: &str, text: &str) -> u32 {
        self.record_word_bigram_weighted(prev, text, 1)
    }

    /// 带权重的词级 bigram 共现记录（用户明确意图加权学习）。
    ///
    /// 与 `record_word_bigram` 区别：增量由 `weight` 控制（而非固定 +1）。
    /// 用于用户 commit 整句时记录子词 bigram——用户主动选词上屏是强意图信号，
    /// 应让一次学习就能超过冷启动种子权重（种子最高 8，用户 commit 加权 +3，
    /// 几次即可建立稳定引导）。
    /// 字+字同样不记（同 `record_word_bigram`）。
    ///
    /// 返回实际写入的增量（会话上限可能截断；0 = 未写）。
    pub fn record_word_bigram_weighted(&mut self, prev: &str, text: &str, weight: u32) -> u32 {
        if prev.is_empty() || text.is_empty() || weight == 0 {
            return 0;
        }
        if prev.chars().count() == 1 && text.chars().count() == 1 {
            return 0; // 字+字不记
        }
        // prev 等价类归一化（同 record_word_bigram）
        let prev_key = equiv_prev(prev);
        // 会话级上限（噪音治理）：同一 (prev,text) 在本会话累计不超过
        // SESSION_BIGRAM_CAP；context_clear（切 app/上下文不连续）清零。
        let key = (prev_key.to_string(), text.to_string());
        let used = self.session_bigram_inc.get(&key).copied().unwrap_or(0);
        let weight = weight.min(SESSION_BIGRAM_CAP.saturating_sub(used));
        if weight == 0 {
            return 0;
        }
        *self.session_bigram_inc.entry(key).or_insert(0) += weight;
        let cur = self.user_bigram.record(prev_key, text, weight);
        self.bigram_dirty
            .entry(prev_key.to_string())
            .or_default()
            .insert(text.to_string(), cur);
        self.bigram_dirty_deletes
            .remove(&(prev_key.to_string(), text.to_string()));
        self.dirty = true;
        weight
    }

    /// 回滚一次 bigram 增量（纠错撤销）：store 与会话增量同步减，
    /// 减到 0 记 dirty delete（flush 删除该行，不落 0 行）。返回是否发生回滚。
    pub(crate) fn revert_word_bigram_weighted(
        &mut self,
        prev: &str,
        text: &str,
        weight: u32,
    ) -> bool {
        if prev.is_empty() || text.is_empty() || weight == 0 {
            return false;
        }
        if prev.chars().count() == 1 && text.chars().count() == 1 {
            return false;
        }
        let prev_key = equiv_prev(prev);
        let key = (prev_key.to_string(), text.to_string());
        if let Some(used) = self.session_bigram_inc.get(&key).copied() {
            let remaining = used.saturating_sub(weight);
            if remaining == 0 {
                self.session_bigram_inc.remove(&key);
            } else {
                self.session_bigram_inc.insert(key.clone(), remaining);
            }
        }
        let next = self.user_bigram.decrement(prev_key, text, weight);
        if next == 0 {
            self.bigram_dirty_deletes.insert(key);
            if let Some(m) = self.bigram_dirty.get_mut(prev_key) {
                m.remove(text);
            }
            if self
                .bigram_dirty
                .get(prev_key)
                .map(|m| m.is_empty())
                .unwrap_or(false)
            {
                self.bigram_dirty.remove(prev_key);
            }
        } else {
            self.bigram_dirty
                .entry(prev_key.to_string())
                .or_default()
                .insert(text.to_string(), next);
            self.bigram_dirty_deletes
                .remove(&(prev_key.to_string(), text.to_string()));
        }
        self.dirty = true;
        true
    }

    /// 重置会话级 bigram 增量计数（切换应用 / 上下文不连续时调用）。
    pub(crate) fn reset_session_bigram_inc(&mut self) {
        self.session_bigram_inc.clear();
    }

    /// 词级 bigram 查询（用户优先 + 种子回退 + equiv_prev 泛化）。
    ///
    /// 三层查询（双表方案）：
    /// 1. 用户精确查询：直接查 user_bigram (prev, text)。命中返回（用户优先，硬规则）。
    /// 2. 种子精确回退：用户没 commit 过，查 seed_bigram_counts (prev, text)。命中返回
    ///    （冷启动兜底，count 不与用户累加）。
    /// 3. equiv_prev 泛化：prev 归一化后查两个表（用户优先），半权返回。
    ///    让代词/程度副词/能愿动词等价类在冷启动也享受种子数据。
    ///
    /// 用户优先的硬规则语义：用户有 commit 记录就完全忽略种子（不看 count 量级）。
    /// 这与候选排序 collocation 优先的"硬规则层"架构一致。
    pub fn word_bigram_count(&self, prev: &str, text: &str) -> u32 {
        // 1. 用户精确查询（优先）
        if let Some(n) = self.user_bigram.get(prev, text) {
            return n;
        }
        // 2. 种子精确回退（冷启动兜底）
        if let Some(n) = self.seed_bigram.get(prev, text) {
            return n;
        }
        // 3. equiv_prev 泛化回退（用户优先 + 种子兜底）
        let equiv = equiv_prev(prev);
        if equiv != prev {
            // 3a. 用户泛化
            if let Some(n) = self.user_bigram.get(equiv, text) {
                // 泛化衰减：语法推断半权。(n+1)/2 防止单次记录归零
                return n.div_ceil(2);
            }
            // 3b. 种子泛化（冷启动时 PRON/DEGREE/AUX 等价类也享受种子数据）
            if let Some(n) = self.seed_bigram.get(equiv, text) {
                return n.div_ceil(2);
            }
        }
        0
    }

    /// 泛化增强版词级 bigram 查询（「十分+反感」类切分裁决）。
    ///
    /// 在 `word_bigram_count`（精确 + 类键行）之外，再补一层成员行聚合：
    /// 等价类（PRON/DEGREE/AUX）的成员各自的历史搭配同属类语义证据——种子
    /// 与用户表都只存原始 prev 行（如 很→反感=10；写侧把新增记录归一化到类键
    /// DEGREE→反感，但种子从未物化类键行，SQL 直查 DEGREE 行数=0），
    /// 「十分→反感」这类成员缺席查询因此只能靠本层兜底。聚合 Σ成员(user+seed)
    /// 后按泛化半权（div_ceil(2)，与 3a/3b 同衰减）。
    ///
    /// 语义示例：上文「十分」+ 输入 fangan → 方案(0) vs 反感
    ///（经 DEGREE 成员 很→反感=10 → 5）→ 反感读法胜。
    ///
    /// 调用面刻意收窄：当前唯一调用方 = 切分选举的读法偏好裁决
    ///（seg_order_cmp → reading_preference_cmp 的上文层）。不进入 LM 先验 /
    /// 候选加分等全局面——聚合证据改变读法选举（低风险、有证据才动），但不
    /// 放大到全排序面，避免行为面扩张（如需要再单独评估）。
    pub fn word_bigram_count_generalized(&self, prev: &str, text: &str) -> u32 {
        let base = self.word_bigram_count(prev, text);
        if base > 0 {
            return base;
        }
        let class = equiv_prev(prev);
        if class == prev {
            return 0;
        }
        let mut sum = 0u32;
        for &m in equiv_members(class) {
            if m == prev {
                continue;
            }
            sum = sum.saturating_add(self.user_bigram.get(m, text).unwrap_or(0));
            sum = sum.saturating_add(self.seed_bigram.get(m, text).unwrap_or(0));
        }
        sum.div_ceil(2)
    }

    /// 用户词级 bigram 查询（仅用户表，不含种子回退）。
    ///
    /// 用于候选排序「紧邻强关联硬规则」层——硬规则只认
    /// 用户通过 commit 明确建立的关联；种子语料搭配只走 context_bonus 软加分，
    /// 不升级为硬规则，避免语料高频搭配大范围颠覆既有排序。
    /// equiv_prev 泛化与 `word_bigram_count` 一致（代词等价类半权返回）。
    pub fn user_word_bigram_count(&self, prev: &str, text: &str) -> u32 {
        if let Some(n) = self.user_bigram.get(prev, text) {
            return n;
        }
        let equiv = equiv_prev(prev);
        if equiv != prev {
            if let Some(n) = self.user_bigram.get(equiv, text) {
                // 泛化衰减：语法推断半权（与 word_bigram_count 一致）
                return n.div_ceil(2);
            }
        }
        0
    }

    /// 返回用户词级 bigram 表的最高频次（自适应排序用，决定 dyn_cap）。
    /// O(n) 扫描全表，仅后台 60s 周期调用，不在热路径。
    pub fn max_bigram_count(&self) -> u32 {
        self.user_bigram.max_count()
    }

    /// counts 是否为空（user_modulator 冷启动判定用）。
    /// 冷启动（counts 空）时所有词不衰减，行为同现状。
    pub fn counts_is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    /// counts 是否包含某 text（user_modulator 判定"用户用过"用）。
    /// freq.counts 记录所有 commit 过的词，contains_key 即代表用户主动选过。
    pub fn counts_contains(&self, text: &str) -> bool {
        self.counts.contains_key(text)
    }

    // decay_counts（全表 ×7/8 时间衰减）已废除——真实时间不进权重公式。
    // 退化由 competition.rs 同音零和归一承担（commit 时 total+1/selects[chosen]+1，
    // 未选中者被分母稀释，无时间参数）。原函数还无条件 self.dirty=true，绕过
    // flush 调度的 is_dirty 短路，失活抖动下会每次失活全量落盘。

    /// 批量设置词级 bigram 共现（从 DB 加载用户积累时使用）
    ///
    /// 接收迭代器 (prev, text, count)，构建 CSR 基底（同 (prev, text) 重复
    /// 条目后写覆盖——与旧嵌套结构的 `+= count` 折叠等价，DB 主键约束下
    /// 实际不出现重复）。词库种子在 seed_bigram_from_phrase 单独注入，
    /// 不通过此方法。
    pub fn set_bigram_counts<I>(&mut self, entries: I)
    where
        I: IntoIterator<Item = (String, String, u32)>,
    {
        self.user_bigram = NgramStore::from_entries(entries);
        // 加载即磁盘状态 → 清空 dirty（无变化可落盘，零额外成本）
        self.bigram_dirty.clear();
        self.bigram_dirty_deletes.clear();
    }

    /// 取出自上次落盘以来的 bigram 增量（take 语义）：(upserts, deletes)。
    ///
    /// 调用方写盘失败时必须回填（`restore_bigram_delta`）——否则未落盘的变化
    /// 会丢（与 pending_deletions 的 take+回填模式一致）。
    pub fn take_bigram_delta(&mut self) -> (Vec<(String, String, u32)>, Vec<(String, String)>) {
        let ups: Vec<(String, String, u32)> = std::mem::take(&mut self.bigram_dirty)
            .into_iter()
            .filter(|(_, m)| !m.is_empty())
            .flat_map(|(prev, m)| {
                m.into_iter()
                    .map(move |(t, c)| (prev.clone(), t, c))
                    .collect::<Vec<_>>()
            })
            .collect();
        let del: Vec<(String, String)> = std::mem::take(&mut self.bigram_dirty_deletes)
            .into_iter()
            .collect();
        (ups, del)
    }

    /// 写盘失败回填（保持"未落盘"状态，下次 flush 重试）。
    pub fn restore_bigram_delta(
        &mut self,
        upserts: &[(String, String, u32)],
        deletes: &[(String, String)],
    ) {
        for (prev, text, count) in upserts {
            self.bigram_dirty
                .entry(prev.clone())
                .or_default()
                .insert(text.clone(), *count);
        }
        for (prev, text) in deletes {
            self.bigram_dirty_deletes
                .insert((prev.clone(), text.clone()));
        }
    }

    /// 设置种子 bigram（双表方案，冷启动兜底数据）。
    ///
    /// 与 set_bigram_counts 区别：用直接赋值（`=`）而非累加（`+=`）。
    /// 种子表是只读的（离线生成），不参与运行时增量。
    /// 不标记 dirty（种子数据不入 flush 流程，只随种子 DB 流转）。
    pub fn set_seed_bigram_counts<I>(&mut self, entries: I)
    where
        I: IntoIterator<Item = (String, String, u32)>,
    {
        self.seed_bigram = BigramCsrTable::from_entries(entries);
    }

    /// 直接安装构建好的种子 bigram 表（磁盘缓存快速路径：
    /// load_seed_bigram_table_cached 反序列化产物直接安装，跳过排序构建）。
    /// 与 set_seed_bigram_counts 同为整表替换，不标记 dirty。
    pub fn set_seed_bigram_table(&mut self, table: BigramCsrTable) {
        self.seed_bigram = table;
    }

    /// 种子 bigram 计数 + prev 出度总次数（插值 LM 的分子分母）。
    ///
    /// 查询路径与 `word_bigram_count` 的种子部分一致（精确 → equiv_prev 泛化半权），
    /// 但只查种子表（用户表由调用方 `lm_prior_score` 做硬覆盖判定，不走这里），
    /// 并同时返回匹配 key 的出度总次数作分母（equiv 命中时返回 equiv 桶的 total）。
    /// 返回 (0, 0) 表示种子未命中（调用方走 collocation 伪 count 保底或 ε 地板）。
    pub(crate) fn seed_bigram_count_and_total(&self, prev: &str, text: &str) -> (u32, u64) {
        if let Some(hit) = self.seed_bigram.get_and_total(prev, text) {
            return hit;
        }
        let equiv = equiv_prev(prev);
        if equiv != prev {
            // 泛化衰减半权（与 word_bigram_count 一致），分母用 equiv 桶
            if let Some((n, total)) = self.seed_bigram.get_and_total(equiv, text) {
                return (n.div_ceil(2), total);
            }
        }
        (0, 0)
    }

    /// 种子 bigram 反向边际（双向词格）：text 作为「后词」的
    /// 入度总量 Σcount。后向条件概率 P(left|right) 的分母——与
    /// `seed_bigram_count_and_total` 共用同一份对计数，只换归一化视角
    /// （前向除以 prev 出度、后向除以 next 入度）。精确匹配，不做 equiv
    /// 泛化（equiv_prev 是前词语义；后向分母的泛化留待实测证明需要）。
    pub(crate) fn seed_bigram_next_total(&self, text: &str) -> u64 {
        self.seed_bigram.next_total(text)
    }

    /// 获取词级 bigram 共现的扁平快照（全量重建/诊断工具用）。
    ///
    /// 注意：flush 已不用它（改 dirty 增量，见 `take_bigram_delta`）
    /// ——本方法保留给需要整表导出的场景（DB 重建/诊断），非落盘路径。
    #[allow(dead_code)]
    pub fn bigram_counts_flat(&self) -> Vec<(String, String, u32)> {
        self.user_bigram.entries_flat()
    }

    /// 遗忘含某词的所有 bigram 记录（删除用户词时清理）
    ///
    /// 扫两层：作为 prev（外层 key）或 text（内层 value）。CSR 基底不可变 →
    /// 以 overlay 墓碑遮蔽（语义等价性见 `ngram_store_equivalence` 守护测试）。
    pub fn forget_bigram(&mut self, text: &str) {
        let deleted = self.user_bigram.forget_text(text);
        // 会话增量同步清理（被删词的 pair 不再参与上限计数）
        self.session_bigram_inc
            .retain(|(prev, cur), _| prev != text && cur != text);
        if !deleted.is_empty() {
            for key in deleted {
                // 从 dirty upserts 摘掉（同周期内先写后删：只落删除）
                if let Some(m) = self.bigram_dirty.get_mut(&key.0) {
                    m.remove(&key.1);
                }
                self.bigram_dirty_deletes.insert(key);
            }
            self.dirty = true;
        }
    }

    /// 删除单个 bigram pair（审计/清理用）：当前存在才删；写 dirty delete。
    pub(crate) fn forget_bigram_pair(&mut self, prev: &str, text: &str) -> bool {
        if prev.is_empty() || text.is_empty() {
            return false;
        }
        let prev_key = equiv_prev(prev);
        if self.user_bigram.get(prev_key, text).is_none() {
            return false;
        }
        self.user_bigram.forget_pair(prev_key, text);
        let key = (prev_key.to_string(), text.to_string());
        self.session_bigram_inc.remove(&key);
        self.bigram_dirty_deletes.insert(key.clone());
        if let Some(m) = self.bigram_dirty.get_mut(prev_key) {
            m.remove(text);
        }
        if self
            .bigram_dirty
            .get(prev_key)
            .map(|m| m.is_empty())
            .unwrap_or(false)
        {
            self.bigram_dirty.remove(prev_key);
        }
        self.dirty = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 外部 DB 极端值防护：counts 灌入 u32::MAX
    /// 时运算必须饱和——user_freq 不产生 -inf、rank_boost 不 panic、record 的
    /// +1 不回绕。
    #[test]
    fn freq_extreme_counts_saturate() {
        let mut f = FreqModel::new();
        f.counts.insert("极值词".to_string(), u32::MAX);
        assert!(
            f.user_freq("极值词").is_finite(),
            "u32::MAX 时 user_freq 不得产生 -inf"
        );
        let _ = f.rank_boost("极值词"); // 防回绕 0 → ilog2(0) panic
        f.record("极值词"); // +1 饱和（u32::MAX 不回绕 0）
        assert!(
            f.user_freq("极值词").is_finite(),
            "record 后 user_freq 仍须有限"
        );
        assert!(f.rank_boost("极值词") >= 1000);
    }

    /// CSR 平铺表构建语义守护。
    ///
    /// 与旧嵌套 BTreeMap 版严格一致：
    /// - 同 (prev, text) 重复条目：count 后写覆盖（旧 inner.insert），
    ///   totals 累加全部条目（旧 entry().or_insert(0) += count）；
    /// - 输入无需预排序（构建时排序）；
    /// - 查询路径（word_bigram_count 种子回退 / count_and_total）行为不变。
    #[test]
    fn test_seed_bigram_csr_semantics() {
        let mut freq = FreqModel::new();
        assert!(freq.seed_bigram.is_empty());
        freq.set_seed_bigram_counts(vec![
            ("天".to_string(), "气".to_string(), 100),
            ("饭".to_string(), "吃".to_string(), 15),
            ("饭".to_string(), "吃".to_string(), 8), // 同 (prev,text) 重复：后写覆盖
            ("饭".to_string(), "做".to_string(), 5),
        ]);
        // 覆盖语义：饭→吃 = 8（最后一条）；totals = 15+8+5 = 28（累加全部条目）
        assert_eq!(freq.seed_bigram.get("饭", "吃"), Some(8));
        assert_eq!(freq.seed_bigram.total("饭"), 28);
        assert_eq!(freq.seed_bigram.get("饭", "做"), Some(5));
        assert_eq!(freq.seed_bigram.get("天", "气"), Some(100));
        assert_eq!(freq.seed_bigram.get("饭", "不存在"), None);
        assert_eq!(freq.seed_bigram.get("不存在", "吃"), None);
        assert_eq!(freq.seed_bigram.len(), 3);
        assert_eq!(freq.seed_bigram.prev_len(), 2);
        // count_and_total：分子取覆盖值 + 分母取累加值（与旧版逐条对照）
        assert_eq!(freq.seed_bigram_count_and_total("饭", "吃"), (8, 28));
        // word_bigram_count 种子回退走 CSR（用户表空 → 种子精确命中）
        assert_eq!(freq.word_bigram_count("饭", "吃"), 8);
    }

    /// 磁盘缓存往返守护。
    ///
    /// to_cache_bytes → from_cache_bytes 查询结果与原表逐位一致；
    /// 错哈希/截断/追加/篡改一律返回 None（调用方回退 SQL 重建），绝不 panic。
    #[test]
    fn test_seed_bigram_cache_roundtrip() {
        let t = BigramCsrTable::from_entries(vec![
            ("天".to_string(), "气".to_string(), 100),
            ("饭".to_string(), "吃".to_string(), 15),
            ("饭".to_string(), "吃".to_string(), 8), // 重复：后写覆盖
            ("饭".to_string(), "做".to_string(), 5),
            ("我".to_string(), "们".to_string(), 42),
        ]);
        let hash = "0123456789abcdef";
        let bytes = t.to_cache_bytes(hash);
        let u = BigramCsrTable::from_cache_bytes(&bytes, hash).expect("合法缓存必须解析成功");
        // 查询结果与原表逐位一致（含覆盖语义与 totals 累加语义）
        for (p, x) in [("天", "气"), ("饭", "吃"), ("饭", "做"), ("我", "们")] {
            assert_eq!(u.get(p, x), t.get(p, x), "({}, {}) 不一致", p, x);
        }
        assert_eq!(u.get_and_total("饭", "吃"), t.get_and_total("饭", "吃"));
        assert_eq!(u.total("饭"), t.total("饭"));
        assert_eq!(u.len(), t.len());
        assert_eq!(u.prev_len(), t.prev_len());
        assert_eq!(u.get("不", "在"), None);

        // 哈希不匹配 → None（缓存失效重建）
        assert!(BigramCsrTable::from_cache_bytes(&bytes, "fedcba9876543210").is_none());
        // 魔数错 → None
        let mut bad_magic = bytes.clone();
        bad_magic[0] ^= 0xFF;
        assert!(BigramCsrTable::from_cache_bytes(&bad_magic, hash).is_none());
        // 截断 → None
        assert!(BigramCsrTable::from_cache_bytes(&bytes[..bytes.len() - 3], hash).is_none());
        // 追加 → None（总长度精确一致校验）
        let mut appended = bytes.clone();
        appended.push(0);
        assert!(BigramCsrTable::from_cache_bytes(&appended, hash).is_none());
        // 头部计数篡改（n_prevs 巨大 → 长度不符）→ None
        let mut bad_hdr = bytes.clone();
        bad_hdr[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(BigramCsrTable::from_cache_bytes(&bad_hdr, hash).is_none());
        // body 任意字节翻转：要么校验拒绝要么解析出表，绝不 panic（无 UB）
        let mut flipped = bytes.clone();
        let mid = flipped.len() / 2;
        flipped[mid] ^= 0xFF;
        let _ = BigramCsrTable::from_cache_bytes(&flipped, hash);

        // 空表往返
        let empty = BigramCsrTable::from_entries(Vec::new());
        let eb = empty.to_cache_bytes(hash);
        let eu = BigramCsrTable::from_cache_bytes(&eb, hash).expect("空表缓存应可解析");
        assert_eq!(eu.len(), 0);
        assert_eq!(eu.get("饭", "吃"), None);
    }

    /// 性能探针：测量 record 在不同词表规模下的单次耗时。
    ///
    /// lazy epoch decay 后，record 从 O(词表规模) 全表扫描降为 O(1)，
    /// 各规模的耗时应当基本恒定（不随词表线性增长）。非断言测试，
    /// 用 --nocapture 看输出，跑完保留作为回归基线——若将来退化回 O(N)
    /// 可从此探针立即发现。
    #[test]
    fn bench_record_decay_scan() {
        for &vocab_size in &[100usize, 500, 2000, 10000] {
            let mut model = FreqModel::new();
            // 预填词表到指定规模
            for i in 0..vocab_size {
                model.counts.insert(format!("词{}", i), 1);
                model.recent_scores.insert(
                    format!("词{}", i),
                    RecentEntry {
                        score: 500,
                        epoch: 0,
                    },
                );
            }
            // 测量 100 次 record 的耗时（lazy decay 下应为 O(1)，不随词表增长）
            let start = std::time::Instant::now();
            for i in 0..100 {
                model.record(&format!("新词{}", i));
            }
            let elapsed = start.elapsed();
            eprintln!(
                "vocab={:>6}: 100 次 record 耗时 {:?} (单次 {:?})",
                vocab_size,
                elapsed,
                elapsed / 100
            );
        }
    }

    /// lazy epoch decay 数值等价对照测试。
    ///
    /// 构造与旧版 eager 全表衰减语义等价的参考实现（每次 record 后把所有词
    /// 乘 17/20，再给当前词 +1000），对随机操作序列断言 lazy 实现的 recent_score
    /// 与 eager 参考值差异 ≤ 1（u32 定点乘除顺序的舍入容差）。守护 lazy decay
    /// 不破坏排序数值基线。
    #[test]
    fn test_lazy_decay_equivalence() {
        // eager 参考实现：模拟旧版全表衰减
        let mut eager: BTreeMap<String, u32> = BTreeMap::new();
        let mut lazy = FreqModel::new();

        // 操作序列：混合同一词连续选 + 切换到其他词（产生不同 delta）
        let ops = [
            "苹果", "苹果", "香蕉", "苹果", "橘子", "香蕉", "香蕉", "香蕉", "苹果", "葡萄", "橘子",
            "橘子", "橘子", "橘子", "橘子", "苹果", "香蕉", "葡萄",
        ];

        for &word in ops.iter() {
            // eager: 全表 ×17/20，当前词 +1000
            for v in eager.values_mut() {
                *v = ((*v as u64 * RECENT_DECAY_NUM) / RECENT_DECAY_DEN) as u32;
            }
            *eager.entry(word.to_string()).or_insert(0) += RECENT_SCALE as u32;
            // lazy: 推进 epoch + 更新当前条目
            lazy.record(word);
        }

        // 断言：每个词的 lazy recent_score 与 eager 参考值差异 ≤ 1
        let mut checked = 0;
        for (word, eager_score) in &eager {
            let lazy_score = lazy.recent_score(word);
            let diff = (*eager_score as i64 - lazy_score as i64).abs();
            assert!(
                diff <= 1,
                "词 {:?}: lazy={} eager={} 差 {} > 1（定点舍入容差）",
                word,
                lazy_score,
                eager_score,
                diff
            );
            checked += 1;
        }
        // 确保对照确实跑了非平凡规模（避免空序列假绿）
        assert!(checked >= 4, "对照测试应覆盖至少 4 个词，实际 {}", checked);
    }

    #[test]
    fn test_record_and_sort() {
        let mut model = FreqModel::new();
        model.record("中文");
        model.record("中文");
        assert!(model.user_freq("中文") > model.user_freq("中国"));
    }

    #[test]
    fn test_rank_boost_logarithmic() {
        let mut model = FreqModel::new();
        assert_eq!(model.rank_boost("中文"), 1000);
        model.record("中文");
        // count = 1, log2(2) = 1, boost = 1000 + 150 = 1150
        assert_eq!(model.rank_boost("中文"), 1150);
        // 即使记录 1000 次，增长也很缓慢
        for _ in 0..999 {
            model.record("中文");
        }
        // count = 1000, log2(1001) = 9, boost = 1000 + 9 * 150 = 2350
        assert_eq!(model.rank_boost("中文"), 2350);
    }

    #[test]
    fn test_load_and_save_text() {
        let mut model = FreqModel::new();
        model.record("中文");
        model.record("中文");
        model.record("中国");

        let text = model.save_to_text();
        let mut model2 = FreqModel::new();
        model2.load_from_text(&text);

        assert_eq!(model2.user_freq("中文"), model.user_freq("中文"));
        assert_eq!(model2.user_freq("中国"), model.user_freq("中国"));
        assert_eq!(model2.user_freq("美国"), 1.0);
    }

    #[test]
    fn test_recent_score_decay() {
        let mut model = FreqModel::new();

        // 连续输入 5 次“中文”
        for _ in 0..5 {
            model.record("中文");
        }
        assert!(model.recent_score("中文") > 0);
        assert_eq!(model.recent_score("中国"), 0);

        // 输入其他词把“中文”挤出近期主导位置
        for i in 0..20 {
            model.record(&format!("other{}", i));
        }
        // 衰减后“中文”的近期分应明显下降
        let score_after_gap = model.recent_score("中文");
        assert!(
            score_after_gap < model.recent_score("other19"),
            "久未使用的词近期分应低于刚使用的词"
        );

        // 再次输入“中文”，recent_score 恢复增长
        model.record("中文");
        assert!(model.recent_score("中文") > score_after_gap);
    }

    #[test]
    fn test_forget_clears_recent() {
        let mut model = FreqModel::new();
        model.record("中文");
        assert!(model.recent_score("中文") > 0);
        model.forget("中文");
        assert_eq!(model.recent_score("中文"), 0);
        assert_eq!(model.user_freq("中文"), 1.0);
    }

    // ===== 代词等价类 bigram 泛化测试 =====

    #[test]
    fn test_equiv_pron_generalization() {
        // record (我,知道)+(你,知道) → PRON 桶累计 2
        let mut m = FreqModel::new();
        m.record_word_bigram("我", "知道");
        m.record_word_bigram("你", "知道");
        // 查 (他,知道)：精确 miss → 泛化 PRON → 命中 2，返回 2/2=1
        let bc = m.word_bigram_count("他", "知道");
        assert!(bc >= 1, "代词泛化：(他,知道) 应命中 PRON 桶，实际 {}", bc);
        // 查 (我,知道)：精确 miss（"我" 归一化后不单独存）→ 泛化 PRON → 命中 2，返回 1
        let bc2 = m.word_bigram_count("我", "知道");
        assert!(bc2 >= 1, "代词泛化：(我,知道) 也走 PRON 桶，实际 {}", bc2);
    }

    #[test]
    fn test_equiv_degree_generalization() {
        // record (很,好) → DEGREE 桶
        let mut m = FreqModel::new();
        m.record_word_bigram("很", "好");
        m.record_word_bigram("特别", "好");
        // 查 (非常,好)：泛化 DEGREE 命中
        let bc = m.word_bigram_count("非常", "好");
        assert!(
            bc >= 1,
            "程度副词泛化：(非常,好) 应命中 DEGREE 桶，实际 {}",
            bc
        );
    }

    #[test]
    fn test_equiv_aux_generalization() {
        // record (能,做事) → AUX 桶（"能"是能愿动词单字 prev，text 用双字词——
        // 字+字全线不记，fixture 用 字+词 合法形态）
        let mut m = FreqModel::new();
        m.record_word_bigram("能", "做事");
        // 查 (会,做事)：泛化 AUX 命中
        let bc = m.word_bigram_count("会", "做事");
        assert!(
            bc >= 1,
            "能愿动词泛化：(会,做事) 应命中 AUX 桶，实际 {}",
            bc
        );
    }

    #[test]
    fn test_non_equiv_unchanged() {
        // 非等价类词不泛化
        let mut m = FreqModel::new();
        m.record_word_bigram("吃饭", "没");
        // 查 (睡觉,没)：吃饭/睡觉 不在等价类 → 不泛化 → 0
        let bc = m.word_bigram_count("睡觉", "没");
        assert_eq!(bc, 0, "非等价类词不应泛化：(睡觉,没) 应为 0");
    }

    #[test]
    fn test_precise_overrides_generic() {
        // 精确命中优先于泛化（即使泛化桶也有数据）
        // 用 record_word_bigram_weighted 让原始 key 和 PRON key 分别有值
        let mut m = FreqModel::new();
        // 先存入 PRON 桶（通过代词归一化）
        m.record_word_bigram("我", "知道"); // → PRON 桶
                                            // 直接操作 user_bigram 存一个精确的"他" key
        m.user_bigram.record("他", "知道", 10);
        // 查 (他,知道)：精确命中 10（优先），不走泛化
        let bc = m.word_bigram_count("他", "知道");
        assert_eq!(bc, 10, "精确命中应优先于泛化，实际 {}", bc);
    }

    /// NgramStore 与旧嵌套 BTreeMap 的语义等价性守护（CSR+overlay 改造的核心保险）。
    ///
    /// 参照模型逐字复刻旧实现（`entry().or_default().entry().or_insert(0) += w`、
    /// forget 两层扫描 + 空桶清理、get 链），随机操作序列（确定性 xorshift）
    /// 下逐步比对：每步点查询（覆盖 base 命中 / overlay 遮蔽 / 墓碑删除 /
    /// 删后重记录）+ forget 后全表扁平快照 + 周期性 max/flat/len 一致性。
    /// 初始数据在 base 里预置条目，保证墓碑与遮蔽路径一定被走到。
    #[test]
    fn ngram_store_equivalence() {
        let initial: Vec<(&str, &str, u32)> = vec![
            ("我", "知道", 3),
            ("我", "喜欢", 2),
            ("知道", "我", 1),
            ("电脑", "知道", 5),
            ("喜欢", "电脑", 1),
            ("我", "电脑", 7),
            ("PRON", "知道", 4),
            ("很", "知道", 2),
            ("知道", "电脑", 6),
        ];
        // 参照模型 = 旧嵌套 BTreeMap 实现的逐字复刻
        let mut reference: BTreeMap<String, BTreeMap<String, u32>> = BTreeMap::new();
        let mut store = NgramStore::from_entries(
            initial
                .iter()
                .map(|&(a, b, c)| (a.to_string(), b.to_string(), c)),
        );
        for &(a, b, c) in &initial {
            *reference
                .entry(a.to_string())
                .or_default()
                .entry(b.to_string())
                .or_insert(0) += c;
        }

        let words = ["我", "知道", "电脑", "喜欢", "PRON", "很", "新词", "你"];
        let mut rng: u64 = 0x5EED_0000_0000_0916;
        let mut rnd = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let flat_of = |m: &BTreeMap<String, BTreeMap<String, u32>>| -> Vec<(String, String, u32)> {
            let mut v: Vec<(String, String, u32)> = m
                .iter()
                .flat_map(|(o, inner)| inner.iter().map(move |(t, &c)| (o.clone(), t.clone(), c)))
                .collect();
            v.sort();
            v
        };

        for step in 0..3000 {
            let op = rnd() % 10;
            let a = words[(rnd() % words.len() as u64) as usize];
            let b = words[(rnd() % words.len() as u64) as usize];
            match op {
                0..=4 => {
                    // record（权重 1..=3，覆盖加权与普通两条路径的公共底座）
                    let w = (rnd() % 3 + 1) as u32;
                    let expected = {
                        let e = reference
                            .entry(a.to_string())
                            .or_default()
                            .entry(b.to_string())
                            .or_insert(0);
                        *e += w;
                        *e
                    };
                    let got = store.record(a, b, w);
                    assert_eq!(got, expected, "record 返回值 step={} ({},{})", step, a, b);
                }
                5..=6 => {
                    // forget（墓碑路径）：立即全表比对
                    let _ = store.forget_text(a);
                    for inner in reference.values_mut() {
                        inner.remove(a);
                    }
                    reference.remove(a);
                    reference.retain(|_, m| !m.is_empty());
                    assert_eq!(
                        store.entries_flat(),
                        flat_of(&reference),
                        "forget 后全表一致 step={} forget={}",
                        step,
                        a
                    );
                }
                _ => {
                    // 点查询
                    let expect = reference.get(a).and_then(|m| m.get(b)).copied();
                    assert_eq!(store.get(a, b), expect, "get step={} ({},{})", step, a, b);
                }
            }
            if step % 97 == 0 {
                assert_eq!(
                    store.entries_flat(),
                    flat_of(&reference),
                    "flat step={}",
                    step
                );
                let ref_max = reference
                    .values()
                    .flat_map(|m| m.values())
                    .copied()
                    .max()
                    .unwrap_or(0);
                assert_eq!(store.max_count(), ref_max, "max step={}", step);
                let ref_outer = reference.len();
                let ref_inner: usize = reference.values().map(|m| m.len()).sum();
                assert_eq!(
                    store.len_stats(),
                    (ref_outer, ref_inner),
                    "len_stats step={}",
                    step
                );
            }
        }
        assert_eq!(store.entries_flat(), flat_of(&reference), "最终全表一致");
    }

    /// 墓碑语义定点测试（确定性，可读性优先）：删除遮蔽 base、重记录从 0 起算。
    #[test]
    fn ngram_store_tombstone_rerecord() {
        let mut s = NgramStore::from_entries(vec![("我".into(), "知道".into(), 5)]);
        assert_eq!(s.get("我", "知道"), Some(5));
        s.forget_text("知道");
        assert_eq!(s.get("我", "知道"), None, "墓碑遮蔽 base 旧值");
        let v = s.record("我", "知道", 2);
        assert_eq!(v, 2, "删后重记录从 0 起算（不落回 base 旧值 5）");
        assert_eq!(s.get("我", "知道"), Some(2));
        assert_eq!(
            s.entries_flat(),
            vec![("我".to_string(), "知道".to_string(), 2)]
        );
        // forget 作为 outer：整桶遮蔽
        let mut s2 = NgramStore::from_entries(vec![
            ("我".into(), "知道".into(), 5),
            ("我".into(), "喜欢".into(), 3),
        ]);
        s2.forget_text("我");
        assert_eq!(s2.get("我", "知道"), None);
        assert_eq!(s2.get("我", "喜欢"), None);
        assert!(s2.entries_flat().is_empty());
    }

    /// overlay 遮蔽 base：record 的读改写以 base∪overlay 为当前值。
    #[test]
    fn ngram_store_overlay_shadows_base() {
        let mut s = NgramStore::from_entries(vec![("我".into(), "知道".into(), 5)]);
        assert_eq!(s.record("我", "知道", 1), 6);
        assert_eq!(s.get("我", "知道"), Some(6));
        assert_eq!(s.max_count(), 6);
    }
}

#[cfg(test)]
mod session_cap_tests {
    use super::*;

    /// 学习噪声治理：同一会话同一 (prev,text) 增量上限 5；
    /// `reset_session_bigram_inc`（切换应用/上下文不连续）后重新计数。
    #[test]
    fn session_bigram_increment_cap_and_reset() {
        let mut f = FreqModel::new();
        // prev 不能是代词/程度/能愿（equiv_prev 会展宽成等价类 key），用普通词。
        for _ in 0..20 {
            f.record_word_bigram("今天", "测试");
        }
        assert_eq!(
            f.user_bigram.get("今天", "测试"),
            Some(SESSION_BIGRAM_CAP),
            "同一会话重复记录应封顶"
        );
        f.reset_session_bigram_inc();
        for _ in 0..3 {
            f.record_word_bigram("今天", "测试");
        }
        assert_eq!(
            f.user_bigram.get("今天", "测试"),
            Some(SESSION_BIGRAM_CAP + 3),
            "新会话段应能继续累计"
        );
    }
}
