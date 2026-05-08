//! 运行时菜单句柄包装。
//! 这层把裸 `HMENU` 收成带生命周期的 Rust 类型，减少菜单创建、转移所有权和
//! 销毁时的样板代码。

use std::ptr::null;

use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateMenu, CreatePopupMenu, DestroyMenu, HMENU, MF_CHECKED, MF_DISABLED,
    MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
};

use crate::winutil::to_wide_null;

#[derive(Clone, Copy)]
pub struct MenuItemState {
    // 菜单项状态被单独抽成结构体，便于统一描述启用/勾选语义。
    pub enabled: bool,
    pub checked: bool,
}

impl MenuItemState {
    pub const ENABLED: Self = Self {
        enabled: true,
        checked: false,
    };

    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            checked: false,
        }
    }

    pub const fn checked() -> Self {
        Self {
            enabled: true,
            checked: true,
        }
    }
}

pub struct PopupMenu {
    handle: HMENU,
}

impl PopupMenu {
    pub fn new() -> Option<Self> {
        // `PopupMenu` 拥有一个独立的弹出菜单句柄。
        // SAFETY: creating a new menu has no preconditions beyond process Win32 availability.
        let handle = unsafe { CreatePopupMenu() };
        if handle.is_null() {
            None
        } else {
            Some(Self { handle })
        }
    }

    pub fn as_raw(&self) -> HMENU {
        // 允许只读借出底层句柄给 Win32 API 使用。
        self.handle
    }

    pub fn into_raw(mut self) -> HMENU {
        // 把所有权转移给调用方，避免 `Drop` 再次销毁同一个菜单。
        let handle = self.handle;
        self.handle = std::ptr::null_mut();
        handle
    }

    pub fn append_item(&mut self, command_id: u16, label: &str, state: MenuItemState) -> bool {
        // 统一在这里把 Rust 侧状态翻译成 Win32 `AppendMenuW` 标志位。
        let wide = to_wide_null(label);
        let mut flags = MF_STRING;
        if !state.enabled {
            flags |= MF_GRAYED | MF_DISABLED;
        }
        if state.checked {
            flags |= MF_CHECKED;
        }
        // SAFETY: `self.handle` is owned by this menu and `wide` is a live NUL-terminated label.
        unsafe { AppendMenuW(self.handle, flags, usize::from(command_id), wide.as_ptr()) != 0 }
    }

    pub fn append_separator(&mut self) -> bool {
        // 分隔线不携带文本和命令 ID。
        // SAFETY: appending a separator does not dereference the null text pointer.
        unsafe { AppendMenuW(self.handle, MF_SEPARATOR, 0, null()) != 0 }
    }

    pub fn append_submenu(&mut self, label: &str, submenu: PopupMenu) -> bool {
        // 子菜单会接管传入 `PopupMenu` 的句柄所有权。
        let wide = to_wide_null(label);
        let submenu_handle = submenu.into_raw();
        // SAFETY: both menu handles are valid and the label buffer lives for the call.
        let appended = unsafe {
            AppendMenuW(
                self.handle,
                MF_POPUP | MF_STRING,
                submenu_handle as usize,
                wide.as_ptr(),
            ) != 0
        };
        if !appended {
            // SAFETY: ownership was taken from `submenu`; on failure this function must release it.
            unsafe { DestroyMenu(submenu_handle) };
        }
        appended
    }
}

impl Drop for PopupMenu {
    fn drop(&mut self) {
        // SAFETY: a non-null handle is still owned by this wrapper and has not been transferred.
        unsafe {
            // 只有句柄仍然归当前对象所有时才销毁。
            if !self.handle.is_null() {
                DestroyMenu(self.handle);
            }
        }
    }
}

pub struct MenuBar {
    handle: HMENU,
}

impl MenuBar {
    pub fn new() -> Option<Self> {
        // `MenuBar` 对应窗口主菜单，而不是右键弹出菜单。
        // SAFETY: creating a new menu bar has no additional preconditions.
        let handle = unsafe { CreateMenu() };
        if handle.is_null() {
            None
        } else {
            Some(Self { handle })
        }
    }

    pub fn into_raw(mut self) -> HMENU {
        // 转移所有权给窗口，防止 `Drop` 在附加后再次销毁。
        let handle = self.handle;
        self.handle = std::ptr::null_mut();
        handle
    }

    pub fn append_submenu(&mut self, label: &str, submenu: PopupMenu) -> bool {
        // 主菜单只接受子级弹出菜单，不直接追加普通命令项。
        let wide = to_wide_null(label);
        let submenu_handle = submenu.into_raw();
        // SAFETY: both menu handles are valid and the label buffer lives for the call.
        let appended = unsafe {
            AppendMenuW(
                self.handle,
                MF_POPUP | MF_STRING,
                submenu_handle as usize,
                wide.as_ptr(),
            ) != 0
        };
        if !appended {
            // SAFETY: ownership was transferred out of `submenu`; release it on append failure.
            unsafe { DestroyMenu(submenu_handle) };
        }
        appended
    }
}

impl Drop for MenuBar {
    fn drop(&mut self) {
        // SAFETY: a non-null handle is still owned by this wrapper and has not been attached.
        unsafe {
            if !self.handle.is_null() {
                DestroyMenu(self.handle);
            }
        }
    }
}
