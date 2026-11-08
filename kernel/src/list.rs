//! FreeRTOS 风格的双向链表实现。
//!
//! 对应 FreeRTOS 的 `list.h / list.c`：
//! - [`ListItem`] 对应 `ListItem_t`（`pvOwner`、`xItemValue`、`pvContainer`、前后指针）；
//! - [`List`] 对应 `List_t`（`uxNumberOfItems`、`pxIndex`、`xListEnd` 哨兵）。
//!
//! 就绪列表使用 FIFO 插入（`insert_end`），每个优先级一个列表；
//! 延时列表按唤醒时刻排序插入（`insert`）。

use core::ptr;

/// 列表项，可被插入到某个 [`List`] 中。
///
/// `owner` 通常指向“拥有”该列表项的对象（TCB / Timer 等），
/// `item_value` 用于排序（就绪列表中为优先级，延时列表中为唤醒时刻）。
#[derive(Clone, Copy)]
pub struct ListItem {
    pub item_value: u32,
    pub owner: *mut (),
    pub container: *mut List,
    pub next: *mut ListItem,
    pub prev: *mut ListItem,
}

impl ListItem {
    /// 构造一个“游离”的列表项（不在任何列表中）。
    pub const fn new() -> Self {
        Self {
            item_value: 0,
            owner: ptr::null_mut(),
            container: ptr::null_mut(),
            next: ptr::null_mut(),
            prev: ptr::null_mut(),
        }
    }
}

/// 链表头，内含一个用作哨兵的 [`ListItem`]（`end`）。
pub struct List {
    pub number_of_items: usize,
    pub index: *mut ListItem,
    pub end: ListItem,
}

impl List {
    /// 构造未初始化的列表，随后必须调用 [`List::initialise`]。
    pub const fn new() -> Self {
        Self {
            number_of_items: 0,
            index: ptr::null_mut(),
            end: ListItem::new(),
        }
    }

    /// 初始化列表：哨兵自环，`index` 指向哨兵。
    ///
    /// # Safety
    /// 只能在列表未使用时调用一次（通常在调度器启动前）。
    pub unsafe fn initialise(&mut self) {
        self.end.next = &raw mut self.end;
        self.end.prev = &raw mut self.end;
        self.end.item_value = u32::MAX;
        self.end.container = ptr::null_mut();
        self.index = &mut self.end;
        self.number_of_items = 0;
    }

    /// 按 `item_value` 升序插入（值越小越靠前，用于延时列表：值 = 唤醒时刻）。
    ///
    /// 相同值的项插入到已有相同值项之后，从而保持 FIFO。
    /// 头项（`xListEnd.pxNext`）即最小值项。
    ///
    /// # Safety
    /// `item` 不得已在任何列表中。
    pub unsafe fn insert(&mut self, item: *mut ListItem) {
        let mut it: *mut ListItem = &mut self.end;

        // 与 FreeRTOS 一致：portMAX_DELAY 直接插到末尾之前。
        if (*item).item_value != u32::MAX {
            while (*(*it).next).item_value <= (*item).item_value {
                it = (*it).next;
            }
        } else {
            it = self.end.prev;
        }

        let next = (*it).next;
        (*item).next = next;
        (*item).prev = it;
        (*it).next = item;
        (*next).prev = item;
        (*item).container = self;
        self.number_of_items += 1;
    }

    /// 在末尾插入（FIFO，用于就绪列表，配合 `get_owner_of_next_entry` 实现同优先级轮转）。
    ///
    /// # Safety
    /// `item` 不得已在任何列表中。
    pub unsafe fn insert_end(&mut self, item: *mut ListItem) {
        let prev = self.end.prev;
        (*item).next = &mut self.end;
        (*item).prev = prev;
        (*prev).next = item;
        self.end.prev = item;
        (*item).container = self;
        self.number_of_items += 1;
    }

    /// 将 `item` 从它所在的列表移除，返回移除后该列表剩余项数。
    ///
    /// # Safety
    /// `item` 必须在某个列表中；不在时返回 0。
    pub unsafe fn remove(item: *mut ListItem) -> usize {
        let container = (*item).container;
        if container.is_null() {
            return 0;
        }
        let next = (*item).next;
        let prev = (*item).prev;
        (*prev).next = next;
        (*next).prev = prev;

        if (*container).index == item {
            (*container).index = prev;
        }

        (*item).container = ptr::null_mut();
        (*container).number_of_items -= 1;
        (*item).next = ptr::null_mut();
        (*item).prev = ptr::null_mut();
        (*container).number_of_items
    }

    /// 列表是否为空。
    pub unsafe fn is_empty(&self) -> bool {
        self.number_of_items == 0
    }

    /// 返回“头项”的 owner（`xListEnd.next` 的 owner）。
    ///
    /// 对应 FreeRTOS `listGET_OWNER_OF_HEAD_ENTRY`。空列表返回 `null`。
    pub unsafe fn head_owner(&self) -> *mut () {
        (*self.end.next).owner
    }

    /// 前移 `index`（越过哨兵回绕）并返回新位置项的 owner。
    ///
    /// 对应 FreeRTOS `listGET_OWNER_OF_NEXT_ENTRY`，用于同优先级任务轮转调度。
    pub unsafe fn get_owner_of_next_entry(&mut self) -> *mut () {
        self.index = (*self.index).next;
        if self.index == (&mut self.end as *mut ListItem) {
            self.index = (*self.index).next;
        }
        (*self.index).owner
    }
}
