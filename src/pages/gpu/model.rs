// +-------------------------------------------------------------------------
//
//   taskmgr-rs - GPU 共享数据模型
//
//   文件:       src/pages/gpu/model.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Defines stable adapter/engine identities, source snapshots, and source-specific errors.
//! This module owns no native handles and performs no sampling.

use std::sync::Arc;

use windows_sys::Win32::Foundation::ERROR_INVALID_DATA;
use windows_sys::Win32::System::Performance::{
    PDH_CSTATUS_INVALID_DATA, PDH_CSTATUS_NO_COUNTER, PDH_CSTATUS_NO_OBJECT, PDH_INVALID_PATH,
};

use crate::infrastructure::native::{
    record_hresult_error, record_ntstatus_error, record_pdh_error, record_win32_error,
};

#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AdapterLuid {
    pub(crate) high_part: i32,
    pub(crate) low_part: u32,
}

impl AdapterLuid {
    pub(super) fn from_parts(high_part: u32, low_part: u32) -> Self {
        Self {
            high_part: high_part as i32,
            low_part,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct GpuAdapterId {
    pub(crate) luid: AdapterLuid,
    pub(crate) physical_index: u32,
}

#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct GpuEngineId {
    pub(crate) adapter: GpuAdapterId,
    pub(crate) ordinal: u32,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum GpuEngineKind {
    ThreeD,
    Copy,
    VideoEncode,
    VideoDecode,
    Compute,
    Security,
    Other(String),
}

impl GpuEngineKind {
    pub(crate) fn from_counter_name(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "3d" => Self::ThreeD,
            "copy" => Self::Copy,
            "videoencode" => Self::VideoEncode,
            "videodecode" => Self::VideoDecode,
            "compute" | "computing" => Self::Compute,
            "security" => Self::Security,
            _ => Self::Other(value.to_string()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GpuSampleError {
    Pdh { context: &'static str, status: u32 },
    HResult { context: &'static str, code: i32 },
    NtStatus { context: &'static str, status: i32 },
    Win32 { context: &'static str, code: u32 },
    InvalidData { context: &'static str },
}

impl GpuSampleError {
    pub(crate) fn record(&self) {
        match *self {
            Self::Pdh { context, status } => record_pdh_error(context, status),
            Self::HResult { context, code } => record_hresult_error(context, code),
            Self::NtStatus { context, status } => record_ntstatus_error(context, status),
            Self::Win32 { context, code } => record_win32_error(context, code),
            Self::InvalidData { context } => record_win32_error(context, ERROR_INVALID_DATA),
        }
    }

    pub(crate) fn is_unsupported(&self) -> bool {
        matches!(
            self,
            Self::Pdh {
                status: PDH_CSTATUS_NO_OBJECT | PDH_CSTATUS_NO_COUNTER | PDH_INVALID_PATH,
                ..
            }
        )
    }

    pub(super) fn is_baseline_pending(&self) -> bool {
        matches!(
            self,
            Self::Pdh {
                status: PDH_CSTATUS_INVALID_DATA,
                ..
            }
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GpuDriverDetails {
    pub(crate) version: Option<String>,
    pub(crate) date: Option<String>,
    pub(crate) location: Option<String>,
    pub(crate) location_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpuAdapterInfo {
    pub(crate) id: GpuAdapterId,
    pub(crate) enumeration_index: u32,
    pub(crate) name: String,
    pub(crate) vendor_id: u32,
    pub(crate) device_id: u32,
    pub(crate) subsystem_id: u32,
    pub(crate) revision: u32,
    pub(crate) dedicated_limit_bytes: Option<u64>,
    pub(crate) shared_limit_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpuAdapterMetadata {
    pub(crate) id: GpuAdapterId,
    pub(crate) hardware_reserved_bytes: Option<u64>,
    pub(crate) driver: GpuDriverDetails,
    pub(crate) directx_feature_level: Option<String>,
    pub(crate) metadata_errors: Vec<GpuSampleError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpuEngineSample {
    pub(crate) id: GpuEngineId,
    pub(crate) kind: GpuEngineKind,
    pub(crate) utilization_percent: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpuAdapterSample {
    pub(crate) info: Arc<GpuAdapterInfo>,
    pub(crate) overall_utilization_percent: u8,
    pub(crate) engines: Vec<GpuEngineSample>,
    pub(crate) dedicated_usage_bytes: u64,
    pub(crate) shared_usage_bytes: u64,
    pub(crate) temperature_deci_c: Option<u32>,
    pub(crate) row_errors: Vec<GpuSampleError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpuInventorySnapshot {
    pub(crate) generation: u64,
    pub(crate) adapters: Vec<Arc<GpuAdapterInfo>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpuDynamicSnapshot {
    pub(crate) generation: u64,
    pub(crate) timestamp_ms: u64,
    pub(crate) adapters: Vec<GpuAdapterSample>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpuMetadataSnapshot {
    pub(crate) generation: u64,
    pub(crate) adapters: Vec<GpuAdapterMetadata>,
}

#[derive(Clone, Debug)]
pub(crate) struct GpuMetadataRequest {
    pub(crate) generation: u64,
    pub(crate) adapters: Vec<Arc<GpuAdapterInfo>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GpuCollectOutcome {
    Inventory(GpuInventorySnapshot),
    AwaitingBaseline { generation: u64 },
    Dynamic(GpuDynamicSnapshot),
}
