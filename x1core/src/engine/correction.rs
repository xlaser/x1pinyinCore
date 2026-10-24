//! 智能纠错的纯函数（从 engine/mod.rs 抽出）。
//!
//! 含键盘邻接表 + 编辑距离变形生成 + 发音混淆变形生成 + 简拼混合合法性判断。
//! 全部为纯函数（无 Engine 状态），maybe_correct 方法留在 mod.rs 调用这些函数。

use alloc::string::String;
use alloc::vec::Vec;

// ===========================================================================
// 智能纠错：键盘邻接表与变形生成
// ===========================================================================
//
// 输入拼错（如 nihsou 误触，想打 nihao）时，对 input 做编辑距离 1 的字符替换
// 变形，重新查询补充候选。按 QWERTY 键盘邻接关系优先排序变形（误触高概率）。

/// QWERTY 键盘邻接表：每个键的物理相邻键（误触高概率）。
///
/// 用于智能纠错：把输入的某个字符替换为相邻键时，误触概率最高，优先尝试。
/// 例如 `s` 的相邻键是 `a/w/d/x/z`，`nihsou` 的 `s` → `h`(相邻) 优先于 `s` → `z`(非相邻)。
/// 键盘邻接键表（仅水平同行相邻，用于智能纠错的误触变形）。
///
/// 误触规律（基于人体工学）：
/// - 上下行相邻不算邻接（如 a↔q）：不同手指控制不同行，垂直误触概率极低。
/// - 同行相邻手指不算邻接（如 a↔s 左手小指↔无名指）：同手相邻手指有清晰边界，难按错。
/// - 保留：无名指/小指在水平方向向两端偏时容易按到旁边的键。
///
/// 因此本表只记录「实际误触概率较高」的邻接关系，避免过度纠错。
/// （注：当前实现按「同行水平相邻」简化，保留各行端部及易混淆的邻接对。）
pub(super) const KEYBOARD_NEIGHBORS: &[(char, &[char])] = &[
    // 顶行（数字行下方）：端部易偏
    ('q', &['w']),
    ('w', &['q', 'e']),
    ('e', &['w', 'r']),
    ('r', &['e', 't']),
    ('t', &['r', 'y']),
    ('y', &['t', 'u']),
    ('u', &['y', 'i']),
    ('i', &['u', 'o']),
    ('o', &['i', 'p']),
    ('p', &['o']),
    // 中行：端部易偏
    ('a', &['s']),
    ('s', &['a', 'd']),
    ('d', &['s', 'f']),
    ('f', &['d', 'g']),
    ('g', &['f', 'h']),
    ('h', &['g', 'j']),
    ('j', &['h', 'k']),
    ('k', &['j', 'l']),
    ('l', &['k']),
    // 底行：端部易偏
    ('z', &['x']),
    ('x', &['z', 'c']),
    ('c', &['x', 'v']),
    ('v', &['c', 'b']),
    ('b', &['v', 'n']),
    ('n', &['b', 'm']),
    ('m', &['n']),
];

/// 查找某键的相邻键
pub(super) fn keyboard_neighbors(c: char) -> &'static [char] {
    KEYBOARD_NEIGHBORS
        .iter()
        .find(|(k, _)| *k == c)
        .map(|(_, v)| *v)
        .unwrap_or(&[])
}

/// 对 input 生成编辑距离 1 的变形候选，用于智能纠错。
///
/// 生成顺序（误触概率从高到低）：
/// 1. 邻接替换：把某字符换成键盘相邻键（如 s→a/d/w，最常见误触）
/// 2. 删除：去掉某字符（多打了一个，如 nihsou→nihou 去掉 s）
///
/// 不做非邻接替换：键盘误触场景的边界就是邻接关系，换成不相邻字母是
/// 另一类错误（记忆/发音类，由 phonetic/swap 承载）。非邻接替换会产生
/// 字母序随机的冷僻词噪声（nuihao 的 n→c 出「崔颢」freq=483 占据黄金位），
/// 干扰用户意愿。
///
/// 不做插入（少打字符）：插入变形空间大（N×26），收益低，且少打字符时
/// 用户通常会继续输入，partial 路径已能处理。
pub(super) fn generate_corrections(input: &str) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::new();

    // 1. 邻接替换优先
    for (i, &c) in chars.iter().enumerate() {
        for &neighbor in keyboard_neighbors(c) {
            let mut variant = chars.clone();
            variant[i] = neighbor;
            result.push(variant.into_iter().collect());
        }
    }

    // 2. 删除（多打字符，如 nihsou 去掉 s → nihou）
    //    长度 ≥3 才删除（太短的删除后无意义）
    if chars.len() >= 3 {
        for i in 0..chars.len() {
            let mut variant = chars.clone();
            variant.remove(i);
            result.push(variant.into_iter().collect());
        }
    }
    result
}

/// 双写字母折叠变体（多按一个字母的场景，如 sheng→shenng）。
///
/// 用户多按了一个字母（sheng→shenng）时，混合简拼解析（shen+n+g 合法尾）会
/// 把错误吞掉——纠错的"结构性错误"门控（maybe_correct）对合法简跳过，用户
/// 只能拿到 神女/神农 这类错误解读。合法拼音中相邻重复字母只出现在跨音节
/// 边界（changgan 的 gg），而完整全拼输入在 maybe_correct 第一道门就返回——
/// 所以「折叠一个相邻重复字母后成为完整全拼」是多打字母的高置信信号。
/// 变体有效性（能否完整切分）由调用方判定（与既有纠错变体同一标准）。
pub(super) fn generate_collapse_variants(input: &str) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut result = Vec::new();
    for i in 0..chars.len().saturating_sub(1) {
        if chars[i] == chars[i + 1] {
            let mut variant = chars.clone();
            variant.remove(i);
            let s: String = variant.into_iter().collect();
            if !result.contains(&s) {
                result.push(s);
            }
        }
    }
    result
}

/// 判断输入是否为「合法的全拼+简拼混合」（如 yunxs = yun + x + s）。
///
/// 规则：贪心 parse_pinyin 切分后，所有单字母片段（len==1）必须满足：
/// 1. 都是合法声母（b/p/m/f/d/t/n/l/g/k/h/j/q/x/r/z/c/s/y/w）；
/// 2. 都出现在末尾连续区（完整音节之后），而非夹在两个完整音节中间。
///
/// 第二条是关键区分：
/// - yunxs → [yun, x, s]，x/s 在完整音节 yun 之后 → 合法（全拼+简拼尾）
/// - nihsou → [ni, h, sou]，h 夹在 ni 和 sou 中间 → 不合法（孤立字母=拼错）
///
/// 这样能精准识别「正常简拼尾部」并跳过纠错，同时仍纠错「中间孤立字母」类拼错。
pub(super) fn is_legal_jianpin_mix(input: &str) -> bool {
    let parsed = crate::pinyin::parse_pinyin(input);
    if parsed.syllables.is_empty() {
        return false;
    }
    // 找到最后一个「完整音节」（len > 1 且是有效音节）的位置
    let bytes = input.as_bytes();
    let mut last_full_idx: Option<usize> = None;
    for (i, s) in parsed.syllables.iter().enumerate() {
        let seg = &bytes[s.start..s.end];
        let seg_str = core::str::from_utf8(seg).unwrap_or("");
        if seg.len() > 1 && crate::pinyin::is_valid_syllable_pub(seg_str) {
            last_full_idx = Some(i);
        }
    }
    let last_full = match last_full_idx {
        Some(idx) => idx,
        None => return false, // 没有完整音节 → 不是「全拼+简拼混合」
    };

    // 末尾区（last_full 之后的所有片段）必须都是合法声母单字母
    const LEGAL_INITIALS: &[char] = &[
        'b', 'p', 'm', 'f', 'd', 't', 'n', 'l', 'g', 'k', 'h', 'j', 'q', 'x', 'r', 'z', 'c', 's',
        'y', 'w',
    ];
    for s in &parsed.syllables[last_full + 1..] {
        if s.end - s.start != 1 {
            return false; // 末尾区出现多字符片段 → 不是纯简拼尾
        }
        let c = bytes[s.start] as char;
        if !LEGAL_INITIALS.contains(&c) {
            return false;
        }
    }
    // 中间区（last_full 之前）不能有单字母片段（孤立字母=拼错）
    for s in &parsed.syllables[..last_full] {
        if s.end - s.start == 1 {
            return false;
        }
    }
    // 必须至少有一个末尾单字母（否则不是简拼混合）
    parsed.syllables[last_full + 1..]
        .iter()
        .any(|s| s.end - s.start == 1)
}

/// 发音混淆规则（纠错专用，不受用户模糊音开关影响）。
///
/// 复用 FuzzyConfig 的规则集合，但作为常量写死——即使用户关闭了模糊音
/// （为了降低单字重码），纠错仍能捕捉发音混淆类错误，如 pingzeng → pingzheng
/// （zeng/zheng 混淆）、yunxongshi → yunxingshi（xong 不是有效音节但用户
/// 可能记不准 xing/xiong 的韵母）。
///
/// 每条规则形如 (pattern, replacement)，对 input 的任意子串匹配位置替换。
/// 同时处理声母（zh↔z 等）和韵母（eng↔en 等）混淆。
pub(super) const PHONETIC_CONFUSIONS: &[(&str, &str)] = &[
    // 声母混淆（舌尖前/后音）
    ("zh", "z"),
    ("z", "zh"),
    ("ch", "c"),
    ("c", "ch"),
    ("sh", "s"),
    ("s", "sh"),
    // 鼻音/边音、唇齿/舌根
    ("n", "l"),
    ("l", "n"),
    ("f", "h"),
    ("h", "f"),
    // 韵母混淆（前后鼻音）
    ("eng", "en"),
    ("en", "eng"),
    ("ang", "an"),
    ("an", "ang"),
    ("ing", "in"),
    ("in", "ing"),
];

/// 单个音节的发音变体（上下文证据发音营救用）。
///
/// 用 PHONETIC_CONFUSIONS 规则对一个音节做单点子串替换，返回所有
/// 仍是合法音节且不同于原文的变体（如 "zong"→["zhong"]、"shan"→["shang"]、
/// "fong"→["hong"]）。无效产物（如 "zhong" 的 z→zh 得 "zhhong"）被
/// 有效性过滤掉。
pub(super) fn phonetic_syllable_variants(s: &str) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    for (pattern, replacement) in PHONETIC_CONFUSIONS {
        if let Some(pos) = s.find(pattern) {
            let variant = format!("{}{}{}", &s[..pos], replacement, &s[pos + pattern.len()..]);
            if variant != s
                && crate::pinyin::is_valid_syllable_pub(&variant)
                && !result.contains(&variant)
            {
                result.push(variant);
            }
        }
    }
    result
}

/// 生成发音混淆变形候选，用于智能纠错。
///
/// 对 input 的每个位置，尝试用 PHONETIC_CONFUSIONS 规则做子串替换，
/// 生成所有单点替换的变形。不做多点替换（组合爆炸，收益低）。
///
/// 与 `generate_corrections`（键盘误触变形）互补：键盘变形捕捉物理误触，
/// 发音变形捕捉发音不准。前者优先（误触概率更高）。
///
/// 返回顺序确定（规则表序 + 位置升序）：变体尝试
/// 预算按类别封顶后，顺序直接影响哪些变体被查询——HashSet 随机序会让
/// 同一输入跨运行纠出不同结果（用户与测试都不可复现）。
pub(super) fn generate_phonetic_corrections(input: &str) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    // 对每条规则，在 input 中找所有匹配位置做替换
    for (pattern, replacement) in PHONETIC_CONFUSIONS {
        let mut start = 0;
        while let Some(pos) = input[start..].find(pattern) {
            let abs_pos = start + pos;
            let variant = format!(
                "{}{}{}",
                &input[..abs_pos],
                replacement,
                &input[abs_pos + pattern.len()..]
            );
            if !result.contains(&variant) {
                result.push(variant);
            }
            start = abs_pos + 1; // 继续找下一个匹配位置（允许重叠）
        }
    }
    result
}

/// 生成键序颠倒变形候选，用于智能纠错。
///
/// 相邻字母交换：对 input 的每对相邻字符做交换（N-1 个变体），捕捉
/// 「手快按反了顺序」的误触（如 `teh`→`the`、`nad`→`and`、`chnag`→`chang`）。
///
/// 不设韵母字母乱序规则表：其一，方向易写反——形如 ("ang","nag") 的条目
/// 语义是"在输入中找 ang 替换成 nag"，即从正确形式生成乱序形式，纠错
/// 需要的是反方向；其二，即使翻转方向也与相邻交换完全冗余——常见乱序
/// （nag/agn/egn…）与正确形式恰好差一次相邻交换，已被本函数覆盖。
/// （实例核查：nagta 的「拿他/那他」来自删除变形而非乱序规则——删除 g 得
/// nata=na+ta；egnqi 的「恩情」来自删除 g 得 enqi 前缀匹配 en qing。）
///
/// 与 `generate_corrections`（键盘误触，字符替换）、`generate_phonetic_corrections`
/// （发音混淆，声韵母替换）互补：本函数专攻「字母顺序」错误。
/// 三者优先级：键盘 > 发音 > 键序（误触概率递减）。
///
/// 返回顺序 = 交换位置升序（预算封顶后顺序影响被查询的变体集合，
/// 固定序保证纠错结果跨运行可复现）。
pub(super) fn generate_swap_corrections(input: &str) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    if chars.len() >= 2 {
        for i in 0..chars.len() - 1 {
            let mut variant = chars.clone();
            variant.swap(i, i + 1);
            let s: String = variant.into_iter().collect();
            if !result.contains(&s) {
                result.push(s);
            }
        }
    }
    result
}
