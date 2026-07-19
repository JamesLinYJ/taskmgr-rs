// +-------------------------------------------------------------------------
//
//   taskmgr-rs - CPU 原生拓扑与功能采集
//
//   文件:       src/pages/cpu/native.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 使用 group-aware Win32 拓扑与处理器功能接口构建 CPU 原生诊断快照。
//!
//! 所有查询在 CPU native worker 上执行；失败来源不会推进或覆盖其他来源。

use std::collections::{BTreeMap, BTreeSet};
use std::mem::{offset_of, size_of, zeroed};
use std::ptr::{null_mut, read_unaligned};
use std::slice;

use windows_sys::Win32::Foundation::{ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER, GetLastError};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::SystemInformation::{
    CACHE_RELATIONSHIP, CacheData, CacheInstruction, CacheTrace, CacheUnified, GROUP_AFFINITY,
    GROUP_RELATIONSHIP, GetLogicalProcessorInformationEx, GetNativeSystemInfo,
    NUMA_NODE_RELATIONSHIP, PROCESSOR_GROUP_INFO, PROCESSOR_RELATIONSHIP, RelationAll,
    RelationCache, RelationGroup, RelationNumaNode, RelationNumaNodeEx, RelationProcessorCore,
    RelationProcessorDie, RelationProcessorModule, RelationProcessorPackage, SYSTEM_INFO,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows_sys::Win32::System::SystemServices::CACHE_FULLY_ASSOCIATIVE;
use windows_sys::Win32::System::Threading::{
    IsProcessorFeaturePresent, PF_3DNOW_INSTRUCTIONS_AVAILABLE, PF_ARM_64BIT_LOADSTORE_ATOMIC,
    PF_ARM_DIVIDE_INSTRUCTION_AVAILABLE, PF_ARM_FMAC_INSTRUCTIONS_AVAILABLE,
    PF_ARM_NEON_INSTRUCTIONS_AVAILABLE, PF_ARM_V8_CRC32_INSTRUCTIONS_AVAILABLE,
    PF_ARM_V8_CRYPTO_INSTRUCTIONS_AVAILABLE, PF_ARM_V8_INSTRUCTIONS_AVAILABLE,
    PF_ARM_V81_ATOMIC_INSTRUCTIONS_AVAILABLE, PF_ARM_V82_DP_INSTRUCTIONS_AVAILABLE,
    PF_ARM_V83_JSCVT_INSTRUCTIONS_AVAILABLE, PF_ARM_V83_LRCPC_INSTRUCTIONS_AVAILABLE,
    PF_AVX_INSTRUCTIONS_AVAILABLE, PF_AVX2_INSTRUCTIONS_AVAILABLE,
    PF_AVX512F_INSTRUCTIONS_AVAILABLE, PF_ERMS_AVAILABLE, PF_MMX_INSTRUCTIONS_AVAILABLE,
    PF_NX_ENABLED, PF_PAE_ENABLED, PF_RDPID_INSTRUCTION_AVAILABLE, PF_RDRAND_INSTRUCTION_AVAILABLE,
    PF_RDTSC_INSTRUCTION_AVAILABLE, PF_RDTSCP_INSTRUCTION_AVAILABLE, PF_RDWRFSGSBASE_AVAILABLE,
    PF_SECOND_LEVEL_ADDRESS_TRANSLATION, PF_SSE3_INSTRUCTIONS_AVAILABLE,
    PF_SSE4_1_INSTRUCTIONS_AVAILABLE, PF_SSE4_2_INSTRUCTIONS_AVAILABLE,
    PF_SSSE3_INSTRUCTIONS_AVAILABLE, PF_VIRT_FIRMWARE_ENABLED, PF_XMMI_INSTRUCTIONS_AVAILABLE,
    PF_XMMI64_INSTRUCTIONS_AVAILABLE, PF_XSAVE_ENABLED,
};

use super::model::{
    CpuArchitecture, CpuCacheInfo, CpuCacheKind, CpuComponentUpdate, CpuDetailError,
    CpuDetailRefresh, CpuDetailRequest, CpuDynamicInfo, CpuFeatureInfo, CpuNativeSnapshot,
    CpuSupplementalTopology, CpuTopologyKey, invalid, invalid_error, last_error_or_gen_failure,
};
use super::pdh::{CpuPdhCollectOutcome, CpuPdhQuery};
use crate::infrastructure::native::record_startup_timing;
use crate::system::cpu_topology::LogicalProcessorId;

pub(crate) struct CpuNativeCollector {
    topology_key: Option<CpuTopologyKey>,
    topology_done: bool,
    topology_failed: bool,
    features_done: bool,
    pdh: Option<CpuPdhQuery>,
    pdh_failed: bool,
    startup_started_ms: u64,
    topology_timing_recorded: bool,
    baseline_timing_recorded: bool,
    first_dynamic_recorded: bool,
}

impl CpuNativeCollector {
    pub(crate) fn new() -> Self {
        Self {
            topology_key: None,
            topology_done: false,
            topology_failed: false,
            features_done: false,
            pdh: None,
            pdh_failed: false,
            startup_started_ms: unsafe { GetTickCount64() },
            topology_timing_recorded: false,
            baseline_timing_recorded: false,
            first_dynamic_recorded: false,
        }
    }

    pub(crate) fn collect(&mut self, request: CpuDetailRequest) -> CpuNativeSnapshot {
        if self.topology_key.as_ref() != Some(&request.topology_key) {
            self.reset_for_topology(&request.topology_key);
        }

        let retry_failed = request.refresh == CpuDetailRefresh::User;
        let topology = if !self.topology_done || (retry_failed && self.topology_failed) {
            let started_ms = unsafe { GetTickCount64() };
            let result = query_supplemental_topology(&request.topology_key);
            if !self.topology_timing_recorded {
                record_startup_timing(
                    "CPU native topology",
                    unsafe { GetTickCount64() }.wrapping_sub(started_ms),
                );
                self.topology_timing_recorded = true;
            }
            match result {
                Ok(value) => {
                    self.topology_done = true;
                    self.topology_failed = false;
                    CpuComponentUpdate::Success(value)
                }
                Err(error) => {
                    self.topology_done = true;
                    self.topology_failed = true;
                    CpuComponentUpdate::Failed(error)
                }
            }
        } else {
            CpuComponentUpdate::Unchanged
        };

        let features = if !self.features_done {
            self.features_done = true;
            CpuComponentUpdate::Success(query_cpu_features())
        } else {
            CpuComponentUpdate::Unchanged
        };

        let (dynamic, pdh_baseline_timestamp_ms) = self.collect_dynamic(&request);
        CpuNativeSnapshot {
            topology_key: request.topology_key,
            topology,
            features,
            dynamic,
            pdh_baseline_timestamp_ms,
        }
    }

    fn collect_dynamic(
        &mut self,
        request: &CpuDetailRequest,
    ) -> (CpuComponentUpdate<CpuDynamicInfo>, Option<u64>) {
        if request.refresh == CpuDetailRefresh::User && self.pdh_failed {
            self.pdh = None;
            self.pdh_failed = false;
        }
        if self.pdh.is_none() && !self.pdh_failed {
            match CpuPdhQuery::new(request.topology_key.logical_processors()) {
                Ok(query) => self.pdh = Some(query),
                Err(error) => {
                    self.pdh_failed = true;
                    return (CpuComponentUpdate::Failed(error), None);
                }
            }
        }
        let Some(pdh) = self.pdh.as_mut() else {
            return (CpuComponentUpdate::Unchanged, None);
        };
        if request.refresh == CpuDetailRefresh::Activation {
            pdh.reset_sample_baseline();
        }
        match pdh.collect() {
            Ok(CpuPdhCollectOutcome::Sample(value)) => {
                if !self.first_dynamic_recorded {
                    record_startup_timing(
                        "CPU first dynamic sample",
                        unsafe { GetTickCount64() }.wrapping_sub(self.startup_started_ms),
                    );
                    self.first_dynamic_recorded = true;
                }
                (CpuComponentUpdate::Success(value), None)
            }
            Ok(CpuPdhCollectOutcome::Primed(timestamp_ms)) => {
                if !self.baseline_timing_recorded {
                    record_startup_timing(
                        "CPU PDH baseline",
                        timestamp_ms.wrapping_sub(self.startup_started_ms),
                    );
                    self.baseline_timing_recorded = true;
                }
                (CpuComponentUpdate::Unchanged, Some(timestamp_ms))
            }
            Err(error) => {
                self.pdh = None;
                self.pdh_failed = true;
                (CpuComponentUpdate::Failed(error), None)
            }
        }
    }

    fn reset_for_topology(&mut self, key: &CpuTopologyKey) {
        self.topology_key = Some(key.clone());
        self.topology_done = false;
        self.topology_failed = false;
        self.features_done = false;
        self.pdh = None;
        self.pdh_failed = false;
        self.startup_started_ms = unsafe { GetTickCount64() };
        self.topology_timing_recorded = false;
        self.baseline_timing_recorded = false;
        self.first_dynamic_recorded = false;
    }
}

fn query_supplemental_topology(
    key: &CpuTopologyKey,
) -> Result<CpuSupplementalTopology, CpuDetailError> {
    let mut byte_len = 0u32;
    let first = unsafe { GetLogicalProcessorInformationEx(RelationAll, null_mut(), &mut byte_len) };
    if first != 0 || byte_len == 0 {
        return Err(CpuDetailError::Win32 {
            context: "CPU RelationAll buffer sizing",
            code: last_error_or_gen_failure(),
        });
    }
    let sizing_error = unsafe { GetLastError() };
    if sizing_error != ERROR_INSUFFICIENT_BUFFER {
        return Err(CpuDetailError::Win32 {
            context: "CPU RelationAll buffer sizing",
            code: if sizing_error == 0 {
                ERROR_GEN_FAILURE
            } else {
                sizing_error
            },
        });
    }

    let word_count = usize::try_from(byte_len)
        .ok()
        .and_then(|length| length.checked_add(size_of::<u64>() - 1))
        .map(|length| length / size_of::<u64>())
        .ok_or(CpuDetailError::InvalidData {
            context: "CPU RelationAll buffer length",
        })?;
    let mut words = vec![0u64; word_count];
    let capacity =
        words
            .len()
            .checked_mul(size_of::<u64>())
            .ok_or(CpuDetailError::InvalidData {
                context: "CPU RelationAll allocation length",
            })?;
    let result = unsafe {
        GetLogicalProcessorInformationEx(RelationAll, words.as_mut_ptr().cast(), &mut byte_len)
    };
    if result == 0 {
        return Err(CpuDetailError::Win32 {
            context: "CPU RelationAll query",
            code: last_error_or_gen_failure(),
        });
    }
    let returned_len = usize::try_from(byte_len).map_err(|_| CpuDetailError::InvalidData {
        context: "CPU RelationAll returned length",
    })?;
    if returned_len == 0 || returned_len > capacity {
        return Err(CpuDetailError::InvalidData {
            context: "CPU RelationAll returned bounds",
        });
    }
    let bytes = unsafe { slice::from_raw_parts(words.as_ptr().cast::<u8>(), returned_len) };
    parse_relation_all(bytes, key)
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct CacheKey {
    level: u8,
    kind: CpuCacheKind,
    bytes: u32,
    line_size: u16,
    associativity: Option<u8>,
}

pub(super) fn parse_relation_all(
    bytes: &[u8],
    key: &CpuTopologyKey,
) -> Result<CpuSupplementalTopology, CpuDetailError> {
    const RELATION_PROCESSOR_CORE_VALUE: i32 = RelationProcessorCore;
    const RELATION_PROCESSOR_PACKAGE_VALUE: i32 = RelationProcessorPackage;
    const RELATION_PROCESSOR_DIE_VALUE: i32 = RelationProcessorDie;
    const RELATION_PROCESSOR_MODULE_VALUE: i32 = RelationProcessorModule;
    const RELATION_NUMA_NODE_VALUE: i32 = RelationNumaNode;
    const RELATION_NUMA_NODE_EX_VALUE: i32 = RelationNumaNodeEx;
    const RELATION_CACHE_VALUE: i32 = RelationCache;
    const RELATION_GROUP_VALUE: i32 = RelationGroup;
    const CACHE_UNIFIED_VALUE: i32 = CacheUnified;
    const CACHE_INSTRUCTION_VALUE: i32 = CacheInstruction;
    const CACHE_DATA_VALUE: i32 = CacheData;
    const CACHE_TRACE_VALUE: i32 = CacheTrace;
    const HEADER_SIZE: usize = offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous);
    if bytes.is_empty() {
        return invalid("CPU RelationAll empty result");
    }
    let expected = key
        .identities()
        .iter()
        .map(|processor| processor.id)
        .collect::<BTreeSet<_>>();
    if expected.len() != key.identities().len() || expected.is_empty() {
        return invalid("CPU detail topology key identities");
    }

    let mut package_members = BTreeSet::new();
    let mut numa_members = BTreeSet::new();
    let mut core_members = BTreeSet::new();
    let mut die_members = BTreeSet::new();
    let mut module_members = BTreeSet::new();
    let mut package_count = 0usize;
    let mut numa_nodes = BTreeSet::new();
    let mut die_count = 0usize;
    let mut module_count = 0usize;
    let mut saw_die = false;
    let mut saw_module = false;
    let mut saw_group = false;
    let mut cache_counts = BTreeMap::<CacheKey, usize>::new();

    let mut offset = 0usize;
    while offset < bytes.len() {
        if bytes.len() - offset < HEADER_SIZE {
            return invalid("CPU RelationAll truncated record header");
        }
        let relationship = read_value::<i32>(bytes, offset)?;
        let record_size = usize::try_from(read_value::<u32>(bytes, offset + size_of::<i32>())?)
            .map_err(|_| invalid_error("CPU RelationAll record size"))?;
        if record_size < HEADER_SIZE {
            return invalid("CPU RelationAll record minimum size");
        }
        let record_end = offset
            .checked_add(record_size)
            .ok_or_else(|| invalid_error("CPU RelationAll record overflow"))?;
        if record_end > bytes.len() {
            return invalid("CPU RelationAll record bounds");
        }
        let record = &bytes[offset..record_end];
        match relationship {
            RELATION_PROCESSOR_CORE_VALUE
            | RELATION_PROCESSOR_PACKAGE_VALUE
            | RELATION_PROCESSOR_DIE_VALUE
            | RELATION_PROCESSOR_MODULE_VALUE => {
                let members = parse_processor_members(record)?;
                validate_member_subset(&members, &expected)?;
                match relationship {
                    RELATION_PROCESSOR_CORE_VALUE => insert_partition(&mut core_members, &members)?,
                    RELATION_PROCESSOR_PACKAGE_VALUE => {
                        package_count = package_count.checked_add(1).ok_or_else(|| {
                            invalid_error("CPU package relationship count overflow")
                        })?;
                        insert_partition(&mut package_members, &members)?;
                    }
                    RELATION_PROCESSOR_DIE_VALUE => {
                        saw_die = true;
                        die_count = die_count
                            .checked_add(1)
                            .ok_or_else(|| invalid_error("CPU die relationship count overflow"))?;
                        insert_partition(&mut die_members, &members)?;
                    }
                    RELATION_PROCESSOR_MODULE_VALUE => {
                        saw_module = true;
                        module_count = module_count.checked_add(1).ok_or_else(|| {
                            invalid_error("CPU module relationship count overflow")
                        })?;
                        insert_partition(&mut module_members, &members)?;
                    }
                    _ => unreachable!(),
                }
            }
            RELATION_NUMA_NODE_VALUE | RELATION_NUMA_NODE_EX_VALUE => {
                let numa_offset = HEADER_SIZE;
                let node_number = read_value::<u32>(record, numa_offset)?;
                let members = parse_group_members(
                    record,
                    numa_offset + offset_of!(NUMA_NODE_RELATIONSHIP, GroupCount),
                    numa_offset + offset_of!(NUMA_NODE_RELATIONSHIP, Anonymous),
                )?;
                validate_member_subset(&members, &expected)?;
                insert_partition(&mut numa_members, &members)?;
                numa_nodes.insert(node_number);
            }
            RELATION_CACHE_VALUE => {
                let cache_offset = HEADER_SIZE;
                let level = read_value::<u8>(record, cache_offset)?;
                if !(1..=3).contains(&level) {
                    return invalid("CPU cache level");
                }
                let associativity = read_value::<u8>(
                    record,
                    cache_offset + offset_of!(CACHE_RELATIONSHIP, Associativity),
                )?;
                let line_size = read_value::<u16>(
                    record,
                    cache_offset + offset_of!(CACHE_RELATIONSHIP, LineSize),
                )?;
                let cache_size = read_value::<u32>(
                    record,
                    cache_offset + offset_of!(CACHE_RELATIONSHIP, CacheSize),
                )?;
                let cache_type =
                    read_value::<i32>(record, cache_offset + offset_of!(CACHE_RELATIONSHIP, Type))?;
                if line_size == 0 || cache_size == 0 {
                    return invalid("CPU cache size and line size");
                }
                let kind = match cache_type {
                    CACHE_UNIFIED_VALUE => CpuCacheKind::Unified,
                    CACHE_INSTRUCTION_VALUE => CpuCacheKind::Instruction,
                    CACHE_DATA_VALUE => CpuCacheKind::Data,
                    CACHE_TRACE_VALUE => CpuCacheKind::Trace,
                    _ => return invalid("CPU cache type"),
                };
                let members = parse_group_members(
                    record,
                    cache_offset + offset_of!(CACHE_RELATIONSHIP, GroupCount),
                    cache_offset + offset_of!(CACHE_RELATIONSHIP, Anonymous),
                )?;
                validate_member_subset(&members, &expected)?;
                let cache_key = CacheKey {
                    level,
                    kind,
                    bytes: cache_size,
                    line_size,
                    associativity: (u32::from(associativity) != CACHE_FULLY_ASSOCIATIVE)
                        .then_some(associativity),
                };
                *cache_counts.entry(cache_key).or_default() += 1;
            }
            RELATION_GROUP_VALUE => {
                validate_group_relationship(record, &expected)?;
                if saw_group {
                    return invalid("CPU duplicate group relationship");
                }
                saw_group = true;
            }
            _ => return invalid("CPU RelationAll unknown relationship"),
        }
        offset = record_end;
    }

    if package_count == 0 || package_members != expected || core_members != expected {
        return invalid("CPU package or core topology completeness");
    }
    if numa_nodes.is_empty() || numa_members != expected || !saw_group {
        return invalid("CPU NUMA or group topology completeness");
    }
    if saw_die && die_members != expected {
        return invalid("CPU die topology completeness");
    }
    if saw_module && module_members != expected {
        return invalid("CPU module topology completeness");
    }

    let group_count = expected
        .iter()
        .map(|processor| processor.group)
        .collect::<BTreeSet<_>>()
        .len();
    let caches = cache_counts
        .into_iter()
        .map(|(cache, instance_count)| {
            let bytes_per_instance = u64::from(cache.bytes);
            let total_bytes = bytes_per_instance
                .checked_mul(instance_count as u64)
                .ok_or_else(|| invalid_error("CPU cache total size overflow"))?;
            Ok(CpuCacheInfo {
                level: cache.level,
                kind: cache.kind,
                instance_count,
                bytes_per_instance,
                total_bytes,
                line_size: cache.line_size,
                associativity: cache.associativity,
            })
        })
        .collect::<Result<Vec<_>, CpuDetailError>>()?;
    Ok(CpuSupplementalTopology {
        package_count,
        numa_node_count: numa_nodes.len(),
        group_count,
        die_count: saw_die.then_some(die_count),
        module_count: saw_module.then_some(module_count),
        caches,
    })
}

fn parse_processor_members(record: &[u8]) -> Result<BTreeSet<LogicalProcessorId>, CpuDetailError> {
    const HEADER_SIZE: usize = offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous);
    parse_group_members(
        record,
        HEADER_SIZE + offset_of!(PROCESSOR_RELATIONSHIP, GroupCount),
        HEADER_SIZE + offset_of!(PROCESSOR_RELATIONSHIP, GroupMask),
    )
}

fn parse_group_members(
    record: &[u8],
    group_count_offset: usize,
    masks_offset: usize,
) -> Result<BTreeSet<LogicalProcessorId>, CpuDetailError> {
    let group_count = usize::from(read_value::<u16>(record, group_count_offset)?);
    if group_count == 0 {
        return invalid("CPU topology empty group mask list");
    }
    let required = group_count
        .checked_mul(size_of::<GROUP_AFFINITY>())
        .and_then(|size| masks_offset.checked_add(size))
        .ok_or_else(|| invalid_error("CPU topology group mask size overflow"))?;
    if required > record.len() {
        return invalid("CPU topology group mask bounds");
    }
    let mut groups = BTreeSet::new();
    let mut members = BTreeSet::new();
    for index in 0..group_count {
        let offset = masks_offset
            .checked_add(index * size_of::<GROUP_AFFINITY>())
            .ok_or_else(|| invalid_error("CPU topology group mask offset"))?;
        let affinity = read_value::<GROUP_AFFINITY>(record, offset)?;
        if affinity.Mask == 0 || !groups.insert(affinity.Group) {
            return invalid("CPU topology empty or duplicate group mask");
        }
        let mut mask = affinity.Mask;
        while mask != 0 {
            let number = mask.trailing_zeros();
            if number > u8::MAX as u32 {
                return invalid("CPU topology logical processor number");
            }
            if !members.insert(LogicalProcessorId {
                group: affinity.Group,
                number: number as u8,
            }) {
                return invalid("CPU topology duplicate logical processor");
            }
            mask &= mask - 1;
        }
    }
    Ok(members)
}

fn validate_group_relationship(
    record: &[u8],
    expected: &BTreeSet<LogicalProcessorId>,
) -> Result<(), CpuDetailError> {
    const HEADER_SIZE: usize = offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous);
    let group_offset = HEADER_SIZE;
    let active_count = usize::from(read_value::<u16>(
        record,
        group_offset + offset_of!(GROUP_RELATIONSHIP, ActiveGroupCount),
    )?);
    let maximum_count = usize::from(read_value::<u16>(
        record,
        group_offset + offset_of!(GROUP_RELATIONSHIP, MaximumGroupCount),
    )?);
    if active_count == 0 || active_count > maximum_count {
        return invalid("CPU active processor group count");
    }
    let infos_offset = group_offset + offset_of!(GROUP_RELATIONSHIP, GroupInfo);
    let required = active_count
        .checked_mul(size_of::<PROCESSOR_GROUP_INFO>())
        .and_then(|size| infos_offset.checked_add(size))
        .ok_or_else(|| invalid_error("CPU group relationship size overflow"))?;
    if required > record.len() {
        return invalid("CPU group relationship bounds");
    }
    let mut actual = BTreeSet::new();
    for group in 0..active_count {
        let info = read_value::<PROCESSOR_GROUP_INFO>(
            record,
            infos_offset + group * size_of::<PROCESSOR_GROUP_INFO>(),
        )?;
        if info.ActiveProcessorCount == 0
            || info.ActiveProcessorCount > info.MaximumProcessorCount
            || info.ActiveProcessorMask.count_ones() != u32::from(info.ActiveProcessorCount)
        {
            return invalid("CPU group relationship active mask");
        }
        let group = u16::try_from(group)
            .map_err(|_| invalid_error("CPU processor group identity overflow"))?;
        let mut mask = info.ActiveProcessorMask;
        while mask != 0 {
            actual.insert(LogicalProcessorId {
                group,
                number: mask.trailing_zeros() as u8,
            });
            mask &= mask - 1;
        }
    }
    if &actual != expected {
        return invalid("CPU group relationship topology mismatch");
    }
    Ok(())
}

fn validate_member_subset(
    members: &BTreeSet<LogicalProcessorId>,
    expected: &BTreeSet<LogicalProcessorId>,
) -> Result<(), CpuDetailError> {
    if members.is_empty() || !members.is_subset(expected) {
        return invalid("CPU relationship processor membership");
    }
    Ok(())
}

fn insert_partition(
    destination: &mut BTreeSet<LogicalProcessorId>,
    members: &BTreeSet<LogicalProcessorId>,
) -> Result<(), CpuDetailError> {
    if members.iter().any(|member| !destination.insert(*member)) {
        return invalid("CPU relationship partition overlap");
    }
    Ok(())
}

fn read_value<T: Copy>(bytes: &[u8], offset: usize) -> Result<T, CpuDetailError> {
    let end = offset
        .checked_add(size_of::<T>())
        .ok_or_else(|| invalid_error("CPU variable record read overflow"))?;
    if end > bytes.len() {
        return invalid("CPU variable record read bounds");
    }
    Ok(unsafe { read_unaligned(bytes.as_ptr().add(offset).cast::<T>()) })
}

fn query_cpu_features() -> CpuFeatureInfo {
    let mut system_info = unsafe { zeroed::<SYSTEM_INFO>() };
    unsafe { GetNativeSystemInfo(&mut system_info) };
    let architecture = CpuArchitecture::from_windows(unsafe {
        system_info.Anonymous.Anonymous.wProcessorArchitecture
    });
    let mut isa_features = Vec::new();
    for (id, name) in [
        (PF_MMX_INSTRUCTIONS_AVAILABLE, "MMX"),
        (PF_XMMI_INSTRUCTIONS_AVAILABLE, "SSE"),
        (PF_XMMI64_INSTRUCTIONS_AVAILABLE, "SSE2"),
        (PF_SSE3_INSTRUCTIONS_AVAILABLE, "SSE3"),
        (PF_SSSE3_INSTRUCTIONS_AVAILABLE, "SSSE3"),
        (PF_SSE4_1_INSTRUCTIONS_AVAILABLE, "SSE4.1"),
        (PF_SSE4_2_INSTRUCTIONS_AVAILABLE, "SSE4.2"),
        (PF_AVX_INSTRUCTIONS_AVAILABLE, "AVX"),
        (PF_AVX2_INSTRUCTIONS_AVAILABLE, "AVX2"),
        (PF_AVX512F_INSTRUCTIONS_AVAILABLE, "AVX-512F"),
        (PF_XSAVE_ENABLED, "XSAVE"),
        (PF_RDTSC_INSTRUCTION_AVAILABLE, "RDTSC"),
        (PF_RDTSCP_INSTRUCTION_AVAILABLE, "RDTSCP"),
        (PF_RDRAND_INSTRUCTION_AVAILABLE, "RDRAND"),
        (PF_RDPID_INSTRUCTION_AVAILABLE, "RDPID"),
        (PF_RDWRFSGSBASE_AVAILABLE, "FSGSBASE"),
        (PF_ERMS_AVAILABLE, "ERMS"),
        (PF_NX_ENABLED, "NX"),
        (PF_PAE_ENABLED, "PAE"),
        (PF_3DNOW_INSTRUCTIONS_AVAILABLE, "3DNow!"),
        (PF_ARM_NEON_INSTRUCTIONS_AVAILABLE, "NEON"),
        (PF_ARM_DIVIDE_INSTRUCTION_AVAILABLE, "ARM Divide"),
        (PF_ARM_FMAC_INSTRUCTIONS_AVAILABLE, "ARM FMAC"),
        (PF_ARM_V8_INSTRUCTIONS_AVAILABLE, "ARMv8"),
        (PF_ARM_V8_CRYPTO_INSTRUCTIONS_AVAILABLE, "ARMv8 Crypto"),
        (PF_ARM_V8_CRC32_INSTRUCTIONS_AVAILABLE, "ARMv8 CRC32"),
        (PF_ARM_64BIT_LOADSTORE_ATOMIC, "ARM Atomic"),
        (PF_ARM_V81_ATOMIC_INSTRUCTIONS_AVAILABLE, "ARMv8.1 Atomic"),
        (PF_ARM_V82_DP_INSTRUCTIONS_AVAILABLE, "ARMv8.2 Dot Product"),
        (PF_ARM_V83_JSCVT_INSTRUCTIONS_AVAILABLE, "ARMv8.3 JSCVT"),
        (PF_ARM_V83_LRCPC_INSTRUCTIONS_AVAILABLE, "ARMv8.3 LRCPC"),
    ] {
        if unsafe { IsProcessorFeaturePresent(id) } != 0 {
            isa_features.push(name);
        }
    }
    CpuFeatureInfo {
        architecture,
        virtualization_firmware_enabled: unsafe {
            IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED)
        } != 0,
        second_level_address_translation: unsafe {
            IsProcessorFeaturePresent(PF_SECOND_LEVEL_ADDRESS_TRANSLATION)
        } != 0,
        isa_features,
    }
}
