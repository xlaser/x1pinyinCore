//! 词频固化提案。
//!
//! 把 user.db 的竞争证据翻译成**种子 freq 调整提案**——日常使用/自动学习中
//! 的选择分布已经实时改写排序（competition 零和层），本模块把「同音语境内
//! 的实际胜率 vs 种子 freq 先验」的稳定偏离换算成 freq 建议值，供人工审核
//! 后固化回种子（freq 谱调整走提案-人审制，禁全量重算）。
//!
//! 口径基准（与运行时打分零漂移，同源 competition.rs / scoring.rs）：
//!   · 先验 prior_w = 语境（x1dict_words 中 pinyin=ctx 的条目）freq 份额归一
//!     ——scoring.rs pin_priors_full 的同数据源口径；
//!   · 实际 smoothed_w = (selects + α_pin·prior_w) / (total + α_pin)，
//!     α_pin=4（competition.rs::smoothed）；
//!   · 偏离 dynamic = smoothed_w − prior_w，即打分动态项 ±480 的同源量。
//!
//! 提案单位 = (simplified, pinyin) 词典条目：多音字各读音独立演化
//! （「的 de」降不影响「的 di」）。只提案系统词条目；用户自造词走
//! promoteToSeed 通道（本模块不碰）。

use std::path::Path;

use rusqlite::Connection;

use crate::competition::ALPHA_PIN;

/// 同音语境最小证据量（total 低于此值先验主导，不提案）。
pub const MIN_TOTAL: u32 = 20;
/// 最小偏离（|smoothed − prior|，0-1 份额）。
pub const MIN_DEVIATION: f32 = 0.15;
/// 高证据档门槛：样本量与偏离双达标（UI 默认全选档）。
pub const HIGH_TOTAL: u32 = 50;
pub const HIGH_DEVIATION: f32 = 0.25;
/// 单次提案幅度钳制（防过冲：freq 谱手工调优资产，一次只走一步）。
pub const CLAMP_LO: f32 = 0.5;
pub const CLAMP_HI: f32 = 1.5;
/// 最小相对变化（|new−old|/old）：过滤谱顶噪声——9990→9999 这类被谱界钳平的
/// 微调无信息量（语境内胜率高≠该词全局频率低，高频功能词推顶格是常见形态）。
pub const MIN_RELATIVE_CHANGE: f32 = 0.10;
/// 升频提案的 old_freq 上限：谱高位词谈不上「被低估」，胜率垄断只是语言常态
///（的/是/我 在各自同音组天然接近垄断）——升频只对谱中低位词有意义；
/// 降频不限（同音组输家降频总是真实信号，如 得 de 输给 的）。
pub const LIFT_MAX_OLD: u32 = 5000;
/// freq 谱界（种子 freq 保持真实值 50-9999，勿压扁勿超谱）。
pub const FREQ_MAX: u32 = 9999;
pub const FREQ_MIN: u32 = 50;

/// 一条同音竞争证据。
#[derive(Debug, Clone)]
pub struct Evidence {
    /// 竞争语境（词的完整拼音，如 "ci ju"）
    pub ctx: String,
    pub selects: u32,
    pub total: u32,
    pub prior: f32,
    pub smoothed: f32,
}

/// 一条 freq 调整提案。
#[derive(Debug, Clone)]
pub struct FreqProposal {
    pub text: String,
    pub pinyin: String,
    pub old_freq: u32,
    pub new_freq: u32,
    /// smoothed/prior（调整方向与幅度的来源）
    pub ratio: f32,
    /// "high" = 证据强（UI 默认选中）；"mid" = 一般证据（默认不选）
    pub level: &'static str,
    pub evidence: Evidence,
}

/// 分析双库（dict.db 词频先验 + user.db 竞争证据），生成提案，按偏离幅度降序。
///
/// 两个连接都以只读打开（分析零副作用）。
pub fn analyze_freq_proposals(dict_db: &Path, user_db: &Path) -> Result<Vec<FreqProposal>, String> {
    let dict = open_ro(dict_db)?;
    let user = open_ro(user_db)?;

    // (ctx, word) → selects；ctx → total（仅同音层 layer=1）
    let rows: Vec<(String, String, u32, u32)> = {
        let mut stmt = user
            .prepare(
                "SELECT c.ctx, c.word, c.selects, t.total FROM competition c \
                 JOIN competition_totals t ON t.layer = c.layer AND t.ctx = c.ctx \
                 WHERE c.layer = 1",
            )
            .map_err(|e| format!("查询 competition 失败: {e}"))?;
        let it = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u32>(2)?,
                    r.get::<_, u32>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        it.filter_map(Result::ok).collect()
    };

    let mut out: Vec<FreqProposal> = Vec::new();
    for (ctx, word, selects, total) in rows {
        if total < MIN_TOTAL {
            continue;
        }
        // 先验：语境（pinyin=ctx）内系统词条目 freq 份额归一
        let mut stmt = match dict.prepare("SELECT freq FROM x1dict_words WHERE pinyin = ?1") {
            Ok(s) => s,
            Err(_) => continue,
        };
        let freqs: Vec<i64> = stmt
            .query_map([&ctx], |r| r.get(0))
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default();
        let sum: i64 = freqs.iter().sum();
        if sum <= 0 {
            continue;
        }
        // 该词的先验与旧 freq（(text, pinyin) 条目精确匹配）
        let old_freq: i64 = match dict.query_row(
            "SELECT freq FROM x1dict_words WHERE simplified = ?1 AND pinyin = ?2",
            [&word, &ctx],
            |r| r.get(0),
        ) {
            Ok(f) => f,
            Err(_) => continue, // 非系统词条目（用户词等），不提案
        };
        let prior_w = old_freq as f32 / sum as f32;
        let smoothed_w =
            (selects as f32 + ALPHA_PIN as f32 * prior_w) / (total as f32 + ALPHA_PIN as f32);
        let deviation = smoothed_w - prior_w;
        if deviation.abs() < MIN_DEVIATION {
            continue;
        }
        // 升频限谱中低位（见 LIFT_MAX_OLD 文档）
        if deviation > 0.0 && old_freq as u32 >= LIFT_MAX_OLD {
            continue;
        }
        let ratio = if prior_w > 0.0 {
            smoothed_w / prior_w
        } else {
            continue;
        };
        let clamped = ratio.clamp(CLAMP_LO, CLAMP_HI);
        let new_freq = ((old_freq as f32 * clamped).round() as i64)
            .clamp(FREQ_MIN as i64, FREQ_MAX as i64) as u32;
        // 相对变化门槛：谱界钳平后的微调无信息量，不提案
        let rel_change = (f64::from(new_freq) - old_freq as f64).abs() / old_freq as f64;
        if rel_change < f64::from(MIN_RELATIVE_CHANGE) {
            continue;
        }
        let level: &'static str = if total >= HIGH_TOTAL && deviation.abs() >= HIGH_DEVIATION {
            "high"
        } else {
            "mid"
        };
        out.push(FreqProposal {
            text: word,
            pinyin: ctx.clone(),
            old_freq: old_freq as u32,
            new_freq,
            ratio,
            level,
            evidence: Evidence {
                ctx,
                selects,
                total,
                prior: prior_w,
                smoothed: smoothed_w,
            },
        });
    }
    out.sort_by(|a, b| {
        let da = (a.evidence.smoothed - a.evidence.prior).abs();
        let db = (b.evidence.smoothed - b.evidence.prior).abs();
        db.partial_cmp(&da).unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

fn open_ro(path: &Path) -> Result<Connection, String> {
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    Connection::open_with_flags(path, flags)
        .map_err(|e| format!("只读打开 {} 失败: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_tmp;

    fn build_pair(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = test_tmp::dir().join(format!("freq-promote-{tag}"));
        // 幂等：复跑先清残留（CREATE TABLE 非 IF NOT EXISTS）
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dict = dir.join("dict.db");
        let user = dir.join("user.db");
        {
            let c = Connection::open(&dict).unwrap();
            c.execute_batch(
                "CREATE TABLE x1dict_words (id INTEGER PRIMARY KEY, simplified TEXT NOT NULL,
                    pinyin TEXT NOT NULL, freq INTEGER DEFAULT 0, is_primary INTEGER DEFAULT 1);
                 INSERT INTO x1dict_words (simplified, pinyin, freq) VALUES
                    ('此句', 'ci ju', 100), ('词句', 'ci ju', 900);",
            )
            .unwrap();
        }
        {
            let c = Connection::open(&user).unwrap();
            c.execute_batch(
                "CREATE TABLE competition (layer INTEGER NOT NULL, ctx TEXT NOT NULL,
                    word TEXT NOT NULL, selects INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (layer, ctx, word));
                 CREATE TABLE competition_totals (layer INTEGER NOT NULL, ctx TEXT NOT NULL,
                    total INTEGER NOT NULL, PRIMARY KEY (layer, ctx));",
            )
            .unwrap();
        }
        (dict, user)
    }

    fn set_evidence(user: &Path, selects: u32, total: u32) {
        let c = Connection::open(user).unwrap();
        c.execute(
            "INSERT INTO competition (layer, ctx, word, selects) VALUES (1, 'ci ju', '此句', ?1)
             ON CONFLICT(layer, ctx, word) DO UPDATE SET selects = ?1",
            [selects],
        )
        .unwrap();
        c.execute(
            "INSERT INTO competition_totals (layer, ctx, total) VALUES (1, 'ci ju', ?1)
             ON CONFLICT(layer, ctx) DO UPDATE SET total = ?1",
            [total],
        )
        .unwrap();
    }

    #[test]
    fn proposes_lift_for_underdog_with_strong_evidence() {
        let (dict, user) = build_pair("lift");
        // 先验 此句=0.1；强证据 60 次里选了 48 次（smoothed≈0.6）→ 升频提案
        set_evidence(&user, 48, 60);
        let ps = analyze_freq_proposals(&dict, &user).unwrap();
        assert_eq!(ps.len(), 1);
        let p = &ps[0];
        assert_eq!(p.text, "此句");
        assert_eq!(p.old_freq, 100);
        assert!(p.new_freq > p.old_freq, "应升频: {p:?}");
        assert_eq!(p.level, "high");
        // 幅度钳制上限 1.5x
        assert!(p.new_freq as f32 <= 100.0 * CLAMP_HI + 1.0, "应钳制: {p:?}");
    }

    #[test]
    fn skips_low_total_and_small_deviation() {
        let (dict, user) = build_pair("threshold");
        // total < 20：不提案
        set_evidence(&user, 5, 10);
        assert!(analyze_freq_proposals(&dict, &user).unwrap().is_empty());
        // total 够但偏离小（4/20，prior 0.1，smoothed≈0.32 → 偏离 0.22 过 0.15 会提案；
        // 改成 3/40：smoothed≈0.34/prior 0.1 偏离大……用接近先验的分布验证跳过）
        set_evidence(&user, 4, 40); // smoothed=(4+0.4)/44≈0.1，与 prior 0.1 几乎一致
        assert!(analyze_freq_proposals(&dict, &user).unwrap().is_empty());
    }

    #[test]
    fn freq_spectrum_ceiling_respected() {
        let (dict, user) = build_pair("ceiling");
        // 组内三条平分先验，「此句」已顶格 9999：强证据升频 → 谱界钳回 9999 → new==old 不提案
        {
            let c = Connection::open(&dict).unwrap();
            c.execute(
                "UPDATE x1dict_words SET freq = 9999 WHERE simplified = '此句'",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO x1dict_words (simplified, pinyin, freq) VALUES ('另句', 'ci ju', 5000)",
                [],
            )
            .unwrap();
        }
        set_evidence(&user, 55, 60); // prior≈0.5，smoothed≈0.89，偏离 0.39 足额
        let ps = analyze_freq_proposals(&dict, &user).unwrap();
        assert!(
            ps.iter().all(|p| p.text != "此句" || p.new_freq <= 9999),
            "顶格词不应有提案或不得超谱: {ps:?}"
        );
    }

    #[test]
    fn non_system_word_never_proposed() {
        let (dict, user) = build_pair("nonword");
        let c = Connection::open(&user).unwrap();
        c.execute(
            "INSERT INTO competition (layer, ctx, word, selects) VALUES (1, 'ci ju', '自造词', 30)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO competition_totals (layer, ctx, total) VALUES (1, 'ci ju', 60)
             ON CONFLICT(layer, ctx) DO UPDATE SET total = 60",
            [],
        )
        .unwrap();
        assert!(analyze_freq_proposals(&dict, &user).unwrap().is_empty());
    }
}
