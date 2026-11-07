//! MPU 保护（Cortex-M7 Memory Protection Unit）。
//!
//! 对应 FreeRTOS MPU 端口使用的 ARMv7-M MPU：提供寄存器级配置接口。
//! 典型用法：为每个任务配置一段受保护区域（栈/外设），切换任务时重写区域。

use core::ptr;

// ---------------------------------------------------------------------
// 寄存器地址（Cortex-M7，ARMv7-M）
// ---------------------------------------------------------------------

pub const MPU_TYPE: *mut u32 = 0xE000_ED90 as *mut u32;
pub const MPU_CTRL: *mut u32 = 0xE000_ED94 as *mut u32;
pub const MPU_RNR: *mut u32 = 0xE000_ED98 as *mut u32;
pub const MPU_RBAR: *mut u32 = 0xE000_ED9C as *mut u32;
pub const MPU_RASR: *mut u32 = 0xE000_EDA0 as *mut u32;

// CTRL 位
pub const CTRL_ENABLE: u32 = 1 << 0;
pub const CTRL_HFNMIENA: u32 = 1 << 1;
pub const CTRL_PRIVDEFENA: u32 = 1 << 2;

// RASR.AP 访问权限（bits 26-24）
/// 禁止访问。
pub const AP_NO_ACCESS: u32 = 0b000 << 24;
/// 特权 RW，用户禁止。
pub const AP_PRIV_RW: u32 = 0b001 << 24;
/// 特权 RW，用户 RO。
pub const AP_PRIV_RW_USER_RO: u32 = 0b010 << 24;
/// 特权与用户均 RW。
pub const AP_FULL: u32 = 0b011 << 24;
/// 特权 RO，用户禁止。
pub const AP_PRIV_RO: u32 = 0b101 << 24;
/// 特权与用户均 RO。
pub const AP_RO: u32 = 0b110 << 24;

// RASR 内存属性（TEX/S/C/B，bits 21-16）
/// 普通可缓存内存（TEX=001, C=1, B=0）。
pub const ATTR_NORMAL: u32 = (0b001 << 19) | (0b0 << 18) | (0b1 << 17) | (0b0 << 16);
/// 设备内存（TEX=000, S=1, C=0, B=1）。
pub const ATTR_DEVICE: u32 = (0b000 << 19) | (0b1 << 18) | (0b0 << 17) | (0b1 << 16);
/// 强序内存（TEX=000, S=0, C=0, B=0）。
pub const ATTR_STRONGLY_ORDERED: u32 = 0;

// RASR.XN（bit 28）
const XN: u32 = 1 << 28;
// RASR.ENABLE（bit 0）
const ENABLE: u32 = 1 << 0;
// RBAR.VALID（bit 4）
const VALID: u32 = 1 << 4;

/// MPU 区域配置。
#[derive(Clone, Copy)]
pub struct MpuRegion {
    /// 基地址（须按区域大小对齐，内部会按 32 字节掩码）。
    pub base: u32,
    /// 区域大小（字节，向上取 2 的幂，最小 32，最大 4 GB）。
    pub size: u32,
    /// 访问权限（`AP_*` 常量之一）。
    pub access: u32,
    /// 内存属性（`ATTR_*` 常量之一）。
    pub attributes: u32,
    /// 是否允许取指执行（false 表示 XN=1，禁止执行）。
    pub executable: bool,
}

/// 计算 RASR 的 SIZE 字段（`2^(字段+1)` 字节，字段范围 4..31）。
pub fn mpu_size_field(size: u32) -> u32 {
    if size <= 32 {
        return 4;
    }
    let mut s = 32u32;
    let mut field = 4u32;
    while s < size && field < 31 {
        s <<= 1;
        field += 1;
    }
    field
}

/// 配置一个 MPU 区域（region 0..15）。
pub fn mpu_configure_region(region: u32, cfg: &MpuRegion) {
    unsafe {
        let r = region & 0xF;
        ptr::write_volatile(MPU_RNR, r);
        ptr::write_volatile(MPU_RBAR, (cfg.base & !0x1F) | VALID | r);
        let rasr = (if cfg.executable { 0 } else { XN })
            | (cfg.access & (0b111 << 24))
            | (cfg.attributes & 0x003F_0000)
            | (mpu_size_field(cfg.size) << 1)
            | ENABLE;
        ptr::write_volatile(MPU_RASR, rasr);
        // 数据同步屏障，保证配置立即生效
        core::arch::asm!("dsb", "isb");
    }
}

/// 禁用指定 MPU 区域。
pub fn mpu_disable_region(region: u32) {
    unsafe {
        ptr::write_volatile(MPU_RNR, region & 0xF);
        ptr::write_volatile(MPU_RASR, 0);
        core::arch::asm!("dsb", "isb");
    }
}

/// 使能 MPU。
///
/// - `privdefena`：特权级默认映射使能（否则特权级访问未配置区域也受保护）；
/// - `hfnmiena`：HardFault/NMI 期间 MPU 保持使能。
pub fn mpu_enable(privdefena: bool, hfnmiena: bool) {
    unsafe {
        let mut ctrl = CTRL_ENABLE;
        if privdefena {
            ctrl |= CTRL_PRIVDEFENA;
        }
        if hfnmiena {
            ctrl |= CTRL_HFNMIENA;
        }
        ptr::write_volatile(MPU_CTRL, ctrl);
        core::arch::asm!("dsb", "isb");
    }
}

/// 禁用 MPU。
pub fn mpu_disable() {
    unsafe {
        ptr::write_volatile(MPU_CTRL, 0);
        core::arch::asm!("dsb", "isb");
    }
}

/// 读取 MPU 区域数量。
pub fn mpu_region_count() -> u32 {
    unsafe { ptr::read_volatile(MPU_TYPE) & 0xFF }
}
