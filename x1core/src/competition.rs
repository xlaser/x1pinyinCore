//! 竞争频率存储（「退化」通道核心）。
//!
//! # 设计哲学
//! 词/关联的权重不由「绝对用了多少次」决定，而由**同音/同场景竞争**里
//! 用户的选择分布决定：
//!
//! ```text
//! freq(word | ctx) = selects(ctx, word) / total(ctx)      （∈ [0,1]，归一）
//! ```
//!
//! - **无时间参数**：真实时间（秒/天/周）完全不进公式；不用电脑/输入法
//!   期间权重**逐位冻结**（没有新竞争事件，两张表都不动）。
//! - **进化**：commit 时 `total(ctx) += 1`、`selects(ctx, word) += 1`；
//! - **退化**：未被选中的同场词分子不动，分母增大 → 频率自然稀释
//!   （零和归一，结构上不可能无限增长）。
//!
//! # 与现有表的关系
//! - **同音层（Pin）**：本模块新建表 `competition*`（多字词与单字统一走
//!   word 的完整拼音作 ctx——选「练习」无论经 lianxi/lianx/lx，都是同一场
//!   同音竞争）。不复用 user_pinyin_counts（该表只记单字、按音隔离语义，
//!   且历史数据混有清理噪声，污染语义另算）。
//! - **组词层（Bigram）**：复用 `user_bigram_counts`（分子 =
//!   count(prev,text)、分母 = Σ_text count(prev,text)）。
//! - **整句层（Input）**：复用 `user_input_mappings`（分子 =
//!   count(input,text)、分母 = Σ_text count(input,text)）。
//! 三个层的**读取**统一走 [`CompetitionStore::smoothed`]（Laplace 平滑）。

use std::collections::BTreeMap;

/// 同音层先验强度（α_pin）：前 ~α 次选择几乎回先验，之后用户证据主导。
pub const ALPHA_PIN: u32 = 4;
/// 组词层先验强度（α_bigram）：搭配证据慢变（绿词错误关联需 ~12 次才主导）。
pub const ALPHA_BIGRAM: u32 = 12;
/// 整句层先验强度（α_input）。
pub const ALPHA_INPUT: u32 = 8;
/// 上下文同音层先验强度（α_pin_ctx）：只对**同一上屏词之后的
/// 同音选择**生效。比全局同音层（α=4）更快响应——一次选择就应让该语境下的
/// 读法获得优势（用户明确在「机场」后选了「出发」，这是强意图信号）。
pub const ALPHA_PIN_CTX: u32 = 2;

/// 竞争语境键（三层共用一张表，层前缀防冲突）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CtxKey {
    /// 同音层：word 的完整拼音（"lian xi"、"de"）。
    Pin(String),
    /// 组词层：上一上屏词（"正确的"）。写侧先行累计。
    Bigram(String),
    /// 整句层：原始输入串（"z'g'r'm'j'f'j'nan"）。写侧先行累计。
    Input(String),
    /// 上下文同音层：`(同音槽拼音, 上屏词)` → 选择。
    ///
    /// 语义：**在某个上屏词之后**，用户对同一拼音槽选了哪个词。用来让
    /// 「机场 → chufa → 出发」这类跨输入语境学一次就生效，而不是把偏好
    /// 扩散成全局（全局偏好走 `Pin` 层 / 子词 bigram）。
    /// 持久化 ctx 文本 = `"{pinyin}\u{2}{prev}"`（\u{2} 不出现在拼音/汉字中）。
    PinCtx { pinyin: String, prev: String },
}

impl CtxKey {
    /// 持久化的层编号（DB layer 列）。
    pub fn layer(&self) -> i64 {
        match self {
            CtxKey::Pin(_) => 1,
            CtxKey::Bigram(_) => 2,
            CtxKey::Input(_) => 3,
            CtxKey::PinCtx { .. } => 4,
        }
    }
    /// 持久化的 ctx 文本。
    pub fn ctx_text(&self) -> String {
        match self {
            CtxKey::Pin(s) | CtxKey::Bigram(s) | CtxKey::Input(s) => s.clone(),
            CtxKey::PinCtx { pinyin, prev } => format!("{pinyin}\u{2}{prev}"),
        }
    }
}

/// 竞争频率存储（内存权威；经 flush 落 competition / competition_totals）。
#[derive(Default)]
pub struct CompetitionStore {
    /// (ctx, word) → 选中次数（分子）。
    selects: BTreeMap<(CtxKey, String), u32>,
    /// ctx → 该语境总选择次数（分母）。
    totals: BTreeMap<CtxKey, u32>,
    /// 脏集合（增量落盘用；全量快照 replace 语义下可省略，保留给未来增量落盘）。
    dirty: bool,
    /// 是否存在 layer 4（上下文同音）证据。**热路径守卫**：
    /// `score_breakdown` 对每个精确候选都要查一次上下文证据；没有这个 O(1) 标志时
    /// 每次查询都会构造两次 String 的 `CtxKey`（100 候选 ≈ 200 次堆分配/按键）。
    /// 绝大多数用户没有 layer 4 数据，标志为 false 时直接跳过。
    has_pin_ctx: bool,
}

impl CompetitionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 一次竞争事件：被选词进化 + 同场其余词被动稀释。
    pub fn record_choice(&mut self, ctxs: &[CtxKey], chosen: &str) {
        for ctx in ctxs {
            if ctx.layer() == 4 {
                self.has_pin_ctx = true;
            }
            *self.totals.entry(ctx.clone()).or_insert(0) += 1;
            *self
                .selects
                .entry((ctx.clone(), chosen.to_string()))
                .or_insert(0) += 1;
        }
        self.dirty = true;
    }

    /// 是否存在 layer 4（上下文同音）证据——`pin_ctx_boost*` 的热路径守卫。
    pub fn has_pin_ctx(&self) -> bool {
        self.has_pin_ctx
    }

    /// Laplace 平滑的相对频率：` (selects + α·prior) / (total + α) `。
    /// `prior_w` = 该词在此语境的先验频率（归一化 base_freq 份额，调用方给）。
    /// total == 0（零证据）时恒等于 prior_w（回落种子基准）。
    pub fn smoothed(&self, ctx: &CtxKey, word: &str, alpha: u32, prior_w: f32) -> f32 {
        let total = self.totals.get(ctx).copied().unwrap_or(0) as f32;
        let s = self
            .selects
            .get(&(ctx.clone(), word.to_string()))
            .copied()
            .unwrap_or(0) as f32;
        (s + alpha as f32 * prior_w) / (total + alpha as f32)
    }

    /// 分子（审计/测试）。
    pub fn selects_of(&self, ctx: &CtxKey, word: &str) -> u32 {
        self.selects
            .get(&(ctx.clone(), word.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// 分母（审计/测试/归一断言）。
    pub fn total_of(&self, ctx: &CtxKey) -> u32 {
        self.totals.get(ctx).copied().unwrap_or(0)
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    /// 快照（flush 全量 upsert 用）：(layer, ctx, word, selects)。
    pub fn snapshot_selects(&self) -> Vec<(i64, String, String, u32)> {
        self.selects
            .iter()
            .map(|((ctx, w), s)| (ctx.layer(), ctx.ctx_text().to_string(), w.clone(), *s))
            .collect()
    }

    /// 快照（flush）：(layer, ctx, total)。
    pub fn snapshot_totals(&self) -> Vec<(i64, String, u32)> {
        self.totals
            .iter()
            .map(|(ctx, t)| (ctx.layer(), ctx.ctx_text().to_string(), *t))
            .collect()
    }

    /// 启动加载（DB → 内存）。
    pub fn load(
        &mut self,
        selects: Vec<(i64, String, String, u32)>,
        totals: Vec<(i64, String, u32)>,
    ) {
        for (layer, ctx, total) in totals {
            let Some(key) = ctxkey_from(layer, ctx) else {
                continue;
            };
            if key.layer() == 4 {
                self.has_pin_ctx = true;
            }
            self.totals.insert(key, total);
        }
        for (layer, ctx, word, s) in selects {
            let Some(key) = ctxkey_from(layer, ctx) else {
                continue;
            };
            if key.layer() == 4 {
                self.has_pin_ctx = true;
            }
            self.selects.insert((key, word), s);
        }
        self.dirty = false;
    }
}

/// 层编号 + ctx 文本 → CtxKey（未知层丢弃——向前兼容旧行）。
fn ctxkey_from(layer: i64, ctx: String) -> Option<CtxKey> {
    match layer {
        1 => Some(CtxKey::Pin(ctx)),
        2 => Some(CtxKey::Bigram(ctx)),
        3 => Some(CtxKey::Input(ctx)),
        4 => {
            // 上下文同音层，ctx = "{pinyin}\u{2}{prev}"。
            // 分隔符缺失 = 脏行，丢弃（不 panic：旧/损坏 DB 必须可加载）。
            let (pinyin, prev) = ctx.split_once('\u{2}')?;
            if pinyin.is_empty() || prev.is_empty() {
                return None;
            }
            Some(CtxKey::PinCtx {
                pinyin: pinyin.to_string(),
                prev: prev.to_string(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dilution_lowers_loser_frequency_without_time() {
        let mut s = CompetitionStore::new();
        let ctx = CtxKey::Pin("ci ju".into());
        // A 先赢 10 次（~100%）
        for _ in 0..10 {
            s.record_choice(std::slice::from_ref(&ctx), "词句");
        }
        let fa = s.smoothed(&ctx, "词句", ALPHA_PIN, 0.2);
        assert!(fa > 0.6, "A 连胜后应高频: {fa}");
        // B 随后赢 20 次 → A 被动稀释（无时间参与，纯竞争稀释）
        for _ in 0..20 {
            s.record_choice(std::slice::from_ref(&ctx), "此举");
        }
        let fa2 = s.smoothed(&ctx, "词句", ALPHA_PIN, 0.2);
        let fb2 = s.smoothed(&ctx, "此举", ALPHA_PIN, 0.2);
        assert!(fb2 > fa2, "对手胜出后频率应反转: A={fa2} B={fb2}");
        assert!(fa2 < 0.5, "A 应被稀释到半血以下: {fa2}");
    }

    #[test]
    fn frozen_when_no_events() {
        let mut s = CompetitionStore::new();
        let ctx = CtxKey::Pin("de".into());
        for _ in 0..5 {
            s.record_choice(std::slice::from_ref(&ctx), "的");
        }
        let snap1 = (
            s.total_of(&ctx),
            s.selects_of(&ctx, "的"),
            s.smoothed(&ctx, "的", ALPHA_PIN, 0.9),
        );
        // 「很久没用电脑」= 没有任何 API 调用——数值逐位冻结
        let snap2 = (
            s.total_of(&ctx),
            s.selects_of(&ctx, "的"),
            s.smoothed(&ctx, "的", ALPHA_PIN, 0.9),
        );
        assert_eq!(snap1.0, snap2.0);
        assert_eq!(snap1.1, snap2.1);
        assert_eq!(snap1.2.to_bits(), snap2.2.to_bits());
    }

    #[test]
    fn zero_evidence_falls_back_to_prior() {
        let s = CompetitionStore::new();
        let ctx = CtxKey::Pin("de".into());
        // 零证据：smoothed 恒等 prior（回落种子基准）
        let f = s.smoothed(&ctx, "的", ALPHA_PIN, 0.8);
        assert_eq!(f, 0.8);
    }

    #[test]
    fn roundtrip_snapshot_load() {
        let mut s = CompetitionStore::new();
        let ctx = CtxKey::Pin("ci ju".into());
        s.record_choice(std::slice::from_ref(&ctx), "词句");
        s.record_choice(std::slice::from_ref(&ctx), "词句");
        s.record_choice(std::slice::from_ref(&ctx), "此举");
        let sel = s.snapshot_selects();
        let tot = s.snapshot_totals();
        let mut s2 = CompetitionStore::new();
        s2.load(sel, tot);
        assert_eq!(s2.total_of(&ctx), 3);
        assert_eq!(s2.selects_of(&ctx, "词句"), 2);
        assert_eq!(
            s2.smoothed(&ctx, "词句", ALPHA_PIN, 0.5).to_bits(),
            s.smoothed(&ctx, "词句", ALPHA_PIN, 0.5).to_bits()
        );
    }

    #[test]
    fn pin_ctx_layer4_roundtrip_and_parse() {
        let ctx = CtxKey::PinCtx {
            pinyin: "chu fa".into(),
            prev: "机场".into(),
        };
        assert_eq!(ctx.layer(), 4);
        // ctx 文本经 DB 往返后必须还原（层 4 解析依赖 \u{2} 分隔）。
        assert_eq!(
            ctxkey_from(ctx.layer(), ctx.ctx_text()),
            Some(ctx.clone()),
            "层 4 ctx 文本往返失败"
        );
        // 脏行（缺分隔符/空段）静默丢弃，不 panic。
        assert_eq!(ctxkey_from(4, "malformed".into()), None);
        assert_eq!(ctxkey_from(4, "\u{2}机场".into()), None);
        assert_eq!(ctxkey_from(4, "chu fa\u{2}".into()), None);
        // 未知层仍丢弃。
        assert_eq!(ctxkey_from(9, "x".into()), None);
    }

    #[test]
    fn has_pin_ctx_fast_path_flag() {
        let mut s = CompetitionStore::new();
        assert!(!s.has_pin_ctx(), "默认无 layer 4 证据");
        // 非 layer 4 写入不置位。
        s.record_choice(&[CtxKey::Pin("chu fa".into())], "处罚");
        assert!(!s.has_pin_ctx(), "layer 1 不应置位");
        // layer 4 写入置位。
        s.record_choice(
            &[CtxKey::PinCtx {
                pinyin: "chu fa".into(),
                prev: "机场".into(),
            }],
            "出发",
        );
        assert!(s.has_pin_ctx(), "layer 4 应置位");
        // 加载路径同样置位（重启后热路径守卫不能误判为空）。
        let mut s2 = CompetitionStore::new();
        s2.load(
            vec![(4, "chu fa\u{2}机场".to_string(), "出发".to_string(), 1)],
            vec![(4, "chu fa\u{2}机场".to_string(), 1)],
        );
        assert!(s2.has_pin_ctx(), "load 应识别 layer 4");
    }

    #[test]
    fn pin_ctx_evidence_dilutes_without_touching_global_pin() {
        let mut s = CompetitionStore::new();
        let ctx = CtxKey::PinCtx {
            pinyin: "chu fa".into(),
            prev: "机场".into(),
        };
        s.record_choice(std::slice::from_ref(&ctx), "出发");
        let f = s.smoothed(&ctx, "出发", ALPHA_PIN_CTX, 0.3);
        assert!(f > 0.3, "一次上下文选择后应高于先验: {f}");
        // 全局同音层零证据（上下文层不污染全局层语义）。
        assert_eq!(s.total_of(&CtxKey::Pin("chu fa".into())), 0);
    }
}
