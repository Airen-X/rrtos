//! # rrtos 精简内核
//!
//! 面向 STM32H7 (Cortex-M7) 的、仿 FreeRTOS 逻辑的抢占式 RTOS 内核。
//! 实现：优先级调度器、互斥锁（含优先级继承）、软件定时器、信号量、
//! 事件组、消息队列。
//!
//! 用法概览：
//! 1. 用 `#[kernel::task]` / `#[kernel::timer]` 宏定义任务与定时器；
//! 2. 用 `#[kernel::entry(...)]` 定义入口（可选宏参数配置内核），在函数体内
//!    创建内核对象并用 [`kernel::spawn`] 启动任务；
//! 3. 在向量表的 `SysTick_Handler` 中调用 [`kernel::tick_handler`]。

#![no_std]
// 内核各模块在临界区保护下操作裸指针；与参考实现一致，unsafe 函数体内
// 允许直接解引用（避免在链表/TCB 等热点路径上铺满 unsafe 块）。
#![allow(unsafe_op_in_unsafe_fn)]

#[cfg(not(target_arch = "arm"))]
compile_error!("rrtos 内核仅支持 arm (Cortex-M) 架构");

// 再导出过程宏，从而支持 `#[kernel::entry]` / `#[kernel::task]` / `#[kernel::timer]`
// 与静态对象声明 `kernel::static_queue!` / `kernel::static_mutex!` 等。
// 依赖高级特性的声明宏按对应 feature 再导出。
pub use kcore_macro::{entry, static_event_group, static_mutex, static_queue, static_semaphore, task, timer};
#[cfg(feature = "stream-buffer")]
pub use kcore_macro::{static_message_buffer, static_stream_buffer};

pub mod config;
pub mod list;
mod port;
pub mod task;
mod scheduler;
pub mod queue;
pub mod semaphore;
pub mod mutex;
pub mod event_group;
pub mod timer;

// ---- 高级特性模块（按 feature 条件编译）----
#[cfg(feature = "heap")]
pub mod heap;
#[cfg(feature = "task-notification")]
pub mod notification;
#[cfg(feature = "stream-buffer")]
pub mod stream_buffer;
#[cfg(feature = "coroutine")]
pub mod coroutine;
#[cfg(feature = "mpu")]
pub mod mpu;

pub use config::{KernelConfig, Priority, TaskHandle, TickType, INFINITE};
pub use port::{enter_critical, exit_critical};
pub use task::{
    EntryFn, Task, TaskState, current_task, frame_size, load_param, spawn, task_count, task_create,
    task_delete, task_delay, task_get_priority, task_resume, task_set_priority, task_suspend,
    task_yield,
};
pub use scheduler::{init, max_priorities, max_tasks, start_scheduler, tick_handler, tick_rate_hz};
pub use queue::{
    queue_create, queue_is_empty, queue_is_full, queue_messages_waiting, queue_receive,
    queue_receive_from_isr, queue_send, queue_send_from_isr, Queue, QueueError, QueueHandle,
    QUEUE_TYPE_BASE, QUEUE_TYPE_BINARY_SEMAPHORE, QUEUE_TYPE_COUNTING_SEMAPHORE, QUEUE_TYPE_MUTEX,
};
pub use semaphore::{
    semaphore_count, semaphore_create_binary, semaphore_create_counting, semaphore_give,
    semaphore_give_from_isr, semaphore_take, semaphore_take_from_isr,
};
pub use mutex::{mutex_create, mutex_holder, mutex_lock, mutex_unlock};
pub use event_group::{
    event_group_clear_bits, event_group_create, event_group_get_bits, event_group_set_bits,
    event_group_set_bits_from_isr, event_group_wait_bits, EventGroup, EventGroupHandle,
};
pub use timer::{
    timer_create, timer_reset, timer_start, timer_start_from_isr, timer_stop, timer_stop_from_isr,
    Timer, TimerCallbackFn, TimerHandle,
};

// ---- 高级特性再导出（按 feature 条件编译）----
#[cfg(feature = "heap")]
pub use heap::{heap_free_bytes, heap_init, heap_total_bytes, kfree, kmalloc, KernelAllocator};
#[cfg(feature = "task-notification")]
pub use notification::{
    notify_give, notify_give_from_isr, notify_overwrite, notify_set_bits, notify_take,
    notify_value, notify_wait,
};
#[cfg(feature = "stream-buffer")]
pub use stream_buffer::{
    message_buffer_create, message_buffer_receive, message_buffer_send, stream_buffer_bytes,
    stream_buffer_create, stream_buffer_receive, stream_buffer_receive_from_isr, stream_buffer_send,
    stream_buffer_send_from_isr, stream_buffer_spaces, MessageBuffer, MessageBufferHandle,
    StreamBuffer, StreamBufferHandle,
};
#[cfg(feature = "coroutine")]
pub use coroutine::{
    co_queue_count, co_queue_create, co_queue_receive, co_queue_send, co_routine_create,
    co_routine_delete, co_routine_resume, scheduler_tick as co_routine_scheduler_tick, CoRoutine,
    CoRoutineFn, CoRoutineQueue, CoRoutineQueueHandle, CoRoutineState,
};
#[cfg(feature = "mpu")]
pub use mpu::{
    mpu_configure_region, mpu_disable, mpu_disable_region, mpu_enable, mpu_region_count,
    mpu_size_field, MpuRegion, AP_FULL, AP_NO_ACCESS, AP_PRIV_RO, AP_PRIV_RW,
    AP_PRIV_RW_USER_RO, AP_RO, ATTR_DEVICE, ATTR_NORMAL, ATTR_STRONGLY_ORDERED,
};
