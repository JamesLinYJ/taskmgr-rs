// 跨图表复用的 GDI 绘图工具。
// 性能页和网络页共享这些底层绘制原语，避免代码重复。
use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, GetStockObject, BLACK_BRUSH, HBRUSH, HDC,
};

#[derive(Clone, Default)]
pub struct HistoryBuffer {
    values: Vec<u8>,
    head: usize,
}

#[derive(Clone, Copy)]
pub struct HistoryView<'a> {
    values: &'a [u8],
    head: usize,
}

impl HistoryBuffer {
    pub fn with_len(len: usize) -> Self {
        Self {
            values: vec![0; len],
            head: 0,
        }
    }

    pub fn push(&mut self, value: u8) {
        if self.values.is_empty() {
            return;
        }

        self.head = if self.head == 0 {
            self.values.len() - 1
        } else {
            self.head - 1
        };
        self.values[self.head] = value;
    }

    pub fn view(&self) -> HistoryView<'_> {
        HistoryView {
            values: &self.values,
            head: self.head,
        }
    }
}

impl<'a> HistoryView<'a> {
    pub fn from_slice(values: &'a [u8]) -> Self {
        Self { values, head: 0 }
    }

    pub fn len(self) -> usize {
        self.values.len()
    }

    pub fn is_empty(self) -> bool {
        self.values.is_empty()
    }

    pub fn first(self) -> Option<u8> {
        self.get(0)
    }

    pub fn get(self, index: usize) -> Option<u8> {
        if self.values.is_empty() || index >= self.values.len() {
            return None;
        }
        Some(self.values[(self.head + index) % self.values.len()])
    }

    pub fn iter(self) -> HistoryIter<'a> {
        HistoryIter {
            view: self,
            index: 0,
        }
    }
}

pub struct HistoryIter<'a> {
    view: HistoryView<'a>,
    index: usize,
}

impl<'a> Iterator for HistoryIter<'a> {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        let value = self.view.get(self.index)?;
        self.index += 1;
        Some(value)
    }
}

pub fn fill_black(hdc: HDC, rect: &RECT) {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        FillRect(hdc, rect, GetStockObject(BLACK_BRUSH) as HBRUSH);
    }
}

pub fn fill_rect_color(hdc: HDC, rect: &RECT, color: u32) {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
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
