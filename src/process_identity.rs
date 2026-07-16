// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 进程身份与句柄验证
//
//   文件:       src/process_identity.rs
//
//   日期:       2026年07月16日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Stable process identities and creation-time-verified process handles.
//!
//! Windows can reuse process identifiers. Any cached sample or action that outlives a single
//! enumeration therefore carries both the PID and process creation time. Opening a process through
//! this module verifies that pair before exposing the owned handle to the caller.

use std::mem::zeroed;

use windows_sys::Win32::Foundation::{
    ERROR_GEN_FAILURE, ERROR_INVALID_PARAMETER, FILETIME, GetLastError,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::winutil::{OwnedHandle, is_32_bit_process_handle};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct ProcIdentity {
    pub(crate) pid: u32,
    pub(crate) creation_time_100ns: u64,
}

impl ProcIdentity {
    pub(crate) const fn new(pid: u32, creation_time_100ns: u64) -> Self {
        Self {
            pid,
            creation_time_100ns,
        }
    }

    pub(crate) const fn pid_only(pid: u32) -> Self {
        Self::new(pid, 0)
    }

    pub(crate) const fn is_verified(self) -> bool {
        self.creation_time_100ns != 0
    }
}

pub(crate) fn query_process_identity_for_pid(pid: u32) -> Result<ProcIdentity, u32> {
    let handle = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let creation_time_100ns = query_process_creation_time(handle.as_raw())?;
    Ok(ProcIdentity::new(pid, creation_time_100ns))
}

pub(crate) fn query_process_is_32_bit(identity: ProcIdentity) -> Result<bool, u32> {
    let handle = open_process_for_identity(identity, PROCESS_QUERY_LIMITED_INFORMATION)?;
    is_32_bit_process_handle(handle.as_raw())
}

pub(crate) fn open_process_for_identity(
    identity: ProcIdentity,
    access: u32,
) -> Result<OwnedHandle, u32> {
    if !identity.is_verified() {
        return Err(ERROR_INVALID_PARAMETER);
    }

    let handle = open_process(identity.pid, access)?;
    let creation_time_100ns = query_process_creation_time(handle.as_raw())?;
    if creation_time_100ns != identity.creation_time_100ns {
        return Err(ERROR_INVALID_PARAMETER);
    }

    Ok(handle)
}

fn open_process(pid: u32, access: u32) -> Result<OwnedHandle, u32> {
    // SAFETY: OpenProcess takes scalar values only. A successful raw handle is transferred into
    // OwnedHandle immediately so every return path closes it exactly once.
    let raw_handle = unsafe { OpenProcess(access, 0, pid) };
    OwnedHandle::new(raw_handle).ok_or_else(last_error_or_gen_failure)
}

fn query_process_creation_time(handle: windows_sys::Win32::Foundation::HANDLE) -> Result<u64, u32> {
    // SAFETY: `handle` is owned by the caller for the duration of this synchronous query and each
    // FILETIME output points to valid initialized storage.
    unsafe {
        let mut creation = zeroed::<FILETIME>();
        let mut exit = zeroed::<FILETIME>();
        let mut kernel = zeroed::<FILETIME>();
        let mut user = zeroed::<FILETIME>();
        if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) == 0 {
            return Err(last_error_or_gen_failure());
        }

        Ok(filetime_to_u64(creation))
    }
}

fn last_error_or_gen_failure() -> u32 {
    // SAFETY: GetLastError has no preconditions.
    let error = unsafe { GetLastError() };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

const fn filetime_to_u64(filetime: FILETIME) -> u64 {
    ((filetime.dwHighDateTime as u64) << 32) | filetime.dwLowDateTime as u64
}

#[cfg(test)]
mod tests {
    use super::ProcIdentity;

    #[test]
    fn pid_only_identity_is_never_verified() {
        assert!(!ProcIdentity::pid_only(42).is_verified());
        assert!(ProcIdentity::new(42, 1).is_verified());
    }

    #[test]
    fn creation_time_distinguishes_reused_pid() {
        assert_ne!(ProcIdentity::new(42, 100), ProcIdentity::new(42, 200));
    }
}
