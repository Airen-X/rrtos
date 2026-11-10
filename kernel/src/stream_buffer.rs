//! 流缓冲与消息缓冲。
//!
//! 对应 FreeRTOS `stream_buffer.c` 的 `xStreamBuffer*` 与 `xMessageBuffer*`。
//! - 流缓冲：面向字节流，带触发级别（达到后唤醒接收者）；
//! - 消息缓冲：在流缓冲之上增加 4 字节消息长度头，可跨多次调用读取一条消息。

use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::config::{INFINITE, TickType};
use crate::list::List;
use crate::port;
use crate::scheduler::{self, TimeOut};

/// 流缓冲控制块，对应 FreeRTOS `StreamBuffer_t`（简化）。
pub struct StreamBuffer {
    /// 数据存储区（`size` 字节）。
    pub storage: *mut u8,
    /// 缓冲总大小（字节）。
    pub size: usize,
    /// 触发级别：可用字节数达到该值才唤醒接收者。
    pub trigger_level: usize,
    /// 环形读指针。
    read_idx: usize,
    /// 环形写指针。
    write_idx: usize,
    /// 当前可用字节数。
    bytes: usize,
    /// 等待发送空间的任务。
    pub senders: List,
    /// 等待数据到达的任务。
    pub receivers: List,
}

pub type StreamBufferHandle = *mut StreamBuffer;

impl StreamBuffer {
    pub const fn empty() -> Self {
        Self {
            storage: ptr::null_mut(),
            size: 0,
            trigger_level: 1,
            read_idx: 0,
            write_idx: 0,
            bytes: 0,
            senders: List::new(),
            receivers: List::new(),
        }
    }
}

/// 创建（初始化）流缓冲。
///
/// # Safety
/// `storage` 必须指向至少 `size` 字节、在缓冲生命周期内有效的可写内存。
pub unsafe fn stream_buffer_create(
    h: StreamBufferHandle,
    storage: *mut u8,
    size: usize,
    trigger_level: usize,
) -> StreamBufferHandle {
    unsafe {
        let b = &mut *h;
        b.storage = storage;
        b.size = size;
        b.trigger_level = if trigger_level == 0 { 1 } else { trigger_level };
        b.read_idx = 0;
        b.write_idx = 0;
        b.bytes = 0;
        b.senders.initialise();
        b.receivers.initialise();
        h
    }
}

/// 当前可用字节数，对应 `xStreamBufferBytesAvailable`。
pub fn stream_buffer_bytes(h: StreamBufferHandle) -> usize {
    unsafe { (*h).bytes }
}

/// 当前空闲字节数，对应 `xStreamBufferSpacesAvailable`。
pub fn stream_buffer_spaces(h: StreamBufferHandle) -> usize {
    unsafe { (*h).size - (*h).bytes }
}

/// 发送 `len` 字节（任务级，可阻塞），返回实际写入字节数。
pub fn stream_buffer_send(h: StreamBufferHandle, data: &[u8], timeout: TickType) -> usize {
    unsafe {
        let mut to = TimeOut::new();
        let mut remaining = timeout;
        let len = data.len();
        if timeout != 0 && timeout != INFINITE {
            scheduler::set_timeout_state(&mut to);
        }
        let mut written = 0usize;

        while written < len {
            port::enter_critical();
            let space = (*h).size - (*h).bytes;
            if space > 0 {
                let n = if space < len - written {
                    space
                } else {
                    len - written
                };
                let mut i = 0usize;
                while i < n {
                    *(*h).storage.add((*h).write_idx) = data[written + i];
                    (*h).write_idx += 1;
                    if (*h).write_idx >= (*h).size {
                        (*h).write_idx = 0;
                    }
                    i += 1;
                }
                (*h).bytes += n;
                written += n;

                // 达到触发级别则唤醒接收者
                let mut yield_needed = false;
                if (*h).bytes >= (*h).trigger_level && !(*h).receivers.is_empty() {
                    yield_needed = scheduler::remove_from_event_list(&mut (*h).receivers);
                }
                port::exit_critical();
                if yield_needed {
                    port::yield_from_task();
                }
                continue;
            }

            if timeout == 0 {
                port::exit_critical();
                return written;
            }
            if scheduler::check_timeout(&mut to, &mut remaining) {
                port::exit_critical();
                return written;
            }
            scheduler::block_current(&mut (*h).senders, remaining);
            port::exit_critical();
            port::yield_from_task();
        }
        written
    }
}

/// 接收最多 `len` 字节（任务级，可阻塞），返回实际读出字节数。
pub fn stream_buffer_receive(h: StreamBufferHandle, buf: &mut [u8], timeout: TickType) -> usize {
    unsafe {
        let len = buf.len();
        let mut to = TimeOut::new();
        let mut remaining = timeout;
        if timeout != 0 && timeout != INFINITE {
            scheduler::set_timeout_state(&mut to);
        }
        let mut read = 0usize;

        while read < len {
            port::enter_critical();
            if (*h).bytes > 0 {
                let n = if (*h).bytes < len - read {
                    (*h).bytes
                } else {
                    len - read
                };
                let mut i = 0usize;
                while i < n {
                    buf[read + i] = *(*h).storage.add((*h).read_idx);
                    (*h).read_idx += 1;
                    if (*h).read_idx >= (*h).size {
                        (*h).read_idx = 0;
                    }
                    i += 1;
                }
                (*h).bytes -= n;
                read += n;

                // 腾出空间后唤醒发送者
                let mut yield_needed = false;
                if !(*h).senders.is_empty() {
                    yield_needed = scheduler::remove_from_event_list(&mut (*h).senders);
                }
                port::exit_critical();
                if yield_needed {
                    port::yield_from_task();
                }
                continue;
            }

            if timeout == 0 {
                port::exit_critical();
                return read;
            }
            if scheduler::check_timeout(&mut to, &mut remaining) {
                port::exit_critical();
                return read;
            }
            scheduler::block_current(&mut (*h).receivers, remaining);
            port::exit_critical();
            port::yield_from_task();
        }
        read
    }
}

/// 从中断发送（不阻塞）。返回 `true` 表示应触发上下文切换。
pub unsafe fn stream_buffer_send_from_isr(h: StreamBufferHandle, data: &[u8]) -> bool {
    unsafe {
        let len = data.len();
        port::enter_critical();
        let mut yield_needed = false;
        let space = (*h).size - (*h).bytes;
        let n = if space < len { space } else { len };
        let mut i = 0usize;
        while i < n {
            *(*h).storage.add((*h).write_idx) = data[i];
            (*h).write_idx += 1;
            if (*h).write_idx >= (*h).size {
                (*h).write_idx = 0;
            }
            i += 1;
        }
        (*h).bytes += n;
        if (*h).bytes >= (*h).trigger_level && !(*h).receivers.is_empty() {
            yield_needed = scheduler::remove_from_event_list(&mut (*h).receivers);
        }
        port::exit_critical();
        yield_needed
    }
}

/// 从中断接收（不阻塞）。返回 `true` 表示应触发上下文切换。
pub unsafe fn stream_buffer_receive_from_isr(h: StreamBufferHandle, buf: &mut [u8]) -> bool {
    unsafe {
        let len = buf.len();
        port::enter_critical();
        let mut yield_needed = false;
        let n = if (*h).bytes < len { (*h).bytes } else { len };
        let mut i = 0usize;
        while i < n {
            buf[i] = *(*h).storage.add((*h).read_idx);
            (*h).read_idx += 1;
            if (*h).read_idx >= (*h).size {
                (*h).read_idx = 0;
            }
            i += 1;
        }
        (*h).bytes -= n;
        if !(*h).senders.is_empty() {
            yield_needed = scheduler::remove_from_event_list(&mut (*h).senders);
        }
        port::exit_critical();
        yield_needed
    }
}

// =====================================================================
// 消息缓冲（流缓冲 + 4 字节长度头）—— 原子化版本
// =====================================================================

/// 消息长度头字节数，对应 FreeRTOS `sbBYTES_TO_STORE_MESSAGE_LENGTH`。
pub const MESSAGE_LENGTH_BYTES: usize = 4;

/// 消息缓冲控制块，对应 FreeRTOS `MessageBuffer_t`（简化）。
pub struct MessageBuffer {
    /// 底层流缓冲。
    pub stream: StreamBuffer,
}

pub type MessageBufferHandle = *mut MessageBuffer;

impl MessageBuffer {
    pub const fn empty() -> Self {
        Self {
            stream: StreamBuffer::empty(),
        }
    }
}

/// 创建消息缓冲（`size` 为数据区总大小，需 >= 长度头 4 字节 + 1）。
pub unsafe fn message_buffer_create(
    h: MessageBufferHandle,
    storage: *mut u8,
    size: usize,
) -> MessageBufferHandle {
    unsafe {
        let b = &mut *h;
        // trigger_level 用 1：消息缓冲不需要按字节攒批，唤醒由"完整消息可达"判断。
        stream_buffer_create(&mut b.stream, storage, size, 1);
        h
    }
}

/// 在持有临界区的前提下，向流缓冲写入至多 `n` 字节（不阻塞、不失败，n <= 剩余空间）。
/// 调用者必须保证 `n <= size - bytes`，且已进入临界区。
unsafe fn write_bytes_locked(h: StreamBufferHandle, data: &[u8]) {
    unsafe {
        let n = data.len();
        let mut i = 0usize;
        while i < n {
            *(*h).storage.add((*h).write_idx) = data[i];
            (*h).write_idx += 1;
            if (*h).write_idx >= (*h).size {
                (*h).write_idx = 0;
            }
            i += 1;
        }
        (*h).bytes += n;
    }
}

/// 在持有临界区的前提下，从流缓冲读出至多 `n` 字节（不阻塞、不失败，n <= bytes）。
/// 调用者必须保证 `n <= bytes`，且已进入临界区。
unsafe fn read_bytes_locked(h: StreamBufferHandle, buf: &mut [u8]) {
    unsafe {
        let n = buf.len();
        let mut i = 0usize;
        while i < n {
            buf[i] = *(*h).storage.add((*h).read_idx);
            (*h).read_idx += 1;
            if (*h).read_idx >= (*h).size {
                (*h).read_idx = 0;
            }
            i += 1;
        }
        (*h).bytes -= n;
    }
}

/// 发送一条消息（长度头 + 数据），返回 `true` 成功 / `false` 失败（消息过大或超时）。
///
/// 原子性保证：
/// - 先等到 `4 + len` 连续空间，再在**单个临界区**内一次性写入头+数据；
/// - 写入路径无阻塞、无失败分支，不存在"头写了数据没写"的中间态。
pub fn message_buffer_send(h: MessageBufferHandle, data: &[u8], timeout: TickType) -> bool {
    unsafe {
        let len = data.len();
        let total = MESSAGE_LENGTH_BYTES + len;
        // 消息过大（含头都放不下）直接失败。
        if len == 0 || total > (*h).stream.size {
            return false;
        }

        let stream = h as *mut StreamBuffer;
        let mut to = TimeOut::new();
        let mut remaining = timeout;
        if timeout != 0 && timeout != INFINITE {
            scheduler::set_timeout_state(&mut to);
        }
        let len_bytes = (len as u32).to_le_bytes();

        // 阶段一：等待足够的连续空间（可阻塞 / 可超时）。
        loop {
            port::enter_critical();
            let space = (*stream).size - (*stream).bytes;
            if space >= total {
                // 阶段二：空间已足够，一次性写入头+数据（不可中断、无失败）。
                write_bytes_locked(stream, &len_bytes);
                write_bytes_locked(stream, data);

                // 唤醒接收者：完整消息已就绪（bytes >= trigger_level 恒成立，trigger_level=1）。
                let mut yield_needed = false;
                if !(*stream).receivers.is_empty() {
                    yield_needed = scheduler::remove_from_event_list(&mut (*stream).receivers);
                }
                port::exit_critical();
                if yield_needed {
                    port::yield_from_task();
                }
                return true;
            }

            // 空间不足：处理超时 / 阻塞。
            if timeout == 0 {
                port::exit_critical();
                return false;
            }
            if scheduler::check_timeout(&mut to, &mut remaining) {
                port::exit_critical();
                return false;
            }
            scheduler::block_current(&mut (*stream).senders, remaining);
            port::exit_critical();
            port::yield_from_task();
        }
    }
}

/// 接收一条消息，返回本次读出的字节数（0 表示超时 / 无消息）。
///
/// 原子性保证：
/// - 先确保"长度头 + 完整消息数据"都在缓冲里，再在**单个临界区**内一次性读头+数据；
/// - 不存在"头已消费、数据没读到"的歧义；返回 0 一定意味着没有任何字节被消费。
///
/// 注意：本版本一次性读出整条消息，`buf.len()` 必须 >= 消息长度，
/// 否则返回 0 且不消费任何数据（避免部分接收的状态机复杂度）。
pub fn message_buffer_receive(h: MessageBufferHandle, buf: &mut [u8], timeout: TickType) -> usize {
    unsafe {
        let stream = h as *mut StreamBuffer;
        let mut to = TimeOut::new();
        let mut remaining = timeout;
        if timeout != 0 && timeout != INFINITE {
            scheduler::set_timeout_state(&mut to);
        }

        loop {
            port::enter_critical();
            // 先看头是否可达：至少要有 4 字节才能读长度。
            if (*stream).bytes < MESSAGE_LENGTH_BYTES {
                // 头都不完整：进入等待。
                if timeout == 0 {
                    port::exit_critical();
                    return 0;
                }
                if scheduler::check_timeout(&mut to, &mut remaining) {
                    port::exit_critical();
                    return 0;
                }
                scheduler::block_current(&mut (*stream).receivers, remaining);
                port::exit_critical();
                port::yield_from_task();
                continue;
            }

            // 读长度头（只在临界区内读，但不消费——先 peek）。
            let mut len_bytes = [0u8; MESSAGE_LENGTH_BYTES];
            let mut idx = (*stream).read_idx;
            let mut i = 0usize;
            while i < MESSAGE_LENGTH_BYTES {
                len_bytes[i] = *(*stream).storage.add(idx);
                idx += 1;
                if idx >= (*stream).size {
                    idx = 0;
                }
                i += 1;
            }
            let msg_len = u32::from_le_bytes(len_bytes) as usize;

            // 数据是否完整到达？不完整则等待（头尚未消费，可安全重试）。
            if (*stream).bytes < MESSAGE_LENGTH_BYTES + msg_len {
                if timeout == 0 {
                    port::exit_critical();
                    return 0;
                }
                if scheduler::check_timeout(&mut to, &mut remaining) {
                    port::exit_critical();
                    return 0;
                }
                scheduler::block_current(&mut (*stream).receivers, remaining);
                port::exit_critical();
                port::yield_from_task();
                continue;
            }

            // 调用者缓冲是否足够？不足则返回 0 且不消费（避免部分接收歧义）。
            if buf.len() < msg_len {
                port::exit_critical();
                return 0;
            }

            // 完整消息已就绪，且在单个临界区内一次性消费头+数据。
            let mut head = [0u8; MESSAGE_LENGTH_BYTES];
            read_bytes_locked(stream, &mut head);
            read_bytes_locked(stream, &mut buf[..msg_len]);

            // 腾出空间，唤醒发送者。
            let mut yield_needed = false;
            if !(*stream).senders.is_empty() {
                yield_needed = scheduler::remove_from_event_list(&mut (*stream).senders);
            }
            port::exit_critical();
            if yield_needed {
                port::yield_from_task();
            }
            return msg_len;
        }
    }
}

// =====================================================================
// Rust 风格流/消息缓冲：`StaticStreamBuffer<N>` / `StaticMessageBuffer<N>`
// =====================================================================

/// Rust 风格静态流缓冲（容量 `N` 字节，普通 `static` 即可使用）。
///
/// ```ignore
/// static SB: kernel::StaticStreamBuffer<256> = kernel::StaticStreamBuffer::new();
///
/// let n = SB.write(b"hello");    // 阻塞写入全部
/// let mut buf = [0u8; 16];
/// let n = SB.read(&mut buf);     // 阻塞读取
/// ```
pub struct StaticStreamBuffer<const N: usize> {
    inner: StreamBuffer,
    buf: UnsafeCell<[u8; N]>,
    initialised: AtomicBool,
}

impl<const N: usize> StaticStreamBuffer<N> {
    pub const fn new() -> Self {
        Self {
            inner: StreamBuffer::empty(),
            buf: UnsafeCell::new([0u8; N]),
            initialised: AtomicBool::new(false),
        }
    }

    #[inline]
    fn handle(&self) -> StreamBufferHandle {
        &self.inner as *const StreamBuffer as *mut StreamBuffer
    }

    fn ensure_init(&self) {
        if !self.initialised.load(Ordering::Acquire) {
            unsafe {
                port::enter_critical();
                if !self.initialised.load(Ordering::Relaxed) {
                    stream_buffer_create(
                        self.handle(),
                        self.buf.get() as *mut u8,
                        N,
                        1,
                    );
                    self.initialised.store(true, Ordering::Release);
                }
                port::exit_critical();
            }
        }
    }

    /// 阻塞写入，直到全部写入（返回写入字节数，等于 `data.len()`）。
    pub fn write(&self, data: &[u8]) -> usize {
        self.ensure_init();
        stream_buffer_send(self.handle(), data, INFINITE)
    }

    /// 非阻塞写入（可能只写入一部分，返回实际写入字节数）。
    pub fn try_write(&self, data: &[u8]) -> usize {
        self.ensure_init();
        stream_buffer_send(self.handle(), data, 0)
    }

    /// 带超时写入。
    pub fn write_timeout(&self, data: &[u8], timeout: TickType) -> usize {
        self.ensure_init();
        stream_buffer_send(self.handle(), data, timeout)
    }

    /// 阻塞读取，直到 `buf` 填满（返回实际读出字节数）。
    pub fn read(&self, buf: &mut [u8]) -> usize {
        self.ensure_init();
        stream_buffer_receive(self.handle(), buf, INFINITE)
    }

    /// 非阻塞读取。
    pub fn try_read(&self, buf: &mut [u8]) -> usize {
        self.ensure_init();
        stream_buffer_receive(self.handle(), buf, 0)
    }

    /// 带超时读取。
    pub fn read_timeout(&self, buf: &mut [u8], timeout: TickType) -> usize {
        self.ensure_init();
        stream_buffer_receive(self.handle(), buf, timeout)
    }

    /// 当前可用字节数。
    pub fn available(&self) -> usize {
        stream_buffer_bytes(self.handle())
    }

    /// 当前空闲字节数。
    pub fn space(&self) -> usize {
        stream_buffer_spaces(self.handle())
    }

    /// 容量。
    pub const fn capacity(&self) -> usize {
        N
    }
}

/// Rust 风格静态消息缓冲（容量 `N` 字节，含 4 字节消息长度头）。
///
/// 注意：`recv` 一次性读出**整条**消息，`buf` 长度须 >= 消息长度，
/// 否则返回 0 且不消费任何数据。
///
/// ```ignore
/// static MB: kernel::StaticMessageBuffer<256> = kernel::StaticMessageBuffer::new();
///
/// MB.send(b"hello");              // 阻塞发送一条消息
/// let mut buf = [0u8; 32];
/// let n = MB.recv(&mut buf);      // 阻塞接收整条消息
/// ```
pub struct StaticMessageBuffer<const N: usize> {
    inner: MessageBuffer,
    buf: UnsafeCell<[u8; N]>,
    initialised: AtomicBool,
}

impl<const N: usize> StaticMessageBuffer<N> {
    pub const fn new() -> Self {
        Self {
            inner: MessageBuffer::empty(),
            buf: UnsafeCell::new([0u8; N]),
            initialised: AtomicBool::new(false),
        }
    }

    #[inline]
    fn handle(&self) -> MessageBufferHandle {
        &self.inner as *const MessageBuffer as *mut MessageBuffer
    }

    fn ensure_init(&self) {
        if !self.initialised.load(Ordering::Acquire) {
            unsafe {
                port::enter_critical();
                if !self.initialised.load(Ordering::Relaxed) {
                    message_buffer_create(self.handle(), self.buf.get() as *mut u8, N);
                    self.initialised.store(true, Ordering::Release);
                }
                port::exit_critical();
            }
        }
    }

    /// 阻塞发送一条消息；消息过长（> N-4）时返回 `false`。
    pub fn send(&self, data: &[u8]) -> bool {
        self.ensure_init();
        message_buffer_send(self.handle(), data, INFINITE)
    }

    /// 非阻塞发送。
    pub fn try_send(&self, data: &[u8]) -> bool {
        self.ensure_init();
        message_buffer_send(self.handle(), data, 0)
    }

    /// 带超时发送。
    pub fn send_timeout(&self, data: &[u8], timeout: TickType) -> bool {
        self.ensure_init();
        message_buffer_send(self.handle(), data, timeout)
    }

    /// 阻塞接收整条消息（`buf` 须 >= 消息长度，否则返回 0 且不消费）。
    pub fn recv(&self, buf: &mut [u8]) -> usize {
        self.ensure_init();
        message_buffer_receive(self.handle(), buf, INFINITE)
    }

    /// 非阻塞接收。
    pub fn try_recv(&self, buf: &mut [u8]) -> usize {
        self.ensure_init();
        message_buffer_receive(self.handle(), buf, 0)
    }

    /// 带超时接收。
    pub fn recv_timeout(&self, buf: &mut [u8], timeout: TickType) -> usize {
        self.ensure_init();
        message_buffer_receive(self.handle(), buf, timeout)
    }

    /// 容量。
    pub const fn capacity(&self) -> usize {
        N
    }
}

unsafe impl<const N: usize> Send for StaticStreamBuffer<N> {}
unsafe impl<const N: usize> Sync for StaticStreamBuffer<N> {}
unsafe impl<const N: usize> Send for StaticMessageBuffer<N> {}
unsafe impl<const N: usize> Sync for StaticMessageBuffer<N> {}
