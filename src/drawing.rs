// 跨图表复用的 GDI 绘图工具。
// 性能页和网络页共享这些底层绘制原语，避免代码重复。
use windows_sys::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, GetStockObject, BLACK_BRUSH, HBRUSH, HDC,
};
use windows_sys::Win32::Foundation::RECT;

pub fn push_history(history: &mut [u8], value: u8) {
    // 历史值按"最新在前"滚动，绘图时就可以直接从右向左连接。
    if history.is_empty() {
        return;
    }
    history.copy_within(..history.len() - 1, 1);
    history[0] = value;
}

pub fn fill_black(hdc: HDC, rect: &RECT) {
    // SAFETY: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        FillRect(hdc, rect, GetStockObject(BLACK_BRUSH) as HBRUSH);
    }
}

pub fn fill_rect_color(hdc: HDC, rect: &RECT, color: u32) {
    // SAFETY: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        let brush = CreateSolidBrush(color);
        if brush.is_null() {
            return;
        }
        FillRect(hdc, rect, brush);
        DeleteObject(brush as _);
    }
}

pub const fn rgb(red: u8, green: u8, blue: u8) -> u32 {
    red as u32 | ((green as u32) << 8) | ((blue as u32) << 16)
}
