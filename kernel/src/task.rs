//! 任务控制块（TCB）与任务生命周期 API。
//!
//! 对应 FreeRTOS `task.h` 中的 `tskTCB` 与 `xTaskCreate`/`vTaskDelete` 等。

use core::ptr;

use crate::config::{Priority, TaskHandle, TickType};
use crate::list::ListItem;
use crate::port;
use crate::scheduler;

/// 任务入口函数：接收一个 `*mut ()` 参数，永不返回。
pub type EntryFn = unsafe extern "C" fn(*mut ()) -> !;

/// 任务状态，对应 FreeRTOS `eTaskState`。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskState {
    Running,
    Ready,
    Blocked,
    Suspended,
    Deleted,
}

/// 任务控制块（TCB）。
///
/// **`top_of_stack` 必须是第一个字段**：PendSV/SVC 汇编直接以
/// `pxCurrentTCB` 指向的地址作为 `top_of_stack` 进行存取。
#[repr(C)]
pub struct Task {
    pub top_of_stack: *mut u32,
    /// 通用状态列表项：就绪时位于就绪列表，阻塞/延时时位于延时列表。
    pub state_item: ListItem,
    /// 事件列表项：阻塞在队列/信号量/事件组上时位于对象的等待列表。
    pub event_item: ListItem,
    /// 当前优先级（可能因互斥锁优先级继承被临时提升）。
    pub priority: Priority,
    /// 基本优先级（未继承时的优先级）。
    pub base_priority: Priority,
    /// 已持有的互斥锁数量。
    pub mutexes_held: u32,
    pub state: TaskState,
    pub stack_base: *mut u8,
    pub stack_size: usize,
    pub entry: EntryFn,
    pub param: *mut (),
    pub name: &'static str,
    /// 当前阻塞所在的内核对象（用于互斥锁优先级继承的递归传播）。
    pub blocked_on: *mut (),
    /// 任务通知值（对应 FreeRTOS `ulNotifiedValue`，需 `task-notification` 特性）。
    #[cfg(feature = "task-notification")]
    pub notification: u32,
    /// 任务是否正在等待通知（供通知发送方直接唤醒，需 `task-notification` 特性）。
    #[cfg(feature = "task-notification")]
    pub notify_waiting: bool,
}

// 任务对象会跨上下文/中断移动；裸指针本身非 Send/Sync，
// 这里由调用者保证单核、临界区保护下的安全（典型 RTOS 场景）。
unsafe impl Send for Task {}
unsafe impl Sync for Task {}

impl Task {
    /// 构造一个“空白”TCB，字段随后由 [`task_create`] 填充。
    pub const fn empty() -> Self {
        Self {
            top_of_stack: ptr::null_mut(),
            state_item: ListItem::new(),
            event_item: ListItem::new(),
            priority: 0,
            base_priority: 0,
            mutexes_held: 0,
            state: TaskState::Deleted,
            stack_base: ptr::null_mut(),
            stack_size: 0,
            entry: _default_entry,
            param: ptr::null_mut(),
            name: "",
            blocked_on: ptr::null_mut(),
            #[cfg(feature = "task-notification")]
            notification: 0,
            #[cfg(feature = "task-notification")]
            notify_waiting: false,
        }
    }
}

/// 占位入口（仅用于静态初始化，实际任务创建时会覆盖）。
unsafe extern "C" fn _default_entry(_: *mut ()) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

/// 创建任务（**不**加入就绪列表，需用 [`spawn`] 启动）。
///
/// 对应 FreeRTOS `xTaskCreateStatic`（静态分配版）：
/// 调用方（或 `#[task]` 宏）提供 TCB 与栈内存。
///
/// # Safety
/// - `tcb` 与 `stack` 必须指向在整个任务生命周期内有效、可写的内存；
/// - `stack` 必须 8 字节对齐，`size` 至少容纳一个异常帧；
/// - 建议在调度器启动前创建任务。
pub unsafe fn task_create(
    tcb: TaskHandle,
    entry: EntryFn,
    name: &'static str,
    stack: *mut u8,
    size: usize,
    param: *mut (),
    priority: Priority,
) -> TaskHandle {
    unsafe {
        scheduler::ensure_lists_initialised();

        // 先用图案填充整块任务栈（供高水位检测）。
        // 注意：`#[task]` 宏生成的任务参数在 task_create 之后才写入参数区，
        // 因此这里填充不会覆盖参数。
        mark_task_stack(stack, size);

        let sp = port::init_task_stack(stack, size, entry, param);

        let t = &mut *tcb;
        t.top_of_stack = sp;
        t.state_item = ListItem::new();
        t.event_item = ListItem::new();
        t.state_item.owner = tcb as *mut ();
        t.event_item.owner = tcb as *mut ();
        t.priority = priority;
        t.base_priority = priority;
        t.mutexes_held = 0;
        t.state = TaskState::Ready;
        t.stack_base = stack;
        t.stack_size = size;
        t.entry = entry;
        t.param = param;
        t.name = name;
        t.blocked_on = ptr::null_mut();
        #[cfg(feature = "task-notification")]
        {
            t.notification = 0;
            t.notify_waiting = false;
        }

        scheduler::bump_task_count();
        tcb
    }
}

/// 把已创建的任务加入就绪列表（开始参与调度）。
///
/// 对应“spawn”语义：`#[task]` 只是让函数可被创建，调用 [`spawn`] 才真正启动它。
pub fn spawn(handle: TaskHandle) -> TaskHandle {
    unsafe {
        port::enter_critical();
        let prio = (*handle).priority as usize;
        debug_assert!(prio < scheduler::max_priorities(), "任务优先级超出配置的 max_priorities");
        scheduler::add_to_ready_list(handle);
        port::exit_critical();
    }
    handle
}

/// 当前任务句柄，对应 FreeRTOS `xTaskGetCurrentTaskHandle`。
pub fn current_task() -> TaskHandle {
    unsafe { scheduler::current() }
}

/// 让出 CPU（触发一次同优先级轮转），对应 `taskYIELD`。
pub fn task_yield() {
    unsafe { port::yield_from_task() }
}

/// 相对延时 `ticks` 个节拍，对应 `vTaskDelay`。
pub fn task_delay(ticks: TickType) {
    unsafe {
        if ticks == 0 {
            port::yield_from_task();
            return;
        }
        port::enter_critical();
        scheduler::add_current_to_delayed_list(ticks, true);
        port::exit_critical();
        port::yield_from_task();
    }
}

/// 获取任务优先级，对应 `uxTaskPriorityGet`。
pub fn task_get_priority(handle: TaskHandle) -> Priority {
    unsafe { (*handle).priority }
}

/// 设置任务优先级，对应 `vTaskPrioritySet`。
///
/// 会同步更新 `base_priority`（即取消任何继承提升），并在任务处于就绪态时
/// 把它移动到新优先级对应的就绪列表。
pub fn task_set_priority(handle: TaskHandle, new_priority: Priority) {
    unsafe {
        port::enter_critical();
        let t = &mut *handle;
        let old = t.priority;
        t.base_priority = new_priority;
        t.priority = new_priority;
        t.state_item.item_value = new_priority as u32;
        // 若在就绪列表，跨列表移动
        if t.state == TaskState::Ready || t.state == TaskState::Running {
            scheduler::reposition_ready(handle, old);
        }
        port::exit_critical();
        // 降低优先级时让出 CPU，以便更高优先级任务及时运行
        if new_priority < old {
            port::yield_from_task();
        }
    }
}

/// 挂起任务，对应 `vTaskSuspend`。
pub fn task_suspend(handle: TaskHandle) {
    unsafe {
        port::enter_critical();
        let t = &mut *handle;
        if t.state == TaskState::Suspended {
            port::exit_critical();
            return;
        }
        // 从当前所在列表移除（就绪或延时列表）
        scheduler::remove_from_any_list(handle);
        t.state = TaskState::Suspended;
        scheduler::add_to_suspended_list(handle);
        let is_current = handle == scheduler::current();
        port::exit_critical();
        if is_current {
            port::yield_from_task();
        }
    }
}

/// 恢复任务，对应 `vTaskResume`。
pub fn task_resume(handle: TaskHandle) {
    unsafe {
        port::enter_critical();
        let t = &mut *handle;
        if t.state != TaskState::Suspended {
            port::exit_critical();
            return;
        }
        crate::list::List::remove(&mut t.state_item);
        scheduler::add_to_ready_list(handle);
        let need_yield = (*handle).priority > (*scheduler::current()).priority;
        port::exit_critical();
        if need_yield {
            port::yield_from_task();
        }
    }
}

/// 删除任务（简化实现：从所有列表移除并标记为已删除）。
pub fn task_delete(handle: TaskHandle) {
    unsafe {
        port::enter_critical();
        scheduler::remove_from_any_list(handle);
        (*handle).state = TaskState::Deleted;
        port::exit_critical();
    }
}

/// 返回任务栈头保留字节数（r4-r11 + EXC_RETURN + 异常帧 = 68），
/// 供 `#[task]` 宏计算参数区（参数放在栈头下方）。
pub fn task_stack_header_size() -> usize {
    crate::port::STACK_HEADER_SIZE
}

/// 任务栈填充图案（0xE7，创建任务时写入整块栈）。
const STACK_FILL: u32 = 0xE7E7_E7E7;

/// 用 [`STACK_FILL`] 图案填充任务栈（供高水位检测；`task_create` 会自动调用）。
///
/// # Safety
/// `stack` 必须指向至少 `size` 字节的可写内存。
pub unsafe fn mark_task_stack(stack: *mut u8, size: usize) {
    unsafe {
        let words = size / 4;
        let p = stack as *mut u32;
        for i in 0..words {
            p.add(i).write_volatile(STACK_FILL);
        }
    }
}

/// 返回任务栈“最小剩余”字节数（栈底向上连续未被覆盖的字节）。
///
/// 任务创建时整块栈被填充为 0xE7，运行时栈向下生长会覆盖图案，
/// 从栈底向上扫描第一个非图案字节即为历史最深水位。
pub fn task_stack_free(handle: TaskHandle) -> usize {
    unsafe {
        let t = &*handle;
        let words = t.stack_size / 4;
        let p = t.stack_base as *const u32;
        let mut free = 0usize;
        for i in 0..words {
            if p.add(i).read_volatile() != STACK_FILL {
                break;
            }
            free += 4;
        }
        free
    }
}

/// 返回任务栈历史最大已用字节数（高水位）。
pub fn task_stack_used(handle: TaskHandle) -> usize {
    unsafe { (*handle).stack_size - task_stack_free(handle) }
}

/// 从参数指针“移动”取出实参（供 `#[task]` 宏生成的任务入口使用）。
///
/// 参数值在任务创建时已被移动到任务自己的栈上，这里用 `ptr::read` 再次
/// 移动取出，因此实参类型**无需 `Copy`**（可用于拥有外设所有权的结构体）。
#[inline(always)]
pub unsafe fn load_param<T>(param: *mut ()) -> T {
    unsafe { core::ptr::read(param as *const T) }
}

/// 返回当前已创建任务数（简化统计）。
pub fn task_count() -> usize {
    unsafe { scheduler::task_count() }
}
