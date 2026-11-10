//! 软件定时器。
//!
//! 对应 FreeRTOS `timers.c`：定时器服务（守护）任务维护一个按到期时刻排序的
//! 激活列表；启动/停止/复位通过命令队列投递，因此可从 ISR 安全调用。

use core::mem::size_of;
use core::ptr;

use crate::config::{self, TickType};
use crate::list::{List, ListItem};
use crate::port;
use crate::queue::{self, QUEUE_TYPE_BASE, Queue, QueueError};
use crate::scheduler;
use crate::task::{self, Task};

/// 定时器回调，对应 FreeRTOS `TimerCallbackFunction_t`。
pub type TimerCallbackFn = unsafe extern "C" fn(*mut ());

/// 定时器控制块，对应 FreeRTOS `Timer_t`（简化）。
pub struct Timer {
    /// 用于激活列表的列表项（item_value = 到期时刻）。
    pub list_item: ListItem,
    pub callback: TimerCallbackFn,
    pub name: &'static str,
    pub period: TickType,
    pub auto_reload: bool,
    pub id: *mut (),
    pub active: bool,
}

pub type TimerHandle = *mut Timer;

impl Timer {
    pub const fn empty() -> Self {
        Self {
            list_item: ListItem::new(),
            callback: _noop_callback,
            name: "",
            period: 0,
            auto_reload: false,
            id: ptr::null_mut(),
            active: false,
        }
    }
}

unsafe extern "C" fn _noop_callback(_: *mut ()) {}

// 命令类型
const CMD_START: u8 = 0;
const CMD_STOP: u8 = 1;
const CMD_RESET: u8 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct TimerCommandMsg {
    cmd: u8,
    handle: TimerHandle,
    time: TickType,
}

// 定时器子系统静态存储
static mut ACTIVE_TIMERS: List = List::new();
static mut TIMER_CMD_QUEUE: Queue = Queue::empty();
static mut TIMER_CMD_STORAGE: [u8; size_of::<TimerCommandMsg>() * config::TIMER_COMMAND_QUEUE_LEN] =
    [0; size_of::<TimerCommandMsg>() * config::TIMER_COMMAND_QUEUE_LEN];
static mut TIMER_DAEMON_TCB: Task = Task::empty();

#[repr(align(8))]
#[allow(dead_code)]
struct DaemonStack([u8; config::TIMER_DAEMON_STACK_SIZE]);
static mut TIMER_DAEMON_STACK: DaemonStack = DaemonStack([0; config::TIMER_DAEMON_STACK_SIZE]);

// Rust 2024 禁止对 `static mut` 取引用，统一走裸指针访问。
#[inline(always)]
unsafe fn active_timers() -> *mut List {
    &raw mut ACTIVE_TIMERS
}

/// 时刻 `target` 是否已到达（考虑回绕，假设差值 < 2^31）。
fn tick_reached(target: TickType) -> bool {
    let d = target.wrapping_sub(unsafe { scheduler::tick_count() });
    d == 0 || (d & 0x8000_0000) != 0
}

/// 到下一个定时器到期还需等待的节拍数（已到期返回 0）。
fn next_expiry_ticks() -> TickType {
    unsafe {
        if (*active_timers()).is_empty() {
            return config::INFINITE;
        }
        let t = (*active_timers()).head_owner() as TimerHandle;
        let expire = (*t).list_item.item_value;
        if tick_reached(expire) {
            0
        } else {
            expire.wrapping_sub(scheduler::tick_count())
        }
    }
}

unsafe fn insert_active(t: TimerHandle, expire: TickType) {
    unsafe {
        (*t).list_item.item_value = expire;
        (*active_timers()).insert(&mut (*t).list_item);
        (*t).active = true;
    }
}

unsafe fn start_timer(t: TimerHandle) {
    unsafe {
        port::enter_critical();
        if !(*t).active {
            insert_active(t, scheduler::tick_count().wrapping_add((*t).period));
        }
        port::exit_critical();
    }
}

unsafe fn stop_timer(t: TimerHandle) {
    unsafe {
        port::enter_critical();
        if (*t).active {
            List::remove(&mut (*t).list_item);
            (*t).active = false;
        }
        port::exit_critical();
    }
}

/// 复位 = 停止后按完整周期重新启动（简化，未保留原始相位）。
unsafe fn reset_timer(t: TimerHandle) {
    unsafe {
        port::enter_critical();
        if (*t).active {
            List::remove(&mut (*t).list_item);
        }
        insert_active(t, scheduler::tick_count().wrapping_add((*t).period));
        port::exit_critical();
    }
}

unsafe fn process_command(msg: &TimerCommandMsg) {
    unsafe {
        match msg.cmd {
            CMD_START => start_timer(msg.handle),
            CMD_STOP => stop_timer(msg.handle),
            CMD_RESET => reset_timer(msg.handle),
            _ => {}
        }
    }
}

unsafe fn process_expired() {
    unsafe {
        loop {
            port::enter_critical();
            if (*active_timers()).is_empty() {
                port::exit_critical();
                break;
            }
            let t = (*active_timers()).head_owner() as TimerHandle;
            if !tick_reached((*t).list_item.item_value) {
                port::exit_critical();
                break;
            }
            List::remove(&mut (*t).list_item);
            (*t).active = false;
            let cb = (*t).callback;
            let id = (*t).id;
            let auto = (*t).auto_reload;
            let period = (*t).period;
            port::exit_critical();

            // 回调在临界区外执行
            cb(id);

            if auto {
                port::enter_critical();
                insert_active(t, scheduler::tick_count().wrapping_add(period));
                port::exit_critical();
            }
        }
    }
}

unsafe extern "C" fn timer_daemon(_: *mut ()) -> ! {
    loop {
        let next = next_expiry_ticks();
        let mut msg = TimerCommandMsg {
            cmd: u8::MAX,
            handle: ptr::null_mut(),
            time: 0,
        };
        let got = queue::queue_receive(
            &raw mut TIMER_CMD_QUEUE,
            &raw mut msg as *mut TimerCommandMsg as *mut u8,
            next,
        );
        if got.is_ok() {
            process_command(&msg);
        }
        process_expired();
    }
}

/// 初始化软件定时器子系统（由调度器启动时调用）。
pub(crate) unsafe fn init() {
    unsafe {
        (*active_timers()).initialise();
        queue::queue_create(
            &raw mut TIMER_CMD_QUEUE,
            &raw mut TIMER_CMD_STORAGE as *mut u8,
            config::TIMER_COMMAND_QUEUE_LEN,
            size_of::<TimerCommandMsg>(),
            QUEUE_TYPE_BASE,
        );
        // 定时器服务任务取最高优先级
        let prio = (crate::scheduler::max_priorities() - 1) as crate::config::Priority;
        let h = task::task_create(
            &raw mut TIMER_DAEMON_TCB,
            timer_daemon,
            "tmr svc",
            &raw mut TIMER_DAEMON_STACK as *mut u8,
            config::TIMER_DAEMON_STACK_SIZE,
            ptr::null_mut(),
            prio,
        );
        task::spawn(h);
    }
}

/// 创建定时器（不启动），对应 `xTimerCreate`。
pub unsafe fn timer_create(
    timer: TimerHandle,
    callback: TimerCallbackFn,
    name: &'static str,
    period: TickType,
    auto_reload: bool,
    id: *mut (),
) -> TimerHandle {
    unsafe {
        let t = &mut *timer;
        t.list_item = ListItem::new();
        t.list_item.owner = timer as *mut ();
        t.callback = callback;
        t.name = name;
        t.period = period;
        t.auto_reload = auto_reload;
        t.id = id;
        t.active = false;
        timer
    }
}

/// 启动定时器（任务级），对应 `xTimerStart`。
pub fn timer_start(handle: TimerHandle, timeout: TickType) -> Result<(), QueueError> {
    unsafe {
        let msg = TimerCommandMsg {
            cmd: CMD_START,
            handle,
            time: scheduler::tick_count(),
        };
        queue::queue_send(
            &raw mut TIMER_CMD_QUEUE,
            &raw const msg as *const u8,
            timeout,
        )
    }
}

/// 停止定时器（任务级），对应 `xTimerStop`。
pub fn timer_stop(handle: TimerHandle, timeout: TickType) -> Result<(), QueueError> {
    let msg = TimerCommandMsg {
        cmd: CMD_STOP,
        handle,
        time: 0,
    };
    queue::queue_send(
        &raw mut TIMER_CMD_QUEUE,
        &raw const msg as *const u8,
        timeout,
    )
}

/// 复位定时器（任务级），对应 `xTimerReset`。
pub fn timer_reset(handle: TimerHandle, timeout: TickType) -> Result<(), QueueError> {
    unsafe {
        let msg = TimerCommandMsg {
            cmd: CMD_RESET,
            handle,
            time: scheduler::tick_count(),
        };
        queue::queue_send(
            &raw mut TIMER_CMD_QUEUE,
            &raw const msg as *const u8,
            timeout,
        )
    }
}

/// 从中断启动定时器。返回 `true` 表示应触发上下文切换。
pub unsafe fn timer_start_from_isr(handle: TimerHandle) -> bool {
    unsafe {
        let msg = TimerCommandMsg {
            cmd: CMD_START,
            handle,
            time: scheduler::tick_count(),
        };
        queue::queue_send_from_isr(&raw mut TIMER_CMD_QUEUE, &raw const msg as *const u8)
    }
}

/// 从中断停止定时器。返回 `true` 表示应触发上下文切换。
pub unsafe fn timer_stop_from_isr(handle: TimerHandle) -> bool {
    unsafe {
        let msg = TimerCommandMsg {
            cmd: CMD_STOP,
            handle,
            time: 0,
        };
        queue::queue_send_from_isr(&raw mut TIMER_CMD_QUEUE, &raw const msg as *const u8)
    }
}
