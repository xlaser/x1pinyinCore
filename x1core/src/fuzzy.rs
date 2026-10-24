//! 模糊拼音配置与扩展
//!
//! 所有模糊音规则均为纯本地映射，不依赖任何网络或外部数据。
//! 用于帮助发音不准的用户，以及支持简拼中 zh/ch/sh 的兼容。

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

/// 模糊拼音配置
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FuzzyConfig {
    pub enabled: bool,
    /// z ↔ zh
    pub z_zh: bool,
    /// c ↔ ch
    pub c_ch: bool,
    /// s ↔ sh
    pub s_sh: bool,
    /// n ↔ l
    pub n_l: bool,
    /// f ↔ h
    pub f_h: bool,
    /// an ↔ ang
    pub an_ang: bool,
    /// en ↔ eng
    pub en_eng: bool,
    /// in ↔ ing
    pub in_ing: bool,
    /// 简拼中把 zh/ch/sh 视为单个首字母，如 zhw 匹配 [zh, w]
    pub jianpin_merge_zh_ch_sh: bool,
}

impl FuzzyConfig {
    /// 从逗号分隔的开关名解析配置
    ///
    /// 例如 "z_zh,c_ch,s_sh,jianpin_merge_zh_ch_sh" 表示开启这三项。
    /// 若包含 "enabled" 则总开关开启。
    pub fn from_csv(s: &str) -> Self {
        let mut cfg = Self::default();
        for part in s.split(',') {
            match part.trim() {
                "enabled" => cfg.enabled = true,
                "z_zh" => {
                    cfg.enabled = true;
                    cfg.z_zh = true;
                }
                "c_ch" => {
                    cfg.enabled = true;
                    cfg.c_ch = true;
                }
                "s_sh" => {
                    cfg.enabled = true;
                    cfg.s_sh = true;
                }
                "n_l" => {
                    cfg.enabled = true;
                    cfg.n_l = true;
                }
                "f_h" => {
                    cfg.enabled = true;
                    cfg.f_h = true;
                }
                "an_ang" => {
                    cfg.enabled = true;
                    cfg.an_ang = true;
                }
                "en_eng" => {
                    cfg.enabled = true;
                    cfg.en_eng = true;
                }
                "in_ing" => {
                    cfg.enabled = true;
                    cfg.in_ing = true;
                }
                "jianpin_merge_zh_ch_sh" => cfg.jianpin_merge_zh_ch_sh = true,
                _ => {}
            }
        }
        cfg
    }

    /// 序列化为逗号分隔的开关名
    pub fn to_csv(&self) -> String {
        let mut parts = Vec::new();
        if self.enabled {
            parts.push("enabled");
        }
        if self.z_zh {
            parts.push("z_zh");
        }
        if self.c_ch {
            parts.push("c_ch");
        }
        if self.s_sh {
            parts.push("s_sh");
        }
        if self.n_l {
            parts.push("n_l");
        }
        if self.f_h {
            parts.push("f_h");
        }
        if self.an_ang {
            parts.push("an_ang");
        }
        if self.en_eng {
            parts.push("en_eng");
        }
        if self.in_ing {
            parts.push("in_ing");
        }
        if self.jianpin_merge_zh_ch_sh {
            parts.push("jianpin_merge_zh_ch_sh");
        }
        parts.join(",")
    }

    /// 判断当前是否没有任何模糊规则生效
    pub fn is_empty(&self) -> bool {
        !self.enabled
            || (!self.z_zh
                && !self.c_ch
                && !self.s_sh
                && !self.n_l
                && !self.f_h
                && !self.an_ang
                && !self.en_eng
                && !self.in_ing)
    }

    /// 扩展一个音节为其所有等价音节（含自身）。
    /// 结果仅保留有效音节。
    ///
    /// **仅适用于完整音节**（全拼序列扩展 `expand_syllables` 等）。
    /// 前缀类查询（裸声母/半截音节，如 "z"、"zho"）必须用 [`Self::expand_prefix`]：
    /// 本函数的有效性过滤会把它们整组清空（"z" 不是有效音节 → 空集）。
    pub fn expand_syllable(&self, s: &str) -> Vec<String> {
        self.expand_variants(s)
            .into_iter()
            .filter(|x| crate::pinyin::is_valid_syllable_pub(x))
            .collect()
    }

    /// 扩展为等价**前缀**变体（含自身；**不过滤有效音节**）。
    ///
    /// 根因背景（「开了模糊音后简拼单字候选全消失 / 长简拼彻底断词」）：
    /// `query_single_chars_fuzzy` / `query_prefix_filtered_fuzzy` 的前缀参数是
    /// **裸声母**（"z"/"w"）或**半截音节**（trailing partial "r"、"zho"），
    /// 而 expand_syllable 末尾的 `is_valid_syllable_pub` 过滤会把它们清空
    ///（"z" 不是完整音节）→ 模糊分支的 for 循环零次迭代 → 单字补充/尾部半截
    /// 候选在开启模糊音（z_zh / in_ing 任一对）后全部消失；纯简拼长串（无绿词
    /// 可拼时）表现为完全断词。未开模糊音的配置（is_empty 短路走直查分支）不受
    /// 影响——因此该问题只在特定配置下暴露。
    /// 前缀语义下变体本就不要求是完整音节（dict 侧按前缀匹配），不过滤正确；
    /// 顺带带来模糊音的预期行为：z_zh 开启后输入 "z" 的单字补充也会含 zh 系单字。
    pub(crate) fn expand_prefix(&self, s: &str) -> Vec<String> {
        self.expand_variants(s).into_iter().collect()
    }

    /// 变体集合（含自身，应用全部启用的模糊替换规则；**不做音节有效性过滤**）。
    fn expand_variants(&self, s: &str) -> BTreeSet<String> {
        let mut set: BTreeSet<String> = BTreeSet::new();
        set.insert(s.to_string());

        if !self.enabled {
            return set.into_iter().collect();
        }

        // z/zh
        if self.z_zh {
            if let Some(t) = replace_prefix(s, "z", "zh") {
                set.insert(t);
            }
            if let Some(t) = replace_prefix(s, "zh", "z") {
                set.insert(t);
            }
        }
        // c/ch
        if self.c_ch {
            if let Some(t) = replace_prefix(s, "c", "ch") {
                set.insert(t);
            }
            if let Some(t) = replace_prefix(s, "ch", "c") {
                set.insert(t);
            }
        }
        // s/sh
        if self.s_sh {
            if let Some(t) = replace_prefix(s, "s", "sh") {
                set.insert(t);
            }
            if let Some(t) = replace_prefix(s, "sh", "s") {
                set.insert(t);
            }
        }
        // n/l
        if self.n_l {
            if let Some(t) = replace_prefix(s, "n", "l") {
                set.insert(t);
            }
            if let Some(t) = replace_prefix(s, "l", "n") {
                set.insert(t);
            }
        }
        // f/h
        if self.f_h {
            if let Some(t) = replace_prefix(s, "f", "h") {
                set.insert(t);
            }
            if let Some(t) = replace_prefix(s, "h", "f") {
                set.insert(t);
            }
        }
        // an/ang
        if self.an_ang {
            if let Some(t) = replace_suffix(s, "an", "ang") {
                set.insert(t);
            }
            if let Some(t) = replace_suffix(s, "ang", "an") {
                set.insert(t);
            }
        }
        // en/eng
        if self.en_eng {
            if let Some(t) = replace_suffix(s, "en", "eng") {
                set.insert(t);
            }
            if let Some(t) = replace_suffix(s, "eng", "en") {
                set.insert(t);
            }
        }
        // in/ing
        if self.in_ing {
            if let Some(t) = replace_suffix(s, "in", "ing") {
                set.insert(t);
            }
            if let Some(t) = replace_suffix(s, "ing", "in") {
                set.insert(t);
            }
        }

        set
    }

    /// 把简拼输入字符串拆分为首字母 token，并对每个 token 做模糊扩展。
    ///
    /// 注意：返回的 initials 字符串只包含单字符首字母。
    /// "zh/ch/sh" 无论由合并产生还是模糊扩展产生，在简拼中都被视为 z/c/s。
    ///
    /// 例如输入 "zhw" 且开启 jianpin_merge_zh_ch_sh 时，
    /// tokens = ["zh", "w"] → 归一化为 ["z", "w"] → 得到 "zw"，可匹配 "中文"。
    ///
    /// 笛卡尔积规模上限：n/l、f/h 模糊音使每个歧义 token
    /// 产生双选项，结果集 2^n 增长。简拼路径自身有 ≤6 字符上限，但全拼路径的
    /// trailing initials 无长度上限（engine/query.rs），长按 n/l/f/h 键会
    /// 指数分配直至主线程冻结/OOM。超限回退为不扩展的原串——正常输入
    /// （≤10 个歧义 token = ≤1024 组合）不受影响。
    pub(crate) const MAX_JIANPIN_EXPANSIONS: usize = 1024;

    /// 全拼音节序列模糊扩展的笛卡尔积上限：
    /// `Engine::expand_syllables` 对每组音节做模糊选项笛卡尔积，多组模糊音
    /// （z_zh+an_ang+s_sh）下每个歧义音节 3 选项，~11 个歧义音节即 3^11 > 1024，
    /// 15 个即 3^15≈1400 万——无护栏会在候选生成前冻结主线程（整个系统打不了
    /// 字）。超限整体回退为不扩展的原始音节序列（查询降级但可用）。上限与
    /// 简拼侧对齐：正常输入（≤10 个歧义音节）不受影响。
    pub(crate) const MAX_SYLLABLE_EXPANSIONS: usize = 1024;

    pub fn expand_jianpin_initials(&self, input: &str) -> Vec<String> {
        let chars: Vec<char> = input.chars().collect();
        let mut tokens: Vec<String> = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            if self.jianpin_merge_zh_ch_sh && i + 1 < chars.len() {
                let two = format!("{}{}", chars[i], chars[i + 1]);
                if two == "zh" || two == "ch" || two == "sh" {
                    tokens.push(two);
                    i += 2;
                    continue;
                }
            }
            tokens.push(chars[i].to_string());
            i += 1;
        }

        // 对每个 token 做模糊扩展，并把 zh/ch/sh 归一化为 z/c/s
        let mut expanded: Vec<Vec<String>> = Vec::new();
        for token in tokens {
            let mut opts: alloc::collections::BTreeSet<String> =
                alloc::collections::BTreeSet::new();
            // 合并产生的 zh/ch/sh 在简拼中等价于 z/c/s
            match token.as_str() {
                "zh" => {
                    opts.insert("z".to_string());
                }
                "ch" => {
                    opts.insert("c".to_string());
                }
                "sh" => {
                    opts.insert("s".to_string());
                }
                _ => {
                    opts.insert(token.clone());
                }
            }
            if self.enabled {
                if self.z_zh && (token == "z" || token == "zh") {
                    opts.insert("z".to_string());
                }
                if self.c_ch && (token == "c" || token == "ch") {
                    opts.insert("c".to_string());
                }
                if self.s_sh && (token == "s" || token == "sh") {
                    opts.insert("s".to_string());
                }
                if self.n_l {
                    if token == "n" {
                        opts.insert("l".to_string());
                    } else if token == "l" {
                        opts.insert("n".to_string());
                    }
                }
                if self.f_h {
                    if token == "f" {
                        opts.insert("h".to_string());
                    } else if token == "h" {
                        opts.insert("f".to_string());
                    }
                }
            }
            expanded.push(opts.into_iter().collect());
        }

        // 笛卡尔积生成所有可能的 initials 串
        let mut results: Vec<String> = vec![String::new()];
        for opts in expanded {
            let mut next: Vec<String> = Vec::with_capacity(results.len() * opts.len());
            for prefix in &results {
                for opt in &opts {
                    next.push(format!("{}{}", prefix, opt));
                }
            }
            if next.len() > Self::MAX_JIANPIN_EXPANSIONS {
                // 超限（≥11 个歧义 token）：放弃模糊扩展回退原串，防指数爆炸
                return vec![input.to_string()];
            }
            results = next;
        }
        results.sort();
        results.dedup();
        results
    }
}

fn replace_prefix(s: &str, from: &str, to: &str) -> Option<String> {
    s.strip_prefix(from).map(|rest| format!("{}{}", to, rest))
}

fn replace_suffix(s: &str, from: &str, to: &str) -> Option<String> {
    s.strip_suffix(from)
        .map(|prefix| format!("{}{}", prefix, to))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_syllable_basic() {
        let cfg = FuzzyConfig {
            enabled: true,
            z_zh: true,
            n_l: true,
            ..Default::default()
        };

        let v: Vec<String> = cfg.expand_syllable("zao");
        assert!(v.contains(&"zao".to_string()));
        assert!(v.contains(&"zhao".to_string()));

        let v2: Vec<String> = cfg.expand_syllable("lan");
        assert!(v2.contains(&"lan".to_string()));
        assert!(v2.contains(&"nan".to_string()));
    }

    #[test]
    fn test_expand_prefix_vs_syllable_semantics() {
        // 语义分界守护：
        // · expand_syllable = 完整音节语义，非有效音节被过滤（"z" → 空）；
        // · expand_prefix = 前缀语义（裸声母/半截音节），不过滤，含自身 + 变体。
        // 简拼单字补充依赖后者——用错前者会整组清空。
        let cfg = FuzzyConfig {
            enabled: true,
            z_zh: true,
            ..Default::default()
        };
        assert!(
            cfg.expand_syllable("z").is_empty(),
            "expand_syllable 对裸声母按完整音节语义应过滤为空（既有语义不变）"
        );
        let p = cfg.expand_prefix("z");
        assert!(p.contains(&"z".to_string()), "前缀扩展必须含自身");
        assert!(p.contains(&"zh".to_string()), "z_zh 前缀扩展应含 zh 变体");
        // 完整音节两函数一致
        let s = cfg.expand_syllable("zhao");
        let p2 = cfg.expand_prefix("zhao");
        assert!(s.contains(&"zhao".to_string()) && p2.contains(&"zhao".to_string()));
        assert!(s.contains(&"zao".to_string()) && p2.contains(&"zao".to_string()));
    }

    #[test]
    fn test_expand_jianpin_initials() {
        // 合并 zh/ch/sh：zhw 应归一化为 zw
        let cfg = FuzzyConfig {
            jianpin_merge_zh_ch_sh: true,
            ..Default::default()
        };
        let v = cfg.expand_jianpin_initials("zhw");
        assert!(v.contains(&"zw".to_string()));
        assert!(
            !v.iter().any(|s| s.contains("zh")),
            "简拼 initials 不应包含多字符声母"
        );

        // 开启 z/zh 模糊：zgr 应扩展为 zgr（自身）
        let cfg2 = FuzzyConfig {
            enabled: true,
            z_zh: true,
            ..Default::default()
        };
        let v2 = cfg2.expand_jianpin_initials("zgr");
        assert!(v2.contains(&"zgr".to_string()));
    }

    #[test]
    fn test_csv_roundtrip() {
        let cfg = FuzzyConfig {
            enabled: true,
            z_zh: true,
            n_l: true,
            jianpin_merge_zh_ch_sh: true,
            ..Default::default()
        };
        let csv = cfg.to_csv();
        let parsed = FuzzyConfig::from_csv(&csv);
        assert_eq!(cfg, parsed);
    }

    /// 笛卡尔积上限守护：n/l 模糊音下长串歧义 token 会 2^n 无界增长
    ///（30 个 n = 10 亿组合 → 主线程冻结/OOM）。
    /// 超限必须回退为不扩展的单元素原串，且正常长度（≤10 歧义 token）不受影响。
    #[test]
    fn test_expand_jianpin_initials_cap() {
        let mut cfg = FuzzyConfig {
            enabled: true,
            n_l: true,
            ..Default::default()
        };

        // 正常长度：6 个 n/l token = 2^6 = 64 组合，不受上限影响
        let v = cfg.expand_jianpin_initials("nlnlnl");
        assert_eq!(v.len(), 64, "6 个歧义 token 应产生 64 组合");
        assert!(v.contains(&"nlnlnl".to_string()));
        assert!(v.contains(&"lnlnln".to_string()));

        // 病态长度：40 个 n（远超 2^10 上限）必须立即回退原串，不再爆炸
        let long = "n".repeat(40);
        let v2 = cfg.expand_jianpin_initials(&long);
        assert_eq!(v2.len(), 1, "超限应回退为单元素原串");
        assert_eq!(v2[0], long);

        // f/h 同款验证
        cfg.n_l = false;
        cfg.f_h = true;
        let long_fh = "f".repeat(40);
        let v3 = cfg.expand_jianpin_initials(&long_fh);
        assert_eq!(v3.len(), 1);
        assert_eq!(v3[0], long_fh);

        // 关闭模糊音时永不触发扩展（原样返回，无论多长）
        cfg.f_h = false;
        cfg.enabled = false;
        let v4 = cfg.expand_jianpin_initials(&"n".repeat(200));
        assert_eq!(v4, vec!["n".repeat(200)]);
    }
}
