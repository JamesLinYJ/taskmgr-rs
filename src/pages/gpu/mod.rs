// +-------------------------------------------------------------------------
//
//   taskmgr-rs - GPU 性能页面
//
//   文件:       src/pages/gpu/mod.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Owns the classic GPU page controls, histories, workers and layout.
//!
//! The page never performs DXGI, PDH, SetupAPI or D3D queries on the UI thread. Completed worker
//! inventory, dynamic, and metadata snapshots are committed independently by generation. A failed
//! source leaves other trusted content visible and exposes loading, partial, or stale state.

mod counters;
mod inventory;
pub(crate) mod layout;
mod metadata;
pub(crate) mod model;
#[cfg(test)]
mod source_tests;

use std::array;
use std::collections::{HashMap, HashSet};
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_GEN_FAILURE, ERROR_INVALID_DATA, ERROR_INVALID_WINDOW_HANDLE,
    GetLastError, HWND, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{HDC, InvalidateRect};
use windows_sys::Win32::UI::Controls::{SetScrollInfo, ShowScrollBar};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::Shell::{
    SFBS_FLAGS_ROUND_TO_NEAREST_DISPLAYED_DIGIT, StrFormatByteSizeEx,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, CB_ADDSTRING, CB_GETCURSEL, CB_RESETCONTENT, CB_SETCURSEL, CBN_SELCHANGE,
    DeferWindowPos, EndDeferWindowPos, GetClientRect, GetDlgItem, GetScrollInfo, GetWindowRect,
    MapDialogRect, SB_BOTTOM, SB_ENDSCROLL, SB_LINEDOWN, SB_LINEUP, SB_PAGEDOWN, SB_PAGEUP,
    SB_THUMBPOSITION, SB_THUMBTRACK, SB_TOP, SB_VERT, SCROLLINFO, SIF_ALL, SIF_PAGE, SIF_POS,
    SIF_RANGE, SWP_NOACTIVATE, SWP_NOREDRAW, SWP_NOZORDER, SendMessageW, SetWindowTextW,
    WM_GETFONT, WM_SETFONT, WM_SETREDRAW,
};

use self::counters::GpuCollector;
use self::layout::{
    DETAIL_ROW_COUNT, ENGINE_SLOT_COUNT, GpuLayoutMetrics, GpuLayoutPlan, compute_gpu_layout,
};
use self::metadata::GpuMetadataCollector;
use self::model::{
    GpuAdapterId, GpuAdapterInfo, GpuAdapterMetadata, GpuAdapterSample, GpuCollectOutcome,
    GpuDynamicSnapshot, GpuEngineId, GpuEngineKind, GpuInventorySnapshot, GpuMetadataRequest,
    GpuMetadataSnapshot, GpuSampleError,
};
use crate::config::options::Options;
use crate::infrastructure::native::{
    pause_redraw_for_visible_windows, record_win32_error, redraw_window_tree,
    resume_redraw_for_windows, to_wide_null,
};
use crate::infrastructure::worker::{SingleFlightWorker, keep_pending, replace_pending};
use crate::pages::performance::drawing::{
    GRAPH_GRID, HIST_SIZE, HistoryPlotLayout, HistorySeries, draw_graph_label, draw_grid_width,
    draw_grid_width_gpu, draw_history_series, draw_history_series_gpu,
};
use crate::ui::charts::{ChartColor, ChartRenderer};
use crate::ui::drawing::{HistoryBuffer, fill_black};
use crate::ui::localization::{TextKey, text};
use crate::ui::resource_ids::{
    IDC_GPU_DEDICATED_CAPTION, IDC_GPU_DEDICATED_GRAPH, IDC_GPU_DEDICATED_MEMORY_LABEL,
    IDC_GPU_DEDICATED_MEMORY_VALUE, IDC_GPU_DETAILS_GROUP, IDC_GPU_DIRECTX_LABEL,
    IDC_GPU_DIRECTX_VALUE, IDC_GPU_DRIVER_DATE_LABEL, IDC_GPU_DRIVER_DATE_VALUE,
    IDC_GPU_DRIVER_VERSION_LABEL, IDC_GPU_DRIVER_VERSION_VALUE, IDC_GPU_ENGINE_GRAPH_FIRST,
    IDC_GPU_ENGINE_PERCENT_FIRST, IDC_GPU_ENGINE_SELECTOR_FIRST, IDC_GPU_LOCATION_LABEL,
    IDC_GPU_LOCATION_VALUE, IDC_GPU_METRICS_GROUP, IDC_GPU_MODEL, IDC_GPU_RESERVED_MEMORY_LABEL,
    IDC_GPU_RESERVED_MEMORY_VALUE, IDC_GPU_SELECTOR, IDC_GPU_SHARED_CAPTION, IDC_GPU_SHARED_GRAPH,
    IDC_GPU_SHARED_MEMORY_LABEL, IDC_GPU_SHARED_MEMORY_VALUE, IDC_GPU_STATUS,
    IDC_GPU_TEMPERATURE_LABEL, IDC_GPU_TEMPERATURE_VALUE, IDC_GPU_TOTAL_MEMORY_LABEL,
    IDC_GPU_TOTAL_MEMORY_VALUE, IDC_GPU_UTILIZATION_LABEL, IDC_GPU_UTILIZATION_VALUE,
    PWM_GPU_METADATA_WORKER_COMPLETE, PWM_GPU_WORKER_COMPLETE,
};

const GRAPH_COUNT: usize = 6;
const MEMORY_DEDICATED_GRAPH_INDEX: usize = 4;
const MEMORY_SHARED_GRAPH_INDEX: usize = 5;
const GPU_METRIC_ROWS: [(i32, i32); DETAIL_ROW_COUNT] = [
    (IDC_GPU_UTILIZATION_LABEL, IDC_GPU_UTILIZATION_VALUE),
    (IDC_GPU_TOTAL_MEMORY_LABEL, IDC_GPU_TOTAL_MEMORY_VALUE),
    (
        IDC_GPU_DEDICATED_MEMORY_LABEL,
        IDC_GPU_DEDICATED_MEMORY_VALUE,
    ),
    (IDC_GPU_SHARED_MEMORY_LABEL, IDC_GPU_SHARED_MEMORY_VALUE),
    (IDC_GPU_TEMPERATURE_LABEL, IDC_GPU_TEMPERATURE_VALUE),
];
const GPU_DETAIL_ROWS: [(i32, i32); DETAIL_ROW_COUNT] = [
    (IDC_GPU_DRIVER_VERSION_LABEL, IDC_GPU_DRIVER_VERSION_VALUE),
    (IDC_GPU_DRIVER_DATE_LABEL, IDC_GPU_DRIVER_DATE_VALUE),
    (IDC_GPU_DIRECTX_LABEL, IDC_GPU_DIRECTX_VALUE),
    (IDC_GPU_LOCATION_LABEL, IDC_GPU_LOCATION_VALUE),
    (IDC_GPU_RESERVED_MEMORY_LABEL, IDC_GPU_RESERVED_MEMORY_VALUE),
];

type GpuWorkerCompletion = Result<GpuCollectOutcome, GpuSampleError>;
type GpuMetadataCompletion = Result<GpuMetadataSnapshot, GpuSampleError>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum GpuPageStatus {
    #[default]
    Loading,
    LoadingPerformance,
    LoadingDetails,
    Ready,
    Partial,
    NoHardware,
    Unsupported,
    Failed,
    Stale,
}

struct AdapterHistory {
    engine_histories: HashMap<GpuEngineId, HistoryBuffer>,
    engine_kinds: HashMap<GpuEngineId, GpuEngineKind>,
    dedicated_history: HistoryBuffer,
    shared_history: HistoryBuffer,
}

impl AdapterHistory {
    fn new() -> Self {
        Self {
            engine_histories: HashMap::new(),
            engine_kinds: HashMap::new(),
            dedicated_history: HistoryBuffer::zeroed(HIST_SIZE),
            shared_history: HistoryBuffer::zeroed(HIST_SIZE),
        }
    }

    fn push(&mut self, sample: &GpuAdapterSample) {
        let current: HashMap<_, _> = sample
            .engines
            .iter()
            .map(|engine| (engine.id, engine.utilization_percent))
            .collect();
        for (engine_id, history) in &mut self.engine_histories {
            history.push(current.get(engine_id).copied().unwrap_or(0));
        }
        for engine in &sample.engines {
            self.engine_kinds.insert(engine.id, engine.kind.clone());
            self.engine_histories.entry(engine.id).or_insert_with(|| {
                let mut history = HistoryBuffer::zeroed(HIST_SIZE);
                history.push(engine.utilization_percent);
                history
            });
        }
        debug_assert!(
            current
                .keys()
                .all(|engine_id| self.engine_histories.contains_key(engine_id))
        );

        self.dedicated_history.push(memory_percentage(
            sample.dedicated_usage_bytes,
            sample.info.dedicated_limit_bytes,
        ));
        self.shared_history.push(memory_percentage(
            sample.shared_usage_bytes,
            sample.info.shared_limit_bytes,
        ));
    }

    fn engine_options(&self) -> Vec<(GpuEngineId, GpuEngineKind)> {
        let mut options: Vec<_> = self
            .engine_kinds
            .iter()
            .map(|(id, kind)| (*id, kind.clone()))
            .collect();
        options.sort_by(|left, right| {
            engine_priority(&left.1)
                .cmp(&engine_priority(&right.1))
                .then_with(|| left.0.ordinal.cmp(&right.0.ordinal))
                .then_with(|| left.1.cmp(&right.1))
        });
        options
    }
}

pub(crate) struct GpuPageState {
    hwnd: HWND,
    main_hwnd: HWND,
    hwnd_tabs: HWND,
    no_title: bool,
    layout_metrics: Option<GpuLayoutMetrics>,
    chart_renderer: ChartRenderer,
    sample_worker: Option<SingleFlightWorker<(), GpuWorkerCompletion>>,
    metadata_worker: Option<SingleFlightWorker<GpuMetadataRequest, GpuMetadataCompletion>>,
    inventory: Option<GpuInventorySnapshot>,
    dynamic_snapshot: Option<GpuDynamicSnapshot>,
    metadata: HashMap<GpuAdapterId, GpuAdapterMetadata>,
    status: GpuPageStatus,
    last_dynamic_error: Option<GpuSampleError>,
    last_metadata_error: Option<GpuSampleError>,
    last_dynamic_aux_errors: Vec<GpuSampleError>,
    last_metadata_aux_errors: Vec<GpuSampleError>,
    selected_adapter: Option<GpuAdapterId>,
    adapter_signature: Vec<(GpuAdapterId, String)>,
    adapter_options: Vec<GpuAdapterId>,
    current_engine_options: Vec<(GpuEngineId, GpuEngineKind)>,
    engine_selections: HashMap<GpuAdapterId, [Option<GpuEngineId>; ENGINE_SLOT_COUNT]>,
    histories: HashMap<GpuAdapterId, AdapterHistory>,
    graph_labels: [Vec<u16>; GRAPH_COUNT],
    graph_scroll_offset: i32,
    page_scroll_position: i32,
    content_height: i32,
    plot_points: Vec<POINT>,
}

impl GpuPageState {
    pub(crate) fn new() -> Self {
        let mut state = Self {
            hwnd: null_mut(),
            main_hwnd: null_mut(),
            hwnd_tabs: null_mut(),
            no_title: false,
            layout_metrics: None,
            chart_renderer: ChartRenderer::new(),
            sample_worker: None,
            metadata_worker: None,
            inventory: None,
            dynamic_snapshot: None,
            metadata: HashMap::new(),
            status: GpuPageStatus::Loading,
            last_dynamic_error: None,
            last_metadata_error: None,
            last_dynamic_aux_errors: Vec::new(),
            last_metadata_aux_errors: Vec::new(),
            selected_adapter: None,
            adapter_signature: Vec::new(),
            adapter_options: Vec::new(),
            current_engine_options: Vec::new(),
            engine_selections: HashMap::new(),
            histories: HashMap::new(),
            graph_labels: array::from_fn(|_| Vec::new()),
            graph_scroll_offset: 0,
            page_scroll_position: 0,
            content_height: 0,
            plot_points: Vec::new(),
        };
        state.graph_labels[MEMORY_DEDICATED_GRAPH_INDEX] =
            to_wide_null(text(TextKey::GpuDedicatedMemory));
        state.graph_labels[MEMORY_SHARED_GRAPH_INDEX] =
            to_wide_null(text(TextKey::GpuSharedMemory));
        state
    }

    pub(crate) fn initialize(
        &mut self,
        hwnd: HWND,
        main_hwnd: HWND,
        hwnd_tabs: HWND,
    ) -> Result<(), u32> {
        self.hwnd = hwnd;
        self.main_hwnd = main_hwnd;
        self.hwnd_tabs = hwnd_tabs;
        self.layout_metrics = Some(self.capture_layout_metrics()?);
        self.sync_graph_fonts()?;
        self.start_workers()?;
        self.update_status_text();
        self.clear_visible_values();
        self.size_page();
        Ok(())
    }

    fn sync_graph_fonts(&self) -> Result<(), u32> {
        let source = self.control(IDC_GPU_ENGINE_PERCENT_FIRST);
        if source.is_null() {
            return Err(ERROR_INVALID_WINDOW_HANDLE);
        }
        let percentage_font = unsafe { SendMessageW(source, WM_GETFONT, 0, 0) };
        if percentage_font == 0 {
            return Err(ERROR_INVALID_DATA);
        }
        for slot in 0..ENGINE_SLOT_COUNT {
            let graph = self.control(IDC_GPU_ENGINE_GRAPH_FIRST + slot as i32);
            if graph.is_null() {
                return Err(ERROR_INVALID_WINDOW_HANDLE);
            }
            unsafe { SendMessageW(graph, WM_SETFONT, percentage_font as usize, 0) };
        }
        for graph_id in [IDC_GPU_DEDICATED_GRAPH, IDC_GPU_SHARED_GRAPH] {
            let graph = self.control(graph_id);
            if graph.is_null() {
                return Err(ERROR_INVALID_WINDOW_HANDLE);
            }
            unsafe { SendMessageW(graph, WM_SETFONT, percentage_font as usize, 0) };
        }
        Ok(())
    }

    fn start_workers(&mut self) -> Result<(), u32> {
        if self.sample_worker.is_some() && self.metadata_worker.is_some() {
            return Ok(());
        }
        let sample_worker = SingleFlightWorker::spawn_initialized(
            "taskmgr-rs-gpu-worker",
            PWM_GPU_WORKER_COMPLETE,
            keep_pending,
            || {
                let mut collector = GpuCollector::new();
                move |()| collector.collect()
            },
        )?;
        let metadata_worker = SingleFlightWorker::spawn_initialized(
            "taskmgr-rs-gpu-metadata-worker",
            PWM_GPU_METADATA_WORKER_COMPLETE,
            replace_pending,
            || {
                let mut collector = GpuMetadataCollector::new();
                move |request| collector.collect(request)
            },
        )?;
        self.sample_worker = Some(sample_worker);
        self.metadata_worker = Some(metadata_worker);
        Ok(())
    }

    pub(crate) fn apply_options(&mut self, options: &Options) -> bool {
        let no_title = options.no_title();
        if self.no_title != no_title {
            self.no_title = no_title;
            self.page_scroll_position = 0;
            true
        } else {
            false
        }
    }

    pub(crate) fn timer_event(&mut self) {
        self.request_refresh();
    }

    fn request_refresh(&mut self) {
        if self.hwnd.is_null() {
            return;
        }
        let Some(worker) = self.sample_worker.as_mut() else {
            self.handle_dynamic_error(GpuSampleError::Win32 {
                context: "GPU worker state",
                code: ERROR_BROKEN_PIPE,
            });
            return;
        };
        match worker.request((), self.hwnd) {
            Ok(_) => {}
            Err(code) => self.handle_dynamic_error(GpuSampleError::Win32 {
                context: "GPU worker request",
                code,
            }),
        }
    }

    fn request_metadata(&mut self, request: GpuMetadataRequest) {
        let Some(worker) = self.metadata_worker.as_mut() else {
            self.handle_metadata_error(GpuSampleError::Win32 {
                context: "GPU metadata worker state",
                code: ERROR_BROKEN_PIPE,
            });
            return;
        };
        match worker.request(request, self.hwnd) {
            Ok(_) => {
                self.update_status();
            }
            Err(code) => self.handle_metadata_error(GpuSampleError::Win32 {
                context: "GPU metadata worker request",
                code,
            }),
        }
    }

    pub(crate) fn handle_worker_completion(&mut self) {
        let drained = match self.sample_worker.as_mut() {
            Some(worker) => worker.drain(self.hwnd),
            None => return,
        };
        let mut request_follow_up = false;
        if let Some(error) = drained.error {
            self.sample_worker = None;
            self.handle_dynamic_error(GpuSampleError::Win32 {
                context: "GPU worker completion channel",
                code: error,
            });
        }

        for completion in drained.completions {
            match completion {
                Ok(GpuCollectOutcome::Inventory(inventory)) => {
                    self.commit_inventory(inventory);
                    request_follow_up = true;
                }
                Ok(GpuCollectOutcome::AwaitingBaseline { generation }) => {
                    if self.inventory.as_ref().map(|value| value.generation) == Some(generation) {
                        self.update_status();
                    }
                }
                Ok(GpuCollectOutcome::Dynamic(snapshot)) => self.commit_dynamic_snapshot(snapshot),
                Err(error) => self.handle_dynamic_error(error),
            }
        }

        if request_follow_up
            && !self
                .sample_worker
                .as_ref()
                .is_some_and(SingleFlightWorker::is_in_flight)
        {
            self.request_refresh();
        }
    }

    pub(crate) fn handle_metadata_worker_completion(&mut self) {
        let drained = match self.metadata_worker.as_mut() {
            Some(worker) => worker.drain(self.hwnd),
            None => return,
        };
        if let Some(error) = drained.error {
            self.metadata_worker = None;
            self.handle_metadata_error(GpuSampleError::Win32 {
                context: "GPU metadata worker completion channel",
                code: error,
            });
        }
        for completion in drained.completions {
            match completion {
                Ok(snapshot) => self.commit_metadata_snapshot(snapshot),
                Err(error) => self.handle_metadata_error(error),
            }
        }
        self.update_status();
    }

    fn commit_inventory(&mut self, inventory: GpuInventorySnapshot) {
        let active_ids: HashSet<_> = inventory.adapters.iter().map(|info| info.id).collect();
        if active_ids.len() != inventory.adapters.len() {
            self.handle_dynamic_error(GpuSampleError::InvalidData {
                context: "duplicate GPU inventory adapter identity",
            });
            return;
        }
        let generation_changed = snapshot_generation_changed(
            self.inventory.as_ref().map(|current| current.generation),
            inventory.generation,
        );
        if generation_changed {
            self.dynamic_snapshot = None;
            self.metadata.clear();
            self.histories.clear();
            self.current_engine_options.clear();
            self.last_dynamic_aux_errors.clear();
            self.last_metadata_aux_errors.clear();
            self.last_dynamic_error = None;
            self.last_metadata_error = None;
        }
        self.histories.retain(|id, _| active_ids.contains(id));
        self.engine_selections
            .retain(|id, _| active_ids.contains(id));

        let signature: Vec<_> = inventory
            .adapters
            .iter()
            .map(|info| (info.id, info.name.clone()))
            .collect();
        let adapter_list_changed = self.adapter_signature != signature;
        self.adapter_signature = signature;
        self.inventory = Some(inventory);

        if self
            .selected_adapter
            .is_none_or(|selected| !active_ids.contains(&selected))
        {
            self.selected_adapter = self
                .inventory
                .as_ref()
                .and_then(|snapshot| snapshot.adapters.first())
                .map(|info| info.id);
        }
        if adapter_list_changed {
            self.rebuild_adapter_combo();
        }

        if let Some(inventory) = self.inventory.as_ref()
            && !inventory.adapters.is_empty()
        {
            self.request_metadata(GpuMetadataRequest {
                generation: inventory.generation,
                adapters: inventory.adapters.clone(),
            });
        }
        self.update_status();
        self.sync_selected_adapter();
    }

    fn commit_dynamic_snapshot(&mut self, snapshot: GpuDynamicSnapshot) {
        let Some(inventory) = self.inventory.as_ref() else {
            self.handle_dynamic_error(GpuSampleError::InvalidData {
                context: "GPU dynamic snapshot before inventory",
            });
            return;
        };
        if snapshot.generation != inventory.generation {
            return;
        }
        if !snapshot_timestamp_advances(
            self.dynamic_snapshot
                .as_ref()
                .map(|current| current.timestamp_ms),
            snapshot.timestamp_ms,
        ) {
            self.handle_dynamic_error(GpuSampleError::InvalidData {
                context: "GPU snapshot timestamp is not monotonic",
            });
            return;
        }
        let active_ids: HashSet<_> = snapshot
            .adapters
            .iter()
            .map(|adapter| adapter.info.id)
            .collect();
        let inventory_ids: HashSet<_> = inventory.adapters.iter().map(|info| info.id).collect();
        if active_ids.len() != snapshot.adapters.len() || active_ids != inventory_ids {
            self.handle_dynamic_error(GpuSampleError::InvalidData {
                context: "GPU dynamic adapter completeness",
            });
            return;
        }
        for adapter in &snapshot.adapters {
            self.histories
                .entry(adapter.info.id)
                .or_insert_with(AdapterHistory::new)
                .push(adapter);
        }
        self.record_dynamic_aux_errors(&snapshot);
        self.dynamic_snapshot = Some(snapshot);
        self.last_dynamic_error = None;
        self.graph_scroll_offset = (self.graph_scroll_offset + 2) % GRAPH_GRID;
        self.update_status();
        self.sync_selected_adapter();
    }

    fn commit_metadata_snapshot(&mut self, snapshot: GpuMetadataSnapshot) {
        let Some(inventory) = self.inventory.as_ref() else {
            return;
        };
        if snapshot.generation != inventory.generation {
            return;
        }
        let expected: HashSet<_> = inventory.adapters.iter().map(|info| info.id).collect();
        let actual: HashSet<_> = snapshot
            .adapters
            .iter()
            .map(|metadata| metadata.id)
            .collect();
        if actual.len() != snapshot.adapters.len() || actual != expected {
            self.handle_metadata_error(GpuSampleError::InvalidData {
                context: "GPU metadata adapter completeness",
            });
            return;
        }
        self.record_metadata_aux_errors(&snapshot);
        self.metadata = snapshot
            .adapters
            .into_iter()
            .map(|metadata| (metadata.id, metadata))
            .collect();
        self.last_metadata_error = None;
        self.update_status();
        self.sync_selected_adapter();
    }

    fn handle_dynamic_error(&mut self, error: GpuSampleError) {
        if self.last_dynamic_error.as_ref() != Some(&error) {
            error.record();
        }
        let unsupported = error.is_unsupported();
        self.last_dynamic_error = Some(error);
        self.status = status_after_refresh_error(self.dynamic_snapshot.is_some(), unsupported);
        self.update_status_text();
        self.sync_selected_adapter();
    }

    fn handle_metadata_error(&mut self, error: GpuSampleError) {
        if self.last_metadata_error.as_ref() != Some(&error) {
            error.record();
        }
        self.last_metadata_error = Some(error);
        self.update_status();
        self.sync_selected_adapter();
    }

    fn rebuild_adapter_combo(&mut self) {
        let selector = self.control(IDC_GPU_SELECTOR);
        if selector.is_null() {
            return;
        }
        self.adapter_options.clear();
        unsafe { SendMessageW(selector, WM_SETREDRAW, 0, 0) };
        unsafe { SendMessageW(selector, CB_RESETCONTENT, 0, 0) };
        if let Some(inventory) = self.inventory.as_ref() {
            for (index, info) in inventory.adapters.iter().enumerate() {
                let label = format!("GPU {index} - {}", info.name);
                let wide = to_wide_null(&label);
                unsafe { SendMessageW(selector, CB_ADDSTRING, 0, wide.as_ptr() as isize) };
                self.adapter_options.push(info.id);
            }
        }
        let selected_index = self
            .selected_adapter
            .and_then(|selected| self.adapter_options.iter().position(|id| *id == selected))
            .map(|index| index as isize)
            .unwrap_or(-1);
        unsafe {
            SendMessageW(selector, CB_SETCURSEL, selected_index as usize, 0);
            EnableWindow(selector, (!self.adapter_options.is_empty()).into());
            SendMessageW(selector, WM_SETREDRAW, 1, 0);
            InvalidateRect(selector, null(), 1);
        }
    }

    fn sync_selected_adapter(&mut self) {
        self.update_status_text();
        let Some(info) = self.selected_info().cloned() else {
            self.current_engine_options.clear();
            self.clear_engine_combos();
            self.clear_visible_values();
            self.invalidate_graphs();
            return;
        };
        let sample = self.selected_sample().cloned();
        let metadata = self.metadata.get(&info.id).cloned();

        set_control_text(self.control(IDC_GPU_MODEL), &info.name);
        if sample.is_some() {
            let engine_options = self
                .histories
                .get(&info.id)
                .map(AdapterHistory::engine_options)
                .unwrap_or_default();
            if self.current_engine_options != engine_options {
                self.current_engine_options = engine_options;
                self.rebuild_engine_combos(info.id);
            } else {
                self.ensure_engine_selections(info.id);
                self.sync_engine_combo_selection(info.id);
            }
            self.update_graph_labels(info.id);
        } else {
            self.clear_engine_combos();
        }
        self.update_visible_values(&info, sample.as_ref(), metadata.as_ref());
        self.invalidate_graphs();
    }

    fn selected_info(&self) -> Option<&std::sync::Arc<GpuAdapterInfo>> {
        let selected = self.selected_adapter?;
        self.inventory
            .as_ref()?
            .adapters
            .iter()
            .find(|info| info.id == selected)
    }

    fn selected_sample(&self) -> Option<&GpuAdapterSample> {
        let selected = self.selected_adapter?;
        self.dynamic_snapshot
            .as_ref()?
            .adapters
            .iter()
            .find(|adapter| adapter.info.id == selected)
    }

    fn rebuild_engine_combos(&mut self, adapter: GpuAdapterId) {
        self.ensure_engine_selections(adapter);
        for slot in 0..ENGINE_SLOT_COUNT {
            let combo = self.control(IDC_GPU_ENGINE_SELECTOR_FIRST + slot as i32);
            if combo.is_null() {
                continue;
            }
            unsafe {
                SendMessageW(combo, WM_SETREDRAW, 0, 0);
                SendMessageW(combo, CB_RESETCONTENT, 0, 0);
            }
            for (id, kind) in &self.current_engine_options {
                let label = format_engine_label(kind, id.ordinal);
                let wide = to_wide_null(&label);
                unsafe { SendMessageW(combo, CB_ADDSTRING, 0, wide.as_ptr() as isize) };
            }
            unsafe {
                EnableWindow(combo, (!self.current_engine_options.is_empty()).into());
                SendMessageW(combo, WM_SETREDRAW, 1, 0);
                InvalidateRect(combo, null(), 1);
            }
        }
        self.sync_engine_combo_selection(adapter);
    }

    fn ensure_engine_selections(&mut self, adapter: GpuAdapterId) {
        let defaults = default_engine_slots(&self.current_engine_options);
        let available: HashSet<_> = self
            .current_engine_options
            .iter()
            .map(|(id, _)| *id)
            .collect();
        let selections = self.engine_selections.entry(adapter).or_insert(defaults);
        for slot in 0..ENGINE_SLOT_COUNT {
            if selections[slot].is_none_or(|id| !available.contains(&id)) {
                selections[slot] = defaults[slot];
            }
        }
    }

    fn sync_engine_combo_selection(&self, adapter: GpuAdapterId) {
        let selections = self
            .engine_selections
            .get(&adapter)
            .copied()
            .unwrap_or([None; ENGINE_SLOT_COUNT]);
        for (slot, selected) in selections.into_iter().enumerate() {
            let combo = self.control(IDC_GPU_ENGINE_SELECTOR_FIRST + slot as i32);
            if combo.is_null() {
                continue;
            }
            let index = selected
                .and_then(|id| {
                    self.current_engine_options
                        .iter()
                        .position(|(option, _)| *option == id)
                })
                .map(|index| index as isize)
                .unwrap_or(-1);
            unsafe { SendMessageW(combo, CB_SETCURSEL, index as usize, 0) };
        }
    }

    fn update_graph_labels(&mut self, adapter: GpuAdapterId) {
        let selections = self
            .engine_selections
            .get(&adapter)
            .copied()
            .unwrap_or([None; ENGINE_SLOT_COUNT]);
        for (slot, selection) in selections.into_iter().enumerate() {
            let label = selection
                .and_then(|id| {
                    self.current_engine_options
                        .iter()
                        .find(|(candidate, _)| *candidate == id)
                        .map(|(id, kind)| format_engine_label(kind, id.ordinal))
                })
                .unwrap_or_else(|| text(TextKey::NotAvailable).to_string());
            self.graph_labels[slot] = to_wide_null(&label);
            set_control_text(
                self.control(IDC_GPU_ENGINE_GRAPH_FIRST + slot as i32),
                &label,
            );
        }
        set_control_text(
            self.control(IDC_GPU_DEDICATED_GRAPH),
            text(TextKey::GpuDedicatedMemory),
        );
        set_control_text(
            self.control(IDC_GPU_SHARED_GRAPH),
            text(TextKey::GpuSharedMemory),
        );
    }

    fn update_visible_values(
        &self,
        info: &GpuAdapterInfo,
        sample: Option<&GpuAdapterSample>,
        metadata: Option<&GpuAdapterMetadata>,
    ) {
        let unavailable = text(TextKey::NotAvailable);
        let dynamic_placeholder = if self.last_dynamic_error.is_some() {
            unavailable
        } else {
            "--"
        };
        let metadata_placeholder = if self.last_metadata_error.is_some() {
            unavailable
        } else {
            "--"
        };
        let usage_by_engine: HashMap<_, _> = sample
            .map(|sample| {
                sample
                    .engines
                    .iter()
                    .map(|engine| (engine.id, engine.utilization_percent))
                    .collect()
            })
            .unwrap_or_default();
        let selections = self
            .engine_selections
            .get(&info.id)
            .copied()
            .unwrap_or([None; ENGINE_SLOT_COUNT]);
        for (slot, selected) in selections.into_iter().enumerate() {
            let value = sample.map_or_else(
                || dynamic_placeholder.to_string(),
                |_| {
                    format!(
                        "{}%",
                        selected
                            .and_then(|id| usage_by_engine.get(&id).copied())
                            .unwrap_or(0)
                    )
                },
            );
            set_control_text(
                self.control(IDC_GPU_ENGINE_PERCENT_FIRST + slot as i32),
                &value,
            );
        }

        let dedicated_usage = sample.map(|sample| sample.dedicated_usage_bytes);
        let shared_usage = sample.map(|sample| sample.shared_usage_bytes);
        set_control_text(
            self.control(IDC_GPU_DEDICATED_CAPTION),
            &format!(
                "{}   {}",
                text(TextKey::GpuDedicatedMemory),
                format_optional_usage_limit(
                    dedicated_usage,
                    info.dedicated_limit_bytes,
                    dynamic_placeholder,
                )
            ),
        );
        set_control_text(
            self.control(IDC_GPU_SHARED_CAPTION),
            &format!(
                "{}   {}",
                text(TextKey::GpuSharedMemory),
                format_optional_usage_limit(
                    shared_usage,
                    info.shared_limit_bytes,
                    dynamic_placeholder,
                )
            ),
        );

        let total_usage = dedicated_usage
            .zip(shared_usage)
            .and_then(|(dedicated, shared)| dedicated.checked_add(shared));
        let total_limit = info
            .dedicated_limit_bytes
            .zip(info.shared_limit_bytes)
            .and_then(|(dedicated, shared)| dedicated.checked_add(shared));
        set_control_text(
            self.control(IDC_GPU_UTILIZATION_VALUE),
            &sample
                .map(|sample| format!("{}%", sample.overall_utilization_percent))
                .unwrap_or_else(|| dynamic_placeholder.to_string()),
        );
        set_control_text(
            self.control(IDC_GPU_TOTAL_MEMORY_VALUE),
            &format_optional_usage_limit(total_usage, total_limit, dynamic_placeholder),
        );
        set_control_text(
            self.control(IDC_GPU_DEDICATED_MEMORY_VALUE),
            &format_optional_usage_limit(
                dedicated_usage,
                info.dedicated_limit_bytes,
                dynamic_placeholder,
            ),
        );
        set_control_text(
            self.control(IDC_GPU_SHARED_MEMORY_VALUE),
            &format_optional_usage_limit(
                shared_usage,
                info.shared_limit_bytes,
                dynamic_placeholder,
            ),
        );
        set_control_text(
            self.control(IDC_GPU_TEMPERATURE_VALUE),
            &sample
                .and_then(|sample| sample.temperature_deci_c)
                .map(format_temperature)
                .unwrap_or_else(|| {
                    if sample.is_some() {
                        unavailable.to_string()
                    } else {
                        dynamic_placeholder.to_string()
                    }
                }),
        );
        set_control_text(
            self.control(IDC_GPU_DRIVER_VERSION_VALUE),
            metadata
                .and_then(|metadata| metadata.driver.version.as_deref())
                .unwrap_or_else(|| {
                    metadata_value_placeholder(metadata, metadata_placeholder, unavailable)
                }),
        );
        set_control_text(
            self.control(IDC_GPU_DRIVER_DATE_VALUE),
            metadata
                .and_then(|metadata| metadata.driver.date.as_deref())
                .unwrap_or_else(|| {
                    metadata_value_placeholder(metadata, metadata_placeholder, unavailable)
                }),
        );
        set_control_text(
            self.control(IDC_GPU_DIRECTX_VALUE),
            metadata
                .and_then(|metadata| metadata.directx_feature_level.as_deref())
                .unwrap_or_else(|| {
                    metadata_value_placeholder(metadata, metadata_placeholder, unavailable)
                }),
        );
        set_control_text(
            self.control(IDC_GPU_LOCATION_VALUE),
            metadata
                .and_then(|metadata| metadata.driver.location.as_deref())
                .unwrap_or_else(|| {
                    metadata_value_placeholder(metadata, metadata_placeholder, unavailable)
                }),
        );
        set_control_text(
            self.control(IDC_GPU_RESERVED_MEMORY_VALUE),
            &metadata
                .and_then(|metadata| metadata.hardware_reserved_bytes)
                .map(format_bytes)
                .unwrap_or_else(|| {
                    metadata_value_placeholder(metadata, metadata_placeholder, unavailable)
                        .to_string()
                }),
        );
    }

    fn clear_visible_values(&self) {
        set_control_text(self.control(IDC_GPU_MODEL), "");
        for id in [
            IDC_GPU_UTILIZATION_VALUE,
            IDC_GPU_TOTAL_MEMORY_VALUE,
            IDC_GPU_DEDICATED_MEMORY_VALUE,
            IDC_GPU_SHARED_MEMORY_VALUE,
            IDC_GPU_TEMPERATURE_VALUE,
            IDC_GPU_DRIVER_VERSION_VALUE,
            IDC_GPU_DRIVER_DATE_VALUE,
            IDC_GPU_DIRECTX_VALUE,
            IDC_GPU_LOCATION_VALUE,
            IDC_GPU_RESERVED_MEMORY_VALUE,
        ] {
            set_control_text(self.control(id), text(TextKey::NotAvailable));
        }
        for slot in 0..ENGINE_SLOT_COUNT {
            set_control_text(
                self.control(IDC_GPU_ENGINE_PERCENT_FIRST + slot as i32),
                "--",
            );
        }
        set_control_text(
            self.control(IDC_GPU_DEDICATED_CAPTION),
            text(TextKey::GpuDedicatedMemory),
        );
        set_control_text(
            self.control(IDC_GPU_SHARED_CAPTION),
            text(TextKey::GpuSharedMemory),
        );
    }

    fn clear_engine_combos(&mut self) {
        self.current_engine_options.clear();
        for slot in 0..ENGINE_SLOT_COUNT {
            let combo = self.control(IDC_GPU_ENGINE_SELECTOR_FIRST + slot as i32);
            unsafe {
                SendMessageW(combo, CB_RESETCONTENT, 0, 0);
                EnableWindow(combo, 0);
            }
            self.graph_labels[slot] = to_wide_null(text(TextKey::NotAvailable));
        }
    }

    fn update_status_text(&self) {
        let value = match self.status {
            GpuPageStatus::Loading => text(TextKey::GpuLoading),
            GpuPageStatus::LoadingPerformance => text(TextKey::GpuLoadingPerformance),
            GpuPageStatus::LoadingDetails => text(TextKey::GpuLoadingDetails),
            GpuPageStatus::Ready => "",
            GpuPageStatus::Partial => text(TextKey::GpuPartialDetails),
            GpuPageStatus::NoHardware => text(TextKey::NoHardwareGpusFound),
            GpuPageStatus::Unsupported => text(TextKey::GpuRequiresWddm2),
            GpuPageStatus::Failed => text(TextKey::GpuRefreshFailed),
            GpuPageStatus::Stale => text(TextKey::GpuRefreshFailedStale),
        };
        set_control_text(self.control(IDC_GPU_STATUS), value);
    }

    fn update_status(&mut self) {
        self.status = self.derive_status();
        self.update_status_text();
    }

    fn derive_status(&self) -> GpuPageStatus {
        let Some(inventory) = self.inventory.as_ref() else {
            return match self.last_dynamic_error.as_ref() {
                Some(error) if error.is_unsupported() => GpuPageStatus::Unsupported,
                Some(_) => GpuPageStatus::Failed,
                None => GpuPageStatus::Loading,
            };
        };
        if inventory.adapters.is_empty() {
            return GpuPageStatus::NoHardware;
        }
        if let Some(error) = self.last_dynamic_error.as_ref() {
            return if self.dynamic_snapshot.is_some() {
                GpuPageStatus::Stale
            } else if error.is_unsupported() {
                GpuPageStatus::Unsupported
            } else {
                GpuPageStatus::Failed
            };
        }
        if self.dynamic_snapshot.is_none() {
            return GpuPageStatus::LoadingPerformance;
        }
        if self.last_metadata_error.is_some()
            || !self.last_dynamic_aux_errors.is_empty()
            || self
                .metadata
                .values()
                .any(|metadata| !metadata.metadata_errors.is_empty())
        {
            return GpuPageStatus::Partial;
        }
        if self
            .metadata_worker
            .as_ref()
            .is_some_and(|worker| worker.is_in_flight() || worker.has_pending())
            || self.metadata.len() != inventory.adapters.len()
        {
            GpuPageStatus::LoadingDetails
        } else {
            GpuPageStatus::Ready
        }
    }

    fn record_dynamic_aux_errors(&mut self, snapshot: &GpuDynamicSnapshot) {
        let mut errors = Vec::new();
        for sample in &snapshot.adapters {
            for error in &sample.row_errors {
                if !errors.contains(error) {
                    errors.push(error.clone());
                }
            }
        }
        for error in &errors {
            if !self.last_dynamic_aux_errors.contains(error) {
                error.record();
            }
        }
        self.last_dynamic_aux_errors = errors;
    }

    fn record_metadata_aux_errors(&mut self, snapshot: &GpuMetadataSnapshot) {
        let mut errors = Vec::new();
        for metadata in &snapshot.adapters {
            for error in &metadata.metadata_errors {
                if !errors.contains(error) {
                    errors.push(error.clone());
                }
            }
        }
        for error in &errors {
            if !self.last_metadata_aux_errors.contains(error) {
                error.record();
            }
        }
        self.last_metadata_aux_errors = errors;
    }

    pub(crate) fn handle_command(&mut self, wparam: WPARAM) -> isize {
        let control_id = (wparam & 0xFFFF) as i32;
        let notification = ((wparam >> 16) & 0xFFFF) as u32;
        if notification != CBN_SELCHANGE {
            return 0;
        }
        if control_id == IDC_GPU_SELECTOR {
            let selection = unsafe { SendMessageW(self.control(control_id), CB_GETCURSEL, 0, 0) };
            if selection >= 0
                && let Some(id) = self.adapter_options.get(selection as usize).copied()
            {
                self.selected_adapter = Some(id);
                self.current_engine_options.clear();
                self.sync_selected_adapter();
            }
            return 1;
        }
        if (IDC_GPU_ENGINE_SELECTOR_FIRST..IDC_GPU_ENGINE_SELECTOR_FIRST + ENGINE_SLOT_COUNT as i32)
            .contains(&control_id)
        {
            let slot = (control_id - IDC_GPU_ENGINE_SELECTOR_FIRST) as usize;
            let selection = unsafe { SendMessageW(self.control(control_id), CB_GETCURSEL, 0, 0) };
            if selection >= 0
                && let Some((engine_id, _)) =
                    self.current_engine_options.get(selection as usize).cloned()
                && let Some(adapter) = self.selected_adapter
            {
                self.engine_selections
                    .entry(adapter)
                    .or_insert([None; ENGINE_SLOT_COUNT])[slot] = Some(engine_id);
                self.update_graph_labels(adapter);
                self.sync_selected_adapter();
            }
            return 1;
        }
        0
    }

    pub(crate) fn is_graph_control(&self, control_id: i32) -> bool {
        (IDC_GPU_ENGINE_GRAPH_FIRST..IDC_GPU_ENGINE_GRAPH_FIRST + ENGINE_SLOT_COUNT as i32)
            .contains(&control_id)
            || matches!(control_id, IDC_GPU_DEDICATED_GRAPH | IDC_GPU_SHARED_GRAPH)
    }

    pub(crate) fn draw_graph(&mut self, hdc: HDC, rect: RECT, control_id: i32) {
        let Some(graph_index) = graph_index(control_id) else {
            return;
        };
        let (history, color) = history_for_graph(
            self.selected_adapter,
            &self.histories,
            &self.engine_selections,
            graph_index,
        );
        let width = (rect.right - rect.left).max(1);
        let height = (rect.bottom - rect.top).max(1);
        let layout = HistoryPlotLayout {
            graph_height: height,
            width,
            scale: 1,
        };

        let rendered_with_d2d = if let Some(frame) = self.chart_renderer.begin_frame(hdc, rect) {
            let bounds = frame.bounds();
            frame.clear_black();
            draw_grid_width_gpu(&frame, &bounds, width, self.graph_scroll_offset);
            if let Some(history) = history {
                draw_history_series_gpu(
                    &frame,
                    &bounds,
                    layout,
                    HistorySeries {
                        history,
                        color,
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
            if let Some(history) = history {
                draw_history_series(
                    hdc,
                    &rect,
                    layout,
                    HistorySeries {
                        history,
                        color,
                        stop_on_zero: false,
                    },
                    &mut self.plot_points,
                );
            }
        }
        let label = &self.graph_labels[graph_index];
        draw_graph_label(self.control(control_id), hdc, &rect, label, label);
    }

    fn capture_layout_metrics(&self) -> Result<GpuLayoutMetrics, u32> {
        let mut dialog_units = RECT {
            left: 0,
            top: 0,
            right: 4,
            bottom: 8,
        };
        if unsafe { MapDialogRect(self.hwnd, &mut dialog_units) } == 0 {
            return Err(last_error_or_gen_failure());
        }
        let mapped_width = dialog_units.right - dialog_units.left;
        let mapped_height = dialog_units.bottom - dialog_units.top;
        if mapped_width <= 0 || mapped_height <= 0 {
            return Err(ERROR_INVALID_DATA);
        }
        let base_x = (mapped_width + 3) / 4;
        let base_y = (mapped_height + 7) / 8;

        let combo_visible_height = self
            .control_height(IDC_GPU_SELECTOR)?
            .max(self.control_height(IDC_GPU_ENGINE_SELECTOR_FIRST)?);
        let mut text_line_height = 0;
        for control_id in [
            IDC_GPU_MODEL,
            IDC_GPU_STATUS,
            IDC_GPU_ENGINE_PERCENT_FIRST,
            IDC_GPU_DEDICATED_CAPTION,
            IDC_GPU_SHARED_CAPTION,
        ] {
            text_line_height = text_line_height.max(self.control_height(control_id)?);
        }

        Ok(GpuLayoutMetrics::new(
            base_x,
            base_y,
            text_line_height,
            combo_visible_height,
        ))
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
        let height = rect
            .bottom
            .checked_sub(rect.top)
            .filter(|height| *height > 0)
            .ok_or(ERROR_INVALID_DATA)?;
        Ok(height)
    }

    pub(crate) fn size_page(&mut self) -> bool {
        if self.hwnd.is_null() {
            return false;
        }
        let mut client = unsafe { zeroed::<RECT>() };
        if unsafe { GetClientRect(self.hwnd, &mut client) } == 0 {
            record_win32_error("GPU page client rectangle", last_error_or_gen_failure());
            return false;
        }
        let Some(metrics) = self.layout_metrics else {
            record_win32_error("GPU page layout metrics", ERROR_INVALID_DATA);
            return false;
        };
        let initial_layout = compute_gpu_layout(client, metrics, self.page_scroll_position);
        unsafe {
            ShowScrollBar(self.hwnd, SB_VERT, initial_layout.needs_scrollbar.into());
        }
        if unsafe { GetClientRect(self.hwnd, &mut client) } == 0 {
            record_win32_error(
                "GPU page client rectangle after scrollbar update",
                last_error_or_gen_failure(),
            );
            return false;
        }
        let layout = compute_gpu_layout(client, metrics, self.page_scroll_position);
        if let Err(error) = self.commit_layout(&layout) {
            record_win32_error("GPU page layout commit", error);
            return false;
        }

        self.content_height = layout.content_height;
        self.page_scroll_position = layout.scroll_position;
        let available_height = (client.bottom - client.top).max(0);
        let scroll_info = SCROLLINFO {
            cbSize: size_of::<SCROLLINFO>() as u32,
            fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
            nMin: 0,
            nMax: (self.content_height - 1).max(0),
            nPage: available_height as u32,
            nPos: self.page_scroll_position,
            nTrackPos: 0,
        };
        unsafe { SetScrollInfo(self.hwnd, SB_VERT, &scroll_info, 1) };
        true
    }

    fn commit_layout(&self, layout: &GpuLayoutPlan) -> Result<(), u32> {
        let placements = self.layout_placements(layout)?;
        let defer_hint = i32::try_from(placements.len()).map_err(|_| ERROR_GEN_FAILURE)?;
        let mut hdwp = unsafe { BeginDeferWindowPos(defer_hint) };
        if hdwp.is_null() {
            return Err(last_error_or_gen_failure());
        }
        let windows: Vec<_> = placements.iter().map(|(hwnd, _)| *hwnd).collect();
        let paused_windows = pause_redraw_for_visible_windows(&windows);
        let flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW;
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
                    flags,
                )
            };
            if next.is_null() {
                let error = last_error_or_gen_failure();
                resume_redraw_for_windows(&paused_windows);
                return Err(error);
            }
            hdwp = next;
        }
        if unsafe { EndDeferWindowPos(hdwp) } == 0 {
            let error = last_error_or_gen_failure();
            resume_redraw_for_windows(&paused_windows);
            return Err(error);
        }
        resume_redraw_for_windows(&paused_windows);
        Ok(())
    }

    fn layout_placements(&self, layout: &GpuLayoutPlan) -> Result<Vec<(HWND, RECT)>, u32> {
        let mut placements = Vec::with_capacity(41);
        let mut add = |control_id, rect| {
            let hwnd = self.control(control_id);
            if hwnd.is_null() {
                Err(ERROR_INVALID_WINDOW_HANDLE)
            } else {
                placements.push((hwnd, rect));
                Ok(())
            }
        };

        add(IDC_GPU_SELECTOR, layout.selector)?;
        add(IDC_GPU_MODEL, layout.model)?;
        add(IDC_GPU_STATUS, layout.status)?;
        for slot in 0..ENGINE_SLOT_COUNT {
            add(
                IDC_GPU_ENGINE_SELECTOR_FIRST + slot as i32,
                layout.engine_selectors[slot],
            )?;
            add(
                IDC_GPU_ENGINE_PERCENT_FIRST + slot as i32,
                layout.engine_percentages[slot],
            )?;
            add(
                IDC_GPU_ENGINE_GRAPH_FIRST + slot as i32,
                layout.engine_graphs[slot],
            )?;
        }
        add(IDC_GPU_DEDICATED_CAPTION, layout.dedicated_caption)?;
        add(IDC_GPU_DEDICATED_GRAPH, layout.dedicated_graph)?;
        add(IDC_GPU_SHARED_CAPTION, layout.shared_caption)?;
        add(IDC_GPU_SHARED_GRAPH, layout.shared_graph)?;
        add(IDC_GPU_METRICS_GROUP, layout.metrics_group)?;
        add(IDC_GPU_DETAILS_GROUP, layout.details_group)?;
        for index in 0..DETAIL_ROW_COUNT {
            add(GPU_METRIC_ROWS[index].0, layout.metric_labels[index])?;
            add(GPU_METRIC_ROWS[index].1, layout.metric_values[index])?;
            add(GPU_DETAIL_ROWS[index].0, layout.detail_labels[index])?;
            add(GPU_DETAIL_ROWS[index].1, layout.detail_values[index])?;
        }
        Ok(placements)
    }

    pub(crate) fn handle_vscroll(&mut self, wparam: WPARAM) -> isize {
        let mut info = SCROLLINFO {
            cbSize: size_of::<SCROLLINFO>() as u32,
            fMask: SIF_ALL,
            ..unsafe { zeroed() }
        };
        if unsafe { GetScrollInfo(self.hwnd, SB_VERT, &mut info) } == 0 {
            record_win32_error("GPU page scroll state", last_error_or_gen_failure());
            return 1;
        }
        let command = (wparam & 0xFFFF) as i32;
        let line = 24;
        let page = i32::try_from(info.nPage).unwrap_or(i32::MAX).max(line);
        let next = match command {
            SB_TOP => 0,
            SB_BOTTOM => self.content_height,
            SB_LINEUP => self.page_scroll_position - line,
            SB_LINEDOWN => self.page_scroll_position + line,
            SB_PAGEUP => self.page_scroll_position - page,
            SB_PAGEDOWN => self.page_scroll_position + page,
            SB_THUMBPOSITION | SB_THUMBTRACK => info.nTrackPos,
            SB_ENDSCROLL => return 0,
            _ => self.page_scroll_position,
        };
        let max_scroll = (self.content_height - page).max(0);
        let next = next.clamp(0, max_scroll);
        if next != self.page_scroll_position {
            let previous = self.page_scroll_position;
            self.page_scroll_position = next;
            if self.size_page() {
                redraw_window_tree(self.hwnd);
            } else {
                self.page_scroll_position = previous;
            }
        }
        1
    }

    pub(crate) fn handle_mouse_wheel(&mut self, wparam: WPARAM) -> isize {
        let delta = ((wparam >> 16) & 0xFFFF) as i16 as i32;
        if delta == 0 {
            return 0;
        }
        let steps = -(delta / 120).clamp(-3, 3);
        let command = if steps < 0 { SB_LINEUP } else { SB_LINEDOWN };
        for _ in 0..steps.unsigned_abs() {
            self.handle_vscroll(command as usize);
        }
        1
    }

    pub(crate) fn no_title(&self) -> bool {
        self.no_title
    }

    pub(crate) fn destroy(&mut self) {
        self.sample_worker = None;
        self.metadata_worker = None;
        self.inventory = None;
        self.dynamic_snapshot = None;
        self.metadata.clear();
        self.last_dynamic_aux_errors.clear();
        self.last_metadata_aux_errors.clear();
        self.histories.clear();
        self.engine_selections.clear();
        self.layout_metrics = None;
        self.hwnd = null_mut();
        self.main_hwnd = null_mut();
        self.hwnd_tabs = null_mut();
    }

    fn invalidate_graphs(&self) {
        unsafe {
            for slot in 0..ENGINE_SLOT_COUNT {
                InvalidateRect(
                    self.control(IDC_GPU_ENGINE_GRAPH_FIRST + slot as i32),
                    null(),
                    0,
                );
            }
            InvalidateRect(self.control(IDC_GPU_DEDICATED_GRAPH), null(), 0);
            InvalidateRect(self.control(IDC_GPU_SHARED_GRAPH), null(), 0);
        }
    }

    fn control(&self, id: i32) -> HWND {
        if self.hwnd.is_null() {
            null_mut()
        } else {
            unsafe { GetDlgItem(self.hwnd, id) }
        }
    }
}

impl Default for GpuPageState {
    fn default() -> Self {
        Self::new()
    }
}

fn history_for_graph<'a>(
    selected_adapter: Option<GpuAdapterId>,
    histories: &'a HashMap<GpuAdapterId, AdapterHistory>,
    engine_selections: &HashMap<GpuAdapterId, [Option<GpuEngineId>; ENGINE_SLOT_COUNT]>,
    graph_index: usize,
) -> (Option<&'a HistoryBuffer>, ChartColor) {
    let Some(adapter) = selected_adapter else {
        return (None, ChartColor::Green);
    };
    let Some(history) = histories.get(&adapter) else {
        return (None, ChartColor::Green);
    };
    match graph_index {
        0..=3 => {
            let selected = engine_selections
                .get(&adapter)
                .and_then(|selections| selections[graph_index]);
            (
                selected.and_then(|id| history.engine_histories.get(&id)),
                ChartColor::Green,
            )
        }
        MEMORY_DEDICATED_GRAPH_INDEX => (Some(&history.dedicated_history), ChartColor::Yellow),
        MEMORY_SHARED_GRAPH_INDEX => (Some(&history.shared_history), ChartColor::Yellow),
        _ => (None, ChartColor::Green),
    }
}

fn memory_percentage(usage: u64, limit: Option<u64>) -> u8 {
    let Some(limit) = limit.filter(|limit| *limit != 0) else {
        return 0;
    };
    let rounded = (u128::from(usage) * 100 + u128::from(limit) / 2) / u128::from(limit);
    rounded.min(100) as u8
}

fn status_after_refresh_error(has_snapshot: bool, unsupported: bool) -> GpuPageStatus {
    if has_snapshot {
        GpuPageStatus::Stale
    } else if unsupported {
        GpuPageStatus::Unsupported
    } else {
        GpuPageStatus::Failed
    }
}

fn snapshot_generation_changed(current: Option<u64>, candidate: u64) -> bool {
    current.is_some_and(|current| current != candidate)
}

fn snapshot_timestamp_advances(current: Option<u64>, candidate: u64) -> bool {
    current.is_none_or(|current| current < candidate)
}

fn engine_priority(kind: &GpuEngineKind) -> u8 {
    match kind {
        GpuEngineKind::ThreeD => 0,
        GpuEngineKind::Copy => 1,
        GpuEngineKind::VideoEncode => 2,
        GpuEngineKind::VideoDecode => 3,
        GpuEngineKind::Compute => 4,
        GpuEngineKind::Security => 5,
        GpuEngineKind::Other(_) => 6,
    }
}

fn default_engine_slots(
    options: &[(GpuEngineId, GpuEngineKind)],
) -> [Option<GpuEngineId>; ENGINE_SLOT_COUNT] {
    let preferred = [
        GpuEngineKind::ThreeD,
        GpuEngineKind::Copy,
        GpuEngineKind::VideoEncode,
        GpuEngineKind::VideoDecode,
    ];
    let mut selected = Vec::with_capacity(ENGINE_SLOT_COUNT);
    for preferred_kind in &preferred {
        if let Some((id, _)) = options
            .iter()
            .find(|(id, kind)| kind == preferred_kind && !selected.contains(id))
        {
            selected.push(*id);
        }
    }
    for (id, _) in options {
        if selected.len() == ENGINE_SLOT_COUNT {
            break;
        }
        if !selected.contains(id) {
            selected.push(*id);
        }
    }

    let mut slots = [None; ENGINE_SLOT_COUNT];
    for (slot, id) in slots.iter_mut().zip(selected) {
        *slot = Some(id);
    }
    slots
}

fn localized_engine_name(kind: &GpuEngineKind) -> String {
    match kind {
        GpuEngineKind::ThreeD => text(TextKey::GpuEngine3D).to_string(),
        GpuEngineKind::Copy => text(TextKey::GpuEngineCopy).to_string(),
        GpuEngineKind::VideoEncode => text(TextKey::GpuEngineVideoEncode).to_string(),
        GpuEngineKind::VideoDecode => text(TextKey::GpuEngineVideoDecode).to_string(),
        GpuEngineKind::Compute => text(TextKey::GpuEngineCompute).to_string(),
        GpuEngineKind::Security => text(TextKey::GpuEngineSecurity).to_string(),
        GpuEngineKind::Other(value) => value.clone(),
    }
}

fn format_engine_label(kind: &GpuEngineKind, ordinal: u32) -> String {
    format!("{} {ordinal}", localized_engine_name(kind))
}

fn format_bytes(bytes: u64) -> String {
    let mut buffer = [0u16; 48];
    let status = unsafe {
        StrFormatByteSizeEx(
            bytes,
            SFBS_FLAGS_ROUND_TO_NEAREST_DISPLAYED_DIGIT,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
    };
    if status >= 0 {
        let length = buffer
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..length])
    } else {
        format!("{bytes} B")
    }
}

fn format_usage_limit(usage: u64, limit: Option<u64>) -> String {
    match limit {
        Some(limit) => format!("{} / {}", format_bytes(usage), format_bytes(limit)),
        None => format!("{} / {}", format_bytes(usage), text(TextKey::NotAvailable)),
    }
}

fn format_optional_usage_limit(usage: Option<u64>, limit: Option<u64>, missing: &str) -> String {
    match (usage, limit) {
        (Some(usage), limit) => format_usage_limit(usage, limit),
        (None, Some(limit)) => format!("{missing} / {}", format_bytes(limit)),
        (None, None) => missing.to_string(),
    }
}

fn metadata_value_placeholder<'a>(
    metadata: Option<&GpuAdapterMetadata>,
    pending: &'a str,
    unavailable: &'a str,
) -> &'a str {
    if metadata.is_some() {
        unavailable
    } else {
        pending
    }
}

fn format_temperature(deci_c: u32) -> String {
    format!("{}.{:01} °C", deci_c / 10, deci_c % 10)
}

fn graph_index(control_id: i32) -> Option<usize> {
    if (IDC_GPU_ENGINE_GRAPH_FIRST..IDC_GPU_ENGINE_GRAPH_FIRST + ENGINE_SLOT_COUNT as i32)
        .contains(&control_id)
    {
        Some((control_id - IDC_GPU_ENGINE_GRAPH_FIRST) as usize)
    } else if control_id == IDC_GPU_DEDICATED_GRAPH {
        Some(MEMORY_DEDICATED_GRAPH_INDEX)
    } else if control_id == IDC_GPU_SHARED_GRAPH {
        Some(MEMORY_SHARED_GRAPH_INDEX)
    } else {
        None
    }
}

fn last_error_or_gen_failure() -> u32 {
    let error = unsafe { GetLastError() };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

fn set_control_text(hwnd: HWND, value: &str) {
    if hwnd.is_null() {
        return;
    }
    let wide = to_wide_null(value);
    unsafe { SetWindowTextW(hwnd, wide.as_ptr()) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::gpu::model::AdapterLuid;
    use std::sync::Arc;

    fn engine(ordinal: u32, kind: GpuEngineKind) -> (GpuEngineId, GpuEngineKind) {
        (
            GpuEngineId {
                adapter: GpuAdapterId {
                    luid: AdapterLuid {
                        high_part: 0,
                        low_part: 1,
                    },
                    physical_index: 0,
                },
                ordinal,
            },
            kind,
        )
    }

    fn staged_adapter() -> Arc<GpuAdapterInfo> {
        Arc::new(GpuAdapterInfo {
            id: GpuAdapterId {
                luid: AdapterLuid {
                    high_part: 0,
                    low_part: 7,
                },
                physical_index: 0,
            },
            enumeration_index: 0,
            name: "Staged GPU".to_string(),
            vendor_id: 1,
            device_id: 2,
            subsystem_id: 3,
            revision: 4,
            dedicated_limit_bytes: Some(8 * 1024 * 1024 * 1024),
            shared_limit_bytes: Some(4 * 1024 * 1024 * 1024),
        })
    }

    #[test]
    fn staged_sources_make_trustworthy_content_visible_independently() {
        let info = staged_adapter();
        let mut state = GpuPageState::new();
        state.inventory = Some(GpuInventorySnapshot {
            generation: 1,
            adapters: vec![Arc::clone(&info)],
        });
        assert_eq!(state.derive_status(), GpuPageStatus::LoadingPerformance);

        state.dynamic_snapshot = Some(GpuDynamicSnapshot {
            generation: 1,
            timestamp_ms: 1,
            adapters: vec![GpuAdapterSample {
                info: Arc::clone(&info),
                overall_utilization_percent: 5,
                engines: Vec::new(),
                dedicated_usage_bytes: 10,
                shared_usage_bytes: 20,
                temperature_deci_c: Some(420),
                row_errors: Vec::new(),
            }],
        });
        assert_eq!(state.derive_status(), GpuPageStatus::LoadingDetails);

        state.metadata.insert(
            info.id,
            GpuAdapterMetadata {
                id: info.id,
                hardware_reserved_bytes: None,
                driver: Default::default(),
                directx_feature_level: None,
                metadata_errors: Vec::new(),
            },
        );
        assert_eq!(state.derive_status(), GpuPageStatus::Ready);

        state
            .metadata
            .get_mut(&info.id)
            .unwrap()
            .metadata_errors
            .push(GpuSampleError::InvalidData {
                context: "GPU staged metadata test",
            });
        assert_eq!(state.derive_status(), GpuPageStatus::Partial);
    }

    #[test]
    fn default_slots_follow_class_priority_then_ordinal() {
        let mut options = vec![
            engine(8, GpuEngineKind::VideoDecode),
            engine(5, GpuEngineKind::ThreeD),
            engine(4, GpuEngineKind::Copy),
            engine(2, GpuEngineKind::Copy),
            engine(9, GpuEngineKind::VideoEncode),
            engine(3, GpuEngineKind::Other("VR".to_string())),
        ];
        options.sort_by(|left, right| {
            engine_priority(&left.1)
                .cmp(&engine_priority(&right.1))
                .then_with(|| left.0.ordinal.cmp(&right.0.ordinal))
        });
        let slots = default_engine_slots(&options);
        assert_eq!(
            slots.map(|slot| slot.map(|id| id.ordinal)),
            [Some(5), Some(2), Some(9), Some(8)]
        );
    }

    #[test]
    fn memory_percentage_uses_wide_math_and_ui_clamp() {
        assert_eq!(memory_percentage(u64::MAX, Some(u64::MAX)), 100);
        assert_eq!(memory_percentage(1, Some(3)), 33);
        assert_eq!(memory_percentage(1, None), 0);
        assert_eq!(memory_percentage(1, Some(0)), 0);
    }

    #[test]
    fn refresh_errors_only_claim_a_stale_sample_when_one_exists() {
        assert_eq!(
            status_after_refresh_error(false, false),
            GpuPageStatus::Failed
        );
        assert_eq!(
            status_after_refresh_error(false, true),
            GpuPageStatus::Unsupported
        );
        assert_eq!(
            status_after_refresh_error(true, false),
            GpuPageStatus::Stale
        );
    }

    #[test]
    fn topology_generation_change_requires_fresh_histories() {
        assert!(!snapshot_generation_changed(None, 1));
        assert!(!snapshot_generation_changed(Some(2), 2));
        assert!(snapshot_generation_changed(Some(2), 3));
    }

    #[test]
    fn snapshot_time_must_advance_before_history_does() {
        assert!(snapshot_timestamp_advances(None, 0));
        assert!(snapshot_timestamp_advances(Some(10), 11));
        assert!(!snapshot_timestamp_advances(Some(10), 10));
        assert!(!snapshot_timestamp_advances(Some(10), 9));
    }

    #[test]
    fn graph_ids_map_to_stable_slots() {
        for slot in 0..ENGINE_SLOT_COUNT {
            assert_eq!(
                graph_index(IDC_GPU_ENGINE_GRAPH_FIRST + slot as i32),
                Some(slot)
            );
        }
        assert_eq!(
            graph_index(IDC_GPU_DEDICATED_GRAPH),
            Some(MEMORY_DEDICATED_GRAPH_INDEX)
        );
        assert_eq!(
            graph_index(IDC_GPU_SHARED_GRAPH),
            Some(MEMORY_SHARED_GRAPH_INDEX)
        );
    }
}
