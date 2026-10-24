//! 查询类 FFI 接口：候选生成 / 拼音分析 / 特征抽取。
//!
//! 从 ffi.rs 拆出，函数体零改动。

use alloc::ffi::CString;
use core::ffi::c_char;

use super::{
    c_str_to_str, candidates_to_json, features_to_json, guard, log_panic, push_json_escaped,
    X1Engine,
};

/// 处理输入，返回候选 JSON 字符串
///
/// 返回格式：[{"text":"中文","freq":1000}, ...]
/// 返回的字符串必须由 `x1_string_free` 释放
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`input` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_process(
    ptr: *mut X1Engine,
    input: *const c_char,
) -> *mut c_char {
    if ptr.is_null() || input.is_null() {
        return core::ptr::null_mut();
    }

    let engine = &(*ptr).inner;
    let input_str = match c_str_to_str(input) {
        Some(s) => s,
        None => return core::ptr::null_mut(),
    };

    let candidates = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine.process(input_str)
    })) {
        Ok(c) => c,
        Err(e) => {
            log_panic("process", &*e);
            return core::ptr::null_mut();
        }
    };
    let json = candidates_to_json(&candidates);

    match CString::new(json) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

/// 带击键时序的查询（双击故障感知，宿主前端用）：
/// `ms_since_prev_key` = 本次按键与上一次的间隔毫秒（u32::MAX = 未知/首键）。
/// 间隔信息用于门控双写折叠纠错（详见 Engine::process_timed 与
/// DOUBLE_FAULT_MS）。行为：未知间隔与 x1_engine_process 一致。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`input` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_process_timed(
    ptr: *mut X1Engine,
    input: *const c_char,
    ms_since_prev_key: u32,
) -> *mut c_char {
    if ptr.is_null() || input.is_null() {
        return core::ptr::null_mut();
    }

    let engine = &(*ptr).inner;
    let input_str = match c_str_to_str(input) {
        Some(s) => s,
        None => return core::ptr::null_mut(),
    };

    let candidates = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if ms_since_prev_key == u32::MAX {
            engine.process(input_str)
        } else {
            engine.process_timed(input_str, ms_since_prev_key)
        }
    })) {
        Ok(c) => c,
        Err(e) => {
            // 带上输入串，便于定位「排序全序违规」等只对特定输入
            // 触发的 panic（仅函数名难以复现）。
            let ctx = alloc::format!("process_timed(input={:?})", input_str);
            log_panic(&ctx, &*e);
            return core::ptr::null_mut();
        }
    };
    let json = candidates_to_json(&candidates);

    match CString::new(json) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

// 注：x1_engine_collect_candidates + x1_engine_set_semantic_bonuses 已移除。
// NLEmbedding 神经重排序完全移除，两阶段 FFI 协议不再需要。

/// 英文模式前缀补全查询（智能单词识别·英文模式；Windows 侧走 IPC
/// `english_complete`，mac 无服务进程直接 FFI 查表）。
///
/// 返回格式：`[{"text":"morning","freq":1200}, ...]`——精确命中置顶，其余
/// freq 降序，最多 `limit` 条；`smartWordEn` 关时返回 `[]`。与引擎服务侧
/// EnglishComplete 响应的 json 字段同构（双端消费形状一致）。
/// 返回的字符串必须由 `x1_string_free` 释放。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`prefix` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_english_complete(
    ptr: *mut X1Engine,
    prefix: *const c_char,
    limit: usize,
) -> *mut c_char {
    if ptr.is_null() || prefix.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let prefix_str = match c_str_to_str(prefix) {
        Some(s) => s,
        None => return core::ptr::null_mut(),
    };
    let json = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let words = engine.english_complete(prefix_str, limit);
        let mut out = String::from("[");
        for (i, (w, f)) in words.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"text\":\"");
            out.push_str(w);
            out.push_str("\",\"freq\":");
            // u32 恒无转义/格式歧义，直接拼接（与服务端 json 组装同款）
            out.push_str(&f.to_string());
            out.push('}');
        }
        out.push(']');
        out
    })) {
        Ok(j) => j,
        Err(e) => {
            log_panic("english_complete", &*e);
            return core::ptr::null_mut();
        }
    };
    match CString::new(json) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

/// 判断输入是否为完整拼音
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`input` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_is_complete_pinyin(
    ptr: *const X1Engine,
    input: *const c_char,
) -> bool {
    if ptr.is_null() || input.is_null() {
        return false;
    }
    let engine = &(*ptr).inner;
    c_str_to_str(input)
        .map(|s| guard("is_complete_pinyin", false, || engine.is_complete_pinyin(s)))
        .unwrap_or(false)
}

/// 计算输入的音节数
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`input` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_syllable_count(
    ptr: *const X1Engine,
    input: *const c_char,
) -> usize {
    if ptr.is_null() || input.is_null() {
        return 0;
    }
    let engine = &(*ptr).inner;
    c_str_to_str(input)
        .map(|s| guard("syllable_count", 0, || engine.syllable_count(s)))
        .unwrap_or(0)
}

/// 返回输入前 n 个音节所占的字节长度
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`input` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_syllable_prefix_len(
    ptr: *const X1Engine,
    input: *const c_char,
    n: usize,
) -> usize {
    if ptr.is_null() || input.is_null() {
        return 0;
    }
    let engine = &(*ptr).inner;
    c_str_to_str(input)
        .map(|s| {
            guard("syllable_prefix_len", 0, || {
                engine.syllable_prefix_len(s, n)
            })
        })
        .unwrap_or(0)
}

/// 返回 buffer 用 `'` 分隔的音节切分字符串（用于预输入态显示，仿微信输入法）。
///
/// 例：`wenmin` → `wen'min`，`zgrm` → `z'g'r'm`，单音节 `xian` → `xian`（无分隔）。
/// 切分失败或单音节时返回原始 input（不加分隔符，避免干扰）。
/// 返回的字符串需用 `x1_string_free` 释放。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`input` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_segment_buffer(
    ptr: *const X1Engine,
    input: *const c_char,
) -> *mut c_char {
    if ptr.is_null() || input.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let s = c_str_to_str(input)?;
        let segs = engine.syllable_segmentation(s);
        // 0 或 1 个音节：返回原始 input（单音节不加分隔符）
        if segs.len() <= 1 {
            return Some(s.to_string());
        }
        Some(segs.join("'"))
    }));
    match result {
        Ok(Some(joined)) => match CString::new(joined) {
            Ok(cstr) => cstr.into_raw(),
            Err(_) => core::ptr::null_mut(),
        },
        _ => core::ptr::null_mut(),
    }
}

/// 抽取给定 input 下所有候选的打分特征明细（学习型排序器训练数据记录用）。
///
/// 宿主端在 commit 时调用：拿当时候选列表的特征 + 用户选择索引，写训练数据文件。
/// 重新跑一次 process 取候选（低频路径，不影响 per-keystroke 热路径）。
///
/// 返回 JSON（features_to_json 格式），调用方需用 x1_string_free 释放。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`input` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_extract_features(
    ptr: *mut X1Engine,
    input: *const c_char,
) -> *mut c_char {
    if ptr.is_null() || input.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let input_str = match c_str_to_str(input) {
        Some(s) => s,
        None => return core::ptr::null_mut(),
    };
    // catch_unwind 保护（与 x1_engine_process 一致）：extract_features 内部调 process
    // → sort_by → candidate_score → score_breakdown，panic 须被拦截以防进程崩溃。
    // 宿主端拿到 null 时退化为不记训练数据。
    let features = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine.extract_features(input_str)
    })) {
        Ok(f) => f,
        Err(e) => {
            log_panic("extract_features", &*e);
            return core::ptr::null_mut();
        }
    };
    let json = features_to_json(&features);
    match CString::new(json) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

/// 双拼全拼还原串（Windows 走 IPC Process 响应的
/// full_display 字段，mac 直接 FFI 查询）。非双拼/临时全拼/解码失败返回
/// 空字符串（非 null——空串 = 调用方回退原显示，null 走错误分支）。
/// 偏好门控在显示侧（同 Windows：引擎无条件计算，前端按偏好决定是否展示）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`input` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_shuangpin_full_display(
    ptr: *mut X1Engine,
    input: *const c_char,
) -> *mut c_char {
    if ptr.is_null() || input.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let input_str = match c_str_to_str(input) {
        Some(s) => s,
        None => return core::ptr::null_mut(),
    };
    let out = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine.shuangpin_full_display(input_str).unwrap_or_default()
    })) {
        Ok(s) => s,
        Err(e) => {
            log_panic("shuangpin_full_display", &*e);
            return core::ptr::null_mut();
        }
    };
    match CString::new(out) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

/// 整句路径收集开关（宿主端 AI 整句重排层接线；与 Windows
/// 引擎服务侧的 `want_paths` 同语义——见 engine.rs 消费侧）：
/// 开启后下一次 `x1_engine_process(_timed)` 的整句 DP 顺带把 top-K（≤12）
/// 路径写入内部槽（引擎内双跑隔离：主候选与关闭时逐字节一致），
/// 随后用 `x1_engine_take_sentence_paths` 取走。开关关时零开销。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_collect_sentence_paths(ptr: *mut X1Engine, on: i32) {
    if ptr.is_null() {
        return;
    }
    (*ptr).inner.set_collect_sentence_paths(on != 0);
}

/// 取走并清空最近一次收集的整句路径（宿主端 AI 重排层）：
/// 返回 JSON `[{"text":"…","words":["…",…],"score":N},…]`（DP 分数降序）；
/// 未开启收集/本键无整句路由 = `[]`。返回的字符串必须由 `x1_string_free` 释放。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_take_sentence_paths(ptr: *mut X1Engine) -> *mut c_char {
    if ptr.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let out = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let paths = engine.take_sentence_paths();
        let mut json = String::from("[");
        for (i, p) in paths.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            json.push_str("{\"text\":\"");
            push_json_escaped(&mut json, &p.text);
            json.push_str("\",\"words\":[");
            for (j, w) in p.words.iter().enumerate() {
                if j > 0 {
                    json.push(',');
                }
                json.push('"');
                push_json_escaped(&mut json, w);
                json.push('"');
            }
            json.push_str("],\"score\":");
            json.push_str(&alloc::format!("{}", p.score));
            json.push('}');
        }
        json.push(']');
        json
    })) {
        Ok(s) => s,
        Err(e) => {
            log_panic("take_sentence_paths", &*e);
            return core::ptr::null_mut();
        }
    };
    match CString::new(out) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

#[cfg(test)]
mod sentence_paths_ffi_tests {
    use super::*;

    /// FFI 层接线健全性：① 空指针守卫（set 不死、take 返 null）；② 未开启
    /// 收集时 take 返回空数组 JSON（非 null）；③ 开启 + process 后整句输入
    /// 有路径可取，且取走后二次取 = 空（清空语义）。
    #[test]
    fn sentence_paths_ffi_guard_and_collect_flow() {
        unsafe {
            x1_engine_set_collect_sentence_paths(core::ptr::null_mut(), 1); // 不死即合格
            assert!(x1_engine_take_sentence_paths(core::ptr::null_mut()).is_null());
        }
        let mut engine = X1Engine {
            inner: crate::engine::Engine::new(),
        };
        unsafe {
            // 未收集：空数组
            let p = x1_engine_take_sentence_paths(&mut engine as *mut X1Engine);
            assert!(!p.is_null());
            let s = core::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
            crate::ffi::x1_string_free(p);
            assert_eq!(s, "[]");

            // 开启收集 → 整句输入 → 应取到 ≥1 条含 text/words/score 的路径
            x1_engine_set_collect_sentence_paths(&mut engine as *mut X1Engine, 1);
            let input = CString::new("nihaoshijie").unwrap();
            let cands = x1_engine_process(&mut engine as *mut X1Engine, input.as_ptr());
            assert!(!cands.is_null());
            crate::ffi::x1_string_free(cands);
            let p = x1_engine_take_sentence_paths(&mut engine as *mut X1Engine);
            assert!(!p.is_null());
            let s = core::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
            crate::ffi::x1_string_free(p);
            assert!(s.contains("\"text\""), "应有路径: {s}");
            assert!(s.contains("\"score\""), "应有分数: {s}");

            // 清空语义：二次取 = 空
            let p2 = x1_engine_take_sentence_paths(&mut engine as *mut X1Engine);
            let s2 = core::ffi::CStr::from_ptr(p2).to_string_lossy().into_owned();
            crate::ffi::x1_string_free(p2);
            assert_eq!(s2, "[]");
            x1_engine_set_collect_sentence_paths(&mut engine as *mut X1Engine, 0);
        }
    }
}

/// 最近 `max` 个上屏词（旧→新，`\u{001F}` 分隔；空串 = 无上下文）。
/// 前端据此在提交前校验「光标前文档是否确实以这些词结尾」。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_context_tail(ptr: *const X1Engine, max: usize) -> *mut c_char {
    if ptr.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let out = guard("context_tail", String::new(), || {
        engine.context_tail(max).join("\u{1f}")
    });
    match CString::new(out) {
        Ok(c) => c.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

/// 清空上屏上下文 + 会话级 n-gram 增量上限（切换应用 / 上下文不连续）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_context_clear(ptr: *mut X1Engine) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("context_clear", (), || engine.context_clear());
}

#[cfg(test)]
mod english_complete_tests {
    use super::*;

    /// FFI 层接线健全性（行为面由 engine::tests_english 全量守护）：
    /// ① 空指针守卫返回 null；② 内置模式（无 DB）返回空数组 JSON 而非 null
    /// （宿主端按「空数组=无补全」解析，null 会让调用方走错误分支）。
    #[test]
    fn english_complete_ffi_guards_and_empty_shape() {
        unsafe {
            assert!(
                x1_engine_english_complete(core::ptr::null_mut(), core::ptr::null(), 9).is_null()
            );
        }
        let mut engine = X1Engine {
            inner: crate::engine::Engine::new(),
        };
        let prefix = CString::new("mornin").unwrap();
        let out =
            unsafe { x1_engine_english_complete(&mut engine as *mut X1Engine, prefix.as_ptr(), 9) };
        assert!(!out.is_null(), "内置模式应返回空数组 JSON 而非 null");
        let s = unsafe { core::ffi::CStr::from_ptr(out) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(s, "[]", "内置模式（无 DB）英文补全恒空：{s}");
        unsafe { crate::ffi::x1_string_free(out) };
    }
}
