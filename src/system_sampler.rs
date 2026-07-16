// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 系统性能采样器
//
//   文件:       src/system_sampler.rs
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
use std::sync::mpsc::TryRecvError;

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_GEN_FAILURE, ERROR_INVALID_DATA, GetLastError, HWND,
};
use windows_sys::Win32::System::ProcessStatus::{K32GetPerformanceInfo, PERFORMANCE_INFORMATION};

use crate::background_worker::BackgroundWorker;
use crate::cpu_sampler::{ProcessorPerformance, query_processor_performance};
use crate::resource::PWM_SYSTEM_WORKER_COMPLETE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SystemSampleError {
    NtStatus(i32),
    Win32(u32),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SystemSample {
    pub(crate) processor_count: usize,
    pub(crate) cpu_delta_valid: bool,
    pub(crate) cpu_usage: u8,
    pub(crate) kernel_usage: u8,
    pub(crate) processor_cpu_usage: Vec<u8>,
    pub(crate) processor_kernel_usage: Vec<u8>,
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
    worker: Option<BackgroundWorker<(), SystemSampleResult>>,
    collection_in_flight: bool,
    refresh_requested: bool,
}

impl SystemSampler {
    pub(crate) fn new() -> Self {
        Self {
            worker: None,
            collection_in_flight: false,
            refresh_requested: false,
        }
    }

    pub(crate) fn start(&mut self, processor_count: usize) -> Result<(), u32> {
        if self.worker.is_some() {
            return Ok(());
        }
        if processor_count == 0 {
            return Err(ERROR_INVALID_DATA);
        }
        let mut collector = SystemCollector::new(processor_count);
        self.worker = Some(BackgroundWorker::spawn(
            "taskmgr-rs-system-sampler",
            PWM_SYSTEM_WORKER_COMPLETE,
            move |()| collector.collect(),
        )?);
        Ok(())
    }

    pub(crate) fn request(&mut self, notify_hwnd: HWND) -> Result<(), u32> {
        if self.collection_in_flight {
            self.refresh_requested = true;
            return Ok(());
        }
        self.submit(notify_hwnd)
    }

    pub(crate) fn drain(&mut self, notify_hwnd: HWND) -> Result<Vec<SystemSampleResult>, u32> {
        let mut completions = Vec::new();
        loop {
            let result = match self.worker.as_ref() {
                Some(worker) => worker.try_recv(),
                None => return Err(ERROR_BROKEN_PIPE),
            };
            match result {
                Ok(completion) => {
                    self.collection_in_flight = false;
                    completions.push(completion);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.worker = None;
                    self.collection_in_flight = false;
                    self.refresh_requested = false;
                    return Err(ERROR_BROKEN_PIPE);
                }
            }
        }

        if self.refresh_requested && !self.collection_in_flight {
            self.refresh_requested = false;
            self.submit(notify_hwnd)?;
        }
        Ok(completions)
    }

    pub(crate) fn stop(&mut self) {
        self.worker = None;
        self.collection_in_flight = false;
        self.refresh_requested = false;
    }

    fn submit(&mut self, notify_hwnd: HWND) -> Result<(), u32> {
        let Some(worker) = self.worker.as_ref() else {
            return Err(ERROR_BROKEN_PIPE);
        };
        if let Err(error) = worker.submit((), notify_hwnd) {
            self.worker = None;
            self.collection_in_flight = false;
            return Err(error);
        }
        self.collection_in_flight = true;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CpuTimes {
    idle: u64,
    kernel_active: u64,
    total: u64,
}

struct SystemCollector {
    expected_processor_count: usize,
    processor_info: Vec<ProcessorPerformance>,
    previous_cpu_times: Vec<CpuTimes>,
}

impl SystemCollector {
    fn new(expected_processor_count: usize) -> Self {
        Self {
            expected_processor_count,
            processor_info: Vec::with_capacity(expected_processor_count),
            previous_cpu_times: Vec::with_capacity(expected_processor_count),
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
        let cpu_delta = calculate_cpu_delta(&current_cpu_times, &self.previous_cpu_times);
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
            cpu_delta_valid: cpu_delta.valid,
            cpu_usage: cpu_delta.cpu_usage,
            kernel_usage: cpu_delta.kernel_usage,
            processor_cpu_usage: cpu_delta.processor_cpu_usage,
            processor_kernel_usage: cpu_delta.processor_kernel_usage,
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
        Ok(sample)
    }
}

struct CpuDelta {
    valid: bool,
    cpu_usage: u8,
    kernel_usage: u8,
    processor_cpu_usage: Vec<u8>,
    processor_kernel_usage: Vec<u8>,
}

fn calculate_cpu_delta(current: &[CpuTimes], previous: &[CpuTimes]) -> CpuDelta {
    let mut delta = CpuDelta {
        valid: false,
        cpu_usage: 0,
        kernel_usage: 0,
        processor_cpu_usage: vec![0; current.len()],
        processor_kernel_usage: vec![0; current.len()],
    };
    if current.is_empty() || current.len() != previous.len() {
        return delta;
    }

    let mut idle_sum = 0u128;
    let mut kernel_sum = 0u128;
    let mut total_sum = 0u128;
    for (index, (current, previous)) in current.iter().zip(previous).enumerate() {
        let (Some(idle), Some(kernel), Some(total)) = (
            current.idle.checked_sub(previous.idle),
            current.kernel_active.checked_sub(previous.kernel_active),
            current.total.checked_sub(previous.total),
        ) else {
            return delta;
        };
        if idle > total || kernel > total {
            return delta;
        }
        delta.processor_cpu_usage[index] = percent(total - idle, total);
        delta.processor_kernel_usage[index] = percent(kernel, total);
        let (Some(next_idle_sum), Some(next_kernel_sum), Some(next_total_sum)) = (
            idle_sum.checked_add(u128::from(idle)),
            kernel_sum.checked_add(u128::from(kernel)),
            total_sum.checked_add(u128::from(total)),
        ) else {
            return delta;
        };
        idle_sum = next_idle_sum;
        kernel_sum = next_kernel_sum;
        total_sum = next_total_sum;
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
    delta
}

fn cpu_times(processor: &ProcessorPerformance) -> Result<CpuTimes, SystemSampleError> {
    let idle = u64::try_from(processor.idle_time)
        .map_err(|_| SystemSampleError::Win32(ERROR_INVALID_DATA))?;
    let kernel = u64::try_from(processor.kernel_time)
        .map_err(|_| SystemSampleError::Win32(ERROR_INVALID_DATA))?;
    let user = u64::try_from(processor.user_time)
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

    #[test]
    fn first_cpu_sample_only_establishes_a_baseline() {
        let current = [CpuTimes {
            idle: 100,
            kernel_active: 50,
            total: 200,
        }];
        let delta = calculate_cpu_delta(&current, &[]);
        assert!(!delta.valid);
        assert_eq!(delta.cpu_usage, 0);
    }

    #[test]
    fn cpu_delta_aggregates_processors_without_averaging_percentages() {
        let previous = [
            CpuTimes {
                idle: 100,
                kernel_active: 100,
                total: 300,
            },
            CpuTimes {
                idle: 50,
                kernel_active: 50,
                total: 200,
            },
        ];
        let current = [
            CpuTimes {
                idle: 120,
                kernel_active: 140,
                total: 400,
            },
            CpuTimes {
                idle: 130,
                kernel_active: 60,
                total: 300,
            },
        ];
        let delta = calculate_cpu_delta(&current, &previous);
        assert!(delta.valid);
        assert_eq!(delta.processor_cpu_usage, vec![80, 20]);
        assert_eq!(delta.cpu_usage, 50);
        assert_eq!(delta.kernel_usage, 25);
    }

    #[test]
    fn regressed_cpu_counter_restarts_the_baseline_without_a_false_spike() {
        let previous = [CpuTimes {
            idle: 100,
            kernel_active: 100,
            total: 300,
        }];
        let current = [CpuTimes {
            idle: 90,
            kernel_active: 110,
            total: 320,
        }];
        assert!(!calculate_cpu_delta(&current, &previous).valid);
    }

    #[test]
    fn page_conversion_reports_overflow() {
        assert_eq!(
            pages_to_kb(usize::MAX, u64::MAX),
            Err(SystemSampleError::Win32(ERROR_INVALID_DATA))
        );
    }
}
