# rrtos — 仿 FreeRTOS 的精简 RTOS 内核

面向 **STM32H7 (Cortex-M7)** 的抢占式 RTOS 内核，用 Rust 仿照 FreeRTOS 的
结构（TCB、就绪列表、延时列表、PendSV/SVC 上下文切换等）编写。

## 工作区结构

```
rrtos/
├── Cargo.toml            # 工作区（默认 target = thumbv7em-none-eabihf）
├── .cargo/config.toml    # 默认构建目标 STM32H7
├── kernel/               # 核心 crate（name = "kernel"）
│   └── src/
│       ├── lib.rs        # 公共 API 汇总（含 feature 条件编译与再导出）
│       ├── config.rs     # 内核配置常量（对应 FreeRTOSConfig.h）
│       ├── list.rs       # 双向链表（对应 list.h/list.c）
│       ├── port.rs       # Cortex-M 端口：异常帧、临界区、PendSV/SVC/SysTick 汇编
│       ├── task.rs       # TCB 与任务生命周期（对应 task.h）
│       ├── scheduler.rs  # 优先级调度器（对应 tasks.c 调度核心）
│       ├── queue.rs      # 消息队列（对应 queue.c）
│       ├── semaphore.rs  # 二值/计数信号量（对应 semphr.h）
│       ├── mutex.rs      # 互斥锁 + 优先级继承（对应 semphr.h 互斥量）
│       ├── event_group.rs# 事件组（对应 event_groups.c）
│       ├── timer.rs      # 软件定时器（对应 timers.c）
│       ├── heap.rs       # 内存管理（对应 heap_4.c，feature = "heap"）
│       ├── notification.rs # 任务通知（对应 task.h xTaskNotify*，feature = "task-notification"）
│       ├── stream_buffer.rs # 流/消息缓冲（对应 stream_buffer.c，feature = "stream-buffer"）
│       ├── coroutine.rs  # 协程（对应 croutine.h，feature = "coroutine"）
│       └── mpu.rs        # MPU 保护（Cortex-M7 MPU 寄存器抽象，feature = "mpu"）
├── kcore-macro/          # 过程宏 crate（name = "kcore-macro"）
│   └── src/lib.rs        # #[entry]、#[task]、#[timer] 与 static_*!（经 kernel 再导出）
└── examples/             # 全部示例（package 名与目录名相同）
    ├── example/          # API 用法示例（启用全部 feature，仅编译不运行）
    │   └── src/lib.rs
    ├── firmware/         # 实际项目启动示例（cortex-m-rt + stm32h7 裸 PAC，仅基础内核）
    │   ├── memory.x      # STM32H743 内存布局
    │   ├── build.rs      # 把 memory.x 加入链接搜索路径
    │   └── src/main.rs   # 复位入口 + 交替点亮 PC13/PA1 LED
    └── firmware-hal/     # 同上，但用 stm32h7xx-hal 高层 API（feature = "heap"）
        ├── memory.x
        ├── build.rs
        └── src/main.rs   # 把 GPIO 引脚所有权作为任务参数移入任务
```

## 已实现特性

基础内核（**默认启用**，无需任何 feature）：

| 特性 | 对应 FreeRTOS | 说明 |
|------|--------------|------|
| 优先级抢占调度 | `vTaskSwitchContext` / `pxReadyTasksLists` | 每个优先级一个就绪列表，位图 O(1) 找最高优先级 |
| 同优先级轮转 | `listGET_OWNER_OF_NEXT_ENTRY` | 就绪列表 FIFO + `pxIndex` 轮转 |
| 任务延时 | `vTaskDelay` | 双延时列表（正常/溢出）处理 tick 回绕 |
| 挂起/恢复 | `vTaskSuspend` / `vTaskResume` | 挂起列表 |
| 消息队列 | `xQueueSend` / `xQueueReceive` | 环形缓冲 + 阻塞/超时 + 发送/接收等待列表 |
| 互斥锁 | `xSemaphoreCreateMutex` | 优先级继承（沿互斥锁链递归） |
| 二值/计数信号量 | `xSemaphoreCreateBinary/Counting` | 项大小为 0 的队列 |
| 事件组 | `xEventGroupSetBits/WaitBits` | OR/AND 等待、退出清位 |
| 软件定时器 | `xTimerCreate/Start/Stop/Reset` | 定时器守护任务 + 命令队列（ISR 安全） |

高级特性（**可选，按 feature 条件编译**，默认不编译进内核）：

| 特性 | Cargo feature | 对应 FreeRTOS | 说明 |
|------|--------------|--------------|------|
| 内存管理 | `heap` | `pvPortMalloc` / `vPortFree`（heap_4） | kmalloc/kfree + `#[global_allocator]` 桥（`use_alloc!`） |
| 任务通知 | `task-notification` | `xTaskNotify*` / `ulTaskNotifyTake` | 每任务 32 位通知值，计数/置位/覆盖 |
| 流/消息缓冲 | `stream-buffer` | `xStreamBuffer*` / `xMessageBuffer*` | 字节流 + 触发级别；消息缓冲带 4 字节长度头 |
| 协程 | `coroutine` | `croutine.h` | 状态机式协作调度，空闲任务驱动，含协程队列 |
| MPU 保护 | `mpu` | MPU 端口 | Cortex-M7 MPU 区域配置/使能/属性编码 |

启用方式：

```toml
[dependencies]
kernel = { path = "../kernel", features = ["heap", "mpu"] }   # 按需组合
# 或仅基础内核：kernel = { path = "../kernel" }
```

`fpu` 为默认 feature（STM32H7 的 FPU 扩展异常帧），`default-features = false` 可关闭。

## 过程宏

三个过程宏经 `kernel` 再导出，直接以 `#[kernel::...]` 使用。

### `#[kernel::entry]` — 内核入口

```rust
#[kernel::entry]                              // 默认配置
pub fn main() {
    // 创建内核对象 + spawn 任务
    kernel::spawn(worker(42));
    // 函数体结束后自动调用 start_scheduler()
}

// 需要自定义配置时，通过宏参数定义：
#[kernel::entry(tick_rate_hz = 2000, max_priorities = 12, max_tasks = 20)]
pub fn main() { /* ... */ }
```

- 函数无参数，返回 `()` 或 `!`；
- 无宏参数 → 使用 `KernelConfig::default()`；
- 可选参数：`tick_rate_hz`（默认 1000）、`max_priorities`（默认 8，上限 32）、
  `max_tasks`（默认 16，软限制）。

### `#[kernel::task]` — 使函数可被 `spawn`

```rust
#[kernel::task(stack = 1024, priority = 3)]
pub fn worker(arg: u32) -> ! {
    let _ = arg;
    loop { /* ... */ }
}
```

展开为：静态栈 + 静态 TCB + `extern "C"` 入口包装 + 同名构造器
（`worker(arg) -> kernel::TaskHandle`，**仅创建、未调度**）。

- 普通 `fn` 即可，返回 `!`、0 或 1 个参数（无需 `extern "C"`，宏内部自动生成入口包装）；
- `stack` 字节数（默认 512，最小 128）；`priority` 0..32（默认 1，越大越高）；
- **参数无需 `Copy`**：构造器把实参“移动”进任务自己的栈，任务入口再移动取出，
  因此可传递拥有外设所有权、不可 `Copy`/`Clone` 的结构体（内部用 Rust ABI，
  对参数类型无 FFI 布局要求）。

任务创建后需用 [`kernel::spawn`] 启动：

```rust
let h = worker(42);   // 创建任务（返回句柄）
kernel::spawn(h);     // 加入就绪列表，开始参与调度
// 等价简写： kernel::spawn(worker(42));
```

### `#[kernel::timer]` — 软件定时器回调

```rust
#[kernel::timer(period = 500, auto_reload = true)]
pub extern "C" fn blink(id: *mut ()) { /* id 即创建时传入的用户数据 */ }

let h = blink(ptr::null_mut());        // 构造器：blink(id: *mut ()) -> TimerHandle
let _ = kernel::timer_start(h, 0);     // 启动
```

展开为：静态 `Timer` + 回调包装 + 同名构造器（`blink(id: *mut ()) -> kernel::TimerHandle`，未启动）。
构造器传入的 `id` 对应 FreeRTOS 的 `pvTimerID`，定时器触发时原样传给回调。

### Rust 风格互斥锁（`kernel::Mutex<T>`）

锁与被保护数据一体，`lock()` 返回 RAII 守卫，`Deref/DerefMut` 直接读写数据、
离开作用域自动解锁（底层仍是内核的优先级继承互斥量）：

```rust
static CNT: kernel::Mutex<u32> = kernel::Mutex::new(0); // 普通 static，非 static mut

fn bump() {
    let mut g = CNT.lock();      // 阻塞获取（等价 mutex_lock(INFINITE)）
    *g = g.wrapping_add(1);      // DerefMut：无需 unsafe 直接读写数据
}                                // 离开作用域自动解锁（RAII，不可忘记）

let ok = CNT.try_lock().is_some();          // 非阻塞尝试
let g  = CNT.lock_timeout(100).unwrap();    // 带超时（节拍）
```

对应关系：`lock()` ≈ `mutex_lock(INFINITE)`，`lock_timeout(t)` ≈ `mutex_lock(t)`，
`try_lock()` ≈ `mutex_lock(0)`。低层 API（`mutex_lock/mutex_unlock/mutex_create`）
与 `static_mutex!` 仍保留，供 FreeRTOS 风格使用。

### Rust 风格内核对象（类型安全 + 方法调用 + RAII）

除互斥锁外，其余内核对象也提供了 Rust 风格静态类型（普通 `static`、
`&self` 方法、首次使用惰性初始化、离开作用域自动归还）：

```rust
// 消息队列：容量 4 个 u32，类型安全收发
static Q: kernel::StaticQueue<u32, 4> = kernel::StaticQueue::new();
let _ = Q.send(42);                 // 阻塞发送
if let Ok(v) = Q.try_recv() { }     // 非阻塞接收

// 二值 / 计数信号量：acquire 返回 RAII 许可，离开作用域自动 release
static BS: kernel::BinarySemaphore = kernel::BinarySemaphore::new();
let _ = BS.release();
let _permit = BS.acquire();         // 离开作用域自动 release

static CS: kernel::CountingSemaphore<8> = kernel::CountingSemaphore::new();
let _p = CS.acquire();

// 事件组：方法式置位/等待
static EVT: kernel::EventFlags = kernel::EventFlags::new();
EVT.set(0b0001);
let bits = EVT.wait_any(0b0011, true, kernel::INFINITE);

// 流 / 消息缓冲：切片式读写
static SB: kernel::StaticStreamBuffer<256> = kernel::StaticStreamBuffer::new();
let n = SB.write(b"hello");         // 阻塞写入
let mut buf = [0u8; 16];
let n = SB.read(&mut buf);          // 阻塞读取

static MB: kernel::StaticMessageBuffer<256> = kernel::StaticMessageBuffer::new();
MB.send(b"msg");                    // 阻塞发送一条消息
let n = MB.recv(&mut buf);          // 阻塞接收整条消息（buf 需 >= 消息长度）

// 任务通知：Notifier 绑定目标任务句柄，方法式发送
let waiter = notify_waiter(0);      // #[task] 构造器返回 TaskHandle
let n = kernel::Notifier::new(waiter);
n.give();                           // 计数 +1；set_bits / overwrite 同族
```

| Rust 风格类型 | 低层 API |
|--------------|---------|
| `StaticQueue<T, N>` | `queue_send` / `queue_receive` |
| `BinarySemaphore` / `CountingSemaphore<N>` + `SemaphorePermit` | `semaphore_take` / `semaphore_give` |
| `Mutex<T>` + `MutexGuard<T>` | `mutex_lock` / `mutex_unlock` |
| `EventFlags` + `WaitMode` | `event_group_set_bits` / `event_group_wait_bits` |
| `StaticStreamBuffer<N>` / `StaticMessageBuffer<N>` | `stream_buffer_send` / `message_buffer_receive` |
| `Notifier` | `notify_give` / `notify_set_bits` / `notify_overwrite` |

低层 FreeRTOS 风格 API 与 `static_*!` 声明宏全部保留，可按需混用。

### 静态内核对象（`kernel::static_*!`）

内核对象的静态存储与初始化可一行声明，首次访问时在临界区内惰性初始化：

```rust
kernel::static_queue!(MSG_QUEUE, u32, 4);        // 消息队列：4 个 u32
kernel::static_mutex!(SHARED_MUTEX);             // 互斥锁（FreeRTOS 风格低层句柄）
kernel::static_semaphore!(BINARY_SEM);           // 二值信号量
kernel::static_semaphore!(COUNT_SEM, 8, 0);      // 计数信号量（最大 8，初始 0）
kernel::static_event_group!(EVENTS);             // 事件组

// 使用：调用同名访问器获取句柄（首次调用自动初始化）
kernel::queue_send(MSG_QUEUE(), /* item */ ptr, INFINITE);
kernel::mutex_lock(SHARED_MUTEX(), INFINITE);
kernel::semaphore_take(COUNT_SEM(), INFINITE);
kernel::event_group_set_bits(EVENTS(), 0x01);
```

每个宏展开为：静态控制块（+ 数据缓冲）与一个同名访问器函数，
因此无需在 `#[kernel::entry]` 中再手写 `static mut` 与 `*_create` 调用。

## 启动流程

```rust
#[kernel::entry]
pub fn main() {
    // 创建并启动任务
    kernel::spawn(worker(42));

    // 函数体结束后，入口宏自动调用 kernel::start_scheduler()（永不返回）
}
```

## 实际项目启动（cortex-m-rt）

真实工程用 `#[cortex_m_rt::entry]` 作为复位入口，硬件初始化后再交给
`#[kernel::entry]`。见 `examples/firmware/` 示例（交替点亮 PC13/PA1 两个低电平 LED）：

```rust
#![no_std]
#![no_main]

use cortex_m_rt::entry;
use stm32h7::stm32h743 as pac;

const SYSTEM_CLOCK_HZ: u32 = 64_000_000; // 实际 SYSCLK

// 1) 复位入口：初始化硬件，再交给内核
#[entry]
fn main() -> ! {
    init_gpio();
    // SysTick：1ms 节拍，按系统时钟自动计算重载值（内核内部已绑定处理器）
    kernel::systick_init(1000, SYSTEM_CLOCK_HZ);
    rtos_main()     // 永不返回
}

// 2) 内核入口：spawn 任务，末尾自动 start_scheduler()
#[kernel::entry]
fn rtos_main() {
    kernel::spawn(led_pc13());
    kernel::spawn(led_pa1());
}

// 3) 任务：低电平点亮
#[kernel::task(stack = 1024, priority = 1)]
fn led_pc13() -> ! {
    let gpioc = unsafe { &*pac::GPIOC::ptr() };
    loop {
        gpioc.bsrr.write(|w| w.br13().set_bit()); // 低电平点亮
        kernel::task_delay(500);
        gpioc.bsrr.write(|w| w.bs13().set_bit()); // 熄灭
        kernel::task_delay(500);
    }
}
// SysTick 处理器由内核提供，无需再写 #[exception] fn SysTick()
```

要点：

- `PendSV`/`SVC`/`SysTick` 处理器均由内核提供（导出 `PendSV_Handler`/`SVC_Handler`/`SysTick_Handler`
  及短命名别名 `PendSV`/`SVCall`/`SysTick`，兼容 cortex-m-rt 0.7 的向量表），
  应用**不要**再定义同名处理器；
- 系统节拍由 [`kernel::systick_init(interval_us, sysclk_hz)`] 初始化：
  `reload = sysclk_hz × interval_us / 1_000_000`，节拍间隔以**微秒**为单位；
- 需要 `memory.x`（见 `examples/firmware/memory.x`）与 `build.rs` 把其加入链接路径，
  并在 `.cargo/config.toml` 里加 `rustflags = ["-C", "link-arg=-Tlink.x"]`；
- 系统时钟默认按 HSI 64 MHz 计算，若配置了 PLL 请把 `SYSTEM_CLOCK_HZ` 改成实际值。

### HAL 版本（firmware-hal）

`examples/firmware-hal/` 提供等价的 `stm32h7xx-hal` 实现，差异点：

- 用高层时钟/GPIO API：`dp.PWR.constrain()`、`dp.RCC.constrain()` + `sys_ck(100.MHz())`、
  `dp.GPIOC.split(...)` + `into_push_pull_output()`；
- 把 GPIO 引脚（`PC13<Output<PushPull>>`）的**所有权作为任务参数**移动进任务，
  演示 `#[kernel::task]` 的非 `Copy` 参数能力；
- 因需要给任务传引脚，该例用 `kernel::init` + `kernel::spawn` + `kernel::start_scheduler`
  手动初始化，而非无参的 `#[kernel::entry]`。

```rust
type LedPc13 = PC13<Output<PushPull>>;

#[kernel::task(stack = 1024, priority = 1)]
fn led_pc13(mut led: LedPc13) -> ! {
    loop {
        led.set_low();           // 低电平点亮
        kernel::task_delay(500);
        led.set_high();          // 熄灭
        kernel::task_delay(500);
    }
}

#[entry]
fn reset() -> ! {
    let ccdr = /* PWR + RCC 时钟 */;
    let mut led = /* gpioc.pc13.into_push_pull_output() */;
    led.set_high();
    kernel::systick_init(1000, ccdr.clocks.sys_ck().raw()); // SysTick 1ms
    kernel::init(kernel::KernelConfig::default());
    kernel::spawn(led_pc13(led)); // 引脚所有权移入任务
    kernel::start_scheduler()
}
```

## 高级特性用法

> 需在依赖中启用对应 feature（`heap`/`task-notification`/`stream-buffer`/`coroutine`/`mpu`），
> 见上表。`examples/example` 启用了全部特性。

```rust
// 1) 内存管理：静态堆 + kmalloc/kfree（对应 heap_4）
static mut HEAP: [u8; 16 * 1024] = [0; 16 * 1024];
kernel::heap_init(ptr::addr_of_mut!(HEAP) as *mut u8, 16 * 1024);
let p = kernel::kmalloc(256);   // 8 字节对齐，失败返回 null
kernel::kfree(p);
let _ = kernel::heap_free_bytes();

// 1b) 注册为 Rust 原生 #[global_allocator]，直接使用 alloc（Vec/String/Box）
extern crate alloc;
kernel::use_alloc!();                       // 等价于手写 #[global_allocator] static
let mut v: alloc::vec::Vec<u32> = alloc::vec::Vec::with_capacity(16);
v.push(1);
let s: alloc::string::String = alloc::string::String::from("rrtos");
let b: alloc::boxed::Box<u32> = alloc::boxed::Box::new(42);
// 离开作用域自动释放；>8 字节对齐的分配也受支持（自动过度分配 + 指针恢复）

// 2) 任务通知：每任务一个 32 位通知值
let waiter = notify_waiter(0);          // #[task] 构造器返回句柄
kernel::spawn(waiter);
kernel::notify_give(waiter);            // 计数 +1（对应 xTaskNotifyGive）
kernel::notify_set_bits(waiter, 0x01);  // 置位（对应 eSetBits）
kernel::notify_overwrite(waiter, 42);   // 覆盖（对应 eSetValueWithOverwrite）
// 等待方：
let v = kernel::notify_take(true, INFINITE);      // 计数 -1/清零，超时返回 0
let v = kernel::notify_wait(false, INFINITE);     // 读取，返回 Option<u32>

// 3) 流/消息缓冲：一行声明 + 同名访问器
kernel::static_stream_buffer!(STREAM, 256, 1);    // 流缓冲（触发级别 1）
kernel::static_message_buffer!(MSGS, 256);        // 消息缓冲（4 字节长度头）
let n = kernel::stream_buffer_send(STREAM(), data, 4, INFINITE);
let m = kernel::message_buffer_receive(MSGS(), buf, 8, INFINITE); // 可跨调用读完一条消息

// 4) 协程：状态机函数 + 空闲任务自动调度
fn my_co(co: &mut kernel::CoRoutine) {
    match co.step {
        0 => { /* 第一步 */ co.step = 1; co.delay(100); }
        _ => { /* 第二步 */ co.step = 0; co.delay(200); }
    }
}
kernel::co_routine_create(&mut CO1, my_co, 1);   // 无需 spawn，空闲任务驱动
kernel::co_queue_send(&mut CO_Q, item);          // 协程队列（非阻塞）

// 5) MPU 保护：区域配置 + 使能
kernel::mpu_configure_region(0, &kernel::MpuRegion {
    base: 0x2000_0000,
    size: 32 * 1024,
    access: kernel::AP_PRIV_RW,
    attributes: kernel::ATTR_NORMAL,
    executable: false,          // XN：禁止执行
});
kernel::mpu_enable(true, false);
```

完整示例见 `examples/example/src/lib.rs`（一次性覆盖全部特性）。

## 栈溢出排查

任务创建时整块栈被填充为 `0xE7`，可用高水位 API 检查历史最大用量：

```rust
let used = kernel::task_stack_used(handle); // 历史最大已用字节
let free = kernel::task_stack_free(handle); // 最小剩余字节（越接近 0 越危险）
```

可用栈预算 = `stack` 参数 - 68 字节栈头 - 任务参数结构体大小。
带外设驱动（SPI/显示等）的任务建议 `stack >= 2048`，剩余越少越应加大。
典型溢出症状：Escalated BusFault（Imprecise data access）——写穿栈区进入未映射内存。

## 构建

```bash
# 需已安装目标
rustup target add thumbv7em-none-eabihf

cargo build --release              # 编译整个工作区
cargo build -p firmware            # 裸 PAC 版固件（ELF）
cargo build -p firmware-hal        # HAL 版固件（ELF）

# 烧录（以 probe-rs 为例，按你的调试器调整）
probe-rs download target/thumbv7em-none-eabihf/release/firmware
probe-rs download target/thumbv7em-none-eabihf/release/firmware-hal
```

## 与 FreeRTOS 的差异（精简范围）

- 仅实现 STM32H7 (Cortex-M7) 架构，未实现其它移植；
- 互斥锁优先级继承在“释放最后一个锁”时恢复到基本优先级（未维护逐锁继承栈）；
- 软件定时器 `reset` 简化：按完整周期从当前时刻重新计时（不保留原始相位）；
- 堆为“首次适配 + 合并”的简化 heap_4（无 5 区多段、无统计钩子）；
- 任务通知以“通知值非零即已通知”为判定（省略 FreeRTOS 的 `ucNotifyState` 三态机）；
- 协程为显式状态机（Rust 无 C 的 setjmp/longjmp 与协程宏），由空闲任务协作调度；
- MPU 提供寄存器级配置接口，未实现“每任务自动切换区域/栈保护”的完整 MPU 端口。
