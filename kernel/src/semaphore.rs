//! 信号量（二值 / 计数）。
//!
//! 对应 FreeRTOS `semphr.h`：以“项大小为 0 的队列”实现。

use core::ptr;

use crate::config::TickType;
use crate::queue::{self, QueueHandle, QueueError, QUEUE_TYPE_BINARY_SEMAPHORE, QUEUE_TYPE_COUNTING_SEMAPHORE};

/// 创建二值信号量。初始为空，需先 `semaphore_give` 一次才能 `take`。
///
/// 对应 `xSemaphoreCreateBinary`。`sem` 指向的 [`queue::Queue`] 须静态分配。
pub unsafe fn semaphore_create_binary(sem: QueueHandle) -> QueueHandle {
    unsafe { queue::queue_create(sem, ptr::null_mut(), 1, 0, QUEUE_TYPE_BINARY_SEMAPHORE) }
}

/// 创建计数信号量。
///
/// `max_count` 为上限，`initial_count` 为初始计数（<= `max_count`）。
/// 对应 `xSemaphoreCreateCounting`。
pub unsafe fn semaphore_create_counting(
    sem: QueueHandle,
    max_count: usize,
    initial_count: usize,
) -> QueueHandle {
    unsafe {
        debug_assert!(initial_count <= max_count);
        let h = queue::queue_create(sem, ptr::null_mut(), max_count, 0, QUEUE_TYPE_COUNTING_SEMAPHORE);
        (*sem).messages_waiting = initial_count;
        h
    }
}

/// 释放信号量（计数 +1），对应 `xSemaphoreGive`。
pub fn semaphore_give(sem: QueueHandle, timeout: TickType) -> Result<(), QueueError> {
    queue::queue_send(sem, ptr::null(), timeout)
}

/// 获取信号量（计数 -1），对应 `xSemaphoreTake`。
pub fn semaphore_take(sem: QueueHandle, timeout: TickType) -> Result<(), QueueError> {
    queue::queue_receive(sem, ptr::null_mut(), timeout)
}

/// 从中断释放信号量。返回 `true` 表示应触发上下文切换。
pub unsafe fn semaphore_give_from_isr(sem: QueueHandle) -> bool {
    unsafe { queue::queue_send_from_isr(sem, ptr::null()) }
}

/// 从中断获取信号量。返回 `true` 表示应触发上下文切换。
pub unsafe fn semaphore_take_from_isr(sem: QueueHandle) -> bool {
    unsafe { queue::queue_receive_from_isr(sem, ptr::null_mut()) }
}

/// 当前计数。
pub fn semaphore_count(sem: QueueHandle) -> usize {
    queue::queue_messages_waiting(sem)
}
