// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 进程列表模型
//
//   文件:       src/pages/processes/model.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Owns process row data, display formatting, stable sorting, and column-level change tracking.
//! It performs no process opening or destructive operation.

use std::cmp::Ordering;

use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
    IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, REALTIME_PRIORITY_CLASS,
};

use super::ProcessStrings;
use crate::config::options::ColumnId;
use crate::infrastructure::native::append_32_bit_suffix;
use crate::system::process_identity::ProcIdentity;
use crate::ui::resource_ids::NUM_COLUMN;

#[derive(Clone, Copy, Default)]
pub(super) struct DirtyColumns(pub(super) u32);

impl DirtyColumns {
    pub(super) fn from_column(column_id: ColumnId) -> Self {
        Self(1u32 << column_id as u32)
    }

    pub(super) fn from_columns(columns: &[ColumnId]) -> Self {
        columns
            .iter()
            .copied()
            .fold(Self::default(), |mut set, column| {
                set.mark(column);
                set
            })
    }

    pub(super) fn mark(&mut self, column_id: ColumnId) {
        self.0 |= Self::from_column(column_id).0;
    }

    pub(super) fn any(self) -> bool {
        self.0 != 0
    }

    pub(super) fn contains(self, column_id: ColumnId) -> bool {
        self.0 & Self::from_column(column_id).0 != 0
    }
}

#[derive(Clone)]
pub struct ProcEntry {
    // `ProcEntry` 同时承载原始采样值、展示值和刷新期的脏信息。
    pub(super) identity: ProcIdentity,
    pub(super) pid: u32,
    pub(super) image_name: String,
    pub(super) image_name_lower: String,
    pub(super) is_32_bit: Option<bool>,
    pub(super) user_name: String,
    pub(super) user_name_lower: String,
    pub(super) session_id: Option<u32>,
    pub(super) cpu: u8,
    pub(super) cpu_time_100ns: u64,
    pub(super) display_cpu_time_100ns: u64,
    pub(super) mem_usage_kb: u64,
    pub(super) mem_diff_kb: i64,
    pub(super) page_faults: u32,
    pub(super) page_faults_diff: i64,
    pub(super) commit_charge_kb: u64,
    pub(super) paged_pool_kb: u64,
    pub(super) nonpaged_pool_kb: u64,
    pub(super) priority_class: u32,
    pub(super) handle_count: u32,
    pub(super) thread_count: u32,
    pub(super) display_text: [String; NUM_COLUMN],
    pub(super) pass_count: u64,
    pub(super) dirty_columns: DirtyColumns,
}

#[derive(Clone)]
pub(super) struct ProcStaticMetadata {
    pub(super) is_32_bit: Option<bool>,
    pub(super) user_name: String,
    pub(super) user_name_lower: String,
    pub(super) session_id: Option<u32>,
    pub(super) user_identity_resolved: bool,
}

pub(super) fn compare_entries(
    left: &ProcEntry,
    right: &ProcEntry,
    sort_column: ColumnId,
    sort_direction: i32,
) -> Ordering {
    let ordering = match sort_column {
        ColumnId::ImageName => left.image_name_lower.cmp(&right.image_name_lower),
        ColumnId::Pid => left.pid.cmp(&right.pid),
        ColumnId::Username => left.user_name_lower.cmp(&right.user_name_lower),
        ColumnId::SessionId => left.session_id.cmp(&right.session_id),
        ColumnId::Cpu => left.cpu.cmp(&right.cpu),
        ColumnId::CpuTime => left.cpu_time_100ns.cmp(&right.cpu_time_100ns),
        ColumnId::MemUsage => left.mem_usage_kb.cmp(&right.mem_usage_kb),
        ColumnId::MemUsageDiff => left.mem_diff_kb.cmp(&right.mem_diff_kb),
        ColumnId::PageFaults => left.page_faults.cmp(&right.page_faults),
        ColumnId::PageFaultsDiff => left.page_faults_diff.cmp(&right.page_faults_diff),
        ColumnId::CommitCharge => left.commit_charge_kb.cmp(&right.commit_charge_kb),
        ColumnId::PagedPool => left.paged_pool_kb.cmp(&right.paged_pool_kb),
        ColumnId::NonPagedPool => left.nonpaged_pool_kb.cmp(&right.nonpaged_pool_kb),
        ColumnId::BasePriority => {
            priority_rank(left.priority_class).cmp(&priority_rank(right.priority_class))
        }
        ColumnId::HandleCount => left.handle_count.cmp(&right.handle_count),
        ColumnId::ThreadCount => left.thread_count.cmp(&right.thread_count),
    };

    if ordering == Ordering::Equal {
        let tie_break = left.pid.cmp(&right.pid);
        if sort_direction < 0 {
            tie_break.reverse()
        } else {
            tie_break
        }
    } else if sort_direction < 0 {
        ordering.reverse()
    } else {
        ordering
    }
}

// 将 Win32 优先级类常量映射为排序用的数值等级。
fn priority_rank(priority_class: u32) -> u8 {
    match priority_class {
        REALTIME_PRIORITY_CLASS => 5,
        HIGH_PRIORITY_CLASS => 4,
        ABOVE_NORMAL_PRIORITY_CLASS => 3,
        NORMAL_PRIORITY_CLASS => 2,
        BELOW_NORMAL_PRIORITY_CLASS => 1,
        _ => 0,
    }
}

pub(super) fn column_text<'a>(
    entry: &'a ProcEntry,
    column_id: ColumnId,
    strings: &'a ProcessStrings,
) -> &'a str {
    // 快照线程已缓存可变展示值；owner-data 绘制回调只借用，不在 UI 热路径分配。
    match column_id {
        ColumnId::BasePriority => match entry.priority_class {
            value if value == IDLE_PRIORITY_CLASS => &strings.priority_low,
            value if value == BELOW_NORMAL_PRIORITY_CLASS => &strings.priority_below_normal,
            value if value == HIGH_PRIORITY_CLASS => &strings.priority_high,
            value if value == ABOVE_NORMAL_PRIORITY_CLASS => &strings.priority_above_normal,
            value if value == REALTIME_PRIORITY_CLASS => &strings.priority_realtime,
            value if value == NORMAL_PRIORITY_CLASS => &strings.priority_normal,
            _ => &strings.priority_unknown,
        },
        _ => &entry.display_text[column_id as usize],
    }
}

// 将 100ns 精度的 CPU 时间格式化为 HH:MM:SS 友好显示。
fn format_elapsed_time(total_100ns: u64) -> String {
    let total_seconds = total_100ns / 10_000_000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    format!("{hours:2}:{minutes:02}:{seconds:02}")
}

fn format_kilobytes(value: u64) -> String {
    format!("{value} K")
}

fn format_signed_kilobytes(value: i64) -> String {
    format!("{value} K")
}

// 遍历进程树，按“子进程先于父进程”的顺序返回终止列表（后序遍历）。
// 使用 Toolhelp 快照建立父子关系表，递归收集子进程。
impl ProcEntry {
    pub(super) fn rebuild_display_columns(&mut self, columns: &[ColumnId]) {
        for column in columns.iter().copied() {
            self.rebuild_display_column(column);
        }
    }

    fn rebuild_display_column(&mut self, column_id: ColumnId) {
        let text = match column_id {
            ColumnId::ImageName => {
                append_32_bit_suffix(&self.image_name, self.is_32_bit == Some(true)).into_owned()
            }
            ColumnId::Pid => self.pid.to_string(),
            ColumnId::Username => self.user_name.clone(),
            ColumnId::SessionId => self
                .session_id
                .map(|session_id| session_id.to_string())
                .unwrap_or_default(),
            ColumnId::Cpu => format!("{:02} %", self.cpu),
            ColumnId::CpuTime => format_elapsed_time(self.display_cpu_time_100ns),
            ColumnId::MemUsage => format_kilobytes(self.mem_usage_kb),
            ColumnId::MemUsageDiff => format_signed_kilobytes(self.mem_diff_kb),
            ColumnId::PageFaults => self.page_faults.to_string(),
            ColumnId::PageFaultsDiff => self.page_faults_diff.to_string(),
            ColumnId::CommitCharge => format_kilobytes(self.commit_charge_kb),
            ColumnId::PagedPool => format_kilobytes(self.paged_pool_kb),
            ColumnId::NonPagedPool => format_kilobytes(self.nonpaged_pool_kb),
            ColumnId::BasePriority => return,
            ColumnId::HandleCount => self.handle_count.to_string(),
            ColumnId::ThreadCount => self.thread_count.to_string(),
        };
        self.display_text[column_id as usize] = text;
    }

    pub(super) fn apply_static_metadata(&mut self, metadata: &ProcStaticMetadata) {
        self.is_32_bit = metadata.is_32_bit;
        self.user_name.clone_from(&metadata.user_name);
        self.user_name_lower.clone_from(&metadata.user_name_lower);
        self.session_id = metadata.session_id;
    }

    pub(super) fn with_pass_count(
        mut self,
        pass_count: u64,
        active_columns: &[ColumnId],
        visible_columns: DirtyColumns,
    ) -> Self {
        self.pass_count = pass_count;
        self.rebuild_display_columns(active_columns);
        self.dirty_columns = visible_columns;
        self
    }
}

pub(super) fn update_process_entry(
    entry: &mut ProcEntry,
    snapshot: &ProcEntry,
    pass_count: u64,
    visible_columns: DirtyColumns,
) -> DirtyColumns {
    // 增量更新时只给真正变更的列打脏标记，
    // 后续 ListView 才能做到“只重绘必要行/列”。
    entry.pass_count = pass_count;
    let mut changed = DirtyColumns::default();

    let image_name_changed = entry.image_name != snapshot.image_name;
    let bitness_changed = entry.is_32_bit != snapshot.is_32_bit;
    if image_name_changed {
        entry.image_name.clone_from(&snapshot.image_name);
        entry.image_name_lower = snapshot.image_name_lower.clone();
    }
    if bitness_changed {
        entry.is_32_bit = snapshot.is_32_bit;
    }
    if image_name_changed || bitness_changed {
        mark_process_column_changed(entry, &mut changed, ColumnId::ImageName, visible_columns);
    }
    if entry.pid != snapshot.pid {
        entry.pid = snapshot.pid;
        mark_process_column_changed(entry, &mut changed, ColumnId::Pid, visible_columns);
    }
    if entry.user_name != snapshot.user_name {
        entry.user_name.clone_from(&snapshot.user_name);
        entry.user_name_lower = snapshot.user_name_lower.clone();
        mark_process_column_changed(entry, &mut changed, ColumnId::Username, visible_columns);
    }
    if entry.session_id != snapshot.session_id {
        entry.session_id = snapshot.session_id;
        mark_process_column_changed(entry, &mut changed, ColumnId::SessionId, visible_columns);
    }
    if entry.cpu != snapshot.cpu {
        entry.cpu = snapshot.cpu;
        mark_process_column_changed(entry, &mut changed, ColumnId::Cpu, visible_columns);
    }
    if entry.cpu_time_100ns != snapshot.cpu_time_100ns {
        entry.cpu_time_100ns = snapshot.cpu_time_100ns;
    }
    if entry.display_cpu_time_100ns != snapshot.display_cpu_time_100ns {
        entry.display_cpu_time_100ns = snapshot.display_cpu_time_100ns;
        mark_process_column_changed(entry, &mut changed, ColumnId::CpuTime, visible_columns);
    }
    if entry.mem_usage_kb != snapshot.mem_usage_kb {
        entry.mem_usage_kb = snapshot.mem_usage_kb;
        mark_process_column_changed(entry, &mut changed, ColumnId::MemUsage, visible_columns);
    }
    if entry.mem_diff_kb != snapshot.mem_diff_kb {
        entry.mem_diff_kb = snapshot.mem_diff_kb;
        mark_process_column_changed(entry, &mut changed, ColumnId::MemUsageDiff, visible_columns);
    }
    if entry.page_faults != snapshot.page_faults {
        entry.page_faults = snapshot.page_faults;
        mark_process_column_changed(entry, &mut changed, ColumnId::PageFaults, visible_columns);
    }
    if entry.page_faults_diff != snapshot.page_faults_diff {
        entry.page_faults_diff = snapshot.page_faults_diff;
        mark_process_column_changed(
            entry,
            &mut changed,
            ColumnId::PageFaultsDiff,
            visible_columns,
        );
    }
    if entry.commit_charge_kb != snapshot.commit_charge_kb {
        entry.commit_charge_kb = snapshot.commit_charge_kb;
        mark_process_column_changed(entry, &mut changed, ColumnId::CommitCharge, visible_columns);
    }
    if entry.paged_pool_kb != snapshot.paged_pool_kb {
        entry.paged_pool_kb = snapshot.paged_pool_kb;
        mark_process_column_changed(entry, &mut changed, ColumnId::PagedPool, visible_columns);
    }
    if entry.nonpaged_pool_kb != snapshot.nonpaged_pool_kb {
        entry.nonpaged_pool_kb = snapshot.nonpaged_pool_kb;
        mark_process_column_changed(entry, &mut changed, ColumnId::NonPagedPool, visible_columns);
    }
    if entry.priority_class != snapshot.priority_class {
        entry.priority_class = snapshot.priority_class;
        mark_process_column_changed(entry, &mut changed, ColumnId::BasePriority, visible_columns);
    }
    if entry.handle_count != snapshot.handle_count {
        entry.handle_count = snapshot.handle_count;
        mark_process_column_changed(entry, &mut changed, ColumnId::HandleCount, visible_columns);
    }
    if entry.thread_count != snapshot.thread_count {
        entry.thread_count = snapshot.thread_count;
        mark_process_column_changed(entry, &mut changed, ColumnId::ThreadCount, visible_columns);
    }

    changed
}

fn mark_process_column_changed(
    entry: &mut ProcEntry,
    changed: &mut DirtyColumns,
    column_id: ColumnId,
    visible_columns: DirtyColumns,
) {
    changed.mark(column_id);
    if visible_columns.contains(column_id) {
        entry.rebuild_display_column(column_id);
        entry.dirty_columns.mark(column_id);
    }
}
