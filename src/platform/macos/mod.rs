//! macOS 后端占位。
//!
//! 实现 `platform::Backend` 时大致需要的系统能力：
//! - 元素采集：Accessibility API（AXUIElement），需要「辅助功能」权限；
//! - 鼠标注入：CGEvent；
//! - 遮罩层：无边框透明 NSWindow（`ignoresMouseEvents = true`）；
//! - 全局热键：Carbon `RegisterEventHotKey` 或 CGEventTap。

use crate::config::Config;

pub fn run(_config: Config, _config_path: std::path::PathBuf) -> Result<(), String> {
    Err("VimGlide 目前只实现了 Windows 后端，macOS 后端尚未实现".to_string())
}
