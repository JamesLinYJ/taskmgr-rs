// +-------------------------------------------------------------------------
//
//   taskmgr-rs - CPU PDH 性能计数器
//
//   文件:       src/pages/cpu/pdh.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 持久拥有 CPU 频率和系统速率 PDH query，并严格执行双样本基线语义。
//!
//! 逻辑处理器实例必须与当前拓扑一一对应；缺失、重复或非法值会拒绝整轮结果。

use std::collections::{BTreeSet, HashMap};
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;
use std::slice;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE,
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_FMT_LARGE, PDH_HCOUNTER, PDH_HQUERY,
    PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData,
    PdhGetFormattedCounterArrayW, PdhGetFormattedCounterValue, PdhOpenQueryW,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;

use super::model::{CpuDetailError, CpuDynamicInfo, invalid, invalid_error};
use crate::infrastructure::native::{record_pdh_error, to_wide_null};
use crate::system::cpu_topology::LogicalProcessorId;

const FREQUENCY_COUNTER_PATH: &str = r"\Processor Information(*)\Processor Frequency";
const PERFORMANCE_COUNTER_PATH: &str = r"\Processor Information(*)\% Processor Performance";
const CONTEXT_SWITCH_COUNTER_PATH: &str = r"\System\Context Switches/sec";
const SYSTEM_CALL_COUNTER_PATH: &str = r"\System\System Calls/sec";
const PROCESSOR_QUEUE_COUNTER_PATH: &str = r"\System\Processor Queue Length";
const MAX_PDH_ARRAY_BYTES: u32 = 64 * 1024 * 1024;

pub(super) struct CpuPdhQuery {
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

pub(super) enum CpuPdhCollectOutcome {
    Primed(u64),
    Sample(CpuDynamicInfo),
}

impl CpuPdhQuery {
    pub(super) fn new(
        expected_processors: Vec<LogicalProcessorId>,
    ) -> Result<Self, CpuDetailError> {
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

    pub(super) fn reset_sample_baseline(&mut self) {
        self.sample_baseline_ready = false;
    }

    pub(super) fn collect(&mut self) -> Result<CpuPdhCollectOutcome, CpuDetailError> {
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
pub(super) struct PdhArrayValue<T> {
    pub(super) instance: String,
    pub(super) value: T,
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
pub(super) fn validate_frequencies(
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

pub(super) fn effective_frequency_mhz(
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
pub(super) struct PdhProcessorInstance {
    // Processor Information instances are documented as NUMA node and node-local index.
    pub(super) numa_node: u32,
    pub(super) numa_index: u32,
}

pub(super) fn parse_processor_instance(
    value: &str,
) -> Result<Option<PdhProcessorInstance>, CpuDetailError> {
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
