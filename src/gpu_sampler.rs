// +-------------------------------------------------------------------------
//
//   taskmgr-rs - GPU 拓扑与性能采样
//
//   文件:       src/gpu_sampler.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Enumerates hardware display adapters with DXGI and samples their public PDH GPU counters.
//!
//! Adapter and engine identities always include the DXGI LUID. The sampling worker submits a
//! minimal DXGI inventory before PDH has produced its first rate sample; a separate metadata
//! worker owns SetupAPI, D3D12, and KMT enrichment. Every source commits a complete generation-
//! tagged snapshot, so an unavailable sensor cannot invalidate trustworthy inventory or counters.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::Arc;

use windows::Win32::Graphics::Direct3D::{
    D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_12_0,
    D3D_FEATURE_LEVEL_12_1, D3D_FEATURE_LEVEL_12_2,
};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_FEATURE_DATA_FEATURE_LEVELS, D3D12_FEATURE_FEATURE_LEVELS, ID3D12Device,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG3_REMOTE, DXGI_ADAPTER_FLAG3_SOFTWARE,
    DXGI_ERROR_NOT_FOUND, IDXGIAdapter1, IDXGIAdapter4, IDXGIFactory1,
};
use windows::core::Interface;
use windows_sys::Wdk::Graphics::Direct3D::{
    D3DDDI_QUERYREGISTRY_ADAPTERKEY, D3DDDI_QUERYREGISTRY_INFO,
    D3DDDI_QUERYREGISTRY_STATUS_BUFFER_OVERFLOW, D3DDDI_QUERYREGISTRY_STATUS_FAIL,
    D3DDDI_QUERYREGISTRY_STATUS_SUCCESS, D3DKMT_ADAPTER_PERFDATA, D3DKMT_CLOSEADAPTER,
    D3DKMT_OPENADAPTERFROMLUID, D3DKMT_PHYSICAL_ADAPTER_COUNT, D3DKMT_PNP_KEY_HARDWARE,
    D3DKMT_QUERY_PHYSICAL_ADAPTER_PNP_KEY, D3DKMT_QUERYADAPTERINFO, D3DKMTCloseAdapter,
    D3DKMTOpenAdapterFromLuid, D3DKMTQueryAdapterInfo, KMTQAITYPE_ADAPTERPERFDATA,
    KMTQAITYPE_PHYSICALADAPTERCOUNT, KMTQAITYPE_PHYSICALADAPTERPNPKEY, KMTQAITYPE_QUERYREGISTRY,
};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_PRESENT, GUID_DEVCLASS_DISPLAY, HDEVINFO, SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList,
    SetupDiGetClassDevsW, SetupDiGetDevicePropertyW, SetupDiOpenDeviceInfoW,
};
use windows_sys::Win32::Devices::Properties::{
    DEVPKEY_Device_DriverDate, DEVPKEY_Device_DriverVersion, DEVPKEY_Device_LocationInfo,
    DEVPKEY_Device_LocationPaths, DEVPROP_TYPE_FILETIME, DEVPROP_TYPE_STRING,
    DEVPROP_TYPE_STRING_LIST, DEVPROPTYPE,
};
use windows_sys::Win32::Foundation::{
    ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_DATA, ERROR_NOT_FOUND,
    ERROR_SUCCESS, FILETIME, FreeLibrary, GetLastError, HMODULE, INVALID_HANDLE_VALUE,
    LUID as SysLuid, STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL, SYSTEMTIME,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Performance::{
    PDH_CSTATUS_INVALID_DATA, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_NO_COUNTER, PDH_CSTATUS_NO_OBJECT,
    PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_FMT_LARGE,
    PDH_HCOUNTER, PDH_HQUERY, PDH_INVALID_PATH, PDH_MORE_DATA, PdhAddEnglishCounterW,
    PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows_sys::Win32::System::Registry::REG_QWORD;
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Time::FileTimeToSystemTime;

use crate::winutil::{
    record_hresult_error, record_ntstatus_error, record_pdh_error, record_startup_timing,
    record_win32_error, to_wide_null,
};

const ENGINE_COUNTER_PATH: &str = r"\GPU Engine(*)\Utilization Percentage";
const DEDICATED_MEMORY_COUNTER_PATH: &str = r"\GPU Adapter Memory(*)\Dedicated Usage";
const SHARED_MEMORY_COUNTER_PATH: &str = r"\GPU Adapter Memory(*)\Shared Usage";
const MAX_PDH_ARRAY_BYTES: u32 = 64 * 1024 * 1024;
const MAX_PNP_KEY_CHARS: u32 = 32 * 1024;
const INSTALLED_MEMORY_VALUE_NAME: &str = "HardwareInformation.qwMemorySize";

#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AdapterLuid {
    pub(crate) high_part: i32,
    pub(crate) low_part: u32,
}

impl AdapterLuid {
    fn from_parts(high_part: u32, low_part: u32) -> Self {
        Self {
            high_part: high_part as i32,
            low_part,
        }
    }

    fn from_windows(luid: windows::Win32::Foundation::LUID) -> Self {
        Self {
            high_part: luid.HighPart,
            low_part: luid.LowPart,
        }
    }

    fn as_sys(self) -> SysLuid {
        SysLuid {
            LowPart: self.low_part,
            HighPart: self.high_part,
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

    fn is_baseline_pending(&self) -> bool {
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

#[derive(Clone, Debug, PartialEq)]
struct EngineReading {
    instance_name: String,
    utilization: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MemoryReading {
    instance_name: String,
    bytes: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ParsedEngineInstance {
    pid: u32,
    id: GpuEngineId,
    kind: GpuEngineKind,
}

pub(crate) struct GpuCollector {
    topology: Option<GpuTopology>,
    pdh: Option<PdhQuery>,
    pdh_error: Option<GpuSampleError>,
    engine_kinds: HashMap<GpuEngineId, GpuEngineKind>,
    generation: u64,
    inventory_pending: bool,
    dynamic_ready: bool,
    startup_started_ms: u64,
}

impl GpuCollector {
    pub(crate) fn new() -> Self {
        Self {
            topology: None,
            pdh: None,
            pdh_error: None,
            engine_kinds: HashMap::new(),
            generation: 0,
            inventory_pending: false,
            dynamic_ready: false,
            startup_started_ms: unsafe { GetTickCount64() },
        }
    }

    pub(crate) fn collect(&mut self) -> Result<GpuCollectOutcome, GpuSampleError> {
        let topology_stale = self
            .topology
            .as_ref()
            .is_none_or(|topology| !topology.is_current());
        if topology_stale {
            self.rebuild()?;
        }

        let topology = self.topology.as_ref().ok_or(GpuSampleError::InvalidData {
            context: "GPU topology commit",
        })?;
        if self.inventory_pending {
            self.inventory_pending = false;
            record_startup_timing(
                "GPU inventory ready",
                unsafe { GetTickCount64() }.wrapping_sub(self.startup_started_ms),
            );
            return Ok(GpuCollectOutcome::Inventory(GpuInventorySnapshot {
                generation: self.generation,
                adapters: topology.infos.clone(),
            }));
        }
        if topology.infos.is_empty() {
            return Ok(GpuCollectOutcome::Dynamic(GpuDynamicSnapshot {
                generation: self.generation,
                timestamp_ms: unsafe { GetTickCount64() },
                adapters: Vec::new(),
            }));
        }

        if let Some(error) = self.pdh_error.clone() {
            return Err(error);
        }

        let pdh = self.pdh.as_mut().ok_or(GpuSampleError::InvalidData {
            context: "GPU PDH query state",
        })?;
        if !pdh.collect()? {
            return Ok(GpuCollectOutcome::AwaitingBaseline {
                generation: self.generation,
            });
        }

        let engine_readings = match pdh.read_engine_values() {
            Ok(values) => values,
            Err(error) if !self.dynamic_ready && error.is_baseline_pending() => {
                return Ok(GpuCollectOutcome::AwaitingBaseline {
                    generation: self.generation,
                });
            }
            Err(error) => return Err(error),
        };
        let dedicated_readings = pdh.read_dedicated_memory_values()?;
        let shared_readings = pdh.read_shared_memory_values()?;
        let temperatures = topology.query_temperatures();
        let adapters = assemble_samples(
            &topology.infos,
            &topology.known_luids,
            engine_readings,
            dedicated_readings,
            shared_readings,
            temperatures,
        )?;
        self.engine_kinds = validated_engine_kinds(&self.engine_kinds, &adapters)?;
        if !self.dynamic_ready {
            record_startup_timing(
                "GPU first dynamic sample",
                unsafe { GetTickCount64() }.wrapping_sub(self.startup_started_ms),
            );
            self.dynamic_ready = true;
        }

        Ok(GpuCollectOutcome::Dynamic(GpuDynamicSnapshot {
            generation: self.generation,
            timestamp_ms: unsafe { GetTickCount64() },
            adapters,
        }))
    }

    fn rebuild(&mut self) -> Result<(), GpuSampleError> {
        self.startup_started_ms = unsafe { GetTickCount64() };
        let topology_started_ms = self.startup_started_ms;
        let candidate_topology = GpuTopology::query()?;
        record_startup_timing(
            "GPU DXGI inventory",
            unsafe { GetTickCount64() }.wrapping_sub(topology_started_ms),
        );
        let baseline_started_ms = unsafe { GetTickCount64() };
        let (candidate_pdh, pdh_error) = if candidate_topology.infos.is_empty() {
            (None, None)
        } else {
            match PdhQuery::new().and_then(|mut query| query.collect().map(|_| query)) {
                Ok(query) => (Some(query), None),
                Err(error) => (None, Some(error)),
            }
        };
        record_startup_timing(
            "GPU PDH baseline",
            unsafe { GetTickCount64() }.wrapping_sub(baseline_started_ms),
        );

        self.topology = Some(candidate_topology);
        self.pdh = candidate_pdh;
        self.pdh_error = pdh_error;
        self.engine_kinds.clear();
        self.generation = self.generation.wrapping_add(1).max(1);
        self.inventory_pending = true;
        self.dynamic_ready = false;
        Ok(())
    }
}

impl Default for GpuCollector {
    fn default() -> Self {
        Self::new()
    }
}

struct PdhQuery {
    query: PDH_HQUERY,
    engine_counter: PDH_HCOUNTER,
    dedicated_counter: PDH_HCOUNTER,
    shared_counter: PDH_HCOUNTER,
    primed: bool,
    engine_storage: Vec<usize>,
    dedicated_storage: Vec<usize>,
    shared_storage: Vec<usize>,
}

impl PdhQuery {
    fn new() -> Result<Self, GpuSampleError> {
        unsafe {
            let mut query = null_mut();
            let status = PdhOpenQueryW(null(), 0, &mut query);
            if status != ERROR_SUCCESS {
                return Err(GpuSampleError::Pdh {
                    context: "PdhOpenQueryW for GPU counters",
                    status,
                });
            }

            let mut candidate = Self {
                query,
                engine_counter: null_mut(),
                dedicated_counter: null_mut(),
                shared_counter: null_mut(),
                primed: false,
                engine_storage: Vec::new(),
                dedicated_storage: Vec::new(),
                shared_storage: Vec::new(),
            };
            candidate.engine_counter = candidate.add_counter(ENGINE_COUNTER_PATH)?;
            candidate.dedicated_counter = candidate.add_counter(DEDICATED_MEMORY_COUNTER_PATH)?;
            candidate.shared_counter = candidate.add_counter(SHARED_MEMORY_COUNTER_PATH)?;
            Ok(candidate)
        }
    }

    unsafe fn add_counter(&self, path: &'static str) -> Result<PDH_HCOUNTER, GpuSampleError> {
        let wide_path = to_wide_null(path);
        let mut counter = null_mut();
        let status =
            unsafe { PdhAddEnglishCounterW(self.query, wide_path.as_ptr(), 0, &mut counter) };
        if status != ERROR_SUCCESS {
            Err(GpuSampleError::Pdh {
                context: path,
                status,
            })
        } else {
            Ok(counter)
        }
    }

    fn collect(&mut self) -> Result<bool, GpuSampleError> {
        let status = unsafe { PdhCollectQueryData(self.query) };
        if status != ERROR_SUCCESS {
            return Err(GpuSampleError::Pdh {
                context: "PdhCollectQueryData for GPU counters",
                status,
            });
        }
        if !self.primed {
            self.primed = true;
            return Ok(false);
        }
        Ok(true)
    }

    fn read_engine_values(&mut self) -> Result<Vec<EngineReading>, GpuSampleError> {
        let items = query_counter_array(
            self.engine_counter,
            PDH_FMT_DOUBLE,
            &mut self.engine_storage,
        )?;
        items
            .into_iter()
            .map(|item| {
                let utilization = unsafe { item.value.Anonymous.doubleValue };
                if !utilization.is_finite() || utilization < 0.0 {
                    return Err(GpuSampleError::InvalidData {
                        context: "GPU engine utilization value",
                    });
                }
                Ok(EngineReading {
                    instance_name: item.name,
                    utilization,
                })
            })
            .collect()
    }

    fn read_dedicated_memory_values(&mut self) -> Result<Vec<MemoryReading>, GpuSampleError> {
        Self::read_memory_values(self.dedicated_counter, &mut self.dedicated_storage)
    }

    fn read_shared_memory_values(&mut self) -> Result<Vec<MemoryReading>, GpuSampleError> {
        Self::read_memory_values(self.shared_counter, &mut self.shared_storage)
    }

    fn read_memory_values(
        counter: PDH_HCOUNTER,
        storage: &mut Vec<usize>,
    ) -> Result<Vec<MemoryReading>, GpuSampleError> {
        let items = query_counter_array(counter, PDH_FMT_LARGE, storage)?;
        items
            .into_iter()
            .map(|item| {
                let bytes = unsafe { item.value.Anonymous.largeValue };
                if bytes < 0 {
                    return Err(GpuSampleError::InvalidData {
                        context: "GPU memory usage value",
                    });
                }
                Ok(MemoryReading {
                    instance_name: item.name,
                    bytes,
                })
            })
            .collect()
    }
}

impl Drop for PdhQuery {
    fn drop(&mut self) {
        if !self.query.is_null() {
            let status = unsafe { PdhCloseQuery(self.query) };
            if status != ERROR_SUCCESS {
                record_pdh_error("PdhCloseQuery for GPU counters", status);
            }
            self.query = null_mut();
        }
    }
}

struct CounterArrayItem {
    name: String,
    value: windows_sys::Win32::System::Performance::PDH_FMT_COUNTERVALUE,
}

fn query_counter_array(
    counter: PDH_HCOUNTER,
    format: u32,
    storage: &mut Vec<usize>,
) -> Result<Vec<CounterArrayItem>, GpuSampleError> {
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
        if status == ERROR_SUCCESS && item_count == 0 {
            return Ok(Vec::new());
        }
        if status != PDH_MORE_DATA {
            return Err(GpuSampleError::Pdh {
                context: "PdhGetFormattedCounterArrayW size query",
                status,
            });
        }
        if byte_count == 0 || byte_count > MAX_PDH_ARRAY_BYTES {
            return Err(GpuSampleError::InvalidData {
                context: "GPU PDH array buffer size",
            });
        }

        let word_size = size_of::<usize>();
        let words = (byte_count as usize).div_ceil(word_size);
        if storage.len() < words {
            storage.resize(words, 0);
        }
        let status = PdhGetFormattedCounterArrayW(
            counter,
            format,
            &mut byte_count,
            &mut item_count,
            storage.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
        );
        if status != ERROR_SUCCESS {
            return Err(GpuSampleError::Pdh {
                context: "PdhGetFormattedCounterArrayW data query",
                status,
            });
        }

        let used_bytes = byte_count as usize;
        if used_bytes > storage.len() * word_size
            || (item_count as usize)
                .checked_mul(size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>())
                .is_none_or(|size| size > used_bytes)
        {
            return Err(GpuSampleError::InvalidData {
                context: "GPU PDH array item bounds",
            });
        }

        let base = storage.as_ptr().cast::<u8>() as usize;
        let end = base + used_bytes;
        let items = std::slice::from_raw_parts(
            storage.as_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
            item_count as usize,
        );
        let mut result = Vec::with_capacity(items.len());
        for item in items {
            if !matches!(
                item.FmtValue.CStatus,
                PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA
            ) {
                return Err(GpuSampleError::Pdh {
                    context: "GPU PDH counter value status",
                    status: item.FmtValue.CStatus,
                });
            }
            let name = read_bounded_wide_string(item.szName, base, end)?;
            result.push(CounterArrayItem {
                name,
                value: item.FmtValue,
            });
        }
        Ok(result)
    }
}

unsafe fn read_bounded_wide_string(
    pointer: *const u16,
    base: usize,
    end: usize,
) -> Result<String, GpuSampleError> {
    let address = pointer as usize;
    if pointer.is_null()
        || address < base
        || address >= end
        || !address.is_multiple_of(size_of::<u16>())
    {
        return Err(GpuSampleError::InvalidData {
            context: "GPU PDH instance name pointer",
        });
    }

    let max_units = (end - address) / size_of::<u16>();
    let units = unsafe { std::slice::from_raw_parts(pointer, max_units) };
    let Some(length) = units.iter().position(|unit| *unit == 0) else {
        return Err(GpuSampleError::InvalidData {
            context: "GPU PDH instance name terminator",
        });
    };
    String::from_utf16(&units[..length]).map_err(|_| GpuSampleError::InvalidData {
        context: "GPU PDH instance name encoding",
    })
}

struct GpuTopology {
    factory: IDXGIFactory1,
    logical_adapters: Vec<LogicalAdapterRuntime>,
    infos: Vec<Arc<GpuAdapterInfo>>,
    known_luids: HashSet<AdapterLuid>,
}

struct LogicalAdapterRuntime {
    luid: AdapterLuid,
    kmt: OwnedKmtAdapter,
}

impl GpuTopology {
    fn query() -> Result<Self, GpuSampleError> {
        let factory: IDXGIFactory1 =
            unsafe { CreateDXGIFactory1() }.map_err(|error| GpuSampleError::HResult {
                context: "CreateDXGIFactory1 for GPU topology",
                code: error.code().0,
            })?;

        let mut logical_adapters = Vec::new();
        let mut infos = Vec::new();
        let mut known_luids = HashSet::new();
        let mut enumeration_index = 0u32;
        loop {
            let adapter_enumeration_index = enumeration_index;
            let adapter = match unsafe { factory.EnumAdapters1(enumeration_index) } {
                Ok(adapter) => adapter,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => {
                    return Err(GpuSampleError::HResult {
                        context: "IDXGIFactory1::EnumAdapters1",
                        code: error.code().0,
                    });
                }
            };
            enumeration_index =
                enumeration_index
                    .checked_add(1)
                    .ok_or(GpuSampleError::InvalidData {
                        context: "DXGI adapter enumeration index",
                    })?;

            let adapter4: IDXGIAdapter4 =
                adapter.cast().map_err(|error| GpuSampleError::HResult {
                    context: "IDXGIAdapter1 to IDXGIAdapter4",
                    code: error.code().0,
                })?;
            let desc = unsafe { adapter4.GetDesc3() }.map_err(|error| GpuSampleError::HResult {
                context: "IDXGIAdapter4::GetDesc3",
                code: error.code().0,
            })?;
            let luid = AdapterLuid::from_windows(desc.AdapterLuid);
            if !known_luids.insert(luid) {
                return Err(GpuSampleError::InvalidData {
                    context: "duplicate DXGI adapter LUID",
                });
            }
            if desc.Flags.0 & (DXGI_ADAPTER_FLAG3_SOFTWARE.0 | DXGI_ADAPTER_FLAG3_REMOTE.0) != 0 {
                continue;
            }

            let name = decode_fixed_wide(&desc.Description)?;
            let kmt = OwnedKmtAdapter::open(luid)?;
            let physical_count = validated_physical_adapter_count(kmt.physical_count()?)?;

            for physical_index in 0..physical_count {
                let single_physical_adapter = physical_count == 1;
                let dedicated_limit_bytes =
                    single_physical_adapter.then_some(desc.DedicatedVideoMemory as u64);
                let shared_limit_bytes =
                    single_physical_adapter.then_some(desc.SharedSystemMemory as u64);
                infos.push(Arc::new(GpuAdapterInfo {
                    id: GpuAdapterId {
                        luid,
                        physical_index,
                    },
                    enumeration_index: adapter_enumeration_index,
                    name: name.clone(),
                    vendor_id: desc.VendorId,
                    device_id: desc.DeviceId,
                    subsystem_id: desc.SubSysId,
                    revision: desc.Revision,
                    dedicated_limit_bytes,
                    shared_limit_bytes,
                }));
            }
            logical_adapters.push(LogicalAdapterRuntime { luid, kmt });
        }

        Ok(Self {
            factory,
            logical_adapters,
            infos,
            known_luids,
        })
    }

    fn is_current(&self) -> bool {
        unsafe { self.factory.IsCurrent().as_bool() }
    }

    fn query_temperatures(&self) -> HashMap<GpuAdapterId, Result<Option<u32>, GpuSampleError>> {
        let mut values = HashMap::with_capacity(self.infos.len());
        for info in &self.infos {
            let result = self
                .logical_adapters
                .iter()
                .find(|adapter| adapter.luid == info.id.luid)
                .map_or_else(
                    || {
                        Err(GpuSampleError::InvalidData {
                            context: "missing D3DKMT adapter for GPU temperature",
                        })
                    },
                    |adapter| adapter.kmt.temperature(info.id.physical_index),
                );
            values.insert(info.id, result);
        }
        values
    }
}

pub(crate) struct GpuMetadataCollector {
    d3d12: Result<D3d12Runtime, GpuSampleError>,
    generation: Option<u64>,
    inventory: Vec<Arc<GpuAdapterInfo>>,
    kmt_adapters: HashMap<AdapterLuid, OwnedKmtAdapter>,
    snapshot: Option<GpuMetadataSnapshot>,
}

impl GpuMetadataCollector {
    pub(crate) fn new() -> Self {
        let started_ms = unsafe { GetTickCount64() };
        let d3d12 = D3d12Runtime::load();
        record_startup_timing(
            "GPU metadata worker initialization",
            unsafe { GetTickCount64() }.wrapping_sub(started_ms),
        );
        Self {
            d3d12,
            generation: None,
            inventory: Vec::new(),
            kmt_adapters: HashMap::new(),
            snapshot: None,
        }
    }

    pub(crate) fn collect(
        &mut self,
        request: GpuMetadataRequest,
    ) -> Result<GpuMetadataSnapshot, GpuSampleError> {
        let started_ms = unsafe { GetTickCount64() };
        if request.generation == 0 {
            return Err(GpuSampleError::InvalidData {
                context: "GPU metadata generation",
            });
        }
        let mut adapter_ids = HashSet::with_capacity(request.adapters.len());
        if request
            .adapters
            .iter()
            .any(|adapter| !adapter_ids.insert(adapter.id))
        {
            return Err(GpuSampleError::InvalidData {
                context: "duplicate GPU metadata adapter identity",
            });
        }
        if self.generation == Some(request.generation) && self.inventory == request.adapters {
            return self.snapshot.clone().ok_or(GpuSampleError::InvalidData {
                context: "GPU metadata cached snapshot",
            });
        }
        if self.generation == Some(request.generation) {
            return Err(GpuSampleError::InvalidData {
                context: "GPU inventory changed without a new generation",
            });
        }

        let requested_luids: HashSet<_> =
            request.adapters.iter().map(|info| info.id.luid).collect();
        let dxgi_adapters = query_dxgi_adapters(&requested_luids);
        let info_set = OwnedDeviceInfoSet::display_devices();
        let mut kmt_adapters = HashMap::with_capacity(requested_luids.len());
        let mut kmt_errors = HashMap::new();
        for luid in requested_luids.iter().copied() {
            match OwnedKmtAdapter::open(luid) {
                Ok(adapter) => {
                    kmt_adapters.insert(luid, adapter);
                }
                Err(error) => {
                    kmt_errors.insert(luid, error);
                }
            }
        }

        let mut directx_by_luid = HashMap::with_capacity(requested_luids.len());
        for luid in requested_luids.iter().copied() {
            let value = match (&self.d3d12, &dxgi_adapters) {
                (Ok(runtime), Ok(adapters)) => adapters
                    .get(&luid)
                    .ok_or(GpuSampleError::InvalidData {
                        context: "missing DXGI adapter for GPU metadata",
                    })
                    .and_then(|adapter| runtime.query_feature_level(adapter)),
                (Err(error), _) | (_, Err(error)) => Err(error.clone()),
            };
            directx_by_luid.insert(luid, value);
        }

        let mut adapters = Vec::with_capacity(request.adapters.len());
        for info in &request.adapters {
            let mut metadata_errors = Vec::new();
            let mut driver = GpuDriverDetails::default();
            let mut hardware_reserved_bytes = None;

            match kmt_adapters.get(&info.id.luid) {
                Some(kmt) => {
                    match kmt
                        .installed_adapter_memory(info.id.physical_index)
                        .and_then(|installed_memory| {
                            validated_hardware_reserved_memory(
                                installed_memory,
                                info.dedicated_limit_bytes,
                            )
                        }) {
                        Ok(value) => hardware_reserved_bytes = value,
                        Err(error) => metadata_errors.push(error),
                    }
                    match &info_set {
                        Ok(info_set) => {
                            match query_driver_details(kmt, info.id.physical_index, info_set) {
                                Ok((value, errors)) => {
                                    driver = value;
                                    metadata_errors.extend(errors);
                                }
                                Err(error) => metadata_errors.push(error),
                            }
                        }
                        Err(error) => metadata_errors.push(error.clone()),
                    }
                }
                None => metadata_errors.push(kmt_errors.get(&info.id.luid).cloned().unwrap_or(
                    GpuSampleError::InvalidData {
                        context: "missing KMT adapter for GPU metadata",
                    },
                )),
            }

            let directx_feature_level = match directx_by_luid.get(&info.id.luid) {
                Some(Ok(value)) => value.clone(),
                Some(Err(error)) => {
                    metadata_errors.push(error.clone());
                    None
                }
                None => {
                    metadata_errors.push(GpuSampleError::InvalidData {
                        context: "missing DirectX metadata result",
                    });
                    None
                }
            };
            adapters.push(GpuAdapterMetadata {
                id: info.id,
                hardware_reserved_bytes,
                driver,
                directx_feature_level,
                metadata_errors,
            });
        }

        let snapshot = GpuMetadataSnapshot {
            generation: request.generation,
            adapters,
        };
        self.generation = Some(request.generation);
        self.inventory = request.adapters;
        self.kmt_adapters = kmt_adapters;
        self.snapshot = Some(snapshot.clone());
        record_startup_timing(
            "GPU metadata ready",
            unsafe { GetTickCount64() }.wrapping_sub(started_ms),
        );
        Ok(snapshot)
    }
}

fn query_dxgi_adapters(
    requested_luids: &HashSet<AdapterLuid>,
) -> Result<HashMap<AdapterLuid, IDXGIAdapter1>, GpuSampleError> {
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.map_err(|error| GpuSampleError::HResult {
            context: "CreateDXGIFactory1 for GPU metadata",
            code: error.code().0,
        })?;
    let mut adapters = HashMap::with_capacity(requested_luids.len());
    let mut index = 0u32;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => {
                return Err(GpuSampleError::HResult {
                    context: "IDXGIFactory1::EnumAdapters1 for GPU metadata",
                    code: error.code().0,
                });
            }
        };
        index = index.checked_add(1).ok_or(GpuSampleError::InvalidData {
            context: "GPU metadata DXGI enumeration index",
        })?;
        let adapter4: IDXGIAdapter4 = adapter.cast().map_err(|error| GpuSampleError::HResult {
            context: "IDXGIAdapter1 to IDXGIAdapter4 for GPU metadata",
            code: error.code().0,
        })?;
        let desc = unsafe { adapter4.GetDesc3() }.map_err(|error| GpuSampleError::HResult {
            context: "IDXGIAdapter4::GetDesc3 for GPU metadata",
            code: error.code().0,
        })?;
        let luid = AdapterLuid::from_windows(desc.AdapterLuid);
        if requested_luids.contains(&luid) && adapters.insert(luid, adapter).is_some() {
            return Err(GpuSampleError::InvalidData {
                context: "duplicate DXGI LUID for GPU metadata",
            });
        }
    }
    if adapters.len() != requested_luids.len() {
        return Err(GpuSampleError::InvalidData {
            context: "GPU metadata DXGI adapter completeness",
        });
    }
    Ok(adapters)
}

fn validated_physical_adapter_count(count: u32) -> Result<u32, GpuSampleError> {
    if count == 0 {
        Err(GpuSampleError::InvalidData {
            context: "D3DKMT physical adapter count",
        })
    } else {
        Ok(count)
    }
}

fn validated_hardware_reserved_memory(
    installed_memory: Option<u64>,
    dedicated_limit: Option<u64>,
) -> Result<Option<u64>, GpuSampleError> {
    let (Some(installed_memory), Some(dedicated_limit)) = (installed_memory, dedicated_limit)
    else {
        return Ok(None);
    };
    if installed_memory == 0 || dedicated_limit == 0 {
        return Ok(None);
    }
    installed_memory
        .checked_sub(dedicated_limit)
        .map(Some)
        .ok_or(GpuSampleError::InvalidData {
            context: "GPU installed memory is below the dedicated limit",
        })
}

struct OwnedKmtAdapter {
    handle: u32,
}

impl OwnedKmtAdapter {
    fn open(luid: AdapterLuid) -> Result<Self, GpuSampleError> {
        let mut open = D3DKMT_OPENADAPTERFROMLUID {
            AdapterLuid: luid.as_sys(),
            hAdapter: 0,
        };
        let status = unsafe { D3DKMTOpenAdapterFromLuid(&mut open) };
        if status < 0 {
            return Err(GpuSampleError::NtStatus {
                context: "D3DKMTOpenAdapterFromLuid",
                status,
            });
        }
        if open.hAdapter == 0 {
            return Err(GpuSampleError::InvalidData {
                context: "D3DKMTOpenAdapterFromLuid output",
            });
        }
        Ok(Self {
            handle: open.hAdapter,
        })
    }

    fn query<T: Default>(
        &self,
        query_type: i32,
        value: &mut T,
        context: &'static str,
    ) -> Result<(), GpuSampleError> {
        let size = u32::try_from(size_of::<T>()).map_err(|_| GpuSampleError::InvalidData {
            context: "D3DKMT query structure size",
        })?;
        let mut query = D3DKMT_QUERYADAPTERINFO {
            hAdapter: self.handle,
            Type: query_type,
            pPrivateDriverData: (value as *mut T).cast::<c_void>(),
            PrivateDriverDataSize: size,
        };
        let status = unsafe { D3DKMTQueryAdapterInfo(&mut query) };
        if status < 0 {
            Err(GpuSampleError::NtStatus { context, status })
        } else {
            Ok(())
        }
    }

    fn physical_count(&self) -> Result<u32, GpuSampleError> {
        let mut count = D3DKMT_PHYSICAL_ADAPTER_COUNT::default();
        self.query(
            KMTQAITYPE_PHYSICALADAPTERCOUNT,
            &mut count,
            "D3DKMT physical adapter count",
        )?;
        Ok(count.Count)
    }

    fn temperature(&self, physical_index: u32) -> Result<Option<u32>, GpuSampleError> {
        let mut data = D3DKMT_ADAPTER_PERFDATA {
            PhysicalAdapterIndex: physical_index,
            ..Default::default()
        };
        self.query(
            KMTQAITYPE_ADAPTERPERFDATA,
            &mut data,
            "D3DKMT adapter performance data",
        )?;
        Ok(Some(data.Temperature))
    }

    fn installed_adapter_memory(&self, physical_index: u32) -> Result<Option<u64>, GpuSampleError> {
        // KMT routes this cached adapter metadata correctly for physical and paravirtual adapters.
        let mut data = D3DDDI_QUERYREGISTRY_INFO {
            QueryType: D3DDDI_QUERYREGISTRY_ADAPTERKEY,
            ValueName: fixed_wide_value_name(INSTALLED_MEMORY_VALUE_NAME)?,
            ValueType: REG_QWORD,
            PhysicalAdapterIndex: physical_index,
            ..Default::default()
        };
        self.query(
            KMTQAITYPE_QUERYREGISTRY,
            &mut data,
            "D3DKMT installed GPU memory registry query",
        )?;

        match data.Status {
            D3DDDI_QUERYREGISTRY_STATUS_SUCCESS => {
                let value = unsafe { data.Anonymous.OutputQword };
                validate_installed_memory_registry_value(data.OutputValueSize, value).map(Some)
            }
            D3DDDI_QUERYREGISTRY_STATUS_FAIL => Ok(None),
            D3DDDI_QUERYREGISTRY_STATUS_BUFFER_OVERFLOW => Err(GpuSampleError::InvalidData {
                context: "installed GPU memory registry output overflow",
            }),
            _ => Err(GpuSampleError::InvalidData {
                context: "installed GPU memory registry status",
            }),
        }
    }

    fn pnp_hardware_key(&self, physical_index: u32) -> Result<String, GpuSampleError> {
        let mut char_count = 0u32;
        let mut sizing = D3DKMT_QUERY_PHYSICAL_ADAPTER_PNP_KEY {
            PhysicalAdapterIndex: physical_index,
            PnPKeyType: D3DKMT_PNP_KEY_HARDWARE,
            pDest: null_mut(),
            pCchDest: &mut char_count,
        };
        let mut query = D3DKMT_QUERYADAPTERINFO {
            hAdapter: self.handle,
            Type: KMTQAITYPE_PHYSICALADAPTERPNPKEY,
            pPrivateDriverData: (&mut sizing as *mut D3DKMT_QUERY_PHYSICAL_ADAPTER_PNP_KEY).cast(),
            PrivateDriverDataSize: size_of::<D3DKMT_QUERY_PHYSICAL_ADAPTER_PNP_KEY>() as u32,
        };
        let status = unsafe { D3DKMTQueryAdapterInfo(&mut query) };
        if status != STATUS_BUFFER_TOO_SMALL && status != STATUS_BUFFER_OVERFLOW && status < 0 {
            return Err(GpuSampleError::NtStatus {
                context: "D3DKMT physical adapter PnP key size",
                status,
            });
        }
        if char_count == 0 || char_count > MAX_PNP_KEY_CHARS {
            return Err(GpuSampleError::InvalidData {
                context: "D3DKMT physical adapter PnP key size",
            });
        }

        let mut buffer = vec![0u16; char_count as usize];
        let mut actual_count = char_count;
        let mut payload = D3DKMT_QUERY_PHYSICAL_ADAPTER_PNP_KEY {
            PhysicalAdapterIndex: physical_index,
            PnPKeyType: D3DKMT_PNP_KEY_HARDWARE,
            pDest: buffer.as_mut_ptr(),
            pCchDest: &mut actual_count,
        };
        let mut query = D3DKMT_QUERYADAPTERINFO {
            hAdapter: self.handle,
            Type: KMTQAITYPE_PHYSICALADAPTERPNPKEY,
            pPrivateDriverData: (&mut payload as *mut D3DKMT_QUERY_PHYSICAL_ADAPTER_PNP_KEY).cast(),
            PrivateDriverDataSize: size_of::<D3DKMT_QUERY_PHYSICAL_ADAPTER_PNP_KEY>() as u32,
        };
        let status = unsafe { D3DKMTQueryAdapterInfo(&mut query) };
        if status < 0 {
            return Err(GpuSampleError::NtStatus {
                context: "D3DKMT physical adapter PnP key",
                status,
            });
        }
        if actual_count == 0 || actual_count > char_count {
            return Err(GpuSampleError::InvalidData {
                context: "D3DKMT physical adapter PnP key result",
            });
        }
        let length = buffer[..actual_count as usize]
            .iter()
            .position(|unit| *unit == 0)
            .ok_or(GpuSampleError::InvalidData {
                context: "D3DKMT physical adapter PnP key terminator",
            })?;
        String::from_utf16(&buffer[..length]).map_err(|_| GpuSampleError::InvalidData {
            context: "D3DKMT physical adapter PnP key encoding",
        })
    }
}

fn fixed_wide_value_name<const N: usize>(value: &str) -> Result<[u16; N], GpuSampleError> {
    let mut result = [0u16; N];
    for (length, unit) in value.encode_utf16().enumerate() {
        if length >= N.saturating_sub(1) {
            return Err(GpuSampleError::InvalidData {
                context: "GPU registry value name length",
            });
        }
        result[length] = unit;
    }
    Ok(result)
}

fn validate_installed_memory_registry_value(
    output_size: u32,
    value: u64,
) -> Result<u64, GpuSampleError> {
    if output_size != size_of::<u64>() as u32 || value == 0 {
        return Err(GpuSampleError::InvalidData {
            context: "installed GPU memory registry value",
        });
    }
    Ok(value)
}

impl Drop for OwnedKmtAdapter {
    fn drop(&mut self) {
        if self.handle != 0 {
            let close = D3DKMT_CLOSEADAPTER {
                hAdapter: self.handle,
            };
            let status = unsafe { D3DKMTCloseAdapter(&close) };
            if status < 0 {
                record_ntstatus_error("D3DKMTCloseAdapter", status);
            }
            self.handle = 0;
        }
    }
}

fn assemble_samples(
    infos: &[Arc<GpuAdapterInfo>],
    known_luids: &HashSet<AdapterLuid>,
    engine_readings: Vec<EngineReading>,
    dedicated_readings: Vec<MemoryReading>,
    shared_readings: Vec<MemoryReading>,
    mut temperatures: HashMap<GpuAdapterId, Result<Option<u32>, GpuSampleError>>,
) -> Result<Vec<GpuAdapterSample>, GpuSampleError> {
    let displayed_ids: HashSet<_> = infos.iter().map(|info| info.id).collect();
    let mut engine_instances = HashSet::new();
    let mut engines: HashMap<GpuEngineId, (GpuEngineKind, f64)> = HashMap::new();
    for reading in engine_readings {
        let parsed = parse_engine_instance(&reading.instance_name)?;
        if !known_luids.contains(&parsed.id.adapter.luid) {
            return Err(GpuSampleError::InvalidData {
                context: "GPU engine references unknown LUID",
            });
        }
        if !displayed_ids.contains(&parsed.id.adapter) {
            continue;
        }
        if !engine_instances.insert(parsed.clone()) {
            return Err(GpuSampleError::InvalidData {
                context: "duplicate GPU engine process instance",
            });
        }
        let entry = engines
            .entry(parsed.id)
            .or_insert_with(|| (parsed.kind.clone(), 0.0));
        if entry.0 != parsed.kind {
            return Err(GpuSampleError::InvalidData {
                context: "GPU engine type changed within one snapshot",
            });
        }
        entry.1 += reading.utilization;
        if !entry.1.is_finite() {
            return Err(GpuSampleError::InvalidData {
                context: "GPU engine utilization aggregate",
            });
        }
    }

    let dedicated = collect_memory_readings(
        known_luids,
        &displayed_ids,
        dedicated_readings,
        "duplicate dedicated GPU memory instance",
    )?;
    let shared = collect_memory_readings(
        known_luids,
        &displayed_ids,
        shared_readings,
        "duplicate shared GPU memory instance",
    )?;

    let mut engines_by_adapter: HashMap<GpuAdapterId, Vec<GpuEngineSample>> = HashMap::new();
    for (id, (kind, value)) in engines {
        engines_by_adapter
            .entry(id.adapter)
            .or_default()
            .push(GpuEngineSample {
                id,
                kind,
                utilization_percent: percentage_to_u8(value),
            });
    }
    for adapter_engines in engines_by_adapter.values_mut() {
        adapter_engines.sort_by_key(|engine| engine.id.ordinal);
    }

    let mut samples = Vec::with_capacity(infos.len());
    for info in infos {
        let adapter_engines = engines_by_adapter.remove(&info.id).unwrap_or_default();
        let overall_utilization_percent = adapter_engines
            .iter()
            .map(|engine| engine.utilization_percent)
            .max()
            .unwrap_or(0);
        let mut row_errors = Vec::new();
        let temperature_deci_c = match temperatures.remove(&info.id) {
            Some(Ok(value)) => value,
            Some(Err(error)) => {
                row_errors.push(error);
                None
            }
            None => {
                row_errors.push(GpuSampleError::InvalidData {
                    context: "missing GPU temperature query result",
                });
                None
            }
        };
        let dedicated_usage_bytes =
            dedicated
                .get(&info.id)
                .copied()
                .ok_or(GpuSampleError::InvalidData {
                    context: "missing dedicated GPU memory instance",
                })?;
        let shared_usage_bytes =
            shared
                .get(&info.id)
                .copied()
                .ok_or(GpuSampleError::InvalidData {
                    context: "missing shared GPU memory instance",
                })?;
        samples.push(GpuAdapterSample {
            info: Arc::clone(info),
            overall_utilization_percent,
            engines: adapter_engines,
            dedicated_usage_bytes,
            shared_usage_bytes,
            temperature_deci_c,
            row_errors,
        });
    }
    Ok(samples)
}

fn validated_engine_kinds(
    existing: &HashMap<GpuEngineId, GpuEngineKind>,
    samples: &[GpuAdapterSample],
) -> Result<HashMap<GpuEngineId, GpuEngineKind>, GpuSampleError> {
    let mut candidate = existing.clone();
    for engine in samples.iter().flat_map(|sample| &sample.engines) {
        match candidate.get(&engine.id) {
            Some(kind) if kind != &engine.kind => {
                return Err(GpuSampleError::InvalidData {
                    context: "GPU engine type changed without a topology generation",
                });
            }
            Some(_) => {}
            None => {
                candidate.insert(engine.id, engine.kind.clone());
            }
        }
    }
    Ok(candidate)
}

fn collect_memory_readings(
    known_luids: &HashSet<AdapterLuid>,
    displayed_ids: &HashSet<GpuAdapterId>,
    readings: Vec<MemoryReading>,
    duplicate_context: &'static str,
) -> Result<HashMap<GpuAdapterId, u64>, GpuSampleError> {
    let mut values = HashMap::new();
    for reading in readings {
        let id = parse_memory_instance(&reading.instance_name)?;
        if !known_luids.contains(&id.luid) {
            return Err(GpuSampleError::InvalidData {
                context: "GPU memory references unknown LUID",
            });
        }
        if !displayed_ids.contains(&id) {
            continue;
        }
        let bytes = u64::try_from(reading.bytes).map_err(|_| GpuSampleError::InvalidData {
            context: "GPU memory usage conversion",
        })?;
        if values.insert(id, bytes).is_some() {
            return Err(GpuSampleError::InvalidData {
                context: duplicate_context,
            });
        }
    }
    Ok(values)
}

fn percentage_to_u8(value: f64) -> u8 {
    value.round().clamp(0.0, 100.0) as u8
}

fn parse_engine_instance(value: &str) -> Result<ParsedEngineInstance, GpuSampleError> {
    let parts: Vec<_> = value.split('_').collect();
    if parts.len() < 11
        || !parts[0].eq_ignore_ascii_case("pid")
        || !parts[2].eq_ignore_ascii_case("luid")
        || !parts[5].eq_ignore_ascii_case("phys")
        || !parts[7].eq_ignore_ascii_case("eng")
        || !parts[9].eq_ignore_ascii_case("engtype")
    {
        return Err(GpuSampleError::InvalidData {
            context: "GPU engine instance grammar",
        });
    }
    let engine_type = parts[10..].join("_");
    if engine_type.is_empty()
        || engine_type.len() > 128
        || !engine_type
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
    {
        return Err(GpuSampleError::InvalidData {
            context: "GPU engine type token",
        });
    }
    Ok(ParsedEngineInstance {
        pid: parse_decimal(parts[1], "GPU engine PID")?,
        id: GpuEngineId {
            adapter: GpuAdapterId {
                luid: AdapterLuid::from_parts(
                    parse_hex(parts[3], "GPU engine LUID high part")?,
                    parse_hex(parts[4], "GPU engine LUID low part")?,
                ),
                physical_index: parse_decimal(parts[6], "GPU engine physical index")?,
            },
            ordinal: parse_decimal(parts[8], "GPU engine ordinal")?,
        },
        kind: GpuEngineKind::from_counter_name(&engine_type),
    })
}

fn parse_memory_instance(value: &str) -> Result<GpuAdapterId, GpuSampleError> {
    let parts: Vec<_> = value.split('_').collect();
    if parts.len() != 5
        || !parts[0].eq_ignore_ascii_case("luid")
        || !parts[3].eq_ignore_ascii_case("phys")
    {
        return Err(GpuSampleError::InvalidData {
            context: "GPU memory instance grammar",
        });
    }
    Ok(GpuAdapterId {
        luid: AdapterLuid::from_parts(
            parse_hex(parts[1], "GPU memory LUID high part")?,
            parse_hex(parts[2], "GPU memory LUID low part")?,
        ),
        physical_index: parse_decimal(parts[4], "GPU memory physical index")?,
    })
}

fn parse_decimal(value: &str, context: &'static str) -> Result<u32, GpuSampleError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(GpuSampleError::InvalidData { context });
    }
    value
        .parse::<u32>()
        .map_err(|_| GpuSampleError::InvalidData { context })
}

fn parse_hex(value: &str, context: &'static str) -> Result<u32, GpuSampleError> {
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .ok_or(GpuSampleError::InvalidData { context })?;
    if digits.is_empty() || digits.len() > 8 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(GpuSampleError::InvalidData { context });
    }
    u32::from_str_radix(digits, 16).map_err(|_| GpuSampleError::InvalidData { context })
}

fn decode_fixed_wide(value: &[u16]) -> Result<String, GpuSampleError> {
    let length = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    String::from_utf16(&value[..length]).map_err(|_| GpuSampleError::InvalidData {
        context: "DXGI adapter description encoding",
    })
}

fn query_driver_details(
    adapter: &OwnedKmtAdapter,
    physical_index: u32,
    info_set: &OwnedDeviceInfoSet,
) -> Result<(GpuDriverDetails, Vec<GpuSampleError>), GpuSampleError> {
    let key = adapter.pnp_hardware_key(physical_index)?;
    let instance_id = device_instance_id_from_pnp_key(&key).ok_or(GpuSampleError::InvalidData {
        context: "GPU PnP hardware key shape",
    })?;
    query_setupapi_details(info_set, &instance_id)
}

fn device_instance_id_from_pnp_key(value: &str) -> Option<String> {
    let normalized = value.replace('/', "\\");
    let lowercase = normalized.to_ascii_lowercase();
    let marker = "\\enum\\";
    let start = lowercase.find(marker)? + marker.len();
    let remainder = &normalized[start..];
    let mut components = remainder.split('\\').filter(|part| !part.is_empty());
    let bus = components.next()?;
    let device = components.next()?;
    let instance = components.next()?;
    Some(format!("{bus}\\{device}\\{instance}"))
}

struct OwnedDeviceInfoSet(HDEVINFO);

impl OwnedDeviceInfoSet {
    fn display_devices() -> Result<Self, GpuSampleError> {
        let info_set = unsafe {
            SetupDiGetClassDevsW(&GUID_DEVCLASS_DISPLAY, null(), null_mut(), DIGCF_PRESENT)
        };
        if info_set == INVALID_HANDLE_VALUE as isize {
            Err(last_win32_error("SetupDiGetClassDevsW for GPU adapters"))
        } else {
            Ok(Self(info_set))
        }
    }
}

impl Drop for OwnedDeviceInfoSet {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE as isize && self.0 != 0 {
            if unsafe { SetupDiDestroyDeviceInfoList(self.0) } == 0 {
                let error = unsafe { GetLastError() };
                record_win32_error(
                    "SetupDiDestroyDeviceInfoList for GPU adapter",
                    if error == ERROR_SUCCESS {
                        ERROR_GEN_FAILURE
                    } else {
                        error
                    },
                );
            }
            self.0 = INVALID_HANDLE_VALUE as isize;
        }
    }
}

fn query_setupapi_details(
    info_set: &OwnedDeviceInfoSet,
    instance_id: &str,
) -> Result<(GpuDriverDetails, Vec<GpuSampleError>), GpuSampleError> {
    unsafe {
        let mut device_info = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..zeroed()
        };
        let instance_id = to_wide_null(instance_id);
        if SetupDiOpenDeviceInfoW(
            info_set.0,
            instance_id.as_ptr(),
            null_mut(),
            0,
            &mut device_info,
        ) == 0
        {
            return Err(last_win32_error("SetupDiOpenDeviceInfoW for GPU adapter"));
        }

        let mut errors = Vec::new();
        let version = optional_metadata_field(
            query_device_string_property(
                info_set.0,
                &device_info,
                &DEVPKEY_Device_DriverVersion,
                DEVPROP_TYPE_STRING,
                "GPU driver version property",
            ),
            &mut errors,
        );
        let date = optional_metadata_field(
            query_device_filetime_property(
                info_set.0,
                &device_info,
                &DEVPKEY_Device_DriverDate,
                "GPU driver date property",
            ),
            &mut errors,
        );
        let location = optional_metadata_field(
            query_device_string_property(
                info_set.0,
                &device_info,
                &DEVPKEY_Device_LocationInfo,
                DEVPROP_TYPE_STRING,
                "GPU location property",
            ),
            &mut errors,
        );
        let location_path = optional_metadata_field(
            query_device_string_property(
                info_set.0,
                &device_info,
                &DEVPKEY_Device_LocationPaths,
                DEVPROP_TYPE_STRING_LIST,
                "GPU location path property",
            ),
            &mut errors,
        );
        Ok((
            GpuDriverDetails {
                version,
                date,
                location,
                location_path,
            },
            errors,
        ))
    }
}

fn optional_metadata_field<T>(
    result: Result<Option<T>, GpuSampleError>,
    errors: &mut Vec<GpuSampleError>,
) -> Option<T> {
    match result {
        Ok(value) => value,
        Err(error) => {
            errors.push(error);
            None
        }
    }
}

unsafe fn query_device_string_property(
    info_set: HDEVINFO,
    device_info: &SP_DEVINFO_DATA,
    key: &windows_sys::Win32::Foundation::DEVPROPKEY,
    expected_type: DEVPROPTYPE,
    context: &'static str,
) -> Result<Option<String>, GpuSampleError> {
    let Some((property_type, buffer)) =
        (unsafe { query_device_property(info_set, device_info, key, context)? })
    else {
        return Ok(None);
    };
    if property_type != expected_type || buffer.len() % size_of::<u16>() != 0 {
        return Err(GpuSampleError::InvalidData { context });
    }
    let units = unsafe {
        std::slice::from_raw_parts(
            buffer.as_ptr().cast::<u16>(),
            buffer.len() / size_of::<u16>(),
        )
    };
    let length = units
        .iter()
        .position(|unit| *unit == 0)
        .ok_or(GpuSampleError::InvalidData { context })?;
    if length == 0 {
        return Ok(None);
    }
    String::from_utf16(&units[..length])
        .map(Some)
        .map_err(|_| GpuSampleError::InvalidData { context })
}

unsafe fn query_device_filetime_property(
    info_set: HDEVINFO,
    device_info: &SP_DEVINFO_DATA,
    key: &windows_sys::Win32::Foundation::DEVPROPKEY,
    context: &'static str,
) -> Result<Option<String>, GpuSampleError> {
    let Some((property_type, buffer)) =
        (unsafe { query_device_property(info_set, device_info, key, context)? })
    else {
        return Ok(None);
    };
    if property_type != DEVPROP_TYPE_FILETIME || buffer.len() != size_of::<FILETIME>() {
        return Err(GpuSampleError::InvalidData { context });
    }
    let filetime = unsafe { buffer.as_ptr().cast::<FILETIME>().read_unaligned() };
    let mut system_time = unsafe { zeroed::<SYSTEMTIME>() };
    if unsafe { FileTimeToSystemTime(&filetime, &mut system_time) } == 0 {
        return Err(last_win32_error(context));
    }
    Ok(Some(format!(
        "{:04}-{:02}-{:02}",
        system_time.wYear, system_time.wMonth, system_time.wDay
    )))
}

unsafe fn query_device_property(
    info_set: HDEVINFO,
    device_info: &SP_DEVINFO_DATA,
    key: &windows_sys::Win32::Foundation::DEVPROPKEY,
    context: &'static str,
) -> Result<Option<(DEVPROPTYPE, Vec<u8>)>, GpuSampleError> {
    let mut property_type = 0u32;
    let mut required_size = 0u32;
    if unsafe {
        SetupDiGetDevicePropertyW(
            info_set,
            device_info,
            key,
            &mut property_type,
            null_mut(),
            0,
            &mut required_size,
            0,
        )
    } == 0
    {
        let error = unsafe { GetLastError() };
        if error == ERROR_NOT_FOUND {
            return Ok(None);
        }
        if error != ERROR_INSUFFICIENT_BUFFER {
            return Err(GpuSampleError::Win32 {
                context,
                code: if error == ERROR_SUCCESS {
                    ERROR_GEN_FAILURE
                } else {
                    error
                },
            });
        }
    }
    if required_size == 0 || required_size > MAX_PDH_ARRAY_BYTES {
        return Err(GpuSampleError::InvalidData { context });
    }
    let mut buffer = vec![0u8; required_size as usize];
    if unsafe {
        SetupDiGetDevicePropertyW(
            info_set,
            device_info,
            key,
            &mut property_type,
            buffer.as_mut_ptr(),
            required_size,
            &mut required_size,
            0,
        )
    } == 0
    {
        return Err(last_win32_error(context));
    }
    let actual_size = required_size as usize;
    if actual_size > buffer.len() {
        return Err(GpuSampleError::InvalidData { context });
    }
    buffer.truncate(actual_size);
    Ok(Some((property_type, buffer)))
}

type D3d12CreateDevice = unsafe extern "system" fn(
    *mut c_void,
    D3D_FEATURE_LEVEL,
    *const windows::core::GUID,
    *mut *mut c_void,
) -> i32;

struct D3d12Runtime {
    _library: DynamicLibrary,
    create_device: D3d12CreateDevice,
}

impl D3d12Runtime {
    fn load() -> Result<Self, GpuSampleError> {
        let library = DynamicLibrary::load("d3d12.dll")?;
        let procedure = unsafe { GetProcAddress(library.0, c"D3D12CreateDevice".as_ptr().cast()) };
        let Some(procedure) = procedure else {
            return Err(last_win32_error("GetProcAddress for D3D12CreateDevice"));
        };
        // Safety: the symbol is obtained from the loaded system d3d12.dll under its documented
        // export name, and `_library` keeps the code address alive for the runtime's lifetime.
        let create_device: D3d12CreateDevice = unsafe { std::mem::transmute(procedure) };
        Ok(Self {
            _library: library,
            create_device,
        })
    }

    fn query_feature_level(
        &self,
        adapter: &IDXGIAdapter1,
    ) -> Result<Option<String>, GpuSampleError> {
        let mut raw_device = null_mut();
        let result = unsafe {
            (self.create_device)(
                adapter.as_raw(),
                D3D_FEATURE_LEVEL_11_0,
                &ID3D12Device::IID,
                &mut raw_device,
            )
        };
        if result < 0 {
            return Ok(None);
        }
        if raw_device.is_null() {
            return Err(GpuSampleError::InvalidData {
                context: "D3D12CreateDevice output",
            });
        }
        let device = unsafe { ID3D12Device::from_raw(raw_device) };
        let requested = [
            D3D_FEATURE_LEVEL_12_2,
            D3D_FEATURE_LEVEL_12_1,
            D3D_FEATURE_LEVEL_12_0,
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_11_0,
        ];
        let mut levels = D3D12_FEATURE_DATA_FEATURE_LEVELS {
            NumFeatureLevels: requested.len() as u32,
            pFeatureLevelsRequested: requested.as_ptr(),
            MaxSupportedFeatureLevel: D3D_FEATURE_LEVEL_11_0,
        };
        unsafe {
            device.CheckFeatureSupport(
                D3D12_FEATURE_FEATURE_LEVELS,
                (&mut levels as *mut D3D12_FEATURE_DATA_FEATURE_LEVELS).cast(),
                size_of::<D3D12_FEATURE_DATA_FEATURE_LEVELS>() as u32,
            )
        }
        .map_err(|error| GpuSampleError::HResult {
            context: "ID3D12Device::CheckFeatureSupport",
            code: error.code().0,
        })?;
        Ok(feature_level_name(levels.MaxSupportedFeatureLevel).map(str::to_string))
    }
}

fn feature_level_name(level: D3D_FEATURE_LEVEL) -> Option<&'static str> {
    match level {
        D3D_FEATURE_LEVEL_12_2 => Some("12 (FL 12.2)"),
        D3D_FEATURE_LEVEL_12_1 => Some("12 (FL 12.1)"),
        D3D_FEATURE_LEVEL_12_0 => Some("12 (FL 12.0)"),
        D3D_FEATURE_LEVEL_11_1 => Some("12 (FL 11.1)"),
        D3D_FEATURE_LEVEL_11_0 => Some("12 (FL 11.0)"),
        _ => None,
    }
}

struct DynamicLibrary(HMODULE);

impl DynamicLibrary {
    fn load(name: &str) -> Result<Self, GpuSampleError> {
        let name = to_wide_null(name);
        let module = unsafe { LoadLibraryW(name.as_ptr()) };
        if module.is_null() {
            Err(last_win32_error("LoadLibraryW for D3D12"))
        } else {
            Ok(Self(module))
        }
    }
}

impl Drop for DynamicLibrary {
    fn drop(&mut self) {
        if !self.0.is_null() {
            if unsafe { FreeLibrary(self.0) } == 0 {
                let error = unsafe { GetLastError() };
                record_win32_error(
                    "FreeLibrary for D3D12",
                    if error == ERROR_SUCCESS {
                        ERROR_GEN_FAILURE
                    } else {
                        error
                    },
                );
            }
            self.0 = null_mut();
        }
    }
}

fn last_win32_error(context: &'static str) -> GpuSampleError {
    let code = unsafe { GetLastError() };
    GpuSampleError::Win32 {
        context,
        code: if code == ERROR_SUCCESS {
            ERROR_GEN_FAILURE
        } else {
            code
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter_info(id: GpuAdapterId) -> Arc<GpuAdapterInfo> {
        Arc::new(GpuAdapterInfo {
            id,
            enumeration_index: 0,
            name: "Test GPU".to_string(),
            vendor_id: 1,
            device_id: 2,
            subsystem_id: 3,
            revision: 4,
            dedicated_limit_bytes: Some(8 * 1024 * 1024 * 1024),
            shared_limit_bytes: Some(4 * 1024 * 1024 * 1024),
        })
    }

    #[test]
    fn parses_engine_identity_without_guessing_fields() {
        let parsed = parse_engine_instance(
            "pid_104888_luid_0x00000001_0x000119b6_phys_2_eng_7_engtype_videoencode",
        )
        .unwrap();
        assert_eq!(parsed.pid, 104888);
        assert_eq!(parsed.id.adapter.luid.high_part, 1);
        assert_eq!(parsed.id.adapter.luid.low_part, 0x119b6);
        assert_eq!(parsed.id.adapter.physical_index, 2);
        assert_eq!(parsed.id.ordinal, 7);
        assert_eq!(parsed.kind, GpuEngineKind::VideoEncode);
    }

    #[test]
    fn rejects_truncated_or_overflowing_counter_instances() {
        for value in [
            "pid_1_luid_0x0_0x1_phys_0_eng_0",
            "pid_4294967296_luid_0x0_0x1_phys_0_eng_0_engtype_3d",
            "pid_1_luid_0x000000000_0x1_phys_0_eng_0_engtype_3d",
            "pid_1_luid_1_0x1_phys_0_eng_0_engtype_3d",
        ] {
            assert!(parse_engine_instance(value).is_err(), "{value}");
        }
    }

    #[test]
    fn parses_memory_identity() {
        let parsed = parse_memory_instance("luid_0xffffffff_0x00000002_phys_3").unwrap();
        assert_eq!(parsed.luid.high_part, -1);
        assert_eq!(parsed.luid.low_part, 2);
        assert_eq!(parsed.physical_index, 3);
    }

    #[test]
    fn inventory_order_can_preserve_dxgi_enumeration_order() {
        let make_info = |low_part: u32, enumeration_index: u32| {
            let id = GpuAdapterId {
                luid: AdapterLuid::from_parts(0, low_part),
                physical_index: 0,
            };
            let mut info = (*adapter_info(id)).clone();
            info.enumeration_index = enumeration_index;
            Arc::new(info)
        };
        let mut infos = [make_info(1, 2), make_info(2, 0), make_info(3, 1)];
        infos.sort_by_key(|info| (info.enumeration_index, info.id.physical_index));
        assert_eq!(
            infos
                .iter()
                .map(|info| info.id.luid.low_part)
                .collect::<Vec<_>>(),
            vec![2, 3, 1]
        );
    }

    #[test]
    fn physical_adapter_count_is_never_guessed() {
        assert_eq!(validated_physical_adapter_count(2), Ok(2));
        assert_eq!(
            validated_physical_adapter_count(0),
            Err(GpuSampleError::InvalidData {
                context: "D3DKMT physical adapter count"
            })
        );
    }

    #[test]
    fn installed_memory_registry_value_requires_an_exact_qword() {
        assert_eq!(
            validate_installed_memory_registry_value(8, 8 * 1024 * 1024 * 1024),
            Ok(8 * 1024 * 1024 * 1024)
        );
        assert!(validate_installed_memory_registry_value(4, u32::MAX as u64).is_err());
        assert!(validate_installed_memory_registry_value(8, 0).is_err());
    }

    #[test]
    fn hardware_reserved_memory_is_a_checked_difference() {
        let gib = 1024 * 1024 * 1024;
        assert_eq!(
            validated_hardware_reserved_memory(Some(8 * gib), Some(7 * gib)),
            Ok(Some(gib))
        );
        assert_eq!(
            validated_hardware_reserved_memory(None, Some(7 * gib)),
            Ok(None)
        );
        assert_eq!(
            validated_hardware_reserved_memory(Some(8 * gib), None),
            Ok(None)
        );
        assert!(validated_hardware_reserved_memory(Some(7 * gib), Some(8 * gib)).is_err());
    }

    #[test]
    fn registry_value_name_is_nul_terminated_without_truncation() {
        let value = fixed_wide_value_name::<260>(INSTALLED_MEMORY_VALUE_NAME).unwrap();
        let expected = INSTALLED_MEMORY_VALUE_NAME
            .encode_utf16()
            .collect::<Vec<_>>();
        assert_eq!(&value[..expected.len()], expected.as_slice());
        assert_eq!(value[expected.len()], 0);
        assert!(fixed_wide_value_name::<4>("four").is_err());
    }

    #[test]
    fn aggregates_process_instances_per_engine_and_uses_busiest_engine() {
        let id = GpuAdapterId {
            luid: AdapterLuid::from_parts(0, 0x1234),
            physical_index: 0,
        };
        let info = adapter_info(id);
        let known = HashSet::from([id.luid]);
        let engines = vec![
            EngineReading {
                instance_name: "pid_10_luid_0x0_0x1234_phys_0_eng_1_engtype_3d".to_string(),
                utilization: 35.4,
            },
            EngineReading {
                instance_name: "pid_11_luid_0x0_0x1234_phys_0_eng_1_engtype_3d".to_string(),
                utilization: 30.4,
            },
            EngineReading {
                instance_name: "pid_10_luid_0x0_0x1234_phys_0_eng_2_engtype_copy".to_string(),
                utilization: 80.2,
            },
        ];
        let result = assemble_samples(
            &[info],
            &known,
            engines,
            vec![MemoryReading {
                instance_name: "luid_0x0_0x1234_phys_0".to_string(),
                bytes: 1024,
            }],
            vec![MemoryReading {
                instance_name: "luid_0x0_0x1234_phys_0".to_string(),
                bytes: 2048,
            }],
            HashMap::from([(id, Ok(Some(0)))]),
        )
        .unwrap();
        assert_eq!(result[0].engines[0].utilization_percent, 66);
        assert_eq!(result[0].engines[1].utilization_percent, 80);
        assert_eq!(result[0].overall_utilization_percent, 80);
        assert_eq!(result[0].dedicated_usage_bytes, 1024);
        assert_eq!(result[0].shared_usage_bytes, 2048);
        assert_eq!(result[0].temperature_deci_c, Some(0));

        let known_kinds = validated_engine_kinds(&HashMap::new(), &result).unwrap();
        let mut changed = result.clone();
        changed[0].engines[0].kind = GpuEngineKind::Copy;
        assert_eq!(
            validated_engine_kinds(&known_kinds, &changed).unwrap_err(),
            GpuSampleError::InvalidData {
                context: "GPU engine type changed without a topology generation"
            }
        );
    }

    #[test]
    fn missing_memory_counter_instances_are_not_reported_as_zero() {
        let id = GpuAdapterId {
            luid: AdapterLuid::from_parts(0, 1),
            physical_index: 0,
        };
        let error = assemble_samples(
            &[adapter_info(id)],
            &HashSet::from([id.luid]),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HashMap::from([(id, Ok(None))]),
        )
        .unwrap_err();
        assert_eq!(
            error,
            GpuSampleError::InvalidData {
                context: "missing dedicated GPU memory instance"
            }
        );
    }

    #[test]
    fn physical_indices_under_one_luid_remain_distinct() {
        let luid = AdapterLuid::from_parts(0, 0x44);
        let first = GpuAdapterId {
            luid,
            physical_index: 0,
        };
        let second = GpuAdapterId {
            luid,
            physical_index: 1,
        };
        let samples = assemble_samples(
            &[adapter_info(first), adapter_info(second)],
            &HashSet::from([luid]),
            vec![
                EngineReading {
                    instance_name: "pid_1_luid_0x0_0x44_phys_0_eng_0_engtype_3d".to_string(),
                    utilization: 10.0,
                },
                EngineReading {
                    instance_name: "pid_2_luid_0x0_0x44_phys_1_eng_0_engtype_3d".to_string(),
                    utilization: 20.0,
                },
            ],
            vec![
                MemoryReading {
                    instance_name: "luid_0x0_0x44_phys_0".to_string(),
                    bytes: 100,
                },
                MemoryReading {
                    instance_name: "luid_0x0_0x44_phys_1".to_string(),
                    bytes: 200,
                },
            ],
            vec![
                MemoryReading {
                    instance_name: "luid_0x0_0x44_phys_0".to_string(),
                    bytes: 300,
                },
                MemoryReading {
                    instance_name: "luid_0x0_0x44_phys_1".to_string(),
                    bytes: 400,
                },
            ],
            HashMap::from([(first, Ok(None)), (second, Ok(None))]),
        )
        .unwrap();

        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].info.id, first);
        assert_eq!(samples[0].overall_utilization_percent, 10);
        assert_eq!(samples[0].dedicated_usage_bytes, 100);
        assert_eq!(samples[0].shared_usage_bytes, 300);
        assert_eq!(samples[1].info.id, second);
        assert_eq!(samples[1].overall_utilization_percent, 20);
        assert_eq!(samples[1].dedicated_usage_bytes, 200);
        assert_eq!(samples[1].shared_usage_bytes, 400);
    }

    #[test]
    fn known_non_displayed_adapter_instances_are_ignored_without_weakening_identity_checks() {
        let displayed = GpuAdapterId {
            luid: AdapterLuid::from_parts(0, 0x10),
            physical_index: 0,
        };
        let non_displayed_luid = AdapterLuid::from_parts(0, 0x20);
        let samples = assemble_samples(
            &[adapter_info(displayed)],
            &HashSet::from([displayed.luid, non_displayed_luid]),
            vec![EngineReading {
                instance_name: "pid_1_luid_0x0_0x20_phys_0_eng_0_engtype_3d".to_string(),
                utilization: 50.0,
            }],
            vec![MemoryReading {
                instance_name: "luid_0x0_0x10_phys_0".to_string(),
                bytes: 100,
            }],
            vec![MemoryReading {
                instance_name: "luid_0x0_0x10_phys_0".to_string(),
                bytes: 200,
            }],
            HashMap::from([(displayed, Ok(None))]),
        )
        .unwrap();
        assert!(samples[0].engines.is_empty());

        let unknown = EngineReading {
            instance_name: "pid_1_luid_0x0_0x30_phys_0_eng_0_engtype_3d".to_string(),
            utilization: 1.0,
        };
        assert!(
            assemble_samples(
                &[adapter_info(displayed)],
                &HashSet::from([displayed.luid, non_displayed_luid]),
                vec![unknown],
                Vec::new(),
                Vec::new(),
                HashMap::from([(displayed, Ok(None))]),
            )
            .is_err()
        );
    }

    #[test]
    fn clamps_only_the_final_engine_display_value() {
        assert_eq!(percentage_to_u8(101.7), 100);
        assert_eq!(percentage_to_u8(49.5), 50);
    }

    #[test]
    fn rejects_duplicate_process_engine_instances() {
        let id = GpuAdapterId {
            luid: AdapterLuid::from_parts(0, 7),
            physical_index: 0,
        };
        let info = adapter_info(id);
        let reading = EngineReading {
            instance_name: "pid_1_luid_0x0_0x7_phys_0_eng_0_engtype_3d".to_string(),
            utilization: 1.0,
        };
        assert!(
            assemble_samples(
                &[info],
                &HashSet::from([id.luid]),
                vec![reading.clone(), reading],
                Vec::new(),
                Vec::new(),
                HashMap::new(),
            )
            .is_err()
        );
    }

    #[test]
    fn extracts_setupapi_instance_id_from_registry_pnp_key() {
        assert_eq!(
            device_instance_id_from_pnp_key(
                r"\Registry\Machine\System\CurrentControlSet\Enum\PCI\VEN_1234&DEV_5678\ABC\Device Parameters"
            )
            .as_deref(),
            Some(r"PCI\VEN_1234&DEV_5678\ABC")
        );
    }

    #[test]
    #[ignore = "requires live Windows DXGI, KMT, SetupAPI, D3D12, and PDH services"]
    fn live_gpu_sources_submit_inventory_before_optional_details() {
        let mut collector = GpuCollector::new();
        let inventory = match collector.collect().expect("GPU inventory query") {
            GpuCollectOutcome::Inventory(inventory) => inventory,
            other => panic!("first GPU completion was not inventory: {other:?}"),
        };
        assert_ne!(inventory.generation, 0);

        let mut metadata_collector = GpuMetadataCollector::new();
        let metadata = metadata_collector
            .collect(GpuMetadataRequest {
                generation: inventory.generation,
                adapters: inventory.adapters.clone(),
            })
            .expect("GPU metadata query");
        assert_eq!(metadata.generation, inventory.generation);
        assert_eq!(metadata.adapters.len(), inventory.adapters.len());

        match collector.collect().expect("second GPU sample") {
            GpuCollectOutcome::AwaitingBaseline { generation } => {
                assert_eq!(generation, inventory.generation);
            }
            GpuCollectOutcome::Dynamic(snapshot) => {
                assert_eq!(snapshot.generation, inventory.generation);
                assert_eq!(snapshot.adapters.len(), inventory.adapters.len());
            }
            GpuCollectOutcome::Inventory(_) => panic!("inventory was submitted twice"),
        }
    }
}
