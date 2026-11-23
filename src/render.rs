//! 颜色、遮罩绘制指令与极简软件渲染器。
//!
//! 遮罩层由平台后端用 Direct2D 直接绘制 [`Scene`]；这里的 [`Canvas`]
//! 只用于需要 CPU 位图的场景（例如 32x32 的托盘图标）。

use fontdue::{Font, FontSettings};

use crate::model::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::rgba(r, g, b, 255)
    }

    pub fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    /// 解析 `#RRGGBB` / `#RRGGBBAA`，失败时返回兜底颜色。
    pub fn hex(text: &str, fallback: Color) -> Color {
        let s = text.trim().trim_start_matches('#');
        if !s.is_ascii() || (s.len() != 6 && s.len() != 8) {
            return fallback;
        }
        let parse = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
        match (parse(0), parse(2), parse(4), s.len()) {
            (Some(r), Some(g), Some(b), 6) => Color::rgb(r, g, b),
            (Some(r), Some(g), Some(b), _) => match parse(6) {
                Some(a) => Color::rgba(r, g, b, a),
                None => fallback,
            },
            _ => fallback,
        }
    }
}

/// 一段文字（同一行内的一个着色片段）。
pub struct TextRun {
    pub text: String,
    pub color: Color,
    /// 描边色；`None` 表示不描边。
    pub halo: Option<Color>,
}

/// 一段在给定矩形内垂直居中的文字。
///
/// 坐标与尺寸都是物理像素（DPI 缩放由调用方预先算进字号）。
pub struct TextBlock {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub px: f32,
    pub runs: Vec<TextRun>,
}

/// 一条绘制指令。`App` 只负责布局，交给平台后端光栅化。
pub enum Op {
    FillRoundRect {
        rect: Rect,
        radius: f32,
        color: Color,
    },
    StrokeRoundRect {
        rect: Rect,
        radius: f32,
        thickness: f32,
        color: Color,
    },
    Text(TextBlock),
}

/// 一帧的绘制指令列表。
///
/// 尺寸与窗口的物理像素一致；后端不需要额外的整屏 CPU 缓冲区。
pub struct Scene {
    pub width: u32,
    pub height: u32,
    ops: Vec<Op>,
}

impl Scene {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            ops: Vec::new(),
        }
    }

    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    pub fn fill_round_rect(&mut self, rect: Rect, radius: f32, color: Color) {
        self.ops.push(Op::FillRoundRect {
            rect,
            radius,
            color,
        });
    }

    pub fn stroke_round_rect(
        &mut self,
        rect: Rect,
        radius: f32,
        thickness: f32,
        color: Color,
    ) {
        self.ops.push(Op::StrokeRoundRect {
            rect,
            radius,
            thickness,
            color,
        });
    }

    pub fn text(&mut self, block: TextBlock) {
        self.ops.push(Op::Text(block));
    }
}

/// 一块 RGBA8 画布，直接借用平台提供的像素缓冲，避免额外的整屏拷贝。
pub struct Canvas<'a> {
    width: u32,
    height: u32,
    pixels: &'a mut [u8],
}

impl<'a> Canvas<'a> {
    /// `pixels` 长度需为 `width * height * 4`。
    pub fn new(pixels: &'a mut [u8], width: u32, height: u32) -> Self {
        debug_assert_eq!(pixels.len(), width as usize * height as usize * 4);
        Self {
            width,
            height,
            pixels,
        }
    }

    fn blend(&mut self, x: i32, y: i32, color: Color, coverage: f32) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let cover = coverage * (color.a as f32 / 255.0);
        if cover <= 0.0 {
            return;
        }
        let index = (y as usize * self.width as usize + x as usize) * 4;
        let dst_a = self.pixels[index + 3] as f32 / 255.0;
        let out_a = cover + dst_a * (1.0 - cover);
        if out_a <= f32::EPSILON {
            return;
        }
        for (channel, src) in [color.r, color.g, color.b].iter().enumerate() {
            let dst = self.pixels[index + channel] as f32 / 255.0;
            let out = (*src as f32 / 255.0 * cover + dst * dst_a * (1.0 - cover)) / out_a;
            self.pixels[index + channel] = (out * 255.0).round().clamp(0.0, 255.0) as u8;
        }
        self.pixels[index + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
    }

    /// 圆角矩形的有符号距离（像素中心采样）。
    fn rounded_rect_distance(rect: Rect, radius: f32, px: i32, py: i32) -> f32 {
        let hw = rect.w as f32 / 2.0;
        let hh = rect.h as f32 / 2.0;
        let cx = rect.x as f32 + hw;
        let cy = rect.y as f32 + hh;
        let r = radius.max(0.0).min(hw).min(hh);
        let dx = ((px as f32 + 0.5) - cx).abs() - (hw - r);
        let dy = ((py as f32 + 0.5) - cy).abs() - (hh - r);
        let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
        outside + dx.max(dy).min(0.0) - r
    }

    pub fn fill_round_rect(&mut self, rect: Rect, radius: f32, color: Color) {
        if rect.is_empty() {
            return;
        }
        // 抗锯齿需要包含边缘外的 1 像素。
        for y in (rect.y - 1)..(rect.bottom() + 1) {
            for x in (rect.x - 1)..(rect.right() + 1) {
                let d = Self::rounded_rect_distance(rect, radius, x, y);
                let coverage = (0.5 - d).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    self.blend(x, y, color, coverage);
                }
            }
        }
    }

    pub fn stroke_round_rect(&mut self, rect: Rect, radius: f32, thickness: f32, color: Color) {
        if rect.is_empty() {
            return;
        }
        let half = thickness / 2.0;
        for y in (rect.y - 1)..(rect.bottom() + 1) {
            for x in (rect.x - 1)..(rect.right() + 1) {
                let d = Self::rounded_rect_distance(rect, radius, x, y).abs();
                let coverage = (half - d + 0.5).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    self.blend(x, y, color, coverage);
                }
            }
        }
    }
}

/// 基于 fontdue 的文字绘制。
pub struct TextRenderer {
    font: Font,
}

impl TextRenderer {
    pub fn new(font_data: Vec<u8>) -> Option<Self> {
        let font = Font::from_bytes(font_data, FontSettings::default()).ok()?;
        Some(Self { font })
    }

    pub fn measure(&self, text: &str, px: f32) -> f32 {
        text.chars()
            .map(|c| self.font.metrics(c, px).advance_width)
            .sum()
    }

    /// 字体是否包含该字符（用于托盘图标的符号字形）。
    pub fn has_glyph(&self, c: char) -> bool {
        self.font.lookup_glyph_index(c) != 0
    }

    /// 字形外框相对基线的位置：`(xmin, ymin, width, height)`。
    pub fn glyph_bounds(&self, c: char, px: f32) -> (i32, i32, i32, i32) {
        let m = self.font.metrics(c, px);
        (m.xmin, m.ymin, m.width as i32, m.height as i32)
    }

    /// 以基线为基准绘制文本。
    pub fn draw(&self, canvas: &mut Canvas<'_>, text: &str, x: f32, baseline: f32, px: f32, color: Color) {
        let mut pen = x;
        for c in text.chars() {
            let (metrics, bitmap) = self.font.rasterize(c, px);
            if metrics.width > 0 && metrics.height > 0 {
                let ox = pen.round() as i32 + metrics.xmin;
                let oy = baseline.round() as i32 - (metrics.ymin + metrics.height as i32);
                for row in 0..metrics.height {
                    for col in 0..metrics.width {
                        let coverage = bitmap[row * metrics.width + col] as f32 / 255.0;
                        if coverage > 0.0 {
                            canvas.blend(ox + col as i32, oy + row as i32, color, coverage);
                        }
                    }
                }
            }
            pen += metrics.advance_width;
        }
    }

}
