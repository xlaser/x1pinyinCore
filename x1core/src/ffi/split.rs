//! 双库分离 FFI。
//!
//! **职责**：把引擎服务的分离引导链（X1_DB_SPLIT 分支）封装给无服务进程的
//! 宿主——引导与打开都在输入法进程内一次完成：
//!
//! ```text
//! x1_engine_new_split(data_dir, seed_path)
//!   ① ensure_dict_db：dict.db 与种子对齐（哈希门控 + 原子替换 + 修正保护备份）
//!   ② seed bigram 磁盘缓存预置（与 dict.db 同目录；缺省仅省首启重建）
//!      + x1grammar.bin 微型语法模型预置（同目录；model_hash 门控 + 原子替换）
//!   ③ 旧单库 x1pinyin.db → user.db 一次性迁移（零搬运 + 旧库改名保留）
//!   ④ Engine::open_split(dict, user)
//! ```
//!
//! **失败语义**：任一步失败返回 NULL——调用方**回退合并模式**
//! （`x1_engine_new_with_db` 旧行为）。分离是增强不是依赖，回退路径永远可用。
//!
//! **逃生口**：`X1_DB_SPLIT=0` 显式关闭（默认开启）。
//!
//! **约束**：种子 DB（签名 bundle 内）**只读打开**（`read_meta` 已是
//! READ_ONLY，ensure_dict_db 的复制源也只读）——绝不产生 WAL/-shm
//! 破坏签名密封。

use core::ffi::c_char;

use crate::engine::Engine;
use crate::storage::split::ensure_dict_db;
use crate::storage::Storage;

use super::X1Engine;

/// 分离模式创建引擎（引导链见模块注释）。
///
/// - `data_dir`：数据目录（mac：`~/Library/Application Support/x1pinyin`），
///   其下应有/将建 `dict.db`、`user.db`；旧单库 `x1pinyin.db` 存在则迁移；
/// - `seed_path`：种子 DB（bundle 内 `x1pinyin.db`），只读。
///
/// 返回 NULL = 引导或打开失败（调用方回退合并模式）。
///
/// # Safety
/// 两个指针必须是有效的 NUL 结尾 C 字符串；返回指针由 `x1_engine_free` 释放。
#[no_mangle]
pub unsafe extern "C" fn x1_engine_new_split(
    data_dir: *const c_char,
    seed_path: *const c_char,
) -> *mut X1Engine {
    super::guard("x1_engine_new_split", core::ptr::null_mut(), || {
        use std::path::PathBuf;

        // 逃生口：显式关闭（默认开启）
        if std::env::var("X1_DB_SPLIT")
            .map(|v| v == "0")
            .unwrap_or(false)
        {
            return core::ptr::null_mut();
        }

        let dir = match super::c_str_to_str(data_dir) {
            Some(s) if !s.is_empty() => PathBuf::from(s),
            _ => return core::ptr::null_mut(),
        };
        let seed = match super::c_str_to_str(seed_path) {
            Some(s) if !s.is_empty() => PathBuf::from(s),
            _ => return core::ptr::null_mut(),
        };
        if !seed.exists() {
            return core::ptr::null_mut(); // 无种子（异常部署）→ 回退
        }
        if std::fs::create_dir_all(&dir).is_err() {
            return core::ptr::null_mut();
        }

        let dict_db = dir.join("dict.db");
        let user_db = dir.join("user.db");
        let legacy = dir.join("x1pinyin.db");

        // ① dict.db 与种子对齐（哈希门控：种子未变只读一次元数据）
        if let Err(e) = ensure_dict_db(&dict_db, &seed) {
            eprintln!("[split] 铺开/更新 dict.db 失败（回退合并模式）: {}", e);
            return core::ptr::null_mut();
        }
        // ② seed bigram 缓存预置（与 dict.db 同目录；仅省首启，失败无害）
        if let Some(seed_dir) = seed.parent() {
            let src = seed_dir.join("seed_bigram_cache.bin");
            let dst = dir.join("seed_bigram_cache.bin");
            if src.exists() && !dst.exists() {
                let _ = std::fs::copy(&src, &dst);
            }
        }
        // ② b x1grammar.bin 微型语法模型预置（2026-11-01 方案 B Task 6）：与
        // dict.db 同目录（引擎发现逻辑 = dict_path().parent()/x1grammar.bin）。
        // model_hash 门控（种子未换代零复制）+ 临时文件原子替换（读者永见完整
        // 文件）。失败无害：源缺失/损坏、复制失败一律跳过，不阻塞引导、不写
        // 用户词表；源损坏时**不覆盖**用户目录既有可用模型（宁旧勿坏——
        // bundle 资产坏不该废掉上一代正常模型）。
        if let Some(seed_dir) = seed.parent() {
            let src = seed_dir.join("x1grammar.bin");
            let dst = dir.join("x1grammar.bin");
            if src.exists() {
                // 只信任能通过解码校验的源（load_from_path 内含 FNV 哈希门禁）
                let src_hash = crate::grammar_micro::GrammarMicro::load_from_path(&src)
                    .ok()
                    .map(|m| m.info().model_hash);
                let need_copy = match src_hash {
                    Some(h) => crate::grammar_micro::GrammarMicro::load_from_path(&dst)
                        .map(|old| old.info().model_hash != h)
                        .unwrap_or(true), // dst 缺失/损坏 → 复制
                    None => false, // 源损坏 → 不动用户目录现状
                };
                if need_copy {
                    let tmp = dir.join("x1grammar.bin.tmp");
                    if std::fs::copy(&src, &tmp).is_ok() {
                        let _ = std::fs::rename(&tmp, &dst);
                    } else {
                        let _ = std::fs::remove_file(&tmp); // 清半截临时文件
                    }
                }
            }
        }
        // ③ 旧单库 → user.db（幂等：已迁移/无旧库 = no-op；旧库改名保留）
        if let Err(e) = Storage::migrate_split_user_db_if_needed(&user_db, &legacy) {
            eprintln!("[split] 用户库迁移失败（回退合并模式）: {}", e);
            return core::ptr::null_mut();
        }
        // ④ 打开双库（缺失的 user.db 会按用户角色 schema 新建）
        match Engine::open_split(
            dict_db.to_string_lossy().as_ref(),
            user_db.to_string_lossy().as_ref(),
        ) {
            Ok(engine) => {
                let boxed = X1Engine { inner: engine };
                Box::into_raw(Box::new(boxed))
            }
            Err(e) => {
                // 自愈：残留 WAL/SHM（换代替换的遗留）会让 dict.db
                // 报 "database disk image is malformed" → 分离静默退化合并模式。
                // 挪开边车重试一次（挪动非删除，数据可人工恢复）。正常换代
                // 场景已由 ensure_dict_db 的替换前清理根治，这里是二层兜底。
                eprintln!("[split] open_split 失败，尝试清理边车后重试: {}", e);
                if crate::storage::split::quarantine_db_sidecars(&dict_db) {
                    match Engine::open_split(
                        dict_db.to_string_lossy().as_ref(),
                        user_db.to_string_lossy().as_ref(),
                    ) {
                        Ok(engine) => {
                            eprintln!("[split] 自愈成功：清理残留边车后分离模式可用");
                            return Box::into_raw(Box::new(X1Engine { inner: engine }));
                        }
                        Err(e2) => {
                            eprintln!("[split] 自愈后仍失败（回退合并模式）: {}", e2);
                            return core::ptr::null_mut();
                        }
                    }
                }
                eprintln!("[split] open_split 失败（回退合并模式）: {}", e);
                core::ptr::null_mut()
            }
        }
    })
}

/// 查询当前数据目录是否为分离布局（dict.db 存在）——供维护脚本/诊断判断。
/// 返回 1 = 分离布局，0 = 单库布局/不可用。
///
/// # Safety
/// `data_dir` 必须是有效的 NUL 结尾 C 字符串。
#[no_mangle]
pub unsafe extern "C" fn x1_split_layout(data_dir: *const c_char) -> i32 {
    super::guard("x1_split_layout", 0, || {
        match super::c_str_to_str(data_dir) {
            Some(s) if !s.is_empty() && std::path::Path::new(s).join("dict.db").exists() => 1,
            _ => 0,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::ffi::CString;

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = crate::test_tmp::dir().join(format!(
            "x1split_ffi_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 引导链端到端（真实种子的临时副本，不碰仓库种子）：
    /// 全新目录建库 → 旧库迁移零丢失 → 幂等重入 → 出词正常。
    #[test]
    fn bootstrap_fresh_migrate_idempotent() {
        let seed_src = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
        let dir = temp("chain");
        let seed = dir.join("seed.db");
        std::fs::copy(seed_src, &seed).unwrap();
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();

        unsafe {
            // ① 全新目录：无旧库 → dict.db 铺开 + user.db 新建 + 引擎可用
            let p = x1_engine_new_split(
                c(data.to_str().unwrap()).as_ptr(),
                c(seed.to_str().unwrap()).as_ptr(),
            );
            assert!(!p.is_null(), "全新目录引导应成功");
            let e = &*p;
            let cands = e.inner.process("nihao");
            assert!(
                cands.iter().any(|c| c.text.as_ref() == "你好"),
                "分离模式出词正常"
            );
            drop(Box::from_raw(p));
            assert!(data.join("dict.db").exists() && data.join("user.db").exists());

            // ② 学一个词并 flush → 落进 user.db（不在 dict.db）
            let p2 = x1_engine_new_split(
                c(data.to_str().unwrap()).as_ptr(),
                c(seed.to_str().unwrap()).as_ptr(),
            );
            assert!(!p2.is_null(), "幂等重入应成功");
            let e2 = &mut *p2;
            e2.inner
                .commit_with_solidify("ceshi", "测试词组", Some("ce shi ci zu"), true, true);
            e2.inner.flush().unwrap();
            drop(Box::from_raw(p2));
            let user_has: i64 = rusqlite::Connection::open(data.join("user.db"))
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM user_words WHERE text='测试词组'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(user_has, 1, "学习写入应落 user.db");
            let dict_has: i64 = rusqlite::Connection::open(data.join("dict.db"))
                .unwrap()
                .query_row("SELECT COUNT(*) FROM user_words", [], |r| r.get(0))
                .unwrap_or(-1);
            assert!(
                dict_has == 0 || dict_has == -1,
                "dict.db 不应有用户词（表被剥离或不存在）"
            );

            // ③ 旧单库迁移：造一个带用户数据的 legacy → 迁移 → 零丢失 + 旧库改名
            let data2 = dir.join("data2");
            std::fs::create_dir_all(&data2).unwrap();
            {
                let legacy = data2.join("x1pinyin.db");
                std::fs::copy(&seed, &legacy).unwrap();
                let conn = rusqlite::Connection::open(&legacy).unwrap();
                conn.execute(
                    "INSERT INTO user_words (text, pinyin, user_freq, created_at, updated_at)
                     VALUES ('迁移验证词', 'qian yi yan zheng ci', 7, 1, 1)",
                    [],
                )
                .unwrap();
            }
            let p3 = x1_engine_new_split(
                c(data2.to_str().unwrap()).as_ptr(),
                c(seed.to_str().unwrap()).as_ptr(),
            );
            assert!(!p3.is_null(), "旧库存在时引导应成功");
            drop(Box::from_raw(p3));
            let n: i64 = rusqlite::Connection::open(data2.join("user.db"))
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM user_words WHERE text='迁移验证词'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "旧库用户数据零丢失");
            assert!(!data2.join("x1pinyin.db").exists(), "旧库应已被改名");
            assert!(
                std::fs::read_dir(&data2).unwrap().flatten().any(|e| e
                    .file_name()
                    .to_string_lossy()
                    .starts_with("x1pinyin.db.migrated-")),
                "旧库改名保留为 .migrated-*.bak"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 失败回退：坏种子路径 / 空目录参数 → NULL（不 panic）。
    #[test]
    fn failure_returns_null() {
        let dir = temp("fail");
        unsafe {
            let p = x1_engine_new_split(
                c(dir.to_str().unwrap()).as_ptr(),
                c(dir.join("no_such_seed.db").to_str().unwrap()).as_ptr(),
            );
            assert!(p.is_null(), "无种子应返回 NULL");
            let p2 = x1_engine_new_split(c("").as_ptr(), c("").as_ptr());
            assert!(p2.is_null(), "空参数应返回 NULL");
            assert_eq!(x1_split_layout(c(dir.to_str().unwrap()).as_ptr()), 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// x1grammar.bin 预置（2026-11-01 方案 B Task 6）：首次复制 + 引擎端到端
    /// 发现、哈希一致零复制（mtime 钉住验证）、种子换代原子更新、源损坏不
    /// 覆盖既有模型、无源不产文件且引导照常成功。
    #[test]
    fn grammar_model_bootstrap_hash_gated() {
        if crate::test_tmp::skip_without_grammar_model() {
            return;
        }
        let seed_src = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1pinyin.db");
        let model_src = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/x1grammar.bin");
        let dir = temp("gm");
        let seed = dir.join("seed.db");
        std::fs::copy(seed_src, &seed).unwrap();
        std::fs::copy(model_src, dir.join("x1grammar.bin")).unwrap();
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let dst = data.join("x1grammar.bin");
        let src = dir.join("x1grammar.bin");

        // FNV-1a32（与 grammar_micro 构建端同算法）：测期构造「结构合法 +
        // model_hash 不同」的换代模型——翻转 payload 尾字节（block5 末行 de2
        // 计数的高位字节，只进总量不影响结构）后修补头部哈希。
        fn fnv1a32(b: &[u8]) -> u32 {
            b.iter().fold(0x811C_9DC5u32, |h, &x| {
                (h ^ x as u32).wrapping_mul(0x0100_0193)
            })
        }
        fn variant(base: &[u8]) -> Vec<u8> {
            let mut v = base.to_vec();
            let last = v.len() - 1;
            v[last] ^= 0x01;
            let h = fnv1a32(&v[32..]);
            v[8..12].copy_from_slice(&h.to_le_bytes());
            v
        }

        // 引导一次并断言成功（模型预置属「失败无害」，绝不允许它拖垮 split）
        fn boot_ok(data: &std::path::Path, seed: &std::path::Path) -> *mut X1Engine {
            let p = unsafe {
                x1_engine_new_split(
                    c(data.to_str().unwrap()).as_ptr(),
                    c(seed.to_str().unwrap()).as_ptr(),
                )
            };
            assert!(!p.is_null(), "split 引导应成功");
            p
        }

        // ① 首次：复制到数据目录 + 无临时文件残留 + 引擎端到端发现该副本
        {
            let p = boot_ok(&data, &seed);
            let e = unsafe { &mut *p };
            assert!(dst.exists(), "模型应已预置到数据目录");
            assert!(
                !data.join("x1grammar.bin.tmp").exists(),
                "临时文件应已收尾（rename 或清理）"
            );
            let info = e
                .inner
                .grammar_micro_info()
                .expect("引擎应发现 dict.db 同目录模型");
            assert_eq!(
                info.path.as_deref(),
                Some(dst.to_str().unwrap()),
                "引擎加载的应是数据目录副本（bundle 内是只读母版）"
            );
            unsafe { drop(Box::from_raw(p)) };
        }

        // ② 哈希一致（种子未换代）：dst mtime 钉到过去 → 再引导 → mtime 不动
        let pinned =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000); // 2020-09
        std::fs::OpenOptions::new()
            .write(true)
            .open(&dst)
            .unwrap()
            .set_modified(pinned)
            .unwrap();
        let p = boot_ok(&data, &seed);
        unsafe { drop(Box::from_raw(p)) };
        assert_eq!(
            std::fs::metadata(&dst).unwrap().modified().unwrap(),
            pinned,
            "种子未换代不应重写模型（model_hash 门控零复制）"
        );

        // ③ 种子换代：结构合法但哈希不同的新模型 → 原子更新 dst + 引擎加载新代
        let v = variant(&std::fs::read(model_src).unwrap());
        std::fs::write(&src, &v).unwrap();
        let p = boot_ok(&data, &seed);
        let e = unsafe { &mut *p };
        let got_hash = crate::grammar_micro::GrammarMicro::load_from_path(&dst)
            .expect("换代后 dst 应可解码")
            .info()
            .model_hash;
        assert_eq!(got_hash, fnv1a32(&v[32..]), "换代应传导到数据目录副本");
        assert_eq!(
            e.inner.grammar_micro_info().unwrap().model_hash,
            got_hash,
            "引擎本轮应加载新代模型"
        );
        unsafe { drop(Box::from_raw(p)) };

        // ④ 源损坏（截断）：不覆盖既有可用模型（宁旧勿坏），引导照常成功
        let dst_bytes = std::fs::read(&dst).unwrap();
        std::fs::write(&src, &v[..v.len() / 2]).unwrap(); // 半截 = 解码必败
        let p = boot_ok(&data, &seed);
        unsafe { drop(Box::from_raw(p)) };
        assert_eq!(
            std::fs::read(&dst).unwrap(),
            dst_bytes,
            "源损坏时既有模型必须原样保留"
        );

        // ⑤ 无源（bundle 未带资产的老部署常态）：不产文件、引导成功
        std::fs::remove_file(&src).unwrap();
        let data2 = dir.join("data2");
        std::fs::create_dir_all(&data2).unwrap();
        let p = boot_ok(&data2, &seed);
        let e = unsafe { &mut *p };
        assert!(
            !data2.join("x1grammar.bin").exists(),
            "无源不应产生模型文件"
        );
        assert!(
            e.inner.grammar_micro_info().is_none(),
            "无资产 → 引擎空模型降级（老部署常态）"
        );
        unsafe { drop(Box::from_raw(p)) };

        let _ = std::fs::remove_dir_all(&dir);
    }
}
