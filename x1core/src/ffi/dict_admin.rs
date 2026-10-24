//! 词库管理类 FFI 接口：系统词库 / 用户词库 增删改查与导入导出。
//!
//! 从 ffi.rs 拆出，函数体零改动。

use alloc::ffi::CString;
use alloc::string::String;
use core::ffi::{c_char, CStr};

use super::{c_str_to_str, guard, parse_user_word_json, X1Engine};
use crate::storage::{Storage, X1DictWord};

/// 查询指定路径 DB 中 x1dict_words 条目数（只读，不创建文件）
///
/// # Safety
/// `path` 必须是有效 UTF-8 C 字符串
/// # Returns
/// >=0 的条目数，-1 表示查询失败
#[no_mangle]
pub unsafe extern "C" fn x1_engine_x1dict_count_at_path(path: *const c_char) -> i32 {
    let path = match c_str_to_str(path) {
        Some(s) => s,
        None => return -1,
    };
    match Storage::open_readonly(path) {
        Ok(storage) => storage.x1dict_count().unwrap_or(-1) as i32,
        Err(_) => -1,
    }
}

/// 查询当前 DB 中 x1dict_words 条目数
///
/// # Safety
/// `ptr` 必须是有效引擎指针
/// # Returns
/// >=0 的条目数，-1 表示未启用 SQLite 或查询失败
#[no_mangle]
pub unsafe extern "C" fn x1_engine_x1dict_count(ptr: *const X1Engine) -> i32 {
    if ptr.is_null() {
        return -1;
    }
    let engine = &(*ptr).inner;
    engine.x1dict_count() as i32
}

/// 从内置词库同步 x1dict_words，保留 user_words 不变
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`builtin_path` 必须是有效 UTF-8 C 字符串
/// # Returns
/// 0 表示成功，非 0 表示失败
#[no_mangle]
pub unsafe extern "C" fn x1_engine_sync_x1dict(
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
    let engine = &mut (*ptr).inner;
    guard("sync_x1dict", -1, || {
        if engine.sync_x1dict_from_builtin(path) {
            0
        } else {
            -1
        }
    })
}

/// freq 压扁迁移——已废弃为 no-op（已取消压扁回归真实值）。
///
/// FFI 签名保留（宿主端仍调用，防链接断裂）；内部
/// `flatten_freq_and_reload` 为 no-op。历史压扁 DB 的修复改走
/// `x1_engine_restore_x1dict_freq_and_reload`（用种子 DB 覆盖 freq）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_flatten_freq_and_reload(ptr: *mut X1Engine) -> i32 {
    if ptr.is_null() {
        return 0;
    }
    let engine = &mut (*ptr).inner;
    engine.flatten_freq_and_reload() as i32
}

/// 用 bundle 种子 DB 的 x1dict_words 频率覆盖用户 DB（数据修复迁移）。
///
/// 修复 `ensure_freq_flattened` 幂等性问题导致的用户 DB freq 全 100 问题。
/// 检测 db_meta `freq_unflattened_v1` 标记不存在 → 执行覆盖 → 写标记。
/// 只动 x1dict_words.freq/is_primary，保留 user_words 等用户数据表。
///
/// 返回更新的词条数（0 表示已迁移过或失败）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`builtin_path` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_restore_x1dict_freq_and_reload(
    ptr: *mut X1Engine,
    builtin_path: *const c_char,
) -> i32 {
    if ptr.is_null() || builtin_path.is_null() {
        return 0;
    }
    let engine = &mut (*ptr).inner;
    let path = match CStr::from_ptr(builtin_path).to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    guard("restore_x1dict_freq_and_reload", 0, || {
        engine.restore_x1dict_freq_and_reload(path) as i32
    })
}

/// 检查是否需要从内置 DB 同步 x1dict_words（基于内容哈希，而非行数比较）
///
/// 返回 1 表示需要同步（内置词库已更新），0 表示不需要（用户自定义不会被覆盖）。
/// 未启用 SQLite 或无法读取内置 DB 时返回 0。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`builtin_path` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_check_x1dict_sync_needed(
    ptr: *const X1Engine,
    builtin_path: *const c_char,
) -> i32 {
    if ptr.is_null() || builtin_path.is_null() {
        return 0;
    }
    let path = match c_str_to_str(builtin_path) {
        Some(s) => s,
        None => return 0,
    };
    let engine = &(*ptr).inner;
    guard("check_x1dict_sync_needed", 0, || {
        if engine.check_x1dict_sync_needed(path) {
            1
        } else {
            0
        }
    })
}

/// 导出用户词库为 TSV 格式（text\tpinyin\tfreq）
///
/// 返回的字符串必须由 `x1_string_free` 释放。
/// 若未启用 SQLite 或没有用户词，返回空字符串。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_export_user_dict(ptr: *const X1Engine) -> *mut c_char {
    if ptr.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let tsv = guard("export_user_dict", String::new(), || {
        engine.export_user_dict_tsv()
    });
    match CString::new(tsv) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

/// 从 TSV 导入用户词库（text\tpinyin\tfreq）
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`tsv` 必须是有效 UTF-8 C 字符串
/// # Arguments
/// * `merge` — 1 表示合并（频率累加），0 表示覆盖
/// # Returns
/// 成功导入的条目数，-1 表示失败
#[no_mangle]
pub unsafe extern "C" fn x1_engine_import_user_dict(
    ptr: *mut X1Engine,
    tsv: *const c_char,
    merge: i32,
) -> i32 {
    if ptr.is_null() || tsv.is_null() {
        return -1;
    }
    let tsv_str = match c_str_to_str(tsv) {
        Some(s) => s,
        None => return -1,
    };
    let engine = &mut (*ptr).inner;
    guard("import_user_dict", -1, || {
        match engine.import_user_dict_tsv(tsv_str, merge != 0) {
            Ok(n) => n as i32,
            Err(_) => -1,
        }
    })
}

/// 获取全部用户词，返回 JSON 字符串
///
/// 返回格式：[{"text":"你好","pinyin":"ni hao","freq":5}, ...]
/// 返回的字符串必须由 `x1_string_free` 释放
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_get_user_words(ptr: *mut X1Engine) -> *mut c_char {
    if ptr.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let json = guard("get_user_words", String::from("[]"), || {
        let words = engine.get_user_words();
        let mut json = String::from("[");
        for (i, (text, pinyin, freq)) in words.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            json.push('{');
            json.push_str("\"text\":\"");
            super::push_json_escaped(&mut json, text);
            json.push_str("\",\"pinyin\":\"");
            super::push_json_escaped(&mut json, pinyin);
            json.push_str("\",\"freq\":");
            json.push_str(&alloc::format!("{}", freq));
            json.push('}');
        }
        json.push(']');
        json
    });

    match CString::new(json) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

/// 删除一条用户词
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`text` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_delete_user_word(ptr: *mut X1Engine, text: *const c_char) {
    if ptr.is_null() || text.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    if let Some(t) = c_str_to_str(text) {
        guard("delete_user_word", (), || engine.delete_user_word(t));
    }
}

/// 更新一条用户词的拼音和频率
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`text`、`pinyin` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_update_user_word(
    ptr: *mut X1Engine,
    text: *const c_char,
    pinyin: *const c_char,
    freq: u32,
) {
    if ptr.is_null() || text.is_null() || pinyin.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    if let (Some(t), Some(p)) = (c_str_to_str(text), c_str_to_str(pinyin)) {
        guard("update_user_word", (), || {
            engine.update_user_word(t, p, freq)
        });
    }
}

// -----------------------------------------------------------------
// 系统词库管理（供词库管理工具调用）
// -----------------------------------------------------------------

/// 查询系统词库列表，返回 JSON 数组
///
/// 返回格式：[{"text":"中国","pinyin":"zhong guo","freq":5000}, ...]
/// 返回的字符串必须由 `x1_string_free` 释放。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`filter` 可为 null
#[no_mangle]
pub unsafe extern "C" fn x1_engine_get_x1dict_words(
    ptr: *const X1Engine,
    filter: *const c_char,
    limit: usize,
    offset: usize,
) -> *mut c_char {
    if ptr.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let filter_opt = c_str_to_str(filter);
    let json = guard("get_x1dict_words", String::from("[]"), || {
        let words = engine.list_x1dict_words(filter_opt, limit, offset);
        let mut json = String::from("[");
        for (i, w) in words.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            json.push('{');
            json.push_str("\"text\":\"");
            super::push_json_escaped(&mut json, &w.simplified);
            json.push_str("\",\"pinyin\":\"");
            super::push_json_escaped(&mut json, &w.pinyin);
            json.push_str("\",\"freq\":");
            json.push_str(&alloc::format!("{}", w.freq));
            json.push('}');
        }
        json.push(']');
        json
    });

    match CString::new(json) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

/// 查询系统词库条目数（带过滤）
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`filter` 可为 null
#[no_mangle]
pub unsafe extern "C" fn x1_engine_x1dict_word_count(
    ptr: *const X1Engine,
    filter: *const c_char,
) -> i32 {
    if ptr.is_null() {
        return -1;
    }
    let engine = &(*ptr).inner;
    engine.x1dict_count_filtered(c_str_to_str(filter)) as i32
}

/// 添加/替换一条系统词
///
/// # Safety
/// 所有字符串参数必须是有效 UTF-8 C 字符串或 null
#[no_mangle]
pub unsafe extern "C" fn x1_engine_add_x1dict_word(
    ptr: *mut X1Engine,
    text: *const c_char,
    pinyin: *const c_char,
    freq: i64,
) -> i32 {
    if ptr.is_null() || text.is_null() || pinyin.is_null() {
        return -1;
    }
    let simplified = c_str_to_str(text).unwrap_or("").to_string();
    let pinyin_str = c_str_to_str(pinyin).unwrap_or("").to_string();
    let is_primary = simplified
        .chars()
        .next()
        .zip(pinyin_str.split_whitespace().next())
        .map(|(c, p)| crate::char_freq::is_primary_reading(c, p))
        .unwrap_or(true);
    let word = X1DictWord {
        simplified,
        pinyin: pinyin_str,
        freq,
        is_primary,
    };
    if word.simplified.is_empty() || word.pinyin.is_empty() {
        return -1;
    }
    let engine = &mut (*ptr).inner;
    guard("add_x1dict_word", -1, || {
        if engine.add_system_word(&word) {
            0
        } else {
            -1
        }
    })
}

/// 更新一条系统词（按旧主键定位）
///
/// # Safety
/// 所有字符串参数必须是有效 UTF-8 C 字符串或 null
#[no_mangle]
pub unsafe extern "C" fn x1_engine_update_x1dict_word(
    ptr: *mut X1Engine,
    old_text: *const c_char,
    old_pinyin: *const c_char,
    text: *const c_char,
    pinyin: *const c_char,
    freq: i64,
) -> i32 {
    if ptr.is_null()
        || old_text.is_null()
        || old_pinyin.is_null()
        || text.is_null()
        || pinyin.is_null()
    {
        return -1;
    }
    let old_t = match c_str_to_str(old_text) {
        Some(s) => s,
        None => return -1,
    };
    let old_p = match c_str_to_str(old_pinyin) {
        Some(s) => s,
        None => return -1,
    };
    let simplified = c_str_to_str(text).unwrap_or("").to_string();
    let pinyin_str = c_str_to_str(pinyin).unwrap_or("").to_string();
    let is_primary = simplified
        .chars()
        .next()
        .zip(pinyin_str.split_whitespace().next())
        .map(|(c, p)| crate::char_freq::is_primary_reading(c, p))
        .unwrap_or(true);
    let word = X1DictWord {
        simplified,
        pinyin: pinyin_str,
        freq,
        is_primary,
    };
    if word.simplified.is_empty() || word.pinyin.is_empty() {
        return -1;
    }
    let engine = &mut (*ptr).inner;
    guard("update_x1dict_word", -1, || {
        if engine.update_system_word(old_t, old_p, &word) {
            0
        } else {
            -1
        }
    })
}

/// 删除一条系统词
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`text`、`pinyin` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_delete_x1dict_word(
    ptr: *mut X1Engine,
    text: *const c_char,
    pinyin: *const c_char,
) -> i32 {
    if ptr.is_null() || text.is_null() || pinyin.is_null() {
        return -1;
    }
    let engine = &mut (*ptr).inner;
    if let (Some(t), Some(p)) = (c_str_to_str(text), c_str_to_str(pinyin)) {
        guard("delete_x1dict_word", -1, || {
            if engine.delete_system_word(t, p) {
                0
            } else {
                -1
            }
        })
    } else {
        -1
    }
}

// ── 英文词库 CRUD FFI（词库管理器接线；Windows 走引擎服务侧 IPC，
// mac 无服务进程直接 FFI。JSON 形状与 Windows IPC 同构：
// [{"word","pos","cn","freq"}]，宿主端用标准 JSON 解析——cn 释义可
// 含逗号引号，手写切串不可靠）──

/// 英文词分页列表（filter = word 前缀或 cn 包含；freq 降序）。
/// 返回 JSON 数组字符串（`x1_string_free` 释放）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`filter` 可为 null（不过滤）
#[no_mangle]
pub unsafe extern "C" fn x1_engine_english_word_list(
    ptr: *const X1Engine,
    filter: *const c_char,
    limit: usize,
    offset: usize,
) -> *mut c_char {
    if ptr.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let filter_opt = c_str_to_str(filter);
    let json = guard("english_word_list", String::from("[]"), || {
        let words = engine.english_list(filter_opt, limit, offset);
        let mut json = String::from("[");
        for (i, w) in words.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            json.push('{');
            json.push_str("\"word\":\"");
            super::push_json_escaped(&mut json, &w.word);
            json.push_str("\",\"pos\":\"");
            super::push_json_escaped(&mut json, &w.pos);
            json.push_str("\",\"cn\":\"");
            super::push_json_escaped(&mut json, &w.cn);
            json.push_str("\",\"freq\":");
            json.push_str(&alloc::format!("{}", w.freq));
            json.push('}');
        }
        json.push(']');
        json
    });
    match CString::new(json) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

/// 英文词总数（带过滤；无 DB 返回 -1）
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`filter` 可为 null
#[no_mangle]
pub unsafe extern "C" fn x1_engine_english_word_count(
    ptr: *const X1Engine,
    filter: *const c_char,
) -> i64 {
    if ptr.is_null() {
        return -1;
    }
    let engine = &(*ptr).inner;
    engine.english_count(c_str_to_str(filter))
}

/// 新增/覆盖英文词（返回 0 = 成功，-1 = 失败）
///
/// # Safety
/// `ptr` 与字符串参数必须是有效指针/UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_english_word_upsert(
    ptr: *mut X1Engine,
    word: *const c_char,
    pos: *const c_char,
    cn: *const c_char,
    freq: i64,
) -> i32 {
    if ptr.is_null() || word.is_null() {
        return -1;
    }
    let engine = &mut (*ptr).inner;
    if let (Some(w), Some(p), Some(c)) = (c_str_to_str(word), c_str_to_str(pos), c_str_to_str(cn)) {
        let entry = crate::storage::EnglishWord {
            word: w.to_string(),
            pos: p.to_string(),
            cn: c.to_string(),
            freq,
        };
        guard("english_word_upsert", -1, || {
            if engine.english_add(&entry) {
                0
            } else {
                -1
            }
        })
    } else {
        -1
    }
}

/// 更新英文词（old_word 主键可改；返回 0 = 成功，-1 = 失败）
///
/// # Safety
/// `ptr` 与字符串参数必须是有效指针/UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_english_word_update(
    ptr: *mut X1Engine,
    old_word: *const c_char,
    word: *const c_char,
    pos: *const c_char,
    cn: *const c_char,
    freq: i64,
) -> i32 {
    if ptr.is_null() || old_word.is_null() || word.is_null() {
        return -1;
    }
    let engine = &mut (*ptr).inner;
    if let (Some(ow), Some(w), Some(p), Some(c)) = (
        c_str_to_str(old_word),
        c_str_to_str(word),
        c_str_to_str(pos),
        c_str_to_str(cn),
    ) {
        let entry = crate::storage::EnglishWord {
            word: w.to_string(),
            pos: p.to_string(),
            cn: c.to_string(),
            freq,
        };
        guard("english_word_update", -1, || {
            if engine.english_update(ow, &entry) {
                0
            } else {
                -1
            }
        })
    } else {
        -1
    }
}

/// 删除英文词（幂等；返回 0 = 成功，-1 = 失败）
///
/// # Safety
/// `ptr`、`word` 必须是有效指针/UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_english_word_delete(
    ptr: *mut X1Engine,
    word: *const c_char,
) -> i32 {
    if ptr.is_null() || word.is_null() {
        return -1;
    }
    let engine = &mut (*ptr).inner;
    if let Some(w) = c_str_to_str(word) {
        guard("english_word_delete", -1, || {
            if engine.english_delete(w) {
                0
            } else {
                -1
            }
        })
    } else {
        -1
    }
}

/// 候选翻译（候选翻译副栏）：英→中 / 中→英。
///
/// 返回 JSON（`x1_string_free` 释放）：
/// `{"direction":"en2zh|zh2en|","entries":[{"word","pos","cn","freq"}]}`
/// 无翻译时 entries 为空。纯展示，零学习、零网络。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`text` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_translate_candidate(
    ptr: *const X1Engine,
    text: *const c_char,
    limit: usize,
) -> *mut c_char {
    if ptr.is_null() || text.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let Some(t) = c_str_to_str(text) else {
        return core::ptr::null_mut();
    };
    let json = guard("translate_candidate", String::from("{}"), || {
        let result = engine.translate_candidate(t, limit);
        let mut out = String::from("{\"direction\":\"");
        out.push_str(result.direction);
        out.push_str("\",\"entries\":[");
        for (i, e) in result.entries.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"word\":\"");
            super::push_json_escaped(&mut out, &e.word);
            out.push_str("\",\"pos\":\"");
            super::push_json_escaped(&mut out, &e.pos);
            out.push_str("\",\"cn\":\"");
            super::push_json_escaped(&mut out, &e.cn);
            out.push_str("\",\"freq\":");
            out.push_str(&alloc::format!("{}", e.freq));
            out.push('}');
        }
        out.push_str("]}");
        out
    });
    match CString::new(json) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

// 本 mod 之后仍有导出函数（文件按功能分节组织）
#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod translation_ffi_tests {
    use super::*;

    /// FFI JSON 契约守护：中→英 / 英→中 两个方向 + 字段名。
    /// 用种子副本，不触碰仓库种子。
    #[test]
    fn translate_candidate_json_contract() {
        let seed = crate::engine::tests_query::seed_copy("translate_ffi");
        let db = CString::new(seed.clone()).unwrap();
        let ptr = crate::ffi::x1_engine_new_with_db(db.as_ptr());
        assert!(!ptr.is_null(), "seed copy 应能打开");
        unsafe {
            let text = CString::new("人工智能").unwrap();
            let out = x1_engine_translate_candidate(ptr, text.as_ptr(), 3);
            assert!(!out.is_null());
            let json = String::from(CStr::from_ptr(out).to_str().unwrap());
            crate::ffi::x1_string_free(out);
            assert!(json.contains("\"direction\":\"zh2en\""), "json={json}");
            assert!(json.contains("\"word\":\"AI\""), "json={json}");

            let text = CString::new("Microsoft").unwrap();
            let out = x1_engine_translate_candidate(ptr, text.as_ptr(), 3);
            assert!(!out.is_null());
            let json = String::from(CStr::from_ptr(out).to_str().unwrap());
            crate::ffi::x1_string_free(out);
            assert!(json.contains("\"direction\":\"en2zh\""), "json={json}");
            assert!(json.contains("微软公司"), "json={json}");

            crate::ffi::x1_engine_free(ptr);
        }
        crate::engine::tests_query::seed_copy_cleanup(&seed);
    }
}

/// 导出系统词库为 TSV 格式
///
/// 格式：simplified\tpinyin\tfreq\tis_primary
/// 返回的字符串必须由 `x1_string_free` 释放。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_export_x1dict_dict(ptr: *const X1Engine) -> *mut c_char {
    if ptr.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let tsv = guard("export_x1dict_dict", String::new(), || {
        engine.export_x1dict_tsv()
    });
    match CString::new(tsv) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

/// 从 TSV 导入系统词库
///
/// `replace = 1` 时先清空再导入；`replace = 0` 时合并。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`tsv` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_import_x1dict_dict(
    ptr: *mut X1Engine,
    tsv: *const c_char,
    replace: i32,
) -> i32 {
    if ptr.is_null() || tsv.is_null() {
        return -1;
    }
    let engine = &mut (*ptr).inner;
    let tsv_str = match c_str_to_str(tsv) {
        Some(s) => s,
        None => return -1,
    };
    guard("import_x1dict_dict", -1, || {
        match engine.import_x1dict_tsv(tsv_str, replace != 0) {
            Ok(n) => n as i32,
            Err(_) => -1,
        }
    })
}

/// 重新加载系统词库（从 storage 重建内存 Trie）
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_reload_x1dict_dict(ptr: *mut X1Engine) -> i32 {
    if ptr.is_null() {
        return -1;
    }
    let engine = &mut (*ptr).inner;
    guard("reload_x1dict", -1, || match engine.reload_x1dict() {
        Ok(()) => 0,
        Err(_) => -1,
    })
}

/// 清理用户词库中的单字
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_cleanup_single_char_user_words(ptr: *mut X1Engine) -> i32 {
    if ptr.is_null() {
        return -1;
    }
    let engine = &mut (*ptr).inner;
    guard("cleanup_single_char_user_words", -1, || {
        engine.cleanup_single_char_user_words() as i32
    })
}

/// 清空全部用户词（重置学习数据）
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_clear_user_words(ptr: *mut X1Engine) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("clear_user_words", (), || engine.clear_user_words());
}

/// 导入用户词。
///
/// `json` 格式：[{"text":"你好","pinyin":"ni hao","freq":5}, ...]
/// `merge` 为 true 时与现有词库合并（频率累加），为 false 时先清空再导入。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`json` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_import_user_words(
    ptr: *mut X1Engine,
    json: *const c_char,
    merge: bool,
) {
    if ptr.is_null() || json.is_null() {
        return;
    }
    let json_str = match c_str_to_str(json) {
        Some(s) => s,
        None => return,
    };
    let entries = match parse_user_word_json(json_str) {
        Ok(e) => e,
        Err(_) => return,
    };

    let engine = &mut (*ptr).inner;
    guard("import_user_words", (), || {
        engine.import_user_words(&entries, merge);
    });
}

/// 加载用户词库文本
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`text` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_load_user_dict(ptr: *mut X1Engine, text: *const c_char) {
    if ptr.is_null() || text.is_null() {
        return;
    }

    let engine = &mut (*ptr).inner;
    if let Some(s) = c_str_to_str(text) {
        guard("load_user_dict", (), || engine.load_user_dict(s));
    }
}

/// 导出用户词库文本
///
/// 返回的字符串必须由 `x1_string_free` 释放
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_save_user_dict(ptr: *mut X1Engine) -> *mut c_char {
    if ptr.is_null() {
        return core::ptr::null_mut();
    }

    let engine = &(*ptr).inner;
    let text = guard("save_user_dict", String::new(), || engine.save_user_dict());

    match CString::new(text) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

/// 查询某 (prev, text) bigram 的来源信息（调试用）。
///
/// 双表方案下区分种子 vs 用户数据来源，供宿主端调试工具/词库管理器诊断。
/// 返回 JSON 字符串：`{"user_count": N, "seed_count": M, "used": "user"/"seed"/"none"}`
/// - user_count：用户 commit 积累（word_bigram_counts 表）
/// - seed_count：种子语料预填（seed_bigram_counts 表）
/// - used：word_bigram_count 实际用的是哪个（"user" 优先 / "seed" 兜底 / "none" 都无）
///
/// 调用方负责 free 返回的字符串（x1_string_free）。
///
/// # Safety
/// `ptr` 必须是有效的 X1Engine 指针；`prev`/`text` 必须是有效 UTF-8 C 字符串。
#[no_mangle]
pub unsafe extern "C" fn x1_engine_bigram_source_info(
    ptr: *mut X1Engine,
    prev: *const c_char,
    text: *const c_char,
) -> *mut c_char {
    if ptr.is_null() || prev.is_null() || text.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let prev_str = c_str_to_str(prev).unwrap_or("");
    let text_str = c_str_to_str(text).unwrap_or("");
    let json = guard(
        "bigram_source_info",
        String::from(r#"{"user_count":0,"seed_count":0,"used":"none"}"#),
        || {
            let (user, seed, used) = engine.bigram_source_info(prev_str, text_str);
            format!(
                r#"{{"user_count":{},"seed_count":{},"used":"{}"}}"#,
                user, seed, used
            )
        },
    );
    match CString::new(json) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}
