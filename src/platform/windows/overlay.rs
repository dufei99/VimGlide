//! 覆盖全屏的透明遮罩窗口（分层窗口 + DIB），用于绘制提示标签。

use std::ffi::c_void;
use std::ptr;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, GetLastError, HWND, POINT, SIZE, ERROR_CLASS_ALREADY_EXISTS};
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, HBITMAP, HDC, HGDIOBJ, SelectObject,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, HWND_TOPMOST, RegisterClassExW, SW_HIDE, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos, ShowWindow, ULW_ALPHA, UpdateLayeredWindow,
    WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::model::Rect;
use crate::render::Canvas;

const CLASS_NAME: PCWSTR = w!("VimGlide_overlay");

pub struct Overlay {
    hwnd: HWND,
    mem_dc: HDC,
    dib: HBITMAP,
    bits: *mut u8,
    width: i32,
    height: i32,
    origin: (i32, i32),
    visible: bool,
}

impl Overlay {
    pub fn new(screen: Rect) -> Result<Self, String> {
        unsafe {
            let instance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
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

            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
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

            let mem_dc = CreateCompatibleDC(None);
            if mem_dc.is_invalid() {
                return Err("创建内存 DC 失败".to_string());
            }

            let mut overlay = Self {
                hwnd,
                mem_dc,
                dib: HBITMAP::default(),
                bits: ptr::null_mut(),
                width: screen.w,
                height: screen.h,
                origin: (screen.x, screen.y),
                visible: false,
            };
            overlay.create_surface(screen.w, screen.h)?;
            Ok(overlay)
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// 遮罩层所在显示器的缩放系数。
    pub fn dpi_scale(&self) -> f32 {
        // SAFETY: hwnd 有效。
        let dpi = unsafe { GetDpiForWindow(self.hwnd) };
        if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
    }

    fn create_surface(&mut self, width: i32, height: i32) -> Result<(), String> {
        // SAFETY: 所有 GDI 对象都由本结构持有并在 Drop 中释放。
        unsafe {
            if !self.dib.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.dib.0));
                self.dib = HBITMAP::default();
                self.bits = ptr::null_mut();
            }

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    // 负高度 = 自上而下的位图，便于按行拷贝。
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0, // BI_RGB
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits: *mut c_void = ptr::null_mut();
            let dib = CreateDIBSection(Some(self.mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
                .map_err(|e| format!("创建 DIB 失败：{e}"))?;
            SelectObject(self.mem_dc, HGDIOBJ(dib.0));

            self.dib = dib;
            self.bits = bits as *mut u8;
            self.width = width;
            self.height = height;
            Ok(())
        }
    }

    /// 把画布（RGBA 直通 alpha）转换成 Windows 需要的预乘 BGRA 并上屏。
    pub fn present(&mut self, canvas: &Canvas) -> Result<(), String> {
        if self.dib.is_invalid()
            || canvas.width() as i32 != self.width
            || canvas.height() as i32 != self.height
        {
            self.create_surface(canvas.width() as i32, canvas.height() as i32)?;
        }

        let src = canvas.data();
        let dst = unsafe { std::slice::from_raw_parts_mut(self.bits, src.len()) };
        for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
            let alpha = s[3] as u32;
            // 预乘 alpha：channel * a / 255
            d[0] = ((s[2] as u32 * alpha + 127) / 255) as u8;
            d[1] = ((s[1] as u32 * alpha + 127) / 255) as u8;
            d[2] = ((s[0] as u32 * alpha + 127) / 255) as u8;
            d[3] = alpha as u8;
        }

        let dst_point = POINT {
            x: self.origin.0,
            y: self.origin.1,
        };
        let size = SIZE {
            cx: self.width,
            cy: self.height,
        };
        let src_point = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: 0,     // AC_SRC_OVER
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1, // AC_SRC_ALPHA
        };

        // SAFETY: 句柄与位图均在本结构生命周期内有效。
        unsafe {
            UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&dst_point),
                Some(&size),
                Some(self.mem_dc),
                Some(&src_point),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
            .map_err(|e| format!("更新遮罩层失败：{e}"))?;

            if !self.visible {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                self.visible = true;
            }
        }
        Ok(())
    }

    pub fn hide(&mut self) {
        if self.visible {
            // SAFETY: hwnd 有效。
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.visible = false;
        }
        // 隐藏期间释放整屏 DIB（约 8MB/1080p），下次显示再按需重建。
        if !self.dib.is_invalid() {
            // SAFETY: 位图由本结构创建，此处是唯一释放点之一（另一个在 Drop）。
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.dib.0));
            }
            self.dib = HBITMAP::default();
            self.bits = ptr::null_mut();
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // SAFETY: 释放本结构创建的资源。
        unsafe {
            if !self.dib.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.dib.0));
            }
            let _ = DeleteDC(self.mem_dc);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
