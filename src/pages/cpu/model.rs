// +-------------------------------------------------------------------------
//
//   taskmgr-rs - CPU 诊断数据模型
//
//   文件:       src/pages/cpu/model.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! CPU 诊断页各数据源之间共享的身份、请求、快照和错误类型。
//!
//! 这些类型不拥有原生句柄；来源模块只提交完整的单来源候选结果。

use crate::infrastructure::native::{record_hresult_error, record_pdh_error, record_win32_error};
use crate::system::cpu_topology::{
    LogicalProcessorId, ProcessorTopology, ProcessorTopologyIdentity,
};
use windows_sys::Win32::Foundation::{ERROR_GEN_FAILURE, ERROR_INVALID_DATA, GetLastError};
use windows_sys::Win32::System::SystemInformation::{
    PROCESSOR_ARCHITECTURE, PROCESSOR_ARCHITECTURE_ALPHA, PROCESSOR_ARCHITECTURE_ALPHA64,
    PROCESSOR_ARCHITECTURE_AMD64, PROCESSOR_ARCHITECTURE_ARM,
    PROCESSOR_ARCHITECTURE_ARM32_ON_WIN64, PROCESSOR_ARCHITECTURE_ARM64,
    PROCESSOR_ARCHITECTURE_IA32_ON_ARM64, PROCESSOR_ARCHITECTURE_IA32_ON_WIN64,
    PROCESSOR_ARCHITECTURE_IA64, PROCESSOR_ARCHITECTURE_INTEL, PROCESSOR_ARCHITECTURE_MIPS,
    PROCESSOR_ARCHITECTURE_MSIL, PROCESSOR_ARCHITECTURE_NEUTRAL, PROCESSOR_ARCHITECTURE_PPC,
    PROCESSOR_ARCHITECTURE_SHX,
};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct CpuTopologyKey(pub(super) Vec<ProcessorTopologyIdentity>);

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
    pub(super) fn from_windows(value: PROCESSOR_ARCHITECTURE) -> Self {
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

pub(super) fn invalid<T>(context: &'static str) -> Result<T, CpuDetailError> {
    Err(invalid_error(context))
}

pub(super) fn invalid_error(context: &'static str) -> CpuDetailError {
    CpuDetailError::InvalidData { context }
}

pub(super) fn last_error_or_gen_failure() -> u32 {
    let error = unsafe { GetLastError() };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}
