//! 测试/评测临时目录：本 crate 及相关 crate 的测试与评测
//! **一切过程产物**（种子 DB 副本、构造式 DB、测试日志、探针落盘）一律落这里，
//! **绝不落系统临时目录**。
//!
//! 动机：全量测试会并发生成大量种子 DB 副本（每份 53MB 级），落系统临时目录
//! 容易撑满磁盘导致测试随机失败；本模块在代码层固定产物目录，不依赖人工清理。
//!
//! 位置：`<workspace>/target/test-tmp/`（`target/` 在 .gitignore，`cargo clean`
//! 即清空，永不影响 git 与用户数据）。环境变量 `X1_TEST_TMP_DIR` 可覆盖（CI/沙箱）。
//!
//! 自愈：每个测试进程首次取目录时，顺手清掉 **24 小时前** 的旧条目
//! （文件与目录皆可，失败静默）——全量测试每轮产物量大，没有这道清扫会随
//! 时间无限增长；副本文件名全带 pid+纳秒、**从不跨轮复用**，
//! 24h 窗口不会误删任何还在使用的东西（失败现场留一天足够排障）。
//!
//! 例外（勿迁）：`eval_replay.rs` / `eval_baseline.rs` 的两个**结果 JSON**
//! （x1_eval_replay.json / x1_eval_baseline.json，约 5KB）仍写系统临时目录——
//! 那是外部读取方的**跨进程契约路径**（按系统临时目录拼接读取），
//! 迁移需两端同改。它们不是体积问题；**DB 副本**（53MB 级）一律走本模块。

use std::path::{Path, PathBuf};
use std::sync::Once;

/// 旧产物清扫阈值：超过即视为历史测试轮次的垃圾（测试单次运行分钟级，
/// 产物不跨轮复用——见模块注释）。
const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// 测试临时目录根（不存在则创建）。
pub fn dir() -> PathBuf {
    let base = std::env::var_os("X1_TEST_TMP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // CARGO_MANIFEST_DIR = 本 crate 目录（x1core/），上一级 = workspace 根。
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .map(|root| root.join("target").join("test-tmp"))
                .unwrap_or_else(|| PathBuf::from("target/test-tmp"))
        });
    let _ = std::fs::create_dir_all(&base);
    sweep_once(&base);
    base
}

/// 测试临时目录下的文件路径。
pub fn path(name: &str) -> PathBuf {
    dir().join(name)
}

/// 每进程一次：清掉 24h 前的旧文件/旧目录（best effort，锁占用/权限失败静默跳过）。
fn sweep_once(base: &Path) {
    static SWEPT: Once = Once::new();
    SWEPT.call_once(|| {
        let Ok(entries) = std::fs::read_dir(base) else {
            return;
        };
        let now = std::time::SystemTime::now();
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let old = now
                .duration_since(modified)
                .map(|age| age > MAX_AGE)
                .unwrap_or(false);
            if !old {
                continue;
            }
            if meta.is_dir() {
                drop(std::fs::remove_dir_all(entry.path()));
            } else {
                drop(std::fs::remove_file(entry.path()));
            }
        }
    });
}

/// 断言种子 DB 就位：缺失时 panic 并给出获取指引（替代晦涩的 copy 错误）。
/// 依赖真实词库的测试统一先过此检查；纯逻辑测试不需要种子。
#[cfg(test)]
pub fn require_seed_db() {
    let seed = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
    if !std::path::Path::new(seed).exists() {
        panic!(
            "种子 DB 缺失：从 GitHub Releases 下载 x1pinyin.db 放到 \
             x1core/assets/（见 README「词库说明」）；依赖种子的测试会失败，\
             纯逻辑测试不受影响"
        );
    }
}
