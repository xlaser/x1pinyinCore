//! 只读文件映射（mmap）—— 零依赖平台 FFI（词库缓存零拷贝装载用）。
//!
//! 用途：`dict_arena_cache.bin` 的零拷贝装载——映射区只读、进程生命周期内
//! 文件内容不被改写（重写前必先替换持有映射的 Dict，见 dict_cache 模块头），
//! 因此 `MmapRegion` 可以安全地标记 Send/Sync（共享只读区，Drop 时最后一个
//! 引用释放才 unmap）。
//!
//! 设计纪律：
//! - **任何失败一律返回 None**（open/映射/平台差异），调用方回退普通读——
//!   本模块的正确性失败面 = 「映射没成功」，绝不产生错误数据；
//! - 平台面只用手写的最小 FFI（不引第三方 crate，维持 x1core 依赖面）：
//!   unix = open/mmap(PROT_READ, MAP_PRIVATE)/munmap/close；
//!   Windows = CreateFileW(FILE_SHARE_READ|WRITE|DELETE) / GetFileSizeEx /
//!   CreateFileMappingW(PAGE_READONLY) / MapViewOfFile / UnmapViewOfFile / CloseHandle；
//! - Windows 侧映射建立后立即关闭文件/映射句柄（视图保持有效，标准语义），
//!   Drop 只 UnmapViewOfFile；unix 侧同样 mmap 后关 fd。

use std::path::Path;
use std::sync::Arc;

/// 只读映射区。`bytes()` 在 Drop 前恒有效；映射失败时 `open` 返回 None。
pub(crate) struct MmapRegion {
    ptr: *const u8,
    len: usize,
}

// SAFETY：映射区只读且在本进程内不可变（文件重写前必先释放本映射，见模块头）；
// 跨线程共享只读字节切片安全。Drop 只在最后一个 Arc 引用释放时 unmap。
unsafe impl Send for MmapRegion {}
unsafe impl Sync for MmapRegion {}

impl MmapRegion {
    /// 打开并映射（长度 = 文件当前大小）。任何失败 → None。
    pub(crate) fn open(path: &Path) -> Option<Arc<MmapRegion>> {
        let len = std::fs::metadata(path).ok()?.len() as usize;
        if len == 0 {
            return None; // 空文件无映射意义
        }
        let ptr = platform::map(path, len)?;
        Some(Arc::new(MmapRegion { ptr, len }))
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub(crate) fn bytes(&self) -> &[u8] {
        // SAFETY：ptr 来自成功映射、len 为映射长度、Drop 前保持有效；
        // 只读访问不产生别名可变引用。
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for MmapRegion {
    fn drop(&mut self) {
        platform::unmap(self.ptr, self.len);
    }
}

#[cfg(unix)]
mod platform {
    use std::ffi::c_void;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    extern "C" {
        fn open(path: *const i8, flags: i32, mode: i32) -> i32;
        fn close(fd: i32) -> i32;
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> i32;
    }

    const O_RDONLY: i32 = 0;
    const PROT_READ: i32 = 1;
    const MAP_PRIVATE: i32 = 2;
    const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;

    pub(super) fn map(path: &Path, len: usize) -> Option<*const u8> {
        let mut cpath = path.as_os_str().as_bytes().to_vec();
        cpath.push(0);
        // SAFETY：cpath 为 NUL 结尾字节串；只读打开失败返回负值，直接拒绝。
        let fd = unsafe { open(cpath.as_ptr() as *const i8, O_RDONLY, 0) };
        if fd < 0 {
            return None;
        }
        // SAFETY：fd 有效、len>0、偏移 0；失败返回 MAP_FAILED。
        let p = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0) };
        // fd 可立即关闭：映射建立后独立于 fd（POSIX 语义）。
        unsafe {
            close(fd);
        }
        if p == MAP_FAILED || p.is_null() {
            return None;
        }
        Some(p as *const u8)
    }

    pub(super) fn unmap(ptr: *const u8, len: usize) {
        // SAFETY：ptr/len 来自成功映射，本函数仅在 Drop 调用一次。
        unsafe {
            munmap(ptr as *mut c_void, len);
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    type Handle = *mut c_void;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_SHARE_DELETE: u32 = 0x4;
    const OPEN_EXISTING: u32 = 3;
    const PAGE_READONLY: u32 = 0x02;
    const FILE_MAP_READ: u32 = 0x0004;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            sa: *mut c_void,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn GetFileSizeEx(h: Handle, size: *mut i64) -> i32;
        fn CreateFileMappingW(
            h: Handle,
            sa: *mut c_void,
            protect: u32,
            max_hi: u32,
            max_lo: u32,
            name: *const u16,
        ) -> Handle;
        fn MapViewOfFile(
            m: Handle,
            access: u32,
            off_hi: u32,
            off_lo: u32,
            len: usize,
        ) -> *mut c_void;
        fn UnmapViewOfFile(addr: *const c_void) -> i32;
        fn CloseHandle(h: Handle) -> i32;
    }

    pub(super) fn map(path: &Path, len: usize) -> Option<*const u8> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY：wide 为 NUL 结尾宽串；共享位全开（READ|WRITE|DELETE）——让
        // 缓存重写（tmp+删+改名）与杀软扫描不致互锁；只读访问 + OPEN_EXISTING。
        let file = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if file == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut size: i64 = 0;
        // SAFETY：file 有效句柄；size 可写。
        if unsafe { GetFileSizeEx(file, &mut size) } == 0 || size as usize != len {
            unsafe { CloseHandle(file) };
            return None; // 文件在 metadata 与打开之间变了 → 放弃映射（回退读）
        }
        // SAFETY：file 有效；其余参数按文档。
        let mapping = unsafe {
            CreateFileMappingW(
                file,
                std::ptr::null_mut(),
                PAGE_READONLY,
                0,
                0,
                std::ptr::null(),
            )
        };
        if mapping == INVALID_HANDLE_VALUE || mapping.is_null() {
            unsafe { CloseHandle(file) };
            return None;
        }
        // SAFETY：mapping 有效；偏移 0、长度 0 = 全文件映射。
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0) };
        // 视图建立后文件/映射句柄可立即关闭（视图独立保持有效，标准语义）——
        // 也保证不阻塞后续（他进程）对缓存文件的删改。
        unsafe {
            CloseHandle(mapping);
            CloseHandle(file);
        }
        if view.is_null() {
            return None;
        }
        Some(view as *const u8)
    }

    pub(super) fn unmap(ptr: *const u8, _len: usize) {
        // SAFETY：ptr 来自成功映射，本函数仅在 Drop 调用一次。
        unsafe {
            UnmapViewOfFile(ptr as *const c_void);
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::path::Path;

    pub(super) fn map(_path: &Path, _len: usize) -> Option<*const u8> {
        None // 其他平台：不映射，调用方回退普通读
    }

    pub(super) fn unmap(_ptr: *const u8, _len: usize) {}
}
