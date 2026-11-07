//! 流缓冲与消息缓冲。
//!
//! 对应 FreeRTOS `stream_buffer.c` 的 `xStreamBuffer*` 与 `xMessageBuffer*`。
//! - 流缓冲：面向字节流，带触发级别（达到后唤醒接收者）；
//! - 消息缓冲：在流缓冲之上增加 4 字节消息长度头，可跨多次调用读取一条消息。

use core::ptr;

use crate::config::{TickType, INFINITE};
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
pub fn stream_buffer_send(
    h: StreamBufferHandle,
    data: *const u8,
    len: usize,
    timeout: TickType,
) -> usize {
    unsafe {
        let mut to = TimeOut::new();
        let mut remaining = timeout;
        if timeout != 0 && timeout != INFINITE {
            scheduler::set_timeout_state(&mut to);
        }
        let mut written = 0usize;

        while written < len {
            port::enter_critical();
            let space = (*h).size - (*h).bytes;
            if space > 0 {
                let n = if space < len - written { space } else { len - written };
                let mut i = 0usize;
                while i < n {
                    *(*h).storage.add((*h).write_idx) = *data.add(written + i);
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
pub fn stream_buffer_receive(
    h: StreamBufferHandle,
    buf: *mut u8,
    len: usize,
    timeout: TickType,
) -> usize {
    unsafe {
        let mut to = TimeOut::new();
        let mut remaining = timeout;
        if timeout != 0 && timeout != INFINITE {
            scheduler::set_timeout_state(&mut to);
        }
        let mut read = 0usize;

        while read < len {
            port::enter_critical();
            if (*h).bytes > 0 {
                let n = if (*h).bytes < len - read { (*h).bytes } else { len - read };
                let mut i = 0usize;
                while i < n {
                    *buf.add(read + i) = *(*h).storage.add((*h).read_idx);
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
pub unsafe fn stream_buffer_send_from_isr(h: StreamBufferHandle, data: *const u8, len: usize) -> bool {
    unsafe {
        port::enter_critical();
        let mut yield_needed = false;
        let space = (*h).size - (*h).bytes;
        let n = if space < len { space } else { len };
        let mut i = 0usize;
        while i < n {
            *(*h).storage.add((*h).write_idx) = *data.add(i);
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
pub unsafe fn stream_buffer_receive_from_isr(h: StreamBufferHandle, buf: *mut u8, len: usize) -> bool {
    unsafe {
        port::enter_critical();
        let mut yield_needed = false;
        let n = if (*h).bytes < len { (*h).bytes } else { len };
        let mut i = 0usize;
        while i < n {
            *buf.add(i) = *(*h).storage.add((*h).read_idx);
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
// 消息缓冲（流缓冲 + 4 字节长度头）
// =====================================================================

/// 消息长度头字节数，对应 FreeRTOS `sbBYTES_TO_STORE_MESSAGE_LENGTH`。
pub const MESSAGE_LENGTH_BYTES: usize = 4;

/// 消息缓冲控制块，对应 FreeRTOS `MessageBuffer_t`（简化）。
pub struct MessageBuffer {
    /// 底层流缓冲。
    pub stream: StreamBuffer,
    /// 正在接收的消息总长度（0 = 未开始读一条消息）。
    rx_len: usize,
    /// 正在接收的消息剩余字节数。
    rx_remaining: usize,
}

pub type MessageBufferHandle = *mut MessageBuffer;

impl MessageBuffer {
    pub const fn empty() -> Self {
        Self {
            stream: StreamBuffer::empty(),
            rx_len: 0,
            rx_remaining: 0,
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
        stream_buffer_create(&mut b.stream, storage, size, 1);
        b.rx_len = 0;
        b.rx_remaining = 0;
        h
    }
}

/// 发送一条消息（长度头 + 数据），返回 `true` 成功 / `false` 失败（消息过大或超时）。
///
/// 对应 `xMessageBufferSend`。
pub fn message_buffer_send(
    h: MessageBufferHandle,
    data: *const u8,
    len: usize,
    timeout: TickType,
) -> bool {
    unsafe {
        if len == 0 || len > (*h).stream.size - MESSAGE_LENGTH_BYTES {
            return false;
        }
        let len_bytes = (len as u32).to_le_bytes();
        if stream_buffer_send(h as *mut StreamBuffer, len_bytes.as_ptr(), MESSAGE_LENGTH_BYTES, timeout)
            != MESSAGE_LENGTH_BYTES
        {
            return false;
        }
        stream_buffer_send(h as *mut StreamBuffer, data, len, timeout) == len
    }
}

/// 接收一条消息（可跨多次调用读取），返回本次读出的字节数。
///
/// 对应 `xMessageBufferReceive`。第一次调用会先读出长度头并阻塞等待数据；
/// 之后每次调用最多返回 `len` 字节，直到本条消息读完。
pub fn message_buffer_receive(
    h: MessageBufferHandle,
    buf: *mut u8,
    len: usize,
    timeout: TickType,
) -> usize {
    unsafe {
        if (*h).rx_remaining == 0 {
            // 读长度头
            let mut len_bytes = [0u8; MESSAGE_LENGTH_BYTES];
            if stream_buffer_receive(
                h as *mut StreamBuffer,
                len_bytes.as_mut_ptr(),
                MESSAGE_LENGTH_BYTES,
                timeout,
            ) != MESSAGE_LENGTH_BYTES
            {
                return 0;
            }
            (*h).rx_len = u32::from_le_bytes(len_bytes) as usize;
            (*h).rx_remaining = (*h).rx_len;
        }

        let want = if len < (*h).rx_remaining { len } else { (*h).rx_remaining };
        let got = stream_buffer_receive(h as *mut StreamBuffer, buf, want, timeout);
        (*h).rx_remaining -= got;
        got
    }
}
