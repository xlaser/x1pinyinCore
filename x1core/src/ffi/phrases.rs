//! 自定义短语管理 FFI。
//!
//! 词库管理器「自定义短语」来源页的 CRUD + 纯文本导入导出。引擎侧短语注入
//! （process 末位 inject_phrase_candidates）与学习隔离（commit_internal 守卫）
//! mac/win 共享已就绪；本模块只补 mac 进程内引擎的管理入口——写成功即
//! refresh_phrases_memory 即时生效（无需 reload_user_state 全量重载，
//! 不丢 pending 学习态）。Windows 走 x1app DictMgr 直连 engine 同名方法 +
//! notify_reload，不经本 FFI。
//!
//! 返回 JSON 形状：`[{"id":1,"trigger":"noip","text":"信息学省级联赛"}, ...]`
//! （id 升序 = 添加顺序；字符串经 push_json_escaped 转义，短语可含引号/逗号）。

use alloc::ffi::CString;
use alloc::string::String;
use core::ffi::c_char;

use super::{c_str_to_str, guard, X1Engine};

/// 全部短语（id/触发码/文本，id 升序）。返回 JSON 数组字符串，
/// 调用方负责 `x1_string_free` 释放。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_list_phrases(ptr: *const X1Engine) -> *mut c_char {
    if ptr.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let json = guard("list_phrases", String::from("[]"), || {
        let rows = engine.list_phrases();
        let mut json = String::from("[");
        for (i, (id, trigger, text)) in rows.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            json.push('{');
            json.push_str(&alloc::format!("\"id\":{},\"trigger\":\"", id));
            super::push_json_escaped(&mut json, trigger);
            json.push_str("\",\"text\":\"");
            super::push_json_escaped(&mut json, text);
            json.push_str("\"}");
        }
        json.push(']');
        json
    });
    match CString::new(json) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

/// (归一输入, 文本) 是否为已定义自定义短语（关联通道配套）：
/// 调用方 isShortcut 提交门控用——短语候选走 engine commit 进关联通道
/// （上下文 + 双向 bigram，引擎守卫路由），符号/日期类快捷候选保持零学习
/// （否则日期串会污染 freq）。返回 1 = 是 / 0 = 否 / -1 = 参数失败。
///
/// # Safety
/// `ptr` 与字符串参数必须是有效指针/UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_is_user_phrase(
    ptr: *const X1Engine,
    input: *const c_char,
    text: *const c_char,
) -> i32 {
    if ptr.is_null() || input.is_null() || text.is_null() {
        return -1;
    }
    let engine = &(*ptr).inner;
    let input = match c_str_to_str(input) {
        Some(s) => s,
        None => return -1,
    };
    let text = match c_str_to_str(text) {
        Some(s) => s,
        None => return -1,
    };
    if engine.is_user_phrase(input, text) {
        1
    } else {
        0
    }
}

/// 短语总数（无 DB 返回 -1）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_phrase_count(ptr: *const X1Engine) -> i64 {
    if ptr.is_null() {
        return -1;
    }
    let engine = &(*ptr).inner;
    if engine.has_storage() {
        engine.phrase_count() as i64
    } else {
        -1
    }
}

/// 新增一条短语（触发码归一存储，写成功即时进内存候选注入）。
/// 返回 0 = 成功，-1 = 失败。
///
/// # Safety
/// `ptr` 与字符串参数必须是有效指针/UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_add_phrase(
    ptr: *mut X1Engine,
    trigger: *const c_char,
    text: *const c_char,
) -> i32 {
    if ptr.is_null() || trigger.is_null() || text.is_null() {
        return -1;
    }
    let engine = &mut (*ptr).inner;
    let trigger = match c_str_to_str(trigger) {
        Some(s) => s,
        None => return -1,
    };
    let text = match c_str_to_str(text) {
        Some(s) => s,
        None => return -1,
    };
    if engine.add_phrase(trigger, text) {
        0
    } else {
        -1
    }
}

/// 按 id 更新短语（触发码归一，写成功即时重建内存短语表）。
/// 返回 0 = 成功，-1 = 失败。
///
/// # Safety
/// `ptr` 与字符串参数必须是有效指针/UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_update_phrase(
    ptr: *mut X1Engine,
    id: i64,
    trigger: *const c_char,
    text: *const c_char,
) -> i32 {
    if ptr.is_null() || trigger.is_null() || text.is_null() {
        return -1;
    }
    let engine = &mut (*ptr).inner;
    let trigger = match c_str_to_str(trigger) {
        Some(s) => s,
        None => return -1,
    };
    let text = match c_str_to_str(text) {
        Some(s) => s,
        None => return -1,
    };
    if engine.update_phrase(id, trigger, text) {
        0
    } else {
        -1
    }
}

/// 批量删除短语（按 id）。`ids` 为逗号分隔的十进制 id 串（如 "1,3,7"，
/// 管理界面勾选行反查主键后拼接）；空串/全非法 = 不动作返回 0。
/// 返回 0 = 成功，-1 = 失败。
///
/// # Safety
/// `ptr` 与 `ids` 必须是有效指针/UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_delete_phrases(ptr: *mut X1Engine, ids: *const c_char) -> i32 {
    if ptr.is_null() || ids.is_null() {
        return -1;
    }
    let engine = &mut (*ptr).inner;
    let ids_str = match c_str_to_str(ids) {
        Some(s) => s,
        None => return -1,
    };
    let parsed: alloc::vec::Vec<i64> = ids_str
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect();
    if parsed.is_empty() {
        return 0;
    }
    if engine.delete_phrases(&parsed) {
        0
    } else {
        -1
    }
}

/// 导入短语（纯文本，短语在前触发码在后，空格/逗号分隔；`#` 注释/空行忽略）。
/// `replace` 非 0 = 先清空再导入，0 = 追加（默认语义）。返回导入条数（>=0），
/// -1 = 参数失败。写成功即时重建内存短语表。
///
/// # Safety
/// `ptr` 与 `content` 必须是有效指针/UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_import_phrases(
    ptr: *mut X1Engine,
    content: *const c_char,
    replace: i32,
) -> i32 {
    if ptr.is_null() || content.is_null() {
        return -1;
    }
    let engine = &mut (*ptr).inner;
    let content = match c_str_to_str(content) {
        Some(s) => s,
        None => return -1,
    };
    engine.import_phrases(content, replace != 0) as i32
}

/// 导出短语（每行 `短语,触发码`，UTF-8）。返回字符串调用方负责
/// `x1_string_free` 释放；无 DB/空表返回空串（非 null）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_export_phrases(ptr: *const X1Engine) -> *mut c_char {
    if ptr.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let text = guard("export_phrases", String::new(), || engine.export_phrases());
    match CString::new(text) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实词库引擎（种子 DB 复制到 test-tmp，红线 #45：禁直开种子）。
    fn make_engine(tag: &str) -> X1Engine {
        let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
        let tmp = crate::test_tmp::dir().join(format!("x1pinyin_ffi_phrases_{}.db", tag));
        let _ = std::fs::remove_file(&tmp);
        std::fs::copy(seed, &tmp).expect("copy seed db");
        X1Engine {
            inner: crate::engine::Engine::new_with_db(tmp.to_str().unwrap()).expect("open"),
        }
    }

    unsafe fn take_string(ptr: *mut c_char) -> String {
        assert!(!ptr.is_null());
        let s = core::ffi::CStr::from_ptr(ptr)
            .to_string_lossy()
            .into_owned();
        crate::ffi::x1_string_free(ptr);
        s
    }

    #[test]
    fn phrase_ffi_null_guards() {
        unsafe {
            assert!(x1_engine_list_phrases(core::ptr::null()).is_null());
            assert_eq!(x1_engine_phrase_count(core::ptr::null()), -1);
            assert_eq!(
                x1_engine_add_phrase(core::ptr::null_mut(), core::ptr::null(), core::ptr::null()),
                -1
            );
            assert_eq!(
                x1_engine_update_phrase(
                    core::ptr::null_mut(),
                    1,
                    core::ptr::null(),
                    core::ptr::null()
                ),
                -1
            );
            assert_eq!(
                x1_engine_delete_phrases(core::ptr::null_mut(), core::ptr::null()),
                -1
            );
            assert_eq!(
                x1_engine_import_phrases(core::ptr::null_mut(), core::ptr::null(), 0),
                -1
            );
            assert!(x1_engine_export_phrases(core::ptr::null()).is_null());
        }
    }

    /// is_user_phrase 门控 FFI（mac isShortcut 提交分支判定用）。
    #[test]
    fn phrase_ffi_is_user_phrase_gate() {
        let mut engine = make_engine("gate");
        let ptr = &mut engine as *mut X1Engine;
        unsafe {
            assert_eq!(
                x1_engine_is_user_phrase(core::ptr::null(), core::ptr::null(), core::ptr::null()),
                -1
            );
            let t = CString::new("noip").unwrap();
            let x = CString::new("信息学省级联赛").unwrap();
            assert_eq!(x1_engine_is_user_phrase(ptr, t.as_ptr(), x.as_ptr()), 0);
            assert_eq!(x1_engine_add_phrase(ptr, t.as_ptr(), x.as_ptr()), 0);
            assert_eq!(x1_engine_is_user_phrase(ptr, t.as_ptr(), x.as_ptr()), 1);
            // 归一输入判定（大写/隔音符同口径）
            let t2 = CString::new("NO'IP").unwrap();
            assert_eq!(x1_engine_is_user_phrase(ptr, t2.as_ptr(), x.as_ptr()), 1);
            // 符号/日期文本不误判
            let d = CString::new("2026-11-16").unwrap();
            assert_eq!(x1_engine_is_user_phrase(ptr, t.as_ptr(), d.as_ptr()), 0);
        }
    }

    /// mac 核心语义：FFI 增删改**即时**进内存候选注入（无需 reload_user_state）。
    #[test]
    fn phrase_ffi_crud_live_injection() {
        let mut engine = make_engine("crud");
        let ptr = &mut engine as *mut X1Engine;
        unsafe {
            let trigger = CString::new("noip").unwrap();
            let text = CString::new("信息学省级联赛").unwrap();
            assert_eq!(
                x1_engine_add_phrase(ptr, trigger.as_ptr(), text.as_ptr()),
                0
            );
            // 不 reload，直接 process 即命中（内存表已即时维护）
            let cands = engine.inner.process("noip");
            assert_eq!(cands[0].text.as_ref(), "信息学省级联赛");
            assert!(cands[0].is_shortcut);

            // list 形状 + id
            let json = take_string(x1_engine_list_phrases(ptr));
            assert!(json.contains("\"trigger\":\"noip\""), "{json}");
            assert!(json.contains("\"text\":\"信息学省级联赛\""), "{json}");
            let id: i64 = json
                .split("\"id\":")
                .nth(1)
                .unwrap()
                .split(',')
                .next()
                .unwrap()
                .parse()
                .unwrap();

            // update 换触发码 → 旧码不再命中、新码即时命中
            let t2 = CString::new("noi").unwrap();
            let txt2 = CString::new("信息学联赛").unwrap();
            assert_eq!(
                x1_engine_update_phrase(ptr, id, t2.as_ptr(), txt2.as_ptr()),
                0
            );
            assert!(!engine
                .inner
                .process("noip")
                .iter()
                .any(|c| c.is_shortcut && c.text.as_ref() == "信息学省级联赛"));
            assert_eq!(engine.inner.process("noi")[0].text.as_ref(), "信息学联赛");

            // delete（逗号分隔 id 串）→ 注入消失
            let ids = CString::new(format!("{}", id)).unwrap();
            assert_eq!(x1_engine_delete_phrases(ptr, ids.as_ptr()), 0);
            assert!(!engine
                .inner
                .process("noi")
                .iter()
                .any(|c| c.text.as_ref() == "信息学联赛"));
            assert_eq!(x1_engine_phrase_count(ptr), 0);
        }
    }

    /// 导入导出 round-trip + 追加/覆盖语义 + 转义（短语含引号/逗号）。
    #[test]
    fn phrase_ffi_import_export_roundtrip() {
        let mut engine = make_engine("import");
        let ptr = &mut engine as *mut X1Engine;
        unsafe {
            let content =
                CString::new("# 注释\n信息学省级联赛 noip\nhello world,hw\n\"引号\",qh").unwrap();
            assert_eq!(x1_engine_import_phrases(ptr, content.as_ptr(), 0), 3);
            assert_eq!(engine.inner.process("hw")[0].text.as_ref(), "hello world");
            // 追加导入不清存量
            let more = CString::new("序号 xuhao").unwrap();
            assert_eq!(x1_engine_import_phrases(ptr, more.as_ptr(), 0), 1);
            assert_eq!(x1_engine_phrase_count(ptr), 4);
            // 导出 → 再覆盖导入 round-trip 条数不变
            let exported = take_string(x1_engine_export_phrases(ptr));
            let c = CString::new(exported).unwrap();
            assert_eq!(x1_engine_import_phrases(ptr, c.as_ptr(), 1), 4);
            assert_eq!(x1_engine_phrase_count(ptr), 4);
            // 引号短语经 JSON 转义（\" 形态）——裸引号会撕碎 Swift 侧解析
            let json = take_string(x1_engine_list_phrases(ptr));
            assert!(json.contains("\\\"引号\\\""), "引号必须转义：{json}");
            assert!(
                !json.contains("\"text\":\"\"引号\"\""),
                "不得裸引号：{json}"
            );
        }
    }
}
