//! C FFI 接口 — Swift 调用入口
//!
//! 内存管理规则：
//! - Rust 分配的所有内存，必须通过对应的 `x1_*_free` 函数释放
//! - Swift 不得直接解引用 Rust 指针
//! - 字符串使用 UTF-8 C 字符串（null-terminated）
//!
//! 线程契约（C ABI）：Engine 内含 RefCell ×3 + rusqlite Connection
//!（!Sync），**全部导出函数必须在同一线程调用**（宿主前端的输入法主线程 /
//! 引擎服务单线程事件循环）。跨线程并发调用同一 `X1Engine*` = 数据竞争
//!(RefCell already borrowed panic / Connection 别名 UB）。指针不得复制到其他线程。
//!
//! 子模块划分：本 mod.rs 保留：
//! - 不透明指针类型 X1Engine + panic 日志 helper log_panic
//! - 生命周期函数（new / new_with_db / free / migrate_legacy_db_if_needed / flush / is_dirty / string_free）
//! - 跨子模块共用的辅助函数（c_str_to_str / parse_user_word_json / candidates_to_json / features_to_json）

use alloc::ffi::CString;
use alloc::string::String;
use core::ffi::c_char;

use crate::engine::Engine;

// 子模块声明。
pub(crate) mod config;
mod dict_admin;
mod learning;
pub(crate) mod query;
/// 双库分离 FFI（引导链 + open_split，失败回退合并模式）。
mod split;
/// 输入统计 FFI（宿主前端采集器调用）。
mod stats;

/// 不透明指针类型
pub struct X1Engine {
    pub(crate) inner: Engine,
}

/// 把 panic 信息写到文件（eprintln 在 IMK 沙盒被吞，必须写文件才能定位）。
///
/// 路径优先级（编译期平台分派，避免运行时按 HOME 探测在部分平台写错路径）：
/// 1. 环境变量 `X1_PANIC_LOG` 显式指定（任意平台，嵌入式/CI 场景逃生口）
/// 2. 平台默认：
///    - Windows：`%APPDATA%\x1pinyin\rust_panic.log`
///    - macOS：`~/Library/Application Support/x1pinyin/rust_panic.log`
///    - 其他（Linux/BSD 等）：`${XDG_DATA_HOME:-~/.local/share}/x1pinyin/rust_panic.log`
/// 环境变量缺失时静默跳过（与原行为一致）。
pub(super) fn log_panic(fn_name: &str, payload: &(dyn core::any::Any + Send)) {
    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "(unknown panic)".to_string()
    };
    let line = format!("[x1core panic] {} '{}'\n", fn_name, msg);
    if let Some(p) = panic_log_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
    }
}

/// panic 日志路径解析（拆出便于测试）：`X1_PANIC_LOG` 环境变量覆盖 > 平台默认。
pub(super) fn panic_log_path() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("X1_PANIC_LOG") {
        return Some(std::path::PathBuf::from(p));
    }
    default_panic_log_path()
}

fn default_panic_log_path() -> Option<std::path::PathBuf> {
    const FILE: &str = "rust_panic.log";
    if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(|a| std::path::PathBuf::from(a).join("x1pinyin").join(FILE))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| {
            std::path::PathBuf::from(h)
                .join("Library/Application Support/x1pinyin")
                .join(FILE)
        })
    } else {
        // Linux/BSD 等：XDG 数据目录约定（XDG_DATA_HOME 未设时 ~/.local/share）
        std::env::var_os("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
            })
            .map(|d| d.join("x1pinyin").join(FILE))
    }
}

/// FFI panic 护栏：catch_unwind + 统一 panic 日志，异常时返回 `fallback`。
///
/// panic 穿越 `extern "C"` 边界 = 进程终止（宿主 app / 输入法进程直接死，
/// 无 dump）。非平凡导出函数（含查询/提交主链与词库管理、学习入口）统一经
/// 此护栏；纯 setter/getter 可豁免。
pub(super) fn guard<T>(name: &'static str, fallback: T, f: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(e) => {
            log_panic(name, &*e);
            fallback
        }
    }
}

/// 创建引擎实例（内置词库模式，无 SQLite）
///
/// # Safety
/// 返回的指针必须由 `x1_engine_free` 释放
#[no_mangle]
pub extern "C" fn x1_engine_new() -> *mut X1Engine {
    let engine = X1Engine {
        inner: Engine::new(),
    };
    Box::into_raw(Box::new(engine))
}

/// 创建引擎实例（SQLite 模式）
///
/// 从指定路径打开 / 创建 `x1pinyin.db`，加载 X1Dict 词库 + 用户词频。
/// 若 DB 中 X1Dict 表为空（未导入），回退到内置词库。
///
/// # Arguments
/// * `db_path` — SQLite 数据库文件路径（UTF-8 C 字符串）
///
/// # Returns
/// 成功返回引擎指针，失败返回 null（如路径不可写）
///
/// # Safety
/// `db_path` 必须是有效 UTF-8 C 字符串；返回的指针必须由 `x1_engine_free` 释放
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)] // FFI 边界：指针有效性由 C 调用方契约保证
pub extern "C" fn x1_engine_new_with_db(db_path: *const c_char) -> *mut X1Engine {
    let path = match unsafe { c_str_to_str(db_path) } {
        Some(s) => s,
        None => return core::ptr::null_mut(),
    };
    // 护栏：new_with_db 含 schema 迁移 + 全量加载，是最重的导出——
    // panic 跨 extern "C" = 进程直接死。失败兜底 null（调用方
    // 回退内存模式）。
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Engine::new_with_db(path))) {
        Ok(Ok(engine)) => Box::into_raw(Box::new(X1Engine { inner: engine })),
        Ok(Err(_)) => core::ptr::null_mut(),
        Err(e) => {
            log_panic("new_with_db", &*e);
            core::ptr::null_mut()
        }
    }
}

/// 旧加密 DB 一次性迁移（加密/混淆已整体移除）。
///
/// 旧版的发布用户 DB 三表（x1dict_words.freq /
/// seed_bigram_counts.count / seed_trigram_counts.count）是 XOR 密文 + 标记
/// `freq_encryption_version=v1`。加密代码已从引擎移除（无密钥可解），迁移策略
/// 是**丢弃密文数据**：用 bundle 明文种子覆盖 x1dict_words.freq、清空种子
/// 搭配表（下一次 sync 从明文种子重建）、清除标记，并重建内存 Trie + 种子
/// bigram 表。user_words / input_mappings 等用户数据表不受影响。
///
/// 返回：1=已迁移，0=无需迁移（无加密标记），-1=错误。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`builtin_path` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_migrate_legacy_db_if_needed(
    ptr: *mut X1Engine,
    builtin_path: *const c_char,
) -> i32 {
    if ptr.is_null() || builtin_path.is_null() {
        return -1;
    }
    let path = match c_str_to_str(builtin_path) {
        Some(s) => s,
        None => return -1,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let engine = &mut (*ptr).inner;
        engine.migrate_legacy_encrypted_db_if_needed(path)
    }));
    match result {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(e) => {
            log_panic("migrate_legacy_encrypted_db_if_needed", &*e);
            -1
        }
    }
}

/// 核心引擎版本。
///
/// 返回 NUL 终止的 `'static` UTF-8 C 字符串，调用方只读、**不得释放**
/// （编译期字面量，非堆分配）。
#[no_mangle]
pub extern "C" fn x1_engine_version() -> *const c_char {
    // concat! 在编译期把版本号与 "\0" 拼接成 'static 字节串。
    // 直接返回 str::as_ptr() 不保证尾部 NUL——Rust 字面量相邻内存任意，
    // C/Swift 侧 strlen 属越界读 UB。
    const VERSION_CSTR: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();
    VERSION_CSTR.as_ptr().cast()
}

/// 延迟写入 flush：将脏用户频率持久化到 SQLite
///
/// 应在定时器（每 60 秒）或应用退出时调用。
/// 若引擎未启用 SQLite（`x1_engine_new` 创建），则空操作。
///
/// # Returns
/// 0 = 成功或无操作，非 0 = SQLite 错误码
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_flush(ptr: *mut X1Engine) -> i32 {
    if ptr.is_null() {
        return 0;
    }
    let engine = &mut (*ptr).inner;
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.flush())) {
        Ok(Ok(())) => 0,
        Ok(Err(_)) => -1,
        Err(e) => {
            log_panic("flush", &*e);
            -2
        }
    }
}

/// 查询引擎是否有未持久化的脏数据
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_is_dirty(ptr: *const X1Engine) -> bool {
    if ptr.is_null() {
        return false;
    }
    let engine = &(*ptr).inner;
    engine.is_user_dict_dirty()
}

/// 释放引擎实例
///
/// # Safety
/// `ptr` 必须是由 `x1_engine_new` 返回的有效指针，且只能释放一次
#[no_mangle]
pub unsafe extern "C" fn x1_engine_free(ptr: *mut X1Engine) {
    if !ptr.is_null() {
        drop(Box::from_raw(ptr));
    }
}

/// 释放 Rust 分配的字符串
///
/// # Safety
/// `ptr` 必须是由 `x1_engine_process` 或 `x1_engine_save_user_dict` 返回的有效指针
#[no_mangle]
pub unsafe extern "C" fn x1_string_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(CString::from_raw(ptr));
    }
}

/// 内存诊断：返回引擎各 HashMap/BTreeMap 的 entry 数量（JSON 字符串）。
///
/// 用于排查输入法长时间运行后 RSS 膨胀。
/// 返回格式：`{"counts":N,"recent_scores":N,"pinyin_counts":N,"word_bigram_counts":N,
/// "bigram_inner_total":N,"input_mappings":N,"input_mappings_by_text":N,"green_memo":N,
/// "score_cache":N,"trigram_total_cache":N,"context":N,"user_words":N}`
///
/// 返回的字符串必须由 `x1_string_free` 释放。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_memory_stats(ptr: *const X1Engine) -> *mut c_char {
    if ptr.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let stats = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (counts, recent_scores, pinyin_counts, pinyin_recent, bigram_outer, bigram_inner) =
            engine.freq.memory_lens();
        format!(
            r#"{{"counts":{},"recent_scores":{},"pinyin_counts":{},"pinyin_recent_scores":{},"word_bigram_counts":{},"bigram_inner_total":{},"input_mappings":{},"input_mappings_by_text":{},"green_memo":{},"score_cache":{},"trigram_total_cache":{},"context":{},"user_words":{}}}"#,
            counts,
            recent_scores,
            pinyin_counts,
            pinyin_recent,
            bigram_outer,
            bigram_inner,
            engine.input_mappings.len(),
            engine.input_mappings_by_text.len(),
            engine.green_memo.len(),
            engine.score_cache.borrow().len(),
            engine.trigram_total_cache.borrow().len(),
            engine.context.len(),
            engine.user_words.len(),
        )
    })) {
        Ok(s) => s,
        Err(e) => {
            log_panic("memory_stats", &*e);
            return core::ptr::null_mut();
        }
    };
    match CString::new(stats) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
// 辅助函数（被子模块共享）
// ---------------------------------------------------------------------------

pub(super) unsafe fn c_str_to_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    let slice = core::ffi::CStr::from_ptr(ptr).to_bytes();
    core::str::from_utf8(slice).ok()
}

/// 剥掉键/值两侧的 JSON 结构字符与空白。
///
/// 两段式，顺序有讲究：
/// 1. 先反复剥空白/花括号/逗号——`{"text"` 去掉 `{` 后暴露出引号，`"jia ci"}`
///    去掉 `}` 后同样暴露引号，结构字符和空白必须同组反复剥到稳定（历史上
///    「先引号后花括号」的顺序让 key 变成 `"text`，整条静默丢失：单条导入
///    全部落空、N 条数组只剩最后一条能进库）。
/// 2. 引号只剥最外一对——值内可能含转义引号（`他说\"你好\"`），反复剥会把
///    转义序列一半的引号也吃掉，把 `他说"你好"` 写成 `他说"你好\`。
fn clean_token(s: &str) -> &str {
    let s = s.trim_matches(|c: char| c.is_whitespace() || matches!(c, '{' | '}' | ','));
    let b = s.as_bytes();
    if b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"' {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// 值字符串反转义（`\"` → `"`、`\\` → `\`）。导入词条文本可能含引号，JSON 里
/// 必须转义；不做反转义会把字面反斜杠存进用户词库。其余转义序列原样保留
/// （\n 等由词库文本自身承载，不在此层解释）。
fn unescape_value(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// 解析用户词导入 JSON。
///
/// 格式契约（与 `x1_engine_import_user_words` 头文档一致）：
/// `[{"text":"你好","pinyin":"ni hao","freq":5}, ...]`，拼音为空格分隔的
/// 音节序列（入库守卫要求「汉字数 == 音节数」，见 `user_word_pinyin_matches`）。
///
/// 已知边界（有意保留，不在此层解决）：按 `},{` 切分对象，若某个 text 值
/// 自身含未转义的 `},{` 会错切——中文用户词不会出现该序列；调用方如要支持
/// 任意 JSON 应自行解析后走 `Engine::import_user_words`。格式错误时返回
/// `Err(())` 或跳过该条，绝不 panic（FFI 边界）。
pub(super) fn parse_user_word_json(json: &str) -> Result<Vec<(String, String, u32)>, ()> {
    let trimmed = json.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return Err(());
    }
    let body = &trimmed[1..trimmed.len() - 1];
    if body.trim().is_empty() {
        return Ok(Vec::new());
    }

    let objects: Vec<&str> = body.split("},{").collect();
    let mut out = Vec::with_capacity(objects.len());
    for obj in objects {
        let mut text: Option<String> = None;
        let mut pinyin: Option<String> = None;
        let mut freq: u32 = 1;
        for pair in obj.split(',') {
            let mut kv = pair.splitn(2, ':');
            let key = clean_token(kv.next().unwrap_or(""));
            let value = unescape_value(clean_token(kv.next().unwrap_or("")));
            match key {
                "text" => text = Some(value),
                "pinyin" => pinyin = Some(value),
                "freq" => freq = value.parse().unwrap_or(1),
                _ => {}
            }
        }
        if let (Some(t), Some(p)) = (text, pinyin) {
            out.push((t, p, freq));
        }
    }
    Ok(out)
}

/// 单遍 JSON 字符串转义：替代 `.replace('\\',...).replace('"',...)` 的每候选
/// 2 次中间 String 分配。转义 `\`、`"` 及全部控制字符（RFC 8259 禁止字符串含
/// 裸 <0x20——TSV 可导入含 \n 等控制字符的词条，裸输出会让宿主前端候选
/// JSON 整体解析失败，单条脏数据放大为打字不出词）。
pub(crate) fn push_json_escaped(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&alloc::format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

pub(super) fn candidates_to_json(candidates: &[crate::dict::Candidate]) -> String {
    let mut json = String::from("[");
    for (i, c) in candidates.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push('{');
        json.push_str("\"text\":\"");
        push_json_escaped(&mut json, &c.text);
        json.push_str("\",\"freq\":");
        json.push_str(&alloc::format!("{}", c.base_freq));
        json.push_str(",\"pinyin\":\"");
        push_json_escaped(&mut json, &c.pinyin);
        json.push('"');
        // 纠错候选标记：仅 is_correction=true 时输出，保持向后兼容（正常候选无此字段）
        if c.is_correction {
            json.push_str(",\"is_correction\":true");
        }
        // 智能组合候选标记：仅 is_sentence=true 时输出（整句解码/多词拼接产生）
        // 候选框用绿色显示，让用户区分词库词（默认）、纠错词（橙色）、智能组合词（绿色）
        if c.is_sentence {
            json.push_str(",\"is_sentence\":true");
        }
        // 前缀补全猜测候选标记：仅 is_guess=true 时输出
        // 触发：末音节完整但精确结果少时，前缀补全拉来"末音节更长"的候选（如 yonghu
        // 拉来 yong huai 的「用坏」），候选框用紫色显示，提示这是对后续输入的猜测。
        if c.is_guess {
            json.push_str(",\"is_guess\":true");
        }
        // 快捷候选标记：仅 is_shortcut=true 时输出
        // 触发：/date /fh 等前缀产生的符号/日期/时间候选，候选框用蓝色显示。
        if c.is_shortcut {
            json.push_str(",\"is_shortcut\":true");
        }
        // 英文单词候选标记（智能单词识别）：仅 is_english=true 时输出。
        // text=纯小写英文单词、pinyin 空、选中全消费输入且不进中文学习面。
        if c.is_english {
            json.push_str(",\"is_english\":true");
        }
        json.push('}');
    }
    json.push(']');
    json
}

/// 把 (text, ScoreBreakdown) 列表序列化为 JSON（供学习型排序器训练数据记录）。
///
/// 格式：`[{"text":"...","features":{"base":N,"total_bonus":N,...,"total":N}}, ...]`
/// 实现在 engine::features_to_json（各消费端格式契约单一来源）；
/// 此处 re-export 保持子模块 `use super::features_to_json` 不变。
pub(super) use crate::engine::features_to_json;

#[cfg(test)]
mod tests {
    use super::*;

    /// 用户词导入 JSON 解析。
    ///
    /// 守护 2026-10-24 实测发现的缺陷：定界符剥离顺序错误导致
    /// ① 单条导入（头文档里的最小形态）整条落空；
    /// ② N 条数组只有最后一条进库（`split("},{")` 后仅末段以 `"` 开头）；
    /// ③ 对象最后一个字符串值残留引号（拼音被写坏）。
    #[test]
    fn parse_user_word_json_forms() {
        // 头文档的标准形态：单条必须解析出来（曾经的完全失败案例）
        assert_eq!(
            parse_user_word_json(r#"[{"text":"你好","pinyin":"ni hao","freq":5}]"#),
            Ok(vec![("你好".to_string(), "ni hao".to_string(), 5)])
        );
        // 多条：全部进入，不是只剩最后一条
        assert_eq!(
            parse_user_word_json(
                r#"[{"text":"甲词","pinyin":"jia ci","freq":1},{"text":"乙词","pinyin":"yi ci","freq":2},{"text":"丙词","pinyin":"bing ci","freq":3}]"#
            ),
            Ok(vec![
                ("甲词".to_string(), "jia ci".to_string(), 1),
                ("乙词".to_string(), "yi ci".to_string(), 2),
                ("丙词".to_string(), "bing ci".to_string(), 3),
            ])
        );
        // 键序无关：text 在前的首对曾被 `{` 前缀污染
        assert_eq!(
            parse_user_word_json(r#"[{"pinyin":"jia ci","text":"甲词","freq":1}]"#),
            Ok(vec![("甲词".to_string(), "jia ci".to_string(), 1)])
        );
        // freq 在前的形态：末对是字符串值时曾被 `}` 后缀污染出残留引号
        assert_eq!(
            parse_user_word_json(r#"[{"freq":1,"text":"甲词","pinyin":"jia ci"}]"#),
            Ok(vec![("甲词".to_string(), "jia ci".to_string(), 1)])
        );
        // 空白变体 + 缺 freq（默认 1）+ 字符串形态 freq
        assert_eq!(
            parse_user_word_json(r#"[{ "text" : "甲词" , "pinyin" : "jia ci" }]"#),
            Ok(vec![("甲词".to_string(), "jia ci".to_string(), 1)])
        );
        assert_eq!(
            parse_user_word_json(r#"[{"text":"甲词","pinyin":"jia ci","freq":"7"}]"#),
            Ok(vec![("甲词".to_string(), "jia ci".to_string(), 7)])
        );
        // 值内转义引号：反转义后入库，不残留字面反斜杠
        assert_eq!(
            parse_user_word_json(r#"[{"text":"他说\"你好\"","pinyin":"ta shuo","freq":1}]"#),
            Ok(vec![("他说\"你好\"".to_string(), "ta shuo".to_string(), 1)])
        );
        // 畸形输入：不 panic，要么 Err 要么跳过该条
        assert!(parse_user_word_json("not json").is_err());
        assert!(parse_user_word_json("[{").is_err());
        assert_eq!(parse_user_word_json("[]"), Ok(Vec::new()));
        assert_eq!(parse_user_word_json("[{}]"), Ok(Vec::new()));
        // 缺 pinyin 的条目跳过，不影响同数组其他条目
        assert_eq!(
            parse_user_word_json(r#"[{"text":"甲词"},{"text":"乙词","pinyin":"yi ci"}]"#),
            Ok(vec![("乙词".to_string(), "yi ci".to_string(), 1)])
        );
    }

    /// X1_PANIC_LOG 覆盖优先 + 平台默认路径形状（单测试避免 env 竞态）。
    #[test]
    fn panic_log_path_env_override_and_default_shape() {
        std::env::set_var("X1_PANIC_LOG", "Z:/nonexistent/override_panic.log");
        assert_eq!(
            panic_log_path(),
            Some(std::path::PathBuf::from(
                "Z:/nonexistent/override_panic.log"
            ))
        );
        std::env::remove_var("X1_PANIC_LOG");

        // 平台默认：文件名必须是 rust_panic.log（真实环境 HOME/APPDATA 必有其一）
        let p = default_panic_log_path()
            .expect("平台默认 panic 日志路径不应为 None（HOME/APPDATA 缺失）");
        assert_eq!(
            p.file_name().and_then(|f| f.to_str()),
            Some("rust_panic.log"),
            "平台默认路径应以 rust_panic.log 结尾：{:?}",
            p
        );
    }
}
