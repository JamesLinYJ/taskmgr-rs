// +-------------------------------------------------------------------------
//
//   taskmgr-rs - GPU 性能计数器
//
//   文件:       src/pages/gpu/counters.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Owns the persistent PDH query and assembles generation-tagged dynamic GPU snapshots.
//! Counter baselines and topology changes are committed without inventing zero samples.

use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::Arc;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE,
    PDH_FMT_LARGE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCloseQuery,
    PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;

use super::inventory::GpuTopology;
use super::model::{
    AdapterLuid, GpuAdapterId, GpuAdapterInfo, GpuAdapterSample, GpuCollectOutcome,
    GpuDynamicSnapshot, GpuEngineId, GpuEngineKind, GpuEngineSample, GpuInventorySnapshot,
    GpuSampleError,
};
use crate::infrastructure::native::{record_pdh_error, record_startup_timing, to_wide_null};

const ENGINE_COUNTER_PATH: &str = r"\GPU Engine(*)\Utilization Percentage";
const DEDICATED_MEMORY_COUNTER_PATH: &str = r"\GPU Adapter Memory(*)\Dedicated Usage";
const SHARED_MEMORY_COUNTER_PATH: &str = r"\GPU Adapter Memory(*)\Shared Usage";
const MAX_PDH_ARRAY_BYTES: u32 = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct EngineReading {
    pub(super) instance_name: String,
    pub(super) utilization: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MemoryReading {
    pub(super) instance_name: String,
    pub(super) bytes: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ParsedEngineInstance {
    pub(super) pid: u32,
    pub(super) id: GpuEngineId,
    pub(super) kind: GpuEngineKind,
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

pub(super) fn assemble_samples(
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

pub(super) fn validated_engine_kinds(
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

pub(super) fn percentage_to_u8(value: f64) -> u8 {
    value.round().clamp(0.0, 100.0) as u8
}

pub(super) fn parse_engine_instance(value: &str) -> Result<ParsedEngineInstance, GpuSampleError> {
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

pub(super) fn parse_memory_instance(value: &str) -> Result<GpuAdapterId, GpuSampleError> {
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
