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
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    ERROR_BUSY, ERROR_FILE_NOT_FOUND, ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER,
    ERROR_INVALID_DATA, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA,
    ERROR_NO_MORE_FILES, ERROR_NOT_SUPPORTED, ERROR_PATH_NOT_FOUND, FILETIME, GetLastError, HANDLE,
    HWND, LPARAM, WAIT_OBJECT_0, WPARAM,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY, REG_EXPAND_SZ, REG_SZ,
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
};
use windows_sys::Win32::System::SystemInformation::{
    GROUP_AFFINITY, GetSystemTimeAsFileTime, GetWindowsDirectoryW, IMAGE_FILE_MACHINE_AMD64,
    IMAGE_FILE_MACHINE_ARM, IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_ARMNT,
    IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_IA64, IMAGE_FILE_MACHINE_THUMB,
    IMAGE_FILE_MACHINE_UNKNOWN,
};
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, CREATE_NEW_CONSOLE, CreateEventW,
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetProcessAffinityMask, GetProcessGroupAffinity, GetProcessIdOfThread, GetThreadGroupAffinity,
    HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, InitializeProcThreadAttributeList, IsWow64Process2,
    LPPROC_THREAD_ATTRIBUTE_LIST, NORMAL_PRIORITY_CLASS, OpenThread,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_SET_LIMITED_INFORMATION,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, REALTIME_PRIORITY_CLASS, STARTUPINFOEXW,
    STARTUPINFOW, SetPriorityClass, SetProcessAffinityMask, SetProcessDefaultCpuSets,
    SetThreadGroupAffinity, THREAD_QUERY_LIMITED_INFORMATION, THREAD_SET_INFORMATION,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows_sys::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, CheckDlgButton, IsDlgButtonChecked,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CB_ADDSTRING, CB_GETCURSEL, CB_RESETCONTENT, CB_SETCURSEL, CBN_SELCHANGE, EndDialog,
    GetDlgItem, IDCANCEL, IDOK, IDYES, MB_ICONERROR, MB_ICONEXCLAMATION, MB_OK, MB_YESNO,
    MessageBoxW, SendMessageW, SetWindowTextW, WM_COMMAND, WM_INITDIALOG,
};

use super::{ProcPriority, ProcessPageState};
use crate::infrastructure::native::{
    OwnedHandle, get_window_userdata, hiword, loword, set_window_userdata, to_wide_null,
};
use crate::system::cpu_sets::{CpuSetTopology, query_process_default_cpu_sets};
use crate::system::process_identity::{
    ProcIdentity, open_process_for_identity, query_process_identity_for_pid,
};
use crate::ui::dialogs::dialog_box;
use crate::ui::localization::localize_dialog;
use crate::ui::resource_ids::*;

// “设置亲和性”对话框的上下文。每个掩码都与 `topology.groups()[index]` 的组号绑定。
struct AffinityDialogContext {
    page: *mut ProcessPageState,
    topology: CpuSetTopology,
    selected_masks: Vec<usize>,
    selected_group_index: usize,
    original_default_ids: Vec<u32>,
}

const PROCESS_TREE_ACCESS: u32 =
    PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum DescendantProcessOutcome<T> {
    Verified(T),
    GoneOrReused,
    Fatal(u32),
}

pub(super) fn classify_descendant_process_result<T, F>(
    result: Result<T, u32>,
    is_verified: F,
) -> DescendantProcessOutcome<T>
where
    F: FnOnce(&T) -> bool,
{
    match result {
        Ok(value) => {
            if is_verified(&value) {
                DescendantProcessOutcome::Verified(value)
            } else {
                DescendantProcessOutcome::GoneOrReused
            }
        }
        Err(ERROR_INVALID_PARAMETER) => DescendantProcessOutcome::GoneOrReused,
        Err(error) => DescendantProcessOutcome::Fatal(error),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FailedTerminationOutcome {
    AlreadyTerminated,
    Failed(u32),
}

pub(super) const fn classify_failed_termination(
    error: u32,
    wait_result: u32,
) -> FailedTerminationOutcome {
    if wait_result == WAIT_OBJECT_0 {
        FailedTerminationOutcome::AlreadyTerminated
    } else {
        FailedTerminationOutcome::Failed(error)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ProcessTreePrepareError {
    Root(u32),
    Tree(u32),
}

pub(super) struct PreparedProcessTree {
    root_pid: u32,
    targets: Vec<(ProcIdentity, OwnedHandle)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ProcessTreeTerminationOutcome {
    any_success: bool,
    any_completed: bool,
    any_failure: bool,
    root_error: u32,
}

impl ProcessTreeTerminationOutcome {
    pub(super) const fn any_success(self) -> bool {
        self.any_success
    }

    pub(super) const fn any_failure(self) -> bool {
        self.any_failure
    }

    const fn completed_without_failure(self) -> bool {
        self.any_completed && !self.any_failure
    }
}

impl ProcessPageState {
    fn quick_confirm(&self, title: &str, body: &str) -> bool {
        // 用户关闭“确认”选项后，危险操作直接放行，保持与原版 Task Manager 行为一致。
        if !self.confirmations {
            return true;
        }

        let title_wide = to_wide_null(title);
        let body_wide = to_wide_null(body);
        // SAFETY: both UTF-16 buffers are terminated and remain alive for the synchronous call;
        // `hwnd_page` is borrowed and no ownership is transferred.
        unsafe {
            MessageBoxW(
                self.hwnd_page,
                body_wide.as_ptr(),
                title_wide.as_ptr(),
                MB_ICONEXCLAMATION | MB_YESNO,
            ) == IDYES
        }
    }

    pub(super) fn show_failure_message(&self, body: &str, error: u32) {
        let title = if self.strings.warning.is_empty() {
            "Task Manager".to_string()
        } else {
            self.strings.warning.clone()
        };
        let message = format!("{body}\r\n\r\nWin32 error: {error}");
        let title_wide = to_wide_null(&title);
        let message_wide = to_wide_null(&message);
        // SAFETY: both UTF-16 buffers are terminated and remain live for the synchronous call;
        // the page HWND is borrowed and no ownership is transferred.
        unsafe {
            MessageBoxW(
                self.hwnd_page,
                message_wide.as_ptr(),
                title_wide.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }

    // 结束指定 PID 的进程。先弹确认框，再通过 TerminateProcess 终止。
    pub(super) fn kill_process(&mut self, identity: ProcIdentity) -> bool {
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

        // SAFETY: `open_process_for_identity` returned a live handle with PROCESS_TERMINATE
        // access for the exact verified identity.
        if unsafe { TerminateProcess(handle.as_raw(), 1) } == 0 {
            let error = unsafe { GetLastError() };
            self.show_failure_message(&self.strings.cant_kill, error);
            false
        } else {
            self.paused = false;
            self.refresh_processes();
            true
        }
    }

    // 结束进程以及所有子进程。按叶子优先的顺序遍历进程树，逐进程 TerminateProcess。
    pub(super) fn kill_process_tree(&mut self, identity: ProcIdentity) -> bool {
        if !self.quick_confirm(&self.strings.warning, &self.strings.kill_tree) {
            return false;
        }

        let prepared = match prepare_process_tree_termination(identity) {
            Ok(prepared) => prepared,
            Err(ProcessTreePrepareError::Root(error)) => {
                self.show_failure_message(&self.strings.cant_kill, error);
                return false;
            }
            Err(ProcessTreePrepareError::Tree(error)) => {
                self.show_failure_message(&self.strings.kill_tree_fail_body, error);
                return false;
            }
        };
        let outcome = terminate_prepared_process_tree(prepared);

        if outcome.any_completed {
            self.paused = false;
            self.refresh_processes();
        }

        if outcome.root_error != 0 && !outcome.any_success() {
            self.show_failure_message(&self.strings.cant_kill, outcome.root_error);
            return false;
        }

        if outcome.any_failure() {
            let body_wide = to_wide_null(&self.strings.kill_tree_fail_body);
            let title_wide = to_wide_null(&self.strings.kill_tree_fail);
            // SAFETY: the page HWND is borrowed and both terminated buffers outlive this
            // synchronous message box call.
            unsafe {
                MessageBoxW(
                    self.hwnd_page,
                    body_wide.as_ptr(),
                    title_wide.as_ptr(),
                    MB_OK | MB_ICONEXCLAMATION,
                );
            }
            return false;
        }

        outcome.completed_without_failure()
    }

    // 使用目标进程位数对应的 AeDebug 命令模板启动调试器。完整模板中的第一个
    // `%ld` 接收 PID，第二个接收唯一继承给调试器的 ready-event 句柄。
    pub(super) fn attach_debugger(&mut self, identity: ProcIdentity) -> bool {
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
        let registry_view = match debugger_registry_view_for_process(target_handle.as_raw()) {
            Ok(view) => view,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_debug, error);
                return false;
            }
        };
        let debugger = match load_debugger_command(registry_view) {
            Ok(Some(debugger)) => debugger,
            Ok(None) => {
                self.show_failure_message(&self.strings.cant_debug, ERROR_FILE_NOT_FOUND);
                return false;
            }
            Err(error) => {
                self.show_failure_message(&self.strings.cant_debug, error);
                return false;
            }
        };
        if !Path::new(&debugger.executable).is_file() {
            self.show_failure_message(&self.strings.cant_debug, ERROR_FILE_NOT_FOUND);
            return false;
        }

        let security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        // SAFETY: `security` remains live for the synchronous call and requests one unnamed,
        // nonsignaled event whose returned handle is adopted immediately.
        let raw_event = unsafe { CreateEventW(&security, 0, 0, null()) };
        let Some(debugger_ready_event) = (unsafe { OwnedHandle::from_raw(raw_event) }) else {
            self.show_failure_message(&self.strings.cant_debug, nonzero_last_error());
            return false;
        };
        let command_line = match format_debugger_template(
            &debugger.template,
            identity.pid,
            debugger_ready_event.as_raw() as usize,
        ) {
            Ok(command_line) => command_line,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_debug, error);
                return false;
            }
        };
        let attributes = match ProcThreadAttributeList::for_handle(debugger_ready_event.as_raw()) {
            Ok(attributes) => attributes,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_debug, error);
                return false;
            }
        };

        let mut command_line_wide = to_wide_null(&command_line);
        let application_name = to_wide_null(&debugger.executable);
        let startup_info = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: size_of::<STARTUPINFOEXW>() as u32,
                ..unsafe { zeroed() }
            },
            lpAttributeList: attributes.as_ptr(),
        };
        let mut process_info = unsafe { zeroed::<PROCESS_INFORMATION>() };

        // SAFETY: the application/command buffers and extended startup information remain live
        // for the call. bInheritHandles is required by PROC_THREAD_ATTRIBUTE_HANDLE_LIST, which
        // restricts inheritance to the one event in `attributes`.
        let created = unsafe {
            CreateProcessW(
                application_name.as_ptr(),
                command_line_wide.as_mut_ptr(),
                null_mut(),
                null_mut(),
                1,
                CREATE_NEW_CONSOLE | EXTENDED_STARTUPINFO_PRESENT,
                null(),
                null(),
                &startup_info.StartupInfo,
                &mut process_info,
            )
        };
        let create_error = if created == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        drop(target_handle);

        if created == 0 {
            self.show_failure_message(
                &self.strings.cant_debug,
                if create_error == 0 {
                    ERROR_GEN_FAILURE
                } else {
                    create_error
                },
            );
            false
        } else {
            // SAFETY: successful CreateProcessW returned two fresh handles and this is their only
            // ownership transfer. The child owns its inherited copy of the ready event.
            match unsafe { own_created_process_handles(process_info) } {
                Ok(_) => true,
                Err(error) => {
                    self.show_failure_message(&self.strings.cant_debug, error);
                    false
                }
            }
        }
    }

    // 通过 explorer.exe /select 命令在资源管理器中定位进程的可执行文件。
    pub(super) fn open_file_location(&mut self, identity: ProcIdentity) -> bool {
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
            ..unsafe { zeroed() }
        };
        let mut process_info = unsafe { zeroed::<PROCESS_INFORMATION>() };
        // SAFETY: the mutable command line and initialized input/output structs remain live for
        // the synchronous call; successful returned handles are adopted below.
        let created = unsafe {
            CreateProcessW(
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
            )
        };
        if created == 0 {
            let error = unsafe { GetLastError() };
            self.show_failure_message(&self.strings.cant_open_file_location, error);
            return false;
        }

        // SAFETY: successful CreateProcessW returned fresh process/thread handles and this call
        // is their first and only ownership transfer.
        match unsafe { own_created_process_handles(process_info) } {
            Ok(_) => true,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_open_file_location, error);
                false
            }
        }
    }

    // 通过 SetPriorityClass 修改进程优先级类。先弹确认框，操作成功后刷新列表。
    pub(super) fn set_priority(&mut self, identity: ProcIdentity, priority: ProcPriority) -> bool {
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

        // SAFETY: the identity-validated handle has PROCESS_SET_INFORMATION access.
        if unsafe { SetPriorityClass(handle.as_raw(), priority_class) } == 0 {
            let error = unsafe { GetLastError() };
            self.show_failure_message(&self.strings.cant_change_priority, error);
            false
        } else {
            self.paused = false;
            self.refresh_processes();
            true
        }
    }

    // Single-group processes retain the classic hard-affinity API. Multi-group systems use CPU
    // Set IDs, whose group-qualified identities do not collapse at the 64-processor boundary.
    pub(super) fn set_affinity(&mut self, identity: ProcIdentity) -> bool {
        let handle = match open_process_for_identity(
            identity,
            PROCESS_QUERY_INFORMATION
                | PROCESS_QUERY_LIMITED_INFORMATION
                | PROCESS_SET_INFORMATION
                | PROCESS_SET_LIMITED_INFORMATION,
        ) {
            Ok(handle) => handle,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_set_affinity, error);
                return false;
            }
        };

        let topology = match CpuSetTopology::query(handle.as_raw()) {
            Ok(topology) => topology,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_set_affinity, error.win32_code());
                return false;
            }
        };
        let original_default_ids = match query_process_default_cpu_sets(handle.as_raw()) {
            Ok(ids) => ids,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_set_affinity, error.win32_code());
                return false;
            }
        };
        let original_process_groups = match query_process_groups(handle.as_raw()) {
            Ok(groups) => groups,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_set_affinity, error);
                return false;
            }
        };
        let selected_masks = match initial_affinity_masks(
            handle.as_raw(),
            &topology,
            &original_default_ids,
            &original_process_groups,
        ) {
            Ok(masks) => masks,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_set_affinity, error);
                return false;
            }
        };
        let selected_group_index = selected_masks
            .iter()
            .position(|mask| *mask != 0)
            .unwrap_or(0);
        let mut context = AffinityDialogContext {
            page: self as *mut ProcessPageState,
            topology,
            selected_masks,
            selected_group_index,
            original_default_ids,
        };

        match dialog_box(
            self.hinstance,
            IDD_AFFINITY,
            self.hwnd_page,
            Some(affinity_dialog_proc),
            &mut context as *mut AffinityDialogContext as LPARAM,
        ) {
            Ok(result) if result == IDOK as isize => {
                match apply_affinity_selection(handle.as_raw(), identity.pid, &context) {
                    Ok(()) => {
                        self.refresh_processes();
                        true
                    }
                    Err(error) => {
                        self.show_failure_message(&self.strings.cant_set_affinity, error);
                        false
                    }
                }
            }
            Ok(_) => false,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_set_affinity, error);
                false
            }
        }
    }
}

fn initial_affinity_masks(
    process: HANDLE,
    topology: &CpuSetTopology,
    default_ids: &[u32],
    process_groups: &[u16],
) -> Result<Vec<usize>, u32> {
    if !default_ids.is_empty() {
        return topology
            .masks_for_ids(default_ids)
            .map_err(|error| error.win32_code());
    }

    if topology.groups().len() > 1 && process_groups.len() > 1 {
        return Ok(topology.unrestricted_masks());
    }

    let mut process_mask = 0usize;
    let mut system_mask = 0usize;
    if unsafe { GetProcessAffinityMask(process, &mut process_mask, &mut system_mask) } == 0 {
        return Err(nonzero_last_error());
    }
    let group_number = process_groups
        .first()
        .copied()
        .or_else(|| topology.groups().first().map(|group| group.number))
        .ok_or(ERROR_NOT_SUPPORTED)?;
    let group_index = topology
        .groups()
        .iter()
        .position(|group| group.number == group_number)
        .ok_or(ERROR_INVALID_DATA)?;
    let group = &topology.groups()[group_index];
    let selected = process_mask & system_mask & group.assignable_mask;
    if selected == 0 {
        return Err(ERROR_NOT_SUPPORTED);
    }
    let mut masks = vec![0usize; topology.groups().len()];
    masks[group_index] = selected;
    Ok(masks)
}

fn apply_affinity_selection(
    process: HANDLE,
    process_id: u32,
    context: &AffinityDialogContext,
) -> Result<(), u32> {
    let selected_ids = context
        .topology
        .ids_for_masks(&context.selected_masks)
        .map_err(|error| error.win32_code())?;
    if selected_ids.is_empty() {
        return Err(ERROR_INVALID_PARAMETER);
    }

    if context.topology.groups().len() == 1 {
        set_process_default_cpu_sets(process, &[])?;
        if unsafe { SetProcessAffinityMask(process, context.selected_masks[0]) } == 0 {
            let error = nonzero_last_error();
            let _ = set_process_default_cpu_sets(process, &context.original_default_ids);
            return Err(error);
        }
        return Ok(());
    }

    set_process_default_cpu_sets(process, &selected_ids)?;
    let selected_groups = context
        .topology
        .groups()
        .iter()
        .zip(context.selected_masks.iter().copied())
        .filter(|(_, mask)| *mask != 0)
        .map(|(group, mask)| (group.number, mask))
        .collect::<Vec<_>>();

    // CPU Sets span groups, but a restrictive thread affinity mask takes precedence over a
    // conflicting CPU Set assignment. Apply a group-qualified hard mask to every existing thread
    // so an old per-thread affinity cannot keep using a CPU the user just deselected. The process
    // default CPU Sets above cover threads created after the verified thread snapshot.
    if let Err(error) = apply_thread_group_affinities(process_id, &selected_groups) {
        let _ = set_process_default_cpu_sets(process, &context.original_default_ids);
        return Err(error);
    }
    Ok(())
}

fn set_process_default_cpu_sets(process: HANDLE, ids: &[u32]) -> Result<(), u32> {
    let (pointer, count) = if ids.is_empty() {
        (null(), 0)
    } else {
        (
            ids.as_ptr(),
            u32::try_from(ids.len()).map_err(|_| ERROR_INVALID_PARAMETER)?,
        )
    };
    if unsafe { SetProcessDefaultCpuSets(process, pointer, count) } == 0 {
        Err(nonzero_last_error())
    } else {
        Ok(())
    }
}

struct ChangedThreadAffinity {
    handle: OwnedHandle,
    previous: GROUP_AFFINITY,
}

fn apply_thread_group_affinities(
    process_id: u32,
    selected_groups: &[(u16, usize)],
) -> Result<(), u32> {
    if selected_groups.is_empty() {
        return Err(ERROR_INVALID_PARAMETER);
    }

    let mut changed = Vec::<ChangedThreadAffinity>::new();
    let mut seen = HashSet::<u32>::new();
    let mut assignment_index = 0usize;
    let result = (|| -> Result<(), u32> {
        let thread_ids = enumerate_process_threads(process_id)?;
        for thread_id in thread_ids {
            let raw_thread = unsafe {
                OpenThread(
                    THREAD_QUERY_LIMITED_INFORMATION | THREAD_SET_INFORMATION,
                    0,
                    thread_id,
                )
            };
            // SAFETY: a successful OpenThread call returns a newly opened handle owned by this
            // scope and released with CloseHandle; null is mapped to None.
            let Some(thread) = (unsafe { OwnedHandle::from_raw(raw_thread) }) else {
                let error = nonzero_last_error();
                if matches!(error, ERROR_INVALID_HANDLE | ERROR_INVALID_PARAMETER) {
                    continue;
                }
                return Err(error);
            };
            if unsafe { GetProcessIdOfThread(thread.as_raw()) } != process_id {
                return Err(ERROR_INVALID_DATA);
            }

            let (group, mask) = affinity_target_for_thread(assignment_index, selected_groups)
                .ok_or(ERROR_INVALID_PARAMETER)?;
            assignment_index += 1;
            let target = GROUP_AFFINITY {
                Mask: mask,
                Group: group,
                Reserved: [0; 3],
            };
            let mut previous = unsafe { zeroed::<GROUP_AFFINITY>() };
            if unsafe { SetThreadGroupAffinity(thread.as_raw(), &target, &mut previous) } == 0 {
                let error = nonzero_last_error();
                if error == ERROR_INVALID_HANDLE {
                    continue;
                }
                return Err(error);
            }
            seen.insert(thread_id);
            changed.push(ChangedThreadAffinity {
                handle: thread,
                previous,
            });
        }
        if changed.is_empty() {
            return Err(ERROR_NOT_SUPPORTED);
        }
        verify_racing_thread_affinities(process_id, &seen, selected_groups)?;
        Ok(())
    })();

    if result.is_err() {
        for changed_thread in changed.iter().rev() {
            unsafe {
                SetThreadGroupAffinity(
                    changed_thread.handle.as_raw(),
                    &changed_thread.previous,
                    null_mut(),
                );
            }
        }
    }
    result
}

fn verify_racing_thread_affinities(
    process_id: u32,
    changed_thread_ids: &HashSet<u32>,
    selected_groups: &[(u16, usize)],
) -> Result<(), u32> {
    for thread_id in enumerate_process_threads(process_id)? {
        if changed_thread_ids.contains(&thread_id) {
            continue;
        }
        let raw_thread = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, thread_id) };
        // SAFETY: a successful OpenThread call returns a newly opened handle owned by this scope
        // and released with CloseHandle; null is mapped to None.
        let Some(thread) = (unsafe { OwnedHandle::from_raw(raw_thread) }) else {
            let error = nonzero_last_error();
            if matches!(error, ERROR_INVALID_HANDLE | ERROR_INVALID_PARAMETER) {
                continue;
            }
            return Err(error);
        };
        if unsafe { GetProcessIdOfThread(thread.as_raw()) } != process_id {
            return Err(ERROR_INVALID_DATA);
        }
        let mut affinity = unsafe { zeroed::<GROUP_AFFINITY>() };
        if unsafe { GetThreadGroupAffinity(thread.as_raw(), &mut affinity) } == 0 {
            return Err(nonzero_last_error());
        }
        if !affinity_is_within_selection(affinity.Group, affinity.Mask, selected_groups) {
            return Err(ERROR_BUSY);
        }
    }
    Ok(())
}

pub(super) fn affinity_is_within_selection(
    group: u16,
    mask: usize,
    selected_groups: &[(u16, usize)],
) -> bool {
    mask != 0
        && selected_groups
            .iter()
            .find_map(|(selected_group, selected_mask)| {
                (*selected_group == group).then_some(*selected_mask)
            })
            .is_some_and(|selected_mask| mask & !selected_mask == 0)
}

pub(super) fn affinity_target_for_thread(
    thread_index: usize,
    selected_groups: &[(u16, usize)],
) -> Option<(u16, usize)> {
    selected_groups
        .get(thread_index.checked_rem(selected_groups.len())?)
        .copied()
}

fn enumerate_process_threads(process_id: u32) -> Result<Vec<u32>, u32> {
    let raw_snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    // SAFETY: CreateToolhelp32Snapshot returns either INVALID_HANDLE_VALUE or a fresh snapshot
    // handle owned by this scope and released with CloseHandle.
    let Some(snapshot) = (unsafe { OwnedHandle::from_raw(raw_snapshot) }) else {
        return Err(nonzero_last_error());
    };
    let mut entry = unsafe { zeroed::<THREADENTRY32>() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    if unsafe { Thread32First(snapshot.as_raw(), &mut entry) } == 0 {
        let error = nonzero_last_error();
        return if error == ERROR_NO_MORE_FILES {
            Ok(Vec::new())
        } else {
            Err(error)
        };
    }

    let mut thread_ids = Vec::new();
    loop {
        if entry.th32OwnerProcessID == process_id {
            thread_ids.push(entry.th32ThreadID);
        }
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        if unsafe { Thread32Next(snapshot.as_raw(), &mut entry) } == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_NO_MORE_FILES {
                return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
            }
            break;
        }
    }
    thread_ids.sort_unstable();
    thread_ids.dedup();
    Ok(thread_ids)
}

fn query_process_groups(process: HANDLE) -> Result<Vec<u16>, u32> {
    let mut required = 0u16;
    if unsafe { GetProcessGroupAffinity(process, &mut required, null_mut()) } != 0 {
        return Err(ERROR_INVALID_DATA);
    }
    let error = unsafe { GetLastError() };
    if error != ERROR_INSUFFICIENT_BUFFER || required == 0 {
        return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
    }

    for _ in 0..3 {
        // `GetProcessGroupAffinity` requires DWORD-aligned storage even though the public element
        // type is `USHORT`, so back the array with `u32` words and expose only the required u16s.
        let mut storage = vec![0u32; usize::from(required).div_ceil(2)];
        let capacity = storage.len() * 2;
        let mut returned = u16::try_from(capacity).unwrap_or(u16::MAX);
        if unsafe {
            GetProcessGroupAffinity(process, &mut returned, storage.as_mut_ptr().cast::<u16>())
        } != 0
        {
            if returned == 0 || usize::from(returned) > capacity {
                return Err(ERROR_INVALID_DATA);
            }
            let mut groups = Vec::with_capacity(usize::from(returned));
            for index in 0..usize::from(returned) {
                groups.push(unsafe { *storage.as_ptr().cast::<u16>().add(index) });
            }
            groups.sort_unstable();
            groups.dedup();
            return Ok(groups);
        }
        let error = unsafe { GetLastError() };
        if error != ERROR_INSUFFICIENT_BUFFER || returned <= required {
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        }
        required = returned;
    }
    Err(ERROR_INSUFFICIENT_BUFFER)
}

fn nonzero_last_error() -> u32 {
    let error = unsafe { GetLastError() };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

// The dialog shows one processor group at a time, preserving the familiar 64-checkbox layout
// while retaining independent selections for every group.
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
                let context = &mut *(lparam as *mut AffinityDialogContext);
                initialize_affinity_group_selector(hwnd, context);
                render_affinity_group(hwnd, context);
                1
            }
            WM_COMMAND => {
                let command = i32::from(loword(wparam));
                if command == IDC_AFFINITY_GROUP_SELECTOR && hiword(wparam) == CBN_SELCHANGE as u16
                {
                    let context = &mut *(get_window_userdata(hwnd) as *mut AffinityDialogContext);
                    save_affinity_group(hwnd, context);
                    let selection = SendMessageW(
                        GetDlgItem(hwnd, IDC_AFFINITY_GROUP_SELECTOR),
                        CB_GETCURSEL,
                        0,
                        0,
                    );
                    if selection >= 0 {
                        let selection = selection as usize;
                        if selection < context.topology.groups().len() {
                            context.selected_group_index = selection;
                            render_affinity_group(hwnd, context);
                        }
                    }
                    return 1;
                }

                match command {
                    IDCANCEL => {
                        EndDialog(hwnd, IDCANCEL as isize);
                        1
                    }
                    IDOK => {
                        let context =
                            &mut *(get_window_userdata(hwnd) as *mut AffinityDialogContext);
                        let page = &*context.page;
                        save_affinity_group(hwnd, context);
                        if context.selected_masks.iter().all(|mask| *mask == 0) {
                            let title_wide = to_wide_null(&page.strings.invalid_option);
                            let body_wide = to_wide_null(&page.strings.no_affinity_mask);
                            MessageBoxW(
                                hwnd,
                                body_wide.as_ptr(),
                                title_wide.as_ptr(),
                                MB_ICONERROR,
                            );
                            1
                        } else {
                            EndDialog(hwnd, IDOK as isize);
                            1
                        }
                    }
                    _ => 0,
                }
            }
            _ => 0,
        }
    }
}

fn initialize_affinity_group_selector(hwnd: HWND, context: &mut AffinityDialogContext) {
    let selector = unsafe { GetDlgItem(hwnd, IDC_AFFINITY_GROUP_SELECTOR) };
    unsafe { SendMessageW(selector, CB_RESETCONTENT, 0, 0) };
    for group in context.topology.groups() {
        let label = if context.topology.groups().len() > 1 {
            format!("G{} ({})", group.number, group.processor_mask.count_ones())
        } else {
            format!("G{}", group.number)
        };
        let wide = to_wide_null(&label);
        unsafe {
            SendMessageW(selector, CB_ADDSTRING, 0, wide.as_ptr() as isize);
        }
    }
    if context.selected_group_index >= context.topology.groups().len() {
        context.selected_group_index = 0;
    }
    unsafe {
        SendMessageW(selector, CB_SETCURSEL, context.selected_group_index, 0);
        EnableWindow(selector, i32::from(context.topology.groups().len() > 1));
    }
}

fn render_affinity_group(hwnd: HWND, context: &AffinityDialogContext) {
    let group = &context.topology.groups()[context.selected_group_index];
    let selected_mask = context.selected_masks[context.selected_group_index];
    let multiple_groups = context.topology.groups().len() > 1;
    for cpu_index in 0..=MAX_AFFINITY_CPU {
        let control_id = IDC_CPU0 + cpu_index;
        let mask = affinity_cpu_mask(cpu_index);
        let enabled = mask != 0 && group.assignable_mask & mask != 0;
        let label = if multiple_groups {
            format!("G{}:CPU {cpu_index}", group.number)
        } else {
            format!("CPU {cpu_index}")
        };
        let wide = to_wide_null(&label);
        unsafe {
            SetWindowTextW(GetDlgItem(hwnd, control_id), wide.as_ptr());
            EnableWindow(GetDlgItem(hwnd, control_id), i32::from(enabled));
            CheckDlgButton(
                hwnd,
                control_id,
                if enabled && selected_mask & mask != 0 {
                    BST_CHECKED
                } else {
                    BST_UNCHECKED
                },
            );
        }
    }
}

fn save_affinity_group(hwnd: HWND, context: &mut AffinityDialogContext) {
    let group = &context.topology.groups()[context.selected_group_index];
    let mut selected_mask = 0usize;
    for cpu_index in 0..=MAX_AFFINITY_CPU {
        let mask = affinity_cpu_mask(cpu_index);
        if mask != 0
            && group.assignable_mask & mask != 0
            && unsafe { IsDlgButtonChecked(hwnd, IDC_CPU0 + cpu_index) } == BST_CHECKED
        {
            selected_mask |= mask;
        }
    }
    context.selected_masks[context.selected_group_index] = selected_mask;
}

pub(super) fn affinity_cpu_mask(cpu_index: i32) -> usize {
    u32::try_from(cpu_index)
        .ok()
        .and_then(|shift| 1usize.checked_shl(shift))
        .unwrap_or(0)
}

const AEDEBUG_KEY: &str = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\AeDebug";
const AEDEBUG_VALUE: &str = "Debugger";
const MAX_DEBUGGER_COMMAND_BYTES: u32 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DebuggerRegistryView {
    Native,
    Registry32,
    Registry64,
}

impl DebuggerRegistryView {
    const fn access_mask(self) -> u32 {
        match self {
            Self::Native => 0,
            Self::Registry32 => KEY_WOW64_32KEY,
            Self::Registry64 => KEY_WOW64_64KEY,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DebuggerCommand {
    template: String,
    executable: String,
}

struct ProcThreadAttributeList {
    storage: Vec<usize>,
    handles: Box<[HANDLE; 1]>,
    initialized: bool,
}

impl ProcThreadAttributeList {
    fn for_handle(handle: HANDLE) -> Result<Self, u32> {
        if handle.is_null() {
            return Err(ERROR_INVALID_HANDLE);
        }

        let mut byte_count = 0usize;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut byte_count);
        }
        if byte_count == 0 {
            return Err(nonzero_last_error());
        }

        let word_count = byte_count.div_ceil(size_of::<usize>());
        let mut value = Self {
            storage: vec![0usize; word_count],
            handles: Box::new([handle]),
            initialized: false,
        };
        if unsafe { InitializeProcThreadAttributeList(value.as_ptr(), 1, 0, &mut byte_count) } == 0
        {
            return Err(nonzero_last_error());
        }
        value.initialized = true;
        if unsafe {
            UpdateProcThreadAttribute(
                value.as_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                value.handles.as_mut_ptr().cast::<c_void>(),
                size_of::<HANDLE>(),
                null_mut(),
                null_mut(),
            )
        } == 0
        {
            return Err(nonzero_last_error());
        }
        Ok(value)
    }

    fn as_ptr(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST
    }
}

impl Drop for ProcThreadAttributeList {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
        }
    }
}

pub(super) fn load_debugger_path() -> Result<Option<String>, u32> {
    // This is an availability probe for menu state. Launch-time selection is repeated against
    // the selected process' machine type so the command can never use the wrong registry view.
    let mut first_error = None;
    for view in [
        DebuggerRegistryView::Native,
        DebuggerRegistryView::Registry64,
        DebuggerRegistryView::Registry32,
    ] {
        match load_debugger_command(view) {
            Ok(Some(command)) if Path::new(&command.executable).is_file() => {
                return Ok(Some(command.executable));
            }
            Ok(_) => {}
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(None)
    }
}

fn load_debugger_command(view: DebuggerRegistryView) -> Result<Option<DebuggerCommand>, u32> {
    let Some((raw_command, value_type)) = read_aedebug_string(view)? else {
        return Ok(None);
    };
    parse_debugger_command(&raw_command, value_type)
}

fn read_aedebug_string(view: DebuggerRegistryView) -> Result<Option<(String, u32)>, u32> {
    unsafe {
        let key_name = to_wide_null(AEDEBUG_KEY);
        let value_name = to_wide_null(AEDEBUG_VALUE);
        let mut key: HKEY = null_mut();
        let open_status = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            key_name.as_ptr(),
            0,
            KEY_READ | view.access_mask(),
            &mut key,
        );
        if open_status == ERROR_FILE_NOT_FOUND || open_status == ERROR_PATH_NOT_FOUND {
            return Ok(None);
        }
        if open_status != 0 {
            return Err(open_status);
        }

        let mut value_type = 0u32;
        let mut value_size = 0u32;
        let size_status = RegQueryValueExW(
            key,
            value_name.as_ptr(),
            null_mut(),
            &mut value_type,
            null_mut(),
            &mut value_size,
        );
        if size_status == ERROR_FILE_NOT_FOUND {
            let close_status = RegCloseKey(key);
            return if close_status == 0 {
                Ok(None)
            } else {
                Err(close_status)
            };
        }
        if size_status != 0 {
            RegCloseKey(key);
            return Err(size_status);
        }
        if !matches!(value_type, REG_SZ | REG_EXPAND_SZ)
            || value_size < size_of::<u16>() as u32
            || !value_size.is_multiple_of(size_of::<u16>() as u32)
            || value_size > MAX_DEBUGGER_COMMAND_BYTES
        {
            RegCloseKey(key);
            return Err(ERROR_INVALID_DATA);
        }

        let mut buffer = vec![0u16; value_size as usize / size_of::<u16>()];
        let mut actual_size = value_size;
        let read_status = RegQueryValueExW(
            key,
            value_name.as_ptr(),
            null_mut(),
            &mut value_type,
            buffer.as_mut_ptr().cast::<u8>(),
            &mut actual_size,
        );
        let close_status = RegCloseKey(key);
        if read_status == ERROR_MORE_DATA {
            return Err(ERROR_MORE_DATA);
        }
        if read_status != 0 {
            return Err(read_status);
        }
        if close_status != 0 {
            return Err(close_status);
        }
        if !matches!(value_type, REG_SZ | REG_EXPAND_SZ)
            || actual_size < size_of::<u16>() as u32
            || !actual_size.is_multiple_of(size_of::<u16>() as u32)
            || actual_size > value_size
        {
            return Err(ERROR_INVALID_DATA);
        }
        let units = actual_size as usize / size_of::<u16>();
        let Some(length) = buffer[..units].iter().position(|value| *value == 0) else {
            return Err(ERROR_INVALID_DATA);
        };
        Ok(Some((
            String::from_utf16(&buffer[..length]).map_err(|_| ERROR_INVALID_DATA)?,
            value_type,
        )))
    }
}

fn debugger_registry_view_for_process(process: HANDLE) -> Result<DebuggerRegistryView, u32> {
    if process.is_null() {
        return Err(ERROR_INVALID_HANDLE);
    }
    let mut process_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut native_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    if unsafe { IsWow64Process2(process, &mut process_machine, &mut native_machine) } == 0 {
        return Err(nonzero_last_error());
    }
    debugger_registry_view_for_machines(process_machine, native_machine)
}

fn debugger_registry_view_for_machines(
    process_machine: u16,
    native_machine: u16,
) -> Result<DebuggerRegistryView, u32> {
    let effective_machine = if process_machine == IMAGE_FILE_MACHINE_UNKNOWN {
        native_machine
    } else {
        process_machine
    };
    let target_is_32_bit = match effective_machine {
        IMAGE_FILE_MACHINE_I386
        | IMAGE_FILE_MACHINE_ARM
        | IMAGE_FILE_MACHINE_ARMNT
        | IMAGE_FILE_MACHINE_THUMB => true,
        IMAGE_FILE_MACHINE_AMD64 | IMAGE_FILE_MACHINE_ARM64 | IMAGE_FILE_MACHINE_IA64 => false,
        _ => return Err(ERROR_NOT_SUPPORTED),
    };
    let native_is_64_bit = match native_machine {
        IMAGE_FILE_MACHINE_AMD64 | IMAGE_FILE_MACHINE_ARM64 | IMAGE_FILE_MACHINE_IA64 => true,
        IMAGE_FILE_MACHINE_I386
        | IMAGE_FILE_MACHINE_ARM
        | IMAGE_FILE_MACHINE_ARMNT
        | IMAGE_FILE_MACHINE_THUMB => false,
        _ => return Err(ERROR_NOT_SUPPORTED),
    };
    Ok(if !native_is_64_bit {
        DebuggerRegistryView::Native
    } else if target_is_32_bit {
        DebuggerRegistryView::Registry32
    } else {
        DebuggerRegistryView::Registry64
    })
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

fn parse_debugger_command(
    command_line: &str,
    value_type: u32,
) -> Result<Option<DebuggerCommand>, u32> {
    let expanded = if value_type == REG_EXPAND_SZ {
        expand_environment_variables(command_line)?
    } else if value_type == REG_SZ {
        command_line.to_string()
    } else {
        return Err(ERROR_INVALID_DATA);
    };
    parse_expanded_debugger_command(&expanded)
}

fn parse_expanded_debugger_command(command_line: &str) -> Result<Option<DebuggerCommand>, u32> {
    let template = command_line.trim().to_string();
    let executable = extract_first_command_token(&template);
    let file_name = Path::new(&executable)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if executable.is_empty()
        || !Path::new(&executable).is_absolute()
        || file_name.eq_ignore_ascii_case("drwtsn32")
        || file_name.eq_ignore_ascii_case("drwtsn32.exe")
    {
        return Ok(None);
    }
    validate_debugger_template(&template)?;
    Ok(Some(DebuggerCommand {
        template,
        executable,
    }))
}

fn validate_debugger_template(template: &str) -> Result<(), u32> {
    let mut placeholders = 0usize;
    let mut chars = template.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            continue;
        }
        match chars.next() {
            Some('%') => {}
            Some('l' | 'L') => {
                if !matches!(chars.next(), Some('d' | 'D')) {
                    return Err(ERROR_INVALID_DATA);
                }
                placeholders += 1;
            }
            Some('p' | 'P') => return Err(ERROR_NOT_SUPPORTED),
            Some(_) | None => return Err(ERROR_INVALID_DATA),
        }
    }
    if placeholders == 2 {
        Ok(())
    } else {
        Err(ERROR_INVALID_DATA)
    }
}

fn format_debugger_template(
    template: &str,
    process_id: u32,
    event_handle: usize,
) -> Result<String, u32> {
    validate_debugger_template(template)?;
    let replacements = [process_id.to_string(), event_handle.to_string()];
    let mut replacement_index = 0usize;
    let mut output = String::with_capacity(template.len() + 32);
    let mut chars = template.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            output.push(ch);
            continue;
        }
        match chars.next() {
            Some('%') => output.push('%'),
            Some('l' | 'L') => {
                if !matches!(chars.next(), Some('d' | 'D')) {
                    return Err(ERROR_INVALID_DATA);
                }
                output.push_str(
                    replacements
                        .get(replacement_index)
                        .ok_or(ERROR_INVALID_DATA)?,
                );
                replacement_index += 1;
            }
            Some('p' | 'P') => return Err(ERROR_NOT_SUPPORTED),
            Some(_) | None => return Err(ERROR_INVALID_DATA),
        }
    }
    if replacement_index == replacements.len() {
        Ok(output)
    } else {
        Err(ERROR_INVALID_DATA)
    }
}

// Compatibility helper used by the existing pure parsing tests.
#[cfg(test)]
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
    } else if value_type == REG_SZ {
        command_line.to_string()
    } else {
        return None;
    };
    let executable = extract_first_command_token(&expanded);
    let file_name = Path::new(&executable)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if executable.is_empty()
        || file_name.eq_ignore_ascii_case("drwtsn32")
        || file_name.eq_ignore_ascii_case("drwtsn32.exe")
    {
        None
    } else {
        Some(executable)
    }
}

// 展开字符串中的环境变量（如 %SystemRoot%）。
// 使用 Win32 ExpandEnvironmentStringsW API，正确处理 WOW64 重定向和 %% 转义。
fn expand_environment_variables(command_line: &str) -> Result<String, u32> {
    let wide_input = to_wide_null(command_line);
    let required = unsafe { ExpandEnvironmentStringsW(wide_input.as_ptr(), null_mut(), 0) };
    if required == 0 {
        return Err(nonzero_last_error());
    }
    let mut buffer = vec![0u16; required as usize];
    let written =
        unsafe { ExpandEnvironmentStringsW(wide_input.as_ptr(), buffer.as_mut_ptr(), required) };
    if written == 0 || written > required {
        return Err(nonzero_last_error());
    }
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Ok(String::from_utf16_lossy(&buffer[..len]))
}

// 先构建完整的已验证句柄集合，再把不可逆的终止阶段与 UI 结果呈现分离。
pub(super) fn prepare_process_tree_termination(
    root_identity: ProcIdentity,
) -> Result<PreparedProcessTree, ProcessTreePrepareError> {
    let mut root_handle = Some(
        open_process_for_identity(root_identity, PROCESS_TREE_ACCESS)
            .map_err(ProcessTreePrepareError::Root)?,
    );
    let termination_order = collect_process_tree_termination_order(root_identity)
        .map_err(ProcessTreePrepareError::Tree)?;
    if termination_order.is_empty() {
        return Err(ProcessTreePrepareError::Tree(ERROR_GEN_FAILURE));
    }

    // Validate and own every available handle before terminating anything. A descendant that
    // disappeared or changed identity is no longer an actionable target, while permission and
    // system errors still abort preparation before the root can be partially terminated.
    let mut targets = Vec::with_capacity(termination_order.len());
    for target_identity in termination_order {
        if target_identity == root_identity {
            let handle = root_handle
                .take()
                .ok_or(ProcessTreePrepareError::Tree(ERROR_INVALID_DATA))?;
            targets.push((target_identity, handle));
            continue;
        }

        match classify_descendant_process_result(
            open_process_for_identity(target_identity, PROCESS_TREE_ACCESS),
            |_| true,
        ) {
            DescendantProcessOutcome::Verified(handle) => {
                targets.push((target_identity, handle));
            }
            DescendantProcessOutcome::GoneOrReused => {}
            DescendantProcessOutcome::Fatal(error) => {
                return Err(ProcessTreePrepareError::Tree(error));
            }
        }
    }

    if root_handle.is_some() {
        return Err(ProcessTreePrepareError::Tree(ERROR_GEN_FAILURE));
    }

    Ok(PreparedProcessTree {
        root_pid: root_identity.pid,
        targets,
    })
}

pub(super) fn terminate_prepared_process_tree(
    prepared: PreparedProcessTree,
) -> ProcessTreeTerminationOutcome {
    let mut outcome = ProcessTreeTerminationOutcome::default();
    for (target_identity, handle) in prepared.targets {
        if unsafe { TerminateProcess(handle.as_raw(), 1) } != 0 {
            outcome.any_success = true;
            outcome.any_completed = true;
            continue;
        }

        // Capture TerminateProcess's error before WaitForSingleObject can change last-error.
        let error = nonzero_last_error();
        let wait_result = unsafe { WaitForSingleObject(handle.as_raw(), 0) };
        match classify_failed_termination(error, wait_result) {
            FailedTerminationOutcome::AlreadyTerminated => {
                outcome.any_completed = true;
            }
            FailedTerminationOutcome::Failed(error) => {
                outcome.any_failure = true;
                if target_identity.pid == prepared.root_pid {
                    outcome.root_error = error;
                }
            }
        }
    }
    outcome
}

fn collect_process_tree_termination_order(
    root_identity: ProcIdentity,
) -> Result<Vec<ProcIdentity>, u32> {
    let raw_snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    // SAFETY: a successful CreateToolhelp32Snapshot call returns a fresh snapshot handle owned
    // by this scope and released with CloseHandle.
    let Some(snapshot) = (unsafe { OwnedHandle::from_raw(raw_snapshot) }) else {
        let error = unsafe { GetLastError() };
        return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
    };

    let mut snapshot_time = unsafe { zeroed::<FILETIME>() };
    unsafe { GetSystemTimeAsFileTime(&mut snapshot_time) };
    let snapshot_time_100ns = filetime_to_u64(snapshot_time);

    let mut child_map = HashMap::<u32, Vec<u32>>::new();
    let mut process_entry = unsafe { zeroed::<PROCESSENTRY32W>() };
    process_entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

    if unsafe { Process32FirstW(snapshot.as_raw(), &mut process_entry) } == 0 {
        let error = unsafe { GetLastError() };
        return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
    }

    loop {
        child_map
            .entry(process_entry.th32ParentProcessID)
            .or_default()
            .push(process_entry.th32ProcessID);
        if unsafe { Process32NextW(snapshot.as_raw(), &mut process_entry) } == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_NO_MORE_FILES {
                return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
            }
            break;
        }
    }

    // The snapshot is keyed only by PID. Revalidate the root after enumeration so a root that
    // exited and had its PID reused cannot lend the replacement's children to this tree.
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
fn collect_verified_process_tree_children(
    parent: ProcIdentity,
    snapshot_time_100ns: u64,
    child_map: &HashMap<u32, Vec<u32>>,
    visited: &mut HashSet<u32>,
    order: &mut Vec<ProcIdentity>,
) -> Result<(), u32> {
    if !visited.insert(parent.pid) {
        return Ok(());
    }

    if let Some(children) = child_map.get(&parent.pid) {
        for &child_pid in children {
            if visited.contains(&child_pid) {
                continue;
            }
            let child = match classify_descendant_process_result(
                query_process_identity_for_pid(child_pid),
                |child| is_valid_process_tree_edge(parent, *child, snapshot_time_100ns),
            ) {
                DescendantProcessOutcome::Verified(child) => child,
                DescendantProcessOutcome::GoneOrReused => continue,
                DescendantProcessOutcome::Fatal(error) => return Err(error),
            };
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

/// Adopts the two handles returned by a successful `CreateProcessW` call.
///
/// # Safety
///
/// `process_info.hProcess` and `process_info.hThread` must be fresh, uniquely owned handles from
/// the same successful `CreateProcessW` call and must not have been closed or adopted elsewhere.
unsafe fn own_created_process_handles(
    process_info: PROCESS_INFORMATION,
) -> Result<(OwnedHandle, OwnedHandle), u32> {
    let process = unsafe { OwnedHandle::from_raw(process_info.hProcess) };
    let thread = unsafe { OwnedHandle::from_raw(process_info.hThread) };
    match (process, thread) {
        (Some(process), Some(thread)) => Ok((process, thread)),
        _ => Err(ERROR_INVALID_HANDLE),
    }
}

fn query_process_image_path(identity: ProcIdentity) -> Result<String, u32> {
    let handle = open_process_for_identity(identity, PROCESS_QUERY_LIMITED_INFORMATION)?;

    let mut capacity = 32768u32;
    let mut buffer = vec![0u16; capacity as usize];
    // SAFETY: the identity-validated handle is live and `buffer` is writable for `capacity`
    // UTF-16 code units. The API updates `capacity` to the initialized length.
    let success = unsafe {
        QueryFullProcessImageNameW(handle.as_raw(), 0, buffer.as_mut_ptr(), &mut capacity)
    };
    let error = if success == 0 {
        unsafe { GetLastError() }
    } else {
        0
    };
    drop(handle);

    if success == 0 {
        return Err(error);
    }

    Ok(String::from_utf16_lossy(&buffer[..capacity as usize]))
}

fn query_windows_directory() -> Result<String, u32> {
    let mut buffer = vec![0u16; 260];
    loop {
        // SAFETY: `buffer` is writable for the advertised number of UTF-16 code units.
        let length =
            unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
        if length == 0 {
            return Err(unsafe { GetLastError() });
        }
        if length < buffer.len() {
            return Ok(String::from_utf16_lossy(&buffer[..length]));
        }
        buffer.resize(length.saturating_add(1), 0);
    }
}

#[cfg(test)]
mod debugger_tests {
    use super::*;

    #[test]
    fn full_aedebug_template_preserves_debugger_specific_arguments() {
        let template = r#""C:\Debuggers\windbg.exe" -p %ld -e %ld -g"#;
        let command = parse_expanded_debugger_command(template).unwrap().unwrap();
        assert_eq!(command.executable, r"C:\Debuggers\windbg.exe");
        assert_eq!(
            format_debugger_template(&command.template, 1234, 5678).unwrap(),
            r#""C:\Debuggers\windbg.exe" -p 1234 -e 5678 -g"#
        );
    }

    #[test]
    fn visual_studio_jit_template_and_literal_percent_are_supported() {
        let template = r#""C:\Windows\System32\vsjitdebugger.exe" -p %ld -e %ld --label 100%%"#;
        assert_eq!(
            format_debugger_template(template, 42, 99).unwrap(),
            r#""C:\Windows\System32\vsjitdebugger.exe" -p 42 -e 99 --label 100%"#
        );
    }

    #[test]
    fn unsupported_or_ambiguous_templates_are_rejected() {
        assert_eq!(
            parse_expanded_debugger_command(r#""C:\Debuggers\dbg.exe" -p %ld -e %ld -j 0x%p"#),
            Err(ERROR_NOT_SUPPORTED)
        );
        for template in [
            r#""C:\Debuggers\dbg.exe" -p %ld"#,
            r#""C:\Debuggers\dbg.exe" -p %ld -e %ld -x %ld"#,
            r#""C:\Debuggers\dbg.exe" -p %q -e %ld"#,
        ] {
            assert!(parse_expanded_debugger_command(template).is_err());
        }
        assert!(
            parse_expanded_debugger_command(r#""relative\dbg.exe" -p %ld -e %ld"#)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn target_machine_selects_the_matching_registry_view() {
        assert_eq!(
            debugger_registry_view_for_machines(IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_AMD64,)
                .unwrap(),
            DebuggerRegistryView::Registry32
        );
        assert_eq!(
            debugger_registry_view_for_machines(
                IMAGE_FILE_MACHINE_UNKNOWN,
                IMAGE_FILE_MACHINE_AMD64,
            )
            .unwrap(),
            DebuggerRegistryView::Registry64
        );
        assert_eq!(
            debugger_registry_view_for_machines(
                IMAGE_FILE_MACHINE_UNKNOWN,
                IMAGE_FILE_MACHINE_I386,
            )
            .unwrap(),
            DebuggerRegistryView::Native
        );
        assert_eq!(
            debugger_registry_view_for_machines(
                IMAGE_FILE_MACHINE_ARMNT,
                IMAGE_FILE_MACHINE_ARM64,
            )
            .unwrap(),
            DebuggerRegistryView::Registry32
        );
    }
}
