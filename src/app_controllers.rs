//! Small controllers owned by the main application object.
//! They keep long-lived UI state out of `App` while preserving the Win32 message flow.

use std::mem::{size_of, zeroed};
use windows_sys::Win32::Foundation::{FILETIME, HWND};
use windows_sys::Win32::System::ProcessStatus::{K32GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::GetSystemTimes;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{HICON, HMENU, LR_DEFAULTCOLOR, LR_DEFAULTSIZE};

use crate::assets::{load_icon_from_file, TRAY_ICON_FILES};
use crate::perfpage::PerformanceSnapshot;
use crate::resource::PWM_TRAYICON;
use crate::winutil::{format_resource_string, to_wide_null};

const NOTIFY_ICON_TIP_CAPACITY: usize = 128;

#[derive(Default)]
pub struct RuntimeStatsController {
    pub cpu_usage: u8,
    pub mem_usage_kb: u32,
    pub mem_limit_kb: u32,
    pub process_count: u32,
    pub processor_count: u8,
    previous_idle: u64,
    previous_kernel: u64,
    previous_user: u64,
}

impl RuntimeStatsController {
    pub fn apply_snapshot(&mut self, snapshot: PerformanceSnapshot) {
        self.cpu_usage = snapshot.cpu_usage;
        self.mem_usage_kb = snapshot.mem_usage_kb;
        self.mem_limit_kb = snapshot.mem_limit_kb;
        self.process_count = snapshot.process_count;
    }

    pub fn refresh_runtime_stats(&mut self) {
        // SAFETY: all Win32 calls write into initialized local output buffers.
        unsafe {
            let mut idle = zeroed::<FILETIME>();
            let mut kernel = zeroed::<FILETIME>();
            let mut user = zeroed::<FILETIME>();
            if GetSystemTimes(&mut idle, &mut kernel, &mut user) != 0 {
                let idle_value = filetime_to_u64(idle);
                let kernel_value = filetime_to_u64(kernel);
                let user_value = filetime_to_u64(user);

                if self.previous_idle != 0 {
                    let delta_idle = idle_value.saturating_sub(self.previous_idle);
                    let delta_total = kernel_value
                        .saturating_sub(self.previous_kernel)
                        .saturating_add(user_value.saturating_sub(self.previous_user));

                    if delta_total != 0 {
                        let active_ticks = delta_total.saturating_sub(delta_idle);
                        self.cpu_usage = ((active_ticks * 100) / delta_total) as u8;
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
                self.mem_usage_kb = ((memory.ullTotalPhys - memory.ullAvailPhys) / 1024) as u32;
                self.mem_limit_kb = (memory.ullTotalPhys / 1024) as u32;
            }
        }

        self.process_count = process_count();
    }
}

pub struct TrayController {
    icons: Vec<HICON>,
}

impl Default for TrayController {
    fn default() -> Self {
        Self {
            icons: Vec::with_capacity(TRAY_ICON_FILES.len()),
        }
    }
}

impl TrayController {
    pub fn load_icons(&mut self) {
        self.icons.clear();
        for file_name in TRAY_ICON_FILES {
            let icon_handle =
                load_icon_from_file(file_name, 0, 0, LR_DEFAULTCOLOR | LR_DEFAULTSIZE);
            self.icons.push(icon_handle);
        }
    }

    pub fn first_icon(&self) -> Option<HICON> {
        self.icons.first().copied()
    }

    pub fn update_tray(&self, main_hwnd: HWND, command: u32, icon: HICON, tip: &str) {
        // SAFETY: `NOTIFYICONDATAW` is a Win32 POD struct where zero-initialization is valid.
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

        // SAFETY: `data` is fully initialized for Shell_NotifyIconW and lives through the call.
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
}

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

fn filetime_to_u64(filetime: FILETIME) -> u64 {
    (u64::from(filetime.dwHighDateTime) << 32) | u64::from(filetime.dwLowDateTime)
}

fn process_count() -> u32 {
    let mut perf = unsafe { zeroed::<PERFORMANCE_INFORMATION>() };
    perf.cb = size_of::<PERFORMANCE_INFORMATION>() as u32;
    if unsafe { K32GetPerformanceInfo(&mut perf, perf.cb) } == 0 {
        return 0;
    }

    perf.ProcessCount
}
