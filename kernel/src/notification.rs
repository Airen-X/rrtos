//! 任务通知（Task Notification）。
//!
//! 对应 FreeRTOS `task.h` 的 `xTaskNotify*` / `ulTaskNotifyTake` / `xTaskNotifyWait`。
//! 任务通知是比信号量/队列更轻量的任务间通信：每个任务自带一个 32 位通知值。

use crate::config::{TaskHandle, TickType, INFINITE};
use crate::port;
use crate::scheduler::{self, TimeOut};
use crate::task;

/// 唤醒正在等待通知的目标任务（若其阻塞原因正是通知）。
unsafe fn wake_if_waiting(t: TaskHandle) -> bool {
    unsafe {
        if t.is_null() {
            return false;
        }
        let wake = (*t).notify_waiting;
        if wake {
            (*t).notify_waiting = false;
            return scheduler::unblock_task(t);
        }
        false
    }
}

/// 通知值 +1（计数式），对应 `xTaskNotifyGive`。
///
/// 返回 `true` 表示唤醒了更高优先级任务（调用方应触发切换）。
pub unsafe fn notify_give(t: TaskHandle) -> bool {
    unsafe {
        port::enter_critical();
        (*t).notification = (*t).notification.saturating_add(1);
        let wake = wake_if_waiting(t);
        port::exit_critical();
        wake
    }
}

/// 通知值按位或（置位），对应 `xTaskNotify(..., eSetBits)`。
pub unsafe fn notify_set_bits(t: TaskHandle, bits: u32) -> bool {
    unsafe {
        port::enter_critical();
        (*t).notification |= bits;
        let wake = wake_if_waiting(t);
        port::exit_critical();
        wake
    }
}

/// 通知值覆盖写入，对应 `xTaskNotify(..., eSetValueWithOverwrite)`。
pub unsafe fn notify_overwrite(t: TaskHandle, value: u32) -> bool {
    unsafe {
        port::enter_critical();
        (*t).notification = value;
        let wake = wake_if_waiting(t);
        port::exit_critical();
        wake
    }
}

/// 从中断发送“+1”通知，对应 `vTaskNotifyGiveFromISR`。
pub unsafe fn notify_give_from_isr(t: TaskHandle) -> bool {
    unsafe { notify_give(t) }
}

/// 等待通知并取值（可选减 1 或清零），对应 `ulTaskNotifyTake`。
///
/// - `clear_count == true`：返回后把计数清零；
/// - `clear_count == false`：返回后计数减 1；
/// - 超时返回 0。
pub fn notify_take(clear_count: bool, timeout: TickType) -> u32 {
    unsafe {
        let cur = task::current_task();
        let mut to = TimeOut::new();
        let mut remaining = timeout;
        if timeout != 0 && timeout != INFINITE {
            scheduler::set_timeout_state(&mut to);
        }

        loop {
            port::enter_critical();
            (*cur).notify_waiting = false;

            if (*cur).notification != 0 {
                let ret = (*cur).notification;
                if clear_count {
                    (*cur).notification = 0;
                } else {
                    (*cur).notification = ret - 1;
                }
                port::exit_critical();
                return ret;
            }

            if timeout == 0 {
                port::exit_critical();
                return 0;
            }
            if scheduler::check_timeout(&mut to, &mut remaining) {
                port::exit_critical();
                return 0;
            }

            (*cur).notify_waiting = true;
            scheduler::add_current_to_delayed_list(remaining, true);
            port::exit_critical();
            port::yield_from_task();
        }
    }
}

/// 等待通知并读取值，对应 `xTaskNotifyWait`。
///
/// - `clear_on_exit == true`：读取后把通知值清零；
/// - 超时返回 `None`。
pub fn notify_wait(clear_on_exit: bool, timeout: TickType) -> Option<u32> {
    unsafe {
        let cur = task::current_task();
        let mut to = TimeOut::new();
        let mut remaining = timeout;
        if timeout != 0 && timeout != INFINITE {
            scheduler::set_timeout_state(&mut to);
        }

        loop {
            port::enter_critical();
            (*cur).notify_waiting = false;

            if (*cur).notification != 0 {
                let ret = (*cur).notification;
                if clear_on_exit {
                    (*cur).notification = 0;
                }
                port::exit_critical();
                return Some(ret);
            }

            if timeout == 0 {
                port::exit_critical();
                return None;
            }
            if scheduler::check_timeout(&mut to, &mut remaining) {
                port::exit_critical();
                return None;
            }

            (*cur).notify_waiting = true;
            scheduler::add_current_to_delayed_list(remaining, true);
            port::exit_critical();
            port::yield_from_task();
        }
    }
}

/// 读取当前任务的通知值（不等待、不修改）。
pub fn notify_value() -> u32 {
    unsafe { (*task::current_task()).notification }
}

// =====================================================================
// Rust 风格任务通知：`Notifier` —— 面向某个任务的发送端
// =====================================================================

/// Rust 风格通知发送端：绑定一个目标任务，方法式发送。
///
/// 接收端仍使用 [`notify_take`] / [`notify_wait`]（作用于当前任务）。
///
/// ```ignore
/// static WAITER: kernel::Notifier = kernel::Notifier::new(waiter_handle);
///
/// WAITER.give();        // 计数 +1（可从中断调用）
/// WAITER.set_bits(1);   // 按位或
/// WAITER.overwrite(42); // 覆盖写入
/// ```
pub struct Notifier {
    task: TaskHandle,
}

impl Notifier {
    /// 绑定目标任务（`#[task]` 构造器返回的句柄）。
    pub const fn new(task: TaskHandle) -> Self {
        Self { task }
    }

    /// 通知值 +1，对应 `xTaskNotifyGive`。返回 `true` 表示唤醒了更高优先级任务。
    pub fn give(&self) -> bool {
        unsafe { notify_give(self.task) }
    }

    /// 按位或置位，对应 `eSetBits`。
    pub fn set_bits(&self, bits: u32) -> bool {
        unsafe { notify_set_bits(self.task, bits) }
    }

    /// 覆盖写入，对应 `eSetValueWithOverwrite`。
    pub fn overwrite(&self, value: u32) -> bool {
        unsafe { notify_overwrite(self.task, value) }
    }

    /// 从中断调用：通知值 +1。
    pub fn give_from_isr(&self) -> bool {
        unsafe { notify_give_from_isr(self.task) }
    }

    /// 目标任务句柄。
    pub const fn task(&self) -> TaskHandle {
        self.task
    }
}

unsafe impl Send for Notifier {}
unsafe impl Sync for Notifier {}
