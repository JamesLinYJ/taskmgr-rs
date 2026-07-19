// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 应用控制器
//
//   文件:       src/app/controllers.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 主应用对象持有的小型控制器集合。
//! 这些控制器把长期存活的 UI 状态从 `App` 中分离，同时保持 Win32 消息流完整。
//!
//! 四个控制器各司其职：
//! - RuntimeStatsController：保存后台系统采样器提交的汇总快照
//! - TrayController：托盘图标切换与提示文本
//! - MenuController：菜单弹出/跟踪状态机
//! - WindowModeController：无标题模式与置顶模式

use std::cell::Cell;
use std::mem::{size_of, zeroed};
use windows_sys::Win32::Foundation::{ERROR_GEN_FAILURE, ERROR_INVALID_STATE, GetLastError, HWND};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{HICON, HMENU, LR_DEFAULTCOLOR, LR_DEFAULTSIZE};

use crate::infrastructure::native::{
    destroy_icon_handle, format_resource_string, record_win32_error, to_wide_null,
};
use crate::system::sampler::SystemSample;
use crate::ui::assets::{TRAY_CPU_ICON_RESOURCES, load_icon_resource};
use crate::ui::resource_ids::PWM_TRAYICON;

const NOTIFY_ICON_TIP_CAPACITY: usize = 128;

/// 运行时统计控制器。
/// 只保存后台采样器已经验证并原子提交的汇总字段。
#[derive(Default)]
pub struct RuntimeStatsController {
    pub cpu_usage: u8,
    pub mem_usage_kb: u64,
    pub mem_limit_kb: u64,
    pub process_count: u32,
    pub processor_count: usize,
}

impl RuntimeStatsController {
    pub fn apply_sample(&mut self, sample: &SystemSample) {
        self.cpu_usage = sample.cpu_usage;
        self.mem_usage_kb = sample.physical_mem_usage_kb;
        self.mem_limit_kb = sample.physical_mem_limit_kb;
        self.process_count = sample.process_count;
        self.processor_count = sample.processor_count;
    }
}

/// 托盘图标控制器。
/// 管理 12 级 CPU 占用图标和通知区域提示文本。
pub struct TrayController {
    icons: Vec<HICON>,
    registered: Cell<bool>,
    last_error: Cell<Option<u32>>,
}

impl Default for TrayController {
    fn default() -> Self {
        Self {
            icons: Vec::with_capacity(TRAY_CPU_ICON_RESOURCES.len()),
            registered: Cell::new(false),
            last_error: Cell::new(None),
        }
    }
}

impl TrayController {
    pub fn load_icons(&mut self) -> Result<(), u32> {
        let mut loaded = Vec::with_capacity(TRAY_CPU_ICON_RESOURCES.len());
        for resource_name in TRAY_CPU_ICON_RESOURCES {
            let icon_handle =
                load_icon_resource(resource_name, 0, 0, LR_DEFAULTCOLOR | LR_DEFAULTSIZE);
            if icon_handle.is_null() {
                let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
                for icon in loaded {
                    destroy_icon_handle(icon);
                }
                return Err(if error == 0 {
                    windows_sys::Win32::Foundation::ERROR_RESOURCE_DATA_NOT_FOUND
                } else {
                    error
                });
            }
            loaded.push(icon_handle);
        }
        self.clear_icons();
        self.icons = loaded;
        Ok(())
    }

    pub fn first_icon(&self) -> Option<HICON> {
        self.icons.first().copied()
    }

    pub fn update_tray(&self, main_hwnd: HWND, command: u32, icon: HICON, tip: &str) {
        if command == NIM_DELETE && !self.registered.get() {
            return;
        }
        if command == NIM_MODIFY && !self.registered.get() {
            self.record_tray_error(ERROR_INVALID_STATE);
            return;
        }

        // 安全性: `NOTIFYICONDATAW` is a Win32 POD struct where zero-initialization is valid.
        let mut data = unsafe { zeroed::<NOTIFYICONDATAW>() };
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = main_hwnd;
        data.uID = PWM_TRAYICON;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = PWM_TRAYICON;
        data.hIcon = icon;

        let tip_wide = to_wide_null(tip);
        for (index, code_unit) in tip_wide.iter().copied().enumerate() {
            if index >= NOTIFY_ICON_TIP_CAPACITY {
                break;
            }
            data.szTip[index] = code_unit;
        }

        // 安全性: `data` is fully initialized for Shell_NotifyIconW and lives through the call.
        if unsafe { Shell_NotifyIconW(command, &data) } == 0 {
            let error = unsafe { GetLastError() };
            self.record_tray_error(if error == 0 { ERROR_GEN_FAILURE } else { error });
            return;
        }

        if command == NIM_ADD {
            self.registered.set(true);
        } else if command == NIM_DELETE {
            self.registered.set(false);
        }
        self.last_error.set(None);
    }

    fn record_tray_error(&self, error: u32) {
        if self.last_error.replace(Some(error)) != Some(error) {
            record_win32_error("notification area icon update", error);
        }
    }

    pub fn refresh_icon(&self, main_hwnd: HWND, cpu_usage: u8, fmt_cpu: &str) {
        if self.icons.is_empty() {
            return;
        }

        let mut icon_index = (cpu_usage as usize * self.icons.len()) / 100;
        if icon_index >= self.icons.len() {
            icon_index = self.icons.len() - 1;
        }

        let tooltip = format_resource_string(fmt_cpu, &[cpu_usage.to_string()]);
        self.update_tray(
            main_hwnd,
            windows_sys::Win32::UI::Shell::NIM_MODIFY,
            self.icons[icon_index],
            &tooltip,
        );
    }

    pub fn clear_icons(&mut self) {
        for icon in self.icons.drain(..) {
            if !icon.is_null() {
                destroy_icon_handle(icon);
            }
        }
    }
}

impl Drop for TrayController {
    fn drop(&mut self) {
        self.clear_icons();
    }
}

/// 菜单状态控制器。
/// 借用当前页面拥有的活动菜单句柄，并记录菜单跟踪状态和弹出状态，用于任务管理器的
/// "隐藏时最小化"和菜单自动关闭逻辑。
#[derive(Default)]
pub struct MenuController {
    current_menu: HMENU,
    tracking: bool,
    cant_hide: bool,
    in_popup: bool,
}

impl MenuController {
    pub fn current_menu(&self) -> HMENU {
        self.current_menu
    }

    pub fn current_menu_mut(&mut self) -> &mut HMENU {
        &mut self.current_menu
    }

    pub fn clear_current_menu(&mut self) {
        self.current_menu = std::ptr::null_mut();
    }

    pub fn is_tracking(&self) -> bool {
        self.tracking
    }

    pub fn begin_tracking(&mut self) {
        self.tracking = true;
    }

    pub fn end_tracking(&mut self) {
        self.tracking = false;
        self.cant_hide = false;
    }

    pub fn mark_menu_opened(&mut self) {
        self.cant_hide = true;
    }

    pub fn can_temporarily_hide(&self) -> bool {
        !self.in_popup() && !self.cant_hide
    }

    pub fn enter_popup(&mut self) {
        self.in_popup = true;
    }

    pub fn leave_popup(&mut self) {
        self.in_popup = false;
    }

    pub fn set_popup_active(&mut self, active: bool) {
        self.in_popup = active;
    }

    pub fn in_popup(&self) -> bool {
        self.in_popup
    }
}

/// 窗口模式控制器。
/// 管理无标题模式（紧凑视图）和置顶模式的切换，包括窗口风格位操作
/// 和窗口矩形的位置跟踪。
#[derive(Default)]
pub struct WindowModeController {
    framed_style: u32,
    borderless_style: u32,
    temporarily_hidden: bool,
}

impl WindowModeController {
    pub fn set_base_styles(&mut self, framed_style: u32, borderless_style: u32) {
        self.framed_style = framed_style;
        self.borderless_style = borderless_style;
    }

    pub fn is_temporarily_hidden(&self) -> bool {
        self.temporarily_hidden
    }

    pub fn mark_temporarily_hidden(&mut self) {
        self.temporarily_hidden = true;
    }

    pub fn mark_restored(&mut self) {
        self.temporarily_hidden = false;
    }
}
