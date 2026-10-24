//! 内置基础词库数据
//!
//! 词库存放在 `assets/pinyin_dict.txt`，构建时通过 `include_str!` 编译进二进制。
//! 格式：拼音音节（空格分隔）\t汉字\t基础频率

/// 内置词库文本
pub const BUILTIN_DICT: &str = include_str!("../assets/pinyin_dict.txt");
