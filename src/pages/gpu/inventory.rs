// +-------------------------------------------------------------------------
//
//   taskmgr-rs - GPU 适配器清单与 KMT 句柄
//
//   文件:       src/pages/gpu/inventory.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Enumerates hardware adapters with DXGI and owns the KMT adapter handles tied to that topology.
//! A topology candidate is complete before it replaces the previous generation.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;
use std::sync::Arc;

use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG3_REMOTE, DXGI_ADAPTER_FLAG3_SOFTWARE,
    DXGI_ERROR_NOT_FOUND, IDXGIAdapter4, IDXGIFactory1,
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
use windows_sys::Win32::Foundation::{
    LUID as SysLuid, STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL,
};
use windows_sys::Win32::System::Registry::REG_QWORD;

use super::model::{AdapterLuid, GpuAdapterId, GpuAdapterInfo, GpuSampleError};
use crate::infrastructure::native::record_ntstatus_error;

const MAX_PNP_KEY_CHARS: u32 = 32 * 1024;
pub(super) const INSTALLED_MEMORY_VALUE_NAME: &str = "HardwareInformation.qwMemorySize";

impl AdapterLuid {
    pub(super) fn from_windows(luid: windows::Win32::Foundation::LUID) -> Self {
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

pub(super) struct GpuTopology {
    factory: IDXGIFactory1,
    logical_adapters: Vec<LogicalAdapterRuntime>,
    pub(super) infos: Vec<Arc<GpuAdapterInfo>>,
    pub(super) known_luids: HashSet<AdapterLuid>,
}

struct LogicalAdapterRuntime {
    luid: AdapterLuid,
    kmt: OwnedKmtAdapter,
}

impl GpuTopology {
    pub(super) fn query() -> Result<Self, GpuSampleError> {
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

    pub(super) fn is_current(&self) -> bool {
        unsafe { self.factory.IsCurrent().as_bool() }
    }

    pub(super) fn query_temperatures(
        &self,
    ) -> HashMap<GpuAdapterId, Result<Option<u32>, GpuSampleError>> {
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

pub(super) fn validated_physical_adapter_count(count: u32) -> Result<u32, GpuSampleError> {
    if count == 0 {
        Err(GpuSampleError::InvalidData {
            context: "D3DKMT physical adapter count",
        })
    } else {
        Ok(count)
    }
}

pub(super) struct OwnedKmtAdapter {
    handle: u32,
}

impl OwnedKmtAdapter {
    pub(super) fn open(luid: AdapterLuid) -> Result<Self, GpuSampleError> {
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

    pub(super) fn installed_adapter_memory(
        &self,
        physical_index: u32,
    ) -> Result<Option<u64>, GpuSampleError> {
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

    pub(super) fn pnp_hardware_key(&self, physical_index: u32) -> Result<String, GpuSampleError> {
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

pub(super) fn fixed_wide_value_name<const N: usize>(
    value: &str,
) -> Result<[u16; N], GpuSampleError> {
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

pub(super) fn validate_installed_memory_registry_value(
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

fn decode_fixed_wide(value: &[u16]) -> Result<String, GpuSampleError> {
    let length = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    String::from_utf16(&value[..length]).map_err(|_| GpuSampleError::InvalidData {
        context: "DXGI adapter description encoding",
    })
}
