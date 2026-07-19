// +-------------------------------------------------------------------------
//
//   taskmgr-rs - CPU 固件与 WMI 采集
//
//   文件:       src/pages/cpu/firmware.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 在独立 MTA worker 中初始化并复用 WMI 服务，读取固件报告的处理器身份。
//!
//! WMI 失败只影响固件来源；用户主动刷新才重建失败的 provider。

use std::mem::ManuallyDrop;

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
use windows_sys::Win32::System::SystemInformation::GetTickCount64;

use super::model::{
    CpuComponentUpdate, CpuDetailError, CpuDetailRefresh, CpuDetailRequest, CpuFirmwareProcessor,
    CpuFirmwareSnapshot, CpuTopologyKey, invalid, invalid_error,
};
use crate::infrastructure::native::{record_hresult_error, record_startup_timing, to_wide_null};

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

pub(super) fn wmi_i4_to_u16(value: i32) -> Result<u16, CpuDetailError> {
    u16::try_from(value).map_err(|_| invalid_error("Win32_Processor u16 property overflow"))
}

pub(super) fn wmi_i4_to_u32(value: i32) -> u32 {
    u32::from_ne_bytes(value.to_ne_bytes())
}
