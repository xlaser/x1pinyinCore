//! build.rs — 编译时生成内置词库
//!
//! 读取 `assets/pinyin_dict.txt` 中的词库文本，验证格式正确性。
//! 词库通过 `include_str!` 编译进二进制。

use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let dict_path = manifest_dir.join("assets").join("pinyin_dict.txt");

    // 告诉 Cargo 在词库数据变化时重新编译
    println!("cargo:rerun-if-changed=assets/pinyin_dict.txt");

    // 简单验证：确保文件存在且不为空
    let content = fs::read_to_string(&dict_path).expect("failed to read assets/pinyin_dict.txt");
    if content.trim().is_empty() {
        panic!("assets/pinyin_dict.txt is empty");
    }

    // 统计词条数（仅用于构建日志）
    // 用普通 println!（构建脚本 stdout，-vv 才显示）而非 cargo:warning——后者会
    // 让每次构建都打一条不属于警告的"警告"，污染构建警告面。
    let count = content.lines().filter(|l| !l.trim().is_empty()).count();
    println!("dict entries: {}", count);
}
