//! 词典编译缓存（`dict_arena_cache.bin`）——词库紧凑格式 + mmap 的读写层。
//!
//! 文件 = 头 + 三个自包含子 blob（各自带魔数/段表/全量校验，见 trie_arena /
//! dict.rs WordIndexSpans / Dict::encode_single_chars）：
//! ```text
//! off 0  magic "X1DAC001" (8B)
//! off 8  hash 16B（x1dict_sync_hash，失效键）
//! off 24 rev  u64 LE（x1dict_rev，管理器编辑自增）
//! off 32 rows u32 LE（x1dict_words 行数哨兵，防绕过 API 的裸改）
//! off 36 arena_len / wi_len / sc_len (u32×3)
//! off 48 [arena][wi][single_chars]
//! ```
//!
//! 失效键 = (格式魔数, sync_hash, rev, rows)：任一处不匹配/文件缺失/任何
//! 校验失败 → None → 调用方回退「SQL 构建」慢路径并重写缓存（自愈；同
//! seed_bigram 缓存先例）。无 hash 或 rev 的 DB（开发种子/全新库）
//! 不读也不写——防测试污染工作树（同款约定）。
//!
//! mmap 纪律（Windows 关键）：映射区持有期间不可删除/替换目标文件——
//! 写路径（save）必须在替换持有映射的 Dict 之后调用（引擎接线保证）；
//! 本模块内 tmp+删+rename 的「先删后改名」在目标无活跃映射时无碍。
//! `X1_DICT_CACHE=0` 整关缓存；`X1_DICT_MMAP=0` 只关映射（调试旋钮）。

use std::path::{Path, PathBuf};

use crate::dict::{Dict, WordIndexSpans};
use crate::storage::Storage;
use crate::trie_arena::TrieArena;

const MAGIC: &[u8; 8] = b"X1DAC001";
const HEADER_LEN: usize = 48;

/// 缓存文件路径（按 DB 文件名：`dict.db` → `dict.arena_cache.bin`）。
///
/// 必须带 DB 文件名而不是固定名：固定名（`with_file_name("dict_arena_cache.bin")`）
/// 会让同一目录下多个不同 DB 共享一个缓存文件——键（hash/rev/rows）拦得住
/// 大多数，但「同种子的两份副本、编辑历史相同」会真误命中，且两库交替
/// 装载时缓存互相覆盖成反复 miss。文件名带 DB 名后一库一缓存，无共享。
pub(crate) fn cache_path_for(db: &Path) -> PathBuf {
    db.with_extension("arena_cache.bin")
}

/// 缓存文件路径（storage 入口版本）。
fn cache_path(storage: &Storage) -> Option<PathBuf> {
    storage.dict_path().map(cache_path_for)
}

/// 失效键：无（sync_hash 或行数读不到）= 不参与缓存。
/// rev 缺省 0（本键引入前的旧库；首次编辑自增即失效旧缓存）。
fn keys(storage: &Storage) -> Option<(String, u64, u32)> {
    let hash = storage
        .get_seed_meta("x1dict_sync_hash")
        .ok()
        .flatten()
        .filter(|h| h.len() == 16)?;
    let rev = storage
        .get_seed_meta("x1dict_rev")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let rows_i = storage.x1dict_count().ok()?;
    if rows_i < 0 {
        return None;
    }
    Some((hash, rev, rows_i as u32))
}

fn cache_disabled() -> bool {
    std::env::var("X1_DICT_CACHE")
        .map(|v| v == "0")
        .unwrap_or(false)
}

fn mmap_disabled() -> bool {
    std::env::var("X1_DICT_MMAP")
        .map(|v| v == "0")
        .unwrap_or(false)
}

/// 头校验（hash/rev/rows 全匹配 + 段长自洽 + 总长精确）。
fn parse_header(bytes: &[u8], hash: &str, rev: u64, rows: u32) -> Option<(usize, usize, usize)> {
    if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC.as_slice() {
        return None;
    }
    if &bytes[8..24] != hash.as_bytes() {
        return None;
    }
    let get_u64 = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
    let get_u32 = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    if get_u64(24) != rev || get_u32(32) != rows {
        return None;
    }
    let a = get_u32(36) as usize;
    let w = get_u32(40) as usize;
    let s = get_u32(44) as usize;
    if HEADER_LEN + a + w + s != bytes.len() {
        return None; // 精确长度（尾随垃圾/截断都拒绝）
    }
    Some((a, w, s))
}

/// 装载：返回 (arena, wi, single_chars 编码字节)。任何失败 → None。
pub(crate) fn load(storage: &Storage) -> Option<(TrieArena, WordIndexSpans, Vec<u8>)> {
    if cache_disabled() {
        return None;
    }
    let (hash, rev, rows) = keys(storage)?;
    let path = cache_path(storage)?;

    // ① mmap 零拷贝路径（默认尝试；X1_DICT_MMAP=0 或平台失败 → 回退）
    if !mmap_disabled() {
        if let Some(region) = crate::mmap_file::MmapRegion::open(&path) {
            let bytes = region.bytes();
            if let Some((a, w, s)) = parse_header(bytes, &hash, rev, rows) {
                let arena = TrieArena::from_mapped_range(region.clone(), HEADER_LEN, a);
                let wi = WordIndexSpans::from_mapped_range(region.clone(), HEADER_LEN + a, w);
                if let (Some(arena), Some(wi)) = (arena, wi) {
                    let sc = bytes[HEADER_LEN + a + w..HEADER_LEN + a + w + s].to_vec();
                    return Some((arena, wi, sc));
                }
            }
            // 校验失败 → 落回自有读（再校验一次，等价的快速失败）或直接 None
        }
    }

    // ② 自有读回退（mmap 关闭/失败/校验失败）
    let bytes = std::fs::read(&path).ok()?;
    let (a, w, s) = parse_header(&bytes, &hash, rev, rows)?;
    let arena = TrieArena::from_bytes(bytes[HEADER_LEN..HEADER_LEN + a].to_vec())?;
    let wi = WordIndexSpans::from_bytes(bytes[HEADER_LEN + a..HEADER_LEN + a + w].to_vec())?;
    let sc = bytes[HEADER_LEN + a + w..HEADER_LEN + a + w + s].to_vec();
    Some((arena, wi, sc))
}

/// 保存（finish_load 完成态；`dict` 必须是完成态否则静默跳过）。
/// 失败静默（缓存可重建，下次启动重试）。
///
/// 调用前提：替换持有旧映射的 Dict 之后再调用本函数（Windows 删除映射
/// 中文件必败——先 drop 旧映射；见模块头 mmap 纪律）。
pub(crate) fn save(storage: &Storage, dict: &Dict) {
    if cache_disabled() {
        return;
    }
    let Some((hash, rev, rows)) = keys(storage) else {
        return;
    };
    let Some(path) = cache_path(storage) else {
        return;
    };
    let Some((arena_b, wi_b, sc)) = dict.cached_blobs() else {
        return;
    };
    // 流式写：不再先拼一个「头 + 三段」的 19.6MB `out` 缓冲。
    // 缓存文件 ≈ 19.6MB，拼接版在构建路径末尾额外制造一份同等大小的瞬态，
    // 部分平台的分配器不会及时归还这份瞬态（留在进程 footprint 里）。
    // 头字段长度已知（段就是 arena/wi/sc 的借用切片），直接顺序写即可。
    let tmp = path.with_extension("bin.tmp");
    let written = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        let mut head = [0u8; HEADER_LEN];
        head[..8].copy_from_slice(MAGIC);
        head[8..24].copy_from_slice(hash.as_bytes());
        head[24..32].copy_from_slice(&rev.to_le_bytes());
        head[32..36].copy_from_slice(&rows.to_le_bytes());
        head[36..40].copy_from_slice(&(arena_b.len() as u32).to_le_bytes());
        head[40..44].copy_from_slice(&(wi_b.len() as u32).to_le_bytes());
        head[44..48].copy_from_slice(&(sc.len() as u32).to_le_bytes());
        f.write_all(&head)?;
        f.write_all(arena_b)?;
        f.write_all(wi_b)?;
        f.write_all(&sc)
    })();
    if written.is_ok() {
        // Windows 的 rename 不覆盖已存在目标：先删后改名（缓存可重建，崩在
        // 删后改名前只是下次启动重建，无正确性影响；同 seed_bigram 缓存）。
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::rename(&tmp, &path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// 缓存文件是否存在于给定词库路径旁（诊断/测试辅助）。
#[cfg(test)]
pub(crate) fn cache_exists(dict_path: &Path) -> bool {
    cache_path_for(dict_path).exists()
}
