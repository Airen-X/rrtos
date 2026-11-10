//! 信号量（二值 / 计数）。
//!
//! 对应 FreeRTOS `semphr.h`：以“项大小为 0 的队列”实现。

use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::config::{self, TickType};
use crate::port;
use crate::queue::{
    self, Queue, QueueError, QueueHandle, QUEUE_TYPE_BINARY_SEMAPHORE,
    QUEUE_TYPE_COUNTING_SEMAPHORE,
};

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

// =====================================================================
// Rust 风格信号量：`BinarySemaphore` / `CountingSemaphore<N>` + RAII 许可
// =====================================================================

/// 信号量许可（RAII）：`acquire` 返回，离开作用域自动 `release`。
pub struct SemaphorePermit {
    handle: QueueHandle,
}

impl Drop for SemaphorePermit {
    fn drop(&mut self) {
        // 计数满时 give 会失败，忽略即可
        let _ = semaphore_give(self.handle, 0);
    }
}

/// Rust 风格二值信号量（初始为空，须先 `release` 一次才能 `acquire`）。
///
/// ```ignore
/// static SEM: kernel::BinarySemaphore = kernel::BinarySemaphore::new();
///
/// SEM.release();                    // 释放（计数 +1）
/// let p = SEM.acquire();            // 阻塞获取，返回 RAII 许可
/// // ... 使用受保护资源 ...
///                                   // 离开作用域自动 release
/// ```
pub struct BinarySemaphore {
    inner: Queue,
    initialised: AtomicBool,
}

impl BinarySemaphore {
    pub const fn new() -> Self {
        Self {
            inner: Queue::empty(),
            initialised: AtomicBool::new(false),
        }
    }

    #[inline]
    fn handle(&self) -> QueueHandle {
        &self.inner as *const Queue as *mut Queue
    }

    fn ensure_init(&self) {
        if !self.initialised.load(Ordering::Acquire) {
            unsafe {
                port::enter_critical();
                if !self.initialised.load(Ordering::Relaxed) {
                    queue::queue_create(self.handle(), ptr::null_mut(), 1, 0, QUEUE_TYPE_BINARY_SEMAPHORE);
                    self.initialised.store(true, Ordering::Release);
                }
                port::exit_critical();
            }
        }
    }

    /// 阻塞获取，返回 RAII 许可。
    pub fn acquire(&self) -> SemaphorePermit {
        self.ensure_init();
        let _ = semaphore_take(self.handle(), config::INFINITE);
        SemaphorePermit { handle: self.handle() }
    }

    /// 非阻塞获取。
    pub fn try_acquire(&self) -> Option<SemaphorePermit> {
        self.ensure_init();
        semaphore_take(self.handle(), 0)
            .ok()
            .map(|()| SemaphorePermit { handle: self.handle() })
    }

    /// 带超时获取（`timeout` 个节拍）。
    pub fn acquire_timeout(&self, timeout: TickType) -> Option<SemaphorePermit> {
        self.ensure_init();
        semaphore_take(self.handle(), timeout)
            .ok()
            .map(|()| SemaphorePermit { handle: self.handle() })
    }

    /// 释放（计数 +1，可从中断调用）。
    pub fn release(&self) -> Result<(), QueueError> {
        self.ensure_init();
        semaphore_give(self.handle(), 0)
    }

    /// 当前计数（0 或 1）。
    pub fn count(&self) -> usize {
        semaphore_count(self.handle())
    }
}

/// Rust 风格计数信号量（容量 `N`，初始计数 0）。
pub struct CountingSemaphore<const N: usize> {
    inner: Queue,
    initialised: AtomicBool,
}

impl<const N: usize> CountingSemaphore<N> {
    pub const fn new() -> Self {
        Self {
            inner: Queue::empty(),
            initialised: AtomicBool::new(false),
        }
    }

    #[inline]
    fn handle(&self) -> QueueHandle {
        &self.inner as *const Queue as *mut Queue
    }

    fn ensure_init(&self) {
        if !self.initialised.load(Ordering::Acquire) {
            unsafe {
                port::enter_critical();
                if !self.initialised.load(Ordering::Relaxed) {
                    queue::queue_create(self.handle(), ptr::null_mut(), N, 0, QUEUE_TYPE_COUNTING_SEMAPHORE);
                    self.initialised.store(true, Ordering::Release);
                }
                port::exit_critical();
            }
        }
    }

    /// 阻塞获取，返回 RAII 许可。
    pub fn acquire(&self) -> SemaphorePermit {
        self.ensure_init();
        let _ = semaphore_take(self.handle(), config::INFINITE);
        SemaphorePermit { handle: self.handle() }
    }

    /// 非阻塞获取。
    pub fn try_acquire(&self) -> Option<SemaphorePermit> {
        self.ensure_init();
        semaphore_take(self.handle(), 0)
            .ok()
            .map(|()| SemaphorePermit { handle: self.handle() })
    }

    /// 带超时获取。
    pub fn acquire_timeout(&self, timeout: TickType) -> Option<SemaphorePermit> {
        self.ensure_init();
        semaphore_take(self.handle(), timeout)
            .ok()
            .map(|()| SemaphorePermit { handle: self.handle() })
    }

    /// 释放（计数 +1，可从中断调用）。
    pub fn release(&self) -> Result<(), QueueError> {
        self.ensure_init();
        semaphore_give(self.handle(), 0)
    }

    /// 当前计数。
    pub fn count(&self) -> usize {
        semaphore_count(self.handle())
    }

    /// 容量。
    pub const fn capacity(&self) -> usize {
        N
    }
}

unsafe impl Send for BinarySemaphore {}
unsafe impl Sync for BinarySemaphore {}
unsafe impl<const N: usize> Send for CountingSemaphore<N> {}
unsafe impl<const N: usize> Sync for CountingSemaphore<N> {}
