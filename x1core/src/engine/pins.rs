//! 固定候选（user_pins）。
//!
//! 用户把某输入串的字/词钉死在 1-3 号候选位（候选窗 Ctrl+数字），免疫字频/
//! 词频/上下文/绿词记忆的一切排序调整；其余槽位照常学习。
//!
//! 语义定稿：
//! - **键 = 原始输入串**（green_memo 同口径）：全拼 `wo`、简拼 `w`、双拼
//!   原文 `vs` 各是各的键，互不触发；音节级生效（women 里的 wo 也固定）
//!   被明确否决——不碰 DP 整句解码，回归风险最小。
//! - **槽位规则**：同输入同文本只占一个槽（固定到其他槽 = 移动）；槽位被占
//!   直接覆盖；同槽位再固定同文本 = 解除（切换语义，DLL 侧判断后调 unpin）。
//! - **注入点 = 候选全组装之后、返回之前**：晚于 green_memo 提升/整句插入/
//!   纠错追加，固定项天然压过一切提升逻辑（绿词被顶到第一个非固定槽，
//!   不需要改 green_memo 代码）。
//! - 固定文本不在当前候选列表里 = 该槽本次静默失效（不凭空造候选——
//!   造候选需要拼音/freq，且「固定后词库删词」场景隐身最安全）。
//!
//! 持久化：user_pins 表（storage/user_words.rs），启动全量加载（小表），
//! 写路径单行 upsert/delete 立即落盘（不走 60s flush——固定是显式低频操作，
//! 用户预期立即生效且重启不丢）。

use super::{Candidate, Engine};

/// 一个输入串的固定槽位表（下标 0..2 = 1-3 号位）。
pub(crate) type PinSlots = [Option<String>; 3];

/// 固定条目总数护栏（防 IPC 恶意/异常灌入撑爆内存与启动加载）。
const MAX_PIN_ENTRIES: usize = 512;

impl Engine {
    /// 查询某输入串的固定槽表（服务层标注候选 JSON 用；命中才克隆，未命中 None）。
    /// 返回克隆而非引用：PinSlots 是 pub(crate) 类型，不能泄漏到公开签名。
    pub fn pins_for(&self, input: &str) -> Option<[Option<String>; 3]> {
        self.pins.get(input).cloned()
    }

    /// 固定条目总数（运行信息展示用）。
    pub fn pin_count(&self) -> usize {
        self.pins
            .values()
            .map(|s| s.iter().filter(|x| x.is_some()).count())
            .sum()
    }

    /// 全部固定项列表（管理界面用）：(input, slot[1-3], text)，按 input/slot 排序。
    pub fn list_pins(&self) -> Vec<(String, u8, String)> {
        let mut out = Vec::new();
        for (input, slots) in &self.pins {
            for (i, t) in slots.iter().enumerate() {
                if let Some(text) = t {
                    out.push((input.clone(), (i + 1) as u8, text.clone()));
                }
            }
        }
        out.sort();
        out
    }

    /// 固定字/词到指定槽位（slot 1-3）。
    ///
    /// 语义：同输入同文本先从其他槽移除（移动语义），再覆盖目标槽（占用者
    /// 被顶替解除）。内存立即生效 + 立即落盘；内存模式（无 storage）只改内存。
    pub fn pin_candidate(&mut self, input: &str, slot: u8, text: &str) -> Result<(), String> {
        if !(1..=3).contains(&slot) {
            return Err(format!("slot 必须在 1-3: {}", slot));
        }
        // 键 = 原始输入串：允许小写字母、撇号与分号——';' 是部分双拼方案的
        // ing 键（x; = xing），双拼键域对齐 shuangpin::is_valid_key。
        // 长度护栏与 process 输入同口径（MAX_INPUT_BYTES）。
        if input.is_empty()
            || input.len() > crate::pinyin::MAX_INPUT_BYTES
            || !input
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '\'' || c == ';')
        {
            return Err(format!("非法固定键: {:?}", input));
        }
        if !super::is_valid_chinese_text(text) || text.chars().count() > 32 {
            return Err(format!("非法固定文本: {:?}", text));
        }
        let exists = self.pins.contains_key(input);
        if !exists {
            let total: usize = self.pins.values().map(|s| s.iter().flatten().count()).sum();
            if total >= MAX_PIN_ENTRIES {
                return Err(format!("固定候选已达上限 {}", MAX_PIN_ENTRIES));
            }
        }

        let slots = self.pins.entry(input.to_string()).or_default();
        // 移动语义：同输入下该文本已在其他槽 → 先摘掉
        for s in slots.iter_mut() {
            if s.as_deref() == Some(text) {
                *s = None;
            }
        }
        slots[(slot - 1) as usize] = Some(text.to_string());
        // 空壳清理（同输入无任何固定时不留空数组，pins_for 语义干净）
        if slots.iter().all(|s| s.is_none()) {
            self.pins.remove(input);
        }

        if let Some(storage) = self.storage.as_mut() {
            if let Err(e) = storage.upsert_pin(input, slot, text) {
                // 内存已生效、落盘失败：记录并继续（重启后丢失，比打字中断可取）
                eprintln!("[x1core] user_pins 落盘失败: {}", e);
            }
        }
        Ok(())
    }

    /// 解除一个固定位（slot 1-3）。不存在的槽 = 幂等成功。
    pub fn unpin_candidate(&mut self, input: &str, slot: u8) -> Result<(), String> {
        if !(1..=3).contains(&slot) {
            return Err(format!("slot 必须在 1-3: {}", slot));
        }
        if let Some(slots) = self.pins.get_mut(input) {
            slots[(slot - 1) as usize] = None;
            if slots.iter().all(|s| s.is_none()) {
                self.pins.remove(input);
            }
        }
        if let Some(storage) = self.storage.as_mut() {
            if let Err(e) = storage.delete_pin(input, slot) {
                eprintln!("[x1core] user_pins 删除失败: {}", e);
            }
        }
        Ok(())
    }

    /// 候选槽位插入（process_inner / process_shuangpin_inner 返回前调用）。
    ///
    /// 槽位精确语义：固定项落在自己的槽位下标上（1 号位 = index 0）；靠前的
    /// 空槽用普通候选填补（把靠后的固定项顶到精确位置）；普通候选整体顺延
    /// 不被丢弃。候选数不足时固定项自然前移（clamp，候选框无空洞）。
    pub(crate) fn apply_pins(&self, input: &str, candidates: &mut Vec<Candidate>) {
        let Some(slots) = self.pins.get(input) else {
            return;
        };
        if candidates.is_empty() {
            return;
        }
        // 从候选中摘除固定项（按文本匹配；候选列表已 dedup 文本唯一）
        let mut pinned: [Option<Candidate>; 3] = [None, None, None];
        for (i, text) in slots.iter().enumerate() {
            if let Some(t) = text {
                if let Some(pos) = candidates
                    .iter()
                    .position(|c| c.text.as_ref() == t.as_str())
                {
                    pinned[i] = Some(candidates.remove(pos));
                }
            }
        }
        if pinned.iter().all(|p| p.is_none()) {
            return;
        }
        let mut out: Vec<Candidate> = Vec::with_capacity(candidates.len() + 3);
        let mut normals = std::mem::take(candidates).into_iter();
        for i in 0..3 {
            match pinned[i].take() {
                Some(c) => out.push(c),
                // 仅当后面还有固定槽要顶到精确位置时才用普通候选填空
                None => {
                    if pinned[i + 1..].iter().any(|p| p.is_some()) {
                        if let Some(n) = normals.next() {
                            out.push(n);
                        }
                    }
                }
            }
        }
        out.extend(normals);
        *candidates = out;
    }
}
