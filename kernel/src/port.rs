//! Cortex-M 端口层：异常帧、临界区、PendSV/SVC/SysTick 汇编。
//!
//! 对应 FreeRTOS 的 `port.c / portmacro.h`（Cortex-M 端口，ARM_CM4F 方案）。
//! 只面向 STM32H7 (Cortex-M7)。
//!
//! 任务栈布局（低地址 → 高地址，与 FreeRTOS 一致）：
//! ```text
//! [ r4..r11 (8 字) ][ EXC_RETURN (1 字) ][ 异常帧 (8 字，xPSR 在最高处) ]
//!  ↑ top_of_stack                          ↑ 硬件异常返回从此处弹帧
//! ```
//! - 初始异常帧**始终是基本帧（8 字）**，初始 EXC_RETURN = 0xFFFF_FFFD；
//! - 任务一旦使用 FPU，硬件懒堆叠会在异常入口自动压入扩展帧（s0-s15+FPSCR），
//!   PendSV 依据各任务自己保存的 EXC_RETURN（bit4）决定是否保存/恢复 s16-s31，
//!   因此使用与不使用 FPU 的任务可以正确混跑。

use core::arch::{asm, global_asm};
use core::mem::size_of;

use crate::config;
use crate::task::EntryFn;

// =====================================================================
// 异常帧（基本帧，8 字；FPU 扩展帧由硬件懒堆叠动态生成）
// =====================================================================

/// 硬件在异常入口自动压栈的基本寄存器布局（Cortex-M，8 字）。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ExceptionFrame {
    pub r0: u32,
    pub r1: u32,
    pub r2: u32,
    pub r3: u32,
    pub r12: u32,
    pub lr: u32,
    pub pc: u32,
    pub xpsr: u32,
}

/// 任务初始 EXC_RETURN：返回 Thread 模式、使用 PSP、基本帧（bit4=1）。
/// FPU 上下文由懒堆叠处理，任务首次使用 FPU 后其保存的 EXC_RETURN 会变为 bit4=0。
pub const EXC_RETURN_THREAD_PSP: u32 = 0xFFFF_FFFD;

/// 任务退出时跳转的地址（会触发 HardFault），对应 FreeRTOS `prvTaskExitError`。
#[allow(dead_code)]
pub const TASK_EXIT_LR: u32 = 0xFFFF_FFFD;

/// 任务栈头保留字节数：r4-r11(32) + EXC_RETURN(4) + 异常帧(32) = 68。
/// `#[task]` 宏用它把参数放到栈头下方。
pub const STACK_HEADER_SIZE: usize = 9 * 4 + 32;

/// NVIC 中断控制与状态寄存器（ICSR）。
const ICSR: *mut u32 = 0xE000_ED04 as *mut u32;
/// PendSV 置位位（bit 28）。
const PENDSVSET: u32 = 1 << 28;

// =====================================================================
// 临界区（对应 portENTER_CRITICAL / portEXIT_CRITICAL，基于 BASEPRI）
// =====================================================================

static mut CRITICAL_NESTING: u32 = 0;

/// 进入临界区：把 BASEPRI 设为 `MAX_SYSCALL_INTERRUPT_PRIORITY`。
///
/// # Safety
/// 必须在中断上下文或任务上下文中成对调用 `exit_critical`。
#[inline(always)]
pub unsafe fn enter_critical() {
    unsafe {
        asm!(
            "msr basepri, {prio}",
            "dsb",
            "isb",
            prio = in(reg) config::MAX_SYSCALL_INTERRUPT_PRIORITY,
        );
        CRITICAL_NESTING += 1;
    }
}

/// 退出临界区：嵌套计数归零时恢复 BASEPRI。
///
/// # Safety
/// 必须与 `enter_critical` 成对。
#[inline(always)]
pub unsafe fn exit_critical() {
    unsafe {
        if CRITICAL_NESTING == 0 {
            return;
        }
        CRITICAL_NESTING -= 1;
        if CRITICAL_NESTING == 0 {
            asm!(
                "msr basepri, {prio}",
                "dsb",
                "isb",
                prio = in(reg) 0u32,
            );
        }
    }
}

/// 触发 PendSV（任务级 yield），对应 `portYIELD` / `taskYIELD`。
///
/// # Safety
/// 无抢占语义，单核下安全。
#[inline(always)]
pub unsafe fn yield_from_task() {
    unsafe {
        core::ptr::write_volatile(ICSR, PENDSVSET);
        // 内存屏障，保证写立即生效
        asm!("dsb", "isb");
    }
}

/// ISR 中触发 yield，对应 `portYIELD_FROM_ISR`。
#[inline(always)]
pub unsafe fn yield_from_isr() {
    unsafe {
        core::ptr::write_volatile(ICSR, PENDSVSET);
        asm!("dsb", "isb");
    }
}

// =====================================================================
// 任务栈初始化
// =====================================================================

/// 在 `stack`（`size` 字节、8 字节对齐）上伪造初始任务上下文。
///
/// 返回初始 `top_of_stack`：指向 r4-r11 保存区（异常帧下方 9 个字），
/// 与 SVC/PendSV 汇编的恢复顺序严格一致。
///
/// # Safety
/// `stack` 必须在任务整个生命周期内有效且可写，
/// `size` 至少容纳 [`STACK_HEADER_SIZE`]（68 字节，另有参数时更大）。
pub unsafe fn init_task_stack(
    stack: *mut u8,
    size: usize,
    entry: EntryFn,
    param: *mut (),
) -> *mut u32 {
    let frame_size = size_of::<ExceptionFrame>(); // 32
    let top = unsafe { stack.add(size) } as usize;
    let frame_addr = (top - frame_size) & !0b111usize;
    let frame_ptr = frame_addr as *mut ExceptionFrame;

    // 基本异常帧（xPSR.T = 1；PC 保留 Thumb 位）
    unsafe {
        *frame_ptr = ExceptionFrame {
            r0: param as u32,
            r1: 0,
            r2: 0,
            r3: 0,
            r12: 0,
            lr: TASK_EXIT_LR,
            pc: entry as usize as u32,
            xpsr: 0x0100_0000,
        };
    }

    // EXC_RETURN 槽位（帧下方第 1 个字）
    let exc_return = (frame_addr - 4) as *mut u32;
    unsafe {
        *exc_return = EXC_RETURN_THREAD_PSP;
    }

    // top_of_stack = 帧下方 9 个字（r4-r11 保存区）
    (frame_ptr as *mut u32).offset(-9)
}

// =====================================================================
// 汇编：PendSV（上下文切换）、SVC（启动首任务）
// =====================================================================
//
// 符号约定：
//   - `pxCurrentTCB` 为指向“当前任务 TCB”的指针变量（静态存储），
//     其第一个字段必须是 `top_of_stack`；
//   - `vTaskSwitchContext` 在临界区内选出下一个任务并更新 `pxCurrentTCB`；
//   - 每个任务的 EXC_RETURN 保存在自己的栈上（r4-r11 上方），
//     这样“基本帧 / FPU 扩展帧”按任务区分（与 FreeRTOS 一致）。

#[cfg(not(feature = "fpu"))]
global_asm!(
    r#"
    .syntax unified
    .thumb

    .global PendSV_Handler
    .type PendSV_Handler, %function
PendSV_Handler:
    mrs     r0, psp
    isb
    ldr     r3, =pxCurrentTCB
    ldr     r2, [r3]
    stmdb   r0!, {{r4-r11, r14}}
    str     r0, [r2]

    stmdb   sp!, {{r3, r14}}
    mov     r0, {max_syscall}
    msr     basepri, r0
    dsb
    isb
    bl      vTaskSwitchContext
    mov     r0, #0
    msr     basepri, r0
    ldmia   sp!, {{r3, r14}}

    ldr     r1, [r3]
    ldr     r0, [r1]
    ldmia   r0!, {{r4-r11, r14}}
    msr     psp, r0
    isb
    bx      r14
    nop
    .size PendSV_Handler, . - PendSV_Handler
    "#,
    max_syscall = const config::MAX_SYSCALL_INTERRUPT_PRIORITY,
);

#[cfg(feature = "fpu")]
global_asm!(
    r#"
    .syntax unified
    .thumb
    .fpu fpv4-sp-d16

    .global PendSV_Handler
    .type PendSV_Handler, %function
PendSV_Handler:
    mrs     r0, psp
    isb
    tst     lr, #0x10
    it      eq
    vstmdbeq r0!, {{s16-s31}}
    ldr     r3, =pxCurrentTCB
    ldr     r2, [r3]
    stmdb   r0!, {{r4-r11, r14}}
    str     r0, [r2]

    stmdb   sp!, {{r3, r14}}
    mov     r0, {max_syscall}
    msr     basepri, r0
    dsb
    isb
    bl      vTaskSwitchContext
    mov     r0, #0
    msr     basepri, r0
    ldmia   sp!, {{r3, r14}}

    ldr     r1, [r3]
    ldr     r0, [r1]
    ldmia   r0!, {{r4-r11, r14}}
    tst     lr, #0x10
    it      eq
    vldmiaeq r0!, {{s16-s31}}
    msr     psp, r0
    isb
    bx      r14
    nop
    .size PendSV_Handler, . - PendSV_Handler
    "#,
    max_syscall = const config::MAX_SYSCALL_INTERRUPT_PRIORITY,
);

/// SVC 处理器：切换到第一个任务的上下文（Thread 模式 + PSP）。
///
/// 弹出任务的 r4-r11 与它自己的 EXC_RETURN，然后异常返回弹基本帧。
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SVC_Handler() -> ! {
    core::arch::naked_asm!(
        "ldr r3, =pxCurrentTCB",
        "ldr r1, [r3]",
        "ldr r0, [r1]",
        "ldmia r0!, {{r4-r11, r14}}",
        "msr psp, r0",
        "isb",
        "mov r0, #0",
        "msr basepri, r0",
        "bx r14",
    )
}

/// 启动第一个任务：从向量表恢复初始 MSP、开中断、触发 SVC。
///
/// 该函数不会返回（SVC 处理器把控制流交给首个任务）。
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn port_start_first_task() -> ! {
    core::arch::naked_asm!(
        "ldr r0, =0xE000ED08",
        "ldr r0, [r0]",
        "ldr r0, [r0]",
        "msr msp, r0",
        "cpsie i",
        "cpsie f",
        "dsb",
        "isb",
        "svc 0",
    )
}

// 兼容 cortex-m-rt 0.7 的短命名（其向量表引用 `PendSV` / `SVCall`），
// 同时保留 CMSIS 风格的长命名（`PendSV_Handler` / `SVC_Handler`）。
// 这里提供短命名的跳板，转发到上面的实现。
global_asm!(
    r#"
    .syntax unified
    .thumb

    .global PendSV
    .type PendSV, %function
PendSV:
    b PendSV_Handler
    .size PendSV, . - PendSV

    .global SVCall
    .type SVCall, %function
SVCall:
    b SVC_Handler
    .size SVCall, . - SVCall
    "#
);

// 编译期校验异常帧大小（与 FreeRTOS port 层一致）。
const _: () = {
    assert!(size_of::<ExceptionFrame>() == 32);
};
