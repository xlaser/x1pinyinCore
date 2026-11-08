//! 绿词 DP 段缓存守护（缓存只改性能不改结果）。
//!
//! 缓存分层（见 `Engine::seg_query_cached` 与 `seg_cache` 字段注释）：
//! - **缓存层**（跨按键复用）：dict 级查询 + 末段单字边 + 字数 retain。
//! - **新鲜层**（每键执行）：counts 排序 / truncate(50) / green_memo 补充。
//! - **失效**：仅 dict 变异点（insert_user_word_to_dict / remove_by_text /
//!   rebuild_dict 各处 → seg_cache_clear）。
//!
//! 守护矩阵（验收标准 = 「缓存只改性能不改结果」）：
//! 1. 逐字节一致：同态引擎下重复查询（缓存热）与首次查询（缓存冷）候选全等。
//! 2. 选择频次即时生效：commit 系统词只动 freq.counts（dict 不变、缓存不清），
//!    其后结果必须与「未经历首次查询、只做同样 commit」的参照引擎全等。
//! 3. green_memo 即时生效：整句 commit 后同输入重查，绿词记忆候选必须出现
//!    （green_memo 补充在新鲜层，正确性不依赖缓存失效时机）。
//! 4. dict 变异失效：learn 用户词 / remove 删词后，旧缓存不得外泄——与
//!    只做同样变异的参照引擎全等。
//! 5. 逐键打字模拟（真实卡顿场景）：长串逐字母输入，每键的增量缓存结果
//!    与「每键前清缓存的全量重算」引擎逐键全等。
use super::*;

/// 候选签名（比较口径 = 「逐字节一致」）：
/// 文本 / base_freq / 拼音 / is_sentence / DP 路径分（绿词排序依据）。
fn sig(cands: &[Candidate]) -> Vec<(String, u32, String, bool, i32)> {
    cands
        .iter()
        .map(|c| {
            (
                c.text.as_ref().to_string(),
                c.base_freq,
                c.pinyin.as_ref().to_string(),
                c.is_sentence,
                c.sentence_score,
            )
        })
        .collect()
}

/// 1. 缓存热（重复查询）与缓存冷（首次查询）逐字节一致。
#[test]
fn test_seg_cache_warm_equals_cold() {
    let inputs = [
        "zgrmjfjxa",       // 纯简拼长串（简拼子树洪泛主场景）
        "wwww",            // 重复字母简拼
        "woxihuanchifan",  // 全拼长句
        "zhonggrm",        // 全拼 + 简拼尾混合
        "zgrmjfjxan",      // 简拼 + 全拼尾混合
        "mingtianxiawuwu", // 全拼（xqw 声母歧义）
    ];
    for input in inputs {
        let engine = Engine::new();
        let cold = engine.process(input);
        let warm = engine.process(input); // 全部段命中缓存
        assert_eq!(
            sig(&cold),
            sig(&warm),
            "「{}」缓存热/冷结果不一致（缓存改了结果）",
            input
        );
        // 第三次（缓存容量路径稳定）再验一次
        let warm2 = engine.process(input);
        assert_eq!(sig(&cold), sig(&warm2), "「{}」第三次查询漂移", input);
    }
}

/// 2. 选择频次即时生效：commit 系统词（只动 counts，dict 不变）后，
///    缓存热引擎与参照引擎（未预热）全等。
#[test]
fn test_seg_cache_counts_fresh_without_invalidation() {
    // 预热 + 学习 vs 只学习：结果必须一致
    let mut warmed = Engine::new();
    let _ = warmed.process("zgrmjfjxa"); // 预热段缓存
                                         // commit 系统词「解」(jie)：走 freq.record（counts+1），is_system_word
                                         // 命中 → 不进 user_words、不动 Trie → seg_cache 不失效（这正是本测试
                                         // 要钉住的新鲜层语义：counts 排序每键重跑）。
    warmed.commit("jie", "解", Some("jie"), true);

    let mut reference = Engine::new();
    reference.commit("jie", "解", Some("jie"), true);

    let a = warmed.process("zgrmjfjxa");
    let b = reference.process("zgrmjfjxa");
    assert_eq!(
        sig(&a),
        sig(&b),
        "commit 系统词后缓存热引擎与参照引擎结果不一致（counts 未即时生效）"
    );
    // 且「解」的 counts 确实进入了候选排序可见层（同输入再查仍一致）
    let a2 = warmed.process("zgrmjfjxa");
    assert_eq!(sig(&a), sig(&a2));
}

/// 3. green_memo 即时生效：整句 commit 后同输入重查，绿词记忆候选必须出现
///    （green_memo 补充在新鲜层，正确性不依赖缓存失效时机）。
#[test]
fn test_seg_cache_green_memo_fresh() {
    let mut warmed = Engine::new();
    let _ = warmed.process("woxihuanchifan"); // 预热段缓存
    warmed.commit(
        "woxihuanchifan",
        "我喜欢吃饭",
        Some("wo xi huan chi fan"),
        true,
    );

    let mut reference = Engine::new();
    reference.commit(
        "woxihuanchifan",
        "我喜欢吃饭",
        Some("wo xi huan chi fan"),
        true,
    );

    let a = warmed.process("woxihuanchifan");
    let b = reference.process("woxihuanchifan");
    assert_eq!(
        sig(&a),
        sig(&b),
        "整句 commit 后缓存热引擎与参照引擎不一致（green_memo 未即时生效）"
    );
    assert!(
        b.iter().any(|c| c.text.as_ref() == "我喜欢吃饭"),
        "参照引擎应含绿词记忆「我喜欢吃饭」，实际: {:?}",
        sig(&b).iter().take(5).map(|s| &s.0).collect::<Vec<_>>()
    );
}

/// 4. dict 变异失效：learn 用户词 / remove 删词后旧缓存不得外泄。
#[test]
fn test_seg_cache_invalidated_on_dict_mutation() {
    // 4a. learn 用户词（record_user_word_pronunciation → Trie 重建 → 清缓存）
    let mut warmed = Engine::new();
    let _ = warmed.process("zgrmjfjxa");
    warmed.learn("解放军甲", "jie fang jun jia");

    let mut reference = Engine::new();
    reference.learn("解放军甲", "jie fang jun jia");

    let a = warmed.process("zgrmjfjxa");
    let b = reference.process("zgrmjfjxa");
    assert_eq!(
        sig(&a),
        sig(&b),
        "learn 后缓存热引擎与参照引擎不一致（旧缓存外泄）"
    );
    // 新学的用户词在两引擎中都可达（全拼路径）
    let full_input = "jie fang jun jia".replace(' ', "");
    let a_full = warmed.process(&full_input);
    assert!(
        a_full.iter().any(|c| c.text.as_ref() == "解放军甲"),
        "learn 的新词应出现在全拼候选"
    );

    // 4b. remove 删词（Trie 删除 → 清缓存）：先 learn 一个自包含词（不依赖内置
    //     词库内容），确认可见后 remove，缓存热引擎须与「从未学过」的参照一致。
    let mut warmed2 = Engine::new();
    warmed2.learn("解放军甲", "jie fang jun jia");
    let jf_input = "jiefangjunjia";
    let before = warmed2.process(jf_input);
    assert!(
        before.iter().any(|c| c.text.as_ref() == "解放军甲"),
        "前置：learn 后「解放军甲」应可见"
    );
    let _ = warmed2.process("zgrmjfjxa"); // 再预热一批不相关段
    warmed2.remove("解放军甲");

    // 参照：从未学过该词，但对同一词做一次 remove（拿到同样的黑名单副作用）
    let mut reference2 = Engine::new();
    reference2.remove("解放军甲");

    let a = warmed2.process(jf_input);
    let b = reference2.process(jf_input);
    assert_eq!(
        sig(&a),
        sig(&b),
        "remove 后缓存热引擎与参照引擎不一致（删词从缓存复活）"
    );
    assert!(
        !a.iter().any(|c| c.text.as_ref() == "解放军甲"),
        "remove 后「解放军甲」不得再出现"
    );
}

/// 5. 逐键打字模拟：长串逐字母输入（真实卡顿场景），每键的增量缓存结果
///    与「每键前清缓存」的全量重算引擎逐键全等。
#[test]
fn test_seg_cache_typing_simulation_equals_full_recompute() {
    let cached = Engine::new();
    let mut cold = Engine::new();
    for input in ["zgrmjfjxakjkjxy", "woxihuanchifanle"] {
        let mut acc = String::new();
        for ch in input.chars() {
            acc.push(ch);
            cold.seg_cache_clear(); // 模拟全量重算（缓存冷）
            let warm_cands = cached.process(acc.as_str());
            let cold_cands = cold.process(acc.as_str());
            assert_eq!(
                sig(&warm_cands),
                sig(&cold_cands),
                "逐键打字 len={}「{}」增量缓存与全量重算不一致",
                acc.chars().count(),
                acc
            );
        }
    }
}

/// 6. 长句逐键耗时曲线（性能守护，手动跑）：
/// `cargo test -p x1core --release --lib tests_seg_cache -- --ignored --nocapture`
///
/// 无段缓存时（种子 DB 副本，简拼逐字母）：len=4 3.9ms 线性增至
/// len=15 15.1ms（query_seg 占 78%）——段缓存后应为个位数且曲线走平（稳定
/// 前缀段全命中，仅尾部段增量计算）。断言上限取 8ms（留 2 倍余量防调度噪声）。
#[test]
#[ignore]
fn test_seg_cache_typing_latency_curve() {
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    let db = crate::test_tmp::path("seg_cache_curve_seed.db");
    let _ = std::fs::remove_file(&db);
    std::fs::copy(seed, &db).expect("copy seed");
    let db_str = db.to_str().expect("utf8 path").to_string();
    let engine = Engine::new_with_db(&db_str).expect("open seed");

    let letters = "zgrmjfjxakjkjxy"; // 15 个简拼字母（长简拼逐字母卡顿场景）
    let mut acc = String::new();
    let mut worst_ms = 0.0f64;
    for ch in letters.chars() {
        acc.push(ch);
        let t0 = std::time::Instant::now();
        let _ = engine.process(acc.as_str());
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        worst_ms = worst_ms.max(ms);
        println!("process len={:2} {:6.2}ms", acc.chars().count(), ms);
    }
    println!("worst={:.2}ms（无段缓存基线 len=15 ≈ 15.1ms）", worst_ms);
    assert!(
        worst_ms < 8.0,
        "逐键耗时恶化：worst={:.2}ms ≥ 8ms（基线见测试注释）",
        worst_ms
    );
}
