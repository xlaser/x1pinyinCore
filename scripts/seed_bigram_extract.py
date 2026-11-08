#!/usr/bin/env python3
"""自有分词语料 → x1 种子 DB bigram/trigram 搭配对 SQL 生成器。

分词语料格式：每行一句，token 形如 "词/词性"，空格分隔。
专有名词短语用方括号：[纽约/nsf 时报/n]/nz
输出 SQL：INSERT INTO seed_bigram_counts (prev, text, count) VALUES (...)
"""
import sys
import re
import os
import argparse
import math
from collections import Counter
from pathlib import Path

POS_INVALID = {'w', 'nx', 'm', 'mq', 'x'}  # 标点/外文/数词/数量词/非词
CN_CHAR_RE = re.compile(r'[\u4e00-\u9fff]+')


def parse_corpus_line(line):
    """解析一行分词语料 → [(word, pos), ...]。

    - 每 token "词/词性"，无 / 的 token 跳过
    - 方括号短语 [纽约/nsf 时报/n]/nz 跨多个空白 token：
      前导 [ 在首个 token，短语结尾 "]/nz"（外层短语词性）附在末个内部 token 后；
      处理：剥离前导 [，遇 "]/" 时取其前的内部 "word/pos"，外层短语词性丢弃。
    - 词性在末尾，用 rsplit('/', 1) 以兼容词本身含 /
    - 过滤空词
    """
    pairs = []
    for raw in line.split():
        token = raw
        if token.startswith('['):
            token = token[1:]
        if ']/' in token:  # 短语结尾：时报/n]/nz → 内部 时报/n
            token = token.split(']/', 1)[0]
        if '/' not in token:
            continue
        word, pos = token.rsplit('/', 1)
        if not word:
            continue
        pairs.append((word, pos))
    return pairs


def is_valid_word(word, pos, max_len):
    """判断 (word, pos) 是否可进入统计。

    - 长度 1~max_len
    - 纯中文（fullmatch，拒绝 "中文+数字" 等）
    - 词性不在 POS_INVALID（标点/外文/数词/数量词/非词）
    """
    if not word or len(word) < 1 or len(word) > max_len:
        return False
    if pos in POS_INVALID:
        return False
    if not CN_CHAR_RE.fullmatch(word):
        return False
    return True


def compress_count(raw_count):
    """对数压缩原始频次到 x1 种子 bigram 量级（约 5~40）。

    公式 round(log2(raw + 1) * 3)，raw=0 安全返回 0。
    raw=5 → 8、raw=100 → 20、raw=5000 → 37。

    双表方案（seed_bigram_counts 独立表，用户优先由查询层硬规则保证）下
    系数取 ×3，让种子加分可区分高频/低频搭配；用户 commit 1 次即完全覆盖
    种子（查询层用户优先，不看 count 量级）。
    """
    if raw_count <= 0:
        return 0
    return round(math.log2(raw_count + 1) * 3)


def extract_bigrams(input_path, max_word_len):
    """统计语料 bigram 频次 → Counter（按行处理，bigram 不跨句）。"""
    bigrams, _ = extract_ngrams(input_path, max_word_len)
    return bigrams


def extract_ngrams(input_path, max_word_len):
    """单次扫描同产 bigram + trigram 频次 → (bigram Counter, trigram Counter)。

    避免对大语料扫两遍；trigram 同样不跨句。

    字+字不记（与引擎 Freq::record_word_bigram 的 choke point 同一规则）：
    bigram 两侧均单字跳过；trigram 的 (prev1, curr) 过渡均单字跳过。
    两个单字高频相邻应由自造词管线承载，字↔字统计只有噪声价值。
    """
    bigrams = Counter()
    trigrams = Counter()
    with open(input_path, encoding='utf-8') as f:
        for line in f:
            words = [w for w, pos in parse_corpus_line(line)
                     if is_valid_word(w, pos, max_word_len)]
            for prev, curr in zip(words, words[1:]):
                if len(prev) == 1 and len(curr) == 1:
                    continue  # 字+字不记
                bigrams[(prev, curr)] += 1
            for p2, p1, curr in zip(words, words[1:], words[2:]):
                if len(p1) == 1 and len(curr) == 1:
                    continue  # 字+字过渡不记
                trigrams[(p2, p1, curr)] += 1
    return bigrams, trigrams


def load_dict_words(path):
    """加载词库词表（每行一词，来自 sqlite3 x1dict_words.simplified 导出）→ set。

    词库过滤：搭配对两侧词都在词库才保留——保证每对都能在查询中真正命中，
    顺带滤掉语料专名/噪音对，控制表体积。
    """
    with open(path, encoding='utf-8') as f:
        return {line.strip() for line in f if line.strip()}


def filter_counter_by_dict(counter, dict_words):
    """过滤 Counter：n-gram 元组的每个词都必须 in dict_words（泛化 2/3 元组）。"""
    return Counter({ng: c for ng, c in counter.items()
                    if all(w in dict_words for w in ng)})


def write_sql(output_path, top_pairs):
    """写种子 SQL 到 output_path。

    top_pairs: [((prev, curr), raw_count), ...]，按降序。
    频次经 compress_count 压缩到 x1 量级后入库。
    """
    lines = [
        "-- 本文件由 scripts/seed_bigram_extract.py 自动生成，勿手改。",
        "-- 数据源：自有分词语料；频次经 round(log2(raw+1)*3) 压缩",
        "-- 以与 x1 用户日常 commit 量级（约 3~50）对齐。",
        "BEGIN TRANSACTION;",
    ]
    for (prev, curr), raw_count in top_pairs:
        seed_count = compress_count(raw_count)
        prev_esc = prev.replace("'", "''")
        curr_esc = curr.replace("'", "''")
        # 双表方案：种子表与用户表分离；表形态 WITHOUT ROWID（见 storage/stats.rs）
        lines.append(
            f"INSERT OR REPLACE INTO seed_bigram_counts (prev, text, count) "
            f"VALUES ('{prev_esc}', '{curr_esc}', {seed_count});"
        )
    lines.append("COMMIT;")
    # 路径安全：归一化 + 禁止穿越
    safe_path = os.path.realpath(output_path)
    if '..' in safe_path.split(os.sep):
        raise ValueError(f"拒绝非法输出路径（含 ..）: {output_path}")
    Path(safe_path).write_text('\n'.join(lines) + '\n', encoding='utf-8')


def write_trigram_sql(output_path, top_triples):
    """写种子 trigram SQL。

    top_triples: [((prev2, prev1, curr), raw_count), ...]，按降序。
    表：seed_trigram_counts(prev2, prev1, text, count)，WITHOUT ROWID 形态。
    频次压缩与 bigram 同一公式（log2×3）。
    """
    lines = [
        "-- 本文件由 scripts/seed_bigram_extract.py 自动生成，勿手改。",
        "-- 数据源：自有分词语料；种子 trigram（三阶上下文搭配）",
        "BEGIN TRANSACTION;",
    ]
    for (p2, p1, curr), raw_count in top_triples:
        seed_count = compress_count(raw_count)
        p2_esc = p2.replace("'", "''")
        p1_esc = p1.replace("'", "''")
        curr_esc = curr.replace("'", "''")
        lines.append(
            f"INSERT OR REPLACE INTO seed_trigram_counts (prev2, prev1, text, count) "
            f"VALUES ('{p2_esc}', '{p1_esc}', '{curr_esc}', {seed_count});"
        )
    lines.append("COMMIT;")
    # 路径安全：归一化 + 禁止穿越
    safe_path = os.path.realpath(output_path)
    if '..' in safe_path.split(os.sep):
        raise ValueError(f"拒绝非法输出路径（含 ..）: {output_path}")
    Path(safe_path).write_text('\n'.join(lines) + '\n', encoding='utf-8')


def main():
    """命令行入口：自有语料 → 种子 bigram SQL（可选同产 trigram SQL）。"""
    parser = argparse.ArgumentParser(
        description="自有分词语料 → x1 种子 DB bigram/trigram SQL 生成器")
    parser.add_argument('input', help='自有语料路径')
    parser.add_argument('output', help='输出 bigram SQL 路径')
    parser.add_argument('--top', type=int, default=15000,
                        help='取频次最高的前 N 个 bigram（默认 15000）')
    parser.add_argument('--min-count', type=int, default=5,
                        help='原始频次阈值（默认 5）')
    parser.add_argument('--max-word-len', type=int, default=4,
                        help='词最大长度（默认 4）')
    parser.add_argument('--dict-words', default=None,
                        help='词库词表文件（每行一词）；提供后只保留两侧都在词库的搭配')
    parser.add_argument('--trigram-output', default=None,
                        help='可选：trigram SQL 输出路径（提供则同一次扫描产出）')
    parser.add_argument('--trigram-top', type=int, default=100000,
                        help='取频次最高的前 N 个 trigram（默认 100000）')
    parser.add_argument('--trigram-min-count', type=int, default=5,
                        help='trigram 原始频次阈值（默认 5）')
    args = parser.parse_args()

    dict_words = load_dict_words(args.dict_words) if args.dict_words else None
    if dict_words is not None:
        print(f"词库过滤启用：{len(dict_words)} 词（{args.dict_words}）")

    raw_counter, tri_counter = extract_ngrams(args.input, args.max_word_len)
    print(f"原始 bigram 数：{len(raw_counter)}，原始 trigram 数：{len(tri_counter)}")

    if dict_words is not None:
        raw_counter = filter_counter_by_dict(raw_counter, dict_words)
        tri_counter = filter_counter_by_dict(tri_counter, dict_words)
        print(f"词库过滤后：bigram {len(raw_counter)}，trigram {len(tri_counter)}")

    filtered = [(pair, c) for pair, c in raw_counter.items() if c >= args.min_count]
    print(f"≥ min_count={args.min_count} 过滤后：{len(filtered)}")

    filtered.sort(key=lambda x: x[1], reverse=True)
    top_pairs = filtered[:args.top]
    print(f"取 top {args.top}：实际写出 {len(top_pairs)} 条")

    write_sql(args.output, top_pairs)
    print(f"已写入 {args.output}")

    if args.trigram_output:
        tri_filtered = [(t, c) for t, c in tri_counter.items()
                        if c >= args.trigram_min_count]
        print(f"trigram ≥ min_count={args.trigram_min_count} 过滤后：{len(tri_filtered)}")
        tri_filtered.sort(key=lambda x: x[1], reverse=True)
        top_triples = tri_filtered[:args.trigram_top]
        print(f"trigram 取 top {args.trigram_top}：实际写出 {len(top_triples)} 条")
        write_trigram_sql(args.trigram_output, top_triples)
        print(f"已写入 {args.trigram_output}")


if __name__ == "__main__":
    main()
