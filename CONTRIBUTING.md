# 贡献指南

感谢参与！本文说明开发环境、提交前检查与代码约定。

## 开发环境

- Rust stable 工具链（CI 在 Linux / macOS / Windows 三平台以最新 stable 验证）
- 无需其他系统依赖：SQLite 为 bundled 编译

```bash
# 快速验证（无需种子 DB）
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p x1core --lib -- pinyin:: quickcmds symbols:: fuzzy::tests:: stats::tests

# 无种子自举一个最小词库（建库 + 引擎加载自验证）
cargo run -p x1core --bin seed_init -- target/mini_seed.db

# 全量测试：需要完整种子（见 README「词库说明」放置 x1pinyin.db 后）
cargo test -p x1core
```

## 代码约定

- **注释解释"为什么"与现状约束**，用一般现在时描述当前行为；不写变更
  历史（git 负责）、不引入内部编号/排期代号。
- 中文注释与文档是本仓库的工作语言。
- 不引入网络依赖、遥测或平台绑定（engine 层的平台纯净性有测试守护；
  平台差异放 `platform.rs` 垫片）。
- 排序/学习行为的修改请带上回归测试；排序语义变更需同步参考比较器与
  等价性测试。
- 提交信息中文即可，说明"做了什么、为什么"。

## 提交前检查清单

- [ ] `cargo clippy --workspace --all-targets -- -D warnings` 零告警
- [ ] 纯逻辑测试子集全绿（上方命令）
- [ ] 有种子时全量测试全绿（若改动影响查询/学习/存储行为则必跑）
- [ ] FFI 签名变更需重新生成头文件：
      `cd x1core && cbindgen --config cbindgen.toml --output include/x1core_ffi.h`
      并同步更新契约文档

## 许可与贡献条款

本项目以 **GPL-3.0** 发布（见 [LICENSE](LICENSE)）。

向本仓库提交代码即表示贡献者同意：

1. 以 GPL-3.0 发布其贡献；
2. 授予项目作者将该贡献按任意其他协议（含商业协议）再许可的权利——
   项目作者保留整体双许可能力所必需。

通过 PR 提交即视为接受上述条款。若不愿授予再许可权利，请先在 issue
中讨论。

## 报告问题

- 缺陷/功能建议：GitHub Issues（请附输入串、期望与实际候选）
- 安全漏洞：请勿公开 issue，走 [SECURITY.md](SECURITY.md) 的渠道
