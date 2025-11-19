# nvJPEG Memory Manager - Complete Documentation

## 📋 概述

这是一个完整复刻 NVIDIA DALI `nvjpeg_memory.cc` 的 Rust 实现,提供高性能的内存池管理机制,专为 nvJPEG 库设计,支持 JPEG 解码和图像处理的 **in-place** 显存管理。

### 🎯 核心特性

- ✅ **Best-Fit 分配算法**: 最小化内存碎片
- ✅ **自动碎片驱逐**: 防止小碎片堆积
- ✅ **读写锁优化**: 支持高并发场景
- ✅ **三种内存类型**: Device/Pinned/Host
- ✅ **RAII 内存管理**: 自动释放,防止泄漏
- ✅ **线程隔离池**: 无锁设计,避免竞争
- ✅ **统计信息**: 跟踪分配模式
- ✅ **跨平台支持**: Windows/Linux 统一接口
- ✅ **完整的异常处理**: 符合 nvJPEG C API 规范

---

## 🏗️ 架构设计

### 1. 内存池层次结构

```
NvjpegMemoryManager (全局单例)
├── buffer_pool: RwLock<HashMap<ThreadId, ThreadMemoryPool>>
│   └── ThreadMemoryPool = [Vec<Buffer>; 3]
│       ├── [0] Device buffers
│       ├── [1] Pinned buffers
│       └── [2] Host buffers
│
├── alloc_info: RwLock<HashMap<usize, AllocInfo>>
│   └── 跟踪已分配的指针元数据
│
└── stats: Mutex<[MemoryStats; 3]>
    └── 统计每种内存类型的分配次数和最大值
```

### 2. 核心数据结构

#### `Buffer`
```rust
struct Buffer {
    ptr: *mut c_void,      // 内存指针
    kind: MemoryKind,      // 内存类型
    size: usize,           // 大小
    deleter: Arc<DeleterFn>, // 释放函数 (RAII)
}
```

#### `AllocInfo`
```rust
struct AllocInfo {
    kind: MemoryKind,      // 内存类型
    size: usize,           // 分配大小
    thread_id: ThreadId,   // 所属线程
    deleter: Arc<DeleterFn>, // 释放函数
}
```

### 3. 内存分配流程

```
get_buffer(thread_id, kind, size)
    ↓
1. 查找线程池 (读锁)
    ↓
2. Best-Fit 搜索:
    - 找能装下的最小 buffer (best_fit)
    - 找池中最小 buffer (smallest)
    ↓
3. 找到 Best-Fit?
    YES → 从池中取出 → 记录到 alloc_info → 返回
    NO  → ↓
    ↓
4. 执行驱逐策略:
    - 释放 smallest buffer (物理释放)
    ↓
5. 分配新内存:
    - cudaMalloc/cudaMallocHost/malloc
    - 记录到 alloc_info
    - 更新统计信息
    ↓
6. 返回指针
```

### 4. 内存归还流程

```
return_buffer_to_pool(ptr)
    ↓
1. 查找 alloc_info (读锁)
    ↓
2. 获取元数据 (kind, size, thread_id, deleter)
    ↓
3. 从 alloc_info 移除 (写锁)
    ↓
4. 创建 Buffer 对象
    ↓
5. 放入对应线程池 (写锁)
    - buffer_pool[thread_id][kind].push(buffer)
    ↓
6. 完成 (物理内存未释放,保留在池中)
```

---

## 🔧 API 参考

### Rust API

#### 1. 内存分配

```rust
// 获取指定类型的 buffer
pub fn get_buffer<K: Into<MemoryKind>>(
    thread_id: ThreadId,
    kind: K,
    size: usize
) -> Result<*mut c_void, CudaInt>

// 获取 Host/Pinned buffer (根据配置自动选择)
pub fn get_host_buffer(
    thread_id: ThreadId,
    size: usize
) -> Result<*mut c_void, CudaInt>
```

**示例:**
```rust
let tid = thread::current().id();
let ptr = get_buffer(tid, MemoryKind::Device, 1024 * 1024)?;
// 使用 ptr...
get_manager().return_buffer_to_pool(ptr);
```

#### 2. 预分配

```rust
// 预分配 buffer 到池中
pub fn add_buffer<K: Into<MemoryKind>>(
    thread_id: ThreadId,
    kind: K,
    size: usize
) -> Result<(), CudaInt>

// 预分配 Host/Pinned buffer
pub fn add_host_buffer(
    thread_id: ThreadId,
    size: usize
) -> Result<(), CudaInt>
```

**示例:**
```rust
// 在线程初始化时预分配
let tid = thread::current().id();
add_buffer(tid, MemoryKind::Device, 10 * 1024 * 1024)?; // 10MB
add_buffer(tid, MemoryKind::Device, 10 * 1024 * 1024)?; // 10MB
```

#### 3. 清理和统计

```rust
// 删除线程所有 buffer
pub fn delete_all_buffers(thread_id: ThreadId)

// 打印统计信息
pub fn print_mem_stats()

// 启用/禁用统计
pub fn set_enable_mem_stats(enabled: bool)

// 设置限制 Pinned 内存
pub fn set_restrict_pinned_mem(restrict: bool)
```

#### 4. RAII 风格 API

```rust
pub struct ManagedBuffer {
    ptr: *mut c_void,
}

impl ManagedBuffer {
    pub fn new(thread_id: ThreadId, kind: MemoryKind, size: usize)
        -> Result<Self, CudaInt>

    pub fn as_ptr(&self) -> *mut c_void
    pub fn as_device_ptr(&self) -> *mut u8
}

// 自动实现 Drop trait,作用域结束时归还内存
```

**示例:**
```rust
{
    let buffer = ManagedBuffer::new(tid, MemoryKind::Device, 1024)?;
    // 使用 buffer.as_ptr()...
} // 自动归还到池
```

### C FFI API

#### 1. Allocator 结构体

```rust
#[repr(C)]
pub struct NvjpegDevAllocator {
    pub dev_malloc: unsafe extern "C" fn(*mut *mut c_void, usize) -> i32,
    pub dev_free: unsafe extern "C" fn(*mut c_void) -> i32,
}

#[repr(C)]
pub struct NvjpegPinnedAllocator {
    pub pinned_malloc: unsafe extern "C" fn(*mut *mut c_void, usize, u32) -> i32,
    pub pinned_free: unsafe extern "C" fn(*mut c_void) -> i32,
}
```

#### 2. 获取 Allocator

```rust
#[no_mangle]
pub extern "C" fn get_nvjpeg_dev_allocator() -> NvjpegDevAllocator

#[no_mangle]
pub extern "C" fn get_nvjpeg_pinned_allocator() -> NvjpegPinnedAllocator
```

#### 3. 回调函数

```rust
#[no_mangle]
pub unsafe extern "C" fn nvjpeg_dev_malloc(
    ctx: *mut *mut c_void,
    size: usize
) -> i32

#[no_mangle]
pub unsafe extern "C" fn nvjpeg_dev_free(ptr: *mut c_void) -> i32

#[no_mangle]
pub unsafe extern "C" fn nvjpeg_pinned_malloc(
    ctx: *mut *mut c_void,
    size: usize,
    flags: u32
) -> i32

#[no_mangle]
pub unsafe extern "C" fn nvjpeg_pinned_free(ptr: *mut c_void) -> i32
```

---

## 💡 使用场景

### 场景 1: nvJPEG 解码器集成

```rust
use nvjpeg_sys::*; // nvJPEG Rust bindings

unsafe {
    // 1. 获取自定义 allocator
    let dev_alloc = get_nvjpeg_dev_allocator();
    let pin_alloc = get_nvjpeg_pinned_allocator();

    // 2. 创建 nvJPEG handle
    let mut handle: nvjpegHandle_t = std::ptr::null_mut();
    nvjpegCreateEx(
        nvjpegBackend_t::NVJPEG_BACKEND_DEFAULT,
        &dev_alloc as *const _ as *mut _,
        &pin_alloc as *const _ as *mut _,
        0,
        &mut handle,
    );

    // 3. 预分配内存池
    let tid = thread::current().id();
    add_buffer(tid, MemoryKind::Device, 10 * 1024 * 1024).unwrap();
    add_host_buffer(tid, 2 * 1024 * 1024).unwrap();

    // 4. 解码图像
    // nvjpegDecode(handle, ...);

    // 5. 清理
    delete_all_buffers(tid);
    nvjpegDestroy(handle);
}
```

### 场景 2: 批处理 JPEG 解码

```rust
fn batch_decode_images(image_paths: &[String]) {
    let tid = thread::current().id();
    const OUTPUT_SIZE: usize = 1920 * 1080 * 3; // 1080p RGB

    // 预分配 double buffering
    add_buffer(tid, MemoryKind::Device, OUTPUT_SIZE).unwrap();
    add_buffer(tid, MemoryKind::Device, OUTPUT_SIZE).unwrap();

    for path in image_paths {
        // 获取输出 buffer (复用池中内存)
        let output = ManagedBuffer::new(tid, MemoryKind::Device, OUTPUT_SIZE).unwrap();

        // 解码到 output
        decode_jpeg(path, output.as_device_ptr());

        // output 自动归还
    }

    delete_all_buffers(tid);
}
```

### 场景 3: 多线程并行解码

```rust
use rayon::prelude::*;

fn parallel_decode(images: &[ImageData]) {
    images.par_iter().for_each(|img| {
        let tid = thread::current().id();

        // 每个线程独立的内存池
        add_buffer(tid, MemoryKind::Device, img.expected_size()).unwrap();

        let output = ManagedBuffer::new(tid, MemoryKind::Device, img.size()).unwrap();
        decode_image(img, output.as_device_ptr());

        delete_all_buffers(tid);
    });
}
```

### 场景 4: In-Place 图像处理 Pipeline

```rust
fn image_processing_pipeline(input: &[u8]) -> Vec<u8> {
    let tid = thread::current().id();

    // 1. 解码 (Device 内存)
    let decode_output = ManagedBuffer::new(tid, MemoryKind::Device, 10 * 1024 * 1024).unwrap();
    decode_jpeg(input, decode_output.as_device_ptr());

    // 2. Resize (in-place,复用同一块显存)
    let resize_output = decode_output; // 复用
    resize_image_inplace(resize_output.as_device_ptr(), 224, 224);

    // 3. Normalize (in-place)
    normalize_inplace(resize_output.as_device_ptr());

    // 4. 拷贝回 Host
    let mut result = vec![0u8; 224 * 224 * 3];
    cudaMemcpy(
        result.as_mut_ptr() as *mut _,
        resize_output.as_device_ptr() as *const _,
        result.len(),
        cudaMemcpyKind::cudaMemcpyDeviceToHost,
    );

    result
}
```

---

## 🎯 核心算法详解

### Best-Fit 算法

```rust
// 伪代码
fn find_best_fit(buffers: &[Buffer], requested_size: usize) -> Option<usize> {
    let mut best_idx = None;
    let mut best_size = usize::MAX;

    for (i, buf) in buffers.iter().enumerate() {
        if buf.size >= requested_size && buf.size < best_size {
            best_size = buf.size;
            best_idx = Some(i);
        }
    }

    best_idx
}
```

**优势:**
- ✅ 最小化浪费 (选择能装下的最小 buffer)
- ✅ 减少碎片
- ✅ 提高内存利用率

### 碎片驱逐策略

```rust
// 伪代码
fn evict_smallest(buffers: &mut Vec<Buffer>) {
    if let Some(smallest_idx) = find_smallest(buffers) {
        let buffer = buffers.swap_remove(smallest_idx);
        // Drop buffer -> 物理释放内存
    }
}
```

**触发条件:**
- 请求的大小在池中找不到合适的 buffer
- 池中存在 buffer (但都太小)

**效果:**
- ✅ 避免池中堆积大量无用小碎片
- ✅ 为新分配腾出空间
- ✅ 保持池的健康状态

---

## 🔒 线程安全设计

### 1. 读写锁 (RwLock)

```rust
buffer_pool: RwLock<HashMap<ThreadId, ThreadMemoryPool>>
```

**优势:**
- 允许多个线程同时读取 (查找池)
- 只有在取出/放入 buffer 时才需要写锁
- 极大提升并发性能

### 2. 线程隔离

每个线程拥有独立的内存池:
```
Thread A: [Device池, Pinned池, Host池]
Thread B: [Device池, Pinned池, Host池]
Thread C: [Device池, Pinned池, Host池]
```

**优势:**
- ✅ 避免跨线程竞争
- ✅ 无需同步开销
- ✅ 更好的缓存局部性

---

## ⚙️ 配置选项

### 1. 环境变量

#### `DALI_LOG_FILE`
将统计信息输出到文件

```bash
export DALI_LOG_FILE=/tmp/nvjpeg_stats.log
```

```rust
print_mem_stats(); // 输出到 /tmp/nvjpeg_stats.log
```

### 2. 运行时配置

#### 限制 Pinned 内存使用

```rust
set_restrict_pinned_mem(true);

// 此时 get_host_buffer 会使用普通 malloc 而非 cudaMallocHost
let ptr = get_host_buffer(tid, 1024).unwrap();
```

**使用场景:**
- Pinned 内存资源紧张
- 不需要高速 DMA 传输
- 多进程共享 GPU

#### 禁用统计信息

```rust
set_enable_mem_stats(false);

// 分配操作不再记录统计
add_buffer(tid, MemoryKind::Device, 1024).unwrap();
```

**使用场景:**
- 生产环境减少开销
- 不需要调试信息

---

## 📊 性能优化建议

### 1. 预分配策略

❌ **不推荐: 按需分配**
```rust
for i in 0..1000 {
    let ptr = get_buffer(tid, MemoryKind::Device, 10MB).unwrap(); // 每次都可能触发 cudaMalloc
    // ...
    return_buffer_to_pool(ptr);
}
```

✅ **推荐: 预分配**
```rust
// 初始化时预分配
add_buffer(tid, MemoryKind::Device, 10MB).unwrap();
add_buffer(tid, MemoryKind::Device, 10MB).unwrap();

for i in 0..1000 {
    let ptr = get_buffer(tid, MemoryKind::Device, 10MB).unwrap(); // 直接从池中取
    // ...
    return_buffer_to_pool(ptr);
}
```

### 2. Double Buffering

```rust
// 预分配 2 个 buffer
add_buffer(tid, MemoryKind::Device, SIZE).unwrap();
add_buffer(tid, MemoryKind::Device, SIZE).unwrap();

loop {
    // Buffer 1: 解码
    let buf1 = get_buffer(tid, MemoryKind::Device, SIZE).unwrap();
    decode(buf1);

    // Buffer 2: 处理 (与下一次解码并行)
    let buf2 = get_buffer(tid, MemoryKind::Device, SIZE).unwrap();
    process(buf2);

    return_buffer_to_pool(buf1);
    return_buffer_to_pool(buf2);
}
```

### 3. 批量预分配

```rust
// 根据预期负载预分配
const BATCH_SIZE: usize = 32;
const IMAGE_SIZE: usize = 1920 * 1080 * 3;

for _ in 0..BATCH_SIZE {
    add_buffer(tid, MemoryKind::Device, IMAGE_SIZE).unwrap();
}
```

---

## 🐛 调试和诊断

### 1. 统计信息输出

```
#################### NVJPEG STATS ####################
Device memory: 150 allocations, largest = 24883200 bytes
Host (pinned) memory: 50 allocations, largest = 1048576 bytes
Host (regular) memory: 10 allocations, largest = 524288 bytes
################## END NVJPEG STATS ##################
```

**解读:**
- `nallocs`: 总分配次数 (越少越好,说明复用率高)
- `biggest_alloc`: 最大分配大小 (用于优化预分配策略)

### 2. 内存泄漏检测

```rust
// 检查池中剩余 buffer
fn check_pool_size(tid: ThreadId) {
    let pool_map = get_manager().buffer_pool.read().unwrap();
    if let Some(pool) = pool_map.get(&tid) {
        for (i, buffers) in pool.iter().enumerate() {
            let kind = MemoryKind::from_index(i).unwrap();
            println!("{:?}: {} buffers in pool", kind, buffers.len());
        }
    }
}
```

### 3. 错误处理

```rust
match get_buffer(tid, MemoryKind::Device, size) {
    Ok(ptr) => { /* 使用 ptr */ },
    Err(CUDA_ERROR_MEMORY_ALLOCATION) => {
        eprintln!("Out of memory!");
        // 尝试清理池或减小批量大小
    },
    Err(e) => {
        eprintln!("Unexpected error: {}", e);
    }
}
```

---

## 🔄 与 DALI 原始实现的对照

| 特性 | DALI C++ | 本实现 Rust | 差异 |
|------|----------|-------------|------|
| Best-Fit 算法 | ✅ | ✅ | 完全一致 |
| 碎片驱逐 | ✅ | ✅ | 完全一致 |
| 读写锁 | `shared_timed_mutex` | `RwLock` | 语义相同 |
| 内存类型 | Device/Pinned/Host | Device/Pinned/Host | 完全一致 |
| RAII 管理 | `unique_ptr<char, Deleter>` | `Buffer` (Drop trait) | 机制相同 |
| 统计信息 | ✅ | ✅ | 完全一致 |
| 日志输出 | `DALI_LOG_FILE` | `DALI_LOG_FILE` | 完全一致 |
| 异常处理 | `try-catch` | `Result<T, E>` | Rust 惯用法 |
| 线程安全 | `std::mutex` | `Mutex/RwLock` | Rust 编译期保证 |

---

## 📝 最佳实践

### ✅ DO

1. **在线程初始化时预分配内存池**
   ```rust
   thread::spawn(move || {
       let tid = thread::current().id();
       add_buffer(tid, MemoryKind::Device, EXPECTED_SIZE).unwrap();
       // 工作逻辑...
       delete_all_buffers(tid);
   });
   ```

2. **使用 RAII 风格 API**
   ```rust
   let buffer = ManagedBuffer::new(tid, MemoryKind::Device, size)?;
   // 无需手动归还
   ```

3. **启用统计信息 (开发阶段)**
   ```rust
   set_enable_mem_stats(true);
   // 工作逻辑...
   print_mem_stats();
   ```

4. **线程结束前清理池**
   ```rust
   delete_all_buffers(thread::current().id());
   ```

### ❌ DON'T

1. **不要跨线程传递 buffer**
   ```rust
   // ❌ 错误
   let ptr = get_buffer(thread_a_id, ...);
   send_to_thread_b(ptr); // 线程 B 归还时会找不到对应池
   ```

2. **不要忘记归还 buffer**
   ```rust
   // ❌ 错误
   let ptr = get_buffer(tid, ...)?;
   // 使用 ptr...
   // 忘记调用 return_buffer_to_pool(ptr) -> 内存泄漏
   ```

3. **不要在热路径上频繁分配新尺寸的 buffer**
   ```rust
   // ❌ 低效
   for size in [1024, 2048, 4096, 8192] {
       let ptr = get_buffer(tid, MemoryKind::Device, size)?; // 每次都分配新内存
   }
   ```

---

## 🧪 测试

### 单元测试

```bash
cargo test --lib
```

### 基准测试

```bash
cargo test --release -- --nocapture bench
```

### 集成测试

```rust
#[test]
fn test_jpeg_decode_workflow() {
    let tid = thread::current().id();

    // 1. 预分配
    add_buffer(tid, MemoryKind::Device, 10MB).unwrap();

    // 2. 解码
    let output = ManagedBuffer::new(tid, MemoryKind::Device, 10MB).unwrap();
    // decode_jpeg(..., output.as_device_ptr());

    // 3. 验证复用
    drop(output);
    let output2 = ManagedBuffer::new(tid, MemoryKind::Device, 10MB).unwrap();
    assert_eq!(output.as_ptr(), output2.as_ptr()); // 复用同一块内存
}
```

---

## 🚀 编译和集成

### Cargo.toml

```toml
[dependencies]
# 无外部依赖 (仅 std)

[dev-dependencies]
# 用于测试
rayon = "1.7"

[features]
default = []
nvjpeg = [] # 启用 nvJPEG FFI 示例

[lib]
name = "nvjpeg_memory"
crate-type = ["rlib", "cdylib"] # 支持 Rust 和 C FFI
```

### 编译

```bash
# Rust 库
cargo build --release

# C FFI 动态库
cargo build --release --crate-type cdylib

# 生成 .so/.dll
# Linux: target/release/libnvjpeg_memory.so
# Windows: target/release/nvjpeg_memory.dll
```

### C/C++ 集成

```c
// nvjpeg_memory.h
#include <stdint.h>

typedef struct {
    int32_t (*dev_malloc)(void**, size_t);
    int32_t (*dev_free)(void*);
} NvjpegDevAllocator;

typedef struct {
    int32_t (*pinned_malloc)(void**, size_t, uint32_t);
    int32_t (*pinned_free)(void*);
} NvjpegPinnedAllocator;

extern "C" {
    NvjpegDevAllocator get_nvjpeg_dev_allocator();
    NvjpegPinnedAllocator get_nvjpeg_pinned_allocator();
    void print_mem_stats();
}
```

```cpp
// main.cpp
#include "nvjpeg_memory.h"
#include <nvjpeg.h>

int main() {
    auto dev_alloc = get_nvjpeg_dev_allocator();
    auto pin_alloc = get_nvjpeg_pinned_allocator();

    nvjpegHandle_t handle;
    nvjpegCreateEx(NVJPEG_BACKEND_DEFAULT, &dev_alloc, &pin_alloc, 0, &handle);

    // 使用 nvJPEG...

    nvjpegDestroy(handle);
    print_mem_stats();
    return 0;
}
```

---

## 📚 参考资源

- [NVIDIA DALI GitHub](https://github.com/NVIDIA/DALI)
- [nvJPEG Documentation](https://docs.nvidia.com/cuda/nvjpeg/index.html)
- [CUDA Memory Management Best Practices](https://docs.nvidia.com/cuda/cuda-c-best-practices-guide/index.html#memory-optimizations)

---

## 📄 许可证

本实现遵循 Apache 2.0 许可证,与 NVIDIA DALI 保持一致。

---

## 🤝 贡献

欢迎提交 Issue 和 Pull Request!

### 开发路线图

- [x] 核心内存池实现
- [x] Best-Fit 算法
- [x] 碎片驱逐策略
- [x] 读写锁优化
- [x] RAII 风格 API
- [x] 统计信息
- [x] 跨平台支持
- [ ] CUDA Stream 集成
- [ ] 更细粒度的内存对齐
- [ ] GPU 内存池分析工具

---

**最后更新**: 2025-11-19
