//! VimGlide：系统级 Vim 风格提示导航。
//!
//! 按 `Ctrl+Shift+Space` 后，当前前台窗口里所有可点击元素都会显示提示标签，
//! 输入标签即可用键盘完成点击；同时支持 Vim 风格的滚动键。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod hint;
mod model;
mod platform;
mod render;

fn main() {
    let (config, config_path) = match config::Config::load() {
        Ok(loaded) => loaded,
        Err(message) => {
            eprintln!("VimGlide: {message}");
            platform::fatal(&message);
            return;
        }
    };

    if let Err(message) = platform::run(config, config_path) {
        eprintln!("VimGlide: {message}");
        platform::fatal(&message);
    }
}
