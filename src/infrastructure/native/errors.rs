// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 原生错误域记录
//
//   文件:       src/infrastructure/native/errors.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Records Win32, HRESULT, PDH, and NTSTATUS failures without collapsing their error domains.

use std::iter;
use windows_sys::Win32::System::Diagnostics::Debug::OutputDebugStringW;

fn debug_message(message: String) {
    let wide: Vec<u16> = message.encode_utf16().chain(iter::once(0)).collect();
    // Safety: `wide` is null-terminated and remains alive for this synchronous call.
    unsafe { OutputDebugStringW(wide.as_ptr()) };
}

pub fn record_win32_error(component: &str, error: u32) {
    debug_message(format!(
        "taskmgr-rs: {component} failed with Win32 error {error}\r\n"
    ));
}

pub fn record_hresult_error(component: &str, error: i32) {
    debug_message(format!(
        "taskmgr-rs: {component} failed with HRESULT 0x{:08X}\r\n",
        error as u32
    ));
}

pub fn record_pdh_error(component: &str, status: u32) {
    debug_message(format!(
        "taskmgr-rs: {component} failed with PDH status 0x{status:08X}\r\n"
    ));
}

pub fn record_ntstatus_error(component: &str, status: i32) {
    debug_message(format!(
        "taskmgr-rs: {component} failed with NTSTATUS 0x{:08X}\r\n",
        status as u32
    ));
}

pub fn record_startup_timing(stage: &str, elapsed_ms: u64) {
    debug_message(format!(
        "taskmgr-rs startup: {stage} completed in {elapsed_ms} ms\r\n"
    ));
}
