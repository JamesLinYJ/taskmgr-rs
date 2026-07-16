use std::ffi::c_void;
use std::mem::size_of;

const PROCESSOR_PERFORMANCE_INFORMATION_CLASS: i32 = 8;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;
const STATUS_INVALID_BUFFER_SIZE: i32 = 0xC000_0206u32 as i32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct ProcessorPerformance {
    pub idle_time: i64,
    pub kernel_time: i64,
    pub user_time: i64,
    pub dpc_time: i64,
    pub interrupt_time: i64,
    pub interrupt_count: u32,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        system_information_class: i32,
        system_information: *mut c_void,
        system_information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

pub(crate) fn query_processor_performance(
    expected_count: usize,
    output: &mut Vec<ProcessorPerformance>,
) -> Result<(), i32> {
    let item_size = size_of::<ProcessorPerformance>();
    let mut count = expected_count.max(1);

    loop {
        output.resize(count, ProcessorPerformance::default());
        let Some(byte_len) = count
            .checked_mul(item_size)
            .and_then(|value| u32::try_from(value).ok())
        else {
            return Err(STATUS_INVALID_BUFFER_SIZE);
        };
        let mut returned = 0u32;
        let status = unsafe {
            NtQuerySystemInformation(
                PROCESSOR_PERFORMANCE_INFORMATION_CLASS,
                output.as_mut_ptr() as *mut c_void,
                byte_len,
                &mut returned,
            )
        };

        if status >= 0 {
            if returned != 0 {
                let returned = returned as usize;
                if !returned.is_multiple_of(item_size) || returned > byte_len as usize {
                    return Err(STATUS_INVALID_BUFFER_SIZE);
                }
                if returned / item_size != count {
                    return Err(STATUS_INFO_LENGTH_MISMATCH);
                }
            }
            return Ok(());
        }

        if status != STATUS_INFO_LENGTH_MISMATCH || returned as usize <= byte_len as usize {
            return Err(status);
        }
        count = (returned as usize).div_ceil(item_size);
    }
}

pub(crate) fn checked_summed_processor_times(
    processors: &[ProcessorPerformance],
) -> Option<(u64, u64, u64)> {
    processors
        .iter()
        .try_fold((0u64, 0u64, 0u64), |sums, processor| {
            let idle = u64::try_from(processor.idle_time).ok()?;
            let kernel = u64::try_from(processor.kernel_time).ok()?;
            let user = u64::try_from(processor.user_time).ok()?;
            Some((
                sums.0.checked_add(idle)?,
                sums.1.checked_add(kernel)?,
                sums.2.checked_add(user)?,
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::{
        ProcessorPerformance, checked_summed_processor_times, query_processor_performance,
    };
    use windows_sys::Win32::System::Threading::{ALL_PROCESSOR_GROUPS, GetActiveProcessorCount};

    #[test]
    fn processor_query_returns_every_active_logical_processor() {
        let expected = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) } as usize;
        assert!(expected > 0);
        let mut processors = Vec::new();
        query_processor_performance(expected, &mut processors).unwrap();
        assert_eq!(processors.len(), expected);
    }

    #[test]
    fn summed_processor_times_reject_negative_values() {
        let processors = [
            ProcessorPerformance {
                idle_time: 10,
                kernel_time: 30,
                user_time: 20,
                ..ProcessorPerformance::default()
            },
            ProcessorPerformance {
                idle_time: -1,
                kernel_time: i64::MAX,
                user_time: 5,
                ..ProcessorPerformance::default()
            },
        ];

        assert_eq!(checked_summed_processor_times(&processors), None);
    }

    #[test]
    fn summed_processor_times_preserve_valid_counters() {
        let processors = [
            ProcessorPerformance {
                idle_time: 10,
                kernel_time: 30,
                user_time: 20,
                ..ProcessorPerformance::default()
            },
            ProcessorPerformance {
                idle_time: 5,
                kernel_time: 40,
                user_time: 7,
                ..ProcessorPerformance::default()
            },
        ];
        assert_eq!(
            checked_summed_processor_times(&processors),
            Some((15, 70, 27))
        );
    }
}
