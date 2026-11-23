//! 配置的读写。首次运行时会在用户配置目录生成带有默认值的 config.toml。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub hotkey: HotkeyConfig,
    pub hint: HintConfig,
    pub hud: HudConfig,
    pub scroll: ScrollConfig,
    pub ocr: OcrConfig,
    /// 选中后用 hjkl 微调光标的步长与加速。
    #[serde(rename = "move")]
    pub movement: MoveConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct HotkeyConfig {
    /// 唤出/退出提示模式的全局热键。
    pub hint: String,
    /// 退出 VimGlide 进程的热键。
    pub quit: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct HintConfig {
    /// 生成提示标签用的字符表（按顺序取用）。
    pub chars: String,
    /// 标签字号（逻辑像素，会按 DPI 缩放）。
    pub font_size: f32,
    pub label_bg: String,
    pub label_fg: String,
    pub label_border: String,
    /// 已匹配标签的底色/前景色。
    pub matched_bg: String,
    pub matched_fg: String,
    /// 已输入前缀在标签中的高亮色。
    pub prefix_fg: String,
    /// 元素描边颜色与透明度（0-255）。
    pub outline: String,
    pub outline_alpha: u8,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct HudConfig {
    pub font_size: f32,
    pub bg: String,
    pub fg: String,
    pub accent: String,
    pub flash: String,
}

/// UIA 采不到元素时，用 Windows 系统 OCR 识别窗口文字作为兜底。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct OcrConfig {
    /// 是否启用 OCR 兜底。
    pub enabled: bool,
    /// UIA 目标少于该数量时才启用 OCR。
    pub min_uia_targets: usize,
    /// OCR 追加目标的数量上限，同时也是目标总数上限。
    pub max_targets: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ScrollConfig {
    /// 方向键每次滚动的滚轮刻度数。
    pub step_notches: i32,
    /// PageUp / PageDown 每次滚动的滚轮刻度数。
    pub page_notches: i32,
}

/// 选中后 hjkl 微调光标的参数。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct MoveConfig {
    /// 每次 hjkl 移动的像素。
    pub step: i32,
    /// Shift + hjkl 每次移动的像素。
    pub shift_step: i32,
    /// 长按自动重复时，每过多少毫秒提升一档。
    pub accel_ms: u64,
    /// 加速的最高倍率。
    pub accel_max: i32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: HotkeyConfig::default(),
            hint: HintConfig::default(),
            hud: HudConfig::default(),
            scroll: ScrollConfig::default(),
            ocr: OcrConfig::default(),
            movement: MoveConfig::default(),
        }
    }
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            hint: "ctrl+shift+space".to_string(),
            quit: "ctrl+shift+alt+q".to_string(),
        }
    }
}

impl Default for HintConfig {
    fn default() -> Self {
        Self {
            chars: "sadfjklewcmpghiormnvcx".to_string(),
            font_size: 16.0,
            label_bg: "#fdf6b266".to_string(),
            label_fg: "#005700".to_string(),
            label_border: "#e2c94f".to_string(),
            matched_bg: "#7ee78799".to_string(),
            matched_fg: "#0b2b13".to_string(),
            prefix_fg: "#b32020".to_string(),
            outline: "#9a9a9a".to_string(),
            outline_alpha: 90,
        }
    }
}

impl Default for HudConfig {
    fn default() -> Self {
        Self {
            font_size: 14.0,
            // 末两位为透明度，和标签背景一样支持半透明。
            bg: "#1f2430b3".to_string(),
            fg: "#e6e6e6".to_string(),
            accent: "#7ee787".to_string(),
            flash: "#ff7b72".to_string(),
        }
    }
}

impl Default for ScrollConfig {
    fn default() -> Self {
        Self {
            step_notches: 3,
            page_notches: 15,
        }
    }
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_uia_targets: 5,
            max_targets: 200,
        }
    }
}

impl Default for MoveConfig {
    fn default() -> Self {
        Self {
            step: 2,
            shift_step: 10,
            accel_ms: 150,
            accel_max: 16,
        }
    }
}

impl Config {
    /// 解析命令行（仅支持 `--config <path>`）并加载配置，同时返回实际使用的路径。
    pub fn load() -> Result<(Config, PathBuf), String> {
        let path = std::env::args()
            .collect::<Vec<_>>()
            .windows(2)
            .find(|w| w[0] == "--config" || w[0] == "-c")
            .map(|w| PathBuf::from(&w[1]))
            .unwrap_or_else(default_path);

        if !path.exists() {
            write_default(&path)?;
        }

        Ok((Config::from_path(&path)?, path))
    }

    /// 从指定路径读取配置（不写默认文件），用于启动与手动重新载入。
    pub fn from_path(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读取配置文件 {} 失败：{e}", path.display()))?;
        let config: Config = toml::from_str(&text)
            .map_err(|e| format!("解析配置文件 {} 失败：{e}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.hint.font_size <= 0.0 {
            return Err("hint.font_size 必须大于 0".to_string());
        }
        if self.hint.chars.trim().is_empty() {
            return Err("hint.chars 不能为空".to_string());
        }
        if self.movement.step < 1 || self.movement.shift_step < 1 {
            return Err("move.step 与 move.shift_step 必须大于 0".to_string());
        }
        if self.movement.accel_ms < 20 || self.movement.accel_max < 1 {
            return Err("move.accel_ms 不能小于 20，move.accel_max 必须大于 0".to_string());
        }
        Ok(())
    }
}

/// 默认配置路径。
pub fn default_path() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("VimGlide")
        .join("config.toml")
}

fn write_default(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建配置目录 {} 失败：{e}", parent.display()))?;
    }
    let body = toml::to_string_pretty(&Config::default())
        .map_err(|e| format!("生成默认配置失败：{e}"))?;
    let header = "# VimGlide 配置\n# 修改后从托盘菜单选择「重新载入配置」即可生效。\n\n";
    std::fs::write(path, format!("{header}{body}"))
        .map_err(|e| format!("写入默认配置 {} 失败：{e}", path.display()))
}
