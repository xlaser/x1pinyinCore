//! 输入法主引擎
//!
//! 组装拼音解析、词库查询、频率模型的完整流程。
//! 对外暴露简洁的 API。
//!
//! 支持 SQLite 数据库加载：X1Dict 词库 + 用户词频。
//! - `new` : 仅加载内置小词库（向后兼容）
//! - `new_with_db(path)`: 从 SQLite 加载 X1Dict + 用户词频

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::dict::{Candidate, Dict};
use crate::freq::FreqModel;
use crate::fuzzy::FuzzyConfig;
use crate::pinyin::{parse_pinyin, Token};

/// 整句 DP 的一条路径。
///
/// 数据源 = `decode_sentence_paths` 收集的 all_paths（过门禁、按分降序、文本去重）。
/// 用途：DLL 在纯简拼长串（≥4 字母）且 AI 开启时把它随 ai_query 传给 x1ai-service，
/// 由本地模型从多条候选切分里挑上下文最合理的一句（AI 当裁判不当作者）。
/// 旁路信息：只影响 AI 展示层落点，绝不参与
/// 引擎候选排序（硬不变量）。
#[derive(Debug, Clone, PartialEq)]
pub struct SentencePath {
    /// 整句文本（words 直接拼接）。
    pub text: String,
    /// 词分段（提示词展示用，如 ["举个","例子","说明","问题"]）。
    pub words: Vec<String>,
    /// 路径 DP 分数（降序排列的排序键；诊断/观测用）。
    pub score: i32,
}
#[allow(unused_imports)]
use crate::storage::{Storage, X1DictWord};

// 子模块声明。
mod ambiguity; // 同音歧义槽检测 + 同拼音备选
mod backward; // 双向词格重打分：N-best 右文证据
mod correction;
mod dict_admin;
mod english; // 智能单词识别英文词表查询
mod learning;
mod pins;
mod query;
mod scoring;
mod sentence;
mod translation;
// 测试块按主题拆分为独立 mod。
// 每个 mod 文件用 `use super::*` 访问 engine 的 pub 项，与原 tests.rs 行为一致。
#[cfg(test)]
mod tests_ambiguity; // 同音歧义备选可见性 + 学习闭环
#[cfg(test)]
mod tests_backward; // 双向词格重打分
#[cfg(test)]
mod tests_competition_decay; // 「退化」通道（竞争频率）引擎级守护
#[cfg(test)]
mod tests_context; // bigram/trigram 上下文 / 输入映射 / 排序加权（原 tests_learning 拆出）
#[cfg(test)]
mod tests_correction; // 智能纠错 / 多音字 / 模糊音 / 整句切分 / 学习基础（原 tests_learning 拆出）
#[cfg(test)]
mod tests_dict; // 词库 CRUD / TSV 导入导出 / 持久化（原 tests_learning 拆出）
#[cfg(test)]
mod tests_english; // 智能单词识别
/// 平台无关性守护：
/// 源码级扫描 engine/*.rs（不含 tests_*.rs）断言无平台分支/平台路径。
#[cfg(test)]
mod tests_platform_purity;
#[cfg(test)]
mod tests_green; // 绿词 / 多音字次音
#[cfg(test)]
mod tests_green_quality;
#[cfg(test)]
mod tests_guess; // 紫词
#[cfg(test)]
mod tests_longrange; // 长程关联距离/干扰/边界实测
#[cfg(test)]
mod tests_pins;
#[cfg(test)]
pub(crate) mod tests_query; // 基础查询 / DB / 性能探针
#[cfg(test)]
mod tests_quick_input; // 快捷输入（/date /fh 等符号/日期时间）
#[cfg(test)]
mod tests_seg_cache; // 绿词 DP 段缓存守护
#[cfg(test)]
mod tests_seg_chain;
#[cfg(test)]
mod tests_sentence; // 整句解码 / 切分 / 学习基础
#[cfg(test)]
mod tests_shuangpin; // 双拼
#[cfg(test)]
mod tests_shuangpin_acceptance;
#[cfg(test)]
mod tests_tail_initial; // 末尾单辅音=末音节缩写；切分选举误判 / 记忆相对质量门；双拼发布验收矩阵；固定候选 user_pins
                        // 把 correction 的 pub(super) 函数导入到本 mod 顶层作用域，
                        // 让 mod.rs 现有代码（含 tests）调用 generate_corrections 等不用改。
use correction::{
    generate_collapse_variants, generate_corrections, generate_phonetic_corrections,
    generate_swap_corrections, is_legal_jianpin_mix, phonetic_syllable_variants,
};
// 特征序列化 pub use：FFI 与 引擎服务侧 selections 共用的
// 跨进程 JSON 契约，单一来源在 scoring.rs。
pub use scoring::features_to_json;
// 运行时统计：引擎服务侧的「运行信息」请求用。
pub use query::EngineRuntimeStats;

/// 查询候选文本首字的常用度分数。
/// 不在常用字表中返回 0，用于把生僻字排在常用字后面。
fn char_commonness(text: &str) -> u32 {
    text.chars()
        .next()
        .and_then(crate::char_freq::char_freq)
        .unwrap_or(0)
}

/// 规范化拼音字符串为带空格分隔的音节序列（用于 user_words 存储）。
///
/// 防御调用方传入未分隔音节（如 "tongzhi"）导致 Trie 节点错误：
/// 查询时按 "tong zhi" 两音节匹配，会找不到单音节 "tongzhi" 节点。
///
/// 处理：
/// 1. 压缩连续空格、保留显式 `'` 分隔
/// 2. 简拼（每字符当首字母）→ 每字符单独成音节
/// 3. 全拼 → `parse_pinyin` 切分加空格
///
/// 已规范化的输入（如 "tong zhi"）会原样返回（切分结果一致）。
/// 判断文本是否是合法的中文候选（不含 ASCII 字母）。
///
/// 防御性检查：汉字候选不可能含 a-z/A-Z。早期词表来源的字母脏词条、整句解码
/// bug 产出的「系w」「文T」等都含 ASCII 字母，必须拒绝入库（learn/commit）
/// 和过滤输出（process），避免脏候选污染用户 DB 和候选列表。
///
/// 允许：汉字、标点、数字（「ATM」这种缩写词理论上可保留，但旧词表
/// 实际不含，且含字母的多字符词更可能是 bug 产物，统一拒绝更安全）。
fn is_valid_chinese_text(text: &str) -> bool {
    !text.is_empty() && !text.chars().any(|c| c.is_ascii_alphabetic())
}

fn normalize_pinyin_for_storage(input: &str) -> String {
    let normalized = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return String::new();
    }
    if crate::pinyin::is_jianpin(&normalized) {
        normalized
            .chars()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        let seq = parse_pinyin(&normalized);
        seq.syllables
            .iter()
            .map(|s| &normalized[s.start..s.end])
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// 用户词的「纯汉字文本字数 == 拼音音节数」不变量校验。
///
/// 背景（"习惯惯"末字重复上屏根因）：learn/固化入口若接受错位拼音
/// （如 learn("习惯惯","xi guan")——组合学习步骤拼音缺失的产物），3 字词挂在
/// 2 音节的 Trie 键下，之后输入 "xiguan" 直接把"习惯惯"当整词候选顶到最前
/// （实测排名 #1，与合法"习惯"的排序竞争 = 用户感知"偶尔重复上屏"），一次
/// 空格整体上屏。
///
/// learn / commit 固化 / 用户词导入 / user_words 加载四端统一用它防御。
/// 只对纯汉字文本（CJK 基本区）强制一汉字一音节：系统词库有
/// 「21三体综合症」这类含数字/间隔号的合法错位行，混合文本不适用本不变量，
/// 放行（与历史行为一致）；空拼音的纯汉字词（音节数 0）同样判不匹配——
/// 该形态在 Trie 上本就无法正确服务。
pub(crate) fn user_word_pinyin_matches(text: &str, pinyin: &str) -> bool {
    let pure_hanzi = !text.is_empty() && text.chars().all(|c| matches!(c as u32, 0x4E00..=0x9FFF));
    if !pure_hanzi {
        return true;
    }
    let syllables = pinyin.split_whitespace().count();
    syllables > 0 && text.chars().count() == syllables
}

/// 输入法引擎
///
/// 内存优先 + 增量 flush：
/// - 词库（X1Dict 系统词 + 内置 + 用户词）一次性加载到 `dict`（内存 Trie）；
/// - 用户词以 `text -> pinyin` 形式全量驻留内存 HashMap；
/// - 用户频率同样驻留内存 `FreqModel`；
/// - SQLite 仅作为持久化层，由 `flush` 定时/退出时增量同步：
/// 只把新增/修改的词 upsert，把删除的词 delete。
/// 候选打分的分量明细（学习型排序器的训练特征）。
///
/// `candidate_score` 计算总分时各分量的来源，供学习型排序器离线训练用。
/// ranker 的 6 维特征（NLEmbedding 语义 bonus 已随后台
/// embedding 架构一并移除）；新增的
/// `ctx_homophone_bonus` 是绝对分，单独进 `total`、不进 ranker JSON 契约。
#[derive(Debug, Clone, Default)]
pub struct ScoreBreakdown {
    /// `base_freq * rank_boost / 1000`：词库频率 × 用户使用次数对数加成
    pub base: u64,
    /// 累计选择次数 bonus（封顶 15）
    pub total_bonus: u64,
    /// 近期提交分（指数衰减）
    pub recent: u64,
    /// 输入习惯映射 bonus
    pub mapping_bonus: u64,
    /// bigram 上下文共现 bonus（30 词指数衰减求和）。
    ///
    /// 分层设计若把 bigram 从 total 剥离（只让 collocation 第3层决定），
    /// 但这是矫枉过正——collocation 只覆盖「词库权威搭配」（如「记忆」），用户通过 commit
    /// 建立的 word_bigram 共现（如「饭→吃」，词库无「饭吃」）在第3层判定为 false 后，
    /// 第4层 total 又不含 bigram，导致用户上下文学习在全拼/简拼主路径下完全失效。
    ///
    /// 修正：bigram_bonus 重新进 total。第3层 collocation 仍独立优先
    /// （解决「调参顾此失彼」的核心场景：词库权威搭配绝对优先），但第4层 total 重新包含
    /// bigram 作为软加分——这样「饭+chi 出吃」（用户 bigram）和「记+yi 出忆」（词库 collocation）
    /// 各自走自己的路径生效，互不干扰。
    pub bigram_bonus: u64,
    /// 长词匹配 bonus（context 最近 N 词 + 候选拼成多段长词，如「星期一上午会议」）。
    /// 不进 total：与第3层 collocation 语义部分重叠
    /// （collocation 覆盖紧邻两段拼接的主场景），long_word 只覆盖稀有"多词链"场景。
    /// 若未来证明 long_word 也有回归，再单独评估是否回 total。
    pub long_word_bonus: u64,
    /// 上下文搭配命中标记（candidate_cmp 第3层判定用）。
    /// true = 候选与上文 context.last 构成词库词（collocation_freq 命中，如「记忆」）。
    /// 仅多字 context 非空时计算；冷启动 context 空 = false（第3层不生效，行为同现状）。
    pub has_collocation: bool,
    /// 上下文同音证据 bonus：`(同音槽拼音, 上一上屏词)→选择`
    /// 的平滑份额增量折算的绝对分（正增量才计入，钳制 ≤600）。
    ///
    /// 为什么不并进 `bigram_bonus` / `rank_boost`：
    /// - bigram_bonus 是 30 词衰减共现（与同音竞争语义不同）；
    /// - rank_boost 是 base_freq 的乘数——对低频词（触发 738 vs 处罚 2868）
    /// ±480 的乘数增量只有 ~百级绝对分，压不过 4× 词频差（实测触发永远翻不了）。
    /// 本字段按绝对分累加，才能让「用户在该语境选过」这一强意图真正生效。
    /// 不进 features_to_json（6 维 ranker 契约不变；ranker 默认关闭）。
    pub ctx_homophone_bonus: u64,
}

impl ScoreBreakdown {
    /// 综合 frequency/learning/context 总分（6 分量：base/total/recent/mapping/
    /// bigram/ctx_homophone；long_word 保持剥离）。
    ///
    /// 为什么 bigram 进 total 而 long 不进：
    /// - bigram（用户 word_bigram 共现，如「饭→吃」）语义独立于 collocation（词库权威搭配），
    /// collocation 第3层不覆盖用户行为信号。剥离 bigram 会让用户上下文学习在全拼/简拼
    /// 主路径下完全失效（test_trigram_context_boost 回归）。
    /// - long（context 多词链 + 候选拼成长词）与 collocation 语义部分重叠（collocation 已覆盖
    /// 紧邻两段拼接主场景），long 只覆盖稀有"多词链"场景，剥离暂无回归证据。
    ///
    /// 与 candidate_cmp 第3层的关系：第3层 collocation 仍是硬规则（词库权威搭配绝对优先），
    /// 解决「调参顾此失彼」的核心场景（记→yi 出忆）。bigram 回 total 不破坏第3层——它只在
    /// 「双方都无 collocation」时在第4层作软加分生效（如「饭+chi」双方都无 collocation → 进第4层）。
    pub fn total(&self) -> u64 {
        self.base
            + self.total_bonus
            + self.recent
            + self.mapping_bonus
            + self.bigram_bonus
            + self.ctx_homophone_bonus
    }
}

/// 绿词采纳率统计。
///
/// 配合 decode_sentence 的绿词输出（MAX_GREEN_CANDIDATES=2）：
/// - `total_green_picks`：用户选中绿词的总次数（不论第几位）。
/// - `second_green_picks`：用户选中第二位绿词的次数（即第一绿词被跳过的情形）。
///
/// `threshold_ratio` 综合两通道给出第二绿词的分数门槛比例：
/// - 动态阈值：按第一绿词绝对分区间给基础比例（高频词更严，低频略松）。
/// - 采纳率微调：样本 ≥ 20 后，若用户经常选第二绿词（采纳率 > 25%）说明门槛
/// 太严，下调 0.03；若几乎不选（< 5%）说明第二绿词是噪音，上调 0.05。
#[derive(Clone, Copy, Default)]
pub(crate) struct GreenAcceptStats {
    pub total_green_picks: u32,
    pub second_green_picks: u32,
}

impl GreenAcceptStats {
    /// 计算第二绿词门槛比例（动态阈值 + 采纳率微调，范围 0.80~0.92）。
    ///
    /// `first_score` 是第一绿词的 DP 路径累计分（i32）。返回的比例用于
    /// `threshold = first_score * ratio`，第二绿词路径分 < threshold 则丢弃。
    pub fn threshold_ratio(&self, first_score: i32) -> f64 {
        // 动态阈值（按第一绿词绝对分档）
        let dynamic: f64 = if first_score > 80 {
            0.90
        } else if first_score > 50 {
            0.86
        } else {
            0.82
        };
        // 采纳率微调（样本 ≥ 20 才介入，冷启动纯用动态阈值）
        if self.total_green_picks >= 20 {
            let accept_rate = self.second_green_picks as f64 / self.total_green_picks as f64;
            let adjust = if accept_rate > 0.25 {
                -0.03 // 用户常选第二绿词 → 门槛偏松，多给机会
            } else if accept_rate < 0.05 {
                0.05 // 几乎不选 → 第二绿词多为噪音，收紧门槛
            } else {
                0.0
            };
            (dynamic + adjust).clamp(0.80, 0.92)
        } else {
            dynamic
        }
    }
}

/// 上一次普通提交记录的跨词 n-gram 增量。
struct LastCommitNgrams {
    bigram: Option<(String, String, u32)>,
    trigram: Option<(String, String, String, u32)>,
}

/// 学习治理观测计数：验证 / 机制在真实使用中
/// 确实生效（context 清理、会话上限、纠错撤销），并给调参提供数据。
#[derive(Debug, Clone, Default)]
struct LearningStats {
    bigrams_recorded: u64,
    bigrams_skipped: u64,
    bigrams_capped: u64,
    trigrams_recorded: u64,
    trigrams_skipped: u64,
    trigrams_capped: u64,
    context_clears: u64,
    last_commit_reverts: u64,
    bigram_reverts: u64,
}

pub struct Engine {
    /// 当前完整查询词库 = 系统词 + 用户词（逐键查询用，热路径）
    pub(crate) dict: Dict,
    /// 用户频率（内存模型）
    pub(crate) freq: FreqModel,
    /// 三阶上下文模型
    pub(crate) trigram: crate::trigram::TrigramModel,
    /// 竞争频率存储：同音/组词/整句竞争的
    /// 选择分布。写 = commit 事件（learning.rs）；读 = 排序（scoring.rs）。
    /// 无时间参数——无事件即冻结。
    pub(crate) competition: crate::competition::CompetitionStore,
    /// 先验缓存（懒计算）：同音层 ctx → 该拼音下全部候选的归一 base_freq
    /// 份额。只在「读取平滑频率」时按需计算（热路径复用，不重复 dict.query）。
    pub(crate) competition_priors: std::cell::RefCell<HashMap<String, Vec<(String, f32)>>>,
    /// 用户词拼音映射（text -> 全部读音），内存中的用户词库。
    /// 同词多读音共存（同一字用不同读音造词，如 zhujiao→主角 与 zhujue→主角，
    /// 不能互相冲掉——输入不精确是常态，两种读音都该记住）。DB 侧对应
    /// user_words 的 UNIQUE(text, pinyin)。
    pub(crate) user_words: HashMap<String, Vec<String>>,
    /// 自上次 flush 以来需要 upsert 的 text 集合
    pub(crate) pending_upserts: HashSet<String>,
    /// 自上次 flush 以来需要 delete 的 text 集合
    pub(crate) pending_deletions: HashSet<String>,
    /// 自上次 flush 以来读音被显式替换的 text 集合（多读音
    /// 合并补漏）：`update_user_word`（词库管理器编辑）语义是"修正这个词"——
    /// 内存把读音 vec 整体换掉，但 flush 只 upsert 新读音，旧读音行会残留
    /// （用户改拼音后同词出现两条、候选里冒出旧读音）。flush 时对这些 text
    /// 执行「删除除当前读音外的全部行」。与 pending_deletions 分开：后者是
    /// 整词删除（含黑名单语义），前者只在显式编辑时置位。
    pub(crate) pending_reading_prune: HashSet<String>,
    /// 用户明确 remove 过的文本（会话级黑名单）。
    ///
    /// 整句解码（decode_sentence）会把单字拼成搭配，若用户已 remove 某词，
    /// 拼出的整句若与该词文本相同应跳过，尊重用户「不想看到」的意愿。
    /// 会话级：重启后丢失（但重启后该词本就不在用户 DB，整句拼出的是单字组合，
    /// 语义上可接受）。learn 同文本时会清除此记录（用户重新学习了该词）。
    pub(crate) removed_texts: HashSet<String>,
    /// 用户原始输入 → 选择文本 的映射（input, text -> count）
    pub(crate) input_mappings: HashMap<(String, String), u32>,
    /// `input_mappings` 的反向索引：text → Vec<(input, count)>。
    ///
    /// candidate_score 的 prefix mapping 查询需要「按 text 过滤 + input 前缀匹配」，
    /// 直接遍历整个 input_mappings（用户长期积累后可达数千条）会让每次候选比较
    /// 都全表扫描，致 process 随 mapping 数线性退化（实测 6000 条时 2.9ms）。
    /// 此索引把查询从 O(全部 mapping) 降到 O(该 text 的映射数，通常 ≤ 几条)。
    /// 与 input_mappings 同步维护（commit 增量、load/clear 全量重建）。
    pub(crate) input_mappings_by_text: HashMap<String, Vec<(String, u32)>>,
    /// 学习型排序器权重（运行时注入，离线训练完成后调 set_ranker_weights）。
    /// None = 未注入，回退编译期 RANKER_WEIGHTS（若也全0则回退手调公式）。
    pub(crate) ranker_weights: Option<[f64; 6]>,
    /// SQLite 持久化层（可选）
    pub(crate) storage: Option<Storage>,
    /// 内存状态是否脏（需要 flush）
    pub(crate) dirty: bool,
    /// 存储加载不完整：启动时任一表读失败 →
    /// 对应内存是空态。flush 的全量快照 replace 会用空态覆写磁盘真数据
    /// （用户词表清零链），置位后 flush 跳过全量快照只走增量路径。
    /// 最近一次 flush 的阶段耗时。
    /// 形如 [("maps", 3), ("ngram", 1), ...]；只记录成功路径，早退清空。
    pub(crate) last_flush_breakdown: Vec<(&'static str, u128)>,
    pub(crate) storage_load_incomplete: bool,
    /// 落盘失败待重试的黑名单（remove 的
    /// insert_blacklist 落盘错误不能吞——黑名单是删词防复活的唯一持久化
    /// 通道，失败即失效；flush 开头重试）。
    pub(crate) pending_blacklist: Vec<String>,
    /// 模糊拼音配置
    pub(crate) fuzzy_config: FuzzyConfig,
    /// 是否启用输入习惯映射学习（可关闭）
    pub(crate) input_mapping_enabled: bool,
    /// 最近的上屏词历史（最多 `CONTEXT_HISTORY_SIZE`=30 个），用于 bigram 上下文重排。
    /// 会话级，不持久化。
    ///
    /// 清空时机：重启进程、中文标点上屏（宿主前端标点路径
    /// 调 `x1_engine_clear_context` FFI）——
    /// 标点是天然句界，跨句 bigram 信号弱且易误导：前句尾词残留
    /// 在栈里会让下句首字被前句 user_bigram 干扰。详见 `context_bonus_with_ctx`
    /// 的单字链式弱化。
    ///
    /// 基于马尔可夫链"近期依赖最强"思想 + 加权衰减：candidate_score 遍历全部历史词，
    /// 按距离指数衰减加权（最近 weight=1.0=0.85^0，第30个≈0.85^29≈0.0076），
    /// 让近期上下文主导、远处补充。比标准一阶马尔可夫（只看1个词）能看到更长的上下文搭配规律。
    pub(crate) context: Vec<String>,
    /// 上一次普通提交的可撤销跨词 n-gram 增量。
    /// 前端在提交后立即退格删除刚上屏的词时调 `revert_last_commit`；
    /// 只回滚跨词 bigram/trigram（候选内部词对由前端另调 revert_bigram）。
    /// None = 本次提交未记录（首词/连续同词/整句跨词跳过/会话上限抑制）。
    last_commit_ngrams: Option<LastCommitNgrams>,
    /// 学习治理观测计数（会话级；见 LearningStats）。
    learning_stats: LearningStats,
    /// 观测计数总开关（高级页「噪声治理统计」）：宿主前端
    /// 侧合成「调试日志总开关 AND 子开关」后经 FFI 注入；关闭时自增点零开销
    /// 跳过。默认 true——离线调试工具等诊断链依赖计数，产品侧启动即按偏好压掉。
    learning_stats_enabled: bool,
    /// 单字成词抑制标记。
    ///
    /// 场景：用户逐字输入 wei(维)bao(保)，commit 单字「保」时检测到「维+保=维保」
    /// 成词（在 user_words 或系统词库）→ 置此标记为 true。下一个字的候选查询会跳过
    /// 与 context.last 的 collocation_freq（保+证=保证 不再让「证」冒头），让候选走
    /// 纯词频（「正」freq=6970 排前）。commit 下一个单字时标记重置为 false。
    ///
    /// 为什么：用户逐字确认「维保」后，心智上「维保」已是整体，后续字不应再和「保」
    /// 单字组词。这避免了「保+证=保证」的 collocation 让「证」压过用户真正想要的「正」
    /// （关联后字「在」=「正在」，但后字尚未输入，输入法无法预知）。
    ///
    /// 生命周期：commit 单字 X 时，若 X 与 context.last 成词 → true；下次 commit
    /// 任意单字 → false。多字 commit 不触发此机制（多字已是词级 prev，collocation 正常）。
    /// 标点清栈（clear_context）时一并重置为 false。
    pub(crate) suppress_collocation_after_word: bool,
    /// 是否启用智能纠错（候选不足时用编辑距离变形补充候选）。
    /// 默认开。通过 FFI x1_engine_set_smart_correction_enabled 实时同步。
    pub(crate) smart_correction_enabled: bool,
    /// 纠错类型分控：键盘误触纠错（相邻键替换、多打删除等机械性误触变形；
    /// 不做插入变形——少打字符由前缀路径覆盖）。
    /// 默认开。仅在 smart_correction_enabled=true 时生效（总闸）。通过 FFI
    /// x1_engine_set_keyboard_correction_enabled 实时同步。
    /// 设计：南方用户发音混淆多、北方用户键盘误触多，分开控制减少噪音橙词。
    pub(crate) keyboard_correction_enabled: bool,
    /// 纠错类型分控：发音混淆纠错（zh/z、c/ch 等前后鼻音/平翘舌变形）。
    /// 默认开。仅在 smart_correction_enabled=true 时生效（总闸）。通过 FFI
    /// x1_engine_set_phonetic_correction_enabled 实时同步。
    /// 统管两条发音路径（拆分依据是引擎内部触发条件，用户不可见）：
    /// ① 结构非法输入的发音变形补词（maybe_correct 主路径）；
    /// ② 合法输入的上下文证据发音营救（context_phonetic_rescue，zongguo→中国）。
    pub(crate) phonetic_correction_enabled: bool,
    /// 纠错类型分控：键序颠倒纠错（相邻字母交换，如 nihoa→nihao）。
    /// 默认开。仅在 smart_correction_enabled=true 时生效（总闸）。通过 FFI
    /// x1_engine_set_swap_correction_enabled 实时同步。
    /// 捕捉最高频的误打类型之一——字母顺序按反（常见乱序与正确形式恰好差
    /// 一次相邻交换，无需单独的韵母乱序规则表）。
    pub(crate) swap_correction_enabled: bool,
    /// 纠错候选数量上限（控制橙色候选占比，避免挤占正常候选位）。
    /// 默认 5。通过 FFI x1_engine_set_max_corrections 实时同步。
    /// 设为 0 = 完全禁用纠错补充（区别于 smart_correction_enabled 总闸，
    /// 总闸关则 maybe_correct 直接 return；本项关则进函数但不补充候选）。
    pub(crate) max_corrections: usize,
    /// 双击故障判定阈值（毫秒）。默认 100（键盘 bounce 典型 <50ms，宽松界）。
    /// 通过 FFI x1_engine_set_double_fault_threshold_ms 实时同步（偏好设置滑块）。
    /// （原 double_fault_correction_enabled 开关已摘除——
    /// 它与前端防抖开关双绑导致引擎折叠纠错生产环境恒死；折叠的用户开关
    /// 就是 smartCorrection + keyboardCorrectionEnabled，时序检测恒活。）
    pub(crate) double_fault_threshold_ms: u32,
    /// 单音节拆分识别开关。
    /// 输入恰为单个完整音节且存在两段有效拆分（如 yuan→yu|an、tuan→tu|an、
    /// mian→mi|an、yue→yu|e），且词库有对应两音节词时，把最优词按
    /// ambig_split_position 插入候选。默认开；关闭则回到旧行为（用户手动打 '）。
    /// 通过 FFI x1_engine_set_ambig_split_enabled 实时同步（偏好设置「单音节拆分」）。
    pub(crate) ambig_split_enabled: bool,
    /// 拆分词插入位（1 起算，默认 2，钳制 2..=5）。通过 FFI
    /// x1_engine_set_ambig_split_position 实时同步（偏好设置滑块）。
    pub(crate) ambig_split_position: u8,
    /// 整句自动固化阈值（用户选过 N 次后自动固化进 user_words，0=禁用）。
    /// 默认 0（禁用；偏好设置
    /// 「长词记忆」滑块 0-5 可开启）。通过 FFI x1_engine_set_auto_solidify_threshold
    /// 实时同步。解决「蛇鼠一窝」「输入法项目」等固定搭配选多次记不住的问题。
    pub(crate) auto_solidify_threshold: u32,
    /// 长句绿词条数（1..=5，默认 1 条，可按数量设定）：
    /// 长句输入可出多条绿词，第 2 条起受 green_accept 动态
    /// 门槛约束（分差超容忍即不展示），不逐条放宽。旧布尔开关
    /// set_second_green_enabled 保留兼容（true=count≥2，false=1）。
    /// 通过 FFI x1_engine_set_green_candidate_count 实时同步。
    pub(crate) green_candidate_count: u8,
    /// 注：semantic_bonuses 字段已移除。
    /// NLEmbedding 神经重排序已完全移除。
    /// collect_candidates 阶段缓存，process 复用避免重复 Trie 查询（两阶段优化）。
    ///
    /// 预留两阶段缓存优化，当前回退方案未启用：collect_candidates 与 process 各调一次
    /// process_inner（缓存复用因 process 的 &self 签名无法写缓存而回退，性能开销可接受）。
    /// 字段保留供未来优化（如把 process 改为 &mut self 或引入 RefCell），勿删。
    #[allow(dead_code)]
    pub(crate) collected_candidates: Option<Vec<Candidate>>,
    /// AI 切分裁决收集开关：true 时下一次
    /// process 的整句 DP 调用（纯简拼/混合声母）顺带收集 top-K 路径到
    /// `last_sentence_paths`。RefCell 与既有 score_cache 同纪律：Engine 本就
    /// 单线程使用，不引入新约束。
    pub(crate) collect_sentence_paths: std::cell::Cell<bool>,
    /// 最近一次 process 收集的整句路径（collect 关时为上次残留——调用方
    /// take 后按需丢弃；process_inner 在收集开启时先清，防陈旧混入）。
    pub(crate) last_sentence_paths: std::cell::RefCell<Vec<SentencePath>>,
    /// 尾部半截延长词提示：bare 零声母尾
    /// 且有延长词证据时，process_inner 存入最佳延长词（紫，如 jisua→计算），
    /// process 排序定稿后取出注入（freq ≥ 首选则置顶）。仅外层调用
    ///（correct=true）写入，纠错递归子查询不触碰；process 入口先排空防
    /// 早退路径（快捷命令）残留上次提示。
    pub(crate) tail_extension_hint: std::cell::RefCell<Option<Candidate>>,
    /// 预编辑显示的「首选句读音」覆盖（fangandehen 显示/首选分裂场景）：
    /// 学习（green_memo）或仲裁把另一读法的整句顶上首选时，选举切分
    /// 仍是词频裁决的旧读法（fang'an vs 首选 反感得很=fan'gan），预编辑显示
    /// 与首选分裂。每次 process 后记录「首选全覆盖绿词的读音切分」，
    /// syllable_segmentation 对同 input 优先采用（显示跟随首选）。
    /// 覆盖条件极严：is_sentence 且拼音去分隔符后与输入逐字母相等——同字母串
    /// 不同边界的分歧（n/ng 类），任何候选的消费字母数两读法等价，消费链
    ///（consumed_lens 按候选自身拼音 + 选举偏移）不受影响，wangou 同源
    /// 不变量不破。None = 无覆盖（syllable_segmentation 走选举原逻辑）。
    pub(crate) display_reading_override: std::cell::RefCell<Option<(String, Vec<String>)>>,
    /// 绿词记忆：用户自组词的整句（如"各显神通"），会话级有效。
    /// decode_sentence 的 query_seg 查不到 dict 时回退查这里。
    /// 不入 user_words（不固化白词），不持久化。重启后靠 bigram 举一反三。
    ///
    /// 二级索引：
    /// 外层 key = pinyin（查询时 O(1) get），内层 = [(text, freq, 学习时真实读音), ...]。
    /// GREEN_MEMO_MAX_ENTRIES 控制条目数量上界，超过时整体 clear
    ///（会话级缓存，clear 后靠 bigram 举一反三重建）。
    ///
    /// 第三元：commit 时的真实读音
    ///（fan gan de hen）。提升/召回构造候选时优先用它做 pinyin——否则候选
    /// 携带的是当选切分读音（fang an de hen），文本与拼音自相矛盾，且预编辑
    /// 显示无法跟随首选读法。启动从 input_mappings 恢复的条目无此信息
    ///（表无拼音列）→ None → 回退当选切分（旧行为）。
    pub(crate) green_memo: HashMap<String, Vec<(String, u32, Option<String>)>>, // pinyin → [(text, freq, reading)]
    /// 固定候选槽位表：原始输入串 → 1-3 号位固定文本。
    /// 持久化在 user_pins 表（与 green_memo 键口径一致；green_memo 是隐式
    /// 学习、pin 是显式钉死——pin 在候选组装最后一步注入，压过一切提升）。
    /// 详见 pins.rs 头注。
    pub(crate) pins: HashMap<String, pins::PinSlots>,
    /// 自适应 bigram cap 缓存（recompute_modulators 周期更新，热路径只读）。
    /// 冷启动 = BIGRAM_CAP_BASE，后台 60s 重算后随 max_user_bigram 对数增长。
    pub(crate) dyn_cap_cache: u32,
    /// 绿词采纳率统计。
    /// 持久化到 db_meta，跨会话累积；冷启动为 0（纯动态阈值）。
    pub(crate) green_accept: GreenAcceptStats,
    /// 当前双拼方案。None = 双拼关闭（全拼模式，默认）。见 shuangpin.rs。
    pub(crate) shuangpin: Option<crate::shuangpin::ShuangpinScheme>,
    /// 临时强制全拼模式：双拼模式下用 /q 触发。
    /// true 时 process_inner 跳过双拼分支，走全拼路径。resetState 时宿主前端通过 FFI 设回 false。
    pub(crate) force_fullpinyin: bool,
    /// /emoj 表情规模偏好：false = 常用档（emojiScale=common，
    /// 默认），true = 全量档（full）。服务端配置总线经 set_emoji_full 注入。
    pub(crate) emoji_full: bool,
    /// 联想候选（不完全匹配/前缀延伸）开关：
    /// false 时引擎不再生成两类猜测候选——① 完整音节单字查询的前缀延伸
    /// （ji 拉来的 jian「间」/jie「接」）② 多音节末音节前缀猜测（yonghu→用坏）。
    /// 简拼（不完整音节段，如 j、zgrm）不受此开关影响——前缀匹配是简拼本质。
    pub(crate) guess_enabled: bool,
    /// 智能单词识别·中文模式：纯字母输入命中英文词表时
    /// 注入英文单词候选（精确 + 前缀补全，排序后定点插入）。偏好键 smartWordZh。
    pub(crate) smart_word_zh_enabled: bool,
    /// 中文模式子开关·前缀补全（偏好键 smartWordZhPrefix，默认开；总闸
    /// smartWordZh 门控）：未精确命中时给 ≤3 条前缀补全
    /// （mornin→morning）。关闭 = 只出精确英文命中。
    pub(crate) smart_word_zh_prefix_enabled: bool,
    /// 中文模式子开关·英文优先（偏好键 smartWordZhPriority，默认开；总闸门控）：
    /// 开 = 「确定 > 猜测」排位（英文精确命中排绿/紫/橙之前，happy 登顶）；
    /// 关 = 旧排位（英文跟在首个多字候选之后，绿/紫也算锚）。
    pub(crate) smart_word_zh_priority_enabled: bool,
    /// 智能单词识别·英文模式：英文输入前缀补全查询
    /// （english_complete，Windows 服务 EnglishComplete 消息消费）。偏好键 smartWordEn。
    pub(crate) smart_word_en_enabled: bool,
    /// 英文单词使用计数（词频动态调整：开了英文补全则
    /// 英文词频随使用调整，中/英文模式皆然）：word → 使用次数（绝对值，启动
    /// 自 user_english_counts 全量加载）。仅当 smartWordZh/En 任一开启时
    /// commit 才记入——补全都关时无消费面，不学习（与用户语义一致）。
    pub(crate) user_english: HashMap<String, u32>,
    /// 待落盘的英文计数增量（word → 自上次 flush 起的增量；flush 清空）。
    pub(crate) english_delta: HashMap<String, u32>,
    /// candidate_score 单查询缓存。
    /// 背景：candidate_cmp 在 sort_by 比较器里对每对候选各调一次 candidate_score
    /// （uncached），123 候选的排序 ≈860 次比较 ×2 = 1700+ 次打分（每次 ~7.5µs
    /// 的多次 hash 查找）→ 单次查询 17ms，按键热路径卡顿。
    /// 生命周期 = 单次 process_inner 调用（入口清空）：打分依赖的 counts/recent/
    /// context/input 在同一次查询内不变，text 做 key 安全。RefCell 内部可变性
    /// 与兼容（engine 只在主线程使用，无跨线程并发）。
    pub(crate) score_cache: std::cell::RefCell<HashMap<Arc<str>, u64>>,
    /// 窗口成词 freq 缓存：
    /// 切分打分的惩罚豁免与 tie-break 窗口比较都查 window_word_freq，同一
    /// 歧义窗口（如 "fang an"）会在最多 16 个切分里重复出现——单次查询内
    /// 按窗口 key 缓存，64 次查询去重到个位数（实测长句 +21% 开销收回大半）。
    /// 生命周期 = 单次 process_inner 调用（入口清空，同 score_cache）。
    pub(crate) window_freq_cache: std::cell::RefCell<HashMap<String, u32>>,
    /// 双击故障时序感知：`timing_aware` 在前端首次传入
    /// 击键间隔后置位；`double_fault` 记录"当前 composition 里存在间隔 <
    /// DOUBLE_FAULT_MS 的相邻同字母对"（键盘 bounce 签名）。折叠纠错
    /// （inject_collapse_corrections）据此门控——正常节奏的重复字符是用户
    /// 刻意输入（shen+n+g 简拼），不折叠；只有故障级快双击才折。
    /// 未传时序的平台（旧前端/测试）保持旧行为（恒可折叠），零回归。
    /// Cell 内部可变性与兼容（engine 单线程）。
    pub(crate) timing_aware: std::cell::Cell<bool>,
    pub(crate) double_fault: std::cell::Cell<bool>,
    /// trigram 上下文总次数缓存。
    /// (prev2, prev1) → SUM(count)。会话级，HashMap + 512 上限满即 clear
    /// （green_memo 同款策略，clear 后 SQL 按需重建，无正确性影响）。
    /// RefCell 内部可变性与兼容（engine 只在主线程使用）。
    pub(crate) trigram_total_cache: std::cell::RefCell<HashMap<(String, String), u64>>,
    /// 种子 trigram 桶级会话缓存。
    /// (prev2, prev1) → 整桶 {text → count}。冷启动用户 trigram 表为空时，
    /// 候选打分 + DP 转移 + LM 先验每按键 400-600 次单条 SQL → 桶缓存后 2-3 次。
    /// 策略与 trigram_total_cache 同款：512 桶满即整体 clear（clear 后按需重建，
    /// 无正确性影响；桶是单条查询的超集，同桶内 get(text) 语义完全一致）。
    /// RefCell 内部可变性与兼容（engine 只在主线程使用）。
    pub(crate) trigram_bucket_cache:
        std::cell::RefCell<HashMap<(String, String), HashMap<String, u32>>>,
    /// 绿词 DP 段查询缓存。
    /// 键 = (简拼模式, 音节段空格串, 末段标志)，值 = dict 级查询（query_tokens /
    /// query + 末段单字边补充）经字数 retain 后的候选——不含 counts 排序与
    /// truncate（那两层每键新鲜应用，选择学习即时生效）。
    /// 跨按键复用是收益来源：打长句时稳定前缀段的简拼子树收集（单字母段收集
    /// 整棵子树，实测占 DP 耗时 78%，n=15 时 11.3/14.6ms）不每键重算。
    /// 只依赖 self.dict → 失效仅挂 dict 变异点（insert_user_word_to_dict /
    /// 各 remove_by_text 位 / rebuild_dict，见 seg_cache_clear）；counts/recent/
    /// green_memo/context 变化不影响本层。会话级，4096 条满即整体 clear
    /// （trigram_bucket_cache 同款策略）。RefCell 与兼容（单线程）。
    pub(crate) seg_cache: std::cell::RefCell<HashMap<(bool, String, bool), Vec<Candidate>>>,
}

impl Engine {
    /// 创建引擎并加载内置词库（向后兼容入口）
    ///
    /// 不使用 SQLite，用户频率仅存内存。
    // 不实现 Default：`Engine::new` 会装载内置词库（有语义的初始化），
    // 实现 Default 会让调用方误以为得到"空引擎"。
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let mut dict = Dict::new();
        dict.load_from_text(crate::dict_data::BUILTIN_DICT);
        dict.finish_load();

        Self {
            dict,
            freq: FreqModel::new(),
            trigram: crate::trigram::TrigramModel::new(),
            competition: crate::competition::CompetitionStore::new(),
            competition_priors: std::cell::RefCell::new(HashMap::new()),
            user_words: HashMap::new(),
            pending_upserts: HashSet::new(),
            pending_deletions: HashSet::new(),
            pending_reading_prune: HashSet::new(),
            removed_texts: HashSet::new(),
            input_mappings: HashMap::new(),
            input_mappings_by_text: HashMap::new(),
            ranker_weights: None,
            storage: None,
            dirty: false,
            last_flush_breakdown: Vec::new(),
            storage_load_incomplete: false,
            pending_blacklist: Vec::new(),
            fuzzy_config: FuzzyConfig::default(),
            input_mapping_enabled: true,
            context: Vec::new(),
            last_commit_ngrams: None,
            learning_stats: LearningStats::default(),
            learning_stats_enabled: true,
            suppress_collocation_after_word: false,
            smart_correction_enabled: true,
            keyboard_correction_enabled: true,
            phonetic_correction_enabled: true,
            swap_correction_enabled: true,
            max_corrections: Self::DEFAULT_MAX_CORRECTIONS,
            auto_solidify_threshold: Self::DEFAULT_AUTO_SOLIDIFY_THRESHOLD,
            green_candidate_count: 1,
            collected_candidates: None,
            collect_sentence_paths: std::cell::Cell::new(false),
            last_sentence_paths: std::cell::RefCell::new(Vec::new()),
            tail_extension_hint: std::cell::RefCell::new(None),
            display_reading_override: std::cell::RefCell::new(None),
            green_memo: HashMap::new(),
            pins: HashMap::new(),
            dyn_cap_cache: Self::BIGRAM_CAP_BASE,
            green_accept: GreenAcceptStats::default(),
            shuangpin: None,
            force_fullpinyin: false,
            emoji_full: false,
            guess_enabled: true,
            smart_word_zh_enabled: true,
            smart_word_zh_prefix_enabled: true,
            smart_word_zh_priority_enabled: true,
            // 英文模式单词补全：引擎初始值 false（新
            // 部署默认关；生产由配置总线按 prefs 注入覆盖，见 config.rs）。
            smart_word_en_enabled: false,
            user_english: HashMap::new(),
            english_delta: HashMap::new(),
            score_cache: std::cell::RefCell::new(HashMap::new()),
            window_freq_cache: std::cell::RefCell::new(HashMap::new()),
            timing_aware: std::cell::Cell::new(false),
            double_fault: std::cell::Cell::new(false),
            double_fault_threshold_ms: 100,
            ambig_split_enabled: true,
            ambig_split_position: 2,
            trigram_total_cache: std::cell::RefCell::new(HashMap::new()),
            trigram_bucket_cache: std::cell::RefCell::new(HashMap::new()),
            seg_cache: std::cell::RefCell::new(HashMap::new()),
        }
    }

    /// 创建引擎并从 SQLite 数据库加载
    ///
    /// 流程：
    /// 1. 打开 / 创建 DB（自动建表）
    /// 2. 若 x1dict_words 非空 → 加载全表填充 Trie（替代内置词库）
    /// 3. 若 x1dict_words 为空 → 回退到内置词库（首次部署未导入 X1Dict）
    /// 4. 加载 user_words 到内存 HashMap 和 FreqModel，并回填 Trie
    ///
    /// 单库（合并）模式：所有表同处一个文件。双库分离请用 `open_split`。
    pub fn new_with_db(db_path: &str) -> Result<Self, rusqlite::Error> {
        let storage = Storage::open(db_path)?;
        Self::from_storage(storage)
    }

    /// 双库分离模式加载。
    ///
    /// `dict_path` = 系统词库 DB（可被种子母版整体覆盖），`user_path` = 用户库 DB
    /// （承载全部自学习数据，更新永不触碰）。加载逻辑与单库完全一致——分离只改变
    /// 「哪张表在哪个文件」，不改变任何查询/排序/学习语义。
    pub fn open_split(dict_path: &str, user_path: &str) -> Result<Self, rusqlite::Error> {
        let storage = Storage::open_split(dict_path, user_path)?;
        Self::from_storage(storage)
    }

    /// 从已打开的 Storage 加载引擎（两种模式共用）。
    fn from_storage(mut storage: Storage) -> Result<Self, rusqlite::Error> {
        let mut dict = Dict::new();
        let mut freq = FreqModel::new();
        // ── 词典基底：编译缓存优先──
        // 缓存键 = (魔数, x1dict_sync_hash, x1dict_rev, 行数)；命中则 mmap/字节
        // 装载（跳过 SQL 全表扫描 + 节点 Trie 构建 + 转储），失配/损坏/无键一律
        // 回退构建路径并回写（自愈）。缓存只含系统词——用户词在下方
        // finish 之后走 overlay（缓存键按系统词库哈希成立的前提，见设计文档 §2.1）。
        let use_builtin = storage.x1dict_is_empty()?;
        let mut cached_ok = false;
        if !use_builtin {
            if let Some((arena, wi, sc)) = crate::storage::dict_cache::load(&storage) {
                if let Some(d) = Dict::from_cached(arena, wi, &sc) {
                    dict = d;
                    cached_ok = true;
                }
            }
        }
        if !cached_ok {
            if use_builtin {
                dict.load_from_text(crate::dict_data::BUILTIN_DICT);
            } else {
                // 流式加载（逐行 insert 进 Trie），替代全量物化
                // Vec（187k 词启动峰值内存 -60-80MB，见 Storage::for_each_trie_word）。
                storage.for_each_trie_word(|pinyin, text, entry_freq, is_primary| {
                    dict.insert(pinyin, text, entry_freq, is_primary);
                    Ok(())
                })?;
            }
            dict.shrink_to_fit(); // 构建期容量超配释放（同旧路径，省 ~40MB）
            dict.finish_load();
            if !use_builtin {
                crate::storage::dict_cache::save(&storage, &dict); // 尽力回写（失败静默）
            }
        }

        let _ = storage.cleanup_single_char_user_words();
        let _ = storage.backfill_user_pinyin_from_x1dict();
        // 空态覆写闸的信号量：任一表读失败 → 置位，
        // flush 跳过全量快照（见 dict_admin.rs flush 内闸门）。
        let mut storage_load_incomplete = false;

        let mut user_words: HashMap<String, Vec<String>> = HashMap::new();

        // 加载用户词（含自造词）到内存，并回填 Trie。
        // 注意：不在启动时做任何脏数据清理——清理逻辑误判会丢失用户真实输入的
        // 长词（违背「用户 DB 数据不可丢」约束），且全量遍历拖慢启动（实测 3 秒卡顿）。
        // 历史脏整句（早期版本误存的整体）由 commit 拆词学习自然修正——用户重新选中时
        // 走拆词分支并 remove 脏整体记录。如需显式清理，提供独立维护命令而非启动时静默删。
        //
        // 起本段在 finish_load 之后：用户词统一走 arena 模式
        // 的运行时插入出口（`insert_runtime`：基底命中→覆盖表 / 未命中→overlay 节点
        // Trie）——与运行时学习/`reload_user_state` 同一语义路径（那条路径生产已长期
        // 演练）；基底（arena）= 纯系统词，缓存才可按系统词库键控。
        match storage.load_user_words() {
            Ok(rows) => {
                let mut mismatch_skipped = 0usize;
                for (text, pinyin, user_freq) in rows {
                    if text.chars().count() <= 1 {
                        continue;
                    }
                    // 字数==音节数守卫：历史版本学入的错位
                    // 行（如「习惯惯」→"xi guan"，组合学习步骤拼音缺失的产物）挂在短
                    // 音节 Trie 键下，输入 "xiguan" 把 3 字词当整词候选顶出 = 微信
                    // "习惯惯"末字重复上屏。跳过不删除（不丢用户数据 /
                    // 零启动成本——O(1) 检查在现有循环内，无额外 DB 遍历）：
                    // 错位行在 Trie 上本就无法正确服务，跳过后成为惰性数据。
                    if !user_word_pinyin_matches(&text, &pinyin) {
                        mismatch_skipped += 1;
                        continue;
                    }
                    let pys = user_words.entry(text.clone()).or_default();
                    if !pys.iter().any(|p| p == &pinyin) {
                        pys.push(pinyin.clone());
                    }
                    freq.set_count(&text, user_freq);
                    // 用户词合并进 dict：如果该词已在 dict（来自 x1dict_words 真实 freq），
                    // 保留真实 freq。新自造词用 compute_freq 计算合理词频（1~9000 范围），
                    // 不再用 50000 虚高值——虚高频抹平词频差异导致选错字。
                    // 用户词的排序靠 recent_score（用户偏好）+ bigram（上下文关联）体现，
                    // 不靠 base_freq 注水。
                    let existing_freq = dict.find_freq(&pinyin, &text);
                    let insert_freq = existing_freq.unwrap_or_else(|| {
                        // 新自造词：用 compute_freq 算合理词频
                        let computed = crate::char_freq::compute_freq(&text);
                        computed.max(1) // 至少 1，保证可查
                    });
                    dict.insert(&pinyin, &text, insert_freq, true);
                }
                if mismatch_skipped > 0 {
                    eprintln!(
                        "[engine] 加载 user_words：跳过 {} 条字数≠音节数的错位行（历史脏数据，重复上屏根因；行保留在 DB 未删除）",
                        mismatch_skipped
                    );
                }
            }
            Err(_) => storage_load_incomplete = true,
        }
        dict.shrink_to_fit(); // overlay 侧（用户词节点，极小）容量收敛

        // 从 user_words 多字整句反推跨词 bigram 种子。
        //
        // 背景：现有种子只从 x1dict_words 词内反推（飞机→(飞,机)），但 DP 加分用的是
        // 段间词对（坐→飞机）。跨词 bigram (坐,飞机) 全为 0，DP 完全无力纠偏。
        // （早期从 user_words 字数≥3 的整句固化词反推相邻字对作为跨词 bigram 种子，
        // 如「坐飞机出行」→(坐,飞)(飞,机)(机,出)(出,行)。但这些全是字+字，价值低：
        // 真正有价值的跨词 bigram 是词+词 (坐,飞机)，靠离线挖掘 + 用户上屏积累。
        // 字+字种子不被纯词级 DP 句中查询消费，已删除。）

        // 加载用户输入映射到内存
        let input_mappings = match storage.load_input_mappings() {
            Ok(v) => v,
            Err(_) => {
                storage_load_incomplete = true;
                Default::default()
            }
        };
        let input_mappings_by_text = Self::build_mapping_index(&input_mappings);

        // 加载拼音级频率
        let pinyin_counts = match storage.load_pinyin_counts() {
            Ok(v) => v,
            Err(_) => {
                storage_load_incomplete = true;
                Default::default()
            }
        };
        freq.set_pinyin_counts(pinyin_counts);

        // 加载 bigram（词级，用户真实意图数据，落盘）
        let bigram_counts = match storage.load_bigram_counts() {
            Ok(v) => v,
            Err(_) => {
                storage_load_incomplete = true;
                Default::default()
            }
        };
        freq.set_bigram_counts(bigram_counts);
        // 注：字级搭配（如吃→饭）不预存，改为运行时按需查词库组合词（dict.collocation_freq）。
        // 用户 commit「你」后输入「好」，查「你好」是否存在给「好」加分。
        // 零启动开销 + 零内存占用。

        // 加载种子 bigram。
        // 老用户 DB（之前）无 seed_bigram_counts 表，先 ensure 建空表。
        // 种子数据由 import_seed_bigram.sh 离线预填到种子 DB，随 app bundle 流转到用户 DB。
        // 查询时用户优先（word_bigram_count 三层查询），种子只在用户没 commit 过时兜底。
        //改走磁盘缓存快速路径（load_seed_bigram_table_cached）——
        // 缓存键 x1dict_sync_hash，失配/缺失自动回退 SQL 全量 + CSR 构建并重写缓存。
        let _ = storage.ensure_seed_bigram_table();
        match storage.load_seed_bigram_table_cached() {
            Ok(t) => freq.set_seed_bigram_table(t),
            Err(_) => storage_load_incomplete = true,
        }
        // 加载 trigram。冷启动为空，完全靠用户积累。
        let mut trigram = crate::trigram::TrigramModel::new();
        let trigram_entries = match storage.load_trigram_counts() {
            Ok(v) => v,
            Err(_) => {
                storage_load_incomplete = true;
                Default::default()
            }
        };
        trigram.set_counts(trigram_entries);
        // set_counts 把 dirty 置 true（数据源变更标记），但启动加载后内存与磁盘一致，
        // 清除 dirty 避免首次 flush 做无谓全量写回。
        trigram.clear_dirty();
        // 种子 trigram 不启动加载——100k 行全量读入实测 +30~60ms，
        // 贴近 300ms 启动目标线。改按需 SQL 查询（trigram_count_with_seed），
        // 启动零成本。此处只 ensure 表存在（老用户 DB 建空表，O(1) 元数据操作）。
        let _ = storage.ensure_seed_trigram_table();

        // 加载用户选择次数。系统词去重后，
        // 系统词不写 user_words，上面 load_user_words 填的 counts 不含系统词。
        // user_counts 是 flush 时全量快照（含系统词+非系统词），是 counts 的权威源，
        // 覆盖上面 user_words.user_freq 填的初值。空表（首次部署）保持初值不变。
        let counts = match storage.load_counts() {
            Ok(v) => v,
            Err(_) => {
                storage_load_incomplete = true;
                Default::default()
            }
        };
        if !counts.is_empty() {
            freq.set_counts(counts);
        }

        // 加载英文单词使用计数：表小（用户真用过的
        // 英文词，量级数百行）全量加载；读失败 → 空态 + 记加载不完整。
        let mut user_english: HashMap<String, u32> = HashMap::new();
        match storage.load_english_counts() {
            Ok(v) => {
                for (w, c) in v {
                    user_english.insert(w, c);
                }
            }
            Err(_) => storage_load_incomplete = true,
        }

        // 加载竞争频率：competition* 两表一次性
        // 读入（与学习数据同模式）。表不存在（老用户 DB）→ init_schema 已建空表，
        // 读空即可；读失败 → 空态 + 记加载不完整（防空态覆写闸会跳过全量快照）。
        let mut competition = crate::competition::CompetitionStore::new();
        let comp_sel = storage.load_competition_selects().unwrap_or_else(|_| {
            storage_load_incomplete = true;
            Vec::new()
        });
        let comp_tot = storage.load_competition_totals().unwrap_or_else(|_| {
            storage_load_incomplete = true;
            Vec::new()
        });
        competition.load(comp_sel, comp_tot);

        // 刚加载完，内存状态与磁盘一致，标记为干净
        freq.mark_loaded();

        // 重建 green_memo：
        // 从 input_mappings 恢复用户选过的绿词。key 用原始 input（不再推导拼音）。
        // count>=1（首次即记忆，不再要求 count>=2）。
        let mut green_memo: HashMap<String, Vec<(String, u32, Option<String>)>> = HashMap::new();
        for ((input, text), count) in &input_mappings {
            if *count < 1 {
                continue;
            }
            let char_count = text.chars().count();
            if !(3..=8).contains(&char_count) {
                continue;
            }
            if input.is_empty() {
                continue;
            }
            // key = 原始 input（与 commit_internal 写入、decode_sentence 查询完全一致）
            // 恢复条目无学习时读音（input_mappings 无拼音列）→ None（显示回退
            // 当选切分，旧行为；会话内再次 commit 即升级为精确读音）。
            let user_freq = (1000 + *count * 1000).min(10000);
            green_memo
                .entry(input.clone())
                .or_default()
                .push((text.clone(), user_freq, None));
        }
        // 恢复上限：input_mappings 无上限增长（用户长期
        // 使用 + 固化只清 user_words/user_blacklist 不清 input_mappings），恢复的
        // green_memo 可能远超 GREEN_MEMO_MAX_ENTRIES（200）→ 首次 commit 写入即
        // 超限 → 整体 clear → 历史绿词 + 新学绿词全部丢失（实测用户 DB 1429 条映射时
        // green_memo 完全失效）。修复：恢复截断到 MAX/2（按 key 内最高 user_freq 降序
        // 保留最高频记忆），给会话内新学习留一半空间（新学优先，历史高频保留）。
        if green_memo.len() > Self::GREEN_MEMO_MAX_ENTRIES / 2 {
            let mut keys: Vec<(String, u32)> = green_memo
                .iter()
                .map(|(k, v)| (k.clone(), v.iter().map(|(_, f, _)| *f).max().unwrap_or(0)))
                .collect();
            keys.sort_by_key(|x| std::cmp::Reverse(x.1));
            keys.truncate(Self::GREEN_MEMO_MAX_ENTRIES / 2);
            green_memo = keys
                .into_iter()
                .filter_map(|(k, _)| green_memo.remove(&k).map(|v| (k, v)))
                .collect();
        }

        // 加载持久化黑名单（user_blacklist 表）：用户删除过的词跨会话/跨部署保留。
        // 填入 removed_texts，让整句解码降级处理这些词（不排首位，但仍可选以复活）。
        // 部署脚本合并数据时也会读这张表，跳过黑名单词，并清理种子 DB 同名词。
        let blacklist = match storage.load_blacklist() {
            Ok(v) => v,
            Err(_) => {
                storage_load_incomplete = true;
                Default::default()
            }
        };

        // 固定候选：启动全量加载（小表，个位数到几十行）。
        let pins: HashMap<String, pins::PinSlots> = match storage.load_pins() {
            Ok(rows) => {
                let mut m: HashMap<String, pins::PinSlots> = HashMap::new();
                for (input, slot, text) in rows {
                    if (1..=3).contains(&slot) {
                        m.entry(input).or_default()[(slot - 1) as usize] = Some(text);
                    }
                }
                m
            }
            Err(_) => {
                storage_load_incomplete = true;
                Default::default()
            }
        };

        let engine = Engine {
            dict,
            freq,
            trigram,
            competition,
            competition_priors: std::cell::RefCell::new(HashMap::new()),
            user_words,
            user_english,
            pending_upserts: HashSet::new(),
            pending_deletions: HashSet::new(),
            pending_reading_prune: HashSet::new(),
            removed_texts: blacklist,
            input_mappings,
            input_mappings_by_text,
            ranker_weights: None,
            storage: Some(storage),
            dirty: false,
            last_flush_breakdown: Vec::new(),
            storage_load_incomplete,
            pending_blacklist: Vec::new(),
            fuzzy_config: FuzzyConfig::default(),
            input_mapping_enabled: true,
            context: Vec::new(),
            last_commit_ngrams: None,
            learning_stats: LearningStats::default(),
            learning_stats_enabled: true,
            suppress_collocation_after_word: false,
            smart_correction_enabled: true,
            keyboard_correction_enabled: true,
            phonetic_correction_enabled: true,
            swap_correction_enabled: true,
            max_corrections: Self::DEFAULT_MAX_CORRECTIONS,
            auto_solidify_threshold: Self::DEFAULT_AUTO_SOLIDIFY_THRESHOLD,
            green_candidate_count: 1,
            collected_candidates: None,
            collect_sentence_paths: std::cell::Cell::new(false),
            last_sentence_paths: std::cell::RefCell::new(Vec::new()),
            tail_extension_hint: std::cell::RefCell::new(None),
            display_reading_override: std::cell::RefCell::new(None),
            green_memo,
            pins,
            dyn_cap_cache: Self::BIGRAM_CAP_BASE,
            green_accept: GreenAcceptStats::default(),
            shuangpin: None,
            force_fullpinyin: false,
            emoji_full: false,
            guess_enabled: true,
            smart_word_zh_enabled: true,
            smart_word_zh_prefix_enabled: true,
            smart_word_zh_priority_enabled: true,
            // 英文模式单词补全：引擎初始值 false（新
            // 部署默认关；生产由配置总线按 prefs 注入覆盖，见 config.rs）。
            smart_word_en_enabled: false,
            // user_english 用上方 shorthand（from_storage 加载值）；delta 恒新起（启动时内存与磁盘一致）。
            english_delta: HashMap::new(),
            score_cache: std::cell::RefCell::new(HashMap::new()),
            window_freq_cache: std::cell::RefCell::new(HashMap::new()),
            timing_aware: std::cell::Cell::new(false),
            double_fault: std::cell::Cell::new(false),
            double_fault_threshold_ms: 100,
            ambig_split_enabled: true,
            ambig_split_position: 2,
            trigram_total_cache: std::cell::RefCell::new(HashMap::new()),
            trigram_bucket_cache: std::cell::RefCell::new(HashMap::new()),
            seg_cache: std::cell::RefCell::new(HashMap::new()),
        };

        // 自适应排序：用加载的 bigram 数据初始化 dyn_cap_cache
        // （max_user_bigram 从 user_bigram_counts 表加载，决定 dyn_cap 初始值）
        // 只算 dyn_cap，不衰减：
        // 启动时衰减会让刚加载的低频词（count=2）立即降权，违背"累计次数应持久化"语义。
        let mut engine = engine;
        engine.init_dyn_cap();

        // 加载绿词采纳率统计：
        // 从 db_meta 恢复 total_green_picks（second_green_picks 暂未采集，见 commit TODO）。
        if let Some(storage) = &engine.storage {
            let total = storage
                .get_user_meta("green_total_picks")
                .ok()
                .flatten()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            engine.green_accept.total_green_picks = total;

            // 恢复上下文栈快照：flush 时写入的栈尾词跨会话延续，
            // bigram/trigram/collocation 上下文加成不再随进程重启冷启动。
            // 一次 meta 读取，启动成本可忽略。
            if let Ok(Some(snap)) = storage.get_user_meta("context_snapshot") {
                for w in snap.split('\u{1}').filter(|w| !w.is_empty()) {
                    engine.context.push(w.to_string());
                }
            }
        }

        // 防 set_ranker_weights 符号消除：取函数指针制造 Rust 内部引用，
        // 链接器必拉符号。主因是宿主前端的死代码消除，此处双保险。
        let _ = Engine::set_ranker_weights as fn(&mut Engine, [f64; 6]);

        // 构建期瞬态收尾：词典 arena 化后节点 Trie 只是
        // 构建期结构（finish_load 丢弃），但几十 MB 瞬态释放后被 malloc 攥成
        // 空闲缓存——phys_footprint 实测 98MB 中 ~44MB 是 Reclaimable，观感与
        // 真实占用脱节。立即归还系统，后续分配按需再取。
        Self::release_allocator_slack();

        Ok(engine)
    }

    /// 把分配器攥着的空闲堆页立即归还系统。
    ///
    /// 平台无关委托：实现已移入 `crate::platform`——引擎层
    /// 不得出现平台分支（守护 `tests_platform_purity`）。macOS 有实际效果，
    /// 其他平台 no-op；两平台引擎行为完全一致（只影响内存归还时机）。
    ///
    /// 调用点：`from_storage` 收尾（启动）与 `rebuild_dict` 收尾（sync_x1dict
    /// 词库重载，同款构建瞬态）。仅内存归还，零数据影响。
    pub(crate) fn release_allocator_slack() {
        crate::platform::release_allocator_slack();
    }

    /// 把用户词插入当前查询 Trie，使用合理的基础频率
    ///
    /// 新词用 compute_freq 计算（1~9000 范围），不用 50000 虚高值。
    /// 已有词保留真实 freq（由调用方 find_freq 判断后传入）。
    /// 用户词排序靠 recent_score + bigram 体现，不靠 base_freq 注水。
    fn insert_user_word_to_dict(&mut self, text: &str, pinyin: &str, _user_freq: u32) {
        let existing = self.dict.find_freq(pinyin, text);
        let freq = existing.unwrap_or_else(|| crate::char_freq::compute_freq(text).max(1));
        self.dict.insert(pinyin, text, freq, true);
        self.seg_cache_clear();
    }

    /// 段查询缓存层：返回 dict 级查询 + 字数 retain 后的
    /// 候选，供 decode_sentence_paths 的 query_seg 跨按键复用。刻意不含
    /// counts 排序 / truncate / green_memo 补充——那些每键新鲜应用（学习即时
    /// 生效），本层只缓存纯 dict 依赖部分。键见 seg_cache 字段注释。
    pub(crate) fn seg_query_cached(
        &self,
        seg: &[&str],
        is_initial_mode: bool,
        is_last: bool,
    ) -> Vec<crate::dict::Candidate> {
        let key = (is_initial_mode, seg.join(" "), is_initial_mode && is_last);
        if let Some(hit) = self.seg_cache.borrow().get(&key) {
            return hit.clone();
        }
        let cands = if is_initial_mode {
            let tokens: Vec<crate::pinyin::Token> = seg
                .iter()
                .map(|s| {
                    if s.len() == 1 {
                        crate::pinyin::Token::Initial(s.chars().next().unwrap())
                    } else if *s == "zh" || *s == "sh" || *s == "ch" {
                        crate::pinyin::Token::Shengmu(s)
                    } else {
                        crate::pinyin::Token::Full(s)
                    }
                })
                .collect();
            let mut cands = self.dict.query_tokens(&tokens);
            // 末位全拼单字边：query_tokens
            // 混合分支只收「字数 ≥ 2」词级候选（单字约定由首音节补充逻辑展示），
            // 全拼模式下该位由 dict.query 提供单字；initial 模式的混合段因此
            // 在末位音节处缺单字边——整句落不了位（中国人民|解放军|南 的
            // [nan] 段无 南 边 → DP 只能落到 警方|济南 等同段替代（实拍案例：
            // 「打到南京前塌成单字」）。仅补末段（i+k==n）：末音节是用户
            // 正在续打的位置（南 → 南京），与全拼模式「de 结尾出 的」行为一致；
            // 中段全拼位由既有词级 span 机制承载（跨段成词如 过得好；测试
            // verify_branch_quota 的中段 得 依赖词合并，不在此补，基准不变）。
            if is_last && seg.len() == 1 && seg[0].len() >= 2 {
                cands.extend(self.dict.query(seg));
            }
            // 中段全 Full 段补单字边：段内全部是
            // 完整音节（无 Initial/Shengmu）时，该段语义与全拼模式的同一段等价
            // ——用户打 de 就是想要这一格，必须给单字候选，否则 womendeyanj 的
            // [我们|的|研究] 会在中段「的」断链（第二路 DP 全空）。纯简拼段
            // （n/a/n）与混合段（yan+j）不在此补，简拼语义不受影响。
            if seg
                .iter()
                .all(|s| s.len() >= 2 && crate::pinyin::is_valid_syllable_pub(s))
            {
                cands.extend(self.dict.query(seg));
            }
            // 全消费守卫：
            // edge 语义 = 「这段输入解释成这个词」——词必须恰好解释整个 span。
            // 旧 ≤ 守卫允许短词占长 span，会让路径「吞词」：juglzshuomwt 的
            // [ju,g,l,z] 四段被 2 字「莒国」占据（吞掉 l/z 两个 token），结构分
            // 按段数 (k-1)*2 拿到 6（比诚实 [ju,g] span 的 5 还高），且省掉
            // [l,z] 一条边（少一份 LM 负分 + 搭配惩罚）——吞词路径系统性占优，
            // 把「举个例子|说明问题」挤出绿词输出（单打 juglz /
            // shuomwt 都行，拼起来不行）。
            // == 同时天然涵盖旧守卫的防重复长串：seg=[w] 只留单字，wwww 不再
            // 拼出「文件清单×4」。词层前缀词展示不受影响——那是
            // process_hybrid_shengmu 直调 query_tokens 收集的，不走这里。
            let seg_len = seg.len();
            cands.retain(|c| c.text.chars().count() == seg_len);
            cands
        } else {
            self.dict.query(seg)
        };
        let mut cache = self.seg_cache.borrow_mut();
        if cache.len() >= 4096 {
            cache.clear();
        }
        cache.insert(key, cands.clone());
        cands
    }

    /// 段查询缓存失效（方案 A）：只在 dict 变异点调用——insert_user_word_to_dict
    ///（Trie 唯一 insert 漏斗）、各 remove_by_text 位、rebuild_dict（self.dict 整体
    /// 替换）。counts/recent/green_memo/context 变化不需要清（不在缓存依赖里）。
    pub(crate) fn seg_cache_clear(&mut self) {
        self.seg_cache.borrow_mut().clear();
    }

    /// 记录用户词的一个读音并重建该词的全部 Trie 条目。
    ///
    /// 新读音追加（已存在则幂等）；`remove_by_text` 一次后把该词全部读音重插——
    /// 既保持"用户词覆盖系统词"语义，又不冲掉其他读音
    /// （旧实现 text 键直接覆盖 + 单读音重插，zhujue→主角 会把 zhujiao→主角 冲掉）。
    /// 调用方负责前置守卫（`user_word_pinyin_matches`）与 pending_upserts 标脏。
    fn record_user_word_pronunciation(&mut self, text: &str, pinyin: &str) {
        let pys = {
            let v = self.user_words.entry(text.to_string()).or_default();
            if !v.iter().any(|p| p == pinyin) {
                v.push(pinyin.to_string());
            }
            v.clone()
        };
        self.dict.remove_by_text(text);
        let count = self.freq.counts.get(text).copied().unwrap_or(1);
        for p in &pys {
            self.insert_user_word_to_dict(text, p, count);
        }
    }

    /// 从 DB 全量重建完整词库（系统词 + 用户词），清空/导入/版本迁移后使用。
    ///
    /// 重构：原版 `self.dict = self.base_dict.clone` + 重放 user_words，
    /// 依赖常驻的 base_dict 快照（clone 一份完整 Trie 占 ~90M 内存）。去掉 base_dict 后
    /// 改为从 SQL 全量重建。代价：维护操作从 clone(~10ms) 变全量重建(~300-400ms)，
    /// 但这些场景（导入词库/清空用户词/版本更新）都是低频维护操作，耗时可接受。
    /// 不在启动路径，不影响逐键热路径。
    ///
    /// ① 缓存优先（TrieArena 编译缓存，命中则跳过全表扫描 +
    /// 构建——`Reload`/管理器改库后的重建不再付 300-400ms）；② 用户词 overlay 化
    /// （finish_load 之后走 `insert_user_word` 的 arena 运行时出口，与旧语义逐条
    /// 对齐——max 合并 / 基底强设）；③ 缓存回写在替换 self.dict 之后（Windows：
    /// 删除/替换持有映射的文件必须在旧映射 drop 之后，见 dict_cache 模块头）。
    fn rebuild_dict(&mut self) {
        let mut dict = crate::dict::Dict::new();
        let empty = self
            .storage
            .as_ref()
            .map(|s| s.x1dict_is_empty().unwrap_or(true))
            .unwrap_or(true);
        // ① 缓存优先（仅真实词库；内置小词库不缓存）
        let mut cache_hit = false;
        if !empty {
            if let Some(storage) = &self.storage {
                if let Some((arena, wi, sc)) = crate::storage::dict_cache::load(storage) {
                    if let Some(d) = crate::dict::Dict::from_cached(arena, wi, &sc) {
                        dict = d;
                        cache_hit = true;
                    }
                }
            }
        }
        if !cache_hit {
            if empty {
                dict.load_from_text(crate::dict_data::BUILTIN_DICT);
            } else if let Some(storage) = &self.storage {
                // 流式加载（同 new_with_db，sync 重建 Trie 时
                // 新旧两份 Trie 共存已是峰值场景，物化 Vec 会雪上加霜）。
                if storage
                    .for_each_trie_word(|pinyin, text, freq, is_primary| {
                        dict.insert(pinyin, text, freq, is_primary);
                        Ok(())
                    })
                    .is_err()
                {
                    dict.load_from_text(crate::dict_data::BUILTIN_DICT);
                }
            } else {
                dict.load_from_text(crate::dict_data::BUILTIN_DICT);
            }
            // 重建后释放 Vec 超配容量（同 new_with_db，省 ~40MB）
            dict.shrink_to_fit();
            dict.finish_load(); // 排序统一词索引
        }
        // ② 叠加用户词（commit/learn 学的词，让它们立即出现在查询 Trie 里）。
        //用户词为主语义：同 (text, pinyin) 与系统词冲突时覆盖为
        // 用户词 freq（insert 的 max 语义会让系统词 freq 压过导入/学习的用户词）。
        // commit 路径的系统词不会进 user_words（is_system_word 检查），
        // 覆盖只影响真正的冲突场景（用户自造/导入词 vs 系统词）。
        // arena 模式 `insert_user_word` = insert（max 合并）+ 基底强设，与旧
        // finish 前路径逐条对齐（dict.rs insert_user_word 头注）。
        for (text, pys) in &self.user_words {
            let freq = self.freq.counts.get(text).copied().unwrap_or(1);
            for pinyin in pys {
                dict.insert_user_word(pinyin, text, freq, true);
            }
        }
        let rebuilt_from_sql = !cache_hit;
        self.dict = dict;
        self.seg_cache_clear();
        // ③ 回写缓存（替换旧 dict 之后——旧映射已 drop；尽力，失败静默）
        if rebuilt_from_sql && !empty {
            if let Some(storage) = &self.storage {
                crate::storage::dict_cache::save(storage, &self.dict);
            }
        }
        // sync 重载是「新旧两份 Trie 共存」的峰值场景，构建瞬态比启动更大——
        // 收尾同样把空闲页归还系统。
        Self::release_allocator_slack();
    }

    /// freq 压扁迁移。
    ///
    /// 历史： freq 模型重构时引入，把老用户 DB 的连续 freq 压扁到
    /// 4 档（100/200/300/500）。但 `ensure_freq_flattened` 的幂等性 bug——对已压扁
    /// DB 再压时 200/300/500 都 < 1000 被当成低频压成 100——导致用户 DB 全 100，
    /// base 分量完全失效。
    ///
    /// 决策：取消 freq 压扁，回归 compute_freq 真实值（50-9999）。
    /// 此方法保留签名（FFI `x1_engine_flatten_freq_and_reload` 仍可调用）但内部
    /// 直接返回 0，不再执行任何压扁。freq 修正改由 `restore_x1dict_freq_and_reload`
    /// （用种子 DB 覆盖用户 DB 的 x1dict_words）在启动迁移时完成。
    ///
    /// 返回 0（兼容旧 FFI 调用方，非 0 表示有改动——这里恒 0）。
    pub fn flatten_freq_and_reload(&mut self) -> usize {
        0
    }

    /// 用 bundle 种子 DB 的 x1dict_words 频率覆盖用户 DB。
    ///
    /// 修复 `ensure_freq_flattened` 幂等性 bug 导致的用户 DB freq 全 100 问题。
    /// 检测 db_meta `freq_unflattened_v1` 标记不存在 → 执行覆盖 → 写标记。
    /// 只动 x1dict_words.freq/is_primary，保留 user_words 等用户数据表。
    ///
    /// `builtin_path` 是 bundle 内置 DB 路径（Resources/x1pinyin.db）。
    /// 返回更新的词条数（0 表示已迁移过或失败）。
    pub fn restore_x1dict_freq_and_reload(&mut self, builtin_path: &str) -> usize {
        let storage = match self.storage.as_mut() {
            Some(s) => s,
            None => return 0,
        };
        // 幂等检查：已迁移过则跳过
        if let Ok(Some(v)) = storage.get_seed_meta("freq_unflattened_v1") {
            if v == "1" {
                return 0;
            }
        }
        // 只对真实用户 DB 执行（有 x1dict_sync_hash 标记说明经过 sync 流程）
        // 测试临时 DB 没这个标记，跳过避免破坏 setup
        if storage
            .get_seed_meta("x1dict_sync_hash")
            .ok()
            .flatten()
            .is_none()
        {
            return 0;
        }
        let updated = match storage.restore_x1dict_freq_from(builtin_path) {
            Ok(n) => n,
            Err(_) => return 0,
        };
        // 写入迁移标记
        let _ = storage.set_seed_meta("freq_unflattened_v1", "1");
        // rebuild_dict 从 DB 全量重建（拿到修复后的 freq + 重放 user_words）
        self.rebuild_dict();
        updated
    }

    /// 查询某个词的用户频率加成（供测试使用）
    pub fn user_freq(&self, text: &str) -> f32 {
        self.freq.user_freq(text)
    }

    /// 设置模糊拼音配置
    pub fn set_fuzzy_config(&mut self, config: FuzzyConfig) {
        self.fuzzy_config = config;
    }

    /// 设置双拼方案。None 关闭（全拼模式）。
    pub fn set_shuangpin_scheme(&mut self, scheme: Option<crate::shuangpin::ShuangpinScheme>) {
        self.shuangpin = scheme;
    }

    /// /emoj 表情规模：true = 全量档（emojiScale=full）。
    /// 服务端配置总线注入（mac 侧后续接同参数）。
    pub fn set_emoji_full(&mut self, full: bool) {
        self.emoji_full = full;
    }

    /// 长句第二备选开关（兼容旧布尔语义，见 green_candidate_count 字段注释）：
    /// true = 至少 2 条，false = 1 条。新调用方用 set_green_candidate_count。
    pub fn set_second_green_enabled(&mut self, enabled: bool) {
        self.green_candidate_count = if enabled {
            self.green_candidate_count.max(2)
        } else {
            1
        };
    }

    /// 长句绿词条数（1..=5，见字段注释）；1 = 只出首选。
    pub fn set_green_candidate_count(&mut self, count: u8) {
        self.green_candidate_count = count.clamp(1, 5);
    }

    pub fn set_guess_enabled(&mut self, enabled: bool) {
        self.guess_enabled = enabled;
    }

    /// 当前是否启用联想候选（诊断/服务端配置总线回读用）。
    pub fn guess_enabled(&self) -> bool {
        self.guess_enabled
    }

    /// 智能单词识别·中文模式开关（英文单词候选注入）。
    pub fn set_smart_word_zh_enabled(&mut self, enabled: bool) {
        self.smart_word_zh_enabled = enabled;
    }

    pub fn smart_word_zh_enabled(&self) -> bool {
        self.smart_word_zh_enabled
    }

    /// 中文模式子开关·前缀补全（smartWordZhPrefix）。
    pub fn set_smart_word_zh_prefix_enabled(&mut self, enabled: bool) {
        self.smart_word_zh_prefix_enabled = enabled;
    }

    /// 中文模式子开关·英文优先排位（smartWordZhPriority）。
    pub fn set_smart_word_zh_priority_enabled(&mut self, enabled: bool) {
        self.smart_word_zh_priority_enabled = enabled;
    }

    /// 智能单词识别·英文模式开关（英文前缀补全查询）。
    pub fn set_smart_word_en_enabled(&mut self, enabled: bool) {
        self.smart_word_en_enabled = enabled;
    }

    pub fn smart_word_en_enabled(&self) -> bool {
        self.smart_word_en_enabled
    }

    // 英文模式前缀补全（english_complete）与词表查询（english_lookup_exact/
    // prefix）在 engine/english.rs（入库化：词表存 dict.db
    // english_words 独立表，引擎不再编译内嵌——为翻译/词典功能留数据空间）。

    /// 设置临时强制全拼模式（/q 触发，commit/取消后宿主前端设回 false）。
    pub fn set_force_fullpinyin(&mut self, enabled: bool) {
        self.force_fullpinyin = enabled;
    }

    /// 开启/关闭整句路径收集（AI 切分裁决轮；&self——服务/FFI 单线程调用）。
    /// 开启后下一次 `process`/`process_timed` 的整句 DP（纯简拼/混合声母）会把
    /// top-K 路径写入内部槽，调用方随即 [`Self::take_sentence_paths`] 取走。
    pub fn set_collect_sentence_paths(&self, on: bool) {
        self.collect_sentence_paths.set(on);
    }

    /// 取走最近一次收集的整句路径（并清空内部槽；未收集 = 空表）。
    pub fn take_sentence_paths(&self) -> Vec<SentencePath> {
        std::mem::take(&mut self.last_sentence_paths.borrow_mut())
    }

    /// 收集上限（开启时 12 条；关闭 0——decode_sentence_paths 据此短路收集逻辑）。
    /// 上限 8→12 与收集模式的加宽 DP 对齐（TOP_N=12 最多产 12 条终点
    /// 路径）——实测「…汉字词句」这类正确句在 20 音节长句里排位 9-12（短输入
    /// 是，长句分数竞争被压），上限 8 会把它截掉（用户实录案例）。
    pub(crate) fn sentence_paths_limit(&self) -> usize {
        if self.collect_sentence_paths.get() {
            12
        } else {
            0
        }
    }

    /// 整句解码（带路径收集入口）：收集开启时把 DP 路径写入
    /// `last_sentence_paths`（覆盖式——每次 process 只有一条主调用：纯简拼与
    /// 混合声母路由互斥）。关闭时与 [`Self::decode_sentence`] 逐字节等价。
    ///
    ///（AI 组合扩展二期）：收集开启时双跑隔离——主候选来自
    /// 原参数（paths_limit=0）的一次 decode（AI 开/关候选逐字节一致的不变量
    /// 不破）；路径来自加宽 DP（TOP_N 5→12 / 词边 3→6，见
    /// `sentence::decode_sentence_paths` 注释）的第二次 decode——把被剪枝的
    /// 中段合法切分（「…汉字+词句」类）也暴露给 AI 字格。双跑成本：长句
    /// DP ~1-2ms ×2，仅 want_paths（AI 开+长句）时发生，主链路零变化。
    pub(crate) fn decode_sentence_collect(
        &self,
        input: &str,
        syllables: &[&str],
        is_initial_mode: bool,
        has_partial_tail: bool,
    ) -> Vec<Candidate> {
        let limit = self.sentence_paths_limit();
        if limit == 0 {
            let (cands, _) =
                self.decode_sentence_paths(input, syllables, is_initial_mode, has_partial_tail, 0);
            return cands;
        }
        // ① 主候选：原参数（与关闭收集时逐字节一致）
        let (cands, _) =
            self.decode_sentence_paths(input, syllables, is_initial_mode, has_partial_tail, 0);
        // ② 路径：加宽 DP（paths_limit>0 触发放宽）
        let (_, paths) =
            self.decode_sentence_paths(input, syllables, is_initial_mode, has_partial_tail, limit);
        if !paths.is_empty() {
            *self.last_sentence_paths.borrow_mut() = paths;
        }
        cands
    }

    /// 获取当前模糊拼音配置（供宿主前端同步）
    pub fn fuzzy_config(&self) -> &FuzzyConfig {
        &self.fuzzy_config
    }

    /// 处理输入更新，返回候选列表
    ///
    /// # Arguments
    /// * `input` — 当前输入缓冲区（如 "zhongwen" 或 "zw"）
    ///
    /// # Returns
    /// 候选词列表，按「字数少优先、同字数按综合分数降序」排序
    /// 同分 tie-break 的成词强度裁决（修复 jishufanganwendang；
    /// 从成对首分歧窗口比较改为逐侧 max_span_word_freq 键——
    /// 成对窗口随对变化可三元成环，172k 语料 learn 实测触发 sort 全序违规
    /// panic，判例行为由逐侧键复现，详见 scoring.rs 该函数注释）。
    pub fn process(&self, input: &str) -> Vec<Candidate> {
        // 输入长度护栏：pinyin 切分层已有 MAX_INPUT_BYTES
        // 单点护栏（防 DFS 递归栈溢出），这里再挡一道，把简拼遍历、整句 DP、
        // 纠错变形生成等所有下游路径的计算量一并约束住。超限返回空候选。
        if input.len() > crate::pinyin::MAX_INPUT_BYTES {
            return Vec::new();
        }
        // 快捷输入拦截：/date /fh 等前缀触发的符号/日期时间候选。
        // 在 process_inner 之前拦截，绕过 is_valid_chinese_text 过滤（符号候选
        // 可能含 ASCII 如日期）。input 形如 "/fh"（宿主前端把 / 送进 buffer）。
        if let Some(quick) = self.try_quick_input(input) {
            return quick;
        }
        // 斜杠命令族。
        if let Some(cmd) = crate::quickcmds::dispatch(input, self.emoji_full) {
            return cmd;
        }
        // 排空上次残留的延长词提示（早退路径不会消费它）。
        let _ = self.tail_extension_hint.borrow_mut().take();
        let mut cands = self.process_inner(input, true);
        // 兜底防御：过滤含 ASCII 字母的候选（汉字候选不可能含字母）。
        // 英文单词候选（is_english）豁免——智能单词识别。
        cands.retain(|c| c.is_english || is_valid_chinese_text(&c.text));
        // 智能单词识别·中文模式：英文单词候选在过滤后定点注入。
        self.inject_english_candidates(input, &mut cands);
        // 尾部半截延长词注入：bare 零声母
        // 尾 + 延长词证据时的最佳延长预测（紫，jisua→计算）。排序定稿后注入：
        // freq ≥ 当前首选 → 置顶（选中全消费走 consumed 规则①）；否则尾部
        // 追加按紫词语义（弱延长证据不抢确定位）。
        if let Some(c) = self.tail_extension_hint.borrow_mut().take() {
            if !cands.iter().any(|x| x.text == c.text) {
                let top_freq = cands.first().map(|x| x.base_freq).unwrap_or(0);
                if c.base_freq >= top_freq {
                    cands.insert(0, c);
                } else {
                    cands.push(c);
                }
            }
        }
        // 单音节拆分识别：yuan→「预案」类两段拆分词
        // 按设置位插入。最后注入——所有正常排序完成后才定点，不受打分影响。
        self.inject_ambig_split_candidate(input, &mut cands);
        // 预编辑显示跟随首选全覆盖绿词的读音切分（详见 display_reading_override
        // 字段注释）：候选全部定稿后记录，syllable_segmentation 对同 input 采用。
        //
        // 延长态：首选是延长预测（候选紧凑拼音以输入
        // 为严格前缀，如 jisua + 计算 ji'suan）时，选举切分显示成 ji'su'a 会让人
        // 误以为「计算」只吃 ji'su、还剩独立音节 a。显示改按候选读音音节序列
        // 切分，末尾音节截断到已敲字母 → ji'sua（a 并入未打完的 suan），与
        // consumed 规则①全消费（选计算吃 6 字母）视觉自洽。
        {
            let mut slot = self.display_reading_override.borrow_mut();
            *slot = None;
            if let Some(top) = cands.first() {
                let compact: String = top
                    .pinyin
                    .chars()
                    .filter(|c| *c != ' ' && *c != '\'')
                    .collect();
                let syls: Vec<String> = top
                    .pinyin
                    .split_whitespace()
                    .map(|s| s.to_string())
                    .collect();
                if syls.len() >= 2 {
                    if top.is_sentence && compact == input {
                        // 既有语义：只对全覆盖绿词覆盖显示——
                        // 白词读音与选举切分同源，不得抢（fangan 上文证据裁决
                        // fan'gan 的显示不能被白词 方案 fang'an 覆盖）。
                        *slot = Some((input.to_string(), syls));
                    } else if Self::is_jianpin_whole_word(input, top) {
                        // 简拼整词（ssef→似是而非）：每字母一字显示 s's'e'f，
                        // 与 consumed 规则④全消费 4 字母自洽（选举切分 s'se'f
                        // 会让人以为只吃 sse）。
                        let per_letter: Vec<String> =
                            input.chars().map(|ch| ch.to_string()).collect();
                        *slot = Some((input.to_string(), per_letter));
                    } else if (top.is_guess || top.is_sentence)
                        && compact.len() > input.len()
                        && compact.starts_with(input)
                    {
                        // 按候选音节累积截断到输入长度（拼音音节纯 ASCII，
                        // 字节切片安全）。已敲字母恰在音节边界结束时不截断。
                        let mut display: Vec<String> = Vec::new();
                        let mut pos = 0usize;
                        for s in &syls {
                            if pos >= input.len() {
                                break;
                            }
                            let take = s.len().min(input.len() - pos);
                            display.push(s[..take].to_string());
                            pos += take;
                        }
                        if !display.is_empty() {
                            *slot = Some((input.to_string(), display));
                        }
                    }
                }
            }
        }
        cands
    }

    /// 从文本反推读音。
    ///
    /// 场景：green_memo 经 input_mappings 恢复的条目无学习时读音（持久表无
    /// 拼音列，服务重启后即 None），提升/召回构造候选时此前只能用当选切分
    /// 冒充 pinyin 字段——「反感得很」配当选读法 «fang an de hen» = 文本与
    /// 拼音自相矛盾体，显示跟随首选时把假读法显示出来（实测场景：服务重启
    /// 后 反感得很 居首、预编辑却拆 fang'an'de'hen）。
    ///
    /// 反推按词库成词证据：枚举输入的全音节切分，找第一个能把 text 逐词
    /// 对齐的读法（每个词必须是词典该音节串下的真实词条；DP 判定可达）。例：
    /// fangandehen + 反感得很 → fang|an 侧「反感」不成词、fan|gan 侧成词
    /// （反感=fán gǎn + 得很=de hen）→ «fan gan de hen»。真歧义时取枚举序
    /// 首个可对齐读法（确定性）；失败返回 None（调用方回退当选切分）。
    pub(crate) fn derive_reading_from_text(&self, input: &str, text: &str) -> Option<String> {
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        if !(2..=12).contains(&n) {
            return None;
        }
        for seg in crate::pinyin::segmentations(input, 32) {
            if seg.len() != n {
                continue; // 全消费：音节数必须等于字数
            }
            let mut reach = vec![false; n + 1];
            reach[0] = true;
            for i in 0..n {
                if !reach[i] {
                    continue;
                }
                for wl in 1..=4.min(n - i) {
                    let word: String = chars[i..i + wl].iter().collect();
                    if self
                        .dict
                        .query(&seg[i..i + wl])
                        .iter()
                        .any(|c| c.text.as_ref() == word.as_str())
                    {
                        reach[i + wl] = true;
                    }
                }
            }
            if reach[n] {
                return Some(seg.join(" "));
            }
        }
        None
    }

    /// 带击键时序的查询入口。
    ///
    /// `ms_since_prev_key` = 本次按键与上一次按键的间隔（毫秒）。
    /// 仅当本次新输入的字符与前一字符相同（刚完成一个相邻同字母对）且
    /// 间隔 < `double_fault_threshold_ms` 时置位 `double_fault`（键盘 bounce
    /// 签名）；输入不再含相邻同字母对时清位。折叠纠错据此门控（见
    /// inject_collapse_corrections）——刻意输入的正常节奏重复不折叠。
    /// 前端未传时序（直接调 process）保持旧行为，零回归。
    ///
    /// 时序判定不叠加 double_fault_correction_enabled
    /// 开关（该开关与前端「键盘防抖」双绑——防抖关（默认）时引擎折叠恒死、
    /// 防抖开时前端已吞键、折叠同样用不上，生产环境两条路全堵死，shenng→生
    /// 从未真机可达）。折叠的用户开关 = 智能纠错总闸 + 键盘误触子开关
    ///（inject_collapse_corrections 门控）；时序检测恒活。前端防抖吞键
    /// 开关与引擎折叠互不影响。
    pub fn process_timed(&self, input: &str, ms_since_prev_key: u32) -> Vec<Candidate> {
        self.timing_aware.set(true);
        let bytes = input.as_bytes();
        let has_double = bytes.len() >= 2 && bytes[bytes.len() - 2] == bytes[bytes.len() - 1];
        if has_double {
            // 刚完成双写：间隔判定（本次击键 = 对的第二个字符）。
            // 阈值可配（double_fault_threshold_ms，偏好设置滑块）。
            self.double_fault
                .set(ms_since_prev_key < self.double_fault_threshold_ms);
        }
        // 输入已不含尾部双写（继续输入/删除）：保留已有标记（前缀编辑后
        // 位置会漂移，用"仍含任意相邻同字母对"做粗校验，无对则清位）
        let still_has_double = bytes.windows(2).any(|w| w[0] == w[1]);
        if !still_has_double {
            self.double_fault.set(false);
        }
        self.process(input)
    }

    /// 快捷输入拦截：/fh 等前缀 → 符号候选。
    ///
    /// input 以 `/` 开头时触发（宿主前端 handle 把 / 及后续字母送进 buffer）。
    /// 去掉 `/` 前缀后查 symbols 模块的符号表。命中返回带 is_shortcut=true 的候选
    /// 列表（蓝色显示）；未命中返回 None（走正常拼音路径）。
    ///
    /// 注：日期时间（/date /time /week）由宿主前端用系统日期组件生成
    /// （天然本地时区），不走此 Rust 拦截。宿主前端在 handle 里识别这些前缀后
    /// 直接构造候选，不调 engine process。
    ///
    /// 注意：空 key（只有 `/`）返回 None，由宿主前端处理裸 / 的上屏。
    fn try_quick_input(&self, input: &str) -> Option<Vec<Candidate>> {
        let key = input.strip_prefix('/')?;
        if key.is_empty() {
            return None;
        }
        // 符号查表
        if let Some(syms) = crate::symbols::lookup_symbols(key) {
            return Some(
                syms.iter()
                    .map(|&s| Candidate {
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
                    })
                    .collect(),
            );
        }
        None
    }

    // 注：collect_candidates 方法不存在（无两阶段 FFI/语义 embedding 通道）。

    /// 检测输入是否为「双字母声母 + 其他首字母」的混合简拼。
    ///
    /// 仅匹配 zh/sh/ch 开头 + 至少 2 个其他字母的输入（如 zhgrm、shgrm、chgrm）。
    /// 这类输入会被 is_jianpin 误判为纯简拼，需要走 token 路径识别 Shengmu 双试。
    /// 不匹配：纯简拼（zgrm，非 zh/sh/ch 开头）、全拼（zhongguo）、混合全拼尾（zhonggr）。
    fn is_hybrid_shengmu_input(input: &str) -> bool {
        // ASCII 守卫：&input[..2] 字节切片遇多字节
        // 首字符（非 char boundary）直接 panic——测试实测中文输入即崩。非
        // ASCII 输入不可能是混合声母拼音，短路返回。
        if !input.is_ascii() {
            return false;
        }
        let input = input.trim_matches('\'');
        // 必须是 zh/sh/ch 开头，且后续至少 2 个字母
        let prefix = if input.len() >= 2 {
            &input[..2]
        } else {
            return false;
        };
        if prefix != "zh" && prefix != "sh" && prefix != "ch" {
            return false;
        }
        if input.len() < 4 {
            return false;
        }
        let rest = &input[2..];
        if !rest.chars().all(|c| c.is_ascii_lowercase()) {
            return false;
        }
        // 关键判据：zh/sh/ch 是完成
        // 声母（混合简拼）⟺ 不存在以该双字母开头的有效音节使整串「以该音节开头」
        // （zhongguo→zhong）或「是该音节前缀」（zho→zhong 中间态）。旧判据「后续
        // 不得含元音 a/e/i/o/u」把 a/e/o 首字母（西安=xa、安=a）的合法简拼整体
        // 误拒：zhgrmjfjxa 掉进纯简拼路径 → DP 按逐字母拆读（z=走 h=回）→「走回国
        // 人民解放军西安」垃圾压过正解、正确词整条消失。新判据：zhgrmjfjxa 无任何
        // zh 音节匹配开头 → 混合路径 → DP 按 [zh,g,r,m,j,f,j,x,a] 产出 中国人民解
        // 放军西安；zhonggr(m) 由 "zhong" 前缀匹配放行全拼路径（行为不变）。
        !crate::pinyin::is_syllable_initial_prefix(&input[..2], input)
    }

    /// 「首字母串 + 全拼尾」长混合判定：
    /// 用户简拼打入一长串首字母后切换全拼（zgrmjfj + nan → 中国人民解放军南）。
    ///
    /// 判据（三者同真）：
    /// ① 非全拼前缀态（is_pinyin_prefix——zho/zhonggr 类由全拼路径处理，不揽；
    /// zh/sh/ch 开头由 is_hybrid_shengmu_input 先行处理，不重叠）；
    /// ② 首个完整音节前的连续单字母段 > 4（`word_candidates_for_syllables` 的
    /// MAX_INITIAL_SEG_LEN 上限之上——hybrid 词分支与全拼拍扁回退均不覆盖，
    /// 正是「塌成单字」的失效区）；
    /// ③ 该段的每个字母都是合法声母单字母（真简拼的形态证据）。
    /// 命中后按 token 混合路径处理（query_tokens + 绿词 DP，见
    /// [`Self::process_hybrid_shengmu_inner`]）。
    fn is_initial_full_tail_mix_input(input: &str) -> bool {
        if !input.is_ascii() {
            return false;
        }
        let chars: Vec<char> = input.chars().filter(|c| *c != '\'').collect();
        if chars.len() < 6 {
            return false;
        }
        if !chars.iter().all(|c| c.is_ascii_lowercase()) {
            return false;
        }
        // ① 非全拼前缀态
        if crate::pinyin::is_pinyin_prefix(&chars) {
            return false;
        }
        // ②③ 沿前导声母段扫描，找首个完整音节位；段长超 4 才算命中
        const INITIALS: &[char] = &[
            'b', 'p', 'm', 'f', 'd', 't', 'n', 'l', 'g', 'k', 'h', 'j', 'q', 'x', 'r', 'z', 'c',
            's', 'y', 'w',
        ];
        let mut pos = 0usize;
        while pos < chars.len() && INITIALS.contains(&chars[pos]) {
            let max_k = 6.min(chars.len() - pos);
            let has_syllable_here = (2..=max_k).any(|k| {
                let s: String = chars[pos..pos + k].iter().collect();
                crate::pinyin::is_valid_syllable_pub(&s)
            });
            if has_syllable_here {
                return pos > 4;
            }
            pos += 1;
        }
        false
    }

    /// 混合声母简拼处理：token 切分（Shengmu 双试 + 全首字母退化）→ query_tokens → 排序。
    /// `correct` 语义同 process_inner：仅顶层查询补纠错（递归子查询传 false 防循环）。
    fn process_hybrid_shengmu_inner(&self, input: &str, correct: bool) -> Vec<Candidate> {
        let segs = crate::pinyin::segmentations_tokens(input, 16);
        if segs.is_empty() {
            return Vec::new();
        }

        // 对每个切分方案直接调 query_tokens（绕过 word_candidates_for_syllables，
        // 因为它的 duan 保护逻辑依赖 syllables 是 input 的切片，Token 转换会破坏指针）。
        let mut all_candidates: Vec<Candidate> = Vec::new();

        // 绿词 DP 候选段：优先含 Shengmu 的切分（zh/sh/ch 混合的既有行为）；
        // 无 Shengmu 时退「首字母串 + 全拼尾」混合切分（≥4 token 且 Initial
        // 与 Full 并存——zgrmjfjnan 塌陷场景：简拼打入一长串
        // 后切全拼，词长超任何单词，只有 DP 能拼出整句「中国人民解放军南」；
        // 纯 Initial 段由简拼路径负责，不来这里）。
        let dp_seg = segs
            .iter()
            .find(|s| s.iter().any(|t| matches!(t, Token::Shengmu(_))))
            .or_else(|| {
                segs.iter().find(|s| {
                    s.len() >= 4
                        && s.iter().any(|t| matches!(t, Token::Initial(_)))
                        && s.iter().any(|t| matches!(t, Token::Full(_)))
                })
            });

        // 完全匹配判据：候选字数命中任一切分方案
        // 的 token 数即算完全覆盖输入。zhmz 有两种等价覆盖——声母切分 [zh,m,z]
        // （这么着，3 token）与全首字母切分 [z,h,m,z]（中华民族，4 token）——旧判据
        // 只认前者（字数 == shengmu_token_count），后者被单字补充/二字前缀词压到
        // 第 95 位。两种覆盖同属第 1 层，层内由 candidate_score（freq + 上下文 +
        // 用户历史）裁决歧义，不在路由层抢先焊死。
        let full_match_counts: std::collections::BTreeSet<usize> =
            segs.iter().map(|s| s.len()).collect();

        for seg in &segs {
            let cands = self.dict.query_tokens(seg);
            all_candidates.extend(cands);
        }

        // 混合简拼绿词 DP：对 dp_seg（含 Shengmu 或
        // Initial+Full 长混合，token ≥4），调 decode_sentence(is_initial_mode=true)
        // 组合绿词——token 串喂 DP 后，query_seg 按 token 语义查询（单字符位
        // 走首字母子树、多字符位走精确音节），「首字母串 + 全拼尾」得以拼出。
        if let Some(dp_seg) = dp_seg {
            if dp_seg.len() >= 4 {
                let syl_strs: Vec<String> = dp_seg
                    .iter()
                    .map(|t| match t {
                        Token::Full(s) | Token::Shengmu(s) => s.to_string(),
                        Token::Initial(c) => c.to_string(),
                    })
                    .collect();
                let syl_refs: Vec<&str> = syl_strs.iter().map(|s| s.as_str()).collect();
                let green = self.decode_sentence_collect(input, &syl_refs, true, false);
                for g in green {
                    if !all_candidates.iter().any(|c| c.text == g.text) {
                        all_candidates.push(g);
                    }
                }
            }
        }

        all_candidates.sort_by_key(|x| std::cmp::Reverse(x.base_freq));
        all_candidates.dedup_by(|a, b| a.text == b.text);

        // 首音节单字补充（供逐字自造词）。zhgrm 首字母是 z，补充 z 开头单字。
        // 必须在统一排序（candidate_score）之前补充，否则补充的单字会在排序后
        // 追加到末尾，破坏上下文加权排序（机→zhi 场景：补充的「制」
        // 因 collocation(机制) 应排前，但追加在末尾导致看不到）。
        let existing: std::collections::BTreeSet<String> = all_candidates
            .iter()
            .map(|c| c.text.as_ref().to_owned())
            .collect();
        if let Some(first_char) = input.chars().next() {
            for single in self.query_single_char_by_initial_fuzzy(first_char) {
                if !existing.contains(single.text.as_ref()) {
                    all_candidates.push(single);
                }
            }
        }

        // 统一排序：完全匹配优先（字数命中任一切分方案的 token 数），再按
        // candidate_score（含上下文 bigram/collocation 加权）。
        all_candidates.sort_by(|a, b| {
            // 完全匹配优先（字数命中任一切分方案的 token 数）
            let a_full = full_match_counts.contains(&a.text.chars().count());
            let b_full = full_match_counts.contains(&b.text.chars().count());
            match (a_full, b_full) {
                (true, false) => return std::cmp::Ordering::Less,
                (false, true) => return std::cmp::Ordering::Greater,
                _ => {}
            }
            let sa = self.candidate_score(input, a);
            let sb = self.candidate_score(input, b);
            sb.cmp(&sa)
                .then_with(|| a.pinyin.cmp(&b.pinyin).then_with(|| a.text.cmp(&b.text)))
        });

        // 智能纠错回退：zh/sh/ch 开头的拼错（zhognwen→中文）
        // 被混合声母分流劫持时永远走不到全拼路径的 maybe_correct——此处补同一入口。
        // 合法混合输入（zhgrm/zgrmjfjnan 类）的所有单点变形都不构成全有效音节
        // 切分，自然零产出、零打扰；结构非法错输才实际出橙词。
        if correct {
            self.maybe_correct(input, &mut all_candidates);
        }

        all_candidates
    }

    /// 双拼直查：把权威音节序列喂给现有查询路径，跳过全拼 DP 枚举。
    ///
    /// 完整音节序列 → candidates_for_syllables；末尾不完整声母 → query_tokens
    /// （Token::Full + Token::Initial 混合，dict 原生支持）。绿词 DP 复用 decode_sentence。
    /// 注意：音节经 normalize_syllable 归一化（ing→ying 等）后再喂查询。
    fn process_shuangpin_inner(&self, input: &str, syllables: &[String]) -> Vec<Candidate> {
        use crate::pinyin::Token;

        // 归一化音节（ing→ying 等），转 &str 切片
        let normalized: Vec<String> = syllables
            .iter()
            .map(|s| crate::pinyin::normalize_syllable(s).to_string())
            .collect();
        let syl_refs: Vec<&str> = normalized.iter().map(|s| s.as_str()).collect();

        // 末尾悬尾声母判定：
        // - 单字母非有效音节（vsg 的 g、faxmyigew 的 w）；
        // - 双字声母 zh/ch/sh（v/i/u 键尾，如 nihcv→[ni,hao,zh]、faxmyigev 的 v）。
        // 两者语义同为「一键 = 下一个音节的声母」——双拼里 v 是「zh」这一个声母，
        // 不是 z+h 两个简拼首字母（全拼 nihaozh 可拆 z+h 指「你好之后」，那是全拼
        // 两字母输入的另一套语义，本函数不涉及）。
        let last_is_partial = normalized.last().is_some_and(|s| {
            (s.chars().count() == 1 && !crate::pinyin::is_valid_syllable_pub(s))
                || matches!(s.as_str(), "zh" | "ch" | "sh")
        });

        // 单声母退化守卫：整个 buffer 只解出 1 个不完整声母
        // （双拼单键，如小鹤 "d"）时，不得走下面的 query_tokens 混合分支——
        // 纯 Initial token 序列触发 dict 层的子树洪泛收集（2000 上限 + Trie 存储
        // 序遍历），de 节点（的/得/德）永远轮不到访问 →「的」完全消失，且幸存
        // 候选按音节存储序而非频次序排列。改走 candidates_for_syllables——与
        // 「v」→zh（2 字符声母，原路径就正确）及全拼用户输入 "d" 完全同一条
        // 查询路径，天然获得频次序单字 + 前缀联想。
        let mut cands = if last_is_partial && normalized.len() >= 2 {
            // 多音节末尾不完整：前 N-1 精确 + 末音节声母（单字母 Initial /
            // zh.ch.sh Shengmu——zh 尾必须走 Shengmu，Initial('z') 会放过 z 系
            // 音节，语义错位）。
            let tokens: Vec<Token> = normalized
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    if i == normalized.len() - 1 {
                        match s.as_str() {
                            "zh" | "ch" | "sh" => Token::Shengmu(s.as_str()),
                            _ => Token::Initial(s.chars().next().unwrap()),
                        }
                    } else {
                        Token::Full(s.as_str())
                    }
                })
                .collect();
            self.dict.query_tokens(&tokens)
        } else {
            self.candidates_for_syllables(input, &syl_refs)
        };

        // 绿词 DP（解码整句组合候选）—— 复用现有 decode_sentence
        if !last_is_partial {
            let green = self.decode_sentence(input, &syl_refs, false, false);
            for g in green {
                if !cands.iter().any(|c| c.text == g.text) {
                    cands.push(g);
                }
            }
        } else {
            // 悬尾整句：末音节是悬尾声母
            // （单字母 或 zh/ch/sh，见上方判定）时，旧版整句 DP 整体跳过 →
            // 悬尾输入永远没有全消费整句候选（faxmyigew 首位停在部分覆盖的
            // 「发现」，与全拼 faxianyigew 同族）。
            // 现解码 valid 前缀（has_partial_tail=true 免整词短路， 同款），
            // 尾首字母补字（best_char_for_initial，与全拼补字同一函数；该函数
            // 对 zh 尾按完整前缀过滤，只补 zh 系字），完成句 push 进候选池后由
            // 统一排序定位置——全消费句经 is_pinyin_full_match 算 tier 0
            //（"字数==音节数"含悬尾场景恒真），天然排在部分覆盖词（tier 1）
            // 之前，与全拼 的覆盖序语义一致。
            let valid = &normalized[..normalized.len() - 1];
            if valid.len() >= 3
                && valid
                    .iter()
                    .all(|s| crate::pinyin::is_valid_syllable_pub(s))
            {
                let valid_refs: Vec<&str> = valid.iter().map(|s| s.as_str()).collect();
                let tail = normalized.last().map(|s| s.as_str()).unwrap_or("");
                let mut sentences = self.decode_sentence(input, &valid_refs, false, true);
                for sentence in &mut sentences {
                    // 尾补字：与全拼 process_inner 的 has_partial_tail 补字同构
                    let prev_char = sentence.text.chars().last().map(|c| c.to_string());
                    if let Some(ch) = self.best_char_for_initial(tail, prev_char.as_deref()) {
                        let mut text = (*sentence.text).to_owned();
                        text.push_str(&ch);
                        sentence.text = Arc::from(text);
                        let mut pinyin = (*sentence.pinyin).to_owned();
                        pinyin.push(' ');
                        pinyin.push_str(tail);
                        sentence.pinyin = Arc::from(pinyin);
                    }
                }
                for g in sentences {
                    if !cands.iter().any(|c| c.text == g.text) {
                        cands.push(g);
                    }
                }
            }
        }

        // 统一排序 + 去重（与全拼 process_inner 一致）
        // candidates_for_syllables 内部 extend(query_single_chars_fuzzy) 不去重（设计如此，
        // 依赖调用方去重），单音节双拼输入（如 vi→zhi）会因此产生重复单字候选。
        //
        // 分层键预计算排序：裸 candidate_score
        // 排序缺 tier（词/字分层）、collocation 第 3 层 + 强 bigram 第 3.5 层
        // （核心：记→yi 出忆 退化）、primary_reading_bonus（主音优先失效）、
        // 字数分层。改为复用 candidate_sort_key（与全拼同一函数，test_candidate_sort_key_matches_cmp
        // 守护其正确性），tier 覆盖原 full_match_count 语义（词 full-match 恒在单字前）。
        // best_score=0（双拼无切分打分，0 让单字音节位置排序生效）。
        let is_multi_syllable = normalized.len() >= 2;
        let single_syllable_input = normalized.first().map(|s| s.as_str());
        // 纯简拼让位判据：只与 input 有关，排序前算一次
        let input_first_syl_complete = Self::input_starts_with_complete_syllable(input);
        let mut keyed: Vec<(scoring::CandidateSortKey, Candidate)> = std::mem::take(&mut cands)
            .into_iter()
            .map(|c| {
                let key = self.candidate_sort_key(
                    input,
                    &c,
                    is_multi_syllable,
                    single_syllable_input,
                    0,
                    &syl_refs,
                    input_first_syl_complete,
                );
                (key, c)
            })
            .collect();
        // 键非 Copy（CandidateSortKey）→ 保留 sort_by（同 dict.rs 同类注）
        #[allow(clippy::unnecessary_sort_by)]
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        cands = keyed.into_iter().map(|(_, c)| c).collect();
        cands.dedup_by(|a, b| a.text == b.text);
        // 候选数量保险丝：与全拼 MAX_FINAL_CANDIDATES
        // 一致——模糊音全开 + 长输入时候选膨胀，防排序/内存/FFI 传输膨胀。
        // 放在 green_memo lift 之前：lift 的「记忆词优先」保留，截断只砍尾部。
        cands.truncate(Self::MAX_FINAL_CANDIDATES);

        // green_memo 候选优先提升：
        // 与全拼路径 process_inner 的 lift 块等价。
        // decode_sentence 内部对双拼原文 input 的 green_memo 查询恒失效
        // （is_full_segment = seg.concat==input，"zhongguo" != "vsgo"），
        // 且双拼路径此前没有 lift 块——（首次即记忆 + 优先提升）
        // 在双拼模式完全失效，commit 写入的 "vsgo" key 是死数据。
        // 这里在排序后直接查 green_memo（key=双拼原文，与 learning.rs 的
        // green_memo 写入 key 对齐），把命中候选提到最前。三重守卫与全拼 lift 一致
        // （freq>=1500 + 字数==音节数 + user_counts>=1）。放在排序之后：
        // 提升/插入到首位后不再重排，语义 = 「记忆过的词优先」。
        if let Some(entries) = self.green_memo.get(input) {
            let full_char_count = normalized.len();
            let mut sorted: Vec<&(String, u32, Option<String>)> = entries
                .iter()
                .filter(|(_, f, _)| *f >= 1500) // 首次组装 user_freq=2000，门槛 1500 让它通过
                .filter(|(t, _, _)| t.chars().count() == full_char_count)
                .filter(|(t, _, _)| self.freq.counts.get(t).copied().unwrap_or(0) >= 1)
                .collect();
            sorted.sort_by_key(|x| std::cmp::Reverse(x.1));
            for (gt, gf, gp) in sorted.into_iter().rev() {
                if let Some(pos) = cands.iter().position(|c| c.text.as_ref() == gt.as_str()) {
                    let item = cands.remove(pos);
                    cands.insert(0, item);
                } else {
                    cands.insert(
                        0,
                        Candidate {
                            text: gt.as_str().into(),
                            base_freq: *gf,
                            // 学习时真实读音 > 文本反推 > 当选切分
                            pinyin: gp
                                .clone()
                                .or_else(|| self.derive_reading_from_text(input, gt))
                                .unwrap_or_else(|| syl_refs.join(" "))
                                .into(),
                            is_primary: true,
                            is_correction: false,
                            is_sentence: true,
                            is_guess: false,
                            is_shortcut: false,
                            is_english: false,
                            sentence_score: 0,
                        },
                    );
                }
            }
        }

        // 固定候选槽位插入
        self.apply_pins(input, &mut cands);

        cands
    }

    /// process 的内部实现，`correct` 控制是否触发智能纠错回退。
    ///
    /// 公开 `process` 传 true；纠错递归查询变形时传 false，避免无限递归。
    fn process_inner(&self, input: &str, correct: bool) -> Vec<Candidate> {
        if input.is_empty() {
            return Vec::new();
        }
        // 收集开启时清上一次残留下的路径（本键若无简拼/混合声母路由，
        // take_sentence_paths 应得空表而非陈旧键数据）。
        if self.collect_sentence_paths.get() {
            self.last_sentence_paths.borrow_mut().clear();
        }
        // candidate_score 缓存生命周期 = 单次查询：打分依赖 input/counts/recent/context，
        // 跨查询必须清空（同 text 在不同 input 下 mapping_bonus 等分量不同）。
        self.score_cache.borrow_mut().clear();
        self.window_freq_cache.borrow_mut().clear();

        // 双拼分支：scheme=Some 且未强制全拼时尝试解码双拼 buffer → 权威
        // 音节序列，命中则直查 Trie，跳过全拼 DP 枚举。
        // force_fullpinyin=true（/q 临时全拼）时跳过双拼分支，走全拼路径。
        // shuangpin=None（全拼用户）+ force_fullpinyin=false（默认）→ 行为与原来完全一致。
        if !self.force_fullpinyin {
            if let Some(scheme) = &self.shuangpin {
                if let Some(syllables) = crate::shuangpin::decode_buffer(scheme, input) {
                    return self.process_shuangpin_inner(input, &syllables);
                }
            }
        }

        // 混合简拼分流：含 zh/sh/ch 声母 + 其他首字母的输入
        // （如 zhgrm、shgrm、chgrm）。这类输入被 is_jianpin 误判为纯简拼，走
        // process_jianpin_inner 把 zh 当 z、h 两个独立首字母，查不到 zhongguo 路径。
        // 改为走 token 统一路径，识别 Shengmu(zh/sh/ch) + Initial 双试切分。
        if Self::is_hybrid_shengmu_input(input) {
            return self.process_hybrid_shengmu_inner(input, correct);
        }

        // 「首字母串 + 全拼尾」长混合分流（「打进南京时
        // 塌成单字」）：简拼长串后切全拼（zgrmjfj + nan）。旧路径下 hybrid
        // 词分支（MAX_INITIAL_SEG_LEN=4 上限）与全拼拍扁回退都不覆盖 →
        // 整条塌成单字；改走 token 混合路径（query_tokens + 绿词 DP）。
        if Self::is_initial_full_tail_mix_input(input) {
            return self.process_hybrid_shengmu_inner(input, correct);
        }

        // 判断是否为简拼模式
        let is_jp = crate::pinyin::is_jianpin(input);

        if is_jp {
            return self.process_jianpin_inner(input, correct);
        }

        // 全拼：合并两种切分方案：
        // - segmentations: 只产生全有效音节切分（如 san+ge+ren）
        // - segmentations_with_partial: 允许不完整音节的切分（如 b+y+di）
        // 合并去重后打分，确保短音节切分（san）和长音节切分（sang）都能参与。
        // 另加「完整音节前缀+悬尾」保底变体（绿词消失修复，见
        // pinyin::segmentations_prefix_tail）：含尾输入的正解切分会被 DFS 名额
        // 挤出枚举（dang…zhinengd 实测 ren|gong 从未入局），不入局则整句 DP
        // 收到垃圾切分、绿词整体消失。
        let mut segs = crate::pinyin::segmentations(input, 16);
        let partial_segs = crate::pinyin::segmentations_with_partial(input, 16);
        for seg in partial_segs {
            if !segs.iter().any(|s| s == &seg) {
                segs.push(seg);
            }
        }
        for seg in crate::pinyin::segmentations_prefix_tail(input) {
            if !segs.iter().any(|s| s == &seg) {
                segs.push(seg);
            }
        }

        let best_syllables: Vec<&str>;
        let mut all_candidates: Vec<Candidate>;
        let mut best_score: u32;
        // 备选全完整音节切分：切分选举
        // 的启发式打分只是提案，绿词 DP 的词证据才是裁决——备选清单让
        // 次优切分也有机会贡献句子候选（详见下方整句解码段的仲裁注释）。
        // 元素 = (切分, 选举分)：选举分供橙词通道的真平局判定（分差 ≤50 才出示）。
        let mut alt_full_segs: Vec<(Vec<&str>, u32)> = Vec::new();
        // 带拆分惩罚切分的营救清单：
        // 仅在当选切分零产出时启用，门槛 = 全有效音节 + ≥2 个真实词跨度。
        let mut rescue_segs: Vec<(Vec<&str>, u32)> = Vec::new();
        // 拆分前缀收割词的文本清单（排序末期「让位更长延长预测」用，见该块）
        let mut split_prefix_texts: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        if segs.is_empty() {
            // segmentations_with_partial 理论上不会返回空（至少能逐字符切分），
            // 但作为防御性编程，回退到 parse_pinyin。
            let seq = parse_pinyin(input);
            let syllables: Vec<&str> = seq
                .syllables
                .iter()
                .map(|s| &input[s.start..s.end])
                .collect();
            if syllables.is_empty() {
                return Vec::new();
            }
            best_syllables = syllables;
            best_score = 0;
            all_candidates = self.candidates_for_syllables(input, &best_syllables);
        } else {
            // 先给每种切分打分
            let mut scored: Vec<(Vec<&str>, u32)> = Vec::new();
            for seg in segs {
                let score = self.score_for_syllables(input, &seg);
                scored.push((seg, score));
            }

            best_score = scored.iter().map(|(_, s)| *s).max().unwrap_or(0);

            // 自适应扩枚举（yigerenguodehaob / zh 尾修复，与
            // scoring.rs pick_segmentation 同因）：DFS 先序名额被退化子树占满时
            // 正解在 cap 之外——全 0 分即扩枚举重试，出正分即停（正常输入零开销；
            // 大 cap 只对短输入启用防容积膨胀）。
            //
            // 两段式：先 TOKENS 语义——
            // 单字符位不产 Full 分支（SEG_TOKENS 单字符一律 Initial），枚举树
            // 比 PARTIAL 语义小一个量级：实测 `yigerenguodehaobuhaozh` 的
            // reng 贪长子树 PARTIAL 需 2048 条才露出正解（512 不够——上一版
            // 修 b 尾时 512 恰好够、zh 尾更长就爆），TOKENS 仅 128 条即达
            // （显示链 best_token_segmentation 同语义，这正是它一直显示正确
            // 而候选链塌的原因）。TOKENS 与 PARTIAL 的 &str 投影对打分等价，
            // 候选生成只看投影序列。PARTIAL [512, 2048] 保留为第二道兜底
            //（TOKENS 也全 0 的极端退化态）。
            if best_score == 0 && input.len() <= 64 {
                // ① TOKENS 语义（与显示链同源；借 best_token_segmentation
                // 内部的自适应 caps [8,128,512]）
                let tok = self.best_token_segmentation(input);
                if !tok.is_empty() && !scored.iter().any(|(s, _)| s == &tok) {
                    let score = self.score_for_syllables(input, &tok);
                    scored.push((tok, score));
                }
                best_score = scored.iter().map(|(_, s)| *s).max().unwrap_or(0);
                // ② PARTIAL 语义大 cap（TOKENS 仍全 0 才试）
                if best_score == 0 {
                    for cap in [512usize, 2048] {
                        let mut extra = crate::pinyin::segmentations(input, cap);
                        extra.extend(crate::pinyin::segmentations_with_partial(input, cap));
                        for seg in extra {
                            if scored.iter().any(|(s, _)| s == &seg) {
                                continue;
                            }
                            let score = self.score_for_syllables(input, &seg);
                            scored.push((seg, score));
                        }
                        best_score = scored.iter().map(|(_, s)| *s).max().unwrap_or(0);
                        if best_score > 0 {
                            break;
                        }
                    }
                }
            }

            // 选最优切分：seg_order_cmp 与显示/消费链（pick_segmentation）同源
            // 比较器——分数高优先；同分全有效 → 首分歧窗口词频 → 音节数少 →
            // 首音节短。两条链必须裁决一致，否则显示/消费与候选各选各的切分
            //。
            let best_idx = scored
                .iter()
                .enumerate()
                .max_by(|(_, (a, sa)), (_, (b, sb))| self.seg_order_cmp(input, a, *sa, b, *sb))
                .map(|(idx, _)| idx)
                .unwrap_or(0);
            best_syllables = scored[best_idx].0.clone();

            // 备选全完整音节切分（组句仲裁提案清单）：按同源比较器降序，
            // 剔除当选，只留「每个音节都是 len≥2 的完整有效音节」的另一种
            // 全拼读法，且音节数 ≥3（<3 走不到整句 DP 门槛，白跑）。
            // 碎片切分（含简拼首字母位）不入选——中置简拼由当选切分的
            // initial 模式 DP 承载，备选只竞争「全拼读法」。
            //
            // 参赛资格 = 零拆分惩罚（层级 ≥2，分数 ≥1000 eval
            // 11 例翻转回归教训）：带罚切分（ji+an 拆 jian、ku+ai 拆 kuai）
            // 的 DP 句子靠生僻词（几案/苦艾/速按）的词完整性加分系统性压过
            // 「词+尾单字」的正确读法（见/算/换 单字结构性吃亏）——DP 分数
            // 是同一音节格内的排序口径，跨切分比较只对「音节级语法同样
            // 合法」（无罚）的切分公平。jian→ji+an 类读法在切分打分里已
            // 被惩罚体系判为「用户几乎不可能是这个意思」，不配进入仲裁。
            {
                let mut order: Vec<usize> = (0..scored.len()).collect();
                order.sort_by(|&a, &b| {
                    self.seg_order_cmp(input, &scored[b].0, scored[b].1, &scored[a].0, scored[a].1)
                });
                for ai in order {
                    if ai == best_idx {
                        continue;
                    }
                    let (seg, seg_score) = &scored[ai];
                    if seg.len() < 3
                        || !seg
                            .iter()
                            .all(|s| s.len() >= 2 && crate::pinyin::is_valid_syllable_pub(s))
                    {
                        continue;
                    }
                    if *seg_score >= 1000 {
                        if alt_full_segs.len() < 3 {
                            alt_full_segs.push((seg.clone(), *seg_score));
                        }
                    } else if *seg_score > 0 && rescue_segs.len() < 3 {
                        // 拆分惩罚切分的营救清单（场景：
                        // baomianquan→保密安全 不可达）：上面 >=1000 门槛刻意把
                        // 带拆分惩罚的切分挡在常规仲裁之外（duan 保护——ji+an 拆
                        // jian 的读法不配重排）。但当选切分零产出时
                        // （baomianquan 的 [bao,mian,quan] 查不到任何词、DP 全空），
                        // 被挡掉的惩罚切分 [bao,mi,an,quan]（保密 1434 + 安全
                        // 5668）才是唯一有真实词证据的读法——"不配参与仲裁"的
                        // 前提（存在可比较的当选句）不成立。单列清单，只在当选
                        // DP 空时使用；词跨度质量门与 DP 都在营救块内才付费
                        //（常规路径只付本段廉价扫描，不付查询开销）。
                        rescue_segs.push((seg.clone(), *seg_score));
                    }
                }
            }

            // Top-K 切分候选合并。
            // 之前只用单一最优切分，其他合理切分的候选丢失（如 xiana 最优切分 [xia,na]
            // 找不到「西安」，但 [xi,an,a] 切分能找到）。改为对 Top-K 切分都生成候选，
            // 合并去重。K=3 控制性能。
            //
            // 防污染：只合并词级候选（字数≥2），单字候选只用最优切分的（避免后续
            // 音节高频字污染首屏，如 geiwo 的「我」）。这样既补全候选，又不破坏
            // 原有的单字排序保护。
            all_candidates = self.candidates_for_syllables(input, &best_syllables);
            // 注： Top-K 全切分合并尝试后回退——对所有非最优切分补充候选会引入
            // 大量低质量词级候选，破坏 candidate_cmp 排序（长句首候选从 90% 降到 67%）。
            // 改为只在 word_candidates_for_syllables 的 if/else 分支内精准补查 complete
            // 精确匹配（如 [xi,an,a] 的 complete=[xi,an]→西安），不合并其他切分候选。

            // 纯首字母简拼整词读法补充。
            //
            // is_jianpin 是互斥分流（任何连续两字母构成有效音节即判非简拼）：
            // ssef 因 "se" 是音节被判全拼/混合 → 只试 s|se|f 读法，拼出绿词
            // 「渗色服」，词库真词「似是而非」（si shi er fei，freq 459）全无机会。
            // 这里把「每字母一字」的简拼整词读法作为竞争读法并入（词库白词，
            // 非绿/橙），由既有排序（同长白词压制拼凑绿词、白词内部按词频）裁决。
            // 强约束：候选字数 == 输入字母数（4 字母只认 4 字词），且主列表去重、
            // 输入纯小写无撇号、长度 3..=8——噪音面极小（>=4 字母的等字长整词
            // 基本只有成语/固定短语，正是简拼整词的用户意图）。
            if input.len() >= 3
                && input.len() <= 8
                && input.chars().all(|c| c.is_ascii_lowercase())
            {
                let tokens: Vec<crate::pinyin::Token> = input
                    .chars()
                    .map(crate::pinyin::Token::Initial)
                    .collect();
                let mut jp: Vec<Candidate> = self
                    .dict
                    .query_tokens(&tokens)
                    .into_iter()
                    .filter(|c| {
                        c.text.chars().count() == input.len()
                            && !self.removed_texts.contains(c.text.as_ref())
                            && !all_candidates.iter().any(|e| e.text == c.text)
                    })
                    .collect();
                jp.sort_by_key(|c| std::cmp::Reverse(c.base_freq));
                jp.truncate(3);
                all_candidates.extend(jp);
            }

            // 橙词补充：
            //
            // 场景：输入有多种全有效切分（如 aoliao → ao|li|ao 和 ao|liao）。即使修了
            // 拆分惩罚让最优切分能命中词，仍可能有极端情况最优切分选错。作为保险，
            // 扫描其他切分能查到的词级候选（字数≥2），主候选列表里没有的，作为橙词
            // （is_correction=true）补充到末尾，让用户至少能翻页找到。
            //
            // 严格约束（避免回退教训）：
            // 1. 只补词级候选（字数≥2），不补单字（保护单字排序）
            // 2. 只补主候选列表里没有的（去重）
            // 3. 只补全有效音节切分（跳过含不完整音节的低质量切分）
            // 4. 数量上限 MAX_ALT_SEG_WORDS=3（避免大量橙词涌入）
            // 5. 标记 is_correction=true，排序时自然排在正常候选后面
            // 6. 只在多音节输入（最优切分≥2音节）时触发，单音节输入（如 duan）
            // 不补充（du|an 拆出的「都安」不应出现，保护完整音节 duan 不被拆）
            const MAX_ALT_SEG_WORDS: usize = 3;
            let mut alt_words: Vec<Candidate> = Vec::new();
            if best_syllables.len() >= 2 {
                let existing_texts: std::collections::HashSet<&str> =
                    all_candidates.iter().map(|c| c.text.as_ref()).collect();
                for (idx, (seg, _score)) in scored.iter().enumerate() {
                    if idx == best_idx {
                        continue; // 跳过最优切分（已生成主候选）
                    }
                    // 只看全有效音节切分（跳过含不完整音节的）
                    if !seg.iter().all(|s| crate::pinyin::is_valid_syllable_pub(s)) {
                        continue;
                    }
                    let cands = self.dict.query(seg);

                    // 备选切分的前缀词收割（系统化泛化：
                    // baomianzhao→保密 不可达 + baomian 保密误标橙）。
                    //
                    // 背景（xiana→西安 起源）：全有效切分 [xi,an,a] 里，「西安」挂在
                    // xi→an 节点上，dict.query 整段查不到；连续前缀词循环里 [xi,an]
                    // 拼起来等于完整音节 "xian"，被 duan 保护跳过——词级候选需要
                    // 显式收割。
                    //
                    // 泛化路径：xiana→西安（零声母尾 a）→ bendana→笨蛋（
                    // 头部序列泛化）→ 任意尾 + 白词化。原「尾部必须是零
                    // 声母音节」的门槛使可达性依赖尾字母形状——baomian 的尾 an
                    // 恰好零声母所以 保密 可达（还被误标橙），baomianzhao 的尾
                    // zhao 不是零声母所以 保密 整个消失。系统类缺陷：带拆分惩罚
                    // 切分里的真实词证据（保密=bao+mi）只有在尾部碰巧零声母时
                    // 才可达。
                    // 词证据（保密=bao+mi）只有在尾部碰巧零声母时才可达。
                    //
                    // 语义修正：这类「词的前缀读法 + 剩余字母」是
                    // 正常的逐字输入中间态（等价于打完 baomi 选 保密 再继续），
                    // 是白词不是橙词——橙 = 纠错/完整读法歧义，前缀词 = 合法
                    // 输入状态。白词化后：排序按词频正常参与（无橙标）、预上屏
                    // 显示走前缀规则保留尾部剩余字母（bao'mi'an）、选中走正常
                    // 提交学习（input_mapping 记录该前缀习惯）。
                    //
                    // 严格约束（避免回退教训）：
                    // 1. 只对带拆分惩罚（score<1000）的备选切分收割——无罚
                    // 备选的整词读法由既有整段橙词通道承载，不重复不扩散；
                    // 2. 头部前缀从长到短尝试（≤3 次查询），只收词级候选（字数≥2）；
                    // 3. 走既有去重 + MAX_ALT_SEG_WORDS 总量上限；
                    // 4. duan 保护不受影响（duan 单音节不进本循环；duanlian 的
                    // [du,an,lian] 若收割出「都安」类词属合法前缀词可达）。
                    if alt_words.len() < MAX_ALT_SEG_WORDS && seg.len() >= 3 && *_score < 1000 {
                        for k in (2..seg.len()).rev() {
                            if alt_words.len() >= MAX_ALT_SEG_WORDS {
                                break;
                            }
                            let head = &seg[..k];
                            for c in self.dict.query(head) {
                                if alt_words.len() >= MAX_ALT_SEG_WORDS {
                                    break;
                                }
                                if c.text.chars().count() >= 2
                                    && !existing_texts.contains(c.text.as_ref())
                                    && !alt_words.iter().any(|a| a.text == c.text)
                                    && !cands.iter().any(|full| full.text == c.text)
                                {
                                    split_prefix_texts.insert(c.text.as_ref().to_owned());
                                    alt_words.push(Candidate {
                                        text: c.text.clone(),
                                        base_freq: c.base_freq,
                                        pinyin: head.join(" ").into(),
                                        is_primary: c.is_primary,
                                        is_correction: false,
                                        is_sentence: false,
                                        is_guess: false,
                                        is_shortcut: false,
                                        is_english: false,
                                        sentence_score: 0,
                                    });
                                }
                            }
                        }
                    }

                    for c in cands {
                        if alt_words.len() >= MAX_ALT_SEG_WORDS {
                            break;
                        }
                        if c.text.chars().count() >= 2
                            && !existing_texts.contains(c.text.as_ref())
                            && !alt_words.iter().any(|a| a.text == c.text)
                        {
                            // 橙词带正确拼音（正确注音需求：注音含分隔符）
                            alt_words.push(Candidate {
                                text: c.text.clone(),
                                base_freq: c.base_freq,
                                pinyin: seg.join(" ").into(),
                                is_primary: c.is_primary,
                                is_correction: true,
                                is_sentence: false,
                                is_guess: false,
                                is_shortcut: false,
                                is_english: false,
                                sentence_score: 0,
                            });
                        }
                    }
                    if alt_words.len() >= MAX_ALT_SEG_WORDS {
                        break;
                    }
                }
                all_candidates.extend(alt_words);
            }
        }

        // 混合简拼回退：仅当不完整音节出现在第一个完整音节之前时触发。
        // bydi → [b,y,di]：b/y 在 di 之前 → 提取首字母 byd 做简拼匹配
        // buy → [bu,y]：y 在 bu 之后 → 正常混合输入，不走简拼回退
        // yig → [yi,g]：g 在 yi 之后 → 正常混合输入，不走简拼回退
        let first_complete_idx = best_syllables
            .iter()
            .position(|s| crate::pinyin::is_valid_syllable_pub(s) && s.len() > 1);
        let need_jianpin_fallback = first_complete_idx.is_some_and(|idx| {
            idx > 0
                && best_syllables[..idx]
                    .iter()
                    .any(|s| !crate::pinyin::is_valid_syllable_pub(s))
        });
        // 中间完整音节禁拍扁守卫：
        // 第一个完整音节不在末尾（后面还跟着音节）时，说明用户刻意打了这个
        // 完整音节又继续输入——拍扁成首字母会捞出与该音节完全对不上的词：
        // zpiny → [z,pin,y] 拍扁成 zpy → 装配员(zhuang pei yuan)/
        // 找朋友(zhao peng you)/占便宜(zhan pian yi)，第二音节 pei/peng/pian
        // 与用户明确输入的 pin 矛盾，纯噪音。
        // 此时 is_hybrid 路径（query_tokens [I(z),F(pin),I(y)]）已覆盖合法候选
        // （作品/正品/作品一），回退只产噪音，跳过。
        // 完整音节在末尾时（zpin→[z,pin]、bydi→[b,y,di]）仍允许拍扁
        // （zp→照片、byd→不一定）——末尾音节可能是"只打了前两三个字母"的半截，
        // 拍扁是合理猜测；中间音节有后续输入佐证，不是半截。
        //
        // 同位音节前缀约束（「尾部全拼必须精确匹配」，
        // 见 query.rs flatten_tail_compatible）：拍扁把所有音节取首字母，
        // 已完整输入的全拼段也被降级——hdong 的 dong 拍成 d，捞出「回答(hui da)/
        // 海底(hai di)/很多(hen duo)」这类第二音节只共首字母的噪音并霸屏。
        // 拍扁候选必须满足：第 i 音节以输入第 i 完整音节开头。
        // dong 位只出 dong（da/duo/di 全滤）、di 位出 di/ding/dian（续打半截
        // 「不一定」保留）——打完的字要精确，没打完的字母才宽松。
        let complete_not_at_end =
            first_complete_idx.is_some_and(|idx| idx < best_syllables.len() - 1);
        if need_jianpin_fallback && !complete_not_at_end && best_syllables.len() >= 2 {
            let jianpin: String = best_syllables
                .iter()
                .filter_map(|s| s.chars().next())
                .collect();
            if jianpin.len() >= 2 {
                let jp_candidates = self.query_jianpin_fuzzy(&jianpin);
                // 同位音节前缀约束过滤：完整输入的全拼段不许被
                // 拍扁降级成首字母混入对不上的词；简拼/不完整位不约束。
                let jp_candidates: Vec<Candidate> = jp_candidates
                    .into_iter()
                    .filter(|c| self.flatten_tail_compatible(&best_syllables, c))
                    .collect();
                all_candidates.extend(jp_candidates);
            }
        }

        // 用最优切分的音节数决定排序策略
        let is_multi_syllable = best_syllables.len() >= 2;
        let single_syllable_input = best_syllables.first().copied();

        // 统一排序策略：
        // 1. 多音节输入时词组优先（输入 "muq" 时 "目前" 排在 "去" 前面）
        // 2. 多音节输入时，字数多的词优先（houxuankuang 中 "候选" 排在 "后" 前面）
        // 3. 单音节输入时单字优先（输入 "ni" 时 "你" 排在 "你们" 前面）
        // 4. 单音节输入时，精确匹配当前音节的字优先于仅前缀匹配的字
        // 5. 同字数时，单字优先按常用度（char_freq）降序，生僻字靠后
        // 6. 同字数时，综合整数分数 = base_freq × rank_boost / 1000，按降序排列
        //
        // 键预计算排序：candidate_cmp 每层判定都是候选自身属性，
        // 预计算排序键（每候选只做 1 次 collocation Trie 查询 + bigram hash 查询）
        // 再按元组排序，替代 sort_by 里 O(n log n) 次两两 candidate_cmp（每次比较
        // 重复 2 次上下文查询）。排序结果与 candidate_cmp 逐位相同
        // （test_candidate_sort_key_matches_cmp 守护）。
        // 纯简拼让位判据：只与 input 有关，排序前算一次
        let input_first_syl_complete = Self::input_starts_with_complete_syllable(input);
        let mut keyed: Vec<(scoring::CandidateSortKey, Candidate)> =
            std::mem::take(&mut all_candidates)
                .into_iter()
                .map(|c| {
                    let key = self.candidate_sort_key(
                        input,
                        &c,
                        is_multi_syllable,
                        single_syllable_input,
                        best_score,
                        &best_syllables,
                        input_first_syl_complete,
                    );
                    (key, c)
                })
                .collect();
        // 键非 Copy（CandidateSortKey）→ 保留 sort_by（同 dict.rs 同类注）
        #[allow(clippy::unnecessary_sort_by)]
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        all_candidates = keyed.into_iter().map(|(_, c)| c).collect();

        // 去重
        all_candidates.dedup_by(|a, b| a.text == b.text);

        // 无匹配时：只补充第一个音节的单字候选，避免后续音节单字乱序
        if all_candidates.is_empty() {
            if let Some(first) = best_syllables.first() {
                all_candidates.extend(self.query_single_chars_fuzzy(first));
            }
            all_candidates.sort_by(|a, b| {
                let score_a = self.candidate_score(input, a);
                let score_b = self.candidate_score(input, b);
                match score_b.cmp(&score_a) {
                    Ordering::Equal => {
                        let common_a = char_commonness(&a.text);
                        let common_b = char_commonness(&b.text);
                        if common_a != common_b {
                            return common_b.cmp(&common_a);
                        }
                        a.pinyin.cmp(&b.pinyin).then_with(|| a.text.cmp(&b.text))
                    }
                    other => other,
                }
            });
            all_candidates.dedup_by(|a, b| a.text == b.text);
        }

        // 整句解码：长输入（≥2 有效音节）时，把音节串切成最优词组序列拼成整句候选。
        // 整句候选插入到列表最前（让用户连续输入长句时首候选即整句），
        // 但会与已有词组候选去重，避免「我喜欢」这种已是单词的情况重复。
        // 只在最优切分产生 >=2 个词时生成（单词组无整句价值）。
        // 跳过用户已 remove 的文本（removed_texts 会话级黑名单），避免把删掉的词拼回来。
        //
        // 平滑性关键（解决长句输入中途整句候选闪烁回退）：
        // best_syllables 可能末尾含「不完整音节」（用户正在输入，如 xiwangnih 的 h）。
        // 不能因末尾不完整就让整句解码整体失效（会导致「希望你」突然消失回退到「希望」）。
        // 做法：分离 valid 前缀 + partial 尾部，对 valid 前缀解码，partial 尾部用首字母
        // 匹配最高频字补全拼到整句末尾，让整句候选随输入平滑增长不闪烁。
        // 整句解码的音节收集策略（支持全拼+简拼混合输入）：
        // - 收集所有有效音节（长度≥2 的真正拼音音节）
        // - 中间穿插的简拼字母（如 qiehuanshurufzhihou 的 f）展开为该首字母的
        // 最高频音节（f→fa），让 DP 能正常处理「法」+「之后」，覆盖完整输入。
        // 这样混合输入也能生成完整绿词（「切换输入法之后」而非截断的「切换输入法」）。
        // - 末尾不完整声母（如 xiwangnih 的 h）作为 partial tail 用首字母补字。
        let mut valid_syl_owned: Vec<String> = Vec::new();
        // 与 valid_syl_owned 平行：该段是否「撇号显式分隔的单字母有效音节」（完整
        // 音节语义，非简拼）。绿词前缀回退据此区分尾部简拼串
        // 与前缀内撇号音节——回退只允许「前缀全完整音节 + 尾部简拼串」形态触发。
        let mut valid_apos_owned: Vec<bool> = Vec::new();
        // 三种「直接入表（不展开）」的情形合并成一个条件（动作一致：push）：
        // ① 完整拼音音节（长度≥2，如 san）；
        // ② 撇号显式分隔的单字母有效音节（如 lai'a 的 a —— 用户用 ' 强制断
        // 音节，语义是完整音节 a/e/o，不是简拼首字母/半截声母。 修：
        // 绿词 nibuyaoguolai'a 出「你不要过来安」，根因是 a 被当半截尾 →
        // 首字母补字选「安」+ lift 字数 5≠6 失效；s 是 input 切片，用指针
        // 偏移定位前置字节是否为 '）；
        // ③ 简拼字母（后面还有音节/简拼字母，或前面已有简拼字母入表）——
        // 含句首简拼（不规律混合绿词回退修复：z'g'ren'm'j 的
        // 句首 z/g 此前被「前面已有入表段」条件拒收 → 收集循环句首 break →
        // valid_count=0 → 整句解码整体跳过，绿词回退成单词（纯简拼 zgrmj
        // 走 process_jianpin_inner 自建音节表不受影响，行为不对称）。句首
        // 单字母后随内容即简拼意图——半截声母续打会先补本音节字母（f 后来
        // 的是 a 不会是 zhong），不可能出现「句首半截 + 后续完整音节」）——
        // 交给 decode_sentence(is_initial_mode=true) 的 query_seg 走首字母
        // 子树，DP 的 bigram 引导选词比 expand 阶段盲猜音节更准。
        // 边界：末尾「半截声母」（如 jisua 的 a 是 an 的半截）在前缀全为完整音节
        // 时不入表，交给 partial tail 用首字母补字（既有行为不变）；句首简拼形态
        // 下按③「末尾连续简拼的最后一个」随行入表（绿色整句覆盖全部输入）。
        for (idx, &s) in best_syllables.iter().enumerate() {
            let full_syllable = crate::pinyin::is_valid_syllable_pub(s) && s.len() >= 2;
            let apos_single = s.len() == 1
                && !valid_syl_owned.is_empty()
                && crate::pinyin::is_valid_syllable_pub(s)
                && {
                    let off = s.as_ptr() as usize - input.as_ptr() as usize;
                    off > 0 && input.as_bytes()[off - 1] == b'\''
                };
            let jianpin_single = if s.len() == 1 {
                let has_more_valid = best_syllables[idx + 1..]
                    .iter()
                    .any(|&next| crate::pinyin::is_valid_syllable_pub(next) && next.len() >= 2);
                let has_more_initial = best_syllables[idx + 1..]
                    .iter()
                    .any(|&next| next.len() == 1);
                let original_valid_count = best_syllables
                    .iter()
                    .take_while(|s| crate::pinyin::is_valid_syllable_pub(s) && s.len() >= 2)
                    .count();
                has_more_valid || has_more_initial || valid_syl_owned.len() > original_valid_count
            } else {
                false
            };
            if full_syllable || apos_single || jianpin_single {
                valid_syl_owned.push(s.to_string());
                valid_apos_owned.push(apos_single);
            } else {
                break;
            }
        }
        let valid_count = valid_syl_owned.len();
        let total_syllables = best_syllables.len();
        // partial 尾部：valid 之后还有内容（末尾不完整声母 h/x）
        let mut has_partial_tail = valid_count < total_syllables;
        // 裸尾单字母零声母音节：尾部
        // 恰为 1 个单字母有效音节（a/e/o，非辅音半截）——「词+语气词」
        // 的第二路 DP 并入后达到 3 音节门槛，外门对它放行（主路 decode 自带
        // MIN_SYLLABLES=3 自守，2 音节时自然空转）。
        let tail_is_particle = has_partial_tail
            && best_syllables.len() == valid_count + 1
            && best_syllables[valid_count].len() == 1
            && crate::pinyin::is_valid_syllable_pub(best_syllables[valid_count]);
        // ≥3 有效音节才触发整句解码（语气词尾 ≥2 也进）
        if valid_count >= 3 || (valid_count >= 2 && tail_is_particle) {
            let valid_syl: Vec<&str> = valid_syl_owned.iter().map(|s| s.as_str()).collect();
            // decode_sentence 返回 N-best 绿词候选（Top-3），逐个插入候选列表。
            // 第一个（最优）排最前，后续排其后，让用户看到多个绿词选项。
            // 如果 valid_syl 含单字符（简拼字母），用 initial_mode 让 DP 混合处理。
            // 单字母有效音节（撇号分隔的 a/e/o）不算简拼——
            // initial_mode 下 query_seg 把它当首字母 Token，会再捞到 an/ai 词
            // （绿词 nibuyaoguolai'a 出「安」的另一半根因）。
            let has_initial_in_valid = valid_syl
                .iter()
                .any(|s| s.len() == 1 && !crate::pinyin::is_valid_syllable_pub(s));
            // 绿词「全覆盖」判定的音节覆盖数：默认 = 当选切分的
            // valid_count；末尾单辅音被第二路 DP 并入后 +1（该路覆盖了整条输入）。
            let mut coverage_count = valid_count;
            // decode_sentence_collect：收集开关
            // 开启时（AI 请求 want_paths）顺带收集 DP 全消费路径——全拼长句的
            // 多切分变体（如 大致/打只 版）由此供 AI 当裁判挑选；开关关闭时
            // 与 decode_sentence 逐字节等价（limit=0 整段跳过）。
            let mut sentences = self.decode_sentence_collect(
                input,
                &valid_syl,
                has_initial_in_valid,
                has_partial_tail,
            );

            // ── 末尾单辅音 = 末音节缩写────────────
            //
            // 病灶：末尾裸露辅音（xuqiuyanj 的 j）被一律当「半截音节」排除出
            // DP，只剩 partial-tail 按首字母补一个单字（就 9920 > 究 1000），
            // 于是「需求言就」——而更粗的 xuqiuyj 反而因 green_memo 精确命中
            // 出「需求研究」，用户看到「细化输入反而回退」。
            //
            // 判别（关键）：单个辅音字母永远不可能是完整音节，只能是缩写；
            // 而 a/e/o 这类可独立成音节的单字母是半截长音节（jisua 的 a 想打
            // an），必须保持原有补字行为。zh/ch/sh 也保守不动（半截声母语义
            // 更强，且拆成两个 Initial 会发散）。
            //
            // 做法：第二路 DP 把尾巴作为 Initial token 并入，与本来的 partial-tail
            // 补字路比分取优——只有明显更优才替换，零证据/劣分时逐字节保持
            // 现状（平滑性守护 test_long_input_stability 不破）。
            if has_partial_tail {
                let tail = best_syllables[valid_count..].concat();
                let single_consonant = tail.chars().count() == 1
                    && tail.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                    && !crate::pinyin::is_valid_syllable_pub(&tail);
                if single_consonant {
                    let mut syl2: Vec<&str> = valid_syl.clone();
                    syl2.push(best_syllables[valid_count]);
                    let alt = self.decode_sentence(input, &syl2, true, false);
                    let cur_best = sentences.first().map(|c| c.sentence_score);
                    let alt_best = alt.first().map(|c| c.sentence_score);
                    // 严格更优才换（平分保留 partial-tail 现状——过渡态语义）。
                    if let (Some(a), Some(b)) = (alt_best, cur_best) {
                        if a > b {
                            // 诊断日志（默认静音——每次按键都会命中此处，不能刷屏；
                            // 排障时设 X1_DEBUG_TAIL_INITIAL=1）。
                            if std::env::var_os("X1_DEBUG_TAIL_INITIAL").is_some() {
                                eprintln!("[sentence] tail-initial DP wins ({tail}): {a} > {b}");
                            }
                            sentences = alt;
                            has_partial_tail = false; // 尾巴已被 DP 消费，不再补字
                            coverage_count = valid_count + 1;
                        }
                    } else if cur_best.is_none() {
                        // 第一路完全没产出（尾巴把 DP 挡空），第二路有货就用它。
                        if alt_best.is_some() {
                            if std::env::var_os("X1_DEBUG_TAIL_INITIAL").is_some() {
                                eprintln!(
                                    "[sentence] tail-initial DP rescues empty sentence ({tail})"
                                );
                            }
                            sentences = alt;
                            has_partial_tail = false;
                            coverage_count = valid_count + 1;
                        }
                    }
                }
            }

            // ── 尾部裸单字母零声母音节统一规则（统一三类场景：
            // zhenbena 组词 + jisua 中途态不闪绿词 + 微软式延长词置顶）──
            //
            // 输入尾部状态三分法：
            // ① 尾音节完整：完成态——「精确命中即不出猜测」照旧；
            // ② 尾=裸单字母零声母（a/e/o）：两可态，由延长词证据裁决——
            // 有延长词（∃词库词延长整个输入：末音节碎片延长 jisua←计算
            // ji'suan，或尾字母起新音节 woa←我爱 wo'ai）→ 半截语义：
            // 不出语气词绿词（jisuan 续打中途不闪「激素啊」）；最佳延长词
            // 以紫词经 tail_extension_hint 交给 process 注入（freq ≥
            // 当前首选则置顶＝微软式「jisua→计算」，选中全消费走 consumed
            // 规则①）；
            // 无延长词（zhenbena）→ 语气词语义：第二路 DP 把尾音节并入
            // 出全消费整句（真笨啊）与主路共存（consumed 规则③）；
            // ③ 尾=显式撇号后单字母：用户显式断音节恒语气词（apos_single 既有
            // 路径，jisuan'a → 计算啊），不受延长证据影响。
            let mut particle_green_texts: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            if tail_is_particle && valid_count >= 1 {
                let tail = best_syllables[valid_count];
                // 延长词证据（两种形态取并集）：
                // (a) 末音节碎片延长：su+a → 前缀 [ji] + 碎片 "sua" → 计算
                // (b) 尾字母起新音节：前缀 [wo] + "a" → 我爱 (wo ai)
                let mut ext = Vec::new();
                if valid_count >= 2 {
                    let head = &valid_syl[..valid_count - 1];
                    let frag = format!("{}{}", valid_syl[valid_count - 1], tail);
                    ext.extend(self.query_prefix_filtered_fuzzy(head, &frag));
                }
                ext.extend(self.query_prefix_filtered_fuzzy(&valid_syl, tail));
                let best_ext = ext.into_iter().max_by_key(|c| c.base_freq);
                if let Some(mut c) = best_ext {
                    // 紫：对未敲完输入的延长预测。仅外层查询写入
                    //（纠错递归子查询不覆盖主提示）。
                    c.is_guess = true;
                    if correct {
                        *self.tail_extension_hint.borrow_mut() = Some(c);
                    }
                } else if valid_count >= 2 {
                    // 无延长词证据 → 语气词读法（zhenbena 行为）：第二路 DP
                    // 全消费整句共存合并；合并句不参与 partial 补字（拼音已含
                    // 尾音节，再补会叠字），并计入 is_full_match 的全覆盖判定。
                    let mut syl2: Vec<&str> = valid_syl.clone();
                    syl2.push(tail);
                    let alt = self.decode_sentence(input, &syl2, has_initial_in_valid, false);
                    for s in alt {
                        if sentences.iter().all(|e| e.text != s.text) {
                            particle_green_texts.insert(s.text.as_ref().to_owned());
                            sentences.push(s);
                        }
                    }
                }
            }

            // ── 多切分组句仲裁──
            //
            // 原则：切分选举（score_for_syllables 启发式）只是提案，绿词 DP
            // 的词证据才是裁决。旧架构把当选切分当作 DP 唯一输入——选举错
            // 1 分（leng 被拆成 le|n|g，1026:1025），DP 就永远看不到
            // [jiao,leng,shui]→浇冷水 词边，只能在碎链上把 I(n)+I(g) 当简拼
            // 首字母拼出「不是交了那个谁」。与白词层的「橙词补充」（切分歧义
            // 保险）同构对称：对全完整音节的次优切分各跑一遍全拼 DP，各切分
            // 的绿词按 DP 路径分（sentence_score）全局仲裁——[不是][浇冷水]
            // 的词完整性/上下文 LM 证据天然碾压碎片链的负分垃圾路径。
            //
            // 严格约束：
            // · 备选清单已在选举段预筛（全完整音节、len≥3、按同源比较器降序
            // 至多 3 条）——碎片/简拼切分由当选 DP 承载，备选只竞争全拼读法；
            // · has_partial_tail 时跳过：悬尾字母在任何切分下都不成完整音节，
            // 全完整备选必然不存在，省 CPU；
            // · 走 decode_sentence（paths_limit=0 兼容入口）：不污染 AI 路径
            // 收集（last_sentence_paths）；
            // · 备选只贡献绿词候选——白词/单字/显示切分仍由当选切分产出，
            // 零侵扰；
            // · 仲裁用稳定排序：平分保当选切分的句子优先（现状惯性）。
            //
            // 两道收紧：
            // ① 整词短路全局化：当选切分本身命中词库整词（全拼模式整词短路
            // 同款判据）时，备选切分的任何句子都是拼凑——jiaolengshui 的
            // 当选 [jiao,leng,shui] 是「浇冷水」整词（当选 DP 短路为空），
            // 备选 [ji,ao,leng,shui] 的「骥骜冷水」不能钻空顶位；
            // ② 备选只能取代不能陪跑：sentence_score 严格赢过当选最优才入列
            // （nixiangganma 的备选 [ni,xi,ang,gan,ma] 拼出「逆袭昂干嘛」，
            // 分低应整段丢弃，不能塞进候选凑数）。
            // ③ 触发条件（eval 11 例翻转 + wananle 单例两轮修正定稿）：
            // 当选链含简拼首字母位（音节碎裂的结构签名）或当选解码空
            // （rescue）才仲裁。首字母边天生是猜测性证据（n,g→那个 只是
            // 联想），需要无碎裂的全拼读法来裁决；当选已是全完整音节时，
            // 其 DP 是字面读法的忠实解码——按分数二次猜测会惩罚「词+尾
            // 单字」的合法句（wananle：晚安了 -18 分被 南乐 词频倒挂成
            // 袜南乐）。结构签名判定还自带性能纪律：无碎裂即无额外 DP。
            let elected_whole_word = !has_initial_in_valid
                && !has_partial_tail
                && !self.dict.query(&valid_syl).is_empty();
            // ④ 词跨度更丰富的备选才能触发仲裁：
            // 选举用「最高单词频」定序——一个用户词/高频词（首钢 1355）能压过
            // 两个中等词组成更自然的链（手感 767 + 给力 1028）。词跨度数少于
            // 备选 = 当选读法在词典里几乎组不出词组，却因单个词频胜出，此时
            // 才让 DP 用真实语言证据（LM/词完整性）裁决。
            //
            // 为什么不用「选举分差」作触发：nizaiganmane 的当选与备选只差 3 分
            // （1107:1104），固定余量必然误触发；而它的当选词跨度 6 > 备选 3，
            // 说明选举本身选对了。wananle 同理（2 > 1，不触发，保住既有定稿）。
            // 分差上限（≤1000）只用于省 CPU：差太远的备选本来就该输。
            let winner_spans = self.word_span_count(&valid_syl);
            let more_lexical_alt = alt_full_segs.iter().any(|(seg, seg_score)| {
                best_score.saturating_sub(*seg_score) <= 1000
                    && self.word_span_count(seg) > winner_spans
            });
            // 带罚切分同款裁决：
            // 当选读法有产出时上一轮营救块不触发，但「带罚切分词跨度更丰富」
            // 同样说明选举用单词频压错了读法——保密(1434)+按照(~3000) 的 3 跨度
            // 读法应有机会用 DP 语言证据击败 面(1 跨度) 的拼凑句。与 more_lexical_alt
            // 同判据（跨度严格更多 + 分差 ≤1000），只是清单不同。
            let more_lexical_rescue = !sentences.is_empty()
                && rescue_segs.iter().any(|(seg, seg_score)| {
                    best_score.saturating_sub(*seg_score) <= 1000
                        && self.word_span_count(seg) > winner_spans
                });
            let run_arbitration = !has_partial_tail
                && !elected_whole_word
                && (!alt_full_segs.is_empty() || !rescue_segs.is_empty())
                && (has_initial_in_valid
                    || sentences.is_empty()
                    || more_lexical_alt
                    || more_lexical_rescue);
            let mut alt_sentence_texts: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            if run_arbitration {
                let elected_best = sentences.first().map(|c| c.sentence_score);
                let elected_texts: std::collections::HashSet<String> = sentences
                    .iter()
                    .map(|c| c.text.as_ref().to_owned())
                    .collect();
                // DP 池：无罚备选恒入池；带罚营救切分仅在当选有产出时入池
                //（more_lexical_rescue 路径）——当选空（rescue 场景）时带罚切分
                // 交给下方专用营救块（其 all_multi 质量门挡单字拼凑，本池的
                // 严格分胜出判据在无当选句参照时等于无门）。
                let pool: Vec<&(Vec<&str>, u32)> = if sentences.is_empty() {
                    alt_full_segs.iter().collect()
                } else {
                    alt_full_segs.iter().chain(rescue_segs.iter()).collect()
                };
                for (alt_seg, _) in &pool {
                    for s in self.decode_sentence(input, alt_seg, false, false) {
                        if let Some(best) = elected_best {
                            if s.sentence_score <= best {
                                continue;
                            }
                        }
                        if !elected_texts.contains(s.text.as_ref())
                            && !sentences.iter().any(|e| e.text == s.text)
                        {
                            alt_sentence_texts.insert(s.text.as_ref().to_owned());
                            sentences.push(s);
                        }
                    }
                }
                sentences.sort_by_key(|s| std::cmp::Reverse(s.sentence_score));
                // 落选句子的去留服从「长句绿词条数」设定（默认 1 条；
                // 可按数量设定 green_candidate_count）：
                // 条数 1 = 赢家通吃（仲裁只负责选对的那条，不留备选干扰选择）；
                // ≥2 = 复用 green_accept 动态门槛保留分差内的真歧义读法
                //（与单切分 DP 多绿词同一机制），并截断到设定条数。
                if !alt_sentence_texts.is_empty() && !sentences.is_empty() {
                    if self.green_candidate_count >= 2 {
                        let first = sentences[0].sentence_score;
                        let ratio = self.green_accept.threshold_ratio(first);
                        let tolerance =
                            ((first.unsigned_abs() as f64 * (1.0 - ratio)) as i32).max(4);
                        sentences.retain(|s| s.sentence_score >= first - tolerance);
                        sentences.truncate(self.green_candidate_count as usize);
                    } else {
                        sentences.truncate(1);
                    }
                }
            }

            // 拆分惩罚切分的营救仲裁（场景：baomianquan→保密安全
            // 无分隔符不可达）：当选切分 DP 零产出时，交给 rescue_segs 的
            // 惩罚切分跑一次 DP，取第一条「整句由 ≥2 个多字词完整覆盖」的读法
            // ——纯单字拼凑（报|面|全 类）一律拒收，保证营救出来的必然是真实
            // 词组而不是另一个垃圾句。救回一条即停（赢家通吃，与条数设定 1
            // 同语义；多条备选只会干扰选择）。
            // 只在 sentences 空时触发：当选读法有产出时纪律不变（duan 保护不受
            // 影响——duan 的当选 [duan] 有产出，营救不介入）。
            // 整词短路豁免：当选切分命中词库
            // 整词时 DP 按设计短路为空（浇冷水）——这个"空"是成功不是失败，
            // 不得当营救信号（否则带罚切分 [ji,ao,leng,shui] 的「骥骜冷水」
            // 会钻空顶掉整词）。
            if sentences.is_empty()
                && !has_partial_tail
                && !elected_whole_word
                && !rescue_segs.is_empty()
            {
                'rescue: for (alt_seg, _) in &rescue_segs {
                    // 词跨度质量门（此处才付查询开销）：切分在词典里至少能
                    // 组出 2 个真实词段，才值得为它跑一次 DP。
                    if self.word_span_count(alt_seg) < 2 {
                        continue;
                    }
                    for s in self.decode_sentence(input, alt_seg, false, false) {
                        if self.removed_texts.contains(s.text.as_ref()) {
                            continue;
                        }
                        let words = self.split_text_anchored(&s.text, alt_seg);
                        let all_multi = words.len() >= 2
                            && words.iter().all(|(w, _)| w.chars().count() >= 2);
                        if all_multi {
                            // 登记进 alt_sentence_texts：全消费判定按句子自身音节数
                            //（own_syls == green_char_count 即全消费）——营救句的
                            // 音节数（4）≠ 当选切分 valid_count（3），不登记会被
                            // is_full_match 漏判、降位到前缀白词之后
                            //（baomianquan 保密安全 会因此落 #1）。
                            alt_sentence_texts.insert(s.text.as_ref().to_owned());
                            sentences.push(s);
                            break 'rescue;
                        }
                    }
                }
            }

            // ── 拆分前缀词让位「更长延长预测」──
            //
            // 收割白词（六安 = lu|an）与既有紫词（乱七八糟，覆盖全部 7 字母）
            // 同场时，白词按「确定 > 猜测」序会压过紫词——但紫词解释了更多
            // 输入字母（7 > 4），是更强的意图信号（碎片绿词让位 B-fix 同一
            // 哲学）。规则：存在覆盖更长的紫词时，收割词降位到其后（最后一条
            // is_guess/is_sentence 之后）；无紫词或覆盖不更长时不动
            //（baomian 的 保密 #0 不受影响——无紫词在场）。
            if !split_prefix_texts.is_empty() {
                let cov_letters = |c: &Candidate| {
                    c.pinyin
                        .chars()
                        .filter(|ch| *ch != ' ' && *ch != '\'')
                        .count()
                };
                let longest_guess = all_candidates
                    .iter()
                    .filter(|c| c.is_guess)
                    .map(cov_letters)
                    .max();
                if let Some(gcov) = longest_guess {
                    let to_demote: Vec<usize> = all_candidates
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| {
                            split_prefix_texts.contains(c.text.as_ref())
                                && !c.is_sentence
                                && !c.is_guess
                                && !c.is_correction
                                && cov_letters(c) < gcov
                        })
                        .map(|(i, _)| i)
                        .collect();
                    if !to_demote.is_empty() {
                        let anchor = all_candidates
                            .iter()
                            .rposition(|c| c.is_guess || c.is_sentence)
                            .map(|p| p + 1)
                            .unwrap_or(0);
                        let mut demoted: Vec<Candidate> = Vec::new();
                        for i in to_demote.into_iter().rev() {
                            if i < anchor {
                                demoted.insert(0, all_candidates.remove(i));
                            }
                        }
                        if !demoted.is_empty() {
                            let at = anchor.min(all_candidates.len());
                            all_candidates.splice(at..at, demoted);
                        }
                    }
                }
            }

            // green_memo 候选优先提升：
            // decode_sentence 的 DP 打分可能让 green_memo 绿词排不到首位。
            // 这里在 DP 返回后，直接查 green_memo，把命中的候选提到 sentences 最前面。
            //
            //门槛降低（freq>=1500, counts>=1）：
            // 原版要求 counts>=3（用户组装 3 次才解锁），导致首次组装的绿词要等 3 次
            // 才立即排首位，体验滞后。降到 counts>=1（首次组装即可提升）。
            // 误选风险由 removed_texts 黑名单兜底（Command+退格删除后不再生成）。
            // 记忆提升的相对质量门：记忆条目若比
            // 当前当选读法的词组链更差（词跨度数更少），说明它是"一个高频词
            // 拼出来的怪句"（首钢诶里 = 首钢 + 诶 + 里，1 个跨度）——不该因为
            // 用户误选过一次就永久抢首位（手感给力 = 手感/给力，2 个跨度）。
            // 语义：记忆保证「该文本仍可达」，不保证「无条件置顶」；
            // 的首次即记忆语义对链路不劣的条目完全不变。
            //
            // 两侧同口径：基准与条目都用文本划分的多字词段
            // 数（sentence_chain_spans / memo_span_count）。旧基准用
            // word_span_count 的重叠词证据窗口（2 字词链重复计数），与条目的
            // 划分计数不可比——词库补口语组合词（安的/干的）后基准虚高到 2，
            // 学过的「反感得很」(1 段) 被误降级（verify 两测红）。
            // 本轮被记忆提升到首位的条目（仲裁后复核：若链路更差则降级）。
            let mut memo_lifted: Vec<String> = Vec::new();
            // DP 最优句自身词组链段数（记忆相对质量门的基准；无句不降级）。
            let best_sentence_spans = sentences.first().and_then(|c| self.sentence_chain_spans(c));
            if let Some(entries) = self.green_memo.get(input) {
                let full_char_count = valid_syl.len();
                let mut sorted: Vec<&(String, u32, Option<String>)> = entries
                    .iter()
                    .filter(|(_, f, _)| *f >= 1500) // 首次组装 user_freq=2000，门槛 1500 让它通过
                    .filter(|(t, _, _)| t.chars().count() == full_char_count)
                    .filter(|(t, _, _)| self.freq.counts.get(t).copied().unwrap_or(0) >= 1)
                    .collect();
                //tiebreaker（场景：anquanwu 绿词「安全无」4 次 vs
                // 自组「安全屋」4 次，gf 同为 5000 平局——恢复端 gf=1000+count×1000
                // 只看次数，谁第一随机）。用户语义 = 最近选择的赢：recent_score
                // 带时间衰减，最后选择的词分高。仅作平局裁决，不动主排序。
                sorted.sort_by(|a, b| {
                    b.1.cmp(&a.1).then_with(|| {
                        let ra = self.freq.recent_score(&a.0);
                        let rb = self.freq.recent_score(&b.0);
                        rb.cmp(&ra)
                    })
                });
                for (gt, gf, gp) in sorted.into_iter().rev() {
                    // 记忆条目的读音解析（与下方构造候选同一套三级：学习时读音 >
                    // 文本反推 > 当选切分）。质量门按条目自身读音计链长——
                    // 跨读法对齐（把「反感得很」按当选 fang'an 数段）会退化成
                    // 单字段，把正常条目误判成怪句。
                    let resolved_py = gp
                        .clone()
                        .or_else(|| self.derive_reading_from_text(input, gt))
                        .unwrap_or_else(|| valid_syl.join(" "));
                    let memo_syls: Vec<&str> = resolved_py.split_whitespace().collect();
                    // 记忆条目相对质量门：条目词组链
                    // 比「当前 DP 最优句」更差（多字词段更少）= 一个高频词拼出的
                    // 怪句，别抢首位；但保留可达（降到末尾，用户仍可翻到）。
                    // 对齐失败（多音/历史条目）不做判断，保持原提升行为。
                    let worse = match (
                        self.memo_span_count(gt, &memo_syls),
                        best_sentence_spans,
                    ) {
                        (Some(m), Some(b)) => m < b,
                        _ => false,
                    };
                    let at = if worse { sentences.len() } else { 0 };
                    if at == 0 {
                        memo_lifted.push(gt.clone());
                    }
                    // 已在 sentences 里的提升到首位，不在的插入首位
                    if let Some(pos) = sentences
                        .iter()
                        .position(|c| c.text.as_ref() == gt.as_str())
                    {
                        let item = sentences.remove(pos);
                        sentences.insert(at.min(sentences.len()), item);
                    } else {
                        sentences.insert(
                            at,
                            Candidate {
                                text: gt.as_str().into(),
                                base_freq: *gf,
                                // 学习时真实读音 > 文本反推 > 当选切分（
                                // 三级；解析已在质量门前完成 = resolved_py）
                                pinyin: resolved_py.clone().into(),
                                is_primary: true,
                                is_correction: false,
                                is_sentence: true,
                                is_guess: false,
                                is_shortcut: false,
                                is_english: false,
                                sentence_score: 0,
                            },
                        );
                    }
                }
            }
            // 整句解码空时前缀回退（绿词防截断，设计见
            //）：
            // 根因：尾部 1-2 个未拼完字母让 valid_syl 带上尾部简拼段 → 整句解码入
            // initial 模式 → query_tokens 混合分支无中段单字边 → 句首完整音节段死路
            // → DP 可达表全 0 → 绿词整体消失（前后键均走全拼模式，呈「长句突然消失
            // 再重组」跳变）。修法：解码空 + 形态为「≥3 完整音节前缀 + 尾部简拼串」
            // 时，尾部整体当悬尾，全拼模式解码前缀 + best_char_for_initial 补 1 字，
            // 绿词跨 f→fe→fen 逐字稳定。纯增量：只在绿词本来就不存在的空档触发，
            // 已有绿词的输入零影响。走 decode_sentence（paths_limit=0 兼容入口）而非
            // collect：AI 路径收集（last_sentence_paths）不受回退污染。
            let mut fallback_fired = false;
            if sentences.is_empty() && valid_count >= 3 {
                // 剥尾部连续简拼段：len==1 且非撇号分隔（撇号 a/e/o 是完整音节语义，
                // 不入 run）
                let mut run_len = 0usize;
                while run_len < valid_syl.len() {
                    let idx = valid_syl.len() - 1 - run_len;
                    if valid_syl[idx].len() == 1 && !valid_apos_owned[idx] {
                        run_len += 1;
                    } else {
                        break;
                    }
                }
                let prefix_len = valid_syl.len() - run_len;
                // 前缀须全为完整音节（≥2 字母有效）或撇号单字母音节——含中置简拼
                // （如 qiehuanshuruf… 的 f）不触发：那是用户简拼意图，尾补字无法覆盖。
                let prefix_all_full = valid_syl[..prefix_len]
                    .iter()
                    .zip(&valid_apos_owned[..prefix_len])
                    .all(|(s, &apos)| {
                        (s.len() >= 2 && crate::pinyin::is_valid_syllable_pub(s))
                            || (apos && s.len() == 1)
                    });
                if run_len > 0 && prefix_len >= 3 && prefix_all_full {
                    let tail: String = valid_syl[prefix_len..].concat();
                    let prefix_syl: Vec<&str> = valid_syl[..prefix_len].to_vec();
                    // has_partial_tail=true：免整词短路（教训，faxianyigew 同款）
                    let mut fb = self.decode_sentence(input, &prefix_syl, false, true);
                    if !fb.is_empty() {
                        for sentence in &mut fb {
                            let prev_char = sentence.text.chars().last().map(|c| c.to_string());
                            if let Some(ch) =
                                self.best_char_for_initial(&tail, prev_char.as_deref())
                            {
                                // 尾补字：Arc<str> 定长，重建 String 再转回（与上方
                                // 悬尾补字同构，非热路径，一次分配可接受）
                                let mut text = (*sentence.text).to_owned();
                                text.push_str(&ch);
                                sentence.text = Arc::from(text);
                                let mut pinyin = (*sentence.pinyin).to_owned();
                                pinyin.push(' ');
                                pinyin.push_str(&tail);
                                sentence.pinyin = Arc::from(pinyin);
                            }
                        }
                        sentences = fb;
                        fallback_fired = true;
                    }
                }
            }
            for (sent_idx, mut sentence) in sentences.into_iter().enumerate() {
                // 末尾不完整音节补全：用首字母匹配字拼到整句末尾。
                // 语气词读法共存句（拼音已含尾音节）不参与补字——再补会叠字。
                if has_partial_tail
                    && !particle_green_texts.contains(sentence.text.as_ref())
                {
                    let tail: String = best_syllables[valid_count..]
                        .iter()
                        .flat_map(|s| s.chars())
                        .collect();
                    let prev_char = sentence.text.chars().last().map(|c| c.to_string());
                    if let Some(ch) = self.best_char_for_initial(&tail, prev_char.as_deref()) {
                        // 整句末尾补全：Arc<str> 无法原地扩容（str 定长），重建 String 再转回。
                        // 该路径仅"末尾不完整音节补全"场景触发（稀有），非热路径，一次分配可接受。
                        let mut text = (*sentence.text).to_owned();
                        text.push_str(&ch);
                        sentence.text = Arc::from(text);
                        let mut pinyin = (*sentence.pinyin).to_owned();
                        pinyin.push(' ');
                        pinyin.push_str(&tail);
                        sentence.pinyin = Arc::from(pinyin);
                    }
                }
                if self.removed_texts.contains(sentence.text.as_ref()) {
                    // 用户删过的绿词降级排末尾（而非消失），用户可再选复活
                    if !all_candidates.iter().any(|c| c.text == sentence.text) {
                        all_candidates.push(sentence);
                    }
                } else if !all_candidates.iter().any(|c| c.text == sentence.text) {
                    let green_char_count = sentence.text.chars().count();
                    // ── 同长白词在场时不展示绿词──
                    //
                    // 报障：`haowuyc` 出「毫无用处（白）+ 好物因此（绿）」，绿词扎眼。
                    // 实测（120 个「4 字整词 + 尾段简拼」构造样本）：有同长白词时
                    // 74 例绿词被压到第 2 位，74/74 的 top1 白词都是该输入对应的
                    // 真实词，绿词全是「是一政策」「网络电视」「农家出租」这类拼凑
                    // （长尾白词 + 高频单字虚词的伪句）。
                    //
                    // 机制解释：绿词命中的判据只要求「多词拼接 + 音节全覆盖」，
                    // 当整段能拼成一个真词时，它必然也能被拆成「词 + 虚字」的碎片链，
                    // 后者的路径分含 word_adjust/单字惩罚抵消后常常仍然为正。也就是说
                    // 同长白词在场 = 这条绿词没有额外信息量，它只是同一串音节的
                    // 更差读法。既有代码只做到「不抢首位」（has_same_len_white），
                    // 用户要的是「干脆别出现」。
                    //
                    // 豁免：用户亲手教过的记忆条目（memo_lifted，本会话
                    // green_memo 提升进 sentences 首位的那条）仍然展示——那是用户的
                    // 明确选择，不该被白词压掉。
                    let memo_exempt = memo_lifted.iter().any(|m| m == sentence.text.as_ref());
                    // 同字数白词判定：排除绿词(is_sentence)、纠错(is_correction)、紫词(is_guess)。
                    // 紫词是对后续输入的猜测（末音节更长如 quan≠qu），不是用户当前输入的
                    // 精确匹配，不应阻挡 full-match 绿词排首位（修复'工作区'排
                    // 在紫词'工作犬'后面的问题）。
                    let has_same_len_white = all_candidates.iter().any(|c| {
                        !c.is_sentence
                            && !c.is_correction
                            && !c.is_guess
                            && c.text.chars().count() == green_char_count
                    });
                    // 全覆盖判定的 per-sentence 口径：
                    // 仲裁后句子可能来自备选切分，其音节数 ≠ 当选切分的
                    // valid_count（「不是浇冷水」5 字 vs 当选碎链 7 token）——
                    // 按句子自身拼音音节数判定。对当选切分的句子两者恒等
                    //（own == valid_count），前缀回退句（run_len≥2 时 own<
                    // valid_count）由第二子句守住旧口径，行为逐字节不变。
                    let own_syls = sentence.pinyin.split_whitespace().count();
                    // 语气词共存句（裸尾 a/e/o 并入的第二路 DP 产物）拼音已含
                    // 尾音节、字数==自身音节数——按全消费认可（has_partial_tail
                    // 对它们是主路语义，不适用）。
                    let is_particle_green =
                        particle_green_texts.contains(sentence.text.as_ref());
                    let is_full_match = is_particle_green
                        || (!has_partial_tail
                            && green_char_count == own_syls
                            && (alt_sentence_texts.contains(sentence.text.as_ref())
                                || green_char_count == coverage_count));

                    // 同字数白词在场且非记忆豁免 → 整条不展示（见上方长注释）。
                    // 注：`has_same_len_white` 计算在 is_full_match 之前，
                    // 这里仅做决定；位置判断的既有分支保持不变（下方 insert_pos）。
                    if has_same_len_white && !memo_exempt {
                        continue;
                    }
                    // 简拼整词在场：候选里已有「每字母
                    // 一字」的词库真词（4 字母输入 ↔ 4 字词）时，由更少 token 拼凑的
                    // 绿词不展示——与同长白词规则同一哲学（词库真词对位比拼凑链更
                    // 诚实），但长度可不等（渗色服 3 字 vs 似是而非 4 字）。无整词
                    // 命中时（ssefeng 无 7 字词）绿词照常，零影响。
                    let has_jp_whole = all_candidates
                        .iter()
                        .any(|c| Self::is_jianpin_whole_word(input, c));
                    if has_jp_whole && !memo_exempt {
                        continue;
                    }
                    // ── 同字数整词读法在场：拼凑绿词降位──
                    //
                    // 实证：aizengfenmin（min→ming 尾音节截断）延展猜测已产出词库
                    // 真词「爱憎分明」，拼凑绿句「爱憎分民」（爱憎 6690 领衔碎片链）
                    // 却抢占 pos 0——词集全量实测 91 例同型（bianfeiweiba→变废为宝
                    // 同：[紫] rank2 被 [句] 变非为把 8960 压制）。首版只认 is_
                    // correction 是认错通道——这类词来自紫词（延展猜测）通道
                    //（输入是词读音的严格前缀、音节数相同），橙词（编辑距离纠错）
                    // 同理适用，故两类整词读法都算。
                    //
                    // 哲学依据 = 结构层裁决（覆盖全部输入单位的
                    // 单词读法结构层优先于拼凑链，与词频无关）+ 猜测治理（精确
                    // 命中即不出猜测——整词精确命中时猜测本来就不在场，本规则
                    // 只在无精确命中、猜测合法在场的场景重排两个假设的顺序）+
                    // 纠错约束（低置信假设）：只降位不隐藏——整词读法假设
                    // 在前、字面拼凑链假设在后，两者都保留可见。
                    // 仅同字数（=同音节数整词读法）触发，不碰部分覆盖场景。
                    let has_same_len_word_reading = all_candidates.iter().any(|c| {
                        (c.is_correction || c.is_guess)
                            && c.text.chars().count() == green_char_count
                    });
                    // ── 碎片绿词让位「文本前缀延长词」──
                    //
                    // 场景（用户截图实测）：luanqiba 时绿词「乱七八」（三个单字
                    // 拼出的碎片整句）以 is_full_match 抢到 pos 0，而紫词延长
                    // 预测「乱七八糟」只能排 #2；再打一个音节后「乱七八糟」又
                    // 回到 pos 0——用户逐键体验就是「很接近了 → 回退 → 又回来」。
                    //
                    // 判据：绿词文本是某个紫词（延长猜测）的严格文本前缀时，
                    // 它只是同一意图的截断读法，没有独立信息量（真实意图显然
                    // 是继续延长的那个词）。规则：排到该紫词之后——只降位不隐藏
                    // （与 B-fix「同字数整词读法在场时降位」同一哲学，用户仍可翻到）。
                    // 与 has_same_len_word_reading 的区别：那条判等字数（同音节数），
                    // 本条判文本前缀 + 更长（绿词是延长词的前半截）。
                    let prefix_of_guess = all_candidates.iter().position(|c| {
                        c.is_guess
                            && c.text.chars().count() > green_char_count
                            && c.text.starts_with(sentence.text.as_ref())
                    });
                    // 第一个（最优）绿词排首位（完整匹配时），后续绿词排其后
                    let insert_pos = if let Some(p) = prefix_of_guess {
                        // 延长预测在前、碎片绿词紧跟其后
                        p + 1 + sent_idx
                    } else if sent_idx == 0
                        && is_full_match
                        && !has_same_len_white
                        && !has_same_len_word_reading
                    {
                        0
                    } else if has_same_len_word_reading {
                        // 降位：紧跟同字数整词读法（紫/橙）之后
                        all_candidates
                            .iter()
                            .position(|c| {
                                (c.is_correction || c.is_guess)
                                    && c.text.chars().count() == green_char_count
                            })
                            .map(|p| p + 1 + sent_idx)
                            .unwrap_or(sent_idx)
                    } else if has_partial_tail || fallback_fired {
                        // 覆盖序：带尾句子覆盖了全部输入（valid + 尾首字母
                        // 补字），排位按覆盖长度——插到第一个「覆盖不足」的白词之前；覆盖
                        // ≥ 句子（精确命中）的白词仍在其前。修复 faxianyigewent：全消费句子
                        // （14/14）此前固定插在第一个多字白词之后，被 6/14 覆盖的「发现」
                        // 压住，候选随键入长短回跳（实测案例 faxianyigewent）。
                        // 前缀回退候选同走覆盖序——它同样
                        // 覆盖全部输入（前缀解码 + 尾补 1 字），应保持绿词居首不回跳。
                        // 覆盖比较：句子侧用补字后字符数（中文词字数==音节数），白词侧用
                        // 拼音音节数。多句相邻靠扫描自校正：已插入的句子是 is_sentence，
                        // 不进扫描；每插一句锚点白词下标 +1，直接插在其下标处即保持相邻。
                        all_candidates
                            .iter()
                            .position(|c| {
                                !c.is_sentence
                                    && !c.is_correction
                                    && !c.is_guess
                                    && c.pinyin.split_whitespace().count() < green_char_count
                            })
                            .unwrap_or(all_candidates.len())
                    } else {
                        // 后续绿词排在前一个绿词后面，或第一个多字白词后面（也排除紫词）
                        all_candidates
                            .iter()
                            .position(|c| {
                                !c.is_sentence
                                    && !c.is_correction
                                    && !c.is_guess
                                    && c.text.chars().count() >= 2
                            })
                            .map(|p| p + 1 + sent_idx)
                            .unwrap_or(sent_idx)
                    };
                    let insert_pos = insert_pos.min(all_candidates.len());
                    all_candidates.insert(insert_pos, sentence);
                }
            }
        }

        // 备选读法整句的橙词通道：
        //
        // fang'an / fan'gan 类真歧义——两种读法都无拆分惩罚、选举同分
        // （1999:1999），词频裁决
        // 让当选切分走 fang'an，此后候选层整体锁死在 fang 家族：白词没有
        // 「反感」、绿词只出「方案得很」，另一种读法完全不可达——用户
        // 只能拆开成 fan'gan + dehen 分两次输入（真实报障）。
        //
        // 机制：把「橙词补充」从词级扩展到句子级——备选全完整音节切分
        // （零拆分惩罚，与仲裁同一资格口径）解出的整句以橙词入列，紧跟最后
        // 一条绿词之后（插入位置即排位，append 会沉底不可见）。橙 = 备选
        // 读法，不抢首选；用户选一次后 commit 学习（green_memo 按原始 input
        // 记忆）下次直达绿词首位——可达性靠系统通道，首选正确率靠学习收敛。
        //
        // 噪音门（与 election 分差对齐）：仅当备选与当选同层级且分差 ≤50
        // 才出示——选举启发式分不出高下才叫真歧义；wananle（1478 vs 1013）
        // 这类一边倒读法不出示「袜南乐」式垃圾备选。与已入列候选去重（仲裁
        // 已顶位的句子不重复出现）；removed_texts 不回魂；上限 2 条。
        let mut alt_orange_added = 0;
        for (alt_seg, alt_score) in &alt_full_segs {
            if alt_orange_added >= 2 {
                break;
            }
            if best_score.saturating_sub(*alt_score) > 50 {
                continue; // 一边倒，非真歧义
            }
            for s in self.decode_sentence(input, alt_seg, false, false) {
                if alt_orange_added >= 2 {
                    break;
                }
                if all_candidates.iter().any(|c| c.text == s.text) {
                    continue;
                }
                if self.removed_texts.contains(s.text.as_ref()) {
                    continue;
                }
                alt_orange_added += 1;
                let pos = all_candidates
                    .iter()
                    .rposition(|c| c.is_sentence)
                    .map(|p| p + 1)
                    .unwrap_or_else(|| all_candidates.len())
                    .min(all_candidates.len());
                all_candidates.insert(
                    pos,
                    Candidate {
                        is_sentence: false,
                        is_correction: true,
                        ..s
                    },
                );
            }
        }

        // green_memo 全覆盖召回：
        // 用户自组学习过的组合句（green_memo，key=原始 input）召回不依赖
        // DP 与 valid≥3 门槛——尾段 a/e/o 被保守当半截音节的输入（naera
        // 的 a）valid=2，整句解码整体被跳过，学习过的「哪儿啊」此前永远召
        // 不回（场景：naera 依次选「哪儿」+「啊」后不再出「哪儿啊」）。
        // 判据：条目文本字数 == 输入全部音节数（覆盖含尾段的完整输入）——
        // 与 ≥3 块内的旧 lift（覆盖 valid 前缀、走尾段补字）互补；green_memo
        // 只存 3..=8 字组合（写入侧门槛），valid<3 时不存在 n==valid 的条目，
        // 两路不重叠；全覆盖条目按学习原样插入、不补字（文本已覆盖尾段）。
        // 黑名单/去重同旧 lift；排位 = 插到候选列表最前（见下方插入处注释：
        // process_inner 末尾只截断不重排，插入位置即用户可见排位）。
        // 未学过的 DP 绿词仍走 ≥3 门槛（jisua 半截尾不出绿词，
        // verify_truncated_tail_no_green_word 守护不动）。
        // 记忆相对质量门基准（本块在 ≥3 块之外）：取当前候选里最优句的
        // 词组链段数；
        // 没有句子候选时不降级（None → worse=false，与旧注释语义一致）。
        let best_chain_spans = all_candidates
            .iter()
            .find(|c| c.is_sentence)
            .and_then(|c| self.sentence_chain_spans(c));
        if let Some(entries) = self.green_memo.get(input) {
            let total_syl = best_syllables.len();
            let mut sorted: Vec<&(String, u32, Option<String>)> = entries
                .iter()
                .filter(|(_, f, _)| *f >= 1500) // 首次组装 user_freq=2000，门槛 1500 让它通过
                .filter(|(t, _, _)| t.chars().count() == total_syl)
                .filter(|(t, _, _)| self.freq.counts.get(t).copied().unwrap_or(0) >= 1)
                .collect();
            // 平局裁决同旧 lift：最近选择的赢（recent_score 带时间衰减）
            sorted.sort_by(|a, b| {
                b.1.cmp(&a.1).then_with(|| {
                    let ra = self.freq.recent_score(&a.0);
                    let rb = self.freq.recent_score(&b.0);
                    rb.cmp(&ra)
                })
            });
            // 降序迭代 + 插入位锚定：旧版 `.rev()` 升序头插，配合「同名候选
            // 在场即跳过」会产生强度序倒挂——强条目被 DP 产出的同名候选
            // 顶替跳过，弱条目独自头插霸占首位，用户的高频偏好完全丢失。
            // 改为：降序迭代（强在前），缺席条目插在 insert_at（起始 0，
            // 每插一条 +1）；**在场条目不移动**（在场候选排位由统一排序/
            // 上下文 bigram 裁决，教学映射不霸位），但把 insert_at 锚到它
            // 身后——更弱的缺席条目不得越过在场的强者头插。
            let mut insert_at = 0usize;
            for (gt, gf, gp) in sorted.into_iter() {
                if self.removed_texts.contains(gt.as_str()) {
                    continue;
                }
                // 条目读音解析（同 ≥3 块内 lift：学习读音 > 文本反推 > 当选切分）。
                // 质量门按条目自身读音计链长。
                let resolved_py = gp
                    .clone()
                    .or_else(|| self.derive_reading_from_text(input, gt))
                    .unwrap_or_else(|| best_syllables.join(" "));
                let memo_syls: Vec<&str> = resolved_py.split_whitespace().collect();
                // 记忆条目相对质量门（同 ≥3 块内 lift）：条目词组链比当前最优句
                // 更差 → 降级到末尾保可达（对齐失败不判断）。
                let memo_worse = match (
                    self.memo_span_count(gt, &memo_syls),
                    best_chain_spans,
                ) {
                    (Some(m), Some(b)) => m < b,
                    _ => false,
                };
                if let Some(pos) = all_candidates
                    .iter()
                    .position(|c| c.text.as_ref() == gt.as_str())
                {
                    // 在场：不移动，只为更弱条目锚定插入位。
                    insert_at = insert_at.max(pos + 1);
                    continue;
                }
                // 缺席条目：插到 insert_at（强度序位；0 = 列表最前——
                // process_inner 末尾只 truncate 不重排，push 会把召回压到
                // 第一页外）；插完 +1 让后续更弱条目落其后。质量门降级
                // 条目保持尾部保可达的旧语义。
                let at = if memo_worse { all_candidates.len() } else { insert_at };
                all_candidates.insert(
                    at,
                    Candidate {
                        text: gt.as_str().into(),
                        base_freq: *gf,
                        // 学习时真实读音 > 文本反推 > 当选切分（三级；
                        // 解析已在质量门前完成 = resolved_py）
                        pinyin: resolved_py.into(),
                        is_primary: true,
                        is_correction: false,
                        is_sentence: true,
                        is_guess: false,
                        is_shortcut: false,
                        is_english: false,
                        sentence_score: 0,
                    },
                );
                if !memo_worse {
                    insert_at += 1;
                }
            }
        } else {
            // 兜底查持久全集：
            // green_memo 是容量受限缓存（恢复时按 user_freq 保留前 100，
            // 见 load 侧截断注释），教过一两次的记忆 user_freq=2000~3000
            // 挤不进前 100 → 服务重启后召回消失。input_mappings 是持久
            // 全集（无截断）——按 input 精确兜底查回，计算口径与
            // learning.rs 写入侧完全一致（base_freq.max(1000+count×1000)）。
            // 冷缓存 miss 时才扫（HashMap 3-4k 条 ≈ 每键几十 µs，可接受）。
            let total_syl = best_syllables.len();
            let mut recovered: Vec<(String, u32)> = self
                .input_mappings
                .iter()
                .filter(|((mi, _), _)| mi.as_str() == input)
                .map(|((_, mt), cnt)| {
                    let base = crate::char_freq::compute_freq(mt).max(1);
                    let uf = (1000u32.saturating_add((*cnt).saturating_mul(1000))).min(10000);
                    (mt.clone(), base.max(uf))
                })
                .filter(|(t, gf)| {
                    *gf >= 1500
                        && t.chars().count() == total_syl
                        && self.freq.counts.get(t).copied().unwrap_or(0) >= 1
                })
                .collect();
            recovered.sort_by(|a, b| {
                b.1.cmp(&a.1).then_with(|| {
                    let ra = self.freq.recent_score(&a.0);
                    let rb = self.freq.recent_score(&b.0);
                    rb.cmp(&ra)
                })
            });
            // 降序迭代 + 插入位锚定（与 green_memo 命中分支同语义的
            // 强度序倒挂修复）。
            let mut insert_at = 0usize;
            for (gt, gf) in recovered.into_iter() {
                if self.removed_texts.contains(&gt) {
                    continue;
                }
                // 记忆条目相对质量门（与 green_memo lift 同规则）：条目
                // 词组链比当前最优句更差（多字词段更少）→ 只是「一个高频词
                // 拼出的怪句」，降到末尾保可达，不抢首位（误选一次不该
                // 永久霸屏）。
                let recovered_py = self
                    .derive_reading_from_text(input, &gt)
                    .unwrap_or_else(|| best_syllables.join(" "));
                let recovered_syls: Vec<&str> = recovered_py.split_whitespace().collect();
                let recovered_worse = match (
                    self.memo_span_count(&gt, &recovered_syls),
                    best_chain_spans,
                ) {
                    (Some(m), Some(b)) => m < b,
                    _ => false,
                };
                if let Some(pos) = all_candidates
                    .iter()
                    .position(|c| c.text.as_ref() == gt.as_str())
                {
                    // 在场：不移动（统一排序/上下文裁决其排位），只为更弱
                    // 缺席条目锚定插入位。
                    insert_at = insert_at.max(pos + 1);
                    continue;
                }
                let at = if recovered_worse {
                    all_candidates.len()
                } else {
                    insert_at
                };
                all_candidates.insert(
                    at,
                    Candidate {
                        text: gt.into(),
                        base_freq: gf,
                        pinyin: best_syllables.join(" ").into(),
                        is_primary: true,
                        is_correction: false,
                        is_sentence: true,
                        is_guess: false,
                        is_shortcut: false,
                        is_english: false,
                        sentence_score: 0,
                    },
                );
                if !recovered_worse {
                    insert_at += 1;
                }
            }
        }

        // 上下文证据发音营救：合法输入的发音变体
        // 猜测——只在搭配链证据过硬时触发（词级上文证据 / 句级链证据），
        // 模糊音已开的对子跳过。橙色纠错身份，插在最佳自然候选之后。
        if best_syllables
            .iter()
            .all(|s| crate::pinyin::is_valid_syllable_pub(s))
        {
            self.context_phonetic_rescue(&best_syllables, &mut all_candidates);
        }

        // 最终候选数量保险丝：排序+整句插入完成后截断。
        // 用户可见结果零损失——一页 7-9 个候选，300 ≈ 30+ 页，无人翻到；
        // 防模糊音+长输入病理场景（500+ 候选）的内存/排序/FFI 传输膨胀。
        // 放在 maybe_correct 之前：纠错候选是用户强信号（拼错时的救命候选），
        // 即使主列表触顶也必须保留追加。
        all_candidates.truncate(Self::MAX_FINAL_CANDIDATES);

        // 智能纠错回退：候选不足且开关开启时，用编辑距离变形补充候选。
        // 纠错候选标记 is_correction=true，排在正常候选之后，候选框用橙色显示。
        if correct {
            self.maybe_correct(input, &mut all_candidates);
        }

        // 固定候选槽位插入：一切排序/提升完成后的最后一步，
        // 固定项压过 green_memo/整句/上下文（详见 pins.rs 头注）。
        self.apply_pins(input, &mut all_candidates);

        all_candidates
    }

    /// 智能单词识别·中文模式：纯字母输入命中英文词表时注入英文
    /// 单词候选。精确命中（≤1 条）时不出前缀补全（dog 不出 dogma）；未精确命中
    /// 且输入 ≥3 字符切不出完整拼音时才出前缀补全（≤3 条 freq 降序）——
    /// chi/shi 这类合法拼音不出补全噪音（chi→child 会抢「吃」的 space 提交）。
    /// 排位：
    /// ① 紧跟第一个白色词库整词（多字白词）之后（morning 排末日后）；
    /// ② 无词级锚点：英文精确命中让过全部猜测类（绿/紫/橙）排其前（happy 案）；
    /// 可完整切分为拼音（sun/men 类重叠串）再让首个白色单字（sun→孙 在前）；
    /// 否则 index 0 登顶（dog 案）；
    /// ③ 候选列表为空 → index 0。
    /// 注入在所有排序/提升之后定点插入（沿用绿词/pins 模式，不动
    /// candidate_sort_key）；候选只带 is_english 标记、pinyin 恒空
    /// （consumed_lens 天然全消费，前端 rest 恒空）、不进中文学习面。
    fn inject_english_candidates(&self, input: &str, cands: &mut Vec<Candidate>) {
        if !self.smart_word_zh_enabled {
            return;
        }
        let key: String = input.chars().filter(|c| *c != '\'').collect();
        let mut injected: Vec<Candidate> = Vec::new();
        if let Some((word, freq)) = self.english_lookup_exact(input) {
            injected.push(Self::english_candidate(&word, freq));
        } else {
            // 前缀补全（未精确命中时，子开关 smartWordZhPrefix 门控）：归一化后
            // ≥3 字符才给（2 字母前缀噪音大），最多 3 条 freq 降序。
            let key_len = key.chars().count();
            if self.smart_word_zh_prefix_enabled
                && key_len >= 3
                && crate::pinyin::segmentations(&key, 4).is_empty()
            {
                for (w, f) in self.english_lookup_prefix(&key, 3) {
                    injected.push(Self::english_candidate(&w, f));
                }
            }
        }
        if injected.is_empty() {
            return;
        }
        // 排位终版（happy 排「哈珀培养/哈珀」之后的场景：
        // 「与英文匹配度最高的 happy 在第三位不合理」）：
        // 英文精确命中的唯一让位者 = 拼音覆盖整个输入的白色候选——判据
        // 与 consumed_lens 规则①「前缀扩展」同源（候选拼音去空格以输入为前缀）：
        // · 不是 "bushi" 覆盖 bus ✓、问题 "wenti" 覆盖 went ✓、我们 "women"
        // 覆盖 women ✓、孙 "sun" 覆盖 sun ✓ → 中文整词保住首位（打拼音出
        // 中文的基本盘；bus→不是 是既定定案，不得回归）；
        // · 哈珀 "hapo" 覆盖不了 happy ✗、哈珀培养 ✗、末日 "mori" 覆盖不了
        // morning ✗ → 让位，英文登顶。
        // 绿/紫/橙猜测类恒让位（二修「确定 > 猜测」同向）。smartWordZhPriority
        // 关 = 回退旧排位。
        let pinyin_like = !crate::pinyin::segmentations(&key, 4).is_empty();
        let is_full_match_white = |c: &Candidate| {
            if c.is_english || c.is_correction || c.is_guess || c.is_sentence {
                return false;
            }
            let compact: String = c
                .pinyin
                .chars()
                .filter(|ch| *ch != ' ' && *ch != '\'')
                .collect();
            compact.starts_with(key.as_str())
        };
        let pos = if self.smart_word_zh_priority_enabled {
            match cands.iter().position(is_full_match_white) {
                Some(a) => a + 1,
                None => 0,
            }
        } else {
            // 「英文优先」关（smartWordZhPriority=false）：回退旧排位——英文跟在
            // 首个多字候选之后（绿/紫也算锚），无锚时拼音可解释 index 1 / 否则 0。
            match cands
                .iter()
                .position(|c| !c.is_english && !c.is_correction && c.text.chars().count() >= 2)
            {
                Some(a) => a + 1,
                None if pinyin_like && !cands.is_empty() => 1,
                None => 0,
            }
        };
        for (k, cand) in injected.into_iter().enumerate() {
            cands.insert(pos + k, cand);
        }
    }

    /// 英文单词候选构造：text=词形、pinyin 恒空、base_freq=英文侧词频。
    fn english_candidate(word: &str, freq: u32) -> Candidate {
        Candidate {
            text: word.into(),
            base_freq: freq,
            pinyin: "".into(),
            is_primary: true,
            is_correction: false,
            is_sentence: false,
            is_guess: false,
            is_shortcut: false,
            is_english: true,
            sentence_score: 0,
        }
    }

    /// 单音节拆分候选注入。
    ///
    /// 场景：输入恰为单个完整音节且可两段拆成有效音节（yuan→yu|an、
    /// tuan→tu|an、mian→mi|an、yue→yu|e、xian→xi|an……），词库有对应
    /// 两音节词（预案/图案/蜜胺/余额/西安）时，不打 ' 也能拿到拆分词。
    ///
    /// 与既有「duan 保护」的关系：橙词补充（MAX_ALT_SEG_WORDS 路径）刻意把
    /// 单音节输入排除（保护完整音节 duan 不被拆成 du|an 的「都安」）——那是
    /// 无差别默认行为；本功能是用户显式开关（ambig_split_enabled），且只对
    /// 词库确有两音节词的输入生效（duan 无「都安」类词，开了也不出）。
    ///
    /// 插入位 = ambig_split_position（1 起算，2..=5，默认 2）；首位恒留给
    /// 完整音节最优解。已在候选中的文本不重复插入。
    ///
    /// 门控（任一不满足直接 return）：
    /// - 开关关 / 双拼模式（未强制全拼）——双拼 buffer 的拆分由方案本身决定；
    /// - 输入含 '（用户已手动分隔，正常路径已出词）；
    /// - 输入不是单个完整音节，或无两段有效拆分。
    fn inject_ambig_split_candidate(&self, input: &str, cands: &mut Vec<Candidate>) {
        if !self.ambig_split_enabled {
            return;
        }
        if self.shuangpin.is_some() && !self.force_fullpinyin {
            return;
        }
        if input.contains('\'') || !crate::pinyin::is_valid_syllable_pub(input) {
            return;
        }
        // 两段拆分：拼音是 ASCII，字节切点即字符切点；两段都必须是完整音节。
        // 切点范围 2..len（首段≥2 字符，末段可 1 字符——yue→yu|e 的 e；
        // 更短首段无实例：零声母单字 a/e/o 后接不出完整音节）。
        let bytes = input.as_bytes();
        let mut best: Option<Candidate> = None;
        for k in 2..bytes.len() {
            let a = &input[..k];
            let b = &input[k..];
            if !crate::pinyin::is_valid_syllable_pub(a) || !crate::pinyin::is_valid_syllable_pub(b)
            {
                continue;
            }
            // 只要恰好两音节的词（字数==2；更长词挂在同节点属前缀噪声）
            for c in self.dict.query(&[a, b]) {
                if c.text.chars().count() != 2 || c.is_correction || c.is_guess {
                    continue;
                }
                if best.as_ref().is_none_or(|b_| c.base_freq > b_.base_freq) {
                    best = Some(c);
                }
            }
        }
        let Some(cand) = best else { return };
        if cands.iter().any(|c| c.text == cand.text) {
            return; // 正常路径已给出（如用户后续输入触发了别的切分），不重复
        }
        let idx = (self.ambig_split_position as usize - 1).min(cands.len());
        cands.insert(idx, cand);
    }

    /// 智能纠错回退：当输入存在结构性拼音错误时，对输入做变形（键盘误触 +
    /// 发音混淆），重新查询补充候选。纠错候选标记 is_correction=true 并排在正常
    /// 候选之后。
    ///
    /// 触发哲学（结构错误才纠错）：只对「无法切出有效拼音结构」的输入纠错。
    /// 一个输入若满足以下任一条件，视为结构合法，不纠错：
    /// 1. `segmentations(input)` 非空——能完整切成全有效音节（如 nihao、pingzeng）；
    /// 2. `is_legal_jianpin_mix(input)`——合法的「全拼+简拼尾」混合（如 yunxs=yun+x+s）。
    ///
    /// 只有两者都不满足（如 yunxongshi 的 xong 非有效音节且非合法简拼、nihsou
    /// 的 h 孤立夹在完整音节中间）才纠错。
    ///
    /// 这样避免了「词库里没这个词」（如 pingzeng 能切出 ping+zeng）被误判为
    /// 拼错而冒出一堆不相关橙词（敬赠/睥睨/品格）的噪音。
    fn maybe_correct(&self, input: &str, candidates: &mut Vec<Candidate>) {
        if !self.smart_correction_enabled || input.len() < 5 {
            return;
        }
        // 纯字母输入才纠错（含分隔符或非字母的不处理）
        if !input.chars().all(|c| c.is_ascii_lowercase()) {
            return;
        }
        // 智能单词识别短路：输入是完整英文单词（如 morning）时
        // 不纠错——键盘邻接/发音变形会产出橙色噪音（morning→…），英文召回
        // 已由 inject_english_candidates 承担。
        if self.english_lookup_exact(input).is_some() {
            return;
        }
        // 结构合法性检查：能切成全有效音节 → 正常输入，不纠错。
        // 伪音节 m 例外：单字母 "m"（呣）在
        // VALID_SYLLABLES 里，jimtian 的 [ji,m,tian] 靠它达成"完整切分"——
        // 中插字母 typo 被吸收，邻键误触（m→n 的 今天）永不纠错，用户只看到
        // 积木/几秒 这类拼凑候选。m 作为独立音节实际无人会打（呣 极罕见且
        // 不会夹在词中间）；a/e/o（啊/哦/额）保留——有真实独立输入场景。
        // 判定：存在不含 m 段的完整切分才算结构合法。
        let has_real_segmentation = crate::pinyin::segmentations(input, 4)
            .iter()
            .any(|seg| !seg.contains(&"m"));
        if has_real_segmentation {
            return;
        }
        // 结构合法性检查：合法的「全拼+简拼尾」混合 → 正常输入，不纠错。
        // 例外：多打字母类错误会被合法简拼解析吞掉
        // （shen+n+g → 神女/神农 系错误解读）。注入「双写字母折叠」纠错候选后返回。
        if is_legal_jianpin_mix(input) {
            self.inject_collapse_corrections(input, candidates);
            return;
        }
        // 到这里：输入既不能切成全有效音节，也不是合法简拼混合 → 结构性错误，纠错。

        // 类别分层短路：一次打错只有一个成因——按概率
        // 顺序逐类尝试（键盘误触 > 发音混淆 > 键序颠倒），第一类产出候选后
        // 立即停止，不再叠加后续类别的假设。旧实现把三类变体拼成一个列表
        // 混收，"删除+换声母+换字母序"多种错误理论同时上桌，纠出的词离用户
        // 本意越来越远（越纠越偏）。输入真混乱（所有类别都产不出合理候选）时
        // 宁可不出词——用户承担乱打的后果，引擎不硬凑。
        // 纠错类型分控（用户可分别开关三类）。
        let mut existing: std::collections::HashSet<String> = candidates
            .iter()
            .map(|c| c.text.as_ref().to_owned())
            .collect();
        let mut corrections: Vec<Candidate> = Vec::new();
        // 纠错目标已在自然列表（部分匹配已捞到）时的「提升」清单：
        // 旧实现对已存在词条静默跳过——既不标橙也不提升，纠错
        // 的"救命候选 + 提到纠错位"价值被去重吞掉（baiddu 的 百度 本在 #7）。
        // 现改为标橙 + 移到纠错位，与折叠路径（inject_collapse_corrections）
        // 的提升语义对齐。
        let mut promotions: Vec<String> = Vec::new();
        // max_corrections=0 时跳过整个变形查询循环（相当于禁用纠错补充）。
        let max_corrections = self.max_corrections;
        //性能修复 + 预算重修：结构非法输入（乱码，
        // 如 qzvwx）时，变形后能切出全拼音的变体也可能全部查不到词库候选 →
        // corrections 永不达标 → 旧实现跑完所有变体（6 字符 ≈ 162 个）的完整
        // process_inner（每次 1-8ms）→ 单键 50-300ms 冻结。
        // 预算语义：按类别独立计数。旧实现全类别共享 6 次——键盘类
        // 的「结构合法但词库无词」无效变体（weishemne 实测 9 个）会烧光预算，
        // 排在后面的键序类一次都轮不到（为什么 永远纠不出）。每类独立 3 次：
        // 乱码最坏 3 类 × 3 = 9 次查询（≈旧 6 次同量级），正常路径（前几个
        // 邻接替换变体即可收满，见 test_correction_keyboard_neighbors_priority）
        // 完全不受影响。变体顺序：键盘类按误触概率（邻接 > 删除），发音/键序类
        // 为确定性生成序（防预算下结果不可复现）。
        const MAX_CORRECTION_ATTEMPTS_PER_CATEGORY: usize = 3;

        let category_variants: Vec<Vec<String>> = vec![
            if self.keyboard_correction_enabled {
                generate_corrections(input)
            } else {
                Vec::new()
            },
            if self.phonetic_correction_enabled {
                generate_phonetic_corrections(input)
            } else {
                Vec::new()
            },
            if self.swap_correction_enabled {
                generate_swap_corrections(input)
            } else {
                Vec::new()
            },
        ];
        for variants in category_variants {
            let before = corrections.len() + promotions.len();
            let mut attempts: usize = 0;
            self.collect_correction_variants(
                &variants,
                &mut existing,
                &mut corrections,
                &mut promotions,
                max_corrections,
                &mut attempts,
                MAX_CORRECTION_ATTEMPTS_PER_CATEGORY,
            );
            // 短路：本类别已有产出 → 不再尝试后续类别（单成因原则）
            if corrections.len() + promotions.len() > before {
                break;
            }
        }

        // 纠错候选插入到列表前部（紧随第一个正常候选之后）。
        // 若 extend 到末尾，会被大量部分匹配候选淹没（如 nihso 产生 60 个
        // ni 单字候选，纠错候选「你好」会排到第 57 位用户看不到）。
        // 插入策略：保留第 0 位（最优正常匹配），纠错候选紧随其后。
        // 提升清单同位：已在列表的纠错目标标橙后移到同位（恰在第 0 位的
        // 只标橙不移位——它本就是最优正常匹配）。
        if !corrections.is_empty() || !promotions.is_empty() {
            let mut promoted: Vec<Candidate> = Vec::new();
            for t in &promotions {
                // 只动非绿词条目：绿句（is_sentence）已居前排，标橙无增益
                // 且破坏绿词学习语义。
                if let Some(pos) = candidates
                    .iter()
                    .position(|c| c.text.as_ref() == t && !c.is_sentence)
                {
                    if pos == 0 {
                        candidates[0].is_correction = true;
                    } else {
                        let mut c = candidates.remove(pos);
                        c.is_correction = true;
                        promoted.push(c);
                    }
                }
            }
            let insert_pos = candidates.len().min(1);
            let mut merged = corrections;
            merged.extend(promoted);
            candidates.splice(insert_pos..insert_pos, merged);
        }
    }

    /// 单一类别变形的纠错候选收集。
    ///
    /// 过滤链（完整约束）：非绿词 / 词频地板 / 精确拼音匹配
    /// （候选拼音去空格 == 变体串，堵 egnqi→恩情 前缀跳跃）/ 多字词。
    /// `attempts` 为本类别的尝试计数（共享计数会被前类
    /// 无效变体烧光，后类永不执行——weishemne→为什么 实测饿死）。
    /// 纠错目标已在自然列表时不静默跳过——记入 `promotions`（标橙+提升），
    /// 计入 max_corrections 上限。
    // 变体收集的完整过滤链上下文（去重/结果/晋升/上限/尝试计数均由调用方持有）
    #[allow(clippy::too_many_arguments)]
    fn collect_correction_variants(
        &self,
        variants: &[String],
        existing: &mut std::collections::HashSet<String>,
        corrections: &mut Vec<Candidate>,
        promotions: &mut Vec<String>,
        max_corrections: usize,
        attempts: &mut usize,
        max_attempts: usize,
    ) {
        for variant in variants {
            if corrections.len() + promotions.len() >= max_corrections {
                break;
            }
            // 只查变形后构成完整有效拼音的（segmentations 非空）。
            // 变形后仍无效的（如 nihsou→nihou 切不出全拼音）跳过，
            // 避免引入低质量的部分匹配候选。
            if crate::pinyin::segmentations(variant, 1).is_empty() {
                continue;
            }
            *attempts += 1;
            if *attempts > max_attempts {
                break;
            }
            // 纠错只基于词库确定词推断，不自行制造组合词（那是绿词的功能）。
            // 用 process_inner 查询（含前缀匹配、连续前缀词等完整路径），下方
            // 精确匹配过滤保证只收变体的精确命中。
            let mut sub = self.process_inner(variant, false);
            for c in sub.drain(..) {
                // 跳过绿词——纠错不该制造组合词，那是绿词的职责
                if c.is_sentence {
                    continue;
                }
                // 词频地板：纠错候选必须是足够常见的词
                //（拿他 freq=100 类冷僻词不配占黄金位），见 CORRECTION_MIN_WORD_FREQ。
                if c.base_freq < Self::CORRECTION_MIN_WORD_FREQ {
                    continue;
                }
                // 精确拼音匹配：候选的拼音
                // （去空格）必须与变形串完全相等——process_inner 含前缀匹配，
                // "enqi" 查询会捞出拼音为 "en qing" 的「恩情」（en qi ⊂ en qing
                // 前缀跳跃），等于"删字母+音节截断"两步复合的连续错误，音合不上。
                // 合法纠错（nihsou 交换 h,s→nishou=泥守 "ni shou"；nuihao 删 u→
                // nihao=你好 "ni hao"）都是精确相等。
                if c.pinyin.split_whitespace().collect::<String>() != *variant {
                    continue;
                }
                if corrections.len() + promotions.len() >= max_corrections {
                    break;
                }
                // 只接受多字词纠错候选。单字纠错几乎无用——用户输入了多音节串，
                // 返回单字是错的（如 pingzeng→bingzeng→并 没意义）。多字词
                //（如 pingzeng→pingzheng→平整）才是用户真正想纠错到的目标。
                //（注：折叠纠错路径 inject_collapse_corrections 不受此限——折叠
                // 目标常为单音节单字，如 shenng→生。）
                if c.text.chars().count() < 2 {
                    continue;
                }
                if existing.contains(c.text.as_ref()) {
                    // 已在自然列表：不重复入列，记为提升（标橙+移到纠错位）。
                    if !promotions.iter().any(|t| t == c.text.as_ref()) {
                        promotions.push(c.text.as_ref().to_owned());
                    }
                    continue;
                }
                existing.insert(c.text.as_ref().to_owned());
                let mut correction = c;
                correction.is_correction = true;
                corrections.push(correction);
            }
        }
    }

    /// 双写字母折叠纠错注入。
    ///
    /// 在 maybe_correct 的「合法简拼混合」早退门前调用：合法拼音里相邻重复字母
    /// 只出现在跨音节边界（changgan 的 gg），而完整全拼输入已在第一道门返回——
    /// 走到这里的合法简拼若含相邻重复字母，唯一合理解释就是多打了一个字母。
    /// 折叠变体必须能完整切成全拼音节才注入（shenng→sheng ✓；含分隔符输入
    /// 不进本路径）。
    ///
    /// 与主纠错路径的区别：单字候选放行——折叠目标常为单音节（sheng→生），
    /// 用户要的就是那个字；主路径拒绝单字是针对 pingzeng→并 那种"多音节串
    /// 纠成单字"的无意义场景，不适用于此。数量上限 3（折叠变体极少超过 1 个
    /// 重复位点，3 已冗余）。
    /// 上下文证据发音营救。
    ///
    /// 与 maybe_correct 的互补关系：maybe_correct 只救结构非法输入（合法输入
    /// 不纠——设计边界），本方法把发音纠错扩展到合法输入，但门槛是上下文
    /// 证据（原则："只有在明确用户可能存在发音错误的情况下才纠，哪怕没开
    /// 模糊音；缺乏 bigram 数据或既有词则严格按用户设定的模糊音规则来"）：
    ///
    /// - 词级分支（有已上屏上文）：变体音节串精确命中词库词 w，且
    /// link(prev, w) ≥ MIN_LINK（bigram/collocation 强证据）→ w 以橙色
    /// 纠错候选插入最佳自然候选之后。例：上文「…的」+ 输入 zongguo →
    /// (的→中国 32) 强 → 中国[纠]。
    /// - 句级分支（句内链证据）：变体音节串整句解码的词间链证据显著
    /// 强于字面解码 → 变体整句以橙色纠错插入。例：zongguoren 字面
    /// [宗][国][人] 链证据 ~0 vs 变体 [中国人] 带 (中国→人 12)。
    ///
    /// 证据模型刻意不含原始词频——freq 是模糊音的判据（两边都合法、
    /// 比谁常见），本功能的判据是搭配链（bigram/collocation），二者分工
    /// 才满足"无证据则严格按用户模糊音设置"的要求。
    ///
    /// 模糊音互斥：用户已开启的对子（expand_syllable 已覆盖该变体）跳过
    /// ——查询扩展已让变体作为正常候选出现，营救是重复。
    fn context_phonetic_rescue(&self, best_syllables: &[&str], candidates: &mut Vec<Candidate>) {
        // 门控：发音营救归「发音混淆纠错」子开关
        // 统管——不拆独立开关（拆分依据是引擎内部触发条件，用户不可见）。
        if !self.smart_correction_enabled || !self.phonetic_correction_enabled {
            return;
        }
        // 营救同属纠错橙词，必须尊重用户的纠错候选上限——
        // 0 = 用户明确不要纠错补充（此前营救硬编码 3 条不受上限约束，
        // 与「为 0 表示不补充纠错候选」的页脚承诺矛盾）。
        if self.max_corrections == 0 {
            return;
        }
        let rescue_cap = self.max_corrections.min(3);
        let n = best_syllables.len();
        if !(2..=10).contains(&n) {
            return;
        }
        // 变体探索预算（性能：每个变体一次 decode_sentence，封顶 6 次）
        let mut dp_budget = 6usize;
        let mut rescued: Vec<Candidate> = Vec::new();
        let existing: std::collections::HashSet<String> = candidates
            .iter()
            .map(|c| c.text.as_ref().to_owned())
            .collect();

        for i in 0..n {
            if rescued.len() >= rescue_cap {
                break;
            }
            let orig = best_syllables[i];
            for var in phonetic_syllable_variants(orig) {
                // 模糊音互斥：该对子已被用户设置覆盖 → 查询扩展已处理
                if self.fuzzy_config.enabled
                    && self.fuzzy_config.expand_syllable(orig).contains(&var)
                {
                    continue;
                }
                let mut seq: Vec<String> = best_syllables
                    .to_vec()
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                seq[i] = var.clone();
                let seq_refs: Vec<&str> = seq.iter().map(|s| s.as_str()).collect();

                // ── 词级分支：变体串精确命中词库词 + （上下文强证据 或 既有词证据）──
                // 既有词证据（用户原话"bigram 数据或者既有词"）：字面音节串在
                // 同跨度没有任何整词（DP 只能拼单字垃圾如 总国人），而变体
                // 命中词库整词（中国/中国人）——zongguo→中国、zongguoren→中国人。
                // 字面有整词（zonghe→综合）则此证据不成立，严格按模糊音设置。
                let literal_has_exact_word = candidates
                    .iter()
                    .any(|c| !c.is_sentence && !c.is_correction && c.text.chars().count() == n);
                for w in self.dict.query(&seq_refs) {
                    if w.text.chars().count() == n
                        && w.base_freq >= Self::CORRECTION_MIN_WORD_FREQ
                        && !existing.contains(w.text.as_ref())
                        && !rescued.iter().any(|c| c.text == w.text)
                    {
                        let ctx_strong = self
                            .context
                            .last()
                            .map(|prev| {
                                self.link_evidence(prev, &w.text, &w.pinyin)
                                    >= Self::CONTEXT_RESCUE_MIN_LINK
                            })
                            .unwrap_or(false);
                        if ctx_strong || !literal_has_exact_word {
                            let mut c = w;
                            c.is_correction = true;
                            rescued.push(c);
                        }
                    }
                }

                // ── 句级分支：整句解码的链证据对比（字面 vs 变体）──
                if dp_budget == 0 {
                    continue;
                }
                dp_budget -= 1;
                let joined: String = seq.concat();
                let var_sents = self.decode_sentence(&joined, &seq_refs, false, false);
                if let Some(var_best) = var_sents.into_iter().next() {
                    if existing.contains(var_best.text.as_ref())
                        || rescued.iter().any(|c| c.text == var_best.text)
                        || var_best.text.chars().count() != n
                    {
                        continue;
                    }
                    let var_words = self.split_text_anchored(&var_best.text, &seq_refs);
                    if var_words.len() < 2 {
                        continue;
                    }
                    let var_ev = self.chain_evidence(&var_words);
                    // 字面链证据：与变体同位置的词序列对比（用当前最优字面句）
                    let lit_ev = candidates
                        .iter()
                        .filter(|c| c.is_sentence && c.text.chars().count() == n)
                        .map(|c| {
                            let ws = self.split_text_anchored(&c.text, best_syllables);
                            if ws.len() < 2 {
                                0
                            } else {
                                self.chain_evidence(&ws)
                            }
                        })
                        .max()
                        .unwrap_or(0);
                    // 门槛：变体链证据达标 且 显著超过字面（GAP 防止弱翻转）
                    if var_ev >= Self::CONTEXT_RESCUE_MIN_LINK
                        && var_ev >= lit_ev + Self::CONTEXT_RESCUE_GAP
                    {
                        let mut c = var_best;
                        c.is_sentence = false; // 橙色纠错身份（猜测），非绿词
                        c.is_correction = true;
                        rescued.push(c);
                    }
                }
            }
        }

        if !rescued.is_empty() {
            let insert_pos = candidates.len().min(1);
            candidates.splice(insert_pos..insert_pos, rescued);
        }
    }

    /// 搭配链证据：词序列相邻对的 bigram（用户优先+种子回退）+ collocation
    /// 命中（词库权威搭配计满分权重）。句首衔接已上屏上文（若有）也计入
    /// ——两类证据源正是"bigram 数据"与"既有词"。
    fn chain_evidence(&self, words: &[(String, String)]) -> u32 {
        let mut total = 0u32;
        let mut prev: Option<&str> = self.context.last().map(|s| s.as_str());
        for (w, py) in words {
            if let Some(p) = prev {
                total += self.link_evidence(p, w, py);
            }
            prev = Some(w.as_str());
        }
        total
    }

    /// 单条搭配证据：collocation 命中（词库权威，硬规则）计 LINK 满格；
    /// 否则 bigram count（用户表优先 → 种子回退，equiv 泛化语义同查询侧）。
    fn link_evidence(&self, prev: &str, text: &str, pinyin: &str) -> u32 {
        if self.dict.collocation_freq(prev, text, pinyin).is_some() {
            return Self::CONTEXT_RESCUE_MIN_LINK * 4;
        }
        self.freq.word_bigram_count(prev, text)
    }

    fn inject_collapse_corrections(&self, input: &str, candidates: &mut Vec<Candidate>) {
        if !self.smart_correction_enabled || !self.keyboard_correction_enabled {
            return;
        }
        // 双击故障时序门控：前端传过时序（timing_aware）
        // 后，只有故障级快双击（间隔 < DOUBLE_FAULT_MS）才折叠；正常节奏的
        // 重复字符是刻意输入（shen+n+g 类简拼），不折叠。未传时序的平台
        // （旧前端）保持旧行为——恒可折叠，零回归。
        if self.timing_aware.get() && !self.double_fault.get() {
            return;
        }
        let variants = generate_collapse_variants(input);
        if variants.is_empty() {
            return;
        }
        let mut corrections: Vec<Candidate> = Vec::new();
        // 数量上限尊重用户的 maxCorrections 设置，
        // 同时保留绝对上限 3——折叠变体极少超过 1 个重复位点，3 已冗余。
        // 上限 0 必须真关闭（旧实现 push 后才查 cap，cap=0 仍漏
        // 1 条——与「为 0 表示不补充纠错候选」的页脚承诺矛盾）。
        let collapse_cap = self.max_corrections.min(3);
        if collapse_cap == 0 {
            return;
        }
        'outer: for variant in variants {
            // 只接受折叠后构成完整有效拼音的变体（与主纠错路径同一有效性标准）
            if crate::pinyin::segmentations(&variant, 1).is_empty() {
                continue;
            }
            let mut sub = self.process_inner(&variant, false);
            for c in sub.drain(..) {
                if c.is_sentence {
                    continue; // 纠错不制造组合词
                }
                // 词频地板（同主纠错路径，见 CORRECTION_MIN_WORD_FREQ）
                if c.base_freq < Self::CORRECTION_MIN_WORD_FREQ {
                    continue;
                }
                // 精确拼音匹配（同主纠错路径：堵死 enqi→恩情 类前缀跳跃）
                if c.pinyin.split_whitespace().collect::<String>() != variant {
                    continue;
                }
                if corrections.iter().any(|x| x.text == c.text) {
                    continue;
                }
                let mut correction = c;
                correction.is_correction = true;
                corrections.push(correction);
                if corrections.len() >= collapse_cap {
                    break 'outer;
                }
            }
        }
        if !corrections.is_empty() {
            // 提升语义（与主纠错路径的新增语义不同）：折叠目标常已深埋在候选
            // 列表（实测 shenng 的「生」在第 56 位——部分匹配路径捞到的），原位
            // 保留等于没有纠错。从原位移除后插到最佳自然候选之后（min(1)，
            // 与主纠错路径同位；yingg 案例教训：不插第 0 位
            // ——yingg 的自然解读「应该」（2 单位完整匹配）被折叠候选 应/英/影
            // 抢前，过度修正。折叠是"多打一个字母"的备选假设，让位给最强自然
            // 解读：shenng → 神女过[句]#1、生#2；yingg → 应该#1、应#2）。
            let texts: std::collections::HashSet<&str> =
                corrections.iter().map(|c| c.text.as_ref()).collect();
            candidates.retain(|c| !texts.contains(c.text.as_ref()));
            let insert_pos = candidates.len().min(1);
            candidates.splice(insert_pos..insert_pos, corrections);
        }
    }

    /// 简拼查询公开入口（启用纠错的薄包装）。
    ///
    /// 当前无调用方：`process` 内部对简拼直接调 `process_jianpin_inner(input, true)`，
    /// 不经过本包装。保留为语义清晰的公开入口，供未来外部直接发起简拼查询时使用。
    #[allow(dead_code)]
    fn process_jianpin(&self, input: &str) -> Vec<Candidate> {
        self.process_jianpin_inner(input, true)
    }

    /// 简拼输入的声母个数（zh/ch/sh 贪心合并为一个声母）。
    ///
    /// "sh"→1（一个声母）、"wm"→2、"shm"→2（sh+m）、"jsr"→3。
    /// 用于简拼完全匹配加分的双解释判定（见 process_jianpin_inner 排序）。
    /// 防御：遇到非声母字母（理论上 is_jianpin 门控不会放行）返回字母数兜底。
    fn count_jianpin_initials(input: &str) -> usize {
        const DOUBLE: [&str; 3] = ["zh", "ch", "sh"];
        const SINGLE: &str = "bpmfdtnlgkhjqxrzcsyw";
        let s: Vec<char> = input.chars().filter(|&c| c != '\'').collect();
        let mut count = 0usize;
        let mut i = 0usize;
        while i < s.len() {
            if i + 1 < s.len() {
                let pair: String = s[i..i + 2].iter().collect();
                if DOUBLE.contains(&pair.as_str()) {
                    count += 1;
                    i += 2;
                    continue;
                }
            }
            if !SINGLE.contains(s[i]) {
                return s.len(); // 非声母字母：兜底字母数（保持旧行为）
            }
            count += 1;
            i += 1;
        }
        count.max(1)
    }

    /// 简拼查询内部实现，`correct` 控制是否触发纠错（避免递归时无限循环）
    fn process_jianpin_inner(&self, input: &str, correct: bool) -> Vec<Candidate> {
        let mut result = self.query_jianpin_fuzzy(input);
        if input.chars().count() == 1 {
            result.extend(self.query_single_chars_fuzzy(input));
        }

        // 简拼匹配数：首字母个数（去掉分隔符）
        let jp_len = input.chars().filter(|&c| c != '\'').count();
        // 声母感知精确计数：
        // 双字母声母（zh/ch/sh）输入下，单字（"是"=1 字）永远不等于字母数（sh=2），
        // 结构性拿不到下面的完全匹配加分，被全体双字词（"社会"=2字==2字母，
        // +30000 满分）碾压——实测输入 sh 时「是」排第 101 位。
        // 修复：双解释都算精确——字数==字母数（[s,h] 两首字母解释，社会）
        // 或 字数==声母数（[sh] 一声母解释，是）均可获加分；同为加分后由
        // candidate_score（频率主导）分胜负（是 9950 > 社会 6278）。
        // 单字母输入两种计数恒等，行为零变化。
        let jp_initials = Self::count_jianpin_initials(input);

        // 简拼绿词 DP：≥4 首字母时触发。
        // 不对 3 首字母触发（如 zgr）——子树太大性能爆炸（zgr 全 Initial 收集数百候选），
        // 且 3 首字母通常已有白词精确命中，绿词价值低。4+ 首字母的绿词拼装价值更高。
        //
        // 上限 36：
        // DP 是简拼路径唯一随长度线性增长的环节（实测 ~1.4ms/字母：16 字母
        // ≈ 24ms、200 字母 ≈ 281ms）。服务是单线程事件循环，长叠加会冻结
        // 全机打字——护栏保留但按真实需求抬高；36 覆盖 30+ 字
        // 长句（如 zgrmjfjxa 及重复长串），超限只是跳过整句猜测：
        // Trie 查词与单字补充照常返回（绝不出现"候选框全空"的断词悬崖）。
        const MAX_GREEN_CHARS: usize = 36;
        if (4..=MAX_GREEN_CHARS).contains(&jp_len) {
            // 把简拼拆成单字符"音节"序列（如 "jsr" → ["j", "s", "r"]）
            let jp_syllables: Vec<&str> = input
                .char_indices()
                .filter(|(_, c)| *c != '\'')
                .map(|(i, _)| &input[i..i + 1])
                .collect();
            let green = self.decode_sentence_collect(input, &jp_syllables, true, false);
            // 绿词插入：去重后加到 result 末尾（排序时 candidate_score 会处理位置）。
            //
            // removed_texts 黑名单检查：
            // 命中黑名单的绿词直接跳过（不进候选列表）。与全拼路径绿词的
            // "降级排末尾"语义略有差异——简拼绿词都是 DP 临时拼接的变体（如"无我无我"），
            // 用户删了说明根本不想要这种拼凑，不需要保留"翻页复活"能力（不像全拼绿词
            // "明天下午一起去看电影"那种用户可能想恢复的整句）。
            //
            // 历史问题：早期这里没有 removed_texts 检查，导致 Command+退格删除
            // "无我无我"后，下次 wwww 重新 DP 又拼出同样的"无我无我"绕过黑名单。
            // 根因是全拼路径和简拼路径行为不一致——所有删除测试都用
            // 全拼输入（tests_correction.rs），简拼路径是测试盲区。
            for g in green {
                if self.removed_texts.contains(g.text.as_ref()) {
                    continue; // 用户删过的简拼绿词，不再生成
                }
                if !result.iter().any(|c| c.text == g.text) {
                    result.push(g);
                }
            }
        }

        // 为第一个声母补充单字候选，方便用户逐字自造词。只补充第一个声母，
        // 避免用户误选后续音节的字。
        //
        // 排序前注入：旧版在排序去重之后
        // 裸 append，单字不参与完全匹配加分与频率排序——"sh" 输入下「是」排第
        // 101 位（被全体双字词碾压）。移到排序前，配合上面的双解释精确加分
        // （字数==声母数），"sh" 下 是(1字==1声母 sh) 与 社会(2字==2字母 s,h)
        // 同获加分后由频率分胜负（是 9950 > 社会 6278）。
        // 取字用复合声母前缀（"sh"→shi/shuo 系而非整个 s 系），命中用户意图。
        if input.chars().count() > 1 {
            let existing: alloc::collections::BTreeSet<String> =
                result.iter().map(|c| c.text.as_ref().to_owned()).collect();
            let head: String = input.chars().take(2).collect();
            let first_initial = if head == "zh" || head == "ch" || head == "sh" {
                head
            } else {
                input
                    .chars()
                    .next()
                    .map(|c| c.to_string())
                    .unwrap_or_default()
            };
            if !first_initial.is_empty() {
                for single in self.query_single_chars_fuzzy(&first_initial) {
                    if !existing.contains(single.text.as_ref()) {
                        result.push(single);
                    }
                }
            }
        }

        // 「词库整词优先于绿词」裁决预计算：
        // 纯简拼下词库已有覆盖全部输入的整词（如 mldhz → 木兰当户织）时，DP 拼接
        // 的绿词（如"买了顿或者"）默认让位——仅当绿词有显著证据才允许排在整词前：
        // ① 用户学过：input→绿词映射（简拼直选过）或 freq.counts 命中（选过该词）；
        // ② 上下文关联更强：与上文的搭配（bigram/collocation/trigram，与
        // candidate_score 同口径的 context_bonus）非零且严格强于所有整词。
        // 仅当列表同时含两类候选才启用（其余查询零开销）；绿词裁决一次预计算，
        // 比较器里只查表（避免在 sort 内做 hash/SQL 重活）。
        let full_cover_word = |c: &Candidate| {
            !c.is_sentence
                && (c.text.chars().count() == jp_len || c.text.chars().count() == jp_initials)
        };
        let has_green = result.iter().any(|c| c.is_sentence);
        let has_full_word = result.iter().any(&full_cover_word);
        let green_wins: std::collections::HashMap<String, bool> = if has_green && has_full_word {
            let ctx_of = |c: &Candidate| {
                let first = c.text.chars().next().unwrap_or(' ');
                self.context_bonus(&c.text, first, c.text.chars().count() > 1, &c.pinyin)
            };
            let max_word_ctx = result
                .iter()
                .filter(|c| full_cover_word(c))
                .map(ctx_of)
                .max()
                .unwrap_or(0);
            result
                .iter()
                .filter(|c| c.is_sentence)
                .map(|c| {
                    let learned = self
                        .input_mappings
                        .get(&(input.to_string(), c.text.to_string()))
                        .copied()
                        .unwrap_or(0)
                        > 0
                        || self.freq.counts.contains_key(c.text.as_ref());
                    let gctx = ctx_of(c);
                    (
                        c.text.as_ref().to_owned(),
                        learned || (gctx > 0 && gctx > max_word_ctx),
                    )
                })
                .collect()
        } else {
            std::collections::HashMap::new()
        };

        // 「映射逃生口」证据量基准：
        // 每次用户选择都会累积 exact 映射（探索性点击 / 组合逐字提交 / 正常选择
        // 都会记录），旧判据 a_mapping > 0 只看有无——用户选过 中(2)/做(1)/
        // 中国人民(3) 时三者全部晋级高层互撕，排序由频率/上下文噪声摇摆（实测
        // zgrm 在 中/做/中国人民 三种首候选间漂移、「回退成单字」）。
        // 改为证据量支配：部分覆盖候选的逃生口需其映射数严格大于所有
        // 全覆盖候选（字数==字母数或==声母数）的映射数最大值才晋级——用户压倒性
        // 选择偏向谁，谁才获得豁免。全覆盖候选本身恒在高层（不受本基准影响）。
        // 无全覆盖候选时基准为 0 → 退化为旧行为（zgr→在 类单字习惯保留）。
        let max_full_cover_mapping: u32 = result
            .iter()
            .filter(|c| {
                let n = c.text.chars().count();
                n == jp_len || n == jp_initials
            })
            .map(|c| {
                self.input_mappings
                    .get(&(input.to_string(), c.text.to_string()))
                    .copied()
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0);

        result.sort_by(|a, b| {
            let mut score_a = self.candidate_score(input, a);
            let mut score_b = self.candidate_score(input, b);
            // 完全匹配加分：候选字数等于简拼首字母数时，说明每个首字母精确对应一个字，
            // 优先级高于字数更多的词。如 agz（3个首字母）→ 爱国者（3字）优先于 爱国主义（4字）。
            //
            // 挂钩词频：
            // 旧版无差别加固定 FULL_MATCH_BONUS(30000)，导致冷僻成语（蜗行牛步 freq=1040）
            // 压过高频单字（我 freq=9950）。改为按词频缩放：match_bonus = min(30000, base_freq×8)。
            //
            // 用户选择优先：
            // 1. 用户用此简拼直接选过（exact_mapping > 0）→ 固定满分
            // 2. 用户选过该词（freq.counts 有记录，无论全拼还是简拼选的）→ 固定满分
            // 3. 其他词按 base_freq×8 缩放
            // 这样近期高频用户词在简拼下显著靠前，不被高频系统词压制。
            let a_exact = a.text.chars().count() == jp_len || a.text.chars().count() == jp_initials;
            let b_exact = b.text.chars().count() == jp_len || b.text.chars().count() == jp_initials;
            // 覆盖层级：
            // 简拼输入下，覆盖全部输入单位（字数==字母数或==声母数）的候选
            // 硬层级优先于部分覆盖的候选（如首字母附录的单字"在"只覆盖 3 单位
            // 中的 1 个）。旧版只靠加分区分，被单字的重度上下文/学习分越过
            //（实测用户库 zgr：在 bigram 73598+total 7500 压过 中国人 加分 18128）。
            // 全拼路径早有此层级（单字 tier 2/3 < 词 tier 0/1），简拼路径对齐。
            // 逃生口：用户对本输入有精确映射（学过 zgr→某单字）则同享高层——但须
            // 证据量支配（严格大于全部全覆盖候选的映射数，见上方
            // max_full_cover_mapping 预计算）：只看有无会被探索性选择累积的
            // 多路映射搅成漂移。
            let a_mapping = self
                .input_mappings
                .get(&(input.to_string(), a.text.to_string()))
                .copied()
                .unwrap_or(0);
            let b_mapping = self
                .input_mappings
                .get(&(input.to_string(), b.text.to_string()))
                .copied()
                .unwrap_or(0);
            let a_high = a_exact || (a_mapping > 0 && a_mapping > max_full_cover_mapping);
            let b_high = b_exact || (b_mapping > 0 && b_mapping > max_full_cover_mapping);
            if a_high != b_high {
                return if a_high {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            // 「词库整词优先于绿词」层：
            // 覆盖全部输入的白词（词库整词）优先于绿词，除非绿词有显著证据
            // （见上方 green_wins 预计算：用户学过 / 上下文关联更强）。
            // 层级判定而非频率判定——高 freq 的绿词拼接（买了顿或者 3930×8 满额
            // 加分）不得仅凭词频压过低频词库整词（木兰当户织 50，诗词种子）。
            if has_green && has_full_word {
                let a_word = full_cover_word(a);
                let b_word = full_cover_word(b);
                if a_word != b_word {
                    let (word_is_a, green) = if a_word { (true, b) } else { (false, a) };
                    if green.is_sentence
                        && !green_wins
                            .get(green.text.as_ref())
                            .copied()
                            .unwrap_or(false)
                    {
                        return if word_is_a {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        };
                    }
                }
            }
            if a_exact {
                let a_used = self.freq.counts.contains_key(a.text.as_ref());
                if a_mapping > 0 || a_used {
                    score_a += Self::FULL_MATCH_BONUS; // 用户选过 → 固定满分
                } else {
                    score_a += (a.base_freq as u64 * Self::FULL_MATCH_BONUS_SCALE)
                        .min(Self::FULL_MATCH_BONUS);
                }
            }
            if b_exact {
                let b_used = self.freq.counts.contains_key(b.text.as_ref());
                if b_mapping > 0 || b_used {
                    score_b += Self::FULL_MATCH_BONUS; // 用户选过 → 固定满分
                } else {
                    score_b += (b.base_freq as u64 * Self::FULL_MATCH_BONUS_SCALE)
                        .min(Self::FULL_MATCH_BONUS);
                }
            }
            match score_b.cmp(&score_a) {
                Ordering::Equal => {
                    // 同分绿词按 DP 路径分降序（zgrmjfjxa 修复，与
                    // candidate_cmp 同层序）：同构绿词的加分项全同（exact 缩放按
                    // 共享的 max_freq），落到 text 字典序对汉字无语义（「性爱」
                    // U+6027 压「西安」U+897F）——DP 路径分更优的切分反而排后。
                    // 非绿词 sentence_score 恒 0（相等自动跳过）。
                    if a.sentence_score != b.sentence_score {
                        return b.sentence_score.cmp(&a.sentence_score);
                    }
                    a.pinyin.cmp(&b.pinyin).then_with(|| a.text.cmp(&b.text))
                }
                other => other,
            }
        });
        result.dedup_by(|a, b| a.text == b.text);
        // 单字附录已移至排序前（见上方注入块），此处不再裸 append。
        // 简拼场景也触发智能纠错（简拼也可能打错首字母）
        if correct {
            self.maybe_correct(input, &mut result);
        }
        result
    }

    /// 清空 bigram 上下文栈（句子边界/切换输入法时调用）。
    ///
    /// 兼容入口，已委托 `context_clear`：
    /// 本方法与 [`Self::context_clear`] 是同一语义的两个入口——后者是完整版
    /// （额外重置会话级 n-gram 增量上限、清撤销快照、计
    /// `context_clears`），前者是极简入口（只清栈）。服务 IPC `clear_context`
    /// 与旧 FFI 调用统一收口到完整版：栈清理之外，会话增量上限一并重置、
    /// 观测计数正常累计。
    ///
    /// 禁止在此再写第二份实现——语义只有一个来源：`context_clear`。
    pub fn clear_context(&mut self) {
        self.context_clear();
    }

    /// 上下文加分公共函数（candidate_score 和 DP 共用，消除两层公式不一致）。
    ///
    /// 遍历 `context` 栈（最多 `CONTEXT_HISTORY_SIZE`=30 个，按 prev 词去重后），
    /// 对每个上屏词查 bigram 共现次数，按距离指数衰减（`BIGRAM_DECAY^distance`）
    /// 累加对数压缩后的贡献：`Σ log2(min(cnt, dyn_cap) + 1) × 0.85^distance × BIGRAM_BONUS_K`。
    /// 去重规则：同一词只按最近一次（distance 最小）计入，避免教学轮次
    /// 残留的重复词对被弱衰减累加多份、反超 distance-0 的紧邻强关联。
    ///
    /// 双轨查询（bigram 表已拆为 word + char 两张独立表）：
    /// 1. 先查词级 `word_bigram_count(prev, text)`（含 equiv_prev 泛化）
    /// 2. 词级为 0 且候选是多字词（`is_multichar`）→ 回退字级 `char_bigram_count(prev, char_first)`
    ///
    /// trigram 升级：distance==0（紧邻上文 prev1）时，
    /// 若 ctx.len>=2（有 prev2），查 trigram(prev2, prev1, text)，与 bigram 取 max
    /// （不叠加，避免双重计数）。捕捉"prev2·prev1·text"三词框架。
    /// trigram 不做 equiv_prev 泛化（三阶稀疏性高，泛化放大噪声）。
    ///
    /// 与原 scoring.rs 的内联实现数学等价（log2(cap+1) × 0.85^distance × 3000 求和），
    /// 只是抽成公共函数供 candidate_score 调用；DP（sentence.rs）通过 context_bonus_with_ctx 接入。
    pub(crate) fn context_bonus(
        &self,
        text: &str,
        char_first: char,
        is_multichar: bool,
        curr_pinyin: &str,
    ) -> u64 {
        self.context_bonus_with_ctx(&self.context, text, char_first, is_multichar, curr_pinyin)
    }

    /// trigram 查询。
    ///
    /// 用户内存表（TrigramModel，commit 积累）命中（>0）直接返回——用户行为是硬规则，
    /// 完全不看种子（与 word_bigram_count 的用户优先一致）。
    /// 用户表未命中才按需查 seed_trigram_counts 表（SQL prepare_cached + 索引查找
    /// 约 2-5µs）。种子不启动加载的原因见 Storage::seed_trigram_count 注释。
    /// 无 storage（内置词库引擎）时种子部分返回 0。
    pub(crate) fn trigram_count_with_seed(&self, prev2: &str, prev1: &str, text: &str) -> u32 {
        let user = self.trigram.count(prev2, prev1, text);
        if user > 0 {
            return user;
        }
        let Some(storage) = self.storage.as_ref() else {
            return 0;
        };
        // 桶级会话缓存：冷启动用户 trigram 表为空时，
        // 候选打分（每候选 1 次）+ DP 转移（无条件）+ LM 先验（每转移再查）
        // 三处调用点每按键合计 400-600 次单条 SQL，与输入长度线性增长。
        // 同一按键的 (prev2, prev1) 组合通常 ≤2-3 个——以组合为 key 一次拉整桶
        // （seed_trigram_bucket，走 idx_seed_trigram_prev 索引），之后同桶内存
        // O(1) 查。空桶同样缓存（防止重复 SQL）。策略与 trigram_total_cache 同款：
        // 512 桶满即整体 clear（clear 后按需重建，无正确性影响）。
        let key = (prev2.to_string(), prev1.to_string());
        if let Some(bucket) = self.trigram_bucket_cache.borrow().get(&key) {
            return bucket.get(text).copied().unwrap_or(0);
        }
        let bucket = storage.seed_trigram_bucket(prev2, prev1);
        let value = bucket.get(text).copied().unwrap_or(0);
        let mut cache = self.trigram_bucket_cache.borrow_mut();
        if cache.len() >= 512 {
            cache.clear();
        }
        cache.insert(key, bucket);
        value
    }

    /// context_bonus 的内部实现，接受外部 context 切片（candidate_score 传 self.context）。
    ///
    /// 量级说明：返回 u64（log2×decay × BIGRAM_BONUS_K=3000 量级，可能上千），
    /// 供 candidate_score 直接累加到 u64 总分。
    /// 注：DP 路径的统计先验已由插值 LM（`lm_prior_score`，lm.rs）替代
    ///，原 context_bonus_dp 已删除；本函数仅供 candidate_score 使用。
    pub(crate) fn context_bonus_with_ctx(
        &self,
        ctx: &[String],
        text: &str,
        _char_first: char,
        _is_multichar: bool,
        curr_pinyin: &str,
    ) -> u64 {
        if ctx.is_empty() {
            return 0;
        }
        let ctx_len = ctx.len();
        let mut weighted_sum: f64 = 0.0;
        // prev 去重：同一上屏词只按最近一次（从栈尾遍历即 distance 最小）
        // 计入一次。教学轮次会在栈里留下重复词对（利益,放弃,利益,放弃,轮胎），
        // 0.85 距离衰减太弱，同一历史证据被累加多份，陈旧关联总和会反超 distance-0
        // 的紧邻新关联（轮胎→放气 被 利益→放弃×2 翻盘的真实 bug）。
        // 用 Vec 线性查（栈 ≤ CONTEXT_HISTORY_SIZE=30 元素）避免每候选一次 HashSet 分配。
        let mut seen: Vec<&str> = Vec::with_capacity(ctx_len.min(Self::CONTEXT_HISTORY_SIZE));
        // 从最近的上屏词开始（ctx 尾），往前遍历，衰减权重
        for (distance, prev) in ctx.iter().rev().enumerate() {
            if distance >= Self::CONTEXT_HISTORY_SIZE {
                break;
            }
            let weight = Self::BIGRAM_DECAY.powi(distance as i32);
            if weight < 0.01 {
                break; // 权重过低，不值得查
            }
            if seen.contains(&prev.as_str()) {
                continue; // 该词已在更近的位置计过一次
            }
            seen.push(prev.as_str());
            // 种子证据门：候选层与长程 DP 用同一套
            // 采信规则——用户亲自打过的搭配任何计数都采信；种子侧要求
            // 多字内容词 + 计数 ≥8（≈原始 5 次），挡掉「公司→除法=5」
            // 「在→出发=12」这类句法/跨义项噪声。词库组合词兜底不受此限
            // （它是词典级权威，不是语料共现）。
            let user_cnt = self.freq.user_word_bigram_count(prev, text);
            let mut cnt = if user_cnt > 0 {
                user_cnt
            } else {
                let seed = self.freq.word_bigram_count(prev, text);
                if prev.chars().count() >= 2 && seed >= ambiguity::LONG_RANGE_SEED_MIN_COUNT {
                    seed
                } else {
                    0
                }
            };
            if cnt == 0 {
                // 词级未命中 → 按需查组合词（上文+候选拼成的词是否在词库）。
                // 思路：commit「你」后输入「好」，查「你好」freq 给「好」加分。
                // 替代原 char_bigram 预存表——零启动开销、零内存、按需查询更准。
                // freq 对数压缩到 1..=8（与 word_bigram 量级对齐，不抢用户数据话语权）
                //
                // 单字成词抑制：若上一个单字 commit 已与前字组词（如 维+保=维保），
                // suppress_collocation_after_word 标记为 true，此轮查询跳过 collocation_freq
                //（但仍保留 word_bigram_count 上的用户/种子 bigram）。commit 下一个单字时标记重置。
                // 详见 「单字成词抑制状态机」。
                if !self.suppress_collocation_after_word {
                    if let Some(f) = self.dict.collocation_freq(prev, text, curr_pinyin) {
                        cnt = (((f as f64).log2().max(0.0) / 2.0).round() as u32).clamp(1, 8);
                    }
                }
            }
            // trigram 升级：distance==0（prev=ctx末尾=prev1）时，
            // 若 ctx.len>=2，prev2=ctx[ctx_len-2]，查 trigram(prev2, prev1, text)，
            // 与 bigram 取 max（不叠加，避免双重计数）。捕捉"prev2·prev1·text"三词框架。
            if distance == 0 && ctx_len >= 2 {
                let prev2 = &ctx[ctx_len - 2];
                let t_cnt = self.trigram_count_with_seed(prev2, prev, text);
                if t_cnt > cnt {
                    cnt = t_cnt;
                }
            }
            // 自适应 bigram cap：log2 压缩 + 动态 cap。
            let capped = cnt.min(self.dyn_cap_cache);
            weighted_sum += (capped + 1).ilog2() as f64 * weight;
        }
        (weighted_sum * Self::BIGRAM_BONUS_K as f64) as u64
    }

    /// 查询某 (prev, text) bigram 的来源信息。
    ///
    /// 双表方案下天然简单：分别查两个表各自 count，反映 word_bigram_count 实际用哪个。
    /// 返回 (user_count, seed_count, used)：
    /// - user_count：用户表 word_bigram_counts 的 count（0 表示用户没 commit 过）
    /// - seed_count：种子表 seed_bigram_counts 的 count（0 表示种子无此搭配）
    /// - used："user"（用户优先）/ "seed"（冷启动兜底）/ "none"（都无）
    ///
    /// 仅供 FFI/调试工具调用，不影响热路径。
    pub fn bigram_source_info(&self, prev: &str, text: &str) -> (u32, u32, &'static str) {
        let user = self.freq.user_bigram.get(prev, text).unwrap_or(0);
        let seed = self.freq.seed_bigram.get(prev, text).unwrap_or(0);
        let used = if user > 0 {
            "user"
        } else if seed > 0 {
            "seed"
        } else {
            "none"
        };
        (user, seed, used)
    }

    // ---- 上下文栈（生产：提交前连通性校验 / 切换应用清理）----

    /// 最近 `max` 个上屏词（旧→新）。前端在提交前读光标前文本，校验这些词
    /// 是否确实连续出现在文档中；不连续则调用 `context_clear`。
    pub fn context_tail(&self, max: usize) -> Vec<&str> {
        let n = max.min(self.context.len());
        self.context[self.context.len() - n..]
            .iter()
            .map(|s| s.as_str())
            .collect()
    }

    /// 清空上屏上下文与会话级 n-gram 增量上限。
    ///
    /// 前端在「切换应用（失活）」或「提交时发现文档上下文不连续」时调用：
    /// - 跨 app / 移动光标 / 删除错词后的下一次提交不再形成伪 bigram/trigram；
    /// - 同时重置本会话的单 pair 增量上限（用户测试循环换新会话段）。
    /// 只动内存；`dirty` 置位让 `context_snapshot` 在下次 flush 落空。
    pub fn context_clear(&mut self) {
        self.context.clear();
        // 上下文清空 = 上一次提交不再可撤销（切应用/上下文不连续）
        self.last_commit_ngrams = None;
        self.freq.reset_session_bigram_inc();
        self.trigram.reset_session_trigram_inc();
        self.suppress_collocation_after_word = false;
        if self.learning_stats_enabled {
            self.learning_stats.context_clears += 1;
        }
        self.dirty = true;
    }

    /// 回滚上一次普通提交记录的跨词 n-gram 增量。
    /// 返回是否有实际回滚；快照消费后清空（防重复撤销）。
    pub fn revert_last_commit(&mut self) -> bool {
        let Some(snap) = self.last_commit_ngrams.take() else {
            return false;
        };
        let mut any = false;
        if let Some((prev, text, w)) = snap.bigram {
            any |= self.freq.revert_word_bigram_weighted(&prev, &text, w);
        }
        if let Some((p2, p1, text, w)) = snap.trigram {
            any |= self.trigram.revert_weighted(&p2, &p1, &text, w);
        }
        if any {
            if self.learning_stats_enabled {
                self.learning_stats.last_commit_reverts += 1;
            }
            self.dirty = true;
        }
        any
    }

    /// 回滚一次指定 bigram 增量（候选内部词对由前端单独记录，纠错撤销时逐对调用）。
    pub fn revert_bigram(&mut self, prev: &str, text: &str, weight: u32) -> bool {
        let r = self.freq.revert_word_bigram_weighted(prev, text, weight);
        if r {
            if self.learning_stats_enabled {
                self.learning_stats.bigram_reverts += 1;
            }
            self.dirty = true;
        }
        r
    }

    /// 删除单个 bigram pair（审计/清理用；写 dirty delete，flush 删行）。
    pub fn forget_bigram_pair(&mut self, prev: &str, text: &str) -> bool {
        let r = self.freq.forget_bigram_pair(prev, text);
        if r {
            self.dirty = true;
        }
        r
    }

    /// 删除单个 trigram（审计/清理用；写 dirty delete，flush 删行）。
    pub fn forget_trigram_pair(&mut self, prev2: &str, prev1: &str, text: &str) -> bool {
        let r = self.trigram.forget_pair(prev2, prev1, text);
        if r {
            self.dirty = true;
        }
        r
    }

    /// 学习治理观测（会话级计数器）JSON；诊断/CLI 消费。
    pub fn learning_stats_json(&self) -> String {
        let s = &self.learning_stats;
        format!(
            "{{\"bigrams_recorded\":{},\"bigrams_skipped\":{},\"bigrams_capped\":{},\
             \"trigrams_recorded\":{},\"trigrams_skipped\":{},\"trigrams_capped\":{},\
             \"context_clears\":{},\"last_commit_reverts\":{},\"bigram_reverts\":{}}}",
            s.bigrams_recorded,
            s.bigrams_skipped,
            s.bigrams_capped,
            s.trigrams_recorded,
            s.trigrams_skipped,
            s.trigrams_capped,
            s.context_clears,
            s.last_commit_reverts,
            s.bigram_reverts
        )
    }

    /// 噪声治理观测计数开关。
    /// 宿主前端侧合成「调试日志总开关 AND 子开关」后注入；关闭时全部自增点
    /// 零开销跳过（学习行为本身不变，只停观测计数）。见字段注释。
    pub fn set_learning_stats_enabled(&mut self, on: bool) {
        self.learning_stats_enabled = on;
    }

    // ---- 上下文栈测试辅助（仅测试用，模拟上屏词设置上文）----
    #[cfg(test)]
    pub fn context_clear_for_test(&mut self) {
        self.context.clear();
    }
    /// 调试/CLI 用（离线调试工具 `homophone` 重放评测上文）：压入一个上屏词，
    /// 不触发任何学习/持久化（与 `context_push_for_test` 同实现）。
    pub fn debug_push_context(&mut self, word: &str) {
        if word.is_empty() {
            return;
        }
        self.context.push(word.to_string());
        if self.context.len() > Self::CONTEXT_HISTORY_SIZE {
            let excess = self.context.len() - Self::CONTEXT_HISTORY_SIZE;
            self.context.drain(0..excess);
        }
    }
    /// 测试钩子：切换长程语境证据（A/B 对照用；生产无此入口）。
    #[cfg(test)]
    pub fn set_longrange_enabled_for_test(on: bool) {
        sentence::set_longrange_enabled_for_test(on);
    }

    /// 测试钩子：切换双向词格重打分（A/B 对照用；生产无此入口）。
    #[cfg(test)]
    pub fn set_backward_enabled_for_test(on: bool) {
        sentence::set_backward_enabled_for_test(on);
    }

    /// 记忆条目的词组链段数（多字词段个数；无法与音节序列对齐时返回 None）。
    ///
    /// 用途：判断一条 green_memo / input_mappings
    /// 记忆条目是不是「一个高频词拼出来的怪句」（如 首钢诶里 = 首钢+诶+里，
    /// 1 个多字段），从而避免它仅凭一次误选就永久压过词组链更自然的读法
    /// （手感给力 = 手感+给力，2 个多字段）。
    pub(crate) fn memo_span_count(&self, text: &str, syllables: &[&str]) -> Option<u32> {
        let words = self.split_text_anchored(text, syllables);
        if words.is_empty() {
            return None;
        }
        let rebuilt: String = words.iter().map(|(w, _)| w.as_str()).collect();
        if rebuilt != text {
            return None; // 读音对不齐（多音/异读历史条目）→ 不做判断
        }
        Some(words.iter().filter(|(w, _)| w.chars().count() >= 2).count() as u32)
    }

    /// 句子候选的词组链段数（与 `memo_span_count` 同口径：文本按自身读音
    /// 划分后的多字词段数）。质量门两侧必须同口径：旧基准用
    /// `word_span_count` 的重叠词证据窗口（2 字词链会重复计），与条目的划分
    /// 计数不可比——词库补口语组合词（安的/干的）后基准虚高，学习条目
    /// （反感得很 1 段）被误降级。读音对不齐返回 None（不降级）。
    pub(crate) fn sentence_chain_spans(&self, c: &Candidate) -> Option<u32> {
        let syls: Vec<&str> = c.pinyin.split_whitespace().collect();
        self.memo_span_count(c.text.as_ref(), &syls)
    }

    /// 调试/CLI 用：清空上下文栈（不置 dirty、不落盘）。
    pub fn debug_clear_context(&mut self) {
        self.context.clear();
    }
    #[cfg(test)]
    pub fn context_push_for_test(&mut self, word: &str) {
        // 模拟普通提交：push + 截断（保持最多 CONTEXT_HISTORY_SIZE 个元素）
        if !word.is_empty() {
            self.context.push(word.to_string());
            if self.context.len() > Self::CONTEXT_HISTORY_SIZE {
                let excess = self.context.len() - Self::CONTEXT_HISTORY_SIZE;
                self.context.drain(0..excess);
            }
        }
    }
    #[cfg(test)]
    pub fn context_vec_for_test(&self) -> Vec<String> {
        self.context.clone()
    }
    #[cfg(test)]
    pub fn user_words_contains_for_test(&self, text: &str) -> bool {
        self.user_words.contains_key(text)
    }
    #[cfg(test)]
    pub fn user_words_pinyin_for_test(&self, text: &str) -> Option<String> {
        self.user_words
            .get(text)
            .and_then(|pys| pys.first().cloned())
    }
    /// 测试辅助：某用户词的全部读音。
    #[cfg(test)]
    pub fn user_words_pinyins_for_test(&self, text: &str) -> Option<Vec<String>> {
        self.user_words.get(text).cloned()
    }

    /// 测试辅助：检查某词是否已记入 freq.counts（用户选过）。系统词不写
    /// user_words，但 freq.counts 应记录选择次数。
    #[cfg(test)]
    pub fn freq_counts_contains_for_test(&self, text: &str) -> bool {
        self.freq.counts.contains_key(text)
    }

    /// 测试辅助：取某词的累计选择次数（freq.counts）。
    #[cfg(test)]
    pub fn freq_count_for_test(&self, text: &str) -> u32 {
        self.freq.counts.get(text).copied().unwrap_or(0)
    }

    #[cfg(test)]
    pub fn bigram_count_for_test(&self, prev: &str, text: &str) -> u32 {
        self.freq.word_bigram_count(prev, text)
    }
    /// 打分辅助：候选在当前上下文下的总分（examples/probe_cands 诊断用，
    /// 只读无副作用，故不设 cfg(test) 门）。
    pub fn candidate_score_for_test(&self, input: &str, c: &crate::dict::Candidate) -> u64 {
        self.candidate_score(input, c)
    }
    #[cfg(test)]
    pub fn base_dict_has_for_test(&self, text: &str) -> bool {
        self.dict.find_pinyin(text).is_some()
    }
    #[cfg(test)]
    #[cfg(test)]
    pub fn decode_sentence_for_test(&self, input: &str, syllables: &[&str]) -> Vec<Candidate> {
        self.decode_sentence(input, syllables, false, false)
    }
    #[cfg(test)]
    pub fn split_into_words_for_test(&self, syllables: &[&str]) -> Vec<(String, String)> {
        self.split_into_words(syllables)
    }
    #[cfg(test)]
    pub fn is_sentence_input_for_test(&self, text: &str, pinyin: Option<&str>) -> bool {
        self.is_sentence_input(text, pinyin)
    }
    #[cfg(test)]
    pub fn query_for_test(&self, syllables: &[&str]) -> Vec<Candidate> {
        self.dict.query(syllables)
    }
    #[cfg(test)]
    pub fn query_base_for_test(&self, syllables: &[&str]) -> Vec<Candidate> {
        self.dict.query(syllables)
    }

    /// 判断输入是否为完整拼音（所有音节均有效）
    ///
    /// 用于决定 Enter 键是提交候选还是提交原始 buffer。
    pub fn is_complete_pinyin(&self, input: &str) -> bool {
        if input.is_empty() {
            return false;
        }
        // 双拼展开 shim（spec §4.3）：双拼 buffer 展开成全拼再判完整性，下游逻辑无感。
        // scheme=None 时 no-op（unwrap_or_else 返回原值）。
        // force_fullpinyin=true（/q 临时全拼）时跳过展开——buffer 已是全拼，无需展开。
        let input = if !self.force_fullpinyin {
            if let Some(scheme) = &self.shuangpin {
                crate::shuangpin::decode_to_pinyin_string(scheme, input)
                    .unwrap_or_else(|| input.to_string())
            } else {
                input.to_string()
            }
        } else {
            input.to_string()
        };
        let input = input.as_str();
        // 简拼不算完整拼音
        if crate::pinyin::is_jianpin(input) {
            return false;
        }
        let segs = self.best_segmentation(input);
        if segs.is_empty() {
            return false;
        }
        let last = segs.last().unwrap();
        crate::pinyin::is_valid_syllable_pub(last)
    }

    /// 计算输入的拼音音节数
    ///
    /// 用于自组词：知道 buffer 有多少个音节，才能决定选字后吃掉几个音节。
    pub fn syllable_count(&self, input: &str) -> usize {
        if input.is_empty() {
            return 0;
        }
        // 双拼展开 shim（spec §4.3）：双拼 buffer 展开成全拼再算音节数。
        // scheme=None 时 no-op。force_fullpinyin=true（/q 临时全拼）时跳过——buffer 已是全拼。
        let input = if !self.force_fullpinyin {
            if let Some(scheme) = &self.shuangpin {
                crate::shuangpin::decode_to_pinyin_string(scheme, input)
                    .unwrap_or_else(|| input.to_string())
            } else {
                input.to_string()
            }
        } else {
            input.to_string()
        };
        let input = input.as_str();
        if crate::pinyin::is_jianpin(input) {
            return input.chars().count();
        }
        let segs = self.best_segmentation(input);
        if !segs.is_empty() {
            return segs.len();
        }
        // FULL_ONLY 切不出（含声母尾段的混合输入，如 guangzh）→ 与候选生成
        // 同源的 token 语义再试一次。
        let tok = self.best_token_segmentation(input);
        if !tok.is_empty() {
            return tok.len();
        }
        // 两者都切不出（如 kaix 中的 x 这类纯贪心可达的残缺），
        // 回退到 parse_pinyin 的最大正向匹配，把不完整音节也视为一个音节。
        let seq = parse_pinyin(input);
        seq.syllables.len()
    }

    /// 返回 buffer 的音节切分（用于预输入态显示 `'` 分隔符，仿微信输入法）。
    ///
    /// 切分逻辑与 `syllable_count` 完全一致（保证显示与实际处理对齐）：
    /// - 简拼（zgrm）：每字符一个音节 → `["z", "g", "r", "m"]`
    /// - 全拼（wenmin）：best_segmentation 最优切分 → `["wen", "min"]`
    /// - 含不完整音节（kaix）：parse_pinyin 回退 → `["kai", "x"]`
    /// - 单音节（xian）：整体一个音节 → `["xian"]`（不加分隔符）
    ///
    /// 宿主前端用 `'` join 后显示：`wen'min`、`z'g'r'm`。
    /// 单音节时不加分隔符（`xian` 而非 `xian`），避免干扰。
    pub fn syllable_segmentation(&self, input: &str) -> Vec<String> {
        if input.is_empty() {
            return Vec::new();
        }
        // 首选句读音覆盖：process 刚算出首选全覆盖绿词且其读音
        // 与选举切分不同（fang'an vs fan'gan 类）时，显示跟随首选（字段注释
        // 有完整论证：字母逐相等才覆盖，消费链不受影响）。输入不匹配则忽略
        //（防陈旧）。
        if let Some((inp, syls)) = &*self.display_reading_override.borrow() {
            if inp == input {
                return syls.clone();
            }
        }
        // 手动分隔符优先：' 是用户显式切分边界（s'h = s|h），
        // 先按 ' 分段、各段独立自动切分，段界原样保留（join 后撇号恰好落回
        // 用户按下的位置）。旧实现把含 ' 的 buffer 整体喂给切分器：
        // 简拼分支把 ' 当独立音节段（"s'h" → ["s","'","h"] → join 出 s'''h，
        // 撇号爆炸）；token 分支吞掉 '（"wenm'in" → wen'm'i'n，用户边界消失）。
        // 空段（连按/首尾 '）保留为空字符串段——用户按下的撇号必须在显示上
        // 有反馈（join 后该位置恰好一个撇号，如 "sh'" → "s'h'"）。
        if input.contains('\'') {
            let mut out: Vec<String> = Vec::new();
            for part in input.split('\'') {
                if part.is_empty() {
                    out.push(String::new());
                } else {
                    out.extend(self.syllable_segmentation(part));
                }
            }
            return out;
        }
        // 双拼模式：显示双拼原文，按 2 字符一组切分（不展开成全拼，见 spec §6）。
        // 返回 ["vs","go"] → FFI 用 ' join 得 "vs'go"，与全拼 ["wen","min"]→"wen'min" 路径一致。
        // scheme=None 时永不进此分支，行为不变。
        // force_fullpinyin=true（/q 临时全拼）时跳过——buffer 已是全拼，走后续全拼切分，
        // 否则 pre-edit 显示按双拼 2 字符分组与候选的全拼不一致（问题 2 根因）。
        if !self.force_fullpinyin {
            if let Some(scheme) = &self.shuangpin {
                return crate::shuangpin::segment_display(scheme, input)
                    .split('\'')
                    .map(|s| s.to_string())
                    .collect();
            }
        }
        // 简拼：每字符一个音节
        if crate::pinyin::is_jianpin(input) {
            return input.chars().map(|c| c.to_string()).collect();
        }
        // 全拼：best_segmentation 最优切分
        let segs = self.best_segmentation(input);
        if !segs.is_empty() {
            return segs.iter().map(|s| s.to_string()).collect();
        }
        // FULL_ONLY 切不出（混合输入声母尾段）→ token 语义（guangzh 显示
        // guang'zh 而非 guang'z'h，与候选消费口径一致）
        let tok = self.best_token_segmentation(input);
        if !tok.is_empty() {
            return tok.iter().map(|s| s.to_string()).collect();
        }
        // 回退：parse_pinyin 最大正向匹配（含不完整音节）
        let seq = parse_pinyin(input);
        seq.syllables
            .iter()
            .map(|s| seq.raw[s.start..s.end].to_string())
            .collect()
    }

    /// 双拼模式的全拼音节显示串（"zhong'guo"，' 分隔）：偏好「双拼时显示全拼音节」
    /// 开启时 DLL 候选窗顶行替换显示，供确认双拼按键映射与学习。
    /// 非双拼（scheme=None）/ 临时全拼（ffp：buffer 已是全拼）/ 解码失败 → None
    /// （DLL 回退原双拼码显示，行为不变）。
    pub fn shuangpin_full_display(&self, input: &str) -> Option<String> {
        if input.is_empty() || self.force_fullpinyin {
            return None;
        }
        let scheme = self.shuangpin.as_ref()?;
        let syls = crate::shuangpin::decode_buffer(scheme, input)?;
        Some(syls.join("'"))
    }

    /// 返回输入前 n 个音节所占的字节长度
    ///
    /// 自组词时使用：把 buffer 切分成「已消费部分」和「剩余部分」。
    /// n 超过实际音节数时返回整个 input 的长度。
    ///
    /// 双拼模式特殊处理（spec §4.3/§6）：返回值是宿主前端 buffer 的字节偏移，
    /// 而宿主前端 buffer 是双拼原文（如 "vsgo"），所以不能展开成全拼再算偏移
    /// （否则返回的偏移会索引到宿主前端没有的全拼串，slice 越界）。
    /// 正确做法：双拼固定每音节 2 字符，偏移 = min(n*2, input.len) 字节。
    /// 这与 segment_buffer「不展开、按 2 字符分组」一致。
    pub fn syllable_prefix_len(&self, input: &str, n: usize) -> usize {
        self.syllable_prefix_lens(input, &[n]).pop().unwrap_or(0)
    }

    /// 批量版 `syllable_prefix_len`：best_segmentation 只跑一次（
    /// Windows Process 响应每候选都要 consumed——逐个调 syllable_prefix_len
    /// 会对同一 input 重复切分，按键热路径省不起）。语义与逐个调用完全一致
    /// （双拼 2 字符/音节、is_jianpin 1 字符/音节、全拼音节偏移；n=0 → 0）。
    pub fn syllable_prefix_lens(&self, input: &str, ns: &[usize]) -> Vec<usize> {
        if input.is_empty() {
            return vec![0; ns.len()];
        }
        // 双拼模式：直接在原文上按 2 字符/音节算字节偏移（不展开，见
        // syllable_prefix_len 文档注释——宿主前端 buffer 是双拼原文）。
        // force_fullpinyin=true（/q 临时全拼）时跳过——buffer 已是全拼。
        if self.shuangpin.is_some() && !self.force_fullpinyin {
            return ns
                .iter()
                .map(|&n| {
                    input
                        .char_indices()
                        .nth(n.saturating_mul(2))
                        .map(|(idx, _)| idx)
                        .unwrap_or(input.len())
                })
                .collect();
        }
        if crate::pinyin::is_jianpin(input) {
            return ns
                .iter()
                .map(|&n| {
                    input
                        .char_indices()
                        .nth(n)
                        .map(|(idx, _)| idx)
                        .unwrap_or(input.len())
                })
                .collect();
        }
        let segs = self.best_segmentation(input);
        let syllables: Vec<&str> = if !segs.is_empty() {
            segs
        } else {
            // FULL_ONLY 切不出（混合输入声母尾段，如 guangzh）→ token 语义
            // （Shengmu(zh) 整段，与候选查询同源）——否则 [guang,z,h] 贪心
            // 让 广州 只消费 6 字节、h 残留。
            let tok = self.best_token_segmentation(input);
            if !tok.is_empty() {
                tok
            } else {
                let seq = parse_pinyin(input);
                seq.syllables
                    .iter()
                    .map(|s| &input[s.start..s.end])
                    .collect()
            }
        };
        ns.iter()
            .map(|&n| {
                if n == 0 {
                    return 0;
                }
                if n >= syllables.len() {
                    return input.len();
                }
                // syllables[n-1] 是 &str 切片，其 len 就是字节长度
                let s = syllables[n - 1];
                s.as_ptr() as usize - input.as_ptr() as usize + s.len()
            })
            .collect()
    }

    /// 每候选消耗的原始输入字节数（Windows Process 的 consumed 权威值，
    /// DLL 消耗权威分支按它吃字母——editor.cpp「消耗权威分支」）。
    ///
    /// 三条规则按序（声母残留/前缀扩展双修复 + 前缀
    /// 跨段词扩展）：
    /// ① 前缀扩展：候选拼音（去空格/分隔符）以整个输入为前缀 → 消费全部
    /// 输入。这类候选是「对未敲完字母的补全」——sh→是(shi)、guangzho→
    /// 广州(guangzhou)、yonghu→用坏(yonghuai)、guangzh→广州(guangzhou)。
    /// 候选拼音覆盖了所有已敲字母，不存在「该候选没吃掉的字母」，残留恒空。
    /// ② 其余按最优切分（FULL_ONLY → token 混合 → parse 贪心）的前 n 音节
    /// 偏移——正常剩余音节保留语义（zhonggr→中国 consumed=6 留 r；
    /// zaijianh→再见 consumed=7 留 h；jiegw→结果 consumed=4 留 w）。
    /// ③ 前缀候选按自身读音消费：候选拼音
    /// （去分隔符）是输入的前缀时，消费数 = 候选自身字母长度——
    /// 只吃该候选真正解释的字母，剩余原样留在 buffer。
    /// · bendana + 笨蛋(ben dan=6) → 6，留 a（跨段橙词，选举边界只有
    /// ben+da=5 会残留 "na" 错乱）；
    /// · jisuan + 急速(ji su=4) → 4，留 an（输入比候选长，选举边界
    /// ji+suan=6 会吞掉 an 语义——第二轮实测案例）；
    /// · 单字/等长候选与选举边界天然一致（西 xi=2、中 zhong=5）。
    /// v/ü 等正规化不对称的候选拼音不是字母前缀，自然落回规则②。
    ///
    /// ns = 候选字数。
    pub fn consumed_lens(&self, input: &str, candidates: &[crate::dict::Candidate]) -> Vec<usize> {
        let ns: Vec<usize> = candidates.iter().map(|c| c.text.chars().count()).collect();
        if input.is_empty() {
            return vec![0; ns.len()];
        }
        let base = self.syllable_prefix_lens(input, &ns);
        candidates
            .iter()
            .zip(base.iter())
            .map(|(c, b)| {
                let compact: String = c
                    .pinyin
                    .chars()
                    .filter(|ch| *ch != ' ' && *ch != '\'')
                    .collect();
                if compact.starts_with(input) {
                    input.len()
                } else if !compact.is_empty() && input.starts_with(compact.as_str()) {
                    compact.len()
                } else if c.is_correction {
                    // 橙词纠错候选全消费：
                    // 纠错假设解释的是整个输入串（变体与输入恒差编辑距离 1），
                    // 按候选拼音走前缀规则只吃到 ni'h、把 oa 留在预编辑里当简拼
                    // 尾巴——选中后残留无意义字母。选中纠错 = 用户确认了整个
                    // 错串的意图，恒全消费。
                    // 只在两个前缀规则都不命中时生效：bendana→笨蛋 类
                    // 「词+零声母语气词尾」橙词的拼音是输入真前缀（bendan ⊂
                    // bendana），尾巴 a 必须留存——由上面的规则②正确截断，
                    // 本分支不抢。
                    input.len()
                } else {
                    // ④ 简拼整词：候选拼音各音节首字母
                    // 拼接 == 输入 → 全消费（每字母一字，s's'e'f 四段全吃）。
                    let initials: String = c
                        .pinyin
                        .split_whitespace()
                        .filter_map(|s| s.chars().next())
                        .collect();
                    if initials.len() == input.len()
                        && initials.eq_ignore_ascii_case(input)
                    {
                        input.len()
                    } else {
                        *b
                    }
                }
            })
            .collect()
    }
}
