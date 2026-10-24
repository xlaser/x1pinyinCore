//! 拼音音节解析
//!
//! 将原始按键序列（如 "zhongwen"）切分为音节序列（["zhong", "wen"]），
//! 并支持简拼（如 "zw" → "zhongwen" 的模糊匹配）。

use core::str;
use std::collections::HashMap;

/// 统一拼音查询的 Token 类型
///
/// 把任意输入切成 Token 序列，让全拼/纯简拼/混合简拼走同一套查询逻辑。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Token<'a> {
    /// 完整音节：双字符及以上的有效拼音音节（zhong / guo / er / ou）
    Full(&'a str),
    /// 单字符首字母：单字符 ASCII 字母，不论是否有效音节（a/e/m/o 也归此类）
    Initial(char),
    /// 双字母声母前缀：仅 zh / sh / ch 三个（不是完整音节，但代表明确声母）
    Shengmu(&'a str),
}

impl<'a> Token<'a> {
    /// Token 占用的字符数（用于切分位置计算）
    pub fn char_len(&self) -> usize {
        match self {
            Token::Full(s) => s.chars().count(),
            Token::Initial(_) => 1,
            Token::Shengmu(s) => s.chars().count(), // 恒为 2
        }
    }
}

/// 有效拼音音节表（无声调版）
/// 包含所有标准汉语拼音音节，共 ~400 个
pub const VALID_SYLLABLES: &[&str] = &[
    "a", "ai", "an", "ang", "ao", "ba", "bai", "ban", "bang", "bao", "bei", "ben", "beng", "bi",
    "bian", "biao", "bie", "bin", "bing", "bo", "bu", "ca", "cai", "can", "cang", "cao", "ce",
    "cen", "ceng", "cha", "chai", "chan", "chang", "chao", "che", "chen", "cheng", "chi", "chong",
    "chou", "chu", "chua", "chuai", "chuan", "chuang", "chui", "chun", "chuo", "ci", "cong", "cou",
    "cu", "cuan", "cui", "cun", "cuo", "da", "dai", "dan", "dang", "dao", "de", "dei", "den",
    "deng", "di", "dian", "diao", "die", "ding", "diu", "dong", "dou", "du", "duan", "dui", "dun",
    "duo", "e", "ei", "en", "eng", "er", "fa", "fan", "fang", "fei", "fen", "feng", "fo", "fou",
    "fu", "ga", "gai", "gan", "gang", "gao", "ge", "gei", "gen", "geng", "gong", "gou", "gu",
    "gua", "guai", "guan", "guang", "gui", "gun", "guo", "ha", "hai", "han", "hang", "hao", "he",
    "hei", "hen", "heng", "hong", "hou", "hu", "hua", "huai", "huan", "huang", "hui", "hun", "huo",
    "ji", "jia", "jian", "jiang", "jiao", "jie", "jin", "jing", "jiong", "jiu", "ju", "juan",
    "jue", "jun", "jv", "jve", "ka", "kai", "kan", "kang", "kao", "ke", "kei", "ken", "keng",
    "kong", "kou", "ku", "kua", "kuai", "kuan", "kuang", "kui", "kun", "kuo", "la", "lai", "lan",
    "lang", "lao", "le", "lei", "leng", "li", "lia", "lian", "liang", "liao", "lie", "lin", "ling",
    "liu", "lo", "long", "lou", "lu", "luan", "lue", "lun", "luo", "lv", "lve", "m", "ma", "mai",
    "man", "mang", "mao", "me", "mei", "men", "meng", "mi", "mian", "miao", "mie", "min", "ming",
    "miu", "mo", "mou", "mu", "na", "nai", "nan", "nang", "nao", "ne", "nei", "nen", "neng", "ni",
    "nian", "niang", "niao", "nie", "nin", "ning", "niu", "nong", "nou", "nu", "nuan", "nue",
    "nuo", "nv", "nve", "o", "ou", "pa", "pai", "pan", "pang", "pao", "pei", "pen", "peng", "pi",
    "pian", "piao", "pie", "pin", "ping", "po", "pou", "pu", "qi", "qia", "qian", "qiang", "qiao",
    "qie", "qin", "qing", "qiong", "qiu", "qu", "quan", "que", "qun", "qv", "qve", "ran", "rang",
    "rao", "re", "ren", "reng", "ri", "rong", "rou", "ru", "rua", "ruan", "rui", "run", "ruo",
    "sa", "sai", "san", "sang", "sao", "se", "sen", "seng", "sha", "shai", "shan", "shang", "shao",
    "she", "shei", "shen", "sheng", "shi", "shou", "shu", "shua", "shuai", "shuan", "shuang",
    "shui", "shun", "shuo", "si", "song", "sou", "su", "suan", "sui", "sun", "suo", "ta", "tai",
    "tan", "tang", "tao", "te", "tei", "teng", "ti", "tian", "tiao", "tie", "ting", "tong", "tou",
    "tu", "tuan", "tui", "tun", "tuo", "wa", "wai", "wan", "wang", "wei", "wen", "weng", "wo",
    "wu", "xi", "xia", "xian", "xiang", "xiao", "xie", "xin", "xing", "xiong", "xiu", "xu", "xuan",
    "xue", "xun", "xv", "xve", "ya", "yan", "yang", "yao", "ye", "yi", "yin", "ying", "yo", "yong",
    "you", "yu", "yuan", "yue", "yun", "yv", "za", "zai", "zan", "zang", "zao", "ze", "zei", "zen",
    "zeng", "zha", "zhai", "zhan", "zhang", "zhao", "zhe", "zhei", "zhen", "zheng", "zhi", "zhong",
    "zhou", "zhu", "zhua", "zhuai", "zhuan", "zhuang", "zhui", "zhun", "zhuo", "zi", "zong", "zou",
    "zu", "zuan", "zui", "zun", "zuo",
];

/// 单个拼音音节
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Syllable {
    pub start: usize,
    pub end: usize,
}

/// 拼音序列解析结果
#[derive(Debug, Clone)]
pub struct PinyinSeq<'a> {
    pub raw: &'a str,
    pub syllables: Vec<Syllable>,
}

/// 检查字符串是否为有效音节
pub fn is_valid_syllable_pub(s: &str) -> bool {
    VALID_SYLLABLES.binary_search(&s).is_ok()
}

/// 双字母声母（zh/sh/ch）是否可能处于「全拼音节开头」。
///
/// 语义：存在以 `prefix` 开头的有效音节 s，使 `input` 以 s 开头（zhongguo 的
/// zhong），或 `input` 本身是 s 的前缀（zho 打向 zhong 的中间态）。
/// 用于混合声母路由（`Engine::is_hybrid_shengmu_input`）：不满足即 input
/// 的前两字母只能是简拼声母（zhgrm 的 zh）。
///
/// 判据需精确区分「zh 是全拼音节开头」（zhonggrm 放行全拼）与「zh 是完成
/// 声母」（zhgrmjfjxa 归混合）——若按「zh 之后不得含元音」粗判，会把 a/e/o
/// 首字母（西安=xa）的合法简拼整体误拒，掉进纯简拼路径按逐字母拆读。
pub fn is_syllable_initial_prefix(prefix: &str, input: &str) -> bool {
    VALID_SYLLABLES
        .iter()
        .any(|s| s.starts_with(prefix) && (input.starts_with(s) || s.starts_with(input)))
}

/// 判断一个有效音节是否为零声母音节（不以辅音声母开头）。
///
/// 拼音里零声母音节有两类书写形式：
/// 1. 纯韵母：a/ai/an/ang/ao/e/ei/en/eng/er/o/ou
/// 2. y/w 开头（历史拼写约定，本质仍是零声母）：ya/yan/.../wu 等
///
/// 用途（橙词补充零声母尾部补查）：当输入切分形如
/// `[完整音节头..., 零声母尾]` 且头部拼起来恰是完整音节时（如 xianer→[xi,an,er]，
/// xi+an=xian），识别尾部为零声母，放行对头部短词的补查（如「西安」）。
///
/// 非零声母尾部（如 duanlian 的 lian，l 声母）不会命中，保护完整音节不被误拆。
///
/// 前缀词收割系统化后，橙词补充零声母尾部补查已泛化为任意尾
/// （本函数唯一生产调用点已移除）——保留作拼音工具（历史语义文档 + 潜在
/// 复用：诊断/宿主前端侧），#[allow] 防误删。
#[allow(dead_code)]
pub fn is_zero_initial_syllable(s: &str) -> bool {
    if !is_valid_syllable_pub(s) {
        return false;
    }
    let b = s.as_bytes();
    match b[0] {
        // 纯韵母零声母：a/e/o 开头
        b'a' | b'e' | b'o' => true,
        // y/w 开头的零声母音节（拼音书写约定）
        b'y' | b'w' => true,
        _ => false,
    }
}

/// 韵母归一化：将 v（ü）与 u 在韵母位置上等价处理。
///
/// 标准拼音中 v=ü 的使用规则：
/// - l/n + ü 写作 lv/nv/lve/nve（因为 lu/nu 是不同音节=路/努，不能混）
/// - j/q/x + ü 写作 ju/qu/xu/jue/que/xue（j/q/x 后没有真正的 u，u 就是 ü）
///
/// 但用户/其他输入法习惯用 v 代替 ü（jv→ju, qv→qu, xv→xu），
/// 或用 ue 代替 ve（lue→lve, nue→nve），此处统一归一化到标准写法。
///
/// 在 Trie 查询前对每个音节调用，确保查询用标准音节命中词库。
pub fn normalize_syllable(s: &str) -> &str {
    match s {
        // l/n 后：ue → ve（lue→lve, nue→nve）
        "lue" => "lve",
        "nue" => "nve",
        // j/q/x/y 后：v → u（jv→ju, qv→qu, xv→xu, yv→yu）。
        // yv 必须归一：多套双拼方案 yu 的规范编码以
        // 「声母 y + ü 键」结尾（小鹤 yv、微软 yy、紫光 yx），VALID_SYLLABLES
        // 只有 yu 没有 yv → 不归一则这些方案打不出 yu（雨/语/鱼/与/玉/于…）。
        "jv" => "ju",
        "qv" => "qu",
        "xv" => "xu",
        "yv" => "yu",
        // j/q/x 后：ve → ue（jve→jue, qve→que, xve→xue）
        "jve" => "jue",
        "qve" => "que",
        "xve" => "xue",
        // 双拼（microsoft/sogou）的零声母 ing 归一化到 ying。
        // 双拼 o; 解码出 ing，但 VALID_SYLLABLES 用 ying 作独立音节。
        "ing" => "ying",
        _ => s,
    }
}

/// 最大正向匹配切分拼音
///
/// 例如 "zhongwen" → ["zhong", "wen"]
/// 非拼音字符直接作为单字符音节
pub fn parse_pinyin(input: &str) -> PinyinSeq<'_> {
    let mut syllables = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // 显式音节分隔符：用户用 "'" 强制断开音节，如 xi'an → 西安
        if bytes[i] == b'\'' {
            i += 1;
            continue;
        }
        // 空格作为音节分隔符跳过（规范化后的输入可能含空格，
        // 如 "tong zhi"）。不跳过的话空格会被当成单字节"音节"，
        // 导致切分结果含多余空格片段。
        if bytes[i] == b' ' {
            i += 1;
            continue;
        }

        // 尝试匹配最长音节（最长6字符："chuang", "zhuang"）
        let mut matched = false;
        let max_len = (6).min(bytes.len() - i);

        for len in (1..=max_len).rev() {
            if let Ok(s) = str::from_utf8(&bytes[i..i + len]) {
                if is_valid_syllable_pub(s) {
                    syllables.push(Syllable {
                        start: i,
                        end: i + len,
                    });
                    i += len;
                    matched = true;
                    break;
                }
            }
        }

        if !matched {
            // 不是有效音节的一部分，作为单字符处理
            // 这里假设 ASCII 单字节字符；多字节字符（如数字）也按单字节步进
            // 实际应正确处理 UTF-8
            let char_len = if bytes[i].is_ascii() {
                1
            } else {
                // 简单 UTF-8 首字节判断
                let b = bytes[i];
                if b & 0b1111_0000 == 0b1111_0000 {
                    4
                } else if b & 0b1110_0000 == 0b1110_0000 {
                    3
                } else if b & 0b1100_0000 == 0b1100_0000 {
                    2
                } else {
                    1
                }
            };
            syllables.push(Syllable {
                start: i,
                end: i + char_len,
            });
            i += char_len;
        }
    }

    PinyinSeq {
        raw: input,
        syllables,
    }
}

/// 切分语义参数（统一核心 DFS 用）。
///
/// 背景：segmentations / segmentations_with_partial / segmentations_tokens 共享
/// 同一 DFS 核心，差异仅两点（第三处 Shengmu 条件
/// 漂移已在统一中修复——以 tokens 的「有后续 token 或紧邻前是 Full」为准）：
/// - 单字符有效音节（a/e/m/o）：全拼语义算 Full，混合语义算 Initial（tokens
///   的「单字符严格等于首字母」决策，spec §2.2）。两套语义分别对应全拼/混合
///   两个消费场景，是设计差异而非 bug，用参数表达。
/// - 是否允许不完整音节（单字符声母 + zh/sh/ch 双字符声母）。
#[derive(Clone, Copy, PartialEq, Eq)]
struct SegmentOptions {
    /// 单字符有效音节(a/e/m/o)算 Full（true=全拼语义，false=混合语义）
    allow_single_char_full: bool,
    /// 是否允许不完整音节分支（单字符声母 + zh/sh/ch 双字符声母）
    allow_partial: bool,
}

/// segmentations 语义：全有效音节，单字符有效音节算 Full
const SEG_FULL_ONLY: SegmentOptions = SegmentOptions {
    allow_single_char_full: true,
    allow_partial: false,
};
/// segmentations_with_partial 语义：全拼 + 不完整音节
const SEG_PARTIAL: SegmentOptions = SegmentOptions {
    allow_single_char_full: true,
    allow_partial: true,
};
/// segmentations_tokens 语义：混合简拼（单字符严格 Initial）+ 不完整音节
const SEG_TOKENS: SegmentOptions = SegmentOptions {
    allow_single_char_full: false,
    allow_partial: true,
};

/// 输入长度护栏（公开常量，前端可对齐）。
///
/// 切分 DFS 按字节逐级递归（每层前进 1 字节），递归深度 = 输入字节数，
/// 且 memo 每个位置物化最多 max 条完整切分（O(N²) token 内存）。
/// 实测 ~2KB 输入即耗尽 1MB 线程栈（栈溢出 = SIGSEGV，
/// catch_unwind 无法拦截）。正常拼音输入（含长地址句子）远不到 256 字节，
/// 超限输入按「无法切分」处理（空结果），调用方走各自的兜底路径。
pub const MAX_INPUT_BYTES: usize = 256;

/// 枚举输入切分的单一核心 DFS。
///
/// 产出 Token 序列（Full/Initial/Shengmu 三态，信息最全）。三个公开 API 是它的
/// 薄包装：segmentations/with_partial 投影为 &str 序列（tokens_to_strs），
/// segmentations_tokens 直接返回。分支顺序（决定结果排序）：Full（长到短）→
/// Shengmu（zh/sh/ch）→ Initial（单字符兜底），与历史实现逐位一致；Shengmu
/// 放行条件统一为「有后续 token 或紧邻前是 Full」（消除 with_partial 与 tokens
/// 的条件漂移——with_partial 注释声称与 tokens 一致，实现却缺放行路径）。
// 嵌套 dfs 的 `input` 形参只随递归下传（token 切片需要 &str 来源）；
// 参数级 lint 只能由最外层 item 的 allow 覆盖（挂 dfs 自身不生效）。
#[allow(clippy::only_used_in_recursion)]
fn segment_tokens_with_opts(
    input: &str,
    max_results: usize,
    opts: SegmentOptions,
) -> Vec<Vec<Token<'_>>> {
    if input.is_empty() {
        return vec![vec![]];
    }
    if input.len() > MAX_INPUT_BYTES {
        return Vec::new();
    }
    let bytes = input.as_bytes();
    let mut memo: HashMap<(usize, PrevKind), Vec<Vec<Token>>> = HashMap::new();

    fn dfs<'a>(
        input: &'a str,
        bytes: &'a [u8],
        pos: usize,
        max: usize,
        memo: &mut HashMap<(usize, PrevKind), Vec<Vec<Token<'a>>>>,
        prev: PrevKind,
        opts: SegmentOptions,
    ) -> Vec<Vec<Token<'a>>> {
        if pos == bytes.len() {
            return vec![vec![]];
        }
        if let Some(res) = memo.get(&(pos, prev)) {
            return res.clone();
        }

        let mut results: Vec<Vec<Token>> = Vec::new();
        let max_len = 6.min(bytes.len() - pos);

        // 显式音节分隔符：跳过（prev 保持——分隔符不是 token，`guang'zh` 里
        // zh 仍是 Full 之后的声母段）
        if bytes[pos] == b'\'' {
            return dfs(input, bytes, pos + 1, max, memo, prev, opts);
        }

        // 分支清单（顺序契约不变：Full 长到短 → Shengmu → Initial）。
        // 分支配额保留（结构性护栏）：贪长分支的退化子树产出条数随串长乘法
        // 增长（reng|u|o 类误并 + 单字符兜底续命），远超 max 把名额吃光 → 正解
        // 兄弟（如 ren）一条都进不了枚举。纯扩 cap 永远追不上（b 尾 512 够、
        // zh 尾 2048 才够、16 音节+简拼尾再爆）→ 改为 Full 兄弟间让位：同层
        // 每个长度档保证至少 1 条名额（reserve = 剩余 Full 兄弟数），贪长档
        // 至多用 max − reserve 条；兄弟档的第一条即其贪长最优切分（正解所在，
        // 由调用方打分竞争选中：正解 1000+ 分 vs 垃圾延续全 0 分）。
        // 让位只对 Full 兄弟计算，Shengmu/Initial 分支不预留名额：
        // 它们在同层是「另起炉灶」而非同一路线的替代最优——预留会把
        // zh|o|n|g|fan|… 这类重开头的伪切分放进枚举，
        // 高频单字堆分压过正解（zhongfanzuohaole 1022>1020，两条例句整句
        // 候选消失）；旧行为本就只在 Full 系产不满 max 时才轮到它们。
        // 非退化态行为逐位不变：所有分支实际产出总和 ≤ max 时配额不触发。
        let min_full_len = if opts.allow_single_char_full { 1 } else { 2 };
        let full_cands: Vec<&str> = (min_full_len..=max_len)
            .rev()
            .filter_map(|len| str::from_utf8(&bytes[pos..pos + len]).ok())
            .filter(|s| is_valid_syllable_pub(s))
            .collect();
        let shengmu_cand: Option<&str> = if opts.allow_partial && pos + 2 <= bytes.len() {
            str::from_utf8(&bytes[pos..pos + 2])
                .ok()
                .filter(|s| *s == "zh" || *s == "sh" || *s == "ch")
        } else {
            None
        };
        let initial_cand: Option<char> = if opts.allow_partial && bytes[pos].is_ascii_alphabetic() {
            Some(bytes[pos] as char)
        } else {
            None
        };
        let n_branches =
            full_cands.len() + shengmu_cand.is_some() as usize + initial_cand.is_some() as usize;
        if n_branches == 0 {
            memo.insert((pos, prev), results.clone());
            return results;
        }

        // Full 分支（长到短）
        for (bi, s) in full_cands.iter().enumerate() {
            let reserve = full_cands.len() - bi - 1;
            let cap = max.saturating_sub(reserve).max(1);
            let suffixes = dfs(input, bytes, pos + s.len(), max, memo, PrevKind::Full, opts);
            for mut suf in suffixes {
                let mut seg = Vec::with_capacity(1 + suf.len());
                seg.push(Token::Full(s));
                seg.append(&mut suf);
                results.push(seg);
                if results.len() >= max {
                    memo.insert((pos, prev), results.clone());
                    return results;
                }
                if reserve > 0 && results.len() >= cap {
                    break; // 本分支让位（保留后续分支名额）
                }
            }
        }

        // Shengmu 分支：zh/sh/ch（仅 allow_partial 语义启用——segmentations
        // 全有效语义无此分支，zh 不是有效音节）。
        // 放行条件（全语义统一）：
        // ① 有后续 token（zhgrm → [Shengmu(zh), I(g), I(r), I(m)]）；
        // ② 末尾但紧邻前面是 Full（guangzh → [Full(guang), Shengmu(zh)]，
        //    「广州」可达）。纯简拼末尾（zh 单独 / 前一个 token 是 Initial 或
        //    Shengmu）仍禁——拆成 z+h 保双首字母词（智慧等）。
        if let Some(s) = shengmu_cand {
            let suffixes = dfs(input, bytes, pos + 2, max, memo, PrevKind::Shengmu, opts);
            for mut suf in suffixes {
                if !suf.is_empty() || prev == PrevKind::Full {
                    let mut seg = Vec::with_capacity(1 + suf.len());
                    seg.push(Token::Shengmu(s));
                    seg.append(&mut suf);
                    results.push(seg);
                    if results.len() >= max {
                        memo.insert((pos, prev), results.clone());
                        return results;
                    }
                }
            }
        }

        // 单字符作 Initial（末分支；allow_partial 时，不论是否有效音节 a/e/m/o）
        if let Some(c) = initial_cand {
            let suffixes = dfs(input, bytes, pos + 1, max, memo, PrevKind::Initial, opts);
            for mut suf in suffixes {
                let mut seg = Vec::with_capacity(1 + suf.len());
                seg.push(Token::Initial(c));
                seg.append(&mut suf);
                results.push(seg);
                if results.len() >= max {
                    break;
                }
            }
        }

        memo.insert((pos, prev), results.clone());
        results
    }

    dfs(
        input,
        bytes,
        0,
        max_results,
        &mut memo,
        PrevKind::None,
        opts,
    )
}

/// Token 序列投影为 input 的 &str 切片序列。
///
/// segmentations / with_partial 的消费形态是 &str 序列。Full/Shengmu 直接取
/// 切片（DFS 产出时即借用 input，无拷贝）；Initial(char) 需从 input 定位——
/// 输入 buffer 只含 [a-z']，从上一个 token 结束处跳过 ' 顺序扫描匹配，
/// 与 DFS 的推进逻辑一致（总成本 O(input_len)，max=16 枚举下可忽略）。
pub(crate) fn tokens_to_strs<'a>(input: &'a str, tokens: &[Token<'a>]) -> Vec<&'a str> {
    let mut out = Vec::with_capacity(tokens.len());
    // 当前扫描字节位置（只对 Initial 定位需要；Full/Shengmu 用切片指针偏移更新）
    let mut scan_from = 0usize;
    let input_bytes = input.as_bytes();
    for t in tokens {
        match t {
            Token::Full(s) | Token::Shengmu(s) => {
                out.push(*s);
                // s 是 input 的子切片（DFS 借用），结束偏移 = 起始指针差 + 长度
                // （同一 allocation 内的指针运算，安全）
                scan_from = (s.as_ptr() as usize - input.as_ptr() as usize) + s.len();
            }
            Token::Initial(c) => {
                let mut i = scan_from;
                while i < input_bytes.len() {
                    if input_bytes[i] == b'\'' {
                        i += 1;
                        continue;
                    }
                    if input_bytes[i] as char == *c {
                        out.push(&input[i..i + 1]);
                        scan_from = i + 1;
                        break;
                    }
                    i += 1;
                }
            }
        }
    }
    out
}

/// 枚举输入的所有「全有效音节」切分方案（上限 max_results）。
///
/// 与 `parse_pinyin` 不同：当前位置无法构成有效音节时直接返回空，
/// 不会把单个字符硬塞成音节。这样 "neng" 不会拆出 "nen+g" 这种垃圾切分。
///
/// 例如 "keneng" → ["ke", "neng"] 和 ["ken", "eng"]；
/// "xian" → ["xian"] 和 ["xi", "an"]。
/// 若输入含不完整音节（如 "muq"），返回空 Vec，调用方应回退到 `parse_pinyin`。
///
/// 实现：统一核心 `segment_tokens_with_opts` 的薄包装
/// （SEG_FULL_ONLY：全有效 + 单字符有效音节算 Full），行为逐位不变
/// （等价性由 test_segmentations_matches_core_projection 对拍守护）。
pub fn segmentations(input: &str, max_results: usize) -> Vec<Vec<&str>> {
    segment_tokens_with_opts(input, max_results, SEG_FULL_ONLY)
        .iter()
        .map(|tokens| tokens_to_strs(input, tokens))
        .collect()
}

/// 枚举输入的切分方案，允许末尾包含不完整音节（单个声母字符）。
///
/// 与 `segmentations` 的区别：当某个位置无法匹配任何有效音节时，
/// 允许把该位置的单个 ASCII 字符作为"不完整音节"加入结果，
/// 然后继续从下一个位置递归。
///
/// 这样 `bydi` 能产生 `["b", "y", "di"]`（b/y 不完整，di 完整），
/// `yig` 能产生 `["yi", "g"]`（yi 完整，g 不完整）。
///
/// 全有效音节切分（如果存在）排在前面，含不完整音节的切分排在后面。
///
/// 实现：统一核心 `segment_tokens_with_opts` 的薄包装
/// （SEG_PARTIAL：全拼 + 不完整音节）。
/// 行为变化点（修复 Shengmu 条件漂移）：zh/sh/ch 放行条件对齐 tokens——
/// 「有后续 token 或紧邻前是 Full」（早期实现仅「紧邻前是 Full」，注释却自称
/// 与 tokens 一致）。实际受影响输入仅「非简拼 + 非 zh/sh/ch 开头 + 含
/// zh/sh/ch + 有后续 token」的边缘混合（如 guzhx → 现含 [gu,zh,x] 切分，
/// 声母组合更合理；xzhg 类被 is_jianpin 分流截走，生产路径基本不可达）。
pub fn segmentations_with_partial(input: &str, max_results: usize) -> Vec<Vec<&str>> {
    segment_tokens_with_opts(input, max_results, SEG_PARTIAL)
        .iter()
        .map(|tokens| tokens_to_strs(input, tokens))
        .collect()
}

/// 打字进行中形态的保底切分：「完整音节前缀 + 尾部未拼完字母」合成变体
///（如 `dang…zhineng` 后多打一个下句首字母 `d` 的绿词消失场景）。
///
/// 背景：`segmentations`（全有效语义）对含不完整尾的输入整体返回空；
/// 含尾枚举（with_partial/tokens）的 DFS 名额又被贪长退化子树挤满——实测
/// `dangnishiyongrengongzhinengd` 的正解 `…ren|gong|…`（1053 分）落在 cap 之外
/// 从未被枚举，1039 分的 `…reng|o|n|g…`（reng 误并 + 单字符续命）胜出，
/// best_syllables 垃圾化 → 整句 DP 无法组句 → 绿词整体消失。而「≥3 完整音节
/// 前缀 + 1-2 个未拼完字母」正是长句逐键打字的恒常态，必须恒在候选切分里。
///
/// 做法：输入无法全有效切分时，找最长的可全有效切分前缀，尾部字母逐段追加
///（贪心最长有效音节优先——尾部已敲完整的音节整段；无有效音节开头时
/// zh/sh/ch 双字母声母一段，其余单字母一段），合成保底变体。
/// 打分层（score_for_syllables）把它与既有枚举变体同台竞争，正解分高自然胜出；
/// 与既有变体重合时由调用方去重，零行为变化。
///
/// 不触发形态（与前缀回退解码同边界）：
/// - 已能全有效切分的输入（正常完成态）直接返回空——零行为变化的硬保证；
/// - 前缀含中置简拼（qiehuanshurufzhihou 的 f）时任何切分点都切不出全有效
///   前缀 → 返回空，混合简拼意图不受影响。
pub fn segmentations_prefix_tail(input: &str) -> Vec<Vec<&str>> {
    if input.is_empty() || input.len() > MAX_INPUT_BYTES {
        return Vec::new();
    }
    // 全有效切分已存在的输入（正常完成态）不合成
    if !segmentations(input, 1).is_empty() {
        return Vec::new();
    }
    let n = input.len();
    // 尾部最多剥 6 字母（最长音节长度；zh/sh/ch 双字母声母 + 单字母兜底都覆盖）
    for tail_len in 1..=6.min(n - 1) {
        let cut = n - tail_len;
        let tail = &input[cut..];
        // 尾部必须是纯 [a-z]（不吞用户显式撇号边界）
        if !tail.bytes().all(|b| b.is_ascii_lowercase()) {
            continue;
        }
        let prefix = &input[..cut];
        if prefix.is_empty() || prefix.ends_with('\'') {
            continue;
        }
        let prefix_segs = segmentations(prefix, 4);
        if prefix_segs.is_empty() {
            continue;
        }
        // 尾部逐段：贪心最长有效音节优先——尾部里
        // 已经敲完整的音节（pengbfzhan 的 zhan）必须整体成段，与候选链 token
        // 语义（Full 整段）一致。若一律按声母/单字母拆会把 zhan 拆成
        // zh|a|n：显示/计数/消费链数出 6 音节，而 4 字候选「蓬勃发展」只对应
        // 4 音节 →「选 4 字词只消费前 4 音节」在 zh 处截断 → 提交后残留 an
        //（消费链同病）。
        // 无有效音节开头时才退化：zh/sh/ch 双字母声母一段（与 Shengmu 对齐，
        // guangzh 类形态合成 [guang, zh] 与既有枚举重合去重），其余单字母一段，
        // 单字母交给消费侧既有悬尾/简拼语义（partial tail 补字、initial 模式）。
        let mut tail_segs: Vec<&str> = Vec::new();
        let mut i = 0usize;
        while i < tail.len() {
            // 尾段已断言纯 [a-z]（上方守卫），字节切片安全；最长音节 6 字母
            //（zhuang/shuang 类），尾段本身 ≤6 字母
            let rest = &tail[i..];
            let mut step = 0usize;
            for len in (1..=rest.len()).rev() {
                if is_valid_syllable_pub(&rest[..len]) {
                    step = len;
                    break;
                }
            }
            if step == 0 {
                step = if rest.len() >= 2 && matches!(&rest[..2], "zh" | "sh" | "ch") {
                    2
                } else {
                    1
                };
            }
            tail_segs.push(&rest[..step]);
            i += step;
        }
        return prefix_segs
            .into_iter()
            .map(|mut seg| {
                seg.extend(tail_segs.iter().copied());
                seg
            })
            .collect();
    }
    Vec::new()
}

/// Token 化切分：把输入切成 Token 序列的所有可能方案。
///
/// 相比 `segmentations_with_partial`，新增 Shengmu(zh/sh/ch) 切分路径，
/// 让 zhgrm 等双字母声母+简拼输入能被识别。
///
/// Shengmu 生效条件：
/// 1. 「声母+后续 token」场景（递归剩余非空）——zhgrm → [Shengmu(zh), I(g), I(r), I(m)]；
/// 2. 末尾 Shengmu 且紧邻前面是 Full 音节（guangzh → [Full(guang), Shengmu(zh)]，
///    「广州」可达）。若只允许条件 1，`guangzh` 的 zh 在末尾被拆成 I(z)+I(h)
///    （广州会），「广州」永远不可达。
///    条件 2 不放行纯简拼（`zh` 单独输入 prev=None → 仍拆 z+h，保「智慧」等
///    双首字母词）。
/// 单字符一律归 Initial（不论是否 a/e/m/o 有效音节），这是「单字符严格等于首字母」
/// 决策的体现（详见 spec §2.2）。
///
/// 实现：统一核心 `segment_tokens_with_opts` 的薄包装
/// （SEG_TOKENS：混合语义 + 不完整音节），行为逐位不变
/// （等价性由 test_tokens_unchanged 对拍守护）。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum PrevKind {
    /// 起始 / 分隔符后的首个 token 之前
    None,
    Full,
    Initial,
    Shengmu,
}

pub fn segmentations_tokens(input: &str, max_results: usize) -> Vec<Vec<Token<'_>>> {
    segment_tokens_with_opts(input, max_results, SEG_TOKENS)
}

/// 判断输入是否「看起来像打错了」（历史实现，已被 `is_legal_jianpin_mix` 取代）。
///
/// 当前无生产调用方：智能纠错触发判定（`Engine::maybe_correct`）现使用
/// `segmentations` + `is_legal_jianpin_mix` 组合，本函数仅保留供测试对照
/// （`test_is_likely_typo` 验证拼写错误特征识别能力，确保纠错路径不被破坏）。
/// 两者判定逻辑相近但有差异：本函数基于「单字母分布特征」，新实现基于
/// 「合法简拼混合结构」，新实现更精确（能区分 `yunxs` 合法 vs `nihsou` 非法）。
///
/// 与 `segmentations` 的区别：后者要求全部音节完整才返回非空，对正常简拼尾部
/// 输入（如 `zhongg` = zhong+g，用户打「中国」简拼）也返回空，导致纠错误触发。
///
/// 本函数用贪心 `parse_pinyin` 切分后，识别「真正像拼错」的特征：
/// - 出现两个相邻的单字母音节（如 `hs`、`fs`）——不是合法声母组合；
/// - 单字母音节夹在两个完整音节中间（如 `ni-h-sou` 的 h）——孤立字符。
///
/// 正常的「完整音节 + 末尾声母」（如 `zhongg`/`beij`/`shangh`）只产生末尾
/// 单字母，不算拼错，返回 `false`，纠错跳过。
///
/// 返回 `true` 表示像拼错、建议触发纠错。
#[allow(dead_code)]
pub fn is_likely_typo(input: &str) -> bool {
    if input.len() < 2 {
        return false;
    }
    // 非纯小写字母不纠错（含分隔符或非字母）
    if !input.chars().all(|c| c.is_ascii_lowercase()) {
        return false;
    }
    // 全部完整音节 → 正常输入，不纠错
    if !segmentations(input, 1).is_empty() {
        return false;
    }
    // 贪心切分，分析单字母片段的分布
    let parsed = parse_pinyin(input);
    let seg_lens: Vec<usize> = parsed.syllables.iter().map(|s| s.end - s.start).collect();
    if seg_lens.is_empty() {
        return false;
    }
    // 特征 1：相邻两个单字母片段（非法声母组合，如 hs/fs/ht）
    for w in seg_lens.windows(2) {
        if w[0] == 1 && w[1] == 1 {
            return true;
        }
    }
    // 特征 2：单字母夹在两个完整音节中间（孤立字符，如 ni-h-sou 的 h）
    //   即：存在 i，seg_lens[i]==1 且 i 非首非末，且前后都是完整音节(>1)
    for i in 1..seg_lens.len().saturating_sub(1) {
        if seg_lens[i] == 1 && seg_lens[i - 1] > 1 && seg_lens[i + 1] > 1 {
            return true;
        }
    }
    false
}

/// 判断输入是否为简拼模式（每个字符独立作为首字母）
///
/// 简拼判定标准：
/// - 长度 ≥ 2（分类不设上限，见下）
/// - 全部是小写 ASCII 字母
/// - 任何连续两个字符都不能构成有效音节
///   （如果有连续字符能构成有效音节，说明用户在打全拼，如 "ai" 不是简拼）
///
/// 例如 "zw" → true，"agz" → true（a/g/z 任意两个连续都不能构成有效音节），
/// "ai" → false（"ai" 是有效音节，用户在打全拼）。
///
/// 无长度上限：长度上限判据与下方两条语义判据冗余且有害——真实全拼必含
/// 元音（i/u 任意位置、a/e/o 非首位即拒），长度上限只会挡住"纯首字母长串"
/// （如连输 7 个 z，前 6 个正常出词，第 7 个被踢出简拼路径 → 全拼路径切不出
/// 有效音节 → 候选框完全断词）。混合声母路径（zh/sh/ch 开头，见
/// `Engine::is_hybrid_shengmu_input`）本就无长度上限，纯简拼与之对齐。
/// 分类不设限 ≠ 无护栏：算力大头各自有界——模糊展开笛卡尔积 ≤
/// `MAX_JIANPIN_EXPANSIONS`（超限回退原串）、整句猜测 DP ≤ MAX_GREEN_CHARS
/// 字母（`process_jianpin_inner` 内，超限跳过猜测但查词/单字照常）。
/// 所以任意长度都不断词、也不会卡住单线程服务。
///
/// a/e/o 判据用「全拼前缀」测试：a/e/o 可作零声母音节（安/嗯/哦…）的首字母，
/// 是合法简拼写法——西安=xa、西安科技=xakj，`zgrmjfjxa`（中国人民解放军西安）
/// 必须走简拼。若按「a/e/o 非首位即非简拼」会把这类输入整条踢出简拼路径 →
/// 候选塌成单字。但全拼中间态（zho→zhong、zhon）也长这样，不能一刀放宽——
/// 改为两条判据取或：① 整串是某个全拼音节序列的前缀（含末音节半截，
/// [`is_pinyin_prefix`]——zho/zhon/zong 命中，zgrmjfjxa 首字母无法成音节）；
/// ② 整串嵌含 ≥2 字符完整音节（[`has_embedded_full_syllable`]——hdong=h+dong、
/// zhonggr=zhong+g+r 这类「全拼+半截声母」混合形态；仅判据 ① 对 h 开头
/// 整串解析不了，会把它们误判简拼塌成单字，故 ② 必不可少）。
/// 两条都不命中才判简拼（zgrmjfjxa/xa/agz → 简拼）。
pub fn is_jianpin(input: &str) -> bool {
    if input.len() < 2 {
        return false;
    }
    if !input.chars().all(|c| c.is_ascii_lowercase() || c == '\'') {
        return false;
    }
    // 如果任何连续两个非分隔符字符能构成有效音节，说明用户在打全拼，不是简拼。
    // ai → false（ai 是有效音节），ba → false（ba 是有效音节）
    let chars: Vec<char> = input.chars().filter(|&c| c != '\'').collect();
    for i in 0..chars.len().saturating_sub(1) {
        let pair: String = chars[i..=i + 1].iter().collect();
        if is_valid_syllable_pub(&pair) {
            return false;
        }
    }
    // i/u 在任何位置都判非简拼：无音节以 i-/u- 开头（yi/wu 用 y/w），
    // i/u 出现即用户在打全拼韵母（shi/zhu 类）。
    let mut need_prefix_test = false;
    for (i, &c) in chars.iter().enumerate() {
        if c == 'i' || c == 'u' {
            return false;
        }
        if (c == 'a' || c == 'e' || c == 'o') && i > 0 {
            // a/e/o 非首位：可能是简拼首字母（西安=xa），也可能是全拼中间态
            //（zho→zhong）——由「是否全拼前缀」裁决，见函数头注释。
            need_prefix_test = true;
        }
    }
    if need_prefix_test && (is_pinyin_prefix(&chars) || has_embedded_full_syllable(&chars)) {
        return false;
    }
    true
}

/// 判断 `chars` 是否为「某个全拼音节序列的前缀」（含末音节半截态）。
///
/// 语义：存在音节序列 s1·s2·…，使 chars 是其前缀（chars 恰等于整串，或
/// chars 结束在某个音节的中间）。例：
/// - `zho` → true（zhong / zhou 的前缀，全拼中间态）
/// - `zong` → true（自身即音节）
/// - `zhongg` → true（zhong 完整 + g 是 ga… 的前缀，全拼+声母尾形态）
/// - `zgrmjfjxa` → false（z 后接 g 无任何音节匹配，非全拼）
///
/// 仅被 [`is_jianpin`] 的 a/e/o 判据调用（输入 ≤ 256 字节 + memo，成本可控）。
pub fn is_pinyin_prefix(chars: &[char]) -> bool {
    fn go(chars: &[char], pos: usize, memo: &mut [Option<bool>]) -> bool {
        if pos == chars.len() {
            return true;
        }
        if let Some(v) = memo[pos] {
            return v;
        }
        let rem = chars.len() - pos;
        let mut ok = false;
        for syl in VALID_SYLLABLES {
            let sb = syl.as_bytes();
            if sb.len() <= rem {
                // 完整消费该音节，继续看剩余
                if chars[pos..pos + sb.len()]
                    .iter()
                    .zip(sb)
                    .all(|(c, b)| *c as u8 == *b)
                    && go(chars, pos + sb.len(), memo)
                {
                    ok = true;
                    break;
                }
            } else if chars[pos..].iter().zip(sb).all(|(c, b)| *c as u8 == *b) {
                // 剩余部分是某音节的半截前缀（如 zho ⊂ zhong）
                ok = true;
                break;
            }
        }
        memo[pos] = Some(ok);
        ok
    }
    let mut memo = vec![None; chars.len() + 1];
    go(chars, 0, &mut memo)
}

/// 判断 `chars` 中是否「嵌含完整音节（≥2 字符）」，且其余字符全为纯辅音
/// 字母（即半截声母/首字母形态）。
///
/// 服务对象：`hdong`（h+dong）、`zhonggr`（zhong+g+r）这类「全拼 + 半截
/// 声母」混合形态——旧 a/e/o 判据（非首位即全拼）天然放它们走全拼路径，
/// 前缀测试（[`is_pinyin_prefix`]）覆盖不到（h 开头整串解析不了）。
/// 覆盖面由 `hdong`/`zhonggr`/`zhonggrm` 及消费链/诊断矩阵相关测试守护，
/// 判据取「存在切分使每个元音字母都归入 ≥2 字符的完整音节，其余字符全是辅音」。
///
/// 与「连续两字符成音节」检查无交集是此形态能触达本函数的原因：
/// dong 的子串 do/on/ng 皆非音节（ong 族整体如此），pair 检查放行后
/// 才轮到 a/e/o 判据——这正是 `hdong` 类输入的历史通路。
///
/// 语义验证：
/// - `hdong` → true（跳过 h + 消费 dong）
/// - `zhonggr` → true（zhong + 跳过 g/r）
/// - `zgrmjfjxa` → false（无 ≥2 完整音节；尾 a 是元音、无法被辅音段吸收）
/// - `xa` → false（同）
fn has_embedded_full_syllable(chars: &[char]) -> bool {
    fn is_consonant(c: char) -> bool {
        c != 'a' && c != 'e' && c != 'i' && c != 'o' && c != 'u'
    }
    fn go(chars: &[char], pos: usize, seen: bool, memo: &mut [[Option<bool>; 2]]) -> bool {
        if pos == chars.len() {
            return seen;
        }
        if let Some(v) = memo[pos][seen as usize] {
            return v;
        }
        let mut ok = false;
        // ① 纯辅音字母：逐字符跳过（半截声母/简拼首字母位置）
        if is_consonant(chars[pos]) && go(chars, pos + 1, seen, memo) {
            ok = true;
        }
        // ② 完整音节 token（≥2 字符）
        if !ok {
            for syl in VALID_SYLLABLES {
                let sb = syl.as_bytes();
                if sb.len() >= 2
                    && pos + sb.len() <= chars.len()
                    && chars[pos..pos + sb.len()]
                        .iter()
                        .zip(sb)
                        .all(|(c, b)| *c as u8 == *b)
                    && go(chars, pos + sb.len(), true, memo)
                {
                    ok = true;
                    break;
                }
            }
        }
        memo[pos][seen as usize] = Some(ok);
        ok
    }
    let mut memo = vec![[None, None]; chars.len() + 1];
    go(chars, 0, false, &mut memo)
}

/// 为简拼生成可能的音节序列（未实现，保留为占位）。
///
/// 简拼展开是组合爆炸问题（每个首字母对应多个声母/音节，笛卡尔积巨大），
/// 实际引擎中采用更聪明的方式：在 `Dict::query_jianpin` 里直接用首字母前缀
/// 遍历 Trie 子树，不做显式展开。因此本函数恒返回空 Vec。
///
/// 保留函数签名是为了未来若需要显式展开（如精确简拼统计）时的接入点，
/// 当前无任何调用方。
#[allow(dead_code)]
pub fn expand_jianpin(_input: &str) -> Vec<Vec<String>> {
    // 详见上文 doc 注释：实际简拼匹配在 Dict::query_jianpin 中完成，此处不展开。
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 配置契约对拍：segmentations 包装 == 核心(SEG_FULL_ONLY) 投影。
    ///
    /// 包装就是「核心 + 固定参数 + 投影」的同义反复，本测试守护的是配置契约：
    /// 未来若有人改 SEG_FULL_ONLY 参数或改包装投影逻辑，这里立即失败——防止
    /// 「全有效语义」被意外混入 partial/混合语义。输入集覆盖全拼/分隔符/悬尾/
    /// 零声母/长句边界。
    #[test]
    fn test_segmentations_matches_core_projection() {
        let inputs = [
            "nihao",
            "keneng",
            "xian",
            "aoliao",
            "du'an",
            "muq",
            "kaix",
            "wenmin",
            "zhongwen",
            "xianer",
            "nengjiejuewenti",
            "bydi",
            "guangzh",
        ];
        for input in inputs {
            for max in [1usize, 4, 8, 16] {
                let via_api = segmentations(input, max);
                let via_core: Vec<Vec<&str>> = segment_tokens_with_opts(input, max, SEG_FULL_ONLY)
                    .iter()
                    .map(|t| tokens_to_strs(input, t))
                    .collect();
                assert_eq!(
                    via_api, via_core,
                    "segmentations({input}, {max}) 与核心投影不一致"
                );
            }
        }
    }

    /// 配置契约对拍：with_partial 包装 == 核心(SEG_PARTIAL) 投影。
    /// 覆盖 partial 边界（悬尾声母/zh/sh/ch 尾/单字符有效音节）。
    #[test]
    fn test_with_partial_matches_core_projection() {
        let inputs = [
            "bydi", "yig", "guangzh", "mzh", "nihsou", "xzhg", "guzhx", "du'an", "aoliao", "kaix",
            "zhongg", "shangh",
        ];
        for input in inputs {
            for max in [1usize, 4, 8, 16] {
                let via_api = segmentations_with_partial(input, max);
                let via_core: Vec<Vec<&str>> = segment_tokens_with_opts(input, max, SEG_PARTIAL)
                    .iter()
                    .map(|t| tokens_to_strs(input, t))
                    .collect();
                assert_eq!(
                    via_api, via_core,
                    "segmentations_with_partial({input}, {max}) 与核心投影不一致"
                );
            }
        }
    }

    /// 配置契约对拍：tokens 包装 == 核心(SEG_TOKENS) 直出。
    /// tokens 是唯一无投影损耗的包装（Token 序列直接返回），逐 Token 相等。
    #[test]
    fn test_tokens_unchanged() {
        let inputs = [
            "zhonggrm",
            "zhgrm",
            "zgrm",
            "zhongguo",
            "zh",
            "guangzh",
            "xzhg",
            "zhonggrenm",
            "wanmeipipei",
            "tingshuo",
            "du'an",
        ];
        for input in inputs {
            for max in [1usize, 4, 8, 16] {
                let via_api = segmentations_tokens(input, max);
                let via_core = segment_tokens_with_opts(input, max, SEG_TOKENS);
                assert_eq!(
                    via_api, via_core,
                    "segmentations_tokens({input}, {max}) 与核心直出不一致"
                );
            }
        }
    }

    /// 行为增强点（Shengmu 条件漂移修复）：with_partial 的
    /// zh/sh/ch 放行条件对齐 tokens——「有后续 token 或紧邻前是 Full」。
    /// xzhg（x 是 Initial，zh 后有 g）：仅按旧条件只有 [x,z,h,g]（zh 前非 Full 不放行），
    /// 现含 [x,zh,g]（声母组合更合理，与 zhgrm 双试同款）。
    #[test]
    fn test_shengmu_tail_alignment() {
        let segs = segmentations_with_partial("xzhg", 16);
        assert!(
            segs.iter().any(|s| s == &["x", "zh", "g"]),
            "xzhg 应含 [x, zh, g]（Shengmu 对齐后），实际: {:?}",
            segs
        );
        // 纯简拼末尾 zh 仍拆 z+h（guangzh 语义不受影响）
        let segs2 = segmentations_with_partial("zh", 16);
        assert!(
            !segs2.iter().any(|s| s == &["zh"]),
            "单独 zh 不应整体保留，实际: {:?}",
            segs2
        );
    }

    #[test]
    fn test_parse_zhongwen() {
        let seq = parse_pinyin("zhongwen");
        assert_eq!(seq.syllables.len(), 2);
        assert_eq!(
            &seq.raw[seq.syllables[0].start..seq.syllables[0].end],
            "zhong"
        );
        assert_eq!(
            &seq.raw[seq.syllables[1].start..seq.syllables[1].end],
            "wen"
        );
    }

    #[test]
    fn test_parse_xian() {
        // "xian" 应该是单个音节，而不是 "xi" + "an"
        let seq = parse_pinyin("xian");
        assert_eq!(seq.syllables.len(), 1);
        assert_eq!(
            &seq.raw[seq.syllables[0].start..seq.syllables[0].end],
            "xian"
        );
    }

    #[test]
    fn test_segmentations() {
        let segs = segmentations("keneng", 8);
        let as_strings: Vec<Vec<String>> = segs
            .iter()
            .map(|v| v.iter().map(|s| s.to_string()).collect())
            .collect();
        assert!(as_strings.contains(&vec!["ke".to_string(), "neng".to_string()]));
        assert!(as_strings.contains(&vec!["ken".to_string(), "eng".to_string()]));

        let segs_xian = segmentations("xian", 8);
        let as_strings_xian: Vec<Vec<String>> = segs_xian
            .iter()
            .map(|v| v.iter().map(|s| s.to_string()).collect())
            .collect();
        assert!(as_strings_xian.contains(&vec!["xian".to_string()]));
        assert!(as_strings_xian.contains(&vec!["xi".to_string(), "an".to_string()]));
    }

    #[test]
    fn test_apostrophe_separator() {
        // 单引号作为显式音节分隔符
        let seq = parse_pinyin("du'an");
        let s: Vec<&str> = seq
            .syllables
            .iter()
            .map(|syl| &seq.raw[syl.start..syl.end])
            .collect();
        assert_eq!(s, vec!["du", "an"]);

        let segs = segmentations("du'an", 8);
        let as_strings: Vec<Vec<String>> = segs
            .iter()
            .map(|v| v.iter().map(|s| s.to_string()).collect())
            .collect();
        assert!(as_strings.contains(&vec!["du".to_string(), "an".to_string()]));
        // 带分隔符时不应出现单音节 "duan"
        assert!(!as_strings.iter().any(|v| v.len() == 1 && v[0] == "duan"));
    }

    #[test]
    fn test_is_jianpin() {
        assert!(is_jianpin("zw"));
        assert!(is_jianpin("zgr"));
        assert!(is_jianpin("zzzzzzz")); // 7 连 z（守护：不得引入 6 字上限导致断词）
        assert!(is_jianpin("jgrmjsr")); // 8 首字母长串
        assert!(!is_jianpin("z")); // 太短
        assert!(!is_jianpin("zhongwen")); // 含元音（长度上限已移除，元音判据独立生效）
        assert!(!is_jianpin("zho")); // 全拼中间态：是 zhong/zhou 的前缀
        assert!(!is_jianpin("123")); // 非字母
        assert!(!is_jianpin("ZW")); // 大写

        // a/e/o 作简拼首字母（如 zgrmjfjxa 场景）：
        // 是合法简拼写法，但需与全拼中间态区分——判据 = 整串是否全拼前缀。
        assert!(is_jianpin("zgrmjfjxa")); // 中国人民解放军西安（xa=西安）
        assert!(is_jianpin("zgrmjfjxakj")); // ＋科技（a 中位，非末位）
        assert!(is_jianpin("xa")); // 西安简拼（旧规则误判全拼）
        assert!(is_jianpin("agz")); // a 首位允许（原有语义保持：a 作首字母）
                                    // 全拼中间态/声母尾仍判全拼（前缀测试兜住；逐一守护防放宽误伤）
        assert!(!is_jianpin("zhon")); // zhong 前缀
        assert!(!is_jianpin("zong")); // 自身即音节
        assert!(!is_jianpin("zhuo")); // 自身即音节（pair 规则命中 hu）
        assert!(!is_jianpin("zhei")); // 含 i
        assert!(!is_jianpin("zhong")); // o 非首位 + 完整音节
        assert!(!is_jianpin("zhongg")); // 全拼+声母尾（g 是 ga… 半截 → 全拼前缀）
                                        // 「全拼 + 半截声母」混合形态不得误判简拼
                                        //（hdong/zhonggr/zhonggrm——判据 ② has_embedded_full_syllable 守护）
        assert!(!is_jianpin("hdong")); // h + dong（活动类拍扁形态）
        assert!(!is_jianpin("zhonggr")); // zhong + g + r（中国人混合简拼）
        assert!(!is_jianpin("zhonggrm")); // zhong + g + r + m（中国人民）
    }

    #[test]
    fn test_is_likely_typo() {
        // 正常输入：完整音节 + 末尾声母（简拼尾部）—— 不应判为拼错
        assert!(!is_likely_typo("zhongg"), "zhongg 是中国简拼尾，正常");
        assert!(!is_likely_typo("beij"), "beij 是北京简拼尾，正常");
        assert!(!is_likely_typo("shangh"), "shangh 是上海简拼尾，正常");
        assert!(!is_likely_typo("yig"), "yig = yi+g，正常");

        // 正常输入：全部完整音节
        assert!(!is_likely_typo("nihao"), "nihao 完整，正常");
        assert!(!is_likely_typo("kai"), "kai 完整，正常");

        // 正常输入：打字中间态（短，<5）
        assert!(!is_likely_typo("nih"), "nih 中间态：ni+h，单字母在末尾");
        // 注意：zhon → z/h/o/n（4 个相邻单字母）在语法上像拼错（is_likely_typo=true），
        // 但它其实是 zhong 前缀。这类短前缀靠 maybe_correct 的 len<5 阈值挡住，
        // 不在本函数职责内。本函数只做语法判断，长度由调用方控制。
        assert!(
            is_likely_typo("zhon"),
            "zhon 语法上像拼错（多相邻单字母），由 len 阈值挡"
        );

        // 真拼错：相邻单字母（非法声母组合）
        assert!(is_likely_typo("nihso"), "ni-h-s-o：hs 相邻单字母");
        assert!(is_likely_typo("nihsou"), "ni-h-sou：h 夹中间");
        assert!(is_likely_typo("shurufs"), "shu-ru-f-s：fs 相邻单字母尾");

        // 边界：太短不判
        assert!(!is_likely_typo("a"));
        assert!(!is_likely_typo(""));

        // 非纯小写字母不判
        assert!(!is_likely_typo("ni'hao"));
        assert!(!is_likely_typo("NiHao"));
    }

    #[test]
    fn test_token_zhonggrm_segmentation() {
        // zhonggrm 应切出 [Full(zhong), Initial(g), Initial(r), Initial(m)]
        let segs = segmentations_tokens("zhonggrm", 16);
        let has_target = segs.iter().any(|seg| {
            seg.len() == 4
                && seg[0] == Token::Full("zhong")
                && seg[1] == Token::Initial('g')
                && seg[2] == Token::Initial('r')
                && seg[3] == Token::Initial('m')
        });
        assert!(
            has_target,
            "zhonggrm 应切出 [Full(zhong), Initial(g/r/m)]，实际: {:?}",
            segs
        );
    }

    #[test]
    fn test_token_zhgrm_has_shengmu_path() {
        // zhgrm 应同时切出 Shengmu(zh) 路径和 Initial(z)+Initial(h) 路径（双试）
        let segs = segmentations_tokens("zhgrm", 16);
        let has_shengmu = segs.iter().any(|seg| {
            seg.len() == 4
                && seg[0] == Token::Shengmu("zh")
                && seg[1] == Token::Initial('g')
                && seg[2] == Token::Initial('r')
                && seg[3] == Token::Initial('m')
        });
        assert!(
            has_shengmu,
            "zhgrm 应切出 Shengmu(zh) 路径，实际: {:?}",
            segs
        );

        let has_initial = segs.iter().any(|seg| {
            seg.len() == 5 && seg[0] == Token::Initial('z') && seg[1] == Token::Initial('h')
        });
        assert!(
            has_initial,
            "zhgrm 应同时切出 Initial(z)+Initial(h) 退化路径，实际: {:?}",
            segs
        );
    }

    #[test]
    fn test_token_zgrm_all_initials() {
        // zgrm 纯简拼应切出全 Initial 序列
        let segs = segmentations_tokens("zgrm", 16);
        let has_target = segs.iter().any(|seg| {
            seg.len() == 4
                && seg[0] == Token::Initial('z')
                && seg[1] == Token::Initial('g')
                && seg[2] == Token::Initial('r')
                && seg[3] == Token::Initial('m')
        });
        assert!(has_target, "zgrm 应切出全 Initial 序列，实际: {:?}", segs);
    }

    #[test]
    fn test_token_zhongguo_all_full() {
        // zhongguo 全拼应切出全 Full 序列
        let segs = segmentations_tokens("zhongguo", 16);
        let has_target = segs.iter().any(|seg| {
            seg.len() == 2 && seg[0] == Token::Full("zhong") && seg[1] == Token::Full("guo")
        });
        assert!(has_target, "zhongguo 应切出全 Full 序列，实际: {:?}", segs);
    }

    #[test]
    fn test_token_standalone_zh_is_initials() {
        // 单独 zh（长度 2）按纯简拼处理，拆成 Initial(z)+Initial(h)。
        // Shengmu 只在「声母+后续 token」场景生效（序列长度 ≥ 2 个 token）。
        let segs = segmentations_tokens("zh", 16);
        let has_initial = segs.iter().any(|seg| {
            seg.len() == 2 && seg[0] == Token::Initial('z') && seg[1] == Token::Initial('h')
        });
        assert!(
            has_initial,
            "单独 zh 应拆成 Initial(z)+Initial(h)，实际: {:?}",
            segs
        );
        // 不应出现单独的 Shengmu(zh)（因为后面没有 token）
        let has_lone_shengmu = segs
            .iter()
            .any(|seg| seg.len() == 1 && seg[0] == Token::Shengmu("zh"));
        assert!(
            !has_lone_shengmu,
            "单独 zh 不应切出 Shengmu（无后续 token）"
        );
    }
}
