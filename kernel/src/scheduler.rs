//! 优先级抢占调度器。
//!
//! 对应 FreeRTOS `tasks.c` 中的调度核心：
//! - `pxReadyTasksLists[configMAX_PRIORITIES]`：每个优先级一个就绪列表；
//! - `uxTopReadyPriority`：位图，用于 O(1) 找到最高就绪优先级；
//! - `vTaskSwitchContext` / `xTaskIncrementTick`；
//! - 延时列表（正常 / 溢出双列表）与 `TimeOut` 超时检查。

use core::ptr;

use crate::config::{self, Priority, TaskHandle, TickType};
use crate::list::List;
use crate::port;
use crate::task::{Task, TaskState};

// =====================================================================
// 调度器状态
// =====================================================================

/// 当前任务 TCB 指针（汇编直接引用该符号）。
#[unsafe(no_mangle)]
static mut pxCurrentTCB: TaskHandle = ptr::null_mut();

static mut READY_LISTS: [List; config::MAX_PRIORITIES] =
    [const { List::new() }; config::MAX_PRIORITIES];

/// 正常延时列表（按唤醒时刻升序）。
static mut DELAYED_LIST: List = List::new();
/// 溢出延时列表（tick 计数回绕时与之交换）。
static mut OVERFLOW_DELAYED_LIST: List = List::new();
/// 挂起任务列表（无限阻塞的任务也放在这里，简化处理）。
static mut SUSPENDED_LIST: List = List::new();

/// 系统节拍计数，对应 `xTickCount`。
static mut TICK_COUNT: TickType = 0;
/// tick 计数回绕次数，对应 `xNumOfOverflows`。
static mut NUM_OVERFLOWS: u32 = 0;
/// 下一个任务解除阻塞的时刻，对应 `xNextTaskUnblockTime`。
static mut NEXT_UNBLOCK_TIME: TickType = config::INFINITE;
/// 就绪优先级位图：bit `i` 为 1 表示优先级 `i` 有就绪任务。
static mut TOP_READY_PRIORITY: u32 = 0;
/// 列表是否已初始化（惰性初始化，允许任务在调度器启动前创建）。
static mut LISTS_INITIALISED: bool = false;
/// 已创建任务计数（简化统计）。
static mut TASK_COUNT: usize = 0;
/// 运行时内核配置（由 `#[kernel::entry(...)]` 宏参数经 `init` 设置）。
static mut ACTIVE_CONFIG: config::KernelConfig = config::KernelConfig {
    tick_rate_hz: config::DEFAULT_TICK_RATE_HZ,
    max_priorities: config::DEFAULT_MAX_PRIORITIES,
    max_tasks: config::DEFAULT_MAX_TASKS,
};

// =====================================================================
// 可变静态的裸指针访问器
// （Rust 2024 禁止对 `static mut` 取引用，统一走 `addr_of_mut!`）
// =====================================================================

#[inline(always)]
unsafe fn ready_ptr(prio: usize) -> *mut List {
    unsafe { (ptr::addr_of_mut!(READY_LISTS) as *mut List).add(prio) }
}

#[inline(always)]
unsafe fn delayed_ptr() -> *mut List {
    ptr::addr_of_mut!(DELAYED_LIST)
}

#[inline(always)]
unsafe fn overflow_ptr() -> *mut List {
    ptr::addr_of_mut!(OVERFLOW_DELAYED_LIST)
}

#[inline(always)]
unsafe fn suspended_ptr() -> *mut List {
    ptr::addr_of_mut!(SUSPENDED_LIST)
}

// =====================================================================
// 运行时配置（由入口宏 `#[kernel::entry(...)]` 经 `init` 设置）
// =====================================================================

/// 初始化内核：设置运行时配置并初始化列表。
///
/// 应在创建/`spawn` 任何任务之前调用一次（`#[kernel::entry]` 会自动调用）。
pub fn init(config: config::KernelConfig) {
    unsafe {
        let mut cfg = config;
        if cfg.max_priorities > config::MAX_PRIORITIES {
            cfg.max_priorities = config::MAX_PRIORITIES;
        }
        if cfg.max_priorities == 0 {
            cfg.max_priorities = 1;
        }
        ACTIVE_CONFIG = cfg;
        ensure_lists_initialised();
    }
}

/// 当前配置的最大优先级数量。
pub fn max_priorities() -> usize {
    unsafe { ACTIVE_CONFIG.max_priorities }
}

/// 当前配置的系统节拍频率（Hz）。
pub fn tick_rate_hz() -> u32 {
    unsafe { ACTIVE_CONFIG.tick_rate_hz }
}

/// 当前配置的最大任务数量。
pub fn max_tasks() -> usize {
    unsafe { ACTIVE_CONFIG.max_tasks }
}

// =====================================================================
// 超时检查（对应 FreeRTOS vTaskSetTimeOutState / xTaskCheckForTimeOut）
// =====================================================================

#[derive(Clone, Copy)]
pub struct TimeOut {
    time_on_entering: TickType,
    overflow_count: u32,
}

impl TimeOut {
    pub const fn new() -> Self {
        Self {
            time_on_entering: 0,
            overflow_count: 0,
        }
    }
}

/// 记录开始等待的时刻（进入阻塞前调用）。
pub unsafe fn set_timeout_state(to: &mut TimeOut) {
    unsafe {
        to.overflow_count = NUM_OVERFLOWS;
        to.time_on_entering = TICK_COUNT;
    }
}

/// 检查是否超时；未超时则扣减剩余时间并刷新记录。
///
/// 返回 `true` 表示已超时（并把 `remaining` 清零）。
pub unsafe fn check_timeout(to: &mut TimeOut, remaining: &mut TickType) -> bool {
    unsafe {
        if *remaining == config::INFINITE {
            return false;
        }
        if to.overflow_count != NUM_OVERFLOWS {
            *remaining = 0;
            return true;
        }
        let elapsed = TICK_COUNT.wrapping_sub(to.time_on_entering);
        if elapsed < *remaining {
            *remaining -= elapsed;
            set_timeout_state(to);
            false
        } else {
            *remaining = 0;
            true
        }
    }
}

// =====================================================================
// 列表辅助
// =====================================================================

/// 惰性初始化所有列表（任务可在调度器启动前创建）。
pub unsafe fn ensure_lists_initialised() {
    unsafe {
        if !LISTS_INITIALISED {
            for i in 0..config::MAX_PRIORITIES {
                (*ready_ptr(i)).initialise();
            }
            (*delayed_ptr()).initialise();
            (*overflow_ptr()).initialise();
            (*suspended_ptr()).initialise();
            LISTS_INITIALISED = true;
        }
    }
}

/// 当前任务句柄。
pub unsafe fn current() -> TaskHandle {
    unsafe { pxCurrentTCB }
}

pub unsafe fn task_count() -> usize {
    unsafe { TASK_COUNT }
}

/// 把任务加入其优先级对应的就绪列表（FIFO，末尾插入）。
pub unsafe fn add_to_ready_list(t: TaskHandle) {
    unsafe {
        let prio = (*t).priority as usize;
        debug_assert!(prio < ACTIVE_CONFIG.max_priorities, "任务优先级超出配置范围");
        (*t).state_item.item_value = prio as u32;
        (*ready_ptr(prio)).insert_end(&mut (*t).state_item);
        TOP_READY_PRIORITY |= 1 << prio;
        if (*t).state != TaskState::Running {
            (*t).state = TaskState::Ready;
        }
    }
}

/// 把任务从它当前所在的任意列表（就绪/延时/挂起）移除。
pub unsafe fn remove_from_any_list(t: TaskHandle) {
    unsafe {
        let container = (*t).state_item.container;
        if container.is_null() {
            return;
        }
        for prio in 0..config::MAX_PRIORITIES {
            if container == ready_ptr(prio) {
                let n = List::remove(&mut (*t).state_item);
                if n == 0 {
                    TOP_READY_PRIORITY &= !(1 << prio);
                }
                return;
            }
        }
        // 延时 / 挂起列表
        List::remove(&mut (*t).state_item);
    }
}

/// 把任务加入挂起列表（也用于无限阻塞）。
pub unsafe fn add_to_suspended_list(t: TaskHandle) {
    unsafe {
        (*suspended_ptr()).insert_end(&mut (*t).state_item);
    }
}

/// 优先级发生变化时，在就绪列表之间迁移任务（若它当前就绪）。
pub unsafe fn reposition_ready(t: TaskHandle, old_priority: Priority) {
    unsafe {
        let in_ready = (*t).state_item.container == ready_ptr(old_priority as usize);
        if in_ready {
            let n = List::remove(&mut (*t).state_item);
            if n == 0 {
                TOP_READY_PRIORITY &= !(1 << old_priority);
            }
            add_to_ready_list(t);
        }
    }
}

/// 从就绪列表移除当前任务（用于阻塞/延时前）。
unsafe fn remove_current_from_ready() {
    unsafe {
        let prio = (*pxCurrentTCB).priority as usize;
        let n = List::remove(&mut (*pxCurrentTCB).state_item);
        if n == 0 {
            TOP_READY_PRIORITY &= !(1 << prio);
        }
    }
}

// =====================================================================
// 阻塞 / 唤醒
// =====================================================================

/// 把当前任务加入延时（或挂起）列表。
///
/// `can_block_indefinitely` 为真且 `ticks == INFINITE` 时挂起而非计时。
pub unsafe fn add_current_to_delayed_list(ticks: TickType, can_block_indefinitely: bool) {
    unsafe {
        remove_current_from_ready();
        (*pxCurrentTCB).state = TaskState::Blocked;

        if ticks == config::INFINITE && can_block_indefinitely {
            (*suspended_ptr()).insert_end(&mut (*pxCurrentTCB).state_item);
            return;
        }

        let wake = TICK_COUNT.wrapping_add(ticks);
        (*pxCurrentTCB).state_item.item_value = wake;
        if wake < TICK_COUNT {
            (*overflow_ptr()).insert(&mut (*pxCurrentTCB).state_item);
        } else {
            (*delayed_ptr()).insert(&mut (*pxCurrentTCB).state_item);
        }
        if wake < NEXT_UNBLOCK_TIME {
            NEXT_UNBLOCK_TIME = wake;
        }
    }
}

/// 把当前任务阻塞在某个内核对象的等待列表上。
///
/// `event_list` 中的等待者按“优先级降序”排列（值越小优先级越高），
/// 因此 `head_owner` 返回最高优先级的等待者。
pub unsafe fn block_current(event_list: *mut List, timeout: TickType) {
    unsafe {
        let value = (ACTIVE_CONFIG.max_priorities - 1 - (*pxCurrentTCB).priority as usize) as u32;
        block_current_value(event_list, value, timeout);
    }
}

/// 以自定义事件列表项取值阻塞当前任务（供事件组等使用）。
pub unsafe fn block_current_value(event_list: *mut List, event_value: u32, timeout: TickType) {
    unsafe {
        (*pxCurrentTCB).event_item.item_value = event_value;
        (*event_list).insert(&mut (*pxCurrentTCB).event_item);
        add_current_to_delayed_list(timeout, true);
    }
}

/// 从对象的等待列表唤醒“最高优先级”的任务。
///
/// 返回 `true` 表示被唤醒任务优先级高于当前任务（调用方应触发切换）。
pub unsafe fn remove_from_event_list(event_list: *mut List) -> bool {
    unsafe {
        let t = (*event_list).head_owner() as TaskHandle;
        if t.is_null() {
            return false;
        }
        List::remove(&mut (*t).event_item);
        (*t).blocked_on = ptr::null_mut();
        // 从延时/挂起列表移除，加入就绪列表
        List::remove(&mut (*t).state_item);
        add_to_ready_list(t);
        (*t).priority > (*pxCurrentTCB).priority
    }
}

/// 唤醒一个指定的阻塞/挂起任务（供任务通知等直接唤醒场景使用，
/// 需 `task-notification` 特性）。
///
/// 返回 `true` 表示被唤醒任务优先级高于当前任务（调用方应触发切换）。
#[cfg(feature = "task-notification")]
pub unsafe fn unblock_task(t: TaskHandle) -> bool {
    unsafe {
        if t.is_null() || ((*t).state != TaskState::Blocked && (*t).state != TaskState::Suspended)
        {
            return false;
        }
        List::remove(&mut (*t).state_item);
        (*t).blocked_on = ptr::null_mut();
        add_to_ready_list(t);
        (*t).priority > (*pxCurrentTCB).priority
    }
}

// =====================================================================
// 上下文切换与节拍
// =====================================================================

unsafe fn highest_ready_priority() -> usize {
    unsafe {
        // 只考虑已配置优先级范围内的位
        let mask = if ACTIVE_CONFIG.max_priorities >= 32 {
            u32::MAX
        } else {
            (1u32 << ACTIVE_CONFIG.max_priorities) - 1
        };
        let bits = TOP_READY_PRIORITY & mask;
        if bits == 0 {
            return 0;
        }
        (usize::BITS - 1 - bits.leading_zeros()) as usize
    }
}

/// 选出最高优先级就绪任务（由 PendSV 在临界区内调用）。
///
/// 使用 `get_owner_of_next_entry` 实现同优先级任务的时间片轮转。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vTaskSwitchContext() {
    unsafe {
        let prev = pxCurrentTCB;
        let prio = highest_ready_priority();
        pxCurrentTCB = (*ready_ptr(prio)).get_owner_of_next_entry() as TaskHandle;
        (*pxCurrentTCB).state = TaskState::Running;
        if !prev.is_null() && prev != pxCurrentTCB && (*prev).state == TaskState::Running {
            (*prev).state = TaskState::Ready;
        }
    }
}

unsafe fn reset_next_unblock_time() {
    unsafe {
        if (*delayed_ptr()).is_empty() {
            NEXT_UNBLOCK_TIME = config::INFINITE;
        } else {
            let t = (*delayed_ptr()).head_owner() as TaskHandle;
            NEXT_UNBLOCK_TIME = (*t).state_item.item_value;
        }
    }
}

/// 系统节拍递增（由 SysTick 处理器调用）。
///
/// 返回 `true` 表示需要触发 PendSV 上下文切换：
/// - 唤醒了更高优先级任务；或
/// - 时间片轮转：当前任务仍就绪且同优先级有多个就绪任务
///   （对应 FreeRTOS `configUSE_TIME_SLICING = 1`，每个节拍切换一次）。
pub unsafe fn increment_tick() -> bool {
    unsafe {
        let mut yield_needed = false;

        TICK_COUNT = TICK_COUNT.wrapping_add(1);
        if TICK_COUNT == 0 {
            // 计数回绕：交换两个延时列表
            ptr::swap(delayed_ptr(), overflow_ptr());
            NUM_OVERFLOWS += 1;
            reset_next_unblock_time();
        }

        if TICK_COUNT >= NEXT_UNBLOCK_TIME {
            loop {
                if (*delayed_ptr()).is_empty() {
                    NEXT_UNBLOCK_TIME = config::INFINITE;
                    break;
                }
                let t = (*delayed_ptr()).head_owner() as TaskHandle;
                let value = (*t).state_item.item_value;
                if TICK_COUNT < value {
                    NEXT_UNBLOCK_TIME = value;
                    break;
                }
                List::remove(&mut (*t).state_item);
                // 若仍被某个对象阻塞，一并从该对象等待列表移除
                if !(*t).event_item.container.is_null() {
                    List::remove(&mut (*t).event_item);
                }
                (*t).blocked_on = ptr::null_mut();
                add_to_ready_list(t);
                if (*t).priority > (*pxCurrentTCB).priority {
                    yield_needed = true;
                }
            }
        }

        // 时间片轮转：同优先级就绪任务 > 1 时，每个节拍切换一次。
        // 实际的“轮转顺序”由 PendSV 里的 vTaskSwitchContext 完成：
        // 它用 get_owner_of_next_entry 前移就绪列表的 pxIndex，天然按 FIFO 轮转。
        if !yield_needed && !pxCurrentTCB.is_null() {
            let prio = (*pxCurrentTCB).priority as usize;
            if (*ready_ptr(prio)).number_of_items > 1 {
                yield_needed = true;
            }
        }

        yield_needed
    }
}

/// SysTick 处理器入口：用户在自己的 `SysTick_Handler` 中调用它。
pub fn tick_handler() {
    unsafe {
        let yield_needed = increment_tick();
        if yield_needed {
            port::yield_from_isr();
        }
    }
}

/// 初始化 SysTick 并启动（**由调用方调用**，通常在复位入口、调度器启动前）。
///
/// 依据当前系统时钟计算重载值，节拍间隔以微秒为单位：
/// `reload = sysclk_hz * interval_us / 1_000_000`。
///
/// - `interval_us`：节拍间隔（微秒，>= 1），例如 1 ms = `1000`；
/// - `sysclk_hz`：当前系统时钟频率（Hz），例如 100 MHz = `100_000_000`。
///
/// 同时把内核的 `tick_rate_hz` 更新为 `1_000_000 / interval_us`。
pub fn systick_init(interval_us: u32, sysclk_hz: u32) {
    use cortex_m::peripheral::syst::SystClkSource;

    let us = interval_us.max(1);
    let ticks_per_sec = 1_000_000u64 / us as u64;
    let reload = ((sysclk_hz as u64 * us as u64) / 1_000_000u64).max(1) as u32;

    unsafe {
        let mut syst = cortex_m::Peripherals::steal().SYST;
        syst.set_clock_source(SystClkSource::Core); // 处理器时钟
        syst.set_reload(reload - 1);
        syst.clear_current();
        syst.enable_counter();
        syst.enable_interrupt();

        ACTIVE_CONFIG.tick_rate_hz = ticks_per_sec as u32;
    }
}

/// SysTick 中断处理器（内核已把该符号接入向量表：`SysTick_Handler` / `SysTick`）。
///
/// 应用**无需**再定义 `#[exception] fn SysTick()`。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SysTick_Handler() {
    tick_handler();
}

// =====================================================================
// 启动
// =====================================================================

#[repr(align(8))]
#[allow(dead_code)]
struct IdleStack([u8; config::IDLE_STACK_SIZE]);

static mut IDLE_STACK: IdleStack = IdleStack([0; config::IDLE_STACK_SIZE]);
static mut IDLE_TCB: Task = Task::empty();

unsafe extern "C" fn idle_task(_: *mut ()) -> ! {
    loop {
        // 空闲任务：无任务可运行时在此空转，并驱动协程调度器
        // （真实实现应执行 WFI 低功耗）。
        #[cfg(feature = "coroutine")]
        crate::coroutine::scheduler_tick();
        core::hint::spin_loop();
    }
}

unsafe fn create_idle_task() {
    unsafe {
        let h = crate::task::task_create(
            ptr::addr_of_mut!(IDLE_TCB),
            idle_task,
            "idle",
            ptr::addr_of_mut!(IDLE_STACK) as *mut u8,
            config::IDLE_STACK_SIZE,
            ptr::null_mut(),
            0,
        );
        crate::task::spawn(h);
    }
}

/// 任务创建计数 +1（供 [`crate::task::task_create`] 调用）。
pub(crate) unsafe fn bump_task_count() {
    unsafe {
        TASK_COUNT += 1;
    }
}

/// 启动调度器：初始化列表、创建空闲任务与软件定时器服务，
/// 选出首个任务并切换到任务上下文。**该函数不会返回。**
pub fn start_scheduler() -> ! {
    unsafe {
        ensure_lists_initialised();
        create_idle_task();
        crate::timer::init();
        #[cfg(feature = "coroutine")]
        crate::coroutine::init();

        vTaskSwitchContext();
        port::port_start_first_task();
    }
}

// 访问私有静态的辅助（供任务/互斥锁/定时器等模块读取节拍）。
pub unsafe fn tick_count() -> TickType {
    unsafe { TICK_COUNT }
}
