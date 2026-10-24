//! 用户词规模 → 内存占用探针（13 万用户词场景实测）。
//!
//! 语义 = 输入法进程的真实加载路径：种子 → dict.db 铺开 → user.db 写入
//! N 条用户词 → `Engine::open_split` 全量加载（user_words HashMap + Trie arena +
//! word_index + FreqModel counts）→ 报进程内存增量。
//!
//! 用法：cargo run -p x1core --release --example probe_mem_user_words -- <N> [work_dir]
//! 全程只动 work_dir 内的副本（缺省 test-tmp/probe_mem/），不触碰真实用户库与仓库种子。
//!
//! 平台口径（两端同尺）：
//! - macOS：RSS（ps）+ `malloc_zone_pressure_relief`（只归还分配器空闲页，
//!   「释放后 RSS」≈ 真实驻留）+ vmmap 区域分解；
//! - Windows：`K32GetProcessMemoryInfo`（WS = 任务管理器口径 + 私有提交 =
//!   页文件口径）+ `K32EmptyWorkingSet` 修剪演示——语义与 mac 不同：
//!   Windows 修剪会把**冷但在用**的页也请出，trim 后 WS 低估真实驻留，
//!   横向对比请用「私有提交增量」（提交量不受删除工作集影响）；
//! - 其他平台退化为 0（只保证能编译链接——CI 的 `cargo test` /
//!   `clippy --all-targets` 都会构建 examples）。
//!
//! 合成词规格（决定数字的可比性，改前先想清楚）：
//! - 字数分布 2 字 60% / 3 字 25% / 4 字 10% / 5 字 5%（贴近真实词长分布）
//! - 汉字取 CJK 区码点（UTF-8 均 3 字节，与真实常用字同宽）
//! - 读音 = 对应音节数、从固定音节池取，满足 user_word_pinyin_matches 守卫
//! - 词条两两互不相同（base-POOL 编码 + 长度区分，注入式唯一）

use std::path::PathBuf;
use std::time::Instant;
use x1core::storage::split::ensure_dict_db;
use x1core::storage::Storage;

/// 进程内存快照：工作集 KB + 私有提交 KB（仅 Windows 有；mac 侧置 0，其真实
/// 驻留看「压力释放后 RSS」行）。
#[derive(Clone, Copy, Default)]
struct MemSnap {
    ws_kb: u64,
    private_kb: u64,
}

#[cfg(target_os = "macos")]
fn snap() -> MemSnap {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .expect("ps 调用失败");
    MemSnap {
        ws_kb: String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0),
        private_kb: 0,
    }
}

/// Windows 内存口径（kernel32 的 K32* 导出，Win7+）。手写 FFI 不引 windows
/// crate：本探针是开发期诊断工具，保持零依赖；结构体布局 = psapi.h 的
/// PROCESS_MEMORY_COUNTERS（稳定 ABI，x64 对齐由 repr(C) + 字段序保证）。
#[cfg(windows)]
mod win_mem {
    use std::ffi::c_void;

    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn K32GetProcessMemoryInfo(
            process: *mut c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
        fn K32EmptyWorkingSet(process: *mut c_void) -> i32;
    }

    /// 当前进程伪句柄（GetCurrentProcess = (HANDLE)-1）。
    fn current_process() -> *mut c_void {
        -1isize as *mut c_void
    }

    /// (工作集字节, 私有提交字节)；调用失败回 (0, 0)。
    pub fn counters_bytes() -> (u64, u64) {
        let mut c: ProcessMemoryCounters = unsafe { std::mem::zeroed() };
        c.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
        let ok = unsafe { K32GetProcessMemoryInfo(current_process(), &mut c, c.cb) };
        if ok == 0 {
            return (0, 0);
        }
        (c.working_set_size as u64, c.pagefile_usage as u64)
    }

    /// EmptyWorkingSet：把可丢弃页请出物理驻留（提交量不变）。
    pub fn trim_working_set() -> bool {
        unsafe { K32EmptyWorkingSet(current_process()) != 0 }
    }
}

#[cfg(windows)]
fn snap() -> MemSnap {
    let (ws, private) = win_mem::counters_bytes();
    MemSnap {
        ws_kb: ws / 1024,
        private_kb: private / 1024,
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn snap() -> MemSnap {
    MemSnap::default()
}

#[cfg(target_os = "macos")]
extern "C" {
    /// macOS libmalloc：向系统归还分配器持有的空闲页（返回归还字节数）。
    fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
}

const POOL: usize = 597;
const SYLLABLES: &[&str] = &[
    "ba", "pa", "ma", "fa", "da", "ta", "na", "la", "ga", "ka", "ha", "ji", "qi", "xi", "zhi",
    "chi", "shi", "ri", "zi", "ci", "si", "yi", "wu", "yu", "bo", "po", "mo", "fo", "de", "te",
    "ne", "le", "ge", "ke", "he", "bi", "pi", "mi", "di", "ti", "ni", "li", "gu", "ku", "hu", "ju",
    "qu", "xu", "zhu", "chu", "shu", "ru", "zu", "cu", "su", "bu", "pu", "mu", "fu", "du", "tu",
    "nu", "lu", "ai", "ei", "ao", "ou", "an", "en", "ang", "eng", "er", "ya", "ye", "yao", "you",
    "yan", "yin", "yang", "ying", "wa", "wo", "wai", "wei", "wan", "wen", "wang", "weng", "yue",
    "yuan", "yun", "bei", "pei", "mei", "fei", "dei", "nei", "lei", "gei", "hei", "bao", "pao",
    "mao", "dao", "tao", "nao", "lao", "gao", "kao", "hao", "zhao", "chao", "shao", "rao", "zao",
    "cao", "sao", "bai", "pai", "mai", "dai", "tai", "nai", "lai", "gai", "kai", "hai", "zhou",
    "chou", "shou", "rou", "zou", "cou", "sou", "ban", "pan", "man", "fan", "dan", "tan", "nan",
    "lan", "gan", "kan", "han", "zhan", "chan", "shan", "ran", "zan", "can", "san", "ben", "pen",
    "men", "fen", "den", "nen", "gen", "ken", "hen", "zhen", "chen", "shen", "ren", "zen", "sen",
    "bang", "pang", "mang", "fang", "dang", "tang", "nang", "lang", "gang", "kang", "hang",
    "zhang", "chang", "shang", "rang", "zang", "cang", "sang", "beng", "peng", "meng", "feng",
    "deng", "teng", "neng", "leng", "geng", "keng", "heng", "zheng", "cheng", "sheng", "reng",
    "zeng", "ceng", "seng", "bing", "ping", "ming", "ding", "ting", "ning", "ling", "jing", "qing",
    "xing", "bing", "dong", "tong", "nong", "long", "gong", "kong", "hong", "zhong", "chong",
    "rong", "zong", "cong", "song",
];

/// 确定性合成 N 条互异用户词（text, pinyin, freq）。
fn gen_entries(n: usize) -> Vec<(String, String, u32)> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        // 词长分布：2 字 60% / 3 字 25% / 4 字 10% / 5 字 5%
        let bucket = (i.wrapping_mul(2654435761) >> 8) % 100;
        let len = match bucket {
            0..=59 => 2usize,
            60..=84 => 3,
            85..=94 => 4,
            _ => 5,
        };
        // base-POOL 编码 i → len 个互异字（i < POOL^len 保证注入式唯一）
        let mut v = i;
        let mut text = String::with_capacity(len * 3);
        let mut pinyins: Vec<&str> = Vec::with_capacity(len);
        for pos in 0..len {
            let d = v % POOL;
            v /= POOL;
            let cp = 0x4E00 + (d as u32 * 37) % 20_000 + 1;
            text.push(char::from_u32(cp).unwrap());
            pinyins.push(SYLLABLES[(i / (pos + 1) + d) % SYLLABLES.len()]);
        }
        let freq = 1 + (i.wrapping_mul(7919) % 999) as u32;
        out.push((text, pinyins.join(" "), freq));
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
    let work = PathBuf::from(args.get(2).cloned().unwrap_or_else(|| {
        x1core::test_tmp::dir()
            .join("probe_mem")
            .to_string_lossy()
            .into_owned()
    }));
    std::fs::create_dir_all(&work).expect("create work dir");

    let seed = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db"));
    let dict_db = work.join("dict.db");
    let user_db = work.join("user.db");
    let _ = std::fs::remove_file(&user_db);

    ensure_dict_db(&dict_db, &seed).expect("dict.db 铺开");

    // 写入 N 条用户词（真实学习也是写 user.db 的 user_words 表）
    if n > 0 {
        let mut st = Storage::open_split(&dict_db, &user_db).expect("open user db");
        let entries = gen_entries(n);
        let t = Instant::now();
        let added = st.import_user_words(&entries, false).expect("import");
        // 体积 = 主库 + WAL 边车（WAL 模式下新写入都在 -wal，只看主库是假象）
        let db_kb: u64 = ["", "-wal"]
            .iter()
            .map(|suffix| {
                std::fs::metadata(format!("{}{}", user_db.display(), suffix))
                    .map(|m| m.len() / 1024)
                    .unwrap_or(0)
            })
            .sum();
        println!(
            "① 写入 user_words: {} 条 / {}ms / user.db {}KB（含 WAL）",
            added,
            t.elapsed().as_millis(),
            db_kb
        );
    }

    let s0 = snap();
    let t = Instant::now();
    let engine = x1core::Engine::open_split(dict_db.to_str().unwrap(), user_db.to_str().unwrap())
        .expect("engine open_split");
    let load_ms = t.elapsed().as_millis();
    std::thread::sleep(std::time::Duration::from_millis(150));
    let s1 = snap();

    println!(
        "② 引擎加载: {}ms | WS before={}KB after={}KB (增量 {}KB/{}MB)",
        load_ms,
        s0.ws_kb,
        s1.ws_kb,
        s1.ws_kb.saturating_sub(s0.ws_kb),
        s1.ws_kb.saturating_sub(s0.ws_kb) / 1024
    );
    if s1.private_kb > 0 {
        println!(
            "   私有提交: before={}KB after={}KB (增量 {}KB/{}MB) ← Windows 横向对比主口径（不受工作集修剪影响）",
            s0.private_kb,
            s1.private_kb,
            s1.private_kb.saturating_sub(s0.private_kb),
            s1.private_kb.saturating_sub(s0.private_kb) / 1024
        );
    }

    // 加载后归还未用页（平台语义不同，见各分支注释）。
    #[cfg(target_os = "macos")]
    {
        // malloc_zone_pressure_relief：把 free() 后仍被 malloc 保留的空闲页归还
        // 系统。r1-r2 差额 = 构建期瞬态结构（节点 Trie/word_index 旧形态）的
        // 「已释放但未归还」部分；r2-r0 ≈ 真实驻留数据结构 + SQLite 页。
        let relieved = unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
        std::thread::sleep(std::time::Duration::from_millis(150));
        let s2 = snap();
        println!(
            "   压力释放: 归还 {}KB/{}MB | 释放后 RSS={}KB (相对加载前增量 {}KB/{}MB)",
            relieved / 1024,
            relieved / 1048576,
            s2.ws_kb,
            s2.ws_kb.saturating_sub(s0.ws_kb),
            s2.ws_kb.saturating_sub(s0.ws_kb) / 1024
        );
    }
    #[cfg(windows)]
    {
        // EmptyWorkingSet 修剪演示：与 mac 语义不同（连冷但在用的页一起请出，
        // trim 后 WS 低估真实驻留）——横向对比看上面的「私有提交增量」。
        let trimmed = win_mem::trim_working_set();
        std::thread::sleep(std::time::Duration::from_millis(150));
        let s2 = snap();
        println!(
            "   EmptyWorkingSet 修剪={} | 修剪后 WS={}KB（提交仍为 {}KB，提交不受修剪影响）",
            trimmed as i32, s2.ws_kb, s2.private_kb
        );
    }

    // vmmap 区域分解（macOS 专属；区分 malloc 堆 / 文件映射 / 其他）
    #[cfg(target_os = "macos")]
    {
        let pid = std::process::id().to_string();
        if let Ok(out) = std::process::Command::new("vmmap")
            .args(["--summary", &pid])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut in_zone_table = false;
            println!("③ vmmap --summary（关键行）:");
            for line in text.lines() {
                let l = line.trim();
                if l.starts_with("MALLOC ZONE") {
                    in_zone_table = true;
                    println!("   {l}");
                    continue;
                }
                if in_zone_table {
                    if l.is_empty() {
                        in_zone_table = false;
                    } else {
                        println!("   {l}");
                    }
                    continue;
                }
                if l.starts_with("MALLOC") || l.starts_with("TOTAL") || l.contains("mapped file") {
                    println!("   {l}");
                }
            }
        }
    }
    std::hint::black_box(&engine);
}
