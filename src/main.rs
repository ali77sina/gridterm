mod ai;
mod ai_ui;
mod browser;
mod color;
mod crashlog;
mod grid;
mod pty;
mod renderer;
mod usage;

use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arboard::Clipboard;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use ai::{spawn_agent, AgentInput, AiConfig, AiEvent, ToolCall};
use ai_ui::{tool_summary, ChatState, Role};
use grid::{GridLayout, Rect};
use pty::{PtyTerm, Waker};
use renderer::{PaneDraw, QuadInstance, Renderer};

/// Sent from PTY reader threads to wake the event loop for an immediate redraw.
#[derive(Debug, Clone, Copy)]
struct Wake;

/// One pane = one shell + its rendered text buffer.
struct Pane {
    term: PtyTerm,
    buffer: glyphon::Buffer,
    /// Set when the terminal produced new output and the text buffer needs
    /// to be re-shaped. Selection/cursor changes do NOT require re-shaping.
    text_dirty: bool,
    /// Content hash of the last shaped buffer, to skip redundant re-shaping.
    last_hash: u64,
}

/// Pending grid command state. After the prefix key (Ctrl+A) we capture digits
/// like "4x4" until Enter, then rebuild the grid.
struct GridCommand {
    active: bool,
    input: String,
}

/// In-progress mouse text selection within a pane.
struct Dragging {
    pane: usize,
    last_cell: (usize, usize),
}

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    panes: Vec<Pane>,
    layout: GridLayout,
    active: usize,
    modifiers: ModifiersState,
    grid_cmd: GridCommand,
    clipboard: Option<Clipboard>,
    mouse_pos: PhysicalPosition<f64>,
    dragging: Option<Dragging>,
    /// Shared closure that PTY reader threads call to wake the loop.
    waker: Waker,
    /// Set by Wake events; coalesces many wakeups into one redraw.
    pending_redraw: bool,
    /// A redraw was requested during the frame interval and awaits the slot.
    deferred_redraw: bool,
    last_frame: Instant,
    /// Accumulated fractional scroll for smooth trackpad scrolling.
    scroll_accum: f32,
    /// Frame-time profiling (enabled via GRIDTERM_PROFILE=1).
    profile: bool,
    frame_count: u32,
    frame_time_sum: Duration,
    frame_time_max: Duration,
    stats_since: Instant,
    /// AI chat state + channels to the background agent thread.
    chat: ChatState,
    ai_to_agent: Option<Sender<AgentInput>>,
    ai_events: Option<Receiver<AiEvent>>,
    /// Last rendered chat input text-area rect (x, y, w) for click hit-testing.
    chat_input_geom: Option<(f32, f32, f32)>,
    /// Shared flag to abort an in-flight AI turn (stop button).
    ai_cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Captured agent usage/cost, fed by the in-process OTLP collector.
    usage: usage::UsageStore,
    /// Loopback port the OTLP collector listens on (None if it failed to bind).
    otlp_port: Option<u16>,
    /// Heartbeat the watchdog monitors to detect UI freezes.
    heartbeat: Option<crashlog::Heartbeat>,
}

impl App {
    fn new(proxy: EventLoopProxy<Wake>) -> Self {
        let waker: Waker = Arc::new(move || {
            let _ = proxy.send_event(Wake);
        });
        let ai_available = AiConfig::load().is_some();
        Self {
            window: None,
            renderer: None,
            panes: Vec::new(),
            layout: GridLayout::new(1, 1),
            active: 0,
            modifiers: ModifiersState::empty(),
            grid_cmd: GridCommand {
                active: false,
                input: String::new(),
            },
            clipboard: Clipboard::new().ok(),
            mouse_pos: PhysicalPosition::new(0.0, 0.0),
            dragging: None,
            waker,
            pending_redraw: false,
            deferred_redraw: false,
            last_frame: Instant::now(),
            scroll_accum: 0.0,
            profile: std::env::var("GRIDTERM_PROFILE").is_ok(),
            frame_count: 0,
            frame_time_sum: Duration::ZERO,
            frame_time_max: Duration::ZERO,
            stats_since: Instant::now(),
            chat: ChatState::new(ai_available),
            ai_to_agent: None,
            ai_events: None,
            chat_input_geom: None,
            ai_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            usage: usage::UsageStore::new(),
            otlp_port: None,
            heartbeat: None,
        }
    }

    /// Spawn the background AI agent thread once the grid exists.
    fn init_ai(&mut self) {
        let Some(cfg) = AiConfig::load() else {
            return;
        };
        let (term_cols, term_rows) = self
            .panes
            .first()
            .map(|p| (p.term.cols, p.term.rows))
            .unwrap_or((80, 24));
        let tools = ai::tools::tool_schema();
        let prompt = ai::tools::system_prompt(term_cols, term_rows);
        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<AiEvent>();
        let (in_tx, in_rx) = std::sync::mpsc::channel::<AgentInput>();
        let wake = self.waker.clone();
        // Hand the agent its own private shell so its blocking tools (shell,
        // wait) run on the agent thread and never freeze the UI.
        let ai_shell = self.renderer.as_ref().and_then(|r| {
            PtyTerm::spawn(100, 40, r.cell_w, r.cell_h, self.waker.clone(), None).ok()
        });
        spawn_agent(
            cfg,
            tools,
            prompt,
            ev_tx,
            in_rx,
            wake,
            self.ai_cancel.clone(),
            ai_shell,
        );
        self.ai_to_agent = Some(in_tx);
        self.ai_events = Some(ev_rx);
    }

    /// Width reserved for the chat panel (0 when closed).
    fn chat_panel_width(&self) -> f32 {
        if self.chat.open {
            let (w, _) = self
                .renderer
                .as_ref()
                .map(|r| r.size())
                .unwrap_or((1100.0, 720.0));
            (w * 0.4).clamp(360.0, 560.0)
        } else {
            0.0
        }
    }

    /// (Re)build the pane grid for the current layout, spawning shells as
    /// needed and resizing/dropping panes to match the new pane count.
    fn rebuild_grid(&mut self, cols: usize, rows: usize) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        self.layout = GridLayout::new(cols, rows);
        let full_w = renderer.size().0;
        let panel_w = if self.chat.open {
            (full_w * 0.4).clamp(360.0, 560.0)
        } else {
            0.0
        };
        let (win_w, win_h) = (full_w - panel_w, renderer.size().1);
        let count = self.layout.count();

        self.panes.truncate(count);

        for i in 0..count {
            let rect = self.layout.rect_for(i, win_w, win_h);
            let term_cols = ((rect.w / renderer.cell_w).floor() as usize).max(1);
            let term_rows = ((rect.h / renderer.cell_h).floor() as usize).max(1);

            if let Some(pane) = self.panes.get_mut(i) {
                pane.term
                    .resize(term_cols, term_rows, renderer.cell_w, renderer.cell_h);
                // Keep width unbounded (no wrapping); only bound the height.
                pane.buffer
                    .set_size(&mut renderer.font_system, None, Some(rect.h));
                pane.text_dirty = true;
            } else {
                let term = match PtyTerm::spawn(
                    term_cols,
                    term_rows,
                    renderer.cell_w,
                    renderer.cell_h,
                    self.waker.clone(),
                    self.otlp_port.map(|p| (p, i)),
                ) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("failed to spawn pane: {e}");
                        continue;
                    }
                };
                let buffer = renderer.new_pane_buffer(rect);
                self.panes.push(Pane {
                    term,
                    buffer,
                    text_dirty: true,
                    last_hash: 0,
                });
            }
        }

        if self.active >= self.panes.len() {
            self.active = self.panes.len().saturating_sub(1);
        }
        self.request_redraw();
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Drain streamed AI events into chat state; execute tool calls on panes.
    fn drain_ai(&mut self) {
        let mut events = Vec::new();
        if let Some(rx) = &self.ai_events {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        for ev in events {
            match ev {
                AiEvent::Token(t) => self.chat.push_assistant_token(&t),
                AiEvent::ToolCall(tc) => {
                    // Display only — show what the agent is doing.
                    self.chat.push_tool(tool_summary(&tc));
                }
                AiEvent::ExecPaneTool(tc) => {
                    // Pane-touching tool: execute on the main thread (instant,
                    // no blocking) and reply to the agent.
                    let result = self.execute_tool(&tc);
                    if let Some(tx) = &self.ai_to_agent {
                        let _ = tx.send(AgentInput::ToolResult {
                            id: tc.id.clone(),
                            content: result,
                        });
                    }
                }
                AiEvent::Done => self.chat.streaming = false,
                AiEvent::Error(e) => {
                    self.chat.push_assistant_token(&format!("\n[error: {e}]"));
                    self.chat.streaming = false;
                }
            }
        }
    }

    /// Execute an agent tool call against the panes; return a text result.
    fn execute_tool(&mut self, tc: &ToolCall) -> String {
        if let Some(h) = &self.heartbeat {
            h.mark(format!("tool:{}", tc.name));
        }
        let args: serde_json::Value =
            serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null);
        let pane_idx = args["pane"].as_i64().unwrap_or(self.active as i64) as usize;

        match tc.name.as_str() {
            "run_command" => {
                let cmd = args["command"].as_str().unwrap_or("");
                if let Some(pane) = self.panes.get_mut(pane_idx) {
                    pane.term.scroll_to_bottom();
                    pane.term.write(cmd.as_bytes());
                    pane.term.write(b"\r");
                    format!("ran in pane {pane_idx}: {cmd}\n(use read_pane to see output)")
                } else {
                    format!("no pane {pane_idx}")
                }
            }
            "send_keys" => {
                let keys = args["keys"].as_str().unwrap_or("");
                if let Some(pane) = self.panes.get_mut(pane_idx) {
                    pane.term.write(keys.as_bytes());
                    format!("sent keys to pane {pane_idx}")
                } else {
                    format!("no pane {pane_idx}")
                }
            }
            "read_pane" => {
                let lines = args["lines"].as_i64().unwrap_or(80).clamp(1, 400) as usize;
                if let Some(pane) = self.panes.get(pane_idx) {
                    let text = pane.term.scrollback_text(lines);
                    ai::tools::truncate_tokens(&text, 1500)
                } else {
                    format!("no pane {pane_idx}")
                }
            }
            "grep_pane" => {
                let pattern = args["pattern"].as_str().unwrap_or("");
                if let Some(pane) = self.panes.get(pane_idx) {
                    let text = pane.term.scrollback_text(2000);
                    let matches: Vec<&str> = text
                        .lines()
                        .filter(|l| l.to_lowercase().contains(&pattern.to_lowercase()))
                        .collect();
                    if matches.is_empty() {
                        format!("no matches for '{pattern}' in pane {pane_idx}")
                    } else {
                        ai::tools::truncate_tokens(&matches.join("\n"), 1200)
                    }
                } else {
                    format!("no pane {pane_idx}")
                }
            }
            "list_panes" => {
                let mut out = String::new();
                for (i, _p) in self.panes.iter().enumerate() {
                    let col = i % self.layout.cols;
                    let row = i / self.layout.cols;
                    out.push_str(&format!(
                        "pane {i}: grid ({col},{row}){}\n",
                        if i == self.active { " [focused]" } else { "" }
                    ));
                }
                out
            }
            "shell" | "shell_bg" | "read_shell" | "wait" => {
                // These are executed on the agent thread, never here.
                "(handled by agent)".into()
            }
            "set_grid" => {
                let cols = args["cols"].as_i64().unwrap_or(1).clamp(1, 12) as usize;
                let rows = args["rows"].as_i64().unwrap_or(1).clamp(1, 12) as usize;
                let allow_close = args["allow_close"].as_bool().unwrap_or(false);
                let new_count = cols * rows;
                let cur_count = self.panes.len();
                if new_count < cur_count && !allow_close {
                    format!(
                        "refused: a {cols}x{rows} grid ({new_count} panes) would close {} existing \
terminal(s). If the user explicitly wants to close panes, call again with allow_close=true.",
                        cur_count - new_count
                    )
                } else {
                    self.rebuild_grid(cols, rows);
                    format!("grid is now {cols}x{rows} ({new_count} panes)")
                }
            }
            "add_pane" => {
                let cur = self.panes.len();
                // Grow the dimension that keeps the grid most square.
                if self.layout.cols <= self.layout.rows {
                    self.add_column();
                } else {
                    self.add_row();
                }
                format!("added pane (now {} panes); new pane index {}", self.panes.len(), cur)
            }
            "usage_report" => self.usage.report(),
            "browser_setup" => {
                let mode = match args["mode"].as_str().unwrap_or("on") {
                    "off" => browser::BrowserMode::Off,
                    _ => browser::BrowserMode::Shared,
                };
                match browser::set_mode(mode) {
                    Ok(msg) => format!("{msg}\n(Restart agents in panes to pick up the change.)"),
                    Err(e) => format!("failed: {e}"),
                }
            }
            "browser_request_login" => {
                let url = args["url"].as_str();
                browser::surface_for_login(url)
            }
            "browser_save_login" => browser::save_login_state(),
            other => format!("unknown tool: {other}"),
        }
    }

    /// Build the default context the agent sees: every pane's visible content.
    fn build_context(&self) -> String {
        let mut out = String::new();
        for (i, pane) in self.panes.iter().enumerate() {
            let snap = pane.term.snapshot();
            let mut text = String::new();
            for row in &snap.rows {
                for run in &row.runs {
                    text.push_str(&run.text);
                }
                text.push('\n');
            }
            let text = ai::tools::truncate_tokens(text.trim_end(), 500);
            out.push_str(&format!(
                "--- pane {i}{} ---\n{text}\n",
                if i == self.active { " (focused)" } else { "" }
            ));
        }
        out
    }

    /// Send the current chat input as a user turn to the agent.
    fn send_chat(&mut self) {
        let text = self.chat.input.trim().to_string();
        if text.is_empty() || self.chat.streaming {
            return;
        }
        self.chat.clear_input();
        self.chat.push_user(text.clone());
        self.chat.streaming = true;
        let context = self.build_context();
        if let Some(tx) = &self.ai_to_agent {
            let _ = tx.send(AgentInput::User { text, context });
        }
        self.request_redraw();
    }

    /// Add one column, keeping all existing panes alive.
    fn add_column(&mut self) {
        let (c, r) = (self.layout.cols + 1, self.layout.rows);
        if c * r <= 64 {
            self.rebuild_grid(c, r);
        }
    }

    /// Add one row, keeping all existing panes alive.
    fn add_row(&mut self) {
        let (c, r) = (self.layout.cols, self.layout.rows + 1);
        if c * r <= 64 {
            self.rebuild_grid(c, r);
        }
    }

    /// Remove the last column (closes the shells in it).
    fn remove_column(&mut self) {
        if self.layout.cols > 1 {
            self.rebuild_grid(self.layout.cols - 1, self.layout.rows);
        }
    }

    /// Remove the last row (closes the shells in it).
    fn remove_row(&mut self) {
        if self.layout.rows > 1 {
            self.rebuild_grid(self.layout.cols, self.layout.rows - 1);
        }
    }

    /// Convert a physical mouse position into (pane index, col, row, side).
    fn mouse_to_cell(&self, pos: PhysicalPosition<f64>) -> Option<(usize, usize, usize, bool)> {
        let renderer = self.renderer.as_ref()?;
        let panel_w = self.chat_panel_width();
        let win_w = renderer.size().0 - panel_w;
        let win_h = renderer.size().1;
        let (px, py) = (pos.x as f32, pos.y as f32);
        for i in 0..self.panes.len() {
            let rect = self.layout.rect_for(i, win_w, win_h);
            if px >= rect.x && px < rect.x + rect.w && py >= rect.y && py < rect.y + rect.h {
                let local_x = px - rect.x;
                let local_y = py - rect.y;
                let col = (local_x / renderer.cell_w).floor() as usize;
                let row = (local_y / renderer.cell_h).floor() as usize;
                let frac = (local_x / renderer.cell_w).fract();
                let side_right = frac > 0.5;
                let cols = ((rect.w / renderer.cell_w).floor() as usize).max(1);
                let rows = ((rect.h / renderer.cell_h).floor() as usize).max(1);
                return Some((i, col.min(cols - 1), row.min(rows - 1), side_right));
            }
        }
        None
    }

    /// Pull each Term snapshot, build cursor/selection quads, then draw.
    fn render(&mut self) {
        let heartbeat_ref = self.heartbeat.clone();
        let chat_open = self.chat.open;
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let full_w = renderer.size().0;
        let win_h = renderer.size().1;
        let panel_w = if chat_open {
            (full_w * 0.4).clamp(360.0, 560.0)
        } else {
            0.0
        };
        let win_w = full_w - panel_w;
        let (cell_w, cell_h) = (renderer.cell_w, renderer.cell_h);
        let (cursor_top, cursor_height) = (renderer.cursor_top, renderer.cursor_height);

        let mut draws: Vec<PaneDraw> = Vec::with_capacity(self.panes.len());
        let mut quads: Vec<QuadInstance> = Vec::new();
        // Per-pane cost badges: (text, right-edge x, top y).
        let mut badges: Vec<(String, f32, f32)> = Vec::new();

        for (i, pane) in self.panes.iter_mut().enumerate() {
            if let Some(h) = &heartbeat_ref {
                h.mark(format!("render:snapshot pane {i}"));
            }
            let rect: Rect = self.layout.rect_for(i, win_w, win_h);
            let active = i == self.active;
            let snap = pane.term.snapshot();

            // Cost/usage badge for this pane (only if the agent reported any).
            if let Some(u) = self.usage.pane(i) {
                let cost = u.best_cost();
                let label = if cost > 0.0 {
                    format!("${cost:.3}")
                } else if u.total_tokens() > 0 {
                    format!("{}t", compact_tokens(u.total_tokens()))
                } else {
                    String::new()
                };
                if !label.is_empty() {
                    // Place the badge on the pane's top border line, where there
                    // is no terminal text to show through behind it.
                    badges.push((label, rect.x + rect.w - 8.0, rect.y - cell_h * 0.5));
                }
            }

            // Pane background panel + border so the grid is visibly tiled.
            let bg = if active {
                [0.0667, 0.0667, 0.106, 1.0] // 0x11111b
            } else {
                [0.043, 0.043, 0.075, 1.0] // slightly darker when inactive
            };
            let border = if active {
                [0.537, 0.706, 0.98, 1.0] // accent blue (0x89b4fa)
            } else {
                [0.12, 0.12, 0.16, 1.0] // muted
            };
            let bw = 1.0; // thin, modern border
            // Border (slightly larger rect behind the panel).
            quads.push(QuadInstance {
                rect: [rect.x - bw, rect.y - bw, rect.w + 2.0 * bw, rect.h + 2.0 * bw],
                color: border,
            });
            // Panel background.
            quads.push(QuadInstance {
                rect: [rect.x, rect.y, rect.w, rect.h],
                color: bg,
            });

            // Only re-shape the text when the visible content actually changed.
            // The content hash catches the no-op case (idle/background panes),
            // and mouse selection just adds quads, so dragging stays cheap.
            if pane.text_dirty && snap.content_hash != pane.last_hash {
                renderer.set_pane_rows(&mut pane.buffer, &snap.rows);
                pane.last_hash = snap.content_hash;
            }
            pane.text_dirty = false;

            // Per-cell background color quads (only for non-default backgrounds).
            for (ri, row) in snap.rows.iter().enumerate() {
                let y = rect.y + ri as f32 * cell_h;
                if y + cell_h < rect.y || y > rect.y + rect.h {
                    continue;
                }
                for run in &row.runs {
                    if run.bg == color::DEFAULT_BG {
                        continue;
                    }
                    let cells = run.text.chars().count() as f32;
                    let x = rect.x + run.col_start as f32 * cell_w;
                    quads.push(QuadInstance {
                        rect: [x, y, cells * cell_w, cell_h],
                        color: [
                            run.bg.r as f32 / 255.0,
                            run.bg.g as f32 / 255.0,
                            run.bg.b as f32 / 255.0,
                            1.0,
                        ],
                    });
                }
            }

            // Selection highlight quads (drawn first, behind text).
            for (row, scol, ecol) in &snap.selection_spans {
                let x = rect.x + *scol as f32 * cell_w;
                let y = rect.y + *row as f32 * cell_h;
                let w = (*ecol as f32 - *scol as f32 + 1.0) * cell_w;
                quads.push(QuadInstance {
                    rect: [x, y, w, cell_h],
                    color: [0.25, 0.40, 0.75, 0.55],
                });
            }

            // Cursor quad. Solid block when active, hollow-ish dim when not.
            // Use the measured glyph offset/height so it sits on the text.
            if let Some((col, row)) = snap.cursor {
                if snap.cursor_shape_block {
                    let x = rect.x + col as f32 * cell_w;
                    let y = rect.y + row as f32 * cell_h + cursor_top;
                    let color = if active {
                        [0.85, 0.85, 0.85, 0.9]
                    } else {
                        [0.5, 0.5, 0.5, 0.5]
                    };
                    quads.push(QuadInstance {
                        rect: [x, y, cell_w, cursor_height],
                        color,
                    });
                }
            }

            // Slick auto-hiding scrollbar: only when there's scrollback. Thin,
            // overlaid on the right edge (takes no layout space). Brighter when
            // scrolled up, dim when at the live bottom.
            if snap.total_lines > snap.screen_lines {
                let sb_w = 3.0;
                let sb_x = rect.x + rect.w - sb_w - 2.0;
                let track_h = rect.h - 4.0;
                let track_y = rect.y + 2.0;
                let total = snap.total_lines as f32;
                let visible = snap.screen_lines as f32;
                let thumb_h = (visible / total * track_h).max(18.0).min(track_h);
                // offset 0 = bottom; map to thumb position from the top.
                let max_off = (snap.total_lines - snap.screen_lines).max(1) as f32;
                let frac_from_bottom = snap.scroll_offset as f32 / max_off;
                let thumb_y = track_y + (track_h - thumb_h) * (1.0 - frac_from_bottom);
                let scrolled = snap.scroll_offset > 0;
                // Track (very subtle).
                quads.push(QuadInstance {
                    rect: [sb_x, track_y, sb_w, track_h],
                    color: [1.0, 1.0, 1.0, 0.05],
                });
                // Thumb.
                let a = if scrolled { 0.55 } else { 0.22 };
                quads.push(QuadInstance {
                    rect: [sb_x, thumb_y, sb_w, thumb_h],
                    color: [0.7, 0.75, 0.85, a],
                });
            }

            draws.push(PaneDraw {
                rect,
                buffer: pane.buffer.clone(),
            });
        }

        // Upload per-pane cost badges and draw a solid pill behind each so the
        // text is readable regardless of the terminal content underneath.
        let pill_rects = renderer.set_badges(&badges);
        for r in &pill_rects {
            quads.push(QuadInstance {
                rect: *r,
                color: [0.10, 0.22, 0.21, 1.0], // solid deep teal pill
            });
        }

        // AI chat side panel, occupying the reserved space to the right of the
        // (now narrower) terminal grid — so it never overlaps the panes.
        if self.chat.open {
            let px = win_w; // content ends here; panel starts here
            // Panel background (frosted dark).
            quads.push(QuadInstance {
                rect: [px, 0.0, panel_w, win_h],
                color: [0.055, 0.055, 0.09, 1.0],
            });
            // Left accent edge.
            quads.push(QuadInstance {
                rect: [px, 0.0, 2.0, win_h],
                color: [0.537, 0.706, 0.98, 1.0],
            });

            // Build the conversation as styled segments: role labels get a
            // color, message bodies stay default. (Just the label is colored.)
            let pad = 16.0;
            let mut segments: Vec<(String, Option<glyphon::Color>)> = Vec::new();
            let user_col = glyphon::Color::rgb(0x89, 0xb4, 0xfa); // blue
            let ai_col = glyphon::Color::rgb(0xa6, 0xe3, 0xa1); // green
            let tool_col = glyphon::Color::rgb(0xf9, 0xe2, 0xaf); // yellow

            // Live cost/usage header across all panes.
            let total = self.usage.total_cost();
            let usage_line = if total > 0.0 {
                format!("Session cost: ${total:.4}\n\n")
            } else {
                "Session cost: $0.00  (auto-tracked for Claude Code / Codex)\n\n".to_string()
            };
            segments.push((usage_line, Some(glyphon::Color::rgb(0x94, 0xe2, 0xd5))));
            for e in &self.chat.entries {
                let (label, col) = match e.role {
                    Role::User => ("You  ", user_col),
                    Role::Assistant => ("AI  ", ai_col),
                    Role::Tool => ("·  ", tool_col),
                };
                segments.push((label.to_string(), Some(col)));
                segments.push((format!("{}\n\n", e.text), None));
            }
            if self.chat.streaming {
                segments.push(("AI  ".to_string(), Some(ai_col)));
                segments.push(("…  ".to_string(), None));
                segments.push((
                    "(Esc to stop)".to_string(),
                    Some(glyphon::Color::rgb(0x7f, 0x84, 0x9c)),
                ));
            }
            if self.chat.entries.is_empty() {
                segments.push((
                    "Ask me to run things across your terminals.\nCmd+J to toggle • Enter to send • Esc to type in panes".to_string(),
                    Some(glyphon::Color::rgb(0x7f, 0x84, 0x9c)),
                ));
            }

            // Input box grows with content, capped to ~6 lines.
            let inner_w = panel_w - pad * 2.0;
            let text_w = inner_w - 16.0;
            let has_input = !self.chat.input.is_empty();
            let prompt = if has_input {
                format!("› {}", self.chat.input)
            } else {
                "› type a message…".to_string()
            };
            // Measure caret position (prefix = "› " + input up to the caret).
            let caret_prefix = format!("› {}", &self.chat.input[..self.chat.input_cursor]);
            let (caret_x, caret_line) = renderer.measure_caret(&caret_prefix, text_w);
            // Now set the full input text for rendering.
            let input_text_h = renderer.set_input_text(&prompt, text_w);
            let line_h = renderer.line_height;
            let input_box_h = (input_text_h + 20.0).clamp(line_h + 20.0, line_h * 6.0 + 20.0);

            // Conversation occupies the space above the input box.
            let convo_x = px + pad;
            let convo_y = pad;
            let convo_w = inner_w;
            let convo_h = win_h - input_box_h - pad * 2.5;

            let total_h = renderer.set_overlay_segments(&segments, convo_w);
            // Clamp scroll: 0 = bottom (newest). Max scroll shows the top.
            let max_scroll = (total_h - convo_h).max(0.0);
            if self.chat.stick_bottom {
                self.chat.scroll = 0.0;
                self.chat.stick_bottom = false;
            }
            self.chat.scroll = self.chat.scroll.clamp(0.0, max_scroll);
            // Shift text so the bottom is visible: top offset moves content up.
            let text_top = convo_y - (max_scroll - self.chat.scroll);

            let convo_rect = Rect {
                x: convo_x,
                y: text_top,
                w: convo_w,
                h: convo_h,
            };
            // A separate clip rect keeps text inside the visible area.
            let clip_rect = Rect {
                x: convo_x,
                y: convo_y,
                w: convo_w,
                h: convo_h,
            };

            // Input box.
            let in_y = win_h - input_box_h - pad * 0.5;
            quads.push(QuadInstance {
                rect: [px + pad, in_y, inner_w, input_box_h],
                color: [0.09, 0.09, 0.14, 1.0],
            });

            // Blinking caret in the input box (only when there is input focus).
            let text_x = px + pad + 8.0;
            let text_y = in_y + 10.0;
            self.chat_input_geom = Some((text_x, text_y, text_w));
            let caret_px = text_x + if has_input { caret_x } else { 0.0 };
            let caret_py = text_y + caret_line as f32 * line_h;
            // Only show the caret when the input is focused.
            if self.chat.input_focused {
                quads.push(QuadInstance {
                    rect: [caret_px + 1.0, caret_py + 2.0, 2.0, line_h * 0.9],
                    color: [0.9, 0.92, 1.0, 0.9],
                });
            }

            // Conversation scrollbar (when content overflows).
            if max_scroll > 1.0 {
                let track_h = convo_h;
                let thumb_h = (convo_h / total_h * convo_h).max(24.0).min(convo_h);
                let frac = self.chat.scroll / max_scroll; // 0 bottom .. 1 top
                let thumb_y = convo_y + (track_h - thumb_h) * (1.0 - frac);
                quads.push(QuadInstance {
                    rect: [px + panel_w - 5.0, thumb_y, 3.0, thumb_h],
                    color: [0.7, 0.75, 0.85, 0.5],
                });
            }

            renderer.render_with_overlay(
                &mut draws,
                &quads,
                convo_rect,
                clip_rect,
                Rect {
                    x: px + pad + 8.0,
                    y: in_y + 10.0,
                    w: inner_w - 16.0,
                    h: input_box_h,
                },
            );
            return;
        }

        renderer.render(&mut draws, &quads);
    }

    /// Check each pane for new terminal output, marking text buffers dirty.
    /// Returns true if any pane needs a redraw.
    fn any_dirty(&mut self) -> bool {
        let mut dirty = false;
        for pane in self.panes.iter_mut() {
            if pane.term.proxy.take_dirty() {
                pane.text_dirty = true;
                dirty = true;
            }
        }
        dirty
    }

    fn copy_selection(&mut self) {
        if let Some(pane) = self.panes.get(self.active) {
            if let Some(text) = pane.term.selection_text() {
                if !text.is_empty() {
                    if let Some(cb) = self.clipboard.as_mut() {
                        let _ = cb.set_text(text);
                    }
                }
            }
        }
    }

    fn paste(&mut self) {
        let text = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok());
        if let Some(text) = text {
            if let Some(pane) = self.panes.get_mut(self.active) {
                // Bracketed paste so the shell treats it as literal input.
                pane.term.write(b"\x1b[200~");
                pane.term.write(text.as_bytes());
                pane.term.write(b"\x1b[201~");
            }
        }
    }

    fn handle_key(&mut self, key: &Key, text: Option<&str>) {
        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();
        let cmd = self.modifiers.super_key();
        let shift = self.modifiers.shift_key();

        // Esc stops an in-flight AI response from anywhere while chat is open.
        if self.chat.open && self.chat.streaming {
            if let Key::Named(NamedKey::Escape) = key {
                self.ai_cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                self.request_redraw();
                return;
            }
        }

        // Cmd+J toggles the AI chat overlay.
        if cmd {
            if let Key::Character(c) = key {
                if c.as_str() == "j" {
                    if self.chat.available {
                        self.chat.open = !self.chat.open;
                        self.chat.input_focused = self.chat.open;
                        // Reflow the grid so panes shrink/grow for the panel.
                        let (cols, rows) = (self.layout.cols, self.layout.rows);
                        self.rebuild_grid(cols, rows);
                        self.request_redraw();
                    }
                    return;
                }
            }
        }

        // When the chat input is focused, keystrokes drive the chat input with
        // proper text editing. (Cmd+J above toggles; clicking a pane unfocuses
        // the chat so you can type into terminals while the chat stays open.)
        if self.chat.open && self.chat.input_focused {
            match key {
                Key::Named(NamedKey::Escape) => {
                    self.chat.input_focused = false;
                    self.request_redraw();
                    return;
                }
                Key::Named(NamedKey::Enter) => {
                    // Shift+Enter inserts a newline; Enter sends.
                    if shift {
                        self.chat.insert("\n");
                    } else {
                        self.send_chat();
                    }
                    self.request_redraw();
                    return;
                }
                Key::Named(NamedKey::Backspace) => {
                    if cmd {
                        self.chat.delete_to_line_start();
                    } else if alt {
                        self.chat.delete_word();
                    } else {
                        self.chat.backspace();
                    }
                    self.request_redraw();
                    return;
                }
                Key::Named(NamedKey::ArrowLeft) => {
                    if cmd {
                        self.chat.move_home();
                    } else if alt {
                        // Word-left: delete_word logic in reverse — just step.
                        for _ in 0..1 {
                            self.chat.move_left();
                        }
                        while self.chat.input_cursor > 0
                            && !self.chat.input.as_bytes()[self.chat.input_cursor - 1]
                                .is_ascii_whitespace()
                        {
                            self.chat.move_left();
                        }
                    } else {
                        self.chat.move_left();
                    }
                    self.request_redraw();
                    return;
                }
                Key::Named(NamedKey::ArrowRight) => {
                    if cmd {
                        self.chat.move_end();
                    } else {
                        self.chat.move_right();
                    }
                    self.request_redraw();
                    return;
                }
                Key::Named(NamedKey::Home) => {
                    self.chat.move_home();
                    self.request_redraw();
                    return;
                }
                Key::Named(NamedKey::End) => {
                    self.chat.move_end();
                    self.request_redraw();
                    return;
                }
                Key::Named(NamedKey::Space) => {
                    self.chat.insert(" ");
                    self.request_redraw();
                    return;
                }
                _ => {
                    if cmd {
                        if let Key::Character(c) = key {
                            match c.as_str() {
                                "v" => {
                                    if let Some(t) =
                                        self.clipboard.as_mut().and_then(|cb| cb.get_text().ok())
                                    {
                                        self.chat.insert(&t);
                                        self.request_redraw();
                                    }
                                    return;
                                }
                                "a" => {
                                    self.chat.move_home();
                                    self.request_redraw();
                                    return;
                                }
                                "e" => {
                                    self.chat.move_end();
                                    self.request_redraw();
                                    return;
                                }
                                _ => return,
                            }
                        }
                    }
                    if let Some(t) = text {
                        if !t.chars().any(|c| c.is_control()) {
                            self.chat.insert(t);
                            self.request_redraw();
                        }
                    }
                    return;
                }
            }
        }

        // Grid command mode: capture "NxM" then Enter.
        if self.grid_cmd.active {
            match key {
                Key::Named(NamedKey::Enter) => {
                    let parsed = parse_grid(&self.grid_cmd.input);
                    self.grid_cmd.active = false;
                    self.grid_cmd.input.clear();
                    if let Some((c, r)) = parsed {
                        self.rebuild_grid(c, r);
                    }
                    return;
                }
                Key::Named(NamedKey::Escape) => {
                    self.grid_cmd.active = false;
                    self.grid_cmd.input.clear();
                    return;
                }
                Key::Named(NamedKey::Backspace) => {
                    self.grid_cmd.input.pop();
                    return;
                }
                _ => {
                    if let Some(t) = text {
                        self.grid_cmd.input.push_str(t);
                    }
                    return;
                }
            }
        }

        // Cmd shortcuts (macOS).
        if cmd {
            if let Key::Character(c) = key {
                match c.as_str() {
                    "c" => {
                        self.copy_selection();
                        return;
                    }
                    "v" => {
                        self.paste();
                        return;
                    }
                    // Cmd+G enters grid-layout command mode.
                    "g" => {
                        self.grid_cmd.active = true;
                        self.grid_cmd.input.clear();
                        self.request_redraw();
                        return;
                    }
                    // Cmd+1..9: quick square grids.
                    d if d.len() == 1 && d.chars().next().unwrap().is_ascii_digit() => {
                        let n = d.parse::<usize>().unwrap_or(1).max(1);
                        self.rebuild_grid(n, n);
                        return;
                    }
                    _ => {}
                }
            }
            // Cmd+Backspace: delete to start of line (Ctrl+U in the shell).
            if let Key::Named(NamedKey::Backspace) = key {
                if let Some(pane) = self.panes.get_mut(self.active) {
                    pane.term.write(&[0x15]);
                }
                return;
            }
            // Cmd+Shift+Arrows: grow/shrink the grid without losing terminals.
            //   Shift+Right add column, Shift+Left remove column,
            //   Shift+Down  add row,    Shift+Up   remove row.
            if shift {
                match key {
                    Key::Named(NamedKey::ArrowRight) => {
                        self.add_column();
                        return;
                    }
                    Key::Named(NamedKey::ArrowLeft) => {
                        self.remove_column();
                        return;
                    }
                    Key::Named(NamedKey::ArrowDown) => {
                        self.add_row();
                        return;
                    }
                    Key::Named(NamedKey::ArrowUp) => {
                        self.remove_row();
                        return;
                    }
                    _ => {}
                }
            }
            // Cmd+Left / Cmd+Right: line start / end.
            if let Key::Named(NamedKey::ArrowLeft) = key {
                if let Some(pane) = self.panes.get_mut(self.active) {
                    pane.term.write(&[0x01]); // Ctrl+A
                }
                return;
            }
            if let Key::Named(NamedKey::ArrowRight) = key {
                if let Some(pane) = self.panes.get_mut(self.active) {
                    pane.term.write(&[0x05]); // Ctrl+E
                }
                return;
            }
        }

        // Ctrl+] cycles the active pane (Ctrl+A is left for the shell).
        if ctrl {
            if let Key::Character(c) = key {
                if c.as_str() == "]" {
                    if !self.panes.is_empty() {
                        self.active = (self.active + 1) % self.panes.len();
                        self.request_redraw();
                    }
                    return;
                }
            }
        }

        // Typing into a pane clears any selection.
        if let Some(pane) = self.panes.get(self.active) {
            pane.term.selection_clear();
        }

        let Some(pane) = self.panes.get_mut(self.active) else {
            return;
        };
        let bytes = key_to_bytes(key, text, ctrl, alt);
        if !bytes.is_empty() {
            // Jump back to the live prompt when the user types.
            pane.term.scroll_to_bottom();
            pane.term.write(&bytes);
        }
    }
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("gridterm")
            .with_inner_size(LogicalSize::new(1100.0, 720.0));
        let window = Arc::new(event_loop.create_window(attrs).unwrap());
        let renderer = pollster::block_on(Renderer::new(window.clone()));
        self.window = Some(window);
        self.renderer = Some(renderer);

        // Start the in-process usage/cost collector before spawning shells so
        // each pane's agent can be pointed at it via env vars.
        self.otlp_port = usage::collector::start(self.usage.clone());

        // Pre-configure a headless browser MCP so coding agents in panes can
        // test web apps with zero setup. Non-destructive merge into .mcp.json.
        match browser::ensure_default() {
            Ok(true) => crashlog::append("BROWSER_MCP", "added headless playwright MCP"),
            Ok(false) => {}
            Err(e) => crashlog::append("BROWSER_MCP_ERR", &format!("{e}")),
        }

        // Optional startup grid for testing, e.g. GRIDTERM_GRID=4x2.
        let (c, r) = std::env::var("GRIDTERM_GRID")
            .ok()
            .and_then(|s| parse_grid(&s))
            .unwrap_or((1, 1));
        self.rebuild_grid(c, r);

        // Start the background AI agent (no-op if no config/key present).
        self.init_ai();

        // Optional stress mode: blast heavy output into every pane.
        if std::env::var("GRIDTERM_STRESS").is_ok() {
            for pane in self.panes.iter_mut() {
                // Continuously print numbered lines as fast as the shell can.
                pane.term
                    .write(b"while true; do seq 1 200; done\n");
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Freeze watchdog: active for the duration of event handling.
        let _hb = self.heartbeat.as_ref().map(|h| h.guard());
        if let Some(h) = &self.heartbeat {
            h.mark(format!("window_event:{}", event_label(&event)));
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                }
                let (c, rrows) = (self.layout.cols, self.layout.rows);
                self.rebuild_grid(c, rrows);
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = position;
                let cell = self.mouse_to_cell(position);
                let update = match (self.dragging.as_mut(), cell) {
                    (Some(drag), Some((i, col, row, side))) => {
                        if i == drag.pane && (col, row) != drag.last_cell {
                            drag.last_cell = (col, row);
                            Some((drag.pane, col, row, side))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some((pane_idx, col, row, side)) = update {
                    if let Some(pane) = self.panes.get(pane_idx) {
                        pane.term.selection_update(col, row, side);
                    }
                    self.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let cell_h = self.renderer.as_ref().map(|r| r.cell_h).unwrap_or(20.0);
                let line_px = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y * cell_h,
                    winit::event::MouseScrollDelta::PixelDelta(p) => p.y as f32,
                };

                // If the chat panel is open and the cursor is over it, scroll
                // the conversation instead of a terminal pane.
                let panel_w = self.chat_panel_width();
                let over_panel = self.chat.open
                    && self.renderer.as_ref().map_or(false, |r| {
                        self.mouse_pos.x as f32 >= r.size().0 - panel_w
                    });
                if over_panel {
                    // Scroll up (positive line_px) reveals older messages.
                    self.chat.scroll += line_px;
                    if self.chat.scroll < 0.0 {
                        self.chat.scroll = 0.0;
                    }
                    self.request_redraw();
                    return;
                }

                // Otherwise scroll the terminal pane under the cursor.
                self.scroll_accum += line_px / cell_h * 1.2;
                let lines = self.scroll_accum.trunc() as i32;
                if lines != 0 {
                    self.scroll_accum -= lines as f32;
                    if let Some((i, _, _, _)) = self.mouse_to_cell(self.mouse_pos) {
                        if let Some(pane) = self.panes.get_mut(i) {
                            pane.term
                                .scroll(alacritty_terminal::grid::Scroll::Delta(lines));
                            pane.text_dirty = true;
                        }
                        self.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left {
                    match state {
                        ElementState::Pressed => {
                            // First: is the click inside the open chat panel?
                            let panel_w = self.chat_panel_width();
                            let in_panel = self.chat.open
                                && self.renderer.as_ref().map_or(false, |r| {
                                    self.mouse_pos.x as f32 >= r.size().0 - panel_w
                                });
                            if in_panel {
                                // Focus the chat input; if the click is on the
                                // input text, position the caret there.
                                self.chat.input_focused = true;
                                if let Some((tx, ty, tw)) = self.chat_input_geom {
                                    let mx = self.mouse_pos.x as f32;
                                    let my = self.mouse_pos.y as f32;
                                    let line_h =
                                        self.renderer.as_ref().map(|r| r.line_height).unwrap_or(20.0);
                                    // Only treat clicks near the input row as caret placement.
                                    if my >= ty - line_h * 0.5 {
                                        let full = format!("› {}", self.chat.input);
                                        if let Some(r) = self.renderer.as_mut() {
                                            let off = r.hit_input(&full, tw, mx - tx, my - ty);
                                            // Subtract the 2-byte "› " prefix... '›' is 3 bytes
                                            // in UTF-8 plus a space = 4 bytes.
                                            let prefix_len = "› ".len();
                                            self.chat.input_cursor =
                                                off.saturating_sub(prefix_len).min(self.chat.input.len());
                                        }
                                    }
                                }
                                self.request_redraw();
                                return;
                            }
                            // Otherwise: terminal pane selection (and unfocus chat).
                            self.chat.input_focused = false;
                            if let Some((i, col, row, side)) = self.mouse_to_cell(self.mouse_pos) {
                                self.active = i;
                                if let Some(pane) = self.panes.get(i) {
                                    pane.term.selection_start(col, row, side);
                                }
                                self.dragging = Some(Dragging {
                                    pane: i,
                                    last_cell: (col, row),
                                });
                                self.request_redraw();
                            }
                        }
                        ElementState::Released => {
                            self.dragging = None;
                            // Keep the selection so Cmd+C can copy it.
                        }
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    let text = event.text.as_ref().map(|s| s.as_str());
                    self.handle_key(&event.logical_key, text);
                    self.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                let t0 = Instant::now();
                self.render();
                self.pending_redraw = false;
                self.last_frame = Instant::now();

                if self.profile {
                    let dt = t0.elapsed();
                    self.frame_count += 1;
                    self.frame_time_sum += dt;
                    if dt > self.frame_time_max {
                        self.frame_time_max = dt;
                    }
                    if self.stats_since.elapsed() >= Duration::from_secs(1) {
                        let n = self.frame_count.max(1);
                        let avg = self.frame_time_sum / n;
                        eprintln!(
                            "[profile] {} panes | {} fps | avg {:.2}ms | max {:.2}ms",
                            self.panes.len(),
                            self.frame_count,
                            avg.as_secs_f64() * 1000.0,
                            self.frame_time_max.as_secs_f64() * 1000.0,
                        );
                        self.frame_count = 0;
                        self.frame_time_sum = Duration::ZERO;
                        self.frame_time_max = Duration::ZERO;
                        self.stats_since = Instant::now();
                    }
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: Wake) {
        let _hb = self.heartbeat.as_ref().map(|h| h.guard());
        // PTY produced output and/or the AI agent streamed an event. Drain AI
        // events first so chat updates render this frame.
        self.drain_ai();
        // Mark panes dirty, then throttle: redraw at most once per frame
        // interval. If we're inside the interval, defer; the about_to_wait
        // timer fires the redraw. Drops intermediate frames under heavy output.
        self.any_dirty();
        if self.pending_redraw {
            return;
        }
        let min_interval = Duration::from_micros(8000); // ~120 fps cap
        let since = self.last_frame.elapsed();
        if since >= min_interval {
            self.pending_redraw = true;
            self.request_redraw();
        } else {
            // Defer: ensure the loop wakes to render at the next slot.
            self.deferred_redraw = true;
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.deferred_redraw && !self.pending_redraw {
            let min_interval = Duration::from_micros(8000);
            let since = self.last_frame.elapsed();
            if since >= min_interval {
                self.deferred_redraw = false;
                self.pending_redraw = true;
                self.request_redraw();
                event_loop.set_control_flow(ControlFlow::Wait);
            } else {
                // Sleep just until the next frame slot, then redraw.
                let wake_at = Instant::now() + (min_interval - since);
                event_loop.set_control_flow(ControlFlow::WaitUntil(wake_at));
            }
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

/// Translate a winit key into the bytes a shell expects.
fn key_to_bytes(key: &Key, text: Option<&str>, ctrl: bool, alt: bool) -> Vec<u8> {
    match key {
        Key::Named(named) => match named {
            NamedKey::Enter => vec![b'\r'],
            // Option+Backspace deletes the previous word (Ctrl+W).
            NamedKey::Backspace if alt => vec![0x17],
            NamedKey::Backspace => vec![0x7f],
            NamedKey::Tab => vec![b'\t'],
            NamedKey::Escape => vec![0x1b],
            // Option+Arrow = word motion (ESC b / ESC f), else normal arrows.
            NamedKey::ArrowLeft if alt => vec![0x1b, b'b'],
            NamedKey::ArrowRight if alt => vec![0x1b, b'f'],
            NamedKey::ArrowUp => vec![0x1b, b'[', b'A'],
            NamedKey::ArrowDown => vec![0x1b, b'[', b'B'],
            NamedKey::ArrowRight => vec![0x1b, b'[', b'C'],
            NamedKey::ArrowLeft => vec![0x1b, b'[', b'D'],
            NamedKey::Home => vec![0x1b, b'[', b'H'],
            NamedKey::End => vec![0x1b, b'[', b'F'],
            NamedKey::Delete => vec![0x1b, b'[', b'3', b'~'],
            NamedKey::Space => vec![b' '],
            _ => Vec::new(),
        },
        Key::Character(s) => {
            if ctrl {
                if let Some(ch) = s.chars().next() {
                    let lower = ch.to_ascii_lowercase();
                    if lower.is_ascii_alphabetic() {
                        return vec![(lower as u8 - b'a') + 1];
                    }
                }
            }
            text.map(|t| t.as_bytes().to_vec())
                .unwrap_or_else(|| s.as_bytes().to_vec())
        }
        _ => text.map(|t| t.as_bytes().to_vec()).unwrap_or_default(),
    }
}

/// Short label for a window event, used in freeze breadcrumbs.
fn event_label(event: &WindowEvent) -> &'static str {
    match event {
        WindowEvent::RedrawRequested => "redraw",
        WindowEvent::KeyboardInput { .. } => "key",
        WindowEvent::MouseInput { .. } => "mouse",
        WindowEvent::MouseWheel { .. } => "wheel",
        WindowEvent::CursorMoved { .. } => "cursor",
        WindowEvent::Resized(_) => "resize",
        WindowEvent::ModifiersChanged(_) => "mods",
        WindowEvent::CloseRequested => "close",
        _ => "other",
    }
}

/// Format a token count compactly (1234 -> 1.2k, 1500000 -> 1.5M).
fn compact_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Parse strings like "4x4", "3x2", "4 2", "2,3" into (cols, rows).
fn parse_grid(s: &str) -> Option<(usize, usize)> {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_ascii_digit() { c } else { ' ' })
        .collect();
    let parts: Vec<usize> = cleaned
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect();
    match parts.as_slice() {
        [c, r] if *c > 0 && *r > 0 && *c <= 12 && *r <= 12 => Some((*c, *r)),
        [n] if *n > 0 && *n <= 12 => Some((*n, *n)),
        _ => None,
    }
}

fn main() {
    env_logger::init();
    // Capture panics (full backtrace) and freezes to ~/.gridterm/crash.log.
    crashlog::install_panic_hook();
    let heartbeat = crashlog::Heartbeat::new();
    heartbeat.spawn_watchdog(std::time::Duration::from_secs(5));

    let event_loop = EventLoop::<Wake>::with_user_event().build().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy);
    app.heartbeat = Some(heartbeat);
    if let Err(e) = event_loop.run_app(&mut app) {
        crashlog::append("EVENTLOOP_ERROR", &format!("{e:?}"));
    }
}
