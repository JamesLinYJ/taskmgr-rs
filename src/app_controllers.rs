//! 主应用对象持有的小型控制器集合。
//! 这些控制器把长期存活的 UI 状态从 `App` 中分离，同时保持 Win32 消息流完整。
//!
//! 四个控制器各司其职：
//! - RuntimeStatsController：CPU/内存采样与差值计算
//! - TrayController：托盘图标切换与提示文本
//! - MenuController：菜单弹出/跟踪状态机
//! - WindowModeController：无标题模式与置顶模式

use std::mem::{size_of, zeroed};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::ProcessStatus::{K32GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{HICON, HMENU, LR_DEFAULTCOLOR, LR_DEFAULTSIZE};

use crate::assets::{load_icon_resource, TRAY_ICON_RESOURCES};
use crate::cpu_sampler::{
    query_processor_performance, summed_processor_times, ProcessorPerformance,
};
use crate::perfpage::PerformanceSnapshot;
use crate::resource::PWM_TRAYICON;
use crate::winutil::{destroy_icon_handle, format_resource_string, to_wide_null};

const NOTIFY_ICON_TIP_CAPACITY: usize = 128;

/// 运行时统计控制器。
/// 通过全 processor-group 累计时间差计算 CPU 使用率，并通过 GlobalMemoryStatusEx 获取内存占用。
#[derive(Default)]
pub struct RuntimeStatsController {
    pub cpu_usage: u8,
    pub mem_usage_kb: u64,
    pub mem_limit_kb: u64,
    pub process_count: u32,
    pub processor_count: usize,
    previous_idle: u64,
    previous_kernel: u64,
    previous_user: u64,
    processor_info: Vec<ProcessorPerformance>,
}

impl RuntimeStatsController {
    pub fn apply_snapshot(&mut self, snapshot: PerformanceSnapshot) {
        self.cpu_usage = snapshot.cpu_usage;
        self.mem_usage_kb = snapshot.mem_usage_kb;
        self.mem_limit_kb = snapshot.mem_limit_kb;
        self.process_count = snapshot.process_count;
        self.processor_count = snapshot.processor_count;
    }

    pub fn refresh_runtime_stats(&mut self) {
        // 安全性: all Win32 calls write into initialized local output buffers.
        unsafe {
            if query_processor_performance(self.processor_count.max(1), &mut self.processor_info)
                .is_ok()
                && !self.processor_info.is_empty()
            {
                let processor_count_changed = self.processor_count != self.processor_info.len();
                self.processor_count = self.processor_info.len();
                let (idle_value, kernel_value, user_value) =
                    summed_processor_times(&self.processor_info);

                if processor_count_changed {
                    self.previous_idle = 0;
                    self.previous_kernel = 0;
                    self.previous_user = 0;
                }

                if self.previous_idle != 0 {
                    let delta_idle = idle_value.saturating_sub(self.previous_idle);
                    let delta_total = kernel_value
                        .saturating_sub(self.previous_kernel)
                        .saturating_add(user_value.saturating_sub(self.previous_user));

                    if delta_total != 0 {
                        let active_ticks = delta_total.saturating_sub(delta_idle);
                        self.cpu_usage =
                            ((active_ticks.saturating_mul(100)) / delta_total).min(100) as u8;
                    }
                }

                self.previous_idle = idle_value;
                self.previous_kernel = kernel_value;
                self.previous_user = user_value;
            }

            let mut memory = MEMORYSTATUSEX {
                dwLength: size_of::<MEMORYSTATUSEX>() as u32,
                ..zeroed()
            };
            if GlobalMemoryStatusEx(&mut memory) != 0 {
                self.mem_usage_kb = memory.ullTotalPhys.saturating_sub(memory.ullAvailPhys) / 1024;
                self.mem_limit_kb = memory.ullTotalPhys / 1024;
            }
        }

        if let Some(process_count) = process_count() {
            self.process_count = process_count;
        }
    }
}

/// 托盘图标控制器。
/// 管理 12 级 CPU 占用图标和通知区域提示文本。
pub struct TrayController {
    icons: Vec<HICON>,
}

impl Default for TrayController {
    fn default() -> Self {
        Self {
            icons: Vec::with_capacity(TRAY_ICON_RESOURCES.len()),
        }
    }
}

impl TrayController {
    pub fn load_icons(&mut self) -> Result<(), u32> {
        let mut loaded = Vec::with_capacity(TRAY_ICON_RESOURCES.len());
        for resource_name in TRAY_ICON_RESOURCES {
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
        unsafe { Shell_NotifyIconW(command, &data) };
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
/// 记录当前活动菜单句柄、菜单跟踪状态和弹出状态，用于任务管理器的
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

fn process_count() -> Option<u32> {
    let mut perf = unsafe { zeroed::<PERFORMANCE_INFORMATION>() };
    perf.cb = size_of::<PERFORMANCE_INFORMATION>() as u32;
    if unsafe { K32GetPerformanceInfo(&mut perf, perf.cb) } == 0 {
        return None;
    }

    Some(perf.ProcessCount)
}
