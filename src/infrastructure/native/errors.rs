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

use std::ptr::null;

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::Diagnostics::Debug::{
    FORMAT_MESSAGE_FROM_HMODULE, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS,
    FormatMessageW,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

use crate::infrastructure::diagnostics::{self, Field, Level};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ErrorDomain {
    Win32,
    Hresult,
    Pdh,
    NtStatus,
}

struct ErrorIdentity {
    event_name: &'static str,
    domain: &'static str,
    code: u64,
    display_code: String,
}

#[track_caller]
pub fn record_win32_error(component: &str, error: u32) {
    record_win32_error_with_fields(component, error, &[]);
}

#[track_caller]
pub(crate) fn record_win32_error_with_fields(component: &str, error: u32, fields: &[Field]) {
    record_error(
        component,
        error_identity(ErrorDomain::Win32, error),
        format_system_message(error),
        fields,
    );
}

#[track_caller]
pub fn record_hresult_error(component: &str, error: i32) {
    record_hresult_error_with_fields(component, error, &[]);
}

#[track_caller]
pub(crate) fn record_hresult_error_with_fields(component: &str, error: i32, fields: &[Field]) {
    let raw = error as u32;
    record_error(
        component,
        error_identity(ErrorDomain::Hresult, raw),
        format_system_message(raw),
        fields,
    );
}

#[track_caller]
pub fn record_pdh_error(component: &str, status: u32) {
    record_pdh_error_with_fields(component, status, &[]);
}

#[track_caller]
pub(crate) fn record_pdh_error_with_fields(component: &str, status: u32, fields: &[Field]) {
    record_error(
        component,
        error_identity(ErrorDomain::Pdh, status),
        format_module_message("pdh.dll", status).or_else(|| format_system_message(status)),
        fields,
    );
}

#[track_caller]
pub fn record_ntstatus_error(component: &str, status: i32) {
    record_ntstatus_error_with_fields(component, status, &[]);
}

#[track_caller]
pub(crate) fn record_ntstatus_error_with_fields(component: &str, status: i32, fields: &[Field]) {
    let raw = status as u32;
    record_error(
        component,
        error_identity(ErrorDomain::NtStatus, raw),
        format_module_message("ntdll.dll", raw).or_else(|| format_system_message(raw)),
        fields,
    );
}

#[track_caller]
pub fn record_startup_timing(stage: &str, elapsed_ms: u64) {
    diagnostics::event_with(
        Level::Info,
        "startup.stage_completed",
        "startup",
        &format!("{stage} completed"),
        None,
        Some(elapsed_ms),
        &[Field::text("stage", stage)],
    );
}

#[track_caller]
fn record_error(
    component: &str,
    identity: ErrorIdentity,
    decoded_message: Option<String>,
    extra_fields: &[Field],
) {
    let mut fields = vec![
        Field::text("error_domain", identity.domain),
        Field::unsigned("error_code", identity.code),
        Field::text("error_code_display", &identity.display_code),
    ];
    if let Some(decoded_message) = decoded_message {
        fields.push(Field::text("error_message", decoded_message));
    }
    fields.extend_from_slice(extra_fields);
    diagnostics::event(
        Level::Error,
        identity.event_name,
        component,
        &format!("{component} failed with {}", identity.display_code),
        &fields,
    );
}

fn error_identity(domain: ErrorDomain, raw: u32) -> ErrorIdentity {
    match domain {
        ErrorDomain::Win32 => ErrorIdentity {
            event_name: "native.win32_error",
            domain: "win32",
            code: u64::from(raw),
            display_code: format!("Win32 error {raw}"),
        },
        ErrorDomain::Hresult => ErrorIdentity {
            event_name: "native.hresult_error",
            domain: "hresult",
            code: u64::from(raw),
            display_code: format!("HRESULT 0x{raw:08X}"),
        },
        ErrorDomain::Pdh => ErrorIdentity {
            event_name: "native.pdh_error",
            domain: "pdh",
            code: u64::from(raw),
            display_code: format!("PDH status 0x{raw:08X}"),
        },
        ErrorDomain::NtStatus => ErrorIdentity {
            event_name: "native.ntstatus_error",
            domain: "ntstatus",
            code: u64::from(raw),
            display_code: format!("NTSTATUS 0x{raw:08X}"),
        },
    }
}

fn format_system_message(code: u32) -> Option<String> {
    format_message(code, None)
}

fn format_module_message(module_name: &str, code: u32) -> Option<String> {
    let module_name = module_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // Safety: the module name is NUL-terminated and this query does not change its reference count.
    let module = unsafe { GetModuleHandleW(module_name.as_ptr()) };
    if module.is_null() {
        return None;
    }
    format_message(code, Some(module))
}

fn format_message(code: u32, module: Option<HMODULE>) -> Option<String> {
    let (source, flags) = match module {
        Some(module) => (
            module.cast_const(),
            FORMAT_MESSAGE_FROM_HMODULE | FORMAT_MESSAGE_FROM_SYSTEM,
        ),
        None => (null(), FORMAT_MESSAGE_FROM_SYSTEM),
    };
    let mut buffer = [0u16; 1024];
    // Safety: a module source comes only from successful GetModuleHandleW and remains loaded for
    // this synchronous query; the buffer is writable and no insert arguments are requested.
    let count = unsafe {
        FormatMessageW(
            flags | FORMAT_MESSAGE_IGNORE_INSERTS,
            source,
            code,
            0,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            null(),
        )
    };
    if count == 0 {
        return None;
    }
    let message = String::from_utf16_lossy(&buffer[..count as usize])
        .trim()
        .trim_end_matches('.')
        .to_string();
    (!message.is_empty()).then_some(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_error_domains_keep_raw_codes_and_stable_names() {
        let cases = [
            (
                ErrorDomain::Win32,
                5,
                "native.win32_error",
                "win32",
                "Win32 error 5",
            ),
            (
                ErrorDomain::Hresult,
                0x8007_0005,
                "native.hresult_error",
                "hresult",
                "HRESULT 0x80070005",
            ),
            (
                ErrorDomain::Pdh,
                0xC000_0BB8,
                "native.pdh_error",
                "pdh",
                "PDH status 0xC0000BB8",
            ),
            (
                ErrorDomain::NtStatus,
                0xC000_0001,
                "native.ntstatus_error",
                "ntstatus",
                "NTSTATUS 0xC0000001",
            ),
        ];
        for (domain, raw, event_name, name, display_code) in cases {
            let identity = error_identity(domain, raw);
            assert_eq!(identity.event_name, event_name);
            assert_eq!(identity.domain, name);
            assert_eq!(identity.code, u64::from(raw));
            assert_eq!(identity.display_code, display_code);
        }
    }

    #[test]
    fn win32_system_messages_are_attached_when_windows_provides_one() {
        let message = format_system_message(5).expect("access denied should have a system message");
        assert!(!message.is_empty());
    }
}
