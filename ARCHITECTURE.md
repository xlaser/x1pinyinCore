# 架构

x1core 是一个纯 Rust 的拼音输入法引擎库：无网络依赖、无遥测，通过 C FFI
被宿主输入法（macOS / Windows 前端）链接。本文描述整体结构、数据流与
已知取舍。

## 总览：一次按键的生命周期

```
按键串（如 "xiwangnih"）
   │
   ▼
拼音解析（pinyin.rs）
   全拼/简拼/混合切分（DFS 枚举 + 配额）；双拼方案解码（shuangpin.rs）；
   模糊音展开（fuzzy.rs，带数量上限护栏）；纠错变体（engine/correction.rs）
   │
   ▼
候选收集（engine/query.rs + dict.rs Trie）
   音节图逐边查询 Trie；长词延展猜测（紫词）；英文词表（english.rs /
   translation.rs，独立表）
   │
   ▼
整句解码（engine/sentence.rs）
   音节图 DP 切分 → N-best 路径 → 绿词（整句候选）；
   双向词格重打分（engine/backward.rs，右文 bigram 二次打分）
   │
   ▼
分层排序（engine/scoring.rs）
   键预计算 + 元组排序（tier 分层 → collocation → 强 bigram → 打分 →
   常用度 → 确定性全序）；参考比较器双实现互证（测试守护等价）
   │
   ▼
JSON 候选（ffi/）──→ 宿主候选窗
   │
   ▼ 用户上屏
commit 学习（engine/learning.rs）
   词频 / 拆词 bigram·trigram / 输入映射 / 绿词记忆 / 固定候选 / 竞争退化通道
   │
   ▼ 定期 flush
SQLite（storage/）：内存优先 + 增量落盘
```

## 模块地图

```
x1core/src/
├── pinyin.rs          拼音模型：音节表、切分（全拼/简拼/混合）、归一化
├── shuangpin.rs       双拼：8 套内置方案 + 自定义方案 JSON 解析
├── fuzzy.rs           模糊音（多组可开关，展开带上限）
├── dict.rs            词典与 Trie（Trie arena：见下）
├── char_freq.rs       单字频率与主音判定
├── freq.rs            词频模型与上下文调制（近期衰减、竞争退化）
├── lm.rs / trigram.rs 插值 n-gram 语言模型（unigram/bigram/trigram）
├── competition.rs     竞争频率存储（同音竞争的选择分布决定权重）
├── engine/
│   ├── mod.rs         Engine 状态机：查询编排、配置、启动加载
│   ├── query.rs       候选收集（音节图逐边、紫词延展、英文补全）
│   ├── sentence.rs    整句 DP：N-best 切分、绿词生成、弱搭配惩罚
│   ├── backward.rs    双向词格重打分：右文证据对 N-best 二次排序
│   ├── scoring.rs     分层排序：打分分量、排序键、参考比较器
│   ├── learning.rs    commit 学习闭环与删词
│   ├── correction.rs  智能纠错（邻键误触/发音混淆/双写折叠/双拼双误触）
│   ├── ambiguity.rs   同音歧义判定与上下文证据
│   ├── pins.rs        固定候选位（用户钉死 1-3 号位）
│   ├── english.rs / translation.rs  英文补全与中英互译副栏
│   └── dict_admin.rs  词库管理与 flush（失败回填、空态覆写闸）
├── storage/           SQLite 层：单库 / 双库分离布局、迁移、快照 flush、
│                      词典缓存（dict_cache.rs + mmap_file.rs）
├── stats/             可选的输入统计（独立 x1stats.db，宿主显式启用）
├── ffi/               C ABI 导出（guard 护栏、JSON 序列化、契约文档）
├── platform.rs        平台差异垫片（进程内存诊断等，engine 保持平台无关）
├── quickcmds.rs       斜杠快捷输入（/date、/upper 等）
└── symbols.rs         标点与符号表
```

## 关键设计

### Trie arena

词典不是指针节点树，而是紧凑数组布局：DFS 前序 id + CSR 连续段表示子节点，
音节段内排序支持二分，字符串进 blob 池。19 万节点压进少数几个精确预留的
`Vec`，查询零指针追逐、缓存友好。不可变基底 + overlay（用户词覆盖表 +
删除墓碑）——用户修改不重建基底，重启后由 DB 重建同构结构。

### 内存优先 + 增量 flush

学习写入先落内存模型，dirty 标记驱动定期批量落盘；flush 采用
"写成功才清 pending，失败回填重试"语义，并有空态覆写闸（启动加载失败时
禁止用空快照覆写用户表）。

### 双库分离布局

默认单库（`x1pinyin.db`）。宿主可选分离布局：`dict.db`（词库，可被种子
整体覆盖）+ `user.db`（用户数据，永不触碰）——词库更新与用户数据隔离。
`db_meta.x1dict_sync_hash` 是词库内容哈希，用户库启动比对后增量同步。

### 排序：分层全序 + 双实现互证

排序语义集中在分层判定（tier → collocation 硬规则 → 强 bigram 硬规则 →
打分 → 常用度 → 字数 → 拼音/文本全序）。生产路径用键预计算（每候选一次
查询）+ 元组排序；另保留一份逐对比较的参考实现，由等价性测试逐位守护两
种实现一致——结构可以换，语义契约被测试钉死。

### FFI 纪律

111 个导出全部带契约文档；非平凡路径统一经 `catch_unwind` 护栏（panic
返回默认值并写文件日志，绝不跨 FFI 边界传播）；内存单属主（`x1_string_free`
唯一释放口）；线程契约成文（Engine 非线程安全，单线程串行调用）。

## 已知取舍与演进方向

诚实列出当前方案的局限（都是有意取舍而非疏忽）：

1. **模糊音在查询期展开**，而非编译进音节表（spelling algebra 路线）。
   查询期方案实现简单、可运行时开关，代价是歧义音节数量组合增长——已有
   数量上限护栏（超限回退不展开），但极端配置仍有理论膨胀空间。演进方向：
   音节代数编译。
2. **Trie 节点非最小化**（无后缀合并 / DAWG 类压缩），每节点多数组平行
   表示。当前内存与启动速度达标（见 `examples/probe_mem_user_words.rs`
   的实测口径），词库数量级增长后再考虑 FST/后缀压缩。
3. **启动从 SQLite 重建 Trie**，而非 mmap 零拷贝直接加载。以磁盘
   bigram 缓存 + 词典编译缓存（`dict_cache.rs`）缓解；mmap 直接布局是
   明确的演进方向。
4. **排序权重为手工调优整数群**，靠量级注释互相约束；提供学习型排序器
   注入口（`tools/ranker_trainer` 离线训练注入），默认不启用。演进方向：
   训练定标文档化与默认化。
5. **词库以二进制 SQLite 为资产本体**（决策与维护链见
   [docs/DICT.md](docs/DICT.md)），不自持"文本词表 → 库"的完整重建管线
   （`seed_init` 仅自举最小库）。

## 测试体系

- 纯逻辑测试：无种子即可运行（CI 三平台常跑）；
- 种子测试：放置完整种子后全量通过（约 680 项，含排序矩阵、整句正确率
  守护、FFI JSON 契约、行为回放评测）；
- 双实现互证：排序键 vs 参考比较器、NgramStore vs 参照实现的随机操作
  对拍；
- 平台纯净性测试：守护 engine 层不引入平台绑定。
