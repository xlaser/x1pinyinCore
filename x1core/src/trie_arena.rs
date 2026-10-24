//! Trie arena：词典 Trie 的紧凑数组化基底（字节段化布局，
//! 兼作编译缓存/mmap 的表示层）。
//!
//! **为什么**：节点 Trie 是 19 万节点 × 3 个独立堆分配（SmallStr 内联但
//! children/candidates 两个 Vec 各一堆缓冲），存活约 45MB + 构建期 shrink
//! 拷贝与存活分配交错 → ~50MB 不可归还碎片（HeapCompact 无效）。arena 把整棵
//! 树压进**少数几个大 Vec**（精确容量，零增长垃圾），碎片来源整体消失。
//!
//! **字节段布局**：整棵树 = 一块 `bytes` + 13 个固定序段
//! （段表在文件头，见 `Sec`/常量）。**磁盘格式 = 内存格式**（单一事实源）：
//! - 自有构建（`from_node_trie`）：两遍法（精确计数 → layout → 直写缓冲，
//!   零 realloc、零装配拷贝）；
//! - 装载（`from_bytes` / `from_mapped_range`）：一次全量校验后直接索引；
//! - 缓存读写 = 读写这块 bytes，无转译层。
//!
//! 数字一律 little-endian 显式编码（`from_le_bytes` 读取，无对齐要求——
//! 映射页与 Vec<u8> 共享同一套访问器：段内字节偏移算术）。
//! 字符串 = blob + (off,len) span（span 相对所在段起点）。
//!
//! **不可变基底 + Dict 层 overlay**：arena 构建后不再修改。运行时插入走 Dict
//! 的 overlay 节点 Trie（量级 = 用户词，极小）；基底条目的 freq 修正/主音合并
//! 走 Dict 的覆盖表；删除走墓碑集合（查询期过滤）。重启自然重整（DB 是唯一
//! 持久层）——与 NgramStore 的 base/overlay 分层同构（计算存储分离）。
//! 用户词不进基底（缓存键按系统词库哈希成立的前提）。
//!
//! **校验纪律**：`from_bytes`/`from_mapped_range` 的校验通过 = 查询路径
//! `from_utf8_unchecked`/直接索引的安全前提（思路同 BigramCsrTable 缓存）：
//! 段界精确、孩子 id 界内且**每节点孩子音节严格升序**
//! （二分前提）、全部 span 界内 + UTF-8 合法。

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::dict::TrieNode;
use crate::mmap_file::MmapRegion;

/// 段索引（固定序；磁盘/内存共用）。
const SEC_SYLL_BLOB: usize = 0;
const SEC_SYLL_SPANS: usize = 1;
const SEC_FIRST_CHILD: usize = 2;
const SEC_N_CHILDREN: usize = 3;
const SEC_CAND_START: usize = 4;
const SEC_N_CANDS: usize = 5;
const SEC_CHILD_IDS: usize = 6;
const SEC_CAND_FREQ: usize = 7;
const SEC_CAND_PRIMARY: usize = 8;
const SEC_TEXT_BLOB: usize = 9;
const SEC_TEXT_SPANS: usize = 10;
const SEC_PY_BLOB: usize = 11;
const SEC_PY_SPANS: usize = 12;
const N_SECTIONS: usize = 13;

/// 魔数（布局变更时 bump 末位；旧缓存校验失败走构建自愈）。
const MAGIC: &[u8; 8] = b"X1ARENA1";
/// 段表偏移（magic 之后）。
const TABLE_OFF: usize = 8;
/// 数据区起点（8 对齐：段偏移随之全部 8 对齐）。
const DATA_OFF: usize = TABLE_OFF + N_SECTIONS * 8;

/// 段位置（相对 bytes 起点）。
#[derive(Clone, Copy)]
struct Sec {
    off: u32,
    len: u32,
}

/// 底层字节来源：自有缓冲（构建/读入）或只读文件映射。
/// 两者共享同一套段访问器；`Clone` 均为 Arc 浅拷（arena 不可变，共享安全）。
#[derive(Clone)]
pub(crate) enum Backing {
    /// 自有字节。构建路径与「缓存读入」路径（mmap 不可用时的回退）。
    Owned(Arc<[u8]>),
    /// 文件映射切片：`region[off..off+len]` 即本 blob（映射生命周期由 Arc 守）。
    Mapped {
        region: Arc<MmapRegion>,
        off: usize,
        len: usize,
    },
}

impl Backing {
    #[inline]
    pub(crate) fn bytes(&self) -> &[u8] {
        match self {
            Backing::Owned(b) => b,
            Backing::Mapped { region, off, len } => &region.bytes()[*off..*off + *len],
        }
    }
}

/// Clone 供 Dict 的 derive(Clone)（引擎快照/测试用；Arc 浅拷 + 映射共享）。
#[derive(Clone)]
pub(crate) struct TrieArena {
    backing: Backing,
    secs: [Sec; N_SECTIONS],
}

/// 容量统计（第一遍数出，第二遍精确 layout + 直写——构建期零 realloc/零装配）。
struct ArenaCounts {
    nodes: usize,
    children: usize,
    cands: usize,
    syll_bytes: usize,
    text_bytes: usize,
    py_bytes: usize,
}

impl TrieArena {
    pub(crate) const ROOT: u32 = 0;

    // ---------------- 构建 ----------------

    fn count_node(node: &TrieNode, c: &mut ArenaCounts) {
        c.nodes += 1;
        c.children += node.children.len();
        c.cands += node.candidates.len();
        c.syll_bytes += node.syllable.as_str().len();
        for cand in &node.candidates {
            c.text_bytes += cand.text.len();
            c.py_bytes += cand.pinyin.len();
        }
        for child in &node.children {
            Self::count_node(child, c);
        }
    }

    /// 从节点 Trie 转储（finish_load 调用；children 已按音节有序、candidates
    /// 已按 freq 降序——insert 的不变量，转储保持原序）。
    pub(crate) fn from_node_trie(root: &TrieNode) -> Self {
        let mut c = ArenaCounts {
            nodes: 0,
            children: 0,
            cands: 0,
            syll_bytes: 0,
            text_bytes: 0,
            py_bytes: 0,
        };
        Self::count_node(root, &mut c);
        let mut b = Builder::new(&c);
        b.dump(root);
        TrieArena {
            backing: Backing::Owned(Arc::from(b.buf)),
            secs: b.secs,
        }
    }

    // ---------------- 装载（缓存/mmap） ----------------

    /// 自有字节装载（校验通过才返回）。`bytes` 原样持有。
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Option<Self> {
        let secs = Self::validate(&bytes)?;
        Some(TrieArena {
            backing: Backing::Owned(Arc::from(bytes)),
            secs,
        })
    }

    /// 映射区切片装载（`region[off..off+len]`）。校验通过才返回。
    pub(crate) fn from_mapped_range(
        region: Arc<MmapRegion>,
        off: usize,
        len: usize,
    ) -> Option<Self> {
        if off.checked_add(len)? > region.len() {
            return None;
        }
        let bytes = &region.bytes()[off..off + len];
        let secs = Self::validate(bytes)?;
        Some(TrieArena {
            backing: Backing::Mapped { region, off, len },
            secs,
        })
    }

    /// 本 arena 的序列化字节（含头 + 段表；缓存直接落盘这块）。
    pub(crate) fn to_bytes(&self) -> &[u8] {
        self.backing.bytes()
    }

    /// 全量校验：通过 = 查询路径直接索引/`from_utf8_unchecked` 的安全前提。
    /// 任何一处不满足 → None（调用方回退构建路径自愈）。
    fn validate(bytes: &[u8]) -> Option<[Sec; N_SECTIONS]> {
        if bytes.len() < DATA_OFF || &bytes[..8] != MAGIC.as_slice() {
            return None;
        }
        let mut secs = [Sec { off: 0, len: 0 }; N_SECTIONS];
        let mut prev_end = DATA_OFF as u32;
        for (i, s) in secs.iter_mut().enumerate() {
            let o = TABLE_OFF + i * 8;
            let off = u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
            let len = u32::from_le_bytes([bytes[o + 4], bytes[o + 5], bytes[o + 6], bytes[o + 7]]);
            let end = off.checked_add(len)?;
            if (off as usize) < DATA_OFF
                || end as usize > bytes.len()
                || !(off as usize).is_multiple_of(8)
                || off < prev_end
            {
                return None; // 越界 / 未对齐 / 段乱序重叠
            }
            prev_end = end;
            *s = Sec { off, len };
        }
        // 段尺寸自洽（固定元素宽度的段长必须整除且互相对齐）
        let n_nodes = secs[SEC_SYLL_SPANS].len as usize / 8;
        if n_nodes == 0
            || !(secs[SEC_SYLL_SPANS].len as usize).is_multiple_of(8)
            || secs[SEC_FIRST_CHILD].len as usize != n_nodes * 4
            || secs[SEC_N_CHILDREN].len as usize != n_nodes * 2
            || secs[SEC_CAND_START].len as usize != n_nodes * 4
            || secs[SEC_N_CANDS].len as usize != n_nodes * 2
            || !(secs[SEC_CHILD_IDS].len as usize).is_multiple_of(4)
        {
            return None;
        }
        let n_cands = secs[SEC_CAND_FREQ].len as usize / 4;
        if !(secs[SEC_CAND_FREQ].len as usize).is_multiple_of(4)
            || secs[SEC_CAND_PRIMARY].len as usize != n_cands
            || secs[SEC_TEXT_SPANS].len as usize != n_cands * 8
            || secs[SEC_PY_SPANS].len as usize != n_cands * 8
        {
            return None;
        }
        let n_children = secs[SEC_CHILD_IDS].len as usize / 4;

        // 逐节点深校验（借用 bytes 的只读视图；此处不能走 from_utf8_unchecked——
        // 校验本身用 checked 版本，通过后才解锁未检查读取）
        let u32_at = |sec: usize, idx: usize| -> u32 {
            let o = secs[sec].off as usize + idx * 4;
            u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
        };
        let u16_at = |sec: usize, idx: usize| -> u16 {
            let o = secs[sec].off as usize + idx * 2;
            u16::from_le_bytes([bytes[o], bytes[o + 1]])
        };
        let str_at = |blob: usize, off: u32, len: u32| -> Option<&str> {
            if (off as usize).checked_add(len as usize)? > secs[blob].len as usize {
                return None;
            }
            let o = secs[blob].off as usize + off as usize;
            std::str::from_utf8(&bytes[o..o + len as usize]).ok()
        };
        let span_at = |sec: usize, idx: usize| -> (u32, u32) {
            (u32_at(sec, idx * 2), u32_at(sec, idx * 2 + 1))
        };

        // 根音节必须为空串（偏位即拒；root 由构建方保证）
        let (o, l) = span_at(SEC_SYLL_SPANS, 0);
        if !str_at(SEC_SYLL_BLOB, o, l)?.is_empty() {
            return None;
        }
        for node in 0..n_nodes {
            let s = u32_at(SEC_FIRST_CHILD, node) as usize;
            let n = u16_at(SEC_N_CHILDREN, node) as usize;
            if s + n > n_children {
                return None;
            }
            // 孩子段：id 界内 + 音节严格升序（binary_search 前提）
            let mut prev: Option<&str> = None;
            for i in s..s + n {
                let cid = u32_at(SEC_CHILD_IDS, i);
                if cid as usize >= n_nodes {
                    return None;
                }
                let (o, l) = span_at(SEC_SYLL_SPANS, cid as usize);
                let syl = str_at(SEC_SYLL_BLOB, o, l)?;
                if let Some(p) = prev {
                    if p >= syl {
                        return None; // 非严格升序
                    }
                }
                prev = Some(syl);
            }
            // 候选段：界内 + 每条候选的 text/py span UTF-8 合法
            let cs = u32_at(SEC_CAND_START, node) as usize;
            let cn = u16_at(SEC_N_CANDS, node) as usize;
            if cs + cn > n_cands {
                return None;
            }
            for i in cs..cs + cn {
                let (to, tl) = span_at(SEC_TEXT_SPANS, i);
                str_at(SEC_TEXT_BLOB, to, tl)?;
                let (po, pl) = span_at(SEC_PY_SPANS, i);
                str_at(SEC_PY_BLOB, po, pl)?;
            }
        }
        Some(secs)
    }

    // ---------------- 段读取（校验通过后的免检查路径） ----------------

    #[inline]
    fn sec(&self, i: usize) -> &[u8] {
        let s = self.secs[i];
        &self.backing.bytes()[s.off as usize..(s.off + s.len) as usize]
    }

    #[inline]
    fn u32_at(&self, sec: usize, idx: usize) -> u32 {
        let s = self.sec(sec);
        let o = idx * 4;
        u32::from_le_bytes([s[o], s[o + 1], s[o + 2], s[o + 3]])
    }

    #[inline]
    fn u16_at(&self, sec: usize, idx: usize) -> u16 {
        let s = self.sec(sec);
        let o = idx * 2;
        u16::from_le_bytes([s[o], s[o + 1]])
    }

    #[inline]
    fn u8_at(&self, sec: usize, idx: usize) -> u8 {
        self.sec(sec)[idx]
    }

    #[inline]
    fn span_at(&self, sec: usize, idx: usize) -> (u32, u32) {
        (self.u32_at(sec, idx * 2), self.u32_at(sec, idx * 2 + 1))
    }

    #[inline]
    fn str_at(&self, blob: usize, off: u32, len: u32) -> &str {
        let s = &self.sec(blob)[off as usize..(off + len) as usize];
        // 安全：span 由构建方从合法 &str 写入；装载路径经 validate 全量校验
        unsafe { core::str::from_utf8_unchecked(s) }
    }

    /// 节点音节（= span 切 Syll blob）。
    #[inline]
    fn syll(&self, id: u32) -> &str {
        let (o, l) = self.span_at(SEC_SYLL_SPANS, id as usize);
        self.str_at(SEC_SYLL_BLOB, o, l)
    }

    /// 节点总数（finish_load 预建物化缓存槽用）。
    #[inline]
    pub(crate) fn node_count(&self) -> usize {
        self.secs[SEC_SYLL_SPANS].len as usize / 8
    }

    #[cfg(test)]
    pub(crate) fn candidate_count(&self) -> usize {
        self.secs[SEC_CAND_FREQ].len as usize / 4
    }

    /// 诊断：段字节合计（blob 与平行数组的精确占用）。
    #[cfg(test)]
    pub(crate) fn estimated_bytes(&self) -> usize {
        self.secs.iter().map(|s| s.len as usize).sum()
    }

    #[inline]
    pub(crate) fn find_child(&self, node: u32, syllable: &str) -> Option<u32> {
        let s = self.u32_at(SEC_FIRST_CHILD, node as usize) as usize;
        let n = self.u16_at(SEC_N_CHILDREN, node as usize) as usize;
        // 手写二分（字节底座不能借用 &[u32] 切片；语义与 slice::binary_search 一致）
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let cid = self.u32_at(SEC_CHILD_IDS, s + mid);
            match self.syll(cid).cmp(syllable) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(cid),
            }
        }
        None
    }

    /// 谓词遍历孩子（Initial/Shengmu/prefix_filtered 枚举用）。
    #[inline]
    pub(crate) fn for_each_child(&self, node: u32, mut f: impl FnMut(u32, &str)) {
        let s = self.u32_at(SEC_FIRST_CHILD, node as usize) as usize;
        let n = self.u16_at(SEC_N_CHILDREN, node as usize) as usize;
        for i in s..s + n {
            let id = self.u32_at(SEC_CHILD_IDS, i);
            f(id, self.syll(id));
        }
    }

    #[inline]
    pub(crate) fn child_syllable(&self, node: u32) -> &str {
        self.syll(node)
    }

    #[cfg(test)]
    #[inline]
    pub(crate) fn cand_count(&self, node: u32) -> usize {
        self.u16_at(SEC_N_CANDS, node as usize) as usize
    }

    /// 节点候选的平铺区间 [start, end)（Dict 层按此区间逐条物化——
    /// 覆盖表/墓碑在 Dict 手里，arena 不持有可变状态）。
    #[inline]
    pub(crate) fn cand_range(&self, node: u32) -> (usize, usize) {
        let s = self.u32_at(SEC_CAND_START, node as usize) as usize;
        (s, s + self.u16_at(SEC_N_CANDS, node as usize) as usize)
    }

    /// 深度为 depth 的子树节点数（诊断用）。
    #[cfg(test)]
    pub(crate) fn subtree_nodes(&self, node: u32) -> usize {
        let s = self.u32_at(SEC_FIRST_CHILD, node as usize) as usize;
        let n = self.u16_at(SEC_N_CHILDREN, node as usize) as usize;
        1 + (0..n)
            .map(|i| self.subtree_nodes(self.u32_at(SEC_CHILD_IDS, s + i)))
            .sum::<usize>()
    }

    #[inline]
    pub(crate) fn cand_text(&self, flat: usize) -> &str {
        let (o, l) = self.span_at(SEC_TEXT_SPANS, flat);
        self.str_at(SEC_TEXT_BLOB, o, l)
    }

    #[inline]
    pub(crate) fn cand_pinyin(&self, flat: usize) -> &str {
        let (o, l) = self.span_at(SEC_PY_SPANS, flat);
        self.str_at(SEC_PY_BLOB, o, l)
    }

    /// 按 span 直接切 text blob（intern 物化用；安全性同 cand_text）。
    #[inline]
    pub(crate) fn blob_text(&self, o: u32, l: u32) -> &str {
        self.str_at(SEC_TEXT_BLOB, o, l)
    }

    /// 按 span 直接切 pinyin blob（intern 物化用）。
    #[inline]
    pub(crate) fn blob_py(&self, o: u32, l: u32) -> &str {
        self.str_at(SEC_PY_BLOB, o, l)
    }

    #[inline]
    pub(crate) fn cand_freq(&self, flat: usize) -> u32 {
        self.u32_at(SEC_CAND_FREQ, flat)
    }

    #[inline]
    pub(crate) fn cand_is_primary(&self, flat: usize) -> bool {
        self.u8_at(SEC_CAND_PRIMARY, flat) != 0
    }

    /// 候选的 text/pinyin span（Dict 物化 intern 缓存的定位输入）。
    #[inline]
    pub(crate) fn cand_spans(&self, flat: usize) -> ((u32, u32), (u32, u32)) {
        (
            self.span_at(SEC_TEXT_SPANS, flat),
            self.span_at(SEC_PY_SPANS, flat),
        )
    }

    /// 在节点候选段内精确找 (text, pinyin)（Dict 运行时 insert 判基底命中用）。
    /// 返回 (freq, is_primary)。
    pub(crate) fn find_candidate(
        &self,
        node: u32,
        text: &str,
        pinyin: &str,
    ) -> Option<(u32, bool)> {
        let (s, e) = self.cand_range(node);
        for i in s..e {
            if self.cand_text(i) == text && self.cand_pinyin(i) == pinyin {
                return Some((self.cand_freq(i), self.cand_is_primary(i)));
            }
        }
        None
    }
}

// ---------------- 构建器（两遍法：精确 layout + 直写，零装配拷贝） ----------------

struct Builder {
    buf: Vec<u8>,
    secs: [Sec; N_SECTIONS],
    /// 各段写入游标（绝对字节偏移）
    nexts: [usize; N_SECTIONS],
}

impl Builder {
    fn new(c: &ArenaCounts) -> Self {
        debug_assert!(c.nodes <= u32::MAX as usize);
        let sizes: [usize; N_SECTIONS] = [
            c.syll_bytes,   // SYLL_BLOB
            c.nodes * 8,    // SYLL_SPANS
            c.nodes * 4,    // FIRST_CHILD
            c.nodes * 2,    // N_CHILDREN
            c.nodes * 4,    // CAND_START
            c.nodes * 2,    // N_CANDS
            c.children * 4, // CHILD_IDS
            c.cands * 4,    // CAND_FREQ
            c.cands,        // CAND_PRIMARY（u8）
            c.text_bytes,   // TEXT_BLOB
            c.cands * 8,    // TEXT_SPANS
            c.py_bytes,     // PY_BLOB
            c.cands * 8,    // PY_SPANS
        ];
        let mut secs = [Sec { off: 0, len: 0 }; N_SECTIONS];
        let mut off = DATA_OFF;
        for (i, size) in sizes.iter().enumerate() {
            secs[i] = Sec {
                off: off as u32,
                len: *size as u32,
            };
            off += size;
            off = (off + 7) & !7usize; // 段 8 对齐（读取不依赖，防未来对齐型读取器）
        }
        let mut buf = vec![0u8; off];
        buf[..8].copy_from_slice(MAGIC);
        for (i, s) in secs.iter().enumerate() {
            let o = TABLE_OFF + i * 8;
            buf[o..o + 4].copy_from_slice(&s.off.to_le_bytes());
            buf[o + 4..o + 8].copy_from_slice(&s.len.to_le_bytes());
        }
        let nexts = secs.map(|s| s.off as usize);
        Builder { buf, secs, nexts }
    }

    #[inline]
    fn push_bytes(&mut self, sec: usize, bytes: &[u8]) {
        let p = self.nexts[sec];
        self.buf[p..p + bytes.len()].copy_from_slice(bytes);
        self.nexts[sec] = p + bytes.len();
    }

    #[inline]
    fn push_u16(&mut self, sec: usize, v: u16) {
        self.push_bytes(sec, &v.to_le_bytes());
    }

    #[inline]
    fn push_u32(&mut self, sec: usize, v: u32) {
        self.push_bytes(sec, &v.to_le_bytes());
    }

    #[inline]
    fn push_u8(&mut self, sec: usize, v: u8) {
        self.push_bytes(sec, &[v]);
    }

    /// 段内已写长度（span 以此相对段起点）。
    #[inline]
    fn written(&self, sec: usize) -> usize {
        self.nexts[sec] - self.secs[sec].off as usize
    }

    #[inline]
    fn write_u32_at(&mut self, sec: usize, byte_off_in_sec: usize, v: u32) {
        let p = self.secs[sec].off as usize + byte_off_in_sec;
        self.buf[p..p + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// DFS 前序转储，返回本节点 id。孩子段先占位后回填（CSR 连续性）。
    fn dump(&mut self, node: &TrieNode) -> u32 {
        let id = (self.nexts[SEC_SYLL_SPANS] - self.secs[SEC_SYLL_SPANS].off as usize) as u32 / 8;
        let syl = node.syllable.as_str();
        let so = self.written(SEC_SYLL_BLOB) as u32;
        self.push_bytes(SEC_SYLL_BLOB, syl.as_bytes());
        self.push_u32(SEC_SYLL_SPANS, so);
        self.push_u32(SEC_SYLL_SPANS, syl.len() as u32);

        debug_assert!(node.children.len() <= u16::MAX as usize);
        debug_assert!(node.candidates.len() <= u16::MAX as usize);
        self.push_u32(SEC_FIRST_CHILD, 0); // 占位，末尾回填
        self.push_u16(SEC_N_CHILDREN, node.children.len() as u16);
        let cs = (self.written(SEC_CAND_FREQ) / 4) as u32;
        self.push_u32(SEC_CAND_START, cs);
        self.push_u16(SEC_N_CANDS, node.candidates.len() as u16);
        for cand in &node.candidates {
            let to = self.written(SEC_TEXT_BLOB) as u32;
            self.push_bytes(SEC_TEXT_BLOB, cand.text.as_bytes());
            self.push_u32(SEC_TEXT_SPANS, to);
            self.push_u32(SEC_TEXT_SPANS, cand.text.len() as u32);
            let po = self.written(SEC_PY_BLOB) as u32;
            self.push_bytes(SEC_PY_BLOB, cand.pinyin.as_bytes());
            self.push_u32(SEC_PY_SPANS, po);
            self.push_u32(SEC_PY_SPANS, cand.pinyin.len() as u32);
            self.push_u32(SEC_CAND_FREQ, cand.base_freq);
            self.push_u8(SEC_CAND_PRIMARY, cand.is_primary as u8);
        }
        // 孩子段占位 → 递归回填（段在 child_ids 连续，段内顺序 = children 原序 = 音节序）
        let seg = (self.written(SEC_CHILD_IDS) / 4) as u32;
        for _ in &node.children {
            self.push_u32(SEC_CHILD_IDS, 0);
        }
        self.write_u32_at(SEC_FIRST_CHILD, id as usize * 4, seg);
        for (k, child) in node.children.iter().enumerate() {
            let cid = self.dump(child);
            self.write_u32_at(SEC_CHILD_IDS, (seg as usize + k) * 4, cid);
        }
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::Candidate;
    use std::sync::Arc;

    fn node(syl: &str, cands: &[(&str, u32, &str)], children: &[TrieNode]) -> TrieNode {
        let mut n = TrieNode::new(syl);
        for &(t, f, p) in cands {
            n.candidates.push(Candidate {
                text: Arc::from(t),
                base_freq: f,
                pinyin: Arc::from(p),
                is_primary: true,
                is_correction: false,
                is_sentence: false,
                is_guess: false,
                is_shortcut: false,
                is_english: false,
                sentence_score: 0,
            });
        }
        for c in children {
            n.insert_child(c.clone());
        }
        n
    }

    /// 手工树：ni(你) / ni hao(你好) / ni hao ma(你好吗) / nve(略)
    fn sample_tree() -> TrieNode {
        node(
            "",
            &[],
            &[
                node(
                    "ni",
                    &[("你", 5000, "ni")],
                    &[node(
                        "hao",
                        &[("你好", 4000, "ni hao")],
                        &[node("ma", &[("你好吗", 3000, "ni hao ma")], &[])],
                    )],
                ),
                node("nve", &[("略", 200, "nve")], &[]),
            ],
        )
    }

    #[test]
    fn arena_walk_matches_node_trie() {
        let tree = sample_tree();
        let arena = TrieArena::from_node_trie(&tree);

        // 精确走树等价
        assert!(arena.find_child(TrieArena::ROOT, "ni").is_some());
        let ni = arena.find_child(TrieArena::ROOT, "ni").unwrap();
        assert!(arena.find_child(TrieArena::ROOT, "nve").is_some());
        assert_eq!(arena.find_child(TrieArena::ROOT, "nu"), None);
        let hao = arena.find_child(ni, "hao").unwrap();
        assert_eq!(arena.find_child(hao, "max"), None);

        // 候选访问（span 切片）
        let (s, e) = arena.cand_range(ni);
        assert_eq!(e - s, 1);
        assert_eq!(arena.cand_text(s), "你");
        assert_eq!(arena.cand_freq(s), 5000);
        assert_eq!(arena.cand_pinyin(s), "ni");
        assert_eq!(arena.cand_count(ni), 1);

        // find_candidate
        assert_eq!(
            arena.find_candidate(hao, "你好", "ni hao"),
            Some((4000, true))
        );
        assert_eq!(arena.find_candidate(hao, "你好", "ni haoo"), None);

        // 谓词遍历
        let mut sylls = Vec::new();
        arena.for_each_child(TrieArena::ROOT, |_, s| sylls.push(s.to_string()));
        assert_eq!(sylls, vec!["ni", "nve"]);
        assert_eq!(arena.node_count(), 5);
        assert_eq!(arena.candidate_count(), 4);
        assert_eq!(arena.subtree_nodes(ni), 3, "ni 子树 = ni/hao/ma 三节点");
    }

    #[test]
    fn arena_empty_trie() {
        let arena = TrieArena::from_node_trie(&TrieNode::new(""));
        assert_eq!(arena.node_count(), 1);
        assert_eq!(arena.candidate_count(), 0);
        assert_eq!(arena.find_child(TrieArena::ROOT, "a"), None);
        let (s, e) = arena.cand_range(TrieArena::ROOT);
        assert_eq!(s, e);
        assert_eq!(arena.subtree_nodes(TrieArena::ROOT), 1);
    }

    /// 逐节点全树等价遍历（roundtrip/篡改测试共用）。
    fn walk_equals(a: &TrieArena, b: &TrieArena) {
        assert_eq!(a.node_count(), b.node_count());
        assert_eq!(a.candidate_count(), b.candidate_count());
        let mut stack = vec![(TrieArena::ROOT, TrieArena::ROOT)];
        while let Some((x, y)) = stack.pop() {
            assert_eq!(a.child_syllable(x), b.child_syllable(y));
            let (sx, ex) = a.cand_range(x);
            let (sy, ey) = b.cand_range(y);
            assert_eq!(ex - sx, ey - sy);
            for (i, j) in (sx..ex).zip(sy..ey) {
                assert_eq!(a.cand_text(i), b.cand_text(j));
                assert_eq!(a.cand_pinyin(i), b.cand_pinyin(j));
                assert_eq!(a.cand_freq(i), b.cand_freq(j));
                assert_eq!(a.cand_is_primary(i), b.cand_is_primary(j));
            }
            let mut ca = Vec::new();
            let mut cb = Vec::new();
            a.for_each_child(x, |id, s| ca.push((id, s.to_string())));
            b.for_each_child(y, |id, s| cb.push((id, s.to_string())));
            assert_eq!(ca.len(), cb.len());
            for k in 0..ca.len() {
                stack.push((ca[k].0, cb[k].0));
            }
        }
    }

    #[test]
    fn arena_roundtrip_bytes_exact() {
        // 自有构建 → 字节 → 装载：逐节点/逐候选全树等价 + 字节逐位一致
        let tree = sample_tree();
        let a = TrieArena::from_node_trie(&tree);
        let bytes = a.to_bytes().to_vec();
        let b = TrieArena::from_bytes(bytes.clone()).expect("校验应通过");
        walk_equals(&a, &b);
        assert_eq!(b.to_bytes(), bytes.as_slice(), "装载不应改写任何字节");
        // 再序列化 = 同字节（幂等）
        assert_eq!(b.to_bytes(), a.to_bytes());
    }

    #[test]
    fn arena_rejects_tampering() {
        let tree = sample_tree();
        let a = TrieArena::from_node_trie(&tree);
        let good = a.to_bytes().to_vec();

        // ① 魔数损坏
        let mut bad = good.clone();
        bad[0] ^= 0xFF;
        assert!(TrieArena::from_bytes(bad).is_none(), "坏魔数应拒绝");

        // ② 截断（段表指向越界）
        assert!(
            TrieArena::from_bytes(good[..good.len() - 4].to_vec()).is_none(),
            "截断应拒绝"
        );

        // ③ 段表偏移错乱（改 syll_blob 段 off）
        let mut bad = good.clone();
        // 段表项 0 = (off, len)，off 在 TABLE_OFF..+4
        let off0 = u32::from_le_bytes([bad[8], bad[9], bad[10], bad[11]]);
        bad[TABLE_OFF..TABLE_OFF + 4].copy_from_slice(&(off0 + 3).to_le_bytes()); // 破坏 8 对齐
        assert!(TrieArena::from_bytes(bad).is_none(), "非对齐段应拒绝");

        // ④ 孩子音节反序（严格升序前提破坏）：找 child_ids 段改一个 id 指向音节更大者
        // 简化：把 root 的第一个孩子 id 指向第二个孩子（重复 → 非严格升序）
        let mut bad = good.clone();
        let secs = TrieArena::validate(&good).unwrap();
        let ci = secs[SEC_CHILD_IDS].off as usize;
        let a0 = u32::from_le_bytes([bad[ci], bad[ci + 1], bad[ci + 2], bad[ci + 3]]);
        let b0 = u32::from_le_bytes([bad[ci + 4], bad[ci + 5], bad[ci + 6], bad[ci + 7]]);
        bad[ci..ci + 4].copy_from_slice(&b0.to_le_bytes());
        bad[ci + 4..ci + 8].copy_from_slice(&a0.to_le_bytes()); // 交换 → ni/nve 反序
        assert!(
            TrieArena::from_bytes(bad).is_none(),
            "孩子反序（二分前提破坏）应拒绝"
        );

        // ⑤ 候选 text span 越界
        let mut bad = good.clone();
        let ts = secs[SEC_TEXT_SPANS].off as usize;
        let big = u32::MAX / 2;
        bad[ts..ts + 4].copy_from_slice(&big.to_le_bytes());
        assert!(TrieArena::from_bytes(bad).is_none(), "越界 span 应拒绝");
    }

    #[test]
    fn arena_mapped_range_parity() {
        // 写文件 → mmap → 装载：与自有字节访问逐位一致（映射路径本机实测）
        let tree = sample_tree();
        let a = TrieArena::from_node_trie(&tree);
        let bytes = a.to_bytes().to_vec();
        let dir = crate::test_tmp::dir().join(format!("x1arena_mmap_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("arena.bin");
        std::fs::write(&p, &bytes).unwrap();

        match crate::mmap_file::MmapRegion::open(&p) {
            Some(region) => {
                let m = TrieArena::from_mapped_range(region, 0, bytes.len())
                    .expect("映射装载校验应通过");
                walk_equals(&a, &m);
                assert_eq!(m.to_bytes(), bytes.as_slice());
                // 区间错误应拒绝
                let region2 = crate::mmap_file::MmapRegion::open(&p).unwrap();
                assert!(TrieArena::from_mapped_range(region2, 1, bytes.len()).is_none());
                let region3 = crate::mmap_file::MmapRegion::open(&p).unwrap();
                assert!(
                    TrieArena::from_mapped_range(region3, 0, bytes.len() + 1).is_none(),
                    "越界区间应拒绝"
                );
            }
            None => {
                // 本机映射不可用（极端环境）：跳过（装载回退自有读路径已由 roundtrip 覆盖）
                eprintln!("[test] mmap 不可用，跳过映射 parity（不影响回退正确性）");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
