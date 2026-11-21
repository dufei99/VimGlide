//! 系统托盘图标与右键菜单。

use std::path::{Path, PathBuf};
use std::ptr;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, POINT, TRUE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DestroyIcon, DestroyMenu,
    DestroyWindow, GetCursorPos, HICON, ICONINFO, MB_ICONERROR, MB_ICONINFORMATION, MB_OK,
    MF_SEPARATOR, MF_STRING, MessageBoxW, PostMessageW, RegisterClassExW, SW_SHOWNORMAL,
    SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_LBUTTONUP, WM_NULL,
    WM_RBUTTONUP, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::model::Rect;
use crate::render::{Canvas, Color, TextRenderer};

const ICON_ID: u32 = 1;

/// 托盘图标与右键菜单的宿主窗口类。
///
/// 遮罩层带 `WS_EX_NOACTIVATE`，不能被设为前台窗口；而弹出菜单需要前台窗口
/// 才能在点击别处时正常关闭、并响应键盘导航。所以这里额外创建一个隐藏的
/// 普通窗口专门负责托盘消息和菜单。
const OWNER_CLASS: PCWSTR = w!("VimGlide_tray_owner");
const OWNER_TITLE: PCWSTR = w!("VimGlide tray");

const CMD_TOGGLE: usize = 1;
const CMD_CONFIG: usize = 2;
const CMD_HELP: usize = 3;
const CMD_QUIT: usize = 4;
const CMD_ELEVATE: usize = 5;
const CMD_RELOAD: usize = 6;

/// 托盘交互产生的动作，由消息循环执行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    None,
    Toggle,
    OpenConfig,
    Reload,
    Help,
    Elevate,
    Quit,
}

pub struct Tray {
    /// 隐藏的宿主窗口：接收托盘回调、作为菜单的前台窗口。
    hwnd: HWND,
    icon: HICON,
    config_path: PathBuf,
    hint_hotkey: String,
    quit_hotkey: String,
    /// 本进程是否已经是管理员；是的话菜单里不再显示“以管理员身份重启”。
    elevated: bool,
    added: bool,
}

impl Tray {
    pub fn new(
        config_path: &Path,
        hint_hotkey: &str,
        quit_hotkey: &str,
        elevated: bool,
    ) -> Result<Self, String> {
        let hwnd = create_owner_window()?;
        let icon = match create_icon() {
            Some(icon) => icon,
            None => {
                // SAFETY: 窗口由本函数创建。
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err("创建托盘图标失败".to_string());
            }
        };
        let mut tray = Self {
            hwnd,
            icon,
            config_path: config_path.to_path_buf(),
            hint_hotkey: hint_hotkey.to_string(),
            quit_hotkey: quit_hotkey.to_string(),
            elevated,
            added: false,
        };
        if let Err(e) = tray.add() {
            tray.drop_icon();
            // SAFETY: 窗口由本函数创建。
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return Err(e);
        }
        Ok(tray)
    }

    fn notify_data(&self) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: super::WM_VIMGLIDE_TRAY,
            hIcon: self.icon,
            ..Default::default()
        };
        let tip: Vec<u16> = format!("VimGlide - {}", self.hint_hotkey)
            .encode_utf16()
            .take(data.szTip.len() - 1)
            .collect();
        data.szTip[..tip.len()].copy_from_slice(&tip);
        data
    }

    fn add(&mut self) -> Result<(), String> {
        let data = self.notify_data();
        // SAFETY: 结构体大小与句柄均有效。
        if unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
            self.added = true;
            Ok(())
        } else {
            Err("向系统托盘添加图标失败".to_string())
        }
    }

    /// 处理托盘图标上的鼠标消息。
    pub fn on_mouse(&mut self, lparam: LPARAM) -> TrayAction {
        match lparam.0 as u32 {
            WM_LBUTTONUP => TrayAction::Help,
            WM_RBUTTONUP => self.show_menu(),
            _ => TrayAction::None,
        }
    }

    /// 配置重新载入后刷新悬停提示与菜单里的热键文字。
    pub fn update_hotkeys(&mut self, hint_hotkey: &str, quit_hotkey: &str) {
        self.hint_hotkey = hint_hotkey.to_string();
        self.quit_hotkey = quit_hotkey.to_string();
        let data = self.notify_data();
        // SAFETY: 结构体大小与句柄均有效。
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }

    fn show_menu(&mut self) -> TrayAction {        let toggle = wide(&format!("显示提示 ({})", self.hint_hotkey));
        let open = wide("打开配置文件");
        let reload = wide("重新载入配置");
        let help = wide("帮助");
        let elevate = wide("以管理员身份重启");
        let quit = wide(&format!("退出 VimGlide ({})", self.quit_hotkey));

        // SAFETY: 菜单与窗口句柄都在本函数内有效。
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return TrayAction::None;
            };
            let _ = AppendMenuW(menu, MF_STRING, CMD_TOGGLE, PCWSTR(toggle.as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, CMD_CONFIG, PCWSTR(open.as_ptr()));
            let _ = AppendMenuW(menu, MF_STRING, CMD_RELOAD, PCWSTR(reload.as_ptr()));
            let _ = AppendMenuW(menu, MF_STRING, CMD_HELP, PCWSTR(help.as_ptr()));
            if !self.elevated {
                let _ = AppendMenuW(menu, MF_STRING, CMD_ELEVATE, PCWSTR(elevate.as_ptr()));
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT, PCWSTR(quit.as_ptr()));

            let mut point = POINT::default();
            let _ = GetCursorPos(&mut point);
            // 不设置前台窗口时，点击菜单外部无法关闭菜单。
            let _ = SetForegroundWindow(self.hwnd);
            let command = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                None,
                self.hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
            let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));

            match command.0 as usize {
                CMD_TOGGLE => TrayAction::Toggle,
                CMD_CONFIG => TrayAction::OpenConfig,
                CMD_RELOAD => TrayAction::Reload,
                CMD_HELP => TrayAction::Help,
                CMD_ELEVATE => TrayAction::Elevate,
                CMD_QUIT => TrayAction::Quit,
                _ => TrayAction::None,
            }
        }
    }

    /// 用系统默认程序打开配置文件。
    pub fn open_config(&self) -> Result<(), String> {
        let verb = wide("open");
        let path = wide(&self.config_path.to_string_lossy());
        // SAFETY: 字符串缓冲区在调用期间有效。
        let result = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(verb.as_ptr()),
                PCWSTR(path.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize <= 32 {
            Err(format!("无法打开配置文件 {}", self.config_path.display()))
        } else {
            Ok(())
        }
    }

    /// 用 UAC 以管理员身份重新启动自己（保留当前配置文件路径）。
    pub fn relaunch_elevated(&self) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| format!("获取自身路径失败：{e}"))?;
        let exe = wide(&exe.to_string_lossy());
        let arguments = wide(&format!("--config \"{}\"", self.config_path.display()));
        let verb = wide("runas");
        // SAFETY: 字符串缓冲区在调用期间有效。
        let result = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(verb.as_ptr()),
                PCWSTR(exe.as_ptr()),
                PCWSTR(arguments.as_ptr()),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize <= 32 {
            Err("提权启动失败（可能取消了 UAC 提示）".to_string())
        } else {
            Ok(())
        }
    }

    /// 弹出错误提示（发行版没有控制台）。
    pub fn show_error(&self, message: &str) {
        let text = wide(message);
        let caption = wide("VimGlide");
        // SAFETY: 文本缓冲区在调用期间有效。
        unsafe {
            MessageBoxW(
                None,
                PCWSTR(text.as_ptr()),
                PCWSTR(caption.as_ptr()),
                MB_OK | MB_ICONERROR,
            );
        }
    }

    /// 显示按键说明。
    pub fn show_help(&self) {
        let text = format!(
            "VimGlide - 系统级 Vim 提示导航\n\n\
             {0}      显示提示标签\n\
             输入标签            点击对应元素（标签来自元素名称首字母）\n\
             Backspace           回退一位\n\
             Esc                 退出提示模式\n\
             方向键 / PageUp/Down / Home/End   滚动\n\
             j k h l d u g G     滚动（缓冲区为空且无匹配标签时生效）\n\
             托盘左键            打开帮助\n\
             托盘右键            这个菜单\n\n\
             热键被占用时会自动退到 ctrl+alt+space 或 alt+shift+space。\n\
             配置文件：{1}\n\
             修改配置后可从托盘菜单选择「重新载入配置」生效。",
            self.hint_hotkey,
            self.config_path.display()
        );
        let text = wide(&text);
        let caption = wide("VimGlide 帮助");
        // SAFETY: 文本缓冲区在调用期间有效。
        unsafe {
            MessageBoxW(
                None,
                PCWSTR(text.as_ptr()),
                PCWSTR(caption.as_ptr()),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.drop_icon();
        // SAFETY: 窗口由本结构创建且只销毁一次。
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

impl Tray {
    /// 从托盘移除图标并释放图标句柄。
    fn drop_icon(&mut self) {
        // SAFETY: 句柄由本结构创建且只释放一次。
        unsafe {
            if self.added {
                let data = self.notify_data();
                let _ = Shell_NotifyIconW(NIM_DELETE, &data);
                self.added = false;
            }
            let _ = DestroyIcon(self.icon);
            self.icon = HICON(ptr::null_mut());
        }
    }
}

/// 创建托盘消息用的隐藏窗口。
fn create_owner_window() -> Result<HWND, String> {
    // SAFETY: 窗口类与窗口都是本进程创建，句柄由 Tray 持有到退出。
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|e| format!("获取模块句柄失败：{e}"))?;
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(super::wnd_proc),
            hInstance: instance.into(),
            lpszClassName: OWNER_CLASS,
            ..Default::default()
        };
        if RegisterClassExW(&class) == 0 {
            let err = GetLastError();
            if err != ERROR_CLASS_ALREADY_EXISTS {
                return Err(format!("注册托盘窗口类失败：{err:?}"));
            }
        }
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            OWNER_CLASS,
            OWNER_TITLE,
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| format!("创建托盘窗口失败：{e}"))
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 读取 Segoe UI Symbol，它包含键盘符号 ⌨（U+2328）。
fn load_symbol_font() -> Option<Vec<u8>> {
    let windir = std::env::var_os("WINDIR")?;
    let path = PathBuf::from(windir).join("Fonts").join("seguisym.ttf");
    std::fs::read(path).ok()
}

/// 画一个 32x32 的托盘图标：黄色圆角方块 + 键盘符号 ⌨（U+2328）。
fn icon_canvas() -> Canvas {
    const SIZE: i32 = 32;
    let mut canvas = Canvas::new(SIZE as u32, SIZE as u32);
    canvas.fill_round_rect(
        Rect::new(1, 1, SIZE - 2, SIZE - 2),
        7.0,
        Color::rgb(253, 246, 178),
    );
    let dark = Color::rgb(32, 32, 32);

    let mut drawn = false;
    if let Some(text) = load_symbol_font().and_then(TextRenderer::new) {
        if text.has_glyph('⌨') {
            let px = 28.0;
            let width = text.measure("⌨", px);
            let (_, ymin, _, height) = text.glyph_bounds('⌨', px);
            // 让字形外框的垂直中心落在图标中心。
            let baseline = SIZE as f32 / 2.0 + ymin as f32 + height as f32 / 2.0;
            text.draw(
                &mut canvas,
                "⌨",
                (SIZE as f32 - width) / 2.0,
                baseline,
                px,
                dark,
            );
            drawn = true;
        }
    }
    if !drawn {
        // 没有符号字体时画一个简化键盘，保证图标永远可用。
        canvas.stroke_round_rect(Rect::new(5, 8, 22, 16), 2.0, 2.0, dark);
        for row in 0..2 {
            for col in 0..5 {
                canvas.fill_round_rect(Rect::new(9 + col * 3, 12 + row * 3, 2, 2), 0.5, dark);
            }
        }
        canvas.fill_round_rect(Rect::new(11, 20, 10, 2), 1.0, dark);
    }
    canvas
}

/// 把画布转成托盘图标句柄。
fn create_icon() -> Option<HICON> {
    const SIZE: i32 = 32;
    let canvas = icon_canvas();

    // SAFETY: 位图与图标句柄都在本函数内创建，复制后立即释放临时位图。
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: SIZE,
                biHeight: -SIZE,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };
        let dc = CreateCompatibleDC(None);
        let mut bits: *mut std::ffi::c_void = ptr::null_mut();
        let dib = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (SIZE * SIZE * 4) as usize);
        for (source, target) in canvas.data().chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
            target[0] = source[2];
            target[1] = source[1];
            target[2] = source[0];
            target[3] = source[3];
        }

        // 32 位图标使用 alpha 通道，掩码保持全 0（不透明）即可。
        let mask = CreateBitmap(SIZE, SIZE, 1, 1, None);
        let info = ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: dib,
        };
        let icon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
        let _ = DeleteDC(dc);
        icon
    }
}
