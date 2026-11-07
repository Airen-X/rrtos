//! 内核配置常量、类型别名与运行时配置。
//!
//! 对应 FreeRTOS `FreeRTOSConfig.h`。分两层：
//! - 编译期上限（数组尺寸，固定）：`MAX_PRIORITIES` / `MAX_TASKS` 等；
//! - 运行时配置 [`KernelConfig`]：由 `#[kernel::entry(...)]` 宏参数提供，
//!   未提供时使用 [`KernelConfig::default`]。

#![allow(dead_code)]

/// 优先级类型（0 最低，越大越高），对应 FreeRTOS `UBaseType_t`。
pub type Priority = u8;

/// 节拍计数类型，对应 FreeRTOS `TickType_t`。
pub type TickType = u32;

/// 任务句柄（指向 TCB 的裸指针），对应 FreeRTOS `TaskHandle_t`。
pub type TaskHandle = *mut crate::task::Task;

/// 无限等待，对应 FreeRTOS `portMAX_DELAY`。
pub const INFINITE: TickType = 0xFFFF_FFFF;

/// 就绪列表数组的编译期上限（可配置的最大优先级数量）。
/// 运行时由 `KernelConfig::max_priorities` 限制实际使用的数量。
pub const MAX_PRIORITIES: usize = 32;

/// 内核对象等待者/事件组唤醒缓冲等使用的编译期任务数上限。
pub const MAX_TASKS: usize = 32;

/// 默认系统节拍频率（Hz），对应 `configTICK_RATE_HZ`。
pub const DEFAULT_TICK_RATE_HZ: u32 = 1000;

/// 默认最大优先级数量。
pub const DEFAULT_MAX_PRIORITIES: usize = 8;

/// 默认最大任务数量。
pub const DEFAULT_MAX_TASKS: usize = 16;

/// 已按 STM32H7 (4-bit NVIC 优先级) 左移后的 `configMAX_SYSCALL_INTERRUPT_PRIORITY`。
/// 逻辑优先级 5 << 4 = 0x50。内核临界区向 BASEPRI 写入该值。
pub const MAX_SYSCALL_INTERRUPT_PRIORITY: u32 = 0x50;

/// SysTick / PendSV 建议配置为最低优先级（数值最大）。
pub const LOWEST_INTERRUPT_PRIORITY: u32 = 0xF0;

/// 空闲任务栈大小（字节）。
pub const IDLE_STACK_SIZE: usize = 256;

/// 软件定时器命令队列长度。
pub const TIMER_COMMAND_QUEUE_LEN: usize = 8;

/// 软件定时器服务任务栈大小（字节）。
pub const TIMER_DAEMON_STACK_SIZE: usize = 512;

/// 运行时内核配置。由 `#[kernel::entry(...)]` 宏参数构建，缺省用 [`Default`]。
#[derive(Clone, Copy, Debug)]
pub struct KernelConfig {
    /// 系统节拍频率（Hz），用户据此配置 SysTick。
    pub tick_rate_hz: u32,
    /// 实际使用的优先级数量（<= `MAX_PRIORITIES`）。
    pub max_priorities: usize,
    /// 最大任务数量（软限制）。
    pub max_tasks: usize,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            tick_rate_hz: DEFAULT_TICK_RATE_HZ,
            max_priorities: DEFAULT_MAX_PRIORITIES,
            max_tasks: DEFAULT_MAX_TASKS,
        }
    }
}
