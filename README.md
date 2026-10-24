# x1core — x1拼音输入法引擎核心

[x1拼音（x1pinyin）](https://x1pinyin.pages.dev) 的输入法引擎核心，纯 Rust 实现：不依赖网络、不采集任何数据，用户学习数据只写本地。同一份核心经 C FFI 供 macOS（IMK / Swift）与 Windows（TSF / 服务进程）宿主链接。

## 特性

- **输入形态**：全拼、简拼、双拼（8 套内置方案 + 自定义方案 JSON）
- **整句引擎**：音节图 DP 切分 + 分层候选排序（词频 / 用户行为 / 上下文关联逐层裁决）
- **上下文学习**：bigram / trigram 三阶上下文模型、自造词固化、绿词（整句）记忆、固定候选位
- **容错**：拼音纠错（键盘误触 / 发音混淆 / 折叠输入 / 双拼双误触）
- **存储**：SQLite 词库，用户数据与系统词库分表（支持双库分离布局）
- **FFI**：全部能力经 `extern "C"` 导出，头文件已入库（`x1core/include/`，cbindgen 生成）

## 文档

| 文档 | 内容 |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | 架构：数据流、模块地图、关键设计、已知取舍与演进方向 |
| [docs/DICT.md](docs/DICT.md) | 词库数据卡：资产形态、来源口径、结构权威、维护链 |
| [docs/GLOSSARY.md](docs/GLOSSARY.md) | 词汇表：绿/白/紫/橙词等待解释术语 |
| [docs/PRIVACY.md](docs/PRIVACY.md) | 隐私：本地明文存储清单与清除方式 |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 贡献指南：开发环境、检查清单、许可与贡献条款 |
| [SECURITY.md](SECURITY.md) | 安全策略：漏洞报告渠道与威胁模型 |
| [CHANGELOG.md](CHANGELOG.md) | 更新日志 |

## 构建与测试

[![CI](https://github.com/xlaser/x1pinyinCore/actions/workflows/ci.yml/badge.svg)](https://github.com/xlaser/x1pinyinCore/actions/workflows/ci.yml)

Rust stable 工具链（CI 在 Linux / macOS / Windows 三平台验证），无需其他系统依赖。

```bash
cargo build --release
cargo test -p x1core
```

## 宿主集成（C FFI）

全部能力经 `extern "C"` 导出；头文件已生成入库：[`x1core/include/x1core_ffi.h`](x1core/include/x1core_ffi.h)（由 cbindgen 生成，重新生成方式见文件头注释）。

最小集成示例（创建 → 查询 → 上屏学习 → 释放）：[`examples/minimal.c`](examples/minimal.c)

```c
X1Engine *e = x1_engine_new();
char *json = x1_engine_process(e, "nihao");  /* 候选 JSON，用毕 x1_string_free */
x1_engine_commit(e, "你好");                  /* 上屏学习，立即生效 */
x1_engine_free(e);
```

两条契约（详见头文件头注释与 `src/ffi/` 模块文档）：

- **内存**：`x1_engine_process` 等返回的字符串必须用 `x1_string_free` 释放；引擎指针用 `x1_engine_free` 释放一次；`x1_engine_version` 返回静态串不得释放。
- **线程**：Engine 非线程安全（内部含 `RefCell` 缓存与 SQLite 连接），同一实例的全部调用必须在同一线程串行执行（输入法主线程 / 单线程事件循环）。

无完整种子时引擎自动回退内置单字表，集成即可运行；完整种子的放置见下方词库说明。

## 词库说明

仓库内置一个小型单字词库（`x1core/assets/pinyin_dict.txt`，约 3000 条，编译进二进制），开箱即可构建与运行；纯逻辑测试不需要额外数据。

完整种子词库（约 20.7 万词条，约 31.5MB SQLite 单文件）**不随源码分发**：

- 从本仓库 [Releases](../../releases) 下载 `x1pinyin.db`，放到 `x1core/assets/x1pinyin.db`；
- 词库资产与源码同为 **GPL-3.0** 许可：可自由下载、修改、再分发，衍生须以 GPL-3.0 开源；
- 未放置种子时，依赖真实词频的测试（约 200 个）会失败，失败信息中含种子获取指引；放置后全量测试通过（另有少数标记 `#[ignore]` 的长时评测测试需显式运行）；
- 环境变量 `X1_USER_DB_DIR=<目录>` 可让诊断类测试改用本机输入法数据目录（`dict.db`/`user.db` 双库或旧版单库 `x1pinyin.db`），未设置或打开失败时回退种子副本；
- 词库维护工具见 `x1core/src/tools/`（`freq_rebuild`、`seed_init` 等）与 `scripts/`（种子搭配导入）；词库资产形态、结构权威与维护红线见 [docs/DICT.md](docs/DICT.md)；
- 仓库内的文本资产（内置单字表 / 英文词表 / 中译英词表 / 评测语料）均为自建或由 DeepSeek 生成后整理，**不含任何第三方数据集**。

## 目录结构

```
x1core/
├── include/         C FFI 头文件（cbindgen 生成入库）
├── src/
│   ├── engine/      引擎状态机：查询 / 排序 / 学习 / 整句 / 纠错 / 固定候选
│   ├── storage/     SQLite 存储层（用户数据与词库分表、双库分离）
│   ├── ffi/         C FFI 导出
│   ├── tools/       维护工具（freq_rebuild / seed_init / ranker_trainer）
│   ├── dict.rs      词典与 Trie（紧凑 arena 布局）
│   ├── pinyin.rs    拼音解析与切分
│   ├── shuangpin.rs 双拼方案与解码
│   └── ...
├── assets/          内置单字表 / 英文与中英词表 / 评测语料
└── examples/        探针示例 + minimal.c（C 集成示例）
scripts/             词库维护脚本（种子搭配导入）
docs/                数据卡 / 词汇表 / 隐私说明
```

## 许可

本仓库以 **GPL-3.0** 发布（见 [LICENSE](LICENSE)）：

- 任何人可以自由使用、修改、再分发本源码；**衍生作品必须以 GPL-3.0 开源发布**（包括链接、编译或包含本源码的软件）；
- 商业使用在遵守 GPL-3.0（开源衍生作品）的前提下被允许；
- x1pinyin 作者（版权持有人）对其自有贡献及依贡献条款获得再许可授权的贡献，保留再许可的权利。如需闭源集成或商业授权，请联系作者。

贡献即表示同意以 GPL-3.0 发布并授予作者再许可权利，条款细节见
[CONTRIBUTING.md](CONTRIBUTING.md)。
