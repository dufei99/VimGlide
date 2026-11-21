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
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GWL_EXSTYLE, GetForegroundWindow, GetWindowLongPtrW,
    GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOPMOST, RegisterClassExW, SW_HIDE,
    SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, ULW_ALPHA, UpdateLayeredWindow, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
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
    /// 是否为了让遮罩压住任务栏而临时抢占了前台。
    activated: bool,
    /// 下一次显示时是否允许抢占前台（仅提示模式，启动提示不抢）。
    activate_pending: bool,
    /// 抢占前的前台窗口，退出提示模式时恢复。
    restore: HWND,
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
                activated: false,
                activate_pending: false,
                restore: HWND::default(),
            };
            overlay.create_surface(screen.w, screen.h)?;
            Ok(overlay)
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

/// 窗口是否带 `WS_EX_TOPMOST`。
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
