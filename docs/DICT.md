# 词库数据卡

本文说明 x1core 词库的资产形态、数据来源、结构权威与维护链。

## 资产形态：种子 DB 即词库本体

完整词库以 SQLite 单文件（`x1pinyin.db`，约 20.7 万词条 / 约 31.5MB）分发——
词库经长期维护演化为自有资产，**DB 文件本体即唯一权威**，不存在"从上游
词表重建"的管线（历史导入链已移除）。

- 许可：与源码同为 **GPL-3.0**——任何人可下载、编辑、再分发，衍生作品
  （含修改后的词库）须以 GPL-3.0 开源发布；商业使用遵守同一条款；
- 获取：从 GitHub [Releases](https://github.com/xlaser/x1pinyinCore/releases)
  下载 `x1pinyin.db`，放到 `x1core/assets/x1pinyin.db`；
- 审计：SQLite 明文库，任何标准工具可查可 diff（见下文"审计速查"）；
- 形态：VACUUM 归档后的单文件（无 `-wal`/`-shm` 边车依赖）；
- 纯净性：发布版用户表为近似空态（`user_words` 仅少量测试残留、
  `user_input_mappings` 为 0），不含任何真实用户输入历史。

## 数据来源口径

| 资产 | 来源 |
|---|---|
| 内置单字表 `pinyin_dict.txt`（约 3000 条，编译进二进制） | DeepSeek 生成后整理 |
| 英文词表 `english_words.tsv` / 中译英词表 `zh_en_words.tsv` | DeepSeek 生成后整理 |
| 种子搭配（bigram/trigram 统计） | 自有分词语料统计（语料协议 CC0） |
| 完整种子 DB 词条本体 | 长期维护的自有资产（多轮增补与调优演化） |
| 评测语料 `assets/eval/*.tsv` | 自建原创句集 |

**不含任何第三方数据集。**

## 结构权威

- 表结构以代码为唯一权威：`x1core/src/storage/mod.rs`（DDL）与
  `x1core/src/storage/schema.rs`（结构版本三态校验：Ok / Legacy / Newer）。
- 单库布局：用户表（`user_*`，12 张）+ 词库表（`x1dict_words` 等）+ 英文/中英
  词表（`english_words` / `zh_en_words`）+ `db_meta`。
- 双库分离布局（宿主可选）：`dict.db`（词库三表）+ `user.db`（用户表），
  见 `storage/split.rs`。

### 审计速查

```sh
sqlite3 x1core/assets/x1pinyin.db ".tables"
sqlite3 x1core/assets/x1pinyin.db "SELECT COUNT(*) FROM x1dict_words;"
sqlite3 x1core/assets/x1pinyin.db \
  "SELECT prev, text, count FROM seed_bigram_counts ORDER BY count DESC LIMIT 10;"
sqlite3 x1core/assets/x1pinyin.db "SELECT * FROM db_meta;"
```

## 维护链（随仓库分发的工具）

| 工具 | 用途 |
|---|---|
| `freq_rebuild`（bin） | 重算/修正 `x1dict_words.freq`；`--hash-only` 只更新内容哈希（外部数据变更后必跑）；`--normalize` 收敛 WAL 形态 |
| `scripts/import_seed_bigram.sh` + `scripts/seed_bigram_extract.py` | 自有 CC0 分词语料 → 种子 bigram/trigram 搭配对导入（含备份与哈希联动） |
| `examples/regen_seed_cache.rs` | 种子 bigram 磁盘缓存（`seed_bigram_cache.bin`）再生 |
| `seed_init`（bin） | **自举**：无完整种子时，从仓库自带 TSV 构建结构完整的最小词库 DB（开发/CI 用，覆盖面以单字为主） |

维护红线（口径）：**禁止对种子/用户 DB 跑 `freq_rebuild` 全量重算**——种子的
freq 谱是手工调优数据，`compute_freq` 无法重现；外部数据变更后只跑
`--hash-only`。

## 增量同步机制（宿主集成者需知）

`db_meta.x1dict_sync_hash` 是词库内容哈希（覆盖 `x1dict_words` 与种子搭配表）。
宿主侧用户 DB 启动时比对哈希，不一致即触发从种子到用户库的增量同步
（`sync_x1dict_from_builtin`）——修改种子后必须用 `freq_rebuild --hash-only`
重写哈希，否则用户侧不感知。
