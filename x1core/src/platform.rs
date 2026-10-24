//! platform —— 平台差异垫片：收敛全部平台相关代码。
//!
//! 架构边界：`x1core::engine` 及其依赖的算法/数据层不得出现任何平台分支
//! 或平台路径——同一份逻辑在各平台必须逐位等价。平台差异只允许存在于本
//! 模块及少数基础设施垫片（`mmap_file` / `stats` 的本地时间、`ffi` 的
//! panic 日志路径），且必须是同语义的 no-op 或等价实现，不改变引擎行为。
//!
//! 守护：`engine::tests_platform_purity` 扫描 `src/engine/*.rs`（不含
//! tests_*.rs）断言无 `cfg(target_os` / `cfg(windows` / `cfg(unix` /
//! `APPDATA` / `ProgramFiles` / `HKEY`。新增平台代码请放这里。

/// 把分配器攥着的空闲堆页立即归还系统（仅 macOS 有效果）。
///
/// 背景：启动加载的节点 Trie 构建/丢弃是几十 MB 级瞬态，释放的页会留在
/// malloc 空闲缓存里。`malloc_zone_pressure_relief(NULL, 0)` 让 libmalloc
/// 即刻归还全部可归还页（goal=0 = 尽量归还）。Windows 分配器行为不同
/// （空闲页不计入工作集，无对应 API），此处 no-op。
///
/// 调用点：`Engine::from_storage` 收尾（启动）与 `Engine::rebuild_dict`
/// 收尾（词库重载，同款构建瞬态）。仅内存归还，零数据影响、零行为差异。
pub(crate) fn release_allocator_slack() {
    #[cfg(target_os = "macos")]
    unsafe {
        extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut core::ffi::c_void, goal: usize) -> usize;
        }
        malloc_zone_pressure_relief(core::ptr::null_mut(), 0);
    }
    // 非 macOS：no-op（保持引擎行为跨平台一致；不引入任何平台语义）
    #[cfg(not(target_os = "macos"))]
    {
        // 空实现：Windows/Linux 分配器无等价操作
    }
}
