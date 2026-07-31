// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 原生崩溃诊断
//
//   文件:       src/infrastructure/diagnostics/crash.rs
//
//   日期:       2026年07月27日
//   环境:       Fedora Linux 45 x86_64；Linux 内核 7.2.0-0.rc4.260725g0ce37745d4bf.39.fc45.x86_64；Rust 1.97.1；MinGW GCC 16.1.1；Wine 11.14 (Staging)
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 未处理异常的最后一道诊断边界。
//!
//! 异常过滤器不能依赖日志线程、堆分配或互斥锁。会话目录在正常启动时预编码为
//! UTF-16；过滤器只使用栈缓冲区和 Win32 文件 API 写最小异常记录。minidump 仍由
//! DbgHelp 生成，但只有用户显式开启时才调用。

use std::env;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Foundation::{CloseHandle, HMODULE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
    FlushFileBuffers, WriteFile,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS, MINIDUMP_EXCEPTION_INFORMATION, MiniDumpNormal,
    MiniDumpWithThreadInfo, MiniDumpWithUnloadedModules, MiniDumpWriteDump,
    SetUnhandledExceptionFilter,
};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleHandleExW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId,
};

const MAX_CRASH_PATH_UNITS: usize = 2_048;
const MAX_CRASH_RECORD_BYTES: usize = 768;
const TEST_CRASH_ENVIRONMENT: &str = "TASKMGR_RS_DIAGNOSTIC_TEST_CRASH";

static CRASH_DIRECTORY: OnceLock<Box<[u16]>> = OnceLock::new();
static MINIDUMP_ENABLED: AtomicBool = AtomicBool::new(false);

pub(super) fn install(directory: &Path, minidump_enabled: bool) -> Result<(), String> {
    let mut wide = OsStr::new(directory.as_os_str())
        .encode_wide()
        .collect::<Vec<_>>();
    if wide.len() + 64 >= MAX_CRASH_PATH_UNITS {
        return Err("diagnostic crash directory path is too long".to_string());
    }
    if !wide.ends_with(&[b'\\' as u16]) && !wide.ends_with(&[b'/' as u16]) {
        wide.push(b'\\' as u16);
    }
    CRASH_DIRECTORY
        .set(wide.into_boxed_slice())
        .map_err(|_| "native crash diagnostics were already installed".to_string())?;
    MINIDUMP_ENABLED.store(minidump_enabled, Ordering::Release);

    // Safety: the callback has the required system ABI and all data it references is process
    // static. It returns CONTINUE_SEARCH so normal Windows exception processing is preserved.
    unsafe {
        SetUnhandledExceptionFilter(Some(unhandled_exception_filter));
    }
    Ok(())
}

pub(super) fn set_minidump_enabled(enabled: bool) {
    MINIDUMP_ENABLED.store(enabled, Ordering::Release);
}

pub(super) fn test_crash_requested(detailed: bool, minidump_enabled: bool) -> bool {
    detailed
        && minidump_enabled
        && env::var_os(TEST_CRASH_ENVIRONMENT)
            .is_some_and(|value| test_crash_value_requested(Some(value.as_os_str())))
}

fn test_crash_value_requested(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| value == OsStr::new("access-violation"))
}

#[cold]
#[inline(never)]
pub(super) fn trigger_test_access_violation() -> ! {
    let address = std::hint::black_box(0usize);
    // Safety: this deliberately emits one faulting instruction for the opt-in diagnostic crash
    // test. Inline assembly keeps the exception address inside this function, so the matching
    // developer symbol file can prove source-level resolution.
    unsafe {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        core::arch::asm!(
            "mov byte ptr [{address}], 0x54",
            address = in(reg) address,
            options(nostack, preserves_flags)
        );
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!(
            "strb wzr, [{address}]",
            address = in(reg) address,
            options(nostack, preserves_flags)
        );
    }
    std::process::abort()
}

unsafe extern "system" fn unhandled_exception_filter(
    exception_info: *const EXCEPTION_POINTERS,
) -> i32 {
    let pid = unsafe { GetCurrentProcessId() };
    let tid = unsafe { GetCurrentThreadId() };
    let (exception_code, exception_address) = exception_details(exception_info);
    let module_base = module_base_for_address(exception_address);
    let module_offset = exception_address.saturating_sub(module_base);

    let mut path = [0u16; MAX_CRASH_PATH_UNITS];
    if build_crash_path(&mut path, pid, tid, ".crash.json") {
        write_crash_record(
            path.as_ptr(),
            pid,
            tid,
            exception_code,
            exception_address,
            module_base,
            module_offset,
        );
    }

    if MINIDUMP_ENABLED.load(Ordering::Acquire) && build_crash_path(&mut path, pid, tid, ".dmp") {
        write_minidump(path.as_ptr(), pid, tid, exception_info);
    }

    EXCEPTION_CONTINUE_SEARCH
}

fn exception_details(exception_info: *const EXCEPTION_POINTERS) -> (u32, usize) {
    if exception_info.is_null() {
        return (0, 0);
    }
    // Safety: Windows supplies EXCEPTION_POINTERS and its record for the duration of the filter.
    let record = unsafe { (*exception_info).ExceptionRecord };
    if record.is_null() {
        return (0, 0);
    }
    // Safety: `record` was checked and belongs to the active exception dispatch.
    unsafe {
        (
            (*record).ExceptionCode as u32,
            (*record).ExceptionAddress as usize,
        )
    }
}

fn module_base_for_address(address: usize) -> usize {
    if address == 0 {
        return 0;
    }
    let mut module = null_mut::<core::ffi::c_void>() as HMODULE;
    // Safety: FROM_ADDRESS instructs the API to interpret the second argument as an address.
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            address as *const u16,
            &mut module,
        )
    };
    if found == 0 { 0 } else { module as usize }
}

fn build_crash_path(
    output: &mut [u16; MAX_CRASH_PATH_UNITS],
    pid: u32,
    tid: u32,
    extension: &str,
) -> bool {
    let Some(directory) = CRASH_DIRECTORY.get() else {
        return false;
    };
    let mut offset = 0usize;
    if !push_wide(output, &mut offset, directory) {
        return false;
    }
    for byte in b"crash-" {
        if !push_wide_unit(output, &mut offset, u16::from(*byte)) {
            return false;
        }
    }
    if !push_decimal(output, &mut offset, pid)
        || !push_wide_unit(output, &mut offset, b'-' as u16)
        || !push_decimal(output, &mut offset, tid)
    {
        return false;
    }
    for byte in extension.as_bytes() {
        if !push_wide_unit(output, &mut offset, u16::from(*byte)) {
            return false;
        }
    }
    push_wide_unit(output, &mut offset, 0)
}

fn push_wide(output: &mut [u16], offset: &mut usize, value: &[u16]) -> bool {
    if output.len().saturating_sub(*offset) < value.len() {
        return false;
    }
    output[*offset..*offset + value.len()].copy_from_slice(value);
    *offset += value.len();
    true
}

fn push_wide_unit(output: &mut [u16], offset: &mut usize, value: u16) -> bool {
    if *offset >= output.len() {
        return false;
    }
    output[*offset] = value;
    *offset += 1;
    true
}

fn push_decimal(output: &mut [u16], offset: &mut usize, value: u32) -> bool {
    let mut digits = [0u16; 10];
    let mut count = 0usize;
    let mut remaining = value;
    loop {
        digits[count] = u16::from(b'0') + (remaining % 10) as u16;
        count += 1;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    for digit in digits[..count].iter().rev() {
        if !push_wide_unit(output, offset, *digit) {
            return false;
        }
    }
    true
}

fn write_crash_record(
    path: *const u16,
    pid: u32,
    tid: u32,
    exception_code: u32,
    exception_address: usize,
    module_base: usize,
    module_offset: usize,
) {
    // Safety: `path` points at the NUL-terminated stack buffer built above.
    let file = unsafe {
        CreateFileW(
            path,
            GENERIC_WRITE,
            FILE_SHARE_READ,
            null(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return;
    }

    let mut record = FixedAscii::<MAX_CRASH_RECORD_BYTES>::new();
    record.push_str("{\"schema_version\":1,\"event\":\"process.unhandled_exception\"");
    record.push_str(",\"exception_code\":\"0x");
    record.push_hex_u64(u64::from(exception_code), 8);
    record.push_str("\",\"exception_address\":\"0x");
    record.push_hex_usize(exception_address);
    record.push_str("\",\"module_base\":\"0x");
    record.push_hex_usize(module_base);
    record.push_str("\",\"module_offset\":\"0x");
    record.push_hex_usize(module_offset);
    record.push_str("\",\"pid\":");
    record.push_decimal_u64(u64::from(pid));
    record.push_str(",\"tid\":");
    record.push_decimal_u64(u64::from(tid));
    record.push_str("}\r\n");

    let mut written = 0u32;
    // Safety: the handle is owned here and the fixed buffer is valid for this synchronous write.
    unsafe {
        WriteFile(
            file,
            record.as_bytes().as_ptr(),
            record.len() as u32,
            &mut written,
            null_mut(),
        );
        FlushFileBuffers(file);
        CloseHandle(file);
    }
}

fn write_minidump(path: *const u16, pid: u32, tid: u32, exception_info: *const EXCEPTION_POINTERS) {
    // Safety: `path` points at the NUL-terminated stack buffer built above.
    let file = unsafe {
        CreateFileW(
            path,
            GENERIC_WRITE,
            FILE_SHARE_READ,
            null(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return;
    }

    let info = MINIDUMP_EXCEPTION_INFORMATION {
        ThreadId: tid,
        ExceptionPointers: exception_info.cast_mut(),
        ClientPointers: 0,
    };
    let dump_type = MiniDumpNormal | MiniDumpWithThreadInfo | MiniDumpWithUnloadedModules;
    // Safety: the exception pointers remain valid during the top-level filter and the file handle
    // stays owned until DbgHelp returns.
    unsafe {
        MiniDumpWriteDump(
            GetCurrentProcess(),
            pid,
            file,
            dump_type,
            &info,
            null(),
            null(),
        );
        FlushFileBuffers(file);
        CloseHandle(file);
    }
}

struct FixedAscii<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> FixedAscii<N> {
    const fn new() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
        }
    }

    fn push_str(&mut self, value: &str) {
        let available = N.saturating_sub(self.len);
        let count = value.len().min(available);
        self.bytes[self.len..self.len + count].copy_from_slice(&value.as_bytes()[..count]);
        self.len += count;
    }

    fn push_hex_usize(&mut self, value: usize) {
        self.push_hex_u64(value as u64, usize::BITS as usize / 4);
    }

    fn push_hex_u64(&mut self, value: u64, digits: usize) {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        for shift in (0..digits).rev() {
            if self.len == N {
                break;
            }
            self.bytes[self.len] = HEX[((value >> (shift * 4)) & 0xF) as usize];
            self.len += 1;
        }
    }

    fn push_decimal_u64(&mut self, value: u64) {
        let mut digits = [0u8; 20];
        let mut count = 0usize;
        let mut remaining = value;
        loop {
            digits[count] = b'0' + (remaining % 10) as u8;
            count += 1;
            remaining /= 10;
            if remaining == 0 {
                break;
            }
        }
        for digit in digits[..count].iter().rev() {
            if self.len == N {
                break;
            }
            self.bytes[self.len] = *digit;
            self.len += 1;
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    const fn len(&self) -> usize {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_ascii_formats_addresses_and_decimal_values() {
        let mut output = FixedAscii::<64>::new();
        output.push_hex_u64(0x12AB, 8);
        output.push_str(":");
        output.push_decimal_u64(42);
        assert_eq!(output.as_bytes(), b"000012AB:42");
    }

    #[test]
    fn crash_file_name_is_process_and_thread_specific() {
        let _ = CRASH_DIRECTORY.set(vec![b'C' as u16, b':' as u16, b'\\' as u16].into());
        let mut path = [0u16; MAX_CRASH_PATH_UNITS];
        assert!(build_crash_path(&mut path, 12, 34, ".dmp"));
        let end = path.iter().position(|unit| *unit == 0).unwrap();
        assert_eq!(
            String::from_utf16_lossy(&path[..end]),
            "C:\\crash-12-34.dmp"
        );
    }

    #[test]
    fn controlled_crash_requires_the_exact_opt_in_value() {
        assert!(test_crash_value_requested(Some(OsStr::new(
            "access-violation"
        ))));
        assert!(!test_crash_value_requested(Some(OsStr::new("yes"))));
        assert!(!test_crash_value_requested(None));
    }
}
