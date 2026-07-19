// +-------------------------------------------------------------------------
//
//   taskmgr-rs - GPU 适配器元数据
//
//   文件:       src/pages/gpu/metadata.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Owns SetupAPI, D3D12, and metadata enrichment for one GPU topology generation.
//! Optional field failures remain attached to their adapter and never invalidate inventory.

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
    CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND, IDXGIAdapter1, IDXGIAdapter4, IDXGIFactory1,
};
use windows::core::Interface;
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
    ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER, ERROR_NOT_FOUND, ERROR_SUCCESS, FILETIME,
    FreeLibrary, GetLastError, HMODULE, INVALID_HANDLE_VALUE, SYSTEMTIME,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Time::FileTimeToSystemTime;

use super::inventory::OwnedKmtAdapter;
use super::model::{
    AdapterLuid, GpuAdapterInfo, GpuAdapterMetadata, GpuDriverDetails, GpuMetadataRequest,
    GpuMetadataSnapshot, GpuSampleError,
};
use crate::infrastructure::native::{record_startup_timing, record_win32_error, to_wide_null};

const MAX_DEVICE_PROPERTY_BYTES: u32 = 64 * 1024 * 1024;

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

pub(super) fn validated_hardware_reserved_memory(
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

pub(super) fn device_instance_id_from_pnp_key(value: &str) -> Option<String> {
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
    if required_size == 0 || required_size > MAX_DEVICE_PROPERTY_BYTES {
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
