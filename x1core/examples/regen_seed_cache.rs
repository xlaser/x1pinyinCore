//! 刷新 seed_bigram_cache.bin 资产（种子 bigram 维护后）。
//!
//! 种子表更新后（离线合并工具增量合并），freq_rebuild
//! --hash-only 会换 x1dict_sync_hash → 旧缓存自动失效。本工具走真实引擎
//! 路径（open_split → from_storage → load_seed_bigram_table_cached）触发
//! 慢路径重读 + 重写缓存到 dict 同目录，保持交付资产首启即快速路径。
//!
//! 用法：cargo run -p x1core --release --example regen_seed_cache -- [dict] [user]
//!   dict 默认 x1core/assets/x1pinyin.db；user 默认 target/test-tmp 一次性库
//!   （仅承载用户侧空表，引擎会自动建）。
//!
//! ⚠️ 引擎打开会把 journal 转 WAL（probe_cands 同款警告）：跑完本工具后
//! **必须** `freq_rebuild <dict> --normalize-journal` 归一回 DELETE 交付形态。

use x1core::engine::Engine;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dict = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "x1core/assets/x1pinyin.db".into());
    let user = args.get(2).cloned().unwrap_or_else(|| {
        std::path::Path::new("target/test-tmp/regen_scratch_user.db")
            .to_string_lossy()
            .into_owned()
    });
    if let Some(dir) = std::path::Path::new(&user).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let t0 = std::time::Instant::now();
    let engine = match Engine::open_split(&dict, &user) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("❌ open_split({dict}) 失败: {e}");
            std::process::exit(1);
        }
    };
    let load_ms = t0.elapsed().as_millis();
    // 引擎冒烟：证明合并后的 DB 能正常出候选（维护链的引擎级验收）
    let cands = engine.process("nihao");
    let top = cands.first().map(|c| c.text.clone()).unwrap_or_default();
    println!(
        "✅ 引擎加载 {load_ms}ms；冒烟 nihao → {} 候选（首位 {top:?}）",
        cands.len()
    );
    println!("缓存已重写：{} 同目录 seed_bigram_cache.bin", dict);
    println!("⚠️ 记得：freq_rebuild {dict} --normalize-journal（journal 已转 WAL）");
}
