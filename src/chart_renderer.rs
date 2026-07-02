//! 图表渲染抽象。
//! 当前优先走 Direct2D；如果初始化或每帧绑定失败，调用方可以回退到原有 GDI 路径。

use windows::Win32::Foundation::RECT as WinRect;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1SolidColorBrush,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN;
use windows::Win32::Graphics::Gdi::HDC as WinHdc;
use windows_numerics::Vector2;

type SysRect = windows_sys::Win32::Foundation::RECT;
type SysHdc = windows_sys::Win32::Graphics::Gdi::HDC;

const GRID_STROKE_WIDTH: f32 = 1.0;
const SERIES_STROKE_WIDTH: f32 = 2.0;

#[derive(Clone, Copy)]
pub enum ChartColor {
    // 统一图表配色枚举，避免页面代码直接散落 RGB 常量。
    Black,
    Green,
    Yellow,
    Red,
    Grid,
}

pub struct ChartRenderer {
    // `ChartRenderer` 是页面能看到的唯一渲染入口。
    backend: RendererBackend,
}

enum RendererBackend {
    // 目前后端只有 Direct2D 和“不可用时回退”两种状态。
    Direct2D(Direct2DRenderer),
    Unavailable,
}

struct Direct2DRenderer {
    // 共享一份 D2D DC RenderTarget，逐帧绑定到不同 HDC 上使用。
    target: ID2D1DCRenderTarget,
}

pub struct ChartFrame<'a> {
    // `ChartFrame` 代表一帧有效的绘制上下文，结束时由调用方显式提交。
    renderer: &'a Direct2DRenderer,
    black: ID2D1SolidColorBrush,
    green: ID2D1SolidColorBrush,
    yellow: ID2D1SolidColorBrush,
    red: ID2D1SolidColorBrush,
    grid: ID2D1SolidColorBrush,
    rect: WinRect,
}

impl ChartRenderer {
    pub fn new() -> Self {
        // 初始化失败时静默进入 `Unavailable`，让调用方自然回退到 GDI 路径。
        Self {
            backend: Direct2DRenderer::new()
                .map(RendererBackend::Direct2D)
                .unwrap_or(RendererBackend::Unavailable),
        }
    }

    pub fn begin_frame(&self, hdc: SysHdc, rect: SysRect) -> Option<ChartFrame<'_>> {
        // 只有后端可用并且本帧成功绑定到目标 DC 时才返回可绘制帧。
        match &self.backend {
            RendererBackend::Direct2D(renderer) => renderer.begin_frame(hdc, rect),
            RendererBackend::Unavailable => None,
        }
    }

    #[cfg(debug_assertions)]
    #[allow(dead_code)]
    pub fn debug_backend_name(&self) -> &'static str {
        match self.backend {
            RendererBackend::Direct2D(_) => "direct2d",
            RendererBackend::Unavailable => "gdi-fallback",
        }
    }
}

impl Default for ChartRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Direct2DRenderer {
    fn new() -> Option<Self> {
        // 安全性: Direct2D COM wrapper methods validate HRESULTs; the factory and render target
        // are retained by `Direct2DRenderer` and used on the UI drawing thread.
        unsafe {
            // 图表绘制是 2D 单线程工作负载，因此单线程 D2D factory 足够。
            let factory: ID2D1Factory =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?;
            let properties = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_UNKNOWN,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: 0.0,
                dpiY: 0.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let target = factory.CreateDCRenderTarget(&properties).ok()?;
            Some(Self { target })
        }
    }

    fn begin_frame(&self, hdc: SysHdc, rect: SysRect) -> Option<ChartFrame<'_>> {
        // 安全性: `hdc` and `rect` come from the current paint operation; D2D calls are
        // synchronous and brushes are owned by the returned frame until `end`.
        unsafe {
            // D2D 绑定到 HDC 后，后续绘制统一使用局部坐标系。
            let rect = to_win_rect(rect);
            let local_rect = WinRect {
                left: 0,
                top: 0,
                right: rect.right - rect.left,
                bottom: rect.bottom - rect.top,
            };
            self.target.BindDC(WinHdc(hdc as _), &rect).ok()?;
            self.target.BeginDraw();

            let frame = (|| {
                let black = self
                    .target
                    .CreateSolidColorBrush(&color(0, 0, 0), None)
                    .ok()?;
                let green = self
                    .target
                    .CreateSolidColorBrush(&color(0, 255, 0), None)
                    .ok()?;
                let yellow = self
                    .target
                    .CreateSolidColorBrush(&color(255, 255, 0), None)
                    .ok()?;
                let red = self
                    .target
                    .CreateSolidColorBrush(&color(255, 0, 0), None)
                    .ok()?;
                let grid = self
                    .target
                    .CreateSolidColorBrush(&color(0, 128, 64), None)
                    .ok()?;

                Some(ChartFrame {
                    renderer: self,
                    black,
                    green,
                    yellow,
                    red,
                    grid,
                    rect: local_rect,
                })
            })();

            if frame.is_none() {
                let _ = self.target.EndDraw(None, None);
            }

            frame
        }
    }
}

impl ChartFrame<'_> {
    pub fn clear_black(&self) {
        // 大多数图表都以黑底开始，因此单独提供快捷入口。
        self.fill_color(self.rect, ChartColor::Black);
    }

    pub fn bounds(&self) -> SysRect {
        // 暴露当前帧的局部边界，方便上层布局和裁剪。
        SysRect {
            left: self.rect.left,
            top: self.rect.top,
            right: self.rect.right,
            bottom: self.rect.bottom,
        }
    }

    pub(crate) fn fill_color(&self, rect: impl IntoWinRect, color: ChartColor) {
        // 统一走颜色枚举，避免页面层直接碰 D2D brush。
        self.fill_rect(rect.into_win_rect(), self.brush(color));
    }

    pub fn draw_grid_line(&self, x0: f32, y0: f32, x1: f32, y1: f32, color: ChartColor) {
        self.draw_line_with_width(x0, y0, x1, y1, color, GRID_STROKE_WIDTH);
    }

    pub fn draw_series_line(&self, x0: f32, y0: f32, x1: f32, y1: f32, color: ChartColor) {
        self.draw_line_with_width(x0, y0, x1, y1, color, SERIES_STROKE_WIDTH);
    }

    fn draw_line_with_width(
        &self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        color: ChartColor,
        width: f32,
    ) {
        // 网格线和数据折线共用这一条底层线段绘制路径。
        // 安全性: the render target is inside an active BeginDraw/EndDraw frame and the brush
        // belongs to this frame.
        unsafe {
            self.renderer.target.DrawLine(
                Vector2 { X: x0, Y: y0 },
                Vector2 { X: x1, Y: y1 },
                self.brush(color),
                width,
                None,
            );
        }
    }

    pub fn fill_rect(&self, rect: WinRect, brush: &ID2D1SolidColorBrush) {
        // 填充矩形是最常见的图表背景/块状绘制原语。
        let rect = to_d2d_rect(rect);
        // 安全性: the render target is inside an active frame and `rect`/`brush` are valid for
        // the duration of the synchronous call.
        unsafe {
            self.renderer.target.FillRectangle(&rect, brush);
        }
    }

    pub fn end(self) -> bool {
        // `EndDraw` 失败时由调用方决定是否退回 GDI。
        // 安全性: this consumes the active frame and closes the matching BeginDraw call.
        unsafe { self.renderer.target.EndDraw(None, None).is_ok() }
    }

    fn brush(&self, color: ChartColor) -> &ID2D1SolidColorBrush {
        match color {
            ChartColor::Black => &self.black,
            ChartColor::Green => &self.green,
            ChartColor::Yellow => &self.yellow,
            ChartColor::Red => &self.red,
            ChartColor::Grid => &self.grid,
        }
    }
}

fn to_d2d_rect(rect: WinRect) -> D2D_RECT_F {
    // Win32 像素矩形和 D2D 浮点矩形之间的轻量转换。
    D2D_RECT_F {
        left: rect.left as f32,
        top: rect.top as f32,
        right: rect.right as f32,
        bottom: rect.bottom as f32,
    }
}

pub(crate) trait IntoWinRect {
    // 让页面层可以在 `windows` 和 `windows-sys` 两套 RECT 之间无痛复用接口。
    fn into_win_rect(self) -> WinRect;
}

impl IntoWinRect for WinRect {
    fn into_win_rect(self) -> WinRect {
        self
    }
}

impl IntoWinRect for SysRect {
    fn into_win_rect(self) -> WinRect {
        to_win_rect(self)
    }
}

fn to_win_rect(rect: SysRect) -> WinRect {
    WinRect {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

fn color(red: u8, green: u8, blue: u8) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: f32::from(red) / 255.0,
        g: f32::from(green) / 255.0,
        b: f32::from(blue) / 255.0,
        a: 1.0,
    }
}
