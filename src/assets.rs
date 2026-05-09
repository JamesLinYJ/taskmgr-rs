//! 运行时资产定位与加载。
//! 这里负责在可执行文件附近查找图标、位图等外部资产，并构建应用用到的
//! 加速键表，避免业务模块直接关心文件系统查找规则。

use std::env;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::HINSTANCE;
use windows_sys::Win32::Graphics::Gdi::HBITMAP;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    VK_DELETE, VK_ESCAPE, VK_F5, VK_RETURN, VK_TAB,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateAcceleratorTableW, LoadImageW, ACCEL, FCONTROL, FNOINVERT, FSHIFT, FVIRTKEY, HACCEL,
    HICON, IMAGE_BITMAP, IMAGE_ICON, LR_LOADFROMFILE,
};

use crate::resource::{IDC_ENDTASK, IDC_NEXTTAB, IDC_PREVTAB, IDC_SWITCHTO, IDM_HIDE, IDM_REFRESH};

pub const TRAY_ICON_FILES: [&str; 12] = [
    "tray0.ico",
    "tray1.ico",
    "tray2.ico",
    "tray3.ico",
    "tray4.ico",
    "tray5.ico",
    "tray6.ico",
    "tray7.ico",
    "tray8.ico",
    "tray9.ico",
    "tray10.ico",
    "tray11.ico",
];

fn to_wide_null(path: &Path) -> Vec<u16> {
    // `LoadImageW` 需要零结尾 UTF-16 路径，这里统一做一次转换。
    OsStr::new(path.as_os_str())
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn candidate_roots() -> Vec<PathBuf> {
    // 运行时会优先从 exe 所在目录逐级向上找资源，
    // 这样开发态和发布态都能复用同一套查找逻辑。
    let mut roots = Vec::new();
    if let Ok(exe) = env::current_exe() {
        let mut current = exe.parent().map(Path::to_path_buf);
        for _ in 0..5 {
            let Some(path) = current else {
                break;
            };
            roots.push(path.clone());
            current = path.parent().map(Path::to_path_buf);
        }
    }
    roots
}

pub fn locate_asset(file_name: &str) -> Option<PathBuf> {
    // 返回第一个实际存在的候选资源路径。
    candidate_roots()
        .into_iter()
        .map(|root| root.join(file_name))
        .find(|candidate| candidate.exists())
}

pub fn load_icon_from_file(file_name: &str, width: i32, height: i32, flags: u32) -> HICON {
    // 图标直接按文件加载，避免重新引入 `.rc` 资源依赖。
    let Some(path) = locate_asset(file_name) else {
        return null_mut();
    };
    let wide = to_wide_null(&path);
    // SAFETY: `wide` is a live, NUL-terminated UTF-16 path and `LoadImageW` only borrows it
    // for the duration of the call.
    unsafe {
        LoadImageW(
            0 as HINSTANCE,
            wide.as_ptr(),
            IMAGE_ICON,
            width,
            height,
            flags | LR_LOADFROMFILE,
        ) as HICON
    }
}

pub fn load_bitmap_from_file(file_name: &str) -> HBITMAP {
    // 位图和图标共享同一套定位逻辑，只是加载的 Win32 类型不同。
    let Some(path) = locate_asset(file_name) else {
        return null_mut();
    };
    let wide = to_wide_null(&path);
    // SAFETY: `wide` is a live, NUL-terminated UTF-16 path and `LoadImageW` only borrows it
    // for the duration of the call.
    unsafe {
        LoadImageW(
            0 as HINSTANCE,
            wide.as_ptr(),
            IMAGE_BITMAP,
            0,
            0,
            LR_LOADFROMFILE,
        ) as HBITMAP
    }
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
    // SAFETY: `accelerators` is a valid slice of ACCEL entries and the API copies the table.
    unsafe { CreateAcceleratorTableW(accelerators.as_ptr(), accelerators.len() as i32) }
}
