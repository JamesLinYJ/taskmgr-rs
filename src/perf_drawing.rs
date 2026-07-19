// 性能页专用绘图工具。
// 从 perfpage.rs 抽取的 GDI/GPU 图表绘制函数及相关辅助函数。
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    DC_PEN, DT_CALCRECT, DT_CENTER, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_TOP, DT_VCENTER,
    DrawTextW, GetCurrentObject, GetObjectW, GetStockObject, HDC, HFONT, LOGFONTW, LineTo,
    MoveToEx, OBJ_FONT, Polyline, SelectObject, SetBkMode, SetDCPenColor, SetTextColor,
    TRANSPARENT,
};
use windows_sys::Win32::UI::Shell::{
    SFBS_FLAGS_ROUND_TO_NEAREST_DISPLAYED_DIGIT, StrFormatByteSizeEx,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DeferWindowPos, HDWP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOREDRAW, SWP_NOZORDER, SendMessageW,
    SetDlgItemTextW, WM_GETFONT,
};

use crate::chart_renderer::{ChartColor, ChartFrame};
use crate::drawing::{HistoryBuffer, fill_black, fill_rect_color, rgb};
use crate::winutil::to_wide_null;

pub const HIST_SIZE: usize = 2000;
pub const GRAPH_GRID: i32 = 12;

pub fn defer_resize(hdwp: HDWP, hwnd: HWND, width: i32, height: i32) -> HDWP {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
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

pub fn set_numeric_text(hwnd_page: HWND, control_id: i32, value: u64) {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        let mut buf = [0u16; 24];
        write_u64_utf16(value, &mut buf);
        SetDlgItemTextW(hwnd_page, control_id, buf.as_ptr());
    }
}

pub fn draw_graph_label(
    graph_hwnd: HWND,
    hdc: HDC,
    rect: &RECT,
    full_label: &[u16],
    compact_label: &[u16],
) {
    if rect.right - rect.left < 8 || rect.bottom - rect.top < 8 {
        return;
    }

    // Labels are transparent overlays on the graph. Both forms are cached by the page, so paint
    // only measures and selects between the full and compact representations.
    unsafe {
        if graph_hwnd.is_null() {
            return;
        }
        let font = SendMessageW(graph_hwnd, WM_GETFONT, 0, 0) as HFONT;
        if font.is_null() {
            return;
        }
        let old_font = SelectObject(hdc, font);
        if old_font.is_null() || old_font as isize == -1 {
            return;
        }

        let available_width = (rect.right - rect.left - 4).max(0);
        let available_height = (rect.bottom - rect.top - 4).max(0);
        let mut selected_label = None;
        for label in [full_label, compact_label] {
            if label.first().copied().unwrap_or(0) == 0 {
                continue;
            }
            let mut measured = RECT {
                left: 0,
                top: 0,
                right: available_width,
                bottom: available_height,
            };
            let measured_height = DrawTextW(
                hdc,
                label.as_ptr() as *mut u16,
                -1,
                &mut measured,
                DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
            );
            if measured_height > 0
                && measured_height <= available_height
                && measured.right - measured.left <= available_width
            {
                selected_label = Some(label);
                break;
            }
        }

        let old_bk_mode = SetBkMode(hdc, TRANSPARENT as i32);
        let old_text_color = SetTextColor(hdc, rgb(255, 255, 255));
        if let Some(label) = selected_label {
            let mut text_rect = RECT {
                left: rect.left + 2,
                top: rect.top + 2,
                right: rect.right - 2,
                bottom: rect.bottom - 2,
            };
            DrawTextW(
                hdc,
                label.as_ptr() as *mut u16,
                -1,
                &mut text_rect,
                DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX,
            );
        }

        SetTextColor(hdc, old_text_color);
        SetBkMode(hdc, old_bk_mode);
        SelectObject(hdc, old_font);
    }
}

fn write_u64_utf16(mut value: u64, buf: &mut [u16]) {
    if buf.is_empty() {
        return;
    }
    buf.fill(0);
    if value == 0 {
        if buf.len() > 1 {
            buf[0] = b'0' as u16;
        }
        return;
    }

    // 最后一个槽位始终保留给 NUL；数字先倒序写到尾部，再整体移到开头。
    let digits_end = buf.len() - 1;
    let mut i = digits_end;
    while value > 0 && i > 0 {
        i -= 1;
        buf[i] = (b'0' + (value % 10) as u8) as u16;
        value /= 10;
    }
    let digit_count = digits_end - i;
    buf.copy_within(i..digits_end, 0);
    buf[digit_count] = 0;
}

pub fn format_mem_meter_text(mem_usage_kb: u64) -> String {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        let mut buffer = [0u16; 32];
        if let Some(byte_count) = mem_usage_kb.checked_mul(1024)
            && StrFormatByteSizeEx(
                byte_count,
                SFBS_FLAGS_ROUND_TO_NEAREST_DISPLAYED_DIGIT,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            ) >= 0
        {
            let len = buffer
                .iter()
                .position(|&ch| ch == 0)
                .unwrap_or(buffer.len());
            return String::from_utf16_lossy(&buffer[..len]);
        }
        format!("{mem_usage_kb} KB")
    }
}

pub fn draw_grid_width(hdc: HDC, rect: &RECT, width: i32, scroll_offset: i32) {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        let old_pen = SelectObject(hdc, GetStockObject(DC_PEN) as _);
        SetDCPenColor(hdc, rgb(0, 128, 64));
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
    }
}

#[derive(Clone, Copy)]
pub struct HistoryPlotLayout {
    pub graph_height: i32,
    pub width: i32,
    pub scale: usize,
}

#[derive(Clone, Copy)]
pub struct HistorySeries<'a, H: HistoryAccess + ?Sized> {
    pub history: &'a H,
    pub color: ChartColor,
    pub stop_on_zero: bool,
}

pub trait HistoryAccess {
    fn is_empty(&self) -> bool;
    fn len(&self) -> usize;
    fn newest_value(&self) -> u8;
    fn value_at(&self, index: usize) -> Option<u8>;
}

impl HistoryAccess for [u8] {
    fn is_empty(&self) -> bool {
        <[u8]>::is_empty(self)
    }

    fn len(&self) -> usize {
        <[u8]>::len(self)
    }

    fn newest_value(&self) -> u8 {
        self.first().copied().unwrap_or(0)
    }

    fn value_at(&self, index: usize) -> Option<u8> {
        self.get(index).copied()
    }
}

impl HistoryAccess for Vec<u8> {
    fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }

    fn len(&self) -> usize {
        self.as_slice().len()
    }

    fn newest_value(&self) -> u8 {
        self.as_slice().first().copied().unwrap_or(0)
    }

    fn value_at(&self, index: usize) -> Option<u8> {
        self.get(index).copied()
    }
}

impl HistoryAccess for HistoryBuffer {
    fn is_empty(&self) -> bool {
        HistoryBuffer::is_empty(self)
    }

    fn len(&self) -> usize {
        HistoryBuffer::len(self)
    }

    fn newest_value(&self) -> u8 {
        HistoryBuffer::newest_value(self)
    }

    fn value_at(&self, index: usize) -> Option<u8> {
        HistoryBuffer::value_at(self, index)
    }
}

pub fn draw_history_series<H: HistoryAccess + ?Sized>(
    hdc: HDC,
    rect: &RECT,
    layout: HistoryPlotLayout,
    series: HistorySeries<'_, H>,
    points: &mut Vec<POINT>,
) {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        if series.history.is_empty() {
            return;
        }

        let color_rgb = match series.color {
            ChartColor::Black => rgb(0, 0, 0),
            ChartColor::Green => rgb(0, 255, 0),
            ChartColor::Yellow => rgb(255, 255, 0),
            ChartColor::Red => rgb(255, 0, 0),
            ChartColor::Grid => rgb(0, 128, 64),
        };

        let old_pen = SelectObject(hdc, GetStockObject(DC_PEN) as _);
        SetDCPenColor(hdc, color_rgb);

        let max_points = (layout.width as usize / layout.scale).min(series.history.len());
        let start_x = rect.right;
        let start_y =
            rect.bottom - (i32::from(series.history.newest_value()) * layout.graph_height) / 100;
        points.clear();
        points.reserve(max_points.saturating_add(1));
        points.push(POINT {
            x: start_x,
            y: start_y,
        });

        for index in 0..max_points {
            let value = series.history.value_at(index).unwrap_or(0);
            if series.stop_on_zero && value == 0 {
                break;
            }
            points.push(POINT {
                x: rect.right - (layout.scale * index) as i32,
                y: rect.bottom - (i32::from(value) * layout.graph_height) / 100,
            });
        }

        if points.len() > 1 {
            Polyline(hdc, points.as_ptr(), points.len() as i32);
        }

        SelectObject(hdc, old_pen);
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

pub fn draw_history_series_gpu<H: HistoryAccess + ?Sized>(
    frame: &ChartFrame<'_>,
    rect: &RECT,
    layout: HistoryPlotLayout,
    series: HistorySeries<'_, H>,
) {
    if series.history.is_empty() {
        return;
    }

    let mut previous_x = rect.right as f32;
    let mut previous_y = (rect.bottom
        - (i32::from(series.history.newest_value()) * layout.graph_height) / 100)
        as f32;

    for index in 0..series.history.len() {
        if index * layout.scale >= layout.width as usize {
            break;
        }
        let value = series.history.value_at(index).unwrap_or(0);
        if series.stop_on_zero && value == 0 {
            break;
        }

        let x = (rect.right - (layout.scale * index) as i32) as f32;
        let y = (rect.bottom - (i32::from(value) * layout.graph_height) / 100) as f32;
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
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
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

pub fn current_font_height(hdc: HDC) -> i32 {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u64_formatter_preserves_the_full_value_and_terminator() {
        let mut buffer = [0u16; 24];
        write_u64_utf16(u64::MAX, &mut buffer);
        let length = buffer.iter().position(|value| *value == 0).unwrap();
        assert_eq!(
            String::from_utf16_lossy(&buffer[..length]),
            u64::MAX.to_string()
        );
    }

    #[test]
    fn memory_formatter_preserves_unrepresentable_byte_counts_as_kilobytes() {
        assert_eq!(format_mem_meter_text(u64::MAX), format!("{} KB", u64::MAX));
    }
}
