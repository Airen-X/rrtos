//! STM32H7 实际项目启动示例（stm32h7xx-hal 版本）。
//!
//! 功能与 `examples/firmware/` 相同：交替点亮 PC13 与 PA1 两个低电平 LED。
//! 区别在于使用 stm32h7xx-hal 的高层 API（PWR/RCC 时钟树、GPIO 类型安全引脚），
//! 并把 GPIO 引脚的“所有权”作为任务参数移动进任务（演示非 Copy 参数）。

#![no_std]
#![no_main]

extern crate alloc;

use cortex_m_rt::entry;
use stm32h7xx_hal::gpio::{Output, PushPull, PA1, PC13};
use stm32h7xx_hal::{pac, prelude::*};

// 把内核堆注册为 Rust 全局分配器：之后可用 Vec/String/Box 等
kernel::use_alloc!();

// 类型别名：任务参数用的具体引脚类型
type LedPc13 = PC13<Output<PushPull>>;
type LedPa1 = PA1<Output<PushPull>>;

// 静态堆（供 #[global_allocator] 使用）
static mut HEAP: [u8; 16 * 1024] = [0; 16 * 1024];

// ---------------------------------------------------------------------
// 复位入口
// ---------------------------------------------------------------------

#[entry]
fn reset() -> ! {
    let dp = pac::Peripherals::take().unwrap();

    // 电源约束 + 时钟：SYSCLK = 100 MHz（PLL）
    let pwr = dp.PWR.constrain();
    let pwrcfg = pwr.freeze();
    let rcc = dp.RCC.constrain();
    let ccdr = rcc.sys_ck(100.MHz()).freeze(pwrcfg, &dp.SYSCFG);

    // 初始化内核堆（必须先于任何分配）
    unsafe {
        kernel::heap_init(
            core::ptr::addr_of_mut!(HEAP) as *mut u8,
            16 * 1024,
        );
    }

    // 演示 Rust 原生 alloc（Vec 分配并自动释放）
    {
        let mut v: alloc::vec::Vec<u32> = alloc::vec::Vec::new();
        v.push(0xDEAD_BEEF);
        let mut s: alloc::string::String = alloc::string::String::new();
        s.push_str("rrtos-hal");
        let _ = v[0];
        let _ = s.len();
    }

    // GPIO：把引脚配置为推挽输出，初始高电平（低电平有效 → 熄灭）
    let gpioa = dp.GPIOA.split(ccdr.peripheral.GPIOA);
    let gpioc = dp.GPIOC.split(ccdr.peripheral.GPIOC);
    let mut led_pa1_pin = gpioa.pa1.into_push_pull_output();
    let mut led_pc13_pin = gpioc.pc13.into_push_pull_output();
    led_pa1_pin.set_high();
    led_pc13_pin.set_high();

    // SysTick：用冻结后的实际系统时钟计算 1ms 重载值（内核内部绑定处理器）
    kernel::systick_init(1000, ccdr.clocks.sys_ck().raw());

    // 内核：这里手动初始化（因为要把引脚所有权移交给任务，不走无参的 #[kernel::entry]）
    kernel::init(kernel::KernelConfig::default());
    kernel::spawn(led_pc13(led_pc13_pin));
    kernel::spawn(led_pa1(led_pa1_pin));
    kernel::start_scheduler()
}

// ---------------------------------------------------------------------
// 任务：每个任务独占一个 GPIO 引脚（所有权随参数移入）
// ---------------------------------------------------------------------

/// PC13 LED：500ms 亮、500ms 灭。
#[kernel::task(stack = 1024, priority = 1)]
extern "C" fn led_pc13(mut led: LedPc13) -> ! {
    loop {
        led.set_low(); // 低电平 → 点亮
        kernel::task_delay(500);
        led.set_high(); // 高电平 → 熄灭
        kernel::task_delay(500);
    }
}

/// PA1 LED：先错开 500ms 相位，再 500ms 亮、500ms 灭 → 与 PC13 交替点亮。
#[kernel::task(stack = 1024, priority = 1)]
extern "C" fn led_pa1(mut led: LedPa1) -> ! {
    kernel::task_delay(500);
    loop {
        led.set_low(); // 低电平 → 点亮
        kernel::task_delay(500);
        led.set_high(); // 高电平 → 熄灭
        kernel::task_delay(500);
    }
}

// SysTick / PendSV / SVC 处理器均由内核提供，无需在此定义。

// ---------------------------------------------------------------------
// 恐慌处理
// ---------------------------------------------------------------------

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}
