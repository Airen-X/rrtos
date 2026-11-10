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

// =====================================================================
// Rust 风格消息队列：`StaticQueue<T, N>` —— 类型安全 + 静态存储 + 方法调用
// =====================================================================

use core::cell::UnsafeCell;
use core::mem::{size_of, MaybeUninit};
use core::sync::atomic::{AtomicBool, Ordering};

/// Rust 风格的静态消息队列：`StaticQueue<T, N>`（容量 `N` 个 `T`）。
///
/// 普通 `static` 即可使用，方法式收发（元素按原始字节环形拷贝，无 `Copy` 要求）：
/// ```ignore
/// static Q: kernel::StaticQueue<u32, 4> = kernel::StaticQueue::new();
///
/// Q.send(42);                       // 阻塞直到有空间
/// if let Ok(v) = Q.try_recv() { }   // 非阻塞接收
/// ```
pub struct StaticQueue<T, const N: usize> {
    inner: Queue,
    buf: UnsafeCell<MaybeUninit<[T; N]>>,
    initialised: AtomicBool,
}

impl<T, const N: usize> StaticQueue<T, N> {
    /// 构造空队列（`const`，可用于 `static` 初始化；首次使用时惰性初始化）。
    pub const fn new() -> Self {
        Self {
            inner: Queue::empty(),
            buf: UnsafeCell::new(MaybeUninit::uninit()),
            initialised: AtomicBool::new(false),
        }
    }

    #[inline]
    fn handle(&self) -> QueueHandle {
        &self.inner as *const Queue as *mut Queue
    }

    /// 惰性初始化（线程/中断安全，仅执行一次）。
    fn ensure_init(&self) {
        if !self.initialised.load(Ordering::Acquire) {
            unsafe {
                port::enter_critical();
                if !self.initialised.load(Ordering::Relaxed) {
                    queue_create(
                        self.handle(),
                        self.buf.get() as *mut u8,
                        N,
                        size_of::<T>(),
                        QUEUE_TYPE_BASE,
                    );
                    self.initialised.store(true, Ordering::Release);
                }
                port::exit_critical();
            }
        }
    }

    /// 发送（阻塞直到有空间）。
    pub fn send(&self, item: T) -> Result<(), QueueError> {
        self.ensure_init();
        queue_send(self.handle(), &item as *const T as *const u8, config::INFINITE)
    }

    /// 非阻塞发送。
    pub fn try_send(&self, item: T) -> Result<(), QueueError> {
        self.ensure_init();
        queue_send(self.handle(), &item as *const T as *const u8, 0)
    }

    /// 带超时的发送（`timeout` 个节拍，`config::INFINITE` 表示无限）。
    pub fn send_timeout(&self, item: T, timeout: TickType) -> Result<(), QueueError> {
        self.ensure_init();
        queue_send(self.handle(), &item as *const T as *const u8, timeout)
    }

    /// 接收（阻塞直到有数据）。
    pub fn recv(&self) -> Result<T, QueueError> {
        self.ensure_init();
        unsafe {
            let mut slot = MaybeUninit::<T>::uninit();
            queue_receive(self.handle(), slot.as_mut_ptr() as *mut u8, config::INFINITE)
                .map(|()| slot.assume_init())
        }
    }

    /// 非阻塞接收。
    pub fn try_recv(&self) -> Result<T, QueueError> {
        self.ensure_init();
        unsafe {
            let mut slot = MaybeUninit::<T>::uninit();
            queue_receive(self.handle(), slot.as_mut_ptr() as *mut u8, 0)
                .map(|()| slot.assume_init())
        }
    }

    /// 带超时的接收。
    pub fn recv_timeout(&self, timeout: TickType) -> Result<T, QueueError> {
        self.ensure_init();
        unsafe {
            let mut slot = MaybeUninit::<T>::uninit();
            queue_receive(self.handle(), slot.as_mut_ptr() as *mut u8, timeout)
                .map(|()| slot.assume_init())
        }
    }

    /// 当前消息数。
    pub fn len(&self) -> usize {
        queue_messages_waiting(self.handle())
    }

    /// 容量。
    pub const fn capacity(&self) -> usize {
        N
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 是否已满。
    pub fn is_full(&self) -> bool {
        self.len() >= N
    }

    /// 是否已初始化（调试用）。
    pub fn is_initialised(&self) -> bool {
        self.initialised.load(Ordering::Acquire)
    }
}

// 内部含裸指针（Queue），手工声明线程安全语义：队列自身在临界区内串行化访问。
unsafe impl<T: Send, const N: usize> Send for StaticQueue<T, N> {}
unsafe impl<T: Send, const N: usize> Sync for StaticQueue<T, N> {}
