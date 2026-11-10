//! 互斥锁（带优先级继承）。
//!
//! 对应 FreeRTOS `semphr.h` 中的互斥量：以“项大小为 0、长度为 1 的队列”实现，
//! 并维护持有者以支持优先级继承。

use core::ptr;

use crate::config::{self, Priority, TaskHandle, TickType};
use crate::queue::{self, Queue, QueueError, QueueHandle, QUEUE_TYPE_MUTEX};
use crate::port;
use crate::scheduler::{self, TimeOut};
use crate::task;

/// 创建互斥锁。初始可用（持有者为空）。
///
/// 对应 `xSemaphoreCreateMutex`。
pub unsafe fn mutex_create(mutex: QueueHandle) -> QueueHandle {
    unsafe {
        let h = queue::queue_create(mutex, ptr::null_mut(), 1, 0, QUEUE_TYPE_MUTEX);
        (*mutex).messages_waiting = 1; // 初始可用
        (*mutex).holder = ptr::null_mut();
        h
    }
}

/// 优先级继承：把 `holder` 的优先级提升到 `prio`（若更低），并沿互斥锁链递归。
unsafe fn inherit_priority(holder: TaskHandle, prio: Priority) {
    unsafe {
        let mut h = holder;
        loop {
            if (*h).priority >= prio {
                break;
            }
            let old = (*h).priority;
            (*h).priority = prio;
            (*h).state_item.item_value = prio as u32;
            // 若任务当前就绪，迁移到新优先级就绪列表
            scheduler::reposition_ready(h, old);

            // 若 holder 本身阻塞在另一个互斥锁上，继续向上传播
            let blocked_on = (*h).blocked_on;
            if blocked_on.is_null() {
                break;
            }
            let q = blocked_on as *mut Queue;
            if (*q).queue_type == QUEUE_TYPE_MUTEX && !(*q).holder.is_null() {
                h = (*q).holder;
            } else {
                break;
            }
        }
    }
}

/// 取消继承：持有锁数量归零时恢复基本优先级。
unsafe fn disinherit_priority(t: TaskHandle) {
    unsafe {
        if (*t).mutexes_held > 0 {
            return;
        }
        if (*t).priority != (*t).base_priority {
            let old = (*t).priority;
            (*t).priority = (*t).base_priority;
            (*t).state_item.item_value = (*t).base_priority as u32;
            scheduler::reposition_ready(t, old);
        }
    }
}

/// 加锁，对应 `xSemaphoreTake`（互斥锁版）。
pub fn mutex_lock(mutex: QueueHandle, timeout: TickType) -> Result<(), QueueError> {
    unsafe {
        let mut to = TimeOut::new();
        let mut remaining = timeout;
        if timeout != 0 && timeout != config::INFINITE {
            scheduler::set_timeout_state(&mut to);
        }

        loop {
            port::enter_critical();
            if !queue::queue_is_empty(mutex) {
                // 可用：取走并登记持有者
                queue::copy_data_from_queue(mutex, ptr::null_mut());
                let cur = task::current_task();
                (*mutex).holder = cur;
                (*cur).mutexes_held += 1;
                port::exit_critical();
                return Ok(());
            }

            if timeout == 0 {
                port::exit_critical();
                return Err(QueueError::Empty);
            }
            if scheduler::check_timeout(&mut to, &mut remaining) {
                port::exit_critical();
                return Err(QueueError::Timeout);
            }

            // 阻塞前：优先级继承
            let holder = (*mutex).holder;
            if !holder.is_null() {
                inherit_priority(holder, (*task::current_task()).priority);
            }
            (*task::current_task()).blocked_on = mutex as *mut ();
            scheduler::block_current(&mut (*mutex).receivers, remaining);
            port::exit_critical();
            port::yield_from_task();
        }
    }
}

/// 解锁，对应 `xSemaphoreGive`（互斥锁版）。
pub fn mutex_unlock(mutex: QueueHandle) -> Result<(), QueueError> {
    unsafe {
        port::enter_critical();
        let cur = task::current_task();
        if (*mutex).holder != cur {
            port::exit_critical();
            return Err(QueueError::NotOwner);
        }

        (*cur).mutexes_held -= 1;
        (*mutex).holder = ptr::null_mut();
        disinherit_priority(cur);

        // 归还：计数 +1，并唤醒等待者
        queue::copy_data_to_queue(mutex, ptr::null_mut());
        let mut yield_needed = false;
        if !(*mutex).receivers.is_empty() {
            yield_needed = scheduler::remove_from_event_list(&mut (*mutex).receivers);
        }
        port::exit_critical();
        if yield_needed {
            port::yield_from_task();
        }
        Ok(())
    }
}

/// 查询互斥锁当前持有者（无则为空指针）。
pub fn mutex_holder(mutex: QueueHandle) -> TaskHandle {
    unsafe { (*mutex).holder }
}

// =====================================================================
// Rust 风格互斥锁：`Mutex<T>` —— 锁与被保护数据一体，RAII 守卫自动解锁
// =====================================================================

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// Rust 风格的互斥锁：内置互斥量控制块与被保护的数据 `T`。
///
/// 与 [`std::sync::Mutex`] 语义一致（底层仍使用内核的优先级继承互斥量）：
/// ```ignore
/// static CNT: kernel::Mutex<u32> = kernel::Mutex::new(0);
///
/// fn bump() {
///     let mut g = CNT.lock();   // 阻塞获取
///     *g = g.wrapping_add(1);   // DerefMut 直接读写数据
/// }                              // 离开作用域自动解锁（不可忘记）
/// ```
///
/// 使用普通 `static`（而非 `static mut`）即可：数据通过 [`UnsafeCell`] 在锁保护下
/// 访问，读写被保护值无需 `unsafe`。
pub struct Mutex<T> {
    /// 底层互斥量（项大小 0、长度 1 的队列 + 持有者 + 优先级继承）。
    inner: Queue,
    /// 被保护的数据。
    data: UnsafeCell<T>,
    /// 内部互斥量是否已初始化（首次加锁时在临界区内完成）。
    initialised: AtomicBool,
}

impl<T> Mutex<T> {
    /// 以初始值构造互斥锁（`const`，可用于 `static` 初始化）。
    pub const fn new(value: T) -> Self {
        Self {
            inner: Queue::empty(),
            data: UnsafeCell::new(value),
            initialised: AtomicBool::new(false),
        }
    }

    /// 惰性初始化内部互斥量（线程/中断安全，仅执行一次）。
    fn ensure_init(&self) {
        if !self.initialised.load(Ordering::Acquire) {
            unsafe {
                port::enter_critical();
                if !self.initialised.load(Ordering::Relaxed) {
                    mutex_create(&self.inner as *const Queue as *mut Queue);
                    self.initialised.store(true, Ordering::Release);
                }
                port::exit_critical();
            }
        }
    }

    #[inline]
    fn handle(&self) -> QueueHandle {
        &self.inner as *const Queue as *mut Queue
    }

    /// 阻塞获取锁（无限等待），返回 RAII 守卫。
    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.ensure_init();
        let _ = mutex_lock(self.handle(), config::INFINITE);
        MutexGuard { mutex: self }
    }

    /// 带超时的获取：`timeout` 个节拍内未获得则返回 `None`。
    pub fn lock_timeout(&self, timeout: TickType) -> Option<MutexGuard<'_, T>> {
        self.ensure_init();
        match mutex_lock(self.handle(), timeout) {
            Ok(()) => Some(MutexGuard { mutex: self }),
            Err(_) => None,
        }
    }

    /// 尝试获取（不阻塞）：立即成功返回守卫，否则 `None`。
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.ensure_init();
        match mutex_lock(self.handle(), 0) {
            Ok(()) => Some(MutexGuard { mutex: self }),
            Err(_) => None,
        }
    }

    /// 是否已被初始化（调试用）。
    pub fn is_initialised(&self) -> bool {
        self.initialised.load(Ordering::Acquire)
    }
}

/// 互斥锁 RAII 守卫：`Deref/DerefMut` 访问数据，`Drop` 自动解锁。
pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        // 持有者必然是本任务；失败仅可能源于误用，忽略即可
        let _ = mutex_unlock(self.mutex.handle());
    }
}

// `Mutex<T>` 内含裸指针（Queue），手工声明线程安全语义：
// 同一时刻只有一个任务能通过守卫访问数据。
unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}
