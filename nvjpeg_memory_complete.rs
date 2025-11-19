// nvjpeg_memory_complete.rs
// 完整复刻 NVIDIA DALI nvjpeg_memory.cc 的所有功能
// 特性:
// - Best-Fit 分配算法 + 碎片驱逐策略
// - 读写锁优化并发性能
// - 三种内存类型支持 (Device/Pinned/Host)
// - RAII 风格内存管理
// - 完整的异常处理
// - 跨平台支持 (Windows/Linux)
// - 线程隔离内存池
// - 统计信息输出

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread::{self, ThreadId};
use std::fs::OpenOptions;
use std::io::Write as IoWrite;

// ============================================================================
// 1. CUDA FFI (跨平台)
// ============================================================================

pub type CudaSizeT = usize;
pub type CudaInt = i32;
pub type CudaUint = u32;

pub const CUDA_SUCCESS: CudaInt = 0;
pub const CUDA_ERROR_MEMORY_ALLOCATION: CudaInt = 2;
pub const CUDA_ERROR_UNKNOWN: CudaInt = 999;

#[cfg(target_os = "windows")]
#[link(name = "cudart")]
extern "C" {
    pub fn cudaMalloc(devPtr: *mut *mut c_void, size: CudaSizeT) -> CudaInt;
    pub fn cudaMallocHost(ptr: *mut *mut c_void, size: CudaSizeT) -> CudaInt;
    pub fn cudaFree(devPtr: *mut c_void) -> CudaInt;
    pub fn cudaFreeHost(ptr: *mut c_void) -> CudaInt;
}

#[cfg(not(target_os = "windows"))]
#[link(name = "cudart")]
extern "C" {
    pub fn cudaMalloc(devPtr: *mut *mut c_void, size: CudaSizeT) -> CudaInt;
    pub fn cudaMallocHost(ptr: *mut *mut c_void, size: CudaSizeT) -> CudaInt;
    pub fn cudaFree(devPtr: *mut c_void) -> CudaInt;
    pub fn cudaFreeHost(ptr: *mut c_void) -> CudaInt;
}

// 普通 malloc/free (用于 Host 内存)
extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

// ============================================================================
// 2. 核心数据结构
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum MemoryKind {
    Device = 0,
    Pinned = 1,
    Host = 2,   // 普通 malloc，不占用 Pinned 资源
}

impl MemoryKind {
    const COUNT: usize = 3;

    fn index(&self) -> usize {
        *self as usize
    }

    fn from_index(idx: usize) -> Option<Self> {
        match idx {
            0 => Some(MemoryKind::Device),
            1 => Some(MemoryKind::Pinned),
            2 => Some(MemoryKind::Host),
            _ => None,
        }
    }
}

// Deleter 函数指针类型
type DeleterFn = Box<dyn Fn(*mut c_void) + Send + Sync>;

// AllocInfo: 记录每个分配的元数据
struct AllocInfo {
    kind: MemoryKind,
    size: usize,
    thread_id: ThreadId,
    deleter: Arc<DeleterFn>,
}

// Buffer: 带智能指针管理的内存块
struct Buffer {
    ptr: *mut c_void,
    kind: MemoryKind,
    size: usize,
    deleter: Arc<DeleterFn>,
}

impl Buffer {
    fn new(ptr: *mut c_void, kind: MemoryKind, size: usize, deleter: Arc<DeleterFn>) -> Self {
        Self { ptr, kind, size, deleter }
    }

    fn release(self) -> *mut c_void {
        let ptr = self.ptr;
        std::mem::forget(self); // 防止 Drop
        ptr
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            (self.deleter)(self.ptr);
        }
    }
}

unsafe impl Send for Buffer {}

// 统计信息
#[derive(Default, Clone, Copy, Debug)]
struct MemoryStats {
    nallocs: usize,
    biggest_alloc: usize,
}

// 每线程内存池: [Device 列表, Pinned 列表, Host 列表]
type ThreadMemoryPool = [Vec<Buffer>; MemoryKind::COUNT];

// ============================================================================
// 3. 内存管理器 (Singleton)
// ============================================================================

struct NvjpegMemoryManager {
    // Buffer Pool: ThreadId -> [Vec<Buffer>; 3]
    // 使用 RwLock 优化并发读性能
    buffer_pool: RwLock<HashMap<ThreadId, ThreadMemoryPool>>,

    // Allocation Info: Ptr -> AllocInfo
    // 使用 RwLock 优化查询性能
    alloc_info: RwLock<HashMap<usize, AllocInfo>>,

    // Statistics
    stats: Mutex<[MemoryStats; MemoryKind::COUNT]>,
    stats_enabled: std::sync::atomic::AtomicBool,

    // 配置项
    restrict_pinned_mem: std::sync::atomic::AtomicBool,
}

impl NvjpegMemoryManager {
    fn new() -> Self {
        Self {
            buffer_pool: RwLock::new(HashMap::new()),
            alloc_info: RwLock::new(HashMap::new()),
            stats: Mutex::new([MemoryStats::default(); MemoryKind::COUNT]),
            stats_enabled: std::sync::atomic::AtomicBool::new(true),
            restrict_pinned_mem: std::sync::atomic::AtomicBool::new(false),
        }
    }

    // ========================================================================
    // 配置管理
    // ========================================================================

    pub fn set_enable_mem_stats(&self, enabled: bool) {
        self.stats_enabled.store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn set_restrict_pinned_mem(&self, restrict: bool) {
        self.restrict_pinned_mem.store(restrict, std::sync::atomic::Ordering::Relaxed);
    }

    fn restrict_pinned_mem_usage(&self) -> bool {
        self.restrict_pinned_mem.load(std::sync::atomic::Ordering::Relaxed)
    }

    // ========================================================================
    // 统计信息
    // ========================================================================

    fn add_stats(&self, kind: MemoryKind, size: usize) {
        if !self.stats_enabled.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }

        let mut stats = self.stats.lock().unwrap();
        let idx = kind.index();
        stats[idx].nallocs += 1;
        if size > stats[idx].biggest_alloc {
            stats[idx].biggest_alloc = size;
        }
    }

    pub fn print_stats(&self) {
        if !self.stats_enabled.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }

        let stats = self.stats.lock().unwrap();

        // 支持输出到文件 (类似 DALI 的 DALI_LOG_FILE)
        let log_path = std::env::var("DALI_LOG_FILE").ok();

        let mut output: Box<dyn IoWrite> = if let Some(path) = log_path {
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => Box::new(file),
                Err(e) => {
                    eprintln!("Failed to open log file {}: {}", path, e);
                    Box::new(std::io::stdout())
                }
            }
        } else {
            Box::new(std::io::stdout())
        };

        let _ = writeln!(output, "#################### NVJPEG STATS ####################");
        let dev = stats[MemoryKind::Device.index()];
        let _ = writeln!(output, "Device memory: {} allocations, largest = {} bytes",
                         dev.nallocs, dev.biggest_alloc);
        let pin = stats[MemoryKind::Pinned.index()];
        let _ = writeln!(output, "Host (pinned) memory: {} allocations, largest = {} bytes",
                         pin.nallocs, pin.biggest_alloc);
        let host = stats[MemoryKind::Host.index()];
        let _ = writeln!(output, "Host (regular) memory: {} allocations, largest = {} bytes",
                         host.nallocs, host.biggest_alloc);
        let _ = writeln!(output, "################## END NVJPEG STATS ##################");
    }

    // ========================================================================
    // 内存分配核心逻辑
    // ========================================================================

    fn create_deleter(kind: MemoryKind) -> Arc<DeleterFn> {
        Arc::new(Box::new(move |ptr: *mut c_void| {
            if ptr.is_null() { return; }
            unsafe {
                match kind {
                    MemoryKind::Device => { cudaFree(ptr); }
                    MemoryKind::Pinned => { cudaFreeHost(ptr); }
                    MemoryKind::Host => { free(ptr); }
                }
            }
        }))
    }

    fn allocate_raw(&self, kind: MemoryKind, size: usize) -> Result<*mut c_void, CudaInt> {
        let mut ptr: *mut c_void = ptr::null_mut();

        let result = unsafe {
            match kind {
                MemoryKind::Device => cudaMalloc(&mut ptr as *mut _, size),
                MemoryKind::Pinned => cudaMallocHost(&mut ptr as *mut _, size),
                MemoryKind::Host => {
                    ptr = malloc(size);
                    if ptr.is_null() {
                        CUDA_ERROR_MEMORY_ALLOCATION
                    } else {
                        CUDA_SUCCESS
                    }
                }
            }
        };

        if result != CUDA_SUCCESS {
            return Err(result);
        }

        Ok(ptr)
    }

    fn allocate_fresh(
        &self,
        thread_id: ThreadId,
        kind: MemoryKind,
        size: usize,
    ) -> Result<*mut c_void, CudaInt> {
        let ptr = self.allocate_raw(kind, size)?;
        let deleter = Self::create_deleter(kind);

        // 记录到 alloc_info
        {
            let mut info_map = self.alloc_info.write().unwrap();
            info_map.insert(
                ptr as usize,
                AllocInfo {
                    kind,
                    size,
                    thread_id,
                    deleter: deleter.clone(),
                },
            );
        }

        self.add_stats(kind, size);
        Ok(ptr)
    }

    pub fn get_buffer(&self, thread_id: ThreadId, kind: MemoryKind, size: usize) -> Result<*mut c_void, CudaInt> {
        // 1. 读锁查找线程池
        let pool_exists = {
            let pool_map = self.buffer_pool.read().unwrap();
            pool_map.contains_key(&thread_id)
        };

        if !pool_exists {
            // 没有预分配的池，直接分配新内存
            return self.allocate_fresh(thread_id, kind, size);
        }

        // 2. Best-Fit 搜索 (读锁阶段)
        let (best_fit_idx, smallest_idx) = {
            let pool_map = self.buffer_pool.read().unwrap();
            let buffers = &pool_map[&thread_id][kind.index()];

            let mut best_fit_idx = None;
            let mut smallest_idx = None;
            let mut best_size = usize::MAX;
            let mut min_size = usize::MAX;

            for (i, buf) in buffers.iter().enumerate() {
                // 最小的 (用于驱逐)
                if buf.size < min_size {
                    min_size = buf.size;
                    smallest_idx = Some(i);
                }

                // Best-Fit
                if buf.size >= size && buf.size < best_size {
                    best_size = buf.size;
                    best_fit_idx = Some(i);
                }
            }

            (best_fit_idx, smallest_idx)
        };

        // 3. 找到合适的 Buffer，取出
        if let Some(idx) = best_fit_idx {
            let buffer = {
                let mut pool_map = self.buffer_pool.write().unwrap();
                let buffers = &mut pool_map.get_mut(&thread_id).unwrap()[kind.index()];
                buffers.swap_remove(idx)
            };

            let ptr = buffer.release();

            // 记录到 alloc_info (从池子拿出 = 重新分配)
            {
                let mut info_map = self.alloc_info.write().unwrap();
                info_map.insert(
                    ptr as usize,
                    AllocInfo {
                        kind: buffer.kind,
                        size: buffer.size,
                        thread_id,
                        deleter: buffer.deleter.clone(),
                    },
                );
            }

            return Ok(ptr);
        }

        // 4. 没找到 Best-Fit，执行驱逐策略
        if let Some(idx) = smallest_idx {
            let mut pool_map = self.buffer_pool.write().unwrap();
            let buffers = &mut pool_map.get_mut(&thread_id).unwrap()[kind.index()];
            buffers.swap_remove(idx); // Drop 会自动释放
        }

        // 5. 分配新内存
        self.allocate_fresh(thread_id, kind, size)
    }

    pub fn return_buffer_to_pool(&self, ptr: *mut c_void) -> CudaInt {
        if ptr.is_null() {
            return CUDA_SUCCESS;
        }

        // 1. 获取 AllocInfo (并保留在 map 中，后续移除)
        let info = {
            let info_map = self.alloc_info.read().unwrap();
            match info_map.get(&(ptr as usize)) {
                Some(info) => AllocInfo {
                    kind: info.kind,
                    size: info.size,
                    thread_id: info.thread_id,
                    deleter: info.deleter.clone(),
                },
                None => {
                    eprintln!(
                        "NVJPEG_MEMORY ERROR: Attempt to free unknown pointer {:p}",
                        ptr
                    );
                    return CUDA_ERROR_UNKNOWN;
                }
            }
        };

        // 2. 从 alloc_info 移除
        {
            let mut info_map = self.alloc_info.write().unwrap();
            info_map.remove(&(ptr as usize));
        }

        // 3. 归还到池子
        let buffer = Buffer::new(ptr, info.kind, info.size, info.deleter);

        {
            let mut pool_map = self.buffer_pool.write().unwrap();
            let thread_pool = pool_map
                .entry(info.thread_id)
                .or_insert_with(|| Default::default());
            thread_pool[info.kind.index()].push(buffer);
        }

        CUDA_SUCCESS
    }

    pub fn add_buffer(&self, thread_id: ThreadId, kind: MemoryKind, size: usize) -> Result<(), CudaInt> {
        let ptr = self.allocate_raw(kind, size)?;
        let deleter = Self::create_deleter(kind);
        let buffer = Buffer::new(ptr, kind, size, deleter);

        {
            let mut pool_map = self.buffer_pool.write().unwrap();
            let thread_pool = pool_map
                .entry(thread_id)
                .or_insert_with(|| Default::default());
            thread_pool[kind.index()].push(buffer);
        }

        self.add_stats(kind, size);
        Ok(())
    }

    pub fn delete_all_buffers(&self, thread_id: ThreadId) {
        let mut pool_map = self.buffer_pool.write().unwrap();
        if let Some(mut pools) = pool_map.remove(&thread_id) {
            // 清空所有池子 (Drop 会自动释放内存)
            for pool in pools.iter_mut() {
                pool.clear();
            }
        }
    }
}

impl Default for ThreadMemoryPool {
    fn default() -> Self {
        [Vec::new(), Vec::new(), Vec::new()]
    }
}

// ============================================================================
// 4. 全局单例
// ============================================================================

static MANAGER: OnceLock<NvjpegMemoryManager> = OnceLock::new();

fn get_manager() -> &'static NvjpegMemoryManager {
    MANAGER.get_or_init(|| NvjpegMemoryManager::new())
}

// ============================================================================
// 5. 公开 API (Rust 侧)
// ============================================================================

pub fn set_enable_mem_stats(enabled: bool) {
    get_manager().set_enable_mem_stats(enabled);
}

pub fn set_restrict_pinned_mem(restrict: bool) {
    get_manager().set_restrict_pinned_mem(restrict);
}

pub fn print_mem_stats() {
    get_manager().print_stats();
}

pub fn get_buffer<K: Into<MemoryKind>>(thread_id: ThreadId, kind: K, size: usize) -> Result<*mut c_void, CudaInt> {
    get_manager().get_buffer(thread_id, kind.into(), size)
}

pub fn get_host_buffer(thread_id: ThreadId, size: usize) -> Result<*mut c_void, CudaInt> {
    let kind = if get_manager().restrict_pinned_mem_usage() {
        MemoryKind::Host
    } else {
        MemoryKind::Pinned
    };
    get_manager().get_buffer(thread_id, kind, size)
}

pub fn add_buffer<K: Into<MemoryKind>>(thread_id: ThreadId, kind: K, size: usize) -> Result<(), CudaInt> {
    get_manager().add_buffer(thread_id, kind.into(), size)
}

pub fn add_host_buffer(thread_id: ThreadId, size: usize) -> Result<(), CudaInt> {
    let kind = if get_manager().restrict_pinned_mem_usage() {
        MemoryKind::Host
    } else {
        MemoryKind::Pinned
    };
    get_manager().add_buffer(thread_id, kind, size)
}

pub fn delete_all_buffers(thread_id: ThreadId) {
    get_manager().delete_all_buffers(thread_id);
}

// ============================================================================
// 6. FFI 回调 (nvJPEG Allocator)
// ============================================================================

#[repr(C)]
pub struct NvjpegDevAllocator {
    pub dev_malloc: unsafe extern "C" fn(*mut *mut c_void, CudaSizeT) -> CudaInt,
    pub dev_free: unsafe extern "C" fn(*mut c_void) -> CudaInt,
}

#[repr(C)]
pub struct NvjpegPinnedAllocator {
    pub pinned_malloc: unsafe extern "C" fn(*mut *mut c_void, CudaSizeT, CudaUint) -> CudaInt,
    pub pinned_free: unsafe extern "C" fn(*mut c_void) -> CudaInt,
}

// --- Device Callbacks ---

#[no_mangle]
pub unsafe extern "C" fn nvjpeg_dev_malloc(ctx: *mut *mut c_void, size: CudaSizeT) -> CudaInt {
    if size == 0 {
        *ctx = ptr::null_mut();
        return CUDA_SUCCESS;
    }

    match get_manager().get_buffer(thread::current().id(), MemoryKind::Device, size) {
        Ok(ptr) => {
            *ctx = ptr;
            CUDA_SUCCESS
        }
        Err(code) => code,
    }
}

#[no_mangle]
pub unsafe extern "C" fn nvjpeg_dev_free(ptr: *mut c_void) -> CudaInt {
    get_manager().return_buffer_to_pool(ptr)
}

// --- Pinned/Host Callbacks ---

#[no_mangle]
pub unsafe extern "C" fn nvjpeg_pinned_malloc(
    ctx: *mut *mut c_void,
    size: CudaSizeT,
    _flags: CudaUint,
) -> CudaInt {
    if size == 0 {
        *ctx = ptr::null_mut();
        return CUDA_SUCCESS;
    }

    match get_host_buffer(thread::current().id(), size) {
        Ok(ptr) => {
            *ctx = ptr;
            CUDA_SUCCESS
        }
        Err(code) => code,
    }
}

#[no_mangle]
pub unsafe extern "C" fn nvjpeg_pinned_free(ptr: *mut c_void) -> CudaInt {
    get_manager().return_buffer_to_pool(ptr)
}

// --- 获取 Allocator 实例 ---

#[no_mangle]
pub extern "C" fn get_nvjpeg_dev_allocator() -> NvjpegDevAllocator {
    NvjpegDevAllocator {
        dev_malloc: nvjpeg_dev_malloc,
        dev_free: nvjpeg_dev_free,
    }
}

#[no_mangle]
pub extern "C" fn get_nvjpeg_pinned_allocator() -> NvjpegPinnedAllocator {
    NvjpegPinnedAllocator {
        pinned_malloc: nvjpeg_pinned_malloc,
        pinned_free: nvjpeg_pinned_free,
    }
}

// ============================================================================
// 7. 辅助工具
// ============================================================================

// 预分配助手宏
#[macro_export]
macro_rules! preallocate_buffers {
    ($thread_id:expr, Device => $size:expr) => {
        $crate::add_buffer($thread_id, $crate::MemoryKind::Device, $size)
    };
    ($thread_id:expr, Pinned => $size:expr) => {
        $crate::add_buffer($thread_id, $crate::MemoryKind::Pinned, $size)
    };
    ($thread_id:expr, Host => $size:expr) => {
        $crate::add_buffer($thread_id, $crate::MemoryKind::Host, $size)
    };
}

// RAII 风格的 Buffer 管理器
pub struct ManagedBuffer {
    ptr: *mut c_void,
    _phantom: std::marker::PhantomData<*mut c_void>,
}

impl ManagedBuffer {
    pub fn new(thread_id: ThreadId, kind: MemoryKind, size: usize) -> Result<Self, CudaInt> {
        let ptr = get_buffer(thread_id, kind, size)?;
        Ok(Self {
            ptr,
            _phantom: std::marker::PhantomData,
        })
    }

    pub fn as_ptr(&self) -> *mut c_void {
        self.ptr
    }

    pub fn as_device_ptr(&self) -> *mut u8 {
        self.ptr as *mut u8
    }
}

impl Drop for ManagedBuffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            get_manager().return_buffer_to_pool(self.ptr);
        }
    }
}

unsafe impl Send for ManagedBuffer {}
unsafe impl Sync for ManagedBuffer {}

// ============================================================================
// 8. 测试模块
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_allocation() {
        let tid = thread::current().id();

        // 分配 Device 内存
        let ptr1 = get_buffer(tid, MemoryKind::Device, 1024).expect("Allocation failed");
        assert!(!ptr1.is_null());

        // 归还
        get_manager().return_buffer_to_pool(ptr1);

        // 再次获取 (应该复用)
        let ptr2 = get_buffer(tid, MemoryKind::Device, 1024).expect("Allocation failed");
        assert_eq!(ptr1, ptr2);
    }

    #[test]
    fn test_best_fit() {
        let tid = thread::current().id();

        // 预分配 3 个不同大小的 buffer
        add_buffer(tid, MemoryKind::Device, 512).unwrap();
        add_buffer(tid, MemoryKind::Device, 1024).unwrap();
        add_buffer(tid, MemoryKind::Device, 2048).unwrap();

        // 请求 1000 字节，应该返回 1024 的 (Best-Fit)
        let ptr = get_buffer(tid, MemoryKind::Device, 1000).expect("Allocation failed");

        // 验证返回的是 1024 的 buffer
        let info = {
            let info_map = get_manager().alloc_info.read().unwrap();
            info_map.get(&(ptr as usize)).unwrap().size
        };
        assert_eq!(info, 1024);
    }

    #[test]
    fn test_managed_buffer() {
        let tid = thread::current().id();

        {
            let _buf = ManagedBuffer::new(tid, MemoryKind::Device, 2048).unwrap();
            // buf 会在作用域结束时自动归还
        }

        // 验证 buffer 已经归还到池子
        let pool_size = {
            let pool_map = get_manager().buffer_pool.read().unwrap();
            pool_map.get(&tid).map(|p| p[MemoryKind::Device.index()].len()).unwrap_or(0)
        };
        assert_eq!(pool_size, 1);
    }
}
