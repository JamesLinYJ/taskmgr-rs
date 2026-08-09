// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 原生资源加载
//
//   文件:       src/ui/assets.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 运行时内嵌资产加载。
//! 这里负责从当前 exe 模块资源加载图标、位图，并构建应用用到的加速键表。

use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    ERROR_GEN_FAILURE, ERROR_INVALID_PARAMETER, GetLastError, HINSTANCE,
};
use windows_sys::Win32::Graphics::Gdi::HBITMAP;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    VK_DELETE, VK_ESCAPE, VK_F5, VK_RETURN, VK_TAB,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ACCEL, CreateAcceleratorTableW, FCONTROL, FNOINVERT, FSHIFT, FVIRTKEY, HACCEL, HICON,
    IMAGE_BITMAP, IMAGE_ICON, LR_SHARED, LoadImageW,
};

use crate::infrastructure::native::OwnedIcon;
use crate::ui::resource_ids::{
    IDB_METER_LIT_GREEN, IDB_METER_LIT_RED, IDB_METER_UNLIT, IDC_ENDTASK, IDC_NEXTTAB, IDC_PREVTAB,
    IDC_SWITCHTO, IDI_APPLICATION, IDI_DEFAULT_PROCESS, IDM_HIDE, IDM_REFRESH, TRAY_ICON_IDS,
};

pub const APPLICATION_ICON_RESOURCE: u16 = IDI_APPLICATION;
pub const DEFAULT_PROCESS_ICON_RESOURCE: u16 = IDI_DEFAULT_PROCESS;
pub const METER_LIT_GREEN_BITMAP_RESOURCE: u16 = IDB_METER_LIT_GREEN;
pub const METER_LIT_RED_BITMAP_RESOURCE: u16 = IDB_METER_LIT_RED;
pub const METER_UNLIT_BITMAP_RESOURCE: u16 = IDB_METER_UNLIT;

pub const TRAY_CPU_ICON_RESOURCES: [u16; 12] = TRAY_ICON_IDS;

const _: () = {
    assert!(APPLICATION_ICON_RESOURCE < DEFAULT_PROCESS_ICON_RESOURCE);
    let mut index = 0;
    while index < TRAY_CPU_ICON_RESOURCES.len() {
        assert!(APPLICATION_ICON_RESOURCE < TRAY_CPU_ICON_RESOURCES[index]);
        index += 1;
    }
};

fn current_module() -> HINSTANCE {
    // 安全性: null module name asks Windows for the module handle of the current process image.
    unsafe { GetModuleHandleW(null::<u16>()) as HINSTANCE }
}

pub fn load_icon_resource(
    resource_id: u16,
    width: i32,
    height: i32,
    flags: u32,
) -> Result<OwnedIcon, u32> {
    // Shared icons remain system-owned and must never enter the `OwnedIcon` destructor path.
    if flags & LR_SHARED != 0 {
        return Err(ERROR_INVALID_PARAMETER);
    }

    let module = current_module();
    if module.is_null() {
        return Err(last_error_or_gen_failure());
    }

    // Win32 encodes integer resource IDs as pointer-sized values whose high word is zero.
    let resource = resource_id as usize as *const u16;
    // 安全性: `resource` is a valid MAKEINTRESOURCE-style value and `module` is the current
    // executable image containing the compiled icon table.
    let icon = unsafe { LoadImageW(module, resource, IMAGE_ICON, width, height, flags) as HICON };
    if icon.is_null() {
        return Err(last_error_or_gen_failure());
    }

    // 安全性: LR_SHARED was rejected and successful `LoadImageW(IMAGE_ICON)` transfers one icon
    // that Microsoft documents must be released with `DestroyIcon`.
    unsafe { OwnedIcon::from_raw(icon) }.ok_or(ERROR_GEN_FAILURE)
}

fn last_error_or_gen_failure() -> u32 {
    // 安全性: reading the calling thread's last-error slot has no caller-side preconditions.
    let error = unsafe { GetLastError() };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

pub fn load_bitmap_resource(resource_id: u16) -> HBITMAP {
    let module = current_module();
    if module.is_null() {
        return null_mut();
    }

    let resource = resource_id as usize as *const u16;
    // Win32 interprets this low-valued pointer as a MAKEINTRESOURCE-style integer ID.
    unsafe { LoadImageW(module, resource, IMAGE_BITMAP, 0, 0, 0) as HBITMAP }
}

pub fn create_accelerator_table() -> HACCEL {
    // 加速键表在 Rust 侧声明，运行时一次性创建成 Win32 `HACCEL`。
    let accelerators = [
        ACCEL {
            fVirt: FVIRTKEY | FNOINVERT,
            key: VK_DELETE,
            cmd: IDC_ENDTASK as u16,
        },
        ACCEL {
            fVirt: FVIRTKEY | FSHIFT | FNOINVERT,
            key: VK_ESCAPE,
            cmd: IDM_HIDE,
        },
        ACCEL {
            fVirt: FVIRTKEY | FNOINVERT,
            key: VK_F5,
            cmd: IDM_REFRESH,
        },
        ACCEL {
            fVirt: FVIRTKEY | FNOINVERT,
            key: VK_RETURN,
            cmd: IDC_SWITCHTO as u16,
        },
        ACCEL {
            fVirt: FVIRTKEY | FCONTROL | FNOINVERT,
            key: VK_TAB,
            cmd: IDC_NEXTTAB,
        },
        ACCEL {
            fVirt: FVIRTKEY | FSHIFT | FCONTROL | FNOINVERT,
            key: VK_TAB,
            cmd: IDC_PREVTAB,
        },
    ];
    // 安全性: `accelerators` is a valid slice of ACCEL entries and the API copies the table.
    unsafe { CreateAcceleratorTableW(accelerators.as_ptr(), accelerators.len() as i32) }
}
