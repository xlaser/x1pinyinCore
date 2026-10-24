//! 候选翻译副栏：只翻译当前选中的候选词，按需查询、纯展示。
//! - 英文词（ASCII 字母 2..=24）→ 英译中：`english_words` 精确查
//!   （大小写不敏感），返回规范词形 + 词性 + 中文释义；
//! - 中文词（含 CJK）→ 中译英两级：
//!   ① 精确词典（`zh_en_words` 首选）：词级精确命中
//!     （含单字实词——水→water、你→you 均为学习刚需；语气/结构助词
//!     如「的/了/着」数据层不收），多译按词典 freq 降序最多 3 条；
//!   ② cn 释义反查回退（`english_words.cn` `LIKE` 缩小集合，按 `；`
//!     切分义项，只保留整条义项精确命中，按种子 freq 降序最多 3 条；
//!     子串提及不算对译：「开发环境」不应译出 VisualStudio（其释义
//!     「集成开发环境」只是*提到*该词），同类「微软→微软公司」
//!     「智能→智能手机」全是实例而非对译，宁缺毋滥；合法对译需求由
//!     `zh_en_words` 数据侧承接）——维持 ≥2 字门槛（单字 LIKE 噪声大）；
//! - 数字/符号、ASCII 非词形、无收录词 → 空结果（不硬猜）。
//!
//! 零网络、零学习、不参与候选排序；数据源只有 `dict.db` 的
//! `english_words` + `zh_en_words`。反查 10k 行约 ~0.3ms，精确查是
//! B 树点查（μs 级），只在选中项变化时调用（非按键热路径）。

use alloc::string::String;
use alloc::vec::Vec;

use super::Engine;

/// 一条翻译结果（英文词 + 词性 + 中文释义 + 种子词频）。
#[derive(Debug, Clone, Default)]
pub struct TranslationEntry {
    pub word: String,
    pub pos: String,
    pub cn: String,
    pub freq: i64,
}

/// 翻译结果。`direction` = `"en2zh"` / `"zh2en"`；空结果时 direction 为 ""。
#[derive(Debug, Clone, Default)]
pub struct TranslationResult {
    pub direction: &'static str,
    pub entries: Vec<TranslationEntry>,
}

fn empty() -> TranslationResult {
    TranslationResult {
        direction: "",
        entries: Vec::new(),
    }
}

/// ASCII 词形（与智能单词识别同口径：2..=24 个字母，允许大小写）。
fn is_ascii_word(s: &str) -> bool {
    s.len() >= 2 && s.len() <= 24 && s.bytes().all(|b| b.is_ascii_alphabetic())
}

/// 是否含 CJK 汉字（中文候选判定；标点/数字不算）。
fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(
            c as u32,
            0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F
        )
    })
}

impl Engine {
    /// 翻译一个候选词（英→中 / 中→英）。`limit` 只约束中→英条数（1..=5）。
    pub fn translate_candidate(&self, text: &str, limit: usize) -> TranslationResult {
        let term = text.trim();
        if term.is_empty() {
            return empty();
        }
        let limit = limit.clamp(1, 5);
        let Some(storage) = self.storage.as_ref() else {
            return empty();
        };

        // ── 英译中：精确命中（大小写不敏感）──
        if is_ascii_word(term) {
            let Some((word, freq)) = storage.english_exact(term) else {
                return empty();
            };
            let Some((pos, cn, _)) = storage.english_word_detail(term) else {
                return empty();
            };
            return TranslationResult {
                direction: "en2zh",
                entries: alloc::vec![TranslationEntry {
                    word,
                    pos,
                    cn,
                    freq,
                }],
            };
        }

        // ── 中译英①：精确词典（zh_en_words 点查，单字内容词可用）──
        if has_cjk(term) {
            let exact = storage.zh_en_exact(term, limit);
            if !exact.is_empty() {
                return TranslationResult {
                    direction: "zh2en",
                    entries: exact
                        .into_iter()
                        .map(|(en, pos, freq)| TranslationEntry {
                            word: en,
                            pos,
                            cn: alloc::string::String::from(term),
                            freq,
                        })
                        .collect(),
                };
            }
        }

        // ── 中译英②回退：cn 释义反查，只保留整条义项精确命中（≥2 字门槛
        //    保留——单字 LIKE 噪声大，精确词典未收的单字宁可空结果）。
        //    子串提及不算对译（释义「集成开发环境」提到
        //    「开发环境」≠ VisualStudio 是它的对译），无精确义项 → 空结果
        //    不硬猜；被挡掉的合法对译（微软/智能/开发环境…）由
        //    zh_en_words 数据侧收录。──
        if has_cjk(term) && term.chars().count() >= 2 {
            let rows = storage.english_reverse_cn(term, 32);
            let mut ranked: Vec<TranslationEntry> = rows
                .into_iter()
                .filter(|(_, _, cn, _)| {
                    cn.split(['；', ';'])
                        .any(|part| part.trim() == term)
                })
                .map(|(word, pos, cn, freq)| TranslationEntry {
                    word,
                    pos,
                    cn,
                    freq,
                })
                .collect();
            if ranked.is_empty() {
                return empty();
            }
            // 种子词频降序 → 词形字典序（结果确定性）
            ranked.sort_by(|a, b| {
                b.freq
                    .cmp(&a.freq)
                    .then_with(|| a.word.cmp(&b.word))
            });
            ranked.truncate(limit);
            return TranslationResult {
                direction: "zh2en",
                entries: ranked,
            };
        }

        empty()
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::tests_english::en_engine;

    #[test]
    fn english_to_chinese_display_and_case() {
        let e = en_engine();
        let r = e.translate_candidate("microsoft", 3);
        assert_eq!(r.direction, "en2zh");
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].word, "Microsoft");
        assert_eq!(r.entries[0].cn, "微软公司");
        assert!(!r.entries[0].pos.is_empty());
        // 大小写不敏感
        assert_eq!(
            e.translate_candidate("MICROSOFT", 3).entries[0].word,
            "Microsoft"
        );
    }

    #[test]
    fn chinese_to_english_exact_dictionary_first() {
        let e = en_engine();
        // 精确词典优先：多译按词典 freq 降序（artificial intelligence 在前）
        let r = e.translate_candidate("人工智能", 3);
        assert_eq!(r.direction, "zh2en");
        assert_eq!(
            r.entries[0].word, "artificial intelligence",
            "精确词典命中，整词对译在前"
        );
        assert!(r.entries.iter().any(|x| x.word == "AI"));

        assert_eq!(e.translate_candidate("公司", 3).entries[0].word, "company");
        assert_eq!(e.translate_candidate("电脑", 3).entries[0].word, "computer");
        assert_eq!(e.translate_candidate("你好", 3).entries[0].word, "hello");
    }

    #[test]
    fn chinese_single_char_content_word() {
        let e = en_engine();
        // 单字实词走精确词典（水→water、你→you 是学习刚需，代词亦收录）
        let r = e.translate_candidate("水", 3);
        assert_eq!(r.direction, "zh2en");
        assert_eq!(r.entries[0].word, "water");
        assert_eq!(e.translate_candidate("你", 3).entries[0].word, "you");
        assert_eq!(e.translate_candidate("谁", 3).entries[0].word, "who");
    }

    #[test]
    fn chinese_single_char_particle_untranslated() {
        let e = en_engine();
        // 语气/结构助词数据层不收 + 反查回退维持 ≥2 字门槛 → 空结果不硬猜
        assert!(e.translate_candidate("的", 3).entries.is_empty());
        assert!(e.translate_candidate("了", 3).entries.is_empty());
        assert!(e.translate_candidate("着", 3).entries.is_empty());
    }

    #[test]
    fn chinese_reverse_fallback_still_works() {
        let e = en_engine();
        // 精确词典未收录的词仍走 cn 释义反查（≥2 字）——整条义项精确命中
        // 保留：OpenAI 释义恰为「人工智能公司」
        let r = e.translate_candidate("人工智能公司", 3);
        assert_eq!(r.direction, "zh2en");
        assert!(r.entries.iter().any(|x| x.word == "OpenAI"));
    }

    #[test]
    fn chinese_reverse_fallback_drops_substring_mentions() {
        let e = en_engine();
        // 错误类守护：「开发环境」不得译出 VisualStudio——其释义
        // 「集成开发环境」只是*提到*该词，子串提及不是对译。回退只认整条
        // 义项精确命中，无精确义项 → 空结果不硬猜。「智能手」是策略探针
        // （smartphone 释义「智能手机」的子串，永远不整条相等，也不会进
        // zh_en_words）——LIKE 有命中但必须为空。
        let r = e.translate_candidate("智能手", 3);
        assert!(r.entries.is_empty(), "子串提及不得当对译: {:?}", r.entries);
        // 「集成」同理：VisualStudio/Jenkins 的释义都只是包含它
        let r = e.translate_candidate("集成", 3);
        assert!(
            r.entries.iter().all(|x| x.word != "VisualStudio" && x.word != "Jenkins"),
            "释义提及类实例词不得出现在结果: {:?}",
            r.entries
        );
    }

    #[test]
    fn zh_to_en_exact_dictionary_hits() {
        let e = en_engine();
        // 中译英精确词典正译守护（含多词短语对译）
        let r = e.translate_candidate("开发环境", 3);
        assert_eq!(r.direction, "zh2en");
        assert_eq!(r.entries[0].word, "development environment");
        assert_eq!(e.translate_candidate("输入法", 3).entries[0].word, "input method");
        assert_eq!(e.translate_candidate("水瓶", 3).entries[0].word, "water bottle");
        assert_eq!(e.translate_candidate("售卖", 3).entries[0].word, "sell");
        assert_eq!(e.translate_candidate("瘦", 3).entries[0].word, "thin");
        assert_eq!(e.translate_candidate("胖", 3).entries[0].word, "fat");
        assert_eq!(e.translate_candidate("信息学", 3).entries[0].word, "informatics");
        // 被回退收紧挡掉的合法对译由精确词典承接
        assert_eq!(e.translate_candidate("微软", 3).entries[0].word, "Microsoft");
        assert_eq!(e.translate_candidate("智能", 3).entries[0].word, "smart");
    }

    #[test]
    fn no_translation_cases() {
        let e = en_engine();
        // 语气助词不翻（数据层不收；单字实词已可翻——你/水 等见专项测试）
        assert!(e.translate_candidate("哦", 3).entries.is_empty());
        // 词表未收录
        assert!(e.translate_candidate("zzzz", 3).entries.is_empty());
        // 符号 / 非词形（含 LIKE 通配符不注入）
        assert!(e.translate_candidate("%", 3).entries.is_empty());
        assert!(e.translate_candidate("，", 3).entries.is_empty());
        assert!(e.translate_candidate("公司_", 3).entries.is_empty());
    }

    #[test]
    fn limit_caps_entries() {
        let e = en_engine();
        let r = e.translate_candidate("公司", 1);
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].word, "company");
    }
}
