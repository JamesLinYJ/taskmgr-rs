// +-------------------------------------------------------------------------
//
//   taskmgr-rs - Windows 安全与进程能力
//
//   文件:       src/infrastructure/native/safety.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Contains privilege, elevation, and process-machine checks with explicit Win32 failures.

use std::mem::zeroed;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    ERROR_GEN_FAILURE, ERROR_INVALID_DATA, ERROR_INVALID_HANDLE, ERROR_NOT_ALL_ASSIGNED,
    GetLastError, HANDLE, SetLastError,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
    SE_DEBUG_NAME, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION,
    TOKEN_PRIVILEGES, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::SystemInformation::{
    IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM, IMAGE_FILE_MACHINE_ARM64,
    IMAGE_FILE_MACHINE_ARMNT, IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_IA64,
    IMAGE_FILE_MACHINE_THUMB, IMAGE_FILE_MACHINE_UNKNOWN,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2, OpenProcessToken};

use super::handles::OwnedHandle;

pub fn enable_debug_privilege() -> Result<(), u32> {
    // Task Manager needs SeDebugPrivilege to query process tokens owned by services and SYSTEM.
    // AdjustTokenPrivileges may return success while reporting ERROR_NOT_ALL_ASSIGNED, so both
    // return channels must be checked.
    unsafe {
        let mut raw_token = null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut raw_token,
        ) == 0
        {
            return Err(GetLastError());
        }
        let Some(token) = OwnedHandle::new(raw_token) else {
            return Err(ERROR_NOT_ALL_ASSIGNED);
        };

        let mut luid = zeroed();
        if LookupPrivilegeValueW(null(), SE_DEBUG_NAME, &mut luid) == 0 {
            return Err(GetLastError());
        }

        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        SetLastError(0);
        if AdjustTokenPrivileges(token.as_raw(), 0, &privileges, 0, null_mut(), null_mut()) == 0 {
            return Err(GetLastError());
        }

        let error = GetLastError();
        if error == 0 { Ok(()) } else { Err(error) }
    }
}

pub fn process_is_elevated() -> Result<bool, u32> {
    unsafe {
        let mut raw_token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) == 0 {
            let error = GetLastError();
            return Err(if error == 0 {
                ERROR_NOT_ALL_ASSIGNED
            } else {
                error
            });
        }
        let Some(token) = OwnedHandle::new(raw_token) else {
            return Err(ERROR_NOT_ALL_ASSIGNED);
        };

        let mut elevation = zeroed::<TOKEN_ELEVATION>();
        let mut returned = 0u32;
        if GetTokenInformation(
            token.as_raw(),
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        ) == 0
        {
            let error = GetLastError();
            return Err(if error == 0 {
                ERROR_NOT_ALL_ASSIGNED
            } else {
                error
            });
        }

        Ok(elevation.TokenIsElevated != 0)
    }
}
pub fn is_32_bit_process_handle(handle: HANDLE) -> Result<bool, u32> {
    if handle.is_null() {
        return Err(ERROR_INVALID_HANDLE);
    }

    let mut process_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut native_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    // 安全性: `handle` is checked non-null and both machine values are valid out parameters.
    if unsafe { IsWow64Process2(handle, &mut process_machine, &mut native_machine) } == 0 {
        let error = unsafe { GetLastError() };
        Err(if error == 0 { ERROR_GEN_FAILURE } else { error })
    } else {
        process_machine_is_32_bit(process_machine, native_machine).ok_or(ERROR_INVALID_DATA)
    }
}

fn process_machine_is_32_bit(process_machine: u16, native_machine: u16) -> Option<bool> {
    let effective_machine = if process_machine == IMAGE_FILE_MACHINE_UNKNOWN {
        native_machine
    } else {
        process_machine
    };
    match effective_machine {
        IMAGE_FILE_MACHINE_I386
        | IMAGE_FILE_MACHINE_ARM
        | IMAGE_FILE_MACHINE_ARMNT
        | IMAGE_FILE_MACHINE_THUMB => Some(true),
        IMAGE_FILE_MACHINE_AMD64 | IMAGE_FILE_MACHINE_ARM64 | IMAGE_FILE_MACHINE_IA64 => {
            Some(false)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::process_machine_is_32_bit;
    use windows_sys::Win32::System::SystemInformation::{
        IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_ARMNT,
        IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_UNKNOWN,
    };

    #[test]
    fn process_machine_width_distinguishes_emulation_from_bitness() {
        assert_eq!(
            process_machine_is_32_bit(IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_AMD64),
            Some(true)
        );
        assert_eq!(
            process_machine_is_32_bit(IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64),
            Some(false)
        );
        assert_eq!(
            process_machine_is_32_bit(IMAGE_FILE_MACHINE_UNKNOWN, IMAGE_FILE_MACHINE_ARM64),
            Some(false)
        );
        assert_eq!(
            process_machine_is_32_bit(IMAGE_FILE_MACHINE_ARMNT, IMAGE_FILE_MACHINE_ARM64),
            Some(true)
        );
        assert_eq!(
            process_machine_is_32_bit(0xffff, IMAGE_FILE_MACHINE_ARM64),
            None
        );
    }
}
