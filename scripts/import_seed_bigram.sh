#!/bin/bash
# import_seed_bigram.sh — 用自有分词语料生成 bigram 搭配对，导入种子 DB
#
# 用法：
#   ./scripts/import_seed_bigram.sh                       # 默认语料 + 默认种子 DB
#   ./scripts/import_seed_bigram.sh /path/to/output.db    # 指定输出 DB
#   CORPUS_FILE=/custom/path.txt ./scripts/import_seed_bigram.sh
#   TOP_N=20000 ./scripts/import_seed_bigram.sh           # 自定义 top N
#
# 前置：自备分词语料（自有语料，协议需允许再分发，如 CC0），默认放 dict/corpus.txt，
#   或用 CORPUS_FILE 指定。格式：每行一句，token 形如 "词/词性"。
#
# 流程：
#   1. Python 提取搭配对 → SQL
#   2. 备份种子 DB 的 seed_bigram_counts（防御）
#   3. 清空 + 导入新搭配对
#   4. 写 db_meta 标记（seed_bigram_loaded/hash/count）
#   5. freq_rebuild --hash-only 更新 x1dict_sync_hash（触发用户 DB 侧同步）
#   6. 验证测试

set -e

# bash 3.2（macOS 自带）在 UTF-8 locale 下展开「$VAR 紧跟多字节字符」会吞掉变量值。
# 强制 LC_ALL=C 按字节处理，避免隐蔽破坏。
export LC_ALL=C

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

# 路径配置
CORPUS_FILE="${CORPUS_FILE:-$ROOT_DIR/dict/corpus.txt}"
SEED_DB="${1:-$ROOT_DIR/x1core/assets/x1pinyin.db}"
TOP_N="${TOP_N:-15000}"
MIN_COUNT="${MIN_COUNT:-5}"
# 词库过滤默认开（搭配两侧词必须在 x1dict_words，保证可命中 + 控体积）。
# DICT_FILTER=0 关闭。
DICT_FILTER="${DICT_FILTER:-1}"
# 种子 trigram：TRIGRAM_TOP>0 时同一次扫描产出并导入 seed_trigram_counts。
TRIGRAM_TOP="${TRIGRAM_TOP:-0}"
TRIGRAM_MIN_COUNT="${TRIGRAM_MIN_COUNT:-5}"
# 中间产物写 mktemp 私有目录（0700）+ EXIT trap 清理：共享 /tmp 里的固定名字
# 可被同机其他用户预置符号链接，重定向写穿 / `< file` 执行掉包 SQL 都是攻击面。
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/x1_seed_bigram.XXXXXX")"
trap '[ -n "$TMP_DIR" ] && rm -rf "$TMP_DIR"' EXIT
TMP_SQL="$TMP_DIR/seed_bigram.sql"
TMP_TRI_SQL="$TMP_DIR/seed_trigram.sql"
DICT_WORDS_FILE="$TMP_DIR/x1dict_words.txt"
# 备份留在仓库 backups/（放临时目录会被 EXIT trap 删掉，回滚指引就成死指引）。
mkdir -p "$ROOT_DIR/backups"
BACKUP_SQL="$ROOT_DIR/backups/seed_bigram_backup_$(date +%s).sql"

# 颜色输出
info()  { echo -e "\033[1;32m=== $* ===\033[0m"; }
warn()  { echo -e "\033[1;33m[!] $*\033[0m"; }
error() { echo -e "\033[1;31m[x] $*\033[0m"; exit 1; }

# 1. 检查前置
info "检查前置条件"
[[ -f "$CORPUS_FILE" ]] || error "分词语料不存在：$CORPUS_FILE（自备语料放 dict/ 或 CORPUS_FILE 指定）"
[[ -f "$SEED_DB" ]] || error "种子 DB 不存在：$SEED_DB"
command -v python3 >/dev/null || error "python3 未安装"
command -v sqlite3 >/dev/null || error "sqlite3 未安装"
echo "  语料：$CORPUS_FILE ($(du -h "$CORPUS_FILE" | cut -f1))"
echo "  种子 DB：$SEED_DB ($(du -h "$SEED_DB" | cut -f1))"
echo "  top N：$TOP_N，最低频次：$MIN_COUNT，词库过滤：$DICT_FILTER，trigram top：$TRIGRAM_TOP"

# 2. Python 提取搭配对
info "提取 bigram 搭配对（Python，语料较大可能需 30-60 秒）"
EXTRACT_ARGS=("$CORPUS_FILE" "$TMP_SQL" --top "$TOP_N" --min-count "$MIN_COUNT")
if [[ "$DICT_FILTER" == "1" ]]; then
    # 词库过滤：导出 x1dict_words.simplified 词表，extract 只保留两侧在词库的搭配
    sqlite3 "$SEED_DB" "SELECT DISTINCT simplified FROM x1dict_words;" > "$DICT_WORDS_FILE"
    echo "  词库词表：$(wc -l < "$DICT_WORDS_FILE" | tr -d ' ') 词 → $DICT_WORDS_FILE"
    EXTRACT_ARGS+=(--dict-words "$DICT_WORDS_FILE")
fi
if [[ "$TRIGRAM_TOP" -gt 0 ]]; then
    EXTRACT_ARGS+=(--trigram-output "$TMP_TRI_SQL" --trigram-top "$TRIGRAM_TOP" --trigram-min-count "$TRIGRAM_MIN_COUNT")
fi
python3 "$ROOT_DIR/scripts/seed_bigram_extract.py" "${EXTRACT_ARGS[@]}"

[[ -s "$TMP_SQL" ]] || error "SQL 文件为空，提取失败"
echo "  生成 SQL：$TMP_SQL ($(du -h "$TMP_SQL" | cut -f1))"

# 3. 备份种子 DB 当前 seed_bigram_counts（防御）
info "备份 seed_bigram_counts"
sqlite3 "$SEED_DB" ".dump seed_bigram_counts" > "$BACKUP_SQL" 2>/dev/null || true
BACKUP_LINES=$(wc -l < "$BACKUP_SQL")
echo "  备份 → $BACKUP_SQL ($BACKUP_LINES 行)"

# 4. 建表（如不存在）+ 清空 + 导入
#    双表方案：seed_bigram_counts 与 user_bigram_counts 分离。
#    DROP+CREATE（非 IF NOT EXISTS）：导入前重建保证形态一致（老形态库直接换代）。
info "建表 + 清空 + 导入新搭配对"
sqlite3 "$SEED_DB" "
    DROP TABLE IF EXISTS seed_bigram_counts;
    CREATE TABLE seed_bigram_counts (
        prev  TEXT NOT NULL,
        text  TEXT NOT NULL,
        count INTEGER NOT NULL DEFAULT 1,
        PRIMARY KEY (prev, text)
    ) WITHOUT ROWID;
"
sqlite3 "$SEED_DB" < "$TMP_SQL"
COUNT=$(sqlite3 "$SEED_DB" "SELECT COUNT(*) FROM seed_bigram_counts;")
echo "  导入 $COUNT 条搭配对到 seed_bigram_counts"

# 4b. 种子 trigram（TRIGRAM_TOP>0 时）：建表 + 清空 + 导入
TRI_COUNT=0
if [[ "$TRIGRAM_TOP" -gt 0 ]]; then
    [[ -s "$TMP_TRI_SQL" ]] || error "trigram SQL 为空，提取失败"
    info "导入种子 trigram（seed_trigram_counts）"
    sqlite3 "$SEED_DB" "
        DROP TABLE IF EXISTS seed_trigram_counts;
        CREATE TABLE seed_trigram_counts (
            prev2 TEXT NOT NULL,
            prev1 TEXT NOT NULL,
            text  TEXT NOT NULL,
            count INTEGER NOT NULL DEFAULT 1,
            PRIMARY KEY (prev2, prev1, text)
        ) WITHOUT ROWID;
    "
    sqlite3 "$SEED_DB" < "$TMP_TRI_SQL"
    TRI_COUNT=$(sqlite3 "$SEED_DB" "SELECT COUNT(*) FROM seed_trigram_counts;")
    echo "  导入 $TRI_COUNT 条三元组到 seed_trigram_counts"
fi

# 5. 写 db_meta 标记
info "写 db_meta 标记"
if command -v md5 >/dev/null 2>&1; then
    HASH=$(md5 -q "$TMP_SQL")
elif command -v md5sum >/dev/null 2>&1; then
    HASH=$(md5sum "$TMP_SQL" | cut -d' ' -f1)
else
    HASH="unknown"
fi
sqlite3 "$SEED_DB" "
    INSERT OR REPLACE INTO db_meta(key, value) VALUES('seed_bigram_loaded','true');
    INSERT OR REPLACE INTO db_meta(key, value) VALUES('seed_bigram_hash','$HASH');
    INSERT OR REPLACE INTO db_meta(key, value) VALUES('seed_bigram_count','$COUNT');
"
echo "  seed_bigram_loaded=true, hash=${HASH:0:12}, count=$COUNT"
if [[ "$TRIGRAM_TOP" -gt 0 ]]; then
    sqlite3 "$SEED_DB" "
        INSERT OR REPLACE INTO db_meta(key, value) VALUES('seed_trigram_loaded','true');
        INSERT OR REPLACE INTO db_meta(key, value) VALUES('seed_trigram_count','$TRI_COUNT');
    "
    echo "  seed_trigram_loaded=true, count=$TRI_COUNT"
fi

# 6. 更新 x1dict_sync_hash（调 freq_rebuild --hash-only）：
# **必须用 --hash-only**：本步唯一目的是重写 x1dict_sync_hash——
# x1dict_content_hash 已覆盖 seed_bigram_counts，hash 一变用户 DB 启动比对即触发
# sync_x1dict_from_builtin 同步种子 bigram 到用户 DB。
# 禁止用 --skip-flatten / 全量重算：那会连带 compute_freq 重算全部 freq，
# 摧毁手工调优的 freq 谱（种子 freq 谱是维护数据，compute_freq 无法重现）。
# --hash-only 只写 hash，不动 freq/用户表。
#
# **顺序关键**：必须先导入 seed_bigram（步骤 4-5），再跑 freq_rebuild，
# 否则算出的 hash 不含新种子。
info "freq_rebuild --hash-only 更新 x1dict_sync_hash（含 seed_bigram_counts，触发用户 DB sync）"
cargo run -p x1core --release --bin freq_rebuild -- "$SEED_DB" --hash-only 2>&1 | tail -5

# 7. 验证
info "验证：种子 DB 大小 + 抽样搭配"
echo "  种子 DB 大小：$(du -h "$SEED_DB" | cut -f1)"
echo "  seed_bigram_counts 总数：$COUNT"
echo "  user_bigram_counts 总数（应为 0，种子 DB 无用户数据）：$(sqlite3 "$SEED_DB" 'SELECT COUNT(*) FROM user_bigram_counts;' 2>/dev/null || echo 0)"
echo "  抽样 top 10 种子搭配对（按 count 降序）："
sqlite3 -header -column "$SEED_DB" "SELECT prev, text, count FROM seed_bigram_counts ORDER BY count DESC LIMIT 10;"

info "完成。建议运行：cargo test -p x1core --release --lib -- diag_matrix_real_db"
warn "如需回滚：sqlite3 $SEED_DB < $BACKUP_SQL"
