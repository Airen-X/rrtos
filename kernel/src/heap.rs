//! 内存管理（简化版 heap_4：首次适配 + 相邻空闲块合并）。
//!
//! 对应 FreeRTOS `heap_4.c` 的 `pvPortMalloc` / `vPortFree`。
//! 用法：先 [`heap_init`] 指定一段静态内存作为堆，再用 [`kmalloc`] / [`kfree`]。

use core::ptr;

/// 对齐粒度（Cortex-M 要求 8 字节对齐）。
const ALIGN: usize = 8;
/// 块头中“已分配”标志（占用 size 的最低位）。
const BLOCK_ALLOCATED: usize = 1;

/// 堆块头：位于每块数据之前。
#[repr(C)]
struct Block {
    /// 块总大小（含头，8 字节对齐），bit0 = 已分配标志。
    size: usize,
    /// 空闲链表下一块（仅空闲块有效）。
    next: *mut Block,
}

const fn align_up(n: usize, a: usize) -> usize {
    (n + a - 1) & !(a - 1)
}

const HEADER_SIZE: usize = align_up(core::mem::size_of::<Block>(), ALIGN);

static mut HEAP_BASE: *mut u8 = ptr::null_mut();
static mut HEAP_END: *mut u8 = ptr::null_mut();
static mut FREE_LIST: *mut Block = ptr::null_mut();

#[inline]
unsafe fn block_size(b: *mut Block) -> usize {
    unsafe { (*b).size & !BLOCK_ALLOCATED }
}

#[inline]
unsafe fn is_allocated(b: *mut Block) -> bool {
    unsafe { ((*b).size & BLOCK_ALLOCATED) != 0 }
}

#[inline]
unsafe fn set_allocated(b: *mut Block, alloc: bool) {
    unsafe {
        if alloc {
            (*b).size |= BLOCK_ALLOCATED;
        } else {
            (*b).size &= !BLOCK_ALLOCATED;
        }
    }
}

/// 按地址升序插入空闲链表。
unsafe fn insert_free(block: *mut Block) {
    unsafe {
        let mut prev: *mut Block = ptr::null_mut();
        let mut cur = FREE_LIST;
        while !cur.is_null() && (cur as usize) < (block as usize) {
            prev = cur;
            cur = (*cur).next;
        }
        (*block).next = cur;
        if prev.is_null() {
            FREE_LIST = block;
        } else {
            (*prev).next = block;
        }
    }
}

/// 从空闲链表移除指定块。
unsafe fn remove_free(block: *mut Block) {
    unsafe {
        let mut prev: *mut Block = ptr::null_mut();
        let mut cur = FREE_LIST;
        while !cur.is_null() && cur != block {
            prev = cur;
            cur = (*cur).next;
        }
        if cur == block {
            if prev.is_null() {
                FREE_LIST = (*cur).next;
            } else {
                (*prev).next = (*cur).next;
            }
        }
    }
}

/// 初始化堆：把 `region[0..size]` 作为堆区（内部会做 8 字节对齐）。
///
/// # Safety
/// `region` 必须指向在堆使用期间始终有效的可写内存。
pub unsafe fn heap_init(region: *mut u8, size: usize) {
    unsafe {
        let base = (region as usize + ALIGN - 1) & !(ALIGN - 1);
        let end = (region as usize + size) & !(ALIGN - 1);
        HEAP_BASE = base as *mut u8;
        HEAP_END = end as *mut u8;

        let block = base as *mut Block;
        (*block).size = end - base;
        (*block).next = ptr::null_mut();
        FREE_LIST = block;
    }
}

/// 分配 `size` 字节，返回 8 字节对齐的数据指针；失败返回 `null`。
///
/// 对应 `pvPortMalloc`。
pub unsafe fn kmalloc(size: usize) -> *mut u8 {
    unsafe {
        if size == 0 {
            return ptr::null_mut();
        }
        let total = align_up(size, ALIGN) + HEADER_SIZE;

        // 首次适配
        let mut prev: *mut Block = ptr::null_mut();
        let mut cur = FREE_LIST;
        while !cur.is_null() {
            if block_size(cur) >= total {
                // 剩余空间足够再放一个最小块时才分裂
                let remain = block_size(cur) - total;
                if remain >= HEADER_SIZE + ALIGN {
                    let new_block = (cur as usize + total) as *mut Block;
                    (*new_block).size = remain;
                    (*new_block).next = (*cur).next;
                    (*cur).size = total;
                } else {
                    // 不分裂，整个块都分出去
                }
                // 摘除 cur
                if prev.is_null() {
                    FREE_LIST = (*cur).next;
                } else {
                    (*prev).next = (*cur).next;
                }
                set_allocated(cur, true);
                return (cur as *mut u8).add(HEADER_SIZE);
            }
            prev = cur;
            cur = (*cur).next;
        }
        ptr::null_mut()
    }
}

/// 释放 [`kmalloc`] 返回的指针，并合并相邻空闲块。
///
/// 对应 `vPortFree`。
pub unsafe fn kfree(ptr: *mut u8) {
    unsafe {
        if ptr.is_null() {
            return;
        }
        let block = (ptr as usize - HEADER_SIZE) as *mut Block;
        if !is_allocated(block) {
            return; // 重复释放 / 非法指针
        }
        set_allocated(block, false);

        // 合并物理后继（若空闲）
        let next = (block as usize + block_size(block)) as *mut Block;
        if (next as *mut u8) < HEAP_END && !is_allocated(next) {
            let merged = block_size(block) + block_size(next);
            remove_free(next);
            (*block).size = merged;
        }

        // 合并物理前驱（在空闲链表中找“结尾 == block 开头”的空闲块）
        let mut prev_phys: *mut Block = ptr::null_mut();
        let mut cur = FREE_LIST;
        while !cur.is_null() {
            if (cur as usize + block_size(cur)) == (block as usize) {
                prev_phys = cur;
                break;
            }
            cur = (*cur).next;
        }

        if !prev_phys.is_null() {
            let merged = block_size(prev_phys) + block_size(block);
            remove_free(prev_phys);
            (*prev_phys).size = merged;
            insert_free(prev_phys);
        } else {
            insert_free(block);
        }
    }
}

/// 返回堆中当前可用的最大空闲块大小（字节）。
pub fn heap_free_bytes() -> usize {
    unsafe {
        let mut max = 0usize;
        let mut cur = FREE_LIST;
        while !cur.is_null() {
            let sz = block_size(cur);
            if sz > max {
                max = sz;
            }
            cur = (*cur).next;
        }
        max
    }
}

/// 返回堆总大小（字节）。
pub fn heap_total_bytes() -> usize {
    unsafe { (HEAP_END as usize).saturating_sub(HEAP_BASE as usize) }
}

// =====================================================================
// #[global_allocator] 桥接：让 Rust 原生 `alloc`（Vec/String/Box…）可用
// =====================================================================

/// 全局分配器：桥接 [`kmalloc`] / [`kfree`]，供 `#[global_allocator]` 使用。
///
/// 用法（在应用 crate 根模块）：
/// ```ignore
/// #[global_allocator]
/// static ALLOCATOR: kernel::KernelAllocator = kernel::KernelAllocator;
/// // 或直接 kernel::use_alloc!();
/// ```
pub struct KernelAllocator;

unsafe impl core::alloc::GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        unsafe {
            if layout.size() == 0 {
                return core::ptr::NonNull::dangling().as_ptr();
            }
            let align = layout.align().max(8);
            if align <= 8 {
                // kmalloc 天然 8 字节对齐
                return kmalloc(layout.size());
            }
            // 对齐大于 8：多分配一段，在对齐后的指针前 8 字节保存原始指针，
            // 以便 dealloc 时恢复。
            let raw = kmalloc(layout.size() + align);
            if raw.is_null() {
                return ptr::null_mut();
            }
            let aligned = ((raw as usize + 8 + align - 1) & !(align - 1)) as *mut u8;
            *((aligned as *mut usize).offset(-1)) = raw as usize;
            aligned
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        unsafe {
            if ptr.is_null() || layout.size() == 0 {
                return;
            }
            let align = layout.align().max(8);
            if align <= 8 {
                kfree(ptr);
            } else {
                let raw = *((ptr as *mut usize).offset(-1)) as *mut u8;
                kfree(raw);
            }
        }
    }
}

/// 注册 [`KernelAllocator`] 为全局分配器（等价于手写 `#[global_allocator]`）。
/// 并建立静态堆
/// 在应用 crate 根模块调用一次：
/// ```ignore
/// extern crate alloc;
/// kernel::use_alloc!(HEAPNAME:SIZE);          // 之后即可使用 Vec/String/Box 等
/// ```    
/// #[repr(align(8))]
/// SIZE
#[macro_export]
macro_rules! use_alloc {
    ($name:ident:$size:expr) => {
        #[global_allocator]
        static __RRTOS_GLOBAL_ALLOCATOR__: $crate::KernelAllocator = $crate::KernelAllocator;
        static mut $name: [u8; $size] = [0; $size];
    };
}
