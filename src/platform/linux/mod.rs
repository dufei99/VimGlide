//! Linux 后端占位。
//!
//! 实现 `platform::Backend` 时大致需要的系统能力：
//! - 元素采集：AT-SPI2（通过 `atspi`/D-Bus）或各工具包的无障碍接口；
//! - 鼠标注入：`libei` / `uinput` / X11 XTEST；
//! - 遮罩层：X11 override-redirect 透明窗口，或 Wayland 下的 layer-shell；
//! - 全局热键：X11 XGrabKey / 桌面环境的 portal（Wayland 下限制较多）。

use crate::config::Config;

pub fn run(_config: Config, _config_path: std::path::PathBuf) -> Result<(), String> {
    Err("VimGlide 目前只实现了 Windows 后端，Linux 后端尚未实现".to_string())
}
