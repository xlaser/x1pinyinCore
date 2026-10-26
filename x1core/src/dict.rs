//! 词库数据结构
//!
//! 使用 Trie（前缀树）存储拼音到汉字的映射。
//! 每个节点存储一个音节对应的候选字/词列表。
//!
//! 内存布局优化：
//! - Trie 节点用 Vec 存储子节点，按音节首字母排序，支持二分查找
//! - 候选词列表在叶节点存储
//! - 基础词库编译进二进制（include_bytes!），启动时反序列化

use crate::pinyin::Token;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;
use std::collections::HashMap;
use std::sync::Arc;

/// 小字符串：≤N 字节栈内联（零堆分配），超长回退堆。
///
/// 用途：
/// - `TrieNode.syllable`（SmallStr<8>）：标准音节最长 6 字符（"chuang"/"zhuang"），
///   8 字节全内联——消除 19 万节点各 1 次 String 堆分配 + 24B 栈开销
/// - `CandidateSortKey.pinyin/text`（scoring.rs，SmallStr<48>/<24>）：排序键构造免堆分配
///
/// 比较语义与 String 完全一致（按 as_str 字典序）。
/// pub 因 pub struct TrieNode 的 pub 字段引用了它（私有类型泄漏 lint）。
pub enum SmallStr<const N: usize> {
    Inline { buf: [u8; N], len: u8 },
    Heap(String),
}

impl<const N: usize> SmallStr<N> {
    pub(crate) fn from_str(s: &str) -> Self {
        let b = s.as_bytes();
        if b.len() <= N && b.len() <= u8::MAX as usize {
            let mut buf = [0u8; N];
            buf[..b.len()].copy_from_slice(b);
            SmallStr::Inline {
                buf,
                len: b.len() as u8,
            }
        } else {
            SmallStr::Heap(s.to_string())
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        match self {
            SmallStr::Inline { buf, len } => unsafe {
                // 安全论证：构造时 buf[..len] 来自合法 UTF-8 的字节拷贝，必为合法 UTF-8
                core::str::from_utf8_unchecked(&buf[..*len as usize])
            },
            SmallStr::Heap(s) => s,
        }
    }

    /// 堆字节（内联为 0，Heap 为内容长度）——内存 profiling 用（仅测试构建，
    /// 见 engine/tests_query.rs bench_trie_memory_profile）。
    #[cfg(test)]
    pub(crate) fn heap_bytes(&self) -> usize {
        match self {
            SmallStr::Inline { .. } => 0,
            SmallStr::Heap(s) => s.len(),
        }
    }
}

impl<const N: usize> Clone for SmallStr<N> {
    fn clone(&self) -> Self {
        match self {
            SmallStr::Inline { buf, len } => SmallStr::Inline {
                buf: *buf,
                len: *len,
            },
            SmallStr::Heap(s) => SmallStr::Heap(s.clone()),
        }
    }
}

impl<const N: usize> PartialEq for SmallStr<N> {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}
impl<const N: usize> Eq for SmallStr<N> {}
impl<const N: usize> PartialOrd for SmallStr<N> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<const N: usize> Ord for SmallStr<N> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}
impl<const N: usize> core::fmt::Debug for SmallStr<N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 单个候选结果
///
/// text/pinyin 用 `Arc<str>` 共享：
/// Trie 构建时从 DB 加载的字符串所有权转入 Arc（零额外分配），查询
/// `node.candidates.clone()` 时仅引用计数 +1，消除热路径每候选 2 次
/// 堆分配+拷贝+释放。候选从 Trie 取出后全程只读（比较/排序/JSON 序列化），
/// Arc 内容比较语义与 String 一致，dedup/`==` 无缝。动态构造的候选
/// （自组词/整句/green_memo）用 `Arc::from(String)` 转移所有权，同样零拷贝。
#[derive(Debug, Clone)]
pub struct Candidate {
    pub text: Arc<str>,
    pub base_freq: u32,
    /// 该候选对应的完整拼音（用于自组词时拼装整词拼音）
    pub pinyin: Arc<str>,
    /// 该候选当前读音是否为该字的主音。
    ///
    /// 仅对多音单字有意义：主音字在精确匹配同分组内排序优先于次音字，
    /// 避免「地」(主音 di，次音 de) 在 de 查询里压过「德」(独音 de)。
    /// 用户实际输入习惯可通过 `FreqModel::pinyin_count` 动态覆盖该默认优先级。
    /// 多字词和非多音字恒为 `true`。
    pub is_primary: bool,
    /// 是否为纠错候选（输入拼错时编辑距离变形查到）。
    ///
    /// 仅在 `process` 候选不足时触发纠错回退生成。纠错候选排在正常候选之后，
    /// 候选框用橙色显示，让用户知道这是「猜的」而非精确匹配。
    /// 正常查询路径产生的候选恒为 `false`。
    pub is_correction: bool,
    /// 是否为智能组合候选（整句解码/多词拼接产生，非词库整词）。
    ///
    /// 由 `decode_sentence` 把多个词拼成完整句子（如「我喜欢吃饭」)时标记。
    /// 这类候选不是词库里的单个词条，而是算法智能组合的，候选框用绿色显示，
    /// 让用户一眼区分：词库词（默认色）、纠错词（橙色）、智能组合词（绿色）。
    /// 词库精确匹配的候选恒为 `false`。
    pub is_sentence: bool,
    /// 是否为「前缀补全猜测」候选（末音节更长，染紫色）。
    ///
    /// 触发：用户输入末音节完整但精确匹配结果 < 3（如 yonghu 末音节 hu 完整），
    /// 引擎把末音节当前缀扫子节点（`query_prefix_filtered`）拉来「末音节更长」的
    /// 候选——如「用坏」(yong huai) 被拉进 yonghu 输入。这类候选是「对用户后续
    /// 输入的猜测」（用户若接着输 ai 才会真正选「用坏」)，染紫色让用户视觉区分。
    ///
    /// 判据：候选 pinyin 的末音节 ≠ 用户输入末音节（如 huai ≠ hu）。
    /// 标记点：`Engine::word_candidates_for_syllables` 分支 A（query.rs 的
    /// `is_last_valid` 分支内 `query_prefix_filtered_fuzzy` 调用之后）。
    /// 分支 B（末音节不完整的输入实时提示场景）不染紫，因为那是合法前缀补全。
    ///
    /// 不持久化、不影响排序打分（与橙词/绿词一致，是查询期临时标记）。
    /// 优先级链（drawItem if 顺序）：橙（纠错）> 绿（整句）> 紫（猜测）> 蓝（快捷）> 白（默认）。
    pub is_guess: bool,
    /// 是否为快捷输入候选（/date /fh 等前缀触发的符号/日期/时间）。
    ///
    /// 触发：用户输入 `/` 引导的前缀（如 `/fh`、`/date`），引擎 `try_quick_input`
    /// 拦截后从符号表产生候选，或宿主端生成日期时间。
    /// 候选框用蓝色显示，让用户区分词库精确词（白）与快捷候选（蓝）。
    ///
    /// 快捷候选的 pinyin 字段为空（符号/日期没有拼音），base_freq 设高值（确保排前）。
    /// 不持久化、不影响排序打分（查询期临时标记）。
    pub is_shortcut: bool,
    /// 是否为英文单词候选（智能单词识别）。
    ///
    /// 两种来源共用此标记：中文模式（拼音输入命中英文词表，`inject_english_candidates`）
    /// 与英文模式前缀补全（服务 EnglishComplete 查询后前端自行组织）。
    /// 候选特征：text 为纯小写英文单词、pinyin 恒空、base_freq 为英文侧词频
    /// （仅英文候选间定序，不与中文 freq 谱混用）、选中后全消费输入且
    /// 不进中文学习面（commit 对含 ASCII 字母 text 直接跳过）。
    /// 优先级链（drawItem if 顺序）：橙 > 绿 > 紫 > 蓝 > 英文 > 白。
    /// 不持久化、不影响中文排序打分（排序后定点插入，不动 candidate_sort_key）。
    pub is_english: bool,
    /// 绿词 DP 路径分：`decode_sentence` 的 N-best
    /// 路径分（`is_sentence` 时 >0/<0 都可能），非绿词恒 0。
    ///
    /// 用途：candidate_sort_key 的同分 tie 层——绿词候选的
    /// `base_freq` 取切分最高词频（共享前缀的同构绿词恒同分）、pinyin 是同一串
    /// 简拼字母（恒相同），旧 tie-break 落到 text 字典序（汉字 Unicode 序无语义，
    /// 「性爱」U+6027 会压「西安」U+897F）——DP 认为更优的切分反而排后。本字段
    /// 把「哪条切分更好」的信号带到候选层：同分绿词按路径分降序（西安 27 > 性爱 26）。
    pub sentence_score: i32,
}

/// Trie 节点
#[derive(Clone)]
pub struct TrieNode {
    /// 当前节点对应的音节（完整音节字符串，SmallStr<8> 内联——标准音节 ≤6 字符零堆分配）
    pub syllable: SmallStr<8>,
    /// 该音节对应的候选词列表（按基础频率排序）
    pub candidates: Vec<Candidate>,
    /// 子节点（按 syllable 排序）
    pub children: Vec<TrieNode>,
}

impl TrieNode {
    pub fn new(syllable: &str) -> Self {
        Self {
            syllable: SmallStr::from_str(syllable),
            candidates: Vec::new(),
            children: Vec::new(),
        }
    }

    /// 查找子节点（二分查找）
    pub fn find_child(&self, syllable: &str) -> Option<&TrieNode> {
        self.children
            .binary_search_by(|n| n.syllable.as_str().cmp(syllable))
            .ok()
            .map(|idx| &self.children[idx])
    }

    pub fn find_child_mut(&mut self, syllable: &str) -> Option<&mut TrieNode> {
        self.children
            .binary_search_by(|n| n.syllable.as_str().cmp(syllable))
            .ok()
            .map(|idx| &mut self.children[idx])
    }

    /// 插入子节点并保持排序
    pub fn insert_child(&mut self, node: TrieNode) {
        let pos = self
            .children
            .binary_search_by(|n| n.syllable.as_str().cmp(node.syllable.as_str()))
            .unwrap_or_else(|e| e);
        self.children.insert(pos, node);
    }
}

/// Trie 内存构成统计结果（仅测试构建使用；节点 Trie 字段只计运行时
/// overlay 增量，词典主存储的规模/字节看 arena_* 与 wi_span_* 字段）。
#[cfg(test)]
#[derive(Default, Debug)]
pub(crate) struct TrieMemoryProfile {
    /// Trie 节点总数（arena 化后 = overlay 节点数，量级极小）
    pub node_count: usize,
    /// 全部节点 syllable 的堆字节（String 内容）
    pub syllable_heap_bytes: usize,
    /// 候选总数
    pub candidate_count: usize,
    /// 全部候选 text 的堆字节
    pub text_heap_bytes: usize,
    /// 全部候选 pinyin 的堆字节（按节点共享，每节点计一次）
    pub pinyin_heap_bytes: usize,
    /// 持有候选的节点数（= pinyin Arc 分配数，共享后每次插入仅一份）
    pub pinyin_alloc_count: usize,
    /// 统一词索引条目数（text→freq/pinyin 侧索引）
    pub word_index_len: usize,
    /// 全部 children Vec 的容量（元素个数，×24B = 实际占用）
    pub children_capacity: usize,
    /// 全部 candidates Vec 的容量（元素个数，×TrieNode 大小 = 实际占用）
    pub candidates_capacity: usize,
    /// 单字表桶数（音节数）
    pub sc_buckets: usize,
    /// 单字表候选总数
    pub sc_candidates: usize,
    /// arena 基底估算堆字节（紧凑数组 + blob，
    /// 精确预留容量；finish_load 前为 0）
    pub arena_bytes: usize,
    /// arena 基底节点数 / 候选数（词典主存储规模）
    pub arena_nodes: usize,
    pub arena_candidates: usize,
    /// 词索引 span 基底字节（entries 16B/条 + text/py blob；
    /// 运行时 word_index Vec 已清空，word_index_len 只计增量）
    pub wi_span_bytes: usize,
    /// span 基底条目数
    pub wi_span_len: usize,
}

/// 词索引 span 基底。
///
/// 为什么必须 span 化（而不是继续持 Arc）：arena 化后 finish_load 会整体
/// 丢弃节点 Trie——word_index 若保留 187k × 2 个旧 Arc 引用，会把旧内存区
/// 几乎每一页都钉死（小分配与 Trie 节点交错分配），碎片一分收不回。
/// 查询走 blob 切片（`find_word_entry` 统一入口，语义同旧 binary_search）。
///
/// 字节段化（与 TrieArena 同款）：磁盘格式 = 内存格式（3 段：text_blob / py_blob / entries(16B 定长)），自有构建/缓存
/// 装载/映射装载共用同一套段读取；校验通过（段界+UTF-8+text 严格升序）是
/// 直接索引的安全前提。
#[derive(Clone)]
pub(crate) struct WordIndexSpans {
    backing: crate::trie_arena::Backing,
    secs: [(u32, u32); 3],
}

const WI_MAGIC: &[u8; 8] = b"X1WISPN1";
const WI_TABLE_OFF: usize = 8;
const WI_DATA_OFF: usize = WI_TABLE_OFF + 3 * 8;
const WI_SEC_TEXT_BLOB: usize = 0;
const WI_SEC_PY_BLOB: usize = 1;
const WI_SEC_ENTRIES: usize = 2;
/// entries 记录宽（text_off u32 + py_off u32 + text_len u16 + py_len u16 + freq u32）。
const WI_ENTRY_W: usize = 16;

impl WordIndexSpans {
    /// 从排序去重后的词索引转储（两遍法：精确 layout 直写）。
    fn build(items: &[(Arc<str>, u32, Arc<str>)]) -> Self {
        let n = items.len();
        let text_bytes: usize = items.iter().map(|(t, _, _)| t.len()).sum();
        let py_bytes: usize = items.iter().map(|(_, _, p)| p.len()).sum();
        let sizes = [text_bytes, py_bytes, n * WI_ENTRY_W];
        let mut secs = [(0u32, 0u32); 3];
        let mut off = WI_DATA_OFF;
        for (i, size) in sizes.iter().enumerate() {
            secs[i] = (off as u32, *size as u32);
            off += size;
            off = (off + 7) & !7usize;
        }
        let mut buf = vec![0u8; off];
        buf[..8].copy_from_slice(WI_MAGIC);
        for (i, s) in secs.iter().enumerate() {
            let o = WI_TABLE_OFF + i * 8;
            buf[o..o + 4].copy_from_slice(&s.0.to_le_bytes());
            buf[o + 4..o + 8].copy_from_slice(&s.1.to_le_bytes());
        }
        let mut to = 0usize;
        let mut po = 0usize;
        for (k, (t, f, p)) in items.iter().enumerate() {
            let eo = secs[WI_SEC_ENTRIES].0 as usize + k * WI_ENTRY_W;
            buf[eo..eo + 4].copy_from_slice(&(to as u32).to_le_bytes());
            buf[eo + 4..eo + 8].copy_from_slice(&(po as u32).to_le_bytes());
            buf[eo + 8..eo + 10].copy_from_slice(&(t.len() as u16).to_le_bytes());
            buf[eo + 10..eo + 12].copy_from_slice(&(p.len() as u16).to_le_bytes());
            buf[eo + 12..eo + 16].copy_from_slice(&f.to_le_bytes());
            let tb = secs[WI_SEC_TEXT_BLOB].0 as usize + to;
            buf[tb..tb + t.len()].copy_from_slice(t.as_bytes());
            let pb = secs[WI_SEC_PY_BLOB].0 as usize + po;
            buf[pb..pb + p.len()].copy_from_slice(p.as_bytes());
            to += t.len();
            po += p.len();
        }
        WordIndexSpans {
            backing: crate::trie_arena::Backing::Owned(Arc::from(buf)),
            secs,
        }
    }

    /// 自有字节装载（校验通过才返回）。
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Option<Self> {
        let secs = Self::validate(&bytes)?;
        Some(WordIndexSpans {
            backing: crate::trie_arena::Backing::Owned(Arc::from(bytes)),
            secs,
        })
    }

    /// 映射区切片装载（`region[off..off+len]`）。
    pub(crate) fn from_mapped_range(
        region: Arc<crate::mmap_file::MmapRegion>,
        off: usize,
        len: usize,
    ) -> Option<Self> {
        if off.checked_add(len)? > region.len() {
            return None;
        }
        let bytes = &region.bytes()[off..off + len];
        let secs = Self::validate(bytes)?;
        Some(WordIndexSpans {
            backing: crate::trie_arena::Backing::Mapped { region, off, len },
            secs,
        })
    }

    pub(crate) fn to_bytes(&self) -> &[u8] {
        self.backing.bytes()
    }

    /// 全量校验：段界/记录宽自洽 + 全部 span 界内且 UTF-8 合法 +
    /// text 严格升序（二分前提）。任一不满足 → None（回退构建自愈）。
    fn validate(bytes: &[u8]) -> Option<[(u32, u32); 3]> {
        if bytes.len() < WI_DATA_OFF || &bytes[..8] != WI_MAGIC.as_slice() {
            return None;
        }
        let mut secs = [(0u32, 0u32); 3];
        let mut prev_end = WI_DATA_OFF as u32;
        for (i, s) in secs.iter_mut().enumerate() {
            let o = WI_TABLE_OFF + i * 8;
            let off = u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
            let len = u32::from_le_bytes([bytes[o + 4], bytes[o + 5], bytes[o + 6], bytes[o + 7]]);
            let end = off.checked_add(len)?;
            if (off as usize) < WI_DATA_OFF
                || end as usize > bytes.len()
                || !(off as usize).is_multiple_of(8)
                || off < prev_end
            {
                return None;
            }
            prev_end = end;
            *s = (off, len);
        }
        if !(secs[WI_SEC_ENTRIES].1 as usize).is_multiple_of(WI_ENTRY_W) {
            return None;
        }
        let n = secs[WI_SEC_ENTRIES].1 as usize / WI_ENTRY_W;
        let u32_at = |sec: usize, idx: usize| -> u32 {
            let o = secs[sec].0 as usize + idx * 4;
            u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
        };
        let u16_at = |sec: usize, idx: usize| -> u16 {
            let o = secs[sec].0 as usize + idx * 2;
            u16::from_le_bytes([bytes[o], bytes[o + 1]])
        };
        let str_at = |blob: usize, off: u32, len: u32| -> Option<&str> {
            if (off as usize).checked_add(len as usize)? > secs[blob].1 as usize {
                return None;
            }
            let o = secs[blob].0 as usize + off as usize;
            std::str::from_utf8(&bytes[o..o + len as usize]).ok()
        };
        let mut prev: Option<&str> = None;
        for i in 0..n {
            // u32 索引：每条目 16B = 4×u32 → 基址 i*4；u16 索引：i*8+4/+5
            let to = u32_at(WI_SEC_ENTRIES, i * 4);
            let po = u32_at(WI_SEC_ENTRIES, i * 4 + 1);
            let tl = u16_at(WI_SEC_ENTRIES, i * 8 + 4);
            let pl = u16_at(WI_SEC_ENTRIES, i * 8 + 5);
            let text = str_at(WI_SEC_TEXT_BLOB, to, tl as u32)?;
            str_at(WI_SEC_PY_BLOB, po, pl as u32)?;
            if let Some(p) = prev {
                if p >= text {
                    return None;
                }
            }
            prev = Some(text);
        }
        Some(secs)
    }

    #[inline]
    fn sec(&self, i: usize) -> &[u8] {
        let (off, len) = self.secs[i];
        &self.backing.bytes()[off as usize..(off + len) as usize]
    }

    /// 第 i 条：(text_off, py_off, text_len, py_len, freq)。
    #[inline]
    fn entry(&self, i: usize) -> (u32, u32, u16, u16, u32) {
        let e = &self.sec(WI_SEC_ENTRIES)[i * WI_ENTRY_W..(i + 1) * WI_ENTRY_W];
        (
            u32::from_le_bytes([e[0], e[1], e[2], e[3]]),
            u32::from_le_bytes([e[4], e[5], e[6], e[7]]),
            u16::from_le_bytes([e[8], e[9]]),
            u16::from_le_bytes([e[10], e[11]]),
            u32::from_le_bytes([e[12], e[13], e[14], e[15]]),
        )
    }

    #[inline]
    fn len(&self) -> usize {
        self.secs[WI_SEC_ENTRIES].1 as usize / WI_ENTRY_W
    }

    #[inline]
    fn text_at(&self, i: usize) -> &str {
        let (to, _, tl, _, _) = self.entry(i);
        let s = &self.sec(WI_SEC_TEXT_BLOB)[to as usize..(to + tl as u32) as usize];
        // 安全：构建方写合法 &str；装载路径经 validate 全量校验
        unsafe { core::str::from_utf8_unchecked(s) }
    }

    #[inline]
    fn pinyin(&self, i: usize) -> &str {
        let (_, po, _, pl, _) = self.entry(i);
        let s = &self.sec(WI_SEC_PY_BLOB)[po as usize..(po + pl as u32) as usize];
        unsafe { core::str::from_utf8_unchecked(s) }
    }

    fn find(&self, text: &str) -> Option<usize> {
        let (mut lo, mut hi) = (0usize, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.text_at(mid).cmp(text) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    #[cfg(test)]
    fn estimated_bytes(&self) -> usize {
        self.secs.iter().map(|s| s.1 as usize).sum()
    }
}

/// 词库结构
#[derive(Clone)]
pub struct Dict {
    /// 构建期 = 全量节点 Trie；finish_load 转储 arena 后 = 运行时 overlay
    /// （清空重来，只承接运行时插入——量级 = 用户词，极小）。
    root: TrieNode,
    /// finish_load 转储的紧凑基底：None = 构建期
    /// （全部走 root，行为与旧版逐位一致）；Some = 双源查询（base+overlay）。
    arena: Option<crate::trie_arena::TrieArena>,
    /// 基底候选的删除墓碑（remove_by_text 对不可变 base 的生效机制）。
    /// 查询物化时按 text 过滤；重插入（insert 命中墓碑 text）时摘除。
    removed_texts: std::collections::HashSet<String>,
    /// 基底候选的 (freq, is_primary) 覆盖：text → pinyin → 覆盖值。
    /// 运行时 insert 命中基底词条（max 合并）与 insert_user_word（强设）写入。
    cand_overrides: HashMap<String, HashMap<String, (u32, bool)>>,
    /// 单字表（用于简拼匹配时快速查找）
    single_chars: Vec<(String, Vec<Candidate>)>,
    /// 统一词索引——构建期形态（finish_load 后清空，由 wi_base/wi_runtime 接管）：
    /// 按 text 字典序排序的 flat Vec。
    /// (text, best_freq, best_pinyin)：同一 text 多音时取 freq 最高的那条。
    /// contains_text / long_word_freq / find_pinyin 全部走 binary_search。
    /// bulk_load 阶段 push 不排序，finish_load() 一次性 sort。
    word_index: Vec<(Arc<str>, u32, Arc<str>)>,
    /// bulk_load 后是否已排序（排序前 binary_search 无效，insert 走 push）
    word_index_sorted: bool,
    /// 词索引基底（span 版，finish_load 转储；运行时不可变）
    wi_base: Option<WordIndexSpans>,
    /// 词索引运行时增量（用户词，条目极少；兼作基底 freq 覆盖的载体——
    /// 条目存在即代表「该 text 的当前权威 freq/pinyin」）
    wi_runtime: Vec<(Arc<str>, u32, Arc<str>)>,
    /// 词索引墓碑（remove_by_text；find_word_entry 过滤）
    wi_removed: std::collections::HashSet<String>,
    /// 节点候选物化缓存：arena 候选 span→Arc 逐候选构造
    /// 是 bench +50% 回归的根源；本缓存按节点 id 数组槽直索引（O(1)，
    /// 零哈希——HashMap 版曾因子树收集单查询访问上万节点 + 容量清空而抖动）
    /// 缓存物化结果（Arc<Vec<Candidate>>，raw 值——不含覆盖/墓碑效果），命中后
    /// `extend(iter().cloned())` 与旧节点 Trie 的候选 clone 同款成本。
    /// 稀疏：空候选节点不占槽（Some 只为有候选的节点而设）； populated 超
    /// 8192 全量扫清（trigram 桶缓存同款「clear 后按需重建」语义）。
    /// RefCell 单线程安全（引擎只在主线程使用）。
    node_cache: std::cell::RefCell<Vec<Option<Arc<Vec<Candidate>>>>>,
    /// node_cache 占用槽计数（超阈值全量扫清的判据，避免每次 miss 全扫）。
    node_cache_populated: std::cell::Cell<usize>,
    /// 单字表音节 → 桶下标的侧索引。
    ///
    /// 背景：insert 的单字去重原按音节线性扫描 single_chars（433 桶），
    /// 启动期 1.1 万单字行 × 平均 216 次字符串比较 ≈ 10ms。侧索引把桶定位
    /// 降到 O(1)。同步约束：single_chars 的一切结构性变更（增/删桶）
    /// 必须经 `single_chars_bucket_mut` / remove_by_text 末尾的索引重建，
    /// 桶内候选的 in-place 修改（freq/is_primary）不影响索引。
    single_chars_index: HashMap<Box<str>, usize>,
}

/// 走树节点句柄：A = arena 基底节点 id，
/// L = overlay 节点引用。Copy 传递（u32 / 引用都廉价），统一 8 条查询
/// 路径的双源遍历。
#[derive(Clone, Copy)]
enum NodeRef<'a> {
    A(u32),
    L(&'a TrieNode),
}

impl Default for Dict {
    fn default() -> Self {
        Self::new()
    }
}

impl Dict {
    /// Trie 内存构成统计（仅测试构建；返回 overlay 增量 + arena/wi_span 基底规模）。
    ///
    /// 确定性计算（不依赖分配器）：节点数 + String/Vec 的堆字节 +
    /// arena/wi_span 的紧凑数组估算。
    #[cfg(test)]
    pub(crate) fn memory_profile(&self) -> TrieMemoryProfile {
        let mut p = TrieMemoryProfile::default();
        Self::profile_node(&self.root, &mut p);
        p.word_index_len = self.word_index.len();
        if let Some(a) = &self.arena {
            p.arena_bytes = a.estimated_bytes();
            p.arena_nodes = a.node_count();
            p.arena_candidates = a.candidate_count();
        }
        if let Some(w) = &self.wi_base {
            p.wi_span_len = w.len();
            p.wi_span_bytes = w.estimated_bytes();
        }
        p.sc_buckets = self.single_chars.len();
        p.sc_candidates = self.single_chars.iter().map(|(_, c)| c.len()).sum();
        p
    }

    #[cfg(test)]
    fn profile_node(node: &TrieNode, p: &mut TrieMemoryProfile) {
        p.node_count += 1;
        p.syllable_heap_bytes += node.syllable.heap_bytes();
        p.candidate_count += node.candidates.len();
        // Vec 容量（构建期指数增长的历史峰值——容量可能远超 len）
        p.children_capacity += node.children.capacity();
        p.candidates_capacity += node.candidates.capacity();
        for c in &node.candidates {
            p.text_heap_bytes += c.text.len();
        }
        // 拼音 Arc 按节点共享：同节点全部候选拼音相同，
        // 只计一次分配（首候选），否则同一分配被重复计数虚高。
        if let Some(first) = node.candidates.first() {
            p.pinyin_heap_bytes += first.pinyin.len();
            p.pinyin_alloc_count += 1;
        }
        for child in &node.children {
            Self::profile_node(child, p);
        }
    }

    /// 构建完成后释放 Vec 多余容量。
    ///
    /// 背景：构建期逐行 insert_child/candidates.push 导致 Vec 指数扩容，容量超配
    /// 实测 2.3-3.1×（children 435k/192k、candidates 589k/187k）——占 Trie 内存
    /// 大头的 64MB。全树一次 shrink 释放多余容量，实测估算 -40MB。
    /// 内容与顺序零变化；运行时低频插入（用户词/词库管理 UI）会重新扩容，可接受。
    pub(crate) fn shrink_to_fit(&mut self) {
        Self::shrink_node(&mut self.root);
        for (_, cands) in &mut self.single_chars {
            cands.shrink_to_fit();
        }
        self.word_index.shrink_to_fit();
    }

    fn shrink_node(node: &mut TrieNode) {
        node.candidates.shrink_to_fit();
        node.children.shrink_to_fit();
        for child in &mut node.children {
            Self::shrink_node(child);
        }
    }

    pub fn new() -> Self {
        Self {
            root: TrieNode::new(""),
            arena: None,
            removed_texts: std::collections::HashSet::new(),
            cand_overrides: HashMap::new(),
            single_chars: Vec::new(),
            word_index: Vec::new(),
            word_index_sorted: false,
            wi_base: None,
            wi_runtime: Vec::new(),
            wi_removed: std::collections::HashSet::new(),
            node_cache: std::cell::RefCell::new(Vec::new()),
            node_cache_populated: std::cell::Cell::new(0),
            single_chars_index: HashMap::new(),
        }
    }

    /// 定位（或创建）单字桶，返回可变引用；同步维护 single_chars_index。
    /// 桶内候选的 in-place 修改走返回值即可（不影响索引）。
    fn single_chars_bucket_mut(&mut self, syllable: &str) -> &mut Vec<Candidate> {
        let idx = match self.single_chars_index.get(syllable) {
            Some(&i) => i,
            None => {
                let i = self.single_chars.len();
                self.single_chars.push((syllable.into(), Vec::new()));
                self.single_chars_index.insert(syllable.into(), i);
                i
            }
        };
        &mut self.single_chars[idx].1
    }

    /// 从内置二进制数据加载词库
    pub fn from_builtin(data: &[u8]) -> Self {
        let mut dict = Self::new();
        dict.load_from_text(core::str::from_utf8(data).unwrap_or(""));
        dict
    }

    // -----------------------------------------------------------------
    // 双源走树（arena 基底 + overlay 节点 Trie 的统一遍历；
    // NodeRef 枚举在 impl 外的模块级声明）
    // -----------------------------------------------------------------

    #[inline]
    fn nr_syllable<'a>(&'a self, nr: NodeRef<'a>) -> &'a str {
        match nr {
            NodeRef::A(id) => self.arena.as_ref().unwrap().child_syllable(id),
            NodeRef::L(n) => n.syllable.as_str(),
        }
    }

    #[inline]
    fn nr_find_child<'a>(&'a self, nr: NodeRef<'a>, syllable: &str) -> Option<NodeRef<'a>> {
        match nr {
            NodeRef::A(id) => self
                .arena
                .as_ref()
                .unwrap()
                .find_child(id, syllable)
                .map(NodeRef::A),
            NodeRef::L(n) => n.find_child(syllable).map(NodeRef::L),
        }
    }

    /// arena 节点候选物化（raw 值，无覆盖/墓碑效果）+ 节点槽缓存。
    /// 命中 = Option<Arc> 直索引（O(1)）；未命中按 span 构造后入槽。
    /// 空候选节点直接返回共享空向量（零分配零占槽——简拼巨子树收集
    /// 访问的节点大半无候选，逐个分配空 Arc 曾是 zgrm 类查询翻倍的根源）。
    fn materialized_node(&self, id: u32) -> Arc<Vec<Candidate>> {
        {
            let cache = self.node_cache.borrow();
            if let Some(Some(v)) = cache.get(id as usize) {
                return v.clone();
            }
        }
        let a = self.arena.as_ref().unwrap();
        let (s, e) = a.cand_range(id);
        if s == e {
            static EMPTY: std::sync::OnceLock<Arc<Vec<Candidate>>> = std::sync::OnceLock::new();
            return EMPTY.get_or_init(|| Arc::new(Vec::new())).clone();
        }
        let v: Vec<Candidate> = (s..e)
            .map(|i| {
                let ((to, tl), (po, pl)) = a.cand_spans(i);
                Candidate {
                    text: Arc::from(a.blob_text(to, tl)),
                    base_freq: a.cand_freq(i),
                    pinyin: Arc::from(a.blob_py(po, pl)),
                    is_primary: a.cand_is_primary(i),
                    is_correction: false,
                    is_sentence: false,
                    is_guess: false,
                    is_shortcut: false,
                    is_english: false,
                    sentence_score: 0,
                }
            })
            .collect();
        let shared = Arc::new(v);
        let mut cache = self.node_cache.borrow_mut();
        if cache.len() <= id as usize {
            cache.resize(id as usize + 1, None);
        }
        // 稀疏上限：占用槽超 8192 → 全量扫清（计数器判据，低频一次性 ~百 µs）
        if self.node_cache_populated.get() > 8192 {
            for slot in cache.iter_mut() {
                *slot = None;
            }
            self.node_cache_populated.set(0);
        }
        if cache[id as usize].is_none() {
            self.node_cache_populated
                .set(self.node_cache_populated.get() + 1);
        }
        cache[id as usize] = Some(shared.clone());
        shared
    }

    fn nr_push_candidates<'a>(&'a self, nr: NodeRef<'a>, out: &mut Vec<Candidate>) {
        match nr {
            NodeRef::L(n) => out.extend(n.candidates.iter().cloned()),
            NodeRef::A(id) => {
                let cached = self.materialized_node(id);
                // 快速路径：无墓碑/覆盖（常态）→ 缓存候选直拷
                //（与旧节点 Trie 的 candidates.clone() 同款成本）
                if self.removed_texts.is_empty() && self.cand_overrides.is_empty() {
                    out.extend(cached.iter().cloned());
                    return;
                }
                for c in cached.iter() {
                    if self.removed_texts.contains(c.text.as_ref()) {
                        continue;
                    }
                    let (freq, primary) = match self
                        .cand_overrides
                        .get(c.text.as_ref())
                        .and_then(|m| m.get(c.pinyin.as_ref()))
                    {
                        Some(&(f, p)) => (f, p),
                        None => (c.base_freq, c.is_primary),
                    };
                    out.push(Candidate {
                        text: c.text.clone(),
                        base_freq: freq,
                        pinyin: c.pinyin.clone(),
                        is_primary: primary,
                        is_correction: false,
                        is_sentence: false,
                        is_guess: false,
                        is_shortcut: false,
                        is_english: false,
                        sentence_score: 0,
                    });
                }
            }
        }
    }

    /// 子树候选收集（collect_candidates 的双源版；2000 上限语义同旧：
    /// 达到上限即不再深入/不再扩展）。
    fn nr_collect_subtree<'a>(&'a self, nr: NodeRef<'a>, result: &mut Vec<Candidate>) {
        if result.len() >= 2000 {
            return;
        }
        self.nr_push_candidates(nr, result);
        self.nr_for_each_child(nr, |child, _| {
            self.nr_collect_subtree(child, result);
        });
    }

    /// 节点本地词级候选（字数 ≥2 过滤；query_tokens 混合分支与
    /// collect_prefix_words_tokens 用——两处都是只收当前节点不深入）。
    fn nr_push_candidates_words<'a>(&'a self, nr: NodeRef<'a>, result: &mut Vec<Candidate>) {
        match nr {
            NodeRef::L(n) => result.extend(
                n.candidates
                    .iter()
                    .filter(|c| c.text.chars().count() >= 2)
                    .cloned(),
            ),
            NodeRef::A(id) => {
                let cached = self.materialized_node(id);
                for c in cached.iter() {
                    if c.text.chars().count() < 2 || self.removed_texts.contains(c.text.as_ref()) {
                        continue;
                    }
                    if self.cand_overrides.is_empty() {
                        result.push(c.clone());
                        continue;
                    }
                    let (freq, primary) = match self
                        .cand_overrides
                        .get(c.text.as_ref())
                        .and_then(|m| m.get(c.pinyin.as_ref()))
                    {
                        Some(&(f, p)) => (f, p),
                        None => (c.base_freq, c.is_primary),
                    };
                    result.push(Candidate {
                        text: c.text.clone(),
                        base_freq: freq,
                        pinyin: c.pinyin.clone(),
                        is_primary: primary,
                        is_correction: false,
                        is_sentence: false,
                        is_guess: false,
                        is_shortcut: false,
                        is_english: false,
                        sentence_score: 0,
                    });
                }
            }
        }
    }

    #[inline]
    fn nr_for_each_child<'a>(&'a self, nr: NodeRef<'a>, mut f: impl FnMut(NodeRef<'a>, &str)) {
        match nr {
            NodeRef::A(id) => self.arena.as_ref().unwrap().for_each_child(id, |cid, s| {
                f(NodeRef::A(cid), s);
            }),
            NodeRef::L(n) => {
                for child in &n.children {
                    f(NodeRef::L(child), child.syllable.as_str());
                }
            }
        }
    }

    /// 双源根（arena 模式 = 基底根 + overlay 根；构建期 = 单 root）。
    #[inline]
    fn roots<'a>(&'a self) -> Vec<NodeRef<'a>> {
        let mut v = Vec::with_capacity(2);
        if self.arena.is_some() {
            v.push(NodeRef::A(crate::trie_arena::TrieArena::ROOT));
        }
        v.push(NodeRef::L(&self.root));
        v
    }

    /// 沿完整音节序列下钻（含韵母归一化），返回两端命中的节点。
    fn walk_both_norm<'a>(&'a self, pinyin_seq: &[&str]) -> Vec<NodeRef<'a>> {
        let mut out = Vec::new();
        for root in self.roots() {
            let mut cur = root;
            let mut ok = true;
            for syllable in pinyin_seq {
                let s = crate::pinyin::normalize_syllable(syllable);
                match self.nr_find_child(cur, s) {
                    Some(n) => cur = n,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                out.push(cur);
            }
        }
        out
    }

    /// 从文本加载（格式：pinyin\ttext\tfreq[\tis_primary]）
    ///
    /// 内置小词库格式。第 4 列 is_primary 可选（1=主音），缺省按字音推断。
    pub fn load_from_text(&mut self, text: &str) {
        for line in text.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }
            let pinyin = parts[0];
            let text = parts[1];
            let freq = parts
                .get(2)
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(100);
            let is_primary = parts
                .get(3)
                .and_then(|s| s.parse::<u32>().ok())
                .map(|v| v != 0)
                .unwrap_or_else(|| {
                    // 未指定时按字音推断（与 importer 行为一致）
                    text.chars()
                        .next()
                        .map(|c| crate::char_freq::is_primary_reading(c, pinyin))
                        .unwrap_or(true)
                });

            self.insert(pinyin, text, freq, is_primary);
        }
    }

    /// 插入词条
    ///
    /// 相同 text + pinyin 的条目会去重，保留更高的 base_freq。
    /// 这样 X1Dict 中重复的条目（如多个繁体对应同一简体+拼音）不会
    /// 在候选框里重复出现，同时用户词更新时新频率会覆盖旧频率。
    ///
    /// `is_primary` 标记该候选当前读音是否为主音（仅对多音单字有意义），
    /// 用于排序时主音字优先。去重时若新旧记录是同一 (text, pinyin)，
    /// 主音标记取「主音优先」（任一为主音则保留主音）。
    pub fn insert(&mut self, pinyin: &str, text: &str, freq: u32, is_primary: bool) {
        // freq 硬上限：任何来源的 freq 都截断到 MAX_FREQ=10000，
        // 防止虚高频词污染 Trie 排序。
        let freq = freq.min(crate::char_freq::MAX_FREQ);
        // arena 模式（finish_load 后）：基底命中 → 覆盖表；未命中 → overlay
        // 插入。构建期（arena None）走下方原路径（行为与旧版逐位一致）。
        if self.arena.is_some() {
            self.insert_runtime(pinyin, text, freq, is_primary);
            return;
        }
        // 栈上音节切分：原 `split_whitespace().collect::<Vec>()`
        // 每行 1 次堆分配（启动 18.7 万行 = 18.7 万次）。词拼音音节数 ≤16 全覆盖
        // （词库最长词 8 字），超长按防御回退堆分配（实际不会触发）。
        let mut syl_buf = [""; 16];
        let mut syl_n = 0;
        let mut overflow = false;
        for s in pinyin.split_whitespace() {
            if syl_n < syl_buf.len() {
                syl_buf[syl_n] = s;
                syl_n += 1;
            } else {
                overflow = true;
                break;
            }
        }
        let heap_syllables;
        let syllables: &[&str] = if overflow {
            heap_syllables = pinyin.split_whitespace().collect::<Vec<_>>();
            &heap_syllables
        } else {
            &syl_buf[..syl_n]
        };
        if syllables.is_empty() {
            return;
        }

        // word_index 复用 Trie 候选的 Arc<str>：
        // Trie 插入时捕获 text/pinyin 的 Arc（新建或既有候选 clone），
        // 词索引自身零独立堆分配（省 187k × 2 次 String 分配 + 结构体 56B→24B）。
        // Trie 循环必达末节点（每层二分定位缺失即就位插入），
        // 两条路径（去重命中/新建）都会捕获，下方 unwrap 恒成立。
        let mut wi_text: Option<Arc<str>> = None;
        let mut wi_pinyin: Option<Arc<str>> = None;

        let is_single_char = syllables.len() == 1 && text.chars().count() == 1;

        let mut node = &mut self.root;
        for (i, &syllable) in syllables.iter().enumerate() {
            // 单次二分定位+就位插入：原实现
            // find_child_mut（判在）+ insert_child（再二分找插入点）+ find_child_mut
            // （取回）= 每层 3 次二分；合并为 1 次，启动期省 ~190k 节点 × 2 层搜索。
            // insert_child 语义 = 二分找 Err 位 + insert，与此处逐位一致。
            let pos = match node
                .children
                .binary_search_by(|n| n.syllable.as_str().cmp(syllable))
            {
                Ok(p) => p,
                Err(p) => {
                    node.children.insert(p, TrieNode::new(syllable));
                    p
                }
            };
            node = &mut node.children[pos];

            // 最后一个音节，添加候选
            if i == syllables.len() - 1 {
                // 去重：同一 text + pinyin 只保留最高频率
                let mut found = false;
                let mut grew = false; // freq 真正变大才可能破坏降序
                for c in node.candidates.iter_mut() {
                    if c.text.as_ref() == text && c.pinyin.as_ref() == pinyin {
                        // 保序守卫：max 合并只在 freq 真正变大时才可能
                        // 破坏降序（种子批量加载按 freq DESC 到达，恒不触发重排）；
                        // sort_by 是稳定排序，已序输入恒等——跳过不改变结果。
                        if freq > c.base_freq {
                            c.base_freq = freq;
                            grew = true;
                        }
                        c.is_primary = c.is_primary || is_primary;
                        wi_text = Some(c.text.clone());
                        wi_pinyin = Some(c.pinyin.clone());
                        found = true;
                        break;
                    }
                }
                if grew {
                    node.candidates
                        .sort_by_key(|x| std::cmp::Reverse(x.base_freq));
                }
                if !found {
                    // 同节点全部候选的完整拼音相同（节点由唯一音节路径定位）——
                    // 共享一份 Arc（省 18.7 万候选各自的重复分配）。
                    // 防御：拼音原文与音节路径可能有空白差异（"zhong  guo"），
                    // 共享前先比对，不等则独立分配。
                    let py: Arc<str> = match node.candidates.first() {
                        Some(first) if first.pinyin.as_ref() == pinyin => first.pinyin.clone(),
                        _ => Arc::from(pinyin),
                    };
                    let text_arc: Arc<str> = text.into();
                    // 保序守卫：追加元素 ≤ 队尾时降序保持（freq DESC 批量加载恒成立），
                    // 仅乱序插入（运行时学词）才重排。结果与无条件 sort 逐位一致。
                    let breaks_order = node
                        .candidates
                        .last()
                        .is_some_and(|last| last.base_freq < freq);
                    node.candidates.push(Candidate {
                        text: text_arc.clone(),
                        base_freq: freq,
                        pinyin: py.clone(),
                        is_primary,
                        is_correction: false,
                        is_sentence: false,
                        is_guess: false,
                        is_shortcut: false,
                        is_english: false,
                        sentence_score: 0,
                    });
                    if breaks_order {
                        node.candidates
                            .sort_by_key(|x| std::cmp::Reverse(x.base_freq));
                    }
                    wi_text = Some(text_arc);
                    wi_pinyin = Some(py);
                }
            }
        }

        // 单字表维护：从 Trie 循环之前移到之后，
        // 候选 text/pinyin 直接 clone Trie 候选已捕获的 Arc（wi_text/wi_pinyin
        // 单字=单音节路径必捕获）——原实现每字各 1-2 次独立堆分配
        // （11k 字 ≈ 0.5MB + 11k 次分配）。桶定位走侧索引（O(1)），
        // 替代原 433 桶线性扫描（启动期 ~10ms）。
        // 去重语义不变：同一音节+同一单字只保留最高频率、主音标记按或合并。
        if is_single_char {
            let bucket = self.single_chars_bucket_mut(syllables[0]);
            match bucket.iter_mut().find(|c| c.text.as_ref() == text) {
                Some(c) => {
                    c.base_freq = c.base_freq.max(freq);
                    c.is_primary = c.is_primary || is_primary;
                }
                None => bucket.push(Candidate {
                    text: wi_text.as_ref().unwrap().clone(),
                    base_freq: freq,
                    pinyin: wi_pinyin.as_ref().unwrap().clone(),
                    is_primary,
                    is_correction: false,
                    is_sentence: false,
                    is_guess: false,
                    is_shortcut: false,
                    is_english: false,
                    sentence_score: 0,
                }),
            }
        }

        // 同步统一词索引（替代原 HashSet + 2×BTreeMap 三份数据）。
        // 同一 text 可能多次插入（不同拼音或更高 freq），保留最大 freq + 对应拼音。
        if self.word_index_sorted {
            // 运行时插入：binary_search 定位（bulk load 后走此路径）
            match self.word_index.binary_search_by(|e| e.0.as_ref().cmp(text)) {
                Ok(idx) => {
                    if freq > self.word_index[idx].1 {
                        self.word_index[idx].1 = freq;
                        self.word_index[idx].2 = wi_pinyin.unwrap();
                    }
                }
                Err(idx) => {
                    self.word_index
                        .insert(idx, (wi_text.unwrap(), freq, wi_pinyin.unwrap()));
                }
            }
        } else {
            // bulk load 阶段：push 不排序（finish_load() 统一 sort）
            // 同 text 重复插入：找末尾最近的（bulk load 时同 text 连续出现）
            if let Some(last) = self.word_index.last_mut() {
                if last.0.as_ref() == text {
                    if freq > last.1 {
                        last.1 = freq;
                        last.2 = wi_pinyin.unwrap();
                    }
                    return;
                }
            }
            self.word_index
                .push((wi_text.unwrap(), freq, wi_pinyin.unwrap()));
        }
    }

    /// 运行时插入（arena 模式）：基底命中 → 覆盖表/墓碑摘除；
    /// 未命中 → overlay 节点 Trie 插入（复用构建期逻辑作用于 root）。
    /// 语义与旧单 Trie 版逐条对齐（max 合并 / 主音或 / 删后重插 = 全新词条）。
    fn insert_runtime(&mut self, pinyin: &str, text: &str, freq: u32, is_primary: bool) {
        let syllables: Vec<&str> = pinyin.split_whitespace().collect();
        if syllables.is_empty() {
            return;
        }
        let is_single_char = syllables.len() == 1 && text.chars().count() == 1;

        // 1) 基底命中判定（路径 + (text, pinyin) 精确）
        let mut base_hit: Option<(u32, bool)> = None;
        if let Some(a) = &self.arena {
            let mut n = crate::trie_arena::TrieArena::ROOT;
            let mut path_ok = true;
            for s in &syllables {
                match a.find_child(n, s) {
                    Some(x) => n = x,
                    None => {
                        path_ok = false;
                        break;
                    }
                }
            }
            if path_ok {
                base_hit = a.find_candidate(n, text, pinyin);
            }
        }

        if let Some((bf, bp)) = base_hit {
            // 基底词条命中：墓碑态 → 重插 = 全新词条；否则 max 合并 + 主音或
            let tombstoned = self.removed_texts.contains(text);
            let (nf, np) = if tombstoned {
                (freq, is_primary)
            } else {
                let cur = self
                    .cand_overrides
                    .get(text)
                    .and_then(|m| m.get(pinyin))
                    .copied()
                    .unwrap_or((bf, bp));
                (cur.0.max(freq), cur.1 || is_primary)
            };
            self.cand_overrides
                .entry(text.to_string())
                .or_default()
                .insert(pinyin.to_string(), (nf, np));
            if tombstoned {
                self.removed_texts.remove(text);
                self.wi_removed.remove(text);
            }
            // 词索引（旧 insert 语义：freq 高于现值才更新；墓碑复活必入）
            let cur_wi = self.find_word_entry(text).map(|(f, _)| f);
            if tombstoned || cur_wi.is_none_or(|cf| nf > cf) {
                self.wi_runtime_upsert(text, nf, pinyin);
            }
            // 单字表维护（同旧 insert 尾部语义：桶当前值 max，独立于覆盖表）
            if is_single_char {
                let bucket = self.single_chars_bucket_mut(syllables[0]);
                match bucket.iter_mut().find(|c| c.text.as_ref() == text) {
                    Some(c) => {
                        c.base_freq = c.base_freq.max(freq);
                        c.is_primary = c.is_primary || is_primary;
                    }
                    None => bucket.push(Candidate {
                        text: Arc::from(text),
                        base_freq: freq,
                        pinyin: Arc::from(pinyin),
                        is_primary,
                        is_correction: false,
                        is_sentence: false,
                        is_guess: false,
                        is_shortcut: false,
                        is_english: false,
                        sentence_score: 0,
                    }),
                }
            }
            return;
        }

        // 2) 未命中基底 → overlay 插入（节点路径与构建期 insert 相同的逻辑，
        //    作用于 root；overlay 量级 = 用户词，极小）
        let mut wi_text: Option<Arc<str>> = None;
        let mut wi_pinyin: Option<Arc<str>> = None;
        let mut node = &mut self.root;
        for (i, &syllable) in syllables.iter().enumerate() {
            let pos = match node
                .children
                .binary_search_by(|n| n.syllable.as_str().cmp(syllable))
            {
                Ok(p) => p,
                Err(p) => {
                    node.children.insert(p, TrieNode::new(syllable));
                    p
                }
            };
            node = &mut node.children[pos];
            if i == syllables.len() - 1 {
                let mut found = false;
                let mut grew = false;
                for c in node.candidates.iter_mut() {
                    if c.text.as_ref() == text && c.pinyin.as_ref() == pinyin {
                        if freq > c.base_freq {
                            c.base_freq = freq;
                            grew = true;
                        }
                        c.is_primary = c.is_primary || is_primary;
                        wi_text = Some(c.text.clone());
                        wi_pinyin = Some(c.pinyin.clone());
                        found = true;
                        break;
                    }
                }
                if grew {
                    node.candidates
                        .sort_by_key(|x| std::cmp::Reverse(x.base_freq));
                }
                if !found {
                    let py: Arc<str> = match node.candidates.first() {
                        Some(first) if first.pinyin.as_ref() == pinyin => first.pinyin.clone(),
                        _ => Arc::from(pinyin),
                    };
                    let text_arc: Arc<str> = text.into();
                    let breaks_order = node
                        .candidates
                        .last()
                        .is_some_and(|last| last.base_freq < freq);
                    node.candidates.push(Candidate {
                        text: text_arc.clone(),
                        base_freq: freq,
                        pinyin: py.clone(),
                        is_primary,
                        is_correction: false,
                        is_sentence: false,
                        is_guess: false,
                        is_shortcut: false,
                        is_english: false,
                        sentence_score: 0,
                    });
                    if breaks_order {
                        node.candidates
                            .sort_by_key(|x| std::cmp::Reverse(x.base_freq));
                    }
                    wi_text = Some(text_arc);
                    wi_pinyin = Some(py);
                }
            }
        }
        if is_single_char {
            let bucket = self.single_chars_bucket_mut(syllables[0]);
            match bucket.iter_mut().find(|c| c.text.as_ref() == text) {
                Some(c) => {
                    c.base_freq = c.base_freq.max(freq);
                    c.is_primary = c.is_primary || is_primary;
                }
                None => bucket.push(Candidate {
                    text: wi_text.as_ref().unwrap().clone(),
                    base_freq: freq,
                    pinyin: wi_pinyin.as_ref().unwrap().clone(),
                    is_primary,
                    is_correction: false,
                    is_sentence: false,
                    is_guess: false,
                    is_shortcut: false,
                    is_english: false,
                    sentence_score: 0,
                }),
            }
        }
        // 词索引：runtime 增量（旧语义：高于现值才更新）
        let (t, p) = (wi_text.unwrap(), wi_pinyin.unwrap());
        let cur = self.find_word_entry(text).map(|(f, _)| f);
        if cur.is_none_or(|cf| freq > cf) {
            self.wi_runtime_upsert_arc(t, freq, p);
        }
    }

    /// 词索引 runtime 增量 upsert（借用 &str 版）。
    fn wi_runtime_upsert(&mut self, text: &str, freq: u32, pinyin: &str) {
        if let Some(e) = self
            .wi_runtime
            .iter_mut()
            .find(|(t, _, _)| t.as_ref() == text)
        {
            if freq > e.1 {
                e.1 = freq;
                e.2 = Arc::from(pinyin);
            }
        } else {
            self.wi_runtime
                .push((Arc::from(text), freq, Arc::from(pinyin)));
        }
    }

    /// 词索引 runtime 增量 upsert（Arc 转移版，overlay 插入路径用）。
    fn wi_runtime_upsert_arc(&mut self, text: Arc<str>, freq: u32, pinyin: Arc<str>) {
        if let Some(e) = self.wi_runtime.iter_mut().find(|(t, _, _)| *t == text) {
            if freq > e.1 {
                e.1 = freq;
                e.2 = pinyin;
            }
        } else {
            self.wi_runtime.push((text, freq, pinyin));
        }
    }

    /// 插入用户词（用户词为主语义）：先走 `insert`（max 合并去重），
    /// 再对同 (text, pinyin) 候选覆盖 freq 为用户词值。
    ///
    /// 背景：rebuild_dict 叠加 user_words 时用 insert 的 max 语义会让系统词 freq
    /// 压过用户词——导入用户词库与系统词冲突时以用户词为主（系统词库仅编辑
    /// 不可导出，用户词库导入导出 + 冲突用户词优先）。
    /// 调用点：rebuild_dict 用户词叠加（导入/学习后重建）。
    pub fn insert_user_word(&mut self, pinyin: &str, text: &str, freq: u32, is_primary: bool) {
        let freq = freq.min(crate::char_freq::MAX_FREQ);
        // arena 模式：insert（覆盖表/overlay 双路径）+ 基底强设；
        // 词索引不在此强设（旧版语义：wi 只受 insert 的 max 更新影响）
        if self.arena.is_some() {
            self.insert(pinyin, text, freq, is_primary);
            // 基底命中 → 覆盖 freq = 用户值（primary 保持当前有效值）
            if let Some(a) = &self.arena {
                let mut n = crate::trie_arena::TrieArena::ROOT;
                let mut path_ok = true;
                for s in pinyin.split_whitespace() {
                    match a.find_child(n, s) {
                        Some(x) => n = x,
                        None => {
                            path_ok = false;
                            break;
                        }
                    }
                }
                if path_ok {
                    if let Some((_bf, bp)) = a.find_candidate(n, text, pinyin) {
                        let primary = self
                            .cand_overrides
                            .get(text)
                            .and_then(|m| m.get(pinyin))
                            .map(|&(_, p)| p)
                            .unwrap_or(bp || is_primary);
                        self.cand_overrides
                            .entry(text.to_string())
                            .or_default()
                            .insert(pinyin.to_string(), (freq, primary));
                    }
                }
            }
            // overlay 命中（新自造词在 root 里）：现有逻辑直接作用于 root
            if let Some(node) = Self::find_node_mut(&mut self.root, pinyin) {
                for c in &mut node.candidates {
                    if c.text.as_ref() == text && c.pinyin.as_ref() == pinyin {
                        c.base_freq = freq;
                        break;
                    }
                }
                node.candidates
                    .sort_by_key(|x| std::cmp::Reverse(x.base_freq));
            }
            // 单字表覆盖（现有代码，双模式通用）
            if text.chars().count() == 1 && !pinyin.contains(' ') {
                if let Some(&i) = self.single_chars_index.get(pinyin) {
                    for c in self.single_chars[i].1.iter_mut() {
                        if c.text.as_ref() == text {
                            c.base_freq = freq;
                            break;
                        }
                    }
                }
            }
            return;
        }
        self.insert(pinyin, text, freq, is_primary);
        // 覆盖 Trie 候选 freq（insert 的 max 已合并，这里修正为用户词为主）
        if let Some(node) = Self::find_node_mut(&mut self.root, pinyin) {
            for c in &mut node.candidates {
                if c.text.as_ref() == text && c.pinyin.as_ref() == pinyin {
                    c.base_freq = freq;
                    break;
                }
            }
            node.candidates
                .sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        }
        // 覆盖单字表（防御：用户词单字场景；用户词库只留多字词但学习路径可能涉及）
        if text.chars().count() == 1 && !pinyin.contains(' ') {
            if let Some(&i) = self.single_chars_index.get(pinyin) {
                for c in self.single_chars[i].1.iter_mut() {
                    if c.text.as_ref() == text {
                        c.base_freq = freq;
                        break;
                    }
                }
            }
        }
    }

    /// 重建单字表侧索引（remove_by_text 移除空桶后调用）。
    fn rebuild_single_chars_index(&mut self) {
        self.single_chars_index.clear();
        for (i, (s, _)) in self.single_chars.iter().enumerate() {
            self.single_chars_index.insert(s.as_str().into(), i);
        }
    }

    /// 按音节路径找可变节点（insert_user_word 覆盖修正用）。
    fn find_node_mut<'a>(node: &'a mut TrieNode, pinyin: &str) -> Option<&'a mut TrieNode> {
        let mut cur = node;
        for syl in pinyin.split_whitespace() {
            cur = cur.find_child_mut(syl)?;
        }
        Some(cur)
    }

    /// 精确查询：给定完整拼音序列，返回候选列表
    pub fn query(&self, pinyin_seq: &[&str]) -> Vec<Candidate> {
        let mut out = Vec::new();
        let nodes = self.walk_both_norm(pinyin_seq);
        for n in nodes {
            self.nr_push_candidates(n, &mut out);
        }
        // arena 模式：双源合并 + 覆盖表可能破坏基底降序 → 稳定排序兜底
        // （构建期/单源由 insert 不变量保证有序，跳过——行为与旧版一致）。
        // 稳定排序对已序输入是恒等变换，eval 逐字一致的前提。
        if self.arena.is_some() {
            out.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        }
        out
    }

    /// 前缀查询：返回给定拼音前缀的所有可能候选
    /// 用于输入过程中实时提示
    pub fn query_prefix(&self, pinyin_seq: &[&str]) -> Vec<Candidate> {
        let nodes = self.walk_both_norm(pinyin_seq);
        // 收集该节点及其所有子孙的候选
        let mut result = Vec::new();
        for n in nodes {
            self.nr_collect_subtree(n, &mut result);
        }

        // 去重并排序
        result.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        result.dedup_by(|a, b| a.text == b.text);
        result.truncate(50); // 最多返回 50 个
        result
    }

    /// 长词延展查询（全拼前缀 → 更长词猜测）。
    ///
    /// 返回拼音以 `syllables` 为完整音节前缀、且音节数严格更多的词：
    /// 走到 `syllables` 节点后收集严格后代（跳过本节点——等长精确词
    /// 是 query() 的职责，混入会与精确结果重复且被误标猜测）。
    /// 例：["hu","peng"] → 狐朋狗友(hu peng gou you)/呼朋引类(hu peng yin lei)。
    ///
    /// 与 query_prefix_filtered（末音节字符前缀、只看下一层）互补：
    /// 那是「最后一音节没打完」的补全，这是「后面还有音节没打」的猜测——
    /// 纯简拼路径（query_tokens all_initials 子树收集）本就具备后者能力，
    /// 本查询补齐全拼侧的对称性。子树收集带 2000 上限（nr_collect_subtree）。
    pub fn query_continuation(&self, syllables: &[&str]) -> Vec<Candidate> {
        let nodes = self.walk_both_norm(syllables);
        let mut result = Vec::new();
        for n in nodes {
            self.nr_for_each_child(n, |child, _| {
                self.nr_collect_subtree(child, &mut result);
            });
        }
        result.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        result.dedup_by(|a, b| a.text == b.text);
        result.truncate(50);
        result
    }

    /// 带前缀过滤的前缀查询
    /// 用于最后一个音节不完整的情况
    /// 例如 syllables=["ni"], prefix="h" → 查 "ni" 的子树中音节以 "h" 开头的候选
    ///
    /// 只检查「下一层」子节点，不继续深入，避免过度联想出 3/4/5 字词。
    pub fn query_prefix_filtered(&self, syllables: &[&str], prefix: &str) -> Vec<Candidate> {
        let nodes = self.walk_both_norm(syllables);

        let mut result = Vec::new();
        for n in nodes {
            self.nr_for_each_child(n, |child, syl| {
                if syl.starts_with(prefix) {
                    self.nr_push_candidates(child, &mut result);
                }
            });
        }

        result.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        result.dedup_by(|a, b| a.text == b.text);
        result.truncate(50);
        result
    }

    /// 单音节前缀查询（输入只有一个不完整音节，如 "h"）
    /// 匹配所有音节以 prefix 开头的候选
    pub fn query_prefix_single(&self, prefix: &str) -> Vec<Candidate> {
        let mut result = Vec::new();
        let mut stack: Vec<NodeRef<'_>> = Vec::new();

        // 从两棵根的子节点开始（根本身无音节）
        for root in self.roots() {
            self.nr_for_each_child(root, |child, _| stack.push(child));
        }

        while let Some(n) = stack.pop() {
            if self.nr_syllable(n).starts_with(prefix) {
                self.nr_collect_subtree(n, &mut result);
            } else {
                self.nr_for_each_child(n, |child, _| stack.push(child));
            }
        }

        result.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        result.dedup_by(|a, b| a.text == b.text);
        result.truncate(50);
        result
    }

    /// 按首字母查询单字候选
    ///
    /// 用于简拼输入时给每个首字母补充单字候选，方便用户逐字自造词。
    /// 返回以该首字母开头的最高频单字（最多 15 个）。
    pub fn query_single_char_by_initial(&self, initial: char) -> Vec<Candidate> {
        let initial_str = initial.to_string();
        let mut result = Vec::new();
        for (syllable, candidates) in &self.single_chars {
            if syllable.starts_with(&initial_str) {
                result.extend(candidates.iter().cloned());
            }
        }
        result.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        result.dedup_by(|a, b| a.text == b.text);
        result.truncate(15);
        result
    }

    /// 查询以 prefix 开头的所有音节对应的单字候选
    ///
    /// 用于改善单字输入体验（如输入 "n" 时补充 "你、那、年" 等单字）。
    /// 完整音节匹配优先于前缀匹配：输入 "hen" 时，"很"（hen）排在
    /// "横"（heng）等仅以 he/hen 为前缀的字前面。
    pub fn query_single_chars(&self, prefix: &str) -> Vec<Candidate> {
        // 归一化 prefix（lue→lve, nue→nve 等），与词库存储的标准音节对齐。
        // 词级 query 有 normalize_syllable，但单字表查询若遗漏归一化，
        // lueyou 的首音节单字「略」（存在 lve 键下）会查不到。
        let normalized_prefix = crate::pinyin::normalize_syllable(prefix);
        let mut exact = Vec::new();
        let mut prefix_matches = Vec::new();
        for (syllable, candidates) in &self.single_chars {
            if syllable == normalized_prefix {
                exact.extend(candidates.iter().cloned());
            } else if syllable.starts_with(normalized_prefix) {
                prefix_matches.extend(candidates.iter().cloned());
            }
        }
        exact.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        exact.dedup_by(|a, b| a.text == b.text);
        prefix_matches.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        prefix_matches.dedup_by(|a, b| a.text == b.text);

        // 精确匹配全部保留（确保用户翻页能找到所有同音字，如 li 的 117 个字）。
        // 前缀延伸分两种（「猜测 ≤3 条」统一上限）：
        // - 精确列表非空 → 延伸是「对后续输入的猜测」（ji→间/接 类），染紫且
        //   ≤3 条（freq 降序取前）——防紫词刷屏；
        // - 精确列表为空（简拼/单声母，如 j、zh、zgrm 的段）→ 前缀匹配是简拼
        //   本质，不染紫，保持前 20 条上限（原行为，避免 liao/liang 占满名额）。
        if !exact.is_empty() {
            prefix_matches.truncate(3);
            for c in prefix_matches.iter_mut() {
                c.is_guess = true;
            }
        } else {
            prefix_matches.truncate(20);
        }

        let mut result = exact;
        result.extend(prefix_matches);
        result.dedup_by(|a, b| a.text == b.text);
        result
    }

    /// 从词库中删除指定文本的所有词条
    ///
    /// 遍历 Trie 和单字表，移除所有 text 匹配的候选。
    pub fn remove_by_text(&mut self, text: &str) {
        Self::remove_from_node(&mut self.root, text);
        if self.arena.is_some() {
            // 基底不可变 → 墓碑（查询物化过滤）；覆盖表随删除清理。
            // 无条件入集合（不扫基底确证存在——全树扫描太贵；条目 = 用户
            // 删过的词数，极小，幂等过滤无正确性影响）。
            self.removed_texts.insert(text.to_string());
            self.cand_overrides.remove(text);
            self.wi_removed.insert(text.to_string());
            self.wi_runtime.retain(|(t, _, _)| t.as_ref() != text);
        } else if self.word_index_sorted {
            if let Ok(idx) = self.word_index.binary_search_by(|e| e.0.as_ref().cmp(text)) {
                self.word_index.remove(idx);
            }
        } else {
            self.word_index.retain(|(t, _, _)| t.as_ref() != text);
        }
        // 单字表：桶内真实移除匹配候选，空桶移除。
        // 旧实现克隆过滤后丢弃结果，桶内候选从未被移除，只有「桶内全是
        // 该词」才整桶移除；现与 Trie 路径（remove_from_node）行为一致
        // （Shift+Backspace 删单字后简拼单字补全不再残留）。
        for (_, candidates) in &mut self.single_chars {
            candidates.retain(|c| c.text.as_ref() != text);
        }
        let buckets_before = self.single_chars.len();
        self.single_chars
            .retain(|(_, candidates)| !candidates.is_empty());
        // 空桶移除会让后续桶下标移位：重建侧索引（删词是低频操作，数百桶重建微秒级）。
        if self.single_chars.len() != buckets_before {
            self.rebuild_single_chars_index();
        }
    }

    /// 查询某文本作为完整词在词库中的最高 base_freq。
    ///
    /// 用于上下文长词匹配 bonus：当 `context_last + candidate_text` 拼成的新文本
    /// 在词库中存在时，返回该长词的 freq 据此给候选加分（词频越高加分越多）。
    /// 不存在返回 None。
    pub fn long_word_freq(&self, text: &str) -> Option<u32> {
        self.find_word_entry(text).map(|(f, _)| f)
    }

    /// 词条总数（统一词索引条数：系统词 + 用户词，运行信息展示用）。
    pub fn entry_count(&self) -> usize {
        if let Some(wi) = &self.wi_base {
            return wi.len() + self.wi_runtime.len();
        }
        self.word_index.len()
    }

    /// 查「上文词 + 候选词」拼成的组合词是否在词库（按需搭配查询）。
    ///
    /// 按需查询替代全量反推：
    /// 用户 commit「你」后输入 `hao`，查「你好」是否存在 + freq。命中则给「好」加分。
    /// - 零启动开销（不预计算）
    /// - 零内存占用（不建 char_bigram 表）
    /// - O(log N) 查询（text_freq_map BTreeMap.get）
    /// - 覆盖 2 字及以上组合（你好/你们好/你知道吗）
    ///
    /// 用于 context_bonus 的字级回退 + DP 搭配判定，替代原 char_bigram_count。
    ///
    /// 读音校验：`curr_pinyin` 非空时，命中组合词后额外校验组合词
    /// 的最后一个音节是否等于 `curr_pinyin`。不一致则返回 None（读音不匹配不算搭配）。
    /// 解决多音字误匹配：用户打 `ni` 查「呢」(读 ni)，但「你呢」在词库的拼音是 `ni ne`
    ///（呢读 ne）——字形拼接「你+呢」命中，但读音 ni≠ne，不应给「呢」加分。
    /// `curr_pinyin` 为空（DP 路径暂未传拼音）时跳过校验，保持原行为。
    pub fn collocation_freq(&self, prev: &str, curr: &str, curr_pinyin: &str) -> Option<u32> {
        if prev.is_empty() || curr.is_empty() {
            return None;
        }
        // 栈上拼接查组合词：原 format! 每查询一次堆分配。
        // 组合词通常 ≤ 8 字（24B），栈缓冲免分配；超长罕见回退堆。
        let mut buf = [0u8; 24];
        let (pb, cb) = (prev.as_bytes(), curr.as_bytes());
        let heap_combined;
        let combined: &str = if pb.len() + cb.len() <= buf.len() {
            buf[..pb.len()].copy_from_slice(pb);
            buf[pb.len()..pb.len() + cb.len()].copy_from_slice(cb);
            // 安全论证：prev/curr 均为合法 UTF-8（词库/候选文本），拼接发生在码点边界；
            // UTF-8 的连续字节（10xxxxxx）只出现在多字节码点内部，prev 末尾的尾字节
            // 与 curr 的首字节（必为首字节，非 10xxxxxx）拼接不会形成非法序列。
            unsafe { core::str::from_utf8_unchecked(&buf[..pb.len() + cb.len()]) }
        } else {
            heap_combined = alloc::format!("{}{}", prev, curr);
            &heap_combined
        };
        self.collocation_freq_lookup(combined, curr_pinyin)
    }

    /// collocation_freq 的查询主体（栈缓冲与堆回退共用）。
    fn collocation_freq_lookup(&self, combined: &str, curr_pinyin: &str) -> Option<u32> {
        let freq = self.find_word_entry(combined).map(|(f, _)| f)?;
        // 读音校验：组合词的最后音节必须等于候选的实际读音。
        // 如「你呢」拼音 ni ne，候选「呢」读 ni → 最后音节 ne ≠ ni → 不匹配，返回 None。
        // find_pinyin_fast O(1) 查侧索引；未命中侧索引保守放行（不误杀）。
        if !curr_pinyin.is_empty() {
            if let Some(combined_pinyin) = self.find_pinyin_fast(combined) {
                let last_syl = combined_pinyin.split_whitespace().last();
                if let Some(last) = last_syl {
                    // curr_pinyin 可能是多音节（多字候选），只校验最后一个音节
                    let curr_last = curr_pinyin.split_whitespace().last().unwrap_or(curr_pinyin);
                    if last != curr_last {
                        return None; // 读音不匹配，不算搭配
                    }
                }
            }
        }
        Some(freq)
    }

    /// 判断某文本是否是词库中的词。
    pub fn contains_text(&self, text: &str) -> bool {
        self.find_word_entry(text).is_some()
    }

    fn remove_from_node(node: &mut TrieNode, text: &str) {
        node.candidates.retain(|c| c.text.as_ref() != text);
        for child in &mut node.children {
            Self::remove_from_node(child, text);
        }
    }

    /// 在词库中查找指定文本的拼音（取最高 base_freq 的拼音）
    ///
    /// 用于用户选择系统多字词时，把该词连同拼音一起写入 `user_words`，
    /// 避免生成 pinyin 为空的用户词记录。
    pub fn find_pinyin(&self, text: &str) -> Option<String> {
        // 统一词索引 O(log n) 查找（word_index 在 insert 时同步维护，
        // 记录每个 text 最高 freq 对应的拼音）。
        if let Some((_, p)) = self.find_word_entry(text) {
            return Some(p.to_string());
        }
        // 回退：全树遍历（防御索引未命中的边界情况，正常路径不应走到这里）。
        let mut best: Option<(String, u32)> = None;
        for child in &self.root.children {
            Self::find_pinyin_in_node(child, "", text, &mut best);
        }
        best.map(|(p, _)| p)
    }

    /// O(log n) 快速查拼音（只用统一索引，不做全树遍历）。
    /// 用于启动时 green_memo 构建（避免 11066 条 × 全树遍历 = 2.5 秒）。
    /// 索引未命中返回 None（调用方用 normalize_pinyin_for_storage 回退）。
    pub fn find_pinyin_fast(&self, text: &str) -> Option<String> {
        self.find_word_entry(text).map(|(_, p)| p.to_string())
    }

    /// O(1) 快速查 text 的最高 base_freq（侧索引，与 find_pinyin_fast 对齐）。
    /// 用于联想候选包装：按 text 查词库拿真实 freq，避免联想词排序失真。
    /// 未命中返回 None（调用方用默认 freq 回退）。
    pub fn freq_for_text(&self, text: &str) -> Option<u32> {
        self.find_word_entry(text).map(|(f, _)| f)
    }

    /// 统一词索引 binary_search（所有文本查找的统一入口）。
    /// 返回 (freq, pinyin)；未排序时返回 None（bulk load 阶段调用方不应查询——
    /// finish_load 前无查询）。arena 模式：runtime 增量（含覆盖权威值）优先 →
    /// 基底 span 二分（墓碑过滤）。
    fn find_word_entry(&self, text: &str) -> Option<(u32, &str)> {
        if let Some(wi) = &self.wi_base {
            for (t, f, p) in &self.wi_runtime {
                if t.as_ref() == text {
                    return Some((*f, p.as_ref()));
                }
            }
            if !self.wi_removed.contains(text) {
                if let Some(i) = wi.find(text) {
                    return Some((wi.entry(i).4, wi.pinyin(i)));
                }
            }
            return None;
        }
        if !self.word_index_sorted {
            return None;
        }
        self.word_index
            .binary_search_by(|e| e.0.as_ref().cmp(text))
            .ok()
            .map(|i| (self.word_index[i].1, self.word_index[i].2.as_ref()))
    }

    /// bulk load 完成后调用：排序统一词索引 + arena 转储。
    /// 同 text 多条（不同拼音）时去重保留 freq 最高的（与原
    /// text_freq_map 语义一致）。
    ///
    /// arena 转储链（顺序不可换）：
    /// 1. 节点 Trie → 紧凑数组基底（精确容量，children 原序 = 音节序保持）；
    /// 2. 词索引 → span 基底（blob+span）——必须先于清空 word_index；
    /// 3. 单字表候选的 Arc 刷新为新分配（脱离旧 Trie 的字符串引用）；
    /// 4. 丢弃节点 Trie（root 清空）——旧内存区（~45MB 存活 + 构建期交错垃圾）
    ///    整体归还系统；arena/blob 全在新分配里，零旧引用是前提。
    /// 只应调用一次（构建完成后）；重复调用直接返回（防御 rebuild 路径误用）。
    pub fn finish_load(&mut self) {
        if self.arena.is_some() {
            return;
        }
        // 键非 Copy（CandidateSortKey），sort_by_key 会要求移动 → 保留 sort_by
        #[allow(clippy::unnecessary_sort_by)]
        self.word_index.sort_by(|a, b| a.0.cmp(&b.0));
        self.word_index.dedup_by(|a, b| {
            if a.0 == b.0 {
                if b.1 > a.1 {
                    *a = b.clone();
                }
                true
            } else {
                false
            }
        });
        self.word_index.shrink_to_fit();
        self.word_index_sorted = true;

        // 1) Trie 基底
        self.arena = Some(crate::trie_arena::TrieArena::from_node_trie(&self.root));
        // 物化缓存槽预建（按节点数；空槽 8B × 19 万 ≈ 1.5MB，换查询 O(1) 直索引）
        *self.node_cache.borrow_mut() =
            alloc::vec![None; self.arena.as_ref().unwrap().node_count()];
        self.node_cache_populated.set(0);
        // 2) 词索引 span 基底（吃掉 187k × 2 个 Arc——旧区零引用的前提；
        //    字节段格式同 arena：磁盘格式=内存格式）
        self.wi_base = Some(WordIndexSpans::build(&self.word_index));
        self.word_index = Vec::new();
        // 3) 单字表 Arc 刷新（11k × 2 次新分配 ~0.5MB；数据逐字不变）
        for (_, bucket) in &mut self.single_chars {
            for c in bucket.iter_mut() {
                let nt = Arc::from(c.text.as_ref());
                let np = Arc::from(c.pinyin.as_ref());
                c.text = nt;
                c.pinyin = np;
            }
        }
        // 4) 丢弃节点 Trie（此 drop 是整个优化的落点——mass free 让堆段归还）
        self.root = TrieNode::new("");
        debug_assert!(self.removed_texts.is_empty() && self.cand_overrides.is_empty());
        debug_assert!(self.wi_runtime.is_empty() && self.wi_removed.is_empty());
    }

    // ---------------- 编译缓存 ----------------

    /// finish_load 完成态的缓存三段：arena 字节 / wi 字节 / single_chars 编码。
    /// 非完成态返回 None（构建期/未加载不缓存）。
    pub(crate) fn cached_blobs(&self) -> Option<(&[u8], &[u8], Vec<u8>)> {
        let a = self.arena.as_ref()?;
        let w = self.wi_base.as_ref()?;
        Some((a.to_bytes(), w.to_bytes(), self.encode_single_chars()))
    }

    /// single_chars 编码（逐位保真：桶序 = 首次创建序、桶内候选序 = 插入序
    /// ——查询侧 freq 稳定排序的平频 tie 序依赖它，重建（按音节 DFS）会漂移，
    /// 故直接序列化结构本身）。
    ///
    /// 布局（LE）：`u32 桶数` × { `u16 音节长 + 字节` + `u16 候选数` ×
    /// { `u16 文本长 + 字节` + `u16 拼音长 + 字节` + `u32 freq` + `u8 primary` } }。
    /// 候选的其余标志（is_correction 等）在 finish_load 态恒为默认值——
    /// debug_assert 守卫，防未来新增运行期标志时静默丢字段。
    fn encode_single_chars(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 * 1024);
        out.extend_from_slice(&(self.single_chars.len() as u32).to_le_bytes());
        for (syl, cands) in &self.single_chars {
            out.extend_from_slice(&(syl.len() as u16).to_le_bytes());
            out.extend_from_slice(syl.as_bytes());
            out.extend_from_slice(&(cands.len() as u16).to_le_bytes());
            for c in cands {
                debug_assert!(
                    !c.is_correction
                        && !c.is_sentence
                        && !c.is_guess
                        && !c.is_shortcut
                        && !c.is_english
                        && c.sentence_score == 0,
                    "single_chars 编码只承载 (text,py,freq,primary)——出现运行期标志即须扩格式"
                );
                out.extend_from_slice(&(c.text.len() as u16).to_le_bytes());
                out.extend_from_slice(c.text.as_bytes());
                out.extend_from_slice(&(c.pinyin.len() as u16).to_le_bytes());
                out.extend_from_slice(c.pinyin.as_bytes());
                out.extend_from_slice(&c.base_freq.to_le_bytes());
                out.push(c.is_primary as u8);
            }
        }
        out
    }

    /// single_chars 解码（加载路径；任何越界/UTF-8 非法/尾随字节 → None，
    /// 调用方整体回退构建路径）。
    fn decode_single_chars(bytes: &[u8]) -> Option<Vec<(String, Vec<Candidate>)>> {
        struct Cur<'a> {
            b: &'a [u8],
            p: usize,
        }
        impl Cur<'_> {
            fn take(&mut self, n: usize) -> Option<&[u8]> {
                let s = self.b.get(self.p..self.p.checked_add(n)?)?;
                self.p += n;
                Some(s)
            }
            fn u16(&mut self) -> Option<u16> {
                let s = self.take(2)?;
                Some(u16::from_le_bytes([s[0], s[1]]))
            }
            fn u32(&mut self) -> Option<u32> {
                let s = self.take(4)?;
                Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
            }
            fn str16(&mut self) -> Option<&str> {
                let n = self.u16()? as usize;
                std::str::from_utf8(self.take(n)?).ok()
            }
        }
        let mut c = Cur { b: bytes, p: 0 };
        let n_buckets = c.u32()? as usize;
        // 防御：桶数上限（真实 ~433）
        if n_buckets > 4096 {
            return None;
        }
        let mut out = Vec::with_capacity(n_buckets);
        for _ in 0..n_buckets {
            let syl = c.str16()?.to_string();
            let n = c.u16()? as usize;
            let mut cands = Vec::with_capacity(n);
            for _ in 0..n {
                let text = Arc::from(c.str16()?);
                let pinyin = Arc::from(c.str16()?);
                let base_freq = c.u32()?;
                let primary = c.take(1)?[0] != 0;
                cands.push(Candidate {
                    text,
                    base_freq,
                    pinyin,
                    is_primary: primary,
                    is_correction: false,
                    is_sentence: false,
                    is_guess: false,
                    is_shortcut: false,
                    is_english: false,
                    sentence_score: 0,
                });
            }
            out.push((syl, cands));
        }
        if c.p != bytes.len() {
            return None; // 尾随垃圾 = 编码不符
        }
        Some(out)
    }

    /// 从编译缓存装载：arena + wi 直接来自缓存字节，
    /// single_chars 解码还原——跳过 SQL 全表扫描 / 节点 Trie 构建 / 转储，
    /// 结果与 `finish_load` 完成态逐位等价。
    /// 任何一段解码失败 → None（调用方回退构建路径自愈）。
    pub(crate) fn from_cached(
        arena: crate::trie_arena::TrieArena,
        wi: WordIndexSpans,
        sc_bytes: &[u8],
    ) -> Option<Self> {
        let single_chars = Self::decode_single_chars(sc_bytes)?;
        let mut index: HashMap<Box<str>, usize> = HashMap::with_capacity(single_chars.len());
        for (i, (s, _)) in single_chars.iter().enumerate() {
            index.insert(s.as_str().into(), i);
        }
        Some(Dict {
            root: TrieNode::new(""),
            node_cache: std::cell::RefCell::new(alloc::vec![None; arena.node_count()]),
            node_cache_populated: std::cell::Cell::new(0),
            single_chars,
            single_chars_index: index,
            wi_base: Some(wi),
            word_index_sorted: true,
            arena: Some(arena),
            ..Dict::new()
        })
    }

    /// 查指定 (pinyin, text) 在 Trie 里的 base_freq。
    /// 用于 user_words 合并时判断词是否已有真实 freq（避免 50000 覆盖）。
    pub fn find_freq(&self, pinyin: &str, text: &str) -> Option<u32> {
        let syllables: Vec<&str> = pinyin.split_whitespace().collect();
        self.query(&syllables)
            .into_iter()
            .find(|c| c.text.as_ref() == text)
            .map(|c| c.base_freq)
    }

    fn find_pinyin_in_node(
        node: &TrieNode,
        prefix: &str,
        text: &str,
        best: &mut Option<(String, u32)>,
    ) {
        let path = if prefix.is_empty() {
            node.syllable.as_str().to_string()
        } else {
            format!("{} {}", prefix, node.syllable.as_str())
        };

        for cand in &node.candidates {
            if cand.text.as_ref() == text && best.as_ref().is_none_or(|(_, b)| cand.base_freq > *b)
            {
                *best = Some((cand.pinyin.as_ref().to_owned(), cand.base_freq));
            }
        }

        for child in &node.children {
            Self::find_pinyin_in_node(child, &path, text, best);
        }
    }

    /// 混合查询：前面是完整音节，后面按首字母简拼匹配
    ///
    /// 例如 complete=["zhong"], initials="gr" → 匹配 "zhong guo ren"（中国人）。
    /// 只返回指定深度的词（complete.len() + initials.len() 个音节），避免过度联想。
    pub fn query_mixed(&self, complete: &[&str], initials: &str) -> Vec<Candidate> {
        if initials.is_empty() {
            return Vec::new();
        }

        // 注意：complete 的下钻不做韵母归一化（与旧版一致——query/query_prefix
        // 归一而 mixed 不归一，是历史行为，逐位保持）
        let mut nodes = Vec::new();
        for root in self.roots() {
            let mut cur = root;
            let mut ok = true;
            for syllable in complete {
                match self.nr_find_child(cur, syllable) {
                    Some(n) => cur = n,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                nodes.push(cur);
            }
        }

        let initials: Vec<char> = initials.chars().collect();
        let mut result = Vec::new();
        let mut stack: Vec<(NodeRef<'_>, usize)> = nodes.into_iter().map(|n| (n, 0)).collect();

        while let Some((n, depth)) = stack.pop() {
            if depth >= initials.len() {
                // 到达目标深度，只收集当前节点候选（不继续深入）
                self.nr_push_candidates(n, &mut result);
                continue;
            }

            let target = initials[depth];
            self.nr_for_each_child(n, |child, syl| {
                if syl.starts_with(target) {
                    stack.push((child, depth + 1));
                }
            });
        }

        result.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        result.dedup_by(|a, b| a.text == b.text);
        result.truncate(50);
        result
    }

    /// 简拼查询：用首字母前缀匹配
    ///
    /// 例如 "zw" 匹配所有首音节以 'z' 开头、第二音节以 'w' 开头的词条，
    /// 如 "zhong wen"（中文）、"zuo wei"（作为）、"zi wo"（自我）。
    pub fn query_jianpin(&self, initials: &str) -> Vec<Candidate> {
        if initials.is_empty() {
            return Vec::new();
        }

        let initials: Vec<char> = initials.chars().collect();
        let mut result = Vec::new();
        let mut stack: Vec<(NodeRef<'_>, usize)> =
            self.roots().into_iter().map(|r| (r, 0)).collect();

        while let Some((node, depth)) = stack.pop() {
            if depth >= initials.len() {
                // 已匹配完所有首字母，收集该节点及其所有子孙
                self.nr_collect_subtree(node, &mut result);
                continue;
            }

            let target = initials[depth];
            self.nr_for_each_child(node, |child, syl| {
                if syl.starts_with(target) {
                    stack.push((child, depth + 1));
                }
            });
        }

        result.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        result.dedup_by(|a, b| a.text == b.text);
        // 不在 dict 层 truncate（曾 truncate(50) 导致用户高频词被 base_freq 挤出）。
        // 交给调用方（query_jianpin_fuzzy 的 rerank_by_freq_then_truncate）用 freq.counts
        // 和 recent_score 重排后截断，确保用户选过的词不被丢失。
        result
    }

    /// 统一 Token 查询：根据 Token 序列下钻 Trie。
    ///
    /// 三种 Token 的下钻策略：
    /// - `Full(syl)`: `find_child(syl)` 精确下钻（韵母归一化）
    /// - `Initial(ch)`: 枚举所有 `syllable.chars().next()==ch` 的子节点
    /// - `Shengmu("zh")`: 枚举所有 `syllable.starts_with("zh")` 的子节点
    ///
    /// 候选收集策略：
    /// - 全 Initial 序列（等同纯简拼）：收集整棵子树（兼容 query_jianpin 行为，
    ///   如 zgrm 既收集「中国人民」也收集「中国人民银行」）
    /// - 含 Full/Shengmu：收集当前深度精确候选 + 沿途 Full 节点的前缀词
    ///   （如 zhonggr 想看到「中国人」也想看到「中国」）
    pub fn query_tokens(&self, tokens: &[Token]) -> Vec<Candidate> {
        if tokens.is_empty() {
            return Vec::new();
        }

        let n = tokens.len();
        let all_initials = tokens.iter().all(|t| matches!(t, Token::Initial(_)));

        let mut result = Vec::new();
        let mut stack: Vec<(NodeRef<'_>, usize)> =
            self.roots().into_iter().map(|r| (r, 0)).collect();

        while let Some((node, depth)) = stack.pop() {
            if depth == n {
                if all_initials {
                    // 纯 Initial 序列：收集整棵子树（兼容 query_jianpin）
                    self.nr_collect_subtree(node, &mut result);
                } else {
                    // 含 Full/Shengmu：只收集当前节点的词级候选（字数 ≥ 2）。
                    // 混合查询不产生单字候选（单字由 process_inner 首音节补充逻辑处理）。
                    self.nr_push_candidates_words(node, &mut result);
                }
                continue;
            }

            match &tokens[depth] {
                Token::Full(syl) => {
                    let s = crate::pinyin::normalize_syllable(syl);
                    if let Some(child) = self.nr_find_child(node, s) {
                        stack.push((child, depth + 1));
                    }
                }
                Token::Initial(ch) => {
                    self.nr_for_each_child(node, |child, syl| {
                        if syl.starts_with(*ch) {
                            stack.push((child, depth + 1));
                        }
                    });
                }
                Token::Shengmu(prefix) => {
                    self.nr_for_each_child(node, |child, syl| {
                        if syl.starts_with(prefix) {
                            stack.push((child, depth + 1));
                        }
                    });
                }
            }
        }

        // 前缀词收集：含 Full/Shengmu 的混合场景，沿途每个前缀节点的候选也收集
        // （如 zhonggr 想看到「中国人」也想看到「中国」）。
        // 用 BFS/DFS 沿 tokens 下钻，每个深度 < n 的节点候选都收集。
        if !all_initials {
            self.collect_prefix_words_tokens(tokens, &mut result);
        }

        // 排序：完全匹配（字数 == token 数）优先于前缀词，再按 freq 降序。
        // 避免高频前缀词（如「中国」freq=5000）把低频完全匹配（如「中国人民」freq=2）
        // 挤出 truncate(50) 范围。混合查询的核心价值是命中完全匹配的长词。
        let token_count = tokens.len();
        result.sort_by(|a, b| {
            let a_full = a.text.chars().count() == token_count;
            let b_full = b.text.chars().count() == token_count;
            match (a_full, b_full) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => b.base_freq.cmp(&a.base_freq),
            }
        });
        result.dedup_by(|a, b| a.text == b.text);
        // 不在 dict 层 truncate（曾 truncate(50) 导致用户低频词被挤出）。
        // 交给调用方（decode_sentence 的 query_seg）用 freq.counts 重排后截断。
        result
    }

    /// tokens 全消费后收集子树（更长的词）——「延长猜测」专用。
    ///
    /// 与 [`Self::query_tokens`] 的区别：后者在 depth==n 只收等长词
    /// （当前节点候选），本方法收该节点整棵子树 = 「输入是某个更长词的前缀」。
    /// 场景（候选回退闪烁）：luanqib（luan qi b）在
    /// 词库里没有等长词，而 luan qi ba zao →乱七八糟 是它的自然延长——延长词
    /// 应以紫词给出预测，使候选在逐键输入过程中保持可见（luanqi →
    /// luanqib → luanqiba 全程同一个词，不再消失又出现）。
    ///
    /// 调用方负责染色（is_guess）、去重与截断（MAX_GUESS_CANDIDATES）。
    pub fn query_tokens_extensions(&self, tokens: &[Token]) -> Vec<Candidate> {
        if tokens.is_empty() {
            return Vec::new();
        }
        let n = tokens.len();
        let mut result = Vec::new();
        let mut stack: Vec<(NodeRef<'_>, usize)> =
            self.roots().into_iter().map(|r| (r, 0)).collect();
        while let Some((node, depth)) = stack.pop() {
            if depth == n {
                // 目标节点整棵子树：等长词 + 更长词（去重合并）
                self.nr_collect_subtree(node, &mut result);
                continue;
            }
            match &tokens[depth] {
                Token::Full(syl) => {
                    let s = crate::pinyin::normalize_syllable(syl);
                    if let Some(child) = self.nr_find_child(node, s) {
                        stack.push((child, depth + 1));
                    }
                }
                Token::Initial(ch) => {
                    self.nr_for_each_child(node, |child, syl| {
                        if syl.starts_with(*ch) {
                            stack.push((child, depth + 1));
                        }
                    });
                }
                Token::Shengmu(prefix) => {
                    self.nr_for_each_child(node, |child, syl| {
                        if syl.starts_with(prefix) {
                            stack.push((child, depth + 1));
                        }
                    });
                }
            }
        }
        result.sort_by_key(|c| std::cmp::Reverse(c.base_freq));
        result.dedup_by(|a, b| a.text == b.text);
        result
    }

    /// Token 查询的前缀词收集：沿 tokens 下钻（Full 精确，Initial/Shengmu 枚举），
    /// 收集每个深度 1..n 的中间节点候选（前缀词，n=目标深度，由主循环已收集）。
    ///
    /// 例：tokens=[Full(zhong), Initial(g), Initial(r)]，
    ///   深度 1（zhong 节点）收集「中」；
    ///   深度 2（zhong→guo 等 g 开头）收集「中国」；
    ///   深度 3（目标深度）由主循环收集「中国人」，这里也收集（去重处理）。
    fn collect_prefix_words_tokens(&self, tokens: &[Token], result: &mut Vec<Candidate>) {
        let n = tokens.len();
        if n <= 1 {
            return; // 单 token 无前缀词
        }
        // BFS：每层记录 (节点, 深度)，对所有可达的深度 1..n 节点收集候选
        let mut stack: Vec<(NodeRef<'_>, usize)> =
            self.roots().into_iter().map(|r| (r, 0)).collect();
        while let Some((node, depth)) = stack.pop() {
            if depth >= n {
                continue; // 超过目标深度，不再下钻
            }
            // 收集当前节点候选（前缀词，深度 ≥ 1；词级过滤防单字污染混合查询，
            // 单字候选由 process_inner 的首音节单字补充逻辑专门处理）
            if depth >= 1 {
                self.nr_push_candidates_words(node, result);
            }
            // 继续下钻到下一层
            match &tokens[depth] {
                Token::Full(syl) => {
                    let s = crate::pinyin::normalize_syllable(syl);
                    if let Some(child) = self.nr_find_child(node, s) {
                        stack.push((child, depth + 1));
                    }
                }
                Token::Initial(ch) => {
                    self.nr_for_each_child(node, |child, syl| {
                        if syl.starts_with(*ch) {
                            stack.push((child, depth + 1));
                        }
                    });
                }
                Token::Shengmu(prefix) => {
                    self.nr_for_each_child(node, |child, syl| {
                        if syl.starts_with(prefix) {
                            stack.push((child, depth + 1));
                        }
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_and_query() {
        let mut dict = Dict::new();
        dict.insert("zhong wen", "中文", 1000, true);
        dict.insert("zhong", "中", 500, true);

        let r = dict.query(&["zhong", "wen"]);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].text.as_ref(), "中文");

        let r2 = dict.query(&["zhong"]);
        assert_eq!(r2[0].text.as_ref(), "中");
    }

    #[test]
    fn test_query_prefix() {
        let mut dict = Dict::new();
        dict.insert("zhong", "中", 500, true);
        dict.insert("zhong wen", "中文", 1000, true);
        dict.insert("zhong guo", "中国", 2000, true);

        let r = dict.query_prefix(&["zhong"]);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn test_text_index_and_long_word_freq() {
        let mut dict = Dict::new();
        dict.insert("xing qi liu", "星期六", 5000, true);
        dict.insert("xing qi tian", "星期天", 4000, true);
        dict.insert("xing qi", "星期", 6000, true);
        dict.insert("liu", "流", 8000, true);
        dict.finish_load(); // 排序统一词索引

        // 文本集合：判断词库词
        assert!(dict.contains_text("星期六"));
        assert!(dict.contains_text("星期"));
        assert!(dict.contains_text("流"));
        assert!(!dict.contains_text("星期流"), "星期流不在词库");
        assert!(!dict.contains_text("星期一"), "星期一未插入");

        // 长词 freq 查询：返回最高 freq
        assert_eq!(dict.long_word_freq("星期六"), Some(5000));
        assert_eq!(dict.long_word_freq("星期天"), Some(4000));
        assert_eq!(dict.long_word_freq("星期流"), None);
        assert_eq!(dict.long_word_freq("不存在的词"), None);
    }

    #[test]
    fn test_text_index_remove_sync() {
        let mut dict = Dict::new();
        dict.insert("xing qi liu", "星期六", 5000, true);
        dict.finish_load();
        assert!(dict.contains_text("星期六"));
        assert_eq!(dict.long_word_freq("星期六"), Some(5000));

        dict.remove_by_text("星期六");
        assert!(!dict.contains_text("星期六"), "remove 后应从索引移除");
        assert_eq!(dict.long_word_freq("星期六"), None);
    }

    /// 回归：remove_by_text 必须真实移除单字表桶内候选。
    /// 旧实现克隆过滤后丢弃结果，多候选桶里的单字从未被移除（删「中」后
    /// query_single_chars("zhong") 仍返回「中」）；现与 Trie 路径一致。
    #[test]
    fn test_remove_by_text_removes_single_char_candidate() {
        let mut dict = Dict::new();
        dict.insert("zhong", "中", 5000, true);
        dict.insert("zhong", "众", 3000, true);
        dict.insert("zhong guo", "中国", 6000, true);
        dict.finish_load();
        assert!(dict
            .query_single_chars("zhong")
            .iter()
            .any(|c| c.text.as_ref() == "中"));

        dict.remove_by_text("中");
        let chars = dict.query_single_chars("zhong");
        assert!(
            chars.iter().all(|c| c.text.as_ref() != "中"),
            "remove 后单字表不应残留「中」: {:?}",
            chars.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        assert!(
            chars.iter().any(|c| c.text.as_ref() == "众"),
            "同桶其他字不受影响"
        );
        assert!(
            dict.query(&["zhong", "guo"])
                .iter()
                .any(|c| c.text.as_ref() == "中国"),
            "词级候选不受影响"
        );

        // 桶空 → 整桶移除 + 侧索引重建后仍可正常插入/查询
        dict.remove_by_text("众");
        assert!(
            dict.query_single_chars("zhong").is_empty(),
            "桶空后查询应为空"
        );
        dict.insert("zhong", "忠", 4000, true);
        assert!(
            dict.query_single_chars("zhong")
                .iter()
                .any(|c| c.text.as_ref() == "忠"),
            "空桶移除后同音节新字应可正常插入（侧索引重建）"
        );
    }

    /// 守护：单字表与 Trie 候选共享 Arc——
    /// 同 (音节,字) 的 text/pinyin 在两张表里必须是同一分配。
    #[test]
    fn test_single_chars_share_trie_arcs() {
        let mut dict = Dict::new();
        dict.insert("zhong", "中", 5000, true);
        dict.insert("zhong", "众", 3000, true);
        let trie_cand = &dict.query(&["zhong"])[0];
        let sc_cand = dict.query_single_chars("zhong");
        let sc_cand = sc_cand.iter().find(|c| c.text.as_ref() == "中").unwrap();
        assert!(
            Arc::ptr_eq(&trie_cand.text, &sc_cand.text),
            "text Arc 应共享（零独立分配）"
        );
        assert!(
            Arc::ptr_eq(&trie_cand.pinyin, &sc_cand.pinyin),
            "pinyin Arc 应共享"
        );
    }

    #[test]
    fn test_text_index_freq_keeps_max() {
        // 同一 text 多次插入（不同 freq）应保留最大 freq
        let mut dict = Dict::new();
        dict.insert("a", "阿", 100, true);
        dict.insert("a", "阿", 500, true);
        dict.insert("a", "阿", 200, true);
        dict.finish_load();
        assert_eq!(dict.long_word_freq("阿"), Some(500));
    }

    /// 测试辅助：构造含「中国」「中国人民」「完成」等词的测试 Trie
    fn build_test_dict() -> Dict {
        let mut d = Dict::new();
        d.insert("zhong guo", "中国", 5000, true);
        d.insert("zhong guo ren min", "中国人民", 1976, true);
        d.insert("zhong guo ren", "中国人", 2500, true);
        d.insert("wan cheng", "完成", 2305, true);
        d.insert("wan cheng le", "完成了", 2183, true);
        d.finish_load();
        d
    }

    #[test]
    fn test_query_tokens_all_full_equivalent_to_query() {
        // 全 Full token 应等同旧 query（精确下钻）
        let d = build_test_dict();
        let tokens = vec![Token::Full("zhong"), Token::Full("guo")];
        let result = d.query_tokens(&tokens);
        assert!(
            result.iter().any(|c| c.text.as_ref() == "中国"),
            "全 Full 应命中「中国」: {:?}",
            result.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_query_tokens_all_initial_equivalent_to_query_jianpin() {
        // 全 Initial token 应等同旧 query_jianpin（收集整棵子树）
        let d = build_test_dict();
        let tokens = vec![
            Token::Initial('z'),
            Token::Initial('g'),
            Token::Initial('r'),
            Token::Initial('m'),
        ];
        let result = d.query_tokens(&tokens);
        assert!(
            result.iter().any(|c| c.text.as_ref() == "中国人民"),
            "全 Initial 应命中「中国人民」: {:?}",
            result.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_query_tokens_mixed_full_then_initial() {
        // Full + Initial 混合（zhonggrm 模式）
        let d = build_test_dict();
        let tokens = vec![
            Token::Full("zhong"),
            Token::Initial('g'),
            Token::Initial('r'),
            Token::Initial('m'),
        ];
        let result = d.query_tokens(&tokens);
        assert!(
            result.iter().any(|c| c.text.as_ref() == "中国人民"),
            "Full+Initial 应命中「中国人民」: {:?}",
            result.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_query_tokens_shengmu_path() {
        // Shengmu(zh) + Initial(g/r/m)：zhgrm 双试的声母路径
        let d = build_test_dict();
        let tokens = vec![
            Token::Shengmu("zh"),
            Token::Initial('g'),
            Token::Initial('r'),
            Token::Initial('m'),
        ];
        let result = d.query_tokens(&tokens);
        assert!(
            result.iter().any(|c| c.text.as_ref() == "中国人民"),
            "Shengmu(zh)+Initial 应命中「中国人民」: {:?}",
            result.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_query_tokens_mixed_collects_prefix_words() {
        // 含 Full 的混合应收集前缀词（zhonggr 应看到「中国」前缀词）
        let d = build_test_dict();
        let tokens = vec![
            Token::Full("zhong"),
            Token::Initial('g'),
            Token::Initial('r'),
        ];
        let result = d.query_tokens(&tokens);
        // 「中国人」是精确候选（depth==3），「中国」是前缀词（depth==2 的 Full 节点候选）
        assert!(
            result.iter().any(|c| c.text.as_ref() == "中国人"),
            "zhonggr 应命中「中国人」: {:?}",
            result.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        assert!(
            result.iter().any(|c| c.text.as_ref() == "中国"),
            "zhonggr 应含前缀词「中国」: {:?}",
            result.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }
}
