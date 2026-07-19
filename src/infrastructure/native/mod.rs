// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 原生基础设施入口
//
//   文件:       src/infrastructure/native/mod.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Exposes intentionally scoped Win32 helpers while implementation ownership stays categorized.

mod errors;
mod handles;
mod safety;
mod ui;

pub use errors::{
    record_hresult_error, record_ntstatus_error, record_pdh_error, record_startup_timing,
    record_win32_error,
};
pub use handles::{OwnedHandle, OwnedWtsMemory, destroy_icon_handle};
pub use safety::{enable_debug_privilege, is_32_bit_process_handle, process_is_elevated};
pub use ui::{
    append_32_bit_suffix, call_window_proc, copy_text_to_callback_buffer, finish_list_view_update,
    format_resource_string, get_window_userdata, height, hiword, loword,
    pause_redraw_for_visible_windows, redraw_window_tree, resume_redraw_for_windows,
    sanitize_task_manager_menu, set_dialog_msg_result, set_style, set_window_userdata,
    set_window_userdata_ptr, subclass_list_view, to_wide_null, widestr_ptr_to_string, width,
    window_rect_relative_to_page, window_userdata_non_null,
};
