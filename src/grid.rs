/// Pixel rectangle for a single pane within the window.
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// An N (cols) x M (rows) arrangement of panes. Index = row * cols + col.
#[derive(Clone, Copy, Debug)]
pub struct GridLayout {
    pub cols: usize,
    pub rows: usize,
    pub gap: f32,
    pub pad: f32,
}

impl GridLayout {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            cols: cols.max(1),
            rows: rows.max(1),
            gap: 8.0,
            pad: 10.0,
        }
    }

    pub fn count(&self) -> usize {
        self.cols * self.rows
    }

    /// Compute the pixel rectangle for the pane at the given index, given the
    /// full window size in physical pixels.
    pub fn rect_for(&self, index: usize, win_w: f32, win_h: f32) -> Rect {
        let col = index % self.cols;
        let row = index / self.cols;

        let usable_w = (win_w - 2.0 * self.pad - self.gap * (self.cols as f32 - 1.0)).max(1.0);
        let usable_h = (win_h - 2.0 * self.pad - self.gap * (self.rows as f32 - 1.0)).max(1.0);

        let cell_w = usable_w / self.cols as f32;
        let cell_h = usable_h / self.rows as f32;

        Rect {
            x: self.pad + col as f32 * (cell_w + self.gap),
            y: self.pad + row as f32 * (cell_h + self.gap),
            w: cell_w,
            h: cell_h,
        }
    }
}
