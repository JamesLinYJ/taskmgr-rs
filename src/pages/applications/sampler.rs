// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 应用程序窗口采样
//
//   文件:       src/pages/applications/sampler.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Enumerates the current interactive desktop and submits lightweight, identity-verified tasks.
//! Icon retrieval is intentionally outside this sampler.

use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{ERROR_INVALID_DATA, HANDLE, HWND, LPARAM};
use windows_sys::Win32::System::StationsAndDesktops::{
    EnumDesktopWindows, GetProcessWindowStation, GetThreadDesktop, GetUserObjectInformationW,
    UOI_NAME,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetWindow, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsHungAppWindow,
    IsWindow, IsWindowVisible,
};
use windows_sys::core::BOOL;

use super::{TaskIdentity, WorkerTaskEntry, last_error_or_gen_failure, window_matches_identity};
use crate::system::process_identity::{
    ProcIdentity, query_process_identity_for_pid, query_process_is_32_bit,
};

#[derive(Default)]
pub(super) struct TaskSamplerCache {
    desktop_names: Option<(String, String)>,
    bitness_by_process: HashMap<ProcIdentity, bool>,
}

pub(super) struct TaskWorkerSnapshot {
    pub(super) tasks: Vec<WorkerTaskEntry>,
    pub(super) row_error: Option<u32>,
}

pub(super) type TaskWorkerResult = Result<TaskWorkerSnapshot, u32>;
pub(super) fn collect_tasks_worker(
    main_hwnd: isize,
    cache: &mut TaskSamplerCache,
) -> TaskWorkerResult {
    // 应用程序页只展示当前交互桌面的顶层窗口。直接枚举 worker 所属桌面，避免把
    // Winlogon 等不可访问安全桌面误判成整轮采样失败。
    let TaskWorkerSnapshot { tasks, row_error } =
        collect_tasks_current_winsta_worker(main_hwnd as HWND, cache)?;
    let mut valid_tasks = Vec::with_capacity(tasks.len());
    // 首先提交轻量窗口快照；图标由独立 worker 补全，慢窗口不会再阻塞列表出现。
    for task in tasks {
        if !window_matches_identity(task.identity) {
            continue;
        }
        valid_tasks.push(task);
    }
    Ok(TaskWorkerSnapshot {
        tasks: valid_tasks,
        row_error,
    })
}

fn collect_tasks_current_winsta_worker(
    main_hwnd: HWND,
    cache: &mut TaskSamplerCache,
) -> TaskWorkerResult {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        let mut tasks = Vec::<WorkerTaskEntry>::with_capacity(64);
        let mut seen_tasks = HashSet::new();
        let mut process_identities = HashMap::with_capacity(64);
        let desktop_handle = GetThreadDesktop(GetCurrentThreadId());
        if desktop_handle.is_null() {
            return Err(last_error_or_gen_failure());
        }
        if cache.desktop_names.is_none() {
            let window_station = GetProcessWindowStation();
            if window_station.is_null() {
                return Err(last_error_or_gen_failure());
            }
            cache.desktop_names = Some((
                current_user_object_name(window_station as HANDLE)?,
                current_user_object_name(desktop_handle as HANDLE)?,
            ));
        }
        let (winstation, desktop) = cache
            .desktop_names
            .as_ref()
            .cloned()
            .ok_or(ERROR_INVALID_DATA)?;
        let mut context = WindowEnumContext {
            tasks: &mut tasks as *mut Vec<WorkerTaskEntry>,
            seen_tasks: &mut seen_tasks as *mut HashSet<TaskIdentity>,
            bitness_by_process: &mut cache.bitness_by_process as *mut HashMap<ProcIdentity, bool>,
            process_identities: &mut process_identities
                as *mut HashMap<u32, Result<ProcIdentity, u32>>,
            row_error: None,
            main_hwnd,
            winstation,
            desktop,
        };
        let enumerated = EnumDesktopWindows(
            desktop_handle,
            Some(enum_window_proc),
            &mut context as *mut WindowEnumContext as LPARAM,
        );
        if enumerated == 0 {
            return Err(last_error_or_gen_failure());
        }
        let current_processes = tasks
            .iter()
            .map(|task| task.identity.process)
            .collect::<HashSet<_>>();
        cache
            .bitness_by_process
            .retain(|identity, _| current_processes.contains(identity));
        Ok(TaskWorkerSnapshot {
            tasks,
            row_error: context.row_error,
        })
    }
}

// 桌面级别的枚举上下文，传递给 enum_window_proc 回调。
struct WindowEnumContext {
    tasks: *mut Vec<WorkerTaskEntry>,
    seen_tasks: *mut HashSet<TaskIdentity>,
    bitness_by_process: *mut HashMap<ProcIdentity, bool>,
    process_identities: *mut HashMap<u32, Result<ProcIdentity, u32>>,
    row_error: Option<u32>,
    main_hwnd: HWND,
    winstation: String,
    desktop: String,
}

unsafe extern "system" fn enum_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        // 任务列表只关心可见、无 owner 的顶层窗口，并显式排除我们自己的主窗口。
        let context = &mut *(lparam as *mut WindowEnumContext);

        if !GetWindow(hwnd, windows_sys::Win32::UI::WindowsAndMessaging::GW_OWNER).is_null()
            || IsWindowVisible(hwnd) == 0
            || hwnd == context.main_hwnd
        {
            return 1;
        }

        let title = window_title(hwnd);
        if title.is_empty() || title.eq_ignore_ascii_case("Program Manager") {
            return 1;
        }

        let mut pid = 0u32;
        let thread_id = GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 || thread_id == 0 {
            return 1;
        }
        let process_identities = &mut *context.process_identities;
        let process_result = if let Some(identity) = process_identities.get(&pid).copied() {
            identity
        } else {
            let identity = query_process_identity_for_pid(pid);
            process_identities.insert(pid, identity);
            identity
        };
        let process = match process_result {
            Ok(identity) => identity,
            Err(error) => {
                if !window_still_has_identity(hwnd, pid, thread_id) {
                    return 1;
                }
                context.row_error.get_or_insert(if error == 0 {
                    ERROR_INVALID_DATA
                } else {
                    error
                });
                return 1;
            }
        };
        if !process.is_verified() {
            context.row_error.get_or_insert(ERROR_INVALID_DATA);
            return 1;
        }
        let identity = TaskIdentity {
            hwnd: hwnd as isize,
            process,
            thread_id,
        };
        let seen_tasks = &mut *context.seen_tasks;
        if !seen_tasks.insert(identity) {
            return 1;
        }
        let bitness_by_process = &mut *context.bitness_by_process;
        let is_32_bit = if let Some(&cached) = bitness_by_process.get(&process) {
            Some(cached)
        } else {
            match query_process_is_32_bit(process) {
                Ok(detected) => {
                    bitness_by_process.insert(process, detected);
                    Some(detected)
                }
                Err(error) => {
                    if window_still_has_identity(hwnd, pid, thread_id) {
                        context.row_error.get_or_insert(if error == 0 {
                            ERROR_INVALID_DATA
                        } else {
                            error
                        });
                    }
                    None
                }
            }
        };
        let tasks = &mut *context.tasks;
        tasks.push(WorkerTaskEntry {
            identity,
            title,
            is_32_bit,
            winstation: context.winstation.clone(),
            desktop: context.desktop.clone(),
            is_hung: IsHungAppWindow(hwnd) != 0,
        });
        1
    }
}

unsafe fn window_still_has_identity(hwnd: HWND, pid: u32, thread_id: u32) -> bool {
    unsafe {
        if IsWindow(hwnd) == 0 {
            return false;
        }
        let mut current_pid = 0u32;
        let current_thread_id = GetWindowThreadProcessId(hwnd, &mut current_pid);
        current_thread_id == thread_id && current_pid == pid
    }
}

unsafe fn window_title(hwnd: HWND) -> String {
    unsafe {
        let length = GetWindowTextLengthW(hwnd);
        let Ok(length) = usize::try_from(length) else {
            return String::new();
        };
        if length == 0 {
            return String::new();
        }

        let capacity = length.saturating_add(1);
        if capacity <= 260 {
            let mut buffer = [0u16; 260];
            let actual = GetWindowTextW(hwnd, buffer.as_mut_ptr(), capacity as i32).max(0) as usize;
            String::from_utf16_lossy(&buffer[..actual.min(length)])
        } else {
            let Ok(capacity_i32) = i32::try_from(capacity) else {
                return String::new();
            };
            let mut buffer = vec![0u16; capacity];
            let actual = GetWindowTextW(hwnd, buffer.as_mut_ptr(), capacity_i32).max(0) as usize;
            String::from_utf16_lossy(&buffer[..actual.min(length)])
        }
    }
}

fn current_user_object_name(handle: HANDLE) -> Result<String, u32> {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        // 窗口站和桌面名都通过 `GetUserObjectInformationW(UOI_NAME)` 读取，
        // 这里统一封装成一个 UTF-16 -> Rust String 的助手。
        let mut needed = 0u32;
        GetUserObjectInformationW(handle, UOI_NAME, null_mut(), 0, &mut needed);
        if needed == 0 {
            return Err(last_error_or_gen_failure());
        }

        let mut buffer = vec![0u16; (needed as usize / size_of::<u16>()).max(1)];
        if GetUserObjectInformationW(
            handle,
            UOI_NAME,
            buffer.as_mut_ptr() as *mut _,
            needed,
            &mut needed,
        ) == 0
        {
            return Err(last_error_or_gen_failure());
        }

        let length = buffer
            .iter()
            .position(|&value| value == 0)
            .unwrap_or(buffer.len());
        Ok(String::from_utf16_lossy(&buffer[..length]))
    }
}

// 每种 WM_GETICON 类型最多查询一次，再分别选出大小图标，避免重复跨进程超时等待。
