// +-------------------------------------------------------------------------
//
//   taskmgr-rs - CPU 诊断页面
//
//   文件:       src/cpupage.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Owns the classic CPU diagnostics page, its histories, workers, and responsive layout.
//!
//! System samples and slower diagnostic sources are committed independently. Every worker
//! completion carries a group-aware topology key; data for a previous topology is discarded
//! rather than relabelled as current. Failed refreshes retain same-topology trusted values and
//! expose a stale state without running any synchronous query on the UI thread.

use std::collections::{BTreeSet, HashMap};
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::mpsc::TryRecvError;

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_GEN_FAILURE, ERROR_INVALID_DATA, ERROR_INVALID_WINDOW_HANDLE,
    GetLastError, HWND, LPARAM, POINT, RECT, SIZE,
};
use windows_sys::Win32::Graphics::Gdi::{
    GetDC, GetTextExtentPoint32W, HDC, HFONT, InvalidateRect, ReleaseDC, SelectObject,
};
use windows_sys::Win32::UI::Controls::{
    NMTTDISPINFOW, TOOLTIPS_CLASSW, TTF_IDISHWND, TTF_SUBCLASS, TTM_ADDTOOLW, TTN_GETDISPINFOW,
    TTS_ALWAYSTIP, TTS_NOPREFIX, TTTOOLINFOW,
};
use windows_sys::Win32::UI::Shell::{
    SFBS_FLAGS_ROUND_TO_NEAREST_DISPLAYED_DIGIT, StrFormatByteSizeEx,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, CreateWindowExW, DeferWindowPos, DestroyWindow, EndDeferWindowPos,
    GetClientRect, GetDlgItem, GetWindowRect, MapDialogRect, SWP_NOACTIVATE, SWP_NOREDRAW,
    SWP_NOZORDER, SendMessageW, SetWindowTextW, WM_GETFONT, WM_SETFONT, WS_POPUP,
};

use crate::background_worker::BackgroundWorker;
use crate::chart_renderer::{ChartColor, ChartRenderer};
use crate::cpu_details::{
    CpuArchitecture, CpuCacheInfo, CpuCacheKind, CpuComponentUpdate, CpuDetailError,
    CpuDetailRefresh, CpuDetailRequest, CpuDynamicInfo, CpuFeatureInfo, CpuFirmwareCollector,
    CpuFirmwareProcessor, CpuFirmwareSnapshot, CpuNativeCollector, CpuNativeSnapshot,
    CpuSupplementalTopology, CpuTopologyKey,
};
use crate::cpu_layout::{CpuLayoutMetrics, CpuLayoutPlan, compute_cpu_layout};
use crate::cpu_topology::{CoreClass, ProcessorTopologySummary};
use crate::drawing::{HistoryBuffer, fill_black};
use crate::language::{TextKey, text};
use crate::options::Options;
use crate::perf_drawing::{
    GRAPH_GRID, HIST_SIZE, HistoryPlotLayout, HistorySeries, draw_graph_label, draw_grid_width,
    draw_grid_width_gpu, draw_history_series, draw_history_series_gpu,
};
use crate::resource::{
    CPU_DETAIL_GROUP_COUNT, CPU_DETAIL_METRIC_COUNTS, IDC_CPU_DETAIL_GRAPH,
    IDC_CPU_DETAIL_GROUP_FIRST, IDC_CPU_DETAIL_LABEL_BASES, IDC_CPU_DETAIL_MODEL,
    IDC_CPU_DETAIL_STATUS, IDC_CPU_DETAIL_TITLE, IDC_CPU_DETAIL_VALUE_BASES,
    PWM_CPU_FIRMWARE_WORKER_COMPLETE, PWM_CPU_WORKER_COMPLETE,
};
use crate::system_sampler::{CpuDiagnosticError, CpuDiagnosticSample, SystemSample};
use crate::winutil::{
    pause_redraw_for_visible_windows, record_win32_error, resume_redraw_for_windows, to_wide_null,
};

pub(crate) const CPU_DETAIL_GROUP_TITLE_KEYS: [TextKey; CPU_DETAIL_GROUP_COUNT] = [
    TextKey::CpuCurrentState,
    TextKey::CpuSystemDiagnostics,
    TextKey::CpuTopologyFeatures,
    TextKey::CpuHardwareCache,
];

pub(crate) const CPU_DETAIL_METRIC_KEYS: [&[TextKey]; CPU_DETAIL_GROUP_COUNT] = [
    &[
        TextKey::CpuUsage,
        TextKey::CpuAverageFrequency,
        TextKey::CpuFrequencyRange,
        TextKey::CpuUserTime,
        TextKey::CpuKernelTime,
        TextKey::CpuDpcTime,
        TextKey::CpuInterruptTime,
        TextKey::CpuInterruptsPerSecond,
        TextKey::CpuUptime,
    ],
    &[
        TextKey::ProcessesLabel,
        TextKey::Threads,
        TextKey::Handles,
        TextKey::CpuProcessorQueueLength,
        TextKey::CpuContextSwitchesPerSecond,
        TextKey::CpuSystemCallsPerSecond,
    ],
    &[
        TextKey::CpuPackages,
        TextKey::CpuNumaNodes,
        TextKey::CpuGroups,
        TextKey::CpuDies,
        TextKey::CpuModules,
        TextKey::CpuPhysicalCores,
        TextKey::CpuLogicalProcessors,
        TextKey::CpuCoreClasses,
        TextKey::CpuSmtCores,
        TextKey::CpuThreadsPerCore,
        TextKey::CpuVirtualization,
        TextKey::CpuSlat,
    ],
    &[
        TextKey::CpuManufacturer,
        TextKey::CpuSocket,
        TextKey::CpuProcessorId,
        TextKey::CpuArchitectureWidth,
        TextKey::CpuFamilyLevel,
        TextKey::CpuRevisionStepping,
        TextKey::CpuFirmwareMaxFrequency,
        TextKey::CpuIsaFeatures,
        TextKey::CpuCacheL1Data,
        TextKey::CpuCacheL1Instruction,
        TextKey::CpuCacheL2,
        TextKey::CpuCacheL3,
    ],
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CpuPageStatus {
    #[default]
    Loading,
    LoadingDetails,
    Ready,
    Partial,
    Unavailable,
    Failed,
    Stale,
}

struct ComponentState<T> {
    value: Option<T>,
    error: Option<CpuDetailError>,
}

impl<T> ComponentState<T> {
    fn new() -> Self {
        Self {
            value: None,
            error: None,
        }
    }

    fn clear(&mut self) {
        self.value = None;
        self.error = None;
    }

    fn apply(&mut self, update: CpuComponentUpdate<T>) {
        match update {
            CpuComponentUpdate::Unchanged => {}
            CpuComponentUpdate::Success(value) => {
                self.value = Some(value);
                self.error = None;
            }
            CpuComponentUpdate::Failed(error) => {
                if self.error.as_ref() != Some(&error) {
                    error.record();
                }
                self.error = Some(error);
            }
        }
    }

    fn has_error(&self) -> bool {
        self.error.is_some()
    }

    fn is_stale(&self) -> bool {
        self.error.is_some() && self.value.is_some()
    }
}

pub(crate) struct CpuPageState {
    hwnd: HWND,
    no_title: bool,
    show_kernel_times: bool,
    layout_metrics: Option<CpuLayoutMetrics>,
    chart_renderer: ChartRenderer,
    native_worker: Option<BackgroundWorker<CpuDetailRequest, CpuNativeSnapshot>>,
    firmware_worker: Option<BackgroundWorker<CpuDetailRequest, CpuFirmwareSnapshot>>,
    native_collection_in_flight: bool,
    firmware_collection_in_flight: bool,
    queued_native_refresh: Option<CpuDetailRefresh>,
    queued_firmware_refresh: Option<CpuDetailRefresh>,
    topology_key: Option<CpuTopologyKey>,
    topology_summary: Option<ProcessorTopologySummary>,
    topology: ComponentState<CpuSupplementalTopology>,
    firmware: ComponentState<Vec<CpuFirmwareProcessor>>,
    features: ComponentState<CpuFeatureInfo>,
    dynamic: ComponentState<CpuDynamicInfo>,
    native_worker_error: Option<CpuDetailError>,
    firmware_worker_error: Option<CpuDetailError>,
    pdh_baseline_timestamp_ms: Option<u64>,
    last_system_sample_timestamp_ms: Option<u64>,
    system_sample_seen: bool,
    system_sample_stale: bool,
    diagnostic_stale: bool,
    cpu_usage: Option<u8>,
    diagnostics: Option<CpuDiagnosticSample>,
    uptime_ms: Option<u64>,
    handle_count: Option<u32>,
    thread_count: Option<u32>,
    process_count: Option<u32>,
    total_history: HistoryBuffer,
    kernel_history: HistoryBuffer,
    history_valid: bool,
    graph_scroll_offset: i32,
    graph_label: Vec<u16>,
    plot_points: Vec<POINT>,
    status: CpuPageStatus,
    control_text_cache: HashMap<i32, String>,
    tooltip_hwnd: HWND,
    tooltip_text: HashMap<i32, Arc<[u16]>>,
    tooltip_active_text: Option<Arc<[u16]>>,
}

impl CpuPageState {
    pub(crate) fn new() -> Self {
        Self {
            hwnd: null_mut(),
            no_title: false,
            show_kernel_times: false,
            layout_metrics: None,
            chart_renderer: ChartRenderer::new(),
            native_worker: None,
            firmware_worker: None,
            native_collection_in_flight: false,
            firmware_collection_in_flight: false,
            queued_native_refresh: None,
            queued_firmware_refresh: None,
            topology_key: None,
            topology_summary: None,
            topology: ComponentState::new(),
            firmware: ComponentState::new(),
            features: ComponentState::new(),
            dynamic: ComponentState::new(),
            native_worker_error: None,
            firmware_worker_error: None,
            pdh_baseline_timestamp_ms: None,
            last_system_sample_timestamp_ms: None,
            system_sample_seen: false,
            system_sample_stale: false,
            diagnostic_stale: false,
            cpu_usage: None,
            diagnostics: None,
            uptime_ms: None,
            handle_count: None,
            thread_count: None,
            process_count: None,
            total_history: HistoryBuffer::zeroed(HIST_SIZE),
            kernel_history: HistoryBuffer::zeroed(HIST_SIZE),
            history_valid: false,
            graph_scroll_offset: 0,
            graph_label: to_wide_null(text(TextKey::CpuUsage)),
            plot_points: Vec::new(),
            status: CpuPageStatus::Loading,
            control_text_cache: HashMap::new(),
            tooltip_hwnd: null_mut(),
            tooltip_text: HashMap::new(),
            tooltip_active_text: None,
        }
    }

    pub(crate) fn initialize(&mut self, hwnd: HWND) -> Result<(), u32> {
        self.hwnd = hwnd;
        self.layout_metrics = Some(self.capture_layout_metrics()?);
        self.sync_graph_font()?;
        self.initialize_tooltips()?;
        self.start_workers()?;
        self.update_visible_texts();
        if !self.size_page() {
            return Err(ERROR_GEN_FAILURE);
        }
        Ok(())
    }

    fn start_workers(&mut self) -> Result<(), u32> {
        if self.native_worker.is_some() && self.firmware_worker.is_some() {
            return Ok(());
        }
        let native_worker = BackgroundWorker::spawn_initialized(
            "taskmgr-rs-cpu-native-worker",
            PWM_CPU_WORKER_COMPLETE,
            || {
                let mut collector = CpuNativeCollector::new();
                move |request| collector.collect(request)
            },
        )?;
        let firmware_worker = BackgroundWorker::spawn_initialized(
            "taskmgr-rs-cpu-firmware-worker",
            PWM_CPU_FIRMWARE_WORKER_COMPLETE,
            || {
                let mut collector = CpuFirmwareCollector::new();
                move |request| collector.collect(request)
            },
        )?;
        self.native_worker = Some(native_worker);
        self.firmware_worker = Some(firmware_worker);
        Ok(())
    }

    pub(crate) fn apply_options(&mut self, options: &Options) -> bool {
        let no_title_changed = self.no_title != options.no_title();
        let kernel_times_changed = self.show_kernel_times != options.kernel_times();
        self.no_title = options.no_title();
        self.show_kernel_times = options.kernel_times();
        if kernel_times_changed {
            self.invalidate_graph();
        }
        no_title_changed
    }

    pub(crate) fn timer_event(&mut self, refresh: CpuDetailRefresh) {
        self.request_native_refresh(refresh);
        if refresh == CpuDetailRefresh::User
            || self.firmware.value.is_none() && self.firmware.error.is_none()
        {
            self.request_firmware_refresh(refresh);
        }
    }

    fn request_native_refresh(&mut self, refresh: CpuDetailRefresh) {
        let Some(topology_key) = self.topology_key.clone() else {
            queue_refresh(&mut self.queued_native_refresh, refresh);
            self.update_status_text();
            return;
        };
        if self.native_collection_in_flight {
            queue_refresh(&mut self.queued_native_refresh, refresh);
            return;
        }
        let Some(worker) = self.native_worker.as_ref() else {
            self.handle_native_worker_error(CpuDetailError::Win32 {
                context: "CPU native worker state",
                code: ERROR_BROKEN_PIPE,
            });
            return;
        };
        let request = CpuDetailRequest {
            topology_key,
            refresh,
        };
        match worker.submit(request, self.hwnd) {
            Ok(()) => {
                self.native_collection_in_flight = true;
                self.update_status_text();
            }
            Err(code) => self.handle_native_worker_error(CpuDetailError::Win32 {
                context: "CPU native worker request",
                code,
            }),
        }
    }

    fn request_firmware_refresh(&mut self, refresh: CpuDetailRefresh) {
        let Some(topology_key) = self.topology_key.clone() else {
            queue_refresh(&mut self.queued_firmware_refresh, refresh);
            self.update_status_text();
            return;
        };
        if self.firmware_collection_in_flight {
            queue_refresh(&mut self.queued_firmware_refresh, refresh);
            return;
        }
        let Some(worker) = self.firmware_worker.as_ref() else {
            self.handle_firmware_worker_error(CpuDetailError::Win32 {
                context: "CPU firmware worker state",
                code: ERROR_BROKEN_PIPE,
            });
            return;
        };
        let request = CpuDetailRequest {
            topology_key,
            refresh,
        };
        match worker.submit(request, self.hwnd) {
            Ok(()) => {
                self.firmware_collection_in_flight = true;
                self.update_status_text();
            }
            Err(code) => self.handle_firmware_worker_error(CpuDetailError::Win32 {
                context: "CPU firmware worker request",
                code,
            }),
        }
    }

    pub(crate) fn handle_native_worker_completion(&mut self) {
        let mut completions = Vec::new();
        let mut disconnected = false;
        if let Some(worker) = self.native_worker.as_ref() {
            loop {
                match worker.try_recv() {
                    Ok(completion) => completions.push(completion),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        self.native_collection_in_flight = false;
        if disconnected {
            self.native_worker = None;
            self.handle_native_worker_error(CpuDetailError::Win32 {
                context: "CPU native worker completion channel",
                code: ERROR_BROKEN_PIPE,
            });
        }

        for completion in completions {
            self.commit_native_snapshot(completion);
        }

        if let Some(refresh) = self.queued_native_refresh.take() {
            self.request_native_refresh(refresh);
        } else {
            self.request_initial_dynamic_if_ready();
            self.update_status_text();
        }
    }

    pub(crate) fn handle_firmware_worker_completion(&mut self) {
        let mut completions = Vec::new();
        let mut disconnected = false;
        if let Some(worker) = self.firmware_worker.as_ref() {
            loop {
                match worker.try_recv() {
                    Ok(completion) => completions.push(completion),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        self.firmware_collection_in_flight = false;
        if disconnected {
            self.firmware_worker = None;
            self.handle_firmware_worker_error(CpuDetailError::Win32 {
                context: "CPU firmware worker completion channel",
                code: ERROR_BROKEN_PIPE,
            });
        }
        for completion in completions {
            self.commit_firmware_snapshot(completion);
        }
        if let Some(refresh) = self.queued_firmware_refresh.take() {
            self.request_firmware_refresh(refresh);
        } else {
            self.update_status_text();
        }
    }

    fn commit_native_snapshot(&mut self, snapshot: CpuNativeSnapshot) {
        if self.topology_key.as_ref() != Some(&snapshot.topology_key) {
            return;
        }
        self.native_worker_error = None;
        if matches!(
            &snapshot.dynamic,
            CpuComponentUpdate::Success(_) | CpuComponentUpdate::Failed(_)
        ) {
            self.pdh_baseline_timestamp_ms = None;
        }
        if let Some(timestamp_ms) = snapshot.pdh_baseline_timestamp_ms {
            self.pdh_baseline_timestamp_ms = Some(timestamp_ms);
        }
        self.topology.apply(snapshot.topology);
        self.features.apply(snapshot.features);
        self.dynamic.apply(snapshot.dynamic);
        self.update_visible_texts();
    }

    fn commit_firmware_snapshot(&mut self, snapshot: CpuFirmwareSnapshot) {
        if self.topology_key.as_ref() != Some(&snapshot.topology_key) {
            return;
        }
        self.firmware_worker_error = None;
        self.firmware.apply(snapshot.firmware);
        self.update_visible_texts();
    }

    fn handle_native_worker_error(&mut self, error: CpuDetailError) {
        if self.native_worker_error.as_ref() != Some(&error) {
            error.record();
        }
        self.native_worker_error = Some(error);
        self.update_visible_texts();
    }

    fn handle_firmware_worker_error(&mut self, error: CpuDetailError) {
        if self.firmware_worker_error.as_ref() != Some(&error) {
            error.record();
        }
        self.firmware_worker_error = Some(error);
        self.update_visible_texts();
    }

    fn request_initial_dynamic_if_ready(&mut self) {
        let ready = self
            .pdh_baseline_timestamp_ms
            .zip(self.last_system_sample_timestamp_ms)
            .is_some_and(|(baseline, sample)| sample > baseline);
        if ready && !self.native_collection_in_flight {
            self.request_native_refresh(CpuDetailRefresh::Periodic);
        }
    }

    pub(crate) fn apply_system_sample(
        &mut self,
        sample: &SystemSample,
        redraw: bool,
    ) -> Result<(), u32> {
        if sample.processor_count == 0
            || sample.processor_cpu_usage.len() != sample.processor_count
            || sample.processor_kernel_usage.len() != sample.processor_count
            || !sample
                .processor_topology
                .matches_sample_len(sample.processor_count)
        {
            return Err(ERROR_INVALID_DATA);
        }

        let next_key = CpuTopologyKey::from_topology(&sample.processor_topology);
        if self.topology_key != next_key {
            self.topology_key = next_key.clone();
            self.topology_summary = sample.processor_topology.summary();
            self.clear_detail_components();
            self.diagnostics = None;
            self.diagnostic_stale = false;
            if next_key.is_some() {
                queue_refresh(&mut self.queued_native_refresh, CpuDetailRefresh::Prewarm);
                queue_refresh(&mut self.queued_firmware_refresh, CpuDetailRefresh::Prewarm);
            }
        } else {
            self.topology_summary = sample.processor_topology.summary();
        }

        self.system_sample_seen = true;
        self.system_sample_stale = false;
        self.last_system_sample_timestamp_ms = Some(sample.uptime_ms);
        self.uptime_ms = Some(sample.uptime_ms);
        self.handle_count = Some(sample.handle_count);
        self.thread_count = Some(sample.thread_count);
        self.process_count = Some(sample.process_count);

        if sample.cpu_delta_valid {
            self.cpu_usage = Some(sample.cpu_usage);
            self.total_history.push(sample.cpu_usage);
            self.kernel_history.push(sample.kernel_usage);
            self.history_valid = true;
            self.graph_scroll_offset = (self.graph_scroll_offset + 2) % GRAPH_GRID;
        }
        match sample.cpu_diagnostics {
            Ok(diagnostics) => {
                self.diagnostics = Some(diagnostics);
                self.diagnostic_stale = false;
            }
            Err(CpuDiagnosticError::BaselineUnavailable) if self.diagnostics.is_none() => {}
            Err(_) => self.diagnostic_stale = self.diagnostics.is_some(),
        }

        if self.topology_key.is_some() {
            if !self.native_collection_in_flight
                && let Some(refresh) = self.queued_native_refresh.take()
            {
                self.request_native_refresh(refresh);
            }
            if !self.firmware_collection_in_flight
                && let Some(refresh) = self.queued_firmware_refresh.take()
            {
                self.request_firmware_refresh(refresh);
            }
            self.request_initial_dynamic_if_ready();
        }
        self.update_visible_texts();
        if redraw {
            self.invalidate_graph();
        }
        Ok(())
    }

    pub(crate) fn mark_system_sample_error(&mut self) {
        self.system_sample_stale = true;
        self.update_status_text();
    }

    fn clear_detail_components(&mut self) {
        self.topology.clear();
        self.firmware.clear();
        self.features.clear();
        self.dynamic.clear();
        self.native_worker_error = None;
        self.firmware_worker_error = None;
        self.pdh_baseline_timestamp_ms = None;
    }

    fn update_visible_texts(&mut self) {
        let model = format_processor_model(self.firmware.value.as_deref());
        let values = self.metric_values();
        self.set_cached_text(IDC_CPU_DETAIL_MODEL, &model);
        for group in 0..CPU_DETAIL_GROUP_COUNT {
            for (index, value) in values[group].iter().enumerate() {
                self.set_cached_text(IDC_CPU_DETAIL_VALUE_BASES[group] + index as i32, value);
            }
        }
        self.update_status_text();
    }

    fn metric_values(&self) -> [Vec<String>; CPU_DETAIL_GROUP_COUNT] {
        let not_available = || text(TextKey::NotAvailable).to_string();
        let diagnostics = self.diagnostics.as_ref();
        let dynamic = self.dynamic.value.as_ref();
        let summary = self.topology_summary.as_ref();
        let topology = self.topology.value.as_ref();
        let firmware = self.firmware.value.as_deref();
        let features = self.features.value.as_ref();

        let current = vec![
            self.cpu_usage
                .map(format_percent)
                .unwrap_or_else(not_available),
            dynamic
                .map(|value| format_frequency(value.average_frequency_mhz))
                .unwrap_or_else(not_available),
            dynamic
                .map(|value| {
                    format!(
                        "{} - {}",
                        format_frequency(value.minimum_frequency_mhz),
                        format_frequency(value.maximum_frequency_mhz)
                    )
                })
                .unwrap_or_else(not_available),
            diagnostics
                .map(|value| format_percent(value.user_usage))
                .unwrap_or_else(not_available),
            diagnostics
                .map(|value| format_percent(value.kernel_usage))
                .unwrap_or_else(not_available),
            diagnostics
                .map(|value| format_percent(value.dpc_usage))
                .unwrap_or_else(not_available),
            diagnostics
                .map(|value| format_percent(value.interrupt_usage))
                .unwrap_or_else(not_available),
            diagnostics
                .map(|value| value.interrupts_per_second.to_string())
                .unwrap_or_else(not_available),
            self.uptime_ms
                .map(format_uptime)
                .unwrap_or_else(not_available),
        ];

        let system = vec![
            self.process_count
                .map(|value| value.to_string())
                .unwrap_or_else(not_available),
            self.thread_count
                .map(|value| value.to_string())
                .unwrap_or_else(not_available),
            self.handle_count
                .map(|value| value.to_string())
                .unwrap_or_else(not_available),
            dynamic
                .map(|value| value.processor_queue_length.to_string())
                .unwrap_or_else(not_available),
            dynamic
                .and_then(|value| value.context_switches_per_second)
                .map(|value| value.to_string())
                .unwrap_or_else(not_available),
            dynamic
                .and_then(|value| value.system_calls_per_second)
                .map(|value| value.to_string())
                .unwrap_or_else(not_available),
        ];

        let topology_features = vec![
            topology
                .map(|value| value.package_count.to_string())
                .unwrap_or_else(not_available),
            topology
                .map(|value| value.numa_node_count.to_string())
                .unwrap_or_else(not_available),
            topology
                .map(|value| value.group_count.to_string())
                .unwrap_or_else(not_available),
            topology
                .and_then(|value| value.die_count)
                .map(|value| value.to_string())
                .unwrap_or_else(not_available),
            topology
                .and_then(|value| value.module_count)
                .map(|value| value.to_string())
                .unwrap_or_else(not_available),
            summary
                .map(|value| value.physical_core_count.to_string())
                .unwrap_or_else(not_available),
            summary
                .map(|value| value.logical_processor_count.to_string())
                .unwrap_or_else(not_available),
            summary
                .map(format_core_classes)
                .unwrap_or_else(not_available),
            summary
                .map(|value| value.smt_core_count.to_string())
                .unwrap_or_else(not_available),
            summary
                .map(format_threads_per_core)
                .unwrap_or_else(not_available),
            features
                .map(|value| format_boolean(value.virtualization_firmware_enabled))
                .unwrap_or_else(not_available),
            features
                .map(|value| format_boolean(value.second_level_address_translation))
                .unwrap_or_else(not_available),
        ];

        let hardware = vec![
            format_firmware_text(firmware, |processor| processor.manufacturer.as_deref()),
            format_socket_text(firmware),
            format_firmware_text(firmware, |processor| processor.processor_id.as_deref()),
            format_architecture_width(features, firmware),
            format_family_level(firmware),
            format_revision_stepping(firmware),
            format_firmware_frequency(firmware),
            features
                .map(|value| {
                    if value.isa_features.is_empty() {
                        not_available()
                    } else {
                        value.isa_features.join(", ")
                    }
                })
                .unwrap_or_else(not_available),
            format_cache(topology, 1, &[CpuCacheKind::Data]),
            format_cache(topology, 1, &[CpuCacheKind::Instruction]),
            format_cache(
                topology,
                2,
                &[
                    CpuCacheKind::Unified,
                    CpuCacheKind::Data,
                    CpuCacheKind::Instruction,
                ],
            ),
            format_cache(
                topology,
                3,
                &[
                    CpuCacheKind::Unified,
                    CpuCacheKind::Data,
                    CpuCacheKind::Instruction,
                ],
            ),
        ];

        debug_assert_eq!(current.len(), CPU_DETAIL_METRIC_COUNTS[0]);
        debug_assert_eq!(system.len(), CPU_DETAIL_METRIC_COUNTS[1]);
        debug_assert_eq!(topology_features.len(), CPU_DETAIL_METRIC_COUNTS[2]);
        debug_assert_eq!(hardware.len(), CPU_DETAIL_METRIC_COUNTS[3]);
        [current, system, topology_features, hardware]
    }

    fn update_status_text(&mut self) {
        self.status = self.derive_status();
        let status_text = match self.status {
            CpuPageStatus::Loading => text(TextKey::CpuLoading).to_string(),
            CpuPageStatus::LoadingDetails => text(TextKey::CpuLoadingDetails).to_string(),
            CpuPageStatus::Ready => {
                format_header_summary(self.topology_summary.as_ref(), self.dynamic.value.as_ref())
            }
            CpuPageStatus::Partial => text(TextKey::CpuPartialDetails).to_string(),
            CpuPageStatus::Unavailable => text(TextKey::CpuUnavailable).to_string(),
            CpuPageStatus::Failed => text(TextKey::CpuRefreshFailed).to_string(),
            CpuPageStatus::Stale => text(TextKey::CpuRefreshFailedStale).to_string(),
        };
        self.set_cached_text(IDC_CPU_DETAIL_STATUS, &status_text);
    }

    fn derive_status(&self) -> CpuPageStatus {
        if !self.system_sample_seen {
            return if self.system_sample_stale {
                CpuPageStatus::Failed
            } else {
                CpuPageStatus::Loading
            };
        }
        if self.topology_key.is_none() {
            return CpuPageStatus::Unavailable;
        }
        let worker_failed =
            self.native_worker_error.is_some() || self.firmware_worker_error.is_some();
        let source_failed = worker_failed
            || self.topology.has_error()
            || self.firmware.has_error()
            || self.features.has_error()
            || self.dynamic.has_error();
        let retained_stale_value = self.system_sample_stale
            || self.diagnostic_stale
            || self.native_worker_error.is_some()
                && (self.topology.value.is_some()
                    || self.features.value.is_some()
                    || self.dynamic.value.is_some())
            || self.firmware_worker_error.is_some() && self.firmware.value.is_some()
            || self.topology.is_stale()
            || self.firmware.is_stale()
            || self.features.is_stale()
            || self.dynamic.is_stale();
        if retained_stale_value {
            return CpuPageStatus::Stale;
        }
        if source_failed {
            return if self.has_detail_values() {
                CpuPageStatus::Partial
            } else {
                CpuPageStatus::Failed
            };
        }
        if !self.has_detail_values() {
            CpuPageStatus::Loading
        } else if self.topology.value.is_none() && !self.topology.has_error()
            || self.features.value.is_none() && !self.features.has_error()
            || self.dynamic.value.is_none() && !self.dynamic.has_error()
            || self.firmware.value.is_none() && !self.firmware.has_error()
        {
            CpuPageStatus::LoadingDetails
        } else {
            CpuPageStatus::Ready
        }
    }

    fn has_detail_values(&self) -> bool {
        self.topology.value.is_some()
            || self.firmware.value.is_some()
            || self.features.value.is_some()
            || self.dynamic.value.is_some()
    }

    fn set_cached_text(&mut self, control_id: i32, value: &str) {
        if self
            .control_text_cache
            .get(&control_id)
            .is_some_and(|current| current == value)
        {
            return;
        }
        let control = self.control(control_id);
        if control.is_null() {
            return;
        }
        let wide = to_wide_null(value);
        unsafe { SetWindowTextW(control, wide.as_ptr()) };
        self.control_text_cache
            .insert(control_id, value.to_string());
        self.tooltip_text
            .insert(control_id, Arc::<[u16]>::from(wide));
    }

    fn initialize_tooltips(&mut self) -> Result<(), u32> {
        let tooltip = unsafe {
            CreateWindowExW(
                0,
                TOOLTIPS_CLASSW,
                null(),
                WS_POPUP | TTS_ALWAYSTIP | TTS_NOPREFIX,
                0,
                0,
                0,
                0,
                self.hwnd,
                null_mut(),
                null_mut(),
                null(),
            )
        };
        if tooltip.is_null() {
            return Err(last_error_or_gen_failure());
        }

        let mut control_ids =
            Vec::with_capacity(2 + CPU_DETAIL_METRIC_COUNTS.iter().copied().sum::<usize>());
        control_ids.push(IDC_CPU_DETAIL_MODEL);
        control_ids.push(IDC_CPU_DETAIL_STATUS);
        for group in 0..CPU_DETAIL_GROUP_COUNT {
            control_ids.extend(
                (0..CPU_DETAIL_METRIC_COUNTS[group])
                    .map(|index| IDC_CPU_DETAIL_VALUE_BASES[group] + index as i32),
            );
        }

        for control_id in control_ids {
            let control = self.control(control_id);
            if control.is_null() {
                unsafe { DestroyWindow(tooltip) };
                return Err(ERROR_INVALID_WINDOW_HANDLE);
            }
            let mut tool = TTTOOLINFOW {
                cbSize: size_of::<TTTOOLINFOW>() as u32,
                uFlags: TTF_IDISHWND | TTF_SUBCLASS,
                hwnd: self.hwnd,
                uId: control as usize,
                lpszText: (-1isize) as *mut u16,
                lParam: control_id as LPARAM,
                ..unsafe { zeroed() }
            };
            if unsafe {
                SendMessageW(
                    tooltip,
                    TTM_ADDTOOLW,
                    0,
                    (&mut tool as *mut TTTOOLINFOW) as LPARAM,
                )
            } == 0
            {
                unsafe { DestroyWindow(tooltip) };
                return Err(ERROR_GEN_FAILURE);
            }
        }
        self.tooltip_hwnd = tooltip;
        Ok(())
    }

    pub(crate) fn handle_notify(&mut self, lparam: LPARAM) -> isize {
        if lparam == 0 || self.tooltip_hwnd.is_null() {
            return 0;
        }
        let header = unsafe { &*(lparam as *const windows_sys::Win32::UI::Controls::NMHDR) };
        if header.hwndFrom != self.tooltip_hwnd || header.code != TTN_GETDISPINFOW {
            return 0;
        }
        let info = unsafe { &mut *(lparam as *mut NMTTDISPINFOW) };
        let Ok(control_id) = i32::try_from(info.lParam) else {
            return 0;
        };
        if let Some(value) = self.tooltip_text.get(&control_id).cloned() {
            // The tooltip may keep this callback pointer while it is visible, so retain the
            // exact allocation until another notification replaces it.
            info.lpszText = value.as_ptr().cast_mut();
            self.tooltip_active_text = Some(value);
        } else {
            info.lpszText = null_mut();
            self.tooltip_active_text = None;
        }
        0
    }

    pub(crate) fn draw_graph(&mut self, hdc: HDC, rect: RECT) {
        let width = (rect.right - rect.left).max(1);
        let height = (rect.bottom - rect.top).max(1);
        let scale = ((width - 1) / HIST_SIZE as i32).max(0);
        let scale = if scale == 0 { 2 } else { scale } as usize;
        let layout = HistoryPlotLayout {
            graph_height: (height - 1).max(1),
            width,
            scale,
        };

        let rendered_with_d2d = if let Some(frame) = self.chart_renderer.begin_frame(hdc, rect) {
            let target_rect = frame.bounds();
            frame.clear_black();
            draw_grid_width_gpu(&frame, &target_rect, width, self.graph_scroll_offset);
            if self.history_valid {
                if self.show_kernel_times {
                    draw_history_series_gpu(
                        &frame,
                        &target_rect,
                        layout,
                        HistorySeries {
                            history: &self.kernel_history,
                            color: ChartColor::Red,
                            stop_on_zero: false,
                        },
                    );
                }
                draw_history_series_gpu(
                    &frame,
                    &target_rect,
                    layout,
                    HistorySeries {
                        history: &self.total_history,
                        color: ChartColor::Green,
                        stop_on_zero: false,
                    },
                );
            }
            frame.end()
        } else {
            false
        };

        if !rendered_with_d2d {
            fill_black(hdc, &rect);
            draw_grid_width(hdc, &rect, width, self.graph_scroll_offset);
            if self.history_valid {
                if self.show_kernel_times {
                    draw_history_series(
                        hdc,
                        &rect,
                        layout,
                        HistorySeries {
                            history: &self.kernel_history,
                            color: ChartColor::Red,
                            stop_on_zero: false,
                        },
                        &mut self.plot_points,
                    );
                }
                draw_history_series(
                    hdc,
                    &rect,
                    layout,
                    HistorySeries {
                        history: &self.total_history,
                        color: ChartColor::Green,
                        stop_on_zero: false,
                    },
                    &mut self.plot_points,
                );
            }
        }

        draw_graph_label(
            self.control(IDC_CPU_DETAIL_GRAPH),
            hdc,
            &rect,
            &self.graph_label,
            &self.graph_label,
        );
    }

    pub(crate) fn is_graph_control(&self, control_id: i32) -> bool {
        control_id == IDC_CPU_DETAIL_GRAPH
    }

    fn invalidate_graph(&self) {
        let graph = self.control(IDC_CPU_DETAIL_GRAPH);
        if !graph.is_null() {
            unsafe { InvalidateRect(graph, null(), 0) };
        }
    }

    fn capture_layout_metrics(&self) -> Result<CpuLayoutMetrics, u32> {
        let mut dialog_units = RECT {
            left: 0,
            top: 0,
            right: 4,
            bottom: 8,
        };
        if unsafe { MapDialogRect(self.hwnd, &mut dialog_units) } == 0 {
            return Err(last_error_or_gen_failure());
        }
        let base_x = ((dialog_units.right - dialog_units.left) + 3) / 4;
        let base_y = ((dialog_units.bottom - dialog_units.top) + 7) / 8;
        if base_x <= 0 || base_y <= 0 {
            return Err(ERROR_INVALID_DATA);
        }

        let title_height = self.control_height(IDC_CPU_DETAIL_TITLE)?;
        let status_height = self.control_height(IDC_CPU_DETAIL_STATUS)?;
        let metric_row_height = self.control_height(IDC_CPU_DETAIL_VALUE_BASES[0])?;
        let title_width = self
            .measure_text_width(IDC_CPU_DETAIL_TITLE, &[text(TextKey::CpuPageTitle)])?
            .saturating_add(base_x.saturating_mul(2));
        let labels: Vec<&str> = CPU_DETAIL_METRIC_KEYS
            .iter()
            .flat_map(|keys| keys.iter().map(|key| text(*key)))
            .collect();
        let label_width = self.measure_text_width(IDC_CPU_DETAIL_VALUE_BASES[0], &labels)?;
        let value_width = self.measure_text_width(
            IDC_CPU_DETAIL_VALUE_BASES[0],
            &["999.99 GHz - 999.99 GHz", "999999999999", "99:23:59:59"],
        )?;
        let pair_width = label_width
            .saturating_add(value_width)
            .saturating_add(base_x.saturating_mul(2));
        let minimum_group_width = pair_width
            .saturating_mul(2)
            .saturating_add(base_x.saturating_mul(3));

        Ok(CpuLayoutMetrics {
            margin_x: base_x.saturating_mul(2),
            margin_y: base_y.saturating_mul(2),
            gap: base_x.saturating_mul(2).max(base_y),
            title_width,
            title_height,
            status_height,
            metric_row_height,
            group_top_padding: title_height.saturating_add(base_y),
            group_bottom_padding: base_y.saturating_mul(2),
            minimum_group_width,
            metric_label_width: label_width.saturating_add(base_x),
            minimum_graph_height: base_y.saturating_mul(32),
        })
    }

    fn sync_graph_font(&self) -> Result<(), u32> {
        let source = self.control(IDC_CPU_DETAIL_VALUE_BASES[0]);
        let graph = self.control(IDC_CPU_DETAIL_GRAPH);
        if source.is_null() || graph.is_null() {
            return Err(ERROR_INVALID_WINDOW_HANDLE);
        }
        let font = unsafe { SendMessageW(source, WM_GETFONT, 0, 0) };
        if font == 0 {
            return Err(ERROR_INVALID_DATA);
        }
        unsafe { SendMessageW(graph, WM_SETFONT, font as usize, 0) };
        Ok(())
    }

    fn measure_text_width(&self, control_id: i32, values: &[&str]) -> Result<i32, u32> {
        let control = self.control(control_id);
        if control.is_null() {
            return Err(ERROR_INVALID_WINDOW_HANDLE);
        }
        let hdc = unsafe { GetDC(control) };
        if hdc.is_null() {
            return Err(last_error_or_gen_failure());
        }
        let font = unsafe { SendMessageW(control, WM_GETFONT, 0, 0) } as HFONT;
        if font.is_null() {
            unsafe { ReleaseDC(control, hdc) };
            return Err(ERROR_INVALID_DATA);
        }
        let previous = unsafe { SelectObject(hdc, font) };
        if previous.is_null() || previous as isize == -1 {
            unsafe { ReleaseDC(control, hdc) };
            return Err(ERROR_GEN_FAILURE);
        }
        let result = (|| {
            let mut maximum = 0;
            for value in values {
                let wide: Vec<u16> = value.encode_utf16().collect();
                let mut extent = SIZE { cx: 0, cy: 0 };
                if !wide.is_empty()
                    && unsafe {
                        GetTextExtentPoint32W(hdc, wide.as_ptr(), wide.len() as i32, &mut extent)
                    } == 0
                {
                    return Err(last_error_or_gen_failure());
                }
                maximum = maximum.max(extent.cx);
            }
            Ok(maximum.max(1))
        })();
        unsafe { SelectObject(hdc, previous) };
        unsafe { ReleaseDC(control, hdc) };
        result
    }

    fn control_height(&self, control_id: i32) -> Result<i32, u32> {
        let control = self.control(control_id);
        if control.is_null() {
            return Err(ERROR_INVALID_WINDOW_HANDLE);
        }
        let mut rect = unsafe { zeroed::<RECT>() };
        if unsafe { GetWindowRect(control, &mut rect) } == 0 {
            return Err(last_error_or_gen_failure());
        }
        (rect.bottom - rect.top)
            .checked_abs()
            .filter(|height| *height > 0)
            .ok_or(ERROR_INVALID_DATA)
    }

    pub(crate) fn size_page(&mut self) -> bool {
        if self.hwnd.is_null() {
            return false;
        }
        let mut client = unsafe { zeroed::<RECT>() };
        if unsafe { GetClientRect(self.hwnd, &mut client) } == 0 {
            record_win32_error("CPU page client rectangle", last_error_or_gen_failure());
            return false;
        }
        let Some(metrics) = self.layout_metrics else {
            record_win32_error("CPU page layout metrics", ERROR_INVALID_DATA);
            return false;
        };
        let layout = compute_cpu_layout(client, metrics);
        debug_assert!(layout.minimum_content_height >= metrics.minimum_graph_height);
        debug_assert_eq!(
            layout.groups[0].top == layout.groups[2].top,
            layout.four_columns
        );
        if let Err(error) = self.commit_layout(&layout) {
            record_win32_error("CPU page layout commit", error);
            return false;
        }
        true
    }

    fn commit_layout(&self, layout: &CpuLayoutPlan) -> Result<(), u32> {
        let placements = self.layout_placements(layout)?;
        let count = i32::try_from(placements.len()).map_err(|_| ERROR_GEN_FAILURE)?;
        let mut hdwp = unsafe { BeginDeferWindowPos(count) };
        if hdwp.is_null() {
            return Err(last_error_or_gen_failure());
        }
        let windows: Vec<_> = placements.iter().map(|(hwnd, _)| *hwnd).collect();
        let paused = pause_redraw_for_visible_windows(&windows);
        for (hwnd, rect) in placements {
            let next = unsafe {
                DeferWindowPos(
                    hdwp,
                    hwnd,
                    null_mut(),
                    rect.left,
                    rect.top,
                    (rect.right - rect.left).max(0),
                    (rect.bottom - rect.top).max(0),
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW,
                )
            };
            if next.is_null() {
                let error = last_error_or_gen_failure();
                resume_redraw_for_windows(&paused);
                return Err(error);
            }
            hdwp = next;
        }
        if unsafe { EndDeferWindowPos(hdwp) } == 0 {
            let error = last_error_or_gen_failure();
            resume_redraw_for_windows(&paused);
            return Err(error);
        }
        resume_redraw_for_windows(&paused);
        Ok(())
    }

    fn layout_placements(&self, layout: &CpuLayoutPlan) -> Result<Vec<(HWND, RECT)>, u32> {
        let placement_count =
            4 + CPU_DETAIL_GROUP_COUNT + CPU_DETAIL_METRIC_COUNTS.iter().sum::<usize>() * 2;
        let mut placements = Vec::with_capacity(placement_count);
        let mut add = |control_id, rect| {
            let hwnd = self.control(control_id);
            if hwnd.is_null() {
                Err(ERROR_INVALID_WINDOW_HANDLE)
            } else {
                placements.push((hwnd, rect));
                Ok(())
            }
        };
        add(IDC_CPU_DETAIL_TITLE, layout.title)?;
        add(IDC_CPU_DETAIL_MODEL, layout.model)?;
        add(IDC_CPU_DETAIL_STATUS, layout.status)?;
        add(IDC_CPU_DETAIL_GRAPH, layout.graph)?;
        for group in 0..CPU_DETAIL_GROUP_COUNT {
            add(
                IDC_CPU_DETAIL_GROUP_FIRST + group as i32,
                layout.groups[group],
            )?;
            for index in 0..CPU_DETAIL_METRIC_COUNTS[group] {
                add(
                    IDC_CPU_DETAIL_LABEL_BASES[group] + index as i32,
                    layout.metric_labels[group][index],
                )?;
                add(
                    IDC_CPU_DETAIL_VALUE_BASES[group] + index as i32,
                    layout.metric_values[group][index],
                )?;
            }
        }
        Ok(placements)
    }

    fn control(&self, control_id: i32) -> HWND {
        if self.hwnd.is_null() {
            null_mut()
        } else {
            unsafe { GetDlgItem(self.hwnd, control_id) }
        }
    }

    pub(crate) fn no_title(&self) -> bool {
        self.no_title
    }

    pub(crate) fn destroy(&mut self) {
        self.native_worker = None;
        self.firmware_worker = None;
        self.native_collection_in_flight = false;
        self.firmware_collection_in_flight = false;
        self.queued_native_refresh = None;
        self.queued_firmware_refresh = None;
        self.last_system_sample_timestamp_ms = None;
        self.layout_metrics = None;
        self.topology_key = None;
        self.clear_detail_components();
        self.control_text_cache.clear();
        if !self.tooltip_hwnd.is_null() {
            unsafe { DestroyWindow(self.tooltip_hwnd) };
            self.tooltip_hwnd = null_mut();
        }
        self.tooltip_active_text = None;
        self.tooltip_text.clear();
        self.hwnd = null_mut();
    }
}

impl Default for CpuPageState {
    fn default() -> Self {
        Self::new()
    }
}

fn refresh_priority(refresh: CpuDetailRefresh) -> u8 {
    match refresh {
        CpuDetailRefresh::Periodic => 0,
        CpuDetailRefresh::Prewarm => 1,
        CpuDetailRefresh::Activation => 2,
        CpuDetailRefresh::User => 3,
    }
}

fn queue_refresh(slot: &mut Option<CpuDetailRefresh>, refresh: CpuDetailRefresh) {
    if slot.is_none_or(|current| refresh_priority(refresh) > refresh_priority(current)) {
        *slot = Some(refresh);
    }
}

fn format_percent(value: u8) -> String {
    format!("{value}%")
}

fn format_frequency(mhz: u64) -> String {
    if mhz >= 1_000 {
        let hundredths = (u128::from(mhz) + 5) / 10;
        format!("{}.{:02} GHz", hundredths / 100, hundredths % 100)
    } else {
        format!("{mhz} MHz")
    }
}

fn format_uptime(uptime_ms: u64) -> String {
    let total_seconds = uptime_ms / 1_000;
    let days = total_seconds / 86_400;
    let hours = total_seconds / 3_600 % 24;
    let minutes = total_seconds / 60 % 60;
    let seconds = total_seconds % 60;
    format!("{days}:{hours:02}:{minutes:02}:{seconds:02}")
}

fn format_boolean(value: bool) -> String {
    text(if value {
        TextKey::CpuYes
    } else {
        TextKey::CpuNo
    })
    .to_string()
}

fn format_core_classes(summary: &ProcessorTopologySummary) -> String {
    let mut values = Vec::with_capacity(summary.class_counts.len());
    for (class, count) in &summary.class_counts {
        let label = match class {
            CoreClass::Uniform => text(TextKey::CpuUniformClass).to_string(),
            CoreClass::Performance => "P".to_string(),
            CoreClass::Efficiency => "E".to_string(),
            CoreClass::Relative(value) => format!("C{value}"),
            CoreClass::Unknown => "?".to_string(),
        };
        values.push(format!("{label}: {count}"));
    }
    values.join(", ")
}

fn format_threads_per_core(summary: &ProcessorTopologySummary) -> String {
    if summary.minimum_threads_per_core == summary.maximum_threads_per_core {
        summary.minimum_threads_per_core.to_string()
    } else {
        format!(
            "{} - {}",
            summary.minimum_threads_per_core, summary.maximum_threads_per_core
        )
    }
}

fn format_header_summary(
    summary: Option<&ProcessorTopologySummary>,
    dynamic: Option<&CpuDynamicInfo>,
) -> String {
    let mut values = Vec::with_capacity(5);
    if let Some(dynamic) = dynamic {
        values.push(format!(
            "{}: {}",
            text(TextKey::CpuAverageFrequency),
            format_frequency(dynamic.average_frequency_mhz)
        ));
    }
    if let Some(summary) = summary {
        values.push(format!(
            "{}: {}",
            text(TextKey::CpuPhysicalCores),
            summary.physical_core_count
        ));
        values.push(format!(
            "{}: {}",
            text(TextKey::CpuLogicalProcessors),
            summary.logical_processor_count
        ));
        values.push(format!(
            "{}: {}",
            text(TextKey::CpuThreadsPerCore),
            format_threads_per_core(summary)
        ));
        values.push(format!(
            "{}: {}",
            text(TextKey::CpuCoreClasses),
            format_core_classes(summary)
        ));
    }
    values.join("    ")
}

fn format_processor_model(processors: Option<&[CpuFirmwareProcessor]>) -> String {
    let Some(processors) = processors else {
        return String::new();
    };
    let names = unique_firmware_values(processors, |processor| processor.name.as_deref());
    if names.is_empty() {
        return text(TextKey::NotAvailable).to_string();
    }
    let mut value = names.join(" | ");
    if processors.len() > 1 {
        value.push_str(&format!(
            " ({} {})",
            processors.len(),
            text(TextKey::CpuSockets)
        ));
    }
    value
}

fn format_firmware_text(
    processors: Option<&[CpuFirmwareProcessor]>,
    select: impl Fn(&CpuFirmwareProcessor) -> Option<&str>,
) -> String {
    let Some(processors) = processors else {
        return text(TextKey::NotAvailable).to_string();
    };
    let values = unique_firmware_values(processors, select);
    if values.is_empty() {
        text(TextKey::NotAvailable).to_string()
    } else {
        values.join(", ")
    }
}

fn unique_firmware_values<'a>(
    processors: &'a [CpuFirmwareProcessor],
    select: impl Fn(&'a CpuFirmwareProcessor) -> Option<&'a str>,
) -> Vec<String> {
    processors
        .iter()
        .filter_map(select)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn format_socket_text(processors: Option<&[CpuFirmwareProcessor]>) -> String {
    let Some(processors) = processors else {
        return text(TextKey::NotAvailable).to_string();
    };
    let values: Vec<_> = processors
        .iter()
        .filter_map(|processor| {
            processor
                .socket
                .as_deref()
                .map(str::trim)
                .and_then(|socket| {
                    (!socket.is_empty()).then(|| format!("{}: {socket}", processor.device_id))
                })
        })
        .collect();
    if values.is_empty() {
        text(TextKey::NotAvailable).to_string()
    } else {
        values.join(", ")
    }
}

fn format_architecture_width(
    features: Option<&CpuFeatureInfo>,
    processors: Option<&[CpuFirmwareProcessor]>,
) -> String {
    let Some(features) = features else {
        return text(TextKey::NotAvailable).to_string();
    };
    let architecture = match features.architecture {
        CpuArchitecture::X86 => "x86".to_string(),
        CpuArchitecture::X64 => "x64".to_string(),
        CpuArchitecture::Arm => "ARM".to_string(),
        CpuArchitecture::Arm64 => "ARM64".to_string(),
        CpuArchitecture::Ia64 => "IA-64".to_string(),
        CpuArchitecture::Alpha => "Alpha".to_string(),
        CpuArchitecture::Alpha64 => "Alpha64".to_string(),
        CpuArchitecture::Mips => "MIPS".to_string(),
        CpuArchitecture::PowerPc => "PowerPC".to_string(),
        CpuArchitecture::Shx => "SHX".to_string(),
        CpuArchitecture::Msil => "MSIL".to_string(),
        CpuArchitecture::X86OnX64 => "x86 / x64".to_string(),
        CpuArchitecture::Arm32OnArm64 => "ARM32 / ARM64".to_string(),
        CpuArchitecture::X86OnArm64 => "x86 / ARM64".to_string(),
        CpuArchitecture::Neutral => "?".to_string(),
        CpuArchitecture::Unknown(value) => format!("? ({value})"),
    };
    let Some(processors) = processors else {
        return architecture;
    };
    let address = unique_numeric_values(processors, |processor| processor.address_width);
    let data = unique_numeric_values(processors, |processor| processor.data_width);
    match (address.is_empty(), data.is_empty()) {
        (true, true) => architecture,
        (false, true) => format!("{architecture}; {}", join_numbers(&address)),
        (true, false) => format!("{architecture}; {}", join_numbers(&data)),
        (false, false) => format!(
            "{architecture}; {} / {}",
            join_numbers(&address),
            join_numbers(&data)
        ),
    }
}

fn format_family_level(processors: Option<&[CpuFirmwareProcessor]>) -> String {
    let Some(processors) = processors else {
        return text(TextKey::NotAvailable).to_string();
    };
    let values = processors
        .iter()
        .filter_map(|processor| match (processor.family, processor.level) {
            (Some(family), Some(level)) => Some(format!("{family} / {level}")),
            (Some(family), None) => Some(family.to_string()),
            (None, Some(level)) => Some(format!("- / {level}")),
            (None, None) => None,
        })
        .collect::<BTreeSet<_>>();
    if values.is_empty() {
        text(TextKey::NotAvailable).to_string()
    } else {
        values.into_iter().collect::<Vec<_>>().join(", ")
    }
}

fn format_revision_stepping(processors: Option<&[CpuFirmwareProcessor]>) -> String {
    let Some(processors) = processors else {
        return text(TextKey::NotAvailable).to_string();
    };
    let values = processors
        .iter()
        .filter_map(
            |processor| match (processor.revision, processor.stepping.as_deref()) {
                (Some(revision), Some(stepping)) => Some(format!("{revision} / {stepping}")),
                (Some(revision), None) => Some(revision.to_string()),
                (None, Some(stepping)) => Some(format!("- / {stepping}")),
                (None, None) => None,
            },
        )
        .collect::<BTreeSet<_>>();
    if values.is_empty() {
        text(TextKey::NotAvailable).to_string()
    } else {
        values.into_iter().collect::<Vec<_>>().join(", ")
    }
}

fn format_firmware_frequency(processors: Option<&[CpuFirmwareProcessor]>) -> String {
    let Some(processors) = processors else {
        return text(TextKey::NotAvailable).to_string();
    };
    let values = unique_numeric_values(processors, |processor| processor.max_clock_mhz);
    if values.is_empty() {
        text(TextKey::NotAvailable).to_string()
    } else {
        values
            .into_iter()
            .map(|value| format_frequency(u64::from(value)))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn unique_numeric_values<T: Copy + Ord>(
    processors: &[CpuFirmwareProcessor],
    select: impl Fn(&CpuFirmwareProcessor) -> Option<T>,
) -> Vec<T> {
    processors
        .iter()
        .filter_map(select)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn join_numbers<T: ToString>(values: &[T]) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("/")
}

fn format_cache(
    topology: Option<&CpuSupplementalTopology>,
    level: u8,
    kinds: &[CpuCacheKind],
) -> String {
    let Some(topology) = topology else {
        return text(TextKey::NotAvailable).to_string();
    };
    let values: Vec<_> = topology
        .caches
        .iter()
        .filter(|cache| cache.level == level && kinds.contains(&cache.kind))
        .map(format_cache_entry)
        .collect();
    if values.is_empty() {
        text(TextKey::NotAvailable).to_string()
    } else {
        values.join("; ")
    }
}

fn format_cache_entry(cache: &CpuCacheInfo) -> String {
    let kind = match cache.kind {
        CpuCacheKind::Unified => "U",
        CpuCacheKind::Instruction => "I",
        CpuCacheKind::Data => "D",
        CpuCacheKind::Trace => "T",
    };
    let associativity = cache.associativity.map_or_else(
        || text(TextKey::CpuFullyAssociative).to_string(),
        |ways| format!("{ways}x"),
    );
    format!(
        "{kind}: {} x {} = {}; {} B; {associativity}",
        cache.instance_count,
        format_bytes(cache.bytes_per_instance),
        format_bytes(cache.total_bytes),
        cache.line_size,
    )
}

fn format_bytes(bytes: u64) -> String {
    let mut buffer = [0u16; 64];
    if unsafe {
        StrFormatByteSizeEx(
            bytes,
            SFBS_FLAGS_ROUND_TO_NEAREST_DISPLAYED_DIGIT,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
    } >= 0
    {
        let length = buffer
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..length])
    } else {
        format!("{bytes} B")
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
    fn queued_refresh_keeps_the_strongest_reason() {
        assert!(
            refresh_priority(CpuDetailRefresh::User)
                > refresh_priority(CpuDetailRefresh::Activation)
        );
        assert!(
            refresh_priority(CpuDetailRefresh::Activation)
                > refresh_priority(CpuDetailRefresh::Prewarm)
        );
        assert!(
            refresh_priority(CpuDetailRefresh::Prewarm)
                > refresh_priority(CpuDetailRefresh::Periodic)
        );
    }

    #[test]
    fn uptime_and_frequency_formats_preserve_units() {
        assert_eq!(format_uptime(90_061_000), "1:01:01:01");
        assert_eq!(format_frequency(4_240), "4.24 GHz");
        assert_eq!(format_frequency(800), "800 MHz");
    }

    #[test]
    fn header_summary_surfaces_dynamic_and_topology_information() {
        let summary = ProcessorTopologySummary {
            physical_core_count: 8,
            logical_processor_count: 12,
            smt_core_count: 4,
            minimum_threads_per_core: 1,
            maximum_threads_per_core: 2,
            class_counts: vec![(CoreClass::Performance, 4), (CoreClass::Efficiency, 4)],
        };
        let dynamic = CpuDynamicInfo {
            average_frequency_mhz: 4_268,
            minimum_frequency_mhz: 3_200,
            maximum_frequency_mhz: 5_100,
            processor_queue_length: 0,
            context_switches_per_second: None,
            system_calls_per_second: None,
        };
        let value = format_header_summary(Some(&summary), Some(&dynamic));
        assert!(value.contains("4.27 GHz"));
        assert!(value.contains('8'));
        assert!(value.contains("12"));
        assert!(value.contains("P: 4"));
        assert!(value.contains("E: 4"));
    }

    #[test]
    fn threads_per_core_reports_uniform_and_mixed_topologies() {
        let mut summary = ProcessorTopologySummary {
            minimum_threads_per_core: 2,
            maximum_threads_per_core: 2,
            ..Default::default()
        };
        assert_eq!(format_threads_per_core(&summary), "2");
        summary.minimum_threads_per_core = 1;
        assert_eq!(format_threads_per_core(&summary), "1 - 2");
    }

    #[test]
    fn component_failure_is_stale_only_when_a_previous_value_exists() {
        let mut component = ComponentState::<u32>::new();
        component.apply(CpuComponentUpdate::Failed(CpuDetailError::InvalidData {
            context: "CPU page test source",
        }));
        assert!(component.has_error());
        assert!(!component.is_stale());

        component.apply(CpuComponentUpdate::Success(42));
        component.apply(CpuComponentUpdate::Failed(CpuDetailError::InvalidData {
            context: "CPU page test source",
        }));
        assert!(component.is_stale());
        assert_eq!(component.value, Some(42));
    }
}
