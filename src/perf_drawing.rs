// 性能页专用绘图工具。
// 从 perfpage.rs 抽取的 GDI/GPU 图表绘制函数及相关辅助函数。
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    CreatePen, DeleteObject, DrawTextW, GetCurrentObject, GetObjectW, LineTo, MoveToEx,
    SelectObject, SetBkMode, SetTextColor, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE,
    DT_VCENTER, HDC, LOGFONTW, OBJ_FONT, PS_SOLID, TRANSPARENT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DeferWindowPos, SetDlgItemTextW, HDWP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOREDRAW, SWP_NOZORDER,
};
use windows_sys::Win32::UI::Shell::StrFormatByteSizeW;

use crate::chart_renderer::{ChartColor, ChartFrame};
use crate::drawing::{fill_black, fill_rect_color, rgb};
use crate::winutil::to_wide_null;

pub const HIST_SIZE: usize = 2000;
pub const GRAPH_GRID: i32 = 12;

pub fn defer_resize(hdwp: HDWP, hwnd: HWND, width: i32, height: i32) -> HDWP {
    // SAFETY: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        // 只改尺寸不改位置，是性能页布局中最常见的 `DeferWindowPos` 变体。
        if hwnd.is_null() {
            return hdwp;
        }
        DeferWindowPos(
            hdwp,
            hwnd,
            null_mut(),
            0,
            0,
            width,
            height,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW,
        )
    }
}

pub fn set_numeric_text(hwnd_page: HWND, control_id: i32, value: u32) {
    // SAFETY: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        let text = to_wide_null(&value.to_string());
        SetDlgItemTextW(hwnd_page, control_id, text.as_ptr());
    }
}

pub fn format_mem_meter_text(mem_usage_kb: u32) -> String {
    // SAFETY: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        let mut buffer = [0u16; 32];
        if !StrFormatByteSizeW(
            i64::from(mem_usage_kb) * 1024,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
        .is_null()
        {
            let len = buffer
                .iter()
                .position(|&ch| ch == 0)
                .unwrap_or(buffer.len());
            return String::from_utf16_lossy(&buffer[..len]);
        }

        // Match XP intent: prefer compact byte-size text over raw kilobytes.
        let mem_usage_bytes = u64::from(mem_usage_kb) * 1024;
        let gib = 1024_u64 * 1024 * 1024;
        let mib = 1024_u64 * 1024;
        if mem_usage_bytes >= gib {
            format!("{:.1} GB", mem_usage_bytes as f64 / gib as f64)
        } else if mem_usage_bytes >= mib {
            format!("{:.1} MB", mem_usage_bytes as f64 / mib as f64)
        } else {
            format!("{mem_usage_kb} KB")
        }
    }
}

pub fn draw_grid_width(hdc: HDC, rect: &RECT, width: i32, scroll_offset: i32) {
    // SAFETY: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        let pen = CreatePen(PS_SOLID, 1, rgb(0, 128, 64));
        if pen.is_null() {
            return;
        }

        let old_pen = SelectObject(hdc, pen as _);
        let left = rect.right - width.max(0);
        let right = rect.right;
        let top = rect.top;
        let bottom = rect.bottom;

        let mut y = top + GRAPH_GRID - 1;
        while y < bottom {
            MoveToEx(hdc, left, y, null_mut());
            LineTo(hdc, right, y);
            y += GRAPH_GRID;
        }

        let mut x = right - scroll_offset;
        while x > left {
            MoveToEx(hdc, x, top, null_mut());
            LineTo(hdc, x, bottom);
            x -= GRAPH_GRID;
        }

        SelectObject(hdc, old_pen);
        DeleteObject(pen as _);
    }
}

#[derive(Clone, Copy)]
pub struct HistoryPlotLayout {
    pub graph_height: i32,
    pub width: i32,
    pub scale: usize,
}

#[derive(Clone, Copy)]
pub struct HistorySeries<'a> {
    pub history: &'a [u8],
    pub color: ChartColor,
    pub stop_on_zero: bool,
}

pub fn draw_history_series(
    hdc: HDC,
    rect: &RECT,
    layout: HistoryPlotLayout,
    series: HistorySeries<'_>,
) {
    // SAFETY: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        if series.history.is_empty() {
            return;
        }

        let pen = CreatePen(
            PS_SOLID,
            2,
            match series.color {
                ChartColor::Black => rgb(0, 0, 0),
                ChartColor::Green => rgb(0, 255, 0),
                ChartColor::Yellow => rgb(255, 255, 0),
                ChartColor::Red => rgb(255, 0, 0),
                ChartColor::Grid => rgb(0, 128, 64),
            },
        );
        if pen.is_null() {
            return;
        }

        let old_pen = SelectObject(hdc, pen as _);
        MoveToEx(
            hdc,
            rect.right,
            rect.bottom - (i32::from(series.history[0]) * layout.graph_height) / 100,
            null_mut(),
        );

        for (index, value) in series.history.iter().enumerate() {
            if index * layout.scale >= layout.width as usize {
                break;
            }
            if series.stop_on_zero && *value == 0 {
                break;
            }

            LineTo(
                hdc,
                rect.right - (layout.scale * index) as i32,
                rect.bottom - (i32::from(*value) * layout.graph_height) / 100,
            );
        }

        SelectObject(hdc, old_pen);
        DeleteObject(pen as _);
    }
}

pub fn draw_grid_width_gpu(frame: &ChartFrame<'_>, rect: &RECT, width: i32, scroll_offset: i32) {
    let left = rect.right - width.max(0);
    let right = rect.right;
    let top = rect.top;
    let bottom = rect.bottom;

    let mut y = top + GRAPH_GRID - 1;
    while y < bottom {
        frame.draw_grid_line(
            left as f32,
            y as f32,
            right as f32,
            y as f32,
            ChartColor::Grid,
        );
        y += GRAPH_GRID;
    }

    let mut x = right - scroll_offset;
    while x > left {
        frame.draw_grid_line(
            x as f32,
            top as f32,
            x as f32,
            bottom as f32,
            ChartColor::Grid,
        );
        x -= GRAPH_GRID;
    }
}

pub fn draw_history_series_gpu(
    frame: &ChartFrame<'_>,
    rect: &RECT,
    layout: HistoryPlotLayout,
    series: HistorySeries<'_>,
) {
    if series.history.is_empty() {
        return;
    }

    let mut previous_x = rect.right as f32;
    let mut previous_y =
        (rect.bottom - (i32::from(series.history[0]) * layout.graph_height) / 100) as f32;

    for (index, value) in series.history.iter().enumerate() {
        if index * layout.scale >= layout.width as usize {
            break;
        }
        if series.stop_on_zero && *value == 0 {
            break;
        }

        let x = (rect.right - (layout.scale * index) as i32) as f32;
        let y = (rect.bottom - (i32::from(*value) * layout.graph_height) / 100) as f32;
        frame.draw_series_line(previous_x, previous_y, x, y, series.color);
        previous_x = x;
        previous_y = y;
    }
}

pub fn draw_meter(
    hdc: HDC,
    rect: RECT,
    label: &str,
    fill_percent: u8,
    red_percent: u8,
    main_color: u32,
    red_color: u32,
) {
    // SAFETY: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        fill_black(hdc, &rect);

        let mut text_rect = rect;
        text_rect.top = rect.bottom - 18;

        let graph_top = rect.top + 4;
        let graph_bottom = (text_rect.top - 4).max(graph_top);
        let graph_height = (graph_bottom - graph_top).max(1);
        let bar_width = 20;
        let bar_left = rect.left + ((rect.right - rect.left - bar_width) / 2).max(0);
        let bar_right = bar_left + bar_width;

        let lit_pixels = ((graph_height * i32::from(fill_percent)) / 100).clamp(0, graph_height);
        let red_pixels = ((graph_height * i32::from(red_percent)) / 100).clamp(0, lit_pixels);

        if lit_pixels < graph_height {
            let unlit_rect = RECT {
                left: bar_left,
                top: graph_top,
                right: bar_right,
                bottom: graph_bottom - lit_pixels,
            };
            fill_rect_color(hdc, &unlit_rect, rgb(32, 32, 32));
        }

        if lit_pixels > red_pixels {
            let lit_rect = RECT {
                left: bar_left,
                top: graph_bottom - lit_pixels,
                right: bar_right,
                bottom: graph_bottom - red_pixels,
            };
            fill_rect_color(hdc, &lit_rect, main_color);
        }

        if red_pixels > 0 {
            let red_rect = RECT {
                left: bar_left,
                top: graph_bottom - red_pixels,
                right: bar_right,
                bottom: graph_bottom,
            };
            fill_rect_color(hdc, &red_rect, red_color);
        }

        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, rgb(0, 255, 0));
        let mut label_wide = to_wide_null(label);
        DrawTextW(
            hdc,
            label_wide.as_mut_ptr(),
            -1,
            &mut text_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }
}

pub fn average_history(history_sets: &[Vec<u8>]) -> Vec<u8> {
    let Some(first_history) = history_sets.first() else {
        return Vec::new();
    };

    let mut averaged = vec![0u8; first_history.len()];
    for (index, value) in averaged.iter_mut().enumerate() {
        let sum = history_sets
            .iter()
            .map(|history| u32::from(history.get(index).copied().unwrap_or_default()))
            .sum::<u32>();
        *value = (sum / history_sets.len() as u32).min(100) as u8;
    }

    averaged
}

pub fn current_font_height(hdc: HDC) -> i32 {
    // SAFETY: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        let font = GetCurrentObject(hdc, OBJ_FONT as u32);
        if font.is_null() {
            return 0;
        }

        let mut font_info = zeroed::<LOGFONTW>();
        if GetObjectW(
            font,
            size_of::<LOGFONTW>() as i32,
            &mut font_info as *mut _ as *mut c_void,
        ) == 0
        {
            return 0;
        }

        font_info.lfHeight.abs()
    }
}
