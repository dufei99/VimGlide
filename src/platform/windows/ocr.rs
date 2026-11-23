//! 用 Windows 系统 OCR（`Windows.Media.Ocr`）识别窗口客户区里的文字。
//!
//! UIA 采不到元素时（例如未加 `--force-renderer-accessibility` 的 Chromium），
//! 按识别出的文字行给出可点击目标，作为兜底。

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BitBlt, ClientToScreen, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HALFTONE, HBITMAP, HDC, HGDIOBJ,
    ReleaseDC, SRCCOPY, SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt,
};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};
use windows::core::HSTRING;
use windows_future::AsyncStatus;

use crate::model::Rect;

/// 识别操作的等待上限。
const TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
/// 单次识别返回的行数硬上限，避免极端画面拖慢标签生成。
const MAX_LINES: usize = 1000;
/// 用户配置语言不可用时依次尝试的识别语言。
const FALLBACK_LANGUAGES: [&str; 4] = ["zh-Hans-CN", "en-US", "zh-Hant-TW", "ja-JP"];

/// 一行识别结果：屏幕坐标矩形与行文本。
pub struct Line {
    pub rect: Rect,
    pub text: String,
}

/// 识别指定窗口客户区里的文字行。
pub fn scan_window(hwnd: HWND) -> Result<Vec<Line>, String> {
    if hwnd.0.is_null() {
        return Err("没有可扫描的窗口".to_string());
    }
    init_winrt()?;
    let max_dimension =
        OcrEngine::MaxImageDimension().map_err(|e| format!("读取 OCR 图像尺寸上限失败：{e}"))?;
    let image = capture_client(hwnd, max_dimension)?;
    if image.pixels.is_empty() {
        return Ok(Vec::new());
    }

    let engine = create_engine()?;
    let bitmap = software_bitmap(&image)?;
    let operation = engine
        .RecognizeAsync(&bitmap)
        .map_err(|e| format!("启动 OCR 识别失败：{e}"))?;

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let status = operation
            .Status()
            .map_err(|e| format!("读取 OCR 状态失败：{e}"))?;
        if status == AsyncStatus::Completed {
            break;
        }
        if status == AsyncStatus::Error || status == AsyncStatus::Canceled {
            return Err("OCR 识别失败（状态异常）".to_string());
        }
        if Instant::now() >= deadline {
            return Err("OCR 识别超时".to_string());
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    let result = operation
        .GetResults()
        .map_err(|e| format!("获取 OCR 结果失败：{e}"))?;
    let lines = result
        .Lines()
        .map_err(|e| format!("读取 OCR 文字行失败：{e}"))?;
    let count = lines
        .Size()
        .map_err(|e| format!("读取 OCR 文字行数量失败：{e}"))?;

    let scale = image.scale.max(f32::EPSILON);
    let inverse = 1.0 / scale;
    let screen = virtual_screen();
    let mut output = Vec::new();

    for index in 0..count {
        let Ok(line) = lines.GetAt(index) else {
            continue;
        };
        let Ok(words) = line.Words() else {
            continue;
        };
        let Ok(word_count) = words.Size() else {
            continue;
        };

        let mut text: Vec<String> = Vec::new();
        let (mut left, mut top) = (f32::MAX, f32::MAX);
        let (mut right, mut bottom) = (f32::MIN, f32::MIN);

        for word_index in 0..word_count {
            let Ok(word) = words.GetAt(word_index) else {
                continue;
            };
            if let Ok(value) = word.Text() {
                let value = value.to_string();
                if !value.trim().is_empty() {
                    text.push(value);
                }
            }
            let Ok(bounds) = word.BoundingRect() else {
                continue;
            };
            if bounds.Width <= 0.0 || bounds.Height <= 0.0 {
                continue;
            }
            left = left.min(bounds.X);
            top = top.min(bounds.Y);
            right = right.max(bounds.X + bounds.Width);
            bottom = bottom.max(bounds.Y + bounds.Height);
        }

        let text = text.join(" ").trim().to_string();
        if text.is_empty() || right <= left || bottom <= top {
            continue;
        }

        // 识别图像可能被等比缩小过，换算回屏幕物理像素。
        let rect = Rect::new(
            image.origin.0 + (left * inverse).floor() as i32,
            image.origin.1 + (top * inverse).floor() as i32,
            ((right - left) * inverse).ceil() as i32,
            ((bottom - top) * inverse).ceil() as i32,
        );
        if rect.w <= 0 || rect.h <= 0 {
            continue;
        }
        let Some(visible) = rect.intersection(&screen) else {
            continue;
        };
        output.push(Line {
            rect: visible,
            text,
        });
        if output.len() >= MAX_LINES {
            break;
        }
    }

    Ok(output)
}

/// 保证线程已初始化 WinRT。
///
/// 消息循环线程已经初始化成 STA，此时再以 MTA 初始化会返回
/// `RPC_E_CHANGED_MODE`；STA 同样可以调用 WinRT，忽略即可。
fn init_winrt() -> Result<(), String> {
    // SAFETY: 只在调用线程做初始化，失败码由返回值判断。
    match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
        Ok(()) => Ok(()),
        Err(e) if e.code().0 as u32 == 0x8001_0106 => Ok(()),
        Err(e) => Err(format!("初始化 WinRT 失败：{e}")),
    }
}

/// 按用户配置的语言创建 OCR 引擎，失败时依次尝试常见语言。
fn create_engine() -> Result<OcrEngine, String> {
    if let Ok(engine) = OcrEngine::TryCreateFromUserProfileLanguages() {
        return Ok(engine);
    }

    let mut failures = Vec::new();
    for tag in FALLBACK_LANGUAGES {
        let tag = HSTRING::from(tag);
        match Language::CreateLanguage(&tag) {
            Ok(language) => match OcrEngine::TryCreateFromLanguage(&language) {
                Ok(engine) => return Ok(engine),
                Err(e) => failures.push(format!("{tag}: {e}")),
            },
            Err(e) => failures.push(format!("{tag}: {e}")),
        }
    }
    Err(format!(
        "系统 OCR 不可用（可能未安装识别语言）：{}",
        failures.join("；")
    ))
}

/// 抓取窗口客户区（只保留落在虚拟屏幕内的部分），必要时等比缩小。
struct Image {
    width: i32,
    height: i32,
    /// 图像像素 / 屏幕像素。
    scale: f32,
    /// 图像左上角对应的屏幕坐标。
    origin: (i32, i32),
    /// BGRA，自上而下。
    pixels: Vec<u8>,
}

fn capture_client(hwnd: HWND, max_dimension: u32) -> Result<Image, String> {
    let mut client = RECT::default();
    // SAFETY: hwnd 由调用方保证有效。
    unsafe { GetClientRect(hwnd, &mut client) }
        .map_err(|e| format!("读取窗口客户区失败：{e}"))?;

    let mut origin = POINT::default();
    // SAFETY: 同上。
    if !unsafe { ClientToScreen(hwnd, &mut origin) }.as_bool() {
        return Err("换算客户区屏幕坐标失败".to_string());
    }

    let full = Rect::new(
        origin.x + client.left,
        origin.y + client.top,
        client.right - client.left,
        client.bottom - client.top,
    );
    let screen = virtual_screen();
    let Some(visible) = full.intersection(&screen) else {
        return Ok(empty_image());
    };
    if visible.w < 2 || visible.h < 2 {
        return Ok(empty_image());
    }

    let source_dc = GetScreenDc::new()?;
    let source = Capture::new(visible.w, visible.h)?;
    let src_x = visible.x - full.x;
    let src_y = visible.y - full.y;
    // SAFETY: 两个 DC 与 DIB 在调用期间有效。
    unsafe {
        BitBlt(
            source.dc,
            0,
            0,
            visible.w,
            visible.h,
            Some(source_dc.0),
            src_x,
            src_y,
            SRCCOPY,
        )
    }
    .map_err(|e| format!("抓取窗口画面失败：{e}"))?;

    let max_dimension = max_dimension.max(1) as i32;
    if visible.w.max(visible.h) <= max_dimension {
        let pixels = source.pixels();
        return Ok(Image {
            width: visible.w,
            height: visible.h,
            scale: 1.0,
            origin: (visible.x, visible.y),
            pixels,
        });
    }

    let factor = max_dimension as f32 / visible.w.max(visible.h) as f32;
    let width = ((visible.w as f32 * factor).floor() as i32).max(1);
    let height = ((visible.h as f32 * factor).floor() as i32).max(1);
    let scaled = Capture::new(width, height)?;
    // SAFETY: 两个 DC 与 DIB 在调用期间有效。
    let ok = unsafe {
        SetStretchBltMode(scaled.dc, HALFTONE);
        let _ = SetBrushOrgEx(scaled.dc, 0, 0, None);
        StretchBlt(
            scaled.dc,
            0,
            0,
            width,
            height,
            Some(source.dc),
            0,
            0,
            visible.w,
            visible.h,
            SRCCOPY,
        )
    };
    if !ok.as_bool() {
        return Err("缩放窗口画面失败".to_string());
    }

    Ok(Image {
        width,
        height,
        scale: width as f32 / visible.w as f32,
        origin: (visible.x, visible.y),
        pixels: scaled.pixels(),
    })
}

fn empty_image() -> Image {
    Image {
        width: 0,
        height: 0,
        scale: 1.0,
        origin: (0, 0),
        pixels: Vec::new(),
    }
}

fn software_bitmap(image: &Image) -> Result<SoftwareBitmap, String> {
    let writer = DataWriter::new().map_err(|e| format!("创建 OCR 缓冲区失败：{e}"))?;
    writer
        .WriteBytes(&image.pixels)
        .map_err(|e| format!("写入 OCR 缓冲区失败：{e}"))?;
    let buffer = writer
        .DetachBuffer()
        .map_err(|e| format!("取出 OCR 缓冲区失败：{e}"))?;
    SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        image.width,
        image.height,
    )
    .map_err(|e| format!("创建 OCR 位图失败：{e}"))
}

/// 屏幕 DC，离开作用域时释放。
struct GetScreenDc(HDC);

impl GetScreenDc {
    fn new() -> Result<Self, String> {
        // SAFETY: 纯查询调用；失败时返回空句柄。
        let dc = unsafe { windows::Win32::Graphics::Gdi::GetDC(None) };
        if dc.0.is_null() {
            return Err("获取屏幕 DC 失败".to_string());
        }
        Ok(Self(dc))
    }
}

impl Drop for GetScreenDc {
    fn drop(&mut self) {
        // SAFETY: 句柄由本结构创建。
        unsafe {
            let _ = ReleaseDC(None, self.0);
        }
    }
}

/// 32 位自上而下的 DIB 及其内存 DC，离开作用域时释放 GDI 资源。
struct Capture {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
}

impl Capture {
    fn new(width: i32, height: i32) -> Result<Self, String> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            bmiColors: Default::default(),
        };

        // SAFETY: 所有句柄都由本函数创建，失败路径上自行清理。
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.0.is_null() {
                return Err("创建内存 DC 失败".to_string());
            }
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap =
                match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                    Ok(bitmap) => bitmap,
                    Err(e) => {
                        let _ = DeleteDC(dc);
                        return Err(format!("创建 DIB 失败：{e}"));
                    }
                };
            if bits.is_null() {
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
                let _ = DeleteDC(dc);
                return Err("DIB 像素缓冲区为空".to_string());
            }
            let previous = SelectObject(dc, HGDIOBJ(bitmap.0));
            Ok(Self {
                dc,
                bitmap,
                previous,
                bits: bits.cast(),
                width,
                height,
            })
        }
    }

    /// 复制像素并把 GDI 未使用的 alpha 通道补成不透明。
    fn pixels(&self) -> Vec<u8> {
        let len = (self.width as usize) * (self.height as usize) * 4;
        // SAFETY: DIB 至少分配了 width * height * 4 字节。
        let mut pixels = unsafe { std::slice::from_raw_parts(self.bits, len) }.to_vec();
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        pixels
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: 句柄由本结构创建。
        unsafe {
            if !self.previous.0.is_null() {
                SelectObject(self.dc, self.previous);
            }
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

fn virtual_screen() -> Rect {
    // SAFETY: 纯查询调用。
    unsafe {
        Rect::new(
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}
