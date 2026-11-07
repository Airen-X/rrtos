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
