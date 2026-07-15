use std::collections::HashMap;

// 网络页实现。
// 该模块轮询网卡统计信息，维护历史曲线，并同步底部列表与顶部图表区域。
use std::mem::zeroed;
use std::ptr::null_mut;
use std::slice;
use std::sync::mpsc::TryRecvError;
use std::time::Instant;

use windows_sys::Win32::Foundation::{HWND, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, DrawTextW,
    FillRect, GetDC, GetStockObject, InvalidateRect, LineTo, MapWindowPoints, MoveToEx, ReleaseDC,
    SelectObject, SetBkMode, SetDCPenColor, SetTextColor, BLACK_BRUSH, DC_PEN, DT_CALCRECT,
    DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_TOP, HBITMAP, HBRUSH, HDC, HGDIOBJ, SRCCOPY,
    TRANSPARENT,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, IF_TYPE_SOFTWARE_LOOPBACK, IF_TYPE_TUNNEL, MIB_IF_ROW2,
    MIB_IF_TABLE2,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::UI::Controls::{
    SetScrollInfo, LVCFMT_LEFT, LVCFMT_RIGHT, LVCF_FMT, LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH,
    LVCOLUMNW, LVIF_PARAM, LVIF_TEXT, LVITEMW, LVM_DELETECOLUMN, LVM_DELETEITEM, LVM_GETITEMCOUNT,
    LVM_GETITEMW, LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMW,
    LVS_EX_FULLROWSELECT, LVS_EX_HEADERDRAGDROP, TCM_ADJUSTRECT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, CreateWindowExW, DeferWindowPos, DestroyWindow, EndDeferWindowPos,
    GetClientRect, GetDialogBaseUnits, GetDlgItem, GetScrollInfo, GetSystemMetrics, SendMessageW,
    SetWindowTextW, ShowWindow, BS_OWNERDRAW, HDWP, HMENU, SB_BOTTOM, SB_CTL, SB_LINEDOWN,
    SB_LINEUP, SB_PAGEDOWN, SB_PAGEUP, SB_THUMBPOSITION, SB_THUMBTRACK, SB_TOP, SCROLLINFO,
    SIF_ALL, SIF_PAGE, SIF_POS, SIF_RANGE, SM_CXVSCROLL, SWP_HIDEWINDOW, SWP_NOACTIVATE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, WHEEL_DELTA, WM_GETFONT, WM_SETFONT, WM_SETREDRAW,
    WS_CHILD, WS_DISABLED, WS_EX_CLIENTEDGE, WS_EX_NOPARENTNOTIFY,
};

use crate::background_worker::BackgroundWorker;
use crate::chart_renderer::{ChartColor, ChartFrame, ChartRenderer};
use crate::language::{adapter_state, network_column_titles};
use crate::options::Options;
use crate::resource::{
    IDC_GRAPHSCROLLVERT, IDC_NICGRAPH, IDC_NICTOTALS, IDC_NOADAPTERS, PWM_NET_WORKER_COMPLETE,
};
use crate::winutil::{
    finish_list_view_update, hiword, loword, record_win32_error, subclass_list_view, to_wide_null,
};

const HIST_SIZE: usize = 2000;
const GRAPH_GRID: i32 = 12;
const MIN_GRAPH_HEIGHT: i32 = 120;
const DEFSPACING_BASE: i32 = 3;
const TOPSPACING_BASE: i32 = 10;
const DLG_SCALE_X: i32 = 4;
const DLG_SCALE_Y: i32 = 8;
const FRAME_CLASS_NAME: &str = "TaskManagerFrame";
const BUTTON_CLASS_NAME: &str = "Button";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct AdapterIdentity {
    luid: u64,
    interface_index: u32,
}

struct RawAdapterEntry {
    // 原始网卡快照保持尽量接近系统 API 返回，便于后续去重和稳定排序。
    interface_index: u32,
    key: AdapterIdentity,
    name: String,
    state: String,
    link_speed_bps: u64,
    bytes_sent: u64,
    bytes_received: u64,
}

type NetworkWorkerResult = Result<Vec<RawAdapterEntry>, u32>;

struct NetworkWorkerCompletion {
    sampled_at: Instant,
    result: NetworkWorkerResult,
}

struct HistoryBuffer {
    values: Vec<u8>,
    newest: usize,
    value_counts: [usize; 256],
    max_value: u8,
}

impl HistoryBuffer {
    fn zeroed(len: usize) -> Self {
        let mut value_counts = [0usize; 256];
        value_counts[0] = len;
        Self {
            values: vec![0; len],
            newest: 0,
            value_counts,
            max_value: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    fn len(&self) -> usize {
        self.values.len()
    }

    fn push(&mut self, value: u8) {
        if self.values.is_empty() {
            return;
        }
        self.newest = if self.newest == 0 {
            self.values.len() - 1
        } else {
            self.newest - 1
        };
        let replaced = self.values[self.newest];
        self.value_counts[replaced as usize] -= 1;
        self.values[self.newest] = value;
        self.value_counts[value as usize] += 1;

        if value > self.max_value {
            self.max_value = value;
        } else if replaced == self.max_value && self.value_counts[replaced as usize] == 0 {
            while self.max_value > 0 && self.value_counts[self.max_value as usize] == 0 {
                self.max_value -= 1;
            }
        }
    }

    fn newest_value(&self) -> u8 {
        self.values.get(self.newest).copied().unwrap_or(0)
    }

    fn max_value(&self) -> u8 {
        self.max_value
    }

    fn iter(&self) -> impl Iterator<Item = u8> + '_ {
        (0..self.values.len()).map(|offset| self.values[(self.newest + offset) % self.values.len()])
    }
}

struct NetworkAdapterEntry {
    // 页面最终展示的数据结构，同时包含当前值和三条历史曲线。
    interface_index: u32,
    key: AdapterIdentity,
    name: String,
    state: String,
    link_speed: String,
    utilization: String,
    bytes_sent: String,
    bytes_received: String,
    bytes_total: String,
    current_sent: u64,
    current_received: u64,
    sent_history: HistoryBuffer,
    received_history: HistoryBuffer,
    total_history: HistoryBuffer,
    dirty: bool,
}

struct PreviousAdapterState {
    // 用上一轮字符串化结果做 diff，减少列表里未变化列的重写。
    name: String,
    state: String,
    link_speed: String,
    utilization: String,
    bytes_sent: String,
    bytes_received: String,
    bytes_total: String,
    current_sent: u64,
    current_received: u64,
}

struct NetworkGraphControl {
    // 每个适配器图表由一个 frame + 一个 owner-draw graph 按钮组成。
    frame_hwnd: HWND,
    graph_hwnd: HWND,
}

#[derive(Default)]
pub struct NetworkPageState {
    // 网络页状态对象维护网卡采样缓存、图表窗口以及滚动位置。
    hwnd: HWND,
    main_hwnd: HWND,
    hwnd_tabs: HWND,
    no_title: bool,
    chart_renderer: ChartRenderer,
    adapters: Vec<NetworkAdapterEntry>,
    graphs: Vec<NetworkGraphControl>,
    graphs_per_page: usize,
    first_visible_adapter: usize,
    scroll_offset: i32,
    last_sample_time: Option<Instant>,
    cached_graph_dc: HDC,
    cached_graph_bitmap: HBITMAP,
    cached_graph_bitmap_old: HGDIOBJ,
    cached_graph_width: i32,
    cached_graph_height: i32,
    worker: Option<BackgroundWorker<(), NetworkWorkerCompletion>>,
    collection_in_flight: bool,
    refresh_requested: bool,
    last_refresh_error: Option<u32>,
}

impl NetworkPageState {
    pub fn new() -> Self {
        Self::default()
    }

    pub unsafe fn initialize(
        &mut self,
        hwnd: HWND,
        main_hwnd: HWND,
        hwnd_tabs: HWND,
    ) -> Result<(), u32> {
        // 初始化只建立控件和基础布局；当前页由激活入口采样，隐藏页由首帧后的预热消息采样。
        self.hwnd = hwnd;
        self.main_hwnd = main_hwnd;
        self.hwnd_tabs = hwnd_tabs;
        self.start_worker_thread()?;
        let list = self.list_hwnd();
        if !list.is_null() {
            subclass_list_view(list);
        }
        self.configure_columns();
        self.size_page();
        Ok(())
    }

    pub unsafe fn apply_options(&mut self, options: &Options) {
        // 网络页当前只有无标题布局依赖全局选项，因此这里比较轻量。
        let previous = self.no_title;
        self.no_title = options.no_title();
        if self.hwnd.is_null() || previous == self.no_title {
            return;
        }

        unsafe {
            self.size_page();
        }
    }

    pub unsafe fn no_title(&self) -> bool {
        self.no_title
    }

    pub unsafe fn timer_event(&mut self) {
        // 每轮刷新都先推动网格滚动，再采样并重绘当前可见图表。
        self.scroll_offset = (self.scroll_offset + 2) % GRAPH_GRID;
        self.refresh();
        self.update_graphs();
    }

    pub unsafe fn destroy(&mut self) {
        self.stop_worker_thread();
        self.destroy_graphs();
    }

    fn start_worker_thread(&mut self) -> Result<(), u32> {
        if self.worker.is_some() {
            return Ok(());
        }

        self.worker = Some(BackgroundWorker::spawn(
            "rtaskmgr-network-sampler",
            PWM_NET_WORKER_COMPLETE,
            |()| NetworkWorkerCompletion {
                sampled_at: Instant::now(),
                result: unsafe { NetworkPageState::collect_adapters() },
            },
        )?);
        Ok(())
    }

    fn stop_worker_thread(&mut self) {
        self.worker = None;
        self.collection_in_flight = false;
        self.refresh_requested = false;
    }

    unsafe fn ensure_graph_surface(&mut self, width: i32, height: i32) -> bool {
        if self.cached_graph_dc.is_null()
            || width > self.cached_graph_width
            || height > self.cached_graph_height
        {
            let screen_dc = GetDC(null_mut());
            if screen_dc.is_null() {
                return false;
            }
            let new_dc = CreateCompatibleDC(screen_dc);
            let new_bitmap = CreateCompatibleBitmap(screen_dc, width, height);
            ReleaseDC(null_mut(), screen_dc);
            if new_dc.is_null() || new_bitmap.is_null() {
                if !new_dc.is_null() {
                    DeleteDC(new_dc);
                }
                if !new_bitmap.is_null() {
                    DeleteObject(new_bitmap as _);
                }
                return false;
            }

            let new_old = SelectObject(new_dc, new_bitmap as _);
            if new_old.is_null() || new_old as isize == -1 {
                DeleteObject(new_bitmap as _);
                DeleteDC(new_dc);
                return false;
            }
            if !self.cached_graph_bitmap_old.is_null() {
                SelectObject(self.cached_graph_dc, self.cached_graph_bitmap_old);
            }
            if !self.cached_graph_bitmap.is_null() {
                DeleteObject(self.cached_graph_bitmap as _);
            }
            if !self.cached_graph_dc.is_null() {
                DeleteDC(self.cached_graph_dc);
            }

            self.cached_graph_dc = new_dc;
            self.cached_graph_bitmap = new_bitmap;
            self.cached_graph_bitmap_old = new_old;
            self.cached_graph_width = width;
            self.cached_graph_height = height;
        }
        true
    }

    pub fn graph_pane_index(&self, control_id: i32) -> Option<usize> {
        let pane_index = control_id.saturating_sub(IDC_NICGRAPH) as usize;
        if control_id >= IDC_NICGRAPH && pane_index < self.graphs.len() {
            Some(pane_index)
        } else {
            None
        }
    }

    pub unsafe fn draw_graph(&mut self, hdc: HDC, rect: RECT, pane_index: usize) {
        // 每个图表面板都根据当前适配器的历史数据独立绘制，
        // 但缩放规则保持一致，便于横向比较。
        let width = (rect.right - rect.left).max(1);
        let height = (rect.bottom - rect.top).max(1);
        let adapter_index = pane_index.saturating_add(self.first_visible_adapter());
        let Some(adapter) = self.adapters.get(adapter_index) else {
            return;
        };

        let scale_max = adapter
            .sent_history
            .max_value()
            .max(adapter.received_history.max_value())
            .max(adapter.total_history.max_value());

        if self.draw_graph_gpu(hdc, rect, adapter, scale_max) {
            draw_graph_scale_overlay(hdc, rect, scale_max);
            return;
        }
        if !self.ensure_graph_surface(width, height) {
            return;
        }
        let Some(adapter) = self.adapters.get(adapter_index) else {
            return;
        };

        let old_bitmap = SelectObject(self.cached_graph_dc, self.cached_graph_bitmap as _);
        let local_rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };

        let scale_top = graph_scale_top_value(scale_max);
        let zoom = graph_zoom(scale_max);
        fill_black(self.cached_graph_dc, &local_rect);
        let plot_left = draw_scale(self.cached_graph_dc, &local_rect, scale_top);
        let plot_rect = RECT {
            left: (local_rect.left + plot_left).min(local_rect.right),
            top: local_rect.top,
            right: local_rect.right,
            bottom: local_rect.bottom,
        };

        if plot_rect.right > plot_rect.left {
            draw_grid(self.cached_graph_dc, &plot_rect, self.scroll_offset, zoom);
            draw_history(
                self.cached_graph_dc,
                &plot_rect,
                &adapter.total_history,
                rgb(0, 255, 0),
                zoom,
            );
            draw_history(
                self.cached_graph_dc,
                &plot_rect,
                &adapter.received_history,
                rgb(255, 255, 0),
                zoom,
            );
            draw_history(
                self.cached_graph_dc,
                &plot_rect,
                &adapter.sent_history,
                rgb(255, 0, 0),
                zoom,
            );
        }

        BitBlt(
            hdc,
            rect.left,
            rect.top,
            width,
            height,
            self.cached_graph_dc,
            0,
            0,
            SRCCOPY,
        );
        SelectObject(self.cached_graph_dc, old_bitmap);
    }

    unsafe fn draw_graph_gpu(
        &self,
        hdc: HDC,
        rect: RECT,
        adapter: &NetworkAdapterEntry,
        scale_max: u8,
    ) -> bool {
        let Some(frame) = self.chart_renderer.begin_frame(hdc, rect) else {
            return false;
        };

        let target_rect = frame.bounds();
        let zoom = graph_zoom(scale_max);
        frame.clear_black();
        let plot_left = draw_scale_gpu(&frame, &target_rect);
        let plot_rect = RECT {
            left: (target_rect.left + plot_left).min(target_rect.right),
            top: target_rect.top,
            right: target_rect.right,
            bottom: target_rect.bottom,
        };

        if plot_rect.right > plot_rect.left {
            draw_grid_gpu(&frame, &plot_rect, self.scroll_offset, zoom);
            draw_history_gpu(
                &frame,
                &plot_rect,
                &adapter.total_history,
                ChartColor::Green,
                zoom,
            );
            draw_history_gpu(
                &frame,
                &plot_rect,
                &adapter.received_history,
                ChartColor::Yellow,
                zoom,
            );
            draw_history_gpu(
                &frame,
                &plot_rect,
                &adapter.sent_history,
                ChartColor::Red,
                zoom,
            );
        }

        frame.end()
    }

    pub unsafe fn size_page(&mut self) {
        // 网络页需要同时布局“多块图表 + 滚动条 + 底部列表”，
        // 因此会先算出一页能显示多少图，再决定是否出现滚动条。
        if self.hwnd.is_null() {
            return;
        }

        let list = self.list_hwnd();
        let label = GetDlgItem(self.hwnd, IDC_NOADAPTERS);
        let scrollbar = GetDlgItem(self.hwnd, IDC_GRAPHSCROLLVERT);
        let adapter_count = self.adapters.len();

        let mut parent_rect = zeroed::<RECT>();
        let (def_spacing, top_spacing) = layout_spacing();
        let mut graph_rect = zeroed::<RECT>();
        let mut need_scrollbar = false;

        self.graphs_per_page = 0;

        let graph_history_height = if self.no_title {
            if self.main_hwnd.is_null() {
                return;
            }
            GetClientRect(self.main_hwnd, &mut parent_rect);
            (parent_rect.bottom - parent_rect.top - def_spacing).max(0)
        } else {
            if self.hwnd_tabs.is_null() {
                return;
            }
            GetClientRect(self.hwnd_tabs, &mut parent_rect);
            MapWindowPoints(
                self.hwnd_tabs,
                self.hwnd,
                &mut parent_rect as *mut _ as _,
                2,
            );
            SendMessageW(
                self.hwnd_tabs,
                TCM_ADJUSTRECT,
                0,
                &mut parent_rect as *mut _ as isize,
            );
            ((parent_rect.bottom - parent_rect.top - def_spacing) * 3 / 4).max(0)
        };

        if adapter_count != 0 {
            let scrollbar_width = scrollbar_width();
            self.graphs_per_page = graphs_per_page(graph_history_height, adapter_count);
            self.ensure_graphs(self.graphs_per_page);
            need_scrollbar = adapter_count > self.graphs_per_page;
            graph_rect.left = parent_rect.left + def_spacing;
            graph_rect.right = (parent_rect.right - parent_rect.left)
                - def_spacing * 2
                - if need_scrollbar {
                    scrollbar_width + def_spacing
                } else {
                    0
                };
            graph_rect.top = parent_rect.top + def_spacing;
            graph_rect.bottom = if self.graphs_per_page > 0 {
                graph_history_height / self.graphs_per_page as i32
            } else {
                0
            };
        }

        let mut hdwp = BeginDeferWindowPos(10);
        if hdwp.is_null() {
            return;
        }

        if !scrollbar.is_null() {
            let scrollbar_width = scrollbar_width();
            hdwp = DeferWindowPos(
                hdwp,
                scrollbar,
                null_mut(),
                parent_rect.right - def_spacing - scrollbar_width,
                parent_rect.top + def_spacing,
                scrollbar_width,
                graph_rect.bottom * self.graphs_per_page as i32,
                if need_scrollbar {
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW
                } else {
                    SWP_HIDEWINDOW
                },
            );
        }

        for index in 0..self.graphs.len() {
            if index < self.graphs_per_page {
                let frame = &self.graphs[index];
                hdwp = size_graph(hdwp, frame, &graph_rect, def_spacing, top_spacing);
                graph_rect.top += graph_rect.bottom;
            } else {
                let frame = &self.graphs[index];
                hdwp = hide_graph(hdwp, frame);
            }
        }

        if !list.is_null() {
            let list_left = graph_rect.left;
            let list_top = graph_rect.top + def_spacing;
            let list_right = parent_rect.right - def_spacing;
            let list_bottom = parent_rect.bottom - def_spacing;
            hdwp = DeferWindowPos(
                hdwp,
                list,
                null_mut(),
                list_left,
                list_top,
                (list_right - list_left).max(0),
                (list_bottom - list_top).max(0),
                if adapter_count == 0 {
                    SWP_HIDEWINDOW
                } else {
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW
                },
            );
        }

        if !label.is_null() {
            hdwp = DeferWindowPos(
                hdwp,
                label,
                null_mut(),
                parent_rect.left,
                parent_rect.top + ((parent_rect.bottom - parent_rect.top) / 2) - 40,
                (parent_rect.right - parent_rect.left).max(0),
                (parent_rect.bottom - parent_rect.top).max(0),
                if adapter_count == 0 {
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW
                } else {
                    SWP_HIDEWINDOW
                },
            );
        }

        EndDeferWindowPos(hdwp);

        if need_scrollbar && !scrollbar.is_null() {
            let scroll_info = SCROLLINFO {
                cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE,
                nMin: 0,
                nMax: adapter_count.saturating_sub(self.graphs_per_page) as i32,
                nPage: 1,
                ..zeroed()
            };
            SetScrollInfo(scrollbar, SB_CTL, &scroll_info, 1);
        }

        self.label_graphs();
    }

    pub unsafe fn handle_vscroll(&mut self, wparam: WPARAM) -> isize {
        self.handle_vscroll_steps(wparam, 1)
    }

    unsafe fn handle_vscroll_steps(&mut self, wparam: WPARAM, steps: i32) -> isize {
        let scrollbar = GetDlgItem(self.hwnd, IDC_GRAPHSCROLLVERT);
        if scrollbar.is_null() {
            return 0;
        }

        let mut scroll_info = SCROLLINFO {
            cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
            fMask: SIF_ALL,
            ..zeroed()
        };
        if GetScrollInfo(scrollbar, SB_CTL, &mut scroll_info) == 0 {
            return 0;
        }

        match i32::from(loword(wparam)) {
            SB_BOTTOM => scroll_info.nPos = scroll_info.nMax,
            SB_TOP => scroll_info.nPos = scroll_info.nMin,
            SB_LINEDOWN => scroll_info.nPos += steps,
            SB_LINEUP => scroll_info.nPos -= steps,
            SB_PAGEDOWN => scroll_info.nPos += self.graphs_per_page as i32 * steps,
            SB_PAGEUP => scroll_info.nPos -= self.graphs_per_page as i32 * steps,
            SB_THUMBTRACK | SB_THUMBPOSITION => scroll_info.nPos = i32::from(hiword(wparam)),
            _ => {}
        }

        if scroll_info.nPos < scroll_info.nMin {
            scroll_info.nPos = scroll_info.nMin;
        }
        if scroll_info.nPos > scroll_info.nMax {
            scroll_info.nPos = scroll_info.nMax;
        }

        let next_first_visible = scroll_info.nPos.max(0) as usize;
        scroll_info.fMask = SIF_POS;
        SetScrollInfo(scrollbar, SB_CTL, &scroll_info, 1);
        if self.first_visible_adapter != next_first_visible {
            self.first_visible_adapter = next_first_visible;
            self.label_graphs();
            self.update_graphs();
        }
        1
    }

    pub unsafe fn handle_mouse_wheel(&mut self, wparam: WPARAM) -> isize {
        // 鼠标滚轮被翻译成垂直滚动命令，保持和滚动条一致的行为。
        let delta = i32::from(hiword(wparam) as i16);
        if delta == 0 {
            return 0;
        }

        let wheel_delta = WHEEL_DELTA as i32;
        let step = ((delta.abs() + wheel_delta - 1) / wheel_delta).max(1);
        let command = if delta < 0 { SB_LINEDOWN } else { SB_LINEUP };
        self.handle_vscroll_steps(command as usize, step)
    }

    unsafe fn refresh(&mut self) {
        self.drain_worker_results();
        if self.collection_in_flight {
            self.refresh_requested = true;
            return;
        }

        self.refresh_requested = false;
        self.schedule_collection();
    }

    unsafe fn schedule_collection(&mut self) {
        let Some(worker) = self.worker.as_ref() else {
            self.set_refresh_error(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE);
            return;
        };

        if let Err(error) = worker.submit((), self.hwnd) {
            self.set_refresh_error(error);
            return;
        }
        self.collection_in_flight = true;
    }

    unsafe fn drain_worker_results(&mut self) {
        loop {
            let result = match self.worker.as_ref() {
                Some(worker) => worker.try_recv(),
                None => return,
            };

            match result {
                Ok(completion) => {
                    self.collection_in_flight = false;
                    match completion.result {
                        Ok(adapters) => {
                            self.last_refresh_error = None;
                            self.apply_adapter_snapshot(adapters, completion.sampled_at);
                        }
                        Err(error) => self.set_refresh_error(error),
                    }
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.worker = None;
                    self.collection_in_flight = false;
                    self.set_refresh_error(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE);
                    return;
                }
            }
        }
    }

    fn set_refresh_error(&mut self, error: u32) {
        if self.last_refresh_error != Some(error) {
            record_win32_error("network refresh", error);
        }
        self.last_refresh_error = Some(error);
    }

    pub unsafe fn handle_worker_completion(&mut self) {
        self.drain_worker_results();
        if self.refresh_requested && !self.collection_in_flight {
            self.refresh_requested = false;
            self.schedule_collection();
        }
    }

    unsafe fn apply_adapter_snapshot(
        &mut self,
        raw_adapters: Vec<RawAdapterEntry>,
        sampled_at: Instant,
    ) {
        // UI 提交阶段把完整原始快照转换为列表文本和历史曲线；过程中没有系统查询。
        let raw_adapters = collapse_raw_adapters(raw_adapters);
        let needs_initial_layout = self.last_sample_time.is_none();
        let previous_adapter_count = self.adapters.len();
        let previous_adapter_order = self
            .adapters
            .iter()
            .map(|adapter| adapter.key)
            .collect::<Vec<_>>();
        let elapsed_secs = self
            .last_sample_time
            .replace(sampled_at)
            .map(|previous| sampled_at.duration_since(previous).as_secs_f64())
            .unwrap_or(0.0);

        let mut previous_by_key = HashMap::with_capacity(self.adapters.len());
        for adapter in self.adapters.drain(..) {
            previous_by_key.insert(adapter.key, adapter);
        }

        let mut adapters = Vec::with_capacity(raw_adapters.len());
        let mut adapter_labels_changed = false;
        for raw in raw_adapters {
            let (mut sent_history, mut received_history, mut total_history, previous_state) =
                if let Some(previous_adapter) = previous_by_key.remove(&raw.key) {
                    adapter_labels_changed |= previous_adapter.name != raw.name;
                    let NetworkAdapterEntry {
                        name,
                        state,
                        link_speed,
                        utilization,
                        bytes_sent,
                        bytes_received,
                        bytes_total,
                        current_sent,
                        current_received,
                        sent_history,
                        received_history,
                        total_history,
                        ..
                    } = previous_adapter;

                    (
                        sent_history,
                        received_history,
                        total_history,
                        Some(PreviousAdapterState {
                            name,
                            state,
                            link_speed,
                            utilization,
                            bytes_sent,
                            bytes_received,
                            bytes_total,
                            current_sent,
                            current_received,
                        }),
                    )
                } else {
                    adapter_labels_changed = true;
                    (
                        HistoryBuffer::zeroed(HIST_SIZE),
                        HistoryBuffer::zeroed(HIST_SIZE),
                        HistoryBuffer::zeroed(HIST_SIZE),
                        None,
                    )
                };
            let (sent_delta, received_delta) = if let Some(previous_state) = previous_state.as_ref()
            {
                (
                    raw.bytes_sent.saturating_sub(previous_state.current_sent),
                    raw.bytes_received
                        .saturating_sub(previous_state.current_received),
                )
            } else {
                (0, 0)
            };
            let total_delta = sent_delta.saturating_add(received_delta);

            let sent_util =
                utilization_percent_for_history(sent_delta, raw.link_speed_bps, elapsed_secs);
            let received_util =
                utilization_percent_for_history(received_delta, raw.link_speed_bps, elapsed_secs);
            let total_util =
                utilization_percent_for_history(total_delta, raw.link_speed_bps, elapsed_secs);

            push_history(&mut sent_history, sent_util);
            push_history(&mut received_history, received_util);
            push_history(&mut total_history, total_util);

            let bytes_total = raw.bytes_sent.saturating_add(raw.bytes_received);
            let mut adapter = NetworkAdapterEntry {
                interface_index: raw.interface_index,
                key: raw.key,
                name: raw.name,
                state: raw.state,
                link_speed: format_link_speed(raw.link_speed_bps),
                utilization: utilization_text(total_delta, raw.link_speed_bps, elapsed_secs),
                bytes_sent: format_counter(raw.bytes_sent),
                bytes_received: format_counter(raw.bytes_received),
                bytes_total: format_counter(bytes_total),
                current_sent: raw.bytes_sent,
                current_received: raw.bytes_received,
                sent_history,
                received_history,
                total_history,
                dirty: true,
            };
            if let Some(previous_state) = previous_state.as_ref() {
                adapter.dirty = previous_state.name != adapter.name
                    || previous_state.state != adapter.state
                    || previous_state.link_speed != adapter.link_speed
                    || previous_state.utilization != adapter.utilization
                    || previous_state.bytes_sent != adapter.bytes_sent
                    || previous_state.bytes_received != adapter.bytes_received
                    || previous_state.bytes_total != adapter.bytes_total;
            }
            adapters.push(adapter);
        }

        self.adapters = adapters;
        let labels_changed = adapter_labels_changed
            || previous_adapter_order
                .iter()
                .copied()
                .ne(self.adapters.iter().map(|adapter| adapter.key));
        self.update_listview();
        if needs_initial_layout || previous_adapter_count != self.adapters.len() {
            self.size_page();
        } else if labels_changed {
            self.label_graphs();
        }
    }

    unsafe fn collect_adapters() -> Result<Vec<RawAdapterEntry>, u32> {
        let mut table = null_mut::<MIB_IF_TABLE2>();
        let status = GetIfTable2(&mut table);
        if status != 0 {
            if !table.is_null() {
                FreeMibTable(table as _);
            }
            return Err(status);
        }
        if table.is_null() {
            return Err(windows_sys::Win32::Foundation::ERROR_INVALID_DATA);
        }

        let count = (*table).NumEntries as usize;
        let mut adapters = Vec::with_capacity(count);
        let rows = slice::from_raw_parts((*table).Table.as_ptr(), count);
        for row in rows {
            if !include_adapter(row) {
                continue;
            }

            let mut name = wide_array_to_string(&row.Alias);
            if name.is_empty() {
                name = wide_array_to_string(&row.Description);
            }
            let key = AdapterIdentity {
                luid: row.InterfaceLuid.Value,
                interface_index: row.InterfaceIndex,
            };

            adapters.push(RawAdapterEntry {
                interface_index: row.InterfaceIndex,
                key,
                name,
                state: adapter_state_text(row.OperStatus),
                link_speed_bps: row.ReceiveLinkSpeed.max(row.TransmitLinkSpeed),
                bytes_sent: row.OutOctets,
                bytes_received: row.InOctets,
            });
        }

        FreeMibTable(table as _);
        Ok(adapters)
    }

    unsafe fn configure_columns(&self) {
        let list = self.list_hwnd();
        if list.is_null() {
            return;
        }

        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            (LVS_EX_HEADERDRAGDROP | LVS_EX_FULLROWSELECT) as usize,
            (LVS_EX_HEADERDRAGDROP | LVS_EX_FULLROWSELECT) as isize,
        );

        while SendMessageW(list, LVM_DELETECOLUMN, 0, 0) != 0 {}

        let titles = network_column_titles();
        let columns = [
            (titles[0], 150, LVCFMT_LEFT),
            (titles[1], 96, LVCFMT_RIGHT),
            (titles[2], 90, LVCFMT_RIGHT),
            (titles[3], 120, LVCFMT_LEFT),
        ];

        for (index, (title, width, fmt)) in columns.iter().enumerate() {
            let mut title_wide = to_wide_null(title);
            let mut column = LVCOLUMNW {
                mask: LVCF_FMT | LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM,
                fmt: *fmt,
                cx: *width,
                pszText: title_wide.as_mut_ptr(),
                cchTextMax: title_wide.len() as i32,
                iSubItem: index as i32,
                ..zeroed()
            };
            SendMessageW(
                list,
                LVM_INSERTCOLUMNW,
                index,
                &mut column as *mut _ as isize,
            );
        }
    }

    unsafe fn update_listview(&self) {
        // 列表只在适配器身份变化时替换整行，普通数值更新尽量走原位写回。
        let list = self.list_hwnd();
        if list.is_null() {
            return;
        }

        SendMessageW(list, WM_SETREDRAW, 0, 0);

        let mut existing_count = SendMessageW(list, LVM_GETITEMCOUNT, 0, 0) as usize;
        let common_count = existing_count.min(self.adapters.len());

        for index in 0..common_count {
            let adapter = &self.adapters[index];
            let mut current_item = LVITEMW {
                mask: LVIF_PARAM,
                iItem: index as i32,
                ..zeroed()
            };
            let current_interface_index =
                if SendMessageW(list, LVM_GETITEMW, 0, &mut current_item as *mut _ as isize) != 0 {
                    Some(current_item.lParam as u32)
                } else {
                    None
                };

            if current_interface_index != Some(adapter.interface_index) {
                self.replace_row(list, index, adapter);
            } else if adapter.dirty {
                self.update_row(list, index, adapter);
            }
        }

        while existing_count > self.adapters.len() {
            existing_count -= 1;
            SendMessageW(list, LVM_DELETEITEM, existing_count, 0);
        }

        for index in common_count..self.adapters.len() {
            self.insert_row(list, index, &self.adapters[index]);
        }

        finish_list_view_update(list);
    }

    unsafe fn insert_row(&self, list: HWND, index: usize, adapter: &NetworkAdapterEntry) {
        let mut name = to_wide_null(&adapter.name);
        let mut item = LVITEMW {
            mask: LVIF_TEXT | LVIF_PARAM,
            iItem: index as i32,
            iSubItem: 0,
            pszText: name.as_mut_ptr(),
            cchTextMax: name.len() as i32,
            lParam: adapter.interface_index as isize,
            ..zeroed()
        };
        SendMessageW(list, LVM_INSERTITEMW, 0, &mut item as *mut _ as isize);
        self.update_row(list, index, adapter);
    }

    unsafe fn replace_row(&self, list: HWND, index: usize, adapter: &NetworkAdapterEntry) {
        let mut name = to_wide_null(&adapter.name);
        let mut item = LVITEMW {
            mask: LVIF_TEXT | LVIF_PARAM,
            iItem: index as i32,
            iSubItem: 0,
            pszText: name.as_mut_ptr(),
            cchTextMax: name.len() as i32,
            lParam: adapter.interface_index as isize,
            ..zeroed()
        };
        SendMessageW(list, LVM_SETITEMW, 0, &mut item as *mut _ as isize);
        self.update_row(list, index, adapter);
    }

    unsafe fn update_row(&self, list: HWND, index: usize, adapter: &NetworkAdapterEntry) {
        for (subitem, text) in adapter_row_texts(adapter).iter().enumerate() {
            let mut value = to_wide_null(text);
            let mut subitem_item = LVITEMW {
                mask: LVIF_TEXT,
                iItem: index as i32,
                iSubItem: subitem as i32,
                pszText: value.as_mut_ptr(),
                cchTextMax: value.len() as i32,
                ..zeroed()
            };
            SendMessageW(list, LVM_SETITEMW, 0, &mut subitem_item as *mut _ as isize);
        }
    }

    unsafe fn ensure_graphs(&mut self, required: usize) {
        if required <= self.graphs.len() || self.hwnd.is_null() {
            return;
        }

        let frame_class = to_wide_null(FRAME_CLASS_NAME);
        let button_class = to_wide_null(BUTTON_CLASS_NAME);
        let empty_text = to_wide_null("");
        let font = SendMessageW(self.hwnd, WM_GETFONT, 0, 0);

        while self.graphs.len() < required {
            let graph_id = IDC_NICGRAPH + self.graphs.len() as i32;
            let graph_hwnd = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                button_class.as_ptr(),
                empty_text.as_ptr(),
                WS_CHILD | WS_DISABLED | BS_OWNERDRAW as u32,
                0,
                0,
                0,
                0,
                self.hwnd,
                graph_id as usize as HMENU,
                null_mut(),
                null_mut(),
            );
            let frame_hwnd = CreateWindowExW(
                WS_EX_NOPARENTNOTIFY,
                frame_class.as_ptr(),
                empty_text.as_ptr(),
                0x0000_0007 | WS_CHILD,
                0,
                0,
                0,
                0,
                self.hwnd,
                null_mut(),
                null_mut(),
                null_mut(),
            );
            if graph_hwnd.is_null() || frame_hwnd.is_null() {
                if !graph_hwnd.is_null() {
                    DestroyWindow(graph_hwnd);
                }
                if !frame_hwnd.is_null() {
                    DestroyWindow(frame_hwnd);
                }
                break;
            }

            SendMessageW(frame_hwnd, WM_SETFONT, font as usize, 0);
            SendMessageW(graph_hwnd, WM_SETFONT, font as usize, 0);
            ShowWindow(frame_hwnd, SW_HIDE);
            ShowWindow(graph_hwnd, SW_HIDE);
            self.graphs.push(NetworkGraphControl {
                frame_hwnd,
                graph_hwnd,
            });
        }
    }

    unsafe fn destroy_graphs(&mut self) {
        for graph in self.graphs.drain(..) {
            if !graph.graph_hwnd.is_null() {
                DestroyWindow(graph.graph_hwnd);
            }
            if !graph.frame_hwnd.is_null() {
                DestroyWindow(graph.frame_hwnd);
            }
        }
        if !self.cached_graph_bitmap_old.is_null() {
            SelectObject(self.cached_graph_dc, self.cached_graph_bitmap_old);
        }
        if !self.cached_graph_bitmap.is_null() {
            DeleteObject(self.cached_graph_bitmap as _);
        }
        if !self.cached_graph_dc.is_null() {
            DeleteDC(self.cached_graph_dc);
        }
        self.cached_graph_dc = null_mut();
        self.cached_graph_bitmap = null_mut();
        self.cached_graph_bitmap_old = null_mut();
        self.cached_graph_width = 0;
        self.cached_graph_height = 0;
        self.graphs_per_page = 0;
        self.first_visible_adapter = 0;
    }

    unsafe fn update_graphs(&self) {
        // 只重绘当前一页真正可见的图表，避免隐藏面板也跟着刷新。
        for pane_index in 0..self.graphs_per_page {
            let Some(graph) = self.graphs.get(pane_index) else {
                break;
            };
            InvalidateRect(graph.graph_hwnd, null_mut(), 0);
        }
    }

    unsafe fn label_graphs(&mut self) {
        // 图表标题始终绑定当前可见适配器切片，滚动后要一起更新标题文字。
        let first_visible = self.first_visible_adapter();
        for pane_index in 0..self.graphs_per_page {
            let Some(graph) = self.graphs.get(pane_index) else {
                break;
            };
            if let Some(adapter) = self.adapters.get(first_visible + pane_index) {
                let title = to_wide_null(&adapter.name);
                SetWindowTextW(graph.frame_hwnd, title.as_ptr());
            } else {
                let title = to_wide_null("");
                SetWindowTextW(graph.frame_hwnd, title.as_ptr());
            }
        }
    }

    fn first_visible_adapter(&self) -> usize {
        if self.adapters.is_empty() || self.graphs_per_page == 0 {
            return 0;
        }

        self.first_visible_adapter.min(
            self.adapters
                .len()
                .saturating_sub(self.graphs_per_page.min(self.adapters.len())),
        )
    }

    fn list_hwnd(&self) -> HWND {
        unsafe { GetDlgItem(self.hwnd, IDC_NICTOTALS) }
    }
}

unsafe fn size_graph(
    mut hdwp: HDWP,
    graph: &NetworkGraphControl,
    rect: &RECT,
    def_spacing: i32,
    top_spacing: i32,
) -> HDWP {
    // 单个网络图由“外层 frame + 内层 owner-draw graph”两层控件组成，这里一次性定位它们。
    let graph_width = (rect.right - def_spacing * 2).max(0);
    let graph_height = (rect.bottom - top_spacing - def_spacing).max(0);

    hdwp = DeferWindowPos(
        hdwp,
        graph.frame_hwnd,
        null_mut(),
        rect.left,
        rect.top,
        rect.right,
        rect.bottom,
        SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );

    let left = rect.left + def_spacing;
    let top = rect.top + top_spacing;
    let right = left + graph_width;
    let bottom = top + graph_height;
    DeferWindowPos(
        hdwp,
        graph.graph_hwnd,
        null_mut(),
        left,
        top,
        right - left,
        bottom - top,
        SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
    )
}

unsafe fn hide_graph(mut hdwp: HDWP, graph: &NetworkGraphControl) -> HDWP {
    hdwp = DeferWindowPos(
        hdwp,
        graph.frame_hwnd,
        null_mut(),
        0,
        0,
        0,
        0,
        SWP_NOZORDER | SWP_NOACTIVATE | SWP_HIDEWINDOW,
    );

    DeferWindowPos(
        hdwp,
        graph.graph_hwnd,
        null_mut(),
        0,
        0,
        0,
        0,
        SWP_NOZORDER | SWP_NOACTIVATE | SWP_HIDEWINDOW,
    )
}

fn graphs_per_page(graph_height: i32, adapter_count: usize) -> usize {
    // 每页图表数量基于当前可用高度动态决定，但不会低于最小可读高度。
    if graph_height <= 0 || adapter_count == 0 {
        return 0;
    }

    let average_height = (graph_height / adapter_count as i32).max(MIN_GRAPH_HEIGHT);
    if graph_height > average_height {
        (graph_height / average_height).max(1) as usize
    } else {
        1
    }
}

fn scrollbar_width() -> i32 {
    unsafe { GetSystemMetrics(SM_CXVSCROLL).max(17) }
}

fn push_history(history: &mut HistoryBuffer, value: u8) {
    history.push(value);
}

fn adapter_row_texts(adapter: &NetworkAdapterEntry) -> [&str; 4] {
    [
        &adapter.name,
        &adapter.utilization,
        &adapter.link_speed,
        &adapter.state,
    ]
}

fn include_adapter(row: &MIB_IF_ROW2) -> bool {
    // 经典任务管理器不显示 loopback / tunnel，这里保持相同过滤策略。
    row.Type != IF_TYPE_SOFTWARE_LOOPBACK && row.Type != IF_TYPE_TUNNEL
}

fn collapse_raw_adapters(adapters: Vec<RawAdapterEntry>) -> Vec<RawAdapterEntry> {
    let mut interface_to_index = HashMap::<u32, usize>::with_capacity(adapters.len());
    let mut collapsed = Vec::<RawAdapterEntry>::with_capacity(adapters.len());
    for adapter in adapters {
        if let Some(&index) = interface_to_index.get(&adapter.interface_index) {
            if raw_adapter_rank(&adapter) > raw_adapter_rank(&collapsed[index]) {
                collapsed[index] = adapter;
            }
        } else {
            interface_to_index.insert(adapter.interface_index, collapsed.len());
            collapsed.push(adapter);
        }
    }
    collapsed.sort_unstable_by_key(|adapter| (adapter.interface_index, adapter.key.luid));
    collapsed
}

fn raw_adapter_rank(adapter: &RawAdapterEntry) -> (u8, u8, i64) {
    (
        u8::from(adapter.link_speed_bps != 0),
        u8::from(!adapter.state.eq_ignore_ascii_case("disconnected")),
        -(adapter.name.len() as i64),
    )
}

fn wide_array_to_string(value: &[u16]) -> String {
    let end = value.iter().position(|&ch| ch == 0).unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end]).trim().to_string()
}

fn adapter_state_text(oper_status: i32) -> String {
    // 操作状态来自 IP Helper API 的枚举值，这里映射成 UI 层展示文案。
    if oper_status == IfOperStatusUp {
        adapter_state("Connected").to_string()
    } else {
        match oper_status {
            2 => adapter_state("Disconnected").to_string(),
            3 => adapter_state("Connecting").to_string(),
            4 => adapter_state("Disconnecting").to_string(),
            5 => adapter_state("Hardware Missing").to_string(),
            6 => adapter_state("Hardware Disabled").to_string(),
            7 => adapter_state("Hardware Malfunction").to_string(),
            _ => adapter_state("Unknown").to_string(),
        }
    }
}

fn utilization_ratio_percent(
    bytes_per_interval: u64,
    link_speed_bps: u64,
    elapsed_secs: f64,
) -> Option<f64> {
    if bytes_per_interval == 0 || link_speed_bps == 0 || elapsed_secs <= 0.0 {
        return None;
    }

    let bits_per_second = (bytes_per_interval as f64 * 8.0) / elapsed_secs;
    Some(((bits_per_second * 100.0) / link_speed_bps as f64).clamp(0.0, 100.0))
}

fn utilization_percent_for_history(
    bytes_per_interval: u64,
    link_speed_bps: u64,
    elapsed_secs: f64,
) -> u8 {
    let Some(ratio_percent) =
        utilization_ratio_percent(bytes_per_interval, link_speed_bps, elapsed_secs)
    else {
        return 0;
    };

    let rounded = ratio_percent.round().clamp(0.0, 100.0) as u8;
    if rounded == 0 && ratio_percent > 0.0 {
        1
    } else {
        rounded
    }
}

fn utilization_text(bytes_per_interval: u64, link_speed_bps: u64, elapsed_secs: f64) -> String {
    let Some(ratio_percent) =
        utilization_ratio_percent(bytes_per_interval, link_speed_bps, elapsed_secs)
    else {
        return "0%".to_string();
    };

    if ratio_percent > 0.0 && ratio_percent < 1.0 {
        "<1%".to_string()
    } else {
        format!("{}%", ratio_percent.round().clamp(0.0, 100.0) as u8)
    }
}

fn format_link_speed(bits_per_second: u64) -> String {
    // 链路速率采用十进制网络单位显示，更符合网卡/交换机常见标注方式。
    if bits_per_second == 0 {
        return "-".to_string();
    }

    let units = ["bps", "Kbps", "Mbps", "Gbps", "Tbps"];
    let mut value = bits_per_second as f64;
    let mut unit = 0usize;
    while value >= 1000.0 && unit + 1 < units.len() {
        value /= 1000.0;
        unit += 1;
    }

    if value >= 100.0 || unit == 0 {
        format!("{value:.0} {}", units[unit])
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

fn format_counter(value: u64) -> String {
    if value == 0 {
        return "0".to_string();
    }

    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index != 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(ch);
    }
    output
}

unsafe fn fill_black(hdc: HDC, rect: &RECT) {
    FillRect(hdc, rect, GetStockObject(BLACK_BRUSH) as HBRUSH);
}

unsafe fn draw_scale(hdc: HDC, rect: &RECT, max_scale_value: u32) -> i32 {
    // 刻度区单独占据左侧一列，返回值是后续真正绘图区域的左边界偏移。
    let top_text = format_scale_label(max_scale_value as f32);
    let middle_text = format_scale_label(max_scale_value as f32 / 2.0);
    let bottom_text = "0 %";
    let (sample_width, sample_height) = measure_graph_text(hdc, " 100 %");
    let (top_width, top_height) = measure_graph_text(hdc, &top_text);
    let (middle_width, middle_height) = measure_graph_text(hdc, &middle_text);
    let (bottom_width, bottom_height) = measure_graph_text(hdc, bottom_text);
    let scale_width = sample_width
        .max(top_width)
        .max(middle_width)
        .max(bottom_width);
    let scale_height = sample_height
        .max(top_height)
        .max(middle_height)
        .max(bottom_height);
    let divider_x = rect.left + scale_width;

    draw_graph_text(
        hdc,
        RECT {
            left: rect.left,
            top: rect.top,
            right: divider_x - 3,
            bottom: rect.top + scale_height,
        },
        &top_text,
        rgb(255, 255, 0),
        DT_RIGHT,
    );
    draw_graph_text(
        hdc,
        RECT {
            left: rect.left,
            top: rect.top + ((rect.bottom - rect.top - scale_height) / 2),
            right: divider_x - 3,
            bottom: rect.top + ((rect.bottom - rect.top + scale_height) / 2),
        },
        &middle_text,
        rgb(255, 255, 0),
        DT_RIGHT,
    );
    draw_graph_text(
        hdc,
        RECT {
            left: rect.left,
            top: rect.bottom - scale_height,
            right: divider_x - 3,
            bottom: rect.bottom,
        },
        bottom_text,
        rgb(255, 255, 0),
        DT_RIGHT,
    );

    let old_pen = SelectObject(hdc, GetStockObject(DC_PEN) as _);
    SetDCPenColor(hdc, rgb(255, 255, 0));
    MoveToEx(hdc, divider_x, rect.top, null_mut());
    LineTo(hdc, divider_x, rect.bottom);
    SelectObject(hdc, old_pen);

    scale_width + 3
}

fn draw_scale_gpu(frame: &ChartFrame<'_>, rect: &RECT) -> i32 {
    let scale_width = 44;
    let divider_x = rect.left + scale_width;

    frame.draw_grid_line(
        divider_x as f32,
        rect.top as f32,
        divider_x as f32,
        rect.bottom as f32,
        ChartColor::Yellow,
    );

    scale_width + 3
}

unsafe fn measure_graph_text(hdc: HDC, text: &str) -> (i32, i32) {
    let mut text_wide = to_wide_null(text);
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    DrawTextW(
        hdc,
        text_wide.as_mut_ptr(),
        -1,
        &mut rect,
        DT_CALCRECT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX,
    );
    (
        (rect.right - rect.left).max(1),
        (rect.bottom - rect.top).max(1),
    )
}

unsafe fn draw_grid(hdc: HDC, rect: &RECT, scroll_offset: i32, zoom: u32) {
    // 网格密度会随着 zoom 调整，避免在低利用率场景下曲线长期贴底看不清。
    let old_pen = SelectObject(hdc, GetStockObject(DC_PEN) as _);
    SetDCPenColor(hdc, rgb(0, 128, 64));
    let square_height = GRAPH_GRID + ((20 * (100 - (100 / zoom.max(1) as i32))) / 100);

    let mut y = rect.bottom - square_height - 1;
    while y > rect.top {
        MoveToEx(hdc, rect.left, y, null_mut());
        LineTo(hdc, rect.right, y);
        y -= square_height.max(1);
    }

    let mut x = rect.right - scroll_offset;
    while x > rect.left {
        MoveToEx(hdc, x, rect.top, null_mut());
        LineTo(hdc, x, rect.bottom);
        x -= GRAPH_GRID;
    }

    SelectObject(hdc, old_pen);
}

fn draw_grid_gpu(frame: &ChartFrame<'_>, rect: &RECT, scroll_offset: i32, zoom: u32) {
    let square_height = GRAPH_GRID + ((20 * (100 - (100 / zoom.max(1) as i32))) / 100);

    let mut y = rect.bottom - square_height - 1;
    while y > rect.top {
        frame.draw_grid_line(
            rect.left as f32,
            y as f32,
            rect.right as f32,
            y as f32,
            ChartColor::Grid,
        );
        y -= square_height.max(1);
    }

    let mut x = rect.right - scroll_offset;
    while x > rect.left {
        frame.draw_grid_line(
            x as f32,
            rect.top as f32,
            x as f32,
            rect.bottom as f32,
            ChartColor::Grid,
        );
        x -= GRAPH_GRID;
    }
}

unsafe fn draw_history(hdc: HDC, rect: &RECT, history: &HistoryBuffer, color: u32, zoom: u32) {
    // 三条折线都共用这套绘制函数，只靠颜色区分 total / received / sent。
    if history.is_empty() {
        return;
    }

    let width = (rect.right - rect.left).max(1) as usize;
    let graph_height = (rect.bottom - rect.top).max(1);
    let scale = ((width - 1) / history.len()).max(1);

    let old_pen = SelectObject(hdc, GetStockObject(DC_PEN) as _);
    SetDCPenColor(hdc, color);
    MoveToEx(
        hdc,
        rect.right,
        scaled_history_y(rect, graph_height, history.newest_value(), zoom),
        null_mut(),
    );

    for (index, value) in history.iter().enumerate() {
        if index * scale >= width {
            break;
        }
        LineTo(
            hdc,
            rect.right - (scale * index) as i32,
            scaled_history_y(rect, graph_height, value, zoom),
        );
    }

    SelectObject(hdc, old_pen);
}

fn draw_history_gpu(
    frame: &ChartFrame<'_>,
    rect: &RECT,
    history: &HistoryBuffer,
    color: ChartColor,
    zoom: u32,
) {
    if history.is_empty() {
        return;
    }

    let width = (rect.right - rect.left).max(1) as usize;
    let graph_height = (rect.bottom - rect.top).max(1);
    let scale = ((width - 1) / history.len()).max(1);

    let mut previous_x = rect.right as f32;
    let mut previous_y = scaled_history_y(rect, graph_height, history.newest_value(), zoom) as f32;

    for (index, value) in history.iter().enumerate() {
        if index * scale >= width {
            break;
        }

        let x = (rect.right - (scale * index) as i32) as f32;
        let y = scaled_history_y(rect, graph_height, value, zoom) as f32;
        frame.draw_series_line(previous_x, previous_y, x, y, color);
        previous_x = x;
        previous_y = y;
    }
}

fn scaled_history_y(rect: &RECT, graph_height: i32, value: u8, zoom: u32) -> i32 {
    if value == 0 {
        return rect.bottom - 1;
    }

    let scaled_value =
        ((i32::from(value) * graph_height * zoom as i32) / 100).clamp(1, graph_height);
    rect.bottom - scaled_value
}

fn graph_scale_top_value(scale_max: u8) -> u32 {
    match scale_max {
        0..=1 => 1,
        2 => 2,
        3..=5 => 5,
        6..=10 => 10,
        11..=25 => 25,
        26..=50 => 50,
        _ => 100,
    }
}

fn graph_zoom(scale_max: u8) -> u32 {
    100 / graph_scale_top_value(scale_max)
}

fn format_scale_label(value: f32) -> String {
    if (value - value.round()).abs() < f32::EPSILON {
        format!("{value:.0} %")
    } else {
        format!("{value:.1} %")
    }
}

unsafe fn draw_graph_text(hdc: HDC, mut rect: RECT, text: &str, color: u32, align: u32) {
    let mut text_wide = to_wide_null(text);
    SetBkMode(hdc, TRANSPARENT as i32);
    SetTextColor(hdc, color);
    DrawTextW(
        hdc,
        text_wide.as_mut_ptr(),
        -1,
        &mut rect,
        align | DT_TOP | DT_SINGLELINE | DT_NOPREFIX,
    );
}

unsafe fn draw_graph_scale_overlay(hdc: HDC, rect: RECT, scale_max: u8) {
    draw_scale(hdc, &rect, graph_scale_top_value(scale_max));
}

unsafe fn layout_spacing() -> (i32, i32) {
    let units = GetDialogBaseUnits() as usize;
    let def_spacing = (DEFSPACING_BASE * i32::from(loword(units))) / DLG_SCALE_X;
    let top_spacing = (TOPSPACING_BASE * i32::from(hiword(units))) / DLG_SCALE_Y;
    (def_spacing, top_spacing)
}

const fn rgb(red: u8, green: u8, blue: u8) -> u32 {
    red as u32 | ((green as u32) << 8) | ((blue as u32) << 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_adapter(name: &str) -> NetworkAdapterEntry {
        NetworkAdapterEntry {
            interface_index: 7,
            key: AdapterIdentity {
                luid: 42,
                interface_index: 7,
            },
            name: name.to_string(),
            state: "Connected".to_string(),
            link_speed: "1.0 Gbps".to_string(),
            utilization: "2%".to_string(),
            bytes_sent: "10".to_string(),
            bytes_received: "20".to_string(),
            bytes_total: "30".to_string(),
            current_sent: 10,
            current_received: 20,
            sent_history: HistoryBuffer::zeroed(3),
            received_history: HistoryBuffer::zeroed(3),
            total_history: HistoryBuffer::zeroed(3),
            dirty: true,
        }
    }

    #[test]
    fn history_max_updates_when_the_previous_maximum_expires() {
        let mut history = HistoryBuffer::zeroed(3);
        history.push(90);
        history.push(10);
        history.push(20);
        assert_eq!(history.max_value(), 90);

        history.push(30);
        assert_eq!(history.max_value(), 30);
        assert_eq!(history.iter().collect::<Vec<_>>(), vec![30, 20, 10]);
    }

    #[test]
    fn renamed_adapter_updates_the_primary_list_column() {
        let adapter = test_adapter("Renamed Ethernet");
        assert_eq!(adapter_row_texts(&adapter)[0], "Renamed Ethernet");
    }
}
