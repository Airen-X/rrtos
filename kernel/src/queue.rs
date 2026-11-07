//! 消息队列。
//!
//! 对应 FreeRTOS `queue.c`：环形缓冲区 + 发送/接收等待列表。
//! 同时作为信号量与互斥锁的底层实现基础。

use core::ptr;

use crate::config::{self, TaskHandle, TickType};
use crate::list::List;
use crate::port;
use crate::scheduler::{self, TimeOut};

/// 队列类型标识，对应 FreeRTOS `queueQUEUE_TYPE_*`。
pub const QUEUE_TYPE_BASE: u8 = 0;
pub const QUEUE_TYPE_MUTEX: u8 = 1;
pub const QUEUE_TYPE_COUNTING_SEMAPHORE: u8 = 2;
pub const QUEUE_TYPE_BINARY_SEMAPHORE: u8 = 3;

/// 队列控制块，对应 FreeRTOS `Queue_t`。
#[repr(C)]
pub struct Queue {
    /// 数据存储区（`length * item_size` 字节）。
    pub storage: *mut u8,
    /// 队列最大项数。
    pub length: usize,
    /// 每项字节数（信号量 / 互斥锁为 0）。
    pub item_size: usize,
    /// 环形缓冲区写指针。
    pub write_to: *mut u8,
    /// 环形缓冲区读指针。
    pub read_from: *mut u8,
    /// 当前消息数。
    pub messages_waiting: usize,
    pub queue_type: u8,
    /// 互斥锁持有者（仅互斥锁有效）。
    pub holder: TaskHandle,
    /// 等待接收的任务列表。
    pub receivers: List,
    /// 等待发送的任务列表。
    pub senders: List,
}

pub type QueueHandle = *mut Queue;

/// 队列操作结果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QueueError {
    /// 队列满（发送失败）。
    Full,
    /// 队列空（接收失败）。
    Empty,
    /// 等待超时。
    Timeout,
    /// 非持有者尝试释放互斥锁。
    NotOwner,
}

impl Queue {
    pub const fn empty() -> Self {
        Self {
            storage: ptr::null_mut(),
            length: 0,
            item_size: 0,
            write_to: ptr::null_mut(),
            read_from: ptr::null_mut(),
            messages_waiting: 0,
            queue_type: QUEUE_TYPE_BASE,
            holder: ptr::null_mut(),
            receivers: List::new(),
            senders: List::new(),
        }
    }
}

/// 把数据拷入队列（环形缓冲）。
pub(crate) unsafe fn copy_data_to_queue(q: QueueHandle, item: *const u8) {
    unsafe {
        let qref = &mut *q;
        if qref.item_size == 0 {
            // 信号量 / 互斥锁：无需拷贝，仅计数
            qref.messages_waiting += 1;
            return;
        }
        ptr::copy_nonoverlapping(item, qref.write_to, qref.item_size);
        qref.write_to = qref.write_to.add(qref.item_size);
        if qref.write_to >= qref.storage.add(qref.length * qref.item_size) {
            qref.write_to = qref.storage;
        }
        qref.messages_waiting += 1;
    }
}

/// 把数据拷出队列。
pub(crate) unsafe fn copy_data_from_queue(q: QueueHandle, buf: *mut u8) {
    unsafe {
        let qref = &mut *q;
        if qref.item_size == 0 {
            qref.messages_waiting -= 1;
            return;
        }
        ptr::copy_nonoverlapping(qref.read_from, buf, qref.item_size);
        qref.read_from = qref.read_from.add(qref.item_size);
        if qref.read_from >= qref.storage.add(qref.length * qref.item_size) {
            qref.read_from = qref.storage;
        }
        qref.messages_waiting -= 1;
    }
}

/// 创建（初始化）队列。
///
/// # Safety
/// - `q` 指向的 `Queue` 与 `storage` 指向的存储区必须在队列生命周期内有效；
/// - `storage` 至少 `length * item_size` 字节（`item_size == 0` 时可为空）。
pub unsafe fn queue_create(
    q: QueueHandle,
    storage: *mut u8,
    length: usize,
    item_size: usize,
    queue_type: u8,
) -> QueueHandle {
    unsafe {
        let qref = &mut *q;
        qref.storage = storage;
        qref.length = length;
        qref.item_size = item_size;
        qref.write_to = storage;
        qref.read_from = storage;
        qref.messages_waiting = 0;
        qref.queue_type = queue_type;
        qref.holder = ptr::null_mut();
        qref.receivers.initialise();
        qref.senders.initialise();
        q
    }
}

/// 队列是否满。
#[inline]
pub unsafe fn queue_is_full(q: QueueHandle) -> bool {
    unsafe { (*q).messages_waiting >= (*q).length }
}

/// 队列是否空。
#[inline]
pub unsafe fn queue_is_empty(q: QueueHandle) -> bool {
    unsafe { (*q).messages_waiting == 0 }
}

/// 发送消息（任务级，可阻塞）。
///
/// `timeout` 为等待节拍数，`config::INFINITE` 表示无限等待，0 表示不阻塞。
pub fn queue_send(q: QueueHandle, item: *const u8, timeout: TickType) -> Result<(), QueueError> {
    unsafe {
        let mut to = TimeOut::new();
        let mut remaining = timeout;
        if timeout != 0 && timeout != config::INFINITE {
            scheduler::set_timeout_state(&mut to);
        }

        loop {
            port::enter_critical();
            if !queue_is_full(q) {
                copy_data_to_queue(q, item);
                let mut yield_needed = false;
                if !(*q).receivers.is_empty() {
                    yield_needed = scheduler::remove_from_event_list(&mut (*q).receivers);
                }
                port::exit_critical();
                if yield_needed {
                    port::yield_from_task();
                }
                return Ok(());
            }

            if timeout == 0 {
                port::exit_critical();
                return Err(QueueError::Full);
            }

            if scheduler::check_timeout(&mut to, &mut remaining) {
                port::exit_critical();
                return Err(QueueError::Full);
            }

            scheduler::block_current(&mut (*q).senders, remaining);
            port::exit_critical();
            port::yield_from_task();
        }
    }
}

/// 接收消息（任务级，可阻塞）。
pub fn queue_receive(
    q: QueueHandle,
    buf: *mut u8,
    timeout: TickType,
) -> Result<(), QueueError> {
    unsafe {
        let mut to = TimeOut::new();
        let mut remaining = timeout;
        if timeout != 0 && timeout != config::INFINITE {
            scheduler::set_timeout_state(&mut to);
        }

        loop {
            port::enter_critical();
            if !queue_is_empty(q) {
                copy_data_from_queue(q, buf);
                let mut yield_needed = false;
                if !(*q).senders.is_empty() {
                    yield_needed = scheduler::remove_from_event_list(&mut (*q).senders);
                }
                port::exit_critical();
                if yield_needed {
                    port::yield_from_task();
                }
                return Ok(());
            }

            if timeout == 0 {
                port::exit_critical();
                return Err(QueueError::Empty);
            }

            if scheduler::check_timeout(&mut to, &mut remaining) {
                port::exit_critical();
                return Err(QueueError::Empty);
            }

            scheduler::block_current(&mut (*q).receivers, remaining);
            port::exit_critical();
            port::yield_from_task();
        }
    }
}

/// 从中断发送（不阻塞）。返回 `true` 表示应触发上下文切换。
pub unsafe fn queue_send_from_isr(q: QueueHandle, item: *const u8) -> bool {
    unsafe {
        port::enter_critical();
        let mut yield_needed = false;
        if !queue_is_full(q) {
            copy_data_to_queue(q, item);
            if !(*q).receivers.is_empty() {
                yield_needed = scheduler::remove_from_event_list(&mut (*q).receivers);
            }
        }
        port::exit_critical();
        yield_needed
    }
}

/// 从中断接收（不阻塞）。返回 `true` 表示应触发上下文切换。
pub unsafe fn queue_receive_from_isr(q: QueueHandle, buf: *mut u8) -> bool {
    unsafe {
        port::enter_critical();
        let mut yield_needed = false;
        if !queue_is_empty(q) {
            copy_data_from_queue(q, buf);
            if !(*q).senders.is_empty() {
                yield_needed = scheduler::remove_from_event_list(&mut (*q).senders);
            }
        }
        port::exit_critical();
        yield_needed
    }
}

/// 返回队列中当前消息数。
pub fn queue_messages_waiting(q: QueueHandle) -> usize {
    unsafe { (*q).messages_waiting }
}
