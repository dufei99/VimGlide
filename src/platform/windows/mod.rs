//! Windows 后端：全局热键 + 键盘拦截 + UIA 采集 + 分层窗口遮罩 + 鼠标注入。

mod hotkey;
mod input;
mod ocr;
mod overlay;
mod tray;
mod uia;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use windows::core::{BOOL, PCWSTR, w};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, RECT, TRUE,
    WPARAM,
};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    SetProcessWorkingSetSize,
};
use windows::Win32::UI::Accessibility::{
    HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent,
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
    CallNextHookEx, DefWindowProcW, DispatchMessageW, EnumWindows, EVENT_SYSTEM_DIALOGSTART,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MENUPOPUPSTART, GetClassNameW, GetForegroundWindow,
    GetGUIThreadInfo, GetMessageW, GetSystemMetrics, GetWindowRect, GetWindowThreadProcessId,
    GUITHREADINFO,
    HTTRANSPARENT, IsWindowVisible, KBDLLHOOKSTRUCT, KillTimer, MSG, MB_ICONINFORMATION, MB_OK,
    MessageBoxW, PM_REMOVE, PeekMessageW, PostMessageW, PostQuitMessage, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetTimer, SetWindowsHookExW,
    TIMERPROC, TranslateMessage, WH_KEYBOARD_LL, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_APP, WM_HOTKEY, WM_KEYDOWN, WM_NCHITTEST, WM_QUIT, WM_SYSKEYDOWN,
    WM_TIMER,
};

use self::hotkey::{HookHandle, HotkeySpec, Hotkeys};
use self::overlay::Overlay;
use self::uia::UiaSource;
use crate::app::App;
use crate::config::{Config, OcrConfig};
use crate::model::{Rect, Target};
use crate::platform::{Backend, Desktop, Key};
use crate::render::Scene;

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
const FLAG_CTRL: isize = 4;

/// 是否处于「键盘只交给 VimGlide」的状态。
static CAPTURE: AtomicBool = AtomicBool::new(false);
/// 叠加层窗口句柄，钩子用它投递消息。
static OVERLAY_HWND: AtomicIsize = AtomicIsize::new(0);
/// 生效的提示热键，用于捕获期间再次触发退出。
static HINT_SPEC: Mutex<Option<HotkeySpec>> = Mutex::new(None);
/// 退出热键，捕获期间需要放行给系统处理。
static QUIT_SPEC: Mutex<Option<HotkeySpec>> = Mutex::new(None);
/// 本次提示模式的采集根窗口，用于判断「是否换了新窗口」。
static SCAN_ROOT: AtomicIsize = AtomicIsize::new(0);
/// WinEvent 是否观察到了新 UI（菜单弹出 / 对话框 / 新前台窗口）。
static NEW_UI: AtomicBool = AtomicBool::new(false);

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
    config_path: PathBuf,
    /// 实际生效的提示热键（可能与配置不同，见 `new`）。
    hint_hotkey: String,
    hint_spec: HotkeySpec,
    quit_hotkey: String,
    quit_spec: HotkeySpec,
    /// 本进程是否以管理员身份运行。
    elevated: bool,
    /// 上一次采集是否因为前台窗口权限更高而失败。
    needs_admin: bool,
    /// 提示模式开始时的前台窗口；遮罩可能抢走前台，采集必须仍以它为准。
    scan_root: HWND,
    /// 最近一次属于其他程序的前台窗口，用于遮罩自己成为前台时兜底。
    last_external: HWND,
    uia: Option<UiaSource>,
    uia_unavailable: bool,
    /// UIA 结果不足时的 OCR 兜底配置。
    ocr: OcrConfig,
    /// 提示模式开始时目标进程的可见顶层窗口快照，用于找出点击后新出现的弹层。
    windows_before: Vec<WindowSnapshot>,
}

/// 可见顶层窗口快照：句柄 + 屏幕矩形。
type WindowSnapshot = (isize, i32, i32, i32, i32);

/// 配置的热键被其它程序占用时，依次尝试这些备用热键。
const HINT_HOTKEY_FALLBACKS: [&str; 2] = ["ctrl+alt+space", "alt+shift+space"];

impl WindowsBackend {
    fn new(
        config: &Config,
        config_path: &Path,
        desktop: Desktop,
        overlay: Overlay,
    ) -> Result<Self, String> {
        let mut hotkeys = Hotkeys::new(overlay.hwnd());
        let (hint_hotkey, hint_spec) =
            Self::register_hint(&mut hotkeys, config.hotkey.hint.clone())?;
        if hint_hotkey != config.hotkey.hint {
            eprintln!(
                "VimGlide: 热键 `{}` 已被占用，改用 `{hint_hotkey}`",
                config.hotkey.hint
            );
        }

        // 退出热键被占用不影响主要功能，只提示一下。
        let quit_hotkey = config.hotkey.quit.clone();
        let quit_spec = HotkeySpec::parse(&quit_hotkey)?;
        if let Err(e) = hotkeys.register(HOTKEY_QUIT, &quit_spec, &quit_hotkey) {
            eprintln!("VimGlide: {e}（可继续使用 Esc 退出提示模式）");
        }

        OVERLAY_HWND.store(overlay.hwnd().0 as isize, Ordering::SeqCst);
        Self::set_specs(Some(hint_spec), Some(quit_spec));

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
            config_path: config_path.to_path_buf(),
            hint_hotkey,
            hint_spec,
            quit_hotkey,
            quit_spec,
            elevated,
            needs_admin: false,
            scan_root: HWND::default(),
            last_external: HWND::default(),
            uia: None,
            uia_unavailable: false,
            ocr: config.ocr.clone(),
            windows_before: Vec::new(),
        })
    }

    /// 注册提示热键；配置值被占用时依次尝试备用热键。
    fn register_hint(hotkeys: &mut Hotkeys, hint: String) -> Result<(String, HotkeySpec), String> {
        let spec = HotkeySpec::parse(&hint)?;
        let mut candidates = vec![(hint, spec)];
        for text in HINT_HOTKEY_FALLBACKS {
            if let Ok(spec) = HotkeySpec::parse(text) {
                candidates.push((text.to_string(), spec));
            }
        }
        let mut last_error = String::new();
        for (text, spec) in candidates {
            match hotkeys.register(HOTKEY_HINT, &spec, &text) {
                Ok(()) => return Ok((text, spec)),
                Err(e) => last_error = e,
            }
        }
        Err(last_error)
    }

    fn set_specs(hint: Option<HotkeySpec>, quit: Option<HotkeySpec>) {
        if let Ok(mut slot) = HINT_SPEC.lock() {
            *slot = hint;
        }
        if let Ok(mut slot) = QUIT_SPEC.lock() {
            *slot = quit;
        }
    }

    /// 从磁盘重新载入配置：热键重新注册，其余设置交给 App。
    fn reload_config(&mut self, app: &mut App) {
        let config = match Config::from_path(&self.config_path) {
            Ok(config) => config,
            Err(e) => {
                // 解析错误信息可能较长，用消息框完整显示，避免被遮罩截断。
                if let Some(tray) = &self.tray {
                    tray.show_error(&e);
                } else {
                    eprintln!("VimGlide: {e}");
                }
                return;
            }
        };

        self._hotkeys.unregister(HOTKEY_HINT);
        self._hotkeys.unregister(HOTKEY_QUIT);

        let Ok((hint_hotkey, hint_spec)) =
            Self::register_hint(&mut self._hotkeys, config.hotkey.hint.clone())
        else {
            // 新热键不可用：恢复旧热键，配置保持原样。
            let _ = self
                ._hotkeys
                .register(HOTKEY_HINT, &self.hint_spec, &self.hint_hotkey);
            let _ = self
                ._hotkeys
                .register(HOTKEY_QUIT, &self.quit_spec, &self.quit_hotkey);
            app.show_message(
                self,
                format!(
                    "热键 `{}` 不可用，配置未重新载入",
                    config.hotkey.hint
                ),
                true,
            );
            return;
        };

        let (quit_hotkey, quit_spec) = match HotkeySpec::parse(&config.hotkey.quit) {
            Ok(spec)
                if self
                    ._hotkeys
                    .register(HOTKEY_QUIT, &spec, &config.hotkey.quit)
                    .is_ok() =>
            {
                (config.hotkey.quit.clone(), spec)
            }
            _ => {
                // 退出热键不可用时沿用旧的。
                let _ = self
                    ._hotkeys
                    .register(HOTKEY_QUIT, &self.quit_spec, &self.quit_hotkey);
                (self.quit_hotkey.clone(), self.quit_spec)
            }
        };

        self.ocr = config.ocr.clone();
        self.hint_hotkey = hint_hotkey.clone();
        self.hint_spec = hint_spec;
        self.quit_hotkey = quit_hotkey.clone();
        self.quit_spec = quit_spec;
        Self::set_specs(Some(hint_spec), Some(quit_spec));
        if let Some(tray) = &mut self.tray {
            tray.update_hotkeys(&hint_hotkey, &quit_hotkey);
        }

        app.apply_config(self, config);
        app.show_message(
            self,
            format!("配置已重新载入 ({hint_hotkey})"),
            false,
        );
    }

    /// OCR 兜底：识别窗口文字行，过滤掉与已有目标重叠的行后追加。
    fn append_ocr_targets(&self, targets: &mut Vec<Target>) {
        let started = Instant::now();
        match ocr::scan_window(self.scan_root) {
            Ok(lines) => {
                // 总数量同样受 max_targets 限制。
                targets.truncate(self.ocr.max_targets);
                let before = targets.len();
                for line in lines {
                    if targets.len() >= self.ocr.max_targets {
                        break;
                    }
                    if overlaps_existing(targets, &line.rect) {
                        continue;
                    }
                    targets.push(Target {
                        rect: line.rect,
                        name: line.text,
                    });
                }
                eprintln!(
                    "VimGlide: OCR 兜底识别补充了 {} 个目标（共 {} 个），耗时 {} ms",
                    targets.len() - before,
                    targets.len(),
                    started.elapsed().as_millis()
                );
            }
            Err(e) => eprintln!("VimGlide: OCR 兜底识别失败：{e}"),
        }
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

    fn begin_hints(&mut self) {
        // SAFETY: 纯查询调用。
        let foreground = unsafe { GetForegroundWindow() };
        let root = if foreground == self.overlay.hwnd() {
            self.last_external
        } else {
            foreground
        };
        self.scan_root = root;
        SCAN_ROOT.store(root.0 as isize, Ordering::SeqCst);
        if !root.0.is_null() {
            self.last_external = root;
        }
        // 记录当前可见顶层窗口，点击后对比即可发现 Chromium 之类的独立弹层。
        self.windows_before = window_pid(root)
            .map(snapshot_windows)
            .unwrap_or_default();
        self.overlay.request_activation();
    }

    fn collect_targets(&mut self) -> Vec<Target> {
        let elevated_target = window_pid(self.scan_root)
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
        let mut targets = match &self.uia {
            Some(source) => source.collect(
                self.desktop.screen,
                Duration::from_millis(1500),
                self.scan_root,
            ),
            None => Vec::new(),
        };
        self.needs_admin = targets.is_empty() && elevated_target && !self.elevated;
        // 扫描结束后不再需要 UIA：释放 COM 树，显示提示期间只保留坐标。
        self.uia = None;

        // UIA 几乎采不到元素时（Chromium 未开启无障碍树等），用系统 OCR 兜底。
        if self.ocr.enabled
            && targets.len() < self.ocr.min_uia_targets
            && !self.scan_root.0.is_null()
        {
            self.append_ocr_targets(&mut targets);
        }
        targets
    }

    fn move_cursor(&mut self, x: i32, y: i32) {
        input::move_to(x, y);
    }

    fn nudge_cursor(&mut self, dx: i32, dy: i32) {
        input::nudge(dx, dy);
    }

    fn click(&mut self, double: bool) {
        if double {
            input::double_click_here();
        } else {
            input::click_here();
        }
    }

    fn right_click(&mut self) {
        input::right_click_here();
    }

    fn wait_new_ui(&mut self, timeout: Duration) -> bool {
        NEW_UI.store(false, Ordering::SeqCst);
        // SAFETY: 钩子仅在本次等待期间安装，回调只写原子标志。
        let foreground_hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_MENUPOPUPSTART,
                None,
                Some(new_ui_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        let dialog_hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_DIALOGSTART,
                EVENT_SYSTEM_DIALOGSTART,
                None,
                Some(new_ui_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };

        let deadline = Instant::now() + timeout;
        let mut found = false;
        // 菜单可能已经弹出，先轮询一次，再等 WinEvent 或轮询兜底。
        while Instant::now() < deadline {
            pump_messages();
            if NEW_UI.load(Ordering::SeqCst)
                || new_ui_present(self.scan_root, self.overlay.hwnd(), &self.windows_before)
            {
                found = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // SAFETY: 句柄由上面的调用返回。
        unsafe {
            if !foreground_hook.is_invalid() {
                let _ = UnhookWinEvent(foreground_hook);
            }
            if !dialog_hook.is_invalid() {
                let _ = UnhookWinEvent(dialog_hook);
            }
        }
        found
    }

    fn collect_chained_targets(&mut self) -> Vec<Target> {
        if self.uia.is_none() && !self.uia_unavailable {
            match UiaSource::new() {
                Ok(source) => self.uia = Some(source),
                Err(e) => {
                    eprintln!("VimGlide: {e}");
                    self.uia_unavailable = true;
                }
            }
        }
        let Some(source) = &self.uia else {
            return Vec::new();
        };
        // 只采集新出现的 UI：弹层、菜单、对话框或新的前台窗口。
        let roots = chained_roots(self.scan_root, self.overlay.hwnd(), &self.windows_before);
        let mut targets = Vec::new();
        for root in roots.iter().take(3) {
            let mut batch = source.collect(
                self.desktop.screen,
                Duration::from_millis(400),
                *root,
            );
            // Chromium 等自绘弹层在未开启无障碍树时 UIA 采不到，对这一小块弹层做 OCR 兜底。
            if batch.is_empty() && self.ocr.enabled {
                batch = ocr_targets(*root, self.ocr.max_targets);
            }
            targets.extend(batch);
        }
        self.uia = None;
        dedupe_targets(targets)
    }

    fn scroll(&mut self, vertical: i32, horizontal: i32) {
        input::wheel(vertical, horizontal);
    }

    fn measure_text(&mut self, text: &str, px: f32) -> f32 {
        self.overlay.measure_text(text, px)
    }

    fn present(&mut self, scene: &Scene) {
        if let Err(e) = self.overlay.present(scene) {
            eprintln!("VimGlide: {e}");
        }
    }

    fn hide(&mut self) {
        self.overlay.hide();
        // 空闲时释放 UIA 对象并把工作集还给系统，降低常驻内存。
        self.uia = None;
        // SAFETY: 仅作用于当前进程。
        unsafe {
            let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
        }
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
                let key = map_key(vk, flags & FLAG_SHIFT != 0, flags & FLAG_CTRL != 0);
                with_runtime(|app, backend| {
                    app.on_key(backend, key);
                });
            }
            LRESULT(0)
        }
        WM_NCHITTEST => {
            // 遮罩只用于显示：命中测试返回透明，鼠标事件落到下面的窗口。
            if OVERLAY_HWND.load(Ordering::SeqCst) == hwnd.0 as isize {
                LRESULT(HTTRANSPARENT as isize)
            } else {
                // SAFETY: 交给系统默认处理。
                unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
            }
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
                    tray::TrayAction::Reload => backend.reload_config(app),
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
    let hint_spec = HINT_SPEC.lock().ok().and_then(|slot| *slot);
    if hint_spec.is_some_and(|spec| spec.matches(kb.vkCode, ctrl, alt, shift, meta)) {
        post_key(hwnd, kb.vkCode, FLAG_TOGGLE);
        return LRESULT(1);
    }

    // 退出热键交给系统（放行），否则会把用户锁在提示模式里。
    let quit_spec = QUIT_SPEC.lock().ok().and_then(|slot| *slot);
    if quit_spec.is_some_and(|spec| spec.matches(kb.vkCode, ctrl, alt, shift, meta)) {
        return passthrough(code);
    }

    let flags = (if shift { FLAG_SHIFT } else { 0 }) | (if ctrl { FLAG_CTRL } else { 0 });
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
fn map_key(vk: u32, shift: bool, ctrl: bool) -> Key {
    match vk {
        0x41..=0x5A => {
            let base = (b'a' + (vk - 0x41) as u8) as char;
            Key::Char(if shift { base.to_ascii_uppercase() } else { base })
        }
        0x30..=0x39 => Key::Char((b'0' + (vk - 0x30) as u8) as char),
        vk if vk == VK_SPACE.0 as u32 => Key::Space { shift, ctrl },
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
    let overlay = Overlay::new(screen)?;
    let desktop = Desktop {
        screen,
        scale: overlay.dpi_scale(),
    };
    let mut backend = WindowsBackend::new(&config, &config_path, desktop, overlay)?;
    let mut app = App::new(config.clone(), backend.desktop())?;

    println!(
        "VimGlide 已启动：{} 进入提示模式，{} 退出",
        backend.hint_hotkey, config.hotkey.quit
    );
    // 发行版没有控制台，用一个短暂的浮层提示「已在运行」和生效的热键。
    let ready = format!("已就绪   {}", backend.hint_hotkey);
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

/// 窗口所属进程的 PID。
fn window_pid(hwnd: HWND) -> Option<u32> {
    // SAFETY: 纯查询调用。
    unsafe {
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        (pid != 0).then_some(pid)
    }
}

/// OCR 行是否与已有目标重复：覆盖了目标面积的一半，或中心点落在目标内。
fn overlaps_existing(targets: &[Target], rect: &Rect) -> bool {
    let (cx, cy) = rect.center();
    targets.iter().any(|target| {
        let overlap = target
            .rect
            .intersection(rect)
            .map(|shared| shared.area())
            .unwrap_or(0);
        let center_inside = cx >= target.rect.x
            && cx < target.rect.right()
            && cy >= target.rect.y
            && cy < target.rect.bottom();
        overlap * 2 > target.rect.area() || center_inside
    })
}

/// WinEvent 回调：只关心菜单弹出、对话框开始和新前台窗口。
unsafe extern "system" fn new_ui_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _id_thread: u32,
    _time: u32,
) {
    if hwnd.0.is_null() {
        return;
    }
    if hwnd.0 as isize == OVERLAY_HWND.load(Ordering::SeqCst) {
        return;
    }
    match event {
        EVENT_SYSTEM_MENUPOPUPSTART | EVENT_SYSTEM_DIALOGSTART => {
            NEW_UI.store(true, Ordering::SeqCst);
        }
        EVENT_SYSTEM_FOREGROUND => {
            // 只有换成另一个顶层窗口才算新 UI。
            if hwnd.0 as isize != SCAN_ROOT.load(Ordering::SeqCst) {
                NEW_UI.store(true, Ordering::SeqCst);
            }
        }
        _ => {}
    }
}

/// 轮询兜底：是否存在菜单、对话框、新前台窗口，或同进程新出现的弹层。
fn new_ui_present(scan_root: HWND, overlay: HWND, before: &[WindowSnapshot]) -> bool {
    // SAFETY: 纯查询调用。
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() || foreground == overlay {
        return false;
    }
    if foreground != scan_root {
        return true;
    }
    // 菜单不一定改变前台窗口，再检查菜单归属与菜单类窗口。
    if menu_owner(foreground).is_some() || !visible_menus(foreground).is_empty() {
        return true;
    }
    // Chromium / XAML 的弹层是独立窗口，不改变前台，也不会出现在菜单类名里。
    window_pid(scan_root)
        .map(|pid| !new_popup_windows(pid, before).is_empty())
        .unwrap_or(false)
}

/// 前台线程是否拥有活动菜单，返回菜单所有者窗口。
fn menu_owner(hwnd: HWND) -> Option<HWND> {
    if hwnd.0.is_null() {
        return None;
    }
    // SAFETY: 纯查询调用。
    let thread = unsafe { GetWindowThreadProcessId(hwnd, None) };
    if thread == 0 {
        return None;
    }
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: cbSize 已按结构体大小填写。
    unsafe { GetGUIThreadInfo(thread, &mut info).ok()? };
    if info.hwndMenuOwner.0.is_null() {
        None
    } else {
        Some(info.hwndMenuOwner)
    }
}

/// Win32 菜单弹窗的类名。
const MENU_CLASS: &str = "#32768";

struct MenuSearch {
    thread: u32,
    out: Vec<HWND>,
}

/// 枚举指定线程当前可见的菜单弹窗。
fn visible_menus(owner: HWND) -> Vec<HWND> {
    // SAFETY: 纯查询调用。
    let thread = unsafe { GetWindowThreadProcessId(owner, None) };
    if thread == 0 {
        return Vec::new();
    }
    let mut search = MenuSearch {
        thread,
        out: Vec::new(),
    };
    // SAFETY: 回调只在本调用期间通过 lparam 访问 search。
    unsafe {
        let _ = EnumWindows(
            Some(enum_menu_proc),
            LPARAM(&mut search as *mut MenuSearch as isize),
        );
    }
    search.out
}

unsafe extern "system" fn enum_menu_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = unsafe { &mut *(lparam.0 as *mut MenuSearch) };
    if unsafe { GetWindowThreadProcessId(hwnd, None) } != search.thread {
        return TRUE;
    }
    if !unsafe { IsWindowVisible(hwnd).as_bool() } {
        return TRUE;
    }
    let mut buffer = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) };
    if len > 0 && String::from_utf16_lossy(&buffer[..len as usize]) == MENU_CLASS {
        search.out.push(hwnd);
    }
    TRUE
}

/// 链式扫描的根窗口：同进程新弹层优先，其次是菜单和新前台窗口。
fn chained_roots(scan_root: HWND, overlay: HWND, before: &[WindowSnapshot]) -> Vec<HWND> {
    // SAFETY: 纯查询调用。
    let foreground = unsafe { GetForegroundWindow() };
    let mut roots: Vec<HWND> = Vec::new();
    // Chromium 下拉、XAML 菜单等独立弹层不改变前台窗口，靠窗口快照对比发现。
    if let Some(pid) = window_pid(scan_root) {
        roots.extend(new_popup_windows(pid, before));
    }
    if !foreground.0.is_null() && foreground != overlay {
        roots.extend(visible_menus(foreground));
        if foreground != scan_root {
            roots.push(foreground);
        }
    }
    if roots.is_empty() && !scan_root.0.is_null() {
        roots.push(scan_root);
    }
    let mut unique: Vec<HWND> = Vec::new();
    for root in roots {
        if !unique.contains(&root) {
            unique.push(root);
        }
    }
    unique
}

struct WindowSearch {
    pid: u32,
    out: Vec<WindowSnapshot>,
}

/// 快照目标进程所有可见顶层窗口（排除工具提示）。
fn snapshot_windows(pid: u32) -> Vec<WindowSnapshot> {
    let mut search = WindowSearch {
        pid,
        out: Vec::new(),
    };
    // SAFETY: 回调只在本调用期间通过 lparam 访问 search。
    unsafe {
        let _ = EnumWindows(
            Some(enum_window_proc),
            LPARAM(&mut search as *mut WindowSearch as isize),
        );
    }
    search.out
}

/// 当前可见但快照里没有的窗口，即点击后新出现的弹层。
fn new_popup_windows(pid: u32, before: &[WindowSnapshot]) -> Vec<HWND> {
    snapshot_windows(pid)
        .into_iter()
        .filter(|(hwnd, ..)| !before.iter().any(|(old, ..)| old == hwnd))
        .map(|(hwnd, ..)| HWND(hwnd as *mut _))
        .collect()
}

unsafe extern "system" fn enum_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = unsafe { &mut *(lparam.0 as *mut WindowSearch) };
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid != search.pid || !unsafe { IsWindowVisible(hwnd).as_bool() } {
        return TRUE;
    }
    if window_class(hwnd) == "tooltips_class32" {
        return TRUE;
    }
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return TRUE;
    }
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    if width <= 0 || height <= 0 {
        return TRUE;
    }
    search
        .out
        .push((hwnd.0 as isize, rect.left, rect.top, width, height));
    TRUE
}

/// 读取窗口类名。
fn window_class(hwnd: HWND) -> String {
    let mut buffer = [0u16; 64];
    // SAFETY: 纯查询调用。
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) };
    if len == 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buffer[..len as usize])
    }
}

/// 对单个窗口做 OCR，把文字行转成目标。
fn ocr_targets(hwnd: HWND, limit: usize) -> Vec<Target> {
    let Ok(lines) = ocr::scan_window(hwnd) else {
        return Vec::new();
    };
    let mut targets: Vec<Target> = Vec::new();
    for line in lines {
        if targets.len() >= limit {
            break;
        }
        if overlaps_existing(&targets, &line.rect) {
            continue;
        }
        targets.push(Target {
            rect: line.rect,
            name: line.text,
        });
    }
    targets
}

/// 按矩形去重，避免多个根窗口采到同一控件。
fn dedupe_targets(targets: Vec<Target>) -> Vec<Target> {
    let mut seen = std::collections::HashSet::new();
    targets
        .into_iter()
        .filter(|target| {
            seen.insert((
                target.rect.x,
                target.rect.y,
                target.rect.w,
                target.rect.h,
            ))
        })
        .collect()
}

/// 等待新 UI 期间抽取消息，保证 WinEvent 回调能被派发。
fn pump_messages() {
    // SAFETY: 消息循环由本线程拥有，这里只是非阻塞地抽取并分发。
    unsafe {
        let mut message = MSG::default();
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            if message.message == WM_QUIT {
                PostQuitMessage(0);
                return;
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
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
