// +-------------------------------------------------------------------------
//
//   taskmgr-rs - Win32 句柄所有权
//
//   文件:       src/infrastructure/native/handles.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Provides unique owners for Win32 and WTS allocations plus explicit owned-icon destruction.

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::RemoteDesktop::WTSFreeMemory;
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};

pub struct OwnedHandle {
    handle: HANDLE,
}

pub struct OwnedWtsMemory<T> {
    ptr: *mut T,
}

pub fn destroy_icon_handle(icon: HICON) {
    if !icon.is_null() {
        // 安全性: callers pass an icon handle they own and want to release.
        unsafe { DestroyIcon(icon) };
    }
}

impl<T> OwnedWtsMemory<T> {
    pub fn new(ptr: *mut T) -> Option<Self> {
        (!ptr.is_null()).then_some(Self { ptr })
    }

    pub fn as_ptr(&self) -> *mut T {
        self.ptr
    }
}

impl<T> Drop for OwnedWtsMemory<T> {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // 安全性: `OwnedWtsMemory` exclusively owns a buffer allocated by WTS APIs.
            unsafe { WTSFreeMemory(self.ptr as _) };
        }
    }
}

impl OwnedHandle {
    pub fn new(handle: HANDLE) -> Option<Self> {
        (!handle.is_null() && handle != INVALID_HANDLE_VALUE).then_some(Self { handle })
    }

    pub fn as_raw(&self) -> HANDLE {
        self.handle
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.handle.is_null() && self.handle != INVALID_HANDLE_VALUE {
            // 安全性: `OwnedHandle` exclusively owns this Win32 HANDLE.
            unsafe { CloseHandle(self.handle) };
        }
    }
}
