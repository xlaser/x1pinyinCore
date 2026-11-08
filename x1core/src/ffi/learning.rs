//! 学习类 FFI 接口：commit / bigram / 排序器权重 / 上下文管理。

use alloc::ffi::CString;
use alloc::string::String;
use core::ffi::{c_char, c_int};

use super::{c_str_to_str, guard, X1Engine};

/// 用户选择候选，记录频率（兼容旧接口，不带原始输入与拼音）
///
/// 系统多字词调用此接口时，因缺少拼音，不会进入 `user_words` 表，
/// 仅更新内存频率。推荐新代码使用 `x1_engine_commit_with_pinyin`。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`text` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_commit(ptr: *mut X1Engine, text: *const c_char) {
    if ptr.is_null() || text.is_null() {
        return;
    }

    let engine = &mut (*ptr).inner;
    if let Some(s) = c_str_to_str(text) {
        guard("commit", (), || engine.commit("", s, None, true));
    }
}

/// 用户选择候选，记录频率、保存拼音，并记录原始输入习惯映射。
///
/// `input` 是用户实际输入的原始拼音串（可为空，为空时不记录输入习惯）。
/// 宿主前端应传入候选的真实拼音，确保多字系统词也能在 `user_words` 中
/// 有正确的拼音，避免出现 pinyin 为空的记录。
///
/// `advance_context`：是否推进 bigram 上下文栈。普通提交传 1（true），
/// 自组词分步提交传 0（false，避免把整词拆成单字污染 bigram）。
/// `is_correction`：选中的是橙色纠错候选时传 1——
/// 学习正确词（freq/bigram/竞争）照常，但不把错串→词写进 input_mappings
/// （错串直达属冗余且误选永久留痕，纠错撤销也不回滚 mapping）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`text`、`pinyin`、`input` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_commit_with_pinyin(
    ptr: *mut X1Engine,
    text: *const c_char,
    pinyin: *const c_char,
    input: *const c_char,
    advance_context: c_int,
    is_correction: c_int,
) {
    if ptr.is_null() || text.is_null() {
        return;
    }

    let engine = &mut (*ptr).inner;
    if let Some(s) = c_str_to_str(text) {
        let p = c_str_to_str(pinyin);
        let inp = c_str_to_str(input).unwrap_or("");
        guard("commit_with_pinyin", (), || {
            if is_correction != 0 {
                engine.commit_correction(inp, s, p, advance_context != 0, false)
            } else {
                engine.commit(inp, s, p, advance_context != 0)
            }
        });
    }
}

/// 用户选择候选并指定是否固化整句为白词（Shift+数字手动固化快捷键）。
///
/// 与 `x1_engine_commit_with_pinyin` 区别：`force_solidify != 0` 时，整句（≤8字）会固化
/// 进 user_words 成为白词。普通数字键选词应调用 `x1_engine_commit_with_pinyin`
/// （默认不固化，只拆词+bigram 学习），Shift+数字键才调用本函数。
/// `is_correction` 语义同 `x1_engine_commit_with_pinyin`。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`text`、`pinyin`、`input` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_commit_with_solidify(
    ptr: *mut X1Engine,
    text: *const c_char,
    pinyin: *const c_char,
    input: *const c_char,
    advance_context: c_int,
    force_solidify: c_int,
    is_correction: c_int,
) {
    if ptr.is_null() || text.is_null() {
        return;
    }

    let engine = &mut (*ptr).inner;
    if let Some(s) = c_str_to_str(text) {
        let p = c_str_to_str(pinyin);
        let inp = c_str_to_str(input).unwrap_or("");
        guard("commit_with_solidify", (), || {
            if is_correction != 0 {
                engine.commit_correction(inp, s, p, advance_context != 0, force_solidify != 0)
            } else {
                engine.commit_with_solidify(inp, s, p, advance_context != 0, force_solidify != 0)
            }
        });
    }
}

/// 记录一条带权重的 bigram 共现（宿主前端在系统分词后调用）。
///
/// 供宿主前端在 commit 后用系统分词器实时切分上屏文本，把切出的相邻词对
/// 作为 bigram 写入引擎内存（与引擎内部的 record_bigram_weighted 同路径，flush 时持久化）。
/// 比离线工具直接写 SQLite 更优：内存即时生效，下次查询即可用，且 flush 统一。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`prev`/`text` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_record_bigram_weighted(
    ptr: *mut X1Engine,
    prev: *const c_char,
    text: *const c_char,
    weight: u32,
) {
    if ptr.is_null() || prev.is_null() || text.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    let (prev_str, text_str) = match (c_str_to_str(prev), c_str_to_str(text)) {
        (Some(p), Some(t)) => (p, t),
        _ => return,
    };
    guard("record_bigram_weighted", (), || {
        engine.record_word_bigram_weighted(prev_str, text_str, weight);
    });
}

/// 回滚上一次提交的跨词 n-gram 增量（纠错撤销）。
/// 返回 1 = 有实际回滚；0 = 无可撤销快照/未记录。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_revert_last_commit(ptr: *mut X1Engine) -> i32 {
    if ptr.is_null() {
        return 0;
    }
    let engine = &mut (*ptr).inner;
    if guard("revert_last_commit", false, || engine.revert_last_commit()) {
        1
    } else {
        0
    }
}

/// 回滚一次指定 bigram 增量（候选内部词对逐对撤销；权重取记录时的实际值）。
/// 返回 1 = 有实际回滚。
///
/// # Safety
/// `ptr`、`prev`、`text` 必须是有效指针/UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_revert_bigram_weighted(
    ptr: *mut X1Engine,
    prev: *const c_char,
    text: *const c_char,
    weight: u32,
) -> i32 {
    if ptr.is_null() || prev.is_null() || text.is_null() {
        return 0;
    }
    let engine = &mut (*ptr).inner;
    let (prev_str, text_str) = match (c_str_to_str(prev), c_str_to_str(text)) {
        (Some(p), Some(t)) => (p, t),
        _ => return 0,
    };
    if guard("revert_bigram_weighted", false, || {
        engine.revert_bigram(prev_str, text_str, weight)
    }) {
        1
    } else {
        0
    }
}

/// 删除单个 bigram pair（审计/清理；返回 1 = 实际删除）。
///
/// # Safety
/// `ptr`、`prev`、`text` 必须是有效指针/UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_forget_bigram(
    ptr: *mut X1Engine,
    prev: *const c_char,
    text: *const c_char,
) -> i32 {
    if ptr.is_null() || prev.is_null() || text.is_null() {
        return 0;
    }
    let engine = &mut (*ptr).inner;
    let (prev_str, text_str) = match (c_str_to_str(prev), c_str_to_str(text)) {
        (Some(p), Some(t)) => (p, t),
        _ => return 0,
    };
    if guard("forget_bigram", false, || {
        engine.forget_bigram_pair(prev_str, text_str)
    }) {
        1
    } else {
        0
    }
}

/// 删除单个 trigram（审计/清理；返回 1 = 实际删除）。
///
/// # Safety
/// `ptr`、`prev2`、`prev1`、`text` 必须是有效指针/UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_forget_trigram(
    ptr: *mut X1Engine,
    prev2: *const c_char,
    prev1: *const c_char,
    text: *const c_char,
) -> i32 {
    if ptr.is_null() || prev2.is_null() || prev1.is_null() || text.is_null() {
        return 0;
    }
    let engine = &mut (*ptr).inner;
    let (p2, p1, t) = match (c_str_to_str(prev2), c_str_to_str(prev1), c_str_to_str(text)) {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        _ => return 0,
    };
    if guard("forget_trigram", false, || {
        engine.forget_trigram_pair(p2, p1, t)
    }) {
        1
    } else {
        0
    }
}

/// 噪声治理观测计数开关（高级页「噪声治理统计」子开关；与调试
/// 日志总开关的合成在宿主前端完成——引擎只收最终布尔）。关闭时全部自增点
/// 零开销跳过；学习行为本身不变，只停观测计数。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_learning_stats_enabled(ptr: *mut X1Engine, enabled: i32) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("set_learning_stats_enabled", (), || {
        engine.set_learning_stats_enabled(enabled != 0)
    });
}

/// 学习治理观测（会话级计数器 JSON）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_learning_stats(ptr: *const X1Engine) -> *mut c_char {
    if ptr.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let json = guard("learning_stats", String::from("{}"), || {
        engine.learning_stats_json()
    });
    match CString::new(json) {
        Ok(c) => c.into_raw(),
        Err(_) => core::ptr::null_mut(),
    }
}

/// 注入学习型排序器权重（离线训练完成后调用，运行时生效）。
///
/// weights_json 是 6 个浮点数的 JSON 数组（如 `[0.002,-0.0001,0,0.001,0,0]`），
/// 顺序与 ScoreBreakdown 对齐：base/total_bonus/recent/mapping_bonus/bigram_bonus/long_word_bonus。
/// 解析失败或长度不对则忽略（不改权重）。全 0 视为未训练。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`weights_json` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_set_ranker_weights(
    ptr: *mut X1Engine,
    weights_json: *const c_char,
) -> i32 {
    if ptr.is_null() || weights_json.is_null() {
        return 0;
    }
    let engine = &mut (*ptr).inner;
    let json = match c_str_to_str(weights_json) {
        Some(s) => s,
        None => return 0,
    };
    // 手写解析 JSON 数组（避免 serde 依赖）：剥 []，按 , 分割，解析浮点。
    let trimmed = json.trim().trim_start_matches('[').trim_end_matches(']');
    let parts: Vec<&str> = trimmed.split(',').collect();
    if parts.len() != 6 {
        return 0;
    }
    let mut weights = [0.0f64; 6];
    for (i, p) in parts.iter().enumerate() {
        match p.trim().parse::<f64>() {
            Ok(v) => weights[i] = v,
            Err(_) => return 0,
        }
    }
    guard("set_ranker_weights", 0, || {
        engine.set_ranker_weights(weights);
        1
    })
}

/// 清空 bigram 上下文栈（句子边界时调用）。
///
/// 标点清栈：宿主前端在中文标点上屏时调用此 FFI。标点是天然句界，
/// 跨句 bigram 信号弱且易误导（前句尾词残留栈里会让下句首字被前句 user_bigram 干扰），
/// 因此标点上屏即清空上下文栈。单字链式弱化见 `context_bonus_with_ctx`。
/// 注意：只清空会话级上下文栈，不清空持久化的 bigram 共现表（那是学习成果）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_clear_context(ptr: *mut X1Engine) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("clear_context", (), || engine.clear_context());
}

/// 自适应排序：重算动态参数（dyn_cap 等）。后台 60s 周期调用，不阻塞按键。
///
/// 宿主前端在 flush 后于后台线程异步调用。recompute 是纯内存计算
///（扫描 bigram 表），无 DB 写入，<1ms，不阻塞输入事件循环。
///
/// # Safety
/// `ptr` 必须是有效引擎指针
#[no_mangle]
pub unsafe extern "C" fn x1_engine_recompute_modulators(ptr: *mut X1Engine) {
    if ptr.is_null() {
        return;
    }
    let engine = &mut (*ptr).inner;
    guard("recompute_modulators", (), || engine.recompute_modulators());
}

/// 从词库中删除一个词
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`text` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_remove(ptr: *mut X1Engine, text: *const c_char) {
    if ptr.is_null() || text.is_null() {
        return;
    }

    // remove 走 5 步全源删除 + SQL（Trie/user_words/input_mappings/黑名单持久化），
    // 必须有 panic 护栏。
    let engine = &mut (*ptr).inner;
    if let Some(t) = c_str_to_str(text) {
        guard("remove", (), || engine.remove(t));
    }
}

/// 手动造词（字+字组合成词，整体固化到 user_words）
///
/// 当前宿主前端自组词已统一走 commit（拆词学习搭配关系），此函数保留为
/// 备用接口：外部工具/脚本造词、未来词库管理器强制固化、特定长词整体保存。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`text` 和 `input` 必须是有效 UTF-8 C 字符串
#[no_mangle]
pub unsafe extern "C" fn x1_engine_learn(
    ptr: *mut X1Engine,
    text: *const c_char,
    input: *const c_char,
) {
    if ptr.is_null() || text.is_null() || input.is_null() {
        return;
    }

    let engine = &mut (*ptr).inner;
    if let (Some(t), Some(i)) = (c_str_to_str(text), c_str_to_str(input)) {
        guard("learn", (), || engine.learn(t, i));
    }
}

// ── 固定候选（user_pins；FFI 与引擎服务 IPC 同语义暴露）──
// 宿主触发 = 候选窗组合键+数字 1-3：固定当前高亮候选到
// N 号槽位，同槽位再按 = 解除。切换判定在调用方。

/// 固定候选到槽位（slot 1-3）。键 = 原始输入串（green_memo 同口径）。
///
/// 返回 0=成功，-1=失败（槽位非法/键非法/文本非法/超 MAX_PIN_ENTRIES 上限）。
/// 内存立即生效（下次查询 apply_pins 重排）+ 立即落盘（不走 60s flush）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`input`、`text` 必须是有效 UTF-8 C 字符串。
/// 必须在主线程调用（engine 无锁，跨线程并发 = UB）。
#[no_mangle]
pub unsafe extern "C" fn x1_engine_pin_candidate(
    ptr: *mut X1Engine,
    input: *const c_char,
    slot: u8,
    text: *const c_char,
) -> c_int {
    if ptr.is_null() || input.is_null() || text.is_null() {
        return -1;
    }
    let engine = &mut (*ptr).inner;
    if let (Some(i), Some(t)) = (c_str_to_str(input), c_str_to_str(text)) {
        guard("pin_candidate", -1, || {
            match engine.pin_candidate(i, slot, t) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("[x1core] pin_candidate 失败: {}", e);
                    -1
                }
            }
        })
    } else {
        -1
    }
}

/// 解除槽位（slot 1-3）的固定。不存在的槽 = 幂等成功。
///
/// 返回 0=成功，-1=失败（槽位非法）。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`input` 必须是有效 UTF-8 C 字符串。主线程调用。
#[no_mangle]
pub unsafe extern "C" fn x1_engine_unpin_candidate(
    ptr: *mut X1Engine,
    input: *const c_char,
    slot: u8,
) -> c_int {
    if ptr.is_null() || input.is_null() {
        return -1;
    }
    let engine = &mut (*ptr).inner;
    if let Some(i) = c_str_to_str(input) {
        guard("unpin_candidate", -1, || {
            match engine.unpin_candidate(i, slot) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("[x1core] unpin_candidate 失败: {}", e);
                    -1
                }
            }
        })
    } else {
        -1
    }
}

/// 查询输入串的固定槽位（读侧标注：候选序号红显）。
///
/// 返回 JSON 数组字符串（恒 3 项，槽位 1-3，未固定 = null），如
/// `["的",null,"地"]`；该输入无任何固定时返回 null 指针。
/// 调用方用 `x1_string_free` 释放返回的字符串。
///
/// # Safety
/// `ptr` 必须是有效引擎指针；`input` 必须是有效 UTF-8 C 字符串。主线程调用。
#[no_mangle]
pub unsafe extern "C" fn x1_engine_pins_for(
    ptr: *mut X1Engine,
    input: *const c_char,
) -> *mut c_char {
    if ptr.is_null() || input.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let Some(i) = c_str_to_str(input) else {
        return core::ptr::null_mut();
    };
    guard("pins_for", core::ptr::null_mut(), || {
        let Some(slots) = engine.pins_for(i) else {
            return core::ptr::null_mut();
        };
        let items: Vec<String> = slots
            .iter()
            .map(|s| match s {
                Some(t) => format!("\"{}\"", pin_json_escape(t)),
                None => "null".to_string(),
            })
            .collect();
        match alloc::ffi::CString::new(format!("[{}]", items.join(","))) {
            Ok(c) => c.into_raw(),
            Err(_) => core::ptr::null_mut(),
        }
    })
}

/// 查输入的用户学习映射（AI 候选尊重用户学习）。
///
/// 用户选过的词按「原始 input」记入 user_input_mappings；AI 候选插入前查此
/// 接口：已有映射且与模型输出不同 = 用户教过这个输入，AI 不得用猜测顶掉用户
/// 的纠正（实证：shicexiaoguo 已映射「实测效果」×4，模型仍推「是测效果」插到
/// #1，而引擎 #0 早已是用户教的词）。
///
/// 扫原始 input_mappings（全长度覆盖）而非 green_memo（后者仅 3-8 字绿词口径，
/// 会漏掉 shice→实测 这类短映射）；数千条线性扫，每次 AI 落位至多一次，微秒级。
/// 返回映射文本（多条取 count 最大者）；无映射返回 null。
/// 调用方用 `x1_string_free` 释放。
///
/// # Safety
/// `ptr` 必须是有效引擎指针。主线程调用。
#[no_mangle]
pub unsafe extern "C" fn x1_engine_user_mapping_for(
    ptr: *mut X1Engine,
    input: *const c_char,
) -> *mut c_char {
    if ptr.is_null() || input.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    let Some(i) = c_str_to_str(input) else {
        return core::ptr::null_mut();
    };
    guard("user_mapping_for", core::ptr::null_mut(), || {
        // 语义源在 Engine::user_mapping_for（引擎服务侧的
        // user_mapping_for IPC 同调该方法，FFI/服务单一实现）
        let Some(text) = engine.user_mapping_for(i) else {
            return core::ptr::null_mut();
        };
        match alloc::ffi::CString::new(text) {
            Ok(c) => c.into_raw(),
            Err(_) => core::ptr::null_mut(),
        }
    })
}

/// 列出全部固定项（偏好设置「固定候选」管理卡片用；引擎服务侧 IPC
/// 提供同语义接口）。
///
/// 返回 JSON 数组字符串，如 `[{"input":"wo","slot":1,"text":"我"}]`
/// （按 input/slot 排序，与引擎 list_pins 一致）；无固定项返回 `"[]"`。
/// 调用方用 `x1_string_free` 释放返回的字符串。
///
/// # Safety
/// `ptr` 必须是有效引擎指针。主线程调用。
#[no_mangle]
pub unsafe extern "C" fn x1_engine_list_pins(ptr: *mut X1Engine) -> *mut c_char {
    if ptr.is_null() {
        return core::ptr::null_mut();
    }
    let engine = &(*ptr).inner;
    guard("list_pins", core::ptr::null_mut(), || {
        let items: Vec<String> = engine
            .list_pins()
            .iter()
            .map(|(input, slot, text)| {
                format!(
                    "{{\"input\":\"{}\",\"slot\":{},\"text\":\"{}\"}}",
                    pin_json_escape(input),
                    slot,
                    pin_json_escape(text)
                )
            })
            .collect();
        match alloc::ffi::CString::new(format!("[{}]", items.join(","))) {
            Ok(c) => c.into_raw(),
            Err(_) => core::ptr::null_mut(),
        }
    })
}

/// pin JSON 字符串字段转义（键为 ascii 字母/撇号、文本为中文——引号与反斜杠
/// 实际不可达，防御性转义保持 JSON 合法性）。
fn pin_json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
