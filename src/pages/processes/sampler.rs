// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 进程快照采样
//
//   文件:       src/pages/processes/sampler.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Owns process enumeration, account/session enrichment, and successful-snapshot delta baselines.
//! Failed collections leave CPU and per-process baselines untouched.

use std::collections::{HashMap, HashSet};
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;
use std::slice;

use windows_sys::Win32::Foundation::{
    ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_DATA, ERROR_NO_MORE_FILES,
    ERROR_NONE_MAPPED, FILETIME, GetLastError, HANDLE, LocalFree, RtlNtStatusToDosError,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    GetLengthSid, GetTokenInformation, IsValidSid, IsWellKnownSid, LookupAccountSidW, SID_NAME_USE,
    TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalServiceSid, WinLocalSystemSid,
    WinNetworkServiceSid,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_PROCESS_INFOW, WTSEnumerateProcessesW,
};
use windows_sys::Win32::System::Threading::{
    GetPriorityClass, GetProcessHandleCount, GetProcessTimes, NORMAL_PRIORITY_CLASS, OpenProcess,
    OpenProcessToken, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_VM_READ,
};

use super::model::{DirtyColumns, ProcEntry, ProcStaticMetadata};
use crate::infrastructure::native::{
    OwnedHandle, OwnedWtsMemory, is_32_bit_process_handle, widestr_ptr_to_string,
};
use crate::system::cpu_sampler::{
    ProcessorPerformance, checked_summed_processor_times, query_processor_performance,
};
use crate::system::process_identity::ProcIdentity;

#[derive(Clone, Default)]
struct PreviousProcSample {
    // 上一轮采样值用于计算 CPU、内存增量和缺页增量。
    raw_cpu_time_100ns: u64,
    mem_usage_kb: u64,
    page_faults: u32,
}

// 列级脏标记位图。每轮刷新时标记变更列，rebuild_listview 只重绘被标记的行。
pub(super) struct WtsProcessIdentity {
    pub(super) user_name: Option<String>,
    pub(super) session_id: u32,
    pub(super) image_name_lower: String,
}

struct WtsProcessIdentitySnapshot {
    identities: HashMap<u32, WtsProcessIdentity>,
    row_error: Option<u32>,
}

#[derive(Clone)]
struct CachedAccountName {
    name: String,
    last_used: u64,
}

#[derive(Default)]
struct AccountNameCache {
    entries: HashMap<Vec<u8>, CachedAccountName>,
    generation: u64,
}

impl AccountNameCache {
    const MAX_ENTRIES: usize = 256;

    fn begin_refresh(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn get(&mut self, sid: &[u8]) -> Option<String> {
        let entry = self.entries.get_mut(sid)?;
        entry.last_used = self.generation;
        Some(entry.name.clone())
    }

    fn insert(&mut self, sid: Vec<u8>, name: String) {
        if !self.entries.contains_key(sid.as_slice())
            && self.entries.len() >= Self::MAX_ENTRIES
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(sid, _)| sid.clone())
        {
            self.entries.remove(oldest.as_slice());
        }
        self.entries.insert(
            sid,
            CachedAccountName {
                name,
                last_used: self.generation,
            },
        );
    }
}

#[derive(Default)]
struct ProcWorkerCache {
    metadata: HashMap<ProcIdentity, ProcStaticMetadata>,
    account_names: AccountNameCache,
    processor_info: Vec<ProcessorPerformance>,
}

// 常用文案缓存，避免命令执行路径上反复查本地化资源。
#[derive(Default)]
pub(super) struct ProcWorkerSnapshot {
    pub(super) entries: Vec<ProcEntry>,
    pub(super) row_error: Option<u32>,
}

struct CollectedProcessEntries {
    entries: Vec<ProcEntry>,
    next_samples: HashMap<ProcIdentity, PreviousProcSample>,
    row_error: Option<u32>,
}

pub(super) type ProcWorkerResult = Result<ProcWorkerSnapshot, u32>;

pub(super) struct ProcWorkerRequest {
    pub(super) processor_count: usize,
}

#[derive(Default)]
pub(super) struct ProcWorkerState {
    cache: ProcWorkerCache,
    previous_samples: HashMap<ProcIdentity, PreviousProcSample>,
    previous_system_time: Option<u64>,
}

impl ProcWorkerState {
    pub(super) fn collect(&mut self, request: ProcWorkerRequest) -> ProcWorkerResult {
        let system_time =
            current_system_time(request.processor_count, &mut self.cache.processor_info)?;
        let total_delta = system_time_delta(system_time, self.previous_system_time);
        let collected = unsafe {
            collect_process_entries(&self.previous_samples, total_delta, &mut self.cache)?
        };

        // CPU and per-process deltas must share the same successful snapshot boundary. A failed
        // process walk leaves both baselines untouched so the next sample remains comparable.
        self.previous_samples = collected.next_samples;
        self.previous_system_time = Some(system_time);
        Ok(ProcWorkerSnapshot {
            entries: collected.entries,
            row_error: collected.row_error,
        })
    }
}

fn well_known_service_name(sid: *mut core::ffi::c_void) -> Option<String> {
    unsafe {
        if IsWellKnownSid(sid, WinLocalSystemSid) != 0 {
            Some("SYSTEM".to_string())
        } else if IsWellKnownSid(sid, WinLocalServiceSid) != 0 {
            Some("LOCAL SERVICE".to_string())
        } else if IsWellKnownSid(sid, WinNetworkServiceSid) != 0 {
            Some("NETWORK SERVICE".to_string())
        } else {
            None
        }
    }
}

// 通过 LookupAccountSidW 将 SID 解析为经典 Task Manager 风格的账户名。
unsafe fn lookup_account_name_from_sid(sid: *mut core::ffi::c_void) -> Result<Option<String>, u32> {
    unsafe {
        if sid.is_null() || IsValidSid(sid) == 0 {
            return Ok(None);
        }
        if let Some(name) = well_known_service_name(sid) {
            return Ok(Some(name));
        }

        let mut name_len = 0u32;
        let mut domain_len = 0u32;
        let mut sid_use = 0 as SID_NAME_USE;
        if LookupAccountSidW(
            null_mut(),
            sid,
            null_mut(),
            &mut name_len,
            null_mut(),
            &mut domain_len,
            &mut sid_use,
        ) == 0
        {
            let error = GetLastError();
            if error == ERROR_NONE_MAPPED {
                return sid_string(sid).map(Some);
            }
            if error != ERROR_INSUFFICIENT_BUFFER {
                return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
            }
        }

        if name_len == 0 {
            return Err(ERROR_INVALID_DATA);
        }

        let mut name = vec![0u16; name_len as usize];
        let mut domain = vec![0u16; domain_len as usize];
        let domain_ptr = if domain.is_empty() {
            null_mut()
        } else {
            domain.as_mut_ptr()
        };
        if LookupAccountSidW(
            null_mut(),
            sid,
            name.as_mut_ptr(),
            &mut name_len,
            domain_ptr,
            &mut domain_len,
            &mut sid_use,
        ) == 0
        {
            let error = GetLastError();
            if error == ERROR_NONE_MAPPED {
                return sid_string(sid).map(Some);
            }
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        }

        let available = (name_len as usize).min(name.len());
        let length = name[..available]
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(available);
        Ok(Some(String::from_utf16_lossy(&name[..length])))
    }
}

unsafe fn lookup_account_name_from_sid_cached(
    sid: *mut core::ffi::c_void,
    account_cache: &mut AccountNameCache,
) -> Result<Option<String>, u32> {
    unsafe {
        if sid.is_null() || IsValidSid(sid) == 0 {
            return lookup_account_name_from_sid(sid);
        }
        let length = GetLengthSid(sid) as usize;
        if length == 0 {
            return lookup_account_name_from_sid(sid);
        }
        let cache_key = slice::from_raw_parts(sid as *const u8, length);
        if let Some(name) = account_cache.get(cache_key) {
            return Ok(Some(name));
        }

        let name = lookup_account_name_from_sid(sid)?;
        if let Some(name) = name.as_ref() {
            account_cache.insert(cache_key.to_vec(), name.clone());
        }
        Ok(name)
    }
}

unsafe fn query_process_account_name(
    process_handle: HANDLE,
    account_cache: &mut AccountNameCache,
) -> Result<Option<String>, u32> {
    unsafe {
        let mut raw_token = null_mut();
        if OpenProcessToken(process_handle, TOKEN_QUERY, &mut raw_token) == 0 {
            let error = GetLastError();
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        }
        let token = OwnedHandle::new(raw_token).ok_or(ERROR_INVALID_DATA)?;

        let mut required_bytes = 0u32;
        if GetTokenInformation(
            token.as_raw(),
            TokenUser,
            null_mut(),
            0,
            &mut required_bytes,
        ) != 0
        {
            return Err(ERROR_INVALID_DATA);
        }
        let error = GetLastError();
        if error != ERROR_INSUFFICIENT_BUFFER || required_bytes < size_of::<TOKEN_USER>() as u32 {
            return Err(if error == 0 {
                ERROR_INVALID_DATA
            } else {
                error
            });
        }

        let word_count = (required_bytes as usize).div_ceil(size_of::<usize>());
        let mut storage = vec![0usize; word_count];
        let storage_bytes = storage
            .len()
            .checked_mul(size_of::<usize>())
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or(ERROR_INVALID_DATA)?;
        let mut returned_bytes = 0u32;
        if GetTokenInformation(
            token.as_raw(),
            TokenUser,
            storage.as_mut_ptr() as *mut _,
            storage_bytes,
            &mut returned_bytes,
        ) == 0
        {
            let error = GetLastError();
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        }
        if returned_bytes < size_of::<TOKEN_USER>() as u32 || returned_bytes > storage_bytes {
            return Err(ERROR_INVALID_DATA);
        }

        let token_user = &*(storage.as_ptr() as *const TOKEN_USER);
        lookup_account_name_from_sid_cached(token_user.User.Sid, account_cache)
    }
}

unsafe fn sid_string(sid: *mut core::ffi::c_void) -> Result<String, u32> {
    unsafe {
        let mut string_sid = null_mut::<u16>();
        if ConvertSidToStringSidW(sid, &mut string_sid) == 0 || string_sid.is_null() {
            let error = GetLastError();
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        }

        let value = widestr_ptr_to_string(string_sid);
        if !LocalFree(string_sid as _).is_null() {
            let error = GetLastError();
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        }
        Ok(value)
    }
}

unsafe fn collect_process_identity_map(
    required_pids: &HashSet<u32>,
    account_cache: &mut AccountNameCache,
) -> Result<WtsProcessIdentitySnapshot, u32> {
    unsafe {
        if required_pids.is_empty() {
            return Ok(WtsProcessIdentitySnapshot {
                identities: HashMap::new(),
                row_error: None,
            });
        }
        // WTS 进程枚举能一次拿到大量进程对应的 SID / Session 信息，
        // 先建表再回填到快照里，比逐进程单查用户名更高效。
        let mut process_info = null_mut::<WTS_PROCESS_INFOW>();
        let mut count = 0u32;

        let enumeration_result = WTSEnumerateProcessesW(
            WTS_CURRENT_SERVER_HANDLE,
            0,
            1,
            &mut process_info,
            &mut count,
        );
        if enumeration_result == 0 {
            let error = windows_sys::Win32::Foundation::GetLastError();
            let _unexpected_buffer = OwnedWtsMemory::new(process_info);
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        }
        let process_info = OwnedWtsMemory::new(process_info).ok_or(ERROR_INVALID_DATA)?;

        let mut identities = HashMap::with_capacity(count as usize);
        let mut row_error = None;
        let processes = slice::from_raw_parts(process_info.as_ptr(), count as usize);
        for process in processes {
            let pid = process.ProcessId;
            if !required_pids.contains(&pid) {
                continue;
            }
            let user_name = if pid == 0 {
                Some("SYSTEM".to_string())
            } else {
                match lookup_account_name_from_sid_cached(process.pUserSid, account_cache) {
                    Ok(name) => name,
                    Err(error) => {
                        row_error.get_or_insert(error);
                        None
                    }
                }
            };
            identities.insert(
                pid,
                WtsProcessIdentity {
                    user_name,
                    session_id: process.SessionId,
                    image_name_lower: widestr_ptr_to_string(process.pProcessName).to_lowercase(),
                },
            );
        }

        Ok(WtsProcessIdentitySnapshot {
            identities,
            row_error,
        })
    }
}

pub(super) fn merge_wts_process_identity(
    entry: &mut ProcEntry,
    wts_identity: Option<&WtsProcessIdentity>,
) -> bool {
    let Some(wts_identity) = wts_identity else {
        return false;
    };

    // WTS only keys rows by PID. Requiring the same image and a compatible session prevents a
    // stale WTS row from overwriting a newer process after PID reuse.
    if !wts_identity_matches(&entry.image_name_lower, entry.session_id, wts_identity) {
        return false;
    }

    let user_name = wts_identity
        .user_name
        .as_ref()
        .filter(|user_name| !user_name.is_empty());
    if entry.user_name.is_empty()
        && let Some(user_name) = user_name
    {
        entry.user_name.clone_from(user_name);
        entry.user_name_lower = user_name.to_lowercase();
    }
    if entry.session_id.is_none() {
        entry.session_id = Some(wts_identity.session_id);
    }
    user_name.is_some()
}

pub(super) fn wts_identity_matches(
    image_name_lower: &str,
    session_id: Option<u32>,
    wts_identity: &WtsProcessIdentity,
) -> bool {
    wts_identity.image_name_lower == image_name_lower
        && session_id.is_none_or(|session_id| session_id == wts_identity.session_id)
}

// 从 Options 中解析当前激活的列列表，过滤无效列 ID。
fn kb_from_bytes(value: usize) -> u64 {
    (value as u64) / 1024
}

pub(super) fn signed_kb_delta(current: u64, previous: u64) -> i64 {
    if current >= previous {
        (current - previous).min(i64::MAX as u64) as i64
    } else {
        -((previous - current).min(i64::MAX as u64) as i64)
    }
}

// 通过 QueryFullProcessImageNameW 查询进程可执行文件的全路径。
unsafe fn collect_process_entries(
    previous_samples: &HashMap<ProcIdentity, PreviousProcSample>,
    total_delta: Option<u64>,
    cache: &mut ProcWorkerCache,
) -> Result<CollectedProcessEntries, u32> {
    unsafe {
        // 采样阶段只构造“当下这一轮”的快照，真正的增量计算依赖外部传入的历史样本。
        let raw_snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        let Some(snapshot) = OwnedHandle::new(raw_snapshot) else {
            let error = windows_sys::Win32::Foundation::GetLastError();
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        };

        let mut entries = Vec::with_capacity(previous_samples.len().max(64));
        let mut next_samples = HashMap::with_capacity(previous_samples.len().max(64));
        let mut resolved_user_identities = HashSet::with_capacity(previous_samples.len().max(64));
        let mut row_error = None;
        let mut process_entry = zeroed::<PROCESSENTRY32W>();
        process_entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

        if Process32FirstW(snapshot.as_raw(), &mut process_entry) == 0 {
            let error = windows_sys::Win32::Foundation::GetLastError();
            return if error == ERROR_NO_MORE_FILES {
                Ok(CollectedProcessEntries {
                    entries,
                    next_samples,
                    row_error,
                })
            } else {
                Err(if error == 0 { ERROR_GEN_FAILURE } else { error })
            };
        }
        cache.account_names.begin_refresh();

        loop {
            let pid = process_entry.th32ProcessID;
            let thread_count = process_entry.cntThreads;
            let image_name = utf16_buffer_to_string(&process_entry.szExeFile);
            let mut entry = ProcEntry {
                identity: ProcIdentity::pid_only(pid),
                pid,
                image_name: image_name.clone(),
                image_name_lower: image_name.to_lowercase(),
                is_32_bit: None,
                user_name: String::new(),
                user_name_lower: String::new(),
                session_id: None,
                cpu: 0,
                cpu_time_100ns: 0,
                display_cpu_time_100ns: 0,
                mem_usage_kb: 0,
                mem_diff_kb: 0,
                page_faults: 0,
                page_faults_diff: 0,
                commit_charge_kb: 0,
                paged_pool_kb: 0,
                nonpaged_pool_kb: 0,
                priority_class: NORMAL_PRIORITY_CLASS,
                handle_count: 0,
                thread_count,
                display_text: std::array::from_fn(|_| String::new()),
                pass_count: 0,
                dirty_columns: DirtyColumns::default(),
            };
            let mut raw_cpu_time_100ns = 0u64;

            if pid == 0 {
                entry.user_name = "SYSTEM".to_string();
                entry.user_name_lower = "system".to_string();
                entry.session_id = Some(0);
            }

            let memory_handle = if pid == 0 {
                None
            } else {
                OwnedHandle::new(OpenProcess(
                    PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
                    0,
                    pid,
                ))
            };
            let query_handle = if pid != 0 && memory_handle.is_none() {
                OwnedHandle::new(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid))
            } else {
                None
            };
            let info_handle = memory_handle
                .as_ref()
                .or(query_handle.as_ref())
                .map(OwnedHandle::as_raw);

            if let Some(info_handle) = info_handle {
                let mut creation = zeroed::<FILETIME>();
                let mut exit = zeroed::<FILETIME>();
                let mut kernel = zeroed::<FILETIME>();
                let mut user = zeroed::<FILETIME>();
                if GetProcessTimes(
                    info_handle,
                    &mut creation,
                    &mut exit,
                    &mut kernel,
                    &mut user,
                ) != 0
                {
                    entry.identity = ProcIdentity::new(pid, filetime_to_u64(creation));
                    let cpu_time_100ns =
                        filetime_to_u64(kernel).saturating_add(filetime_to_u64(user));
                    raw_cpu_time_100ns = cpu_time_100ns;
                    entry.cpu_time_100ns = cpu_time_100ns;
                    entry.display_cpu_time_100ns = cpu_time_100ns;
                    if let Some(previous) = previous_samples.get(&entry.identity)
                        && let Some(total_delta) = total_delta
                        && let Some(process_delta) =
                            cpu_time_100ns.checked_sub(previous.raw_cpu_time_100ns)
                    {
                        entry.cpu = cpu_percent_from_delta(process_delta, total_delta);
                    }
                }

                if let Some(metadata) = cache.metadata.get(&entry.identity) {
                    entry.apply_static_metadata(metadata);
                    if metadata.user_identity_resolved {
                        resolved_user_identities.insert(entry.identity);
                    }
                }
                if entry.identity.is_verified() && entry.is_32_bit.is_none() {
                    entry.is_32_bit = match is_32_bit_process_handle(info_handle) {
                        Ok(is_32_bit) => Some(is_32_bit),
                        Err(error) => {
                            row_error.get_or_insert(if error == 0 {
                                ERROR_GEN_FAILURE
                            } else {
                                error
                            });
                            None
                        }
                    };
                }

                if entry.identity.is_verified()
                    && !resolved_user_identities.contains(&entry.identity)
                {
                    // The token is tied to the already creation-time-verified process handle.
                    // Access-denied and exit races remain row-local; WTS resolves those rows in
                    // one identity-validated batch after the process walk completes.
                    if let Ok(Some(user_name)) =
                        query_process_account_name(info_handle, &mut cache.account_names)
                    {
                        entry.user_name_lower = user_name.to_lowercase();
                        entry.user_name = user_name;
                        resolved_user_identities.insert(entry.identity);
                    }
                }

                if let Some(memory_handle) = memory_handle.as_ref() {
                    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
                        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
                        ..zeroed()
                    };
                    if K32GetProcessMemoryInfo(
                        memory_handle.as_raw(),
                        &mut counters as *mut _ as *mut _,
                        size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
                    ) != 0
                    {
                        entry.mem_usage_kb = kb_from_bytes(counters.WorkingSetSize);
                        entry.page_faults = counters.PageFaultCount;
                        entry.commit_charge_kb = kb_from_bytes(counters.PrivateUsage);
                        entry.paged_pool_kb = kb_from_bytes(counters.QuotaPagedPoolUsage);
                        entry.nonpaged_pool_kb = kb_from_bytes(counters.QuotaNonPagedPoolUsage);

                        if let Some(previous) = previous_samples.get(&entry.identity) {
                            entry.mem_diff_kb =
                                signed_kb_delta(entry.mem_usage_kb, previous.mem_usage_kb);
                            entry.page_faults_diff =
                                i64::from(entry.page_faults) - i64::from(previous.page_faults);
                        }
                    }
                }

                let mut handle_count = 0u32;
                if GetProcessHandleCount(info_handle, &mut handle_count) != 0 {
                    entry.handle_count = handle_count;
                }

                let priority_class = GetPriorityClass(info_handle);
                if priority_class != 0 {
                    entry.priority_class = priority_class;
                }

                if entry.identity.is_verified() {
                    next_samples.insert(
                        entry.identity,
                        PreviousProcSample {
                            raw_cpu_time_100ns,
                            mem_usage_kb: entry.mem_usage_kb,
                            page_faults: entry.page_faults,
                        },
                    );
                }
            }

            entries.push(entry);

            if Process32NextW(snapshot.as_raw(), &mut process_entry) == 0 {
                let error = windows_sys::Win32::Foundation::GetLastError();
                if error != ERROR_NO_MORE_FILES {
                    return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
                }
                break;
            }
        }

        let required_identity_pids = entries
            .iter()
            .filter(|entry| {
                entry.pid != 0
                    && (!entry.identity.is_verified()
                        || cache
                            .metadata
                            .get(&entry.identity)
                            .is_none_or(|metadata| !metadata.user_identity_resolved))
            })
            .map(|entry| entry.pid)
            .collect::<HashSet<_>>();
        let identity_snapshot =
            collect_process_identity_map(&required_identity_pids, &mut cache.account_names)?;
        row_error = row_error.or(identity_snapshot.row_error);
        for entry in entries.iter_mut().filter(|entry| entry.pid != 0) {
            if merge_wts_process_identity(entry, identity_snapshot.identities.get(&entry.pid))
                && entry.identity.is_verified()
            {
                resolved_user_identities.insert(entry.identity);
            }
        }

        let current_identities = entries
            .iter()
            .map(|entry| entry.identity)
            .filter(|identity| identity.is_verified())
            .collect::<HashSet<_>>();
        cache
            .metadata
            .retain(|identity, _| current_identities.contains(identity));
        for entry in entries.iter().filter(|entry| entry.identity.is_verified()) {
            let resolved = resolved_user_identities.contains(&entry.identity);
            if let Some(metadata) = cache.metadata.get_mut(&entry.identity) {
                if resolved && !metadata.user_identity_resolved {
                    metadata.user_name.clone_from(&entry.user_name);
                    metadata.user_name_lower.clone_from(&entry.user_name_lower);
                    metadata.session_id = entry.session_id;
                    metadata.user_identity_resolved = true;
                }
            } else {
                cache.metadata.insert(
                    entry.identity,
                    ProcStaticMetadata {
                        is_32_bit: entry.is_32_bit,
                        user_name: entry.user_name.clone(),
                        user_name_lower: entry.user_name_lower.clone(),
                        session_id: entry.session_id,
                        user_identity_resolved: resolved,
                    },
                );
            }
        }

        Ok(CollectedProcessEntries {
            entries,
            next_samples,
            row_error,
        })
    }
}

// 读取系统总 CPU 时间（kernel + user），用于计算每轮刷新的 CPU 时间增量。
fn current_system_time(
    processor_count: usize,
    processor_info: &mut Vec<ProcessorPerformance>,
) -> Result<u64, u32> {
    query_processor_performance(processor_count.max(1), processor_info)
        .map_err(ntstatus_to_win32)?;
    let (_, kernel, user) =
        checked_summed_processor_times(processor_info).ok_or(ERROR_INVALID_DATA)?;
    kernel.checked_add(user).ok_or(ERROR_INVALID_DATA)
}

fn ntstatus_to_win32(status: i32) -> u32 {
    let error = unsafe { RtlNtStatusToDosError(status) };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

pub(super) fn system_time_delta(current: u64, previous: Option<u64>) -> Option<u64> {
    previous.and_then(|previous| current.checked_sub(previous))
}

// 将 Win32 FILETIME 结构合并为一个 u64（100ns 单位）。
fn filetime_to_u64(filetime: FILETIME) -> u64 {
    (u64::from(filetime.dwHighDateTime) << 32) | u64::from(filetime.dwLowDateTime)
}

// 将以 null 结尾的 UTF-16 切片转换为 Rust String，忽略 BOM 和无效代理对。
fn utf16_buffer_to_string(buffer: &[u16]) -> String {
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

// 根据进程 CPU 时间增量与系统总时间增量计算 CPU 使用率百分比。
pub(super) fn cpu_percent_from_delta(delta_100ns: u64, total_delta_100ns: u64) -> u8 {
    if total_delta_100ns == 0 {
        return 0;
    }
    let rounded = (u128::from(delta_100ns) * 100 + u128::from(total_delta_100ns) / 2)
        / u128::from(total_delta_100ns);
    rounded.min(100) as u8
}
