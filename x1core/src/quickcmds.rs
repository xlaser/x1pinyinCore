//! quickcmds —— 斜杠命令族（/upper 数字转中文、/emoj 表情输入）
//!
//! 定位：`/` 前缀快捷输入的引擎侧命令（与 symbols.rs 的 /fh 同族）。DLL 的
//! 斜杠模式把整个命令串（含数字，如 "/upper1360"）经 process 送引擎，
//! `Engine::process` 在 try_quick_input 之后调本模块 [`dispatch`]。
//!
//! 命令语义：
//!   `/upper<数字>`   → 双候选：中文小写（一千三百六十）+ 财务大写（壹仟叁佰陆拾）
//!   `/calc<算式>`    → 快捷计算器：结果 + 「算式=结果」双候选
//!   `/emoj[拼音]`    → Emoji 候选（拼音前缀过滤；空关键字 = 常用精选）
//!     规模偏好 emojiScale（common/full，默认 common）由服务端 config 总线
//!     注入 Engine::emoji_full —— 常用 = 表中 common 标记条目；全量 = 全表。
//!
//! 约束：纯本地数据与计算，零网络；表数据与转换函数全部纯静态/
//! 纯函数，测试覆盖边界（零折叠/十位特例/万位进位）。

use crate::dict::Candidate;

/// 快捷命令分发。input 形如 "/upper1360"、"/upper1360.5"、"/emojxiao"（DLL
/// 保证以 '/' 开头，空串不会进来）。无匹配返回 None（走正常拼音路径）。
pub fn dispatch(input: &str, emoji_full: bool) -> Option<Vec<Candidate>> {
    let key = input.strip_prefix('/')?;
    if let Some(num) = key.strip_prefix("upper") {
        return upper_candidates(num);
    }
    if let Some(expr) = key.strip_prefix("calc") {
        return calc_candidates(expr);
    }
    if let Some(kw) = key.strip_prefix("emoj") {
        return Some(emoj_candidates(kw, emoji_full));
    }
    None
}

// ---------------------------------------------------------------------------
// /upper：数字 → 中文（小写 + 财务大写）
// ---------------------------------------------------------------------------

const LOWER_DIGITS: [&str; 10] = ["零", "一", "二", "三", "四", "五", "六", "七", "八", "九"];
const UPPER_DIGITS: [&str; 10] = ["零", "壹", "贰", "叁", "肆", "伍", "陆", "柒", "捌", "玖"];
/// 组内单位（从低到高：个十百千；个位无单位）。false = 小写，true = 财务大写
/// （十→拾、百→佰、千→仟——测试首跑即抓到漏改：数字大写了单位没大写）。
const GROUP_UNITS: [[&str; 4]; 2] = [["", "十", "百", "千"], ["", "拾", "佰", "仟"]];
/// 组间单位（4 位一组：万、亿、万亿；大小写同形）。
const SECTION_UNITS: [&str; 4] = ["", "万", "亿", "万亿"];

/// 数字串 → 中文（false = 小写，true = 财务大写）。
/// 规则：非负整数，1..=16 位（到千万亿）；前导零容忍；全零 → 零。
/// 中文习惯：零折叠（1006→一千零六，跨组零也只补一个「零」）；末尾零省略；
/// 最低组 1x → 「十…」不带一（10→十、16→十六）；非最低组的一十保留一
/// （110→一百一十——组内百位后的十位，一十的「一」保留）。
fn digits_to_chinese(digits: &str, upper: bool) -> Option<String> {
    if digits.is_empty() || digits.len() > 16 {
        return None;
    }
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let digits = digits.trim_start_matches('0');
    let d = if digits.is_empty() { "0" } else { digits };
    if d == "0" {
        // 「零」在中文小写与财务大写中同形（大写壹贰叁…从「一」起才有区别）——
        // 不随 upper 分支（曾写成 if upper { "零" } else { "零" }，clippy 同分支告警）。
        return Some("零".to_string());
    }
    let nums: Vec<u8> = d.bytes().map(|b| b - b'0').collect();
    let dg = if upper { &UPPER_DIGITS } else { &LOWER_DIGITS };
    let gu = &GROUP_UNITS[if upper { 1 } else { 0 }];

    // 按 4 位一组（从低位切）
    let mut groups: Vec<Vec<u8>> = Vec::new();
    let mut rest = &nums[..];
    while !rest.is_empty() {
        let take = rest.len().min(4);
        groups.push(rest[rest.len() - take..].to_vec());
        rest = &rest[..rest.len() - take];
    }
    // groups[0] = 最低组

    let mut out = String::new();
    let mut pending_zero = false; // 跨组/组内零折叠标记
    for (gi, g) in groups.iter().enumerate().rev() {
        // gi 从高到低
        let group_nonzero = g.iter().any(|&x| x != 0);
        if !group_nonzero {
            pending_zero = true; // 整组为零：留一个「零」给下一非空组
            continue;
        }
        let mut zero_covered = false; // 跨组零已补（组内前导零不再重复补）
        if pending_zero && !out.is_empty() {
            out.push_str(dg[0]);
            zero_covered = true;
        }
        pending_zero = false;
        // 组内：从最高位读
        let len = g.len();
        let mut group_out = String::new();
        let mut in_zero_run = false;
        for (i, &digit) in g.iter().enumerate() {
            let pos = len - 1 - i; // 组内位（0=个位）
            if digit == 0 {
                in_zero_run = true;
                continue;
            }
            // 零折叠：数字前有零间隙则补一个「零」——组中部间隙（1006→一千零六）
            // 与组内前导零（10086 的低组 0086 → 一万"零"八十六）都算；最高组
            // 首位非零（整体已去前导零），out/group_out 双空时不补。
            if in_zero_run && !zero_covered && !(group_out.is_empty() && out.is_empty()) {
                group_out.push_str(dg[0]);
            }
            zero_covered = false; // 首个非零已读，后续组内间隙走常规补零
            in_zero_run = false;
            // 十位特例：最低组恰好两位且十位为 1 → 省略「一」（10→十、16→十六；
            // 大写财务习惯保留「壹拾」）
            let is_bare_ten = pos == 1 && digit == 1 && len == 2 && gi == 0 && !upper;
            if !is_bare_ten {
                group_out.push_str(dg[digit as usize]);
            }
            group_out.push_str(gu[pos]);
        }
        out.push_str(&group_out);
        out.push_str(SECTION_UNITS[gi]);
    }
    Some(out)
}

/// /upper 候选生成（小数 + 人民币金额支持）：
///   候选 1：中文小写数字（整数部分转换 + 「点」+ 小数逐位：1360.5 →
///           一千三百六十点五）
///   候选 2：人民币大写金额（元/角/分：1360 → 壹仟叁佰陆拾元整；
///           1360.5 → 壹仟叁佰陆拾元伍角；1360.05 → 壹仟叁佰陆拾元零伍分；
///           0.5 → 伍角；0.05 → 伍分）
/// num 形如 "1360"、"1360.5"、".5"（DLL 端保证至多一个点）；非法（多点/
/// 非数字字符）返回 None。
fn upper_candidates(num: &str) -> Option<Vec<Candidate>> {
    if num.is_empty() {
        return None; // 裸 "/upper"（无数字）：DLL 端交互模式已拦截，不该到这
    }
    let (int_part, frac_part) = match num.split_once('.') {
        Some((i, f)) => (i, f),
        None => (num, ""),
    };
    // 校验：整数/小数部分全数字；小数 ≤ 4 位（超出只对候选 1 逐位、金额取前两位）
    if int_part.chars().any(|c| !c.is_ascii_digit())
        || frac_part.chars().any(|c| !c.is_ascii_digit())
    {
        return None;
    }
    if int_part.len() > 16 {
        return None;
    }
    let int_digits = if int_part.is_empty() { "0" } else { int_part };

    // 候选 1：小写（小数 = 点 + 逐位，无单位）
    let mut lower = digits_to_chinese(int_digits, false)?;
    if !frac_part.is_empty() {
        lower.push('点');
        for c in frac_part.chars() {
            lower.push_str(LOWER_DIGITS[c as usize - '0' as usize]);
        }
    }

    // 候选 2：人民币大写金额
    let rmb = rmb_amount(int_digits, frac_part)?;

    Some(
        [&lower, &rmb]
            .iter()
            .map(|s| Candidate {
                text: s.as_str().into(),
                base_freq: 10000,
                pinyin: String::new().into(),
                is_primary: true,
                is_correction: false,
                is_sentence: false,
                is_guess: false,
                is_shortcut: true,
                is_english: false,
                sentence_score: 0,
            })
            .collect(),
    )
}

/// 人民币大写金额（财务规则）：
///   · 整数部分非零：大写数字 + 「元」；无小数 → 尾加「整」
///   · 角位 = 小数第 1 位，分位 = 小数第 2 位；角零分非零 → 元后补「零」
///   · 整数部分为零且有角/分 → 省略「零元」（0.5 → 伍角，0.05 → 伍分）
///   · 小数第 2 位后忽略（金额精度到分）
fn rmb_amount(int_digits: &str, frac_part: &str) -> Option<String> {
    let int_up = digits_to_chinese(int_digits, true)?;
    let jiao = frac_part.chars().next().map(|c| c as usize - '0' as usize);
    let fen = frac_part.chars().nth(1).map(|c| c as usize - '0' as usize);
    let has_frac = matches!(jiao, Some(j) if j > 0) || matches!(fen, Some(f) if f > 0);

    let mut out = String::new();
    let int_zero = int_digits.trim_start_matches('0').is_empty() || int_digits == "0";
    if !int_zero {
        out.push_str(&int_up);
        out.push('元');
    }
    if !has_frac {
        if out.is_empty() {
            out.push_str("零元");
        }
        out.push('整');
        return Some(out);
    }
    let mut need_zero = false;
    if let Some(j) = jiao {
        if j > 0 {
            out.push_str(UPPER_DIGITS[j]);
            out.push('角');
        } else if !int_zero {
            need_zero = true; // 角为零分非零 → 元后补零（壹元零伍分）
        }
    }
    if let Some(f) = fen {
        if f > 0 {
            if need_zero {
                out.push('零');
            }
            out.push_str(UPPER_DIGITS[f]);
            out.push('分');
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// /calc：快捷计算器（独立交互模式 /calc 进入）
// ---------------------------------------------------------------------------

/// 递归下降求值器（零依赖纯函数，零网络）。
/// 文法（优先级从低到高）：expr := term (+|- term)*；term := unary (*|/ unary)*；
/// unary := (-|+)? power；power := atom (^ unary)?（右结合，允许 2^-3）；
/// atom := number | '(' expr ')'。语义对齐数学惯例：-2^2 = -(2^2) = -4。
struct CalcParser<'a> {
    s: &'a [u8],
    pos: usize,
}

impl<'a> CalcParser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.s.len() && self.s[self.pos] == b' ' {
            self.pos += 1;
        }
    }
    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.s.get(self.pos).copied()
    }
    fn expr(&mut self) -> Option<f64> {
        let mut v = self.term()?;
        loop {
            match self.peek() {
                Some(b'+') => {
                    self.pos += 1;
                    v += self.term()?;
                }
                Some(b'-') => {
                    self.pos += 1;
                    v -= self.term()?;
                }
                _ => return Some(v),
            }
        }
    }
    fn term(&mut self) -> Option<f64> {
        let mut v = self.unary()?;
        loop {
            match self.peek() {
                Some(b'*') => {
                    self.pos += 1;
                    v *= self.unary()?;
                }
                Some(b'/') => {
                    self.pos += 1;
                    // 除零 → ±inf / NaN，由 calc_eval 的 is_finite 统一拒绝
                    v /= self.unary()?;
                }
                _ => return Some(v),
            }
        }
    }
    fn unary(&mut self) -> Option<f64> {
        match self.peek() {
            Some(b'-') => {
                self.pos += 1;
                Some(-self.unary()?)
            }
            // 不容忍一元正号（"1++2" 拒绝而非静默算成 3——计算器对畸形输入
            // 严格拒绝，用户在算式行看得见、改得掉）
            _ => self.power(),
        }
    }
    fn power(&mut self) -> Option<f64> {
        let base = self.atom()?;
        if self.peek() == Some(b'^') {
            self.pos += 1;
            let e = self.unary()?; // 右结合：2^3^2 = 2^(3^2) = 512
            Some(base.powf(e))
        } else {
            Some(base)
        }
    }
    fn atom(&mut self) -> Option<f64> {
        match self.peek()? {
            b'(' => {
                self.pos += 1;
                let v = self.expr()?;
                if self.peek() == Some(b')') {
                    self.pos += 1;
                    Some(v)
                } else {
                    None // 括号不闭合
                }
            }
            c if c.is_ascii_digit() || c == b'.' => self.number(),
            _ => None,
        }
    }
    fn number(&mut self) -> Option<f64> {
        self.skip_ws();
        let start = self.pos;
        let mut seen_dot = false;
        while let Some(&c) = self.s.get(self.pos) {
            if c.is_ascii_digit() {
                self.pos += 1;
            } else if c == b'.' && !seen_dot {
                seen_dot = true;
                self.pos += 1;
            } else {
                break;
            }
        }
        // 裸点 "." 非法（"1." 合法 = 1，f64 解析容忍；".5" = 0.5 同）
        if self.s[start..self.pos].iter().all(|&c| c == b'.') {
            return None;
        }
        std::str::from_utf8(&self.s[start..self.pos])
            .ok()?
            .parse::<f64>()
            .ok()
    }
}

/// 算式求值入口：非法/除零/溢出/尾部残留 → None。
fn calc_eval(expr: &str) -> Option<f64> {
    let mut p = CalcParser {
        s: expr.as_bytes(),
        pos: 0,
    };
    let v = p.expr()?;
    p.skip_ws();
    if p.pos != expr.len() {
        return None; // 尾部残留（如 "1+2)"、"1.2.3" 的第二个点段）
    }
    if !v.is_finite() {
        return None;
    }
    Some(v)
}

/// 结果格式化：12 位有效数字圆整清浮点噪声（0.1+0.2 → 0.3、
/// 123*123.1 → 15141.3），整数值（|v| < 1e15）精确直出不受圆整影响。
///
/// 圆整必须走**十进制字符串**（`{:.11e}` = 12 位有效数字的科学计数法，
/// 再转普通计数法）——旧实现 `(v*10^k).round()/10^k` 的乘除往返本身带
/// f64 表示误差，会把噪声重新引回来（实测 123*123.1 出
/// 15141.299999999999——scale 10^7 乘积的 ulp≈3e-5 吞掉 1e-5 量级噪声，
/// round 不动、除回又落回噪声值）。字符串路径无二次舍入，噪声恒清。
fn fmt_calc_num(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    // 整数精确直出（f64 整数精确域 |v| < 2^53 ≈ 9e15）：大整数不被 12 位
    // 有效数字圆整截断（123456789012345 保持原值）
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    // {:.11e} = 1 位整数 + 11 位小数 = 12 位有效数字（IEEE 正确舍入）
    let sci = format!("{:.11e}", v);
    let (mantissa, exp) = sci.split_once('e').expect("scientific format");
    let exp: i32 = exp.parse().expect("exponent");
    let neg = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    // 小数点原在首位数字后，右移 exp 位
    let point = 1 + exp;
    // 极端量级保留科学计数法（防几百字符十进制串撑爆候选窗；旧实现固定
    // 12 位小数会把 1e-20 截成 0，科学计数法既正确又紧凑）
    if !(-12..=16).contains(&point) {
        let m = mantissa.trim_end_matches('0').trim_end_matches('.');
        return format!("{}e{}", m, exp);
    }
    let mut s = if point <= 0 {
        format!("0.{}{}", "0".repeat((-point) as usize), digits)
    } else if (point as usize) >= digits.len() {
        format!("{}{}", digits, "0".repeat(point as usize - digits.len()))
    } else {
        format!(
            "{}.{}",
            &digits[..point as usize],
            &digits[point as usize..]
        )
    };
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if neg {
        s.insert(0, '-');
    }
    s
}

/// /calc 候选生成：候选 1 = 结果；候选 2 = 「算式=结果」（留痕粘贴用）。
/// 约束：算式 ≤64 字符、字符集 数字 . + - * / ^ ( ) 与空格（空格忽略）；
/// 非法/除零/溢出 → None（DLL 交互模式继续显示算式、不出结果候选）。
fn calc_candidates(expr: &str) -> Option<Vec<Candidate>> {
    if expr.is_empty() || expr.len() > 64 {
        return None; // 裸 "/calc"（DLL 交互模式已拦截）与超长防御
    }
    let v = calc_eval(expr)?;
    let result = fmt_calc_num(v);
    // 裸数字（无运算符）只出结果候选：
    // "345=345" 类自指等式候选是垃圾（数字输入中间态每键都出，提交还会
    // 把垃圾刷进历史）。有运算符才出「算式=结果」双候选。
    let shortcut = |s: &str| Candidate {
        text: s.into(),
        base_freq: 10000,
        pinyin: String::new().into(),
        is_primary: true,
        is_correction: false,
        is_sentence: false,
        is_guess: false,
        is_shortcut: true,
        is_english: false,
        sentence_score: 0,
    };
    if !expr
        .bytes()
        .any(|b| matches!(b, b'+' | b'-' | b'*' | b'/' | b'^'))
    {
        return Some(vec![shortcut(&result)]);
    }
    let eq = format!("{}={}", expr.replace(' ', ""), result);
    Some(
        [result, eq]
            .iter()
            .map(|s| Candidate {
                text: s.as_str().into(),
                base_freq: 10000,
                pinyin: String::new().into(),
                is_primary: true,
                is_correction: false,
                is_sentence: false,
                is_guess: false,
                is_shortcut: true,
                is_english: false,
                sentence_score: 0,
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// /emoj：Emoji 表（common = 常用精选；common=false 之外的条目 = 全量补充）
// ---------------------------------------------------------------------------

/// 单条 Emoji：`ch` = 字符（可能多 codepoint），`kw` = 拼音关键字（前缀
/// 过滤），`common` = 是否进「常用」档。
pub(crate) struct Emoj {
    ch: &'static str,
    kw: &'static [&'static str],
    common: bool,
}

macro_rules! emoj {
    ($ch:expr, common = true, [$($k:expr),*]) => { Emoj { ch: $ch, kw: &[$($k),*], common: true } };
    ($ch:expr, common = false, [$($k:expr),*]) => { Emoj { ch: $ch, kw: &[$($k),*], common: false } };
}

include!("emoji_table.rs");

/// 前缀匹配（大小写不敏感——DLL 斜杠模式把输入字母统一转小写，表内
/// 关键字保留 CamelCase 可读性）。双向：关键字以输入开头（渐进过滤）或
/// 输入以关键字开头（关键字很短时的容错）。
fn kw_prefix(kw: &str, k: &str) -> bool {
    if kw.len() <= k.len() {
        k.len() >= kw.len()
            && k.is_char_boundary(kw.len())
            && k[..kw.len()].eq_ignore_ascii_case(kw)
    } else {
        kw.len() >= k.len() && kw.is_char_boundary(k.len()) && kw[..k.len()].eq_ignore_ascii_case(k)
    }
}

fn emoj_candidates(kw: &str, full: bool) -> Vec<Candidate> {
    let kw = kw.trim();
    EMOJ_TABLE
        .iter()
        .filter(|e| full || e.common)
        .filter(|e| kw.is_empty() || e.kw.iter().any(|k| kw_prefix(kw, k)))
        .map(|e| Candidate {
            text: e.ch.into(),
            base_freq: 10000,
            pinyin: String::new().into(),
            is_primary: true,
            is_correction: false,
            is_sentence: false,
            is_guess: false,
            is_shortcut: true,
            is_english: false,
            sentence_score: 0,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_upper_basic() {
        assert_eq!(digits_to_chinese("1360", false).unwrap(), "一千三百六十");
        assert_eq!(digits_to_chinese("1360", true).unwrap(), "壹仟叁佰陆拾");
        assert_eq!(digits_to_chinese("0", false).unwrap(), "零");
        assert_eq!(digits_to_chinese("10", false).unwrap(), "十");
        assert_eq!(digits_to_chinese("10", true).unwrap(), "壹拾");
        assert_eq!(digits_to_chinese("16", false).unwrap(), "十六");
        assert_eq!(digits_to_chinese("110", false).unwrap(), "一百一十");
        assert_eq!(digits_to_chinese("110", true).unwrap(), "壹佰壹拾");
        assert_eq!(digits_to_chinese("1006", false).unwrap(), "一千零六");
        assert_eq!(digits_to_chinese("10000", false).unwrap(), "一万");
        assert_eq!(digits_to_chinese("10086", false).unwrap(), "一万零八十六");
        assert_eq!(digits_to_chinese("100000001", false).unwrap(), "一亿零一");
        assert_eq!(digits_to_chinese("100000001", true).unwrap(), "壹亿零壹");
        assert_eq!(digits_to_chinese("100000000", false).unwrap(), "一亿");
        assert_eq!(digits_to_chinese("10001000", false).unwrap(), "一千万一千");
        assert_eq!(digits_to_chinese("20", false).unwrap(), "二十");
        assert_eq!(digits_to_chinese("120", false).unwrap(), "一百二十");
        assert_eq!(
            digits_to_chinese("1234567890123456", false).unwrap(),
            "一千二百三十四万亿五千六百七十八亿九千零一十二万三千四百五十六"
        );
    }

    #[test]
    fn test_upper_invalid() {
        assert!(digits_to_chinese("", false).is_none());
        assert!(digits_to_chinese("12a", false).is_none());
        assert!(digits_to_chinese("12345678901234567", false).is_none()); // 17 位
        assert!(digits_to_chinese("-5", false).is_none());
    }

    #[test]
    fn test_dispatch_upper() {
        let c = dispatch("/upper1360", false).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(&*c[0].text, "一千三百六十");
        assert_eq!(&*c[1].text, "壹仟叁佰陆拾元整"); // 人民币金额
        assert!(c[0].is_shortcut);
        assert!(dispatch("/upper", false).is_none()); // 无数字
        assert!(dispatch("/upper12x", false).is_none());
        assert!(dispatch("/dat", false).is_none()); // 其他命令不归本模块
    }

    #[test]
    fn test_upper_decimal_and_rmb() {
        // 小数：候选 1 = 点 + 逐位；候选 2 = 人民币角/分
        let c = dispatch("/upper1360.5", false).unwrap();
        assert_eq!(&*c[0].text, "一千三百六十点五");
        assert_eq!(&*c[1].text, "壹仟叁佰陆拾元伍角");
        // 角零分非零 → 元后补零
        let c = dispatch("/upper1360.05", false).unwrap();
        assert_eq!(&*c[0].text, "一千三百六十点零五");
        assert_eq!(&*c[1].text, "壹仟叁佰陆拾元零伍分");
        // 整数为零：只有角/分
        let c = dispatch("/upper0.5", false).unwrap();
        assert_eq!(&*c[0].text, "零点五");
        assert_eq!(&*c[1].text, "伍角");
        let c = dispatch("/upper0.05", false).unwrap();
        assert_eq!(&*c[1].text, "伍分");
        // 角分俱全
        let c = dispatch("/upper3.25", false).unwrap();
        assert_eq!(&*c[0].text, "三点二五");
        assert_eq!(&*c[1].text, "叁元贰角伍分");
        // 末尾零分省略
        let c = dispatch("/upper3.20", false).unwrap();
        assert_eq!(&*c[1].text, "叁元贰角");
        // 前导点（DLL 保证至多一个点；".5" 整数按 0）
        let c = dispatch("/upper.5", false).unwrap();
        assert_eq!(&*c[0].text, "零点五");
        assert_eq!(&*c[1].text, "伍角");
        // 多点非法
        assert!(dispatch("/upper1.2.3", false).is_none());
    }

    #[test]
    fn test_emoj_dispatch() {
        // 空关键字：常用档有条目
        let c = dispatch("/emoj", false).unwrap();
        assert!(!c.is_empty());
        // 拼音前缀过滤
        let c = dispatch("/emojxiao", false).unwrap();
        assert!(!c.is_empty());
        assert!(c.iter().all(|x| x.is_shortcut));
        // 无匹配关键字
        assert!(dispatch("/emojzzzzzz", false).unwrap().is_empty());
        // 全量档 ≥ 常用档
        let full = dispatch("/emoj", true).unwrap().len();
        let common = dispatch("/emoj", false).unwrap().len();
        assert!(full >= common);
    }

    #[test]
    fn test_calc_basic() {
        // 优先级 + 双候选形态
        let c = dispatch("/calc1+2*3", false).unwrap();
        assert_eq!(&*c[0].text, "7");
        assert_eq!(&*c[1].text, "1+2*3=7");
        assert!(c[0].is_shortcut);
        // 裸数字（无运算符）只出结果单候选（
        // "345=345" 自指等式候选是垃圾，且提交会刷进历史）
        let bare = dispatch("/calc345", false).unwrap();
        assert_eq!(bare.len(), 1, "裸数字不得出等式候选");
        assert_eq!(&*bare[0].text, "345");
        let bare2 = dispatch("/calc3465.465", false).unwrap();
        assert_eq!(bare2.len(), 1);
        assert_eq!(&*bare2[0].text, "3465.465");
        assert_eq!(&*dispatch("/calc(1+2)*3", false).unwrap()[0].text, "9");
        assert_eq!(&*dispatch("/calc10-2-3", false).unwrap()[0].text, "5");
        assert_eq!(&*dispatch("/calc10/4", false).unwrap()[0].text, "2.5");
        // 幂：右结合 + 一元负号穿透（-2^2 = -4 数学惯例）
        assert_eq!(&*dispatch("/calc2^10", false).unwrap()[0].text, "1024");
        assert_eq!(&*dispatch("/calc2^3^2", false).unwrap()[0].text, "512");
        assert_eq!(&*dispatch("/calc-2^2", false).unwrap()[0].text, "-4");
        assert_eq!(&*dispatch("/calc2^-3", false).unwrap()[0].text, "0.125");
        // 一元正负号
        assert_eq!(&*dispatch("/calc-5+10", false).unwrap()[0].text, "5");
        assert_eq!(&*dispatch("/calc2*-3", false).unwrap()[0].text, "-6");
        // 空格容忍（等式候选剥空格）
        let c = dispatch("/calc 1 + 2 ", false).unwrap();
        assert_eq!(&*c[0].text, "3");
        assert_eq!(&*c[1].text, "1+2=3");
        // 嵌套括号 + 小数
        assert_eq!(&*dispatch("/calc((1+1)*2.5)", false).unwrap()[0].text, "5");
    }

    #[test]
    fn test_calc_float_cleanup() {
        // 12 位有效数字清理浮点噪声
        assert_eq!(&*dispatch("/calc0.1+0.2", false).unwrap()[0].text, "0.3");
        assert_eq!(
            &*dispatch("/calc1/3", false).unwrap()[0].text,
            "0.333333333333"
        );
        // 整数结果出整数（0.5*4 的 f64 恰为 2）
        assert_eq!(&*dispatch("/calc0.5*4", false).unwrap()[0].text, "2");
        assert_eq!(&*dispatch("/calc3*0.3", false).unwrap()[0].text, "0.9");
        assert_eq!(&*dispatch("/calc0", false).unwrap()[0].text, "0");
        // 123*123.1 旧乘除往返
        // 圆整把噪声引回（scale 10^7 乘积 ulp 吞噪声）。字符串路径恒清。
        assert_eq!(
            &*dispatch("/calc123*123.1", false).unwrap()[0].text,
            "15141.3"
        );
        assert_eq!(
            &*dispatch("/calc123.1*123", false).unwrap()[0].text,
            "15141.3"
        );
        assert_eq!(
            &*dispatch("/calc-123*123.1", false).unwrap()[0].text,
            "-15141.3"
        );
        assert_eq!(&*dispatch("/calc1.1*1.1", false).unwrap()[0].text, "1.21");
        assert_eq!(&*dispatch("/calc0.1*0.2", false).unwrap()[0].text, "0.02");
        assert_eq!(
            &*dispatch("/calc10/3", false).unwrap()[0].text,
            "3.33333333333"
        );
        // 极小数不走固定 12 位小数（旧实现 {:.12} 把 3e-7 截成 0）
        assert_eq!(
            &*dispatch("/calc0.0000001*3", false).unwrap()[0].text,
            "0.0000003"
        );
        // 大整数精确直出（f64 整数精确域内不被 12 位有效数字截断）
        assert_eq!(
            &*dispatch("/calc123456789012345", false).unwrap()[0].text,
            "123456789012345"
        );
        assert_eq!(
            &*dispatch("/calc999999999999999", false).unwrap()[0].text,
            "999999999999999"
        );
        // 「算式=结果」留痕候选同口径
        assert_eq!(
            &*dispatch("/calc123*123.1", false).unwrap()[1].text,
            "123*123.1=15141.3"
        );
        // 极端量级走科学计数法（不截成 0、不出几百字符长串）
        assert_eq!(&*dispatch("/calc1/10^20", false).unwrap()[0].text, "1e-20");
        assert_eq!(
            &*dispatch("/calc10^20*1.5", false).unwrap()[0].text,
            "1.5e20"
        );
    }

    #[test]
    fn test_calc_invalid() {
        assert!(dispatch("/calc", false).is_none()); // 裸命令
        assert!(dispatch("/calc1/0", false).is_none()); // 除零
        assert!(dispatch("/calc0/0", false).is_none()); // NaN
        assert!(dispatch("/calc1++2", false).is_none()); // 连续运算符
        assert!(dispatch("/calc(1+2", false).is_none()); // 括号不闭合
        assert!(dispatch("/calc1+2)", false).is_none()); // 尾部残留
        assert!(dispatch("/calc1.2.3", false).is_none()); // 多点
        assert!(dispatch("/calc.", false).is_none()); // 裸点
        assert!(dispatch("/calcabc", false).is_none()); // 非算式
        assert!(dispatch("/calc10%", false).is_none()); // % 不在 v1 字符集
        assert!(dispatch("/cal", false).is_none()); // 前缀不完整
    }
}
