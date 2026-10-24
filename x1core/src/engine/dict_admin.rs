//! 词库管理（从 engine/mod.rs 抽出）。

use alloc::string::String;
use alloc::vec::Vec;
use std::collections::HashMap;

use crate::freq::FreqModel;
use crate::storage::X1DictWord;

impl crate::engine::Engine {
    /// 延迟写入：将自上次 flush 以来的增量变更同步到 SQLite
    ///
    /// 采用增量策略：
    /// - 新增 / 修改过的词 → `upsert_user_words_batch`（绝对值替换）
    /// - 删除的词 → `delete_user_words_batch`
    ///
    /// 若未启用 SQLite（`new()` 创建），则空操作。
    /// 由宿主前端定时器 / 退出时调用，热路径不直接写磁盘。
    /// 最近一次 flush 的分段耗时（`阶段=ms` 空格分隔；无记录返回空串）。
    /// 诊断 API：卡顿排查需要知道耗时花在哪个阶段。
    pub fn flush_breakdown(&self) -> String {
        self.last_flush_breakdown
            .iter()
            .map(|(k, v)| format!("{}={}ms", k, v))
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn flush(&mut self) -> Result<(), rusqlite::Error> {
        if let Some(storage) = &mut self.storage {
            // 分段计时：定位落盘耗时花在哪一段
            let mut bd: Vec<(&'static str, u128)> = Vec::new();
            let mut t_bd = std::time::Instant::now();
            // 早退判据含模型层脏标记：bigram/trigram 的
            // 变化记在 freq/trigram 自己的 dirty 上——只看 Engine::dirty 会让
            // 「仅模型层变化」（如 forget_bigram）的增量静默不落盘。
            if !self.dirty
                && self.pending_deletions.is_empty()
                && self.pending_upserts.is_empty()
                && !self.freq.is_dirty()
                && !self.trigram.is_dirty()
                && !self.competition.is_dirty()
                && self.english_delta.is_empty()
            {
                // 没有待落盘的数据，也把脏标记清掉，避免 `is_user_dict_dirty()`
                // 在仅更新内存频率后一直返回 true。
                self.freq.mark_clean();
                self.last_flush_breakdown = bd;
                return Ok(());
            }

            // 黑名单/删词落盘重试：remove() 时
            // insert_blacklist / delete_x1dict 失败进的 pending。重试对同一
            // text 同时执行两个操作（幂等，黑名单词语义上两个都该做）。
            if !self.pending_blacklist.is_empty() {
                let retry: Vec<String> = std::mem::take(&mut self.pending_blacklist);
                for text in &retry {
                    if storage.insert_blacklist(text).is_ok() {
                        let _ = storage.delete_x1dict_word_by_text(text);
                    } else {
                        // 仍失败：整批留待下轮（保留去重）
                        self.pending_blacklist.push(text.clone());
                        break;
                    }
                }
            }

            // 先删除（失败回填——drain 后写库失败不得
            // 丢增量，SQLITE_BUSY/磁盘抖动时本轮待删词会静默蒸发；回填后
            // 下轮 flush 重试，dirty 未清保证重试必发生）
            if !self.pending_deletions.is_empty() {
                let taken = std::mem::take(&mut self.pending_deletions);
                let deletions: Vec<String> = taken.iter().cloned().collect();
                if let Err(e) = storage.delete_user_words_batch(&deletions) {
                    self.pending_deletions.extend(taken);
                    return Err(e);
                }
            }

            // 读音替换清理（多读音合并补漏）：显式编辑（词库管理器）
            // 改拼音后，旧读音行必须删掉——内存 vec 是权威读音表，不在其中的行
            // 都是残留（此前只 upsert 新读音 → 同词两条、候选冒旧读音）。
            // 在 upsert 之前执行：先清残留再写新值，中途失败时下一轮
            // flush 重试（集合未清）。
            if !self.pending_reading_prune.is_empty() {
                let taken = std::mem::take(&mut self.pending_reading_prune);
                let mut failed: Vec<String> = Vec::new();
                for text in &taken {
                    let keep = self.user_words.get(text).cloned().unwrap_or_default();
                    if keep.is_empty() {
                        // 内存里没有该词（已被删）→ 交给 pending_deletions 路径
                        continue;
                    }
                    if let Err(e) = storage.prune_user_word_readings_except(text, &keep) {
                        eprintln!("[x1core] flush: 读音替换清理失败 {}: {}", text, e);
                        failed.push(text.clone());
                    }
                }
                if !failed.is_empty() {
                    self.pending_reading_prune.extend(failed);
                }
            }

            // 再 upsert（使用当前内存中的绝对值），用户词库只保留多字词
            //（失败回填同上）
            if !self.pending_upserts.is_empty() {
                let taken = std::mem::take(&mut self.pending_upserts);
                // 多读音共存：一个 text 展开为 (text, 每个读音, freq) 多条
                let entries: Vec<(String, String, u32)> = taken
                    .iter()
                    .filter(|text| text.chars().count() > 1)
                    .flat_map(|text| {
                        let freq = self.freq.counts.get(text).copied().unwrap_or(1);
                        self.user_words
                            .get(text)
                            .cloned()
                            .unwrap_or_default()
                            .into_iter()
                            .map(move |pinyin| (text.clone(), pinyin, freq))
                    })
                    .collect();
                if let Err(e) = storage.upsert_user_words_batch(&entries) {
                    self.pending_upserts.extend(taken);
                    return Err(e);
                }
            }

            // 空态覆写闸：启动时任一表读失败 →
            // 内存是空态，下方"全量快照 replace"会用空态覆写磁盘真数据
            //（用户词表/习惯映射清零链）。读不完整时只做上面的增量路径
            //（增量只含本会话新变化，安全），跳过全部 replace_*，等下次
            // 进程重启读全后恢复全量同步。
            if self.storage_load_incomplete {
                eprintln!(
                    "[x1core] flush: 存储加载不完整（启动读失败），跳过全量快照防空态覆写（增量已落盘）"
                );
                self.dirty = false;
                self.freq.mark_clean();
                return Ok(());
            }

            bd.push(("dict", t_bd.elapsed().as_millis()));
            t_bd = std::time::Instant::now();
            // 同步输入习惯映射（全量快照，数据量可控）
            let mappings: Vec<(String, String, u32)> = self
                .input_mappings
                .iter()
                .map(|((inp, txt), count)| (inp.clone(), txt.clone(), *count))
                .collect();
            storage.replace_input_mappings(&mappings)?;

            bd.push(("maps", t_bd.elapsed().as_millis()));
            t_bd = std::time::Instant::now();
            // 同步拼音级频率（全量快照，多音字主音/次音动态权重）
            let pinyin_counts: Vec<(String, String, u32)> = self
                .freq
                .pinyin_counts_snapshot()
                .iter()
                .map(|((txt, pin), count)| (txt.clone(), pin.clone(), *count))
                .collect();
            storage.replace_pinyin_counts(&pinyin_counts)?;

            bd.push(("pinyin", t_bd.elapsed().as_millis()));
            t_bd = std::time::Instant::now();
            // 同步 bigram 共现统计（词级，用户真实意图数据）
            // bigram 为增量落盘（避免上屏卡顿）：全量快照 replace 需要
            // DELETE 全表 + 重插 7 万行 ≈ 200ms，挂在 Commit 请求路径上 =
            // 每 60s 一次的某次上屏被拖慢。故用 diff 增量（典型数十行 →
            // 个位数 ms）。deletes 覆盖 forget_bigram 的删除语义。
            // take 语义 + 失败回填：写盘失败时把增量放回 dirty，
            // 下次 flush 重试——与 pending_deletions 同款模式，绝不丢变化。
            let (big_up, big_del) = self.freq.take_bigram_delta();
            if !big_up.is_empty() {
                if let Err(e) = storage.upsert_bigram_counts(&big_up) {
                    self.freq.restore_bigram_delta(&big_up, &[]);
                    return Err(e);
                }
            }
            if !big_del.is_empty() {
                if let Err(e) = storage.delete_bigram_counts(&big_del) {
                    self.freq.restore_bigram_delta(&[], &big_del);
                    return Err(e);
                }
            }
            // 注：char_bigram 不落盘（从词库双字词实时反推的派生数据，计算与存储分离）
            // 同步 trigram（增量同 bigram）
            let (tri_up, tri_del) = self.trigram.take_delta();
            if !tri_up.is_empty() {
                if let Err(e) = storage.upsert_trigram_counts(&tri_up) {
                    self.trigram.restore_delta(&tri_up, &[]);
                    return Err(e);
                }
            }
            if !tri_del.is_empty() {
                if let Err(e) = storage.delete_trigram_counts(&tri_del) {
                    self.trigram.restore_delta(&[], &tri_del);
                    return Err(e);
                }
            }

            bd.push(("ngram", t_bd.elapsed().as_millis()));
            t_bd = std::time::Instant::now();
            // 同步用户选择次数（全量快照）。系统词去重后，
            // 系统词不写 user_words，counts 只能通过这张表跨会话存活。
            let counts = self.freq.counts_snapshot();
            storage.replace_counts(&counts)?;
            // 同步竞争频率（「退化」通道；全量快照，与学习数据同模式）。
            // 空态覆写闸由上方 storage_load_incomplete 统一把守（同 counts 表）。
            let comp_sel = self.competition.snapshot_selects();
            storage.replace_competition(&comp_sel)?;
            let comp_tot = self.competition.snapshot_totals();
            storage.replace_competition_totals(&comp_tot)?;
            self.competition.mark_clean();

            bd.push(("counts", t_bd.elapsed().as_millis()));
            t_bd = std::time::Instant::now();
            // 持久化绿词采纳率统计：total_green_picks 跨会话累积，
            // 为后续按采纳率微调动态门槛积累样本。
            let _ = storage.set_user_meta(
                "green_total_picks",
                &self.green_accept.total_green_picks.to_string(),
            );

            // 持久化上下文栈快照：栈尾 CONTEXT_SNAPSHOT_SIZE 个词
            // 跨会话延续，让 bigram/trigram/collocation 上下文加成不在重启后冷启动。
            // SOH(\u{1}) 分隔——真实上屏词不可能含控制字符；空栈写空串覆盖旧快照。
            // 只动 db_meta，不碰任何用户数据表。
            let tail: Vec<&str> = self
                .context
                .iter()
                .rev()
                .take(Self::CONTEXT_SNAPSHOT_SIZE)
                .rev()
                .map(|s| s.as_str())
                .collect();
            let _ = storage.set_user_meta("context_snapshot", &tail.join("\u{1}"));

            bd.push(("meta", t_bd.elapsed().as_millis()));
            t_bd = std::time::Instant::now();
            // 英文使用计数增量（词频动态调整）：只写自上次落盘的
            // 增量（典型 60s 内个位数词 → 单事务个位数行）。失败回滚 delta
            //（内存计数不动，下次重试——追加语义天然幂等）。
            if !self.english_delta.is_empty() {
                let delta: Vec<(String, u32)> = self.english_delta.drain().collect();
                if let Err(e) = storage.add_english_counts(&delta) {
                    for (w, d) in delta {
                        *self.english_delta.entry(w).or_insert(0) += d;
                    }
                    return Err(e);
                }
            }

            bd.push(("en", t_bd.elapsed().as_millis()));
            self.last_flush_breakdown = bd;
            self.freq.mark_clean();
            self.trigram.clear_dirty(); // 同 freq：模型层脏标记随落盘清
            self.dirty = false;
        }
        Ok(())
    }

    /// 加载用户词库文本（兼容旧接口）
    pub fn load_user_dict(&mut self, text: &str) {
        self.freq.load_from_text(text);
        for text in self.freq.counts.keys() {
            self.pending_upserts.insert(text.clone());
        }
        self.dirty = true;
    }

    /// 导出用户词库文本（兼容旧接口）
    pub fn save_user_dict(&self) -> String {
        self.freq.save_to_text()
    }

    /// 导出用户词库为 TSV 格式：text\tpinyin\tfreq
    pub fn export_user_dict_tsv(&self) -> String {
        let mut lines = Vec::new();
        for (text, pinyin, freq) in self.get_user_words() {
            lines.push(format!("{}\t{}\t{}", text, pinyin, freq));
        }
        if lines.is_empty() {
            String::new()
        } else {
            lines.join("\n") + "\n"
        }
    }

    /// 从 TSV 导入用户词库：text\tpinyin\tfreq
    ///
    /// - `merge = true`：与现有词库合并，冲突时累加频率；
    /// - `merge = false`：清空现有用户词库后导入。
    pub fn import_user_dict_tsv(
        &mut self,
        tsv: &str,
        merge: bool,
    ) -> Result<usize, rusqlite::Error> {
        let mut entries = Vec::new();
        for line in tsv.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }
            let text = parts[0].to_string();
            let pinyin = parts[1].to_string();
            let freq = parts
                .get(2)
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(1);
            // 字数==音节数守卫（同 learn）：外部 TSV 的
            // 错位拼音行（如「习惯惯」→"xi guan"）入 Trie 后挂在短音节键下，
            // 输入 "xiguan" 会把 3 字词当整词候选顶出（末字重复上屏根因）。
            if !super::user_word_pinyin_matches(&text, &pinyin) {
                continue;
            }
            entries.push((text, pinyin, freq));
        }

        if let Some(storage) = &mut self.storage {
            let imported = storage.import_user_words(&entries, merge)?;
            self.reload_user_state()?;
            Ok(imported)
        } else {
            // 内存模式：直接操作内存
            if !merge {
                self.user_words.clear();
                self.freq = FreqModel::new();
            }
            for (text, pinyin, freq) in entries {
                // 用户词库只保留多字词
                if text.chars().count() <= 1 {
                    continue;
                }
                let current = self.freq.counts.get(&text).copied().unwrap_or(0);
                let new_freq = if merge { current + freq } else { freq };
                {
                    let pys = self.user_words.entry(text.clone()).or_default();
                    if !pys.iter().any(|p| p == &pinyin) {
                        pys.push(pinyin.clone());
                    }
                }
                self.freq.set_count(&text, new_freq);
                self.insert_user_word_to_dict(&text, &pinyin, new_freq);
                self.pending_upserts.insert(text);
            }
            self.dirty = true;
            Ok(self.user_words.len())
        }
    }

    // -----------------------------------------------------------------
    // 系统词库管理（供词库管理工具调用）
    // -----------------------------------------------------------------

    /// 查询系统词库列表
    pub fn list_x1dict_words(
        &self,
        filter: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Vec<X1DictWord> {
        match &self.storage {
            Some(s) => s
                .list_x1dict_words(filter, limit, offset)
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// 查询系统词库条目数
    pub fn x1dict_count_filtered(&self, filter: Option<&str>) -> i64 {
        match &self.storage {
            Some(s) => s.x1dict_count_filtered(filter).unwrap_or(-1),
            None => -1,
        }
    }

    // ── 英文词库 CRUD（词库管理器用）──
    // 英文查询走 SQL（prepare_cached），改完即时生效、无需 reload（无 Trie）。

    /// 英文词分页列表（filter = word 前缀或 cn 包含；freq 降序）。
    pub fn english_list(
        &self,
        filter: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Vec<crate::storage::EnglishWord> {
        match &self.storage {
            Some(s) => s.english_list_filtered(filter, limit, offset),
            None => Vec::new(),
        }
    }

    /// 英文词总数（过滤后）。
    pub fn english_count(&self, filter: Option<&str>) -> i64 {
        match &self.storage {
            Some(s) => s.english_count_filtered(filter),
            None => -1,
        }
    }

    /// 新增/覆盖英文词。词形校验：ASCII 字母 2..=24（允许规范大小写；
    /// 查询键由存储层 lower(word) 归一）。
    pub fn english_add(&self, w: &crate::storage::EnglishWord) -> bool {
        if !crate::engine::english::is_word_shape(&w.word) {
            return false;
        }
        match &self.storage {
            Some(s) => s.english_upsert(w),
            None => false,
        }
    }

    /// 更新英文词（可改 word 主键，也支持只改大小写）。
    pub fn english_update(&self, old_word: &str, w: &crate::storage::EnglishWord) -> bool {
        if !crate::engine::english::is_word_shape(&w.word) {
            return false;
        }
        match &self.storage {
            Some(s) => s.english_update(old_word, w),
            None => false,
        }
    }

    /// 删除英文词（幂等）。
    pub fn english_delete(&self, word: &str) -> bool {
        match &self.storage {
            Some(s) => s.english_delete(word),
            None => false,
        }
    }

    /// 添加/替换一条系统词
    pub fn add_system_word(&mut self, word: &X1DictWord) -> bool {
        if let Some(storage) = &mut self.storage {
            if storage.add_x1dict_word(word).is_err() {
                return false;
            }
            let freq = word.freq.max(0) as u32;
            self.dict
                .insert(&word.pinyin, &word.simplified, freq, word.is_primary);
            self.seg_cache_clear();
            return true;
        }
        false
    }

    /// 更新一条系统词
    pub fn update_system_word(
        &mut self,
        old_text: &str,
        old_pinyin: &str,
        word: &X1DictWord,
    ) -> bool {
        if let Some(storage) = &mut self.storage {
            if storage
                .update_x1dict_word(old_text, old_pinyin, word)
                .is_err()
            {
                return false;
            }
            // 从内存 Trie 中删除旧条目，再插入新条目
            self.dict.remove_by_text(old_text);
            let freq = word.freq.max(0) as u32;
            self.dict
                .insert(&word.pinyin, &word.simplified, freq, word.is_primary);
            self.seg_cache_clear();
            return true;
        }
        false
    }

    /// 删除一条系统词
    pub fn delete_system_word(&mut self, text: &str, pinyin: &str) -> bool {
        if let Some(storage) = &mut self.storage {
            if storage.delete_x1dict_word(text, pinyin).is_err() {
                return false;
            }
            self.dict.remove_by_text(text);
            self.seg_cache_clear();
            return true;
        }
        false
    }

    /// 导出系统词库为 TSV
    pub fn export_x1dict_tsv(&self) -> String {
        match &self.storage {
            Some(s) => s.export_x1dict_tsv().unwrap_or_default(),
            None => String::new(),
        }
    }

    /// 从 TSV 导入系统词库；replace 为 true 时清空后导入
    pub fn import_x1dict_tsv(
        &mut self,
        tsv: &str,
        replace: bool,
    ) -> Result<usize, rusqlite::Error> {
        if let Some(storage) = &mut self.storage {
            let n = storage.import_x1dict_tsv(tsv, replace)?;
            self.reload_x1dict()?;
            return Ok(n);
        }
        Ok(0)
    }

    /// 从 storage 重新加载 X1Dict 并重建内存 Trie
    ///
    /// rebuild_dict 内部已从 SQL 全量重建（系统词 + user_words），
    /// 这里只需调 rebuild_dict。
    pub fn reload_x1dict(&mut self) -> Result<(), rusqlite::Error> {
        self.rebuild_dict();
        Ok(())
    }

    /// 清理用户词库中的单字并刷新内存状态
    pub fn cleanup_single_char_user_words(&mut self) -> usize {
        if let Some(storage) = &mut self.storage {
            match storage.cleanup_single_char_user_words() {
                Ok(n) => {
                    let _ = self.reload_user_state();
                    return n;
                }
                Err(_) => return 0,
            }
        }
        0
    }

    /// 从 storage 重新加载用户词状态（导入/同步后使用）
    /// 以 DB 为准全量重载用户学习态（user_words/freq/input_mappings/黑名单）+
    /// 重建 Trie + 清 pending。词库管理工具改库后的服务端同步（Reload 请求）
    /// 与单字清理路径共用。pending 丢弃是语义（防旧 upsert 盖掉刚落盘的
    /// 删除，见服务端 Reload 注释）。
    pub fn reload_user_state(&mut self) -> Result<(), rusqlite::Error> {
        if let Some(storage) = &mut self.storage {
            self.user_words.clear();
            for (text, pinyin, _freq) in storage.load_user_words()? {
                // 字数==音节数守卫（同 new_with_db 加载过滤）：错位行
                // 不进内存/Trie（DB 行保留，跳过不删除）。
                if !super::user_word_pinyin_matches(&text, &pinyin) {
                    continue;
                }
                let pys = self.user_words.entry(text).or_default();
                if !pys.iter().any(|p| p == &pinyin) {
                    pys.push(pinyin);
                }
            }
            self.freq = FreqModel::new();
            for (text, freq) in storage.load_user_freq()? {
                self.freq.set_count(&text, freq);
            }
            // 英文使用计数同步重载（管理器/导入改库后的 DB 为准）
            self.user_english.clear();
            for (w, c) in storage.load_english_counts()? {
                self.user_english.insert(w, c);
            }
            self.english_delta.clear();
            self.input_mappings = storage.load_input_mappings()?;
            self.input_mappings_by_text = Self::build_mapping_index(&self.input_mappings);
            // 黑名单同步重载：词库管理器删除词时 remove() 直写
            // user_blacklist——运行中服务的 removed_texts 不知道，reload 后
            // 整句解码/绿词仍可能把词拼回来。以 DB 为准重载（启动加载同源，
            // 「commit 同文本清黑名单」的复活语义不受影响——那时 DB 行已删）。
            if let Ok(blacklist) = storage.load_blacklist() {
                self.removed_texts = blacklist;
            }
            self.rebuild_dict();
            self.pending_upserts.clear();
            self.pending_deletions.clear();
            self.dirty = false;
        }
        Ok(())
    }

    /// 内存状态是否已脏（需要 flush）
    pub fn is_user_dict_dirty(&self) -> bool {
        self.dirty || self.freq.is_dirty() || !self.english_delta.is_empty()
    }

    /// 引擎是否启用了 SQLite 持久化
    pub fn has_storage(&self) -> bool {
        self.storage.is_some()
    }

    /// 旧加密 DB 一次性迁移（加密/混淆已从引擎整体移除）。
    ///
    /// 历史发布版用户 DB 三表是 XOR 密文 + `freq_encryption_version=v1` 标记。
    /// 无密钥可解，迁移策略是丢弃密文数据：用明文种子覆盖 freq、清空种子搭配表
    /// （下一次 sync 重建）、清标记。详见 `Storage::migrate_legacy_encrypted_db_if_needed`。
    /// 迁移后必须重建内存状态（Trie + 种子 bigram），否则整个会话用密文垃圾。
    /// 返回 true=执行了迁移。
    pub fn migrate_legacy_encrypted_db_if_needed(&mut self, builtin_path: &str) -> bool {
        let migrated = match &mut self.storage {
            Some(storage) => match storage.migrate_legacy_encrypted_db_if_needed(builtin_path) {
                Ok(migrated) => migrated,
                Err(_) => return false,
            },
            None => false,
        };
        if migrated {
            // 迁移改了 x1dict freq + 种子搭配表，必须重建内存状态
            //（sync/restore 类写 DB 后立即 reload，否则整个会话用旧数据）。
            let _ = self.reload_x1dict();
            // 种子 bigram 内存表同步重载（trigram 是按需 SQL 查询，自动新鲜，无需处理）。
            if let Some(storage) = &self.storage {
                let counts = storage.load_seed_bigram_counts().unwrap_or_default();
                self.freq.set_seed_bigram_counts(counts);
            }
        }
        migrated
    }

    /// 查询当前 DB 中 x1dict_words 条目数（-1 表示未启用 SQLite 或查询失败）
    pub fn x1dict_count(&self) -> i64 {
        match &self.storage {
            Some(storage) => storage.x1dict_count().unwrap_or(-1),
            None => -1,
        }
    }

    /// 检查是否需要从内置 DB 同步 x1dict_words。
    ///
    /// 基于内容哈希判断，而非行数比较：
    /// - 仅当内置 DB 的内容哈希与用户 DB 中记录的上次同步哈希不同时返回 `true`
    /// - 用户在词库管理器中对系统词的增删改不会改变内置 DB，因此不会误触发同步
    /// - 首次安装（用户 DB 无记录）返回 `true`
    /// - 未启用 SQLite 或无法读取内置 DB 时返回 `false`
    pub fn check_x1dict_sync_needed(&self, builtin_path: &str) -> bool {
        match &self.storage {
            Some(storage) => storage.x1dict_sync_needed(builtin_path).unwrap_or(false),
            None => false,
        }
    }

    /// 从内置词库同步 x1dict_words，保留 user_words 不变。
    /// 同步完成后回填用户词中缺失的拼音，并立即重建内存状态（Trie + 种子 bigram）。
    /// 返回 true 表示成功。
    ///
    /// 双通道：storage.sync 的 INSERT OR IGNORE 只补缺词、不更新已有词 freq——
    /// 种子侧 freq 定标（多音字次音 / 词承载词频）也需传导到存量用户库。
    /// 因此 sync 成功后追加
    /// `restore_x1dict_freq_from`（按 (simplified,pinyin) 业务键 UPDATE freq/
    /// is_primary，不碰用户数据表），哈希门控下仅在种子内容变化的那次启动执行。
    /// 代价：restore 约 1-2s + 与 sync 共享同一次 Trie reload（不额外重建）。
    /// 注意：种子哈希变化时用户在词库管理器对系统词的 freq/is_primary 修改会被
    /// 种子值覆盖（仅种子变更时执行，比每次启动都 restore 更温和）。
    pub fn sync_x1dict_from_builtin(&mut self, builtin_path: &str) -> bool {
        let ok = match &mut self.storage {
            Some(storage) => {
                if storage.sync_x1dict_from_builtin(builtin_path).is_err() {
                    return false;
                }
                // 同步成功后记录内置 DB 的内容哈希，供下次启动时判断是否需要再次同步。
                let _ = storage.record_x1dict_sync(builtin_path);
                let _ = storage.backfill_user_pinyin_from_x1dict();
                // freq 修正通道：补缺词之外，把种子侧 freq 定标刷进
                // 已有词。失败静默降级（freq 保持旧值，不影响本次补词结果）。
                let _ = storage.restore_x1dict_freq_from(builtin_path);
                true
            }
            None => false,
        };
        if ok {
            // sync 后必须立即重建内存状态。
            // sync 调用方在主线程执行（与 process(&self) 同线程串行），
            // reload 修改 engine 内存无并发竞争。
            // 不 reload 的后果：部署更新后整个会话用旧词库——
            // DB 磁盘已是新词库，但运行引擎内存 Trie 是 sync 前的旧版，
            // 必须重启输入法进程才生效。
            // 代价：Trie 重建 ~300-400ms 主线程一次性 stall，仅 sync 发生（版本更新
            // 后首次启动）时付一次，远比整会话旧词库划算。
            let _ = self.reload_x1dict();
            // 种子 bigram 内存表同步重载（trigram 是按需 SQL 查询，自动新鲜，无需处理）。
            if let Some(storage) = &self.storage {
                let counts = storage.load_seed_bigram_counts().unwrap_or_default();
                self.freq.set_seed_bigram_counts(counts);
            }
        }
        ok
    }

    /// 获取全部用户词（供词库管理工具使用）。
    /// 多读音：同词每个读音一条（管理器里逐读音可编辑/删除）。
    /// 输入串的用户学习映射文本（多条取 count 最大者；无映射 None）。
    /// 绿词否决「用户学习优先」豁免用：AI drops 移除前核对
    /// 该输入用户教过的文本——与 x1_engine_user_mapping_for FFI 同语义
    /// （FFI 亦改调本方法，单一语义源）。全长度覆盖（非 green_memo 3-8 字
    /// 口径）；内存表线性扫微秒级，仅 AI 查询/落位链调用，不进按键热路径。
    pub fn user_mapping_for(&self, input: &str) -> Option<String> {
        self.input_mappings
            .iter()
            .filter(|((inp, _), _)| inp == input)
            .max_by_key(|(_, c)| **c)
            .map(|((_, text), _)| text.clone())
    }

    pub fn get_user_words(&self) -> Vec<(String, String, u32)> {
        let mut result: Vec<(String, String, u32)> = self
            .user_words
            .iter()
            .flat_map(|(text, pys)| {
                let freq = self.freq.counts.get(text).copied().unwrap_or(1);
                pys.iter()
                    .map(move |pinyin| (text.clone(), pinyin.clone(), freq))
            })
            .collect();
        // 键非 Copy（(CandidateSortKey, String)）→ 保留 sort_by（同 dict.rs 注）
        #[allow(clippy::unnecessary_sort_by)]
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }

    // decay_input_mappings（×7/8 时间衰减）已废除（时间不进权重公式）。
    // input_mappings 的「退化」由 competition.rs 整句层
    //（CtxKey::Input）零和归一承担——未选中的 (input,text) 被分母稀释。

    /// 删除一条用户词（从内存用户词库、Trie、频率模型中同时删除）
    pub fn delete_user_word(&mut self, text: &str) {
        if self.user_words.remove(text).is_some() {
            self.pending_upserts.remove(text);
            self.pending_deletions.insert(text.to_string());
            self.dirty = true;
        }
        self.dict.remove_by_text(text);
        self.freq.forget(text);
        self.seg_cache_clear();
    }

    /// 更新一条用户词的拼音和频率
    pub fn update_user_word(&mut self, text: &str, pinyin: &str, freq: u32) {
        // 字数==音节数守卫（同 learn）：词库管理器编辑
        // 拼音也可能造出错位条目，拒绝写入。
        if !super::user_word_pinyin_matches(text, pinyin) {
            return;
        }
        // 显式编辑 = 用新读音替换该词全部读音（多读音共存下，管理器编辑语义
        // 是"修正这个词"，不是追加）。
        self.user_words
            .insert(text.to_string(), vec![pinyin.to_string()]);
        // flush 时按内存 vec 清理旧读音行（否则旧读音行残留成"同词两条"，
        // 见 pending_reading_prune 字段注释）
        self.pending_reading_prune.insert(text.to_string());
        self.freq.set_count(text, freq);
        // 同步内存 Trie：先删除旧拼音对应的条目，再插入新拼音
        self.dict.remove_by_text(text);
        self.insert_user_word_to_dict(text, pinyin, freq);
        self.pending_deletions.remove(text);
        self.pending_upserts.insert(text.to_string());
        self.dirty = true;
    }

    /// 清空全部用户词（重置学习数据）
    pub fn clear_user_words(&mut self) {
        for text in self.user_words.keys() {
            self.pending_upserts.remove(text);
            self.pending_deletions.insert(text.clone());
        }
        self.user_words.clear();
        self.freq = FreqModel::new();
        self.dirty = true;
        self.rebuild_dict();
    }

    /// 清空输入习惯映射
    pub fn clear_input_mappings(&mut self) {
        self.input_mappings.clear();
        self.input_mappings_by_text.clear();
        self.dirty = true;
    }

    /// 从 input_mappings 全量构建反向索引（text → Vec<(input, count)>）。
    /// 加载 DB、clear 后重建用。commit 走增量更新（record_input_mapping）。
    pub(crate) fn build_mapping_index(
        mappings: &HashMap<(String, String), u32>,
    ) -> HashMap<String, Vec<(String, u32)>> {
        let mut idx: HashMap<String, Vec<(String, u32)>> = HashMap::new();
        for ((input, text), &count) in mappings {
            idx.entry(text.clone())
                .or_default()
                .push((input.clone(), count));
        }
        idx
    }

    /// 记录一条输入映射并同步反向索引（commit 路径用，替代直接操作 input_mappings）。
    pub(crate) fn record_input_mapping(&mut self, input: &str, text: &str) {
        let entry = self
            .input_mappings
            .entry((input.to_string(), text.to_string()))
            .or_insert(0);
        *entry += 1;
        let count = *entry;
        // 反向索引增量更新：该 text 的 Vec 里找已存在的同 input 更新 count，否则追加。
        let bucket = self
            .input_mappings_by_text
            .entry(text.to_string())
            .or_default();
        if let Some(slot) = bucket.iter_mut().find(|(inp, _)| inp == input) {
            slot.1 = count;
        } else {
            bucket.push((input.to_string(), count));
        }
    }

    /// 设置是否启用输入习惯映射学习与排序加成
    pub fn set_input_mapping_enabled(&mut self, enabled: bool) {
        self.input_mapping_enabled = enabled;
    }

    /// 设置智能纠错开关（实时生效，下次 process 读取）
    pub fn set_smart_correction_enabled(&mut self, enabled: bool) {
        self.smart_correction_enabled = enabled;
    }

    /// 设置键盘误触纠错开关（仅在 smart_correction_enabled=true 总闸开后生效）。
    /// 关闭后 maybe_correct 不再生成键盘误触变形（相邻键替换、多打删除），减少噪音橙词。
    pub fn set_keyboard_correction_enabled(&mut self, enabled: bool) {
        self.keyboard_correction_enabled = enabled;
    }

    /// 设置发音混淆纠错开关（仅在 smart_correction_enabled=true 总闸开后生效）。
    /// 关闭后不再生成发音混淆变形（zh/z、c/ch 等），且上下文证据发音营救
    ///（zongguo→中国 类合法输入纠错）一并关闭——两条发音路径同一开关统管。
    pub fn set_phonetic_correction_enabled(&mut self, enabled: bool) {
        self.phonetic_correction_enabled = enabled;
    }

    /// 设置键序颠倒纠错开关（仅在 smart_correction_enabled=true 总闸开后生效）。
    /// 关闭后 maybe_correct 不再生成键序颠倒变形（相邻字母交换）。
    pub fn set_swap_correction_enabled(&mut self, enabled: bool) {
        self.swap_correction_enabled = enabled;
    }

    /// 设置纠错候选数量上限（0=禁用补充，区别于总闸；推荐 3-10）。
    /// 用于控制橙色候选占比，避免挤占正常候选位。
    pub fn set_max_corrections(&mut self, n: u32) {
        // 钳制到 0..20 防止 preferences.json 被改成离谱值
        self.max_corrections = (n as usize).min(20);
    }

    /// 设置双击故障判定阈值（毫秒，偏好设置滑块）。钳制到 30..1000
    /// 防止 preferences.json 被改成离谱值。键盘 bounce 典型 <50ms，
    /// 默认 100 是宽松界；调大 = 更容易判定为故障（更积极折叠）。
    pub fn set_double_fault_threshold_ms(&mut self, ms: u32) {
        self.double_fault_threshold_ms = ms.clamp(30, 1000);
    }

    /// 设置单音节拆分识别开关（偏好设置「单音节拆分」）。
    /// 关闭后 yuan/tuan/mian/yue 等回到旧行为：只出完整音节候选，
    /// 用户想拆「预案」需手动打 '（yu'an）。
    pub fn set_ambig_split_enabled(&mut self, enabled: bool) {
        self.ambig_split_enabled = enabled;
    }

    /// 设置拆分词插入位（1 起算，偏好设置滑块）。钳制到 2..=5
    /// 防止 preferences.json 被改成离谱值——第 1 位留给完整音节的最优解
    /// （yuan 的单字），拆分词永远不抢首位由钳制下限保证。
    pub fn set_ambig_split_position(&mut self, pos: u32) {
        self.ambig_split_position = pos.clamp(2, 5) as u8;
    }

    /// 设置整句自动固化阈值（用户选过 N 次后自动固化，0=禁用）。
    /// 宿主偏好设置通过 FFI 实时同步。
    pub fn set_auto_solidify_threshold(&mut self, threshold: u32) {
        self.auto_solidify_threshold = threshold;
    }

    /// 当前是否启用输入习惯映射
    pub fn input_mapping_enabled(&self) -> bool {
        self.input_mapping_enabled
    }

    /// 导入用户词。
    /// `merge = true` 时合并（频率累加）；`merge = false` 时先清空再导入。
    pub fn import_user_words(&mut self, entries: &[(String, String, u32)], merge: bool) {
        if !merge {
            for text in self.user_words.keys() {
                self.pending_upserts.remove(text);
                self.pending_deletions.insert(text.clone());
            }
            self.user_words.clear();
            self.freq = FreqModel::new();
        }
        for (text, pinyin, freq) in entries {
            // 字数==音节数守卫（同 learn）：错位拼音行
            // 不入 user_words/Trie（详情见 user_word_pinyin_matches 注释）。
            if !super::user_word_pinyin_matches(text, pinyin) {
                continue;
            }
            let new_count = if let Some(old) = self.freq.counts.get(text).copied() {
                old.saturating_add(*freq)
            } else {
                *freq
            };
            {
                let pys = self.user_words.entry(text.to_string()).or_default();
                if !pys.iter().any(|p| p == pinyin) {
                    pys.push(pinyin.to_string());
                }
            }
            self.freq.set_count(text, new_count);
            self.insert_user_word_to_dict(text, pinyin, new_count);
            self.pending_deletions.remove(text);
            self.pending_upserts.insert(text.clone());
        }
        self.dirty = true;
    }
}
