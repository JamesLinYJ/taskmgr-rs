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
use std::marker::PhantomData;
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

#[derive(Clone, Copy)]
struct NulTerminatedWide<'a> {
    units: &'a [u16],
}

impl<'a> NulTerminatedWide<'a> {
    fn new(units: &'a [u16]) -> Option<Self> {
        let (&terminator, body) = units.split_last()?;
        (terminator == 0 && !body.contains(&0)).then_some(Self { units })
    }

    fn as_ptr(self) -> *const u16 {
        self.units.as_ptr()
    }

    #[cfg(test)]
    fn without_terminator(self) -> &'a [u16] {
        &self.units[..self.units.len() - 1]
    }
}

/// Borrowed exception state supplied for one invocation of the top-level Windows filter.
struct ExceptionContext<'a> {
    raw: *const EXCEPTION_POINTERS,
    _callback_lifetime: PhantomData<&'a EXCEPTION_POINTERS>,
}

impl<'a> ExceptionContext<'a> {
    /// Establishes the raw exception-pointer contract at the callback boundary.
    ///
    /// # Safety
    ///
    /// `raw` must be null or the live `EXCEPTION_POINTERS` value supplied by Windows to the
    /// current unhandled-exception callback. Its nested exception/context records must remain
    /// valid for all reads performed by this module and by `MiniDumpWriteDump`, and the returned
    /// value must not outlive that callback.
    unsafe fn from_callback(raw: *const EXCEPTION_POINTERS) -> Self {
        Self {
            raw,
            _callback_lifetime: PhantomData,
        }
    }

    fn as_raw(&self) -> *const EXCEPTION_POINTERS {
        self.raw
    }
}

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
    // Safety: Windows owns the raw exception graph for the complete synchronous filter callback.
    // This is the sole conversion from that raw graph into the module's typed borrowed boundary.
    let exception = unsafe { ExceptionContext::from_callback(exception_info) };
    let pid = unsafe { GetCurrentProcessId() };
    let tid = unsafe { GetCurrentThreadId() };
    let (exception_code, exception_address) = exception_details(&exception);
    let module_base = module_base_for_address(exception_address);
    let module_offset = exception_address.saturating_sub(module_base);

    let mut path = [0u16; MAX_CRASH_PATH_UNITS];
    if let Some(path) = build_crash_path(&mut path, pid, tid, ".crash.json") {
        write_crash_record(
            path,
            pid,
            tid,
            exception_code,
            exception_address,
            module_base,
            module_offset,
        );
    }

    if MINIDUMP_ENABLED.load(Ordering::Acquire)
        && let Some(path) = build_crash_path(&mut path, pid, tid, ".dmp")
    {
        write_minidump(path, pid, tid, &exception);
    }

    EXCEPTION_CONTINUE_SEARCH
}

fn exception_details(exception: &ExceptionContext<'_>) -> (u32, usize) {
    if exception.as_raw().is_null() {
        return (0, 0);
    }
    // Safety: `ExceptionContext` establishes that the outer record and its nested pointers remain
    // live for this callback.
    let record = unsafe { (*exception.as_raw()).ExceptionRecord };
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

fn build_crash_path<'a>(
    output: &'a mut [u16; MAX_CRASH_PATH_UNITS],
    pid: u32,
    tid: u32,
    extension: &str,
) -> Option<NulTerminatedWide<'a>> {
    let directory = CRASH_DIRECTORY.get()?;
    let mut offset = 0usize;
    if !push_wide(output, &mut offset, directory) {
        return None;
    }
    for byte in b"crash-" {
        if !push_wide_unit(output, &mut offset, u16::from(*byte)) {
            return None;
        }
    }
    if !push_decimal(output, &mut offset, pid)
        || !push_wide_unit(output, &mut offset, b'-' as u16)
        || !push_decimal(output, &mut offset, tid)
    {
        return None;
    }
    for byte in extension.as_bytes() {
        if !push_wide_unit(output, &mut offset, u16::from(*byte)) {
            return None;
        }
    }
    if !push_wide_unit(output, &mut offset, 0) {
        return None;
    }
    NulTerminatedWide::new(&output[..offset])
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
    path: NulTerminatedWide<'_>,
    pid: u32,
    tid: u32,
    exception_code: u32,
    exception_address: usize,
    module_base: usize,
    module_offset: usize,
) {
    // Safety: `path` is a typed NUL-terminated borrow that remains live through CreateFileW.
    let file = unsafe {
        CreateFileW(
            path.as_ptr(),
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

fn write_minidump(
    path: NulTerminatedWide<'_>,
    pid: u32,
    tid: u32,
    exception: &ExceptionContext<'_>,
) {
    // Safety: `path` is a typed NUL-terminated borrow that remains live through CreateFileW.
    let file = unsafe {
        CreateFileW(
            path.as_ptr(),
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
        ExceptionPointers: exception.as_raw().cast_mut(),
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
        let path = build_crash_path(&mut path, 12, 34, ".dmp")
            .expect("the configured crash directory should fit in the path buffer");
        assert_eq!(
            String::from_utf16_lossy(path.without_terminator()),
            "C:\\crash-12-34.dmp"
        );
    }

    #[test]
    fn nul_terminated_wide_rejects_missing_or_interior_terminators() {
        assert!(NulTerminatedWide::new(&[b'C' as u16, 0]).is_some());
        assert!(NulTerminatedWide::new(&[b'C' as u16]).is_none());
        assert!(NulTerminatedWide::new(&[b'C' as u16, 0, b'x' as u16, 0]).is_none());
    }

    #[test]
    fn null_exception_context_has_no_details() {
        // Safety: null is explicitly accepted by the callback-boundary contract.
        let exception = unsafe { ExceptionContext::from_callback(null()) };
        assert_eq!(exception_details(&exception), (0, 0));
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
