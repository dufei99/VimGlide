//! 覆盖全屏的透明遮罩窗口。
//!
//! 窗口使用 `WS_EX_NOREDIRECTIONBITMAP`，内容由 DirectComposition 承载的
//! DXGI 交换链提供，用 Direct2D 直接绘制，不再分配整屏 CPU 位图。

use std::mem::ManuallyDrop;

use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::{GetLastError, HMODULE, HWND, ERROR_CLASS_ALREADY_EXISTS};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateDevice, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_ROUNDED_RECT, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, ID2D1Bitmap1, ID2D1Device,
    ID2D1DeviceContext, ID2D1Image, ID2D1SolidColorBrush, ID2D1StrokeStyle,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, ID3D11Device,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS, DWRITE_WORD_WRAPPING_NO_WRAP, IDWriteFactory,
    IDWriteTextFormat, IDWriteTextLayout,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGIOutput, IDXGISurface, IDXGISwapChain1,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GWL_EXSTYLE, GetForegroundWindow, GetWindowLongPtrW,
    GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOPMOST, RegisterClassExW, SW_HIDE,
    SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, WNDCLASSEXW, WS_EX_NOACTIVATE,
    WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use windows_numerics::Vector2;

use crate::model::Rect;
use crate::render::{Color, Op, Scene, TextBlock};

const CLASS_NAME: PCWSTR = w!("VimGlide_overlay");
/// 测量文字时的宽松上限：只需拿到单行宽度，不需要换行。
const MEASURE_LIMIT: f32 = 100_000.0;

pub struct Overlay {
    hwnd: HWND,
    width: i32,
    height: i32,
    origin: (i32, i32),
    visible: bool,
    /// 是否为了让遮罩压住任务栏而临时抢占了前台。
    activated: bool,
    /// 下一次显示时是否允许抢占前台（仅提示模式，启动提示不抢）。
    activate_pending: bool,
    /// 抢占前的前台窗口，退出提示模式时恢复。
    restore: HWND,
    /// DirectWrite 对象：只在需要布局/绘制文字时持有。
    text: Option<TextEngine>,
    /// 图形设备资源：显示时按需创建，隐藏后整体释放。
    gfx: Option<Gfx>,
}

impl Overlay {
    /// 临时抢占前台，使遮罩成为前台进程的置顶窗口。
    ///
    /// # Safety
    /// `self.hwnd` 必须是有效的遮罩窗口。
    unsafe fn take_foreground(&mut self, foreground: HWND) {
        // SAFETY: 句柄由本结构持有。
        unsafe {
            let style = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            // WS_EX_NOACTIVATE 的窗口无法成为前台，先临时去掉。
            let _ = SetWindowLongPtrW(
                self.hwnd,
                GWL_EXSTYLE,
                style & !(WS_EX_NOACTIVATE.0 as isize),
            );
            force_foreground(self.hwnd);
            if GetForegroundWindow() == self.hwnd {
                self.activated = true;
                self.restore = foreground;
            } else {
                let _ = SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, style);
            }
        }
    }

    pub fn new(screen: Rect) -> Result<Self, String> {
        unsafe {
            let instance = GetModuleHandleW(None)
                .map_err(|e| format!("获取模块句柄失败：{e}"))?;

            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(super::wnd_proc),
                hInstance: instance.into(),
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            if RegisterClassExW(&class) == 0 {
                let err = GetLastError();
                if err != ERROR_CLASS_ALREADY_EXISTS {
                    return Err(format!("注册窗口类失败：{err:?}"));
                }
            }

            // 不创建重定向位图（内容由 DirectComposition 提供），并保持
            // 置顶、鼠标穿透、不抢焦点。
            let hwnd = CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE,
                CLASS_NAME,
                w!("VimGlide"),
                WS_POPUP,
                screen.x,
                screen.y,
                screen.w,
                screen.h,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .map_err(|e| format!("创建遮罩窗口失败：{e}"))?;

            Ok(Self {
                hwnd,
                width: screen.w,
                height: screen.h,
                origin: (screen.x, screen.y),
                visible: false,
                activated: false,
                activate_pending: false,
                restore: HWND::default(),
                text: None,
                gfx: None,
            })
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// 请求下一次显示时抢占前台（用于压住任务栏等置顶窗口）。
    pub fn request_activation(&mut self) {
        self.activate_pending = true;
    }

    /// 遮罩层所在显示器的缩放系数。
    pub fn dpi_scale(&self) -> f32 {
        // SAFETY: hwnd 有效。
        let dpi = unsafe { GetDpiForWindow(self.hwnd) };
        if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
    }

    /// 测量文字宽度（物理像素）。失败时退回按字符数估算，保证布局仍能继续。
    pub fn measure_text(&mut self, text: &str, px: f32) -> f32 {
        if self.text.is_none() {
            match TextEngine::new() {
                Ok(engine) => self.text = Some(engine),
                Err(e) => {
                    eprintln!("VimGlide: {e}");
                    return rough_width(text, px);
                }
            }
        }
        match self.text.as_mut().and_then(|engine| engine.measure(text, px).ok()) {
            Some(width) => width,
            None => rough_width(text, px),
        }
    }

    /// 用 Direct2D 绘制一帧并提交给 DirectComposition。
    pub fn present(&mut self, scene: &Scene) -> Result<(), String> {
        let width = scene.width.max(1);
        let height = scene.height.max(1);
        if width as i32 != self.width || height as i32 != self.height {
            self.resize(width, height)?;
        }
        if self.text.is_none() {
            self.text = Some(TextEngine::new()?);
        }
        if self.gfx.is_none() {
            self.gfx = Some(Gfx::new(self.hwnd, width, height)?);
        }
        {
            let text = self.text.as_mut().expect("文本引擎已创建");
            let gfx = self.gfx.as_mut().expect("图形设备已创建");
            gfx.render(text, scene)?;
        }

        // SAFETY: 句柄与状态都由本结构持有。
        unsafe {
            if !self.visible {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                // 任务栏本身也是置顶窗口，当前台窗口是任务栏时，它会被排到置顶层最前面，
                // 而 ShowWindow 只会按窗口原有的 Z 序显示，遮罩就会被任务栏压住。
                // 先取消置顶再重新置顶，才能把窗口插到置顶层的最前面。
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_NOTOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                // 任务栏若是前台窗口，它作为「前台进程的置顶窗口」始终压在其他置顶窗口之上，
                // 仅调整 Z 序无法盖住它；此时临时抢占前台，让遮罩成为前台置顶窗口。
                if self.activate_pending {
                    self.activate_pending = false;
                    let foreground = GetForegroundWindow();
                    if !foreground.0.is_null() && foreground != self.hwnd && is_topmost(foreground) {
                        self.take_foreground(foreground);
                    }
                }
                self.visible = true;
            }
        }
        Ok(())
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        // 交换链尺寸与窗口绑定，直接释放后按新尺寸重建。
        self.gfx = None;
        self.width = width as i32;
        self.height = height as i32;
        // SAFETY: hwnd 有效，仅调整位置与尺寸。
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                self.origin.0,
                self.origin.1,
                self.width,
                self.height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
            .map_err(|e| format!("调整遮罩尺寸失败：{e}"))?;
        }
        Ok(())
    }

    pub fn hide(&mut self) {
        self.activate_pending = false;
        if self.visible {
            // SAFETY: hwnd 有效。
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
                if self.activated {
                    // 恢复「不激活」样式，再把前台还给原来的窗口。
                    let style = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
                    let _ = SetWindowLongPtrW(
                        self.hwnd,
                        GWL_EXSTYLE,
                        style | WS_EX_NOACTIVATE.0 as isize,
                    );
                    self.activated = false;
                    let restore = self.restore;
                    self.restore = HWND::default();
                    if !restore.0.is_null() {
                        force_foreground(restore);
                    }
                }
            }
            self.visible = false;
        }
        // 隐藏期间释放交换链、D3D/D2D 与 DirectWrite 对象，下次显示再重建。
        self.gfx = None;
        self.text = None;
    }
}

/// 是否带 `WS_EX_TOPMOST`。
///
/// # Safety
/// `hwnd` 必须是有效窗口句柄。
unsafe fn is_topmost(hwnd: HWND) -> bool {
    // SAFETY: 纯查询调用。
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize != 0 }
}

/// 把窗口设为前台。
///
/// 后台进程直接调用 `SetForegroundWindow` 通常会被系统拒绝，
/// 这里先临时附加到当前前台线程的输入队列，成功后再解除附加。
///
/// # Safety
/// `hwnd` 必须是有效窗口句柄。
unsafe fn force_foreground(hwnd: HWND) {
    // SAFETY: 所有调用都是标准的 Win32 前台切换流程。
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground == hwnd {
            return;
        }
        let foreground_thread = if foreground.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, None)
        };
        let current_thread = GetCurrentThreadId();
        if foreground_thread != 0 && foreground_thread != current_thread {
            if AttachThreadInput(current_thread, foreground_thread, true).as_bool() {
                let _ = SetForegroundWindow(hwnd);
                let _ = AttachThreadInput(current_thread, foreground_thread, false);
                return;
            }
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

/// 拿不到 DirectWrite 时的兜底宽度估算。
fn rough_width(text: &str, px: f32) -> f32 {
    text.chars().count() as f32 * px * 0.55
}

/// DirectWrite 工厂与按字号缓存的文本格式。
struct TextEngine {
    factory: IDWriteFactory,
    formats: Vec<((u32, bool), IDWriteTextFormat)>,
}

impl TextEngine {
    fn new() -> Result<Self, String> {
        // SAFETY: 创建进程内共享的 DirectWrite 工厂。
        let factory: IDWriteFactory =
            unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }
                .map_err(|e| format!("创建 DirectWrite 工厂失败：{e}"))?;
        Ok(Self {
            factory,
            formats: Vec::new(),
        })
    }

    fn format(&mut self, px: f32, cjk: bool) -> Result<IDWriteTextFormat, String> {
        let key = (px.to_bits(), cjk);
        if let Some((_, format)) = self.formats.iter().find(|(k, _)| *k == key) {
            return Ok(format.clone());
        }
        // 中文提示用雅黑（含中文字形），纯英文标签继续用 Segoe UI。
        let families: &[PCWSTR] = if cjk {
            &[w!("Microsoft YaHei UI"), w!("Microsoft YaHei"), w!("Segoe UI")]
        } else {
            &[w!("Segoe UI")]
        };
        let mut last_error = String::new();
        for family in families {
            // SAFETY: 工厂与字符串常量在调用期间有效。
            match unsafe {
                self.factory.CreateTextFormat(
                    *family,
                    None,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    px,
                    w!("zh-cn"),
                )
            } {
                Ok(format) => {
                    unsafe {
                        let _ = format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
                        let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
                        // 文字在给定的矩形内垂直居中，替代 CPU 渲染时的基线计算。
                        let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
                    }
                    self.formats.push((key, format.clone()));
                    return Ok(format);
                }
                Err(e) => last_error = e.to_string(),
            }
        }
        Err(format!("创建文本格式失败：{last_error}"))
    }

    fn layout(
        &mut self,
        text: &str,
        px: f32,
        width: f32,
        height: f32,
    ) -> Result<IDWriteTextLayout, String> {
        let cjk = !text.is_ascii();
        let format = self.format(px, cjk)?;
        let wide: Vec<u16> = text.encode_utf16().collect();
        // SAFETY: 缓冲区与格式对象在调用期间有效。
        unsafe { self.factory.CreateTextLayout(&wide, &format, width, height) }
            .map_err(|e| format!("创建文本布局失败：{e}"))
    }

    fn measure(&mut self, text: &str, px: f32) -> Result<f32, String> {
        if text.is_empty() {
            return Ok(0.0);
        }
        let layout = self.layout(text, px, MEASURE_LIMIT, MEASURE_LIMIT)?;
        Ok(text_width(&layout)?)
    }
}

/// 单行文字的宽度（含行尾空白）。
fn text_width(layout: &IDWriteTextLayout) -> Result<f32, String> {
    let mut metrics = DWRITE_TEXT_METRICS::default();
    // SAFETY: 布局对象有效。
    unsafe { layout.GetMetrics(&mut metrics) }.map_err(|e| format!("读取文本尺寸失败：{e}"))?;
    Ok(metrics.widthIncludingTrailingWhitespace.max(metrics.width))
}

/// DirectComposition + Direct2D 的绘制资源。
struct Gfx {
    /// 持有 D3D 设备，保证 D2D/DComp 的底层设备存活。
    _device: ID3D11Device,
    _d2d: ID2D1Device,
    context: ID2D1DeviceContext,
    swapchain: IDXGISwapChain1,
    comp: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    brush: ID2D1SolidColorBrush,
}

impl Gfx {
    fn new(hwnd: HWND, width: u32, height: u32) -> Result<Self, String> {
        let device = create_d3d_device()?;
        // SAFETY: 设备创建后，各接口都在本结构生命周期内有效。
        unsafe {
            let dxgi: IDXGIDevice = device
                .cast()
                .map_err(|e| format!("获取 IDXGIDevice 失败：{e}"))?;
            let d2d: ID2D1Device = D2D1CreateDevice(&dxgi, None)
                .map_err(|e| format!("创建 Direct2D 设备失败：{e}"))?;
            let context: ID2D1DeviceContext = d2d
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
                .map_err(|e| format!("创建 Direct2D 设备上下文失败：{e}"))?;
            // 1 DIP = 1 物理像素：布局坐标本身就是屏幕物理像素。
            context.SetDpi(96.0, 96.0);
            context.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);

            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))
                .map_err(|e| format!("创建 DXGI 工厂失败：{e}"))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                Flags: 0,
            };
            let swapchain = factory
                .CreateSwapChainForComposition(&device, &desc, None::<&IDXGIOutput>)
                .map_err(|e| format!("创建合成交换链失败：{e}"))?;

            let comp: IDCompositionDevice = DCompositionCreateDevice(&dxgi)
                .map_err(|e| format!("创建 DirectComposition 设备失败：{e}"))?;
            let target = comp
                .CreateTargetForHwnd(hwnd, true)
                .map_err(|e| format!("绑定 DirectComposition 目标失败：{e}"))?;
            let visual = comp
                .CreateVisual()
                .map_err(|e| format!("创建 DirectComposition 视觉对象失败：{e}"))?;
            visual
                .SetContent(&swapchain)
                .map_err(|e| format!("设置交换链内容失败：{e}"))?;
            target
                .SetRoot(&visual)
                .map_err(|e| format!("设置视觉根节点失败：{e}"))?;
            comp.Commit()
                .map_err(|e| format!("提交 DirectComposition 失败：{e}"))?;

            let brush = context
                .CreateSolidColorBrush(&D2D1_COLOR_F::default(), None)
                .map_err(|e| format!("创建画刷失败：{e}"))?;

            Ok(Self {
                _device: device,
                _d2d: d2d,
                context,
                swapchain,
                comp,
                _target: target,
                _visual: visual,
                brush,
            })
        }
    }

    fn render(&mut self, text: &mut TextEngine, scene: &Scene) -> Result<(), String> {
        // SAFETY: 所有接口都由本结构持有且在同一线程使用。
        unsafe {
            // 翻转模型的交换链要求在 Present 前释放对后台缓冲区的全部引用，
            // 所以每帧重新包装一次交换链表面，而不是长期缓存位图。
            let surface: IDXGISurface = self
                .swapchain
                .GetBuffer(0)
                .map_err(|e| format!("获取交换链缓冲区失败：{e}"))?;
            let properties = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: ManuallyDrop::new(None),
            };
            let bitmap: ID2D1Bitmap1 = self
                .context
                .CreateBitmapFromDxgiSurface(&surface, Some(&properties))
                .map_err(|e| format!("包装交换链缓冲区失败：{e}"))?;

            self.context.SetTarget(&bitmap);
            let draw = (|| -> Result<(), String> {
                self.context.BeginDraw();
                self.context.Clear(Some(&D2D1_COLOR_F {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.0,
                }));
                for op in scene.ops() {
                    self.draw_op(text, op)?;
                }
                self.context
                    .EndDraw(None, None)
                    .map_err(|e| format!("结束绘制失败：{e}"))
            })();
            self.context.SetTarget(None::<&ID2D1Image>);
            drop(bitmap);
            drop(surface);
            draw?;

            self.swapchain
                .Present(1, DXGI_PRESENT(0))
                .ok()
                .map_err(|e| format!("提交帧失败：{e}"))?;
            self.comp
                .Commit()
                .map_err(|e| format!("提交 DirectComposition 失败：{e}"))?;
        }
        Ok(())
    }

    fn draw_op(&self, text: &mut TextEngine, op: &Op) -> Result<(), String> {
        match op {
            Op::FillRoundRect {
                rect,
                radius,
                color,
            } => {
                self.set_color(*color);
                let rounded = rounded_rect(*rect, *radius);
                // SAFETY: 上下文处于 BeginDraw/EndDraw 之间。
                unsafe { self.context.FillRoundedRectangle(&rounded, &self.brush) };
            }
            Op::StrokeRoundRect {
                rect,
                radius,
                thickness,
                color,
            } => {
                self.set_color(*color);
                let rounded = rounded_rect(*rect, *radius);
                // SAFETY: 同上。
                unsafe {
                    self.context.DrawRoundedRectangle(
                        &rounded,
                        &self.brush,
                        *thickness,
                        None::<&ID2D1StrokeStyle>,
                    )
                };
            }
            Op::Text(block) => self.draw_text(text, block)?,
        }
        Ok(())
    }

    fn draw_text(&self, text: &mut TextEngine, block: &TextBlock) -> Result<(), String> {
        let mut pen = block.x;
        let halo_radius = (block.px / 14.0).round().clamp(1.0, 2.0) as i32;
        for run in &block.runs {
            if run.text.is_empty() {
                continue;
            }
            let available = (block.x + block.w - pen).max(1.0);
            let layout = text.layout(&run.text, block.px, available, block.h)?;
            let width = text_width(&layout)?;
            let origin = Vector2::new(pen, block.y);
            if let Some(halo) = run.halo {
                // 与 CPU 渲染一致：在周围一圈偏移处重复绘制描边色。
                self.set_color(halo);
                for dy in -halo_radius..=halo_radius {
                    for dx in -halo_radius..=halo_radius {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        // SAFETY: 上下文处于绘制状态，布局对象有效。
                        unsafe {
                            self.context.DrawTextLayout(
                                Vector2::new(origin.X + dx as f32, origin.Y + dy as f32),
                                &layout,
                                &self.brush,
                                D2D1_DRAW_TEXT_OPTIONS_NONE,
                            )
                        };
                    }
                }
            }
            self.set_color(run.color);
            // SAFETY: 同上。
            unsafe {
                self.context.DrawTextLayout(
                    origin,
                    &layout,
                    &self.brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                )
            };
            pen += width;
        }
        Ok(())
    }

    fn set_color(&self, color: Color) {
        let value = D2D1_COLOR_F {
            r: color.r as f32 / 255.0,
            g: color.g as f32 / 255.0,
            b: color.b as f32 / 255.0,
            a: color.a as f32 / 255.0,
        };
        // SAFETY: 画刷由本结构持有。
        unsafe { self.brush.SetColor(&value) };
    }
}

/// 创建带 BGRA 支持的 D3D11 设备；硬件失败时退回 WARP。
fn create_d3d_device() -> Result<ID3D11Device, String> {
    let levels = [D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_0];
    let mut last_error = String::new();
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device: Option<ID3D11Device> = None;
        // SAFETY: 无适配器（默认）、无软件模块、无立即上下文。
        let result = unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        };
        match (result, device) {
            (Ok(()), Some(device)) => return Ok(device),
            (Ok(()), None) => last_error = "D3D11CreateDevice 未返回设备".to_string(),
            (Err(e), _) => last_error = format!("创建 D3D11 设备失败：{e}"),
        }
    }
    Err(last_error)
}

fn rounded_rect(rect: Rect, radius: f32) -> D2D1_ROUNDED_RECT {
    let radius = radius
        .max(0.0)
        .min(rect.w as f32 / 2.0)
        .min(rect.h as f32 / 2.0);
    D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left: rect.x as f32,
            top: rect.y as f32,
            right: rect.right() as f32,
            bottom: rect.bottom() as f32,
        },
        radiusX: radius,
        radiusY: radius,
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // SAFETY: 释放本结构创建的资源。
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
