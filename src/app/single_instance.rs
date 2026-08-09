// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 单实例启动认证
//
//   文件:       src/app/single_instance.rs
//
//   日期:       2026年07月31日
//   环境:       Windows NT 10.0.29634 x86_64；Rust 1.97.0 (MSVC)
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 单实例启动的非信任互斥体和对等进程认证。
//!
//! 命名互斥体只缩小两个真实实例同时创建窗口的竞态，不证明持有者身份。只有窗口
//! 所属进程与当前进程的用户、会话、完整性级别、提升状态和内核报告的映像路径都
//! 相符，并且 HWND/PID 在发送消息前后仍绑定到同一个存活进程，启动方才会退出。

use std::ffi::OsString;
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::ptr::{null, null_mut};

use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_GEN_FAILURE, FALSE, GetLastError, HANDLE, HWND,
    LocalFree, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetLengthSid, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenIntegrityLevel, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetWindowThreadProcessId, SMTO_ABORTIFHUNG, SendMessageTimeoutW,
};

use crate::infrastructure::diagnostics::{self, Field, Level};
use crate::infrastructure::native::{OwnedHandle, to_wide_null};

const MAX_IMAGE_PATH_UNITS: usize = 32_768;

pub(super) struct CreatedMutex {
    pub(super) handle: HANDLE,
    pub(super) owned: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MutexWaitDecision {
    Owned,
    ProceedUnlocked,
}

pub(super) fn classify_mutex_wait(result: u32) -> MutexWaitDecision {
    if result == WAIT_OBJECT_0 || result == WAIT_ABANDONED {
        MutexWaitDecision::Owned
    } else {
        MutexWaitDecision::ProceedUnlocked
    }
}

pub(super) fn create_startup_mutex() -> Result<CreatedMutex, u32> {
    create_startup_mutex_with_suffix("")
}

fn create_startup_mutex_with_suffix(suffix: &str) -> Result<CreatedMutex, u32> {
    let identity = query_process_identity(unsafe { GetCurrentProcess() }, unsafe {
        GetCurrentProcessId()
    })?;
    let name = startup_mutex_name(&identity, suffix);
    let sid = identity.user_sid.to_string_sid()?;
    let mandatory_label = if identity.elevated {
        "S:(ML;;NW;;;HI)"
    } else {
        ""
    };
    let descriptor = OwnedSecurityDescriptor::from_sddl(&format!(
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid}){mandatory_label}"
    ))?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_ptr().cast(),
        bInheritHandle: FALSE,
    };

    // Safety: the security descriptor and NUL-terminated name remain live for this call.
    let handle = unsafe { CreateMutexW(&raw mut attributes, 1, name.as_ptr()) };
    if handle.is_null() {
        return Err(last_error());
    }
    // GetLastError must be sampled immediately after CreateMutexW to distinguish a new object.
    let owned = unsafe { GetLastError() } != ERROR_ALREADY_EXISTS;
    Ok(CreatedMutex { handle, owned })
}

fn startup_mutex_name(identity: &ProcessIdentity, suffix: &str) -> Vec<u16> {
    let mut hasher = Sha256::new();
    hasher.update(identity.user_sid.as_bytes());
    let digest = hasher.finalize();
    let user_hash = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    to_wide_null(&format!(
        "Local\\taskmgr-rs.startup.v1.session-{}.user-{user_hash}{suffix}",
        identity.session_id,
    ))
}

pub(super) fn activate_authenticated_instance(title: &str, message: u32, timeout_ms: u32) -> bool {
    if title.is_empty() {
        return false;
    }
    let current_pid = unsafe { GetCurrentProcessId() };
    let current = match query_process_identity(unsafe { GetCurrentProcess() }, current_pid) {
        Ok(identity) => identity,
        Err(error) => {
            diagnostics::event(
                Level::Warn,
                "single_instance.current_identity_failed",
                "startup",
                "unable to establish the current process identity",
                &[Field::unsigned("win32_error", u64::from(error))],
            );
            return false;
        }
    };
    let title = to_wide_null(title);
    let mut after = null_mut();
    loop {
        // Safety: title is NUL-terminated and `after` is either null or the prior enumeration item.
        let hwnd = unsafe { FindWindowExW(null_mut(), after, null(), title.as_ptr()) };
        if hwnd.is_null() {
            return false;
        }
        after = hwnd;
        let Some(candidate) = AuthenticatedWindow::open(hwnd, &current) else {
            continue;
        };
        if !candidate.is_still_bound() {
            continue;
        }
        let mut result = 0usize;
        // Safety: candidate authentication retains the process handle and the output is writable.
        let sent = unsafe {
            SendMessageTimeoutW(
                hwnd,
                message,
                0,
                0,
                SMTO_ABORTIFHUNG,
                timeout_ms,
                &mut result,
            )
        } != 0;
        if sent && result as u32 == message && candidate.is_still_bound() {
            return true;
        }
    }
}

struct AuthenticatedWindow {
    hwnd: HWND,
    process_id: u32,
    process: OwnedHandle,
}

impl AuthenticatedWindow {
    fn open(hwnd: HWND, current: &ProcessIdentity) -> Option<Self> {
        let mut process_id = 0u32;
        // Safety: process_id is writable and hwnd came from the live top-level enumeration.
        if unsafe { GetWindowThreadProcessId(hwnd, &mut process_id) } == 0 || process_id == 0 {
            return None;
        }
        // Safety: OpenProcess returns an owned handle for the requested PID on success.
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                FALSE,
                process_id,
            )
        };
        // Safety: successful OpenProcess returns one owned process handle released by CloseHandle;
        // no other owner is retained after this transfer.
        let process = unsafe { OwnedHandle::from_raw(process) }?;
        let peer = query_process_identity(process.as_raw(), process_id).ok()?;
        if !same_instance_identity(current, &peer) {
            diagnostics::event(
                Level::Trace,
                "single_instance.candidate_rejected",
                "startup",
                "title-matching window failed process identity authentication",
                &[Field::unsigned("candidate_pid", u64::from(process_id))],
            );
            return None;
        }
        Some(Self {
            hwnd,
            process_id,
            process,
        })
    }

    fn is_still_bound(&self) -> bool {
        let mut observed_process_id = 0u32;
        // Safety: the PID output is writable and the retained process handle is live.
        let owner_exists =
            unsafe { GetWindowThreadProcessId(self.hwnd, &mut observed_process_id) } != 0;
        let process_running =
            unsafe { WaitForSingleObject(self.process.as_raw(), 0) } == WAIT_TIMEOUT;
        window_binding_is_current(
            self.process_id,
            observed_process_id,
            owner_exists && process_running,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessIdentity {
    session_id: u32,
    elevated: bool,
    integrity_rid: u32,
    user_sid: OwnedSid,
    image_path: String,
}

fn same_instance_identity(current: &ProcessIdentity, peer: &ProcessIdentity) -> bool {
    current.session_id == peer.session_id
        && current.elevated == peer.elevated
        && current.integrity_rid == peer.integrity_rid
        && current.user_sid == peer.user_sid
        && current.image_path.eq_ignore_ascii_case(&peer.image_path)
}

fn window_binding_is_current(expected_pid: u32, observed_pid: u32, process_running: bool) -> bool {
    process_running && expected_pid != 0 && observed_pid == expected_pid
}

fn query_process_identity(process: HANDLE, process_id: u32) -> Result<ProcessIdentity, u32> {
    let mut session_id = 0u32;
    // Safety: session_id is writable and process_id identifies the queried process.
    if unsafe { ProcessIdToSessionId(process_id, &mut session_id) } == 0 {
        return Err(last_error());
    }
    let token = open_process_token(process)?;
    let elevation = token_information(token.as_raw(), TokenElevation)?;
    if elevation.len().saturating_mul(size_of::<u64>()) < size_of::<TOKEN_ELEVATION>() {
        return Err(ERROR_GEN_FAILURE);
    }
    // Safety: token_information returns a suitably aligned byte allocation and length was checked.
    let elevated =
        unsafe { (*(elevation.as_ptr().cast::<TOKEN_ELEVATION>())).TokenIsElevated != 0 };
    let user = token_information(token.as_raw(), TokenUser)?;
    if user.len().saturating_mul(size_of::<u64>()) < size_of::<TOKEN_USER>() {
        return Err(ERROR_GEN_FAILURE);
    }
    // Safety: the TOKEN_USER header and referenced SID stay inside `user` for its lifetime.
    let user_sid = unsafe { (*(user.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    // Safety: TOKEN_USER came from the live, successfully populated token-information buffer and
    // its SID remains readable while this function copies it.
    let user_sid = unsafe { OwnedSid::copy_from_raw(user_sid) }?;
    let integrity = token_information(token.as_raw(), TokenIntegrityLevel)?;
    if integrity.len().saturating_mul(size_of::<u64>()) < size_of::<TOKEN_MANDATORY_LABEL>() {
        return Err(ERROR_GEN_FAILURE);
    }
    // Safety: the mandatory-label header and SID are valid for the returned token buffer.
    let integrity_sid = unsafe {
        (*(integrity.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()))
            .Label
            .Sid
    };
    // Safety: TOKEN_MANDATORY_LABEL came from the live, successfully populated token-information
    // buffer and its SID remains readable while this function copies it.
    let integrity_sid = unsafe { OwnedSid::copy_from_raw(integrity_sid) }?;
    let integrity_rid = integrity_sid.last_subauthority()?;
    let image_path = query_image_path(process)?;
    Ok(ProcessIdentity {
        session_id,
        elevated,
        integrity_rid,
        user_sid,
        image_path,
    })
}

fn open_process_token(process: HANDLE) -> Result<OwnedHandle, u32> {
    let mut token = null_mut();
    // Safety: token is writable and receives one owned handle on success.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        Err(last_error())
    } else {
        // Safety: successful OpenProcessToken returns one owned token handle released by
        // CloseHandle; ownership moves directly into the guard.
        unsafe { OwnedHandle::from_raw(token) }.ok_or(ERROR_GEN_FAILURE)
    }
}

fn token_information(token: HANDLE, class: i32) -> Result<Vec<u64>, u32> {
    let mut required = 0u32;
    // Safety: the initial null-buffer query writes only the required size.
    unsafe {
        GetTokenInformation(token, class, null_mut(), 0, &mut required);
    }
    // Fixed-size token classes may report ERROR_BAD_LENGTH rather than
    // ERROR_INSUFFICIENT_BUFFER on some supported Windows builds. A nonzero required size is the
    // authoritative result of this sizing call.
    if required == 0 {
        return Err(last_error());
    }
    let word_count = (required as usize).div_ceil(size_of::<u64>());
    let mut buffer = vec![0u64; word_count];
    // Safety: the aligned allocation is at least `required` bytes and remains writable.
    if unsafe {
        GetTokenInformation(
            token,
            class,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        Err(last_error())
    } else {
        Ok(buffer)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OwnedSid {
    // TOKEN_* buffers are u64-aligned. Retaining that alignment avoids rebuilding a PSID from a
    // `Vec<u8>`, whose type-level alignment would not satisfy native SID field accesses.
    storage: Vec<u64>,
    byte_len: usize,
}

impl OwnedSid {
    /// Copies a native SID into self-contained, suitably aligned storage.
    ///
    /// # Safety
    ///
    /// `sid` must point to a live, structurally valid SID that remains readable for the complete
    /// byte length reported by `GetLengthSid` during this call.
    unsafe fn copy_from_raw(sid: PSID) -> Result<Self, u32> {
        if sid.is_null() {
            return Err(ERROR_GEN_FAILURE);
        }
        // Safety: validity and readability are required by the function-level contract.
        let byte_len = unsafe { GetLengthSid(sid) } as usize;
        if byte_len == 0 {
            return Err(last_error());
        }
        let mut storage = vec![0u64; byte_len.div_ceil(size_of::<u64>())];
        // Safety: destination storage is aligned and at least `byte_len` bytes; the source range
        // is readable and non-overlapping by the function-level contract.
        unsafe {
            std::ptr::copy_nonoverlapping(
                sid.cast::<u8>(),
                storage.as_mut_ptr().cast::<u8>(),
                byte_len,
            );
        }
        Ok(Self { storage, byte_len })
    }

    fn as_bytes(&self) -> &[u8] {
        // Safety: `storage` contains at least `byte_len` initialized bytes copied above.
        unsafe { std::slice::from_raw_parts(self.storage.as_ptr().cast::<u8>(), self.byte_len) }
    }

    fn as_psid(&self) -> PSID {
        self.storage.as_ptr().cast_mut().cast()
    }

    fn last_subauthority(&self) -> Result<u32, u32> {
        let sid = self.as_psid();
        // Safety: this owner contains a complete, aligned SID and keeps it live through both calls.
        let count = unsafe { GetSidSubAuthorityCount(sid) };
        if count.is_null() || unsafe { *count } == 0 {
            return Err(ERROR_GEN_FAILURE);
        }
        // Safety: the validated nonzero count indexes the final subauthority of the same SID.
        let value = unsafe { GetSidSubAuthority(sid, u32::from(*count) - 1) };
        if value.is_null() {
            Err(ERROR_GEN_FAILURE)
        } else {
            Ok(unsafe { *value })
        }
    }

    fn to_string_sid(&self) -> Result<String, u32> {
        let mut value = null_mut::<u16>();
        // Safety: this owner supplies a complete aligned SID, and `value` is a writable out slot.
        if unsafe { ConvertSidToStringSidW(self.as_psid(), &mut value) } == 0 {
            return Err(last_error());
        }
        let mut length = 0usize;
        // Safety: the conversion API returns a NUL-terminated LocalAlloc string.
        unsafe {
            while *value.add(length) != 0 {
                length += 1;
            }
        }
        // Safety: the discovered range precedes the terminating NUL.
        let output =
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(value, length) });
        // Safety: ConvertSidToStringSidW transfers a LocalAlloc allocation to the caller.
        unsafe {
            LocalFree(value.cast());
        }
        Ok(output)
    }

    #[cfg(test)]
    fn from_identity_bytes(bytes: &[u8]) -> Self {
        let mut storage = vec![0u64; bytes.len().div_ceil(size_of::<u64>())];
        // Safety: destination has at least `bytes.len()` bytes and cannot overlap the input.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                storage.as_mut_ptr().cast::<u8>(),
                bytes.len(),
            );
        }
        Self {
            storage,
            byte_len: bytes.len(),
        }
    }
}

fn query_image_path(process: HANDLE) -> Result<String, u32> {
    let mut buffer = vec![0u16; MAX_IMAGE_PATH_UNITS];
    let mut length = buffer.len() as u32;
    // Safety: the buffer and length are valid for the synchronous process query.
    if unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) } == 0 {
        return Err(last_error());
    }
    buffer.truncate(length as usize);
    Ok(OsString::from_wide(&buffer).to_string_lossy().into_owned())
}

struct OwnedSecurityDescriptor(PSECURITY_DESCRIPTOR);

impl OwnedSecurityDescriptor {
    fn from_sddl(sddl: &str) -> Result<Self, u32> {
        let wide = to_wide_null(sddl);
        let mut descriptor = null_mut();
        // Safety: the SDDL is NUL-terminated and the API returns one LocalAlloc descriptor.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            Err(last_error())
        } else {
            Ok(Self(descriptor))
        }
    }

    const fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.0
    }
}

impl Drop for OwnedSecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0.cast());
            }
        }
    }
}

fn last_error() -> u32 {
    let error = unsafe { GetLastError() };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::System::Threading::ReleaseMutex;

    fn identity() -> ProcessIdentity {
        ProcessIdentity {
            session_id: 3,
            elevated: true,
            integrity_rid: 0x3000,
            user_sid: OwnedSid::from_identity_bytes(&[1, 2, 3]),
            image_path: r"C:\Program Files\taskmgr-rs\taskmgr.exe".to_string(),
        }
    }

    #[test]
    fn mutex_timeout_and_failure_never_fail_closed() {
        assert_eq!(
            classify_mutex_wait(WAIT_TIMEOUT),
            MutexWaitDecision::ProceedUnlocked
        );
        assert_eq!(
            classify_mutex_wait(u32::MAX),
            MutexWaitDecision::ProceedUnlocked
        );
        assert_eq!(classify_mutex_wait(WAIT_OBJECT_0), MutexWaitDecision::Owned);
    }

    #[test]
    fn title_spoof_is_rejected_when_process_identity_differs() {
        let current = identity();
        let mut spoof = current.clone();
        spoof.elevated = false;
        assert!(!same_instance_identity(&current, &spoof));
        spoof.elevated = true;
        spoof.image_path = r"C:\Temp\spoof.exe".to_string();
        assert!(!same_instance_identity(&current, &spoof));
    }

    #[test]
    fn legitimate_peer_accepts_case_only_image_path_differences() {
        let current = identity();
        let mut peer = current.clone();
        peer.image_path = current.image_path.to_ascii_uppercase();
        assert!(same_instance_identity(&current, &peer));
    }

    #[test]
    fn stale_or_reused_window_binding_is_rejected() {
        assert!(window_binding_is_current(42, 42, true));
        assert!(!window_binding_is_current(42, 43, true));
        assert!(!window_binding_is_current(42, 42, false));
        assert!(!window_binding_is_current(0, 0, true));
    }

    #[test]
    fn current_process_identity_can_seed_mutex_security() {
        let process = unsafe { GetCurrentProcess() };
        let process_id = unsafe { GetCurrentProcessId() };
        let mut session_id = 0;
        assert_ne!(
            unsafe { ProcessIdToSessionId(process_id, &mut session_id) },
            0,
            "current session should be queryable"
        );
        let token = open_process_token(process).expect("current token should open");
        token_information(token.as_raw(), TokenElevation).expect("elevation should be queryable");
        let user = token_information(token.as_raw(), TokenUser).expect("user should be queryable");
        let user_sid = unsafe { (*(user.as_ptr().cast::<TOKEN_USER>())).User.Sid };
        // Safety: the SID is backed by the live TOKEN_USER buffer for this copy.
        let user_sid = unsafe { OwnedSid::copy_from_raw(user_sid) }
            .expect("user SID should copy");
        let integrity = token_information(token.as_raw(), TokenIntegrityLevel)
            .expect("integrity should be queryable");
        let integrity_sid = unsafe {
            (*(integrity.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()))
                .Label
                .Sid
        };
        // Safety: the SID is backed by the live TOKEN_MANDATORY_LABEL buffer for this copy.
        let integrity_sid = unsafe { OwnedSid::copy_from_raw(integrity_sid) }
            .expect("integrity SID should copy");
        integrity_sid
            .last_subauthority()
            .expect("integrity SID should be valid");
        query_image_path(process).expect("current image path should be queryable");
        let sid = user_sid
            .to_string_sid()
            .expect("copied user SID should remain valid");
        OwnedSecurityDescriptor::from_sddl(&format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid})"))
            .expect("mutex security descriptor should parse");
    }

    #[test]
    fn precreated_owned_mutex_times_out_but_does_not_fail_closed() {
        let suffix = format!(".test-{}", unsafe { GetCurrentProcessId() });
        let (ready_sender, ready_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let holder_suffix = suffix.clone();
        let holder = thread::spawn(move || {
            let mutex = create_startup_mutex_with_suffix(&holder_suffix)
                .expect("holder mutex should be created");
            assert!(mutex.owned);
            ready_sender.send(()).expect("holder should signal");
            release_receiver.recv().expect("holder should be released");
            unsafe {
                ReleaseMutex(mutex.handle);
                CloseHandle(mutex.handle);
            }
        });
        ready_receiver.recv().expect("holder should become ready");

        let contender =
            create_startup_mutex_with_suffix(&suffix).expect("contender should open the mutex");
        assert!(!contender.owned);
        let wait = unsafe { WaitForSingleObject(contender.handle, 20) };
        assert_eq!(wait, WAIT_TIMEOUT);
        assert_eq!(
            classify_mutex_wait(wait),
            MutexWaitDecision::ProceedUnlocked
        );
        unsafe { CloseHandle(contender.handle) };

        release_sender.send(()).expect("holder should be released");
        holder.join().expect("holder should stop");
    }

    #[test]
    fn access_denied_precreated_mutex_is_reported_for_unlocked_startup() {
        let identity = query_process_identity(unsafe { GetCurrentProcess() }, unsafe {
            GetCurrentProcessId()
        })
        .expect("current identity should be queryable");
        let suffix = format!(".deny-test-{}", unsafe { GetCurrentProcessId() });
        let name = startup_mutex_name(&identity, &suffix);
        let descriptor = OwnedSecurityDescriptor::from_sddl("D:P(D;;GA;;;WD)")
            .expect("deny descriptor should parse");
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.as_ptr().cast(),
            bInheritHandle: FALSE,
        };
        let hostile = unsafe { CreateMutexW(&raw mut attributes, FALSE, name.as_ptr()) };
        assert!(
            !hostile.is_null(),
            "hostile fixture mutex should be created"
        );

        let error = create_startup_mutex_with_suffix(&suffix)
            .err()
            .expect("opening the denied mutex should fail");
        assert_eq!(error, ERROR_ACCESS_DENIED);

        unsafe { CloseHandle(hostile) };
    }
}
