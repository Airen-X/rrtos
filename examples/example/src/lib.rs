//! rrtos 用法示例：演示全部特性——
//! 基础：调度器、队列、互斥锁、信号量、事件组、软件定时器；
//! 高级：内存管理、任务通知、流/消息缓冲、协程、MPU 保护。
//!
//! 本 crate 仅为演示，无硬件依赖，不能直接运行。

#![no_std]

extern crate alloc;

use core::ptr;

use kernel::{self, INFINITE};

// 注册内核堆为 Rust 全局分配器：之后即可使用 alloc::vec::Vec / String / Box 等
kernel::use_alloc!();

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

// ---------------------------------------------------------------------
// 基础内核对象：一行声明，静态存储由宏生成，首次访问时惰性初始化
// ---------------------------------------------------------------------

// 消息队列：4 个 u32
kernel::static_queue!(MSG_QUEUE, u32, 4);
// 互斥锁
kernel::static_mutex!(SHARED_MUTEX);
// 二值信号量
kernel::static_semaphore!(BINARY_SEM);
// 计数信号量（最多 8，初始 0）
kernel::static_semaphore!(COUNT_SEM, 8, 0);
// 事件组
kernel::static_event_group!(EVENTS);

// ---------------------------------------------------------------------
// 高级特性 1：内存管理（静态堆 + kmalloc/kfree）
// ---------------------------------------------------------------------

static mut HEAP: [u8; 16 * 1024] = [0; 16 * 1024];

/// 演示堆分配/释放的任务：同时展示 kmalloc/kfree 与 Rust 原生 alloc（Vec/String/Box）。
#[kernel::task(stack = 4096, priority = 1)]
pub extern "C" fn heap_task(_arg: u32) -> ! {
    kernel::task_delay(100); // 等堆初始化完成
    loop {
        // 1) C 风格：kmalloc / kfree
        unsafe {
            let p = kernel::kmalloc(256);
            if !p.is_null() {
                p.write_volatile(0x5A);
                let _ = p.read_volatile();
                kernel::kfree(p);
            }
        }

        // 2) Rust 原生 alloc：Vec / String / Box 自动申请与释放
        let mut v: Vec<u32> = Vec::with_capacity(16);
        v.push(1);
        v.push(2);
        let mut s: String = String::new();
        s.push_str("rrtos");
        let b: Box<u32> = Box::new(42);
        let _ = v.len();
        let _ = s.len();
        let _ = *b;
        // 超出作用域自动释放

        let _ = kernel::heap_free_bytes();
        kernel::task_delay(1000);
    }
}

// ---------------------------------------------------------------------
// 高级特性 2：任务通知
// ---------------------------------------------------------------------

static mut NOTIFY_TASK_HANDLE: kernel::TaskHandle = ptr::null_mut();

/// 等待通知的任务（每次被通知后处理一次）。
#[kernel::task(stack = 1024, priority = 2)]
pub extern "C" fn notify_waiter(_arg: u32) -> ! {
    loop {
        // 等待计数式通知并清零，超时则继续等（这里无限等待）
        let count = kernel::notify_take(true, INFINITE);
        let _ = count;
    }
}

/// 周期性发送通知的任务。
#[kernel::task(stack = 1024, priority = 3)]
pub extern "C" fn notifier(_arg: u32) -> ! {
    loop {
        kernel::task_delay(500);
        unsafe {
            let wake = kernel::notify_give(NOTIFY_TASK_HANDLE);
            if wake {
                kernel::task_yield();
            }
        }
    }
}

// ---------------------------------------------------------------------
// 高级特性 3：流缓冲 + 消息缓冲
// ---------------------------------------------------------------------

// 流缓冲：256 字节，触发级别 1
kernel::static_stream_buffer!(STREAM, 256, 1);
// 消息缓冲：256 字节（含 4 字节长度头）
kernel::static_message_buffer!(MSGS, 256);

/// 流/消息生产者。
#[kernel::task(stack = 1024, priority = 2)]
pub extern "C" fn stream_producer(_arg: u32) -> ! {
    let mut counter: u32 = 0;
    loop {
        let data = counter.to_le_bytes();
        let _ = kernel::stream_buffer_send(STREAM(), data.as_ptr(), 4, INFINITE);
        let _ = kernel::message_buffer_send(MSGS(), data.as_ptr(), 4, INFINITE);
        counter = counter.wrapping_add(1);
        kernel::task_delay(200);
    }
}

/// 流/消息消费者。
#[kernel::task(stack = 1024, priority = 1)]
pub extern "C" fn stream_consumer(_arg: u32) -> ! {
    loop {
        let mut buf = [0u8; 8];
        // 流：读多少算多少
        let n = kernel::stream_buffer_receive(STREAM(), buf.as_mut_ptr(), 8, INFINITE);
        let _ = n;
        // 消息：按完整消息读（跨调用时由内核维护进度）
        let mut msg = [0u8; 8];
        let m = kernel::message_buffer_receive(MSGS(), msg.as_mut_ptr(), 8, INFINITE);
        let _ = m;
    }
}

// ---------------------------------------------------------------------
// 高级特性 4：协程（状态机 + 空闲任务驱动 + 协程队列）
// ---------------------------------------------------------------------

static mut CO1: kernel::CoRoutine = kernel::CoRoutine::empty();
static mut CO_QUEUE_STORAGE: [u8; 8 * 4] = [0; 32];
static mut CO_QUEUE: kernel::CoRoutineQueue = kernel::CoRoutineQueue::empty();

/// 协程：周期向协程队列投递数据（每个节拍组运行一步）。
fn producer_co(co: &mut kernel::CoRoutine) {
    match co.step {
        0 => {
            let mut val: u32 = 0xAA;
            if kernel::co_queue_send(ptr::addr_of_mut!(CO_QUEUE), (&raw mut val) as *const u8) {
                co.step = 1;
            }
            co.delay(100);
        }
        _ => {
            co.step = 0;
            co.delay(200);
        }
    }
}

// ---------------------------------------------------------------------
// 高级特性 5：MPU 保护（配置一段内存区域并在启动时使能）
// ---------------------------------------------------------------------

/// 演示基础特性仍然可用的任务（生产者/消费者 + 互斥锁 + 事件组 + 信号量）。
#[kernel::task(stack = 1024, priority = 3)]
pub extern "C" fn producer(_arg: u32) -> ! {
    let mut counter: u32 = 0;
    loop {
        let _ = kernel::queue_send(MSG_QUEUE(), (&raw const counter) as *const u8, INFINITE);
        let _ = kernel::event_group_set_bits(EVENTS(), 0x01);
        counter = counter.wrapping_add(1);
        kernel::task_delay(100);
    }
}

#[kernel::task(stack = 1024, priority = 2)]
pub extern "C" fn consumer(_arg: u32) -> ! {
    loop {
        let mut value: u32 = 0;
        if kernel::queue_receive(MSG_QUEUE(), (&raw mut value) as *mut u8, INFINITE).is_ok() {
            let _ = value;
        }
    }
}

static mut SHARED_COUNTER: u32 = 0;

#[kernel::task(stack = 1024, priority = 4)]
pub extern "C" fn mutex_task(_arg: u32) -> ! {
    loop {
        unsafe {
            let _ = kernel::mutex_lock(SHARED_MUTEX(), INFINITE);
            SHARED_COUNTER = SHARED_COUNTER.wrapping_add(1);
            let _ = kernel::mutex_unlock(SHARED_MUTEX());
        }
        kernel::task_delay(50);
    }
}

#[kernel::task(stack = 1024, priority = 1)]
pub extern "C" fn sync_task(_arg: u32) -> ! {
    loop {
        let bits = kernel::event_group_wait_bits(EVENTS(), 0x01, true, false, INFINITE);
        let _ = bits;
        let _ = kernel::semaphore_take(COUNT_SEM(), INFINITE);
        let _ = kernel::semaphore_give(COUNT_SEM(), 0);
    }
}

// ---------------------------------------------------------------------
// 软件定时器回调
// ---------------------------------------------------------------------

#[kernel::timer(period = 500, auto_reload = true)]
pub extern "C" fn blink(_id: *mut ()) {
    let _ = _id;
}

// ---------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------

/// 内核入口：默认配置。也可用宏参数定制，例如：
/// `#[kernel::entry(tick_rate_hz = 2000, max_priorities = 12)]`
#[kernel::entry]
pub fn setup() {
    // 1) 内存管理：初始化静态堆
    unsafe {
        kernel::heap_init(ptr::addr_of_mut!(HEAP) as *mut u8, 16 * 1024);
        // 协程队列
        kernel::co_queue_create(
            ptr::addr_of_mut!(CO_QUEUE),
            ptr::addr_of_mut!(CO_QUEUE_STORAGE) as *mut u8,
            8,
            4,
        );
    }

    // 2) 任务通知：先创建等待任务，再让发送者持有其句柄
    let waiter = notify_waiter(0);
    unsafe {
        NOTIFY_TASK_HANDLE = waiter;
    }
    kernel::spawn(waiter);

    // 3) 其余任务
    kernel::spawn(heap_task(0));
    kernel::spawn(notifier(0));
    kernel::spawn(stream_producer(0));
    kernel::spawn(stream_consumer(0));
    kernel::spawn(producer(0));
    kernel::spawn(consumer(0));
    kernel::spawn(mutex_task(0));
    kernel::spawn(sync_task(0));

    // 4) 协程：由空闲任务驱动
    kernel::co_routine_create(ptr::addr_of_mut!(CO1), producer_co, 1);

    // 5) 软件定时器
    let timer_handle = blink();
    let _ = kernel::timer_start(timer_handle, 0);

    // 6) MPU：保护堆所在的内存区域（特权读写、不可执行），并使能 MPU
    kernel::mpu_configure_region(
        0,
        &kernel::MpuRegion {
            base: ptr::addr_of_mut!(HEAP) as usize as u32,
            size: 32 * 1024,
            access: kernel::AP_PRIV_RW,
            attributes: kernel::ATTR_NORMAL,
            executable: false,
        },
    );
    kernel::mpu_enable(true, false);

    // 函数体执行完后，`#[kernel::entry]` 会自动调用 start_scheduler()
}
