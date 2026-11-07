//! 事件组（Event Group）。
//!
//! 对应 FreeRTOS `event_groups.c`：一组可独立置位/清零的事件位，
//! 任务可等待任意位（OR）或全部位（AND）被置位。

use core::ptr;

use crate::config::{MAX_TASKS, TaskHandle, TickType};
use crate::list::{List, ListItem};
use crate::port;
use crate::scheduler::{self, TimeOut};

/// 事件位共 24 位可用（高 8 位保留给控制标志），对应 FreeRTOS `eventEVENT_BITS_CONTROL_BYTES`。
pub const EVENT_BITS_MASK: u32 = 0x00FF_FFFF;

const CLEAR_ON_EXIT_BIT: u32 = 0x0100_0000;
const WAIT_FOR_ALL_BIT: u32 = 0x0400_0000;

/// 事件组控制块，对应 FreeRTOS `EventGroup_t`。
pub struct EventGroup {
    pub event_bits: u32,
    pub waiters: List,
}

pub type EventGroupHandle = *mut EventGroup;

impl EventGroup {
    pub const fn empty() -> Self {
        Self {
            event_bits: 0,
            waiters: List::new(),
        }
    }
}

/// 创建事件组，对应 `xEventGroupCreate`。
pub unsafe fn event_group_create(eg: EventGroupHandle) -> EventGroupHandle {
    unsafe {
        (*eg).event_bits = 0;
        (*eg).waiters.initialise();
        eg
    }
}

/// 判断等待条件是否满足，对应 FreeRTOS `prvTestWaitCondition`。
fn test_wait_condition(current: u32, wait: u32, wait_all: bool) -> bool {
    if wait_all {
        (current & wait) == wait
    } else {
        (current & wait) != 0
    }
}

/// 唤醒所有满足条件的事件组等待者。返回 `true` 表示需要切换。
unsafe fn unblock_matching_waiters(eg: EventGroupHandle) -> bool {
    unsafe {
        let mut yield_needed = false;
        let mut matches: [TaskHandle; MAX_TASKS] = [ptr::null_mut(); MAX_TASKS];
        let mut n = 0usize;

        // 先收集匹配项（避免迭代中修改链表）
        let end = &mut (*eg).waiters.end as *mut ListItem;
        let mut item = (*end).next;
        while item != end {
            let t = (*item).owner as TaskHandle;
            let ctrl = (*item).item_value;
            let wait_bits = ctrl & EVENT_BITS_MASK;
            let wait_all = (ctrl & WAIT_FOR_ALL_BIT) != 0;
            if test_wait_condition((*eg).event_bits, wait_bits, wait_all) && n < MAX_TASKS {
                matches[n] = t;
                n += 1;
            }
            item = (*item).next;
        }

        for i in 0..n {
            let t = matches[i];
            List::remove(&mut (*t).event_item);
            (*t).blocked_on = ptr::null_mut();
            List::remove(&mut (*t).state_item);
            scheduler::add_to_ready_list(t);
            if (*t).priority > (*scheduler::current()).priority {
                yield_needed = true;
            }
        }
        yield_needed
    }
}

/// 置位事件位，返回新的位值，对应 `xEventGroupSetBits`。
pub fn event_group_set_bits(eg: EventGroupHandle, bits: u32) -> u32 {
    unsafe {
        port::enter_critical();
        (*eg).event_bits |= bits;
        let yield_needed = unblock_matching_waiters(eg);
        let ret = (*eg).event_bits;
        port::exit_critical();
        if yield_needed {
            port::yield_from_task();
        }
        ret
    }
}

/// 从中断置位事件位。返回 `true` 表示应触发上下文切换。
pub unsafe fn event_group_set_bits_from_isr(eg: EventGroupHandle, bits: u32) -> bool {
    unsafe {
        port::enter_critical();
        (*eg).event_bits |= bits;
        let yield_needed = unblock_matching_waiters(eg);
        port::exit_critical();
        yield_needed
    }
}

/// 清零事件位，返回新的位值，对应 `xEventGroupClearBits`。
pub fn event_group_clear_bits(eg: EventGroupHandle, bits: u32) -> u32 {
    unsafe {
        port::enter_critical();
        (*eg).event_bits &= !bits;
        let ret = (*eg).event_bits;
        port::exit_critical();
        ret
    }
}

/// 读取当前事件位，对应 `xEventGroupGetBits`。
pub fn event_group_get_bits(eg: EventGroupHandle) -> u32 {
    unsafe { (*eg).event_bits }
}

/// 等待事件位，对应 `xEventGroupWaitBits`。
///
/// - `wait_all == false`：任一位置位即返回（OR）；
/// - `wait_all == true`：所有位都置位才返回（AND）；
/// - `clear_on_exit == true`：返回前清零所等待的位；
/// - 返回值为触发时的位值；超时返回 0。
pub fn event_group_wait_bits(
    eg: EventGroupHandle,
    bits: u32,
    clear_on_exit: bool,
    wait_all: bool,
    timeout: TickType,
) -> u32 {
    unsafe {
        let mut to = TimeOut::new();
        let mut remaining = timeout;
        let mut entry_set = false;

        loop {
            port::enter_critical();
            let current = (*eg).event_bits;
            if test_wait_condition(current, bits, wait_all) {
                let ret = current;
                if clear_on_exit {
                    (*eg).event_bits &= !bits;
                }
                // 唤醒其他可能也满足条件的等待者
                let yield_needed = unblock_matching_waiters(eg);
                port::exit_critical();
                if yield_needed {
                    port::yield_from_task();
                }
                return ret;
            }

            if timeout == 0 {
                port::exit_critical();
                return 0;
            }
            if !entry_set {
                scheduler::set_timeout_state(&mut to);
                entry_set = true;
            }
            if scheduler::check_timeout(&mut to, &mut remaining) {
                port::exit_critical();
                return 0;
            }

            let ctrl = (bits & EVENT_BITS_MASK)
                | if clear_on_exit { CLEAR_ON_EXIT_BIT } else { 0 }
                | if wait_all { WAIT_FOR_ALL_BIT } else { 0 };

            scheduler::block_current_value(&mut (*eg).waiters, ctrl, remaining);
            port::exit_critical();
            port::yield_from_task();
        }
    }
}
