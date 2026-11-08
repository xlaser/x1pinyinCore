//! 输入统计 FFI（宿主前端采集器调用）。
//!
//! **平台差异说明**：带引擎服务的平台采集在前端进程内、聚合在服务进程
//！（经 IPC 上报）；单进程宿主采集在按键热路径（纯内存），聚合与落库直接
//! 调用本模块（无 IPC 层）。**存储与 JSON 契约一致**（同一 `x1stats.db`
//! 结构，见 `crate::stats` 模块注释）。
//!
//! **线程模型**：全局 `Mutex<Option<StatsStore>>` 持有一个统计库连接。调用方
//! 约定在**后台队列**调用（按键热路径只做内存累加，不在此列），
//! 锁竞争仅发生在 30s 级批量落库与用户打开统计页查询之间。
//!
//! **失败语义**：全部静默降级（统计宁缺毋滥）——统计库打不开/写失败绝不影响
//! 打字；查询失败返回 `"{}"`。
//!
//! **与引擎完全隔离**：统计库是独立文件，本模块不触碰 `x1pinyin.db` 的任何表。

use alloc::ffi::CString;
use alloc::string::String;
use core::ffi::c_char;
use std::sync::Mutex;

use super::{c_str_to_str, guard};
use crate::stats::StatsStore;

/// 全局统计库（懒打开：首个写入/查询才建库建表，启动零成本）。
static STATS: Mutex<Option<StatsStore>> = Mutex::new(None);

/// 在全局 store 上执行闭包（未初始化时返回 None，调用方静默降级）。
fn with_store<T>(f: impl FnOnce(&mut StatsStore) -> T) -> Option<T> {
    let mut g = STATS.lock().ok()?;
    g.as_mut().map(f)
}

/// 初始化统计库路径。
///
/// `dir` = 数据目录（mac：`~/Library/Application Support/x1pinyin`），库文件为
/// 其下 `x1stats.db`。可在任意时刻调用（重复调用换目录会丢弃旧连接，下次
/// 访问时按新路径重开）。空路径 = 统计禁用。
///
/// # Safety
/// `dir` 必须是有效的 NUL 结尾 C 字符串。
#[no_mangle]
pub unsafe extern "C" fn x1_stats_init(dir: *const c_char) -> bool {
    guard("x1_stats_init", false, || {
        let d = match c_str_to_str(dir) {
            Some(s) if !s.is_empty() => s,
            _ => {
                // 空路径 = 显式禁用统计
                if let Ok(mut g) = STATS.lock() {
                    *g = None;
                }
                return false;
            }
        };
        let path = std::path::PathBuf::from(d).join("x1stats.db");
        if let Ok(mut g) = STATS.lock() {
            *g = Some(StatsStore::new(Some(path)));
            return true;
        }
        false
    })
}

/// 统计是否可用（已 init 且路径有效）。
#[no_mangle]
pub extern "C" fn x1_stats_enabled() -> bool {
    guard("x1_stats_enabled", false, || {
        STATS
            .lock()
            .map(|g| g.as_ref().map(|s| s.enabled()).unwrap_or(false))
            .unwrap_or(false)
    })
}

/// 上报一批按键统计（采集器 30s / 上屏 / 失焦时调用）。
///
/// - `keys_nl`：键名以换行分隔的串（如 `"a\nspace\nenter"`），`count` = 键数；
/// - `zh_cnt` / `en_cnt`：两两平行的计数数组（长度 ≥ count），下标与键名一一对应；
/// - 标量：中文模式字母键（码长分子）/ 英文可见字符 / 退格 / 翻页 / 活跃毫秒。
///
/// 键名与 `KNOWN_KEYS`（三方契约）一致；未知键名照记（DB 层不做白名单，
/// 便于未来扩展），UI 侧未收录的名字自动不着色。
///
/// # Safety
/// 三个指针必须有效；`zh_cnt`/`en_cnt` 至少 `count` 个元素。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn x1_stats_note(
    keys_nl: *const c_char,
    count: usize,
    zh_cnt: *const i64,
    en_cnt: *const i64,
    zh_letters: i64,
    en_chars: i64,
    backspaces: i64,
    page_turns: i64,
    active_ms: i64,
) {
    guard("x1_stats_note", (), || {
        let names = match c_str_to_str(keys_nl) {
            Some(s) => s,
            None => return,
        };
        let mut keys: alloc::vec::Vec<(String, i64, i64)> = alloc::vec::Vec::new();
        if !zh_cnt.is_null() && !en_cnt.is_null() {
            for (i, name) in names.split('\n').take(count).enumerate() {
                if name.is_empty() {
                    continue;
                }
                let zh = *zh_cnt.add(i);
                let en = *en_cnt.add(i);
                if zh == 0 && en == 0 {
                    continue;
                }
                keys.push((String::from(name), zh, en));
            }
        }
        with_store(|s| {
            s.add_keys(
                &keys, zh_letters, en_chars, backspaces, page_turns, active_ms,
            )
        });
    })
}

/// 上报一次上屏（提交）。
///
/// - `text`：本步实际上屏文本（组合逐步提交时传**本步增量**，防重复计数）；
/// - `green`：绿词/整句候选；`first_pick`：首选命中（选中第 1 项）。
///
/// # Safety
/// `text` 必须是有效的 NUL 结尾 C 字符串。
#[no_mangle]
pub unsafe extern "C" fn x1_stats_commit(text: *const c_char, green: bool, first_pick: bool) {
    guard("x1_stats_commit", (), || {
        let t = match c_str_to_str(text) {
            Some(s) => s,
            None => return,
        };
        with_store(|s| s.add_commit(t, green, first_pick));
    })
}

/// 批量落库：keys 批 + 多条 commit **单事务合并写**。
///
/// 采集器的落库节奏是「键统计快照 + 上屏 commits 缓存批」成对出现（persist
/// 路径），分开调用 = 两个事务，WAL 每事务整页追加（日汇总行两次碰）。合并为
/// 本函数单事务后页触碰减半；多条 commit 攒批零丢失（调用方 pendingCommits
/// 内存缓存）。
///
/// `commit_lines`：多条 commit 的行编码，每行 `gf<TAB>text`（g/f = '1'/'0'
/// 依次为 green/firstPick；text 内 `\\` `\n` `\t` 反斜杠转义）。空串 = 纯 keys 批。
/// **语义与分开调用一致**；引擎服务侧走 IPC 两条消息两个事务（单写者自身
/// 节流），本函数是直连采集路径的实现优化，不影响 DDL/查询契约。
///
/// # Safety
/// 指针参数必须有效；`zh_cnt`/`en_cnt` 至少 `count` 个元素。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn x1_stats_flush_all(
    keys_nl: *const c_char,
    count: usize,
    zh_cnt: *const i64,
    en_cnt: *const i64,
    zh_letters: i64,
    en_chars: i64,
    backspaces: i64,
    page_turns: i64,
    active_ms: i64,
    commit_lines: *const c_char,
) {
    guard("x1_stats_flush_all", (), || {
        let mut keys: alloc::vec::Vec<(String, i64, i64)> = alloc::vec::Vec::new();
        if let Some(names) = c_str_to_str(keys_nl) {
            if !zh_cnt.is_null() && !en_cnt.is_null() {
                for (i, name) in names.split('\n').take(count).enumerate() {
                    if name.is_empty() {
                        continue;
                    }
                    let zh = *zh_cnt.add(i);
                    let en = *en_cnt.add(i);
                    if zh == 0 && en == 0 {
                        continue;
                    }
                    keys.push((String::from(name), zh, en));
                }
            }
        }
        let blob = c_str_to_str(commit_lines).unwrap_or("");
        let commits = decode_commit_lines(blob);
        let refs: alloc::vec::Vec<(&str, bool, bool)> = commits
            .iter()
            .map(|(t, g, f)| (t.as_str(), *g, *f))
            .collect();
        with_store(|s| {
            s.add_batch(
                &crate::stats::StatsStore::now(),
                &keys,
                zh_letters,
                en_chars,
                backspaces,
                page_turns,
                active_ms,
                &refs,
            )
        });
    })
}

/// 解码 commit_lines 编码串（每行 `gf<TAB>text`，text 内 `\\` `\n` `\t` 反转义）。
fn decode_commit_lines(blob: &str) -> alloc::vec::Vec<(String, bool, bool)> {
    let mut out = alloc::vec::Vec::new();
    for line in blob.lines() {
        let Some((flags, text)) = line.split_once('\t') else {
            continue;
        };
        let mut chars = flags.chars();
        let green = chars.next() == Some('1');
        let first_pick = chars.next() == Some('1');
        let mut unescaped = String::with_capacity(text.len());
        let mut it = text.chars();
        while let Some(c) = it.next() {
            if c == '\\' {
                match it.next() {
                    Some('n') => unescaped.push('\n'),
                    Some('t') => unescaped.push('\t'),
                    Some('\\') => unescaped.push('\\'),
                    Some(other) => {
                        unescaped.push('\\');
                        unescaped.push(other);
                    }
                    None => unescaped.push('\\'),
                }
            } else {
                unescaped.push(c);
            }
        }
        out.push((unescaped, green, first_pick));
    }
    out
}

/// 查询某视图（scope = day|week|month|year|all）→ JSON 字符串。
///
/// 形状与引擎服务侧 `stats_query` 响应体一致（totals/series/heat/
/// topChars/topWords/records）。失败返回 `"{}"`。
/// 返回的字符串必须由 `x1_string_free` 释放。
///
/// # Safety
/// `scope` 必须是有效的 NUL 结尾 C 字符串。
#[no_mangle]
pub unsafe extern "C" fn x1_stats_query(scope: *const c_char) -> *mut c_char {
    guard("x1_stats_query", core::ptr::null_mut(), || {
        let sc = c_str_to_str(scope).unwrap_or("day");
        let json = with_store(|s| s.query_json(sc)).unwrap_or_else(|| String::from("{}"));
        match CString::new(json) {
            Ok(c) => c.into_raw(),
            Err(_) => core::ptr::null_mut(),
        }
    })
}

/// 重置某视图：删除该时段覆盖的全部原始行（日/周/月/年/全部）。
///
/// 返回 true = 执行成功；false = 统计不可用或失败。
///
/// # Safety
/// `scope` 必须是有效的 NUL 结尾 C 字符串。
#[no_mangle]
pub unsafe extern "C" fn x1_stats_reset(scope: *const c_char) -> bool {
    guard("x1_stats_reset", false, || {
        let sc = c_str_to_str(scope).unwrap_or("");
        with_store(|s| s.reset(sc)).unwrap_or(false)
    })
}

/// 键名表（契约）以换行分隔返回，供宿主前端启动自检漂移
/// （键码映射必须只产出这些名字）。
/// 返回的字符串必须由 `x1_string_free` 释放。
#[no_mangle]
pub extern "C" fn x1_stats_known_keys() -> *mut c_char {
    guard("x1_stats_known_keys", core::ptr::null_mut(), || {
        let joined = crate::stats::KNOWN_KEYS.join("\n");
        match CString::new(joined) {
            Ok(c) => c.into_raw(),
            Err(_) => core::ptr::null_mut(),
        }
    })
}

/// 当前本地「日」字符串（YYYY-MM-DD）——供宿主前端展示/日志对齐时间口径。
/// 返回的字符串必须由 `x1_string_free` 释放。
#[no_mangle]
pub extern "C" fn x1_stats_today() -> *mut c_char {
    guard(
        "x1_stats_today",
        core::ptr::null_mut(),
        || match CString::new(crate::stats::today_string()) {
            Ok(c) => c.into_raw(),
            Err(_) => core::ptr::null_mut(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    /// FFI 全链单一用例（**故意不拆**：三者共享全局 `STATS`，cargo 并行执行
    /// 时互相切换禁用/启用状态会随机失败——共享全局的 FFI 测试必须串行在
    /// 一个函数内）。
    #[test]
    fn ffi_contract() {
        let dir = crate::test_tmp::dir().join(format!("x1stats_ffi_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // ① 未初始化：全部调用安全（静默降级，不 panic）
        unsafe {
            assert!(!x1_stats_init(c("").as_ptr()), "空路径 = 禁用");
            assert!(!x1_stats_enabled());
            let zh = [1i64];
            let en = [0i64];
            x1_stats_note(
                c("a").as_ptr(),
                1,
                zh.as_ptr(),
                en.as_ptr(),
                1,
                0,
                0,
                0,
                100,
            );
            x1_stats_commit(c("你好").as_ptr(), false, true);
            let q = x1_stats_query(c("day").as_ptr());
            assert_eq!(
                CString::from_raw(q).into_string().unwrap(),
                "{}",
                "未初始化 = 统计禁用"
            );
            assert!(!x1_stats_reset(c("day").as_ptr()));
        }

        // ② 键名表 FFI
        unsafe {
            let out = x1_stats_known_keys();
            assert!(!out.is_null(), "键名表可返回");
            let s = CString::from_raw(out).into_string().unwrap();
            let names: Vec<&str> = s.split('\n').collect();
            assert_eq!(names.len(), crate::stats::KNOWN_KEYS.len());
            for k in ["a", "z", "0", "9", "space", "backspace", "win", "fn"] {
                assert!(names.contains(&k), "缺 {}", k);
            }
        }

        // ③ init → note/commit → query → reset 全链
        unsafe {
            assert!(x1_stats_init(c(dir.to_str().unwrap()).as_ptr()));
            assert!(x1_stats_enabled());

            let names = c("a\nspace");
            let zh = [10i64, 5];
            let en = [3i64, 0];
            x1_stats_note(
                names.as_ptr(),
                2,
                zh.as_ptr(),
                en.as_ptr(),
                40,
                12,
                3,
                2,
                65000,
            );
            x1_stats_commit(c("你好").as_ptr(), true, true);

            let json = CString::from_raw(x1_stats_query(c("day").as_ptr()))
                .into_string()
                .unwrap();
            assert!(json.contains("\"zh\":2"), "上屏字数: {}", json);
            assert!(json.contains("\"green\":1"));
            assert!(json.contains("\"firstPick\":1"));
            assert!(json.contains("\"zhKeys\":40"));
            assert!(json.contains(r#"["a",10,3]"#), "热力图: {}", json);
            assert!(json.contains(r#"["你好",1]"#), "热词: {}", json);

            assert!(x1_stats_reset(c("day").as_ptr()));
            let json2 = CString::from_raw(x1_stats_query(c("day").as_ptr()))
                .into_string()
                .unwrap();
            assert!(json2.contains("\"zh\":0"), "重置后: {}", json2);

            // 未知 scope → "{}"
            let json3 = CString::from_raw(x1_stats_query(c("bogus").as_ptr()))
                .into_string()
                .unwrap();
            assert_eq!(json3, "{}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
