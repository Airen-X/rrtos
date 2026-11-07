//! STM32H7 实际项目启动示例：`cortex-m-rt` 裸机入口 + rrtos 内核。
//!
//! 功能：交替点亮 PC13 与 PA1 的两个 LED（**低电平点亮**）。
//!
//! 启动流程：
//! 1. 复位 → `#[cortex_m_rt::entry] reset()`：初始化 GPIO 与 SysTick；
//! 2. `reset()` 调用 `#[kernel::entry] rtos_main()`：spawn 两个 LED 任务；
//! 3. `#[kernel::entry]` 末尾自动调用 `start_scheduler()` 进入调度；
//! 4. SysTick 每 1ms 触发 `SysTick_Handler` → `kernel::tick_handler()`；
//! 5. PendSV/SVC 由内核提供（`PendSV_Handler`/`SVC_Handler` 及 `PendSV`/`SVCall` 别名）。

#![no_std]
#![no_main]

use cortex_m_rt::{entry, exception};
use stm32h7::stm32h743 as pac;

/// 系统时钟：默认 HSI 64 MHz（未配置外部晶振/PLL）。
/// 若你的板子配置了 PLL，请把这里改成实际的 SYSCLK 频率。
const SYSTEM_CLOCK_HZ: u32 = 64_000_000;

// ---------------------------------------------------------------------
// 硬件初始化
// ---------------------------------------------------------------------

/// 使能 GPIOA/GPIOC 时钟，并把 PA1、PC13 配置为推挽输出、初始高电平（熄灭）。
fn init_gpio() {
    let rcc = unsafe { &*pac::RCC::ptr() };
    rcc.ahb4enr
        .modify(|_, w| w.gpioaen().set_bit().gpiocen().set_bit());

    let gpioa = unsafe { &*pac::GPIOA::ptr() };
    let gpioc = unsafe { &*pac::GPIOC::ptr() };

    gpioa.moder.modify(|_, w| w.moder1().output());
    gpioc.moder.modify(|_, w| w.moder13().output());

    // 低电平有效：初始输出高电平 → 熄灭
    gpioa.bsrr.write(|w| w.bs1().set_bit());
    gpioc.bsrr.write(|w| w.bs13().set_bit());
}

/// 配置 SysTick：以处理器时钟为源，每 1ms 触发一次中断。
fn init_systick() {
    const SYST_CSR: *mut u32 = 0xE000_E010 as *mut u32;
    const SYST_RVR: *mut u32 = 0xE000_E014 as *mut u32;
    const SYST_CVR: *mut u32 = 0xE000_E018 as *mut u32;

    let reload = SYSTEM_CLOCK_HZ / kernel::tick_rate_hz() - 1;
    unsafe {
        SYST_RVR.write_volatile(reload);
        SYST_CVR.write_volatile(0);
        // CLKSOURCE=1（处理器时钟）| TICKINT=1（使能中断）| ENABLE=1
        SYST_CSR.write_volatile(0b111);
    }
}

// ---------------------------------------------------------------------
// 复位入口
// ---------------------------------------------------------------------

#[entry]
fn main() -> ! {
    init_gpio();
    init_systick();
    // 交给 rrtos（永不返回）
    rtos_main()
}

// ---------------------------------------------------------------------
// rrtos 内核入口
// ---------------------------------------------------------------------

#[kernel::entry]
fn rtos_main() {
    // 创建并启动两个 LED 任务
    kernel::spawn(led_pc13());
    kernel::spawn(led_pa1());
    // 函数体结束后自动调用 kernel::start_scheduler()
}

// ---------------------------------------------------------------------
// 任务
// ---------------------------------------------------------------------

/// PC13 LED：500ms 亮、500ms 灭。
#[kernel::task(stack = 1024, priority = 1)]
extern "C" fn led_pc13() -> ! {
    let gpioc = unsafe { &*pac::GPIOC::ptr() };
    loop {
        gpioc.bsrr.write(|w| w.br13().set_bit()); // 低电平 → 点亮
        kernel::task_delay(500);
        gpioc.bsrr.write(|w| w.bs13().set_bit()); // 高电平 → 熄灭
        kernel::task_delay(500);
    }
}

/// PA1 LED：先延时 500ms 错开相位，再 500ms 亮、500ms 灭 → 与 PC13 交替点亮。
#[kernel::task(stack = 1024, priority = 1)]
extern "C" fn led_pa1() -> ! {
    let gpioa = unsafe { &*pac::GPIOA::ptr() };
    kernel::task_delay(500);
    loop {
        gpioa.bsrr.write(|w| w.br1().set_bit()); // 低电平 → 点亮
        kernel::task_delay(500);
        gpioa.bsrr.write(|w| w.bs1().set_bit()); // 高电平 → 熄灭
        kernel::task_delay(500);
    }
}

// ---------------------------------------------------------------------
// SysTick 异常：驱动内核节拍
// ---------------------------------------------------------------------

#[exception]
fn SysTick() {
    kernel::tick_handler();
}

// PendSV / SVC 处理器由 kernel 提供，无需在此定义。

// ---------------------------------------------------------------------
// 恐慌处理
// ---------------------------------------------------------------------

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // 简单实现：进入死循环（真实项目可在此做复位/指示等）
    loop {}
}
