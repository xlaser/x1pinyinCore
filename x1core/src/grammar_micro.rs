//! 微型词性搭配模型：`x1grammar.bin` 的解码与查询。
//!
//! 字节级唯一权威定义在离线构建脚本（编码 + `--check`
//! 解析器），本模块是其 Rust 运行时镜像。
//!
//! 格式速记（小端，32 字节头 + payload）：
//! ```text
//! 头   magic "X1GM" | version:u16=1 | header_size:u16=32 | model_hash:u32
//!      | corpus_hash:u32 | timestamp_ns:u64（不参与哈希） | flags:u32 | payload_len:u32
//! payload  5 个 block 依次为 id 1..=5（block_id:u8, row_count:u32, rows...）：
//!   1 一元   class_id:u8, count:u32
//!   2 二元   prev:u8, next:u8, count:u32, score:i8
//!   3 三元   first:u8, second:u8, third:u8, count:u32, score:i8
//!   4 滑动块 len:u8(4–6), ids:[u8;6](len<6 尾部填 0xFF), count:u32, score:i8
//!   5 的地得 prev:u8, next:u8, de:u32, di:u32, de2:u32
//! ```
//!
//! 13 词性粗类（与构建端 CLASS_NAMES 同序）：0名词 1动词 2形容词
//! 3副词 4代词 5数词 6时间词 7方位词 8介词 9连词 10助词 11标点 12其他。
//! 二元/三元把标点/其他类当普通 ID 统计，消费端勿假设
//! 只有内容词类。
//!
//! 工程约束：
//! - 只用 std（`std::fs::read` 一次读入，无 mmap/无新依赖）；
//! - 解码失败一律返回 `Err(GrammarMicroError)`，**不 panic、不修复、不写文件**
//!   （启动秒开约束：启动路径零维护动作；失败由调用方降级空模型）；
//! - 查询零分配（`path_score` 只用栈上哨兵，无 Vec/String）；
//! - score 全部构建期算好，运行时不做浮点对数（的/地/得层存的是原始计数，
//!   该层查询频度极低，沿用 legacy `GrammarModel::delta_for` 的 f64 log2
//!   口径以保证 A/B 可比，见 `func_delta`）。

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

/// magic（构建端 `MAGIC`）。
const MAGIC: [u8; 4] = *b"X1GM";
/// 当前格式版本（构建端 `VERSION`）。
const VERSION: u16 = 1;
/// 头部长度（构建端 `HEADER_SIZE`）。
const HEADER_SIZE: u16 = 32;
/// 词性粗类数。
const N_CLASSES: u8 = 13;
/// Block 4 滑动块长度合法区间（构建端只写 4/5/6）。
const BLOCK_LEN_MIN: u8 = 4;
const BLOCK_LEN_MAX: u8 = 6;
/// Block 4 ids 尾部填充字节（len<6 时）。
const BLOCK_PAD: u8 = 0xFF;

/// FNV-1a 32 位参数（与构建端 `fnv1a32` 一致；官方测试向量见底部测试）。
const FNV_OFFSET: u32 = 0x811C_9DC5;
const FNV_PRIME: u32 = 0x0100_0193;

/// `path_score` 参与打分的路径长度上限（legacy
/// `pattern_window_stats` 同为 24 词上限；引擎包装层同口径）。
const PATH_MAX_LEN: usize = 24;
/// `path_score` 总分钳位区间。
const PATH_SCORE_CLAMP: i32 = 24;
/// 的/地/得语境最小语料量（沿 legacy `delta_for` 的 ctx_total < 4 → 0 门槛）。
const FUNC_CTX_MIN: u64 = 4;
/// 的/地/得单点增量钳位（沿 legacy 口径）。
const FUNC_DELTA_CLAMP: i32 = 12;

/// 的/地/得粒子选择子（`func_delta` 的 `curr` 入参；与构建端 Block 5 的
/// de/di/de2 三列顺序一致，即 ude1/ude2/ude3）。
pub const FUNC_DE: u8 = 0; // 的
pub const FUNC_DI: u8 = 1; // 地
pub const FUNC_DE2: u8 = 2; // 得

/// FNV-1a 32 位（小端字节流哈希；与构建端逐字节一致）。
fn fnv1a32(data: &[u8]) -> u32 {
    let mut h = FNV_OFFSET;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// 二元键打包：`prev<<8 | next`。
fn bigram_key(prev: u8, next: u8) -> u16 {
    ((prev as u16) << 8) | next as u16
}

/// 三元键打包：`first<<16 | second<<8 | third`。
fn trigram_key(first: u8, second: u8, third: u8) -> u32 {
    ((first as u32) << 16) | ((second as u32) << 8) | third as u32
}

/// 滑动块键打包：`len` 放最高字节（不同长度的 ids 序列必须可区分——
/// 截断 0xFF 填充后 [a,b,c,d] 与 [a,b,c,d,x,y] 不共享键），ids 各占一字节。
/// 仅接受 4–6 长度（表内只存在这些长度）。
fn block_key(ids: &[u8]) -> u64 {
    debug_assert!((BLOCK_LEN_MIN as usize..=BLOCK_LEN_MAX as usize).contains(&ids.len()));
    let mut k = (ids.len() as u64) << 48;
    for (i, &id) in ids.iter().enumerate() {
        k |= (id as u64) << (8 * (5 - i));
    }
    k
}

/// 一个 (prev, next) 词性语境下的 的/地/得 计数（Block 5 一行）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FuncMicroCounts {
    pub de: u32,
    pub di: u32,
    pub de2: u32,
}

impl FuncMicroCounts {
    fn total(&self) -> u64 {
        self.de as u64 + self.di as u64 + self.de2 as u64
    }
}

/// 模型加载/解码失败。加载失败不得 panic，由调用方降级为空模型。
#[derive(Debug)]
pub enum GrammarMicroError {
    /// 文件读取失败（不存在/权限等）。
    Io(std::io::Error),
    /// 结构损坏：magic/版本/长度/哈希/越界/非法词性 ID 等。
    InvalidFormat(&'static str),
}

impl std::fmt::Display for GrammarMicroError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GrammarMicroError::Io(e) => write!(f, "x1grammar.bin 读取失败: {e}"),
            GrammarMicroError::InvalidFormat(why) => {
                write!(f, "x1grammar.bin 格式非法: {why}")
            }
        }
    }
}

impl std::error::Error for GrammarMicroError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GrammarMicroError::Io(e) => Some(e),
            GrammarMicroError::InvalidFormat(_) => None,
        }
    }
}

/// 一条词性路径的逐层分（`GrammarMicro::path_breakdown`，诊断观测用）。
/// `total` 恒等于 [`GrammarMicro::path_score`]（单实现），故
/// `(bigram + trigram + block).clamp(-24, 24)` 与 `total` 逐分对账成立。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MicroPathBreakdown {
    /// 相邻二元分之和（总分钳位前的构成）。
    pub bigram: i32,
    /// 相邻三元分之和（总分钳位前的构成）。
    pub trigram: i32,
    /// 4–6 词滑动块最高 3 名各 /2 之和（总分钳位前的构成）。
    pub block: i32,
    /// 钳位后总分（== `path_score`）。
    pub total: i32,
}

/// 一条**词**路径的微型语法模型诊断（`Engine::grammar_micro_path_components`
/// 的返回值，只读观测；词→粗类
/// 映射与逐层分量不在消费侧复刻，避免双份逻辑漂移）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GrammarMicroPathDiagnosis {
    /// 模型是否在场（false = 资产缺失/解码失败的降级空模型，分量全 0）。
    pub loaded: bool,
    /// 相邻二元分之和（钳位前构成）。
    pub bigram: i32,
    /// 相邻三元分之和（钳位前构成）。
    pub trigram: i32,
    /// 滑动块最高 3 名各 /2 之和（钳位前构成）。
    pub block: i32,
    /// 钳位后总分（== 该路径类向量上的 `GrammarMicro::path_score`）。
    pub raw_total: i32,
}

impl GrammarMicroPathDiagnosis {
    /// 上下文让位后的实际分（镜像引擎 `grammar_micro_delta` 的让位规则：
    /// 强语境 → `raw_total / 4`，Rust 整除向零截断——观测口径与排序口径
    /// 同式）。让位判据由调用方给出（用
    /// `grammar_diagnosis().context_yield`，与引擎谓词同款）。
    pub fn effective(&self, yield_context: bool) -> i32 {
        // 正分融合定稿：强上下文完全让位（raw/4 仍会翻转
        // 长程语境）。观测口径必须与 grammar_micro_delta 一致。
        if yield_context {
            0
        } else {
            self.raw_total
        }
    }
}

/// 模型元信息（调试 CLI `grammar-model info` 的数据源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrammarMicroInfo {
    pub version: u16,
    pub path: Option<String>,
    pub size_bytes: u64,
    pub model_hash: u32,
    pub corpus_hash: u32,
    pub unigram_rows: usize,
    pub bigram_rows: usize,
    pub trigram_rows: usize,
    pub block_rows: usize,
    pub func_rows: usize,
    pub load_micros: u128,
}

/// 微型词性搭配模型。`empty()`/`default()` = 未加载（文件缺失/损坏降级态），
/// 所有查询返回 0，`info().path == None` 可作为「未加载」判据。
#[derive(Debug, Default)]
pub struct GrammarMicro {
    version: u16,
    path: Option<String>,
    size_bytes: u64,
    model_hash: u32,
    corpus_hash: u32,
    load_micros: u128,
    /// 一元先验计数（按词性 ID 索引）。查询路径不消费——构建端已把一元
    /// 分布烘焙进二元/三元 score 的期望项；保留给后续诊断/回退用途。
    #[allow(dead_code)]
    unigram: [u32; N_CLASSES as usize],
    /// 一元层实际行数（与 map 不同：数组无法区分「0 计数行」）。
    unigram_rows: usize,
    /// 二元转移分（键 = prev<<8|next）。
    bigram: HashMap<u16, i8>,
    /// 三元组合分（键 = first<<16|second<<8|third）。
    trigram: HashMap<u32, i8>,
    /// 4–6 词滑动块分（键 = len<<48|ids 逐字节）。
    blocks: HashMap<u64, i8>,
    /// 的/地/得语境计数（键 = prev<<8|next，同二元键布局）。
    func: HashMap<u16, FuncMicroCounts>,
    /// 的/地/得三层全局总量（func_delta 的 P(func) 分母）。
    total_de: u64,
    total_di: u64,
    total_de2: u64,
}

impl GrammarMicro {
    /// 空模型（未加载降级态）：全部查询返回 0。
    pub fn empty() -> Self {
        Self::default()
    }

    /// 读取并解码 `x1grammar.bin`。任何结构损坏/哈希不符/越界都返回
    /// `Err`，绝不 panic、绝不写文件。
    pub fn load_from_path(path: &Path) -> Result<Self, GrammarMicroError> {
        let t0 = Instant::now();
        let blob = std::fs::read(path).map_err(GrammarMicroError::Io)?;
        let (version, model_hash, corpus_hash, layers) = decode(&blob)?;
        let GrammarLayers {
            unigram,
            unigram_rows,
            bigram,
            trigram,
            blocks,
            func,
            total_de,
            total_di,
            total_de2,
        } = layers;
        Ok(Self {
            version,
            path: Some(path.display().to_string()),
            size_bytes: blob.len() as u64,
            model_hash,
            corpus_hash,
            load_micros: t0.elapsed().as_micros(),
            unigram,
            unigram_rows,
            bigram,
            trigram,
            blocks,
            func,
            total_de,
            total_di,
            total_de2,
        })
    }

    /// 模型元信息（未加载模型 version=0、path=None、各行数 0）。
    pub fn info(&self) -> GrammarMicroInfo {
        GrammarMicroInfo {
            version: self.version,
            path: self.path.clone(),
            size_bytes: self.size_bytes,
            model_hash: self.model_hash,
            corpus_hash: self.corpus_hash,
            unigram_rows: self.unigram_rows,
            bigram_rows: self.bigram.len(),
            trigram_rows: self.trigram.len(),
            block_rows: self.blocks.len(),
            func_rows: self.func.len(),
            load_micros: self.load_micros,
        }
    }

    /// 是否为已加载的真实模型（`info().path == None` 未加载判据的零分配版，
    /// 供绿词排序热路径做降级回退判定；不构造 info() 的 String 克隆）。
    pub fn is_loaded(&self) -> bool {
        self.path.is_some()
    }

    /// 二元转移分：`P(next|prev)` 相对一元期望的提升，构建期量化为 i8
    /// （±16 钳位）。未命中返回 0（不惩罚未观测组合）。
    pub fn bigram_score(&self, prev: u8, next: u8) -> i8 {
        self.bigram
            .get(&bigram_key(prev, next))
            .copied()
            .unwrap_or(0)
    }

    /// 三元组合分：`P(third|first,second)` 相对期望的提升。未命中返回 0。
    pub fn trigram_score(&self, first: u8, second: u8, third: u8) -> i8 {
        self.trigram
            .get(&trigram_key(first, second, third))
            .copied()
            .unwrap_or(0)
    }

    /// 4–6 词滑动块分。未命中（或长度不在 4–6）返回 0——
    /// 「未命中语法块不惩罚」。
    pub fn block_score(&self, ids: &[u8]) -> i8 {
        if !((BLOCK_LEN_MIN as usize..=BLOCK_LEN_MAX as usize).contains(&ids.len())) {
            return 0;
        }
        self.blocks.get(&block_key(ids)).copied().unwrap_or(0)
    }

    /// 整条词性路径的语法分（绿词排序唯一消费入口）。
    ///
    /// 规则：
    /// ```text
    /// len < 2            → 0
    /// len == 2           → 单个 bigram_score
    /// len >= 3           → sum(相邻二元) + sum(相邻三元)
    /// 存在 4–6 词窗口     → 取最高 3 个 block_score，各除以 2 后累加
    /// 总分 clamp [-24, 24]
    /// ```
    /// 路径长度上限 24（引擎包装层同口径先做了一次门控：按消费口径
    /// `words.len() > 24` 才返回 0，
    /// 否则所有合法路径恒为 0 分）。
    /// 零分配：二元/三元/块窗口全走借用切片，块最高 3 名用栈上哨兵维护。
    pub fn path_score(&self, classes: &[u8]) -> i32 {
        self.path_breakdown(classes).total
    }

    /// 整条词性路径的逐层分（诊断观测；调试 CLI 经
    /// `Engine::grammar_micro_path_components` 消费）。与 [`Self::path_score`]
    /// **同一实现**（`path_score` 即本方法的 `total`），诊断与排序逐层对账
    /// 不漂移；规则、门控、零分配纪律同 `path_score` 文档。
    pub fn path_breakdown(&self, classes: &[u8]) -> MicroPathBreakdown {
        let n = classes.len();
        if !(2..=PATH_MAX_LEN).contains(&n) {
            return MicroPathBreakdown::default();
        }
        let mut bigram: i32 = 0;
        for w in classes.windows(2) {
            bigram += self.bigram_score(w[0], w[1]) as i32;
        }
        let mut trigram: i32 = 0;
        for w in classes.windows(3) {
            trigram += self.trigram_score(w[0], w[1], w[2]) as i32;
        }
        let mut block: i32 = 0;
        if n >= BLOCK_LEN_MIN as usize {
            // 滑动块最高 3 名（t0 >= t1 >= t2）。哨兵 i32::MIN 不可能是
            // 真实分（i8 量化的分数 ≥ -128 > i32::MIN），窗口不足 3 个时
            // 直接跳过；未命中窗口计 0 分，天然不会被负分挤出正分名额。
            let mut t0 = i32::MIN;
            let mut t1 = i32::MIN;
            let mut t2 = i32::MIN;
            for len in BLOCK_LEN_MIN as usize..=BLOCK_LEN_MAX as usize {
                if n < len {
                    break;
                }
                for w in classes.windows(len) {
                    let s = self.block_score(w) as i32;
                    if s > t0 {
                        t2 = t1;
                        t1 = t0;
                        t0 = s;
                    } else if s > t1 {
                        t2 = t1;
                        t1 = s;
                    } else if s > t2 {
                        t2 = s;
                    }
                }
            }
            for t in [t0, t1, t2] {
                if t != i32::MIN {
                    block += t / 2; // 每个除以 2（Rust 整除向零截断）
                }
            }
        }
        let total = (bigram + trigram + block).clamp(-PATH_SCORE_CLAMP, PATH_SCORE_CLAMP);
        MicroPathBreakdown {
            bigram,
            trigram,
            block,
            total,
        }
    }

    /// 的/地/得单点语境增量。
    ///
    /// - `prev` / `next`：前后词的词性粗类 ID（Block 5 的键，构建时已取
    ///   最近的非标点/非其他类词）；
    /// - `curr`：粒子选择子（`FUNC_DE`/`FUNC_DI`/`FUNC_DE2`，即 ude1/2/3），
    ///   其他值返回 0。
    ///
    /// 口径沿 legacy `GrammarModel::delta_for`（A/B 可比）：语境计数 < 4 → 0；
    /// 该粒子语料为 0 → -12；否则 `round(4·log2(lift))` 钳 ±12，其中
    /// `lift = P(func|语境) / P(func)`。的的全局占比 ~96%，原始份额会让任何
    /// 语境都偏向「的」，必须用 lift。该层只对路径里的粒子词逐点调用，
    /// 频度极低，f64 log2 不构成热路径（二元/三元/块层均为预量化整数）。
    pub fn func_delta(&self, prev: u8, curr: u8, next: u8) -> i32 {
        let Some(c) = self.func.get(&bigram_key(prev, next)).copied() else {
            return 0;
        };
        let ctx_total = c.total();
        if ctx_total < FUNC_CTX_MIN {
            return 0;
        }
        let total_all = self.total_de + self.total_di + self.total_de2;
        if total_all == 0 {
            return 0;
        }
        let (count, base_total) = match curr {
            FUNC_DE => (c.de as u64, self.total_de),
            FUNC_DI => (c.di as u64, self.total_di),
            FUNC_DE2 => (c.de2 as u64, self.total_de2),
            _ => return 0,
        };
        if base_total == 0 {
            return 0;
        }
        if count == 0 {
            return -FUNC_DELTA_CLAMP;
        }
        let share = count as f64 / ctx_total as f64; // P(func | 语境)
        let base = base_total as f64 / total_all as f64; // P(func)
        let lift = share / base;
        ((lift.log2() * 4.0).round() as i32).clamp(-FUNC_DELTA_CLAMP, FUNC_DELTA_CLAMP)
    }
}

/// 解码成功后的五层数据（`load_from_path` 组装用）。
struct GrammarLayers {
    unigram: [u32; N_CLASSES as usize],
    unigram_rows: usize,
    bigram: HashMap<u16, i8>,
    trigram: HashMap<u32, i8>,
    blocks: HashMap<u64, i8>,
    func: HashMap<u16, FuncMicroCounts>,
    total_de: u64,
    total_di: u64,
    total_de2: u64,
}

/// 只读小端游标：所有读取越界统一返回 `InvalidFormat`，绝不 panic。
struct Rd<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Rd<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], GrammarMicroError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(GrammarMicroError::InvalidFormat("长度溢出"))?;
        if end > self.buf.len() {
            return Err(GrammarMicroError::InvalidFormat("字节越界"));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, GrammarMicroError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, GrammarMicroError> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }

    fn u32(&mut self) -> Result<u32, GrammarMicroError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn i8(&mut self) -> Result<i8, GrammarMicroError> {
        Ok(self.take(1)?[0] as i8)
    }
}

/// 校验词性 ID 合法（< 13），透传返回。
fn cls(id: u8) -> Result<u8, GrammarMicroError> {
    if id < N_CLASSES {
        Ok(id)
    } else {
        Err(GrammarMicroError::InvalidFormat("词性 ID >= 13"))
    }
}

/// 校验滑动块长度合法（4–6），透传返回。
fn block_len(len: u8) -> Result<u8, GrammarMicroError> {
    if (BLOCK_LEN_MIN..=BLOCK_LEN_MAX).contains(&len) {
        Ok(len)
    } else {
        Err(GrammarMicroError::InvalidFormat("Block 4 len 不在 4–6"))
    }
}

/// 解码全文：头部校验（magic/版本/头长/payload_len/model_hash）+ 五个
/// block 依序解析（block_id 必须按 1..=5 递增且全部到场，payload 恰好
/// 耗尽——row_count 与实际行数的一致性由此保证）。
fn decode(blob: &[u8]) -> Result<(u16, u32, u32, GrammarLayers), GrammarMicroError> {
    let mut rd = Rd::new(blob);
    if rd.take(4)? != MAGIC {
        return Err(GrammarMicroError::InvalidFormat("magic 错误"));
    }
    let version = rd.u16()?;
    if version != VERSION {
        return Err(GrammarMicroError::InvalidFormat("version 错误"));
    }
    let header_size = rd.u16()?;
    if header_size != HEADER_SIZE {
        return Err(GrammarMicroError::InvalidFormat("header_size 错误"));
    }
    let model_hash = rd.u32()?;
    let corpus_hash = rd.u32()?;
    let _timestamp_ns = rd.take(8)?; // 构建时间戳，不参与任何哈希，运行时不消费
    let _flags = rd.u32()?; // bit0 口语 / bit1 滑动块 / bit2 的地得；当前恒 7，不门控
    let payload_len = rd.u32()?;
    let payload = &blob[HEADER_SIZE as usize..];
    if payload.len() != payload_len as usize {
        return Err(GrammarMicroError::InvalidFormat(
            "payload_len 声明与实际不符",
        ));
    }
    if fnv1a32(payload) != model_hash {
        return Err(GrammarMicroError::InvalidFormat("model_hash 不符"));
    }

    let mut layers = GrammarLayers {
        unigram: [0; N_CLASSES as usize],
        unigram_rows: 0,
        bigram: HashMap::new(),
        trigram: HashMap::new(),
        blocks: HashMap::new(),
        func: HashMap::new(),
        total_de: 0,
        total_di: 0,
        total_de2: 0,
    };
    let mut p = Rd::new(payload);
    let mut expected_id = 1u8;
    while p.pos < payload.len() {
        let block_id = p.u8()?;
        // block_id 必须按 1..=5 依序出现（构建端确定性编码的镜像约束）
        if block_id != expected_id || block_id > 5 {
            return Err(GrammarMicroError::InvalidFormat("block_id 异常"));
        }
        let row_count = p.u32()?;
        match block_id {
            1 => {
                for _ in 0..row_count {
                    let cid = cls(p.u8()?)?;
                    let count = p.u32()?;
                    layers.unigram[cid as usize] = count;
                    layers.unigram_rows += 1;
                }
            }
            2 => {
                for _ in 0..row_count {
                    let prev = cls(p.u8()?)?;
                    let next = cls(p.u8()?)?;
                    let _count = p.u32()?; // 运行时只消费预量化 score
                    let score = p.i8()?;
                    layers.bigram.insert(bigram_key(prev, next), score);
                }
            }
            3 => {
                for _ in 0..row_count {
                    let first = cls(p.u8()?)?;
                    let second = cls(p.u8()?)?;
                    let third = cls(p.u8()?)?;
                    let _count = p.u32()?;
                    let score = p.i8()?;
                    layers
                        .trigram
                        .insert(trigram_key(first, second, third), score);
                }
            }
            4 => {
                for _ in 0..row_count {
                    let len = block_len(p.u8()?)?;
                    let ids_raw = p.take(6)?;
                    let mut ids = [0u8; BLOCK_LEN_MAX as usize];
                    for k in 0..6 {
                        let b = ids_raw[k];
                        if (k as u8) < len {
                            ids[k] = cls(b)?;
                        } else if b != BLOCK_PAD {
                            // len<6 时尾部必须是 0xFF 填充（构建端契约）
                            return Err(GrammarMicroError::InvalidFormat(
                                "Block 4 尾部填充必须为 0xFF",
                            ));
                        }
                    }
                    let _count = p.u32()?;
                    let score = p.i8()?;
                    layers.blocks.insert(block_key(&ids[..len as usize]), score);
                }
            }
            _ => {
                for _ in 0..row_count {
                    let prev = cls(p.u8()?)?;
                    let next = cls(p.u8()?)?;
                    let de = p.u32()?;
                    let di = p.u32()?;
                    let de2 = p.u32()?;
                    layers.total_de += de as u64;
                    layers.total_di += di as u64;
                    layers.total_de2 += de2 as u64;
                    layers
                        .func
                        .insert(bigram_key(prev, next), FuncMicroCounts { de, di, de2 });
                }
            }
        }
        expected_id += 1;
    }
    if expected_id != 6 {
        return Err(GrammarMicroError::InvalidFormat(
            "缺少 Block（须 1..=5 全到场）",
        ));
    }
    Ok((version, model_hash, corpus_hash, layers))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_magic_and_truncated_payload() {
        let dir = crate::test_tmp::dir();
        let bad = dir.join("bad.x1gm");
        std::fs::write(&bad, b"NOPE0000").unwrap();
        assert!(GrammarMicro::load_from_path(&bad).is_err());

        let short = dir.join("short.x1gm");
        let mut b = Vec::new();
        b.extend_from_slice(b"X1GM");
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&32u16.to_le_bytes());
        b.extend_from_slice(&[0; 24]);
        b.extend_from_slice(&[1u8, 0, 0, 0, 0]);
        std::fs::write(&short, b).unwrap();
        assert!(GrammarMicro::load_from_path(&short).is_err());
    }

    #[test]
    fn loads_repository_model_and_answers_queries() {
        if crate::test_tmp::skip_without_grammar_model() {
            return;
        }
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/x1grammar.bin");
        let m = GrammarMicro::load_from_path(&path).unwrap();
        let info = m.info();
        assert_eq!(info.version, 1);
        assert!(info.size_bytes <= 80 * 1024);
        assert!(info.bigram_rows > 0);
        assert!(info.trigram_rows > 0);
        assert!(info.block_rows > 0);
        // 打印模型体积与加载耗时（--nocapture 下可见）
        println!(
            "[grammar_micro] size={}B load={}µs rows uni={} bi={} tri={} blk={} func={} \
             model_hash={:08x} corpus_hash={:08x}",
            info.size_bytes,
            info.load_micros,
            info.unigram_rows,
            info.bigram_rows,
            info.trigram_rows,
            info.block_rows,
            info.func_rows,
            info.model_hash,
            info.corpus_hash,
        );
        // 真实模型必须同时存在正/负分（提升/抑制两个方向才有排序意义）
        let mut pos = 0;
        let mut neg = 0;
        for prev in 0..13u8 {
            for next in 0..13u8 {
                let s = m.bigram_score(prev, next);
                pos += (s > 0) as usize;
                neg += (s < 0) as usize;
            }
        }
        assert!(pos > 0 && neg > 0, "二元分须有正有负: pos={pos} neg={neg}");
        let mut t_pos = 0;
        let mut t_neg = 0;
        for a in 0..13u8 {
            for b in 0..13u8 {
                for c in 0..13u8 {
                    let s = m.trigram_score(a, b, c);
                    t_pos += (s > 0) as usize;
                    t_neg += (s < 0) as usize;
                }
            }
        }
        assert!(t_pos > 0 && t_neg > 0, "三元分须有正有负");
        // 路径分：钳位区间内 + 超长路径门控为 0
        let score = m.path_score(&[4, 1, 0]); // 代词-动词-名词
        assert!((-24..=24).contains(&score));
        assert_eq!(m.path_score(&[0u8; 25]), 0);
    }

    #[test]
    fn fnv1a32_official_vectors() {
        // FNV-1a 32 官方测试向量（与构建端 self-test 同一组）
        assert_eq!(fnv1a32(b""), 0x811C_9DC5);
        assert_eq!(fnv1a32(b"a"), 0xE40C_292C);
        assert_eq!(fnv1a32(b"foobar"), 0xBF9C_F968);
    }

    #[test]
    fn empty_model_answers_zero_everywhere() {
        let m = GrammarMicro::empty();
        let info = m.info();
        assert_eq!(info.version, 0);
        assert!(info.path.is_none());
        assert_eq!(info.size_bytes, 0);
        assert_eq!(info.unigram_rows + info.bigram_rows + info.trigram_rows, 0);
        assert_eq!(m.bigram_score(0, 1), 0);
        assert_eq!(m.trigram_score(0, 1, 2), 0);
        assert_eq!(m.block_score(&[0, 1, 2, 3]), 0);
        assert_eq!(m.path_score(&[0, 1, 2, 3]), 0);
        assert_eq!(m.func_delta(1, FUNC_DI, 0), 0);
    }

    // --- 构造式模型：小端编码器（与构建端编码互为镜像）-------------------

    fn put_u32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    /// 头部 + 哈希封装：model_hash 恒为 payload 的真实 FNV-1a32
    /// （除非 `fake_hash` 篡改，用于专项测哈希校验）。
    fn wrap(payload: &[u8], fake_hash: Option<u32>) -> Vec<u8> {
        let mut b = Vec::with_capacity(32 + payload.len());
        b.extend_from_slice(&MAGIC);
        b.extend_from_slice(&VERSION.to_le_bytes());
        b.extend_from_slice(&HEADER_SIZE.to_le_bytes());
        b.extend_from_slice(&fake_hash.unwrap_or(fnv1a32(payload)).to_le_bytes());
        b.extend_from_slice(&0x1234_5678u32.to_le_bytes()); // corpus_hash（任意）
        b.extend_from_slice(&42u64.to_le_bytes()); // timestamp
        b.extend_from_slice(&7u32.to_le_bytes()); // flags
        b.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        b.extend_from_slice(payload);
        b
    }

    fn block_hdr(v: &mut Vec<u8>, id: u8, rows: u32) {
        v.push(id);
        put_u32(v, rows);
    }

    /// 最小合法模型（五层各 1–3 行，分值手工指定以便断言查询语义）：
    /// bigram (0,1)=2 (1,2)=3 (2,3)=4；trigram (0,1,2)=-1 (1,2,3)=1；
    /// block [0,1,2,3]=8 [2,3,0,1,2]=-6 [0,1,2,3,4,5]=16；
    /// func (3,1)=(1,8,3) (0,2)=(3,0,9) (5,6)=(2,1,0)。
    fn minimal_model() -> Vec<u8> {
        let mut p = Vec::new();
        // Block 1：一元
        block_hdr(&mut p, 1, 1);
        p.push(0);
        put_u32(&mut p, 1000);
        // Block 2：二元
        block_hdr(&mut p, 2, 3);
        for (prev, next, score) in [(0u8, 1u8, 2i8), (1, 2, 3), (2, 3, 4)] {
            p.push(prev);
            p.push(next);
            put_u32(&mut p, 10);
            p.push(score as u8);
        }
        // Block 3：三元
        block_hdr(&mut p, 3, 2);
        for (a, b, c, score) in [(0u8, 1u8, 2u8, -1i8), (1, 2, 3, 1)] {
            p.extend_from_slice(&[a, b, c]);
            put_u32(&mut p, 5);
            p.push(score as u8);
        }
        // Block 4：滑动块
        block_hdr(&mut p, 4, 3);
        for (ids, score) in [
            (&[0u8, 1, 2, 3][..], 8i8),
            (&[2u8, 3, 0, 1, 2][..], -6i8),
            (&[0u8, 1, 2, 3, 4, 5][..], 16i8),
        ] {
            p.push(ids.len() as u8);
            let mut raw = ids.to_vec();
            raw.resize(6, BLOCK_PAD);
            p.extend_from_slice(&raw);
            put_u32(&mut p, 100);
            p.push(score as u8);
        }
        // Block 5：的地得
        block_hdr(&mut p, 5, 3);
        for (prev, next, de, di, de2) in [
            (3u8, 1u8, 1u32, 8u32, 3u32),
            (0, 2, 3, 0, 9),
            (5, 6, 2, 1, 0),
        ] {
            p.push(prev);
            p.push(next);
            put_u32(&mut p, de);
            put_u32(&mut p, di);
            put_u32(&mut p, de2);
        }
        wrap(&p, None)
    }

    fn write_model(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = crate::test_tmp::path(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn decodes_crafted_model_and_answers_queries() {
        let path = write_model("mini-ok.x1gm", &minimal_model());
        let m = GrammarMicro::load_from_path(&path).unwrap();
        let info = m.info();
        assert_eq!(info.version, 1);
        assert_eq!(info.size_bytes, minimal_model().len() as u64);
        assert!(info.path.as_deref().unwrap().contains("mini-ok.x1gm"));
        assert_eq!(
            (info.unigram_rows, info.bigram_rows, info.trigram_rows),
            (1, 3, 2)
        );
        assert_eq!((info.block_rows, info.func_rows), (3, 3));

        // 二元/三元/块：命中、未命中（=0 不惩罚）、非法长度
        assert_eq!(m.bigram_score(0, 1), 2);
        assert_eq!(m.bigram_score(1, 2), 3);
        assert_eq!(m.bigram_score(2, 3), 4);
        assert_eq!(m.bigram_score(3, 0), 0);
        assert_eq!(m.trigram_score(0, 1, 2), -1);
        assert_eq!(m.trigram_score(1, 2, 3), 1);
        assert_eq!(m.trigram_score(5, 5, 5), 0);
        assert_eq!(m.block_score(&[0, 1, 2, 3]), 8);
        assert_eq!(m.block_score(&[2, 3, 0, 1, 2]), -6); // len5：0xFF 截断后正确键控
        assert_eq!(m.block_score(&[0, 1, 2, 3, 4, 5]), 16);
        assert_eq!(m.block_score(&[9, 9, 9, 9]), 0);
        assert_eq!(m.block_score(&[0, 1, 2]), 0); // 长度 <4
        assert_eq!(m.block_score(&[0, 1, 2, 3, 4, 5, 6]), 0); // 长度 >6

        // path_score 规则逐条验证
        assert_eq!(m.path_score(&[]), 0); // len < 2
        assert_eq!(m.path_score(&[7]), 0);
        assert_eq!(m.path_score(&[0, 1]), 2); // len == 2 → 单二元
        assert_eq!(m.path_score(&[0, 1, 2]), 2 + 3 + (-1)); // len 3：二元和 + 三元和
                                                            // len 4：二元(2+3+4) + 三元(-1+1)=0 + 块窗口 [0,1,2,3]=8/2
        assert_eq!(m.path_score(&[0, 1, 2, 3]), 9 + 8 / 2);
        // len 6：二元 2+3+4 + 三元 -1+1=0 + 最高 3 块 {16,8,0}/2 → 8+4
        assert_eq!(m.path_score(&[0, 1, 2, 3, 4, 5]), 9 + (16 / 2 + 8 / 2));
        assert_eq!(m.path_score(&[0u8; 25]), 0); // 超长门控
        assert_eq!(m.path_score(&[12u8; 24]), 0); // 全未命中 = 0 分不惩罚

        // path_breakdown（诊断 API）：与 path_score 单实现，
        // 分量之和钳位 == total；长度门控同款。
        let b4 = m.path_breakdown(&[0, 1, 2, 3]);
        assert_eq!((b4.bigram, b4.trigram, b4.block, b4.total), (9, 0, 4, 13));
        assert_eq!(b4.total, m.path_score(&[0, 1, 2, 3]));
        let b6 = m.path_breakdown(&[0, 1, 2, 3, 4, 5]);
        // 最高 3 块 = {16, 8, 0}（未命中窗口计 0 参与前三），各 /2 求和
        assert_eq!((b6.bigram, b6.trigram, b6.block), (9, 0, 16 / 2 + 8 / 2));
        assert_eq!(b6.total, m.path_score(&[0, 1, 2, 3, 4, 5]));
        let b1 = m.path_breakdown(&[7]);
        assert_eq!((b1.bigram, b1.trigram, b1.block, b1.total), (0, 0, 0, 0));
        for v in [&[0u8, 1, 2][..], &[0, 1, 2, 3, 4, 5][..], &[12u8; 24][..]] {
            let b = m.path_breakdown(v);
            assert_eq!(
                (b.bigram + b.trigram + b.block).clamp(-24, 24),
                b.total,
                "分量构成钳位 == total: {v:?}"
            );
        }

        // GrammarMicroPathDiagnosis::effective：强语境完全让位（与引擎
        // grammar_micro_delta 同式）。
        let d = GrammarMicroPathDiagnosis {
            loaded: true,
            bigram: 1,
            trigram: 1,
            block: 0,
            raw_total: -7,
        };
        assert_eq!(d.effective(false), -7);
        assert_eq!(d.effective(true), 0); // 强语境让位归零
        let zero = GrammarMicroPathDiagnosis::default();
        assert!(!zero.loaded);
        assert_eq!(zero.effective(true), 0); // 降级态分量全 0，让位无放大

        // 的/地/得 lift 语义（totals: de=6 di=9 de2=12, all=27）
        assert_eq!(m.func_delta(3, FUNC_DI, 1), 4); // lift=2 → 4·log2(2)
        assert_eq!(m.func_delta(3, FUNC_DE, 1), -6); // lift=0.375 → -5.66 → -6
        assert_eq!(m.func_delta(3, FUNC_DE2, 1), -3); // lift=0.5625 → -3.32 → -3
        assert_eq!(m.func_delta(0, FUNC_DE, 2), 1); // lift=1.125 → 0.68 → 1
        assert_eq!(m.func_delta(0, FUNC_DI, 2), -12); // 语料为 0 → 钳 -12
        assert_eq!(m.func_delta(0, FUNC_DE2, 2), 3); // lift=1.6875 → 3.02 → 3
        assert_eq!(m.func_delta(5, FUNC_DE, 6), 0); // 语境计数 3 < 4 → 0
        assert_eq!(m.func_delta(7, FUNC_DE, 8), 0); // 语境未命中 → 0
        assert_eq!(m.func_delta(3, 9, 1), 0); // 非法粒子选择子 → 0
    }

    #[test]
    fn rejects_structural_corruption() {
        let dir = crate::test_tmp::dir();

        // a) Block 1 声明 1 行但行字节缺失（row_count 与实际不符）
        let mut p = Vec::new();
        block_hdr(&mut p, 1, 1);
        let path = write_model("mini-row-missing.x1gm", &wrap(&p, None));
        assert!(GrammarMicro::load_from_path(&path).is_err());

        // b) 词性 ID >= 13
        let mut p = Vec::new();
        block_hdr(&mut p, 1, 1);
        p.push(13);
        put_u32(&mut p, 7);
        let path = write_model("mini-bad-class.x1gm", &wrap(&p, None));
        assert!(GrammarMicro::load_from_path(&path).is_err());

        // c) Block 4 len = 3（必须 4–6）
        let mut p = Vec::new();
        block_hdr(&mut p, 1, 0);
        block_hdr(&mut p, 2, 0);
        block_hdr(&mut p, 3, 0);
        block_hdr(&mut p, 4, 1);
        p.push(3); // 非法 len
        p.extend_from_slice(&[0, 1, 2, 3, 4, 5]);
        put_u32(&mut p, 100);
        p.push(4);
        let path = write_model("mini-bad-blen.x1gm", &wrap(&p, None));
        assert!(GrammarMicro::load_from_path(&path).is_err());

        // d) Block 4 尾部填充不是 0xFF（len=4 时第 5 字节必须 0xFF）
        let mut p = Vec::new();
        block_hdr(&mut p, 1, 0);
        block_hdr(&mut p, 2, 0);
        block_hdr(&mut p, 3, 0);
        block_hdr(&mut p, 4, 1);
        p.push(4);
        p.extend_from_slice(&[0, 1, 2, 3, 0, 0xFF]); // 填充字节错
        put_u32(&mut p, 100);
        p.push(4);
        let path = write_model("mini-bad-pad.x1gm", &wrap(&p, None));
        assert!(GrammarMicro::load_from_path(&path).is_err());

        // e) 缺少 Block 5（1..=5 必须全到场）
        let mut p = Vec::new();
        for id in 1..=4u8 {
            block_hdr(&mut p, id, 0);
        }
        let path = write_model("mini-missing-block.x1gm", &wrap(&p, None));
        assert!(GrammarMicro::load_from_path(&path).is_err());

        // f) Block 5 之后有尾巴（block_id 序列错乱）
        let mut full = minimal_model();
        full.push(0xAA);
        let path = write_model("mini-trailing.x1gm", &full);
        assert!(GrammarMicro::load_from_path(&path).is_err());

        // g) 头部 model_hash 篡改
        let mini = minimal_model();
        let payload = &mini[32..];
        let path = write_model("mini-bad-hash.x1gm", &wrap(payload, Some(0xDEAD_BEEF)));
        assert!(GrammarMicro::load_from_path(&path).is_err());

        // h) 脏 payload（一字节翻转 → FNV 不符）
        let mut dirty = minimal_model();
        let last = dirty.len() - 1;
        dirty[last] ^= 0xFF;
        let path = write_model("mini-dirty.x1gm", &dirty);
        assert!(GrammarMicro::load_from_path(&path).is_err());

        // i) 空 payload（零 block 也缺 1..=5）
        let path = write_model("mini-empty-payload.x1gm", &wrap(&[], None));
        assert!(GrammarMicro::load_from_path(&path).is_err());

        // j) 空文件
        let path = dir.join("mini-zero.x1gm");
        std::fs::write(&path, b"").unwrap();
        assert!(GrammarMicro::load_from_path(&path).is_err());
    }
}
