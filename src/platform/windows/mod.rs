//! Windows 后端：全局热键 + 键盘拦截 + UIA 采集 + 分层窗口遮罩 + 鼠标注入。

mod hotkey;
mod input;
mod overlay;
mod tray;
mod uia;

use std::cell::Cell;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME,
    VK_INSERT, VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_OEM_1, VK_OEM_2, VK_OEM_3, VK_OEM_4,
    VK_OEM_5, VK_OEM_6, VK_OEM_7, VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD, VK_OEM_PLUS,
    VK_PRIOR, VK_RETURN, VK_RIGHT, VK_RWIN, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DefWindowProcW, DispatchMessageW, GetForegroundWindow, GetMessageW,
    GetSystemMetrics, GetWindowThreadProcessId, KBDLLHOOKSTRUCT, KillTimer, MSG,
    MB_ICONINFORMATION, MB_OK, MessageBoxW, PostMessageW, PostQuitMessage, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetTimer, SetWindowsHookExW,
    TIMERPROC, TranslateMessage, WH_KEYBOARD_LL, WM_APP, WM_HOTKEY, WM_KEYDOWN, WM_SYSKEYDOWN,
    WM_TIMER,
};

use self::hotkey::{HookHandle, HotkeySpec, Hotkeys};
use self::overlay::Overlay;
use self::uia::UiaSource;
use crate::app::App;
use crate::config::Config;
use crate::model::{Rect, Target};
use crate::platform::{Backend, Desktop, Key};
use crate::render::Canvas;

const HOTKEY_HINT: i32 = 1;
const HOTKEY_QUIT: i32 = 2;
const TIMER_ID: usize = 1;

/// 自定义消息：把键盘钩子捕获到的按键转交给消息循环处理。
///
/// 钩子回调有约 300ms 的执行时限，超时会被系统跳过（按键丢失），
/// 所以钩子内只做判断和投递，真正的处理（UIA 扫描、重绘、点击）在消息循环里完成。
const WM_VIMGLIDE_KEY: u32 = WM_APP + 1;
/// 托盘图标的回调消息。
pub(super) const WM_VIMGLIDE_TRAY: u32 = WM_APP + 2;
const FLAG_SHIFT: isize = 1;
const FLAG_TOGGLE: isize = 2;

/// 是否处于「键盘只交给 VimGlide」的状态。
static CAPTURE: AtomicBool = AtomicBool::new(false);
/// 叠加层窗口句柄，钩子用它投递消息。
static OVERLAY_HWND: AtomicIsize = AtomicIsize::new(0);
/// 生效的提示热键，用于捕获期间再次触发退出。
static HINT_SPEC: OnceLock<HotkeySpec> = OnceLock::new();
/// 退出热键，捕获期间需要放行给系统处理。
static QUIT_SPEC: OnceLock<HotkeySpec> = OnceLock::new();

/// 消息循环中分发给 App 的运行时指针。
///
/// 整个程序只有一个线程、一个消息循环，窗口过程与键盘钩子都在该线程被调用，
/// 因此可以用裸指针把 `App` 与 `WindowsBackend` 借给回调。
/// `busy` 用于防止（例如 COM 内部泵消息导致的）重入。
#[derive(Clone, Copy)]
struct Runtime {
    app: *mut App,
    backend: *mut WindowsBackend,
    busy: bool,
}

thread_local! {
    static RUNTIME: Cell<Option<Runtime>> = const { Cell::new(None) };
}

fn with_runtime(f: impl FnOnce(&mut App, &mut WindowsBackend)) {
    let _ = RUNTIME.try_with(|cell| {
        let Some(mut runtime) = cell.get() else {
            return;
        };
        if runtime.busy {
            return;
        }
        runtime.busy = true;
        cell.set(Some(runtime));

        // SAFETY: 指针在 run() 的整个消息循环期间有效，且通过 busy 保证不重入。
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            f(&mut *runtime.app, &mut *runtime.backend)
        }));
        if result.is_err() {
            eprintln!("VimGlide: 处理事件时发生内部错误（已捕获）");
        }

        runtime.busy = false;
        cell.set(Some(runtime));
    });
}

pub struct WindowsBackend {
    desktop: Desktop,
    overlay: Overlay,
    _hotkeys: Hotkeys,
    _hook: HookHandle,
    /// 托盘图标；创建失败不影响主要功能。
    tray: Option<tray::Tray>,
    /// 实际生效的提示热键（可能与配置不同，见 `new`）。
    hint_hotkey: String,
    /// 本进程是否以管理员身份运行。
    elevated: bool,
    /// 上一次采集是否因为前台窗口权限更高而失败。
    needs_admin: bool,
    uia: Option<UiaSource>,
    uia_unavailable: bool,
}

/// 配置的热键被其它程序占用时，依次尝试这些备用热键。
const HINT_HOTKEY_FALLBACKS: [&str; 2] = ["ctrl+alt+space", "alt+shift+space"];

impl WindowsBackend {
    fn new(
        config: &Config,
        config_path: &Path,
        desktop: Desktop,
        overlay: Overlay,
    ) -> Result<Self, String> {
        let hint_spec = HotkeySpec::parse(&config.hotkey.hint)?;
        let mut hotkeys = Hotkeys::new(overlay.hwnd());

        let mut candidates = vec![(config.hotkey.hint.clone(), hint_spec)];
        for text in HINT_HOTKEY_FALLBACKS {
            if let Ok(spec) = HotkeySpec::parse(text) {
                candidates.push((text.to_string(), spec));
            }
        }

        let mut active: Option<(String, HotkeySpec)> = None;
        let mut last_error = String::new();
        for (text, spec) in &candidates {
            match hotkeys.register(HOTKEY_HINT, spec, text) {
                Ok(()) => {
                    active = Some((text.clone(), *spec));
                    break;
                }
                Err(e) => last_error = e,
            }
        }
        let (hint_hotkey, hint_spec) = active.ok_or(last_error)?;
        if hint_hotkey != config.hotkey.hint {
            eprintln!(
                "VimGlide: 热键 `{}` 已被占用，改用 `{hint_hotkey}`",
                config.hotkey.hint
            );
        }

        // 退出热键被占用不影响主要功能，只提示一下。
        let quit_spec = HotkeySpec::parse(&config.hotkey.quit)?;
        if let Err(e) = hotkeys.register(HOTKEY_QUIT, &quit_spec, &config.hotkey.quit) {
            eprintln!("VimGlide: {e}（可继续使用 Esc 退出提示模式）");
        }

        OVERLAY_HWND.store(overlay.hwnd().0 as isize, Ordering::SeqCst);
        let _ = HINT_SPEC.set(hint_spec);
        let _ = QUIT_SPEC.set(quit_spec);

        let elevated = is_elevated();
        let tray = match tray::Tray::new(
            config_path,
            &hint_hotkey,
            &config.hotkey.quit,
            elevated,
        ) {
            Ok(tray) => Some(tray),
            Err(e) => {
                eprintln!("VimGlide: {e}（可继续用热键操作）");
                None
            }
        };

        // SAFETY: 低层键盘钩子是全局的，可在拥有消息循环的线程安装。
        let hook = match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0) } {
            Ok(hook) => HookHandle(hook),
            Err(e) => return Err(format!("安装键盘钩子失败：{e}")),
        };

        Ok(Self {
            desktop,
            overlay,
            _hotkeys: hotkeys,
            _hook: hook,
            tray,
            hint_hotkey,
            elevated,
            needs_admin: false,
            uia: None,
            uia_unavailable: false,
        })
    }

    fn message_loop(&mut self, app: &mut App) -> Result<(), String> {
        // SAFETY: 窗口与消息循环均由本线程创建，句柄在循环期间有效。
        unsafe {
            SetTimer(Some(self.overlay.hwnd()), TIMER_ID, 60, None as TIMERPROC);
            RUNTIME.with(|cell| {
                cell.set(Some(Runtime {
                    app: app as *mut App,
                    backend: self as *mut WindowsBackend,
                    busy: false,
                }))
            });

            let mut message = MSG::default();
            loop {
                let result = GetMessageW(&mut message, None, 0, 0);
                if result.0 == 0 || result.0 == -1 {
                    break;
                }
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }

            let _ = KillTimer(Some(self.overlay.hwnd()), TIMER_ID);
            RUNTIME.with(|cell| cell.set(None));
        }
        Ok(())
    }
}

impl Backend for WindowsBackend {
    fn desktop(&self) -> &Desktop {
        &self.desktop
    }

    fn collect_targets(&mut self) -> Vec<Target> {
        let elevated_target = foreground_pid()
            .map(process_is_elevated)
            .unwrap_or(false);
        if self.uia.is_none() && !self.uia_unavailable {
            match UiaSource::new() {
                Ok(source) => self.uia = Some(source),
                Err(e) => {
                    eprintln!("VimGlide: {e}");
                    self.uia_unavailable = true;
                }
            }
        }
        let targets = match &self.uia {
            Some(source) => source.collect(self.desktop.screen, Duration::from_millis(1500)),
            None => Vec::new(),
        };
        self.needs_admin = targets.is_empty() && elevated_target && !self.elevated;
        targets
    }

    fn click(&mut self, x: i32, y: i32) {
        input::click(x, y);
    }

    fn scroll(&mut self, vertical: i32, horizontal: i32) {
        input::wheel(vertical, horizontal);
    }

    fn present(&mut self, canvas: &Canvas) {
        if let Err(e) = self.overlay.present(canvas) {
            eprintln!("VimGlide: {e}");
        }
    }

    fn hide(&mut self) {
        self.overlay.hide();
    }

    fn set_key_capture(&mut self, enabled: bool) {
        CAPTURE.store(enabled, Ordering::SeqCst);
    }

    fn needs_admin(&self) -> bool {
        self.needs_admin
    }

    fn run(&mut self, app: &mut App) -> Result<(), String> {
        self.message_loop(app)
    }
}

/// 遮罩窗口的窗口过程。
///
/// # Safety
/// 由 Windows 以 `extern "system"` 约定调用。
pub(super) unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_HOTKEY => {
            let id = wparam.0 as i32;
            with_runtime(|app, backend| match id {
                HOTKEY_HINT => app.toggle(backend),
                HOTKEY_QUIT => {
                    app.dismiss(backend);
                    // SAFETY: 结束本线程的消息循环。
                    unsafe { PostQuitMessage(0) };
                }
                _ => {}
            });
            LRESULT(0)
        }
        WM_TIMER => {
            with_runtime(|app, backend| app.on_tick(backend));
            LRESULT(0)
        }
        WM_VIMGLIDE_KEY => {
            let vk = wparam.0 as u32;
            let flags = lparam.0 as isize;
            if flags & FLAG_TOGGLE != 0 {
                with_runtime(|app, backend| app.toggle(backend));
            } else {
                let key = map_key(vk, flags & FLAG_SHIFT != 0);
                with_runtime(|app, backend| {
                    app.on_key(backend, key);
                });
            }
            LRESULT(0)
        }
        WM_VIMGLIDE_TRAY => {
            with_runtime(|app, backend| {
                let action = match backend.tray.as_mut() {
                    Some(tray) => tray.on_mouse(lparam),
                    None => tray::TrayAction::None,
                };
                match action {
                    tray::TrayAction::Toggle => app.toggle(backend),
                    tray::TrayAction::OpenConfig => {
                        if let Some(tray) = &backend.tray {
                            if let Err(e) = tray.open_config() {
                                eprintln!("VimGlide: {e}");
                            }
                        }
                    }
                    tray::TrayAction::Help => {
                        if let Some(tray) = &backend.tray {
                            tray.show_help();
                        }
                    }
                    tray::TrayAction::Elevate => {
                        let result = match &backend.tray {
                            Some(tray) => tray.relaunch_elevated(),
                            None => Ok(()),
                        };
                        match result {
                            Ok(()) => {
                                app.dismiss(backend);
                                // SAFETY: 结束本线程的消息循环；新进程会等待互斥量交接。
                                unsafe { PostQuitMessage(0) };
                            }
                            Err(e) => {
                                if let Some(tray) = &backend.tray {
                                    tray.show_error(&e);
                                }
                            }
                        }
                    }
                    tray::TrayAction::Quit => {
                        app.dismiss(backend);
                        // SAFETY: 结束本线程的消息循环。
                        unsafe { PostQuitMessage(0) };
                    }
                    tray::TrayAction::None => {}
                }
            });
            LRESULT(0)
        }
        _ => {
            // SAFETY: 交给系统默认处理。
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

/// 全局低层键盘钩子：捕获期间吞掉按键，只交给 VimGlide。
///
/// # Safety
/// 由 Windows 以 `extern "system"` 约定调用。
unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let passthrough = |code: i32| {
        // SAFETY: 参数原样透传。
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    };

    if code < 0 || !CAPTURE.load(Ordering::SeqCst) {
        return passthrough(code);
    }

    let message = wparam.0 as u32;

    // SAFETY: 低层键盘钩子的 lparam 指向 KBDLLHOOKSTRUCT。
    let Some(kb) = (unsafe { (lparam.0 as *const KBDLLHOOKSTRUCT).as_ref() }) else {
        return passthrough(code);
    };

    // 修饰键（Ctrl/Shift/Alt/Win，含左右版本）必须放行：
    // 吞掉它们的 keyup 会让系统一直认为修饰键处于按下状态，
    // 之后的按键就会被当成 Ctrl+Alt+字母 之类的组合键。
    if is_modifier_vk(kb.vkCode) {
        return passthrough(code);
    }

    if message != WM_KEYDOWN && message != WM_SYSKEYDOWN {
        // 按键抬起必须放行：否则系统会一直认为该键处于按下状态，
        // 导致下一次按同一热键（如 Ctrl+Alt+Space）被 MOD_NOREPEAT 忽略。
        return passthrough(code);
    }

    let hwnd = OVERLAY_HWND.load(Ordering::SeqCst);
    if hwnd == 0 {
        return passthrough(code);
    }

    let ctrl = key_down(VK_CONTROL);
    let shift = key_down(VK_SHIFT);
    let alt = key_down(VK_MENU);
    let meta = key_down(VK_LWIN) || key_down(VK_RWIN);

    // 捕获期间再次按提示热键 = 退出提示模式。
    if HINT_SPEC
        .get()
        .is_some_and(|spec| spec.matches(kb.vkCode, ctrl, alt, shift, meta))
    {
        post_key(hwnd, kb.vkCode, FLAG_TOGGLE);
        return LRESULT(1);
    }

    // 退出热键交给系统（放行），否则会把用户锁在提示模式里。
    if QUIT_SPEC
        .get()
        .is_some_and(|spec| spec.matches(kb.vkCode, ctrl, alt, shift, meta))
    {
        return passthrough(code);
    }

    let flags = if shift { FLAG_SHIFT } else { 0 };
    post_key(hwnd, kb.vkCode, flags);
    LRESULT(1)
}

/// 把按键投递给消息循环，避免在钩子里做耗时操作。
fn post_key(hwnd: isize, vk: u32, flags: isize) {
    // SAFETY: hwnd 由本进程创建；消息只用于唤醒自己的窗口过程。
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut core::ffi::c_void)),
            WM_VIMGLIDE_KEY,
            WPARAM(vk as usize),
            LPARAM(flags),
        );
    }
}

fn is_modifier_vk(vk: u32) -> bool {
    matches!(
        vk,
        // VK_SHIFT / VK_CONTROL / VK_MENU 以及左右版本、Win 键
        0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0..=0xA5
    )
}

fn key_down(vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(vk.0 as i32) < 0 }
}

/// 虚拟键码 -> 平台无关按键。
fn map_key(vk: u32, shift: bool) -> Key {
    match vk {
        0x41..=0x5A => {
            let base = (b'a' + (vk - 0x41) as u8) as char;
            Key::Char(if shift { base.to_ascii_uppercase() } else { base })
        }
        0x30..=0x39 => Key::Char((b'0' + (vk - 0x30) as u8) as char),
        vk if vk == VK_SPACE.0 as u32 => Key::Char(' '),
        vk if vk == VK_BACK.0 as u32 => Key::Backspace,
        vk if vk == VK_ESCAPE.0 as u32 => Key::Escape,
        vk if vk == VK_RETURN.0 as u32 => Key::Enter,
        vk if vk == VK_TAB.0 as u32 => Key::Tab,
        vk if vk == VK_UP.0 as u32 => Key::Up,
        vk if vk == VK_DOWN.0 as u32 => Key::Down,
        vk if vk == VK_LEFT.0 as u32 => Key::Left,
        vk if vk == VK_RIGHT.0 as u32 => Key::Right,
        vk if vk == VK_PRIOR.0 as u32 => Key::PageUp,
        vk if vk == VK_NEXT.0 as u32 => Key::PageDown,
        vk if vk == VK_HOME.0 as u32 => Key::Home,
        vk if vk == VK_END.0 as u32 => Key::End,
        vk if vk == VK_INSERT.0 as u32 || vk == VK_DELETE.0 as u32 => Key::Other,
        vk if vk == VK_OEM_MINUS.0 as u32 => Key::Char(if shift { '_' } else { '-' }),
        vk if vk == VK_OEM_PLUS.0 as u32 => Key::Char(if shift { '+' } else { '=' }),
        vk if vk == VK_OEM_COMMA.0 as u32 => Key::Char(if shift { '<' } else { ',' }),
        vk if vk == VK_OEM_PERIOD.0 as u32 => Key::Char(if shift { '>' } else { '.' }),
        vk if vk == VK_OEM_1.0 as u32 => Key::Char(if shift { ':' } else { ';' }),
        vk if vk == VK_OEM_2.0 as u32 => Key::Char(if shift { '?' } else { '/' }),
        vk if vk == VK_OEM_3.0 as u32 => Key::Char(if shift { '~' } else { '`' }),
        vk if vk == VK_OEM_4.0 as u32 => Key::Char(if shift { '{' } else { '[' }),
        vk if vk == VK_OEM_5.0 as u32 => Key::Char(if shift { '|' } else { '\\' }),
        vk if vk == VK_OEM_6.0 as u32 => Key::Char(if shift { '}' } else { ']' }),
        vk if vk == VK_OEM_7.0 as u32 => Key::Char(if shift { '"' } else { '\'' }),
        _ => Key::Other,
    }
}

/// 程序入口。
pub fn run(config: Config, config_path: std::path::PathBuf) -> Result<(), String> {
    // 物理像素坐标：UIA 的边界与遮罩层都基于物理像素，必须声明 DPI 感知。
    // SAFETY: 进程启动早期调用，无副作用。
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    init_com();
    if !ensure_single_instance()? {
        show_info("VimGlide 已经在运行，请查看系统托盘图标。");
        return Ok(());
    }

    let screen = virtual_screen();
    if screen.is_empty() {
        return Err("无法获取屏幕尺寸".to_string());
    }
    let font = load_ui_font()
        .ok_or_else(|| "未找到可用的系统字体（尝试过 Segoe UI / Arial）".to_string())?;
    let overlay = Overlay::new(screen)?;
    let desktop = Desktop {
        screen,
        scale: overlay.dpi_scale(),
        font,
    };
    let mut backend = WindowsBackend::new(&config, &config_path, desktop, overlay)?;
    let mut app = App::new(config.clone(), backend.desktop())?;

    println!(
        "VimGlide 已启动：{} 进入提示模式，{} 退出",
        backend.hint_hotkey, config.hotkey.quit
    );
    // 发行版没有控制台，用一个短暂的浮层提示「已在运行」和生效的热键。
    let ready = format!("VimGlide ready   {}", backend.hint_hotkey);
    app.show_toast(&mut backend, ready);
    backend.run(&mut app)
}

fn init_com() {
    // SAFETY: 每个线程只需初始化一次 COM。
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

/// 当前进程是否以管理员身份运行。
fn is_elevated() -> bool {
    // SAFETY: 句柄由本函数创建并释放。
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut std::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// 指定进程是否以管理员身份运行；打不开的进程按“不可访问”处理。
fn process_is_elevated(pid: u32) -> bool {
    // SAFETY: 句柄由本函数创建并释放。
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut token = HANDLE::default();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token).is_err() {
            let _ = CloseHandle(process);
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut std::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
        .is_ok();
        let _ = CloseHandle(token);
        let _ = CloseHandle(process);
        ok && elevation.TokenIsElevated != 0
    }
}

/// 前台窗口所属进程的 PID。
fn foreground_pid() -> Option<u32> {
    // SAFETY: 纯查询调用。
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        (pid != 0).then_some(pid)
    }
}

/// 只允许运行一个实例，避免出现两个托盘图标互相抢热键。
///
/// 返回 `true` 表示本进程是唯一实例，`false` 表示已有实例在运行。
///
/// “以管理员身份重启”时新进程会立刻启动，而旧进程还在退出路上，因此
/// 提权后的进程会对互斥量做短暂重试，完成交接。
fn ensure_single_instance() -> Result<bool, String> {
    // SAFETY: 创建的互斥量只用于检测，句柄在进程生命周期内保持有效。
    unsafe {
        let elevated = is_elevated();
        for attempt in 0..=15 {
            let handle = CreateMutexW(None, false, w!("VimGlide.single-instance"))
                .map_err(|e| format!("创建单实例互斥量失败：{e}"))?;
            if GetLastError() != ERROR_ALREADY_EXISTS {
                // 句柄故意不关闭：进程退出时由系统回收，运行期间保持互斥量存在。
                return Ok(true);
            }
            let _ = CloseHandle(handle);
            if !elevated || attempt == 15 {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    Ok(false)
}

/// 用系统消息框提示（发行版没有控制台）。
fn show_info(message: &str) {
    let text: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: 文本缓冲区在调用期间有效。
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            w!("VimGlide"),
            MB_OK | MB_ICONINFORMATION,
        );
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

fn load_ui_font() -> Option<Vec<u8>> {
    let windows_dir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string());
    for name in ["segoeuisb.ttf", "segoeui.ttf", "arialbd.ttf", "arial.ttf", "tahoma.ttf"] {
        let path = Path::new(&windows_dir).join("Fonts").join(name);
        if let Ok(bytes) = std::fs::read(&path) {
            if fontdue::Font::from_bytes(bytes.clone(), fontdue::FontSettings::default()).is_ok() {
                return Some(bytes);
            }
        }
    }
    None
}

/// 无控制台的发行版里用系统消息框展示致命错误。
pub fn fatal(message: &str) {
    let text: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: 文本缓冲区在调用期间有效。
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            w!("VimGlide 启动失败"),
            windows::Win32::UI::WindowsAndMessaging::MB_OK
                | windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
        );
    }
}
