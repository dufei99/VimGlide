//! 应用状态机：提示模式、标签匹配、滚动与绘制。
//!
//! 这里不直接调用任何 Windows API，只通过 [`Backend`] 与系统交互。

use std::time::{Duration, Instant};

use crate::config::Config;
use crate::hint::{HintSet, Press};
use crate::model::Rect;
use crate::platform::{Backend, Desktop, Key};
use crate::render::{Color, Scene, TextBlock, TextRun};

/// 选中目标后要执行的动作。
#[derive(Clone, Copy)]
enum ClickAction {
    Left,
    Double,
    Right,
}

/// 短暂显示在 HUD 上的提示信息。
struct Notice {
    text: String,
    until: Instant,
    error: bool,
}

pub struct App {
    config: Config,
    hints: Option<HintSet>,
    notice: Option<Notice>,
    visible: bool,
    /// 虚拟桌面左上角，用于把屏幕坐标换算成画布坐标。
    origin: (i32, i32),
    /// 选中后正在用 hjkl 移动光标：方向、按下开始时间、上一次移动时间。
    moving: Option<(char, Instant, Instant)>,
}

impl App {
    pub fn new(config: Config, desktop: &Desktop) -> Result<Self, String> {
        Ok(Self {
            config,
            hints: None,
            notice: None,
            visible: false,
            origin: (desktop.screen.x, desktop.screen.y),
            moving: None,
        })
    }

    /// 热键切换：显示提示 / 退出提示模式。
    pub fn toggle(&mut self, backend: &mut dyn Backend) {
        if self.visible {
            self.dismiss(backend);
        } else {
            self.activate(backend);
        }
    }

    /// 启动时短暂显示一条提示（例如当前生效的热键）。
    pub fn show_toast(&mut self, backend: &mut dyn Backend, text: String) {
        self.visible = true;
        self.hints = None;
        self.notice = Some(Notice {
            text,
            until: Instant::now() + Duration::from_millis(4000),
            error: false,
        });
        self.render(backend);
    }

    /// 应用重新载入的配置；热键由平台层负责更新。
    pub fn apply_config(&mut self, backend: &mut dyn Backend, config: Config) {
        self.config = config;
        if self.visible {
            self.render(backend);
        }
    }

    /// 显示一条临时消息（成功或错误）。
    pub fn show_message(&mut self, backend: &mut dyn Backend, text: String, error: bool) {
        self.visible = true;
        self.hints = None;
        self.notice = Some(Notice {
            text,
            until: Instant::now() + Duration::from_millis(if error { 4000 } else { 1600 }),
            error,
        });
        self.render(backend);
    }

    /// 退出提示模式并隐藏遮罩。
    pub fn dismiss(&mut self, backend: &mut dyn Backend) {
        self.visible = false;
        self.hints = None;
        self.notice = None;
        self.moving = None;
        backend.set_key_capture(false);
        backend.hide();
    }

    fn activate(&mut self, backend: &mut dyn Backend) {
        self.visible = true;
        self.hints = None;
        // 先记录前台窗口：遮罩显示时可能为了盖住任务栏而抢占前台。
        backend.begin_hints();
        self.notice = Some(Notice {
            text: "正在识别可点击元素 ...".to_string(),
            until: Instant::now() + Duration::from_secs(10),
            error: false,
        });
        backend.set_key_capture(true);
        // 先画出扫描提示，避免 UIA 遍历期间屏幕毫无反馈。
        self.render(backend);

        let targets = backend.collect_targets();
        if targets.is_empty() {
            let needs_admin = backend.needs_admin();
            self.notice = Some(Notice {
                text: if needs_admin {
                    "前台窗口以管理员权限运行，请从托盘菜单以管理员身份重启".to_string()
                } else {
                    "没有找到可点击的元素".to_string()
                },
                until: Instant::now() + Duration::from_millis(if needs_admin { 2600 } else { 1200 }),
                error: true,
            });
        } else {
            let hints = HintSet::new(targets, &self.config.hint.chars);
            // 标签出现时先给出完整用法提示，3 秒后自动消失，按 ? 可再次显示。
            self.notice = Some(Notice {
                text: Self::help_message(hints.len()),
                until: Instant::now() + Duration::from_secs(3),
                error: false,
            });
            self.hints = Some(hints);
        }
        self.render(backend);
    }

    /// 帮助提示文案：目标数量 + 可用按键。
    fn help_message(count: usize) -> String {
        format!(
            "找到 {count} 个标签   输入标签后按 hjkl 微调光标   空格单击   shift+空格双击   ctrl+空格右击   esc 取消"
        )
    }

    /// 显示帮助提示（? 键），3 秒后自动消失。
    fn show_help(&mut self, backend: &mut dyn Backend) {
        let Some(count) = self.hints.as_ref().map(|hints| hints.len()) else {
            return;
        };
        self.notice = Some(Notice {
            text: Self::help_message(count),
            until: Instant::now() + Duration::from_secs(3),
            error: false,
        });
        self.render(backend);
    }

    /// 处理一次按键。捕获期间所有按键都会被吞掉，无需返回值。
    pub fn on_key(&mut self, backend: &mut dyn Backend, key: Key) {
        if !self.visible {
            return;
        }
        match key {
            // 有选中项时先取消选中，再按一次才退出提示模式。
            Key::Escape => {
                if self.hints.as_ref().and_then(|hints| hints.selected()).is_some() {
                    if let Some(hints) = &mut self.hints {
                        hints.clear();
                    }
                    self.moving = None;
                    self.render(backend);
                } else {
                    self.dismiss(backend);
                }
            }
            Key::Backspace => {
                if let Some(hints) = &mut self.hints {
                    hints.backspace();
                }
                self.moving = None;
                // 输入时收起帮助，让位给筛选进度。
                self.notice = None;
                self.render(backend);
            }
            // ? 重新显示帮助提示。
            Key::Char('?') => self.show_help(backend),
            Key::Space { shift, ctrl } => {
                let action = if ctrl {
                    ClickAction::Right
                } else if shift {
                    ClickAction::Double
                } else {
                    ClickAction::Left
                };
                self.commit(backend, action);
            }
            Key::Char(c) => self.handle_char(backend, c),
            Key::Up => self.scroll(backend, -self.config.scroll.step_notches, 0),
            Key::Down => self.scroll(backend, self.config.scroll.step_notches, 0),
            Key::Left => self.scroll(backend, 0, -self.config.scroll.step_notches),
            Key::Right => self.scroll(backend, 0, self.config.scroll.step_notches),
            Key::PageUp => self.scroll(backend, -self.config.scroll.page_notches, 0),
            Key::PageDown => self.scroll(backend, self.config.scroll.page_notches, 0),
            Key::Home => self.scroll(backend, -self.config.scroll.page_notches * 4, 0),
            Key::End => self.scroll(backend, self.config.scroll.page_notches * 4, 0),
            Key::Enter | Key::Tab | Key::Other => {}
        }
    }

    fn handle_char(&mut self, backend: &mut dyn Backend, c: char) {
        // 输入标签时收起帮助提示。
        self.notice = None;
        // 选中后 hjkl 用来微调光标位置，其它字母则放弃选择、重新输入。
        if self.hints.as_ref().is_some_and(|hints| hints.selected().is_some()) {
            let dir = c.to_ascii_lowercase();
            if matches!(dir, 'h' | 'j' | 'k' | 'l') {
                self.nudge_cursor(backend, dir, c.is_ascii_uppercase());
                return;
            }
            if let Some(hints) = &mut self.hints {
                hints.clear();
            }
            self.moving = None;
        }
        let empty_before = self
            .hints
            .as_ref()
            .map(|hints| hints.buffer().is_empty())
            .unwrap_or(false);
        let lower = c.to_ascii_lowercase();

        let mut selected = None;
        let mut invalid = false;
        if let Some(hints) = &mut self.hints {
            match hints.press(lower) {
                Press::Selected(index) => selected = Some(index),
                Press::Updated => {}
                Press::Invalid => invalid = true,
            }
        }

        if let Some(index) = selected {
            self.select(backend, index);
            return;
        }

        if invalid {
            // 缓冲区为空时，没有匹配的字母退化为 Vim 风格滚动键。
            if empty_before && self.vim_scroll(backend, c) {
                self.render(backend);
                return;
            }
            if let Some(hints) = &mut self.hints {
                hints.clear();
            }
            self.notice = Some(Notice {
                text: format!("没有以 '{lower}' 开头的标签"),
                until: Instant::now() + Duration::from_millis(700),
                error: true,
            });
        }
        self.render(backend);
    }

    /// 标签输入完整后：高亮目标，并把鼠标移到点击位置，等待空格或 hjkl 微调。
    fn select(&mut self, backend: &mut dyn Backend, index: usize) {
        let Some(hints) = &self.hints else {
            return;
        };
        // 点击点靠左：很多控件的有效区域在左侧的图标/文字处，而矩形可能很宽。
        let rect = hints.target(index).rect;
        let inset = (rect.w / 5).clamp(1, 24);
        let (x, y) = (rect.x + inset, rect.y + rect.h / 2);
        if let Some(hints) = &mut self.hints {
            hints.select(index);
        }
        self.moving = None;
        backend.move_cursor(x, y);
        self.render(backend);
    }

    /// 选中后按 hjkl 微调光标：步长与加速由 `[move]` 配置，长按逐渐加速。
    fn nudge_cursor(&mut self, backend: &mut dyn Backend, dir: char, shift: bool) {
        let now = Instant::now();
        let cfg = &self.config.movement;
        // 只有键盘自动重复（事件间隔很短）才加速，手动连点保持原步长。
        let (started, mult) = match self.moving {
            Some((last, started, at)) if last == dir => {
                if now.duration_since(at) < Duration::from_millis(80) {
                    let held = now.duration_since(started).as_millis() as i64;
                    let interval = cfg.accel_ms.max(20) as i64;
                    (started, (1 + held / interval).clamp(1, cfg.accel_max as i64))
                } else {
                    (now, 1)
                }
            }
            _ => (now, 1),
        };
        self.moving = Some((dir, started, now));
        let step = if shift { cfg.shift_step } else { cfg.step } * mult as i32;
        let (dx, dy) = match dir {
            'h' => (-step, 0),
            'l' => (step, 0),
            'k' => (0, -step),
            'j' => (0, step),
            _ => return,
        };
        backend.nudge_cursor(dx, dy);
    }

    /// j/k/h/l/d/u/g/G 滚动；返回是否命中。
    fn vim_scroll(&mut self, backend: &mut dyn Backend, c: char) -> bool {
        let step = self.config.scroll.step_notches;
        let page = self.config.scroll.page_notches;
        let (vertical, horizontal) = match c {
            'j' => (step, 0),
            'k' => (-step, 0),
            'h' => (0, -step),
            'l' => (0, step),
            'd' => (page, 0),
            'u' => (-page, 0),
            'g' => (-page * 4, 0),
            'G' => (page * 4, 0),
            _ => return false,
        };
        backend.scroll(vertical, horizontal);
        true
    }

    fn scroll(&mut self, backend: &mut dyn Backend, vertical: i32, horizontal: i32) {
        backend.scroll(vertical, horizontal);
    }

    /// 对选中的目标执行动作：空格单击，Shift+空格双击，Ctrl+空格右击。
    ///
    /// 点击位置就是当前光标位置，因此 hjkl 微调后的坐标会被直接使用。
    /// 点击后若弹出菜单 / 对话框 / 新窗口，会自动为新的 UI 打标签，便于连续操作。
    fn commit(&mut self, backend: &mut dyn Backend, action: ClickAction) {
        if self.hints.as_ref().and_then(|hints| hints.selected()).is_none() {
            return;
        }
        // 先隐藏标签再点击，避免残影；键盘捕获暂时保持，等新 UI 出现。
        backend.hide();
        std::thread::sleep(Duration::from_millis(15));
        match action {
            ClickAction::Left => backend.click(false),
            ClickAction::Double => backend.click(true),
            ClickAction::Right => backend.right_click(),
        }

        if !backend.wait_new_ui(Duration::from_millis(600)) {
            self.dismiss(backend);
            return;
        }
        let targets = backend.collect_chained_targets();
        if targets.is_empty() {
            self.dismiss(backend);
            return;
        }
        let hints = HintSet::new(targets, &self.config.hint.chars);
        self.notice = Some(Notice {
            text: Self::help_message(hints.len()),
            until: Instant::now() + Duration::from_secs(3),
            error: false,
        });
        self.hints = Some(hints);
        self.moving = None;
        self.render(backend);
    }

    /// 定时器回调：处理临时提示的自动消失。
    pub fn on_tick(&mut self, backend: &mut dyn Backend) {
        if !self.visible {
            return;
        }
        let Some(notice) = &self.notice else {
            return;
        };
        if Instant::now() < notice.until {
            return;
        }
        let message_only = self.hints.is_none();
        self.notice = None;
        if message_only {
            self.dismiss(backend);
        } else {
            self.render(backend);
        }
    }

    fn render(&mut self, backend: &mut dyn Backend) {
        if !self.visible {
            return;
        }
        let (scale, screen) = {
            let desktop = backend.desktop();
            (desktop.scale, desktop.screen)
        };
        let config = &self.config;
        let hints = self.hints.as_ref();
        let notice = self.notice.as_ref();
        let origin = self.origin;
        // 先算好布局，再交给平台后端用 Direct2D 绘制，不再有整屏 CPU 位图。
        let mut scene = Scene::new(screen.w.max(1) as u32, screen.h.max(1) as u32);
        if let Some(hints) = hints {
            Self::paint_hints(&mut scene, backend, config, hints, scale, origin);
        }
        Self::paint_hud(&mut scene, backend, config, scale, hints, notice);
        backend.present(&scene);
    }

    fn paint_hints(
        scene: &mut Scene,
        backend: &mut dyn Backend,
        config: &Config,
        hints: &HintSet,
        scale: f32,
        origin: (i32, i32),
    ) {
        let cfg = &config.hint;
        let font_px = cfg.font_size * scale;
        let pad_x = 4.0 * scale;
        let pad_y = 2.0 * scale;
        let radius = 3.0 * scale;

        let label_bg = Color::hex(&cfg.label_bg, Color::rgb(253, 246, 178));
        let label_fg = Color::hex(&cfg.label_fg, Color::rgb(32, 32, 32));
        let label_border = Color::hex(&cfg.label_border, Color::rgb(226, 201, 79));
        let matched_bg = Color::hex(&cfg.matched_bg, Color::rgb(126, 231, 135));
        let matched_fg = Color::hex(&cfg.matched_fg, Color::rgb(11, 43, 19));
        let prefix_fg = Color::hex(&cfg.prefix_fg, Color::rgb(179, 32, 32));
        let outline = Color::hex(&cfg.outline, Color::rgb(154, 154, 154)).with_alpha(cfg.outline_alpha);

        let buffer = hints.buffer();
        let selected = hints.selected();
        // 选中后保留全部标签，只高亮选中项；未选中时才按输入前缀过滤。
        let filtering = !buffer.is_empty() && selected.is_none();
        let canvas_w = scene.width as i32;
        let canvas_h = scene.height as i32;
        let mut placed: Vec<Rect> = Vec::new();

        for (index, label) in hints.labels().iter().enumerate() {
            if filtering && !hints.is_matched(index) {
                continue;
            }
            let emphasize = selected == Some(index) || filtering;
            let target = hints.target(index);
            let rect = Rect::new(
                target.rect.x - origin.0,
                target.rect.y - origin.1,
                target.rect.w,
                target.rect.h,
            );
            if rect.is_empty() {
                continue;
            }

            let thickness = if emphasize { 2.0 * scale } else { 1.0 * scale };
            let stroke = if emphasize { matched_bg.with_alpha(230) } else { outline };
            scene.stroke_round_rect(rect, 2.0 * scale, thickness, stroke);

            let (bg, fg) = if emphasize {
                (matched_bg, matched_fg)
            } else {
                (label_bg, label_fg)
            };
            let text_w = backend.measure_text(label, font_px);
            let w = (text_w + pad_x * 2.0).round() as i32;
            let h = (font_px + pad_y * 2.0).round() as i32;
            let box_rect = place_label(rect, w, h, &placed, canvas_w, canvas_h, scale);
            placed.push(box_rect);

            scene.fill_round_rect(box_rect, radius, label_border.with_alpha(bg.a));
            scene.fill_round_rect(box_rect.inset(1, 1), radius - 1.0, bg);

            // 标签底色半透明，给字母描一圈对比色边，保证可读性。
            // 选中项整块高亮，不再按前缀拆分着色。
            let split = if selected.is_some() {
                0
            } else {
                buffer.len().min(label.len())
            };
            let runs = if filtering && split > 0 {
                let (prefix, rest) = label.split_at(split);
                vec![
                    TextRun {
                        text: prefix.to_string(),
                        color: prefix_fg,
                        halo: Some(halo_for(prefix_fg)),
                    },
                    TextRun {
                        text: rest.to_string(),
                        color: fg,
                        halo: Some(halo_for(fg)),
                    },
                ]
            } else {
                vec![TextRun {
                    text: label.clone(),
                    color: fg,
                    halo: Some(halo_for(fg)),
                }]
            };
            scene.text(TextBlock {
                x: box_rect.x as f32 + pad_x,
                y: box_rect.y as f32,
                w: box_rect.w as f32 - pad_x * 2.0,
                h: box_rect.h as f32,
                px: font_px,
                runs,
            });
        }
    }

    fn paint_hud(
        scene: &mut Scene,
        backend: &mut dyn Backend,
        config: &Config,
        scale: f32,
        hints: Option<&HintSet>,
        notice: Option<&Notice>,
    ) {
        let cfg = &config.hud;
        let font_px = cfg.font_size * scale;

        let (message, color) = if let Some(notice) = notice {
            let color = if notice.error {
                Color::hex(&cfg.flash, Color::rgb(255, 123, 114))
            } else {
                Color::hex(&cfg.accent, Color::rgb(126, 231, 135))
            };
            (notice.text.clone(), color)
        } else if let Some(hints) = hints {
            // 完整帮助由 Notice 承担并在 3 秒后消失；这里只显示输入前缀时的筛选进度。
            if hints.selected().is_some() || hints.buffer().is_empty() {
                return;
            }
            let message = format!(
                "[{}]   {} / {}   esc 退出",
                hints.buffer(),
                hints.matched_count(),
                hints.len()
            );
            (message, Color::hex(&cfg.fg, Color::rgb(230, 230, 230)))
        } else {
            return;
        };

        let pad_x = 12.0 * scale;
        let width = backend.measure_text(&message, font_px);
        let h = (font_px + 14.0 * scale).round() as i32;
        let w = (width + pad_x * 2.0).round() as i32;
        let x = ((scene.width as i32 - w) / 2).max(0);
        let y = (scene.height as i32 - h - (72.0 * scale) as i32).max(0);
        let rect = Rect::new(x, y, w, h);

        // 背景支持 #RRGGBBAA；只写了 #RRGGBB 时使用 70% 不透明度，和标签保持一致的半透明观感。
        let bg = Color::hex(&cfg.bg, Color::rgb(31, 36, 48));
        let bg_alpha = if cfg.bg.trim().trim_start_matches('#').len() == 8 {
            bg.a
        } else {
            0xb3
        };

        scene.fill_round_rect(rect, 9.0 * scale, bg.with_alpha(bg_alpha));
        scene.text(TextBlock {
            x: rect.x as f32 + pad_x,
            y: rect.y as f32,
            w: width,
            h: rect.h as f32,
            px: font_px,
            runs: vec![TextRun {
                text: message,
                color,
                halo: None,
            }],
        });
    }
}

/// 按文字亮度选择描边色：深色字配白边，浅色字配黑边。
fn halo_for(fg: Color) -> Color {
    let luma = 0.299 * fg.r as f32 + 0.587 * fg.g as f32 + 0.114 * fg.b as f32;
    if luma < 128.0 {
        Color::rgba(255, 255, 255, 235)
    } else {
        Color::rgba(0, 0, 0, 200)
    }
}

/// 把标签放在元素左上角，尽量避开已经放置的标签。
fn place_label(
    target: Rect,
    w: i32,
    h: i32,
    placed: &[Rect],
    canvas_w: i32,
    canvas_h: i32,
    scale: f32,
) -> Rect {
    let gap = (3.0 * scale).round() as i32;
    let max_x = (canvas_w - w).max(0);
    let max_y = (canvas_h - h).max(0);
    let clamp = |x: i32, y: i32| Rect::new(x.clamp(0, max_x), y.clamp(0, max_y), w, h);
    let free = |rect: &Rect| !placed.iter().any(|p| p.intersection(rect).is_some());

    let candidates = [
        clamp(target.x + 1, target.y + 1),
        clamp(target.right() - w - 1, target.y + 1),
        clamp(target.x + 1, target.bottom() - h - 1),
        clamp(target.x + 1, target.y - h - gap),
        clamp(target.x + 1, target.bottom() + gap),
        clamp(target.center().0 - w / 2, target.center().1 - h / 2),
    ];
    for candidate in candidates {
        if free(&candidate) {
            return candidate;
        }
    }

    let start = candidates[0];
    for i in 1..=8 {
        let candidate = clamp(start.x, start.y + i * (h + gap));
        if free(&candidate) {
            return candidate;
        }
    }
    start
}
