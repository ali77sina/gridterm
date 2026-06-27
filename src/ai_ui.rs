use crate::ai::ToolCall;

/// A visible chat entry in the conversation log.
pub struct ChatEntry {
    pub role: Role,
    pub text: String,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

/// All UI-side state for the AI chat overlay.
pub struct ChatState {
    pub open: bool,
    pub entries: Vec<ChatEntry>,
    pub input: String,
    /// Caret byte-offset within `input`.
    pub input_cursor: usize,
    /// True when the chat input has keyboard focus (vs a terminal pane).
    pub input_focused: bool,
    /// True while a turn is streaming (disables sending another).
    pub streaming: bool,
    /// Whether the AI is available (config loaded).
    pub available: bool,
    /// Pixels scrolled up from the bottom of the conversation.
    pub scroll: f32,
    /// True to snap to the newest content on the next render.
    pub stick_bottom: bool,
}

impl ChatState {
    pub fn new(available: bool) -> Self {
        Self {
            open: false,
            entries: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            input_focused: false,
            streaming: false,
            available,
            scroll: 0.0,
            stick_bottom: true,
        }
    }

    /// The most recent assistant message text, for Cmd+C copy.
    pub fn last_assistant_text(&self) -> Option<String> {
        self.entries
            .iter()
            .rev()
            .find(|e| e.role == Role::Assistant)
            .map(|e| e.text.clone())
            .filter(|t| !t.is_empty())
    }

    /// The full visible transcript (user + assistant + tool lines), for copying
    /// the whole conversation.
    pub fn transcript_text(&self) -> String {
        self.entries
            .iter()
            .map(|e| {
                let tag = match e.role {
                    Role::User => "You",
                    Role::Assistant => "AI",
                    Role::Tool => "tool",
                };
                format!("{tag}: {}", e.text)
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Append streamed text to the current assistant entry (or start one).
    pub fn push_assistant_token(&mut self, t: &str) {
        self.stick_bottom = true;
        match self.entries.last_mut() {
            Some(e) if e.role == Role::Assistant => e.text.push_str(t),
            _ => self.entries.push(ChatEntry {
                role: Role::Assistant,
                text: t.to_string(),
            }),
        }
    }

    pub fn push_user(&mut self, text: String) {
        self.stick_bottom = true;
        self.entries.push(ChatEntry {
            role: Role::User,
            text,
        });
    }

    pub fn push_tool(&mut self, text: String) {
        self.stick_bottom = true;
        self.entries.push(ChatEntry {
            role: Role::Tool,
            text,
        });
    }

    /// Insert a string at the caret.
    pub fn insert(&mut self, s: &str) {
        let at = self.input_cursor.min(self.input.len());
        self.input.insert_str(at, s);
        self.input_cursor = at + s.len();
    }

    /// Delete the character before the caret (Backspace).
    pub fn backspace(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.input, self.input_cursor);
        self.input.replace_range(prev..self.input_cursor, "");
        self.input_cursor = prev;
    }

    /// Delete the word before the caret (Option+Backspace).
    pub fn delete_word(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let bytes = self.input.as_bytes();
        let mut i = self.input_cursor;
        while i > 0 && bytes[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        while i > 0 && !bytes[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        self.input.replace_range(i..self.input_cursor, "");
        self.input_cursor = i;
    }

    /// Delete from the caret to the start of the line (Cmd+Backspace).
    pub fn delete_to_line_start(&mut self) {
        let start = self.input[..self.input_cursor]
            .rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        self.input.replace_range(start..self.input_cursor, "");
        self.input_cursor = start;
    }

    pub fn move_left(&mut self) {
        if self.input_cursor > 0 {
            self.input_cursor = prev_char_boundary(&self.input, self.input_cursor);
        }
    }

    pub fn move_right(&mut self) {
        if self.input_cursor < self.input.len() {
            self.input_cursor = next_char_boundary(&self.input, self.input_cursor);
        }
    }

    pub fn move_home(&mut self) {
        self.input_cursor = self.input[..self.input_cursor]
            .rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(0);
    }

    pub fn move_end(&mut self) {
        self.input_cursor = self.input[self.input_cursor..]
            .find('\n')
            .map(|p| self.input_cursor + p)
            .unwrap_or(self.input.len());
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
    }
}

fn prev_char_boundary(s: &str, i: usize) -> usize {
    let mut p = i - 1;
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

fn next_char_boundary(s: &str, i: usize) -> usize {
    let mut p = i + 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}

/// Render a short human-readable summary line for a tool call (shown in chat).
pub fn tool_summary(tc: &ToolCall) -> String {
    let args: serde_json::Value =
        serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null);
    match tc.name.as_str() {
        "run_command" => format!(
            "$ {} (pane {})",
            args["command"].as_str().unwrap_or("?"),
            args["pane"].as_i64().unwrap_or(-1)
        ),
        "read_pane" => format!("read pane {}", args["pane"].as_i64().unwrap_or(-1)),
        "send_keys" => format!("send keys to pane {}", args["pane"].as_i64().unwrap_or(-1)),
        "grep_pane" => format!(
            "grep '{}' in pane {}",
            args["pattern"].as_str().unwrap_or("?"),
            args["pane"].as_i64().unwrap_or(-1)
        ),
        "list_panes" => "list panes".to_string(),
        other => other.to_string(),
    }
}
