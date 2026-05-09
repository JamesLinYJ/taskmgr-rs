use std::ffi::c_void;
// 性能页实现。
// 该模块负责采样系统级 CPU/内存指标，并绘制经典任务管理器里的折线图、
// 数值面板和状态快照。
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HINSTANCE, HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, DrawTextW, GetDC,
    GetStockObject, InvalidateRect, Rectangle, RedrawWindow, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, UpdateWindow, BLACK_BRUSH, DT_BOTTOM, DT_CENTER, DT_SINGLELINE, HBITMAP, HBRUSH,
    HDC, HGDIOBJ, RDW_ERASE, RDW_INVALIDATE, RDW_NOCHILDREN, RDW_UPDATENOW, SRCCOPY, TRANSPARENT,
};
use windows_sys::Win32::System::ProcessStatus::{K32GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, GetClientRect, GetDialogBaseUnits,
    GetDlgCtrlID, GetDlgItem, IsIconic, IsWindowVisible, SendMessageW, ShowWindow, HDWP,
    SWP_NOACTIVATE, SWP_NOREDRAW, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_SHOW, WM_SETREDRAW,
};

use crate::assets::load_bitmap_from_file;
use crate::chart_renderer::{ChartColor, ChartRenderer};
use crate::drawing::{fill_black, push_history, rgb};
use crate::options::{CpuHistoryMode, Options};
use crate::perf_drawing::{
    average_history_into, current_font_height, defer_resize, draw_grid_width, draw_grid_width_gpu,
    draw_history_series, draw_history_series_gpu, draw_meter, format_mem_meter_text,
    set_numeric_text, HistoryPlotLayout, HistorySeries, GRAPH_GRID, HIST_SIZE,
};
use crate::perf_layout::{
    compute_perf_layout, next_graph_surface_extent, PerfDialogSpacing, PerfLayoutAnchors,
};
use crate::resource::{
    IDC_AVAIL_PHYSICAL, IDC_COMMIT_LIMIT, IDC_COMMIT_PEAK, IDC_COMMIT_TOTAL, IDC_CPUGRAPH,
    IDC_CPUMETER, IDC_CPUUSAGEFRAME, IDC_FILE_CACHE, IDC_KERNEL_NONPAGED, IDC_KERNEL_PAGED,
    IDC_KERNEL_TOTAL, IDC_LAST_CPUGRAPH, IDC_MEMBARFRAME, IDC_MEMFRAME, IDC_MEMGRAPH, IDC_MEMMETER,
    IDC_STATIC1, IDC_STATIC10, IDC_STATIC11, IDC_STATIC12, IDC_STATIC13, IDC_STATIC14,
    IDC_STATIC15, IDC_STATIC16, IDC_STATIC17, IDC_STATIC2, IDC_STATIC3, IDC_STATIC4, IDC_STATIC5,
    IDC_STATIC6, IDC_STATIC8, IDC_STATIC9, IDC_TOTAL_HANDLES, IDC_TOTAL_PHYSICAL,
    IDC_TOTAL_PROCESSES, IDC_TOTAL_THREADS, STATIC_CPU_GRAPH_COUNT,
};
use crate::winutil::{hiword, loword, to_wide_null, window_rect_relative_to_page};

const STRIP_HEIGHT: i32 = 75;
const STRIP_WIDTH: i32 = 33;
const DEFSPACING_BASE: i32 = 3;
const INNERSPACING_BASE: i32 = 2;
const TOPSPACING_BASE: i32 = 10;
const DLG_SCALE_X: i32 = 4;
const DLG_SCALE_Y: i32 = 8;
const CPU_USAGE_FRAME_ID: i32 = IDC_CPUUSAGEFRAME;
const GRAPH_SURFACE_WIDTH_QUANTUM: i32 = 128;
const GRAPH_SURFACE_HEIGHT_QUANTUM: i32 = 64;

const PERF_TEXT_CONTROLS: [i32; 28] = [
    // 这些文本控件在“无标题模式”下会整体隐藏。
    IDC_STATIC1,
    IDC_STATIC2,
    IDC_STATIC3,
    IDC_STATIC4,
    IDC_STATIC5,
    IDC_STATIC6,
    IDC_STATIC8,
    IDC_STATIC9,
    IDC_STATIC10,
    IDC_STATIC11,
    IDC_STATIC12,
    IDC_STATIC13,
    IDC_STATIC14,
    IDC_STATIC15,
    IDC_STATIC16,
    IDC_STATIC17,
    IDC_TOTAL_PHYSICAL,
    IDC_AVAIL_PHYSICAL,
    IDC_FILE_CACHE,
    IDC_COMMIT_TOTAL,
    IDC_COMMIT_LIMIT,
    IDC_COMMIT_PEAK,
    IDC_KERNEL_TOTAL,
    IDC_KERNEL_PAGED,
    IDC_KERNEL_NONPAGED,
    IDC_TOTAL_HANDLES,
    IDC_TOTAL_THREADS,
    IDC_TOTAL_PROCESSES,
];

const PERF_LAYOUT_CONTROLS: [i32; 28] = PERF_TEXT_CONTROLS;
const PERF_FRAME_CONTROLS: [i32; 8] = [
    crate::resource::IDC_CPUFRAME,
    CPU_USAGE_FRAME_ID,
    IDC_MEMBARFRAME,
    IDC_MEMFRAME,
    IDC_STATIC1,
    IDC_STATIC5,
    IDC_STATIC10,
    IDC_STATIC13,
];

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SystemProcessorPerformanceInformation {
    // `NtQuerySystemInformation(SystemProcessorPerformanceInformation)` 的返回结构。
    idle_time: i64,
    kernel_time: i64,
    user_time: i64,
    dpc_time: i64,
    interrupt_time: i64,
    interrupt_count: u32,
}

#[repr(i32)]
enum SystemInformationClass {
    ProcessorPerformanceInformation = 8,
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

#[derive(Clone, Copy, Default)]
pub struct PerformanceSnapshot {
    // 主框架只需要这一小部分汇总信息来更新状态栏和托盘。
    pub cpu_usage: u8,
    pub mem_usage_kb: u32,
    pub mem_limit_kb: u32,
    pub process_count: u32,
}

#[derive(Default)]
pub struct PerformancePageState {
    // 页面级缓存包含采样结果、图表历史和绘制时会复用的 GDI 资源句柄。
    hinstance: HINSTANCE,
    processor_count: usize,
    cpu_usage: u8,
    kernel_usage: u8,
    physical_mem_usage_kb: u32,
    physical_mem_limit_kb: u32,
    commit_total_kb: u32,
    commit_limit_kb: u32,
    commit_peak_kb: u32,
    total_physical_kb: u32,
    avail_physical_kb: u32,
    file_cache_kb: u32,
    kernel_total_kb: u32,
    kernel_paged_kb: u32,
    kernel_nonpaged_kb: u32,
    handle_count: u32,
    thread_count: u32,
    process_count: u32,
    cpu_history_mode: i32,
    show_kernel_times: bool,
    no_title: bool,
    scroll_offset: i32,
    previous_idle_times: Vec<i64>,
    previous_total_times: Vec<i64>,
    previous_kernel_times: Vec<i64>,
    cpu_history: Vec<Vec<u8>>,
    kernel_history: Vec<Vec<u8>>,
    mem_history: Vec<u8>,
    processor_info: Vec<SystemProcessorPerformanceInformation>,
    cached_averaged_cpu: Vec<u8>,
    cached_averaged_kernel: Vec<u8>,
    strip_lit_bitmap: HBITMAP,
    strip_lit_red_bitmap: HBITMAP,
    strip_unlit_bitmap: HBITMAP,
    chart_renderer: ChartRenderer,
    graph_dc: HDC,
    graph_bitmap: HBITMAP,
    graph_bitmap_old: HGDIOBJ,
    graph_bitmap_width: i32,
    graph_bitmap_height: i32,
}

impl PerformancePageState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn initialize(&mut self, hinstance: HINSTANCE, processor_count: usize) {
        // 性能页启动时先准备采样缓冲和仪表位图；
        // 真正依赖窗口尺寸的离屏表面会在布局完成后再创建。
        self.hinstance = hinstance;
        self.ensure_history_capacity(processor_count.max(1));
        self.load_meter_bitmaps();
        self.chart_renderer = ChartRenderer::new();
    }

    pub fn apply_options(&mut self, hwnd_page: HWND, options: &Options, processor_count: usize) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 配置变化会同时影响图表数量、是否叠加内核时间，以及文字区是否折叠。
            self.ensure_history_capacity(processor_count.max(1));
            self.cpu_history_mode = options.cpu_history_mode;
            self.show_kernel_times = options.kernel_times();
            self.no_title = options.no_title();

            let pane_count = self.visible_cpu_graph_count();

            for index in 0..self.cpu_graph_slot_count() {
                let control = self.cpu_graph_hwnd(hwnd_page, index);
                if !control.is_null() {
                    ShowWindow(control, if index < pane_count { SW_SHOW } else { SW_HIDE });
                }
            }

            let detail_state = if self.no_title { SW_HIDE } else { SW_SHOW };
            for control_id in PERF_TEXT_CONTROLS {
                let control = GetDlgItem(hwnd_page, control_id);
                if !control.is_null() {
                    ShowWindow(control, detail_state);
                }
            }

            for control_id in [IDC_MEMGRAPH, IDC_MEMFRAME, IDC_MEMBARFRAME, IDC_MEMMETER] {
                let control = GetDlgItem(hwnd_page, control_id);
                if !control.is_null() {
                    ShowWindow(control, detail_state);
                }
            }

            if !self.no_title {
                self.update_detail_texts(hwnd_page);
            }

            InvalidateRect(hwnd_page, null(), 0);
        }
    }

    pub fn timer_event(&mut self, hwnd_page: HWND, main_hwnd: HWND) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 定时器事件先刷新底层采样，再推动图表滚动与数值文本更新。
            self.refresh_measurements(hwnd_page);
            self.scroll_offset = (self.scroll_offset + 2) % GRAPH_GRID;

            if IsIconic(main_hwnd) == 0 {
                self.invalidate_graph_controls(hwnd_page);
            }
        }
    }

    pub fn snapshot(&self) -> PerformanceSnapshot {
        PerformanceSnapshot {
            cpu_usage: self.cpu_usage,
            mem_usage_kb: self.physical_mem_usage_kb,
            mem_limit_kb: self.physical_mem_limit_kb,
            process_count: self.process_count,
        }
    }

    pub fn no_title(&self) -> bool {
        self.no_title
    }

    pub fn is_graph_control(&self, control_id: i32) -> bool {
        // owner-draw 消息先通过这里判断是不是性能页图表类控件。
        matches!(control_id, IDC_MEMGRAPH | IDC_MEMMETER | IDC_CPUMETER)
            || self.cpu_graph_pane_index(control_id).is_some()
    }

    pub fn cpu_graph_pane_index(&self, control_id: i32) -> Option<usize> {
        // 连续控件 ID 可以直接映射成 CPU pane 下标。
        if (IDC_CPUGRAPH..=IDC_LAST_CPUGRAPH).contains(&control_id) {
            let pane_index = (control_id - IDC_CPUGRAPH) as usize;
            if pane_index < self.cpu_graph_slot_count() {
                Some(pane_index)
            } else {
                None
            }
        } else {
            None
        }
    }

    fn invalidate_graph_controls(&self, hwnd_page: HWND) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 图表刷新只失效图表控件本身，不整页重绘。
            for control_id in [IDC_CPUMETER, IDC_MEMMETER, IDC_MEMGRAPH] {
                let control = GetDlgItem(hwnd_page, control_id);
                if !control.is_null() {
                    InvalidateRect(control, null(), 0);
                }
            }

            let pane_count = self.visible_cpu_graph_count();
            for pane_index in 0..pane_count {
                let control = self.cpu_graph_hwnd(hwnd_page, pane_index);
                if !control.is_null() {
                    InvalidateRect(control, null(), 0);
                }
            }
        }
    }

    pub fn draw_cpu_graph(&self, hdc: HDC, rect: RECT, pane_index: usize) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // CPU 图优先绘制到离屏 DC，再一次性拷回目标 DC，
            // 这样网格线和曲线更新时不会在前台逐步闪出来。
            if pane_index >= self.cpu_history.len() {
                return;
            }

            let width = (rect.right - rect.left).max(1);
            let height = (rect.bottom - rect.top).max(1);
            let use_backbuffer = !self.graph_dc.is_null()
                && self.graph_bitmap_width >= width
                && self.graph_bitmap_height >= height;
            let target_hdc = if use_backbuffer { self.graph_dc } else { hdc };
            let target_rect = if use_backbuffer {
                RECT {
                    left: 0,
                    top: 0,
                    right: self.graph_bitmap_width,
                    bottom: height,
                }
            } else {
                rect
            };

            fill_black(target_hdc, &target_rect);
            draw_grid_width(target_hdc, &target_rect, width, self.scroll_offset);

            let graph_height = (target_rect.bottom - target_rect.top - 1).max(1);
            let scale = ((width - 1) / HIST_SIZE as i32).max(0);
            let scale = if scale == 0 { 2 } else { scale } as usize;

            let plot_layout = HistoryPlotLayout {
                graph_height,
                width,
                scale,
            };

            if self.show_kernel_times {
                if self.cpu_history_mode == CpuHistoryMode::Panes as i32 {
                    draw_history_series(
                        target_hdc,
                        &target_rect,
                        plot_layout,
                        HistorySeries {
                            history: &self.kernel_history[pane_index],
                            color: ChartColor::Red,
                            stop_on_zero: false,
                        },
                    );
                } else {
                    draw_history_series(
                        target_hdc,
                        &target_rect,
                        plot_layout,
                        HistorySeries {
                            history: &self.cached_averaged_kernel,
                            color: ChartColor::Red,
                            stop_on_zero: false,
                        },
                    );
                }
            }

            if self.cpu_history_mode == CpuHistoryMode::Panes as i32 {
                draw_history_series(
                    target_hdc,
                    &target_rect,
                    plot_layout,
                    HistorySeries {
                        history: &self.cpu_history[pane_index],
                        color: ChartColor::Green,
                        stop_on_zero: false,
                    },
                );
            } else {
                draw_history_series(
                    target_hdc,
                    &target_rect,
                    plot_layout,
                    HistorySeries {
                        history: &self.cached_averaged_cpu,
                        color: ChartColor::Green,
                        stop_on_zero: false,
                    },
                );
            }

            if use_backbuffer {
                let x_diff = (self.graph_bitmap_width - width).max(0);
                BitBlt(
                    hdc,
                    rect.left,
                    rect.top,
                    width,
                    height,
                    self.graph_dc,
                    x_diff,
                    0,
                    SRCCOPY,
                );
            }
        }
    }

    pub fn draw_mem_graph(&self, hdc: HDC, rect: RECT) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 内存历史图复用 CPU 图的绘制策略，只是数据源和颜色不同。
            if self.draw_mem_graph_gpu(hdc, rect) {
                return;
            }

            let width = (rect.right - rect.left).max(1);
            let height = (rect.bottom - rect.top).max(1);
            let use_backbuffer = !self.graph_dc.is_null()
                && self.graph_bitmap_width >= width
                && self.graph_bitmap_height >= height;
            let target_hdc = if use_backbuffer { self.graph_dc } else { hdc };
            let target_rect = if use_backbuffer {
                RECT {
                    left: 0,
                    top: 0,
                    right: width,
                    bottom: height,
                }
            } else {
                rect
            };

            fill_black(target_hdc, &target_rect);
            draw_grid_width(target_hdc, &target_rect, width, self.scroll_offset);
            let scale = ((width - 1) / HIST_SIZE as i32).max(0);
            let scale = if scale == 0 { 2 } else { scale } as usize;
            draw_history_series(
                target_hdc,
                &target_rect,
                HistoryPlotLayout {
                    graph_height: (target_rect.bottom - target_rect.top - 1).max(1),
                    width,
                    scale,
                },
                HistorySeries {
                    history: &self.mem_history,
                    color: ChartColor::Yellow,
                    stop_on_zero: true,
                },
            );

            if use_backbuffer {
                BitBlt(
                    hdc,
                    rect.left,
                    rect.top,
                    width,
                    height,
                    self.graph_dc,
                    0,
                    0,
                    SRCCOPY,
                );
            }
        }
    }

    #[allow(dead_code)]
    fn draw_cpu_graph_gpu(&self, hdc: HDC, rect: RECT, pane_index: usize) -> bool {
        let Some(frame) = self.chart_renderer.begin_frame(hdc, rect) else {
            return false;
        };

        let target_rect = frame.bounds();
        let width = (target_rect.right - target_rect.left).max(1);
        frame.clear_black();
        draw_grid_width_gpu(&frame, &target_rect, width, self.scroll_offset);

        let graph_height = (target_rect.bottom - target_rect.top - 1).max(1);
        let scale = ((width - 1) / HIST_SIZE as i32).max(0);
        let scale = if scale == 0 { 2 } else { scale } as usize;
        let plot_layout = HistoryPlotLayout {
            graph_height,
            width,
            scale,
        };

        if self.show_kernel_times {
            if self.cpu_history_mode == CpuHistoryMode::Panes as i32 {
                draw_history_series_gpu(
                    &frame,
                    &target_rect,
                    plot_layout,
                    HistorySeries {
                        history: &self.kernel_history[pane_index],
                        color: ChartColor::Red,
                        stop_on_zero: false,
                    },
                );
            } else {
                draw_history_series_gpu(
                    &frame,
                    &target_rect,
                    plot_layout,
                    HistorySeries {
                        history: &self.cached_averaged_kernel,
                        color: ChartColor::Red,
                        stop_on_zero: false,
                    },
                );
            }
        }

        if self.cpu_history_mode == CpuHistoryMode::Panes as i32 {
            draw_history_series_gpu(
                &frame,
                &target_rect,
                plot_layout,
                HistorySeries {
                    history: &self.cpu_history[pane_index],
                    color: ChartColor::Green,
                    stop_on_zero: false,
                },
            );
        } else {
            draw_history_series_gpu(
                &frame,
                &target_rect,
                plot_layout,
                HistorySeries {
                    history: &self.cached_averaged_cpu,
                    color: ChartColor::Green,
                    stop_on_zero: false,
                },
            );
        }

        frame.end()
    }

    fn draw_mem_graph_gpu(&self, hdc: HDC, rect: RECT) -> bool {
        let Some(frame) = self.chart_renderer.begin_frame(hdc, rect) else {
            return false;
        };

        let target_rect = frame.bounds();
        let width = (target_rect.right - target_rect.left).max(1);
        frame.clear_black();
        draw_grid_width_gpu(&frame, &target_rect, width, self.scroll_offset);
        let scale = ((width - 1) / HIST_SIZE as i32).max(0);
        let scale = if scale == 0 { 2 } else { scale } as usize;
        draw_history_series_gpu(
            &frame,
            &target_rect,
            HistoryPlotLayout {
                graph_height: (target_rect.bottom - target_rect.top - 1).max(1),
                width,
                scale,
            },
            HistorySeries {
                history: &self.mem_history,
                color: ChartColor::Yellow,
                stop_on_zero: true,
            },
        );

        frame.end()
    }

    pub fn draw_cpu_meter(&self, hdc: HDC, rect: RECT) {
        // CPU 仪表优先复用 LED 条形位图，不可用时再退回纯 GDI 绘制。
        if self.draw_strip_meter(
            hdc,
            rect,
            &format!("{} %", self.cpu_usage),
            self.cpu_usage,
            if self.show_kernel_times {
                self.kernel_usage.min(self.cpu_usage)
            } else {
                0
            },
        ) {
            return;
        }

        draw_meter(
            hdc,
            rect,
            &format!("{} %", self.cpu_usage),
            self.cpu_usage,
            if self.show_kernel_times {
                self.kernel_usage.min(self.cpu_usage)
            } else {
                0
            },
            rgb(0, 255, 0),
            rgb(255, 0, 0),
        );
    }

    pub fn draw_mem_meter(&self, hdc: HDC, rect: RECT) {
        // 内存仪表显示的是已用内存字节量文本，而不是百分比文本。
        let mem_percent = if self.physical_mem_limit_kb == 0 {
            0
        } else {
            ((self.physical_mem_usage_kb.saturating_mul(100)) / self.physical_mem_limit_kb).min(100)
                as u8
        };
        let mem_usage_text = format_mem_meter_text(self.physical_mem_usage_kb);
        if self.draw_strip_meter(hdc, rect, &mem_usage_text, mem_percent, 0) {
            return;
        }

        draw_meter(
            hdc,
            rect,
            &mem_usage_text,
            mem_percent,
            0,
            rgb(255, 255, 0),
            rgb(255, 255, 0),
        );
    }

    pub fn size_page(&mut self, hwnd_page: HWND, main_hwnd: HWND) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 布局逻辑尽量贴近经典 Task Manager：
            // 先算整体可用高度，再分配图表、仪表和底部统计区的位置。
            if hwnd_page.is_null() {
                return;
            }

            let mut parent_rect = zeroed::<RECT>();
            if self.no_title {
                // C++ uses GetClientRect(g_hMainWnd) directly — no mapping
                GetClientRect(main_hwnd, &mut parent_rect);
            } else {
                GetClientRect(hwnd_page, &mut parent_rect);
            }

            let pane_count = self.visible_cpu_graph_count();

            let units = GetDialogBaseUnits() as usize;
            let spacing = PerfDialogSpacing {
                def_spacing: (DEFSPACING_BASE * i32::from(loword(units))) / DLG_SCALE_X,
                inner_spacing: (INNERSPACING_BASE * i32::from(loword(units))) / DLG_SCALE_X,
                top_spacing: (TOPSPACING_BASE * i32::from(hiword(units))) / DLG_SCALE_Y,
            };

            let anchors = PerfLayoutAnchors {
                master_rect: window_rect_relative_to_page(
                    GetDlgItem(hwnd_page, IDC_STATIC5),
                    hwnd_page,
                ),
                top_frame: window_rect_relative_to_page(
                    GetDlgItem(hwnd_page, IDC_STATIC13),
                    hwnd_page,
                ),
                cpu_history_frame: window_rect_relative_to_page(
                    GetDlgItem(hwnd_page, crate::resource::IDC_CPUFRAME),
                    hwnd_page,
                ),
                cpu_usage_frame: window_rect_relative_to_page(
                    GetDlgItem(hwnd_page, CPU_USAGE_FRAME_ID),
                    hwnd_page,
                ),
                mem_bar_frame: window_rect_relative_to_page(
                    GetDlgItem(hwnd_page, IDC_MEMBARFRAME),
                    hwnd_page,
                ),
                mem_frame: window_rect_relative_to_page(
                    GetDlgItem(hwnd_page, IDC_MEMFRAME),
                    hwnd_page,
                ),
            };
            let layout =
                compute_perf_layout(parent_rect, anchors, spacing, pane_count, self.no_title);

            let defer_hint = (PERF_LAYOUT_CONTROLS.len() + self.cpu_graph_slot_count() + 6) as i32;
            let mut hdwp: HDWP = BeginDeferWindowPos(defer_hint);
            if hdwp.is_null() {
                return;
            }
            let redraw_windows = self.redraw_windows(hwnd_page);
            set_redraw_for_windows(&redraw_windows, false);

            let resize_flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW;
            let move_flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW;

            if !self.no_title {
                for control_id in PERF_LAYOUT_CONTROLS {
                    let hwnd_ctrl = GetDlgItem(hwnd_page, control_id);
                    if hwnd_ctrl.is_null() {
                        continue;
                    }
                    let rect = window_rect_relative_to_page(hwnd_ctrl, hwnd_page);
                    hdwp = DeferWindowPos(
                        hdwp,
                        hwnd_ctrl,
                        null_mut(),
                        rect.left,
                        rect.top + layout.detail_shift_y,
                        0,
                        0,
                        move_flags,
                    );
                }
            }

            hdwp = defer_resize(
                hdwp,
                GetDlgItem(hwnd_page, crate::resource::IDC_CPUFRAME),
                layout.cpu_history_width,
                layout.cpu_history_height,
            );

            hdwp = defer_resize(
                hdwp,
                GetDlgItem(hwnd_page, CPU_USAGE_FRAME_ID),
                layout.cpu_usage_frame_width,
                layout.cpu_history_height,
            );

            hdwp = DeferWindowPos(
                hdwp,
                GetDlgItem(hwnd_page, IDC_CPUMETER),
                null_mut(),
                layout.meter_rect.left,
                layout.meter_rect.top,
                layout.meter_rect.right - layout.meter_rect.left,
                layout.meter_rect.bottom - layout.meter_rect.top,
                resize_flags,
            );

            if !self.no_title {
                hdwp = DeferWindowPos(
                    hdwp,
                    GetDlgItem(hwnd_page, IDC_MEMBARFRAME),
                    null_mut(),
                    layout.mem_bar_frame_rect.left,
                    layout.mem_bar_frame_rect.top,
                    layout.mem_bar_frame_rect.right - layout.mem_bar_frame_rect.left,
                    layout.mem_bar_frame_rect.bottom - layout.mem_bar_frame_rect.top,
                    resize_flags,
                );

                hdwp = DeferWindowPos(
                    hdwp,
                    GetDlgItem(hwnd_page, IDC_MEMMETER),
                    null_mut(),
                    layout.meter_rect.left,
                    layout.mem_bar_frame_rect.top + spacing.top_spacing,
                    layout.meter_rect.right - layout.meter_rect.left,
                    layout.meter_rect.bottom - layout.meter_rect.top,
                    resize_flags,
                );

                hdwp = DeferWindowPos(
                    hdwp,
                    GetDlgItem(hwnd_page, IDC_MEMFRAME),
                    null_mut(),
                    layout.mem_frame_rect.left,
                    layout.mem_frame_rect.top,
                    layout.mem_frame_rect.right - layout.mem_frame_rect.left,
                    layout.mem_frame_rect.bottom - layout.mem_frame_rect.top,
                    resize_flags,
                );

                hdwp = DeferWindowPos(
                    hdwp,
                    GetDlgItem(hwnd_page, IDC_MEMGRAPH),
                    null_mut(),
                    layout.mem_graph_rect.left,
                    layout.mem_graph_rect.top,
                    layout.mem_graph_rect.right - layout.mem_graph_rect.left,
                    layout.mem_graph_rect.bottom - layout.mem_graph_rect.top,
                    resize_flags,
                );
            }

            for (pane_index, pane_rect) in layout.cpu_pane_rects.iter().enumerate() {
                let cpu_graph = self.cpu_graph_hwnd(hwnd_page, pane_index);
                if cpu_graph.is_null() {
                    continue;
                }
                hdwp = DeferWindowPos(
                    hdwp,
                    cpu_graph,
                    null_mut(),
                    pane_rect.left,
                    pane_rect.top,
                    pane_rect.right - pane_rect.left,
                    pane_rect.bottom - pane_rect.top,
                    resize_flags,
                );
            }

            EndDeferWindowPos(hdwp);
            self.ensure_graph_surface(
                hwnd_page,
                layout.graph_surface_width,
                layout.graph_surface_height,
            );
            set_redraw_for_windows(&redraw_windows, true);
            self.redraw_after_layout(hwnd_page);
        }
    }

    pub fn destroy(&mut self) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 性能页销毁时顺带释放仪表位图和共享离屏表面。
            self.destroy_graph_surface();
            if !self.strip_lit_bitmap.is_null() {
                DeleteObject(self.strip_lit_bitmap as _);
                self.strip_lit_bitmap = null_mut();
            }
            if !self.strip_lit_red_bitmap.is_null() {
                DeleteObject(self.strip_lit_red_bitmap as _);
                self.strip_lit_red_bitmap = null_mut();
            }
            if !self.strip_unlit_bitmap.is_null() {
                DeleteObject(self.strip_unlit_bitmap as _);
                self.strip_unlit_bitmap = null_mut();
            }
        }
    }

    fn ensure_history_capacity(&mut self, processor_count: usize) {
        // 核心数变化时，所有按 CPU 维度分片的历史数组都需要一起重建，
        // 否则“每核图”和“汇总图”会看到不一致的采样长度。
        if self.processor_count == processor_count
            && self.cpu_history.len() == processor_count
            && self.mem_history.len() == HIST_SIZE
        {
            return;
        }

        self.processor_count = processor_count;
        self.previous_idle_times.resize(processor_count, 0);
        self.previous_total_times.resize(processor_count, 0);
        self.previous_kernel_times.resize(processor_count, 0);
        self.cpu_history = vec![vec![0; HIST_SIZE]; processor_count];
        self.kernel_history = vec![vec![0; HIST_SIZE]; processor_count];
        self.mem_history = vec![0; HIST_SIZE];
    }

    fn refresh_measurements(&mut self, hwnd_page: HWND) {
        // 这里集中采集所有性能相关数据，确保一次刷新内各图表看到的是同一时刻的快照。
        if self.processor_count == 0 {
            self.ensure_history_capacity(1);
        }

        self.refresh_cpu_histories();
        self.refresh_system_info(hwnd_page);
    }

    fn refresh_cpu_histories(&mut self) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 内核返回的是累积 CPU 时间，所以这里必须与上一轮做差，
            // 再换算成本轮使用率和内核时间占比。
            self.processor_info.resize(
                self.processor_count,
                SystemProcessorPerformanceInformation::default(),
            );
            let status = NtQuerySystemInformation(
                SystemInformationClass::ProcessorPerformanceInformation as i32,
                self.processor_info.as_mut_ptr() as *mut c_void,
                (self.processor_info.len() * size_of::<SystemProcessorPerformanceInformation>())
                    as u32,
                null_mut(),
            );
            if status < 0 {
                return;
            }

            let mut sum_idle = 0i64;
            let mut sum_total = 0i64;
            let mut sum_kernel = 0i64;

            for (index, entry) in self.processor_info.iter().enumerate() {
                let idle_time = entry.idle_time;
                let kernel_time = entry.kernel_time.saturating_sub(entry.idle_time);
                let total_time = entry.kernel_time.saturating_add(entry.user_time);

                let delta_idle = idle_time.saturating_sub(self.previous_idle_times[index]);
                let delta_kernel = kernel_time.saturating_sub(self.previous_kernel_times[index]);
                let delta_total = total_time.saturating_sub(self.previous_total_times[index]);

                sum_idle = sum_idle.saturating_add(delta_idle);
                sum_kernel = sum_kernel.saturating_add(delta_kernel);
                sum_total = sum_total.saturating_add(delta_total);

                let cpu_percent = if delta_total > 0 {
                    (100 - ((delta_idle * 100) / delta_total)).clamp(0, 100) as u8
                } else {
                    0
                };
                let kernel_percent = if delta_total > 0 {
                    ((delta_kernel * 100) / delta_total).clamp(0, 100) as u8
                } else {
                    0
                };

                push_history(&mut self.cpu_history[index], cpu_percent);
                push_history(&mut self.kernel_history[index], kernel_percent);

                self.previous_idle_times[index] = idle_time;
                self.previous_total_times[index] = total_time;
                self.previous_kernel_times[index] = kernel_time;
            }

            self.cpu_usage = if sum_total > 0 {
                (100 - ((sum_idle * 100) / sum_total)).clamp(0, 100) as u8
            } else {
                0
            };
            self.kernel_usage = if sum_total > 0 {
                ((sum_kernel * 100) / sum_total).clamp(0, 100) as u8
            } else {
                0
            };
        }
        // Pre-compute averaged histories once per tick rather than on every paint.
        average_history_into(&self.cpu_history, &mut self.cached_averaged_cpu);
        average_history_into(&self.kernel_history, &mut self.cached_averaged_kernel);
    }

    fn refresh_system_info(&mut self, hwnd_page: HWND) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 系统级内存、Commit、句柄、线程、进程总数都来源于同一份快照，
            // 统一在这里采样可以保证页面上的数字属于同一个刷新时刻。
            let mut perf = zeroed::<PERFORMANCE_INFORMATION>();
            perf.cb = size_of::<PERFORMANCE_INFORMATION>() as u32;
            if K32GetPerformanceInfo(&mut perf, perf.cb) == 0 {
                return;
            }

            let page_kb = (perf.PageSize / 1024).max(1);
            let pages_to_kb = |page_count: usize| -> u32 {
                page_count.saturating_mul(page_kb).min(u32::MAX as usize) as u32
            };

            self.total_physical_kb = pages_to_kb(perf.PhysicalTotal);
            self.avail_physical_kb = pages_to_kb(perf.PhysicalAvailable);
            self.file_cache_kb = pages_to_kb(perf.SystemCache);
            self.physical_mem_limit_kb = self.total_physical_kb;
            self.physical_mem_usage_kb = self
                .total_physical_kb
                .saturating_sub(self.avail_physical_kb);
            self.commit_total_kb = pages_to_kb(perf.CommitTotal);
            self.commit_limit_kb = pages_to_kb(perf.CommitLimit);
            self.commit_peak_kb = pages_to_kb(perf.CommitPeak);
            self.kernel_total_kb = pages_to_kb(perf.KernelTotal);
            self.kernel_paged_kb = pages_to_kb(perf.KernelPaged);
            self.kernel_nonpaged_kb = pages_to_kb(perf.KernelNonpaged);
            self.handle_count = perf.HandleCount;
            self.process_count = perf.ProcessCount;
            self.thread_count = perf.ThreadCount;

            let mem_percent = if self.physical_mem_limit_kb == 0 {
                0
            } else {
                ((self.physical_mem_usage_kb.saturating_mul(100)) / self.physical_mem_limit_kb)
                    .min(100) as u8
            };
            push_history(&mut self.mem_history, mem_percent);

            if !self.no_title {
                self.update_detail_texts(hwnd_page);
            }
        }
    }

    fn update_detail_texts(&self, hwnd_page: HWND) {
        // 底部统计文本在无标题模式下会被图表覆盖；只在详情区可见时写控件文本，
        // 避免隐藏 static 控件被后续重绘消息带回图表表面。
        set_numeric_text(hwnd_page, IDC_TOTAL_PHYSICAL, self.total_physical_kb);
        set_numeric_text(hwnd_page, IDC_AVAIL_PHYSICAL, self.avail_physical_kb);
        set_numeric_text(hwnd_page, IDC_FILE_CACHE, self.file_cache_kb);
        set_numeric_text(hwnd_page, IDC_COMMIT_TOTAL, self.commit_total_kb);
        set_numeric_text(hwnd_page, IDC_COMMIT_LIMIT, self.commit_limit_kb);
        set_numeric_text(hwnd_page, IDC_COMMIT_PEAK, self.commit_peak_kb);
        set_numeric_text(hwnd_page, IDC_KERNEL_TOTAL, self.kernel_total_kb);
        set_numeric_text(hwnd_page, IDC_KERNEL_PAGED, self.kernel_paged_kb);
        set_numeric_text(hwnd_page, IDC_KERNEL_NONPAGED, self.kernel_nonpaged_kb);
        set_numeric_text(hwnd_page, IDC_TOTAL_HANDLES, self.handle_count);
        set_numeric_text(hwnd_page, IDC_TOTAL_THREADS, self.thread_count);
        set_numeric_text(hwnd_page, IDC_TOTAL_PROCESSES, self.process_count);
    }

    fn load_meter_bitmaps(&mut self) {
        // 条形仪表优先复用资源位图；如果资源已经加载过，就不再重复创建 GDI 对象。
        if !self.strip_lit_bitmap.is_null() {
            return;
        }

        self.strip_lit_bitmap = load_bitmap_from_file("ledlit.bmp");
        self.strip_lit_red_bitmap = load_bitmap_from_file("bitmap1.bmp");
        self.strip_unlit_bitmap = load_bitmap_from_file("ledunlit.bmp");
    }

    fn draw_strip_meter(
        &self,
        hdc: HDC,
        rect: RECT,
        label: &str,
        lit_percent: u8,
        red_percent: u8,
    ) -> bool {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 位图仪表把点亮区、未点亮区和红色内核区拼接成最终视觉效果。
            if self.strip_lit_bitmap.is_null()
                || self.strip_unlit_bitmap.is_null()
                || (red_percent != 0 && self.strip_lit_red_bitmap.is_null())
            {
                return false;
            }

            let black = GetStockObject(BLACK_BRUSH) as HBRUSH;
            let old = SelectObject(hdc, black as HGDIOBJ);
            Rectangle(hdc, rect.left, rect.top, rect.right, rect.bottom);

            let units = GetDialogBaseUnits() as usize;
            let def_spacing = (DEFSPACING_BASE * i32::from(loword(units))) / DLG_SCALE_X;
            let x_bar_offset = ((rect.right - rect.left) - STRIP_WIDTH) / 2;
            let bar_height = rect.bottom - rect.top - (current_font_height(hdc) + def_spacing * 3);
            if bar_height <= 0 {
                SelectObject(hdc, old);
                return true;
            }

            SetBkMode(hdc, TRANSPARENT as i32);
            SetTextColor(hdc, rgb(0, 255, 0));
            let mut label_rect = rect;
            label_rect.bottom -= 4;
            let mut label_wide = to_wide_null(label);
            DrawTextW(
                hdc,
                label_wide.as_mut_ptr(),
                -1,
                &mut label_rect,
                DT_SINGLELINE | DT_CENTER | DT_BOTTOM,
            );

            let hdc_mem = CreateCompatibleDC(hdc);
            if hdc_mem.is_null() {
                SelectObject(hdc, old);
                return true;
            }

            let target_lit = ((i32::from(lit_percent) * bar_height) / 100).max(0);
            let target_red = ((i32::from(red_percent) * bar_height) / 100).clamp(0, target_lit);
            let unlit_pixels = ((bar_height - target_lit) / 3) * 3;
            let lit_pixels = bar_height - unlit_pixels;
            let lit_only_pixels = (lit_pixels - target_red).max(0);

            self.blit_meter_strip(
                hdc,
                hdc_mem,
                self.strip_unlit_bitmap,
                x_bar_offset,
                def_spacing,
                bar_height - lit_pixels,
            );
            if lit_only_pixels > 0 {
                self.blit_meter_strip(
                    hdc,
                    hdc_mem,
                    self.strip_lit_bitmap,
                    x_bar_offset,
                    def_spacing + (bar_height - lit_pixels),
                    lit_only_pixels,
                );
            }
            if target_red > 0 {
                self.blit_meter_strip(
                    hdc,
                    hdc_mem,
                    self.strip_lit_red_bitmap,
                    x_bar_offset,
                    def_spacing + (bar_height - target_red),
                    target_red,
                );
            }

            DeleteDC(hdc_mem);
            SelectObject(hdc, old);
            true
        }
    }

    fn blit_meter_strip(
        &self,
        hdc: HDC,
        hdc_mem: HDC,
        bitmap: HBITMAP,
        x: i32,
        start_y: i32,
        height: i32,
    ) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 条形位图按固定高度平铺，直到覆盖目标像素高度。
            if bitmap.is_null() || height <= 0 {
                return;
            }

            let old_bitmap = SelectObject(hdc_mem, bitmap as HGDIOBJ);
            let mut remaining = height;
            let mut offset = 0;
            while remaining > 0 {
                let chunk = remaining.min(STRIP_HEIGHT);
                BitBlt(
                    hdc,
                    x,
                    start_y + offset,
                    STRIP_WIDTH,
                    chunk,
                    hdc_mem,
                    0,
                    0,
                    SRCCOPY,
                );
                remaining -= chunk;
                offset += chunk;
            }
            SelectObject(hdc_mem, old_bitmap);
        }
    }

    fn ensure_graph_surface(&mut self, hwnd_page: HWND, width: i32, height: i32) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 图表表面的目标尺寸由布局层决定；
            // 这里仅负责按容量策略确保一份足够大的共享离屏表面存在。
            //
            // 图表缓冲按容量管理而不是按精确像素管理：
            // 只有需求超过当前容量时才扩容，并按固定量子向上取整。
            // 这样在慢速拖动窗口边缘时不会因为每增长 1 像素就重建一次位图。
            if width <= 0 || height <= 0 {
                self.destroy_graph_surface();
                return;
            }

            if self.graph_bitmap_width >= width
                && self.graph_bitmap_height >= height
                && !self.graph_dc.is_null()
                && !self.graph_bitmap.is_null()
            {
                return;
            }

            let target_width = next_graph_surface_extent(
                self.graph_bitmap_width,
                width,
                GRAPH_SURFACE_WIDTH_QUANTUM,
            );
            let target_height = next_graph_surface_extent(
                self.graph_bitmap_height,
                height,
                GRAPH_SURFACE_HEIGHT_QUANTUM,
            );

            let page_dc = GetDC(hwnd_page);
            if page_dc.is_null() {
                return;
            }

            let graph_dc = CreateCompatibleDC(page_dc);
            if graph_dc.is_null() {
                ReleaseDC(hwnd_page, page_dc);
                return;
            }

            let graph_bitmap = CreateCompatibleBitmap(page_dc, target_width, target_height);
            ReleaseDC(hwnd_page, page_dc);
            if graph_bitmap.is_null() {
                DeleteDC(graph_dc);
                return;
            }

            let old_bitmap = SelectObject(graph_dc, graph_bitmap as HGDIOBJ);
            let previous_dc = self.graph_dc;
            let previous_bitmap = self.graph_bitmap;
            let previous_old = self.graph_bitmap_old;

            self.graph_dc = graph_dc;
            self.graph_bitmap = graph_bitmap;
            self.graph_bitmap_old = old_bitmap;
            self.graph_bitmap_width = target_width;
            self.graph_bitmap_height = target_height;

            if !previous_dc.is_null() {
                if !previous_old.is_null() {
                    SelectObject(previous_dc, previous_old);
                }
                DeleteDC(previous_dc);
            }
            if !previous_bitmap.is_null() {
                DeleteObject(previous_bitmap as _);
            }
        }
    }

    fn destroy_graph_surface(&mut self) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 共享离屏表面销毁时需要先把旧位图选回 DC 再删对象。
            if !self.graph_dc.is_null() {
                if !self.graph_bitmap_old.is_null() {
                    SelectObject(self.graph_dc, self.graph_bitmap_old);
                    self.graph_bitmap_old = null_mut();
                }
                DeleteDC(self.graph_dc);
                self.graph_dc = null_mut();
            }
            if !self.graph_bitmap.is_null() {
                DeleteObject(self.graph_bitmap as _);
                self.graph_bitmap = null_mut();
            }
            self.graph_bitmap_width = 0;
            self.graph_bitmap_height = 0;
        }
    }

    fn cpu_graph_slot_count(&self) -> usize {
        // 页面模板里预留的 CPU 图槽位数是固定上限。
        STATIC_CPU_GRAPH_COUNT
    }

    fn cpu_graph_control_id(&self, pane_index: usize) -> i32 {
        // CPU pane 控件 ID 在资源编号上是连续分配的。
        IDC_CPUGRAPH + pane_index as i32
    }

    fn cpu_graph_hwnd(&self, hwnd_page: HWND, pane_index: usize) -> HWND {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            if pane_index < self.cpu_graph_slot_count() {
                GetDlgItem(hwnd_page, self.cpu_graph_control_id(pane_index))
            } else {
                null_mut()
            }
        }
    }

    fn visible_cpu_graph_count(&self) -> usize {
        // 汇总模式只显示一张图，多窗格模式按 CPU 数量受上限约束。
        if self.cpu_history_mode == CpuHistoryMode::Panes as i32 {
            self.processor_count.max(1).min(self.cpu_graph_slot_count())
        } else {
            1
        }
    }

    fn redraw_windows(&self, hwnd_page: HWND) -> Vec<HWND> {
        // Resize 时只暂停性能页及其图表相关控件，避免全局重绘状态影响其它页面。
        let mut windows =
            Vec::with_capacity(PERF_LAYOUT_CONTROLS.len() + self.cpu_graph_slot_count() + 8);
        push_unique_window(&mut windows, hwnd_page);

        // 安全性: all child lookups target controls owned by the provided performance page HWND.
        unsafe {
            if !self.no_title {
                for control_id in PERF_LAYOUT_CONTROLS {
                    push_unique_window(&mut windows, GetDlgItem(hwnd_page, control_id));
                }
            }

            for control_id in [
                crate::resource::IDC_CPUFRAME,
                CPU_USAGE_FRAME_ID,
                IDC_CPUMETER,
            ] {
                push_unique_window(&mut windows, GetDlgItem(hwnd_page, control_id));
            }

            if !self.no_title {
                for control_id in [IDC_MEMBARFRAME, IDC_MEMMETER, IDC_MEMFRAME, IDC_MEMGRAPH] {
                    push_unique_window(&mut windows, GetDlgItem(hwnd_page, control_id));
                }
            }
        }

        for pane_index in 0..self.cpu_graph_slot_count() {
            push_unique_window(&mut windows, self.cpu_graph_hwnd(hwnd_page, pane_index));
        }

        windows
    }

    pub fn redraw_after_layout(&self, hwnd_page: HWND) {
        let redraw_windows = self.redraw_windows(hwnd_page);
        redraw_performance_page(hwnd_page, &redraw_windows);
    }
}

fn push_unique_window(windows: &mut Vec<HWND>, hwnd: HWND) {
    if hwnd.is_null() || windows.contains(&hwnd) {
        return;
    }
    windows.push(hwnd);
}

fn set_redraw_for_windows(windows: &[HWND], enabled: bool) {
    // 安全性: callers pass HWNDs collected from the active performance page; WM_SETREDRAW only
    // toggles paint dispatch for those windows and does not transfer ownership.
    unsafe {
        for &hwnd in windows {
            SendMessageW(hwnd, WM_SETREDRAW, usize::from(enabled), 0);
        }
    }
}

fn redraw_performance_page(hwnd_page: HWND, redraw_windows: &[HWND]) {
    if hwnd_page.is_null() {
        return;
    }

    // 安全性: the HWNDs belong to the active performance page. The parent clears stale child
    // positions first, then only visible children repaint from their final layout.
    unsafe {
        RedrawWindow(
            hwnd_page,
            null(),
            null_mut(),
            RDW_INVALIDATE | RDW_ERASE | RDW_NOCHILDREN | RDW_UPDATENOW,
        );

        for &hwnd in redraw_windows {
            if hwnd == hwnd_page || IsWindowVisible(hwnd) == 0 {
                continue;
            }

            InvalidateRect(
                hwnd,
                null(),
                if should_erase_redraw_window(hwnd) {
                    1
                } else {
                    0
                },
            );
            UpdateWindow(hwnd);
        }
    }
}

fn should_erase_redraw_window(hwnd: HWND) -> bool {
    should_erase_redraw_control(unsafe { GetDlgCtrlID(hwnd) })
}

fn should_erase_redraw_control(control_id: i32) -> bool {
    // Frame controls paint a gray interior during WM_ERASEBKGND. Erasing them after a resize
    // clears old chart pixels that can otherwise remain inside group-box interiors.
    PERF_FRAME_CONTROLS.contains(&control_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn performance_frames_are_erased_after_layout_redraw() {
        for control_id in PERF_FRAME_CONTROLS {
            assert!(should_erase_redraw_control(control_id));
        }
    }

    #[test]
    fn non_frame_text_controls_do_not_request_erasing_redraw() {
        assert!(!should_erase_redraw_control(IDC_STATIC2));
    }
}
