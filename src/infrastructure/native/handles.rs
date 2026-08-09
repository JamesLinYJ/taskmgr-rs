// +-------------------------------------------------------------------------
//
//   taskmgr-rs - Win32 句柄所有权
//
//   文件:       src/infrastructure/native/handles.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Provides unique owners for Win32 handles, WTS allocations, and non-shared icons.
//!
//! Raw ownership can only enter these types through `unsafe` constructors. A non-null raw value
//! does not prove allocator provenance, unique ownership, or compatibility with the destructor.

use std::cell::Cell;
use std::marker::PhantomData;
use std::num::NonZeroIsize;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::RemoteDesktop::WTSFreeMemory;
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};

#[must_use = "dropping the owner closes the Win32 handle"]
pub struct OwnedHandle {
    handle: HANDLE,
}

#[must_use = "dropping the owner frees the WTS allocation"]
pub struct OwnedWtsMemory<T> {
    ptr: *mut T,
}

/// Unique owner of a non-shared icon that must be released with `DestroyIcon`.
///
/// Win32 USER handles are not tied to the thread that created them, so the owner may move between
/// threads. `Cell` deliberately keeps shared references from being `Sync`: callers only borrow the
/// raw `HICON` for the duration of a synchronous Win32 call.
#[must_use = "dropping the owner destroys the icon"]
pub struct OwnedIcon {
    icon: NonZeroIsize,
    _not_sync: PhantomData<Cell<()>>,
}

impl<T> OwnedWtsMemory<T> {
    /// Takes ownership of a WTS allocation.
    ///
    /// Null is accepted and returns `None`.
    ///
    /// # Safety
    ///
    /// A non-null `ptr` must identify a live allocation returned by a WTS API whose documented
    /// release function is `WTSFreeMemory`. The caller must transfer unique ownership and must not
    /// use or free the allocation after this call succeeds.
    pub unsafe fn from_raw(ptr: *mut T) -> Option<Self> {
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
    /// Takes ownership of a Win32 kernel handle.
    ///
    /// Null and `INVALID_HANDLE_VALUE` are accepted and return `None`.
    ///
    /// # Safety
    ///
    /// Any other `handle` must be a live, uniquely owned handle whose documented release function
    /// is `CloseHandle`. The caller must not use or close it after this call succeeds. Pseudo
    /// handles and handles released by a different API must not be passed.
    pub unsafe fn from_raw(handle: HANDLE) -> Option<Self> {
        (!handle.is_null() && handle != INVALID_HANDLE_VALUE).then_some(Self { handle })
    }

    pub fn as_raw(&self) -> HANDLE {
        self.handle
    }
}

impl OwnedIcon {
    /// Takes ownership of a non-shared icon.
    ///
    /// Null is accepted and returns `None`.
    ///
    /// # Safety
    ///
    /// A non-null `icon` must be a live icon that the caller uniquely owns and is required to
    /// release with `DestroyIcon` (for example, a successful `CopyIcon` result or `LoadImageW`
    /// result loaded without `LR_SHARED`). Shared, class-owned, or window-owned icons must not be
    /// passed. The caller must not use or destroy the icon after this call succeeds.
    pub unsafe fn from_raw(icon: HICON) -> Option<Self> {
        NonZeroIsize::new(icon as isize).map(|icon| Self {
            icon,
            _not_sync: PhantomData,
        })
    }

    /// Borrows the icon handle for a synchronous Win32 call.
    pub fn as_raw(&self) -> HICON {
        self.icon.get() as HICON
    }
}

impl Drop for OwnedIcon {
    fn drop(&mut self) {
        // 安全性: construction requires unique ownership of a non-shared icon compatible with
        // `DestroyIcon`; the non-zero handle is released exactly once here.
        unsafe { DestroyIcon(self.as_raw()) };
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

#[cfg(test)]
mod tests {
    use super::OwnedIcon;

    #[test]
    fn owned_icon_can_transfer_between_threads() {
        fn assert_send<T: Send>() {}
        assert_send::<OwnedIcon>();
    }
}
