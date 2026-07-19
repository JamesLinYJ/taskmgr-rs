// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 菜单资源所有权
//
//   文件:       src/ui/runtime_menu.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 运行时菜单句柄包装。
//! 这层把裸 `HMENU` 收成带生命周期的 Rust 类型，减少菜单创建、转移所有权和
//! 销毁时的样板代码。

use std::ptr::null;

use windows_sys::Win32::Foundation::{ERROR_GEN_FAILURE, GetLastError};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateMenu, CreatePopupMenu, DestroyMenu, HMENU, MF_CHECKED, MF_DISABLED,
    MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
};

use crate::infrastructure::native::to_wide_null;

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
    pub fn new() -> Result<Self, u32> {
        // `PopupMenu` 拥有一个独立的弹出菜单句柄。
        // 安全性: creating a new menu has no preconditions beyond process Win32 availability.
        let handle = unsafe { CreatePopupMenu() };
        if handle.is_null() {
            Err(last_error_or_gen_failure())
        } else {
            Ok(Self { handle })
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

    pub fn append_item(
        &mut self,
        command_id: u16,
        label: &str,
        state: MenuItemState,
    ) -> Result<(), u32> {
        // 统一在这里把 Rust 侧状态翻译成 Win32 `AppendMenuW` 标志位。
        let wide = to_wide_null(label);
        let mut flags = MF_STRING;
        if !state.enabled {
            flags |= MF_GRAYED | MF_DISABLED;
        }
        if state.checked {
            flags |= MF_CHECKED;
        }
        // 安全性: `self.handle` is owned by this menu and `wide` is a live NUL-terminated label.
        if unsafe { AppendMenuW(self.handle, flags, usize::from(command_id), wide.as_ptr()) } == 0 {
            Err(last_error_or_gen_failure())
        } else {
            Ok(())
        }
    }

    pub fn append_separator(&mut self) -> Result<(), u32> {
        // 分隔线不携带文本和命令 ID。
        // 安全性: appending a separator does not dereference the null text pointer.
        if unsafe { AppendMenuW(self.handle, MF_SEPARATOR, 0, null()) } == 0 {
            Err(last_error_or_gen_failure())
        } else {
            Ok(())
        }
    }

    pub fn append_submenu(&mut self, label: &str, submenu: PopupMenu) -> Result<(), u32> {
        // 子菜单会接管传入 `PopupMenu` 的句柄所有权。
        let wide = to_wide_null(label);
        let submenu_handle = submenu.into_raw();
        // 安全性: both menu handles are valid and the label buffer lives for the call.
        let appended = unsafe {
            AppendMenuW(
                self.handle,
                MF_POPUP | MF_STRING,
                submenu_handle as usize,
                wide.as_ptr(),
            ) != 0
        };
        if !appended {
            let error = last_error_or_gen_failure();
            // 安全性: ownership was taken from `submenu`; on failure this function must release it.
            unsafe { DestroyMenu(submenu_handle) };
            Err(error)
        } else {
            Ok(())
        }
    }
}

impl Drop for PopupMenu {
    fn drop(&mut self) {
        // 安全性: a non-null handle is still owned by this wrapper and has not been transferred.
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
    pub fn new() -> Result<Self, u32> {
        // `MenuBar` 对应窗口主菜单，而不是右键弹出菜单。
        // 安全性: creating a new menu bar has no additional preconditions.
        let handle = unsafe { CreateMenu() };
        if handle.is_null() {
            Err(last_error_or_gen_failure())
        } else {
            Ok(Self { handle })
        }
    }

    pub fn as_raw(&self) -> HMENU {
        self.handle
    }

    pub fn append_submenu(&mut self, label: &str, submenu: PopupMenu) -> Result<(), u32> {
        // 主菜单只接受子级弹出菜单，不直接追加普通命令项。
        let wide = to_wide_null(label);
        let submenu_handle = submenu.into_raw();
        // 安全性: both menu handles are valid and the label buffer lives for the call.
        let appended = unsafe {
            AppendMenuW(
                self.handle,
                MF_POPUP | MF_STRING,
                submenu_handle as usize,
                wide.as_ptr(),
            ) != 0
        };
        if !appended {
            let error = last_error_or_gen_failure();
            // 安全性: ownership was transferred out of `submenu`; release it on append failure.
            unsafe { DestroyMenu(submenu_handle) };
            Err(error)
        } else {
            Ok(())
        }
    }
}

fn last_error_or_gen_failure() -> u32 {
    // SAFETY: GetLastError has no preconditions and is called immediately after a menu API fails.
    let error = unsafe { GetLastError() };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

impl Drop for MenuBar {
    fn drop(&mut self) {
        // 安全性: a non-null handle is still owned by this wrapper and has not been attached.
        unsafe {
            if !self.handle.is_null() {
                DestroyMenu(self.handle);
            }
        }
    }
}
