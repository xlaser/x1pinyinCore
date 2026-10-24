//! 引擎平台无关性守护：扫描 src/engine 非测试源码，禁止平台专属标记。

use std::path::{Path, PathBuf};

/// 禁止出现在引擎源码里的平台专属标记。
const FORBIDDEN: &[&str] = &[
    "cfg(target_os",
    "cfg(windows",
    "cfg(unix",
    "cfg(not(any(unix",
    "APPDATA",
    "ProgramFiles",
    "HKEY_",
    "Library/Application Support",
];

/// 扫描结果：(文件, 行号, 命中标记, 行内容)
fn violations(dir: &Path) -> Vec<(PathBuf, usize, String, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        // 只审 .rs，跳过测试文件（测试允许平台路径/环境变量）
        if !name.ends_with(".rs") || name.starts_with("tests_") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            // 注释行豁免：注释里写「macOS 无对应 API」属说明，不是代码分支
            let code = line.trim_start();
            if code.starts_with("//") || code.starts_with("///") || code.starts_with("//!") {
                continue;
            }
            for pat in FORBIDDEN {
                if line.contains(pat) {
                    out.push((path.clone(), i + 1, (*pat).to_string(), line.trim().to_string()));
                }
            }
        }
    }
    out
}

#[test]
fn engine_sources_are_platform_agnostic() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("engine");
    let v = violations(&dir);
    assert!(
        v.is_empty(),
        "引擎源码出现平台专属代码（应移入 x1core::platform 或对应基础设施垫片）：\n{}",
        v.iter()
            .map(|(f, line, pat, text)| format!(
                "  {}:{} [{}] {}",
                f.file_name().unwrap_or_default().to_string_lossy(),
                line,
                pat,
                text
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
