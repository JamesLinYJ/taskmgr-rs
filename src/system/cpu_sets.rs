// +-------------------------------------------------------------------------
//
//   taskmgr-rs - Windows CPU Set topology
//
//   File:       src/system/cpu_sets.rs
//
//   Date:       2026-07-31
//   Author:     OpenAI Codex
// --------------------------------------------------------------------------

//! Enumerates the Windows CPU Set namespace used by group-aware process affinity.
//!
//! Processor-group-relative bitmasks are not globally unique: bit 0 in group 0 and bit 0 in
//! group 1 identify different logical processors. This module keeps the group number, logical
//! processor number, and CPU Set ID together so callers cannot accidentally collapse a
//! multi-group selection into a single `usize`.

use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;
use std::ptr::{null_mut, read_unaligned};
use std::slice;

use windows_sys::Win32::Foundation::{
    ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_DATA, GetLastError, HANDLE,
};
use windows_sys::Win32::System::SystemInformation::{
    CpuSetInformation, GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION,
    SYSTEM_CPU_SET_INFORMATION_ALLOCATED, SYSTEM_CPU_SET_INFORMATION_ALLOCATED_TO_TARGET_PROCESS,
};
use windows_sys::Win32::System::Threading::GetProcessDefaultCpuSets;

const CPU_SET_HEADER_SIZE: usize = size_of::<u32>() + size_of::<i32>();
const MAX_QUERY_ATTEMPTS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CpuSetError {
    QuerySize(u32),
    QueryData(u32),
    DefaultSetSize(u32),
    DefaultSetData(u32),
    EmptyTopology,
    TruncatedHeader,
    InvalidRecordSize,
    RecordOutOfBounds,
    InvalidLogicalProcessor,
    DuplicateId,
    DuplicateLogicalProcessor,
    MissingDefaultId,
    UnavailableSelection,
}

impl CpuSetError {
    pub(crate) fn win32_code(self) -> u32 {
        match self {
            Self::QuerySize(error)
            | Self::QueryData(error)
            | Self::DefaultSetSize(error)
            | Self::DefaultSetData(error) => error,
            _ => ERROR_INVALID_DATA,
        }
    }

    pub(crate) const fn context(self) -> &'static str {
        match self {
            Self::QuerySize(_) => "GetSystemCpuSetInformation size query",
            Self::QueryData(_) => "GetSystemCpuSetInformation data query",
            Self::DefaultSetSize(_) => "GetProcessDefaultCpuSets size query",
            Self::DefaultSetData(_) => "GetProcessDefaultCpuSets data query",
            Self::EmptyTopology => "empty CPU Set topology",
            Self::TruncatedHeader => "truncated CPU Set record header",
            Self::InvalidRecordSize => "invalid CPU Set record size",
            Self::RecordOutOfBounds => "CPU Set record exceeds returned buffer",
            Self::InvalidLogicalProcessor => "CPU Set logical processor exceeds group mask width",
            Self::DuplicateId => "duplicate CPU Set identifier",
            Self::DuplicateLogicalProcessor => "duplicate group-relative logical processor",
            Self::MissingDefaultId => "process default CPU Set is absent from system topology",
            Self::UnavailableSelection => "CPU Set selection is unavailable to the target process",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CpuSet {
    pub(crate) id: u32,
    pub(crate) group: u16,
    pub(crate) logical_processor: u8,
    pub(crate) assignable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CpuSetGroup {
    pub(crate) number: u16,
    pub(crate) processor_mask: usize,
    pub(crate) assignable_mask: usize,
    cpu_sets: Vec<CpuSet>,
}

impl CpuSetGroup {
    pub(crate) fn cpu_sets(&self) -> &[CpuSet] {
        &self.cpu_sets
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CpuSetTopology {
    groups: Vec<CpuSetGroup>,
}

impl CpuSetTopology {
    pub(crate) fn query(process: HANDLE) -> Result<Self, CpuSetError> {
        let bytes = query_cpu_set_bytes(process)?;
        parse_cpu_set_bytes(&bytes)
    }

    pub(crate) fn groups(&self) -> &[CpuSetGroup] {
        &self.groups
    }

    pub(crate) fn unrestricted_masks(&self) -> Vec<usize> {
        self.groups
            .iter()
            .map(|group| group.assignable_mask)
            .collect()
    }

    pub(crate) fn masks_for_ids(&self, ids: &[u32]) -> Result<Vec<usize>, CpuSetError> {
        if ids.is_empty() {
            return Ok(self.unrestricted_masks());
        }

        let mut masks = vec![0usize; self.groups.len()];
        for id in ids {
            let Some((group_index, cpu_set)) =
                self.groups
                    .iter()
                    .enumerate()
                    .find_map(|(group_index, group)| {
                        group
                            .cpu_sets
                            .iter()
                            .find(|cpu_set| cpu_set.id == *id)
                            .map(|cpu_set| (group_index, cpu_set))
                    })
            else {
                return Err(CpuSetError::MissingDefaultId);
            };
            if !cpu_set.assignable {
                return Err(CpuSetError::UnavailableSelection);
            }
            masks[group_index] |= processor_bit(cpu_set.logical_processor)?;
        }
        Ok(masks)
    }

    pub(crate) fn ids_for_masks(&self, masks: &[usize]) -> Result<Vec<u32>, CpuSetError> {
        if masks.len() != self.groups.len() {
            return Err(CpuSetError::UnavailableSelection);
        }

        let mut ids = Vec::new();
        for (group, mask) in self.groups.iter().zip(masks.iter().copied()) {
            if mask & !group.assignable_mask != 0 {
                return Err(CpuSetError::UnavailableSelection);
            }
            for cpu_set in &group.cpu_sets {
                let bit = processor_bit(cpu_set.logical_processor)?;
                if mask & bit != 0 {
                    ids.push(cpu_set.id);
                }
            }
        }
        Ok(ids)
    }
}

pub(crate) fn query_process_default_cpu_sets(process: HANDLE) -> Result<Vec<u32>, CpuSetError> {
    let mut required = 0u32;
    // SAFETY: the null buffer is paired with a zero capacity and `required` is a valid output.
    let size_result = unsafe { GetProcessDefaultCpuSets(process, null_mut(), 0, &mut required) };
    if size_result != 0 {
        return if required == 0 {
            Ok(Vec::new())
        } else {
            Err(CpuSetError::DefaultSetSize(ERROR_INVALID_DATA))
        };
    }
    let error = unsafe { GetLastError() };
    if error != ERROR_INSUFFICIENT_BUFFER || required == 0 {
        return Err(CpuSetError::DefaultSetSize(nonzero_error(error)));
    }

    for _ in 0..MAX_QUERY_ATTEMPTS {
        let mut ids = vec![0u32; required as usize];
        let mut returned = required;
        // SAFETY: `ids` is writable for the supplied element count and `returned` is a valid
        // output. Invalid or stale process handles are reported by the API as errors.
        if unsafe {
            GetProcessDefaultCpuSets(
                process,
                ids.as_mut_ptr(),
                u32::try_from(ids.len()).unwrap_or(u32::MAX),
                &mut returned,
            )
        } != 0
        {
            if returned as usize > ids.len() {
                return Err(CpuSetError::DefaultSetData(ERROR_INVALID_DATA));
            }
            ids.truncate(returned as usize);
            return Ok(ids);
        }

        let error = unsafe { GetLastError() };
        if error != ERROR_INSUFFICIENT_BUFFER || returned <= required {
            return Err(CpuSetError::DefaultSetData(nonzero_error(error)));
        }
        required = returned;
    }
    Err(CpuSetError::DefaultSetData(ERROR_INSUFFICIENT_BUFFER))
}

fn query_cpu_set_bytes(process: HANDLE) -> Result<Vec<u8>, CpuSetError> {
    unsafe {
        let mut required = 0u32;
        let size_result = GetSystemCpuSetInformation(null_mut(), 0, &mut required, process, 0);
        if size_result == 0 {
            let error = GetLastError();
            if error != ERROR_INSUFFICIENT_BUFFER {
                return Err(CpuSetError::QuerySize(nonzero_error(error)));
            }
        }
        if required == 0 {
            return Err(CpuSetError::EmptyTopology);
        }

        for _ in 0..MAX_QUERY_ATTEMPTS {
            let word_size = size_of::<usize>();
            let word_count = (required as usize)
                .checked_add(word_size - 1)
                .ok_or(CpuSetError::QueryData(ERROR_INVALID_DATA))?
                / word_size;
            let mut storage = vec![0usize; word_count];
            let mut returned = required;
            if GetSystemCpuSetInformation(
                storage.as_mut_ptr().cast(),
                required,
                &mut returned,
                process,
                0,
            ) != 0
            {
                if returned == 0 || returned as usize > storage.len() * word_size {
                    return Err(CpuSetError::QueryData(ERROR_INVALID_DATA));
                }
                let bytes = slice::from_raw_parts(storage.as_ptr().cast::<u8>(), returned as usize);
                return Ok(bytes.to_vec());
            }

            let error = GetLastError();
            if error != ERROR_INSUFFICIENT_BUFFER || returned <= required {
                return Err(CpuSetError::QueryData(nonzero_error(error)));
            }
            required = returned;
        }
        Err(CpuSetError::QueryData(ERROR_INSUFFICIENT_BUFFER))
    }
}

fn parse_cpu_set_bytes(bytes: &[u8]) -> Result<CpuSetTopology, CpuSetError> {
    let mut offset = 0usize;
    let mut cpu_sets = Vec::new();
    while offset < bytes.len() {
        let remaining = bytes.len() - offset;
        if remaining < CPU_SET_HEADER_SIZE {
            return Err(CpuSetError::TruncatedHeader);
        }

        let record_size =
            unsafe { read_unaligned(bytes.as_ptr().add(offset).cast::<u32>()) } as usize;
        if record_size < CPU_SET_HEADER_SIZE {
            return Err(CpuSetError::InvalidRecordSize);
        }
        let end = offset
            .checked_add(record_size)
            .ok_or(CpuSetError::RecordOutOfBounds)?;
        if end > bytes.len() {
            return Err(CpuSetError::RecordOutOfBounds);
        }

        let record_type =
            unsafe { read_unaligned(bytes.as_ptr().add(offset + size_of::<u32>()).cast::<i32>()) };
        if record_type == CpuSetInformation {
            if record_size < size_of::<SYSTEM_CPU_SET_INFORMATION>() {
                return Err(CpuSetError::InvalidRecordSize);
            }
            let record = unsafe {
                read_unaligned(
                    bytes
                        .as_ptr()
                        .add(offset)
                        .cast::<SYSTEM_CPU_SET_INFORMATION>(),
                )
            };
            let cpu_set = unsafe { record.Anonymous.CpuSet };
            let flags = unsafe { cpu_set.Anonymous1.AllFlags };
            let allocated = u32::from(flags) & SYSTEM_CPU_SET_INFORMATION_ALLOCATED != 0;
            let allocated_to_target =
                u32::from(flags) & SYSTEM_CPU_SET_INFORMATION_ALLOCATED_TO_TARGET_PROCESS != 0;
            processor_bit(cpu_set.LogicalProcessorIndex)?;
            cpu_sets.push(CpuSet {
                id: cpu_set.Id,
                group: cpu_set.Group,
                logical_processor: cpu_set.LogicalProcessorIndex,
                assignable: !allocated || allocated_to_target,
            });
        }
        offset = end;
    }

    build_topology(cpu_sets)
}

fn build_topology(mut cpu_sets: Vec<CpuSet>) -> Result<CpuSetTopology, CpuSetError> {
    if cpu_sets.is_empty() {
        return Err(CpuSetError::EmptyTopology);
    }
    cpu_sets.sort_by_key(|cpu_set| (cpu_set.group, cpu_set.logical_processor, cpu_set.id));

    let mut ids = BTreeSet::new();
    let mut logical_processors = BTreeSet::new();
    let mut groups = BTreeMap::<u16, Vec<CpuSet>>::new();
    for cpu_set in cpu_sets {
        if !ids.insert(cpu_set.id) {
            return Err(CpuSetError::DuplicateId);
        }
        if !logical_processors.insert((cpu_set.group, cpu_set.logical_processor)) {
            return Err(CpuSetError::DuplicateLogicalProcessor);
        }
        groups.entry(cpu_set.group).or_default().push(cpu_set);
    }

    let groups = groups
        .into_iter()
        .map(|(number, cpu_sets)| {
            let mut processor_mask = 0usize;
            let mut assignable_mask = 0usize;
            for cpu_set in &cpu_sets {
                let bit = processor_bit(cpu_set.logical_processor)?;
                processor_mask |= bit;
                if cpu_set.assignable {
                    assignable_mask |= bit;
                }
            }
            Ok(CpuSetGroup {
                number,
                processor_mask,
                assignable_mask,
                cpu_sets,
            })
        })
        .collect::<Result<Vec<_>, CpuSetError>>()?;
    Ok(CpuSetTopology { groups })
}

fn processor_bit(logical_processor: u8) -> Result<usize, CpuSetError> {
    1usize
        .checked_shl(u32::from(logical_processor))
        .ok_or(CpuSetError::InvalidLogicalProcessor)
}

fn nonzero_error(error: u32) -> u32 {
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

#[cfg(test)]
mod tests {
    use super::{CpuSetError, build_topology, parse_cpu_set_bytes};
    use std::mem::{size_of, zeroed};
    use std::slice;
    use windows_sys::Win32::System::SystemInformation::{
        CpuSetInformation, SYSTEM_CPU_SET_INFORMATION, SYSTEM_CPU_SET_INFORMATION_ALLOCATED,
        SYSTEM_CPU_SET_INFORMATION_ALLOCATED_TO_TARGET_PROCESS,
    };

    fn record(id: u32, group: u16, processor: u8, flags: u8) -> Vec<u8> {
        let mut record = unsafe { zeroed::<SYSTEM_CPU_SET_INFORMATION>() };
        record.Size = size_of::<SYSTEM_CPU_SET_INFORMATION>() as u32;
        record.Type = CpuSetInformation;
        let cpu_set = unsafe { &mut record.Anonymous.CpuSet };
        cpu_set.Id = id;
        cpu_set.Group = group;
        cpu_set.LogicalProcessorIndex = processor;
        cpu_set.Anonymous1.AllFlags = flags;
        unsafe {
            slice::from_raw_parts(
                (&record as *const SYSTEM_CPU_SET_INFORMATION).cast::<u8>(),
                size_of::<SYSTEM_CPU_SET_INFORMATION>(),
            )
            .to_vec()
        }
    }

    #[test]
    fn parser_preserves_group_qualified_processor_identity() {
        let highest_processor = u8::try_from(usize::BITS - 1).unwrap();
        let highest_bit = 1usize << (usize::BITS - 1);
        let mut bytes = record(10, 0, 0, 0);
        bytes.extend(record(11, 0, highest_processor, 0));
        bytes.extend(record(20, 1, 0, 0));

        let topology = parse_cpu_set_bytes(&bytes).unwrap();
        assert_eq!(topology.groups().len(), 2);
        assert_eq!(topology.groups()[0].number, 0);
        assert_eq!(topology.groups()[0].processor_mask, 1 | highest_bit);
        assert_eq!(topology.groups()[1].number, 1);
        assert_eq!(topology.groups()[1].processor_mask, 1);
        assert_eq!(
            topology.ids_for_masks(&[highest_bit, 1]).unwrap(),
            vec![11, 20]
        );
    }

    #[test]
    fn process_defaults_map_back_to_masks_across_groups() {
        let topology = build_topology(vec![
            super::CpuSet {
                id: 1,
                group: 0,
                logical_processor: 2,
                assignable: true,
            },
            super::CpuSet {
                id: 2,
                group: 1,
                logical_processor: 5,
                assignable: true,
            },
        ])
        .unwrap();

        assert_eq!(topology.masks_for_ids(&[1, 2]).unwrap(), vec![4, 32]);
        assert_eq!(topology.masks_for_ids(&[]).unwrap(), vec![4, 32]);
    }

    #[test]
    fn cpu_sets_reserved_for_another_process_are_not_selectable() {
        let allocated = SYSTEM_CPU_SET_INFORMATION_ALLOCATED as u8;
        let target = (SYSTEM_CPU_SET_INFORMATION_ALLOCATED
            | SYSTEM_CPU_SET_INFORMATION_ALLOCATED_TO_TARGET_PROCESS) as u8;
        let mut bytes = record(1, 0, 0, allocated);
        bytes.extend(record(2, 0, 1, target));

        let topology = parse_cpu_set_bytes(&bytes).unwrap();
        assert_eq!(topology.groups()[0].processor_mask, 0b11);
        assert_eq!(topology.groups()[0].assignable_mask, 0b10);
        assert_eq!(
            topology.ids_for_masks(&[0b01]),
            Err(CpuSetError::UnavailableSelection)
        );
        assert_eq!(topology.ids_for_masks(&[0b10]).unwrap(), vec![2]);
    }

    #[test]
    fn parser_rejects_duplicate_ids_and_processors() {
        let mut duplicate_id = record(1, 0, 0, 0);
        duplicate_id.extend(record(1, 0, 1, 0));
        assert_eq!(
            parse_cpu_set_bytes(&duplicate_id),
            Err(CpuSetError::DuplicateId)
        );

        let mut duplicate_processor = record(1, 0, 0, 0);
        duplicate_processor.extend(record(2, 0, 0, 0));
        assert_eq!(
            parse_cpu_set_bytes(&duplicate_processor),
            Err(CpuSetError::DuplicateLogicalProcessor)
        );
    }

    #[test]
    fn parser_rejects_truncated_and_out_of_range_records() {
        assert_eq!(
            parse_cpu_set_bytes(&[1, 2, 3]),
            Err(CpuSetError::TruncatedHeader)
        );

        let mut bytes = record(1, 0, 0, 0);
        bytes[0..4].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert_eq!(
            parse_cpu_set_bytes(&bytes),
            Err(CpuSetError::RecordOutOfBounds)
        );

        let bytes = record(1, 0, usize::BITS as u8, 0);
        assert_eq!(
            parse_cpu_set_bytes(&bytes),
            Err(CpuSetError::InvalidLogicalProcessor)
        );
    }
}
