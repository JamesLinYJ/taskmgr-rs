// 性能页实现。
// 该模块消费后台系统快照，并绘制经典任务管理器里的折线图和数值面板。
use std::cell::Cell;
use std::mem::zeroed;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    ERROR_ARITHMETIC_OVERFLOW, ERROR_GEN_FAILURE, ERROR_INVALID_DATA, ERROR_INVALID_WINDOW_HANDLE,
    GetLastError, HINSTANCE, HWND, POINT, RECT,
};
use windows_sys::Win32::Graphics::Gdi::{
    BLACK_BRUSH, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DT_BOTTOM, DT_CENTER,
    DT_SINGLELINE, DeleteDC, DeleteObject, DrawTextW, GetDC, GetStockObject, HBITMAP, HBRUSH, HDC,
    HGDIOBJ, InvalidateRect, RDW_ALLCHILDREN, RDW_ERASE, RDW_INVALIDATE, RDW_UPDATENOW, Rectangle,
    RedrawWindow, ReleaseDC, SRCCOPY, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BS_OWNERDRAW, BeginDeferWindowPos, CreateWindowExW, DeferWindowPos, EndDeferWindowPos,
    GetClientRect, GetDialogBaseUnits, GetDlgItem, HDWP, HMENU, IsWindowVisible, SW_HIDE, SW_SHOW,
    SWP_NOACTIVATE, SWP_NOREDRAW, SWP_NOSIZE, SWP_NOZORDER, SendMessageW, ShowWindow, WM_SETREDRAW,
    WS_CHILD, WS_DISABLED,
};

use crate::assets::{
    STRIP_LIT_BITMAP_RESOURCE, STRIP_LIT_RED_BITMAP_RESOURCE, STRIP_UNLIT_BITMAP_RESOURCE,
    load_bitmap_resource,
};
use crate::chart_renderer::{ChartColor, ChartRenderer};
use crate::drawing::{HistoryBuffer, fill_black, rgb};
use crate::options::{CpuHistoryMode, Options};
use crate::perf_drawing::{
    GRAPH_GRID, HIST_SIZE, HistoryPlotLayout, HistorySeries, current_font_height, defer_resize,
    draw_grid_width, draw_grid_width_gpu, draw_history_series, draw_history_series_gpu, draw_meter,
    format_mem_meter_text, set_numeric_text,
};
use crate::perf_layout::{
    PerfDialogSpacing, PerfLayoutAnchors, compute_perf_layout, next_graph_surface_extent,
};
use crate::resource::{
    IDC_AVAIL_PHYSICAL, IDC_COMMIT_LIMIT, IDC_COMMIT_PEAK, IDC_COMMIT_TOTAL, IDC_CPUGRAPH,
    IDC_CPUMETER, IDC_CPUUSAGEFRAME, IDC_FILE_CACHE, IDC_KERNEL_NONPAGED, IDC_KERNEL_PAGED,
    IDC_KERNEL_TOTAL, IDC_MEMBARFRAME, IDC_MEMFRAME, IDC_MEMGRAPH, IDC_MEMMETER, IDC_STATIC1,
    IDC_STATIC2, IDC_STATIC3, IDC_STATIC4, IDC_STATIC5, IDC_STATIC6, IDC_STATIC8, IDC_STATIC9,
    IDC_STATIC10, IDC_STATIC11, IDC_STATIC12, IDC_STATIC13, IDC_STATIC14, IDC_STATIC15,
    IDC_STATIC16, IDC_STATIC17, IDC_TOTAL_HANDLES, IDC_TOTAL_PHYSICAL, IDC_TOTAL_PROCESSES,
    IDC_TOTAL_THREADS, TEMPLATE_CPU_GRAPH_COUNT,
};
use crate::system_sampler::SystemSample;
use crate::winutil::{
    hiword, loword, record_win32_error, to_wide_null, window_rect_relative_to_page,
};

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
const DYNAMIC_CPU_GRAPH_ID_BASE: i32 = 3000;

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
#[derive(Default)]
pub struct PerformancePageState {
    // 页面级缓存包含采样结果、图表历史和绘制时会复用的 GDI 资源句柄。
    hinstance: HINSTANCE,
    processor_count: usize,
    cpu_graph_hwnds: Vec<HWND>,
    cpu_usage: u8,
    kernel_usage: u8,
    physical_mem_usage_kb: u64,
    physical_mem_limit_kb: u64,
    commit_total_kb: u64,
    commit_limit_kb: u64,
    commit_peak_kb: u64,
    total_physical_kb: u64,
    avail_physical_kb: u64,
    file_cache_kb: u64,
    kernel_total_kb: u64,
    kernel_paged_kb: u64,
    kernel_nonpaged_kb: u64,
    handle_count: u32,
    thread_count: u32,
    process_count: u32,
    cpu_history_mode: i32,
    show_kernel_times: bool,
    no_title: bool,
    scroll_offset: i32,
    cpu_history: Vec<HistoryBuffer>,
    kernel_history: Vec<HistoryBuffer>,
    mem_history: HistoryBuffer,
    averaged_cpu_history: HistoryBuffer,
    averaged_kernel_history: HistoryBuffer,
    gdi_history_points: Vec<POINT>,
    strip_lit_bitmap: HBITMAP,
    strip_lit_red_bitmap: HBITMAP,
    strip_unlit_bitmap: HBITMAP,
    chart_renderer: ChartRenderer,
    graph_dc: HDC,
    graph_bitmap: HBITMAP,
    graph_bitmap_old: HGDIOBJ,
    graph_bitmap_width: i32,
    graph_bitmap_height: i32,
    last_meter_draw_error: Cell<Option<u32>>,
}

impl PerformancePageState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn initialize(&mut self, hinstance: HINSTANCE, processor_count: usize) -> Result<(), u32> {
        // 性能页启动时先准备采样缓冲和仪表位图；
        // 真正依赖窗口尺寸的离屏表面会在布局完成后再创建。
        self.hinstance = hinstance;
        self.ensure_history_capacity(processor_count.max(1));
        if let Err(error) = self.load_meter_bitmaps() {
            // The vector meter is a complete renderer, not a partial bitmap approximation.
            // Keep the resource set empty and make the selected renderer explicit in diagnostics.
            record_win32_error(
                "performance bitmap meter initialization; using GDI meter",
                error,
            );
        }
        self.chart_renderer = ChartRenderer::new();
        Ok(())
    }

    pub fn complete_initialize(&mut self, hwnd_page: HWND) -> Result<(), u32> {
        // Build the complete control set before the page can be activated. The dialog template
        // supplies the first 32 panes; larger systems receive additional native owner-draw
        // buttons with the same lifetime as the page window.
        let control_count = self.processor_count.max(1).max(TEMPLATE_CPU_GRAPH_COUNT);
        let mut controls = Vec::with_capacity(control_count);
        for index in 0..TEMPLATE_CPU_GRAPH_COUNT {
            let control = unsafe { GetDlgItem(hwnd_page, IDC_CPUGRAPH + index as i32) };
            if control.is_null() {
                return Err(ERROR_INVALID_WINDOW_HANDLE);
            }
            controls.push(control);
        }

        if control_count > TEMPLATE_CPU_GRAPH_COUNT {
            let button_class = to_wide_null("Button");
            for index in TEMPLATE_CPU_GRAPH_COUNT..control_count {
                let offset = i32::try_from(index - TEMPLATE_CPU_GRAPH_COUNT)
                    .map_err(|_| ERROR_ARITHMETIC_OVERFLOW)?;
                let control_id = DYNAMIC_CPU_GRAPH_ID_BASE
                    .checked_add(offset)
                    .ok_or(ERROR_ARITHMETIC_OVERFLOW)?;
                // 安全性: the class name is NUL-terminated, the parent page and module instance
                // are live, and Windows owns each successful child window until the page dies.
                let control = unsafe {
                    CreateWindowExW(
                        0,
                        button_class.as_ptr(),
                        null(),
                        WS_CHILD | WS_DISABLED | BS_OWNERDRAW as u32,
                        0,
                        0,
                        0,
                        0,
                        hwnd_page,
                        control_id as usize as HMENU,
                        self.hinstance,
                        null(),
                    )
                };
                if control.is_null() {
                    return Err(last_error_or_gen_failure());
                }
                controls.push(control);
            }
        }

        self.cpu_graph_hwnds = controls;
        Ok(())
    }

    pub fn apply_options(
        &mut self,
        hwnd_page: HWND,
        options: &Options,
        processor_count: usize,
    ) -> bool {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 配置变化会同时影响图表数量、是否叠加内核时间，以及文字区是否折叠。
            let processor_count = processor_count.max(1);
            let layout_changed = self.processor_count != processor_count
                || self.cpu_history_mode != options.cpu_history_mode
                || self.no_title != options.no_title();
            let graph_style_changed = self.show_kernel_times != options.kernel_times();

            self.ensure_history_capacity(processor_count);
            self.cpu_history_mode = options.cpu_history_mode;
            self.show_kernel_times = options.kernel_times();
            self.no_title = options.no_title();

            if !layout_changed && !graph_style_changed {
                return false;
            }

            if layout_changed && !self.no_title {
                self.update_detail_texts(hwnd_page);
            }

            InvalidateRect(hwnd_page, null(), 0);
            layout_changed
        }
    }

    pub fn apply_system_sample(
        &mut self,
        hwnd_page: HWND,
        sample: &SystemSample,
        redraw: bool,
    ) -> Result<(), u32> {
        if sample.processor_count == 0
            || sample.processor_cpu_usage.len() != sample.processor_count
            || sample.processor_kernel_usage.len() != sample.processor_count
        {
            return Err(ERROR_INVALID_DATA);
        }
        self.ensure_history_capacity(sample.processor_count);
        self.cpu_usage = sample.cpu_usage;
        self.kernel_usage = sample.kernel_usage;
        self.physical_mem_usage_kb = sample.physical_mem_usage_kb;
        self.physical_mem_limit_kb = sample.physical_mem_limit_kb;
        self.commit_total_kb = sample.commit_total_kb;
        self.commit_limit_kb = sample.commit_limit_kb;
        self.commit_peak_kb = sample.commit_peak_kb;
        self.total_physical_kb = sample.total_physical_kb;
        self.avail_physical_kb = sample.avail_physical_kb;
        self.file_cache_kb = sample.file_cache_kb;
        self.kernel_total_kb = sample.kernel_total_kb;
        self.kernel_paged_kb = sample.kernel_paged_kb;
        self.kernel_nonpaged_kb = sample.kernel_nonpaged_kb;
        self.handle_count = sample.handle_count;
        self.thread_count = sample.thread_count;
        self.process_count = sample.process_count;

        if sample.cpu_delta_valid {
            for (history, usage) in self
                .cpu_history
                .iter_mut()
                .zip(sample.processor_cpu_usage.iter().copied())
            {
                history.push(usage);
            }
            for (history, usage) in self
                .kernel_history
                .iter_mut()
                .zip(sample.processor_kernel_usage.iter().copied())
            {
                history.push(usage);
            }
            self.averaged_cpu_history.push(sample.cpu_usage);
            self.averaged_kernel_history.push(sample.kernel_usage);
        }
        let mem_percent = if sample.physical_mem_limit_kb == 0 {
            0
        } else {
            ((u128::from(sample.physical_mem_usage_kb) * 100)
                / u128::from(sample.physical_mem_limit_kb))
            .min(100) as u8
        };
        self.mem_history.push(mem_percent);
        self.scroll_offset = (self.scroll_offset + 2) % GRAPH_GRID;

        if !self.no_title {
            self.update_detail_texts(hwnd_page);
        }
        if redraw {
            self.invalidate_graph_controls(hwnd_page);
        }
        Ok(())
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
        let pane_index = if (IDC_CPUGRAPH..IDC_CPUGRAPH + TEMPLATE_CPU_GRAPH_COUNT as i32)
            .contains(&control_id)
        {
            usize::try_from(control_id - IDC_CPUGRAPH).ok()?
        } else if control_id >= DYNAMIC_CPU_GRAPH_ID_BASE {
            TEMPLATE_CPU_GRAPH_COUNT
                .checked_add(usize::try_from(control_id - DYNAMIC_CPU_GRAPH_ID_BASE).ok()?)?
        } else {
            return None;
        };
        (pane_index < self.cpu_graph_slot_count()).then_some(pane_index)
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

    pub fn draw_cpu_graph(&mut self, hdc: HDC, rect: RECT, pane_index: usize) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // CPU 图优先绘制到离屏 DC，再一次性拷回目标 DC，
            // 这样网格线和曲线更新时不会在前台逐步闪出来。
            if pane_index >= self.cpu_history.len() {
                return;
            }
            if self.draw_cpu_graph_gpu(hdc, rect, pane_index) {
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
                        &mut self.gdi_history_points,
                    );
                } else {
                    draw_history_series(
                        target_hdc,
                        &target_rect,
                        plot_layout,
                        HistorySeries {
                            history: &self.averaged_kernel_history,
                            color: ChartColor::Red,
                            stop_on_zero: false,
                        },
                        &mut self.gdi_history_points,
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
                    &mut self.gdi_history_points,
                );
            } else {
                draw_history_series(
                    target_hdc,
                    &target_rect,
                    plot_layout,
                    HistorySeries {
                        history: &self.averaged_cpu_history,
                        color: ChartColor::Green,
                        stop_on_zero: false,
                    },
                    &mut self.gdi_history_points,
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

    pub fn draw_mem_graph(&mut self, hdc: HDC, rect: RECT) {
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
                &mut self.gdi_history_points,
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
                        history: &self.averaged_kernel_history,
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
                    history: &self.averaged_cpu_history,
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
        let mem_percent = (u128::from(self.physical_mem_usage_kb) * 100)
            .checked_div(u128::from(self.physical_mem_limit_kb))
            .unwrap_or(0)
            .min(100) as u8;
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
            let paused_redraw_windows = pause_redraw_for_visible_windows(&redraw_windows);

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

            if EndDeferWindowPos(hdwp) == 0 {
                let error = last_error_or_gen_failure();
                resume_redraw_for_windows(&paused_redraw_windows);
                record_win32_error("performance layout commit", error);
                return;
            }
            resume_redraw_for_windows(&paused_redraw_windows);
            // Newly visible CPU panes must not be exposed at their template positions. Visibility
            // changes happen only after every move and resize has committed and redraw state has
            // been restored. WM_SETREDRAW(TRUE) can restore WS_VISIBLE, so applying show/hide
            // before it would corrupt the final visibility set.
            self.sync_control_visibility(hwnd_page);
            if self.chart_renderer.is_available() {
                self.destroy_graph_surface();
            } else {
                self.ensure_graph_surface(
                    hwnd_page,
                    layout.graph_surface_width,
                    layout.graph_surface_height,
                );
            }
        }
    }

    pub fn destroy(&mut self) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 性能页销毁时顺带释放仪表位图和共享离屏表面。
            self.destroy_graph_surface();
            self.destroy_meter_bitmaps();
        }
        self.cpu_graph_hwnds.clear();
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
        self.cpu_history = (0..processor_count)
            .map(|_| HistoryBuffer::zeroed(HIST_SIZE))
            .collect();
        self.kernel_history = (0..processor_count)
            .map(|_| HistoryBuffer::zeroed(HIST_SIZE))
            .collect();
        self.averaged_cpu_history = HistoryBuffer::zeroed(HIST_SIZE);
        self.averaged_kernel_history = HistoryBuffer::zeroed(HIST_SIZE);
        self.mem_history = HistoryBuffer::zeroed(HIST_SIZE);
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
        set_numeric_text(hwnd_page, IDC_TOTAL_HANDLES, u64::from(self.handle_count));
        set_numeric_text(hwnd_page, IDC_TOTAL_THREADS, u64::from(self.thread_count));
        set_numeric_text(
            hwnd_page,
            IDC_TOTAL_PROCESSES,
            u64::from(self.process_count),
        );
    }

    fn load_meter_bitmaps(&mut self) -> Result<(), u32> {
        // 三个位图是一组资源；只有全部加载成功才提交到页面状态。
        if !self.strip_lit_bitmap.is_null()
            && !self.strip_lit_red_bitmap.is_null()
            && !self.strip_unlit_bitmap.is_null()
        {
            return Ok(());
        }
        unsafe { self.destroy_meter_bitmaps() };

        let lit = load_bitmap_resource(STRIP_LIT_BITMAP_RESOURCE);
        if lit.is_null() {
            return Err(last_error_or_gen_failure());
        }
        let lit_red = load_bitmap_resource(STRIP_LIT_RED_BITMAP_RESOURCE);
        if lit_red.is_null() {
            let error = last_error_or_gen_failure();
            unsafe { DeleteObject(lit as _) };
            return Err(error);
        }
        let unlit = load_bitmap_resource(STRIP_UNLIT_BITMAP_RESOURCE);
        if unlit.is_null() {
            let error = last_error_or_gen_failure();
            unsafe {
                DeleteObject(lit as _);
                DeleteObject(lit_red as _);
            }
            return Err(error);
        }

        self.strip_lit_bitmap = lit;
        self.strip_lit_red_bitmap = lit_red;
        self.strip_unlit_bitmap = unlit;
        Ok(())
    }

    unsafe fn destroy_meter_bitmaps(&mut self) {
        unsafe {
            for bitmap in [
                &mut self.strip_lit_bitmap,
                &mut self.strip_lit_red_bitmap,
                &mut self.strip_unlit_bitmap,
            ] {
                if !bitmap.is_null() {
                    DeleteObject(*bitmap as _);
                    *bitmap = null_mut();
                }
            }
        }
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
            if old.is_null() || old as isize == -1 {
                self.record_meter_draw_error(last_error_or_gen_failure());
                return false;
            }
            Rectangle(hdc, rect.left, rect.top, rect.right, rect.bottom);

            let units = GetDialogBaseUnits() as usize;
            let def_spacing = (DEFSPACING_BASE * i32::from(loword(units))) / DLG_SCALE_X;
            let x_bar_offset = ((rect.right - rect.left) - STRIP_WIDTH) / 2;
            let bar_height = rect.bottom - rect.top - (current_font_height(hdc) + def_spacing * 3);
            if bar_height <= 0 {
                SelectObject(hdc, old);
                self.last_meter_draw_error.set(None);
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
                self.record_meter_draw_error(last_error_or_gen_failure());
                return false;
            }

            let target_lit = ((i32::from(lit_percent) * bar_height) / 100).max(0);
            let target_red = ((i32::from(red_percent) * bar_height) / 100).clamp(0, target_lit);
            let unlit_pixels = ((bar_height - target_lit) / 3) * 3;
            let lit_pixels = bar_height - unlit_pixels;
            let lit_only_pixels = (lit_pixels - target_red).max(0);

            let mut draw_result = self.blit_meter_strip(
                hdc,
                hdc_mem,
                self.strip_unlit_bitmap,
                x_bar_offset,
                def_spacing,
                bar_height - lit_pixels,
            );
            if draw_result.is_ok() && lit_only_pixels > 0 {
                draw_result = self.blit_meter_strip(
                    hdc,
                    hdc_mem,
                    self.strip_lit_bitmap,
                    x_bar_offset,
                    def_spacing + (bar_height - lit_pixels),
                    lit_only_pixels,
                );
            }
            if draw_result.is_ok() && target_red > 0 {
                draw_result = self.blit_meter_strip(
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
            match draw_result {
                Ok(()) => {
                    self.last_meter_draw_error.set(None);
                    true
                }
                Err(error) => {
                    self.record_meter_draw_error(error);
                    false
                }
            }
        }
    }

    fn record_meter_draw_error(&self, error: u32) {
        if self.last_meter_draw_error.replace(Some(error)) != Some(error) {
            record_win32_error("performance bitmap meter drawing", error);
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
    ) -> Result<(), u32> {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 条形位图按固定高度平铺，直到覆盖目标像素高度。
            if bitmap.is_null() || height <= 0 {
                return Ok(());
            }

            let old_bitmap = SelectObject(hdc_mem, bitmap as HGDIOBJ);
            if old_bitmap.is_null() || old_bitmap as isize == -1 {
                return Err(last_error_or_gen_failure());
            }
            let mut remaining = height;
            let mut offset = 0;
            let mut result = Ok(());
            while remaining > 0 {
                let chunk = remaining.min(STRIP_HEIGHT);
                if BitBlt(
                    hdc,
                    x,
                    start_y + offset,
                    STRIP_WIDTH,
                    chunk,
                    hdc_mem,
                    0,
                    0,
                    SRCCOPY,
                ) == 0
                {
                    result = Err(last_error_or_gen_failure());
                    break;
                }
                remaining -= chunk;
                offset += chunk;
            }
            SelectObject(hdc_mem, old_bitmap);
            result
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
            if old_bitmap.is_null() || old_bitmap as isize == -1 {
                DeleteObject(graph_bitmap as _);
                DeleteDC(graph_dc);
                return;
            }
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
        self.cpu_graph_hwnds.len()
    }

    fn cpu_graph_hwnd(&self, _hwnd_page: HWND, pane_index: usize) -> HWND {
        self.cpu_graph_hwnds
            .get(pane_index)
            .copied()
            .unwrap_or(null_mut())
    }

    fn visible_cpu_graph_count(&self) -> usize {
        // 汇总模式只显示一张图，多窗格模式按实际创建的处理器图控件显示。
        if self.cpu_history_mode == CpuHistoryMode::Panes as i32 {
            self.processor_count.max(1).min(self.cpu_graph_slot_count())
        } else {
            1
        }
    }

    fn sync_control_visibility(&self, hwnd_page: HWND) {
        // 安全性: all controls belong to the performance page and are positioned before this
        // method is called. Redraw remains disabled until the complete visibility set is applied.
        unsafe {
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
}

fn last_error_or_gen_failure() -> u32 {
    let error = unsafe { GetLastError() };
    if error == 0 { ERROR_GEN_FAILURE } else { error }
}

fn push_unique_window(windows: &mut Vec<HWND>, hwnd: HWND) {
    if hwnd.is_null() || windows.contains(&hwnd) {
        return;
    }
    windows.push(hwnd);
}

fn pause_redraw_for_visible_windows(windows: &[HWND]) -> Vec<HWND> {
    let mut paused = Vec::with_capacity(windows.len());
    // Only visible windows may be paused. DefWindowProc can remove/add WS_VISIBLE while handling
    // WM_SETREDRAW, so sending the enable message to an originally hidden pane would show it.
    unsafe {
        for &hwnd in windows {
            if IsWindowVisible(hwnd) != 0 {
                SendMessageW(hwnd, WM_SETREDRAW, 0, 0);
                paused.push(hwnd);
            }
        }
    }
    paused
}

fn resume_redraw_for_windows(windows: &[HWND]) {
    // 安全性: this list contains exactly the live page windows paused by the matching helper.
    unsafe {
        for &hwnd in windows {
            SendMessageW(hwnd, WM_SETREDRAW, 1, 0);
        }
    }
}

pub fn redraw_performance_page(hwnd_page: HWND) {
    if hwnd_page.is_null() {
        return;
    }

    // Redraw the committed child tree as one operation. Owner-draw buttons that become visible
    // while their parent is hidden do not reliably receive a first paint from child-level
    // UpdateWindow calls; RDW_ALLCHILDREN makes that first frame part of the page redraw.
    unsafe {
        RedrawWindow(
            hwnd_page,
            null(),
            null_mut(),
            RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
        );
    }
}
