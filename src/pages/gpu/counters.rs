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
    GpuSampleError, GpuSampleIssue, GpuSampleSource,
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct CounterItemIssue {
    instance_name: Option<String>,
    error: GpuSampleError,
}

#[derive(Clone, Debug, PartialEq)]
struct CounterRead<T> {
    values: Vec<T>,
    issues: Vec<CounterItemIssue>,
}

impl<T> CounterRead<T> {
    #[cfg(test)]
    fn values(values: Vec<T>) -> Self {
        Self {
            values,
            issues: Vec::new(),
        }
    }

    fn source_error(error: GpuSampleError) -> Self {
        Self {
            values: Vec::new(),
            issues: vec![CounterItemIssue {
                instance_name: None,
                error,
            }],
        }
    }
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

        let engine_readings = pdh.read_engine_values();
        if !self.dynamic_ready
            && engine_readings.values.is_empty()
            && engine_readings
                .issues
                .iter()
                .any(|issue| issue.error.is_baseline_pending())
        {
            return Ok(GpuCollectOutcome::AwaitingBaseline {
                generation: self.generation,
            });
        }
        let dedicated_readings = pdh.read_dedicated_memory_values();
        let shared_readings = pdh.read_shared_memory_values();
        let temperatures = topology.query_temperatures();
        let mut adapters = assemble_counter_samples(
            &topology.infos,
            &topology.known_luids,
            engine_readings,
            dedicated_readings,
            shared_readings,
            temperatures,
        )?;
        self.engine_kinds = validated_engine_kinds(&self.engine_kinds, &mut adapters);
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
    engine_error: Option<GpuSampleError>,
    dedicated_counter: PDH_HCOUNTER,
    dedicated_error: Option<GpuSampleError>,
    shared_counter: PDH_HCOUNTER,
    shared_error: Option<GpuSampleError>,
    primed: bool,
    engine_storage: Vec<usize>,
    dedicated_storage: Vec<usize>,
    shared_storage: Vec<usize>,
}

pub(super) struct GpuCounterCapability {
    pub(super) source: GpuSampleSource,
    pub(super) error: Option<GpuSampleError>,
}

pub(super) fn probe_counter_capabilities() -> Result<[GpuCounterCapability; 3], GpuSampleError> {
    let query = PdhQuery::new()?;
    Ok([
        GpuCounterCapability {
            source: GpuSampleSource::Engine,
            error: query.engine_error.clone(),
        },
        GpuCounterCapability {
            source: GpuSampleSource::DedicatedMemory,
            error: query.dedicated_error.clone(),
        },
        GpuCounterCapability {
            source: GpuSampleSource::SharedMemory,
            error: query.shared_error.clone(),
        },
    ])
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
                engine_error: None,
                dedicated_counter: null_mut(),
                dedicated_error: None,
                shared_counter: null_mut(),
                shared_error: None,
                primed: false,
                engine_storage: Vec::new(),
                dedicated_storage: Vec::new(),
                shared_storage: Vec::new(),
            };
            (candidate.engine_counter, candidate.engine_error) =
                candidate.add_optional_counter(ENGINE_COUNTER_PATH);
            (candidate.dedicated_counter, candidate.dedicated_error) =
                candidate.add_optional_counter(DEDICATED_MEMORY_COUNTER_PATH);
            (candidate.shared_counter, candidate.shared_error) =
                candidate.add_optional_counter(SHARED_MEMORY_COUNTER_PATH);
            Ok(candidate)
        }
    }

    unsafe fn add_optional_counter(
        &self,
        path: &'static str,
    ) -> (PDH_HCOUNTER, Option<GpuSampleError>) {
        let wide_path = to_wide_null(path);
        let mut counter = null_mut();
        let status =
            unsafe { PdhAddEnglishCounterW(self.query, wide_path.as_ptr(), 0, &mut counter) };
        if status != ERROR_SUCCESS {
            (
                null_mut(),
                Some(GpuSampleError::Pdh {
                    context: path,
                    status,
                }),
            )
        } else {
            (counter, None)
        }
    }

    fn collect(&mut self) -> Result<bool, GpuSampleError> {
        if self.engine_counter.is_null()
            && self.dedicated_counter.is_null()
            && self.shared_counter.is_null()
        {
            self.primed = true;
            return Ok(true);
        }
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

    fn read_engine_values(&mut self) -> CounterRead<EngineReading> {
        if let Some(error) = self.engine_error.clone() {
            return CounterRead::source_error(error);
        }
        let items = match query_counter_array(
            self.engine_counter,
            PDH_FMT_DOUBLE,
            &mut self.engine_storage,
        ) {
            Ok(items) => items,
            Err(error) => return CounterRead::source_error(error),
        };
        let mut values = Vec::with_capacity(items.values.len());
        let mut issues = items.issues;
        for item in items.values {
            let utilization = unsafe { item.value.Anonymous.doubleValue };
            if !utilization.is_finite() || utilization < 0.0 {
                issues.push(CounterItemIssue {
                    instance_name: Some(item.name),
                    error: GpuSampleError::InvalidData {
                        context: "GPU engine utilization value",
                    },
                });
            } else {
                values.push(EngineReading {
                    instance_name: item.name,
                    utilization,
                });
            }
        }
        CounterRead { values, issues }
    }

    fn read_dedicated_memory_values(&mut self) -> CounterRead<MemoryReading> {
        Self::read_memory_values(
            self.dedicated_counter,
            self.dedicated_error.clone(),
            &mut self.dedicated_storage,
        )
    }

    fn read_shared_memory_values(&mut self) -> CounterRead<MemoryReading> {
        Self::read_memory_values(
            self.shared_counter,
            self.shared_error.clone(),
            &mut self.shared_storage,
        )
    }

    fn read_memory_values(
        counter: PDH_HCOUNTER,
        source_error: Option<GpuSampleError>,
        storage: &mut Vec<usize>,
    ) -> CounterRead<MemoryReading> {
        if let Some(error) = source_error {
            return CounterRead::source_error(error);
        }
        let items = match query_counter_array(counter, PDH_FMT_LARGE, storage) {
            Ok(items) => items,
            Err(error) => return CounterRead::source_error(error),
        };
        let mut values = Vec::with_capacity(items.values.len());
        let mut issues = items.issues;
        for item in items.values {
            let bytes = unsafe { item.value.Anonymous.largeValue };
            if bytes < 0 {
                issues.push(CounterItemIssue {
                    instance_name: Some(item.name),
                    error: GpuSampleError::InvalidData {
                        context: "GPU memory usage value",
                    },
                });
            } else {
                values.push(MemoryReading {
                    instance_name: item.name,
                    bytes,
                });
            }
        }
        CounterRead { values, issues }
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

struct CounterArrayRead {
    values: Vec<CounterArrayItem>,
    issues: Vec<CounterItemIssue>,
}

fn query_counter_array(
    counter: PDH_HCOUNTER,
    format: u32,
    storage: &mut Vec<usize>,
) -> Result<CounterArrayRead, GpuSampleError> {
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
            return Ok(CounterArrayRead {
                values: Vec::new(),
                issues: Vec::new(),
            });
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
        let mut values = Vec::with_capacity(items.len());
        let mut issues = Vec::new();
        for item in items {
            let name = match read_bounded_wide_string(item.szName, base, end) {
                Ok(name) => name,
                Err(error) => {
                    issues.push(CounterItemIssue {
                        instance_name: None,
                        error,
                    });
                    continue;
                }
            };
            if !matches!(
                item.FmtValue.CStatus,
                PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA
            ) {
                issues.push(CounterItemIssue {
                    instance_name: Some(name),
                    error: GpuSampleError::Pdh {
                        context: "GPU PDH counter value status",
                        status: item.FmtValue.CStatus,
                    },
                });
                continue;
            }
            values.push(CounterArrayItem {
                name,
                value: item.FmtValue,
            });
        }
        Ok(CounterArrayRead { values, issues })
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

#[cfg(test)]
pub(super) fn assemble_samples(
    infos: &[Arc<GpuAdapterInfo>],
    known_luids: &HashSet<AdapterLuid>,
    engine_readings: Vec<EngineReading>,
    dedicated_readings: Vec<MemoryReading>,
    shared_readings: Vec<MemoryReading>,
    temperatures: HashMap<GpuAdapterId, Result<Option<u32>, GpuSampleError>>,
) -> Result<Vec<GpuAdapterSample>, GpuSampleError> {
    assemble_counter_samples(
        infos,
        known_luids,
        CounterRead::values(engine_readings),
        CounterRead::values(dedicated_readings),
        CounterRead::values(shared_readings),
        temperatures,
    )
}

fn assemble_counter_samples(
    infos: &[Arc<GpuAdapterInfo>],
    known_luids: &HashSet<AdapterLuid>,
    engine_readings: CounterRead<EngineReading>,
    dedicated_readings: CounterRead<MemoryReading>,
    shared_readings: CounterRead<MemoryReading>,
    mut temperatures: HashMap<GpuAdapterId, Result<Option<u32>, GpuSampleError>>,
) -> Result<Vec<GpuAdapterSample>, GpuSampleError> {
    let displayed_ids: HashSet<_> = infos.iter().map(|info| info.id).collect();
    let mut row_issues = HashMap::<GpuAdapterId, Vec<GpuSampleIssue>>::new();
    let mut global_issues = Vec::<GpuSampleIssue>::new();
    collect_counter_issues(
        GpuSampleSource::Engine,
        engine_readings.issues,
        &displayed_ids,
        &mut row_issues,
        &mut global_issues,
    );

    let mut engine_instances = HashSet::new();
    let mut engines: HashMap<GpuEngineId, (GpuEngineKind, f64)> = HashMap::new();
    for reading in engine_readings.values {
        let parsed = match parse_engine_instance(&reading.instance_name) {
            Ok(parsed) => parsed,
            Err(error) => {
                global_issues.push(GpuSampleIssue::new(
                    None,
                    GpuSampleSource::Engine,
                    Some(reading.instance_name),
                    error,
                ));
                continue;
            }
        };
        if !known_luids.contains(&parsed.id.adapter.luid) {
            global_issues.push(GpuSampleIssue::new(
                Some(parsed.id.adapter),
                GpuSampleSource::Engine,
                Some(reading.instance_name),
                GpuSampleError::InvalidData {
                    context: "GPU engine references unknown LUID",
                },
            ));
            continue;
        }
        if !displayed_ids.contains(&parsed.id.adapter) {
            continue;
        }
        if !engine_instances.insert(parsed.clone()) {
            row_issues
                .entry(parsed.id.adapter)
                .or_default()
                .push(GpuSampleIssue::new(
                    Some(parsed.id.adapter),
                    GpuSampleSource::Engine,
                    Some(reading.instance_name),
                    GpuSampleError::InvalidData {
                        context: "duplicate GPU engine process instance",
                    },
                ));
            continue;
        }
        if !reading.utilization.is_finite() || reading.utilization < 0.0 {
            row_issues
                .entry(parsed.id.adapter)
                .or_default()
                .push(GpuSampleIssue::new(
                    Some(parsed.id.adapter),
                    GpuSampleSource::Engine,
                    Some(reading.instance_name),
                    GpuSampleError::InvalidData {
                        context: "GPU engine utilization value",
                    },
                ));
            continue;
        }
        if engines
            .get(&parsed.id)
            .is_some_and(|(kind, _)| kind != &parsed.kind)
        {
            row_issues
                .entry(parsed.id.adapter)
                .or_default()
                .push(GpuSampleIssue::new(
                    Some(parsed.id.adapter),
                    GpuSampleSource::Engine,
                    Some(reading.instance_name),
                    GpuSampleError::InvalidData {
                        context: "GPU engine type changed within one snapshot",
                    },
                ));
            continue;
        }
        let entry = engines
            .entry(parsed.id)
            .or_insert_with(|| (parsed.kind.clone(), 0.0));
        let aggregate = entry.1 + reading.utilization;
        if aggregate.is_finite() {
            entry.1 = aggregate;
        } else {
            engines.remove(&parsed.id);
            row_issues
                .entry(parsed.id.adapter)
                .or_default()
                .push(GpuSampleIssue::new(
                    Some(parsed.id.adapter),
                    GpuSampleSource::Engine,
                    Some(reading.instance_name),
                    GpuSampleError::InvalidData {
                        context: "GPU engine utilization aggregate",
                    },
                ));
        }
    }

    let dedicated = collect_memory_readings(
        known_luids,
        &displayed_ids,
        dedicated_readings,
        GpuSampleSource::DedicatedMemory,
        "duplicate dedicated GPU memory instance",
        &mut row_issues,
        &mut global_issues,
    );
    let shared = collect_memory_readings(
        known_luids,
        &displayed_ids,
        shared_readings,
        GpuSampleSource::SharedMemory,
        "duplicate shared GPU memory instance",
        &mut row_issues,
        &mut global_issues,
    );

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
        let mut row_errors = global_issues.clone();
        row_errors.extend(row_issues.remove(&info.id).unwrap_or_default());
        let temperature_deci_c = match temperatures.remove(&info.id) {
            Some(Ok(value)) => value,
            Some(Err(error)) => {
                row_errors.push(GpuSampleIssue::new(
                    Some(info.id),
                    GpuSampleSource::Temperature,
                    None,
                    error,
                ));
                None
            }
            None => {
                row_errors.push(GpuSampleIssue::new(
                    Some(info.id),
                    GpuSampleSource::Temperature,
                    None,
                    GpuSampleError::InvalidData {
                        context: "missing GPU temperature query result",
                    },
                ));
                None
            }
        };
        let dedicated_usage_bytes = dedicated.get(&info.id).copied();
        if dedicated_usage_bytes.is_none() {
            row_errors.push(GpuSampleIssue::new(
                Some(info.id),
                GpuSampleSource::DedicatedMemory,
                None,
                GpuSampleError::InvalidData {
                    context: "missing dedicated GPU memory instance",
                },
            ));
        }
        let shared_usage_bytes = shared.get(&info.id).copied();
        if shared_usage_bytes.is_none() {
            row_errors.push(GpuSampleIssue::new(
                Some(info.id),
                GpuSampleSource::SharedMemory,
                None,
                GpuSampleError::InvalidData {
                    context: "missing shared GPU memory instance",
                },
            ));
        }
        let engine_complete = !row_errors
            .iter()
            .any(|issue| issue.source == GpuSampleSource::Engine);
        let overall_utilization_percent = engine_complete.then(|| {
            adapter_engines
                .iter()
                .map(|engine| engine.utilization_percent)
                .max()
                .unwrap_or(0)
        });
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
    samples: &mut [GpuAdapterSample],
) -> HashMap<GpuEngineId, GpuEngineKind> {
    let mut candidate = existing.clone();
    for sample in samples {
        let mut valid_engines = Vec::with_capacity(sample.engines.len());
        for engine in sample.engines.drain(..) {
            match candidate.get(&engine.id) {
                Some(kind) if kind != &engine.kind => {
                    sample.row_errors.push(GpuSampleIssue::new(
                        Some(sample.info.id),
                        GpuSampleSource::Engine,
                        None,
                        GpuSampleError::InvalidData {
                            context: "GPU engine type changed without a topology generation",
                        },
                    ));
                    sample.overall_utilization_percent = None;
                }
                Some(_) => valid_engines.push(engine),
                None => {
                    candidate.insert(engine.id, engine.kind.clone());
                    valid_engines.push(engine);
                }
            }
        }
        sample.engines = valid_engines;
    }
    candidate
}

fn collect_memory_readings(
    known_luids: &HashSet<AdapterLuid>,
    displayed_ids: &HashSet<GpuAdapterId>,
    readings: CounterRead<MemoryReading>,
    source: GpuSampleSource,
    duplicate_context: &'static str,
    row_issues: &mut HashMap<GpuAdapterId, Vec<GpuSampleIssue>>,
    global_issues: &mut Vec<GpuSampleIssue>,
) -> HashMap<GpuAdapterId, u64> {
    collect_counter_issues(
        source,
        readings.issues,
        displayed_ids,
        row_issues,
        global_issues,
    );
    let mut values = HashMap::new();
    let mut invalid_ids = HashSet::new();
    for reading in readings.values {
        let id = match parse_memory_instance(&reading.instance_name) {
            Ok(id) => id,
            Err(error) => {
                global_issues.push(GpuSampleIssue::new(
                    None,
                    source,
                    Some(reading.instance_name),
                    error,
                ));
                continue;
            }
        };
        if !known_luids.contains(&id.luid) {
            global_issues.push(GpuSampleIssue::new(
                Some(id),
                source,
                Some(reading.instance_name),
                GpuSampleError::InvalidData {
                    context: "GPU memory references unknown LUID",
                },
            ));
            continue;
        }
        if !displayed_ids.contains(&id) {
            continue;
        }
        let bytes = match u64::try_from(reading.bytes) {
            Ok(bytes) => bytes,
            Err(_) => {
                invalid_ids.insert(id);
                values.remove(&id);
                row_issues.entry(id).or_default().push(GpuSampleIssue::new(
                    Some(id),
                    source,
                    Some(reading.instance_name),
                    GpuSampleError::InvalidData {
                        context: "GPU memory usage conversion",
                    },
                ));
                continue;
            }
        };
        if invalid_ids.contains(&id) || values.insert(id, bytes).is_some() {
            invalid_ids.insert(id);
            values.remove(&id);
            row_issues.entry(id).or_default().push(GpuSampleIssue::new(
                Some(id),
                source,
                Some(reading.instance_name),
                GpuSampleError::InvalidData {
                    context: duplicate_context,
                },
            ));
        }
    }
    values
}

fn collect_counter_issues(
    source: GpuSampleSource,
    issues: Vec<CounterItemIssue>,
    displayed_ids: &HashSet<GpuAdapterId>,
    row_issues: &mut HashMap<GpuAdapterId, Vec<GpuSampleIssue>>,
    global_issues: &mut Vec<GpuSampleIssue>,
) {
    for issue in issues {
        let adapter_id = issue
            .instance_name
            .as_deref()
            .and_then(|name| adapter_id_from_instance(source, name));
        let issue = GpuSampleIssue::new(adapter_id, source, issue.instance_name, issue.error);
        if let Some(adapter_id) = adapter_id.filter(|id| displayed_ids.contains(id)) {
            row_issues.entry(adapter_id).or_default().push(issue);
        } else {
            global_issues.push(issue);
        }
    }
}

fn adapter_id_from_instance(source: GpuSampleSource, instance_name: &str) -> Option<GpuAdapterId> {
    match source {
        GpuSampleSource::Engine => parse_engine_instance(instance_name)
            .ok()
            .map(|parsed| parsed.id.adapter),
        GpuSampleSource::DedicatedMemory | GpuSampleSource::SharedMemory => {
            parse_memory_instance(instance_name).ok()
        }
        GpuSampleSource::Temperature => None,
    }
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

#[cfg(test)]
mod partial_failure_tests {
    use super::*;
    use windows_sys::Win32::System::Performance::PDH_CSTATUS_NO_COUNTER;

    fn adapter(id: GpuAdapterId) -> Arc<GpuAdapterInfo> {
        Arc::new(GpuAdapterInfo {
            id,
            enumeration_index: 0,
            name: "Partial GPU".to_string(),
            vendor_id: 0,
            device_id: 0,
            subsystem_id: 0,
            revision: 0,
            dedicated_limit_bytes: Some(1024),
            shared_limit_bytes: Some(1024),
        })
    }

    #[test]
    fn unsupported_memory_counter_keeps_independent_engine_and_shared_data() {
        let id = GpuAdapterId {
            luid: AdapterLuid::from_parts(0, 0x71),
            physical_index: 0,
        };
        let samples = assemble_counter_samples(
            &[adapter(id)],
            &HashSet::from([id.luid]),
            CounterRead::values(vec![EngineReading {
                instance_name: "pid_1_luid_0x0_0x71_phys_0_eng_0_engtype_3d".to_string(),
                utilization: 42.0,
            }]),
            CounterRead::source_error(GpuSampleError::Pdh {
                context: DEDICATED_MEMORY_COUNTER_PATH,
                status: PDH_CSTATUS_NO_COUNTER,
            }),
            CounterRead::values(vec![MemoryReading {
                instance_name: "luid_0x0_0x71_phys_0".to_string(),
                bytes: 256,
            }]),
            HashMap::from([(id, Ok(None))]),
        )
        .unwrap();

        assert_eq!(samples[0].overall_utilization_percent, Some(42));
        assert_eq!(samples[0].dedicated_usage_bytes, None);
        assert_eq!(samples[0].shared_usage_bytes, Some(256));
        let issue = samples[0]
            .row_errors
            .iter()
            .find(|issue| issue.source == GpuSampleSource::DedicatedMemory)
            .unwrap();
        assert_eq!(
            issue.source.counter_path(),
            Some(DEDICATED_MEMORY_COUNTER_PATH)
        );
        assert!(issue.error.is_unsupported());
    }
}
