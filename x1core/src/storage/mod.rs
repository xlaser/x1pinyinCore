//! SQLite 存储管理器
//!
//! 职责：
//! 1. 管理 `x1pinyin.db`（X1Dict 基础词库 + user_words 用户词库）
//! 2. 提供 X1Dict 批量导入接口（供词库管理器/维护工具调用）
//! 3. 提供按拼音前缀/简拼查询的批量加载接口（启动时填充 Trie）
//! 4. 提供用户词频的读取 / 批量 flush 接口（延迟写入落地）
//!
//! 查询策略说明：
//! - 输入法热路径要求 <10ms 响应，SQL LIKE 扫描 12 万行不可接受。
//! - 因此启动时一次性 SELECT 全表加载进内存 Trie（dict.rs），
//!   SQLite 仅作为持久化层 + 用户词库层。例外：seed_trigram 按需 SQL
//!   查询（trigram_count_with_seed，全量加载会多花 30~60ms 启动时间，故
//!   查询路径走 prepare_cached 单条查询）与 collocation_freq 组合词查询——
//!   这两条是仅存的逐键 SQL 路径。
//! - 用户选词后更新内存 FreqModel + 标记 dirty，由定时器 / 退出时
//!   调用 `flush_user_freq()` 批量落盘。

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;

/// X1Dict 词条（对应 `x1dict_words` 表一行）
///
/// 输入法不充当翻译工具，已移除 traditional/pinyin_tones/english 字段；
/// 旧词表原始三字段仅在导入解析时用作中间变量，不入库。
#[derive(Debug, Clone)]
pub struct X1DictWord {
    pub simplified: String,
    pub pinyin: String, // 无声调，如 "ni hao"
    pub freq: i64,
    /// 该 (字/词, 拼音) 组合中，当前拼音是否为该字的主音。
    ///
    /// 仅对多音单字有意义：主音 = 现代汉语最常用读音（见 `char_freq::is_primary_reading`）。
    /// 排序时主音字在精确匹配同分组内优先于次音字，避免「地」(主音 di，次音 de)
    /// 在 de 查询里压过「德」(只有 de 一个读音)。默认 `true`（非多音字/未收录字不误伤）。
    pub is_primary: bool,
}

impl Default for X1DictWord {
    /// 默认 is_primary = true（非多音字/未收录字不误伤）。
    /// 用于测试构造时用 `..Default::default()` 补全该字段。
    fn default() -> Self {
        Self {
            simplified: String::new(),
            pinyin: String::new(),
            freq: 0,
            is_primary: true,
        }
    }
}

/// 用户词频记录（对应 `user_words` 表一行）
#[derive(Debug, Clone)]
pub struct UserWord {
    pub text: String,
    pub pinyin: String,
    pub user_freq: i64,
}

/// SQLite 存储管理器（双库分离）
///
/// 角色化连接：
/// - `dict`：系统词库连接 —— `x1dict_words` + `seed_bigram_counts` +
///   `seed_trigram_counts` + 种子侧 `db_meta`。可被种子母版整体覆盖。
/// - `user`：用户库连接 —— 全部 `user_*` 表 + 用户侧 `db_meta`。
///   唯一不可再生资产，更新永不触碰。
///
/// `user == None` 为合并模式（dict 与 user 同处一个文件，单连接）：
/// 存量单库工具与绝大部分测试走此路径，行为与拆分前逐位一致。
/// 分离模式由 `open_split` 建立，运行时经 `user()` / `user_mut()` 取用户连接。
pub struct Storage {
    dict: Connection,
    user: Option<Connection>,
    /// 系统词库 DB 路径（缓存文件 `seed_bigram_cache.bin` 与它同目录；
    /// `open_readonly` 打开的辅助库为 None——不需要缓存落盘）。
    dict_path: Option<std::path::PathBuf>,
}

impl Storage {
    /// 系统词库连接（只读用途：跨库读取 builtin/source DB 的辅助路径）。
    #[inline]
    pub(crate) fn conn(&self) -> &Connection {
        &self.dict
    }

    /// 用户库连接（合并模式回退到 dict 连接——即拆分前的单库行为）。
    #[inline]
    pub(crate) fn user(&self) -> &Connection {
        self.user.as_ref().unwrap_or(&self.dict)
    }

    /// 用户库连接（可变）。合并模式回退到 dict 连接。
    #[inline]
    pub(crate) fn user_mut(&mut self) -> &mut Connection {
        match self.user.as_mut() {
            Some(c) => c,
            None => &mut self.dict,
        }
    }

    /// 是否双库分离模式。
    #[inline]
    pub fn is_split(&self) -> bool {
        self.user.is_some()
    }

    /// 系统词库 DB 的路径（分离/合并模式均有；只读打开的辅助库为 None）。
    #[inline]
    pub(crate) fn dict_path(&self) -> Option<&Path> {
        self.dict_path.as_deref()
    }
}

impl Storage {
    /// 打开双库分离模式：系统词库 + 用户库两个独立文件。
    ///
    /// 两库各自建自己的表（`init_schema_dict` / `init_schema_user`）——分离后
    /// 单个文件只含自己角色的表，schema 与所有权一致（用户库体积小、可单独备份）。
    /// 用户库文件不存在时按建库语义打开（目录须已存在，由调用方保证）。
    pub fn open_split<P: AsRef<Path>, Q: AsRef<Path>>(
        dict_path: P,
        user_path: Q,
    ) -> rusqlite::Result<Self> {
        #[cfg(test)]
        {
            Self::guard_repo_seed_open(dict_path.as_ref());
            Self::guard_repo_seed_open(user_path.as_ref());
        }
        let mut storage = Self {
            dict: Self::open_raw(dict_path.as_ref())?,
            user: Some(Self::open_raw(user_path.as_ref())?),
            dict_path: Some(dict_path.as_ref().to_path_buf()),
        };
        storage.init_schema_dict()?;
        storage.init_schema_user()?;
        Ok(storage)
    }

    /// 打开（或创建）指定路径的单库（合并模式 = 拆分前的行为）。
    ///
    /// 自动初始化 schema（全部表）；若 x1dict_words 为空表，调用方需先导入 X1Dict。
    ///
    /// 不在启动时做 DB 完整性检查/修复——保证启动秒开。
    /// DB 备份 + REINDEX + VACUUM + 脏数据清理统一在部署期完成，不拖累运行时启动。
    /// 如遇 DB 损坏，运行时静默忽略错误，下次 install/覆盖修复。
    pub fn open<P: AsRef<Path>>(path: P) -> rusqlite::Result<Self> {
        #[cfg(test)]
        Self::guard_repo_seed_open(path.as_ref());
        let mut storage = Self {
            dict: Self::open_raw(path.as_ref())?,
            user: None,
            dict_path: Some(path.as_ref().to_path_buf()),
        };
        storage.init_schema()?;
        Ok(storage)
    }

    /// 守护：测试禁止以可写方式打开仓库种子。
    ///
    /// 背景：套件偶发出现「种子 db_meta 的 x1dict_sync_hash 被改写 +
    /// assets/seed_bigram_cache.bin 被重写 + assets/*.arena_cache.bin 被生成」的
    /// 写脏形态（触发点未锁定）。写脏链的起点
    /// 恒为「可写打开仓库种子」：在这里直接 panic，肇事测试立即显形
    ///（线程名 = 测试名），且种子不会再被污染。副本（target/test-tmp）不受影响。
    #[cfg(test)]
    fn guard_repo_seed_open(path: &Path) {
        let seed = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join("x1pinyin.db");
        let same = path == seed.as_path()
            || matches!(
                (path.canonicalize(), seed.canonicalize()),
                (Ok(a), Ok(b)) if a == b
            );
        if same {
            panic!(
                "测试禁止可写打开仓库种子 {:?}（先复制到 target/test-tmp 再开；\
                 测试线程={:?}）",
                path,
                std::thread::current().name()
            );
        }
    }

    /// 连接建立 + PRAGMA 调优（open / open_split 共用）。
    fn open_raw(path: &Path) -> rusqlite::Result<Connection> {
        let conn = Connection::open(path)?;
        // SQLite mmap 上限默认 256MiB。X1_MMAP_SIZE 仅作调试/基准旋钮
        //（无产品「内存模式」设置：Windows 实测关 mmap 不省
        // 峰值内存——大头是 Trie 堆，而引擎启动全表扫描慢约 2 倍，恒用默认）。
        //
        // WAL 调优（与统计库同标准）：60s 周期 flush 的 diff 事务
        // 默认每 1000 页（4MB）自动 checkpoint 回拷一次；放宽到 2000 页 +
        // 8MB 截断上限——回拷频率减半，WAL 不随会话时长无界增长。双端共享
        // 本函数（mac 同走 open_raw）。
        let mmap_size: i64 = std::env::var("X1_MMAP_SIZE")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(268_435_456);
        conn.execute_batch(&format!(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA temp_store = MEMORY;
             PRAGMA wal_autocheckpoint = 2000;
             PRAGMA journal_size_limit = 8388608;
             PRAGMA mmap_size = {};",
            mmap_size
        ))?;
        conn.busy_timeout(std::time::Duration::from_millis(3000))?;
        Ok(conn)
    }

    /// 以只读方式打开数据库，用于读取其他 DB 的元数据（如内置词库条目数）
    ///
    /// 不会创建新文件，也不会修改 schema。
    pub fn open_readonly<P: AsRef<Path>>(path: P) -> rusqlite::Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(Self {
            dict: conn,
            user: None,
            dict_path: None,
        })
    }

    /// 打开内存数据库（仅用于测试）
    #[cfg(test)]
    pub fn open_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        let mut storage = Self {
            dict: conn,
            user: None,
            dict_path: None,
        };
        storage.init_schema()?;
        Ok(storage)
    }

    /// 系统词库表结构（dict.db 角色）：x1dict_words + 两张种子搭配表 + db_meta。
    fn init_schema_dict(&mut self) -> rusqlite::Result<()> {
        self.dict.execute_batch(Self::SCHEMA_DICT)?;
        self.run_dict_migrations()?;
        Ok(())
    }

    /// 用户库表结构（user.db 角色）：全部 user_* 表 + db_meta。
    fn init_schema_user(&mut self) -> rusqlite::Result<()> {
        self.user_mut().execute_batch(Self::SCHEMA_USER)?;
        self.run_user_migrations()?;
        Ok(())
    }

    /// 初始化全部表结构（幂等，合并模式）。
    fn init_schema(&mut self) -> rusqlite::Result<()> {
        self.dict.execute_batch(Self::SCHEMA_DICT)?;
        self.dict.execute_batch(Self::SCHEMA_USER)?;
        self.run_dict_migrations()?;
        self.run_user_migrations()?;
        Ok(())
    }

    /// dict.db（系统词库）表结构：x1dict_words + db_meta。
    ///
    /// 种子搭配表（seed_bigram_counts / seed_trigram_counts）由
    /// `ensure_seed_bigram_table` / `ensure_seed_trigram_table` 按需建
    /// （与单库模式一致，保持建表路径单一）。db_meta 两库各自持有——
    /// 键按角色分流（种子侧键落 dict.db、用户侧键落 user.db，见 split.rs）。
    const SCHEMA_DICT: &'static str = r#"
CREATE TABLE IF NOT EXISTS x1dict_words (
                id           INTEGER PRIMARY KEY,
                simplified   TEXT NOT NULL,
                pinyin       TEXT NOT NULL,
                freq         INTEGER DEFAULT 0,
                is_primary   INTEGER DEFAULT 1
            );
            CREATE INDEX IF NOT EXISTS idx_x1dict_pinyin
                ON x1dict_words(pinyin);
            CREATE INDEX IF NOT EXISTS idx_x1dict_initials
                ON x1dict_words(substr(pinyin, 1, 1));
            -- freq 索引：无过滤列表 ORDER BY freq DESC 深 OFFSET 会全表扫描 +
            -- 临时 B 树（18.8 万行第 3752 页 ≈308ms/次，首页 27ms）；索引后
            -- 8ms（38 倍）。一次性建 ~110ms，幂等。只加速管理器查询，输入热路径不经过。
            CREATE INDEX IF NOT EXISTS idx_x1dict_freq
                ON x1dict_words(freq DESC);

            -- 英文单词表（智能单词识别）：独立专用表，不与任何中文表混用；
            -- 数据经种子分发，引擎不再编译内嵌——为后续翻译/词典等英文功能
            -- 预留 pos/cn 列。word 主键 = 规范展示词形（IBM / iPhone /
            -- Microsoft，大小写规范化）；查询一律走 lower(word)（输入大小写
            -- 不敏感）。维护源为离线 TSV 批次（小写批次 + casing.tsv 覆盖
            -- 规范词形）→ 导入种子。
            CREATE TABLE IF NOT EXISTS english_words (
                word  TEXT PRIMARY KEY,
                pos   TEXT NOT NULL DEFAULT '',
                cn    TEXT NOT NULL DEFAULT '',
                freq  INTEGER NOT NULL DEFAULT 50
            );
            -- lower(word) 表达式索引：精确/前缀查询仍走索引（按键热路径）。
            -- 唯一性由写路径按 lower 去重（不建 UNIQUE 索引——历史库若已有
            -- 大小写重复行，建唯一索引会让 open 直接失败）。
            CREATE INDEX IF NOT EXISTS idx_english_words_lower
                ON english_words(lower(word));

            -- 中译英词表（候选翻译副栏·中→英精确词典）：独立专用表，
            -- 同 english_words 走种子分发。zh 主键列 B 树点查（翻译只做精确词级
            -- 命中，不做前缀——中英对译无补全语义）；(zh, en) 主键 = 同词多译
            -- 多行，freq 仅用于多译定序。维护源为离线 TSV 批次（→ 导入种子）。
            -- 单字内容词收（水→water），虚词不收（的/了无对译）。
            CREATE TABLE IF NOT EXISTS zh_en_words (
                zh   TEXT NOT NULL,
                en   TEXT NOT NULL,
                pos  TEXT NOT NULL DEFAULT '',
                freq INTEGER NOT NULL DEFAULT 50,
                PRIMARY KEY (zh, en)
            );

            CREATE TABLE IF NOT EXISTS db_meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );"#;

    /// user.db（用户库）表结构。
    const SCHEMA_USER: &'static str = r#"
CREATE TABLE IF NOT EXISTS user_words (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                text       TEXT NOT NULL,
                pinyin     TEXT NOT NULL,
                user_freq  INTEGER DEFAULT 1,
                created_at REAL NOT NULL,
                updated_at REAL NOT NULL,
                UNIQUE(text, pinyin)
            );
            CREATE INDEX IF NOT EXISTS idx_user_text
                ON user_words(text);

            CREATE TABLE IF NOT EXISTS user_input_mappings (
                input      TEXT NOT NULL,
                text       TEXT NOT NULL,
                count      INTEGER DEFAULT 1,
                updated_at REAL NOT NULL,
                PRIMARY KEY (input, text)
            );
            CREATE INDEX IF NOT EXISTS idx_user_input_mappings_input
                ON user_input_mappings(input);

            CREATE TABLE IF NOT EXISTS user_pinyin_counts (
                text       TEXT NOT NULL,
                pinyin     TEXT NOT NULL,
                count      INTEGER DEFAULT 1,
                updated_at REAL NOT NULL,
                PRIMARY KEY (text, pinyin)
            );

            CREATE TABLE IF NOT EXISTS user_english_counts (
                word       TEXT PRIMARY KEY,
                count      INTEGER NOT NULL DEFAULT 1,
                updated_at REAL NOT NULL
            );

            CREATE TABLE IF NOT EXISTS user_bigram_counts (
                prev       TEXT NOT NULL,
                text       TEXT NOT NULL,
                count      INTEGER DEFAULT 1,
                updated_at REAL NOT NULL,
                PRIMARY KEY (prev, text)
            );
            CREATE INDEX IF NOT EXISTS idx_bigram_prev
                ON user_bigram_counts(prev);

            -- 字级 bigram 已改为从词库双字词实时反推（纯内存派生，不落盘）。
            -- 计算与存储分离原则：char_bigram 是可计算的派生数据，
            -- 不持久化到 DB。这里 DROP 老表（user_char_bigram_counts）。
            DROP TABLE IF EXISTS user_char_bigram_counts;

            -- 三阶上下文（trigram）：
            -- (prev2, prev1) → text → count。比 bigram 多一阶，捕捉三词框架（如 我·知道·你）。
            -- 冷启动为空，完全靠用户积累。idx_trigram_prev 支持 (prev2, prev1) 前缀查询。
            CREATE TABLE IF NOT EXISTS user_trigram_counts (
                prev2      TEXT NOT NULL,
                prev1      TEXT NOT NULL,
                text       TEXT NOT NULL,
                count      INTEGER DEFAULT 1,
                updated_at REAL NOT NULL,
                PRIMARY KEY (prev2, prev1, text)
            );
            CREATE INDEX IF NOT EXISTS idx_trigram_prev
                ON user_trigram_counts(prev2, prev1);

            -- 用户选择次数：系统词 commit 后不写 user_words（避免两表重复），
            -- 但 FreqModel.counts 只通过 pending_upserts→user_words 落盘，系统词
            -- 被排除，重启后选择次数会全丢。
            -- 此表独立持久化 counts（全量快照），让系统词的 freq 累积跨会话存活。
            CREATE TABLE IF NOT EXISTS user_counts (
                text       TEXT PRIMARY KEY,
                count      INTEGER NOT NULL,
                updated_at REAL NOT NULL
            );

            -- 用户持久化黑名单：
            -- 用户删除的词写入此表，跨会话/跨部署保留。
            -- 部署脚本合并用户数据时跳过黑名单词，并在部署期清理种子 DB 同名词。
            -- 解决「删除的垃圾词每次部署又回来」的核心痛点。
            CREATE TABLE IF NOT EXISTS user_blacklist (
                text       TEXT PRIMARY KEY,
                updated_at REAL NOT NULL
            );

            -- 固定候选：用户把某输入串的字/词钉死在 1-3 号候选位，
            -- 免疫字频/词频/上下文排序。键 = 原始输入串（与 green_memo 同口径，
            -- 全拼/简拼/双拼原文各是各的键，不互相触发）。同输入同文本只允许占
            -- 一个槽（引擎层先删后插保证）；槽位被占直接覆盖。
            CREATE TABLE IF NOT EXISTS user_pins (
                input      TEXT NOT NULL,
                slot       INTEGER NOT NULL,
                text       TEXT NOT NULL,
                updated_at REAL NOT NULL,
                PRIMARY KEY (input, slot)
            );

            -- 竞争频率（「退化」通道）：
            -- 同音层(1)/组词层(2)/整句层(3) 三层竞争的分子（selects）与分母（total）。
            -- 权重由相对频率 selects/total 驱动（归一、有界、无时间参数）——
            -- 词频进化+退化的数据基础；本表允许整体重建（个性化丢失、正确性不丢）。
            CREATE TABLE IF NOT EXISTS competition (
                layer    INTEGER NOT NULL,
                ctx      TEXT NOT NULL,
                word     TEXT NOT NULL,
                selects  INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (layer, ctx, word)
            );
            CREATE TABLE IF NOT EXISTS competition_totals (
                layer    INTEGER NOT NULL,
                ctx      TEXT NOT NULL,
                total    INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (layer, ctx)
            );

            CREATE TABLE IF NOT EXISTS db_meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );"#;

    /// dict.db 侧一次性迁移（角色分流：全部只动 x1dict_words）。
    fn run_dict_migrations(&mut self) -> rusqlite::Result<()> {
        // 迁移：为旧版 DB（无 is_primary 列）补建列。幂等：仅在列缺失时 ALTER。
        self.migrate_add_is_primary_column()?;
        // 迁移：移除 x1dict_words 的翻译字段（traditional/pinyin_tones/english）。
        // 输入法不做翻译工具，三字段对运行时打字无用（Trie 只读 4 列）。
        // 老库通过重建表彻底清列瘦身；新建库直接是 4 列，迁移幂等跳过。
        self.migrate_x1dict_drop_translate_columns()?;
        // 迁移：为 x1dict_words 添加 (simplified, pinyin) 唯一约束。
        // 修复 sync_x1dict_from_builtin 不再搬内置 id 后的去重依赖（按业务键去重）。
        // 存量 DB 可能有重复 (simplified, pinyin)，先去重（保留每组 freq 最大那条）再加约束。
        self.ensure_x1dict_unique_constraint()?;
        // 迁移：清理 x1dict_words 里的字母脏词条（早期旧词表来源收录的西文字母 A/B/w/T…）。
        // 这些字母泄漏进 Trie 会污染简拼查询和（修复前的）整句解码。
        // 用 db_meta 标记只执行一次。
        self.cleanup_x1dict_letter_dirty_entries()?;
        Ok(())
    }

    /// user.db 侧一次性迁移（全部只动 user_* 表）。
    fn run_user_migrations(&mut self) -> rusqlite::Result<()> {
        // 迁移：trigram 表（3元组）→ bigram 表（2元组），DROP 旧表重建。
        // trigram 积累数据本就稀少（3元组难重复），丢弃可接受。
        self.migrate_trigram_to_bigram()?;
        // 迁移：清理 user_words 里含 ASCII 字母的脏自造词。
        // 整句解码 bug 期间，错误的候选（「系w」「文T」）被 commit 学习进了 user_words，
        // 持久化后每次查询都会返回。清理存量，配合 engine 的入库防御根治。
        self.cleanup_user_words_letter_dirty_entries()?;
        // 迁移：user_words 同词多读音。UNIQUE(text) → UNIQUE(text, pinyin)。
        // 列级 UNIQUE 无法 ALTER 删除，走一次性建新表-拷贝-改名；旧表每 text 仅一行，
        // 拷贝无 (text, pinyin) 冲突。同一字造词时不同读音（zhujiao→主角 造词后再造
        // zhujue→主角）不能互相冲掉——输入不精确是常态，两种读音必须共存。
        self.migrate_user_words_multi_pinyin()?;
        Ok(())
    }

    /// 迁移：把旧的 user_trigram_counts 表（3元组）替换为 user_bigram_counts（2元组）。
    ///
    /// trigram（3元组）无法无损转换为 bigram（2元组），且 trigram 积累数据本就
    /// 稀少（3 词都对上的组合极少），直接 DROP 旧表。新表 user_bigram_counts 由
    /// init_schema 的 CREATE TABLE IF NOT EXISTS 自动创建。
    /// 用 db_meta.bigram_migrated 标记确保只执行一次。
    fn migrate_trigram_to_bigram(&mut self) -> rusqlite::Result<()> {
        let migrated = self.get_user_meta("bigram_migrated")?.is_some();
        if migrated {
            return Ok(());
        }
        // 检测旧表是否存在。注意：user_trigram_counts 表已重新启用，
        // 但用全新 (prev2, prev1, text, count) 三列 schema。这里只 DROP 旧 schema 的表
        //（含 prev 列且不含 prev2 列），新 schema 表由 init_schema 创建并保留。
        // 用列模式区分新旧，避免误删新表。
        let has_old: bool = self.user().query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='user_trigram_counts'",
            [],
            |row| row.get(0),
        )?;
        if has_old {
            // 新 schema 含 prev2 列；只有不含 prev2（旧 2 列 schema）时才 DROP
            let has_prev2: bool = self.user().query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('user_trigram_counts') WHERE name = 'prev2'",
                [],
                |row| row.get(0),
            )?;
            if !has_prev2 {
                self.user()
                    .execute_batch("DROP TABLE user_trigram_counts")?;
                // DROP 后重建新 schema（init_schema 已 CREATE IF NOT EXISTS 但此时表已存在故跳过）
                self.user().execute_batch(
                    "CREATE TABLE IF NOT EXISTS user_trigram_counts (
                        prev2      TEXT NOT NULL,
                        prev1      TEXT NOT NULL,
                        text       TEXT NOT NULL,
                        count      INTEGER DEFAULT 1,
                        updated_at REAL NOT NULL,
                        PRIMARY KEY (prev2, prev1, text)
                    );
                    CREATE INDEX IF NOT EXISTS idx_trigram_prev
                        ON user_trigram_counts(prev2, prev1);",
                )?;
            }
        }
        self.set_user_meta("bigram_migrated", "1")?;
        Ok(())
    }

    /// 为 x1dict_words 添加 (simplified, pinyin) 唯一约束（幂等）。
    ///
    /// 背景：sync_x1dict_from_builtin 修复后不再搬内置 DB 的 id（避免 id 冲突导致
    /// 新词同步不进去），改为不指定 id 插入。但 x1dict_words 原本只有 id 主键，
    /// 没有 (simplified, pinyin) 业务唯一约束，INSERT OR IGNORE 无法按业务键去重，
    /// 会导致重复（实测 sync 后「谁 shei」被插两份）。
    ///
    /// 本迁移：
    /// 1. 若约束已存在（idx_x1dict_unique），幂等跳过
    /// 2. 否则先去重（每组 (simplified, pinyin) 保留 freq 最大、is_primary 优先的一条），
    ///    再创建 UNIQUE INDEX
    ///
    /// 用 db_meta.x1dict_unique_migrated 标记确保去重只执行一次（去重是破坏性操作，
    /// 重复执行虽幂等但浪费）。
    fn ensure_x1dict_unique_constraint(&mut self) -> rusqlite::Result<()> {
        // 检查唯一索引是否已存在（幂等）
        let has_unique: bool = self.dict.query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master
             WHERE type='index' AND name='idx_x1dict_unique'",
            [],
            |row| row.get(0),
        )?;
        if has_unique {
            return Ok(());
        }
        // 去重：每组 (simplified, pinyin) 保留 freq 最大的一条（同 freq 取 id 最小）。
        // 只执行一次，用 db_meta 标记。
        let dedup_done = self.get_seed_meta("x1dict_unique_migrated")?.is_some();
        if !dedup_done {
            self.dict.execute_batch(
                "DELETE FROM x1dict_words
                 WHERE id NOT IN (
                     SELECT id FROM x1dict_words w1
                     WHERE w1.id = (
                         SELECT id FROM x1dict_words w2
                         WHERE w2.simplified = w1.simplified AND w2.pinyin = w1.pinyin
                         ORDER BY w2.freq DESC, w2.id ASC
                         LIMIT 1
                     )
                 );",
            )?;
            self.set_seed_meta("x1dict_unique_migrated", "1")?;
        }
        // 创建唯一约束
        self.dict.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_x1dict_unique
             ON x1dict_words(simplified, pinyin);",
        )?;
        Ok(())
    }

    /// 迁移：user_words 的 UNIQUE(text) → UNIQUE(text, pinyin)（同词多读音）。
    ///
    /// 旧形态检测：列级 UNIQUE 生成的自动索引恰好只覆盖 text 一列（新形态覆盖
    /// (text, pinyin) 两列），用 PRAGMA index_list/index_info 判定，不比对建表 SQL
    /// 文本。SQLite 无法 ALTER 删除列级约束，走建新表-拷贝-改名（事务内；崩溃残留
    /// 的 user_words_v2 先 DROP 重来）。用 db_meta.user_words_multi_pinyin_v1 标记
    /// 只执行一次；旧表每 text 仅一行，拷贝无 (text, pinyin) 冲突。
    fn migrate_user_words_multi_pinyin(&mut self) -> rusqlite::Result<()> {
        if self.get_user_meta("user_words_multi_pinyin_v1")?.is_some() {
            return Ok(());
        }
        let mut legacy_unique_on_text = false;
        {
            let mut list = self.user().prepare(
                "SELECT name FROM pragma_index_list('user_words') WHERE \"unique\" = 1 AND origin = 'u'",
            )?;
            let names: Vec<String> = list
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            for name in names {
                let mut info = self
                    .user()
                    .prepare(&format!("SELECT name FROM pragma_index_info('{}')", name))?;
                let cols: Vec<String> = info
                    .query_map([], |r| r.get(0))?
                    .collect::<Result<_, _>>()?;
                if cols.len() == 1 && cols[0] == "text" {
                    legacy_unique_on_text = true;
                }
            }
        }
        if legacy_unique_on_text {
            let tx = self.user_mut().transaction()?;
            tx.execute_batch(
                "DROP TABLE IF EXISTS user_words_v2;
                 CREATE TABLE user_words_v2 (
                     id         INTEGER PRIMARY KEY AUTOINCREMENT,
                     text       TEXT NOT NULL,
                     pinyin     TEXT NOT NULL,
                     user_freq  INTEGER DEFAULT 1,
                     created_at REAL NOT NULL,
                     updated_at REAL NOT NULL,
                     UNIQUE(text, pinyin)
                 );
                 INSERT INTO user_words_v2 (text, pinyin, user_freq, created_at, updated_at)
                     SELECT text, pinyin, user_freq, created_at, updated_at FROM user_words;
                 DROP TABLE user_words;
                 ALTER TABLE user_words_v2 RENAME TO user_words;
                 CREATE INDEX IF NOT EXISTS idx_user_text ON user_words(text);",
            )?;
            tx.commit()?;
        }
        self.set_user_meta("user_words_multi_pinyin_v1", "1")?;
        Ok(())
    }

    /// 清理 x1dict_words 里的字母脏词条（早期词表来源收录的西文字母 A/B/w/T…）。
    ///
    /// 这些字母泄漏进 Trie 后会污染简拼查询（输入 w 时返回字母 w 候选），
    /// 修复前的整句解码还会把它们拼进候选（xiw→「系w」）。
    /// importer 已在导入源头过滤，但存量用户 DB 里还有旧脏数据，本迁移清理之。
    ///
    /// 判定：simplified 全部由 ASCII 字母组成（CJK 汉字不含 a-z）。
    /// 用 db_meta.x1dict_letters_cleaned 标记只执行一次。
    fn cleanup_x1dict_letter_dirty_entries(&mut self) -> rusqlite::Result<()> {
        if self.get_seed_meta("x1dict_letters_cleaned")?.is_some() {
            return Ok(());
        }
        // 删除单字符 ASCII 字母条目（A/B/w/T…）。
        // unicode() 返回 UTF-8 码点（A=65..Z=90, a=97..z=122），CJK 字符码点远大于此，
        // 不会误删汉字。只删单字符（避免误删「ATM」等多字符合法词）。
        let deleted = self.dict.execute(
            "DELETE FROM x1dict_words
             WHERE simplified GLOB '[A-Za-z]'
               AND length(simplified) = 1",
            [],
        )?;
        if deleted > 0 {
            eprintln!("[storage] 清理了 {} 条字母脏词条", deleted);
        }
        self.set_seed_meta("x1dict_letters_cleaned", "1")?;
        Ok(())
    }

    /// 清理 user_words 里含 ASCII 字母的脏自造词。
    ///
    /// 整句解码 bug 期间（修复前），错误的候选（如「系w」「文T」，来自单字+字母拼接）
    /// 被 commit 学习进了 user_words，持久化后每次查询都会返回，污染候选列表。
    /// 清理存量脏数据，配合 engine 的入库防御（commit/learn 拒绝含字母候选）
    /// 和输出防御（process 过滤含字母候选）三层根治。
    ///
    /// 判定：text 含任何 ASCII 字母（汉字不含 a-z）。
    /// 用 db_meta.user_words_letters_cleaned 标记只执行一次。
    fn cleanup_user_words_letter_dirty_entries(&mut self) -> rusqlite::Result<()> {
        if self.get_user_meta("user_words_letters_cleaned")?.is_some() {
            return Ok(());
        }
        let deleted = self
            .user()
            .execute("DELETE FROM user_words WHERE text GLOB '*[A-Za-z]*'", [])?;
        if deleted > 0 {
            eprintln!("[storage] 清理了 {} 条含字母的脏自造词", deleted);
        }
        self.set_user_meta("user_words_letters_cleaned", "1")?;
        Ok(())
    }

    /// 迁移：检查 x1dict_words 是否有 is_primary 列，没有则补建。
    ///
    /// 旧版 DB（v1 之前）的 x1dict_words 没有 is_primary 列，打开时需要补建，
    /// 否则 INSERT/SELECT 会因列缺失报错。补建后所有旧行默认 is_primary=1。
    /// 首次补建后会立即回填真实主音标记（refresh_polyphone_marks）。
    fn migrate_add_is_primary_column(&mut self) -> rusqlite::Result<()> {
        let has_column: bool = self.dict.query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('x1dict_words') WHERE name = 'is_primary'",
            [],
            |row| row.get(0),
        )?;
        let just_added = !has_column;
        if just_added {
            self.dict.execute_batch(
                "ALTER TABLE x1dict_words ADD COLUMN is_primary INTEGER DEFAULT 1;",
            )?;
        }
        // 首次添加列后，或 db_meta 缺 polyphone_migrated 标记时，回填主音标记。
        // 用 db_meta 标记确保只回填一次，后续用户手动修改不会被覆盖。
        let migrated = self.get_seed_meta("polyphone_migrated")?.is_some();
        if just_added || !migrated {
            self.refresh_polyphone_marks()?;
            self.set_seed_meta("polyphone_migrated", "1")?;
        }
        Ok(())
    }

    /// 迁移：移除 x1dict_words 的翻译字段（traditional / pinyin_tones / english）。
    ///
    /// 输入法不充当翻译工具，三字段在运行时打字路径中完全不用（`load_all_for_trie`
    /// 只读 simplified/pinyin/freq/is_primary 四列），却让 DB 膨胀（english 释义占
    /// ~5MB / 12 万条）。产品决策：DB 不再存储翻译对照内容。
    ///
    /// 迁移策略——重建表范式（参考 `migrate_trigram_to_bigram`）：
    /// 1. 检测旧列是否存在（pragma_table_info 查 traditional）；
    /// 2. 存在则新建 4 列临时表 → INSERT SELECT 复制保留列 → DROP 旧表 → RENAME；
    /// 3. 重建索引（DROP TABLE 会连带删除索引）。
    ///
    /// 幂等：用 `db_meta.x1dict_translate_dropped` 标记确保只执行一次；
    /// 新建库（init_schema 直接是 4 列）跳过实际重建。
    fn migrate_x1dict_drop_translate_columns(&mut self) -> rusqlite::Result<()> {
        let migrated = self.get_seed_meta("x1dict_translate_dropped")?.is_some();
        if migrated {
            return Ok(());
        }
        // 检测旧列是否存在（任一存在即视为老库，需要重建）
        let has_old: bool = self.dict.query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('x1dict_words')
             WHERE name IN ('traditional', 'pinyin_tones', 'english')",
            [],
            |row| row.get(0),
        )?;
        if has_old {
            // 重建表：保留 id/simplified/pinyin/freq/is_primary，丢弃翻译三列。
            // 用临时表名避免与残留对象冲突，DROP 旧表后 RENAME 回原名。
            //
            // 事务包裹 + 前置 DROP IF EXISTS：
            // execute_batch 不自带事务——CREATE 之后崩溃会残留 x1dict_words_new，
            // 下次重跑报 "table already exists" → Storage::open 永久失败（毒库）。
            // 同模式参照 user 侧 migrate_user_words_multi_pinyin（事务内 + 残留先
            // DROP 重来）。标记 x1dict_translate_dropped 留在事务外是安全的：
            // 提交后崩溃 → 重跑时旧列已不存在（has_old=false）跳过重建只补标记。
            let tx = self.dict.transaction()?;
            tx.execute_batch(
                "DROP TABLE IF EXISTS x1dict_words_new;
                 CREATE TABLE x1dict_words_new (
                    id         INTEGER PRIMARY KEY,
                    simplified TEXT NOT NULL,
                    pinyin     TEXT NOT NULL,
                    freq       INTEGER DEFAULT 0,
                    is_primary INTEGER DEFAULT 1
                );
                INSERT INTO x1dict_words_new (id, simplified, pinyin, freq, is_primary)
                    SELECT id, simplified, pinyin, freq,
                           COALESCE(is_primary, 1) AS is_primary
                    FROM x1dict_words;
                DROP TABLE x1dict_words;
                ALTER TABLE x1dict_words_new RENAME TO x1dict_words;
                CREATE INDEX IF NOT EXISTS idx_x1dict_pinyin
                    ON x1dict_words(pinyin);
                CREATE INDEX IF NOT EXISTS idx_x1dict_initials
                    ON x1dict_words(substr(pinyin, 1, 1));",
            )?;
            tx.commit()?;
        }
        self.set_seed_meta("x1dict_translate_dropped", "1")?;
        Ok(())
    }

    /// 回填 x1dict_words 的 is_primary 标记（基于 char_freq::is_primary_reading）。
    ///
    /// 遍历所有单字条目，按 (首字, 拼音首音节) 判断主音：
    /// - 主音拼音 → is_primary = 1
    /// - 次音拼音 → is_primary = 0
    /// 多字词保持 is_primary = 1（恒为主音）。
    ///
    /// 首次打开旧 DB 时自动调用一次（由 migrate_add_is_primary_column 触发），
    /// 之后通过 db_meta.polyphone_migrated 标记避免重复执行。
    pub fn refresh_polyphone_marks(&mut self) -> rusqlite::Result<usize> {
        let rows: Vec<(String, String)> = {
            let mut stmt = self.dict.prepare(
                "SELECT DISTINCT simplified, pinyin FROM x1dict_words
                 WHERE length(simplified) = 1",
            )?;
            let mapped = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut out = Vec::new();
            for r in mapped {
                out.push(r?);
            }
            out
        };

        let tx = self.dict.transaction()?;
        let mut updated = 0usize;
        {
            let mut stmt = tx.prepare(
                "UPDATE x1dict_words SET is_primary = ?1
                 WHERE simplified = ?2 AND pinyin = ?3 AND length(simplified) = 1",
            )?;
            for (simplified, pinyin) in &rows {
                if let (Some(first_char), Some(first_syllable)) =
                    (simplified.chars().next(), pinyin.split_whitespace().next())
                {
                    let is_primary =
                        crate::char_freq::is_primary_reading(first_char, first_syllable);
                    let n = stmt.execute(params![is_primary as i64, simplified, pinyin])?;
                    updated += n;
                }
            }
        }
        tx.commit()?;
        Ok(updated)
    }
}

// -----------------------------------------------------------------------------
// 模块化拆分
//
// storage 按职责拆成以下子模块，每个 impl crate::storage::Storage
// 块在编译期合并：
//
// - x1dict.rs   : x1dict_words 系统词库 CRUD + 同步哈希 + 解密 + Trie 加载
// - tsv.rs      : x1dict TSV 导入/导出 + 列识别辅助函数
// - user_words.rs : user_words 用户词库 CRUD + user_blacklist 黑名单
// - stats.rs    : 运行时统计四表（input_mappings/pinyin_counts/bigram_counts/counts）
// - tests.rs    : 单元测试块
// -----------------------------------------------------------------------------
pub(crate) mod dict_cache;
/// DB 结构版本验证（自定义数据目录配套；只进部署期引导链）。
pub mod schema;
pub mod split;
mod stats;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_split;
mod tsv;
mod user_words;
mod x1dict;

// 英文词条结构（词库管理器·英文词库用，配套 CRUD）。
pub use self::x1dict::EnglishWord;

// 让 tsv.rs 的列识别辅助函数（detect_tsv_column_map / ColumnMap）流入本模块作用域，
// 使 tests.rs 的 `use super::*;` 能直接调用（原单文件里这两项就在 storage 顶层）。
// tsv.rs 里这两项标了 pub(super)，对 storage 模块可见；这里 private use 即可。
#[cfg(test)]
use self::tsv::*;
