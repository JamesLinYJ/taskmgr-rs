// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 已验证进程操作
//
//   文件:       src/pages/processes/actions.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Centralizes destructive and identity-sensitive process operations.
//! Every target process is reopened through `ProcIdentity` immediately before use.

use std::collections::{HashMap, HashSet};
use std::mem::{size_of, zeroed};
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_GEN_FAILURE, ERROR_INVALID_DATA, ERROR_INVALID_HANDLE,
    ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, ERROR_NOT_SUPPORTED, ERROR_PATH_NOT_FOUND,
    FILETIME, GetLastError, HWND, LPARAM, WPARAM,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_EXPAND_SZ, REG_SZ, RegCloseKey, RegOpenKeyExW,
    RegQueryValueExW,
};
use windows_sys::Win32::System::SystemInformation::{
    GetSystemTimeAsFileTime, GetWindowsDirectoryW,
};
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, CreateProcessW,
    GetProcessAffinityMask, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
    PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_INFORMATION, PROCESS_TERMINATE, QueryFullProcessImageNameW,
    REALTIME_PRIORITY_CLASS, STARTUPINFOW, SetPriorityClass, SetProcessAffinityMask,
    TerminateProcess,
};
use windows_sys::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, CheckDlgButton, IsDlgButtonChecked,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EndDialog, GetDlgItem, IDCANCEL, IDOK, IDYES, MB_ICONERROR, MB_ICONEXCLAMATION, MB_OK,
    MB_YESNO, MessageBoxW, WM_COMMAND, WM_INITDIALOG,
};

use super::{ProcPriority, ProcessPageState};
use crate::infrastructure::native::{
    OwnedHandle, get_window_userdata, loword, set_window_userdata, to_wide_null,
};
use crate::system::process_identity::{
    ProcIdentity, open_process_for_identity, query_process_identity_for_pid,
};
use crate::ui::dialogs::dialog_box;
use crate::ui::localization::localize_dialog;
use crate::ui::resource_ids::*;

// “设置亲和性”对话框的上下文，包含当前进程掩码。
struct AffinityDialogContext {
    page: *mut ProcessPageState,
    process_mask: usize,
    system_mask: usize,
}

impl ProcessPageState {
    unsafe fn quick_confirm(&self, title: &str, body: &str) -> bool {
        unsafe {
            // 用户关闭“确认”选项后，危险操作直接放行，保持与原版 Task Manager 行为一致。
            if !self.confirmations {
                return true;
            }

            let title_wide = to_wide_null(title);
            let body_wide = to_wide_null(body);
            MessageBoxW(
                self.hwnd_page,
                body_wide.as_ptr(),
                title_wide.as_ptr(),
                MB_ICONEXCLAMATION | MB_YESNO,
            ) == IDYES
        }
    }

    pub(super) fn show_failure_message(&self, body: &str, error: u32) {
        unsafe {
            let title = if self.strings.warning.is_empty() {
                "Task Manager".to_string()
            } else {
                self.strings.warning.clone()
            };
            let message = format!("{body}\r\n\r\nWin32 error: {error}");
            let title_wide = to_wide_null(&title);
            let message_wide = to_wide_null(&message);
            MessageBoxW(
                self.hwnd_page,
                message_wide.as_ptr(),
                title_wide.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }

    // 结束指定 PID 的进程。先弹确认框，再通过 TerminateProcess 终止。
    pub(super) unsafe fn kill_process(&mut self, identity: ProcIdentity) -> bool {
        unsafe {
            if !self.quick_confirm(&self.strings.warning, &self.strings.kill) {
                return false;
            }

            let handle = match open_process_for_identity(
                identity,
                PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            ) {
                Ok(handle) => handle,
                Err(error) => {
                    self.show_failure_message(&self.strings.cant_kill, error);
                    return false;
                }
            };

            let result = TerminateProcess(handle.as_raw(), 1);
            let error = windows_sys::Win32::Foundation::GetLastError();

            if result == 0 {
                self.show_failure_message(&self.strings.cant_kill, error);
                false
            } else {
                self.paused = false;
                self.refresh_processes();
                true
            }
        }
    }

    // 结束进程以及所有子进程。按叶子优先的顺序遍历进程树，逐进程 TerminateProcess。
    pub(super) unsafe fn kill_process_tree(&mut self, identity: ProcIdentity) -> bool {
        unsafe {
            if !self.quick_confirm(&self.strings.warning, &self.strings.kill_tree) {
                return false;
            }

            let mut root_handle = match open_process_for_identity(
                identity,
                PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            ) {
                Ok(handle) => Some(handle),
                Err(error) => {
                    self.show_failure_message(&self.strings.cant_kill, error);
                    return false;
                }
            };

            let pid = identity.pid;
            let termination_order = match collect_process_tree_termination_order(identity) {
                Ok(order) if !order.is_empty() => order,
                Ok(_) => {
                    self.show_failure_message(
                        &self.strings.kill_tree_fail_body,
                        windows_sys::Win32::Foundation::ERROR_GEN_FAILURE,
                    );
                    return false;
                }
                Err(error) => {
                    self.show_failure_message(&self.strings.kill_tree_fail_body, error);
                    return false;
                }
            };

            // 先验证并打开整棵树，再开始终止，避免权限/身份错误造成可预见的半完成状态。
            let mut targets = Vec::with_capacity(termination_order.len());
            for target_identity in termination_order {
                if target_identity == identity {
                    let Some(handle) = root_handle.take() else {
                        self.show_failure_message(
                            &self.strings.kill_tree_fail_body,
                            ERROR_INVALID_DATA,
                        );
                        return false;
                    };
                    targets.push((target_identity, handle));
                    continue;
                }

                match open_process_for_identity(
                    target_identity,
                    PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                ) {
                    Ok(handle) => targets.push((target_identity, handle)),
                    Err(error) => {
                        self.show_failure_message(&self.strings.kill_tree_fail_body, error);
                        return false;
                    }
                }
            }

            if root_handle.is_some() {
                self.show_failure_message(
                    &self.strings.kill_tree_fail_body,
                    windows_sys::Win32::Foundation::ERROR_GEN_FAILURE,
                );
                return false;
            }

            let mut any_success = false;
            let mut any_failure = false;
            let mut root_error = 0u32;

            for (target_identity, handle) in targets {
                let target_pid = target_identity.pid;
                if TerminateProcess(handle.as_raw(), 1) == 0 {
                    any_failure = true;
                    if target_pid == pid {
                        root_error = windows_sys::Win32::Foundation::GetLastError();
                    }
                } else {
                    any_success = true;
                }
            }

            if any_success {
                self.paused = false;
                self.refresh_processes();
            }

            if root_error != 0 && !any_success {
                self.show_failure_message(&self.strings.cant_kill, root_error);
                return false;
            }

            if any_failure {
                let body_wide = to_wide_null(&self.strings.kill_tree_fail_body);
                let title_wide = to_wide_null(&self.strings.kill_tree_fail);
                MessageBoxW(
                    self.hwnd_page,
                    body_wide.as_ptr(),
                    title_wide.as_ptr(),
                    MB_OK | MB_ICONEXCLAMATION,
                );
                return false;
            }

            any_success
        }
    }

    // 以 AeDebug 注册表配置的调试器启动并附加到目标进程。命令行传 -p <pid>。
    pub(super) unsafe fn attach_debugger(&mut self, identity: ProcIdentity) -> bool {
        unsafe {
            let Some(debugger_path) = self.debugger_path.as_ref() else {
                let error = match self.debugger_error {
                    Some(error) => error,
                    None => ERROR_FILE_NOT_FOUND,
                };
                self.show_failure_message(&self.strings.cant_debug, error);
                return false;
            };

            if !self.quick_confirm(&self.strings.warning, &self.strings.debug) {
                return false;
            }

            let target_handle =
                match open_process_for_identity(identity, PROCESS_QUERY_LIMITED_INFORMATION) {
                    Ok(handle) => handle,
                    Err(error) => {
                        self.show_failure_message(&self.strings.cant_debug, error);
                        return false;
                    }
                };

            let pid = identity.pid;
            let command_line = format!("{} -p {pid}", quote_command_line_arg(debugger_path));
            let mut command_line_wide = to_wide_null(&command_line);
            let application_name = to_wide_null(debugger_path);
            let startup_info = STARTUPINFOW {
                cb: size_of::<STARTUPINFOW>() as u32,
                ..zeroed()
            };
            let mut process_info = zeroed::<PROCESS_INFORMATION>();

            let created = CreateProcessW(
                application_name.as_ptr(),
                command_line_wide.as_mut_ptr(),
                null_mut(),
                null_mut(),
                0,
                windows_sys::Win32::System::Threading::CREATE_NEW_CONSOLE,
                null(),
                null(),
                &startup_info,
                &mut process_info,
            );
            let create_error = windows_sys::Win32::Foundation::GetLastError();
            drop(target_handle);

            if created == 0 {
                self.show_failure_message(&self.strings.cant_debug, create_error);
                false
            } else {
                match own_created_process_handles(process_info) {
                    Ok(_) => true,
                    Err(error) => {
                        self.show_failure_message(&self.strings.cant_debug, error);
                        false
                    }
                }
            }
        }
    }

    // 通过 explorer.exe /select 命令在资源管理器中定位进程的可执行文件。
    pub(super) unsafe fn open_file_location(&mut self, identity: ProcIdentity) -> bool {
        unsafe {
            let image_path = match query_process_image_path(identity) {
                Ok(path) => path,
                Err(error) => {
                    self.show_failure_message(&self.strings.cant_open_file_location, error);
                    return false;
                }
            };

            if !Path::new(&image_path).exists() {
                self.show_failure_message(&self.strings.cant_open_file_location, 2);
                return false;
            }

            let windows_directory = match query_windows_directory() {
                Ok(path) => path,
                Err(error) => {
                    self.show_failure_message(&self.strings.cant_open_file_location, error);
                    return false;
                }
            };
            let explorer_path = format!("{windows_directory}\\explorer.exe");
            let command_line = format!(
                "{explorer_path} /select,{}",
                quote_command_line_arg(&image_path)
            );
            let mut command_line_wide = to_wide_null(&command_line);
            let startup_info = STARTUPINFOW {
                cb: size_of::<STARTUPINFOW>() as u32,
                ..zeroed()
            };
            let mut process_info = zeroed::<PROCESS_INFORMATION>();
            let created = CreateProcessW(
                null(),
                command_line_wide.as_mut_ptr(),
                null_mut(),
                null_mut(),
                0,
                0,
                null(),
                null(),
                &startup_info,
                &mut process_info,
            );
            if created == 0 {
                self.show_failure_message(
                    &self.strings.cant_open_file_location,
                    windows_sys::Win32::Foundation::GetLastError(),
                );
                return false;
            }

            match own_created_process_handles(process_info) {
                Ok(_) => true,
                Err(error) => {
                    self.show_failure_message(&self.strings.cant_open_file_location, error);
                    false
                }
            }
        }
    }

    // 通过 SetPriorityClass 修改进程优先级类。先弹确认框，操作成功后刷新列表。
    pub(super) unsafe fn set_priority(
        &mut self,
        identity: ProcIdentity,
        priority: ProcPriority,
    ) -> bool {
        unsafe {
            let priority_class = match priority {
                ProcPriority::Low => IDLE_PRIORITY_CLASS,
                ProcPriority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
                ProcPriority::Normal => NORMAL_PRIORITY_CLASS,
                ProcPriority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
                ProcPriority::High => HIGH_PRIORITY_CLASS,
                ProcPriority::Realtime => REALTIME_PRIORITY_CLASS,
            };

            if !self.quick_confirm(&self.strings.warning, &self.strings.prichange) {
                return false;
            }

            let handle = match open_process_for_identity(
                identity,
                PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION,
            ) {
                Ok(handle) => handle,
                Err(error) => {
                    self.show_failure_message(&self.strings.cant_change_priority, error);
                    return false;
                }
            };

            let result = SetPriorityClass(handle.as_raw(), priority_class);
            let error = windows_sys::Win32::Foundation::GetLastError();

            if result == 0 {
                self.show_failure_message(&self.strings.cant_change_priority, error);
                false
            } else {
                self.paused = false;
                self.refresh_processes();
                true
            }
        }
    }

    // 通过 SetProcessAffinityMask 设置进程 CPU 亲和性。用户通过对话框选择 CPU。
    pub(super) unsafe fn set_affinity(&mut self, identity: ProcIdentity) -> bool {
        unsafe {
            let handle = match open_process_for_identity(
                identity,
                PROCESS_QUERY_INFORMATION | PROCESS_SET_INFORMATION,
            ) {
                Ok(handle) => handle,
                Err(error) => {
                    self.show_failure_message(&self.strings.cant_set_affinity, error);
                    return false;
                }
            };

            let mut process_mask = 0usize;
            let mut system_mask = 0usize;
            let mut success = false;

            if GetProcessAffinityMask(handle.as_raw(), &mut process_mask, &mut system_mask) != 0 {
                process_mask &= system_mask;
                if process_mask == 0 || system_mask == 0 {
                    self.show_failure_message(&self.strings.cant_set_affinity, ERROR_NOT_SUPPORTED);
                    return false;
                }
                let mut context = AffinityDialogContext {
                    page: self as *mut ProcessPageState,
                    process_mask,
                    system_mask,
                };
                match dialog_box(
                    self.hinstance,
                    IDD_AFFINITY,
                    self.hwnd_page,
                    Some(affinity_dialog_proc),
                    &mut context as *mut AffinityDialogContext as LPARAM,
                ) {
                    Ok(result) if result == IDOK as isize => {
                        if SetProcessAffinityMask(handle.as_raw(), context.process_mask) == 0 {
                            self.show_failure_message(
                                &self.strings.cant_set_affinity,
                                windows_sys::Win32::Foundation::GetLastError(),
                            );
                        } else {
                            self.refresh_processes();
                            success = true;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        self.show_failure_message(&self.strings.cant_set_affinity, error);
                    }
                }
            } else {
                self.show_failure_message(
                    &self.strings.cant_set_affinity,
                    windows_sys::Win32::Foundation::GetLastError(),
                );
            }

            success
        }
    }
}
// “设置亲和性”对话框过程。根据 processor_count 启用/禁用 CPU 勾选框，
// 确认时检查至少选中一个 CPU，否则弹错误提示。
unsafe extern "system" fn affinity_dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    unsafe {
        match msg {
            WM_INITDIALOG => {
                set_window_userdata(hwnd, lparam);
                localize_dialog(hwnd, IDD_AFFINITY);
                let context = &*(lparam as *const AffinityDialogContext);

                for cpu_index in 0..=MAX_AFFINITY_CPU {
                    let control_id = IDC_CPU0 + cpu_index;
                    let mask = affinity_cpu_mask(cpu_index);
                    let enabled = mask != 0 && (context.system_mask & mask) != 0;
                    EnableWindow(GetDlgItem(hwnd, control_id), i32::from(enabled));
                    CheckDlgButton(
                        hwnd,
                        control_id,
                        if enabled && (context.process_mask & mask) != 0 {
                            BST_CHECKED
                        } else {
                            BST_UNCHECKED
                        },
                    );
                }
                1
            }
            WM_COMMAND => match i32::from(loword(wparam)) {
                IDCANCEL => {
                    EndDialog(hwnd, IDCANCEL as isize);
                    1
                }
                IDOK => {
                    let context = &mut *(get_window_userdata(hwnd) as *mut AffinityDialogContext);
                    let page = &*context.page;

                    context.process_mask = 0;
                    for cpu_index in 0..=MAX_AFFINITY_CPU {
                        let mask = affinity_cpu_mask(cpu_index);
                        if mask == 0 || (context.system_mask & mask) == 0 {
                            continue;
                        }
                        if IsDlgButtonChecked(hwnd, IDC_CPU0 + cpu_index) == BST_CHECKED {
                            context.process_mask |= mask;
                        }
                    }

                    if context.process_mask == 0 {
                        let title_wide = to_wide_null(&page.strings.invalid_option);
                        let body_wide = to_wide_null(&page.strings.no_affinity_mask);
                        MessageBoxW(hwnd, body_wide.as_ptr(), title_wide.as_ptr(), MB_ICONERROR);
                        1
                    } else {
                        EndDialog(hwnd, IDOK as isize);
                        1
                    }
                }
                _ => 0,
            },
            _ => 0,
        }
    }
}

pub(super) fn affinity_cpu_mask(cpu_index: i32) -> usize {
    u32::try_from(cpu_index)
        .ok()
        .and_then(|shift| 1usize.checked_shl(shift))
        .unwrap_or(0)
}
pub(super) unsafe fn load_debugger_path() -> Result<Option<String>, u32> {
    unsafe {
        // 进程页的“调试”命令依赖 AeDebug 注册表配置。
        // 这里只提取真正的可执行文件路径，过滤掉旧式 drwtsn32 之类的无效值。
        let mut key: HKEY = null_mut();
        let key_name = to_wide_null("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\AeDebug");
        let value_name = to_wide_null("Debugger");
        let open_status =
            RegOpenKeyExW(HKEY_LOCAL_MACHINE, key_name.as_ptr(), 0, KEY_READ, &mut key);
        if open_status != 0 {
            return if open_status == ERROR_FILE_NOT_FOUND || open_status == ERROR_PATH_NOT_FOUND {
                Ok(None)
            } else {
                Err(open_status)
            };
        }

        let mut value_size = 0u32;
        let size_status = RegQueryValueExW(
            key,
            value_name.as_ptr(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut value_size,
        );
        if size_status != 0 || value_size < 2 {
            let close_status = RegCloseKey(key);
            if close_status != 0 {
                return Err(close_status);
            }
            return if size_status == ERROR_FILE_NOT_FOUND {
                Ok(None)
            } else if size_status != 0 {
                Err(size_status)
            } else {
                Err(ERROR_INVALID_DATA)
            };
        }

        let mut buffer = vec![0u16; (value_size as usize / size_of::<u16>()).max(2)];
        let mut value_type = 0u32;
        let status = RegQueryValueExW(
            key,
            value_name.as_ptr(),
            null_mut(),
            &mut value_type,
            buffer.as_mut_ptr() as *mut u8,
            &mut value_size,
        );
        let close_status = RegCloseKey(key);
        if close_status != 0 {
            return Err(close_status);
        }

        if status != 0 || value_size < 2 || !(value_type == REG_SZ || value_type == REG_EXPAND_SZ) {
            return Err(if status != 0 {
                status
            } else {
                ERROR_INVALID_DATA
            });
        }

        let length = buffer
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(buffer.len());
        let raw = String::from_utf16_lossy(&buffer[..length]);
        let Some(executable) = normalize_debugger_command(&raw, value_type)? else {
            return Ok(None);
        };
        Ok(Path::new(&executable).is_file().then_some(executable))
    }
}

// 引用命令行参数。只在包含空格、制表符或引号时加引号，并正确处理反斜杠转义。
fn quote_command_line_arg(value: &str) -> String {
    if !value.contains([' ', '\t', '"']) {
        return value.to_string();
    }

    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;

    for ch in value.chars() {
        if ch == '\\' {
            backslashes += 1;
            continue;
        }

        if ch == '"' {
            quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
            quoted.push('"');
            backslashes = 0;
            continue;
        }

        if backslashes > 0 {
            quoted.push_str(&"\\".repeat(backslashes));
            backslashes = 0;
        }
        quoted.push(ch);
    }

    if backslashes > 0 {
        quoted.push_str(&"\\".repeat(backslashes * 2));
    }
    quoted.push('"');
    quoted
}

// 从命令行字符串中提取第一个 token（即可执行文件路径）。处理引号包裹和非引号两种格式。
pub(super) fn extract_first_command_token(command_line: &str) -> String {
    let trimmed = command_line.trim();
    if let Some(rest) = trimmed.strip_prefix('"') {
        rest.split('"').next().unwrap_or_default().to_string()
    } else {
        trimmed
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string()
    }
}

// 将 AeDebug 注册表值规范化：展开环境变量后提取可执行文件路径，过滤无效调试器。
fn normalize_debugger_command(command_line: &str, value_type: u32) -> Result<Option<String>, u32> {
    let expanded = if value_type == REG_EXPAND_SZ {
        expand_environment_variables(command_line)?
    } else {
        command_line.to_string()
    };
    Ok(normalize_debugger_command_with(
        &expanded,
        REG_SZ,
        str::to_string,
    ))
}

pub(super) fn normalize_debugger_command_with<F>(
    command_line: &str,
    value_type: u32,
    expand_environment_variables: F,
) -> Option<String>
where
    F: Fn(&str) -> String,
{
    let expanded = if value_type == REG_EXPAND_SZ {
        expand_environment_variables(command_line)
    } else {
        command_line.to_string()
    };
    let executable = extract_first_command_token(&expanded);

    if executable.is_empty()
        || executable.eq_ignore_ascii_case("drwtsn32")
        || executable.eq_ignore_ascii_case("drwtsn32.exe")
    {
        None
    } else {
        Some(executable)
    }
}

// 展开字符串中的环境变量（如 %SystemRoot%）。
// 使用 Win32 ExpandEnvironmentStringsW API，正确处理 WOW64 重定向和 %% 转义。
fn expand_environment_variables(command_line: &str) -> Result<String, u32> {
    // 安全性: the Win32 ExpandEnvironmentStringsW API reads the process environment block
    // maintained by the kernel, which handles system-variable edge cases (WOW64 redirections,
    // %% escaping, variable-length limits) correctly.
    let wide_input = to_wide_null(command_line);
    let required = unsafe { ExpandEnvironmentStringsW(wide_input.as_ptr(), null_mut(), 0) };
    if required == 0 {
        let error = unsafe { GetLastError() };
        return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
    }
    let mut buffer = vec![0u16; required as usize];
    let written =
        unsafe { ExpandEnvironmentStringsW(wide_input.as_ptr(), buffer.as_mut_ptr(), required) };
    if written == 0 || written > required {
        let error = unsafe { GetLastError() };
        return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
    }
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Ok(String::from_utf16_lossy(&buffer[..len]))
}

// 检查 SID 是否为已知服务帐户（SYSTEM、LOCAL SERVICE、NETWORK SERVICE），返回对应名称。
fn collect_process_tree_termination_order(
    root_identity: ProcIdentity,
) -> Result<Vec<ProcIdentity>, u32> {
    unsafe {
        let raw_snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        let Some(snapshot) = OwnedHandle::new(raw_snapshot) else {
            let error = windows_sys::Win32::Foundation::GetLastError();
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        };

        let mut snapshot_time = zeroed::<FILETIME>();
        GetSystemTimeAsFileTime(&mut snapshot_time);
        let snapshot_time_100ns = filetime_to_u64(snapshot_time);

        let mut child_map = HashMap::<u32, Vec<u32>>::new();
        let mut process_entry = zeroed::<PROCESSENTRY32W>();
        process_entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

        if Process32FirstW(snapshot.as_raw(), &mut process_entry) == 0 {
            let error = windows_sys::Win32::Foundation::GetLastError();
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        }

        loop {
            child_map
                .entry(process_entry.th32ParentProcessID)
                .or_default()
                .push(process_entry.th32ProcessID);
            if Process32NextW(snapshot.as_raw(), &mut process_entry) == 0 {
                let error = windows_sys::Win32::Foundation::GetLastError();
                if error != ERROR_NO_MORE_FILES {
                    return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
                }
                break;
            }
        }

        // The snapshot is keyed only by PID. Revalidate the root after enumeration so a root
        // that exited and had its PID reused cannot lend the replacement's children to this tree.
        let current_root = query_process_identity_for_pid(root_identity.pid)?;
        validate_snapshot_root_identity(root_identity, current_root)?;

        let mut identities = Vec::new();
        let mut visited = HashSet::new();
        collect_verified_process_tree_children(
            root_identity,
            snapshot_time_100ns,
            &child_map,
            &mut visited,
            &mut identities,
        )?;
        Ok(identities)
    }
}

const fn filetime_to_u64(filetime: FILETIME) -> u64 {
    ((filetime.dwHighDateTime as u64) << 32) | filetime.dwLowDateTime as u64
}

pub(super) fn validate_snapshot_root_identity(
    expected: ProcIdentity,
    observed: ProcIdentity,
) -> Result<(), u32> {
    if expected.is_verified() && expected == observed {
        Ok(())
    } else {
        Err(ERROR_INVALID_PARAMETER)
    }
}

// 后序遍历进程树；每条边都用创建时间验证，避免父 PID 被复用后串入旧子树。
unsafe fn collect_verified_process_tree_children(
    parent: ProcIdentity,
    snapshot_time_100ns: u64,
    child_map: &HashMap<u32, Vec<u32>>,
    visited: &mut HashSet<u32>,
    order: &mut Vec<ProcIdentity>,
) -> Result<(), u32> {
    unsafe {
        if !visited.insert(parent.pid) {
            return Ok(());
        }

        if let Some(children) = child_map.get(&parent.pid) {
            for &child_pid in children {
                if visited.contains(&child_pid) {
                    continue;
                }
                let child = query_process_identity_for_pid(child_pid)?;
                if !is_valid_process_tree_edge(parent, child, snapshot_time_100ns) {
                    continue;
                }
                collect_verified_process_tree_children(
                    child,
                    snapshot_time_100ns,
                    child_map,
                    visited,
                    order,
                )?;
            }
        }

        order.push(parent);
        Ok(())
    }
}

pub(super) fn is_valid_process_tree_edge(
    parent: ProcIdentity,
    child: ProcIdentity,
    snapshot_time_100ns: u64,
) -> bool {
    parent.is_verified()
        && child.is_verified()
        && parent.creation_time_100ns <= child.creation_time_100ns
        && child.creation_time_100ns <= snapshot_time_100ns
}

fn own_created_process_handles(
    process_info: PROCESS_INFORMATION,
) -> Result<(OwnedHandle, OwnedHandle), u32> {
    let process = OwnedHandle::new(process_info.hProcess);
    let thread = OwnedHandle::new(process_info.hThread);
    match (process, thread) {
        (Some(process), Some(thread)) => Ok((process, thread)),
        _ => Err(ERROR_INVALID_HANDLE),
    }
}

unsafe fn query_process_image_path(identity: ProcIdentity) -> Result<String, u32> {
    unsafe {
        let handle = open_process_for_identity(identity, PROCESS_QUERY_LIMITED_INFORMATION)?;

        let mut capacity = 32768u32;
        let mut buffer = vec![0u16; capacity as usize];
        let success =
            QueryFullProcessImageNameW(handle.as_raw(), 0, buffer.as_mut_ptr(), &mut capacity);
        let error = windows_sys::Win32::Foundation::GetLastError();
        drop(handle);

        if success == 0 {
            return Err(error);
        }

        Ok(String::from_utf16_lossy(&buffer[..capacity as usize]))
    }
}

unsafe fn query_windows_directory() -> Result<String, u32> {
    unsafe {
        let mut buffer = vec![0u16; 260];
        loop {
            let length = GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) as usize;
            if length == 0 {
                return Err(windows_sys::Win32::Foundation::GetLastError());
            }
            if length < buffer.len() {
                return Ok(String::from_utf16_lossy(&buffer[..length]));
            }
            buffer.resize(length.saturating_add(1), 0);
        }
    }
}
