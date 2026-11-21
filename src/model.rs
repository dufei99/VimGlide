//! 平台无关的基础数据结构。

/// 屏幕坐标矩形（物理像素，左上角为原点）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub fn area(&self) -> i64 {
        self.w as i64 * self.h as i64
    }

    /// 与另一个矩形求交，不相交时返回 None。
    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        (right > x && bottom > y).then(|| Rect::new(x, y, right - x, bottom - y))
    }

    /// 向内收缩（负值表示外扩）。
    pub fn inset(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(
            self.x + dx,
            self.y + dy,
            (self.w - dx * 2).max(0),
            (self.h - dy * 2).max(0),
        )
    }
}

/// 一个可点击目标。
#[derive(Clone, Debug)]
pub struct Target {
    pub rect: Rect,
    /// 元素的无障碍名称，用于生成助记标签。
    pub name: String,
}
