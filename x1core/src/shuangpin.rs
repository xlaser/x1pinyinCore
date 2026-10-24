//! 双拼输入编码层。
//!
//! 双拼是「输入编码层」：每个汉字固定 2 键（声母键 + 韵母键），方案 = 两张查找表
//! （声母重映射 + 韵母键映射）。两张表均为多值模型：一个键可映射多个候选
//! （列表顺序即消歧优先级），解码时按声母候选 × 韵母候选笛卡尔积逐个试有效性。
//! 本模块把双拼 buffer 解码成权威音节序列（如 `vsgo` → `["zhong","guo"]`），
//! engine 拿序列直查 Trie，跳过全拼 DP 枚举。

use std::collections::HashMap;
use std::sync::OnceLock;

/// 双拼方案（预制 + 自定义共用此结构）。
#[derive(Clone, Debug)]
pub struct ShuangpinScheme {
    /// 方案 id。预制："xiaohe"/"ziranma"/"microsoft"/"sogou"；自定义："custom-<uuid>"。
    pub id: String,
    /// 显示名（"小鹤双拼" / "我的方案"）。
    pub display_name: String,
    /// 是否预制（true = 不可编辑/不可删除）。
    pub is_builtin: bool,
    /// 韵母键 → 候选韵母列表（顺序即消歧优先级）。键含 a-z，微软/搜狗/自定义还可能含 ';'。
    pub final_map: HashMap<char, Vec<String>>,
    /// 声母键重映射（仅列与「键=自身」不同的项；一个键可映射多个声母候选，顺序即消歧优先级）。
    pub initial_remap: HashMap<char, Vec<String>>,
    /// 零声母编码策略。
    pub zero_initial_style: ZeroInitialStyle,
    /// 零声母逐音节编码表（仅 Custom 风格）：
    /// (首键, 末键) → 音节。首道式的记忆码方案（a→aa/ai→ai/ang→ay/
    /// e→ue/ei→ui/eng→uf/er→er/...）既非字面元音也非 o 前缀，只能整表表达。
    pub zero_initial_codes: HashMap<(char, char), String>,
}

/// 零声母音节（a/o/e/ai/ei/...）的编码策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZeroInitialStyle {
    /// 小鹤/自然码：首键 = 字面首元音，末键 = 韵母键；单元音双写（aa/oo/ee）。
    LiteralVowel,
    /// 微软/搜狗：首键恒 'o'，末键 = 韵母键；单元音 = oo。
    OPrefixed,
    /// 首道式：逐音节记忆码表（zero_initial_codes）。解码时表优先于常规
    /// 路径（表内码与常规路径要么同答案要么常规路径本就无解，已逐码验证）。
    Custom,
}

static BUILTINS_CELL: OnceLock<Vec<ShuangpinScheme>> = OnceLock::new();

impl ShuangpinScheme {
    /// 8 个预制方案（首次访问时构造，之后只读）。智能ABC/拼音加加/紫光为后续
    /// 扩充，键位与主流双拼方案键位表交叉验证。
    pub fn builtins() -> &'static [ShuangpinScheme] {
        BUILTINS_CELL.get_or_init(|| {
            vec![
                xiaohe(),
                ziranma(),
                microsoft(),
                sogou(),
                abc(),
                pyjj(),
                ziguang(),
                shoudao(),
            ]
        })
    }

    /// 从 8 个预制方案里按 id 查找。
    pub fn builtin_by_id(id: &str) -> Option<&'static ShuangpinScheme> {
        Self::builtins().iter().find(|s| s.id == id)
    }
}

// ── 预制方案构造 helper（减少重复）──────────────────────────────

fn initial_remap_zhchsh() -> HashMap<char, Vec<String>> {
    // 4 个方案的声母重映射完全相同：v→zh, i→ch, u→sh（多值模型下为单元素列表）
    let mut m = HashMap::new();
    m.insert('v', vec!["zh".to_string()]);
    m.insert('i', vec!["ch".to_string()]);
    m.insert('u', vec!["sh".to_string()]);
    m
}

fn final_map_from(pairs: &[(&str, &[&str])]) -> HashMap<char, Vec<String>> {
    pairs
        .iter()
        .map(|(k, vs)| {
            (
                k.chars().next().unwrap(),
                vs.iter().map(|s| s.to_string()).collect(),
            )
        })
        .collect()
}

// ── 预制方案 ──

fn xiaohe() -> ShuangpinScheme {
    ShuangpinScheme {
        id: "xiaohe".to_string(),
        display_name: "小鹤双拼".to_string(),
        is_builtin: true,
        zero_initial_style: ZeroInitialStyle::LiteralVowel,
        zero_initial_codes: HashMap::new(),
        initial_remap: initial_remap_zhchsh(),
        final_map: final_map_from(&[
            ("a", &["a"]),
            ("b", &["in"]),
            ("c", &["ao"]),
            ("d", &["ai"]),
            ("e", &["e"]),
            ("f", &["en"]),
            ("g", &["eng"]),
            ("h", &["ang"]),
            ("i", &["i"]),
            ("j", &["an"]),
            ("k", &["ing", "uai"]),
            ("l", &["iang", "uang"]),
            ("m", &["ian"]),
            ("n", &["iao"]),
            ("o", &["uo", "o"]),
            ("p", &["ie"]),
            ("q", &["iu"]),
            ("r", &["uan"]),
            ("s", &["iong", "ong"]),
            ("t", &["ue", "ve"]),
            ("u", &["u"]),
            ("v", &["ui", "v"]),
            ("w", &["ei"]),
            ("x", &["ia", "ua"]),
            ("y", &["un"]),
            ("z", &["ou"]),
        ]),
    }
}

fn ziranma() -> ShuangpinScheme {
    ShuangpinScheme {
        id: "ziranma".to_string(),
        display_name: "自然码".to_string(),
        is_builtin: true,
        zero_initial_style: ZeroInitialStyle::LiteralVowel,
        zero_initial_codes: HashMap::new(),
        initial_remap: initial_remap_zhchsh(),
        final_map: final_map_from(&[
            ("a", &["a"]),
            ("b", &["ou"]),
            ("c", &["iao"]),
            ("d", &["iang", "uang"]),
            ("e", &["e"]),
            ("f", &["en"]),
            ("g", &["eng"]),
            ("h", &["ang"]),
            ("i", &["i"]),
            ("j", &["an"]),
            ("k", &["ao"]),
            ("l", &["ai"]),
            ("m", &["ian"]),
            ("n", &["in"]),
            ("o", &["uo", "o"]),
            ("p", &["un"]),
            ("q", &["iu"]),
            ("r", &["uan"]),
            ("s", &["iong", "ong"]),
            ("t", &["ue", "ve"]),
            ("u", &["u"]),
            ("v", &["ui", "v"]),
            ("w", &["ia", "ua"]),
            ("x", &["ie"]),
            ("y", &["ing", "uai"]),
            ("z", &["ei"]),
        ]),
    }
}

fn microsoft() -> ShuangpinScheme {
    ShuangpinScheme {
        id: "microsoft".to_string(),
        display_name: "微软双拼".to_string(),
        is_builtin: true,
        zero_initial_style: ZeroInitialStyle::OPrefixed,
        zero_initial_codes: HashMap::new(),
        initial_remap: initial_remap_zhchsh(),
        final_map: final_map_from(&[
            ("a", &["a"]),
            ("b", &["ou"]),
            ("c", &["iao"]),
            ("d", &["iang", "uang"]),
            ("e", &["e"]),
            ("f", &["en"]),
            ("g", &["eng"]),
            ("h", &["ang"]),
            ("i", &["i"]),
            ("j", &["an"]),
            ("k", &["ao"]),
            ("l", &["ai"]),
            ("m", &["ian"]),
            ("n", &["in"]),
            ("o", &["uo", "o"]),
            ("p", &["un"]),
            ("q", &["iu"]),
            ("r", &["uan"]),
            ("s", &["iong", "ong"]),
            ("t", &["ue", "ve"]),
            ("u", &["u"]),
            ("v", &["ui", "v"]),
            ("w", &["ia", "ua"]),
            ("x", &["ie"]),
            ("y", &["uai", "v"]),
            ("z", &["ei"]),
            (";", &["ing"]),
        ]),
    }
}

fn sogou() -> ShuangpinScheme {
    // 搜狗方案与微软方案键位相同，单独成方案（id/display_name 不同）。
    let mut s = microsoft();
    s.id = "sogou".to_string();
    s.display_name = "搜狗双拼".to_string();
    s
}

// ── 后续扩充 3 套（键位与主流双拼方案键位表交叉验证）──

/// 智能ABC双拼：声母 zh→a / ch→e / sh→v（与常见 v/i/u 布局不同），
/// 零声母 o 前缀（OPrefixed）。er 编码 = or（r 键 = er/iu，命中既有 r→er 特例）。
fn abc() -> ShuangpinScheme {
    ShuangpinScheme {
        id: "abc".to_string(),
        display_name: "智能ABC双拼".to_string(),
        is_builtin: true,
        zero_initial_style: ZeroInitialStyle::OPrefixed,
        zero_initial_codes: HashMap::new(),
        initial_remap: {
            let mut m = HashMap::new();
            m.insert('a', vec!["zh".to_string()]);
            m.insert('e', vec!["ch".to_string()]);
            m.insert('v', vec!["sh".to_string()]);
            m
        },
        final_map: final_map_from(&[
            ("a", &["a"]),
            ("b", &["ou"]),
            ("c", &["in", "uai"]),
            ("d", &["ia", "ua"]),
            ("e", &["e"]),
            ("f", &["en"]),
            ("g", &["eng"]),
            ("h", &["ang"]),
            ("i", &["i"]),
            ("j", &["an"]),
            ("k", &["ao"]),
            ("l", &["ai"]),
            ("m", &["ue", "ve", "ui"]),
            ("n", &["un"]),
            ("o", &["uo", "o"]),
            ("p", &["uan"]),
            ("q", &["ei"]),
            ("r", &["er", "iu"]),
            ("s", &["iong", "ong"]),
            ("t", &["iang", "uang"]),
            ("u", &["u"]),
            ("v", &["v"]),
            ("w", &["ian"]),
            ("x", &["ie"]),
            ("y", &["ing"]),
            ("z", &["iao"]),
        ]),
    }
}

/// 拼音加加双拼：声母 v→zh / i→sh / u→ch（注意 i→sh、u→ch 与
/// 小鹤系相反！），零声母字面元音（a/e 双写首字母，o 字面，LiteralVowel）。
/// er 编码 = eq（q 键 = er/ing），ing 零声母无（ying = y+q）。
fn pyjj() -> ShuangpinScheme {
    ShuangpinScheme {
        id: "pyjj".to_string(),
        display_name: "拼音加加双拼".to_string(),
        is_builtin: true,
        zero_initial_style: ZeroInitialStyle::LiteralVowel,
        zero_initial_codes: HashMap::new(),
        initial_remap: {
            let mut m = HashMap::new();
            m.insert('v', vec!["zh".to_string()]);
            m.insert('i', vec!["sh".to_string()]);
            m.insert('u', vec!["ch".to_string()]);
            m
        },
        final_map: final_map_from(&[
            ("a", &["a"]),
            ("b", &["ia", "ua"]),
            ("c", &["uan"]),
            ("d", &["ao"]),
            ("e", &["e"]),
            ("f", &["an"]),
            ("g", &["ang"]),
            ("h", &["iang", "uang"]),
            ("i", &["i"]),
            ("j", &["ian"]),
            ("k", &["iao"]),
            ("l", &["in"]),
            ("m", &["ie"]),
            ("n", &["iu"]),
            ("o", &["uo", "o"]),
            ("p", &["ou"]),
            ("q", &["er", "ing"]),
            ("r", &["en"]),
            ("s", &["ai"]),
            ("t", &["eng"]),
            ("u", &["u"]),
            ("v", &["ui", "v"]),
            ("w", &["ei"]),
            ("x", &["ue", "ve", "uai"]),
            ("y", &["iong", "ong"]),
            ("z", &["un"]),
        ]),
    }
}

/// 紫光双拼：声母 zh→a / ch→u / sh→i，零声母 o 前缀
/// （OPrefixed），ing 在分号键（o; → ing，命中既有 ';' 特例）。er 编码 = oq（q 键 = er）。
fn ziguang() -> ShuangpinScheme {
    ShuangpinScheme {
        id: "ziguang".to_string(),
        display_name: "紫光双拼".to_string(),
        is_builtin: true,
        zero_initial_style: ZeroInitialStyle::OPrefixed,
        zero_initial_codes: HashMap::new(),
        initial_remap: {
            let mut m = HashMap::new();
            m.insert('a', vec!["zh".to_string()]);
            m.insert('u', vec!["ch".to_string()]);
            m.insert('i', vec!["sh".to_string()]);
            m
        },
        final_map: final_map_from(&[
            ("a", &["a"]),
            ("b", &["ia", "ua"]),
            ("c", &["uan"]),
            ("d", &["ao"]),
            ("e", &["e"]),
            ("f", &["an"]),
            ("g", &["ang"]),
            ("h", &["iang", "uang"]),
            ("i", &["i"]),
            ("j", &["ian"]),
            ("k", &["iao"]),
            ("l", &["in"]),
            ("m", &["ie"]),
            ("n", &["iu"]),
            ("o", &["uo", "o"]),
            ("p", &["ou"]),
            ("q", &["er"]),
            ("r", &["en"]),
            ("s", &["ai"]),
            ("t", &["eng"]),
            ("u", &["u"]),
            ("v", &["ue", "ve", "ui"]),
            ("w", &["ei"]),
            ("x", &["v", "uai"]),
            ("y", &["iong", "ong"]),
            ("z", &["un"]),
            (";", &["ing"]),
        ]),
    }
}

/// 首道双拼（第 8 套预制）。
/// 特色：zh→V/ch→I/sh→E（e 键兼作 sh 声母与 e 韵母——声母/韵母分槽无冲突）；
/// üe 拆两家（nüe/lüe 走 B 键 ve，jue/que/xue 走 L 键 ue）；零声母用逐音节
/// 记忆码表（a→aa/ai→ai/an→an/ang→ay/ao→ao/e→ue/ei→ui/en→en/eng→uf/
/// er→er/o→oo/ou→ou）——既非字面元音也非 o 前缀，引擎为此新增 Custom 风格。
/// 表内码与常规解码路径已逐码验证无冲突（要么同答案、要么常规路径本就无解，
/// 如 en = E(sh)+N(ian)→shian 无效、er = E(sh)+R(ie)→shie 无效）。
fn shoudao() -> ShuangpinScheme {
    let zero_codes: Vec<(&str, &str)> = [
        ("a", "aa"),
        ("ai", "ai"),
        ("an", "an"),
        ("ang", "ay"),
        ("ao", "ao"),
        ("e", "ue"),
        ("ei", "ui"),
        ("en", "en"),
        ("eng", "uf"),
        ("er", "er"),
        ("o", "oo"),
        ("ou", "ou"),
    ]
    .to_vec();
    let mut zero_initial_codes = HashMap::new();
    for (syl, code) in zero_codes {
        let mut cs = code.chars();
        let (k1, k2) = (cs.next().unwrap(), cs.next().unwrap());
        zero_initial_codes.insert((k1, k2), syl.to_string());
    }
    ShuangpinScheme {
        id: "shoudao".to_string(),
        display_name: "首道双拼".to_string(),
        is_builtin: true,
        zero_initial_style: ZeroInitialStyle::Custom,
        zero_initial_codes,
        initial_remap: {
            let mut m = HashMap::new();
            m.insert('v', vec!["zh".to_string()]);
            m.insert('i', vec!["ch".to_string()]);
            m.insert('e', vec!["sh".to_string()]);
            m
        },
        final_map: final_map_from(&[
            ("q", &["iu"]),
            ("w", &["ua"]),
            ("e", &["e"]),
            ("r", &["ie"]),
            ("t", &["uan"]),
            ("y", &["ang"]),
            ("u", &["u"]),
            ("i", &["i"]),
            ("o", &["o", "uo"]),
            ("p", &["iao"]),
            ("a", &["a"]),
            ("s", &["ou"]),
            ("d", &["ao"]),
            ("f", &["eng"]),
            ("g", &["ing", "uai"]),
            ("h", &["ong", "iong"]),
            ("j", &["an"]),
            ("k", &["en", "ia"]),
            ("l", &["ai", "ue"]),
            ("z", &["un"]),
            ("x", &["iang", "uang"]),
            ("c", &["in"]),
            ("v", &["v", "ui"]),
            ("b", &["ve"]),
            ("n", &["ian"]),
            ("m", &["ei"]),
        ]),
    }
}

use crate::pinyin::is_valid_syllable_pub;

/// 一组按键（最多 2 键，末组可能 1 键 = 不完整声母）。
struct Pair {
    k1: char,
    k2: Option<char>,
}

/// 按位置每 2 字符切分。`'` 是分隔符（不进组，重置计数）。
fn split_pairs(input: &str) -> Vec<Pair> {
    let mut pairs = Vec::new();
    let mut buf: Vec<char> = Vec::new();
    for ch in input.chars() {
        if ch == '\'' {
            flush(&mut buf, &mut pairs);
        } else {
            buf.push(ch);
            if buf.len() == 2 {
                flush(&mut buf, &mut pairs);
            }
        }
    }
    flush(&mut buf, &mut pairs);
    pairs
}

fn flush(buf: &mut Vec<char>, pairs: &mut Vec<Pair>) {
    if buf.is_empty() {
        return;
    }
    if buf.len() == 1 {
        pairs.push(Pair {
            k1: buf[0],
            k2: None,
        });
    } else {
        pairs.push(Pair {
            k1: buf[0],
            k2: Some(buf[1]),
        });
    }
    buf.clear();
}

/// 把双拼 buffer 解码成权威音节序列。
///
/// 返回 None 的情形：buffer 为空 / 含非法字符 / 某组解不出合法音节（且非单键不完整组）。
/// 返回 Some 时，末项可能是单字母声母（不完整，如 `vsg` → `[zhong, g]`）。
pub fn decode_buffer(scheme: &ShuangpinScheme, input: &str) -> Option<Vec<String>> {
    if input.is_empty() {
        return None;
    }
    let pairs = split_pairs(input);
    if pairs.is_empty() {
        return None;
    }

    for p in &pairs {
        if !is_valid_key(p.k1) {
            return None;
        }
        if let Some(k2) = p.k2 {
            if !is_valid_key(k2) {
                return None;
            }
        }
    }

    let mut syllables = Vec::new();
    for p in &pairs {
        {
            let syl = decode_pair(scheme, p)?;
            syllables.push(syl)
        }
    }
    Some(syllables)
}

fn is_valid_key(c: char) -> bool {
    c.is_ascii_lowercase() || c == ';'
}

/// 解码一组 (k1, k2)。None = 解不出合法音节。
fn decode_pair(scheme: &ShuangpinScheme, p: &Pair) -> Option<String> {
    // 单键组（末尾不完整声母）→ 输出该键的首个声母候选（多声母：取消歧顺序首位）
    if p.k2.is_none() {
        let initials = resolve_initials(scheme, p.k1);
        return Some(
            initials
                .first()
                .cloned()
                .unwrap_or_else(|| p.k1.to_string()),
        );
    }
    let k2 = p.k2.unwrap();

    // Custom 方案（首道式）：零声母记忆码表**优先**查表——表内码与常规路径
    // 无冲突（同答案或常规本就无解，见 shoudao() 注释），且 en/er 这类
    // E(sh)+韵母 组合必须先命中表才不会掉进 sh声母路径解出无效音节。
    if scheme.zero_initial_style == ZeroInitialStyle::Custom {
        if let Some(syl) = scheme.zero_initial_codes.get(&(p.k1, k2)) {
            return Some(syl.clone());
        }
    }

    // OPrefixed 方案 + k1=='o' → 零声母路径
    if scheme.zero_initial_style == ZeroInitialStyle::OPrefixed && p.k1 == 'o' {
        return decode_zero_initial_oprefixed(scheme, k2);
    }

    // LiteralVowel 方案 + k1 是字面首元音（a/o/e）→ 零声母路径。
    // 零声母音节本身就是韵母（如 ai/an/ang/er/ing 等）：先试「韵母作独立音节」，
    // 再退化到「字面首元音 + k2」（兜底 er 这种 r 键在 final_map 是 uan、但字面 e+r=er 命中的情形）。
    if scheme.zero_initial_style == ZeroInitialStyle::LiteralVowel
        && (p.k1 == 'a' || p.k1 == 'o' || p.k1 == 'e')
    {
        // 单元音双写：aa→a, oo→o, ee→e
        if k2 == p.k1 {
            return Some(p.k1.to_string());
        }
        if let Some(finals) = scheme.final_map.get(&k2) {
            // 多个韵母可能同为合法零声母音节（用户把 e 和 er 绑同一键时）：
            // 取「最长」合法音节——裸单字母元音 a/o/e 可由双写（aa/ee）或自身键到达，
            // 不应短路更长的 er 等音节。
            let mut best: Option<&String> = None;
            for f in finals {
                if is_valid_syllable_pub(f)
                    && best.is_none_or(|b| f.chars().count() > b.chars().count())
                {
                    best = Some(f);
                }
            }
            if let Some(f) = best {
                return Some(f.clone());
            }
            // 次选：字面首元音 + 韵母键里的字面字符（如小鹤 er：e + r 键字面 = er）
            let lit = format!("{}{}", p.k1, k2);
            if is_valid_syllable_pub(&lit) {
                return Some(lit);
            }
        }
        return None;
    }

    // 有声母路径：声母候选 × 韵母候选，逐个试有效性（多声母笛卡尔积）
    let initials = resolve_initials(scheme, p.k1);
    if let Some(finals) = scheme.final_map.get(&k2) {
        for ini in &initials {
            for f in finals {
                let candidate = format!("{}{}", ini, f);
                if is_valid_syllable_pub(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// 解析一个键对应的声母候选列表（重映射优先，否则键本身）。
/// 顺序即消歧优先级（from_json 保留 JSON 顺序）。
fn resolve_initials(scheme: &ShuangpinScheme, k: char) -> Vec<String> {
    if let Some(remapped) = scheme.initial_remap.get(&k) {
        return remapped.clone();
    }
    vec![k.to_string()]
}

/// OPrefixed 零声母第二键解码。
///
/// `ing` 与 `er` 同为「双拼方案规定但 is_valid_syllable_pub 不直接认可/易歧义」的零声母特例：
/// 标准拼音独立形式是 ying/er，但部分方案规定 `o;`→ing（微软/搜狗/紫光）、`or`→er（微软/搜狗/智能ABC）。
/// 特例放在 final_map 尝试**之后**：先试韵母键本身能否解出合法零声母音节，
/// 失败才落特例——避免误伤 final_map 能解的键（如紫光 r 键 = en，or 应解 en 而非 er）。
/// 此处产出方案字面音节 `ing`；若下游 Trie 需 `ying`，应在归一化层处理（类比 jv→ju）。
fn decode_zero_initial_oprefixed(scheme: &ShuangpinScheme, k2: char) -> Option<String> {
    // 同 decode_pair 的 LiteralVowel 零声母路径：取「最长」合法音节，避免裸元音（e）
    // 短路同键的 er。候选含两种形态：韵母本身（f）与 o 前缀组合（o+f）。
    if let Some(finals) = scheme.final_map.get(&k2) {
        let mut best: Option<String> = None;
        for f in finals {
            for cand in [f.clone(), format!("o{}", f)] {
                if is_valid_syllable_pub(&cand)
                    && best
                        .as_deref()
                        .is_none_or(|b| cand.chars().count() > b.chars().count())
                {
                    best = Some(cand);
                }
            }
        }
        if let Some(b) = best {
            return Some(b);
        }
    }
    if k2 == 'r' {
        return Some("er".to_string());
    } // 微软/搜狗/智能ABC：r 键韵母非零声母音节（uan/iu）
    if k2 == ';' {
        return Some("ing".to_string());
    } // 微软/搜狗/紫光：独立 ing 非标准音节
    if k2 == 'o' {
        return Some("o".to_string());
    } // 单元音 o（o 键 = uo/o，两者都非独立音节）
    None
}

/// decode_buffer 的字符串包装：把音节序列用 ' 拼接成全拼字符串（commit/学习路径用）。
pub fn decode_to_pinyin_string(scheme: &ShuangpinScheme, input: &str) -> Option<String> {
    let syls = decode_buffer(scheme, input)?;
    Some(syls.join(""))
}

/// 双拼 buffer 的 pre-edit 显示：按 2 字符一组插 ' 分隔。
/// "vsgo" → "vs'go"，"vsg" → "vs'g"。不展开成全拼（显示双拼原文）。
///
/// 与全拼 segment_buffer 的语义对应：全拼按音节切分插 '，双拼固定 2 键一组插 '。
/// 已含 ' 的位置重置分组计数（用户手动插入的分隔符不打乱后续分组）；
/// 末尾单键单独成组（与 split_pairs 的切分一致）。
pub fn segment_display(_scheme: &ShuangpinScheme, input: &str) -> String {
    let mut out = String::new();
    let mut in_pair = 0usize; // 当前组已写入的非 ' 字符数（遇 ' 重置）
    for c in input.chars() {
        if c == '\'' {
            in_pair = 0;
            out.push(c);
            continue;
        }
        if in_pair == 2 {
            out.push('\'');
            in_pair = 0;
        }
        out.push(c);
        in_pair += 1;
    }
    out
}

/// from_json 校验结果。
pub struct ValidatedScheme {
    pub scheme: ShuangpinScheme,
    /// 软警告（覆盖率不足等），不阻止使用。
    pub warnings: Vec<String>,
}

/// 方案错误。
#[derive(Debug)]
pub enum SchemeError {
    JsonParse(String),
    InvalidKey(char),
    InvalidFinal(String),
    InvalidInitial(String),
    ErUnencodable,
}

impl std::fmt::Display for SchemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemeError::JsonParse(m) => write!(f, "JSON 解析失败: {}", m),
            SchemeError::InvalidKey(c) => write!(f, "非法键 '{}'", c),
            SchemeError::InvalidFinal(s) => write!(f, "非法韵母 '{}'", s),
            SchemeError::InvalidInitial(s) => write!(f, "非法声母 '{}'", s),
            SchemeError::ErUnencodable => write!(f, "OPrefixed 方案下 er 无法编码"),
        }
    }
}

/// 合法声母集合（pub：双拼管理器浮层 chips 列表以引擎为准）。
pub const VALID_INITIALS: &[&str] = &[
    "b", "p", "m", "f", "d", "t", "n", "l", "g", "k", "h", "j", "q", "x", "r", "z", "c", "s", "y",
    "w", "zh", "ch", "sh",
];

/// 合法韵母集合（含单元音 a/e/o 作零声母；pub：双拼管理器浮层分组以引擎为准，
/// 单韵母/复韵母/鼻韵母/特殊分组在调用方按本表聚合）。
pub const VALID_FINALS: &[&str] = &[
    "a", "e", "o", "i", "u", "v", "ai", "ei", "ao", "ou", "an", "en", "ang", "eng", "er", "ia",
    "ie", "iao", "iu", "ian", "iang", "iong", "ing", "in", "ua", "uo", "ue", "ve", "uai", "ui",
    "uan", "un", "uang", "ong",
];

fn is_valid_initial(s: &str) -> bool {
    VALID_INITIALS.contains(&s)
}
fn is_valid_final(s: &str) -> bool {
    VALID_FINALS.contains(&s)
}

impl ShuangpinScheme {
    /// 手写解析自定义方案 JSON（无 serde 依赖，遵循项目惯例）。
    ///
    /// JSON 格式：
    /// {"id":"...","displayName":"...","finalMap":{"o":["uo","o"],...},
    ///  "initialRemap":{"v":["zh"],...},"zeroInitialStyle":"literalVowel"|"oPrefixed"}
    /// initialRemap 值兼容两种形态：数组（多候选）与旧版单字符串（自动包成数组）。
    pub fn from_json(json: &str) -> Result<ValidatedScheme, SchemeError> {
        let id =
            extract_string_field(json, "id").ok_or(SchemeError::JsonParse("missing id".into()))?;
        let display_name = extract_string_field(json, "displayName")
            .ok_or(SchemeError::JsonParse("missing displayName".into()))?;
        let style_str = extract_string_field(json, "zeroInitialStyle")
            .ok_or(SchemeError::JsonParse("missing zeroInitialStyle".into()))?;
        let zero_initial_style = match style_str.as_str() {
            "literalVowel" => ZeroInitialStyle::LiteralVowel,
            "oPrefixed" => ZeroInitialStyle::OPrefixed,
            "custom" => ZeroInitialStyle::Custom,
            _ => return Err(SchemeError::JsonParse("invalid zeroInitialStyle".into())),
        };

        let final_map_raw = extract_string_array_map(json, "finalMap")
            .ok_or(SchemeError::JsonParse("missing finalMap".into()))?;
        let initial_remap_raw = extract_string_array_map(json, "initialRemap")
            .ok_or(SchemeError::JsonParse("missing initialRemap".into()))?;
        // 零声母记忆码表：字段缺失 = 空表（老方案照常）；仅 custom 风格
        // 要求非空，其他风格带表 = 配置自相矛盾拒绝（防静默忽略掩盖错误）
        let zero_codes_raw = extract_string_map(json, "zeroInitialCodes").unwrap_or_default();
        if zero_initial_style == ZeroInitialStyle::Custom && zero_codes_raw.is_empty() {
            return Err(SchemeError::JsonParse(
                "custom style requires non-empty zeroInitialCodes".into(),
            ));
        }
        if zero_initial_style != ZeroInitialStyle::Custom && !zero_codes_raw.is_empty() {
            return Err(SchemeError::JsonParse(
                "zeroInitialCodes requires zeroInitialStyle=custom".into(),
            ));
        }

        let mut scheme = ShuangpinScheme {
            id,
            display_name,
            is_builtin: false,
            final_map: HashMap::new(),
            initial_remap: HashMap::new(),
            zero_initial_style,
            zero_initial_codes: HashMap::new(),
        };

        // 校验 + 转 HashMap
        for (k, vs) in &final_map_raw {
            let kc = k
                .chars()
                .next()
                .ok_or(SchemeError::JsonParse("empty key".into()))?;
            if !is_valid_key(kc) {
                return Err(SchemeError::InvalidKey(kc));
            }
            let mut validated_vs = Vec::new();
            for v in vs {
                if !is_valid_final(v) {
                    return Err(SchemeError::InvalidFinal(v.clone()));
                }
                validated_vs.push(v.clone());
            }
            if validated_vs.is_empty() {
                return Err(SchemeError::InvalidFinal("empty list".into()));
            }
            scheme.final_map.insert(kc, validated_vs);
        }
        for (k, vs) in &initial_remap_raw {
            let kc = k
                .chars()
                .next()
                .ok_or(SchemeError::JsonParse("empty key".into()))?;
            if !is_valid_key(kc) {
                return Err(SchemeError::InvalidKey(kc));
            }
            let mut validated_vs = Vec::new();
            for v in vs {
                if !is_valid_initial(v) {
                    return Err(SchemeError::InvalidInitial(v.clone()));
                }
                validated_vs.push(v.clone());
            }
            if validated_vs.is_empty() {
                return Err(SchemeError::InvalidInitial("empty list".into()));
            }
            scheme.initial_remap.insert(kc, validated_vs);
        }
        // 零声母码表校验：音节必须是合法的 a/o/e 开头独立音节；码 = 恰好
        // 两个合法键；码冲突（同码两音节）拒绝——反查表歧义无法消歧。
        for (syl, code) in &zero_codes_raw {
            if !is_valid_syllable_pub(syl) || !matches!(syl.chars().next(), Some('a' | 'o' | 'e')) {
                return Err(SchemeError::JsonParse(format!(
                    "zeroInitialCodes 音节非法: {}",
                    syl
                )));
            }
            let chars: Vec<char> = code.chars().collect();
            if chars.len() != 2 || !is_valid_key(chars[0]) || !is_valid_key(chars[1]) {
                return Err(SchemeError::JsonParse(format!(
                    "zeroInitialCodes 编码非法（须两个合法键）: {}",
                    code
                )));
            }
            let key = (chars[0], chars[1]);
            if scheme.zero_initial_codes.contains_key(&key) {
                return Err(SchemeError::JsonParse(format!(
                    "zeroInitialCodes 编码冲突: {}",
                    code
                )));
            }
            scheme.zero_initial_codes.insert(key, syl.clone());
        }

        // OPrefixed 方案 er 必须可编码（模拟解码 er，解不出则拒绝）。
        // 存在任意 "o"+k2 组合能解出 er 即可——不能硬编码 "or"→er：会误伤
        // r 键 ≠ er 的方案（如紫光 r 键 = en，"or" 合法解 "en"、er 在 q 键 "oq"）。
        if zero_initial_style == ZeroInitialStyle::OPrefixed {
            let er_encodable = ('a'..='z').chain(std::iter::once(';')).any(|k2| {
                decode_buffer(&scheme, &format!("o{}", k2)).as_deref()
                    == Some(["er".to_string()].as_slice())
            });
            if !er_encodable {
                return Err(SchemeError::ErUnencodable);
            }
        }

        // 覆盖率软警告
        let mut warnings = Vec::new();

        // 声母重叠软警告（多值模型：重叠仍可解码，仅提示歧义，不阻止保存）
        let remap_entries: Vec<(&char, &Vec<String>)> = scheme.initial_remap.iter().collect();
        for i in 0..remap_entries.len() {
            for j in (i + 1)..remap_entries.len() {
                let (k1, v1) = remap_entries[i];
                let (k2, v2) = remap_entries[j];
                let overlap: Vec<&String> = v1.iter().filter(|a| v2.contains(a)).collect();
                if !overlap.is_empty() {
                    warnings.push(format!(
                        "声母重叠：{} 和 {} 都映射 {}",
                        k1,
                        k2,
                        overlap
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join("/")
                    ));
                }
            }
        }
        let all_finals: std::collections::HashSet<&str> = VALID_FINALS.iter().copied().collect();
        let mapped: std::collections::HashSet<&str> = scheme
            .final_map
            .values()
            .flatten()
            .map(|s| s.as_str())
            .collect();
        let unmapped: Vec<&str> = all_finals.difference(&mapped).copied().collect();
        if !unmapped.is_empty() {
            warnings.push(format!("以下韵母未映射：{}", unmapped.join(", ")));
        }
        // Custom 风格：标准零声母音节（12 个）应全部可编码——缺了打不出
        // （啊/诶/安/昂/奥/额/诶/恩/鞥/儿/哦/欧），软警告逐个点名
        if zero_initial_style == ZeroInitialStyle::Custom {
            let covered: std::collections::HashSet<&str> = scheme
                .zero_initial_codes
                .values()
                .map(|s| s.as_str())
                .collect();
            let missing: Vec<&str> = [
                "a", "ai", "an", "ang", "ao", "e", "ei", "en", "eng", "er", "o", "ou",
            ]
            .iter()
            .copied()
            .filter(|s| !covered.contains(s))
            .collect();
            if !missing.is_empty() {
                warnings.push(format!("以下零声母音节未编码：{}", missing.join(", ")));
            }
        }

        Ok(ValidatedScheme { scheme, warnings })
    }
}

// ── 手写 JSON 字段提取 helper ──────────────────────────

fn extract_string_field(json: &str, field: &str) -> Option<String> {
    let key = format!("\"{}\"", field);
    let idx = json.find(&key)?;
    let after = &json[idx + key.len()..];
    let colon = after.find(':')?;
    let after = &after[colon + 1..];
    let quote_start = after.find('"')?;
    let rest = &after[quote_start + 1..];
    let quote_end = rest.find('"')?;
    Some(rest[..quote_end].to_string())
}

/// 提取 {key: [str,...] | "str", ...} 形式的 map（finalMap/initialRemap 用）。
/// 值兼容两种形态：数组（多候选）与单字符串（旧版单值格式，自动包成数组）。
fn extract_string_array_map(json: &str, field: &str) -> Option<Vec<(String, Vec<String>)>> {
    let key = format!("\"{}\"", field);
    let idx = json.find(&key)?;
    let after = &json[idx + key.len()..];
    let colon = after.find(':')?;
    let after = &after[colon + 1..];
    let brace_start = after.find('{')?;
    let mut depth = 0;
    let mut brace_end = brace_start;
    for (i, c) in after[brace_start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    brace_end = brace_start + i;
                    break;
                }
            }
            _ => {}
        }
    }
    let inner = &after[brace_start + 1..brace_end];
    let mut result = Vec::new();
    let mut pos = 0;
    while pos < inner.len() {
        let qs = match inner[pos..].find('"') {
            Some(i) => i,
            None => break,
        };
        let abs_qs = pos + qs;
        let rest = &inner[abs_qs + 1..];
        let qe = match rest.find('"') {
            Some(i) => i,
            None => break,
        };
        let map_key = rest[..qe].to_string();
        let after_key = &rest[qe + 1..];
        let colon2 = match after_key.find(':') {
            Some(i) => i,
            None => break,
        };
        let value_part = after_key[colon2 + 1..].trim_start();
        let mut vals: Vec<String> = Vec::new();
        if let Some(v) = value_part.strip_prefix('[') {
            // 数组形态：[ "a", "b" ]
            let end = match v.find(']') {
                Some(i) => i,
                None => break,
            };
            vals = v[..end]
                .split(',')
                .map(|s| s.trim().trim_matches('"').to_string())
                .filter(|s| !s.is_empty())
                .collect();
            result.push((map_key, vals));
            pos = abs_qs + 1 + qe + 1 + colon2 + 1 + end + 2;
        } else if let Some(vrest) = value_part.strip_prefix('"') {
            // 旧版单字符串形态："zh"
            let vend = match vrest.find('"') {
                Some(i) => i,
                None => break,
            };
            vals.push(vrest[..vend].to_string());
            result.push((map_key, vals));
            pos = abs_qs + 1 + qe + 1 + colon2 + 1 + vend + 2;
        } else {
            break;
        }
    }
    Some(result)
}

/// 提取 {key: "value", ...} 形式的字符串→字符串 map（zeroInitialCodes 用）。
/// 字段缺失返回 None；空对象返回 Some(vec![])。
fn extract_string_map(json: &str, field: &str) -> Option<Vec<(String, String)>> {
    let key = format!("\"{}\"", field);
    let idx = json.find(&key)?;
    let after = &json[idx + key.len()..];
    let colon = after.find(':')?;
    let after = &after[colon + 1..];
    let brace_start = after.find('{')?;
    let mut depth = 0;
    let mut brace_end = brace_start;
    for (i, c) in after[brace_start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    brace_end = brace_start + i;
                    break;
                }
            }
            _ => {}
        }
    }
    let inner = &after[brace_start + 1..brace_end];
    let mut result = Vec::new();
    let mut pos = 0;
    while pos < inner.len() {
        let qs = match inner[pos..].find('"') {
            Some(i) => i,
            None => break,
        };
        let qe = match inner[pos + qs + 1..].find('"') {
            Some(i) => pos + qs + 1 + i,
            None => break,
        };
        let k = inner[pos + qs + 1..qe].to_string();
        let colon = match inner[qe + 1..].find(':') {
            Some(i) => qe + 1 + i,
            None => break,
        };
        let vs = match inner[colon + 1..].find('"') {
            Some(i) => i,
            None => break,
        };
        let ve = match inner[colon + 1 + vs + 1..].find('"') {
            Some(i) => colon + 1 + vs + 1 + i,
            None => break,
        };
        let v = inner[colon + 1 + vs + 1..ve].to_string();
        result.push((k, v));
        pos = ve + 1;
    }
    Some(result)
}

/// validate_json：返回 warnings JSON 数组字符串（fatal 错误返回 None）。编辑器实时校验用。
pub fn validate_json(json: &str) -> Option<String> {
    match ShuangpinScheme::from_json(json) {
        Ok(v) => {
            let items: Vec<String> = v
                .warnings
                .iter()
                .map(|w| format!("\"{}\"", w.replace('"', "\\\"")))
                .collect();
            Some(format!("[{}]", items.join(",")))
        }
        Err(_) => None,
    }
}

/// 手写序列化（无 serde 依赖，与 from_json 同惯例）。
/// 输出与 Swift `ShuangpinSchemeData` Codable 的 key 一致（camelCase）：
/// {"id","displayName","finalMap","initialRemap","zeroInitialStyle"}。
/// finalMap/initialRemap 值为多值数组（顺序即消歧优先级）。
fn json_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '"' => "\\\"".to_string(),
            '\\' => "\\\\".to_string(),
            c => c.to_string(),
        })
        .collect()
}

pub fn scheme_to_json(s: &ShuangpinScheme) -> String {
    let final_map: String = {
        let mut entries: Vec<String> = s
            .final_map
            .iter()
            .map(|(k, vs)| {
                let vals: Vec<String> = vs.iter().map(|v| format!("\"{}\"", v)).collect();
                format!("\"{}\":[{}]", k, vals.join(","))
            })
            .collect();
        entries.sort(); // 确定性输出（HashMap 无序）
        entries.join(",")
    };
    let initial_remap: String = {
        let mut entries: Vec<String> = s
            .initial_remap
            .iter()
            .map(|(k, vs)| {
                let vals: Vec<String> = vs.iter().map(|v| format!("\"{}\"", v)).collect();
                format!("\"{}\":[{}]", k, vals.join(","))
            })
            .collect();
        entries.sort();
        entries.join(",")
    };
    let style = match s.zero_initial_style {
        ZeroInitialStyle::LiteralVowel => "literalVowel",
        ZeroInitialStyle::OPrefixed => "oPrefixed",
        ZeroInitialStyle::Custom => "custom",
    };
    // Custom 风格补记忆码表（排序保确定性输出）；其他风格省略字段
    // （与既有自定义 JSON 形态兼容，from_json 容忍缺失）
    let zero_codes = if s.zero_initial_style == ZeroInitialStyle::Custom {
        let mut entries: Vec<String> = s
            .zero_initial_codes
            .iter()
            .map(|((k1, k2), syl)| format!("\"{}\":\"{}{}\"", json_escape(syl), k1, k2))
            .collect();
        entries.sort();
        format!(",\"zeroInitialCodes\":{{{}}}", entries.join(","))
    } else {
        String::new()
    };
    format!(
        "{{\"id\":\"{}\",\"displayName\":\"{}\",\"finalMap\":{{{}}},\"initialRemap\":{{{}}},\"zeroInitialStyle\":\"{}\"{}}}",
        json_escape(&s.id),
        json_escape(&s.display_name),
        final_map,
        initial_remap,
        style,
        zero_codes
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── er 不被同键的裸元音 e 短路（回归守护）──

    #[test]
    fn test_er_not_shadowed_by_e_same_key() {
        // 用户把 er 和 e 绑到同一个键（如 q 键 = [e, er]）。
        // 旧实现：零声母解码循环返回「第一个」合法音节 → e（合法）短路 → er 永远到不了。
        // 修复：零声母路径取「最长」合法音节——裸单字母元音 a/o/e 可由双写或自身键到达，
        // 不应遮蔽更长的 er。验证两种零声母策略。
        use std::collections::HashMap;

        // LiteralVowel（小鹤系）：eq 应解出 er 而非 e
        let mut fm_lv = HashMap::new();
        fm_lv.insert('q', vec!["e".to_string(), "er".to_string()]);
        let lv = ShuangpinScheme {
            id: "t-lv".into(),
            display_name: "t".into(),
            is_builtin: false,
            final_map: fm_lv,
            initial_remap: HashMap::new(),
            zero_initial_style: ZeroInitialStyle::LiteralVowel,
            zero_initial_codes: HashMap::new(),
        };
        assert_eq!(
            decode_buffer(&lv, "eq"),
            Some(vec!["er".to_string()]),
            "LiteralVowel：q=[e,er] 输入 eq 应解出 er（不被 e 短路）"
        );

        // OPrefixed（微软系）：oq 应解出 er 而非 e
        let mut fm_op = HashMap::new();
        fm_op.insert('q', vec!["e".to_string(), "er".to_string()]);
        let op = ShuangpinScheme {
            id: "t-op".into(),
            display_name: "t".into(),
            is_builtin: false,
            final_map: fm_op,
            initial_remap: HashMap::new(),
            zero_initial_style: ZeroInitialStyle::OPrefixed,
            zero_initial_codes: HashMap::new(),
        };
        assert_eq!(
            decode_buffer(&op, "oq"),
            Some(vec!["er".to_string()]),
            "OPrefixed：q=[e,er] 输入 oq 应解出 er（不被 e 短路）"
        );
    }

    #[test]
    fn test_xiaohe_basic_decode() {
        let s = ShuangpinScheme::builtin_by_id("xiaohe").unwrap();
        // vs = zh+ong = zhong（小鹤：s 键=[iong,ong]）；vo 才是 zh+uo=zhuo
        assert_eq!(decode_buffer(s, "vs"), Some(vec!["zhong".to_string()]));
        assert_eq!(
            decode_buffer(s, "vsgo"),
            Some(vec!["zhong".to_string(), "guo".to_string()])
        );
        assert_eq!(decode_buffer(s, "ni"), Some(vec!["ni".to_string()]));
        // nihc = ni + hao（小鹤 c 键=ao），验证多音节解码
        assert_eq!(
            decode_buffer(s, "nihc"),
            Some(vec!["ni".to_string(), "hao".to_string()])
        );
    }

    #[test]
    fn test_xiaohe_incomplete_trailing_initial() {
        let s = ShuangpinScheme::builtin_by_id("xiaohe").unwrap();
        assert_eq!(
            decode_buffer(s, "vsg"),
            Some(vec!["zhong".to_string(), "g".to_string()])
        );
        assert_eq!(decode_buffer(s, "v"), Some(vec!["zh".to_string()]));
    }

    #[test]
    fn test_xiaohe_separator() {
        let s = ShuangpinScheme::builtin_by_id("xiaohe").unwrap();
        // vs'go = zhong + guo（vo 才是 zhuo）
        assert_eq!(
            decode_buffer(s, "vs'go"),
            Some(vec!["zhong".to_string(), "guo".to_string()])
        );
    }

    #[test]
    fn test_xiaohe_zero_initial() {
        let s = ShuangpinScheme::builtin_by_id("xiaohe").unwrap();
        assert_eq!(decode_buffer(s, "aa"), Some(vec!["a".to_string()]));
        assert_eq!(decode_buffer(s, "oo"), Some(vec!["o".to_string()]));
        assert_eq!(decode_buffer(s, "ad"), Some(vec!["ai".to_string()]));
        assert_eq!(decode_buffer(s, "aj"), Some(vec!["an".to_string()]));
        assert_eq!(decode_buffer(s, "ah"), Some(vec!["ang".to_string()]));
        assert_eq!(decode_buffer(s, "er"), Some(vec!["er".to_string()]));
    }

    #[test]
    fn test_microsoft_er_special() {
        let s = ShuangpinScheme::builtin_by_id("microsoft").unwrap();
        assert_eq!(decode_buffer(s, "or"), Some(vec!["er".to_string()]));
        assert_eq!(decode_buffer(s, "vr"), Some(vec!["zhuan".to_string()]));
    }

    #[test]
    fn test_microsoft_zero_initial_oprefixed() {
        let s = ShuangpinScheme::builtin_by_id("microsoft").unwrap();
        assert_eq!(decode_buffer(s, "oa"), Some(vec!["a".to_string()]));
        assert_eq!(decode_buffer(s, "oo"), Some(vec!["o".to_string()]));
        assert_eq!(decode_buffer(s, "ol"), Some(vec!["ai".to_string()]));
        assert_eq!(decode_buffer(s, "oj"), Some(vec!["an".to_string()]));
        assert_eq!(decode_buffer(s, "og"), Some(vec!["eng".to_string()]));
    }

    #[test]
    fn test_microsoft_semicolon_key() {
        let s = ShuangpinScheme::builtin_by_id("microsoft").unwrap();
        assert_eq!(decode_buffer(s, "o;"), Some(vec!["ing".to_string()]));
        assert_eq!(decode_buffer(s, "x;"), Some(vec!["xing".to_string()]));
    }

    #[test]
    fn test_sogou_same_as_microsoft() {
        let sg = ShuangpinScheme::builtin_by_id("sogou").unwrap();
        let ms = ShuangpinScheme::builtin_by_id("microsoft").unwrap();
        for inp in &["or", "oa", "x;", "vsgo"] {
            assert_eq!(
                decode_buffer(sg, inp),
                decode_buffer(ms, inp),
                "diverge on {}",
                inp
            );
        }
    }

    #[test]
    fn test_yu_syllable_all_schemes() {
        // 守护每套方案的 yu 编码：声母 y + ü 键的规范音节是 "yv"（雨/语/鱼/
        // 与/玉/于/遇/预/育…高频字），由 normalize_syllable 归一化 "yv"→"yu"。
        // 本测试逐套守护。各方案 ü 所在键：小鹤/自然码/加加/ABC v 键、微软/搜狗 y 键、紫光 x 键。
        let cases: [(&str, &str); 7] = [
            ("xiaohe", "yv"),    // 小鹤 v 键含 ü
            ("ziranma", "yv"),   // 自然码 v 键含 ü
            ("microsoft", "yy"), // 微软 y 键=[uai,ü]
            ("sogou", "yy"),     // 搜狗同微软
            ("abc", "yv"),       // ABC v 键=ü
            ("pyjj", "yv"),      // 拼音加加 v 键含 ü
            ("ziguang", "yx"),   // 紫光 x 键=[ü,uai]
        ];
        for (id, code) in cases {
            let s = ShuangpinScheme::builtin_by_id(id).unwrap();
            let decoded = decode_buffer(s, code)
                .unwrap_or_else(|| panic!("方案 {} 的 {} 应解码成功", id, code));
            // decode 层返回原始音节（ü 的代码写法 v）；engine 层由
            // normalize_syllable 归一化 "yv"→"yu"（process_shuangpin_inner 入口）
            assert_eq!(
                decoded,
                vec!["yv".to_string()],
                "方案 {} 的 {} 解码结果",
                id,
                code
            );
            assert_eq!(
                crate::pinyin::normalize_syllable(&decoded[0]),
                "yu",
                "方案 {} 的 {} 归一化后应为 yu",
                id,
                code
            );
        }
    }

    // ── 后续扩充 3 套方案 ──

    #[test]
    fn test_abc_basic_decode() {
        let s = ShuangpinScheme::builtin_by_id("abc").unwrap();
        // zh→a 键：as = zh+ong = zhong；ch→e 键：et = ch+uang = chuang；sh→v 键：vh = sh+ang
        assert_eq!(decode_buffer(s, "as"), Some(vec!["zhong".to_string()]));
        assert_eq!(decode_buffer(s, "et"), Some(vec!["chuang".to_string()]));
        assert_eq!(decode_buffer(s, "vh"), Some(vec!["shang".to_string()]));
        assert_eq!(
            decode_buffer(s, "asgo"),
            Some(vec!["zhong".to_string(), "guo".to_string()])
        );
    }

    #[test]
    fn test_abc_zero_initial_oprefixed() {
        let s = ShuangpinScheme::builtin_by_id("abc").unwrap();
        assert_eq!(decode_buffer(s, "oa"), Some(vec!["a".to_string()]));
        assert_eq!(decode_buffer(s, "oo"), Some(vec!["o".to_string()]));
        assert_eq!(decode_buffer(s, "ol"), Some(vec!["ai".to_string()]));
        assert_eq!(decode_buffer(s, "oj"), Some(vec!["an".to_string()]));
        assert_eq!(decode_buffer(s, "oh"), Some(vec!["ang".to_string()]));
        // er 在 r 键（er/iu）：or → 既有 r→er 特例
        assert_eq!(decode_buffer(s, "or"), Some(vec!["er".to_string()]));
        // 零声母 ing 无 o 前缀编码（拼写 ying）：oy 无效，yy = y+ing = ying
        assert_eq!(decode_buffer(s, "yy"), Some(vec!["ying".to_string()]));
        assert_eq!(decode_buffer(s, "oy"), None);
    }

    #[test]
    fn test_pyjj_basic_decode() {
        let s = ShuangpinScheme::builtin_by_id("pyjj").unwrap();
        // v→zh, i→sh, u→ch（与常见布局相反！）：vy=zhong, uh=chuang, ii=shi
        assert_eq!(decode_buffer(s, "vy"), Some(vec!["zhong".to_string()]));
        assert_eq!(decode_buffer(s, "uh"), Some(vec!["chuang".to_string()]));
        assert_eq!(decode_buffer(s, "ii"), Some(vec!["shi".to_string()]));
        assert_eq!(
            decode_buffer(s, "vygo"),
            Some(vec!["zhong".to_string(), "guo".to_string()])
        );
    }

    #[test]
    fn test_pyjj_zero_initial_literal() {
        let s = ShuangpinScheme::builtin_by_id("pyjj").unwrap();
        // a/e 双写 + 韵母键：as=ai, af=an, ad=ao, aw=ei, ar=en, at=eng, ag=ang
        assert_eq!(decode_buffer(s, "aa"), Some(vec!["a".to_string()]));
        assert_eq!(decode_buffer(s, "as"), Some(vec!["ai".to_string()]));
        assert_eq!(decode_buffer(s, "af"), Some(vec!["an".to_string()]));
        assert_eq!(decode_buffer(s, "ad"), Some(vec!["ao".to_string()]));
        assert_eq!(decode_buffer(s, "aw"), Some(vec!["ei".to_string()]));
        assert_eq!(decode_buffer(s, "ar"), Some(vec!["en".to_string()]));
        assert_eq!(decode_buffer(s, "at"), Some(vec!["eng".to_string()]));
        assert_eq!(decode_buffer(s, "ag"), Some(vec!["ang".to_string()]));
        assert_eq!(decode_buffer(s, "oo"), Some(vec!["o".to_string()]));
        // er = eq（q 键 = er/ing）；ying = y+q
        assert_eq!(decode_buffer(s, "eq"), Some(vec!["er".to_string()]));
        assert_eq!(decode_buffer(s, "yq"), Some(vec!["ying".to_string()]));
    }

    #[test]
    fn test_ziguang_basic_decode() {
        let s = ShuangpinScheme::builtin_by_id("ziguang").unwrap();
        // zh→a 键：ax = zh+uai = zhuai；ch→u 键：uh = ch+uang = chuang；sh→i 键：iv = sh+ui = shui
        assert_eq!(decode_buffer(s, "ax"), Some(vec!["zhuai".to_string()]));
        assert_eq!(decode_buffer(s, "uh"), Some(vec!["chuang".to_string()]));
        assert_eq!(decode_buffer(s, "iv"), Some(vec!["shui".to_string()]));
        assert_eq!(decode_buffer(s, "yy"), Some(vec!["yong".to_string()]));
    }

    #[test]
    fn test_ziguang_zero_initial_oprefixed() {
        let s = ShuangpinScheme::builtin_by_id("ziguang").unwrap();
        assert_eq!(decode_buffer(s, "oa"), Some(vec!["a".to_string()]));
        assert_eq!(decode_buffer(s, "oo"), Some(vec!["o".to_string()]));
        // 紫光键位：ai=s 键, an=f 键, ang=g 键, ei=w 键, en=r 键
        assert_eq!(decode_buffer(s, "os"), Some(vec!["ai".to_string()]));
        assert_eq!(decode_buffer(s, "of"), Some(vec!["an".to_string()]));
        assert_eq!(decode_buffer(s, "og"), Some(vec!["ang".to_string()]));
        assert_eq!(decode_buffer(s, "ow"), Some(vec!["ei".to_string()]));
        // 紫光 r 键 = en（非微软 uan）：or 必须解 en，不被 r→er 特例误伤
        assert_eq!(decode_buffer(s, "or"), Some(vec!["en".to_string()]));
        // er = oq（q 键 = er）；ing 在分号键：o; → 既有 ';' 特例
        assert_eq!(decode_buffer(s, "oq"), Some(vec!["er".to_string()]));
        assert_eq!(decode_buffer(s, "o;"), Some(vec!["ing".to_string()]));
        assert_eq!(decode_buffer(s, "x;"), Some(vec!["xing".to_string()]));
    }

    #[test]
    fn test_ziranma_zero_initial() {
        let s = ShuangpinScheme::builtin_by_id("ziranma").unwrap();
        assert_eq!(decode_buffer(s, "aa"), Some(vec!["a".to_string()]));
        assert_eq!(decode_buffer(s, "al"), Some(vec!["ai".to_string()]));
        assert_eq!(decode_buffer(s, "aj"), Some(vec!["an".to_string()]));
    }

    #[test]
    fn test_invalid_char_returns_none() {
        let s = ShuangpinScheme::builtin_by_id("xiaohe").unwrap();
        assert_eq!(decode_buffer(s, "vs1go"), None);
    }

    #[test]
    fn test_empty_returns_none() {
        let s = ShuangpinScheme::builtin_by_id("xiaohe").unwrap();
        assert_eq!(decode_buffer(s, ""), None);
    }

    #[test]
    fn test_xiaohe_literal_form_compat() {
        let s = ShuangpinScheme::builtin_by_id("xiaohe").unwrap();
        assert_eq!(decode_buffer(s, "ai"), Some(vec!["ai".to_string()]));
        assert_eq!(decode_buffer(s, "an"), Some(vec!["an".to_string()]));
    }

    #[test]
    fn test_from_json_valid_scheme() {
        let json = r#"{"id":"custom-1","displayName":"测试","finalMap":{"o":["uo","o"]},"initialRemap":{"v":"zh"},"zeroInitialStyle":"literalVowel"}"#;
        let result = ShuangpinScheme::from_json(json);
        assert!(result.is_ok(), "should parse: {:?}", result.err());
        let validated = result.unwrap();
        assert_eq!(validated.scheme.id, "custom-1");
        assert_eq!(validated.scheme.display_name, "测试");
        assert!(
            validated.warnings.is_empty()
                || validated.warnings.iter().any(|w| w.contains("未映射"))
        );
    }

    #[test]
    fn test_xiaohe_template_json_passes_validation() {
        // 小鹤模板 JSON（含 m→ian、k→ing/uai、v→ui/v）必须通过校验：
        // 守护 VALID_FINALS 覆盖 ian（缺失会让此类模板保存失败）
        let json = r#"{"id":"custom-1","displayName":"小鹤模板","finalMap":{"a":["a"],"b":["in"],"c":["ao"],"d":["ai"],"e":["e"],"f":["en"],"g":["eng"],"h":["ang"],"i":["i"],"j":["an"],"k":["ing","uai"],"l":["iang","uang"],"m":["ian"],"n":["iao"],"o":["uo","o"],"p":["ie"],"q":["iu"],"r":["uan"],"s":["iong","ong"],"t":["ue","ve"],"u":["u"],"v":["ui","v"],"w":["ei"],"x":["ia","ua"],"y":["un"],"z":["ou"]},"initialRemap":{"v":["zh"],"i":["ch"],"u":["sh"]},"zeroInitialStyle":"literalVowel"}"#;
        let v = ShuangpinScheme::from_json(json);
        assert!(v.is_ok(), "xiaohe template must validate: {:?}", v.err());
    }

    #[test]
    fn test_from_json_invalid_key() {
        let json = r#"{"id":"x","displayName":"x","finalMap":{"1":["a"]},"initialRemap":{},"zeroInitialStyle":"literalVowel"}"#;
        assert!(ShuangpinScheme::from_json(json).is_err());
    }

    #[test]
    fn test_multi_initial_decode() {
        // x 键双声母：xo → ch+uo = chuo（ch 优先于 sh）
        let json = r#"{"id":"x","displayName":"x","finalMap":{"o":["uo","o"]},"initialRemap":{"x":["ch","sh"]},"zeroInitialStyle":"literalVowel"}"#;
        let v = ShuangpinScheme::from_json(json).expect("valid");
        assert_eq!(
            decode_buffer(&v.scheme, "xo"),
            Some(vec!["chuo".to_string()])
        );
        // 单键组输出首个声母候选
        assert_eq!(decode_buffer(&v.scheme, "x"), Some(vec!["ch".to_string()]));
    }

    #[test]
    fn test_multi_initial_with_multi_final() {
        // 双声母 × 双韵母：xi → ch+i = chi（首声母首选韵母胜出）；sh+ing 非法被跳过
        let json = r#"{"id":"x","displayName":"x","finalMap":{"i":["i","ing"]},"initialRemap":{"x":["ch","sh"]},"zeroInitialStyle":"literalVowel"}"#;
        let v = ShuangpinScheme::from_json(json).expect("valid");
        assert_eq!(
            decode_buffer(&v.scheme, "xi"),
            Some(vec!["chi".to_string()])
        );
        assert_eq!(decode_buffer(&v.scheme, "x"), Some(vec!["ch".to_string()]));
    }

    #[test]
    fn test_initial_remap_old_single_value_json() {
        // 旧版单值格式 {"v":"zh"} 兼容：解析成功且值为 ["zh"]
        let json = r#"{"id":"x","displayName":"x","finalMap":{"o":["uo","o"]},"initialRemap":{"v":"zh"},"zeroInitialStyle":"literalVowel"}"#;
        let v = ShuangpinScheme::from_json(json).expect("old single-value format should parse");
        assert_eq!(
            v.scheme.initial_remap.get(&'v'),
            Some(&vec!["zh".to_string()])
        );
        assert_eq!(
            decode_buffer(&v.scheme, "vo"),
            Some(vec!["zhuo".to_string()])
        );
    }

    #[test]
    fn test_initial_overlap_warns_not_errors() {
        // 两键都含 zh：可解析（软警告），不再是硬错误
        let json = r#"{"id":"x","displayName":"x","finalMap":{"o":["uo"]},"initialRemap":{"v":["zh"],"x":["zh"]},"zeroInitialStyle":"literalVowel"}"#;
        let v = ShuangpinScheme::from_json(json);
        assert!(v.is_ok(), "overlap must not be fatal: {:?}", v.err());
        let v = v.unwrap();
        assert!(
            v.warnings.iter().any(|w| w.contains("声母重叠")),
            "warnings: {:?}",
            v.warnings
        );
    }

    #[test]
    fn test_initial_overlap_decode_works() {
        let json = r#"{"id":"x","displayName":"x","finalMap":{"o":["uo"]},"initialRemap":{"v":["zh"],"x":["zh"]},"zeroInitialStyle":"literalVowel"}"#;
        let v = ShuangpinScheme::from_json(json).unwrap();
        assert_eq!(
            decode_buffer(&v.scheme, "vo"),
            Some(vec!["zhuo".to_string()])
        );
        assert_eq!(
            decode_buffer(&v.scheme, "xo"),
            Some(vec!["zhuo".to_string()])
        );
    }

    #[test]
    fn test_empty_initial_list_rejected() {
        let json = r#"{"id":"x","displayName":"x","finalMap":{},"initialRemap":{"v":[]},"zeroInitialStyle":"literalVowel"}"#;
        assert!(ShuangpinScheme::from_json(json).is_err());
    }

    #[test]
    fn test_from_json_invalid_final_value() {
        let json = r#"{"id":"x","displayName":"x","finalMap":{"o":["xyz"]},"initialRemap":{},"zeroInitialStyle":"literalVowel"}"#;
        assert!(ShuangpinScheme::from_json(json).is_err());
    }

    #[test]
    fn test_from_json_malformed() {
        assert!(ShuangpinScheme::from_json("not json").is_err());
        assert!(ShuangpinScheme::from_json("").is_err());
    }

    #[test]
    fn test_scheme_to_json_roundtrip_all_builtins() {
        for s in ShuangpinScheme::builtins() {
            let json = scheme_to_json(s);
            let parsed = ShuangpinScheme::from_json(&json).expect("builtin json must parse");
            assert_eq!(parsed.scheme.id, s.id);
            assert_eq!(parsed.scheme.display_name, s.display_name);
            assert_eq!(parsed.scheme.final_map, s.final_map);
            assert_eq!(parsed.scheme.initial_remap, s.initial_remap);
            assert_eq!(parsed.scheme.zero_initial_style, s.zero_initial_style);
        }
    }

    #[test]
    fn test_from_json_oprefixed_er_unencodable_rejected() {
        // OPrefixed 方案 er 必须能编码。本例 r 键 = en（遮挡 r→er 特例，
        // 模拟紫光键位），且无任何 "o"+k2 组合解出 er（紫光靠 q 键 "oq" 才合法）→ 拒绝。
        let json = r#"{"id":"x","displayName":"x","finalMap":{"a":["a"],"e":["e"],"o":["uo","o"],"r":["en"]},"initialRemap":{},"zeroInitialStyle":"oPrefixed"}"#;
        assert!(
            ShuangpinScheme::from_json(json).is_err(),
            "er unencodable must be rejected"
        );
    }

    // ── 首道双拼（第 8 套预制）──

    #[test]
    fn test_shoudao_basic_decode() {
        let s = ShuangpinScheme::builtin_by_id("shoudao").unwrap();
        // 示例用例：首=ES（sh+ou）道=DD（d+ao）
        assert_eq!(decode_buffer(s, "es"), Some(vec!["shou".to_string()]));
        assert_eq!(decode_buffer(s, "dd"), Some(vec!["dao".to_string()]));
        // zh→V / ch→I / sh→E（e 键兼 sh 声母与 e 韵母，分槽无冲突）
        assert_eq!(decode_buffer(s, "vh"), Some(vec!["zhong".to_string()]));
        assert_eq!(decode_buffer(s, "ii"), Some(vec!["chi".to_string()]));
        assert_eq!(decode_buffer(s, "ee"), Some(vec!["she".to_string()]));
        // 多值韵母键消歧：g=[ing,uai] h=[ong,iong] l=[ai,ue] k=[en,ia] v=[v,ui]
        assert_eq!(decode_buffer(s, "gg"), Some(vec!["guai".to_string()]));
        assert_eq!(decode_buffer(s, "yg"), Some(vec!["ying".to_string()]));
        assert_eq!(decode_buffer(s, "jh"), Some(vec!["jiong".to_string()]));
        assert_eq!(decode_buffer(s, "vk"), Some(vec!["zhen".to_string()]));
        assert_eq!(decode_buffer(s, "jk"), Some(vec!["jia".to_string()]));
        assert_eq!(decode_buffer(s, "vv"), Some(vec!["zhui".to_string()]));
        // y/w 声母照常：yi/wu/yang/yong/weng
        assert_eq!(decode_buffer(s, "yi"), Some(vec!["yi".to_string()]));
        assert_eq!(decode_buffer(s, "wu"), Some(vec!["wu".to_string()]));
        assert_eq!(decode_buffer(s, "yy"), Some(vec!["yang".to_string()]));
        assert_eq!(decode_buffer(s, "yh"), Some(vec!["yong".to_string()]));
        assert_eq!(decode_buffer(s, "wf"), Some(vec!["weng".to_string()]));
        // 连打
        assert_eq!(
            decode_buffer(s, "esdd"),
            Some(vec!["shou".to_string(), "dao".to_string()])
        );
    }

    #[test]
    fn test_shoudao_zero_initial_custom_codes() {
        let s = ShuangpinScheme::builtin_by_id("shoudao").unwrap();
        // 零声母记忆码表全 12 项
        for (code, syl) in [
            ("aa", "a"),
            ("ai", "ai"),
            ("an", "an"),
            ("ay", "ang"),
            ("ao", "ao"),
            ("ue", "e"),
            ("ui", "ei"),
            ("en", "en"),
            ("uf", "eng"),
            ("er", "er"),
            ("oo", "o"),
            ("ou", "ou"),
        ] {
            assert_eq!(
                decode_buffer(s, code),
                Some(vec![syl.to_string()]),
                "零声母 {} 应解码为 {}",
                code,
                syl
            );
        }
    }

    /// 正交性（首道设计的精妙处，勿破坏）：e 家族零声母码与 sh 声母路径共存——
    /// "ei" 是 sh+i=shi 而"诶 ei 音节"打 "ui"；"ue" 是零声母 e 而 u 非声母
    /// 本就无常规解；"en/er" 表优先（常规 sh+ian/sh+ie 解出无效音节）。
    #[test]
    fn test_shoudao_code_orthogonality() {
        let s = ShuangpinScheme::builtin_by_id("shoudao").unwrap();
        assert_eq!(
            decode_buffer(s, "ei"),
            Some(vec!["shi".to_string()]),
            "E=sh I=i"
        );
        assert_eq!(
            decode_buffer(s, "ui"),
            Some(vec!["ei".to_string()]),
            "表优先：诶"
        );
        assert_eq!(
            decode_buffer(s, "ue"),
            Some(vec!["e".to_string()]),
            "表优先：额"
        );
        assert_eq!(
            decode_buffer(s, "en"),
            Some(vec!["en".to_string()]),
            "shian 无效让位表"
        );
        assert_eq!(
            decode_buffer(s, "er"),
            Some(vec!["er".to_string()]),
            "shie 无效让位表"
        );
        // ü 双轨：nüe/lüe 走 B 键（ve），jue/que/xue 走 L 键（ue）
        assert_eq!(decode_buffer(s, "nb"), Some(vec!["nve".to_string()]));
        assert_eq!(decode_buffer(s, "lb"), Some(vec!["lve".to_string()]));
        assert_eq!(decode_buffer(s, "jl"), Some(vec!["jue".to_string()]));
        assert_eq!(decode_buffer(s, "ql"), Some(vec!["que".to_string()]));
        assert_eq!(decode_buffer(s, "xl"), Some(vec!["xue".to_string()]));
        // jv/qv/xv/yv（ü 直拼）与现有 jv→ju 归一化层同轨
        assert_eq!(decode_buffer(s, "jv"), Some(vec!["jv".to_string()]));
        assert_eq!(decode_buffer(s, "yv"), Some(vec!["yv".to_string()]));
    }

    /// 首道 JSON 往返：scheme_to_json 必须带 zeroInitialCodes，from_json 还原
    /// 码表；老格式（无字段 + 非 custom 风格）照常解析（向后兼容）。
    #[test]
    fn test_shoudao_json_roundtrip_and_legacy_compat() {
        let s = ShuangpinScheme::builtin_by_id("shoudao").unwrap();
        let json = scheme_to_json(s);
        assert!(
            json.contains("zeroInitialCodes"),
            "custom 风格必须序列化码表: {}",
            json
        );
        let parsed = ShuangpinScheme::from_json(&json).expect("往返解析");
        assert_eq!(parsed.scheme.zero_initial_style, ZeroInitialStyle::Custom);
        assert_eq!(parsed.scheme.zero_initial_codes, s.zero_initial_codes);
        // 老格式（既有自定义方案无 zeroInitialCodes 字段）不回归
        let legacy = r#"{"id":"c1","displayName":"旧","finalMap":{"a":["a"]},"initialRemap":{"v":["zh"]},"zeroInitialStyle":"literalVowel"}"#;
        assert!(ShuangpinScheme::from_json(legacy).is_ok());
        // 风格与码表错配拒绝（防静默忽略掩盖配置错误）
        let mismatched = r#"{"id":"c2","displayName":"错配","finalMap":{"a":["a"]},"initialRemap":{},"zeroInitialStyle":"literalVowel","zeroInitialCodes":{"a":"aa"}}"#;
        assert!(ShuangpinScheme::from_json(mismatched).is_err());
        // custom 空码表拒绝；码表非法值拒绝
        let empty = r#"{"id":"c3","displayName":"空表","finalMap":{"a":["a"]},"initialRemap":{},"zeroInitialStyle":"custom","zeroInitialCodes":{}}"#;
        assert!(ShuangpinScheme::from_json(empty).is_err());
        let bad_code = r#"{"id":"c4","displayName":"坏码","finalMap":{"a":["a"]},"initialRemap":{},"zeroInitialStyle":"custom","zeroInitialCodes":{"a":"aaa"}}"#;
        assert!(ShuangpinScheme::from_json(bad_code).is_err());
        let bad_syl = r#"{"id":"c5","displayName":"坏音节","finalMap":{"a":["a"]},"initialRemap":{},"zeroInitialStyle":"custom","zeroInitialCodes":{"ba":"ba"}}"#;
        assert!(
            ShuangpinScheme::from_json(bad_syl).is_err(),
            "ba 非零声母音节"
        );
    }
}
