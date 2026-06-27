/// Pixel rectangle for a single pane within the window.
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// An N (cols) x M (rows) arrangement of panes. Index = row * cols + col.
///
/// Columns and rows can be individually resized: `col_fracs`/`row_fracs` hold
/// each track's fraction of the usable space (each vector sums to 1.0). They
/// default to equal and are reset whenever the grid dimensions change.
#[derive(Clone, Debug)]
pub struct GridLayout {
    pub cols: usize,
    pub rows: usize,
    pub gap: f32,
    pub pad: f32,
    col_fracs: Vec<f32>,
    row_fracs: Vec<f32>,
}

/// A draggable divider between two tracks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Divider {
    /// Vertical divider between column `i` and `i+1`.
    Col(usize),
    /// Horizontal divider between row `i` and `i+1`.
    Row(usize),
}

impl GridLayout {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            gap: 8.0,
            pad: 10.0,
            col_fracs: equal_fracs(cols),
            row_fracs: equal_fracs(rows),
        }
    }

    pub fn count(&self) -> usize {
        self.cols * self.rows
    }

    fn usable(&self, win_w: f32, win_h: f32) -> (f32, f32) {
        let usable_w = (win_w - 2.0 * self.pad - self.gap * (self.cols as f32 - 1.0)).max(1.0);
        let usable_h = (win_h - 2.0 * self.pad - self.gap * (self.rows as f32 - 1.0)).max(1.0);
        (usable_w, usable_h)
    }

    /// Compute the pixel rectangle for the pane at the given index, given the
    /// full window size in physical pixels.
    pub fn rect_for(&self, index: usize, win_w: f32, win_h: f32) -> Rect {
        let col = index % self.cols;
        let row = index / self.cols;
        let (usable_w, usable_h) = self.usable(win_w, win_h);

        // Sum of fractions before this track gives its start offset.
        let x_frac_before: f32 = self.col_fracs[..col].iter().sum();
        let y_frac_before: f32 = self.row_fracs[..row].iter().sum();

        let w = usable_w * self.col_fracs[col];
        let h = usable_h * self.row_fracs[row];
        let x = self.pad + x_frac_before * usable_w + col as f32 * self.gap;
        let y = self.pad + y_frac_before * usable_h + row as f32 * self.gap;

        Rect { x, y, w, h }
    }

    /// Hit-test a point against the gutters between tracks. Returns the divider
    /// the point is over (within `tol` px of a gap centerline), if any. Used to
    /// show a resize cursor and to start a drag.
    pub fn divider_at(&self, px: f32, py: f32, win_w: f32, win_h: f32, tol: f32) -> Option<Divider> {
        let (usable_w, usable_h) = self.usable(win_w, win_h);
        // Vertical dividers (between columns): there are cols-1 of them.
        let mut acc = 0.0f32;
        for i in 0..self.cols.saturating_sub(1) {
            acc += self.col_fracs[i];
            let center = self.pad + acc * usable_w + (i as f32 + 0.5) * self.gap;
            if (px - center).abs() <= tol {
                // Only count if py is within the grid's vertical extent.
                if py >= self.pad && py <= win_h - self.pad {
                    return Some(Divider::Col(i));
                }
            }
        }
        let mut acc = 0.0f32;
        for i in 0..self.rows.saturating_sub(1) {
            acc += self.row_fracs[i];
            let center = self.pad + acc * usable_h + (i as f32 + 0.5) * self.gap;
            if (py - center).abs() <= tol {
                if px >= self.pad && px <= win_w - self.pad {
                    return Some(Divider::Row(i));
                }
            }
        }
        None
    }

    /// Drag a divider to an absolute pixel position. Adjusts the two adjacent
    /// tracks' fractions, keeping every track above a sane minimum so panes
    /// never collapse to nothing.
    pub fn drag_divider(&mut self, div: Divider, px: f32, py: f32, win_w: f32, win_h: f32) {
        const MIN_FRAC: f32 = 0.05;
        match div {
            Divider::Col(i) => {
                let (usable_w, _) = self.usable(win_w, win_h);
                let before: f32 = self.col_fracs[..i].iter().sum();
                // Desired fraction position of the boundary between i and i+1.
                let pos = ((px - self.pad - (i as f32 + 0.5) * self.gap) / usable_w).clamp(0.0, 1.0);
                let pair = self.col_fracs[i] + self.col_fracs[i + 1];
                let mut left = pos - before;
                left = left.clamp(MIN_FRAC, pair - MIN_FRAC);
                self.col_fracs[i] = left;
                self.col_fracs[i + 1] = pair - left;
            }
            Divider::Row(i) => {
                let (_, usable_h) = self.usable(win_w, win_h);
                let before: f32 = self.row_fracs[..i].iter().sum();
                let pos = ((py - self.pad - (i as f32 + 0.5) * self.gap) / usable_h).clamp(0.0, 1.0);
                let pair = self.row_fracs[i] + self.row_fracs[i + 1];
                let mut top = pos - before;
                top = top.clamp(MIN_FRAC, pair - MIN_FRAC);
                self.row_fracs[i] = top;
                self.row_fracs[i + 1] = pair - top;
            }
        }
    }
}

fn equal_fracs(n: usize) -> Vec<f32> {
    let n = n.max(1);
    vec![1.0 / n as f32; n]
}
