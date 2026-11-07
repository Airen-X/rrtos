//! 协程（Co-routine）。
//!
//! 对应 FreeRTOS `croutine.h`：比任务更轻量的协作式执行单元。
//! 简化实现为“显式状态机”：每个协程函数在每次被调度时运行一步，
//! 通过 `CoRoutine::delay` 挂起若干个节拍，由空闲任务中的
//! [`scheduler_tick`] 统一调度（无抢占、共享同一上下文）。

use core::ptr;

use crate::config::TickType;
use crate::list::{List, ListItem};
use crate::scheduler;

/// 协程状态。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CoRoutineState {
    Ready,
    Delayed,
    Suspended,
}

/// 协程函数：每次被调度时调用一次，内部用 `co.step` 等字段维持进度。
pub type CoRoutineFn = fn(&mut CoRoutine);

/// 协程控制块，对应 FreeRTOS `CoRoutine_t`（简化）。
pub struct CoRoutine {
    /// 就绪/延时列表项（item_value = 优先级编码或唤醒时刻）。
    pub item: ListItem,
    pub priority: u8,
    pub state: CoRoutineState,
    pub func: CoRoutineFn,
    /// 用户状态机进度（自由使用）。
    pub step: usize,
    /// 延时到何时（tick）。
    pub delay_until: TickType,
}

pub type CoRoutineHandle = *mut CoRoutine;

fn _noop_co(_: &mut CoRoutine) {}

impl CoRoutine {
    /// 构造一个“空白”协程，随后由 [`co_routine_create`] 填充。
    pub const fn empty() -> Self {
        Self {
            item: ListItem::new(),
            priority: 0,
            state: CoRoutineState::Suspended,
            func: _noop_co,
            step: 0,
            delay_until: 0,
        }
    }
}

static mut READY: List = List::new();
static mut DELAYED: List = List::new();

// Rust 2024 禁止对 `static mut` 取引用，统一走裸指针访问。
#[inline(always)]
unsafe fn ready_ptr() -> *mut List {
    ptr::addr_of_mut!(READY)
}

#[inline(always)]
unsafe fn delayed_ptr() -> *mut List {
    ptr::addr_of_mut!(DELAYED)
}

// 就绪列表按“优先级越高值越小”排序（0 = 最高优先级在最前）。
#[inline]
unsafe fn add_ready(co: CoRoutineHandle) {
    unsafe {
        (*co).item.item_value = 255 - (*co).priority as u32;
        (*ready_ptr()).insert(&mut (*co).item);
    }
}

/// 初始化协程子系统（由调度器启动时调用）。
pub(crate) unsafe fn init() {
    unsafe {
        (*ready_ptr()).initialise();
        (*delayed_ptr()).initialise();
    }
}

impl CoRoutine {
    /// 协程挂起 `ticks` 个节拍（调用后应立即返回函数体）。
    pub fn delay(&mut self, ticks: TickType) {
        unsafe {
            if self.state != CoRoutineState::Ready {
                return;
            }
            self.delay_until = scheduler::tick_count().wrapping_add(ticks);
            self.state = CoRoutineState::Delayed;
            self.item.item_value = self.delay_until;
            List::remove(&mut self.item);
            (*delayed_ptr()).insert(&mut self.item);
        }
    }

    /// 挂起协程（直到被 `resume`）。
    pub fn suspend(&mut self) {
        unsafe {
            if self.state != CoRoutineState::Ready {
                return;
            }
            self.state = CoRoutineState::Suspended;
            List::remove(&mut self.item);
        }
    }
}

/// 创建协程并加入就绪列表，对应 `xCoRoutineCreate`。
pub fn co_routine_create(co: CoRoutineHandle, func: CoRoutineFn, priority: u8) -> CoRoutineHandle {
    unsafe {
        let c = &mut *co;
        c.item = ListItem::new();
        c.item.owner = co as *mut ();
        c.priority = priority;
        c.state = CoRoutineState::Ready;
        c.func = func;
        c.step = 0;
        c.delay_until = 0;
        add_ready(co);
        co
    }
}

/// 恢复一个被挂起的协程，对应 `vCoRoutineResume`。
pub fn co_routine_resume(co: CoRoutineHandle) {
    unsafe {
        if (*co).state == CoRoutineState::Suspended {
            (*co).state = CoRoutineState::Ready;
            add_ready(co);
        }
    }
}

/// 删除协程（从所在列表移除），对应 `vCoRoutineDelete`。
pub fn co_routine_delete(co: CoRoutineHandle) {
    unsafe {
        if (*co).state != CoRoutineState::Suspended {
            List::remove(&mut (*co).item);
        }
        (*co).state = CoRoutineState::Suspended;
    }
}

/// 协程调度器：由空闲任务周期调用。
///
/// 1. 把到期的延时协程移入就绪；
/// 2. 每个就绪协程运行一步（同优先级按轮转）。
pub fn scheduler_tick() {
    unsafe {
        let now = scheduler::tick_count();

        // 到期延时协程 → 就绪
        while !(*delayed_ptr()).is_empty() {
            let co = (*delayed_ptr()).head_owner() as CoRoutineHandle;
            if (*co).item.item_value > now {
                break;
            }
            List::remove(&mut (*co).item);
            (*co).state = CoRoutineState::Ready;
            add_ready(co);
        }

        // 每个就绪协程运行一步
        let count = (*ready_ptr()).number_of_items;
        for _ in 0..count {
            if (*ready_ptr()).is_empty() {
                break;
            }
            let co = (*ready_ptr()).head_owner() as CoRoutineHandle;
            let f = (*co).func;
            f(&mut *co);
            // 若仍就绪（未 delay/suspend），轮转到队尾
            if (*co).state == CoRoutineState::Ready {
                List::remove(&mut (*co).item);
                add_ready(co);
            }
        }
    }
}

// =====================================================================
// 协程队列（协程间通信，非阻塞）
// =====================================================================

/// 协程队列，对应 FreeRTOS `CoRoutineQueue_t`（简化：非阻塞环形缓冲）。
pub struct CoRoutineQueue {
    storage: *mut u8,
    item_size: usize,
    length: usize,
    read_idx: usize,
    write_idx: usize,
    count: usize,
}

pub type CoRoutineQueueHandle = *mut CoRoutineQueue;

impl CoRoutineQueue {
    pub const fn empty() -> Self {
        Self {
            storage: ptr::null_mut(),
            item_size: 0,
            length: 0,
            read_idx: 0,
            write_idx: 0,
            count: 0,
        }
    }
}

/// 创建协程队列。
///
/// # Safety
/// `storage` 必须指向至少 `length * item_size` 字节的有效内存。
pub unsafe fn co_queue_create(
    q: CoRoutineQueueHandle,
    storage: *mut u8,
    length: usize,
    item_size: usize,
) -> CoRoutineQueueHandle {
    unsafe {
        let r = &mut *q;
        r.storage = storage;
        r.item_size = item_size;
        r.length = length;
        r.read_idx = 0;
        r.write_idx = 0;
        r.count = 0;
        q
    }
}

/// 入队（非阻塞）。返回 `true` 成功，`false` 队满。
pub fn co_queue_send(q: CoRoutineQueueHandle, item: *const u8) -> bool {
    unsafe {
        if (*q).count >= (*q).length {
            return false;
        }
        ptr::copy_nonoverlapping(
            item,
            (*q).storage.add((*q).write_idx * (*q).item_size),
            (*q).item_size,
        );
        (*q).write_idx += 1;
        if (*q).write_idx >= (*q).length {
            (*q).write_idx = 0;
        }
        (*q).count += 1;
        true
    }
}

/// 出队（非阻塞）。返回 `true` 成功，`false` 队空。
pub fn co_queue_receive(q: CoRoutineQueueHandle, out: *mut u8) -> bool {
    unsafe {
        if (*q).count == 0 {
            return false;
        }
        ptr::copy_nonoverlapping(
            (*q).storage.add((*q).read_idx * (*q).item_size),
            out,
            (*q).item_size,
        );
        (*q).read_idx += 1;
        if (*q).read_idx >= (*q).length {
            (*q).read_idx = 0;
        }
        (*q).count -= 1;
        true
    }
}

/// 当前队列元素数。
pub fn co_queue_count(q: CoRoutineQueueHandle) -> usize {
    unsafe { (*q).count }
}
