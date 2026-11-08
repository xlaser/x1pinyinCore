//! 配置类 FFI 接口：模糊拼音 / 输入映射 / 智能纠错 / 自动固化阈值。

use core::ffi::c_char;

use alloc::ffi::CString;

use super::{c_str_to_str, guard, X1Engine};

/// 设置模糊拼音配置
///
/// `config` 为逗号分隔的开关名，例如：
/// - `"enabled,z_zh,c_ch,s_sh"` 开启总开关和平翘舌模糊
/// - `"n_l,f_h"` 只开启 n/l 和 f/h 模糊
/// - `"jianpin_merge_zh_ch_sh"` 只开启简拼 zh/ch/sh 合并
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`config` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_fuzzy_config(
    ptr: *mut X1Engine,
    config: *const c_char,
) -> i32 {
    if ptr.is_null() || config.is_null() {
        return -1;
    }
    let csv = match c_str_to_str(config) {
        Some(s) => s,
        None => return -1,
    };
    let engine = &mut (*ptr).inner;
    guard("set_fuzzy_config", 0, || {
        engine.set_fuzzy_config(crate::fuzzy::FuzzyConfig::from_csv(csv));
        0
    })
}

/// 设置是否启用输入习惯映射学习与排序加成
///
/// `enabled` 为 1 表示启用，0 表示禁用。禁用后既不会记录新的
/// (input, text) 映射，也不会在排序时使用已有映射加分。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_input_mapping_enabled(ptr: *mut X1Engine, enabled: i32) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("set_input_mapping_enabled", (), || {
        engine.set_input_mapping_enabled(enabled != 0)
    });
}

/// 设置智能纠错开关（候选不足时用编辑距离变形补充候选，橙色显示）。
///
/// 实时生效，下次 `process` 读取。默认开（1）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_smart_correction_enabled(ptr: *mut X1Engine, enabled: i32) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("set_smart_correction_enabled", (), || {
        engine.set_smart_correction_enabled(enabled != 0)
    });
}

/// 设置整句自动固化阈值（用户选过 N 次后自动固化，0=禁用）
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_auto_solidify_threshold(ptr: *mut X1Engine, threshold: u32) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("set_auto_solidify_threshold", (), || {
        engine.set_auto_solidify_threshold(threshold)
    });
}

/// 长句第二备选开关：默认关（只给 1 条绿词）；开=2 条+动态门槛。
/// 为兼容保留（新调用方用 x1_engine_set_green_candidate_count）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_second_green_enabled(ptr: *mut X1Engine, enabled: i32) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("set_second_green_enabled", (), || {
        engine.set_second_green_enabled(enabled != 0)
    });
}

/// 长句绿词条数（1..=5）：1=只出首选（旧默认）；
/// ≥2 时第 2 条起受动态门槛约束。见 engine::green_candidate_count。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_green_candidate_count(ptr: *mut X1Engine, count: i32) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("set_green_candidate_count", (), || {
        engine.set_green_candidate_count(count.clamp(1, 5) as u8)
    });
}

/// 设置键盘误触纠错开关（仅在 smart_correction_enabled 总闸开后生效）。
///
/// 关闭后不再生成键盘误触变形（相邻键、漏键等），适合键盘误触噪音多于
/// 发音混淆的用户。默认开（1）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_keyboard_correction_enabled(
    ptr: *mut X1Engine,
    enabled: i32,
) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("set_keyboard_correction_enabled", (), || {
        engine.set_keyboard_correction_enabled(enabled != 0)
    });
}

/// 设置发音混淆纠错开关（仅在 smart_correction_enabled 总闸开后生效）。
///
/// 关闭后不再生成发音混淆变形（zh/z、c/ch 等前后鼻音/平翘舌），且上下文
/// 证据发音营救（zongguo→中国 类合法输入纠错）一并关闭——两条发音路径
/// 同一开关统管。默认开（1）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_phonetic_correction_enabled(
    ptr: *mut X1Engine,
    enabled: i32,
) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("set_phonetic_correction_enabled", (), || {
        engine.set_phonetic_correction_enabled(enabled != 0)
    });
}

/// 设置键序颠倒纠错开关（仅在 smart_correction_enabled 总闸开后生效）。
///
/// 关闭后不再生成键序颠倒变形（相邻字母交换，如 nihoa→nihao），
/// 适合觉得键序纠错噪音过多的用户。默认开（1）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_swap_correction_enabled(ptr: *mut X1Engine, enabled: i32) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("set_swap_correction_enabled", (), || {
        engine.set_swap_correction_enabled(enabled != 0)
    });
}

/// 设置纠错候选数量上限（0=禁用纠错补充，推荐 3-10）。
///
/// 控制橙色候选占比，避免挤占正常候选位。区别于 smart_correction_enabled
/// 总闸——总闸关则 maybe_correct 直接 return；本项关则进入函数但不补充候选。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
/// 设置双击故障判定阈值毫秒（偏好设置滑块；引擎内钳制 30..1000）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_double_fault_threshold_ms(ptr: *mut X1Engine, ms: u32) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("set_double_fault_threshold_ms", (), || {
        engine.set_double_fault_threshold_ms(ms)
    });
}

/// 设置单音节拆分识别开关（偏好设置「单音节拆分」）。
/// 开启后 yuan/tuan/mian/yue 等可拆音节在词库有两音节词时定点插入
/// （如 yuan→「预案」）；关闭回到旧行为（用户手动打 ' 拆分）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_ambig_split_enabled(ptr: *mut X1Engine, enabled: i32) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("set_ambig_split_enabled", (), || {
        engine.set_ambig_split_enabled(enabled != 0)
    });
}

/// 设置拆分词插入位（1 起算，偏好设置滑块；引擎内钳制 2..=5，首位恒留
/// 给完整音节最优解）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_ambig_split_position(ptr: *mut X1Engine, pos: u32) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("set_ambig_split_position", (), || {
        engine.set_ambig_split_position(pos)
    });
}

#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_max_corrections(ptr: *mut X1Engine, max: u32) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("set_max_corrections", (), || {
        engine.set_max_corrections(max)
    });
}

/// /emoj 表情规模：full != 0 = 全量档（emojiScale=full）。
/// 宿主偏好「Emoji 范围」热同步注入（引擎服务侧走 config 总线）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_emoji_full(ptr: *mut X1Engine, full: u32) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    engine.set_emoji_full(full != 0);
}

/// 联想候选开关（false = 不生成前缀延伸/末音节前缀猜测候选；简拼不受影响）。
/// 宿主偏好「联想候选」热同步注入（引擎服务侧走 config 总线）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_guess_enabled(ptr: *mut X1Engine, enabled: u32) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    engine.set_guess_enabled(enabled != 0);
}

/// 智能单词识别·中文模式开关（英文单词候选注入，偏好键 smartWordZh）。
/// 宿主偏好热同步注入（引擎服务侧走 config 总线）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_smart_word_zh_enabled(ptr: *mut X1Engine, enabled: u32) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    engine.set_smart_word_zh_enabled(enabled != 0);
}

/// 中文模式子开关·前缀补全（smartWordZhPrefix，总闸 smartWordZh 门控）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_smart_word_zh_prefix_enabled(
    ptr: *mut X1Engine,
    enabled: u32,
) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    engine.set_smart_word_zh_prefix_enabled(enabled != 0);
}

/// 中文模式子开关·英文优先排位（smartWordZhPriority，总闸门控）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_smart_word_zh_priority_enabled(
    ptr: *mut X1Engine,
    enabled: u32,
) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    engine.set_smart_word_zh_priority_enabled(enabled != 0);
}

/// 智能单词识别·英文模式开关（英文前缀补全查询，偏好键 smartWordEn）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_smart_word_en_enabled(ptr: *mut X1Engine, enabled: u32) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    engine.set_smart_word_en_enabled(enabled != 0);
}

/// 清空全部输入习惯映射
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_clear_input_mappings(ptr: *mut X1Engine) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("clear_input_mappings", (), || engine.clear_input_mappings());
}

// ── 双拼 ──────────────────────────────────────────────

/// 关闭双拼输入模式（恢复全拼）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_disable_shuangpin(ptr: *mut X1Engine) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("set_shuangpin_scheme", (), || {
        engine.set_shuangpin_scheme(None)
    });
}

/// 启用预制双拼方案。
///
/// `id` 取 "xiaohe"/"ziranma"/"microsoft"/"sogou" 之一；未知 id 时关闭双拼。
///
/// # Safety
/// `ptr` 必须有效；`id` 必须是合法 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_builtin_shuangpin(ptr: *mut X1Engine, id: *const c_char) {
    if ptr.is_null() || id.is_null() {
        return;
    }
    let id_str = match c_str_to_str(id) {
        Some(s) => s,
        None => return,
    };
    let scheme = crate::shuangpin::ShuangpinScheme::builtin_by_id(id_str).cloned();
    let engine = &mut (*ptr).inner;
    guard("set_builtin_shuangpin", (), || {
        engine.set_shuangpin_scheme(scheme)
    });
}

/// 按 id 返回内置双拼方案的 JSON 字符串（编辑器「编辑内置」的初始数据）。
///
/// 返回 NULL 表示 id 未知或参数非法；合法时返回必须由 `x1_string_free` 释放的 C 字符串。
/// JSON 格式与 `x1_engine_set_custom_shuangpin` 的入参一致（可被 `from_json` 解析）。
///
/// # Safety
/// `id` 必须是合法 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_builtin_shuangpin_json(id: *const c_char) -> *mut c_char {
    if id.is_null() {
        return std::ptr::null_mut();
    }
    let id_str = match c_str_to_str(id) {
        Some(s) => s,
        None => return std::ptr::null_mut(),
    };
    guard("builtin_shuangpin_json", std::ptr::null_mut(), || {
        match crate::shuangpin::ShuangpinScheme::builtin_by_id(id_str) {
            Some(scheme) => {
                let json = crate::shuangpin::scheme_to_json(scheme);
                // CString::new 失败仅当含 NUL（内置数据不可能），退化为 NULL
                std::ffi::CString::new(json)
                    .ok()
                    .map(|c| c.into_raw())
                    .unwrap_or(std::ptr::null_mut())
            }
            None => std::ptr::null_mut(),
        }
    })
}

/// 启用自定义双拼方案（完整方案数据以 JSON 传入）。
///
/// 返回 0=成功；-1=JSON 无效或方案不合法（此时双拼被保守地关闭）。
/// JSON 格式见 `ShuangpinScheme::from_json`。
///
/// # Safety
/// `ptr` 必须有效；`json` 必须是合法 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_custom_shuangpin(
    ptr: *mut X1Engine,
    json: *const c_char,
) -> i32 {
    if ptr.is_null() || json.is_null() {
        return -1;
    }
    let json_str = match c_str_to_str(json) {
        Some(s) => s,
        None => return -1,
    };
    // from_json 解析任意 JSON（手写解析器）+ scheme 应用——全链护栏
    let parsed = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::shuangpin::ShuangpinScheme::from_json(json_str)
    })) {
        Ok(r) => r.map_err(|_| "invalid"),
        Err(_) => Err("panic"),
    };
    match parsed {
        Ok(validated) => {
            let engine = &mut (*ptr).inner;
            guard("set_custom_shuangpin", (), || {
                engine.set_shuangpin_scheme(Some(validated.scheme))
            });
            0
        }
        Err(_) => {
            let engine = &mut (*ptr).inner;
            guard("set_shuangpin_scheme", (), || {
                engine.set_shuangpin_scheme(None)
            }); // 保守降级
            -1
        }
    }
}

/// 校验自定义双拼方案 JSON（不注入引擎）。
///
/// 返回 warnings 的 JSON 数组字符串（如 `["未映射韵母: uai"]`，空数组 `[]`=无软警告）；
/// fatal 错误（JSON 无法解析 / 方案根本不合法）返回 NULL。
/// 返回值必须由 `x1_string_free` 释放。
///
/// # Safety
/// `json` 必须是合法 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_validate_shuangpin_json(json: *const c_char) -> *mut c_char {
    if json.is_null() {
        return core::ptr::null_mut();
    }
    let json_str = match c_str_to_str(json) {
        Some(s) => s,
        None => return core::ptr::null_mut(),
    };
    match crate::shuangpin::validate_json(json_str) {
        Some(warnings_json) => match CString::new(warnings_json) {
            Ok(cstr) => cstr.into_raw(),
            Err(_) => core::ptr::null_mut(),
        },
        None => core::ptr::null_mut(),
    }
}

/// 设置临时强制全拼模式（/q 触发）。
///
/// 双拼模式下 enabled=1 时 process_inner 跳过双拼分支走全拼路径；
/// enabled=0 恢复双拼。宿主前端在重置输入状态（commit/cancel/ESC）时设回 0。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_force_fullpinyin(ptr: *mut X1Engine, enabled: i32) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("set_force_fullpinyin", (), || {
        engine.set_force_fullpinyin(enabled != 0)
    });
}

/// 用给定方案 JSON 解码双拼输入串（纯函数，不依赖引擎状态；试打预览用）。
///
/// 返回空格分隔的音节序列（如 `vsgo` → `"zhong guo"`）；失败（JSON 无效 /
/// 方案不合法 / 输入含非法字符或解不出）返回 NULL。
/// 返回值必须由 `x1_string_free` 释放。
///
/// # Safety
/// `json` 与 `input` 必须是合法 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_decode_shuangpin_json(
    json: *const c_char,
    input: *const c_char,
) -> *mut c_char {
    if json.is_null() || input.is_null() {
        return core::ptr::null_mut();
    }
    let json_str = match c_str_to_str(json) {
        Some(s) => s,
        None => return core::ptr::null_mut(),
    };
    let input_str = match c_str_to_str(input) {
        Some(s) => s,
        None => return core::ptr::null_mut(),
    };
    let validated = match crate::shuangpin::ShuangpinScheme::from_json(json_str) {
        Ok(v) => v,
        Err(_) => return core::ptr::null_mut(),
    };
    let syls = match crate::shuangpin::decode_buffer(&validated.scheme, input_str) {
        Some(s) => s,
        None => return core::ptr::null_mut(),
    };
    let out = syls.join(" ");
    match CString::new(out) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_shuangpin_json_ffi_roundtrip() {
        let json = CString::new(
            r#"{"id":"x","displayName":"x","finalMap":{"s":["iong","ong"]},"initialRemap":{"v":["zh"]},"zeroInitialStyle":"literalVowel"}"#,
        ).unwrap();
        let input = CString::new("vs").unwrap();
        let out = unsafe { x1_decode_shuangpin_json(json.as_ptr(), input.as_ptr()) };
        assert!(!out.is_null(), "decode should succeed");
        let s = unsafe { core::ffi::CStr::from_ptr(out) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(s, "zhong");
        unsafe { crate::ffi::x1_string_free(out) };
    }

    #[test]
    fn test_decode_shuangpin_json_invalid_returns_null() {
        let json = CString::new("not json").unwrap();
        let input = CString::new("vs").unwrap();
        let out = unsafe { x1_decode_shuangpin_json(json.as_ptr(), input.as_ptr()) };
        assert!(out.is_null());
        let json2 = CString::new(
            r#"{"id":"x","displayName":"x","finalMap":{"s":["iong","ong"]},"initialRemap":{"v":["zh"]},"zeroInitialStyle":"literalVowel"}"#,
        ).unwrap();
        let input2 = CString::new("vs1go").unwrap();
        let out2 = unsafe { x1_decode_shuangpin_json(json2.as_ptr(), input2.as_ptr()) };
        assert!(out2.is_null());
    }

    #[test]
    fn test_builtin_shuangpin_json_ffi_roundtrip() {
        let id = c"xiaohe";
        let out = unsafe { x1_builtin_shuangpin_json(id.as_ptr()) };
        assert!(!out.is_null(), "xiaohe 应返回 JSON");
        let s = unsafe { std::ffi::CStr::from_ptr(out) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe { crate::ffi::x1_string_free(out) };
        // 返回的 JSON 必须能被 from_json 解析且 id 一致
        let parsed = crate::shuangpin::ShuangpinScheme::from_json(&s).expect("builtin json valid");
        assert_eq!(parsed.scheme.id, "xiaohe");
        assert_eq!(parsed.scheme.display_name, "小鹤双拼");
    }

    #[test]
    fn test_builtin_shuangpin_json_unknown_id_returns_null() {
        let id = c"no_such_scheme";
        let out = unsafe { x1_builtin_shuangpin_json(id.as_ptr()) };
        assert!(out.is_null(), "未知 id 应返回 NULL");
        let id2 = c"";
        let out2 = unsafe { x1_builtin_shuangpin_json(id2.as_ptr()) };
        assert!(out2.is_null(), "空 id 应返回 NULL");
    }
}
