// x1core — 拼音输入法核心引擎
//
// 设计原则：
// 1. 热路径低分配：查询/切分以借用切片为主，仅在候选物化与 JSON 序列化时分配
//    （候选数、绿词、缓存均有硬上限）
// 2. 纯本地：无网络、无云、无 AI
// 3. 可预测的性能：O(n) 或 O(log n) 查询；输入长度有硬护栏（pinyin::MAX_INPUT_BYTES）

// === clippy 许可清单（存量警告清零以启用 CI -D warnings）===
// 下列 lint 是有意识的设计取舍，逐条说明理由后许可（其余警告已全部修掉）：
//
// - type_complexity：引擎内部大量使用多元素元组做临时返回/缓存结构
//   （如 `Vec<(Vec<String>, Vec<String>, usize, u32, i32)>` 路径 N-best、
//   `Vec<(String, i64, i64, i64, i64, i64)>` 统计行）。提取 type alias 只增加
//   一层间接（部分含生命周期、部分只在单函数内使用），不提升可读性。
// - doc_lazy_continuation / doc_overindented_list_items：模块文档里的中文列表与
//   公式续行用的是悬挂缩进排版（`+ recent_score(text) * RECENT_BONUS` 这类
//   算式对齐），改成 rustdoc 列表语法会把公式拆散、破坏可读性；本项目的
//   canonical 文档在 `docs/*.md`（不发布 rustdoc）。若将来要发布 rustdoc，
//   逐条修正这两项后撤销许可。
#![allow(clippy::type_complexity)]
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::doc_overindented_list_items)]

extern crate alloc;

/// 测试/评测临时产物目录：一律落 `<workspace>/target/test-tmp/`，
/// 绝不落系统临时目录（系统临时盘可能被种子副本撑满，见模块文档）。
pub mod test_tmp;

pub mod char_freq; // 公开供 freq_rebuild 等维护工具使用
/// 竞争频率存储（「退化」通道）：同音/组词/整句三层竞争的
/// 选择分布，供词频进化+退化。
pub mod competition;
/// Trie 词库 + 单字表（Windows 引擎服务 crate 直连所需；
/// x1core 作为跨平台引擎库被宿主前端/引擎服务侧直接 crate 引用）
pub mod dict;
mod dict_data;
pub mod engine;
#[cfg(test)]
mod eval;
/// 整句正确率评测基线：固定语料 + Top1/Top3/字级准确率 + 错误归因 + JSON 落盘。
#[cfg(test)]
mod eval_baseline;
/// 同音歧义评测：同音节合法歧义族 + 上文语境，独立于 500 句基线。
#[cfg(test)]
mod eval_homophone;
/// 行为回放评测：真实打字记录重放，行为类旋钮的定标基准。
#[cfg(test)]
mod eval_replay;
pub mod ffi;
mod freq;
/// 词频固化提案（竞争证据 → 种子 freq 调整建议）。
pub mod freq_promote;
/// 模糊音配置（from_csv 供引擎服务配置总线复刻门控逻辑）。
pub mod fuzzy;
mod lm;
/// 只读文件映射（mmap，零依赖平台 FFI；词典缓存零拷贝装载用）。
mod mmap_file;
mod pinyin;
/// 平台差异垫片：唯一允许出现平台分支的位置——引擎与算法层
/// 必须平台无关（守护 tests_platform_purity）。
mod platform;
/// 斜杠命令族（/upper 数字转中文、/emoj 表情输入）。
pub mod quickcmds;
/// 学习型排序器权重（ranker_trainer 工具生成；全 0 = 未训练）。
pub mod ranker_weights;
/// 双拼方案（builtin_by_id/from_json 供引擎服务配置总线）。
pub mod shuangpin;
/// 输入统计核心（跨平台共享实现）：五张事实表 + 聚合查询 +
/// 重置，schema 与 JSON 契约与 Windows 侧服务实现逐字一致（统一标准）。
pub mod stats;
pub mod storage;
/// 快捷输入（符号表 + 日期时间生成，/date /fh 等前缀触发的候选）。
mod symbols;
/// Trie arena 紧凑基底（Dict 内部使用）。
mod trie_arena;
/// 三阶上下文模型（trigram）。
mod trigram;
#[cfg(test)]
mod verify_sentence_learning;

pub use dict::{Candidate, Dict};
pub use engine::Engine;
// segmentations / segmentations_with_partial / segmentations_prefix_tail：供调试
// CLI 的切分选举诊断复现引擎全拼路径的枚举链（与引擎内部同一实现，零口径漂移）。
pub use pinyin::{
    segmentations, segmentations_prefix_tail, segmentations_tokens, segmentations_with_partial,
    PinyinSeq, Syllable, Token,
};

/// 核心引擎版本（构建期从 Cargo.toml 固化，运行时暴露）。
///
/// 版本分层：引擎版本是 x1core/Cargo.toml 的单一来源，
/// 供 mac/win 前端在日志/关于页显示引擎层版本，与前端产品版本独立演进。
pub fn engine_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
