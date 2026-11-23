//! 平台抽象层。
//!
//! 与系统耦合的部分（元素采集、鼠标注入、遮罩层、全局热键、键盘捕获）
//! 都收敛在下面的 [`Backend`] trait 中；上层 `App` 只依赖这个 trait。
//! 目前只实现了 Windows，其它平台保留了同构的占位实现。

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

use crate::app::App;
use crate::config::Config;
use crate::model::{Rect, Target};
use crate::render::Scene;

/// 一次按键（已从平台的键码/字符映射而来）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// 字符键，字母保留大小写（Shift+g 会得到 'G'）。
    Char(char),
    /// 空格键，`shift`/`ctrl` 表示是否同时按下了对应修饰键。
    /// Shift 用于双击，Ctrl 用于右击。
    Space { shift: bool, ctrl: bool },
    Backspace,
    Enter,
    Escape,
    Tab,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Other,
}

/// 运行时环境信息。
pub struct Desktop {
    /// 虚拟桌面（所有显示器）的物理像素范围。
    pub screen: Rect,
    /// 主显示器缩放系数（96 DPI = 1.0）。
    pub scale: f32,
}

/// 平台后端：所有需要与操作系统交互的能力。
pub trait Backend {
    fn desktop(&self) -> &Desktop;

    /// 采集当前前台窗口里可点击的元素。
    fn collect_targets(&mut self) -> Vec<Target>;

    /// 进入提示模式之前的准备：记录要采集的前台窗口等。
    fn begin_hints(&mut self) {}

    /// 把鼠标移到屏幕坐标处（选中目标后定位）。
    fn move_cursor(&mut self, x: i32, y: i32);

    /// 相对移动鼠标（像素），用于选中后微调。
    fn nudge_cursor(&mut self, dx: i32, dy: i32);

    /// 在当前鼠标位置模拟左键：`double` 为真时双击，否则单击。
    fn click(&mut self, double: bool);

    /// 在当前鼠标位置模拟右键。
    fn right_click(&mut self);

    /// 点击后等待新 UI（菜单 / 对话框 / 新窗口）出现，最多 `timeout`。
    fn wait_new_ui(&mut self, _timeout: std::time::Duration) -> bool {
        false
    }

    /// 采集新出现 UI 的目标（链式扫描，不启用 OCR 兜底）。
    fn collect_chained_targets(&mut self) -> Vec<Target> {
        Vec::new()
    }

    /// 模拟滚轮，单位为滚轮刻度（正值向下 / 向右）。
    fn scroll(&mut self, vertical: i32, horizontal: i32);

    /// 测量一行文字的宽度（物理像素），用于标签与 HUD 的布局。
    fn measure_text(&mut self, text: &str, px: f32) -> f32;

    /// 把一帧绘制指令显示到叠加层上。
    fn present(&mut self, scene: &Scene);

    /// 隐藏叠加层。
    fn hide(&mut self);

    /// 开启/关闭键盘拦截（拦截期间按键只交给 VimGlide）。
    fn set_key_capture(&mut self, enabled: bool);

    /// 上一次采集是否因为目标窗口权限更高而被拒绝。
    fn needs_admin(&self) -> bool {
        false
    }

    /// 进入平台消息循环，把事件转发给 `App`，直到退出。
    fn run(&mut self, app: &mut App) -> Result<(), String>;
}

/// 启动程序（阻塞在消息循环中）。
#[cfg(windows)]
pub fn run(config: Config, config_path: std::path::PathBuf) -> Result<(), String> {
    windows::run(config, config_path)
}

#[cfg(target_os = "linux")]
pub fn run(config: Config, config_path: std::path::PathBuf) -> Result<(), String> {
    linux::run(config, config_path)
}

#[cfg(target_os = "macos")]
pub fn run(config: Config, config_path: std::path::PathBuf) -> Result<(), String> {
    macos::run(config, config_path)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn run(_config: Config, _config_path: std::path::PathBuf) -> Result<(), String> {
    Err("当前平台尚未实现后端".to_string())
}

/// 展示致命错误（Windows 下弹系统消息框，便于无控制台的发行版排查）。
#[cfg(windows)]
pub fn fatal(message: &str) {
    windows::fatal(message)
}

#[cfg(not(windows))]
pub fn fatal(message: &str) {
    eprintln!("{message}");
}
