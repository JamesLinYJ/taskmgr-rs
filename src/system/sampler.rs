// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 系统性能采样器
//
//   文件:       src/system/sampler.rs
//
//   日期:       2026年07月16日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Coherent system-wide CPU and memory sampling on one persistent worker.
//!
//! The collector owns cumulative CPU baselines. A candidate is committed only after both the
//! processor and `PERFORMANCE_INFORMATION` queries succeed, so consumers never combine values
//! from different refreshes.

use std::mem::{size_of, zeroed};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_GEN_FAILURE, ERROR_INVALID_DATA, GetLastError, HWND,
};
use windows_sys::Win32::System::ProcessStatus::{K32GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;

use crate::infrastructure::worker::{SingleFlightWorker, keep_pending};
use crate::system::cpu_sampler::{ProcessorPerformance, query_processor_performance};
use crate::system::cpu_topology::{ProcessorTopology, query_processor_topology};
use crate::ui::resource_ids::PWM_SYSTEM_WORKER_COMPLETE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SystemSampleError {
    NtStatus(i32),
    Win32(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CpuDiagnosticError {
    BaselineUnavailable,
    CounterRegression,
    InvalidCounterRelationship,
    InvalidElapsedTime,
    ArithmeticOverflow,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CpuDiagnosticSample {
    pub(crate) user_usage: u8,
    pub(crate) kernel_usage: u8,
    pub(crate) dpc_usage: u8,
    pub(crate) interrupt_usage: u8,
    pub(crate) interrupts_per_second: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SystemSample {
    pub(crate) processor_count: usize,
    pub(crate) processor_topology: Arc<ProcessorTopology>,
    pub(crate) cpu_delta_valid: bool,
    pub(crate) cpu_usage: u8,
    pub(crate) kernel_usage: u8,
    pub(crate) processor_cpu_usage: Vec<u8>,
    pub(crate) processor_kernel_usage: Vec<u8>,
    pub(crate) cpu_diagnostics: Result<CpuDiagnosticSample, CpuDiagnosticError>,
    pub(crate) uptime_ms: u64,
    pub(crate) physical_mem_usage_kb: u64,
    pub(crate) physical_mem_limit_kb: u64,
    pub(crate) commit_total_kb: u64,
    pub(crate) commit_limit_kb: u64,
    pub(crate) commit_peak_kb: u64,
    pub(crate) total_physical_kb: u64,
    pub(crate) avail_physical_kb: u64,
    pub(crate) file_cache_kb: u64,
    pub(crate) kernel_total_kb: u64,
    pub(crate) kernel_paged_kb: u64,
    pub(crate) kernel_nonpaged_kb: u64,
    pub(crate) handle_count: u32,
    pub(crate) thread_count: u32,
    pub(crate) process_count: u32,
}

pub(crate) type SystemSampleResult = Result<SystemSample, SystemSampleError>;

pub(crate) struct SystemSampler {
    worker: Option<SingleFlightWorker<(), SystemSampleResult>>,
}

impl SystemSampler {
    pub(crate) fn new() -> Self {
        Self { worker: None }
    }

    pub(crate) fn start(&mut self, processor_count: usize) -> Result<(), u32> {
        if self.worker.is_some() {
            return Ok(());
        }
        if processor_count == 0 {
            return Err(ERROR_INVALID_DATA);
        }
        let mut collector = SystemCollector::new(processor_count);
        self.worker = Some(SingleFlightWorker::spawn(
            "taskmgr-rs-system-sampler",
            PWM_SYSTEM_WORKER_COMPLETE,
            keep_pending,
            move |()| collector.collect(),
        )?);
        Ok(())
    }

    pub(crate) fn request(&mut self, notify_hwnd: HWND) -> Result<(), u32> {
        self.worker
            .as_mut()
            .ok_or(ERROR_BROKEN_PIPE)?
            .request((), notify_hwnd)
            .map(|_| ())
    }

    pub(crate) fn drain(&mut self, notify_hwnd: HWND) -> Result<Vec<SystemSampleResult>, u32> {
        let drained = self
            .worker
            .as_mut()
            .ok_or(ERROR_BROKEN_PIPE)?
            .drain(notify_hwnd);
        if let Some(error) = drained.error {
            self.worker = None;
            return Err(error);
        }
        Ok(drained.completions)
    }

    pub(crate) fn stop(&mut self) {
        self.worker = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CpuTimes {
    idle: u64,
    kernel_active: u64,
    user: u64,
    dpc: u64,
    interrupt: u64,
    interrupt_count: u32,
    total: u64,
}

struct SystemCollector {
    expected_processor_count: usize,
    processor_info: Vec<ProcessorPerformance>,
    previous_cpu_times: Vec<CpuTimes>,
    previous_timestamp_ms: Option<u64>,
    processor_topology: Option<Arc<ProcessorTopology>>,
}

impl SystemCollector {
    fn new(expected_processor_count: usize) -> Self {
        Self {
            expected_processor_count,
            processor_info: Vec::with_capacity(expected_processor_count),
            previous_cpu_times: Vec::with_capacity(expected_processor_count),
            previous_timestamp_ms: None,
            processor_topology: None,
        }
    }

    fn collect(&mut self) -> SystemSampleResult {
        query_processor_performance(
            self.expected_processor_count.max(1),
            &mut self.processor_info,
        )
        .map_err(SystemSampleError::NtStatus)?;
        if self.processor_info.is_empty() {
            return Err(SystemSampleError::Win32(ERROR_INVALID_DATA));
        }

        let mut performance = unsafe { zeroed::<PERFORMANCE_INFORMATION>() };
        performance.cb = size_of::<PERFORMANCE_INFORMATION>() as u32;
        if unsafe { K32GetPerformanceInfo(&mut performance, performance.cb) } == 0 {
            return Err(SystemSampleError::Win32(last_error_or_gen_failure()));
        }

        let current_cpu_times = self
            .processor_info
            .iter()
            .map(cpu_times)
            .collect::<Result<Vec<_>, _>>()?;
        let processor_topology = self
            .processor_topology
            .as_ref()
            .filter(|topology| topology.matches_sample_len(current_cpu_times.len()))
            .cloned()
            .unwrap_or_else(|| Arc::new(query_processor_topology(current_cpu_times.len())));
        let timestamp_ms = unsafe { GetTickCount64() };
        let elapsed_ms = self
            .previous_timestamp_ms
            .and_then(|previous| timestamp_ms.checked_sub(previous));
        let cpu_delta =
            calculate_cpu_delta(&current_cpu_times, &self.previous_cpu_times, elapsed_ms);
        let page_size = u64::try_from(performance.PageSize)
            .ok()
            .filter(|value| *value != 0)
            .ok_or(SystemSampleError::Win32(ERROR_INVALID_DATA))?;
        let physical_used_pages = performance
            .PhysicalTotal
            .checked_sub(performance.PhysicalAvailable)
            .ok_or(SystemSampleError::Win32(ERROR_INVALID_DATA))?;

        let sample = SystemSample {
            processor_count: current_cpu_times.len(),
            processor_topology: Arc::clone(&processor_topology),
            cpu_delta_valid: cpu_delta.valid,
            cpu_usage: cpu_delta.cpu_usage,
            kernel_usage: cpu_delta.kernel_usage,
            processor_cpu_usage: cpu_delta.processor_cpu_usage,
            processor_kernel_usage: cpu_delta.processor_kernel_usage,
            cpu_diagnostics: cpu_delta.diagnostics,
            uptime_ms: timestamp_ms,
            physical_mem_usage_kb: pages_to_kb(physical_used_pages, page_size)?,
            physical_mem_limit_kb: pages_to_kb(performance.PhysicalTotal, page_size)?,
            commit_total_kb: pages_to_kb(performance.CommitTotal, page_size)?,
            commit_limit_kb: pages_to_kb(performance.CommitLimit, page_size)?,
            commit_peak_kb: pages_to_kb(performance.CommitPeak, page_size)?,
            total_physical_kb: pages_to_kb(performance.PhysicalTotal, page_size)?,
            avail_physical_kb: pages_to_kb(performance.PhysicalAvailable, page_size)?,
            file_cache_kb: pages_to_kb(performance.SystemCache, page_size)?,
            kernel_total_kb: pages_to_kb(performance.KernelTotal, page_size)?,
            kernel_paged_kb: pages_to_kb(performance.KernelPaged, page_size)?,
            kernel_nonpaged_kb: pages_to_kb(performance.KernelNonpaged, page_size)?,
            handle_count: performance.HandleCount,
            thread_count: performance.ThreadCount,
            process_count: performance.ProcessCount,
        };

        self.expected_processor_count = current_cpu_times.len();
        self.previous_cpu_times = current_cpu_times;
        self.previous_timestamp_ms = Some(timestamp_ms);
        self.processor_topology = Some(processor_topology);
        Ok(sample)
    }
}

struct CpuDelta {
    valid: bool,
    cpu_usage: u8,
    kernel_usage: u8,
    processor_cpu_usage: Vec<u8>,
    processor_kernel_usage: Vec<u8>,
    diagnostics: Result<CpuDiagnosticSample, CpuDiagnosticError>,
}

fn calculate_cpu_delta(
    current: &[CpuTimes],
    previous: &[CpuTimes],
    elapsed_ms: Option<u64>,
) -> CpuDelta {
    let mut delta = CpuDelta {
        valid: false,
        cpu_usage: 0,
        kernel_usage: 0,
        processor_cpu_usage: vec![0; current.len()],
        processor_kernel_usage: vec![0; current.len()],
        diagnostics: Err(CpuDiagnosticError::BaselineUnavailable),
    };
    if current.is_empty() || current.len() != previous.len() {
        return delta;
    }

    let mut idle_sum = 0u128;
    let mut kernel_sum = 0u128;
    let mut user_sum = 0u128;
    let mut dpc_sum = 0u128;
    let mut interrupt_sum = 0u128;
    let mut interrupt_count_sum = 0u128;
    let mut total_sum = 0u128;
    let mut diagnostic_error = None;
    for (index, (current, previous)) in current.iter().zip(previous).enumerate() {
        let (Some(idle), Some(kernel), Some(total)) = (
            current.idle.checked_sub(previous.idle),
            current.kernel_active.checked_sub(previous.kernel_active),
            current.total.checked_sub(previous.total),
        ) else {
            delta.diagnostics = Err(CpuDiagnosticError::CounterRegression);
            return delta;
        };
        if idle > total || kernel > total {
            delta.diagnostics = Err(CpuDiagnosticError::InvalidCounterRelationship);
            return delta;
        }
        delta.processor_cpu_usage[index] = percent(total - idle, total);
        delta.processor_kernel_usage[index] = percent(kernel, total);
        let (Some(next_idle_sum), Some(next_kernel_sum), Some(next_total_sum)) = (
            idle_sum.checked_add(u128::from(idle)),
            kernel_sum.checked_add(u128::from(kernel)),
            total_sum.checked_add(u128::from(total)),
        ) else {
            delta.diagnostics = Err(CpuDiagnosticError::ArithmeticOverflow);
            return delta;
        };
        idle_sum = next_idle_sum;
        kernel_sum = next_kernel_sum;
        total_sum = next_total_sum;

        let (Some(user), Some(dpc), Some(interrupt), Some(interrupt_count)) = (
            current.user.checked_sub(previous.user),
            current.dpc.checked_sub(previous.dpc),
            current.interrupt.checked_sub(previous.interrupt),
            current
                .interrupt_count
                .checked_sub(previous.interrupt_count),
        ) else {
            diagnostic_error.get_or_insert(CpuDiagnosticError::CounterRegression);
            continue;
        };
        if user > total
            || dpc > kernel
            || interrupt > kernel
            || dpc
                .checked_add(interrupt)
                .is_none_or(|value| value > kernel)
        {
            diagnostic_error.get_or_insert(CpuDiagnosticError::InvalidCounterRelationship);
            continue;
        }
        let (
            Some(next_user_sum),
            Some(next_dpc_sum),
            Some(next_interrupt_sum),
            Some(next_interrupt_count_sum),
        ) = (
            user_sum.checked_add(u128::from(user)),
            dpc_sum.checked_add(u128::from(dpc)),
            interrupt_sum.checked_add(u128::from(interrupt)),
            interrupt_count_sum.checked_add(u128::from(interrupt_count)),
        )
        else {
            diagnostic_error.get_or_insert(CpuDiagnosticError::ArithmeticOverflow);
            continue;
        };
        user_sum = next_user_sum;
        dpc_sum = next_dpc_sum;
        interrupt_sum = next_interrupt_sum;
        interrupt_count_sum = next_interrupt_count_sum;
    }
    if total_sum == 0 || idle_sum > total_sum || kernel_sum > total_sum {
        return delta;
    }

    let (Some(cpu_usage), Some(kernel_usage)) = (
        checked_percent_u128(total_sum - idle_sum, total_sum),
        checked_percent_u128(kernel_sum, total_sum),
    ) else {
        return delta;
    };
    delta.valid = true;
    delta.cpu_usage = cpu_usage;
    delta.kernel_usage = kernel_usage;
    if let Some(error) = diagnostic_error {
        delta.diagnostics = Err(error);
        return delta;
    }
    let Some(elapsed_ms) = elapsed_ms.filter(|elapsed| *elapsed != 0) else {
        delta.diagnostics = Err(CpuDiagnosticError::InvalidElapsedTime);
        return delta;
    };
    let (Some(user_usage), Some(dpc_usage), Some(interrupt_usage)) = (
        checked_percent_u128(user_sum, total_sum),
        checked_percent_u128(dpc_sum, total_sum),
        checked_percent_u128(interrupt_sum, total_sum),
    ) else {
        delta.diagnostics = Err(CpuDiagnosticError::ArithmeticOverflow);
        return delta;
    };
    let Some(interrupts_per_second) = interrupt_count_sum
        .checked_mul(1000)
        .and_then(|value| value.checked_add(u128::from(elapsed_ms / 2)))
        .and_then(|value| value.checked_div(u128::from(elapsed_ms)))
        .and_then(|value| u64::try_from(value).ok())
    else {
        delta.diagnostics = Err(CpuDiagnosticError::ArithmeticOverflow);
        return delta;
    };
    delta.diagnostics = Ok(CpuDiagnosticSample {
        user_usage,
        kernel_usage,
        dpc_usage,
        interrupt_usage,
        interrupts_per_second,
    });
    delta
}

fn cpu_times(processor: &ProcessorPerformance) -> Result<CpuTimes, SystemSampleError> {
    let idle = u64::try_from(processor.idle_time)
        .map_err(|_| SystemSampleError::Win32(ERROR_INVALID_DATA))?;
    let kernel = u64::try_from(processor.kernel_time)
        .map_err(|_| SystemSampleError::Win32(ERROR_INVALID_DATA))?;
    let user = u64::try_from(processor.user_time)
        .map_err(|_| SystemSampleError::Win32(ERROR_INVALID_DATA))?;
    let dpc = u64::try_from(processor.dpc_time)
        .map_err(|_| SystemSampleError::Win32(ERROR_INVALID_DATA))?;
    let interrupt = u64::try_from(processor.interrupt_time)
        .map_err(|_| SystemSampleError::Win32(ERROR_INVALID_DATA))?;
    let kernel_active = kernel
        .checked_sub(idle)
        .ok_or(SystemSampleError::Win32(ERROR_INVALID_DATA))?;
    let total = kernel
        .checked_add(user)
        .ok_or(SystemSampleError::Win32(ERROR_INVALID_DATA))?;
    Ok(CpuTimes {
        idle,
        kernel_active,
        user,
        dpc,
        interrupt,
        interrupt_count: processor.interrupt_count,
        total,
    })
}

fn pages_to_kb(pages: usize, page_size: u64) -> Result<u64, SystemSampleError> {
    let pages = u64::try_from(pages).map_err(|_| SystemSampleError::Win32(ERROR_INVALID_DATA))?;
    pages
        .checked_mul(page_size)
        .map(|bytes| bytes / 1024)
        .ok_or(SystemSampleError::Win32(ERROR_INVALID_DATA))
}

fn percent(numerator: u64, denominator: u64) -> u8 {
    if denominator == 0 {
        0
    } else {
        ((u128::from(numerator) * 100) / u128::from(denominator)).min(100) as u8
    }
}

fn checked_percent_u128(numerator: u128, denominator: u128) -> Option<u8> {
    if denominator == 0 {
        Some(0)
    } else {
        let percent = numerator.checked_mul(100)?.checked_div(denominator)?;
        u8::try_from(percent.min(100)).ok()
    }
}

fn last_error_or_gen_failure() -> u32 {
    let error = unsafe { GetLastError() };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn times(
        idle: u64,
        kernel_active: u64,
        user: u64,
        dpc: u64,
        interrupt: u64,
        interrupt_count: u32,
    ) -> CpuTimes {
        CpuTimes {
            idle,
            kernel_active,
            user,
            dpc,
            interrupt,
            interrupt_count,
            total: idle + kernel_active + user,
        }
    }

    #[test]
    fn first_cpu_sample_only_establishes_a_baseline() {
        let current = [times(100, 50, 50, 5, 2, 10)];
        let delta = calculate_cpu_delta(&current, &[], None);
        assert!(!delta.valid);
        assert_eq!(delta.cpu_usage, 0);
        assert_eq!(
            delta.diagnostics,
            Err(CpuDiagnosticError::BaselineUnavailable)
        );
    }

    #[test]
    fn cpu_delta_aggregates_processors_without_averaging_percentages() {
        let previous = [
            times(100, 100, 100, 10, 5, 100),
            times(50, 50, 100, 4, 2, 50),
        ];
        let current = [
            times(120, 140, 140, 18, 8, 120),
            times(130, 60, 110, 6, 4, 60),
        ];
        let delta = calculate_cpu_delta(&current, &previous, Some(1000));
        assert!(delta.valid);
        assert_eq!(delta.processor_cpu_usage, vec![80, 20]);
        assert_eq!(delta.cpu_usage, 50);
        assert_eq!(delta.kernel_usage, 25);
        assert_eq!(
            delta.diagnostics,
            Ok(CpuDiagnosticSample {
                user_usage: 25,
                kernel_usage: 25,
                dpc_usage: 5,
                interrupt_usage: 2,
                interrupts_per_second: 30,
            })
        );
    }

    #[test]
    fn regressed_cpu_counter_restarts_the_baseline_without_a_false_spike() {
        let previous = [times(100, 100, 100, 10, 5, 100)];
        let current = [times(90, 110, 120, 12, 6, 110)];
        assert!(!calculate_cpu_delta(&current, &previous, Some(1000)).valid);
    }

    #[test]
    fn regressed_interrupt_counter_keeps_basic_cpu_delta_but_rejects_diagnostics() {
        let previous = [times(100, 100, 100, 10, 5, 100)];
        let current = [times(120, 140, 140, 18, 8, 90)];
        let delta = calculate_cpu_delta(&current, &previous, Some(1000));
        assert!(delta.valid);
        assert_eq!(
            delta.diagnostics,
            Err(CpuDiagnosticError::CounterRegression)
        );
    }

    #[test]
    fn zero_elapsed_time_never_fabricates_an_interrupt_rate() {
        let previous = [times(100, 100, 100, 10, 5, 100)];
        let current = [times(120, 140, 140, 18, 8, 120)];
        let delta = calculate_cpu_delta(&current, &previous, Some(0));
        assert!(delta.valid);
        assert_eq!(
            delta.diagnostics,
            Err(CpuDiagnosticError::InvalidElapsedTime)
        );
    }

    #[test]
    fn page_conversion_reports_overflow() {
        assert_eq!(
            pages_to_kb(usize::MAX, u64::MAX),
            Err(SystemSampleError::Win32(ERROR_INVALID_DATA))
        );
    }
}
