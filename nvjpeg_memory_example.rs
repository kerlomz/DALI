// nvjpeg_memory_example.rs
// 使用示例: 展示如何在实际应用中使用 nvJPEG 内存管理器

mod nvjpeg_memory_complete;

use nvjpeg_memory_complete::*;
use std::thread;

// ============================================================================
// 示例 1: 基础使用 - JPEG 解码场景
// ============================================================================

fn example_basic_jpeg_decode() {
    println!("=== Example 1: Basic JPEG Decode ===");

    let thread_id = thread::current().id();

    // 1. 预分配内存池 (推荐在线程初始化时进行)
    // 假设解码输出大小约为 4K 图像 (3840x2160x3 = 24MB)
    const DECODE_OUTPUT_SIZE: usize = 24 * 1024 * 1024;

    // 预分配 2 个 Device buffer 用于 double buffering
    add_buffer(thread_id, MemoryKind::Device, DECODE_OUTPUT_SIZE).unwrap();
    add_buffer(thread_id, MemoryKind::Device, DECODE_OUTPUT_SIZE).unwrap();

    // 预分配 Host buffer 用于解码过程中的临时数据
    add_host_buffer(thread_id, 1024 * 1024).unwrap();

    // 2. 模拟解码流程
    for i in 0..5 {
        println!("Decoding image {}", i);

        // 获取输出 buffer
        let output_ptr = get_buffer(thread_id, MemoryKind::Device, DECODE_OUTPUT_SIZE)
            .expect("Failed to allocate output buffer");

        // 模拟解码操作
        println!("  Output buffer allocated at: {:p}", output_ptr);

        // 解码完成后归还 buffer
        get_manager().return_buffer_to_pool(output_ptr);
    }

    // 3. 清理
    delete_all_buffers(thread_id);
    print_mem_stats();
}

// ============================================================================
// 示例 2: RAII 风格使用
// ============================================================================

fn example_raii_style() {
    println!("\n=== Example 2: RAII Style ===");

    let thread_id = thread::current().id();

    // 使用 ManagedBuffer 自动管理生命周期
    {
        let buffer1 = ManagedBuffer::new(thread_id, MemoryKind::Device, 1024 * 1024)
            .expect("Allocation failed");
        println!("Buffer 1 allocated at: {:p}", buffer1.as_ptr());

        {
            let buffer2 = ManagedBuffer::new(thread_id, MemoryKind::Pinned, 512 * 1024)
                .expect("Allocation failed");
            println!("Buffer 2 allocated at: {:p}", buffer2.as_ptr());

            // buffer2 在这里自动归还
        }

        println!("Buffer 2 returned to pool");

        // buffer1 在这里自动归还
    }

    println!("All buffers returned");
    print_mem_stats();
}

// ============================================================================
// 示例 3: 多线程场景 - 批处理 JPEG 解码
// ============================================================================

fn example_multithreaded_batch_decode() {
    println!("\n=== Example 3: Multithreaded Batch Decode ===");

    const NUM_THREADS: usize = 4;
    const IMAGES_PER_THREAD: usize = 10;
    const IMAGE_SIZE: usize = 1920 * 1080 * 3; // 1080p RGB

    let handles: Vec<_> = (0..NUM_THREADS)
        .map(|thread_idx| {
            thread::spawn(move || {
                let tid = thread::current().id();

                // 每个线程预分配自己的内存池
                add_buffer(tid, MemoryKind::Device, IMAGE_SIZE).unwrap();
                add_buffer(tid, MemoryKind::Device, IMAGE_SIZE).unwrap();

                println!("Thread {} started", thread_idx);

                for img_idx in 0..IMAGES_PER_THREAD {
                    // 获取 buffer
                    let output = ManagedBuffer::new(tid, MemoryKind::Device, IMAGE_SIZE)
                        .expect("Allocation failed");

                    // 模拟解码
                    if img_idx == 0 {
                        println!(
                            "Thread {} processing image {} at {:p}",
                            thread_idx,
                            img_idx,
                            output.as_ptr()
                        );
                    }

                    // output 自动归还
                }

                // 清理线程池
                delete_all_buffers(tid);
                println!("Thread {} finished", thread_idx);
            })
        })
        .collect();

    // 等待所有线程完成
    for handle in handles {
        handle.join().unwrap();
    }

    print_mem_stats();
}

// ============================================================================
// 示例 4: nvJPEG C API 集成
// ============================================================================

#[cfg(feature = "nvjpeg")]
fn example_nvjpeg_integration() {
    use std::ptr;

    println!("\n=== Example 4: nvJPEG C API Integration ===");

    unsafe {
        // 获取自定义 allocator
        let dev_allocator = get_nvjpeg_dev_allocator();
        let pinned_allocator = get_nvjpeg_pinned_allocator();

        // 创建 nvJPEG handle (伪代码)
        // let mut handle: nvjpegHandle_t = ptr::null_mut();
        // nvjpegCreateEx(
        //     nvjpegBackend_t::NVJPEG_BACKEND_DEFAULT,
        //     &dev_allocator,
        //     &pinned_allocator,
        //     0,
        //     &mut handle,
        // );

        println!("nvJPEG allocators registered:");
        println!("  Device malloc:  {:p}", dev_allocator.dev_malloc as *const ());
        println!("  Device free:    {:p}", dev_allocator.dev_free as *const ());
        println!("  Pinned malloc:  {:p}", pinned_allocator.pinned_malloc as *const ());
        println!("  Pinned free:    {:p}", pinned_allocator.pinned_free as *const ());

        // 使用 handle 进行解码...
        // nvjpegDecode(handle, ...);

        // 销毁 handle
        // nvjpegDestroy(handle);
    }

    print_mem_stats();
}

// ============================================================================
// 示例 5: Best-Fit 策略演示
// ============================================================================

fn example_best_fit_strategy() {
    println!("\n=== Example 5: Best-Fit Strategy ===");

    let thread_id = thread::current().id();

    // 预分配多个不同大小的 buffer
    println!("Pre-allocating buffers:");
    add_buffer(thread_id, MemoryKind::Device, 512).unwrap();
    println!("  Added 512 bytes");
    add_buffer(thread_id, MemoryKind::Device, 1024).unwrap();
    println!("  Added 1024 bytes");
    add_buffer(thread_id, MemoryKind::Device, 2048).unwrap();
    println!("  Added 2048 bytes");
    add_buffer(thread_id, MemoryKind::Device, 4096).unwrap();
    println!("  Added 4096 bytes");

    // 请求 1000 字节
    println!("\nRequesting 1000 bytes...");
    let ptr1 = get_buffer(thread_id, MemoryKind::Device, 1000).unwrap();

    // 检查分配的实际大小
    let actual_size = {
        let info_map = get_manager().alloc_info.read().unwrap();
        info_map.get(&(ptr1 as usize)).unwrap().size
    };
    println!("  Allocated buffer size: {} bytes (Best-Fit)", actual_size);
    assert_eq!(actual_size, 1024, "Should allocate 1024 bytes buffer (best fit)");

    // 归还
    get_manager().return_buffer_to_pool(ptr1);

    // 请求 3000 字节
    println!("\nRequesting 3000 bytes...");
    let ptr2 = get_buffer(thread_id, MemoryKind::Device, 3000).unwrap();

    let actual_size2 = {
        let info_map = get_manager().alloc_info.read().unwrap();
        info_map.get(&(ptr2 as usize)).unwrap().size
    };
    println!("  Allocated buffer size: {} bytes (Best-Fit)", actual_size2);
    assert_eq!(actual_size2, 4096, "Should allocate 4096 bytes buffer (best fit)");

    delete_all_buffers(thread_id);
    print_mem_stats();
}

// ============================================================================
// 示例 6: 配置选项
// ============================================================================

fn example_configuration() {
    println!("\n=== Example 6: Configuration Options ===");

    // 禁用统计信息
    set_enable_mem_stats(false);
    println!("Memory stats disabled");

    let thread_id = thread::current().id();
    add_buffer(thread_id, MemoryKind::Device, 1024).unwrap();
    get_buffer(thread_id, MemoryKind::Device, 1024).unwrap();

    print_mem_stats(); // 不会输出

    // 重新启用
    set_enable_mem_stats(true);
    println!("\nMemory stats enabled");

    // 设置限制 Pinned 内存使用
    set_restrict_pinned_mem(true);
    println!("Restrict pinned memory: enabled");

    // 此时 get_host_buffer 会使用普通 Host 内存而非 Pinned
    let host_ptr = get_host_buffer(thread_id, 2048).unwrap();
    println!("Host buffer allocated at: {:p}", host_ptr);

    // 验证类型
    let kind = {
        let info_map = get_manager().alloc_info.read().unwrap();
        info_map.get(&(host_ptr as usize)).unwrap().kind
    };
    println!("Buffer type: {:?}", kind);
    assert_eq!(kind, MemoryKind::Host);

    delete_all_buffers(thread_id);
}

// ============================================================================
// 示例 7: 环境变量配置
// ============================================================================

fn example_env_config() {
    println!("\n=== Example 7: Environment Variable Config ===");

    // 设置日志文件路径
    std::env::set_var("DALI_LOG_FILE", "/tmp/nvjpeg_stats.log");

    let thread_id = thread::current().id();
    add_buffer(thread_id, MemoryKind::Device, 1024 * 1024).unwrap();
    get_buffer(thread_id, MemoryKind::Device, 1024 * 1024).unwrap();

    print_mem_stats(); // 输出到 /tmp/nvjpeg_stats.log

    println!("Stats written to /tmp/nvjpeg_stats.log");

    delete_all_buffers(thread_id);
    std::env::remove_var("DALI_LOG_FILE");
}

// ============================================================================
// 主函数
// ============================================================================

fn main() {
    println!("nvJPEG Memory Manager - Usage Examples\n");

    example_basic_jpeg_decode();
    example_raii_style();
    example_multithreaded_batch_decode();
    example_best_fit_strategy();
    example_configuration();
    example_env_config();

    #[cfg(feature = "nvjpeg")]
    example_nvjpeg_integration();

    println!("\n=== All examples completed ===");
}

// ============================================================================
// 性能测试
// ============================================================================

#[cfg(test)]
mod benchmarks {
    use super::*;
    use std::time::Instant;

    #[test]
    fn bench_allocation_reuse() {
        let thread_id = thread::current().id();
        const ITERATIONS: usize = 10000;
        const SIZE: usize = 1024 * 1024;

        // 预分配
        add_buffer(thread_id, MemoryKind::Device, SIZE).unwrap();

        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let ptr = get_buffer(thread_id, MemoryKind::Device, SIZE).unwrap();
            get_manager().return_buffer_to_pool(ptr);
        }
        let duration = start.elapsed();

        println!(
            "Allocation reuse: {} iterations in {:?} ({:.2} µs/iter)",
            ITERATIONS,
            duration,
            duration.as_micros() as f64 / ITERATIONS as f64
        );
    }

    #[test]
    fn bench_best_fit_search() {
        let thread_id = thread::current().id();
        const POOL_SIZE: usize = 100;
        const SIZE: usize = 1024;

        // 预分配大量不同大小的 buffer
        for i in 1..=POOL_SIZE {
            add_buffer(thread_id, MemoryKind::Device, SIZE * i).unwrap();
        }

        let start = Instant::now();
        for _ in 0..1000 {
            let ptr = get_buffer(thread_id, MemoryKind::Device, SIZE * 50).unwrap();
            get_manager().return_buffer_to_pool(ptr);
        }
        let duration = start.elapsed();

        println!(
            "Best-fit search (pool size {}): {:?}",
            POOL_SIZE, duration
        );
    }
}
