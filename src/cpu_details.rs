// +-------------------------------------------------------------------------
//
//   taskmgr-rs - CPU 诊断信息采集
//
//   文件:       src/cpu_details.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Collects CPU topology diagnostics, firmware identity, processor features, and PDH counters.
//!
//! The collector is constructed and used only on its owning MTA worker. Each data source is
//! committed independently and is keyed by the complete group-aware processor topology, so a
//! failed optional source cannot erase unrelated trusted values or be applied to another CPU
//! topology.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::mem::{ManuallyDrop, offset_of, size_of, zeroed};
use std::ptr::{null_mut, read_unaligned};
use std::slice;

use windows::Win32::Foundation::RPC_E_TOO_LATE;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    CoInitializeSecurity, CoSetProxyBlanket, CoUninitialize, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
    RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Variant::{VARIANT, VT_BSTR, VT_EMPTY, VT_I4, VT_NULL, VariantClear};
use windows::Win32::System::Wmi::{
    CIM_STRING, CIM_UINT16, CIM_UINT32, IWbemClassObject, IWbemLocator, IWbemServices,
    WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_INFINITE, WbemLocator,
};
use windows::core::{BSTR, PCWSTR};
use windows_sys::Win32::Foundation::{
    ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_DATA, ERROR_SUCCESS, GetLastError,
};
use windows_sys::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE,
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_FMT_LARGE, PDH_HCOUNTER, PDH_HQUERY,
    PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData,
    PdhGetFormattedCounterArrayW, PdhGetFormattedCounterValue, PdhOpenQueryW,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::SystemInformation::{
    CACHE_RELATIONSHIP, CacheData, CacheInstruction, CacheTrace, CacheUnified, GROUP_AFFINITY,
    GROUP_RELATIONSHIP, GetLogicalProcessorInformationEx, GetNativeSystemInfo,
    NUMA_NODE_RELATIONSHIP, PROCESSOR_ARCHITECTURE, PROCESSOR_ARCHITECTURE_ALPHA,
    PROCESSOR_ARCHITECTURE_ALPHA64, PROCESSOR_ARCHITECTURE_AMD64, PROCESSOR_ARCHITECTURE_ARM,
    PROCESSOR_ARCHITECTURE_ARM32_ON_WIN64, PROCESSOR_ARCHITECTURE_ARM64,
    PROCESSOR_ARCHITECTURE_IA32_ON_ARM64, PROCESSOR_ARCHITECTURE_IA32_ON_WIN64,
    PROCESSOR_ARCHITECTURE_IA64, PROCESSOR_ARCHITECTURE_INTEL, PROCESSOR_ARCHITECTURE_MIPS,
    PROCESSOR_ARCHITECTURE_MSIL, PROCESSOR_ARCHITECTURE_NEUTRAL, PROCESSOR_ARCHITECTURE_PPC,
    PROCESSOR_ARCHITECTURE_SHX, PROCESSOR_GROUP_INFO, PROCESSOR_RELATIONSHIP, RelationAll,
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

use crate::cpu_topology::{LogicalProcessorId, ProcessorTopology, ProcessorTopologyIdentity};
use crate::winutil::{
    record_hresult_error, record_pdh_error, record_startup_timing, record_win32_error, to_wide_null,
};

const FREQUENCY_COUNTER_PATH: &str = r"\Processor Information(*)\Processor Frequency";
const PERFORMANCE_COUNTER_PATH: &str = r"\Processor Information(*)\% Processor Performance";
const CONTEXT_SWITCH_COUNTER_PATH: &str = r"\System\Context Switches/sec";
const SYSTEM_CALL_COUNTER_PATH: &str = r"\System\System Calls/sec";
const PROCESSOR_QUEUE_COUNTER_PATH: &str = r"\System\Processor Queue Length";
const MAX_PDH_ARRAY_BYTES: u32 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct CpuTopologyKey(Vec<ProcessorTopologyIdentity>);

impl CpuTopologyKey {
    pub(crate) fn from_topology(topology: &ProcessorTopology) -> Option<Self> {
        topology.identity().map(Self)
    }

    pub(crate) fn logical_processors(&self) -> Vec<LogicalProcessorId> {
        self.0.iter().map(|processor| processor.id).collect()
    }

    pub(crate) fn identities(&self) -> &[ProcessorTopologyIdentity] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CpuDetailRefresh {
    Prewarm,
    Activation,
    Periodic,
    User,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CpuDetailRequest {
    pub(crate) topology_key: CpuTopologyKey,
    pub(crate) refresh: CpuDetailRefresh,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CpuDetailError {
    Pdh { context: &'static str, status: u32 },
    HResult { context: &'static str, code: i32 },
    Win32 { context: &'static str, code: u32 },
    InvalidData { context: &'static str },
}

impl CpuDetailError {
    pub(crate) fn record(&self) {
        match *self {
            Self::Pdh { context, status } => record_pdh_error(context, status),
            Self::HResult { context, code } => record_hresult_error(context, code),
            Self::Win32 { context, code } => record_win32_error(context, code),
            Self::InvalidData { context } => record_win32_error(context, ERROR_INVALID_DATA),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CpuComponentUpdate<T> {
    Unchanged,
    Success(T),
    Failed(CpuDetailError),
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CpuCacheKind {
    Unified,
    Instruction,
    Data,
    Trace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CpuCacheInfo {
    pub(crate) level: u8,
    pub(crate) kind: CpuCacheKind,
    pub(crate) instance_count: usize,
    pub(crate) bytes_per_instance: u64,
    pub(crate) total_bytes: u64,
    pub(crate) line_size: u16,
    pub(crate) associativity: Option<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CpuSupplementalTopology {
    pub(crate) package_count: usize,
    pub(crate) numa_node_count: usize,
    pub(crate) group_count: usize,
    pub(crate) die_count: Option<usize>,
    pub(crate) module_count: Option<usize>,
    pub(crate) caches: Vec<CpuCacheInfo>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CpuFirmwareProcessor {
    pub(crate) device_id: String,
    pub(crate) name: Option<String>,
    pub(crate) manufacturer: Option<String>,
    pub(crate) socket: Option<String>,
    pub(crate) processor_id: Option<String>,
    pub(crate) family: Option<u16>,
    pub(crate) level: Option<u16>,
    pub(crate) revision: Option<u16>,
    pub(crate) stepping: Option<String>,
    pub(crate) address_width: Option<u16>,
    pub(crate) data_width: Option<u16>,
    pub(crate) max_clock_mhz: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CpuArchitecture {
    X86,
    X64,
    Arm,
    Arm64,
    Ia64,
    Alpha,
    Alpha64,
    Mips,
    PowerPc,
    Shx,
    Msil,
    X86OnX64,
    Arm32OnArm64,
    X86OnArm64,
    Neutral,
    Unknown(u16),
}

impl CpuArchitecture {
    fn from_windows(value: PROCESSOR_ARCHITECTURE) -> Self {
        match value {
            PROCESSOR_ARCHITECTURE_INTEL => Self::X86,
            PROCESSOR_ARCHITECTURE_AMD64 => Self::X64,
            PROCESSOR_ARCHITECTURE_ARM => Self::Arm,
            PROCESSOR_ARCHITECTURE_ARM64 => Self::Arm64,
            PROCESSOR_ARCHITECTURE_IA64 => Self::Ia64,
            PROCESSOR_ARCHITECTURE_ALPHA => Self::Alpha,
            PROCESSOR_ARCHITECTURE_ALPHA64 => Self::Alpha64,
            PROCESSOR_ARCHITECTURE_MIPS => Self::Mips,
            PROCESSOR_ARCHITECTURE_PPC => Self::PowerPc,
            PROCESSOR_ARCHITECTURE_SHX => Self::Shx,
            PROCESSOR_ARCHITECTURE_MSIL => Self::Msil,
            PROCESSOR_ARCHITECTURE_IA32_ON_WIN64 => Self::X86OnX64,
            PROCESSOR_ARCHITECTURE_ARM32_ON_WIN64 => Self::Arm32OnArm64,
            PROCESSOR_ARCHITECTURE_IA32_ON_ARM64 => Self::X86OnArm64,
            PROCESSOR_ARCHITECTURE_NEUTRAL => Self::Neutral,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CpuFeatureInfo {
    pub(crate) architecture: CpuArchitecture,
    pub(crate) virtualization_firmware_enabled: bool,
    pub(crate) second_level_address_translation: bool,
    pub(crate) isa_features: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CpuDynamicInfo {
    pub(crate) average_frequency_mhz: u64,
    pub(crate) minimum_frequency_mhz: u64,
    pub(crate) maximum_frequency_mhz: u64,
    pub(crate) processor_queue_length: u64,
    pub(crate) context_switches_per_second: Option<u64>,
    pub(crate) system_calls_per_second: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CpuNativeSnapshot {
    pub(crate) topology_key: CpuTopologyKey,
    pub(crate) topology: CpuComponentUpdate<CpuSupplementalTopology>,
    pub(crate) features: CpuComponentUpdate<CpuFeatureInfo>,
    pub(crate) dynamic: CpuComponentUpdate<CpuDynamicInfo>,
    pub(crate) pdh_baseline_timestamp_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CpuFirmwareSnapshot {
    pub(crate) topology_key: CpuTopologyKey,
    pub(crate) firmware: CpuComponentUpdate<Vec<CpuFirmwareProcessor>>,
}

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

pub(crate) struct CpuFirmwareCollector {
    provider: Result<CpuWmiProvider, CpuDetailError>,
    topology_key: Option<CpuTopologyKey>,
    firmware_done: bool,
    firmware_failed: bool,
    startup_started_ms: u64,
    timing_recorded: bool,
}

impl CpuFirmwareCollector {
    pub(crate) fn new() -> Self {
        let startup_started_ms = unsafe { GetTickCount64() };
        let provider = CpuWmiProvider::connect();
        record_startup_timing(
            "CPU WMI initialization",
            unsafe { GetTickCount64() }.wrapping_sub(startup_started_ms),
        );
        Self {
            provider,
            topology_key: None,
            firmware_done: false,
            firmware_failed: false,
            startup_started_ms,
            timing_recorded: false,
        }
    }

    pub(crate) fn collect(&mut self, request: CpuDetailRequest) -> CpuFirmwareSnapshot {
        if self.topology_key.as_ref() != Some(&request.topology_key) {
            self.topology_key = Some(request.topology_key.clone());
            self.firmware_done = false;
            self.firmware_failed = false;
            self.timing_recorded = false;
        }
        let retry_failed = request.refresh == CpuDetailRefresh::User && self.firmware_failed;
        if retry_failed {
            self.provider = CpuWmiProvider::connect();
            self.firmware_done = false;
            self.firmware_failed = false;
        }
        let firmware = if !self.firmware_done {
            let query_started_ms = unsafe { GetTickCount64() };
            let result = self
                .provider
                .as_ref()
                .map_err(Clone::clone)
                .and_then(CpuWmiProvider::query_processors);
            if !self.timing_recorded {
                record_startup_timing(
                    "CPU WMI firmware query",
                    unsafe { GetTickCount64() }.wrapping_sub(query_started_ms),
                );
                record_startup_timing(
                    "CPU firmware completed",
                    unsafe { GetTickCount64() }.wrapping_sub(self.startup_started_ms),
                );
                self.timing_recorded = true;
            }
            self.firmware_done = true;
            match result {
                Ok(value) => {
                    self.firmware_failed = false;
                    CpuComponentUpdate::Success(value)
                }
                Err(error) => {
                    self.firmware_failed = true;
                    CpuComponentUpdate::Failed(error)
                }
            }
        } else {
            CpuComponentUpdate::Unchanged
        };
        CpuFirmwareSnapshot {
            topology_key: request.topology_key,
            firmware,
        }
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

fn parse_relation_all(
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

struct CpuPdhQuery {
    query: PDH_HQUERY,
    frequency: PDH_HCOUNTER,
    performance: PDH_HCOUNTER,
    context_switches: PDH_HCOUNTER,
    system_calls: PDH_HCOUNTER,
    processor_queue: PDH_HCOUNTER,
    expected_processor_count: usize,
    processor_indices: Option<HashMap<PdhProcessorInstance, usize>>,
    nominal_values: Vec<i64>,
    performance_values: Vec<f64>,
    nominal_seen: Vec<u32>,
    performance_seen: Vec<u32>,
    sample_generation: u32,
    sample_baseline_ready: bool,
}

enum CpuPdhCollectOutcome {
    Primed(u64),
    Sample(CpuDynamicInfo),
}

impl CpuPdhQuery {
    fn new(expected_processors: Vec<LogicalProcessorId>) -> Result<Self, CpuDetailError> {
        if expected_processors.is_empty()
            || expected_processors
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len()
                != expected_processors.len()
        {
            return invalid("CPU PDH expected processor identities");
        }
        let mut query = null_mut();
        let status = unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) };
        if status != ERROR_SUCCESS {
            return Err(CpuDetailError::Pdh {
                context: "PdhOpenQueryW for CPU diagnostics",
                status,
            });
        }
        let mut result = Self {
            query,
            frequency: null_mut(),
            performance: null_mut(),
            context_switches: null_mut(),
            system_calls: null_mut(),
            processor_queue: null_mut(),
            expected_processor_count: expected_processors.len(),
            processor_indices: None,
            nominal_values: Vec::new(),
            performance_values: Vec::new(),
            nominal_seen: Vec::new(),
            performance_seen: Vec::new(),
            sample_generation: 0,
            sample_baseline_ready: false,
        };
        result.frequency = result.add_counter(FREQUENCY_COUNTER_PATH)?;
        result.performance = result.add_counter(PERFORMANCE_COUNTER_PATH)?;
        result.context_switches = result.add_counter(CONTEXT_SWITCH_COUNTER_PATH)?;
        result.system_calls = result.add_counter(SYSTEM_CALL_COUNTER_PATH)?;
        result.processor_queue = result.add_counter(PROCESSOR_QUEUE_COUNTER_PATH)?;
        Ok(result)
    }

    fn add_counter(&self, path: &str) -> Result<PDH_HCOUNTER, CpuDetailError> {
        let path = to_wide_null(path);
        let mut counter = null_mut();
        let status = unsafe { PdhAddEnglishCounterW(self.query, path.as_ptr(), 0, &mut counter) };
        if status != ERROR_SUCCESS {
            return Err(CpuDetailError::Pdh {
                context: "PdhAddEnglishCounterW for CPU diagnostics",
                status,
            });
        }
        Ok(counter)
    }

    fn reset_sample_baseline(&mut self) {
        self.sample_baseline_ready = false;
    }

    fn collect(&mut self) -> Result<CpuPdhCollectOutcome, CpuDetailError> {
        let status = unsafe { PdhCollectQueryData(self.query) };
        if status != ERROR_SUCCESS {
            return Err(CpuDetailError::Pdh {
                context: "PdhCollectQueryData for CPU diagnostics",
                status,
            });
        }
        if !self.sample_baseline_ready {
            self.sample_baseline_ready = true;
            return Ok(CpuPdhCollectOutcome::Primed(unsafe { GetTickCount64() }));
        }
        let frequencies = query_counter_array(self.frequency, PDH_FMT_LARGE, |value| unsafe {
            value.Anonymous.largeValue
        })?;
        let performance = query_counter_array(self.performance, PDH_FMT_DOUBLE, |value| unsafe {
            value.Anonymous.doubleValue
        })?;
        let (average_frequency_mhz, minimum_frequency_mhz, maximum_frequency_mhz) =
            self.validate_frequencies(&frequencies, &performance)?;
        let processor_queue_length = query_single_counter(self.processor_queue)?;
        let context_switches_per_second = Some(query_single_counter(self.context_switches)?);
        let system_calls_per_second = Some(query_single_counter(self.system_calls)?);
        Ok(CpuPdhCollectOutcome::Sample(CpuDynamicInfo {
            average_frequency_mhz,
            minimum_frequency_mhz,
            maximum_frequency_mhz,
            processor_queue_length,
            context_switches_per_second,
            system_calls_per_second,
        }))
    }

    fn validate_frequencies(
        &mut self,
        nominal: &[PdhArrayValue<i64>],
        performance: &[PdhArrayValue<f64>],
    ) -> Result<(u64, u64, u64), CpuDetailError> {
        if self.processor_indices.is_none() {
            self.processor_indices = Some(build_processor_indices(
                nominal,
                self.expected_processor_count,
            )?);
            let count = self.expected_processor_count;
            self.nominal_values.resize(count, 0);
            self.performance_values.resize(count, 0.0);
            self.nominal_seen.resize(count, 0);
            self.performance_seen.resize(count, 0);
        }
        self.sample_generation = self.sample_generation.wrapping_add(1);
        if self.sample_generation == 0 {
            self.nominal_seen.fill(0);
            self.performance_seen.fill(0);
            self.sample_generation = 1;
        }
        let indices = self
            .processor_indices
            .as_ref()
            .ok_or_else(|| invalid_error("CPU processor index state"))?;
        fill_processor_values(
            nominal,
            indices,
            &mut self.nominal_values,
            &mut self.nominal_seen,
            self.sample_generation,
            |value| value > 0,
            ProcessorValueErrors::NOMINAL,
        )?;
        fill_processor_values(
            performance,
            indices,
            &mut self.performance_values,
            &mut self.performance_seen,
            self.sample_generation,
            |value| value.is_finite() && value >= 0.0,
            ProcessorValueErrors::PERFORMANCE,
        )?;
        summarize_frequencies(&self.nominal_values, &self.performance_values)
    }
}

impl Drop for CpuPdhQuery {
    fn drop(&mut self) {
        if !self.query.is_null() {
            let status = unsafe { PdhCloseQuery(self.query) };
            if status != ERROR_SUCCESS {
                record_pdh_error("PdhCloseQuery for CPU diagnostics", status);
            }
            self.query = null_mut();
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct PdhArrayValue<T> {
    instance: String,
    value: T,
}

fn query_counter_array<T>(
    counter: PDH_HCOUNTER,
    format: u32,
    extract: impl Fn(&PDH_FMT_COUNTERVALUE) -> T,
) -> Result<Vec<PdhArrayValue<T>>, CpuDetailError> {
    unsafe {
        let mut byte_count = 0u32;
        let mut item_count = 0u32;
        let status = PdhGetFormattedCounterArrayW(
            counter,
            format,
            &mut byte_count,
            &mut item_count,
            null_mut(),
        );
        if status != PDH_MORE_DATA {
            return Err(CpuDetailError::Pdh {
                context: "PdhGetFormattedCounterArrayW CPU size query",
                status,
            });
        }
        if byte_count == 0 || byte_count > MAX_PDH_ARRAY_BYTES {
            return invalid("CPU PDH wildcard buffer size");
        }
        let mut storage = vec![0usize; (byte_count as usize).div_ceil(size_of::<usize>())];
        let status = PdhGetFormattedCounterArrayW(
            counter,
            format,
            &mut byte_count,
            &mut item_count,
            storage.as_mut_ptr().cast(),
        );
        if status != ERROR_SUCCESS {
            return Err(CpuDetailError::Pdh {
                context: "PdhGetFormattedCounterArrayW CPU data query",
                status,
            });
        }
        let used_bytes = byte_count as usize;
        if used_bytes > storage.len() * size_of::<usize>()
            || (item_count as usize)
                .checked_mul(size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>())
                .is_none_or(|size| size > used_bytes)
        {
            return invalid("CPU PDH wildcard item bounds");
        }
        let base = storage.as_ptr() as usize;
        let end = base
            .checked_add(used_bytes)
            .ok_or_else(|| invalid_error("CPU PDH wildcard address overflow"))?;
        let items = slice::from_raw_parts(
            storage.as_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
            item_count as usize,
        );
        let mut values = Vec::with_capacity(items.len());
        for item in items {
            validate_pdh_status(item.FmtValue.CStatus)?;
            values.push(PdhArrayValue {
                instance: read_bounded_wide(item.szName, base, end)?,
                value: extract(&item.FmtValue),
            });
        }
        Ok(values)
    }
}

fn query_single_counter(counter: PDH_HCOUNTER) -> Result<u64, CpuDetailError> {
    let mut value = unsafe { zeroed::<PDH_FMT_COUNTERVALUE>() };
    let status =
        unsafe { PdhGetFormattedCounterValue(counter, PDH_FMT_LARGE, null_mut(), &mut value) };
    if status != ERROR_SUCCESS {
        return Err(CpuDetailError::Pdh {
            context: "PdhGetFormattedCounterValue for CPU diagnostics",
            status,
        });
    }
    validate_pdh_status(value.CStatus)?;
    let value = unsafe { value.Anonymous.largeValue };
    u64::try_from(value).map_err(|_| invalid_error("CPU PDH negative counter value"))
}

fn validate_pdh_status(status: u32) -> Result<(), CpuDetailError> {
    if matches!(status, PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA) {
        Ok(())
    } else {
        Err(CpuDetailError::Pdh {
            context: "CPU PDH formatted counter status",
            status,
        })
    }
}

#[cfg(test)]
fn validate_frequencies(
    nominal_values: &[PdhArrayValue<i64>],
    performance_values: &[PdhArrayValue<f64>],
    expected: &[LogicalProcessorId],
) -> Result<(u64, u64, u64), CpuDetailError> {
    let expected_count = expected.iter().copied().collect::<BTreeSet<_>>().len();
    if expected_count == 0 || expected_count != expected.len() {
        return invalid("CPU expected processor identities");
    }
    let indices = build_processor_indices(nominal_values, expected_count)?;
    let mut nominal = vec![0i64; expected_count];
    let mut performance = vec![0.0f64; expected_count];
    let mut nominal_seen = vec![0u32; expected_count];
    let mut performance_seen = vec![0u32; expected_count];
    fill_processor_values(
        nominal_values,
        &indices,
        &mut nominal,
        &mut nominal_seen,
        1,
        |value| value > 0,
        ProcessorValueErrors::NOMINAL,
    )?;
    fill_processor_values(
        performance_values,
        &indices,
        &mut performance,
        &mut performance_seen,
        1,
        |value| value.is_finite() && value >= 0.0,
        ProcessorValueErrors::PERFORMANCE,
    )?;
    summarize_frequencies(&nominal, &performance)
}

fn summarize_frequencies(
    nominal: &[i64],
    performance: &[f64],
) -> Result<(u64, u64, u64), CpuDetailError> {
    if nominal.is_empty() || nominal.len() != performance.len() {
        return invalid("CPU frequency and performance instance mismatch");
    }
    let mut sum = 0u128;
    let mut minimum = u64::MAX;
    let mut maximum = 0u64;
    for (&nominal_mhz, &performance_percent) in nominal.iter().zip(performance) {
        // Windows reports nominal MHz separately from relative performance, which may exceed
        // 100 percent while the processor is boosting.
        let frequency = effective_frequency_mhz(nominal_mhz, performance_percent)?;
        sum = sum
            .checked_add(u128::from(frequency))
            .ok_or_else(|| invalid_error("CPU frequency sum overflow"))?;
        minimum = minimum.min(frequency);
        maximum = maximum.max(frequency);
    }
    let count = nominal.len() as u128;
    let average = sum
        .checked_add(count / 2)
        .and_then(|value| value.checked_div(count))
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| invalid_error("CPU frequency average overflow"))?;
    Ok((average, minimum, maximum))
}

fn build_processor_indices(
    values: &[PdhArrayValue<i64>],
    expected_count: usize,
) -> Result<HashMap<PdhProcessorInstance, usize>, CpuDetailError> {
    if expected_count == 0 {
        return invalid("CPU expected processor instance count");
    }
    let mut indices = HashMap::with_capacity(expected_count);
    for item in values {
        let Some(id) = parse_processor_instance(&item.instance)? else {
            continue;
        };
        if item.value <= 0 {
            return invalid("CPU nominal frequency instance or value");
        }
        let next_index = indices.len();
        if indices.insert(id, next_index).is_some() {
            return invalid("CPU duplicate nominal frequency instance");
        }
    }
    if indices.len() != expected_count {
        return invalid("CPU nominal frequency instance completeness");
    }
    Ok(indices)
}

#[derive(Clone, Copy)]
struct ProcessorValueErrors {
    invalid: &'static str,
    duplicate: &'static str,
    incomplete: &'static str,
}

impl ProcessorValueErrors {
    const NOMINAL: Self = Self {
        invalid: "CPU nominal frequency instance or value",
        duplicate: "CPU duplicate nominal frequency instance",
        incomplete: "CPU nominal frequency instance completeness",
    };
    const PERFORMANCE: Self = Self {
        invalid: "CPU performance instance or value",
        duplicate: "CPU duplicate performance instance",
        incomplete: "CPU performance instance completeness",
    };
}

fn fill_processor_values<T: Copy>(
    values: &[PdhArrayValue<T>],
    indices: &HashMap<PdhProcessorInstance, usize>,
    output: &mut [T],
    seen: &mut [u32],
    generation: u32,
    is_valid: impl Fn(T) -> bool,
    errors: ProcessorValueErrors,
) -> Result<(), CpuDetailError> {
    if generation == 0 || output.len() != indices.len() || seen.len() != indices.len() {
        return invalid("CPU processor value buffer shape");
    }
    let mut mapped_count = 0usize;
    for item in values {
        let Some(id) = parse_processor_instance(&item.instance)? else {
            continue;
        };
        if !is_valid(item.value) {
            return invalid(errors.invalid);
        }
        let index = *indices
            .get(&id)
            .ok_or_else(|| invalid_error("CPU frequency and performance instance mismatch"))?;
        if seen[index] == generation {
            return invalid(errors.duplicate);
        }
        seen[index] = generation;
        output[index] = item.value;
        mapped_count = mapped_count
            .checked_add(1)
            .ok_or_else(|| invalid_error("CPU processor value count overflow"))?;
    }
    if mapped_count != indices.len() {
        return invalid(errors.incomplete);
    }
    Ok(())
}

fn effective_frequency_mhz(
    nominal_mhz: i64,
    performance_percent: f64,
) -> Result<u64, CpuDetailError> {
    let frequency = nominal_mhz as f64 * performance_percent / 100.0;
    if !frequency.is_finite() || frequency < 0.0 || frequency > u64::MAX as f64 {
        return invalid("CPU effective frequency overflow");
    }
    Ok(frequency.round() as u64)
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct PdhProcessorInstance {
    // Processor Information instances are documented as NUMA node and node-local index.
    numa_node: u32,
    numa_index: u32,
}

fn parse_processor_instance(value: &str) -> Result<Option<PdhProcessorInstance>, CpuDetailError> {
    if value.eq_ignore_ascii_case("_Total") {
        return Ok(None);
    }
    let mut fields = value.split(',');
    let numa_node = fields
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| invalid_error("CPU processor NUMA node instance"))?;
    let numa_index = fields
        .next()
        .ok_or_else(|| invalid_error("CPU processor NUMA index instance"))?;
    if fields.next().is_some() {
        return invalid("CPU processor instance field count");
    }
    if numa_index.eq_ignore_ascii_case("_Total") {
        return Ok(None);
    }
    let numa_index = numa_index
        .parse::<u32>()
        .map_err(|_| invalid_error("CPU processor NUMA index instance"))?;
    Ok(Some(PdhProcessorInstance {
        numa_node,
        numa_index,
    }))
}

unsafe fn read_bounded_wide(
    pointer: *const u16,
    base: usize,
    end: usize,
) -> Result<String, CpuDetailError> {
    let address = pointer as usize;
    if pointer.is_null()
        || address < base
        || address >= end
        || !address.is_multiple_of(size_of::<u16>())
    {
        return invalid("CPU PDH instance name pointer");
    }
    let max_units = (end - address) / size_of::<u16>();
    let units = unsafe { slice::from_raw_parts(pointer, max_units) };
    let length = units
        .iter()
        .position(|unit| *unit == 0)
        .ok_or_else(|| invalid_error("CPU PDH instance name terminator"))?;
    String::from_utf16(&units[..length])
        .map_err(|_| invalid_error("CPU PDH instance name encoding"))
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> Result<Self, CpuDetailError> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|error| CpuDetailError::HResult {
                context: "CoInitializeEx for CPU WMI worker",
                code: error.code().0,
            })?;
        match unsafe {
            CoInitializeSecurity(
                None,
                -1,
                None,
                None,
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
                None,
            )
        } {
            Ok(()) => Ok(Self),
            Err(error) if error.code() == RPC_E_TOO_LATE => Ok(Self),
            Err(error) => {
                unsafe { CoUninitialize() };
                Err(CpuDetailError::HResult {
                    context: "CoInitializeSecurity for CPU WMI worker",
                    code: error.code().0,
                })
            }
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct CpuWmiProvider {
    services: IWbemServices,
    _apartment: ComApartment,
}

impl CpuWmiProvider {
    fn connect() -> Result<Self, CpuDetailError> {
        let apartment = ComApartment::initialize()?;
        let locator: IWbemLocator =
            unsafe { CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER) }.map_err(
                |error| CpuDetailError::HResult {
                    context: "CoCreateInstance IWbemLocator",
                    code: error.code().0,
                },
            )?;
        let empty = BSTR::new();
        let services = unsafe {
            locator.ConnectServer(
                &BSTR::from("ROOT\\CIMV2"),
                &empty,
                &empty,
                &empty,
                0,
                &empty,
                None,
            )
        }
        .map_err(|error| CpuDetailError::HResult {
            context: "IWbemLocator::ConnectServer for CPU details",
            code: error.code().0,
        })?;
        unsafe {
            CoSetProxyBlanket(
                &services,
                RPC_C_AUTHN_WINNT,
                RPC_C_AUTHZ_NONE,
                PCWSTR::null(),
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
            )
        }
        .map_err(|error| CpuDetailError::HResult {
            context: "CoSetProxyBlanket for CPU WMI service",
            code: error.code().0,
        })?;
        Ok(Self {
            services,
            _apartment: apartment,
        })
    }

    fn query_processors(&self) -> Result<Vec<CpuFirmwareProcessor>, CpuDetailError> {
        let query = BSTR::from(
            "SELECT DeviceID, Name, Manufacturer, SocketDesignation, ProcessorId, Family, Level, \
         Revision, Stepping, AddressWidth, DataWidth, MaxClockSpeed FROM Win32_Processor",
        );
        let enumerator = unsafe {
            self.services.ExecQuery(
                &BSTR::from("WQL"),
                &query,
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
            )
        }
        .map_err(|error| CpuDetailError::HResult {
            context: "IWbemServices::ExecQuery Win32_Processor",
            code: error.code().0,
        })?;

        let mut processors = Vec::new();
        loop {
            let mut objects: [Option<IWbemClassObject>; 1] = [None];
            let mut returned = 0u32;
            let result = unsafe { enumerator.Next(WBEM_INFINITE, &mut objects, &mut returned) };
            if result.is_err() {
                return Err(CpuDetailError::HResult {
                    context: "IEnumWbemClassObject::Next Win32_Processor",
                    code: result.0,
                });
            }
            if returned == 0 {
                break;
            }
            if returned != 1 {
                return invalid("Win32_Processor enumerator returned count");
            }
            let object = objects[0]
                .take()
                .ok_or_else(|| invalid_error("Win32_Processor null object"))?;
            let device_id = get_wmi_string(&object, "DeviceID")?
                .filter(|value| !value.is_empty())
                .ok_or_else(|| invalid_error("Win32_Processor DeviceID"))?;
            processors.push(CpuFirmwareProcessor {
                device_id,
                name: get_wmi_string(&object, "Name")?,
                manufacturer: get_wmi_string(&object, "Manufacturer")?,
                socket: get_wmi_string(&object, "SocketDesignation")?,
                processor_id: get_wmi_string(&object, "ProcessorId")?,
                family: get_wmi_u16(&object, "Family")?,
                level: get_wmi_u16(&object, "Level")?,
                revision: get_wmi_u16(&object, "Revision")?,
                stepping: get_wmi_string(&object, "Stepping")?,
                address_width: get_wmi_u16(&object, "AddressWidth")?,
                data_width: get_wmi_u16(&object, "DataWidth")?,
                max_clock_mhz: get_wmi_u32(&object, "MaxClockSpeed")?,
            });
        }
        if processors.is_empty() {
            return invalid("Win32_Processor empty result");
        }
        processors.sort_by(|left, right| left.device_id.cmp(&right.device_id));
        if processors
            .windows(2)
            .any(|pair| pair[0].device_id == pair[1].device_id)
        {
            return invalid("Win32_Processor duplicate DeviceID");
        }
        Ok(processors)
    }
}

struct OwnedVariant(VARIANT);

impl OwnedVariant {
    fn new() -> Self {
        Self(VARIANT::default())
    }

    fn as_mut_ptr(&mut self) -> *mut VARIANT {
        &mut self.0
    }

    fn value(&self) -> &windows::Win32::System::Variant::VARIANT_0_0 {
        unsafe { &self.0.Anonymous.Anonymous }
    }
}

impl Drop for OwnedVariant {
    fn drop(&mut self) {
        if let Err(error) = unsafe { VariantClear(&mut self.0) } {
            record_hresult_error("VariantClear for CPU WMI property", error.code().0);
        }
    }
}

struct WmiProperty {
    value: OwnedVariant,
    cim_type: i32,
}

fn get_wmi_property(object: &IWbemClassObject, name: &str) -> Result<WmiProperty, CpuDetailError> {
    let name = to_wide_null(name);
    let mut value = OwnedVariant::new();
    let mut cim_type = 0;
    unsafe {
        object.Get(
            PCWSTR(name.as_ptr()),
            0,
            value.as_mut_ptr(),
            Some(&mut cim_type),
            None,
        )
    }
    .map_err(|error| CpuDetailError::HResult {
        context: "IWbemClassObject::Get CPU property",
        code: error.code().0,
    })?;
    Ok(WmiProperty { value, cim_type })
}

fn get_wmi_string(object: &IWbemClassObject, name: &str) -> Result<Option<String>, CpuDetailError> {
    let property = get_wmi_property(object, name)?;
    if property.cim_type != CIM_STRING.0 {
        return invalid("Win32_Processor string CIM type");
    }
    let inner = property.value.value();
    match inner.vt {
        VT_EMPTY | VT_NULL => Ok(None),
        VT_BSTR => {
            let bstr: &ManuallyDrop<BSTR> = unsafe { &inner.Anonymous.bstrVal };
            // `VT_BSTR` proves the active union member; `ManuallyDrop<T>` has the same layout as
            // `T`, while `VariantClear` remains the sole owner responsible for releasing it.
            let bstr = unsafe { &*(bstr as *const ManuallyDrop<BSTR>).cast::<BSTR>() };
            let string = String::from_utf16(bstr)
                .map_err(|_| invalid_error("Win32_Processor string encoding"))?;
            let trimmed = string.trim();
            Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
        }
        _ => invalid("Win32_Processor string VARIANT type"),
    }
}

fn get_wmi_u16(object: &IWbemClassObject, name: &str) -> Result<Option<u16>, CpuDetailError> {
    let property = get_wmi_property(object, name)?;
    if property.cim_type != CIM_UINT16.0 {
        return invalid("Win32_Processor u16 CIM type");
    }
    let inner = property.value.value();
    match inner.vt {
        VT_EMPTY | VT_NULL => Ok(None),
        VT_I4 => wmi_i4_to_u16(unsafe { inner.Anonymous.lVal }).map(Some),
        _ => invalid("Win32_Processor u16 VARIANT type"),
    }
}

fn get_wmi_u32(object: &IWbemClassObject, name: &str) -> Result<Option<u32>, CpuDetailError> {
    let property = get_wmi_property(object, name)?;
    if property.cim_type != CIM_UINT32.0 {
        return invalid("Win32_Processor u32 CIM type");
    }
    let inner = property.value.value();
    match inner.vt {
        VT_EMPTY | VT_NULL => Ok(None),
        VT_I4 => Ok(Some(wmi_i4_to_u32(unsafe { inner.Anonymous.lVal }))),
        _ => invalid("Win32_Processor u32 VARIANT type"),
    }
}

fn wmi_i4_to_u16(value: i32) -> Result<u16, CpuDetailError> {
    u16::try_from(value).map_err(|_| invalid_error("Win32_Processor u16 property overflow"))
}

fn wmi_i4_to_u32(value: i32) -> u32 {
    u32::from_ne_bytes(value.to_ne_bytes())
}

fn invalid<T>(context: &'static str) -> Result<T, CpuDetailError> {
    Err(invalid_error(context))
}

fn invalid_error(context: &'static str) -> CpuDetailError {
    CpuDetailError::InvalidData { context }
}

fn last_error_or_gen_failure() -> u32 {
    let error = unsafe { GetLastError() };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu_topology::{CoreClass, ProcessorTopologyIdentity};

    fn id(group: u16, number: u8) -> LogicalProcessorId {
        LogicalProcessorId { group, number }
    }

    fn topology_key() -> CpuTopologyKey {
        CpuTopologyKey(vec![
            ProcessorTopologyIdentity {
                id: id(0, 0),
                physical_core_index: 0,
                smt_index: Some(0),
                class: CoreClass::Uniform,
            },
            ProcessorTopologyIdentity {
                id: id(0, 1),
                physical_core_index: 0,
                smt_index: Some(1),
                class: CoreClass::Uniform,
            },
        ])
    }

    fn affinity(mask: usize) -> GROUP_AFFINITY {
        GROUP_AFFINITY {
            Mask: mask,
            Group: 0,
            Reserved: [0; 3],
        }
    }

    fn processor_record(relationship: i32, mask: usize) -> Vec<u8> {
        let mut processor = PROCESSOR_RELATIONSHIP {
            GroupCount: 1,
            ..Default::default()
        };
        processor.GroupMask[0] = affinity(mask);
        let mut record = SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX {
            Relationship: relationship,
            Size: size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() as u32,
            ..Default::default()
        };
        record.Anonymous.Processor = processor;
        record_bytes(&record)
    }

    fn numa_record(mask: usize) -> Vec<u8> {
        let mut numa = NUMA_NODE_RELATIONSHIP {
            NodeNumber: 0,
            GroupCount: 1,
            ..Default::default()
        };
        numa.Anonymous.GroupMask = affinity(mask);
        let mut record = SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX {
            Relationship: RelationNumaNodeEx,
            Size: size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() as u32,
            ..Default::default()
        };
        record.Anonymous.NumaNode = numa;
        record_bytes(&record)
    }

    fn group_record(mask: usize) -> Vec<u8> {
        let mut group = GROUP_RELATIONSHIP {
            MaximumGroupCount: 1,
            ActiveGroupCount: 1,
            ..Default::default()
        };
        group.GroupInfo[0].MaximumProcessorCount = usize::BITS as u8;
        group.GroupInfo[0].ActiveProcessorCount = mask.count_ones() as u8;
        group.GroupInfo[0].ActiveProcessorMask = mask;
        let mut record = SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX {
            Relationship: RelationGroup,
            Size: size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() as u32,
            ..Default::default()
        };
        record.Anonymous.Group = group;
        record_bytes(&record)
    }

    fn cache_record(mask: usize) -> Vec<u8> {
        let mut cache = CACHE_RELATIONSHIP {
            Level: 2,
            Associativity: 8,
            LineSize: 64,
            CacheSize: 1_048_576,
            Type: CacheUnified,
            GroupCount: 1,
            ..Default::default()
        };
        cache.Anonymous.GroupMask = affinity(mask);
        let mut record = SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX {
            Relationship: RelationCache,
            Size: size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() as u32,
            ..Default::default()
        };
        record.Anonymous.Cache = cache;
        record_bytes(&record)
    }

    fn record_bytes(record: &SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX) -> Vec<u8> {
        unsafe {
            slice::from_raw_parts(
                (record as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX).cast::<u8>(),
                size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(),
            )
            .to_vec()
        }
    }

    fn valid_relation_all() -> Vec<u8> {
        [
            processor_record(RelationProcessorCore, 0b11),
            processor_record(RelationProcessorPackage, 0b11),
            numa_record(0b11),
            group_record(0b11),
            cache_record(0b11),
        ]
        .concat()
    }

    #[test]
    fn processor_frequency_instances_follow_the_documented_numa_format() {
        assert_eq!(
            parse_processor_instance("0,31").unwrap(),
            Some(PdhProcessorInstance {
                numa_node: 0,
                numa_index: 31
            })
        );
        assert_eq!(
            parse_processor_instance("2,7").unwrap(),
            Some(PdhProcessorInstance {
                numa_node: 2,
                numa_index: 7
            })
        );
        assert_eq!(parse_processor_instance("_Total").unwrap(), None);
        assert_eq!(parse_processor_instance("0,_total").unwrap(), None);
        assert!(parse_processor_instance("31").is_err());
        assert!(parse_processor_instance("0,1,2").is_err());
        assert!(parse_processor_instance("-1,0").is_err());
        assert!(parse_processor_instance("group,_Total").is_err());
    }

    #[test]
    fn frequency_set_requires_exact_unique_processor_membership() {
        let expected = [id(0, 0), id(0, 1)];
        let nominal = [
            PdhArrayValue {
                instance: "0,0".into(),
                value: 3_000,
            },
            PdhArrayValue {
                instance: "0,1".into(),
                value: 3_200,
            },
            PdhArrayValue {
                instance: "_Total".into(),
                value: 3_100,
            },
        ];
        let performance = [
            PdhArrayValue {
                instance: "0,0".into(),
                value: 150.0,
            },
            PdhArrayValue {
                instance: "0,1".into(),
                value: 50.0,
            },
            PdhArrayValue {
                instance: "_Total".into(),
                value: 100.0,
            },
        ];
        assert_eq!(
            validate_frequencies(&nominal, &performance, &expected).unwrap(),
            (3_050, 1_600, 4_500)
        );

        let duplicate = [nominal[0].clone(), nominal[0].clone()];
        assert!(validate_frequencies(&duplicate, &performance, &expected).is_err());
        assert!(validate_frequencies(&nominal[..1], &performance, &expected).is_err());
        assert!(validate_frequencies(&nominal, &performance[..1], &expected).is_err());

        let mut mismatched = performance.clone();
        mismatched[1].instance = "1,0".into();
        assert!(validate_frequencies(&nominal, &mismatched, &expected).is_err());
    }

    #[test]
    fn effective_frequency_preserves_boost_and_rejects_invalid_performance() {
        assert_eq!(effective_frequency_mhz(2_401, 177.77).unwrap(), 4_268);
        assert!(effective_frequency_mhz(2_401, -1.0).is_err());
        assert!(effective_frequency_mhz(2_401, f64::NAN).is_err());
    }

    #[test]
    fn architecture_mapping_keeps_unknown_values_explicit() {
        assert_eq!(
            CpuArchitecture::from_windows(PROCESSOR_ARCHITECTURE_AMD64),
            CpuArchitecture::X64
        );
        assert_eq!(
            CpuArchitecture::from_windows(1234),
            CpuArchitecture::Unknown(1234)
        );
    }

    #[test]
    fn wmi_unsigned_integer_mapping_matches_automation_contract() {
        assert_eq!(wmi_i4_to_u16(0).unwrap(), 0);
        assert_eq!(wmi_i4_to_u16(i32::from(u16::MAX)).unwrap(), u16::MAX);
        assert!(wmi_i4_to_u16(-1).is_err());
        assert!(wmi_i4_to_u16(i32::from(u16::MAX) + 1).is_err());

        assert_eq!(wmi_i4_to_u32(0), 0);
        assert_eq!(wmi_i4_to_u32(i32::MAX), i32::MAX as u32);
        assert_eq!(wmi_i4_to_u32(i32::MIN), 0x8000_0000);
        assert_eq!(wmi_i4_to_u32(-1), u32::MAX);
    }

    #[test]
    fn relation_all_accepts_complete_group_aware_records() {
        let topology = parse_relation_all(&valid_relation_all(), &topology_key()).unwrap();
        assert_eq!(topology.package_count, 1);
        assert_eq!(topology.numa_node_count, 1);
        assert_eq!(topology.group_count, 1);
        assert_eq!(topology.caches.len(), 1);
        assert_eq!(topology.caches[0].instance_count, 1);
        assert_eq!(topology.caches[0].total_bytes, 1_048_576);
    }

    #[test]
    fn relation_all_rejects_truncation_and_invalid_record_sizes() {
        let mut truncated = valid_relation_all();
        truncated.pop();
        assert!(parse_relation_all(&truncated, &topology_key()).is_err());

        let mut invalid_size = valid_relation_all();
        invalid_size[4..8].copy_from_slice(&4u32.to_ne_bytes());
        assert!(parse_relation_all(&invalid_size, &topology_key()).is_err());
    }

    #[test]
    fn relation_all_rejects_duplicate_missing_and_out_of_range_members() {
        let mut duplicate = valid_relation_all();
        duplicate.extend(processor_record(RelationProcessorCore, 0b11));
        assert!(parse_relation_all(&duplicate, &topology_key()).is_err());

        let missing = [
            processor_record(RelationProcessorCore, 0b11),
            processor_record(RelationProcessorPackage, 0b01),
            numa_record(0b11),
            group_record(0b11),
        ]
        .concat();
        assert!(parse_relation_all(&missing, &topology_key()).is_err());

        let out_of_range = [
            processor_record(RelationProcessorCore, 0b111),
            processor_record(RelationProcessorPackage, 0b11),
            numa_record(0b11),
            group_record(0b11),
        ]
        .concat();
        assert!(parse_relation_all(&out_of_range, &topology_key()).is_err());
    }

    #[test]
    #[ignore = "requires live Windows topology, WMI, and PDH services"]
    fn live_cpu_detail_sources_return_one_coherent_topology() {
        let processor_count = unsafe {
            windows_sys::Win32::System::Threading::GetActiveProcessorCount(
                windows_sys::Win32::System::Threading::ALL_PROCESSOR_GROUPS,
            )
        };
        assert_ne!(processor_count, 0);
        assert_ne!(processor_count, u32::MAX);
        let topology = crate::cpu_topology::query_processor_topology(processor_count as usize);
        let key = CpuTopologyKey::from_topology(&topology)
            .expect("live processor topology should be complete");
        let mut native_collector = CpuNativeCollector::new();
        let native = native_collector.collect(CpuDetailRequest {
            topology_key: key.clone(),
            refresh: CpuDetailRefresh::Prewarm,
        });
        assert_eq!(native.topology_key, key);
        assert!(
            matches!(&native.topology, CpuComponentUpdate::Success(_)),
            "{:?}",
            native.topology
        );
        assert!(
            matches!(&native.features, CpuComponentUpdate::Success(_)),
            "{:?}",
            native.features
        );
        assert!(
            matches!(&native.dynamic, CpuComponentUpdate::Unchanged),
            "{:?}",
            native.dynamic
        );
        assert!(native.pdh_baseline_timestamp_ms.is_some());

        let mut firmware_collector = CpuFirmwareCollector::new();
        let firmware = firmware_collector.collect(CpuDetailRequest {
            topology_key: key.clone(),
            refresh: CpuDetailRefresh::Prewarm,
        });
        assert_eq!(firmware.topology_key, key);
        assert!(
            matches!(&firmware.firmware, CpuComponentUpdate::Success(_)),
            "{:?}",
            firmware.firmware
        );

        std::thread::sleep(std::time::Duration::from_secs(1));
        let native = native_collector.collect(CpuDetailRequest {
            topology_key: key,
            refresh: CpuDetailRefresh::Periodic,
        });
        assert!(
            matches!(&native.dynamic, CpuComponentUpdate::Success(_)),
            "{:?}",
            native.dynamic
        );
    }
}
