// 跨图表复用的 GDI 绘图工具。
// 性能页和网络页共享这些底层绘制原语，避免代码重复。
use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::{
    BLACK_BRUSH, DC_BRUSH, FillRect, GetStockObject, HBRUSH, HDC, SetDCBrushColor,
};

#[derive(Clone, Default)]
pub struct HistoryBuffer {
    // 环形历史缓冲，逻辑顺序仍然是“最新在前”，但 push 不再搬移整段数组。
    values: Vec<u8>,
    newest: usize,
}

impl HistoryBuffer {
    pub fn zeroed(len: usize) -> Self {
        Self {
            values: vec![0; len],
            newest: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn push(&mut self, value: u8) {
        if self.values.is_empty() {
            return;
        }
        self.newest = if self.newest == 0 {
            self.values.len() - 1
        } else {
            self.newest - 1
        };
        self.values[self.newest] = value;
    }

    pub fn value_at(&self, offset_from_newest: usize) -> Option<u8> {
        if offset_from_newest >= self.values.len() {
            return None;
        }
        Some(self.values[(self.newest + offset_from_newest) % self.values.len()])
    }

    pub fn newest_value(&self) -> u8 {
        self.value_at(0).unwrap_or(0)
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
        let brush = GetStockObject(DC_BRUSH) as HBRUSH;
        SetDCBrushColor(hdc, color);
        FillRect(hdc, rect, brush);
    }
}

pub const fn rgb(red: u8, green: u8, blue: u8) -> u32 {
    red as u32 | ((green as u32) << 8) | ((blue as u32) << 16)
}

#[cfg(test)]
mod tests {
    use super::HistoryBuffer;

    #[test]
    fn history_buffer_reads_newest_first_without_shifting() {
        let mut history = HistoryBuffer::zeroed(4);
        history.push(1);
        history.push(2);
        history.push(3);

        let values: Vec<u8> = (0..history.len())
            .map(|index| history.value_at(index).unwrap())
            .collect();
        assert_eq!(values, vec![3, 2, 1, 0]);

        history.push(4);
        history.push(5);
        let values: Vec<u8> = (0..history.len())
            .map(|index| history.value_at(index).unwrap())
            .collect();
        assert_eq!(values, vec![5, 4, 3, 2]);
    }
}
