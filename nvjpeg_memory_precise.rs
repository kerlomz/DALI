// nvjpeg_memory_precise.rs
// 完全精确复刻 NVIDIA DALI nvjpeg_memory.cc 的每一个实现细节
//
// 关键改进:
// 1. ReturnBufferToPool 不会从 alloc_info 移除 (只有物理释放时才移除)
// 2. 锁的精细控制 - 最小化锁持有时间
// 3. DeleteAllBuffers 只清空 vector,不删除 map 条目
// 4. RestrictPinnedMemUsage 通过环境变量控制 (懒加载)
// 5. 单例初始化顺序保证
// 6. Allocate 的精确语义

use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::ptr;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread::{self, ThreadId};
use std::fs::OpenOptions;
use std::io::Write as IoWrite;
use std::os::raw::c_char;

// ============================================================================
// 1. CUDA FFI
// ============================================================================

pub type CudaSizeT = usize;
pub type CudaInt = i32;
pub type CudaUint = u32;

pub const CUDA_SUCCESS: CudaInt = 0;
pub const CUDA_ERROR_MEMORY_ALLOCATION: CudaInt = 2;
pub const CUDA_ERROR_UNKNOWN: CudaInt = 999;

#[link(name = "cudart")]
extern "C" {
    pub fn cudaMalloc(devPtr: *mut *mut c_void, size: CudaSizeT) -> CudaInt;
    pub fn cudaMallocHost(ptr: *mut *mut c_void, size: CudaSizeT) -> CudaInt;
    pub fn cudaFree(devPtr: *mut c_void) -> CudaInt;
    pub fn cudaFreeHost(ptr: *mut c_void) -> CudaInt;
}

extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn getenv(name: *const c_char) -> *const c_char;
    fn atoi(s: *const c_char) -> i32;
}

// ============================================================================
// 2. 核心数据结构
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum MemoryKind {
    Device = 0,
    Pinned = 1,
    Host = 2,
}

impl MemoryKind {
    const COUNT: usize = 3;

    fn index(&self) -> usize {
        *self as usize
    }
}

// Deleter 函数类型
type DeleterFn = Box<dyn Fn(*mut c_void) + Send + Sync>;

// AllocInfo: 存储在全局 map 中,记录每个指针的元数据
#[derive(Clone)]
struct AllocInfo {
    kind: MemoryKind,
    size: usize,
    thread_id: ThreadId,
    deleter: Arc<DeleterFn>,  // 真正的释放函数
}

// PoolDeleter: 当 Buffer 被物理释放时调用
// 这个 Deleter 会从 alloc_info 中移除条目,然后调用真正的 deleter
struct PoolDeleter {
    alloc_info: Arc<RwLock<HashMap<usize, AllocInfo>>>,
}

impl PoolDeleter {
    fn call(&self, ptr: *mut c_void) {
        if ptr.is_null() {
            return;
        }

        // 从 alloc_info 中移除并获取真正的 deleter
        let ai = {
            let mut info_map = self.alloc_info.write().unwrap();
            match info_map.remove(&(ptr as usize)) {
                Some(ai) => ai,
                None => {
                    // 按照 DALI 的做法,这里应该 assert
                    panic!("NVJPEG_MEMORY: Attempt to delete unknown pointer {:p}", ptr);
                }
            }
        };

        // 调用真正的 deleter
        (ai.deleter)(ptr);
    }
}

// UniqueBuffer: 类似 C++ 的 unique_ptr<char, Deleter>
struct UniqueBuffer {
    ptr: *mut c_void,
    deleter: Arc<PoolDeleter>,
}

impl UniqueBuffer {
    fn new(ptr: *mut c_void, deleter: Arc<PoolDeleter>) -> Self {
        Self { ptr, deleter }
    }

    fn release(mut self) -> *mut c_void {
        let ptr = self.ptr;
        self.ptr = ptr::null_mut();
        std::mem::forget(self);
        ptr
    }

    fn get(&self) -> *mut c_void {
        self.ptr
    }
}

impl Drop for UniqueBuffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            self.deleter.call(self.ptr);
        }
    }
}

unsafe impl Send for UniqueBuffer {}

// Buffer: 池中的 buffer
struct Buffer {
    ptr: UniqueBuffer,
    kind: MemoryKind,
    size: usize,
}

impl Buffer {
    fn new(ptr: UniqueBuffer, kind: MemoryKind, size: usize) -> Self {
        Self { ptr, kind, size }
    }
}

// 统计信息
#[derive(Default, Clone, Copy, Debug)]
struct MemoryStats {
    nallocs: usize,
    biggest_alloc: usize,
}

// 每线程内存池
type ThreadMemoryPool = [Vec<Buffer>; MemoryKind::COUNT];

// ============================================================================
// 3. 全局状态
// ============================================================================

// 全局 alloc_info (所有正在使用或池中的指针)
static ALLOC_INFO: OnceLock<Arc<RwLock<HashMap<usize, AllocInfo>>>> = OnceLock::new();

fn get_alloc_info() -> &'static Arc<RwLock<HashMap<usize, AllocInfo>>> {
    ALLOC_INFO.get_or_init(|| Arc::new(RwLock::new(HashMap::new())))
}

// RestrictPinnedMemUsage: 懒加载,从环境变量读取
pub fn restrict_pinned_mem_usage() -> bool {
    static RESTRICT: OnceLock<bool> = OnceLock::new();
    *RESTRICT.get_or_init(|| {
        unsafe {
            let env = getenv(b"DALI_RESTRICT_PINNED_MEM\0".as_ptr() as *const c_char);
            if env.is_null() {
                false
            } else {
                atoi(env) != 0
            }
        }
    })
}

// ============================================================================
// 4. 内存管理器
// ============================================================================

struct NvjpegMemoryManager {
    buffer_pool: RwLock<HashMap<ThreadId, ThreadMemoryPool>>,
    stats: Mutex<[MemoryStats; MemoryKind::COUNT]>,
    stats_enabled: std::sync::atomic::AtomicBool,
    pool_deleter: Arc<PoolDeleter>,
}

impl NvjpegMemoryManager {
    fn new() -> Self {
        let alloc_info = get_alloc_info().clone();
        Self {
            buffer_pool: RwLock::new(HashMap::new()),
            stats: Mutex::new([MemoryStats::default(); MemoryKind::COUNT]),
            stats_enabled: std::sync::atomic::AtomicBool::new(true),
            pool_deleter: Arc::new(PoolDeleter { alloc_info }),
        }
    }

    // ========================================================================
    // 统计信息
    // ========================================================================

    pub fn set_enable_mem_stats(&self, enabled: bool) {
        self.stats_enabled.store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    fn add_mem_stats(&self, kind: MemoryKind, size: usize) {
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

    pub fn print_mem_stats(&self) {
        if !self.stats_enabled.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }

        let stats = self.stats.lock().unwrap();

        // 支持 DALI_LOG_FILE 环境变量
        let log_path = std::env::var("DALI_LOG_FILE").ok();
        let mut output: Box<dyn IoWrite> = if let Some(path) = log_path {
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => Box::new(file),
                Err(_) => Box::new(std::io::stdout()),
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
    // 内存分配
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

    // Allocate: 分配内存并记录到 alloc_info
    // 返回 UniqueBuffer (类似 DALI 的 unique_ptr<char, Deleter>)
    fn allocate(
        &self,
        thread_id: ThreadId,
        kind: MemoryKind,
        size: usize,
    ) -> Result<UniqueBuffer, CudaInt> {
        let ptr = self.allocate_raw(kind, size)?;
        let deleter = Self::create_deleter(kind);

        // 记录到 alloc_info (使用写锁)
        {
            let mut info_map = get_alloc_info().write().unwrap();
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

        Ok(UniqueBuffer::new(ptr, self.pool_deleter.clone()))
    }

    // GetBuffer: 按照 DALI 的精确逻辑
    pub fn get_buffer(&self, thread_id: ThreadId, kind: MemoryKind, size: usize) -> Result<*mut c_void, CudaInt> {
        // 1. 用读锁查找线程池,立即解锁
        let pool_exists = {
            let pool_map = self.buffer_pool.read().unwrap();
            let it = pool_map.get(&thread_id);
            let exists = it.is_some();
            // 解锁 (drop lock)
            exists
        };

        // 2. 如果池存在,搜索 Best-Fit
        if pool_exists {
            // 获取读锁,找到线程池的引用
            let (best_fit_idx, smallest_idx) = {
                let pool_map = self.buffer_pool.read().unwrap();
                let buffers = &pool_map[&thread_id][kind.index()];

                let mut best_fit = None;
                let mut smallest = None;
                let mut best_size = usize::MAX;
                let mut min_size = usize::MAX;

                for (i, buf) in buffers.iter().enumerate() {
                    // 找最小的
                    if smallest.is_none() || buf.size < min_size {
                        min_size = buf.size;
                        smallest = Some(i);
                    }

                    // 找 Best-Fit
                    if buf.size >= size && buf.size < best_size {
                        best_size = buf.size;
                        best_fit = Some(i);
                    }
                }

                // 解锁 (drop lock)
                (best_fit, smallest)
            };

            // 3. 找到 Best-Fit,从池中取出 (需要写锁)
            if let Some(idx) = best_fit_idx {
                let mut pool_map = self.buffer_pool.write().unwrap();
                let buffers = &mut pool_map.get_mut(&thread_id).unwrap()[kind.index()];

                // swap to back and pop
                buffers.swap(idx, buffers.len() - 1);
                let buffer = buffers.pop().unwrap();

                // 释放 UniqueBuffer 的所有权,返回裸指针
                return Ok(buffer.ptr.release());
            }

            // 4. 没找到 Best-Fit,驱逐最小的
            if let Some(idx) = smallest_idx {
                let mut pool_map = self.buffer_pool.write().unwrap();
                let buffers = &mut pool_map.get_mut(&thread_id).unwrap()[kind.index()];

                buffers.swap(idx, buffers.len() - 1);
                buffers.pop(); // Drop buffer (物理释放)
            }
        }

        // 5. 分配新内存
        self.add_mem_stats(kind, size);
        let unique_buf = self.allocate(thread_id, kind, size)?;
        Ok(unique_buf.release())
    }

    // ReturnBufferToPool: 按照 DALI 的精确逻辑
    // 关键: 不从 alloc_info 移除! 只有物理释放时才移除
    pub fn return_buffer_to_pool(&self, ptr: *mut c_void) -> CudaInt {
        if ptr.is_null() {
            return CUDA_SUCCESS;
        }

        // 1. 用读锁读取 alloc_info (不移除!)
        let info = {
            let info_map = get_alloc_info().read().unwrap();
            match info_map.get(&(ptr as usize)) {
                Some(info) => info.clone(),
                None => {
                    eprintln!("NVJPEG_MEMORY ERROR: Attempt to free unknown pointer {:p}", ptr);
                    return CUDA_ERROR_UNKNOWN;
                }
            }
            // 解锁 (drop lock)
        };

        // 2. 创建 UniqueBuffer (使用 pool_deleter)
        let unique_buf = UniqueBuffer::new(ptr, self.pool_deleter.clone());

        // 3. 查找或创建线程池
        let pool_ref = {
            let pool_map = self.buffer_pool.read().unwrap();
            let exists = pool_map.contains_key(&info.thread_id);
            // 解锁
            exists
        };

        if !pool_ref {
            // 需要创建新池 (写锁)
            let mut pool_map = self.buffer_pool.write().unwrap();
            pool_map.entry(info.thread_id).or_insert_with(|| Default::default());
            // 解锁
        }

        // 4. 放入池子 (写锁)
        {
            let mut pool_map = self.buffer_pool.write().unwrap();
            let buffers = &mut pool_map.get_mut(&info.thread_id).unwrap()[info.kind.index()];
            buffers.push(Buffer::new(unique_buf, info.kind, info.size));
        }

        CUDA_SUCCESS
    }

    // AddBuffer: 预分配 buffer 到池
    pub fn add_buffer(&self, thread_id: ThreadId, kind: MemoryKind, size: usize) -> Result<(), CudaInt> {
        // 1. 用写锁获取或创建线程池,立即解锁
        {
            let mut pool_map = self.buffer_pool.write().unwrap();
            pool_map.entry(thread_id).or_insert_with(|| Default::default());
            // 解锁
        }

        // 2. 分配内存
        let unique_buf = self.allocate(thread_id, kind, size)?;

        // 3. 放入池子
        {
            let mut pool_map = self.buffer_pool.write().unwrap();
            let buffers = &mut pool_map.get_mut(&thread_id).unwrap()[kind.index()];
            buffers.push(Buffer::new(unique_buf, kind, size));
        }

        self.add_mem_stats(kind, size);
        Ok(())
    }

    // DeleteAllBuffers: 按照 DALI 的逻辑,只清空 vector
    pub fn delete_all_buffers(&self, thread_id: ThreadId) {
        let pool_map = self.buffer_pool.read().unwrap();
        if let Some(pools) = pool_map.get(&thread_id) {
            // 这里有个问题: 我们不能在读锁下修改
            // 但 DALI 的做法是先获取引用,解锁,然后清空
            // 这在 Rust 中不安全,所以我们需要用写锁
            drop(pool_map);

            let mut pool_map = self.buffer_pool.write().unwrap();
            if let Some(pools) = pool_map.get_mut(&thread_id) {
                for pool in pools.iter_mut() {
                    pool.clear(); // Drop 所有 Buffer (触发物理释放)
                }
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
// 5. 全局单例
// ============================================================================

static MANAGER: OnceLock<NvjpegMemoryManager> = OnceLock::new();

fn get_manager() -> &'static NvjpegMemoryManager {
    MANAGER.get_or_init(|| {
        // 确保依赖的资源先初始化 (模拟 DALI 的初始化顺序)
        let _ = get_alloc_info();
        NvjpegMemoryManager::new()
    })
}

// ============================================================================
// 6. 公开 API
// ============================================================================

pub fn set_enable_mem_stats(enabled: bool) {
    get_manager().set_enable_mem_stats(enabled);
}

pub fn print_mem_stats() {
    get_manager().print_mem_stats();
}

pub fn get_buffer<K: Into<MemoryKind>>(thread_id: ThreadId, kind: K, size: usize) -> Result<*mut c_void, CudaInt> {
    get_manager().get_buffer(thread_id, kind.into(), size)
}

pub fn get_host_buffer(thread_id: ThreadId, size: usize) -> Result<*mut c_void, CudaInt> {
    let kind = if restrict_pinned_mem_usage() {
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
    let kind = if restrict_pinned_mem_usage() {
        MemoryKind::Host
    } else {
        MemoryKind::Pinned
    };
    get_manager().add_buffer(thread_id, kind, size)
}

pub fn delete_all_buffers(thread_id: ThreadId) {
    get_manager().delete_all_buffers(thread_id);
}

fn return_buffer_to_pool(ptr: *mut c_void) -> CudaInt {
    get_manager().return_buffer_to_pool(ptr)
}

// ============================================================================
// 7. FFI 回调 (nvJPEG Allocator)
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

// Device malloc - 完全复刻 DALI 的异常处理
#[no_mangle]
pub unsafe extern "C" fn nvjpeg_dev_malloc_impl(ctx: *mut *mut c_void, size: CudaSizeT) -> CudaInt {
    if size == 0 {
        *ctx = ptr::null_mut();
        return CUDA_SUCCESS;
    }

    // 捕获 panic (类似 C++ 的 try-catch)
    let result = std::panic::catch_unwind(|| {
        get_manager().get_buffer(thread::current().id(), MemoryKind::Device, size)
    });

    match result {
        Ok(Ok(ptr)) => {
            *ctx = ptr;
            if ptr.is_null() {
                CUDA_ERROR_MEMORY_ALLOCATION
            } else {
                CUDA_SUCCESS
            }
        }
        Ok(Err(code)) => {
            *ctx = ptr::null_mut();
            code
        }
        Err(_) => {
            *ctx = ptr::null_mut();
            CUDA_ERROR_UNKNOWN
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn nvjpeg_dev_free_impl(ptr: *mut c_void) -> CudaInt {
    return_buffer_to_pool(ptr)
}

// Pinned malloc - HostNew 在 DALI 中
#[no_mangle]
pub unsafe extern "C" fn nvjpeg_pinned_malloc_impl(
    ctx: *mut *mut c_void,
    size: CudaSizeT,
    _flags: CudaUint,
) -> CudaInt {
    if size == 0 {
        *ctx = ptr::null_mut();
        return CUDA_SUCCESS;
    }

    let result = std::panic::catch_unwind(|| {
        get_host_buffer(thread::current().id(), size)
    });

    match result {
        Ok(Ok(ptr)) => {
            *ctx = ptr;
            if ptr.is_null() {
                CUDA_ERROR_MEMORY_ALLOCATION
            } else {
                CUDA_SUCCESS
            }
        }
        Ok(Err(code)) => {
            *ctx = ptr::null_mut();
            code
        }
        Err(_) => {
            *ctx = ptr::null_mut();
            CUDA_ERROR_UNKNOWN
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn nvjpeg_pinned_free_impl(ptr: *mut c_void) -> CudaInt {
    return_buffer_to_pool(ptr)
}

#[no_mangle]
pub extern "C" fn get_nvjpeg_dev_allocator() -> NvjpegDevAllocator {
    NvjpegDevAllocator {
        dev_malloc: nvjpeg_dev_malloc_impl,
        dev_free: nvjpeg_dev_free_impl,
    }
}

#[no_mangle]
pub extern "C" fn get_nvjpeg_pinned_allocator() -> NvjpegPinnedAllocator {
    NvjpegPinnedAllocator {
        pinned_malloc: nvjpeg_pinned_malloc_impl,
        pinned_free: nvjpeg_pinned_free_impl,
    }
}

// nvJPEG2K 支持
#[repr(C)]
pub struct Nvjpeg2kDevAllocator {
    pub device_malloc: unsafe extern "C" fn(*mut *mut c_void, CudaSizeT) -> CudaInt,
    pub device_free: unsafe extern "C" fn(*mut c_void) -> CudaInt,
}

#[repr(C)]
pub struct Nvjpeg2kPinnedAllocator {
    pub pinned_malloc: unsafe extern "C" fn(*mut *mut c_void, CudaSizeT, CudaUint) -> CudaInt,
    pub pinned_free: unsafe extern "C" fn(*mut c_void) -> CudaInt,
}

#[no_mangle]
pub extern "C" fn get_nvjpeg2k_dev_allocator() -> Nvjpeg2kDevAllocator {
    Nvjpeg2kDevAllocator {
        device_malloc: nvjpeg_dev_malloc_impl,
        device_free: nvjpeg_dev_free_impl,
    }
}

#[no_mangle]
pub extern "C" fn get_nvjpeg2k_pinned_allocator() -> Nvjpeg2kPinnedAllocator {
    Nvjpeg2kPinnedAllocator {
        pinned_malloc: nvjpeg_pinned_malloc_impl,
        pinned_free: nvjpeg_pinned_free_impl,
    }
}

// ============================================================================
// 8. RAII 辅助类型 (可选)
// ============================================================================

pub struct ManagedBuffer {
    ptr: *mut c_void,
}

impl ManagedBuffer {
    pub fn new(thread_id: ThreadId, kind: MemoryKind, size: usize) -> Result<Self, CudaInt> {
        let ptr = get_buffer(thread_id, kind, size)?;
        Ok(Self { ptr })
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
            return_buffer_to_pool(self.ptr);
        }
    }
}

unsafe impl Send for ManagedBuffer {}
unsafe impl Sync for ManagedBuffer {}

// ============================================================================
// 9. 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alloc_info_persistence() {
        let tid = thread::current().id();

        // 分配一个 buffer
        let ptr = get_buffer(tid, MemoryKind::Device, 1024).unwrap();

        // 检查 alloc_info 中有这个指针
        {
            let info_map = get_alloc_info().read().unwrap();
            assert!(info_map.contains_key(&(ptr as usize)));
        }

        // 归还到池子
        return_buffer_to_pool(ptr);

        // 关键: alloc_info 中**仍然有这个指针** (这是与之前实现的主要差异)
        {
            let info_map = get_alloc_info().read().unwrap();
            assert!(info_map.contains_key(&(ptr as usize)), "alloc_info should still contain the pointer after returning to pool");
        }

        // 删除所有 buffer (物理释放)
        delete_all_buffers(tid);

        // 现在 alloc_info 中应该没有了
        {
            let info_map = get_alloc_info().read().unwrap();
            assert!(!info_map.contains_key(&(ptr as usize)), "alloc_info should not contain the pointer after physical deletion");
        }
    }

    #[test]
    fn test_restrict_pinned_mem() {
        // 测试环境变量
        std::env::set_var("DALI_RESTRICT_PINNED_MEM", "1");
        // 注意: 由于使用了 OnceLock,需要重启进程才能看到变化
        // 这里只是演示 API
    }

    #[test]
    fn test_best_fit_reuse() {
        let tid = thread::current().id();

        // 预分配
        add_buffer(tid, MemoryKind::Device, 1024).unwrap();

        // 获取
        let ptr1 = get_buffer(tid, MemoryKind::Device, 1024).unwrap();

        // 归还
        return_buffer_to_pool(ptr1);

        // 再次获取,应该复用
        let ptr2 = get_buffer(tid, MemoryKind::Device, 1024).unwrap();
        assert_eq!(ptr1, ptr2);

        delete_all_buffers(tid);
    }
}
