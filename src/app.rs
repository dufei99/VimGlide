//! 应用状态机：提示模式、标签匹配、滚动与绘制。
//!
//! 这里不直接调用任何 Windows API，只通过 [`Backend`] 与系统交互。

use std::time::{Duration, Instant};

use crate::config::Config;
use crate::hint::{HintSet, Press};
use crate::model::Rect;
use crate::platform::{Backend, Desktop, Key};
use crate::render::{Canvas, Color, TextRenderer};

/// 短暂显示在 HUD 上的提示信息。
struct Notice {
    text: String,
    until: Instant,
    error: bool,
}

pub struct App {
    config: Config,
    canvas: Canvas,
    text: TextRenderer,
    hints: Option<HintSet>,
    notice: Option<Notice>,
    visible: bool,
    /// 虚拟桌面左上角，用于把屏幕坐标换算成画布坐标。
    origin: (i32, i32),
}

impl App {
    pub fn new(config: Config, desktop: &Desktop) -> Result<Self, String> {
        let text = TextRenderer::new(desktop.font.clone())
            .ok_or_else(|| "无法解析系统字体用于绘制提示".to_string())?;
        Ok(Self {
            canvas: Canvas::new(
                desktop.screen.w.max(1) as u32,
                desktop.screen.h.max(1) as u32,
            ),
            config,
            text,
            hints: None,
            notice: None,
            visible: false,
            origin: (desktop.screen.x, desktop.screen.y),
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

    /// 退出提示模式并隐藏遮罩。
    pub fn dismiss(&mut self, backend: &mut dyn Backend) {
        self.visible = false;
        self.hints = None;
        self.notice = None;
        backend.set_key_capture(false);
        backend.hide();
        // 释放整屏画布（约 8MB/1080p），下次显示时再分配。
        self.canvas.release();
    }

    fn activate(&mut self, backend: &mut dyn Backend) {
        self.visible = true;
        self.hints = None;
        self.notice = Some(Notice {
            text: "VimGlide: scanning clickable elements ...".to_string(),
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
                    "VimGlide: window runs as admin - restart as administrator from tray menu"
                        .to_string()
                } else {
                    "VimGlide: no clickable elements found".to_string()
                },
                until: Instant::now() + Duration::from_millis(if needs_admin { 2600 } else { 1200 }),
                error: true,
            });
        } else {
            self.hints = Some(HintSet::new(targets, &self.config.hint.chars));
            self.notice = None;
        }
        self.render(backend);
    }

    /// 处理一次按键。捕获期间所有按键都会被吞掉，无需返回值。
    pub fn on_key(&mut self, backend: &mut dyn Backend, key: Key) {
        if !self.visible {
            return;
        }
        match key {
            Key::Escape => self.dismiss(backend),
            Key::Backspace => {
                if let Some(hints) = &mut self.hints {
                    hints.backspace();
                }
                self.render(backend);
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
        if c == ' ' {
            return;
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
                text: format!("VimGlide: no hint starts with '{lower}'"),
                until: Instant::now() + Duration::from_millis(700),
                error: true,
            });
        }
        self.render(backend);
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

    fn select(&mut self, backend: &mut dyn Backend, index: usize) {
        let Some(hints) = &self.hints else {
            return;
        };
        let (x, y) = hints.target(index).rect.center();
        self.dismiss(backend);
        // 等遮罩消失一帧再点击，避免视觉上的残影。
        std::thread::sleep(Duration::from_millis(15));
        backend.click(x, y);
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
        self.canvas
            .ensure_size(screen.w.max(1) as u32, screen.h.max(1) as u32);
        self.canvas.clear();
        if let Some(hints) = &self.hints {
            Self::paint_hints(
                &mut self.canvas,
                &self.text,
                &self.config,
                hints,
                scale,
                self.origin,
            );
        }
        Self::paint_hud(
            &mut self.canvas,
            &self.text,
            &self.config,
            scale,
            self.hints.as_ref(),
            self.notice.as_ref(),
        );
        backend.present(&self.canvas);
    }

    fn paint_hints(
        canvas: &mut Canvas,
        text: &TextRenderer,
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
        let filtering = !buffer.is_empty();
        let canvas_w = canvas.width() as i32;
        let canvas_h = canvas.height() as i32;
        let mut placed: Vec<Rect> = Vec::new();

        for (index, label) in hints.labels().iter().enumerate() {
            let matched = hints.is_matched(index);
            if filtering && !matched {
                continue;
            }
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

            let thickness = if filtering { 2.0 * scale } else { 1.0 * scale };
            let stroke = if filtering { matched_bg.with_alpha(230) } else { outline };
            canvas.stroke_round_rect(rect, 2.0 * scale, thickness, stroke);

            let (bg, fg) = if filtering {
                (matched_bg, matched_fg)
            } else {
                (label_bg, label_fg)
            };
            let text_w = text.measure(label, font_px);
            let w = (text_w + pad_x * 2.0).round() as i32;
            let h = (font_px + pad_y * 2.0).round() as i32;
            let box_rect = place_label(rect, w, h, &placed, canvas_w, canvas_h, scale);
            placed.push(box_rect);

            canvas.fill_round_rect(box_rect, radius, label_border.with_alpha(bg.a));
            canvas.fill_round_rect(box_rect.inset(1, 1), radius - 1.0, bg);

            let baseline = box_rect.y as f32 + (box_rect.h as f32 + text.ascent(font_px)) / 2.0;
            let start_x = box_rect.x as f32 + pad_x;
            let split = buffer.len().min(label.len());
            if filtering && split > 0 {
                let (prefix, rest) = label.split_at(split);
                let prefix_w = text.measure(prefix, font_px);
                text.draw(canvas, prefix, start_x, baseline, font_px, prefix_fg);
                text.draw(canvas, rest, start_x + prefix_w, baseline, font_px, fg);
            } else {
                text.draw(canvas, label, start_x, baseline, font_px, fg);
            }
        }
    }

    fn paint_hud(
        canvas: &mut Canvas,
        text: &TextRenderer,
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
            let buffer = hints.buffer();
            let message = if buffer.is_empty() {
                format!(
                    "VimGlide   {} targets   type label   arrows scroll   esc exit",
                    hints.len()
                )
            } else {
                format!(
                    "VimGlide   [{}]   {} / {}   esc exit",
                    buffer,
                    hints.matched_count(),
                    hints.len()
                )
            };
            (message, Color::hex(&cfg.fg, Color::rgb(230, 230, 230)))
        } else {
            return;
        };

        let pad_x = 12.0 * scale;
        let width = text.measure(&message, font_px);
        let h = (font_px + 14.0 * scale).round() as i32;
        let w = (width + pad_x * 2.0).round() as i32;
        let x = ((canvas.width() as i32 - w) / 2).max(0);
        let y = (canvas.height() as i32 - h - (72.0 * scale) as i32).max(0);
        let rect = Rect::new(x, y, w, h);

        canvas.fill_round_rect(
            rect,
            9.0 * scale,
            Color::hex(&cfg.bg, Color::rgb(31, 36, 48)).with_alpha(238),
        );
        let baseline = rect.y as f32 + (rect.h as f32 + text.ascent(font_px)) / 2.0;
        text.draw(canvas, &message, rect.x as f32 + pad_x, baseline, font_px, color);
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
