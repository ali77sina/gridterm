use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{test::TermSize, Config};
use alacritty_terminal::vte::ansi::Processor;
use alacritty_terminal::Term;
use portable_pty::{CommandBuilder, NativePtySystem, PtyPair, PtySize, PtySystem};

/// Callback the PTY reader thread invokes the instant new output arrives, so
/// the UI event loop can wake and redraw immediately (low input latency).
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Tracks whether a pane produced new output since the last redraw. The flag is
/// an atomic so the reader thread can set it without locking.
#[derive(Clone)]
pub struct EventProxy {
    dirty: Arc<AtomicBool>,
}

impl EventProxy {
    fn new() -> Self {
        Self {
            dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns true (and clears the flag) if the terminal produced new output
    /// since the last check. Used to avoid rendering when nothing changed.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        if let Event::Wakeup = event {
            self.mark_dirty();
        }
    }
}

/// One terminal pane: an alacritty Term driven by bytes from a PTY child
/// process. The Term lives behind a FairMutex so the reader thread and the
/// render thread can share it without starving each other.
pub struct PtyTerm {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub proxy: EventProxy,
    writer: Box<dyn std::io::Write + Send>,
    _pair: PtyPair,
    pub cols: usize,
    pub rows: usize,
}

impl PtyTerm {
    /// Spawn a shell in a fresh PTY sized to `cols`x`rows` and wire a reader
    /// thread that feeds the PTY output into the Term state machine. `waker` is
    /// invoked whenever new output arrives so the UI can redraw immediately.
    pub fn spawn(
        cols: usize,
        rows: usize,
        cell_w: f32,
        cell_h: f32,
        waker: Waker,
        telemetry: Option<(u16, usize)>,
    ) -> std::io::Result<Self> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: (cols as f32 * cell_w) as u16,
                pixel_height: (rows as f32 * cell_h) as u16,
            })
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let mut cmd = CommandBuilder::new(shell);
        // Advertise full color support so TUIs (vim, htop, claude code, etc.)
        // enable 24-bit color instead of falling back to monochrome.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "gridterm");
        // Point OTEL-capable coding agents (Claude Code, Codex, ...) at our
        // in-process collector so we can capture their token/cost usage. The
        // pane index is tagged as a resource attribute for attribution.
        if let Some((port, pane)) = telemetry {
            let endpoint = format!("http://127.0.0.1:{port}");
            cmd.env("CLAUDE_CODE_ENABLE_TELEMETRY", "1");
            cmd.env("CODEX_ENABLE_TELEMETRY", "1");
            cmd.env("OTEL_METRICS_EXPORTER", "otlp");
            cmd.env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json");
            cmd.env("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint);
            cmd.env("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", format!("{endpoint}/v1/metrics"));
            cmd.env("OTEL_METRICS_INCLUDE_RESOURCE_ATTRIBUTES", "true");
            cmd.env("OTEL_RESOURCE_ATTRIBUTES", format!("gridterm.pane={pane}"));
            // Flush quickly so the UI cost meter feels live.
            cmd.env("OTEL_METRIC_EXPORT_INTERVAL", "5000");
        }
        if let Ok(dir) = std::env::current_dir() {
            cmd.cwd(dir);
        }

        let _child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let proxy = EventProxy::new();
        let size = TermSize::new(cols, rows);
        let config = Config {
            scrolling_history: 10_000,
            ..Config::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, proxy.clone())));

        // Reader thread: pull raw bytes off the PTY and advance the parser.
        // We drain everything currently available before waking the UI so a
        // flood of output (e.g. a coding agent streaming) coalesces into one
        // redraw instead of one per read syscall.
        let term_reader = term.clone();
        let proxy_reader = proxy.clone();
        std::thread::spawn(move || {
            let mut parser: Processor = Processor::new();
            let mut buf = [0u8; 65536];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut term = term_reader.lock();
                        parser.advance(&mut *term, &buf[..n]);
                        drop(term);
                        proxy_reader.mark_dirty();
                        // Wake the UI event loop; it throttles to the display
                        // refresh, so bursts don't cause redundant frames.
                        waker();
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            term,
            proxy,
            writer,
            _pair: pair,
            cols,
            rows,
        })
    }

    /// Send raw bytes (keystrokes / escape sequences) to the child process.
    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// Scroll the viewport within the scrollback buffer.
    pub fn scroll(&self, scroll: Scroll) {
        self.term.lock().scroll_display(scroll);
    }

    /// Jump back to the live bottom of the buffer (e.g. when typing).
    pub fn scroll_to_bottom(&self) {
        self.term.lock().scroll_display(Scroll::Bottom);
    }

    /// Resize both the PTY and the Term grid when the pane geometry changes.
    pub fn resize(&mut self, cols: usize, rows: usize, cell_w: f32, cell_h: f32) {
        if cols == 0 || rows == 0 || (cols == self.cols && rows == self.rows) {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        let _ = self._pair.master.resize(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: (cols as f32 * cell_w) as u16,
            pixel_height: (rows as f32 * cell_h) as u16,
        });
        let size = TermSize::new(cols, rows);
        self.term.lock().resize(size);
    }

    /// Capture everything needed to render this pane in one lock acquisition:
    /// styled text runs, the cursor cell, and the per-row selection spans.
    pub fn snapshot(&self) -> Snapshot {
        use alacritty_terminal::term::cell::Flags;
        use std::hash::{Hash, Hasher};

        let term = self.term.lock();
        let grid = term.grid();
        let cols = grid.columns();
        let display_offset = grid.display_offset() as i32;

        // Cheap rolling hash of visible content so the renderer can skip work
        // when nothing changed (idle/background panes cost almost nothing).
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        display_offset.hash(&mut hasher);

        // Walk visible cells, grouping consecutive same-color cells into runs.
        // Track which visible row index corresponds to the cursor's grid line,
        // so the cursor block lands on exactly the same row as the text.
        let cursor_line = term.grid().cursor.point.line;
        let mut rows: Vec<RowRuns> = Vec::with_capacity(self.rows);
        let mut cur_line: Option<i32> = None;
        let mut cur_row = RowRuns::default();
        let mut cur_run: Option<Run> = None;
        let mut col: usize = 0;
        let mut row_index: usize = 0; // increments as we finish each line
        let mut cursor_row_idx: Option<usize> = None;

        let flush_run = |row: &mut RowRuns, run: &mut Option<Run>| {
            if let Some(r) = run.take() {
                row.runs.push(r);
            }
        };

        for indexed in grid.display_iter() {
            let line = indexed.point.line.0;
            if cur_line != Some(line) {
                if cur_line.is_some() {
                    flush_run(&mut cur_row, &mut cur_run);
                    rows.push(std::mem::take(&mut cur_row));
                    row_index += 1;
                }
                cur_line = Some(line);
                col = 0;
            }
            // The first cell of the cursor's line tells us its viewport row.
            if indexed.point.line == cursor_line && cursor_row_idx.is_none() {
                cursor_row_idx = Some(row_index);
            }

            let cell = &indexed.cell;
            let flags = cell.flags;
            // Honor reverse video by swapping fg/bg.
            let (mut fg, mut bg) = (
                crate::color::resolve_fg(cell.fg, flags.contains(Flags::BOLD)),
                crate::color::resolve(cell.bg),
            );
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            // Dim attribute darkens the foreground.
            if flags.contains(Flags::DIM) {
                fg = crate::color::dim_rgba(fg);
            }
            let c = cell.c;
            let ch = if c == '\0' { ' ' } else { c };

            // Fold cell into the content hash.
            ch.hash(&mut hasher);
            (fg.r, fg.g, fg.b, bg.r, bg.g, bg.b).hash(&mut hasher);

            match cur_run.as_mut() {
                Some(r) if r.fg == fg && r.bg == bg => r.text.push(ch),
                _ => {
                    flush_run(&mut cur_row, &mut cur_run);
                    let mut text = String::new();
                    text.push(ch);
                    cur_run = Some(Run {
                        col_start: col,
                        fg,
                        bg,
                        text,
                    });
                }
            }
            col += 1;
        }
        flush_run(&mut cur_row, &mut cur_run);
        if cur_line.is_some() {
            rows.push(cur_row);
        }

        // Cursor position: column from the renderable cursor, row from the
        // index we tracked while walking the visible grid (avoids off-by-one).
        let content = term.renderable_content();
        let cursor_point = content.cursor.point;
        let cursor = match cursor_row_idx {
            Some(r) if r < self.rows => Some((cursor_point.column.0, r)),
            _ => None,
        };
        cursor.hash(&mut hasher);
        let content_hash = hasher.finish();

        // Scrollbar geometry: how many lines of history exist and how far up
        // the viewport is currently scrolled.
        let history = grid.history_size();
        let screen = grid.screen_lines();
        let scroll_offset = grid.display_offset();
        let total_lines = history + screen;

        // Selection spans per visible row.
        let mut selection_spans: Vec<(usize, usize, usize)> = Vec::new();
        if let Some(sel) = &term.selection {
            if let Some(range) = sel.to_range(&term) {
                let start = range.start;
                let end = range.end;
                for line_idx in start.line.0..=end.line.0 {
                    let row = line_idx + display_offset;
                    if row < 0 || row as usize >= self.rows {
                        continue;
                    }
                    let row = row as usize;
                    let scol = if line_idx == start.line.0 {
                        start.column.0
                    } else {
                        0
                    };
                    let ecol = if line_idx == end.line.0 {
                        end.column.0
                    } else {
                        cols.saturating_sub(1)
                    };
                    if ecol >= scol {
                        selection_spans.push((row, scol, ecol));
                    }
                }
            }
        }

        Snapshot {
            rows,
            cursor,
            cursor_shape_block: !matches!(
                content.cursor.shape,
                alacritty_terminal::vte::ansi::CursorShape::Hidden
            ),
            selection_spans,
            content_hash,
            scroll_offset,
            total_lines,
            screen_lines: screen,
        }
    }

    /// Begin a simple text selection at a viewport cell.
    pub fn selection_start(&self, col: usize, row: usize, side_right: bool) {
        let mut term = self.term.lock();
        let display_offset = term.grid().display_offset() as i32;
        let point = Point::new(Line(row as i32 - display_offset), Column(col));
        let side = if side_right { Side::Right } else { Side::Left };
        term.selection = Some(Selection::new(SelectionType::Simple, point, side));
    }

    /// Extend the active selection to a viewport cell.
    pub fn selection_update(&self, col: usize, row: usize, side_right: bool) {
        let mut term = self.term.lock();
        let display_offset = term.grid().display_offset() as i32;
        let point = Point::new(Line(row as i32 - display_offset), Column(col));
        let side = if side_right { Side::Right } else { Side::Left };
        if let Some(sel) = term.selection.as_mut() {
            sel.update(point, side);
        }
    }

    pub fn selection_clear(&self) {
        self.term.lock().selection = None;
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_to_string()
    }

    /// Return the last `lines` lines of the buffer (scrollback + visible) as
    /// plain text, for feeding terminal output to the AI agent.
    pub fn scrollback_text(&self, lines: usize) -> String {
        let term = self.term.lock();
        let grid = term.grid();
        let total = grid.total_lines();
        let cols = grid.columns();
        let start = total.saturating_sub(lines);
        let mut out = String::with_capacity(lines * cols);
        // Iterate from the topmost requested line down to the bottom.
        for buf_line in start..total {
            // Grid is indexed with Line(0) = top of screen; scrollback is
            // negative. Convert buffer index to a Line value.
            let screen = grid.screen_lines();
            let line_val = buf_line as i32 - (total - screen) as i32;
            let line = alacritty_terminal::index::Line(line_val);
            let row = &grid[line];
            let mut s = String::with_capacity(cols);
            for col in 0..cols {
                let c = row[alacritty_terminal::index::Column(col)].c;
                s.push(if c == '\0' { ' ' } else { c });
            }
            out.push_str(s.trim_end());
            out.push('\n');
        }
        out
    }
}

/// A run of consecutive cells sharing the same fg/bg color on one row.
pub struct Run {
    pub col_start: usize,
    pub fg: crate::color::Rgba,
    pub bg: crate::color::Rgba,
    pub text: String,
}

/// All color runs for a single visible row.
#[derive(Default)]
pub struct RowRuns {
    pub runs: Vec<Run>,
}

/// A consistent view of a pane's contents for one rendered frame.
pub struct Snapshot {
    /// Styled text runs, one entry per visible row (top to bottom).
    pub rows: Vec<RowRuns>,
    /// (col, row) of the cursor in viewport cells, if visible.
    pub cursor: Option<(usize, usize)>,
    pub cursor_shape_block: bool,
    /// (row, start_col, end_col) inclusive selection spans in viewport cells.
    pub selection_spans: Vec<(usize, usize, usize)>,
    /// Hash of visible content; lets the renderer skip re-shaping when equal.
    pub content_hash: u64,
    /// Lines scrolled up from the live bottom (0 = at bottom).
    pub scroll_offset: usize,
    /// Total lines including scrollback history.
    pub total_lines: usize,
    /// Number of visible rows on screen.
    pub screen_lines: usize,
}

// Read trait import for reader.read in the thread above.
use std::io::Read;
