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
kernel::use_alloc!(HEAP:16*1024);

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

// ---------------------------------------------------------------------
// 内核对象：Rust 风格静态类型（普通 static，方法调用，首次使用惰性初始化）
// ---------------------------------------------------------------------

// 消息队列：容量 4 个 u32
static MSG_QUEUE: kernel::StaticQueue<u32, 4> = kernel::StaticQueue::new();
// 二值信号量
static BINARY_SEM: kernel::BinarySemaphore = kernel::BinarySemaphore::new();
// 计数信号量（容量 8，初始 0）
static COUNT_SEM: kernel::CountingSemaphore<8> = kernel::CountingSemaphore::new();
// 事件组
static EVENTS: kernel::EventFlags = kernel::EventFlags::new();

// ---------------------------------------------------------------------
// 高级特性 1：内存管理（静态堆 + kmalloc/kfree）
// ---------------------------------------------------------------------

/// 演示堆分配/释放的任务：同时展示 kmalloc/kfree 与 Rust 原生 alloc（Vec/String/Box）。
#[kernel::task(stack = 4096, priority = 1)]
pub fn heap_task(_arg: u32) -> ! {
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

/// 等待通知的任务（每次被通知后处理一次）。
#[kernel::task(stack = 1024, priority = 2)]
pub fn notify_waiter(_arg: u32) -> ! {
    loop {
        // 等待计数式通知并清零，超时则继续等（这里无限等待）
        let count = kernel::notify_take(true, INFINITE);
        let _ = count;
    }
}

/// 周期性发送通知的任务：`Notifier` 绑定目标任务的句柄，方法式发送。
#[kernel::task(stack = 1024, priority = 3)]
pub fn notifier(n: kernel::Notifier) -> ! {
    loop {
        kernel::task_delay(500);
        if n.give() {
            kernel::task_yield();
        }
    }
}

// ---------------------------------------------------------------------
// 高级特性 3：流缓冲 + 消息缓冲
// ---------------------------------------------------------------------

// 流缓冲：256 字节
static STREAM: kernel::StaticStreamBuffer<256> = kernel::StaticStreamBuffer::new();
// 消息缓冲：256 字节（含 4 字节长度头）
static MSGS: kernel::StaticMessageBuffer<256> = kernel::StaticMessageBuffer::new();

/// 流/消息生产者。
#[kernel::task(stack = 1024, priority = 2)]
pub fn stream_producer(_arg: u32) -> ! {
    let mut counter: u32 = 0;
    loop {
        let data = counter.to_le_bytes();
        let _ = STREAM.write(&data);
        let _ = MSGS.send(&data);
        counter = counter.wrapping_add(1);
        kernel::task_delay(200);
    }
}

/// 流/消息消费者。
#[kernel::task(stack = 1024, priority = 1)]
pub fn stream_consumer(_arg: u32) -> ! {
    loop {
        let mut buf = [0u8; 8];
        // 流：读多少算多少
        let n = STREAM.read(&mut buf);
        let _ = n;
        // 消息：一次性读出整条消息（buf 需 >= 消息长度）
        let mut msg = [0u8; 8];
        let m = MSGS.recv(&mut msg);
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
pub fn producer(_arg: u32) -> ! {
    let mut counter: u32 = 0;
    loop {
        let _ = MSG_QUEUE.send(counter);
        let _ = EVENTS.set(0x01);
        counter = counter.wrapping_add(1);
        kernel::task_delay(100);
    }
}

#[kernel::task(stack = 1024, priority = 2)]
pub fn consumer(_arg: u32) -> ! {
    loop {
        if let Ok(value) = MSG_QUEUE.recv() {
            let _ = value;
        }
    }
}

/// Rust 风格互斥锁：锁与被保护数据一体（普通 `static`，无需 unsafe 访问数据）。
static SHARED_COUNTER: kernel::Mutex<u32> = kernel::Mutex::new(0);

#[kernel::task(stack = 1024, priority = 4)]
pub fn mutex_task(_arg: u32) -> ! {
    loop {
        {
            let mut g = SHARED_COUNTER.lock(); // 阻塞获取（内部优先级继承）
            *g = g.wrapping_add(1); // DerefMut 直接读写数据
        } // 离开作用域自动解锁（RAII）
        kernel::task_delay(50);
    }
}

#[kernel::task(stack = 1024, priority = 1)]
pub fn sync_task(_arg: u32) -> ! {
    loop {
        let bits = EVENTS.wait_any(0x01, true, INFINITE);
        let _ = bits;
        // 计数信号量：acquire 返回 RAII 许可，离开作用域自动 release
        let _permit = COUNT_SEM.acquire();
        // 二值信号量：初始为空，release 一次后 acquire 取到许可
        let _ = BINARY_SEM.release();
        let _gate = BINARY_SEM.acquire();
    }
}

// ---------------------------------------------------------------------
// 软件定时器回调
// ---------------------------------------------------------------------

#[kernel::timer(period = 500, auto_reload = true)]
pub extern "C" fn blink(id: *mut ()) {
    // id 是创建时传入的定时器 ID（FreeRTOS 的 pvTimerID）
    let _ = id;
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

    // 2) 任务通知：先创建等待任务，再把它的句柄封装成 Notifier 传给发送者
    let waiter = notify_waiter(0);
    kernel::spawn(waiter);
    kernel::spawn(notifier(kernel::Notifier::new(waiter)));

    // 3) 其余任务
    kernel::spawn(heap_task(0));
    kernel::spawn(stream_producer(0));
    kernel::spawn(stream_consumer(0));
    kernel::spawn(producer(0));
    kernel::spawn(consumer(0));
    kernel::spawn(mutex_task(0));
    kernel::spawn(sync_task(0));

    // 4) 协程：由空闲任务驱动
    kernel::co_routine_create(ptr::addr_of_mut!(CO1), producer_co, 1);
    // 5) 软件定时器：创建（传入定时器 ID 用户数据）并启动
    let timer_handle = blink(ptr::null_mut());
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
