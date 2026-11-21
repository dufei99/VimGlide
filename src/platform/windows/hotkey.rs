//! 全局热键的解析与注册。

use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
    UnregisterHotKey, VK_BACK, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_HOME, VK_INSERT,
    VK_LEFT, VK_NEXT, VK_OEM_1, VK_OEM_2, VK_OEM_3, VK_OEM_4, VK_OEM_5, VK_OEM_6, VK_OEM_7,
    VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD, VK_OEM_PLUS, VK_PRIOR, VK_RETURN, VK_RIGHT,
    VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::HHOOK;

/// 解析后的热键。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotkeySpec {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    /// Win32 虚拟键码。
    pub vk: u32,
}

impl HotkeySpec {
    /// 解析形如 `ctrl+shift+space` 的字符串。
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut spec = HotkeySpec {
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
            vk: 0,
        };
        let mut has_key = false;

        for part in text.split('+').map(str::trim).filter(|p| !p.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => spec.ctrl = true,
                "shift" => spec.shift = true,
                "alt" | "option" => spec.alt = true,
                "win" | "meta" | "super" | "cmd" => spec.meta = true,
                key => {
                    if has_key {
                        return Err(format!("热键 `{text}` 中包含多个主键"));
                    }
                    spec.vk = key_code(key)
                        .ok_or_else(|| format!("无法识别的按键 `{key}`（热键 `{text}`）"))?;
                    has_key = true;
                }
            }
        }

        if !has_key {
            return Err(format!("热键 `{text}` 缺少主键"));
        }
        if !(spec.ctrl || spec.alt || spec.shift || spec.meta) {
            return Err(format!("热键 `{text}` 至少需要一个修饰键，避免抢占普通输入"));
        }
        Ok(spec)
    }

    /// 判断一次按键是否正好匹配本热键。
    pub fn matches(&self, vk: u32, ctrl: bool, alt: bool, shift: bool, meta: bool) -> bool {
        self.vk == vk
            && self.ctrl == ctrl
            && self.alt == alt
            && self.shift == shift
            && self.meta == meta
    }

    fn modifiers(&self) -> HOT_KEY_MODIFIERS {
        let mut modifiers = MOD_NOREPEAT;
        if self.ctrl {
            modifiers |= MOD_CONTROL;
        }
        if self.alt {
            modifiers |= MOD_ALT;
        }
        if self.shift {
            modifiers |= MOD_SHIFT;
        }
        if self.meta {
            modifiers |= MOD_WIN;
        }
        modifiers
    }
}

fn key_code(name: &str) -> Option<u32> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphanumeric() {
            return Some(c.to_ascii_uppercase() as u32);
        }
    }

    if let Some(rest) = name.strip_prefix('f') {
        if let Ok(n) = rest.parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some(VK_F1.0 as u32 + n - 1);
            }
        }
    }

    let vk = match name {
        "space" => VK_SPACE,
        "esc" | "escape" => VK_ESCAPE,
        "tab" => VK_TAB,
        "enter" | "return" => VK_RETURN,
        "backspace" | "bksp" => VK_BACK,
        "up" => VK_UP,
        "down" => VK_DOWN,
        "left" => VK_LEFT,
        "right" => VK_RIGHT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "pgup" => VK_PRIOR,
        "pagedown" | "pgdn" => VK_NEXT,
        "insert" | "ins" => VK_INSERT,
        "delete" | "del" => VK_DELETE,
        "semicolon" | ";" => VK_OEM_1,
        "slash" | "/" => VK_OEM_2,
        "grave" | "backtick" | "`" => VK_OEM_3,
        "leftbracket" | "[" => VK_OEM_4,
        "backslash" | "\\" => VK_OEM_5,
        "rightbracket" | "]" => VK_OEM_6,
        "quote" | "'" => VK_OEM_7,
        "comma" | "," => VK_OEM_COMMA,
        "period" | "." => VK_OEM_PERIOD,
        "minus" | "-" => VK_OEM_MINUS,
        "equal" | "=" => VK_OEM_PLUS,
        _ => return None,
    };
    Some(vk.0 as u32)
}

/// 已注册的热键集合，Drop 时自动注销。
pub struct Hotkeys {
    ids: Vec<i32>,
    hwnd: windows::Win32::Foundation::HWND,
}

impl Hotkeys {
    pub fn new(hwnd: windows::Win32::Foundation::HWND) -> Self {
        Self {
            ids: Vec::new(),
            hwnd,
        }
    }

    /// 注册单个热键；`label` 仅用于错误信息。
    pub fn register(&mut self, id: i32, spec: &HotkeySpec, label: &str) -> Result<(), String> {
        // SAFETY: hwnd 由本进程创建且有效。
        unsafe { RegisterHotKey(Some(self.hwnd), id, spec.modifiers(), spec.vk) }
            .map_err(|e| format!("注册全局热键 `{label}` 失败（可能被其它程序占用）：{e}"))?;
        self.ids.push(id);
        Ok(())
    }

    /// 注销指定热键；未注册时忽略。
    pub fn unregister(&mut self, id: i32) {
        if let Some(position) = self.ids.iter().position(|registered| *registered == id) {
            self.ids.remove(position);
            // SAFETY: 与注册时相同的 hwnd 与 id。
            let _ = unsafe { UnregisterHotKey(Some(self.hwnd), id) };
        }
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        for id in &self.ids {
            // SAFETY: 与注册时相同的 hwnd 与 id。
            let _ = unsafe { UnregisterHotKey(Some(self.hwnd), *id) };
        }
    }
}

/// 键盘钩子的句柄包装，Drop 时卸载。
pub struct HookHandle(pub HHOOK);

impl Drop for HookHandle {
    fn drop(&mut self) {
        // SAFETY: 句柄由 SetWindowsHookExW 返回且只卸载一次。
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx(self.0) };
    }
}
