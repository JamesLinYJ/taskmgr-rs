// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 诊断日志对话框
//
//   文件:       src/app/diagnostics_dialog.rs
//
//   日期:       2026年07月27日
//   环境:       Fedora Linux 45 x86_64；Linux 内核 7.2.0-0.rc4.260725g0ce37745d4bf.39.fc45.x86_64；Rust 1.97.1；MinGW GCC 16.1.1；Wine 11.14 (Staging)
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! “帮助 -> 诊断日志”模态对话框。
//!
//! 对话框只修改当前诊断会话的原子配置。ZIP 导出在独立线程执行，完成后通过窗口
//! 消息回到 UI；因此即使日志接近保留上限，主消息循环仍保持响应。

use std::ffi::OsString;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr::null;
use std::sync::{Arc, Mutex};
use std::thread;

use windows_sys::Win32::Foundation::{
    ERROR_GEN_FAILURE, ERROR_NOT_ENOUGH_MEMORY, HINSTANCE, HWND, LPARAM, WPARAM,
};
use windows_sys::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError, GetSaveFileNameW, OFN_EXPLORER, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST,
    OPENFILENAMEW,
};
use windows_sys::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, CheckDlgButton, IsDlgButtonChecked,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EndDialog, GetDlgItem, IDCANCEL, IDYES, MB_ICONERROR, MB_ICONEXCLAMATION, MB_OK, MB_YESNO,
    MessageBoxW, PostMessageW, SW_SHOWNORMAL, SetDlgItemTextW, WM_COMMAND, WM_INITDIALOG,
};

use crate::infrastructure::diagnostics::{self, Field, Level};
use crate::infrastructure::native::{
    format_resource_string, get_window_userdata, loword, record_win32_error, set_window_userdata,
    to_wide_null,
};
use crate::ui::dialogs::dialog_box;
use crate::ui::localization::{TextKey, localize_dialog, text};
use crate::ui::resource_ids::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DiagnosticDialogOutcome {
    Closed,
    RestartDetailed,
}

struct DiagnosticDialogContext {
    operation_id: u64,
    export_result: Arc<Mutex<Option<Result<PathBuf, String>>>>,
    export_in_progress: bool,
}

pub(super) fn show(
    hinstance: HINSTANCE,
    parent: HWND,
    operation_id: u64,
) -> Result<DiagnosticDialogOutcome, u32> {
    let mut context = DiagnosticDialogContext {
        operation_id,
        export_result: Arc::new(Mutex::new(None)),
        export_in_progress: false,
    };
    // DialogBox runs a nested message loop. Clear the ambient command correlation while it is
    // active so unrelated timers and worker completions are not attributed to this UI command.
    let result = diagnostics::without_operation_id(|| {
        dialog_box(
            hinstance,
            IDD_DIAGNOSTICS,
            parent,
            Some(diagnostics_dialog_proc),
            &mut context as *mut DiagnosticDialogContext as LPARAM,
        )
    })?;
    Ok(if result == IDC_DIAGNOSTIC_RESTART as isize {
        DiagnosticDialogOutcome::RestartDetailed
    } else {
        DiagnosticDialogOutcome::Closed
    })
}

unsafe extern "system" fn diagnostics_dialog_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    // Safety: all pointer access is limited to the modal `dialog_box` call that owns the context.
    unsafe {
        match message {
            WM_INITDIALOG => {
                set_window_userdata(hwnd, lparam);
                localize_dialog(hwnd, IDD_DIAGNOSTICS);
                refresh_dialog_state(hwnd);
                1
            }
            WM_COMMAND => {
                let control_id = i32::from(loword(wparam));
                let operation_id = context(hwnd).operation_id;
                match control_id {
                    IDCANCEL => {
                        if context(hwnd).export_in_progress {
                            return 1;
                        }
                        EndDialog(hwnd, IDCANCEL as isize);
                        1
                    }
                    IDC_DIAGNOSTIC_DETAILED => {
                        let enabled =
                            IsDlgButtonChecked(hwnd, IDC_DIAGNOSTIC_DETAILED) == BST_CHECKED;
                        diagnostics::with_operation_id(operation_id, || {
                            if !enabled {
                                diagnostics::set_sensitive(false);
                                diagnostics::set_minidump(false);
                            }
                            diagnostics::set_detailed(enabled);
                        });
                        refresh_dialog_state(hwnd);
                        1
                    }
                    IDC_DIAGNOSTIC_SENSITIVE => {
                        diagnostics::with_operation_id(operation_id, || {
                            diagnostics::set_sensitive(
                                IsDlgButtonChecked(hwnd, IDC_DIAGNOSTIC_SENSITIVE) == BST_CHECKED,
                            );
                        });
                        refresh_dialog_state(hwnd);
                        1
                    }
                    IDC_DIAGNOSTIC_MINIDUMP => {
                        diagnostics::with_operation_id(operation_id, || {
                            diagnostics::set_minidump(
                                IsDlgButtonChecked(hwnd, IDC_DIAGNOSTIC_MINIDUMP) == BST_CHECKED,
                            );
                        });
                        refresh_dialog_state(hwnd);
                        1
                    }
                    IDC_DIAGNOSTIC_RESTART => {
                        EndDialog(hwnd, IDC_DIAGNOSTIC_RESTART as isize);
                        1
                    }
                    IDC_DIAGNOSTIC_OPEN_FOLDER => {
                        open_log_folder(hwnd, operation_id);
                        1
                    }
                    IDC_DIAGNOSTIC_EXPORT => {
                        begin_export(hwnd, operation_id);
                        1
                    }
                    _ => 0,
                }
            }
            PWM_DIAGNOSTIC_EXPORT_COMPLETE => {
                complete_export(hwnd);
                1
            }
            _ => 0,
        }
    }
}

fn refresh_dialog_state(hwnd: HWND) {
    let status = diagnostics::status();
    let logging_status = if status.file_active {
        format_resource_string(
            text(TextKey::DiagnosticLoggingActive),
            &[status.level.as_str().to_string()],
        )
    } else {
        text(TextKey::DiagnosticLoggingUnavailable).to_string()
    };
    let dropped_status = format_resource_string(
        text(TextKey::DiagnosticDroppedEvents),
        &[status.dropped_events.to_string()],
    );
    let status_text = format!("{logging_status} · {dropped_status}");
    set_dialog_text(hwnd, IDC_DIAGNOSTIC_STATUS, &status_text);
    set_dialog_text(hwnd, IDC_DIAGNOSTIC_SESSION, &status.session_id);
    set_dialog_text(
        hwnd,
        IDC_DIAGNOSTIC_DIRECTORY,
        &status
            .directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| text(TextKey::NotAvailable).to_string()),
    );
    if let Some(error) = status.sink_error {
        set_dialog_text(hwnd, IDC_DIAGNOSTIC_EXPORT_STATUS, &error);
    }

    let detailed = status.level > Level::Info;
    check_dialog_button(hwnd, IDC_DIAGNOSTIC_DETAILED, detailed);
    check_dialog_button(hwnd, IDC_DIAGNOSTIC_SENSITIVE, status.sensitive);
    check_dialog_button(hwnd, IDC_DIAGNOSTIC_MINIDUMP, status.minidump);

    // Safety: each ID belongs to the live modal dialog and null controls are accepted by
    // EnableWindow as a no-op failure.
    unsafe {
        EnableWindow(
            GetDlgItem(hwnd, IDC_DIAGNOSTIC_OPEN_FOLDER),
            i32::from(status.directory.is_some()),
        );
        EnableWindow(
            GetDlgItem(hwnd, IDC_DIAGNOSTIC_EXPORT),
            i32::from(status.directory.is_some()),
        );
    }
}

fn check_dialog_button(hwnd: HWND, control_id: i32, checked: bool) {
    // Safety: the button is owned by the live modal dialog.
    unsafe {
        CheckDlgButton(
            hwnd,
            control_id,
            if checked { BST_CHECKED } else { BST_UNCHECKED },
        );
    }
}

fn open_log_folder(hwnd: HWND, operation_id: u64) {
    let Some(directory) = diagnostics::status().directory else {
        show_error(
            hwnd,
            TextKey::DiagnosticLogsTitle,
            text(TextKey::DiagnosticOpenFolderFailed),
        );
        return;
    };
    let verb = to_wide_null("open");
    let directory_wide = to_wide_null(&directory.to_string_lossy());
    // Safety: strings are NUL-terminated and the dialog HWND remains live for the call.
    let result = unsafe {
        ShellExecuteW(
            hwnd,
            verb.as_ptr(),
            directory_wide.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if result <= 32 {
        let error = result.max(1) as u32;
        diagnostics::with_operation_id(operation_id, || {
            record_win32_error("opening diagnostic log directory", error);
        });
        show_error(
            hwnd,
            TextKey::DiagnosticLogsTitle,
            text(TextKey::DiagnosticOpenFolderFailed),
        );
    } else {
        diagnostics::event_with(
            Level::Info,
            "diagnostics.folder_opened",
            "diagnostics-ui",
            "diagnostic log folder opened",
            Some(operation_id),
            None,
            &[Field::sensitive_text(
                "directory",
                directory.to_string_lossy(),
            )],
        );
    }
}

unsafe fn begin_export(hwnd: HWND, operation_id: u64) {
    // Safety: this function is called only while the modal dialog owns its stack context.
    let context = unsafe { context(hwnd) };
    if context.export_in_progress {
        return;
    }
    if diagnostics::export_requires_privacy_warning() && !confirm_sensitive_export(hwnd) {
        return;
    }
    let destination = match choose_bundle_destination(hwnd) {
        Ok(Some(path)) => path,
        Ok(None) => return,
        Err(error) => {
            diagnostics::with_operation_id(operation_id, || {
                record_win32_error("diagnostic save dialog", error);
            });
            show_error(
                hwnd,
                TextKey::DiagnosticExportFailedTitle,
                &format!("{} ({error:#010X})", text(TextKey::Win32ErrorPrefix)),
            );
            return;
        }
    };

    context.export_in_progress = true;
    set_export_controls_enabled(hwnd, false);
    set_dialog_text(
        hwnd,
        IDC_DIAGNOSTIC_EXPORT_STATUS,
        text(TextKey::DiagnosticExporting),
    );
    let result = Arc::clone(&context.export_result);
    let window = hwnd as isize;
    let spawn = thread::Builder::new()
        .name("taskmgr-rs-diagnostic-export".to_string())
        .spawn(move || {
            diagnostics::with_operation_id(operation_id, || {
                let export = diagnostics::export_bundle(&destination).map(|()| destination);
                if let Ok(mut slot) = result.lock() {
                    *slot = Some(export);
                }
                // Safety: HWND is used only as an opaque message target; failure means the dialog
                // closed while the independent export was completing.
                unsafe {
                    PostMessageW(window as HWND, PWM_DIAGNOSTIC_EXPORT_COMPLETE, 0, 0);
                }
            });
        });
    if spawn.is_err() {
        context.export_in_progress = false;
        set_export_controls_enabled(hwnd, true);
        diagnostics::with_operation_id(operation_id, || {
            record_win32_error("diagnostic export worker", ERROR_NOT_ENOUGH_MEMORY);
        });
        show_error(
            hwnd,
            TextKey::DiagnosticExportFailedTitle,
            text(TextKey::DiagnosticExportFailedTitle),
        );
    }
}

unsafe fn complete_export(hwnd: HWND) {
    // Safety: the completion message is handled only before the modal dialog returns.
    let context = unsafe { context(hwnd) };
    context.export_in_progress = false;
    set_export_controls_enabled(hwnd, true);
    let result = context
        .export_result
        .lock()
        .ok()
        .and_then(|mut result| result.take());
    match result {
        Some(Ok(path)) => {
            set_dialog_text(
                hwnd,
                IDC_DIAGNOSTIC_EXPORT_STATUS,
                &format!(
                    "{} {}",
                    text(TextKey::DiagnosticExportSucceeded),
                    path.to_string_lossy()
                ),
            );
        }
        Some(Err(error)) => {
            set_dialog_text(hwnd, IDC_DIAGNOSTIC_EXPORT_STATUS, &error);
            show_error(hwnd, TextKey::DiagnosticExportFailedTitle, &error);
        }
        None => {
            show_error(
                hwnd,
                TextKey::DiagnosticExportFailedTitle,
                text(TextKey::DiagnosticExportFailedTitle),
            );
        }
    }
}

fn set_export_controls_enabled(hwnd: HWND, enabled: bool) {
    // Safety: the controls are children of the live diagnostics dialog.
    unsafe {
        for id in [
            IDC_DIAGNOSTIC_EXPORT,
            IDC_DIAGNOSTIC_RESTART,
            IDC_DIAGNOSTIC_OPEN_FOLDER,
        ] {
            EnableWindow(GetDlgItem(hwnd, id), i32::from(enabled));
        }
    }
}

fn confirm_sensitive_export(hwnd: HWND) -> bool {
    let title = to_wide_null(text(TextKey::WarningTitle));
    let body = to_wide_null(text(TextKey::DiagnosticSensitiveExportWarning));
    // Safety: strings and owner HWND remain valid for this synchronous prompt.
    unsafe {
        MessageBoxW(
            hwnd,
            body.as_ptr(),
            title.as_ptr(),
            MB_YESNO | MB_ICONEXCLAMATION,
        ) == IDYES
    }
}

fn choose_bundle_destination(hwnd: HWND) -> Result<Option<PathBuf>, u32> {
    let mut file_buffer = vec![0u16; 32_768];
    let default_name = to_wide_null(&diagnostics::default_bundle_name());
    let copy_length = default_name.len().min(file_buffer.len());
    file_buffer[..copy_length].copy_from_slice(&default_name[..copy_length]);

    let filter_label = format!("{} (*.zip)", text(TextKey::DiagnosticSaveBundle));
    let mut filter = filter_label.encode_utf16().collect::<Vec<_>>();
    filter.push(0);
    filter.extend("*.zip".encode_utf16());
    filter.extend([0, 0]);
    let title = to_wide_null(text(TextKey::DiagnosticSaveBundle));
    let extension = to_wide_null("zip");
    let mut dialog = unsafe { zeroed::<OPENFILENAMEW>() };
    dialog.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    dialog.hwndOwner = hwnd;
    dialog.lpstrFilter = filter.as_ptr();
    dialog.nFilterIndex = 1;
    dialog.lpstrFile = file_buffer.as_mut_ptr();
    dialog.nMaxFile = file_buffer.len() as u32;
    dialog.lpstrTitle = title.as_ptr();
    dialog.Flags = OFN_EXPLORER | OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST;
    dialog.lpstrDefExt = extension.as_ptr();

    // Safety: OPENFILENAMEW references buffers that remain alive for the modal API call.
    if unsafe { GetSaveFileNameW(&mut dialog) } == 0 {
        // Safety: this query immediately follows the common-dialog failure/cancel result.
        let error = unsafe { CommDlgExtendedError() };
        return if error == 0 { Ok(None) } else { Err(error) };
    }
    let length = file_buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(file_buffer.len());
    if length == 0 {
        return Err(ERROR_GEN_FAILURE);
    }
    Ok(Some(PathBuf::from(OsString::from_wide(
        &file_buffer[..length],
    ))))
}

unsafe fn context<'a>(hwnd: HWND) -> &'a mut DiagnosticDialogContext {
    // Safety: the modal call stores a pointer to its stack-owned context at WM_INITDIALOG and
    // does not return until this HWND stops receiving dialog messages.
    unsafe { &mut *(get_window_userdata(hwnd) as *mut DiagnosticDialogContext) }
}

fn set_dialog_text(hwnd: HWND, control_id: i32, value: &str) {
    let wide = to_wide_null(value);
    // Safety: the control belongs to the dialog and `wide` lives for the synchronous call.
    unsafe {
        SetDlgItemTextW(hwnd, control_id, wide.as_ptr());
    }
}

fn show_error(hwnd: HWND, title_key: TextKey, body: &str) {
    let title = to_wide_null(text(title_key));
    let body = to_wide_null(body);
    // Safety: strings and owner HWND remain valid for this synchronous prompt.
    unsafe {
        MessageBoxW(hwnd, body.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
    }
}
