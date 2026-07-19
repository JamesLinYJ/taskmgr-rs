// +-------------------------------------------------------------------------
//
//   taskmgr-rs - CPU 拓扑与核心分类
//
//   文件:       src/system/cpu_topology.rs
//
//   日期:       2026年07月17日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Group-aware logical processor identities, physical-core/SMT relationships, and Windows
//! efficiency-class topology.
//!
//! The runtime query uses the documented variable-length
//! `GetLogicalProcessorInformationEx(RelationProcessorCore)` contract. Raw records are parsed
//! only after their sizes and bounds have been validated. A topology query failure remains an
//! explicit state: sampling can continue, but every affected graph is marked as unknown.

use std::collections::{BTreeMap, BTreeSet};
use std::mem::{offset_of, size_of};
use std::ptr::{null_mut, read_unaligned};
use std::slice;

use windows_sys::Win32::Foundation::{
    ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_DATA, GetLastError,
};
use windows_sys::Win32::System::SystemInformation::{
    GROUP_AFFINITY, GetLogicalProcessorInformationEx, PROCESSOR_RELATIONSHIP,
    RelationProcessorCore, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows_sys::Win32::System::SystemServices::LTP_PC_SMT;
use windows_sys::Win32::System::Threading::{
    GetActiveProcessorCount, GetActiveProcessorGroupCount,
};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct LogicalProcessorId {
    pub(crate) group: u16,
    pub(crate) number: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PhysicalCoreId(usize);

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CoreClass {
    Uniform,
    Performance,
    Efficiency,
    Relative(u8),
    Unknown,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ProcessorTopologyIdentity {
    pub(crate) id: LogicalProcessorId,
    pub(crate) physical_core_index: usize,
    pub(crate) smt_index: Option<u16>,
    pub(crate) class: CoreClass,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProcessorTopologySummary {
    pub(crate) physical_core_count: usize,
    pub(crate) logical_processor_count: usize,
    pub(crate) smt_core_count: usize,
    pub(crate) minimum_threads_per_core: usize,
    pub(crate) maximum_threads_per_core: usize,
    pub(crate) class_counts: Vec<(CoreClass, usize)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CpuTopologyError {
    ActiveGroupCount(u32),
    ActiveProcessorCount(u32),
    ProcessorCountMismatch,
    QuerySize(u32),
    QueryData(u32),
    EmptyBuffer,
    TruncatedHeader,
    InvalidRecordSize,
    RecordOutOfBounds,
    UnexpectedRelationship,
    InvalidGroupCount,
    GroupOutOfRange,
    DuplicateGroupMask,
    EmptyAffinityMask,
    OutOfRangeAffinityMask,
    DuplicateLogicalProcessor,
    MissingLogicalProcessor,
    SmtFlagMismatch,
}

impl CpuTopologyError {
    pub(crate) fn win32_code(self) -> u32 {
        match self {
            Self::ActiveGroupCount(error)
            | Self::ActiveProcessorCount(error)
            | Self::QuerySize(error)
            | Self::QueryData(error) => error,
            _ => ERROR_INVALID_DATA,
        }
    }

    pub(crate) fn context(self) -> &'static str {
        match self {
            Self::ActiveGroupCount(_) => "CPU topology active group query",
            Self::ActiveProcessorCount(_) => "CPU topology active processor query",
            Self::ProcessorCountMismatch => "CPU topology processor count validation",
            Self::QuerySize(_) => "CPU topology buffer sizing",
            Self::QueryData(_) => "CPU topology data query",
            Self::EmptyBuffer => "CPU topology empty result validation",
            Self::TruncatedHeader => "CPU topology record header validation",
            Self::InvalidRecordSize => "CPU topology record size validation",
            Self::RecordOutOfBounds => "CPU topology record bounds validation",
            Self::UnexpectedRelationship => "CPU topology relationship validation",
            Self::InvalidGroupCount => "CPU topology group count validation",
            Self::GroupOutOfRange => "CPU topology group identity validation",
            Self::DuplicateGroupMask => "CPU topology group mask validation",
            Self::EmptyAffinityMask => "CPU topology empty affinity validation",
            Self::OutOfRangeAffinityMask => "CPU topology affinity bounds validation",
            Self::DuplicateLogicalProcessor => "CPU topology duplicate processor validation",
            Self::MissingLogicalProcessor => "CPU topology completeness validation",
            Self::SmtFlagMismatch => "CPU topology SMT relationship validation",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LogicalProcessor {
    pub(crate) id: LogicalProcessorId,
    physical_core: Option<PhysicalCoreId>,
    smt_index: Option<u16>,
    pub(crate) class: CoreClass,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PhysicalCore {
    id: PhysicalCoreId,
    efficiency_class: u8,
    has_smt: bool,
    logical_processors: Vec<LogicalProcessorId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProcessorTopology {
    processors: Vec<Option<LogicalProcessor>>,
    physical_cores: Vec<PhysicalCore>,
    group_count: u16,
    error: Option<CpuTopologyError>,
}

impl ProcessorTopology {
    pub(crate) fn processors(&self) -> &[Option<LogicalProcessor>] {
        &self.processors
    }

    pub(crate) fn group_count(&self) -> u16 {
        self.group_count
    }

    pub(crate) fn error(&self) -> Option<CpuTopologyError> {
        self.error
    }

    pub(crate) fn matches_sample_len(&self, processor_count: usize) -> bool {
        if self.processors.len() != processor_count {
            return false;
        }
        debug_assert!(self.is_internally_consistent());
        true
    }

    pub(crate) fn identity(&self) -> Option<Vec<ProcessorTopologyIdentity>> {
        if self.error.is_some() || !self.is_internally_consistent() {
            return None;
        }
        self.processors
            .iter()
            .map(|processor| {
                let processor = processor.as_ref()?;
                Some(ProcessorTopologyIdentity {
                    id: processor.id,
                    physical_core_index: processor.physical_core?.0,
                    smt_index: processor.smt_index,
                    class: processor.class,
                })
            })
            .collect()
    }

    pub(crate) fn summary(&self) -> Option<ProcessorTopologySummary> {
        if self.error.is_some() || !self.is_internally_consistent() {
            return None;
        }
        let mut class_counts = BTreeMap::<CoreClass, usize>::new();
        let mut minimum_threads_per_core = usize::MAX;
        let mut maximum_threads_per_core = 0usize;
        let mut smt_core_count = 0usize;
        let mut classes_by_core = vec![None; self.physical_cores.len()];
        for processor in self.processors.iter().flatten() {
            let core_index = processor.physical_core?.0;
            let class = classes_by_core.get_mut(core_index)?;
            if class.is_some_and(|existing| existing != processor.class) {
                return None;
            }
            *class = Some(processor.class);
        }
        for core in &self.physical_cores {
            let thread_count = core.logical_processors.len();
            minimum_threads_per_core = minimum_threads_per_core.min(thread_count);
            maximum_threads_per_core = maximum_threads_per_core.max(thread_count);
            smt_core_count += usize::from(core.has_smt);
            let class = classes_by_core.get(core.id.0).copied().flatten()?;
            *class_counts.entry(class).or_default() += 1;
        }
        Some(ProcessorTopologySummary {
            physical_core_count: self.physical_cores.len(),
            logical_processor_count: self.processors.len(),
            smt_core_count,
            minimum_threads_per_core,
            maximum_threads_per_core,
            class_counts: class_counts.into_iter().collect(),
        })
    }

    fn is_internally_consistent(&self) -> bool {
        if self.error.is_some() {
            return self.physical_cores.is_empty()
                && self.processors.iter().all(|processor| {
                    processor.as_ref().is_none_or(|processor| {
                        processor.physical_core.is_none()
                            && processor.smt_index.is_none()
                            && processor.class == CoreClass::Unknown
                    })
                });
        }

        if self.processors.is_empty() || self.physical_cores.is_empty() {
            return false;
        }

        let core_ids_are_valid = self.physical_cores.iter().enumerate().all(|(index, core)| {
            core.id == PhysicalCoreId(index) && !core.logical_processors.is_empty()
        });
        if !core_ids_are_valid {
            return false;
        }

        let mut classes = [false; 256];
        for core in &self.physical_cores {
            classes[usize::from(core.efficiency_class)] = true;
        }
        let distinct_class_count = classes.iter().filter(|present| **present).count();
        let minimum_class = classes.iter().position(|present| *present).unwrap_or(0) as u8;

        self.processors.iter().all(|processor| {
            let Some(processor) = processor.as_ref() else {
                return false;
            };
            let Some(core_id) = processor.physical_core else {
                return false;
            };
            let Some(core) = self.physical_cores.get(core_id.0) else {
                return false;
            };
            let Some(thread_index) = core
                .logical_processors
                .iter()
                .position(|id| *id == processor.id)
                .and_then(|index| u16::try_from(index).ok())
            else {
                return false;
            };
            core.id == core_id
                && if core.has_smt {
                    processor.smt_index == Some(thread_index)
                } else {
                    core.logical_processors.len() == 1 && processor.smt_index.is_none()
                }
                && match distinct_class_count {
                    1 => processor.class == CoreClass::Uniform,
                    2 if core.efficiency_class == minimum_class => {
                        processor.class == CoreClass::Efficiency
                    }
                    2 => processor.class == CoreClass::Performance,
                    _ => processor.class == CoreClass::Relative(core.efficiency_class),
                }
        })
    }

    fn unknown(
        processor_count: usize,
        group_count: u16,
        ids: Option<&[LogicalProcessorId]>,
        error: CpuTopologyError,
    ) -> Self {
        let processors = match ids {
            Some(ids) if ids.len() == processor_count => ids
                .iter()
                .copied()
                .map(|id| {
                    Some(LogicalProcessor {
                        id,
                        physical_core: None,
                        smt_index: None,
                        class: CoreClass::Unknown,
                    })
                })
                .collect(),
            _ => vec![None; processor_count],
        };
        Self {
            processors,
            physical_cores: Vec::new(),
            group_count,
            error: Some(error),
        }
    }
}

pub(crate) fn query_processor_topology(expected_processor_count: usize) -> ProcessorTopology {
    let (ids, group_counts, group_count) = match enumerate_active_processors() {
        Ok(active) => active,
        Err(error) => return ProcessorTopology::unknown(expected_processor_count, 0, None, error),
    };
    if ids.len() != expected_processor_count {
        return ProcessorTopology::unknown(
            expected_processor_count,
            group_count,
            None,
            CpuTopologyError::ProcessorCountMismatch,
        );
    }

    let records = match query_core_records() {
        Ok(records) => records,
        Err(error) => {
            return ProcessorTopology::unknown(
                expected_processor_count,
                group_count,
                Some(&ids),
                error,
            );
        }
    };
    match build_topology(&ids, &group_counts, records) {
        Ok(topology) => topology,
        Err(error) => {
            ProcessorTopology::unknown(expected_processor_count, group_count, Some(&ids), error)
        }
    }
}

pub(crate) fn format_cpu_graph_label(processor: &LogicalProcessor, group_count: u16) -> String {
    let mut label = if group_count > 1 {
        format!("G{}:CPU {}", processor.id.group, processor.id.number)
    } else {
        format!("CPU {}", processor.id.number)
    };
    match processor.class {
        CoreClass::Uniform => {}
        CoreClass::Performance => label.push_str(" · P"),
        CoreClass::Efficiency => label.push_str(" · E"),
        CoreClass::Relative(class) => label.push_str(&format!(" · C{class}")),
        CoreClass::Unknown => label.push_str(" · ?"),
    }
    if let Some(smt_index) = processor.smt_index {
        label.push_str(&format!(" · SMT{smt_index}"));
    }
    label
}

pub(crate) fn format_compact_cpu_graph_label(
    processor: &LogicalProcessor,
    group_count: u16,
) -> String {
    let mut label = if group_count > 1 {
        format!("G{}:{}", processor.id.group, processor.id.number)
    } else {
        processor.id.number.to_string()
    };
    match processor.class {
        CoreClass::Uniform => {}
        CoreClass::Performance => label.push_str(" P"),
        CoreClass::Efficiency => label.push_str(" E"),
        CoreClass::Relative(class) => label.push_str(&format!(" C{class}")),
        CoreClass::Unknown => label.push_str(" ?"),
    }
    if let Some(smt_index) = processor.smt_index {
        label.push_str(&format!(" SMT{smt_index}"));
    }
    label
}

fn enumerate_active_processors()
-> Result<(Vec<LogicalProcessorId>, Vec<u32>, u16), CpuTopologyError> {
    let group_count = unsafe { GetActiveProcessorGroupCount() };
    if group_count == 0 {
        return Err(CpuTopologyError::ActiveGroupCount(
            last_error_or_gen_failure(),
        ));
    }

    let mut ids = Vec::new();
    let mut group_counts = Vec::with_capacity(usize::from(group_count));
    for group in 0..group_count {
        let processor_count = unsafe { GetActiveProcessorCount(group) };
        if processor_count == 0 || processor_count > u8::MAX as u32 + 1 {
            return Err(CpuTopologyError::ActiveProcessorCount(
                last_error_or_gen_failure(),
            ));
        }
        group_counts.push(processor_count);
        for number in 0..processor_count {
            ids.push(LogicalProcessorId {
                group,
                number: number as u8,
            });
        }
    }
    Ok((ids, group_counts, group_count))
}

struct CoreRecord {
    efficiency_class: u8,
    has_smt: bool,
    group_masks: Vec<GROUP_AFFINITY>,
}

fn query_core_records() -> Result<Vec<CoreRecord>, CpuTopologyError> {
    let mut byte_len = 0u32;
    let first_result = unsafe {
        GetLogicalProcessorInformationEx(RelationProcessorCore, null_mut(), &mut byte_len)
    };
    if first_result != 0 || byte_len == 0 {
        return Err(CpuTopologyError::QuerySize(last_error_or_gen_failure()));
    }
    let first_error = unsafe { GetLastError() };
    if first_error != ERROR_INSUFFICIENT_BUFFER {
        return Err(CpuTopologyError::QuerySize(if first_error == 0 {
            ERROR_GEN_FAILURE
        } else {
            first_error
        }));
    }

    let word_count = usize::try_from(byte_len)
        .ok()
        .and_then(|len| len.checked_add(size_of::<u64>() - 1))
        .map(|len| len / size_of::<u64>())
        .ok_or(CpuTopologyError::QueryData(ERROR_INVALID_DATA))?;
    let mut words = vec![0u64; word_count];
    let allocated_len = words
        .len()
        .checked_mul(size_of::<u64>())
        .ok_or(CpuTopologyError::QueryData(ERROR_INVALID_DATA))?;
    let result = unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            words.as_mut_ptr().cast(),
            &mut byte_len,
        )
    };
    if result == 0 {
        return Err(CpuTopologyError::QueryData(last_error_or_gen_failure()));
    }
    let returned_len =
        usize::try_from(byte_len).map_err(|_| CpuTopologyError::QueryData(ERROR_INVALID_DATA))?;
    if returned_len == 0 || returned_len > allocated_len {
        return Err(CpuTopologyError::QueryData(ERROR_INVALID_DATA));
    }

    let bytes = unsafe { slice::from_raw_parts(words.as_ptr().cast::<u8>(), returned_len) };
    parse_core_records(bytes)
}

fn parse_core_records(bytes: &[u8]) -> Result<Vec<CoreRecord>, CpuTopologyError> {
    const RECORD_HEADER_SIZE: usize =
        offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous);
    const PROCESSOR_GROUP_MASK_OFFSET: usize = offset_of!(PROCESSOR_RELATIONSHIP, GroupMask);
    const PROCESSOR_GROUP_COUNT_OFFSET: usize = offset_of!(PROCESSOR_RELATIONSHIP, GroupCount);
    const PROCESSOR_EFFICIENCY_OFFSET: usize = offset_of!(PROCESSOR_RELATIONSHIP, EfficiencyClass);
    const PROCESSOR_FLAGS_OFFSET: usize = offset_of!(PROCESSOR_RELATIONSHIP, Flags);

    if bytes.is_empty() {
        return Err(CpuTopologyError::EmptyBuffer);
    }

    let mut records = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        if bytes.len() - offset < RECORD_HEADER_SIZE {
            return Err(CpuTopologyError::TruncatedHeader);
        }
        let relationship = read_value::<i32>(bytes, offset)?;
        let record_size = usize::try_from(read_value::<u32>(bytes, offset + size_of::<i32>())?)
            .map_err(|_| CpuTopologyError::InvalidRecordSize)?;
        let minimum_size = RECORD_HEADER_SIZE
            .checked_add(PROCESSOR_GROUP_MASK_OFFSET)
            .ok_or(CpuTopologyError::InvalidRecordSize)?;
        if record_size < minimum_size {
            return Err(CpuTopologyError::InvalidRecordSize);
        }
        let record_end = offset
            .checked_add(record_size)
            .ok_or(CpuTopologyError::RecordOutOfBounds)?;
        if record_end > bytes.len() {
            return Err(CpuTopologyError::RecordOutOfBounds);
        }
        if relationship != RelationProcessorCore {
            return Err(CpuTopologyError::UnexpectedRelationship);
        }

        let processor_offset = offset + RECORD_HEADER_SIZE;
        let flags = read_value::<u8>(bytes, processor_offset + PROCESSOR_FLAGS_OFFSET)?;
        let efficiency_class =
            read_value::<u8>(bytes, processor_offset + PROCESSOR_EFFICIENCY_OFFSET)?;
        let group_count = usize::from(read_value::<u16>(
            bytes,
            processor_offset + PROCESSOR_GROUP_COUNT_OFFSET,
        )?);
        if group_count == 0 {
            return Err(CpuTopologyError::InvalidGroupCount);
        }
        let masks_size = group_count
            .checked_mul(size_of::<GROUP_AFFINITY>())
            .ok_or(CpuTopologyError::InvalidGroupCount)?;
        let required_size = minimum_size
            .checked_add(masks_size)
            .ok_or(CpuTopologyError::InvalidGroupCount)?;
        if required_size > record_size {
            return Err(CpuTopologyError::InvalidGroupCount);
        }

        let mut group_masks = Vec::with_capacity(group_count);
        let masks_offset = processor_offset + PROCESSOR_GROUP_MASK_OFFSET;
        for index in 0..group_count {
            let affinity_offset = masks_offset
                .checked_add(index * size_of::<GROUP_AFFINITY>())
                .ok_or(CpuTopologyError::InvalidGroupCount)?;
            group_masks.push(read_value::<GROUP_AFFINITY>(bytes, affinity_offset)?);
        }
        records.push(CoreRecord {
            efficiency_class,
            has_smt: flags & LTP_PC_SMT as u8 != 0,
            group_masks,
        });
        offset = record_end;
    }

    if records.is_empty() {
        Err(CpuTopologyError::EmptyBuffer)
    } else {
        Ok(records)
    }
}

fn read_value<T: Copy>(bytes: &[u8], offset: usize) -> Result<T, CpuTopologyError> {
    let end = offset
        .checked_add(size_of::<T>())
        .ok_or(CpuTopologyError::RecordOutOfBounds)?;
    if end > bytes.len() {
        return Err(CpuTopologyError::RecordOutOfBounds);
    }
    Ok(unsafe { read_unaligned(bytes.as_ptr().add(offset).cast::<T>()) })
}

fn build_topology(
    active_ids: &[LogicalProcessorId],
    group_counts: &[u32],
    mut records: Vec<CoreRecord>,
) -> Result<ProcessorTopology, CpuTopologyError> {
    records.sort_by_key(first_processor_id);
    let active_set = active_ids.iter().copied().collect::<BTreeSet<_>>();
    let mut assignments = BTreeMap::<LogicalProcessorId, (PhysicalCoreId, u16)>::new();
    let mut physical_cores = Vec::with_capacity(records.len());

    for record in records {
        let mut seen_groups = BTreeSet::new();
        let mut logical_processors = Vec::new();
        let core_id = PhysicalCoreId(physical_cores.len());
        let mut group_masks = record.group_masks;
        group_masks.sort_by_key(|affinity| affinity.Group);
        for affinity in group_masks {
            if !seen_groups.insert(affinity.Group) {
                return Err(CpuTopologyError::DuplicateGroupMask);
            }
            let Some(&active_count) = group_counts.get(usize::from(affinity.Group)) else {
                return Err(CpuTopologyError::GroupOutOfRange);
            };
            if affinity.Mask == 0 {
                return Err(CpuTopologyError::EmptyAffinityMask);
            }
            let valid_mask = if active_count == usize::BITS {
                usize::MAX
            } else if active_count < usize::BITS {
                (1usize << active_count) - 1
            } else {
                return Err(CpuTopologyError::OutOfRangeAffinityMask);
            };
            if affinity.Mask & !valid_mask != 0 {
                return Err(CpuTopologyError::OutOfRangeAffinityMask);
            }

            let mut mask = affinity.Mask;
            while mask != 0 {
                let number = mask.trailing_zeros();
                let id = LogicalProcessorId {
                    group: affinity.Group,
                    number: number as u8,
                };
                if !active_set.contains(&id) {
                    return Err(CpuTopologyError::OutOfRangeAffinityMask);
                }
                let smt_index = u16::try_from(logical_processors.len())
                    .map_err(|_| CpuTopologyError::InvalidGroupCount)?;
                if assignments.insert(id, (core_id, smt_index)).is_some() {
                    return Err(CpuTopologyError::DuplicateLogicalProcessor);
                }
                logical_processors.push(id);
                mask &= mask - 1;
            }
        }
        if logical_processors.is_empty() {
            return Err(CpuTopologyError::EmptyAffinityMask);
        }
        if record.has_smt != (logical_processors.len() > 1) {
            return Err(CpuTopologyError::SmtFlagMismatch);
        }
        physical_cores.push(PhysicalCore {
            id: core_id,
            efficiency_class: record.efficiency_class,
            has_smt: record.has_smt,
            logical_processors,
        });
    }

    if assignments.len() != active_ids.len()
        || active_ids.iter().any(|id| !assignments.contains_key(id))
    {
        return Err(CpuTopologyError::MissingLogicalProcessor);
    }

    let efficiency_classes = physical_cores
        .iter()
        .map(|core| core.efficiency_class)
        .collect::<BTreeSet<_>>();
    let processors = active_ids
        .iter()
        .map(|id| {
            let (physical_core, smt_index) = assignments[id];
            let core = &physical_cores[physical_core.0];
            Some(LogicalProcessor {
                id: *id,
                physical_core: Some(physical_core),
                smt_index: core.has_smt.then_some(smt_index),
                class: classify_efficiency(core.efficiency_class, &efficiency_classes),
            })
        })
        .collect();

    Ok(ProcessorTopology {
        processors,
        physical_cores,
        group_count: u16::try_from(group_counts.len())
            .map_err(|_| CpuTopologyError::InvalidGroupCount)?,
        error: None,
    })
}

fn first_processor_id(record: &CoreRecord) -> Option<LogicalProcessorId> {
    record
        .group_masks
        .iter()
        .filter(|affinity| affinity.Mask != 0)
        .map(|affinity| LogicalProcessorId {
            group: affinity.Group,
            number: affinity.Mask.trailing_zeros() as u8,
        })
        .min()
}

fn classify_efficiency(class: u8, classes: &BTreeSet<u8>) -> CoreClass {
    match classes.len() {
        0 => CoreClass::Unknown,
        1 => CoreClass::Uniform,
        2 if classes.first().is_some_and(|minimum| class == *minimum) => CoreClass::Efficiency,
        2 => CoreClass::Performance,
        _ => CoreClass::Relative(class),
    }
}

fn last_error_or_gen_failure() -> u32 {
    let error = unsafe { GetLastError() };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::write_unaligned;

    fn id(group: u16, number: u8) -> LogicalProcessorId {
        LogicalProcessorId { group, number }
    }

    fn affinity(group: u16, mask: usize) -> GROUP_AFFINITY {
        GROUP_AFFINITY {
            Mask: mask,
            Group: group,
            Reserved: [0; 3],
        }
    }

    fn record(class: u8, masks: &[(u16, usize)]) -> CoreRecord {
        let logical_processor_count = masks.iter().map(|(_, mask)| mask.count_ones()).sum::<u32>();
        CoreRecord {
            efficiency_class: class,
            has_smt: logical_processor_count > 1,
            group_masks: masks
                .iter()
                .map(|&(group, mask)| affinity(group, mask))
                .collect(),
        }
    }

    fn encoded_record(class: u8, masks: &[(u16, usize)]) -> Vec<u8> {
        let header_size = offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous);
        let group_mask_offset = offset_of!(PROCESSOR_RELATIONSHIP, GroupMask);
        let record_size =
            header_size + group_mask_offset + masks.len() * size_of::<GROUP_AFFINITY>();
        let mut bytes = vec![0u8; record_size];
        unsafe {
            write_unaligned(bytes.as_mut_ptr().cast::<i32>(), RelationProcessorCore);
            write_unaligned(
                bytes.as_mut_ptr().add(size_of::<i32>()).cast::<u32>(),
                record_size as u32,
            );
            let processor = bytes.as_mut_ptr().add(header_size);
            let logical_processor_count =
                masks.iter().map(|(_, mask)| mask.count_ones()).sum::<u32>();
            write_unaligned(
                processor
                    .add(offset_of!(PROCESSOR_RELATIONSHIP, Flags))
                    .cast::<u8>(),
                if logical_processor_count > 1 {
                    LTP_PC_SMT as u8
                } else {
                    0
                },
            );
            write_unaligned(
                processor
                    .add(offset_of!(PROCESSOR_RELATIONSHIP, EfficiencyClass))
                    .cast::<u8>(),
                class,
            );
            write_unaligned(
                processor
                    .add(offset_of!(PROCESSOR_RELATIONSHIP, GroupCount))
                    .cast::<u16>(),
                masks.len() as u16,
            );
            for (index, &(group, mask)) in masks.iter().enumerate() {
                write_unaligned(
                    processor
                        .add(group_mask_offset + index * size_of::<GROUP_AFFINITY>())
                        .cast::<GROUP_AFFINITY>(),
                    affinity(group, mask),
                );
            }
        }
        bytes
    }

    #[test]
    fn homogeneous_topology_omits_a_core_class_suffix() {
        let ids = [id(0, 0), id(0, 1), id(0, 2), id(0, 3)];
        let topology = build_topology(
            &ids,
            &[4],
            vec![record(0, &[(0, 0b0011)]), record(0, &[(0, 0b1100)])],
        )
        .unwrap();
        assert!(topology.matches_sample_len(4));
        assert!(
            topology
                .processors()
                .iter()
                .flatten()
                .all(|processor| processor.class == CoreClass::Uniform)
        );
        assert_eq!(
            format_cpu_graph_label(topology.processors()[0].as_ref().unwrap(), 1),
            "CPU 0 · SMT0"
        );
        assert_eq!(
            format_cpu_graph_label(topology.processors()[1].as_ref().unwrap(), 1),
            "CPU 1 · SMT1"
        );
        assert_eq!(
            format_compact_cpu_graph_label(topology.processors()[1].as_ref().unwrap(), 1),
            "1 SMT1"
        );
    }

    #[test]
    fn single_threaded_core_has_no_smt_suffix() {
        let ids = [id(0, 0), id(0, 1)];
        let topology = build_topology(
            &ids,
            &[2],
            vec![record(0, &[(0, 0b0001)]), record(0, &[(0, 0b0010)])],
        )
        .unwrap();

        assert_eq!(
            format_cpu_graph_label(topology.processors()[0].as_ref().unwrap(), 1),
            "CPU 0"
        );
        assert_eq!(
            format_compact_cpu_graph_label(topology.processors()[1].as_ref().unwrap(), 1),
            "1"
        );
    }

    #[test]
    fn smt_flag_must_match_the_physical_core_thread_set() {
        let ids = [id(0, 0), id(0, 1)];
        let mut unflagged_smt_core = record(0, &[(0, 0b0011)]);
        unflagged_smt_core.has_smt = false;
        assert_eq!(
            build_topology(&ids, &[2], vec![unflagged_smt_core]),
            Err(CpuTopologyError::SmtFlagMismatch)
        );

        let mut falsely_flagged_single_core = record(0, &[(0, 0b0001)]);
        falsely_flagged_single_core.has_smt = true;
        assert_eq!(
            build_topology(&ids[..1], &[1], vec![falsely_flagged_single_core]),
            Err(CpuTopologyError::SmtFlagMismatch)
        );
    }

    #[test]
    fn two_efficiency_levels_classify_smt_siblings_as_e_and_p() {
        let ids = [id(0, 0), id(0, 1), id(0, 2), id(0, 3)];
        let topology = build_topology(
            &ids,
            &[4],
            vec![record(2, &[(0, 0b0011)]), record(9, &[(0, 0b1100)])],
        )
        .unwrap();
        let classes = topology
            .processors()
            .iter()
            .flatten()
            .map(|processor| processor.class)
            .collect::<Vec<_>>();
        assert_eq!(
            classes,
            vec![
                CoreClass::Efficiency,
                CoreClass::Efficiency,
                CoreClass::Performance,
                CoreClass::Performance
            ]
        );
        assert_eq!(
            format_cpu_graph_label(topology.processors()[3].as_ref().unwrap(), 1),
            "CPU 3 · P · SMT1"
        );
        assert_eq!(
            format_compact_cpu_graph_label(topology.processors()[3].as_ref().unwrap(), 1),
            "3 P SMT1"
        );
    }

    #[test]
    fn three_efficiency_levels_preserve_the_windows_class_value() {
        let ids = [id(0, 0), id(0, 1), id(0, 2)];
        let topology = build_topology(
            &ids,
            &[3],
            vec![
                record(1, &[(0, 1)]),
                record(4, &[(0, 2)]),
                record(7, &[(0, 4)]),
            ],
        )
        .unwrap();
        assert_eq!(
            topology.processors()[1].as_ref().unwrap().class,
            CoreClass::Relative(4)
        );
        assert_eq!(
            format_cpu_graph_label(topology.processors()[1].as_ref().unwrap(), 1),
            "CPU 1 · C4"
        );
    }

    #[test]
    fn processor_groups_keep_windows_group_then_number_order() {
        let ids = [id(0, 0), id(0, 1), id(1, 0), id(1, 1)];
        let topology = build_topology(
            &ids,
            &[2, 2],
            vec![
                record(0, &[(1, 0b0010)]),
                record(0, &[(0, 0b0010)]),
                record(0, &[(1, 0b0001)]),
                record(0, &[(0, 0b0001)]),
            ],
        )
        .unwrap();
        assert_eq!(
            topology
                .processors()
                .iter()
                .flatten()
                .map(|processor| processor.id)
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(
            format_cpu_graph_label(topology.processors()[2].as_ref().unwrap(), 2),
            "G1:CPU 0"
        );
        assert_eq!(
            format_compact_cpu_graph_label(topology.processors()[2].as_ref().unwrap(), 2),
            "G1:0"
        );
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn more_than_sixty_four_processors_keep_group_aware_labels() {
        let mut ids = (0..64).map(|number| id(0, number)).collect::<Vec<_>>();
        ids.extend((0..8).map(|number| id(1, number)));
        let mut records = (0..64)
            .map(|number| record(0, &[(0, 1usize << number)]))
            .collect::<Vec<_>>();
        records.extend((0..8).map(|number| record(0, &[(1, 1usize << number)])));

        let topology = build_topology(&ids, &[64, 8], records).unwrap();
        assert!(topology.matches_sample_len(72));
        assert_eq!(
            format_cpu_graph_label(topology.processors()[64].as_ref().unwrap(), 2),
            "G1:CPU 0"
        );
    }

    #[test]
    fn malformed_variable_records_are_rejected() {
        let valid = encoded_record(0, &[(0, 1)]);
        assert_eq!(parse_core_records(&valid).unwrap().len(), 1);

        let mut truncated = valid.clone();
        truncated.pop();
        assert!(matches!(
            parse_core_records(&truncated),
            Err(CpuTopologyError::RecordOutOfBounds)
        ));

        let mut invalid_size = valid.clone();
        invalid_size[size_of::<i32>()..size_of::<i32>() + size_of::<u32>()]
            .copy_from_slice(&0u32.to_ne_bytes());
        assert!(matches!(
            parse_core_records(&invalid_size),
            Err(CpuTopologyError::InvalidRecordSize)
        ));

        let mut invalid_group_count = valid;
        let group_count_offset = offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous)
            + offset_of!(PROCESSOR_RELATIONSHIP, GroupCount);
        invalid_group_count[group_count_offset..group_count_offset + size_of::<u16>()]
            .copy_from_slice(&2u16.to_ne_bytes());
        assert!(matches!(
            parse_core_records(&invalid_group_count),
            Err(CpuTopologyError::InvalidGroupCount)
        ));
    }

    #[test]
    fn duplicate_missing_and_out_of_range_processors_are_rejected() {
        let ids = [id(0, 0), id(0, 1)];
        assert_eq!(
            build_topology(&ids, &[2], vec![record(0, &[(0, 1)]), record(0, &[(0, 1)])]),
            Err(CpuTopologyError::DuplicateLogicalProcessor)
        );
        assert_eq!(
            build_topology(&ids, &[2], vec![record(0, &[(0, 1)])]),
            Err(CpuTopologyError::MissingLogicalProcessor)
        );
        assert_eq!(
            build_topology(&ids, &[2], vec![record(0, &[(0, 0b100)])]),
            Err(CpuTopologyError::OutOfRangeAffinityMask)
        );
        assert_eq!(
            build_topology(&ids, &[2], vec![record(0, &[(0, 1), (0, 2)])]),
            Err(CpuTopologyError::DuplicateGroupMask)
        );
    }

    #[test]
    fn failed_classification_is_explicit_and_still_matches_sample_shape() {
        let ids = [id(0, 0), id(0, 1)];
        let topology =
            ProcessorTopology::unknown(ids.len(), 1, Some(&ids), CpuTopologyError::QueryData(5));
        assert!(topology.matches_sample_len(ids.len()));
        assert_eq!(topology.error(), Some(CpuTopologyError::QueryData(5)));
        assert_eq!(
            format_cpu_graph_label(topology.processors()[0].as_ref().unwrap(), 1),
            "CPU 0 · ?"
        );
    }

    #[test]
    fn live_windows_query_is_shape_safe_or_explicitly_unknown() {
        use windows_sys::Win32::System::Threading::ALL_PROCESSOR_GROUPS;

        let count = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) } as usize;
        let topology = query_processor_topology(count);
        assert!(topology.matches_sample_len(count));
        assert_eq!(topology.error(), None);
        assert!(topology.processors().iter().all(Option::is_some));
    }
}
