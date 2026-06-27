use std::io::{BufRead, BufReader};
use std::sync::mpsc::{Receiver, Sender};

use serde::Deserialize;
use serde_json::{json, Value};

use super::config::AiConfig;

/// A chat message in the conversation, mirroring the OpenAI schema closely.
#[derive(Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    /// For assistant messages that requested tools.
    pub tool_calls: Vec<ToolCall>,
    /// For tool result messages: which call id this answers.
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn tool_result(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(id.into()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments string.
    pub arguments: String,
}

/// Events streamed from the agent thread back to the UI.
#[derive(Debug)]
pub enum AiEvent {
    /// A chunk of assistant text.
    Token(String),
    /// Display-only: the agent is invoking a tool (show it in chat).
    ToolCall(ToolCall),
    /// The UI must execute this pane-touching tool and reply with ToolResult.
    ExecPaneTool(ToolCall),
    /// The assistant turn finished (no more tools this round).
    Done,
    /// An error occurred.
    Error(String),
}

/// Convert our messages into the OpenAI JSON array.
fn messages_json(messages: &[ChatMessage]) -> Value {
    let arr: Vec<Value> = messages
        .iter()
        .map(|m| {
            let mut obj = json!({ "role": m.role });
            if !m.content.is_empty() || m.tool_calls.is_empty() {
                obj["content"] = json!(m.content);
            }
            if !m.tool_calls.is_empty() {
                obj["tool_calls"] = json!(m
                    .tool_calls
                    .iter()
                    .map(|tc| json!({
                        "id": tc.id,
                        "type": "function",
                        "function": { "name": tc.name, "arguments": tc.arguments }
                    }))
                    .collect::<Vec<_>>());
            }
            if let Some(id) = &m.tool_call_id {
                obj["tool_call_id"] = json!(id);
            }
            obj
        })
        .collect();
    json!(arr)
}

/// Stream one assistant turn. Sends Token events live as they arrive (for the
/// UI to render) and returns the full assistant text plus any tool calls.
/// Blocking; runs on the agent's background thread. Aborts early if `cancel`
/// is set.
pub fn stream_turn(
    cfg: &AiConfig,
    messages: &[ChatMessage],
    tools: &Value,
    tx: &Sender<AiEvent>,
    wake: &(dyn Fn() + Send + Sync),
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(String, Vec<ToolCall>), String> {
    let body = json!({
        "messages": messages_json(messages),
        "tools": tools,
        "stream": true,
        "max_completion_tokens": 4096,
    });

    let resp = ureq::post(&cfg.chat_url())
        .set("api-key", &cfg.api_key)
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| format!("request failed: {e}"))?;

    let reader = BufReader::new(resp.into_reader());
    let mut full_text = String::new();
    let mut tool_acc: Vec<(String, String, String)> = Vec::new(); // (id, name, args)

    for line in reader.lines() {
        // Bail immediately if the user requested a stop.
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let data = match line.strip_prefix("data: ") {
            Some(d) => d,
            None => continue,
        };
        if data.trim() == "[DONE]" {
            break;
        }
        let chunk: StreamChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let Some(choice) = chunk.choices.into_iter().next() else {
            continue;
        };
        if let Some(content) = choice.delta.content {
            if !content.is_empty() {
                full_text.push_str(&content);
                let _ = tx.send(AiEvent::Token(content));
                wake();
            }
        }
        if let Some(calls) = choice.delta.tool_calls {
            for tc in calls {
                let idx = tc.index as usize;
                while tool_acc.len() <= idx {
                    tool_acc.push((String::new(), String::new(), String::new()));
                }
                if let Some(id) = tc.id {
                    tool_acc[idx].0 = id;
                }
                if let Some(f) = tc.function {
                    if let Some(name) = f.name {
                        tool_acc[idx].1 = name;
                    }
                    if let Some(args) = f.arguments {
                        tool_acc[idx].2.push_str(&args);
                    }
                }
            }
        }
    }

    let tool_calls: Vec<ToolCall> = tool_acc
        .into_iter()
        .filter(|(_, name, _)| !name.is_empty())
        .map(|(id, name, arguments)| ToolCall {
            id,
            name,
            arguments,
        })
        .collect();
    Ok((full_text, tool_calls))
}

// ---- SSE chunk deserialization ----

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCall>>,
}

#[derive(Deserialize)]
struct StreamToolCall {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamToolFn>,
}

#[derive(Deserialize)]
struct StreamToolFn {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// A request from the UI to the agent thread (a new user turn or a tool result).
pub enum AgentInput {
    User { text: String, context: String },
    ToolResult { id: String, content: String },
}

/// Tools the agent executes ITSELF on its background thread (they only need the
/// private shell, never the user's panes), so blocking work (sleep, polling a
/// command) never freezes the UI.
fn is_local_tool(name: &str) -> bool {
    matches!(
        name,
        "shell"
            | "shell_bg"
            | "read_shell"
            | "wait"
            | "browser_open"
            | "browser_read"
            | "browser_request_login"
            | "browser_save_login"
            | "browser_setup"
    )
}

/// Spawn the agent loop on a background thread. It owns the conversation,
/// streams turns, and emits AiEvents. Tool calls are sent to the UI which
/// replies with AgentInput::ToolResult on the same `rx` channel. `cancel` is a
/// shared flag the UI can set to abort the current turn.
pub fn spawn_agent(
    cfg: AiConfig,
    tools: Value,
    system_prompt: String,
    tx: Sender<AiEvent>,
    rx: Receiver<AgentInput>,
    wake: std::sync::Arc<dyn Fn() + Send + Sync>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    mut ai_shell: Option<crate::pty::PtyTerm>,
) {
    std::thread::spawn(move || {
        let mut messages = vec![ChatMessage::system(system_prompt)];
        while let Ok(input) = rx.recv() {
            match input {
                AgentInput::User { text, context } => {
                    cancel.store(false, std::sync::atomic::Ordering::Relaxed);
                    if !context.is_empty() {
                        messages.push(ChatMessage::user(format!(
                            "[terminal context]\n{context}\n[/terminal context]\n\n{text}"
                        )));
                    } else {
                        messages.push(ChatMessage::user(text));
                    }
                    run_until_settled(
                        &cfg, &tools, &mut messages, &tx, &rx, &*wake, &cancel, &mut ai_shell,
                    );
                }
                AgentInput::ToolResult { .. } => {}
            }
        }
    });
}

/// Execute a local (private-shell) tool on the agent thread. Returns None if
/// the tool isn't local (and must be sent to the UI for pane access).
fn execute_local_tool(
    tc: &ToolCall,
    ai_shell: &mut Option<crate::pty::PtyTerm>,
    cancel: &std::sync::atomic::AtomicBool,
) -> Option<String> {
    if !is_local_tool(&tc.name) {
        return None;
    }
    let args: Value = serde_json::from_str(&tc.arguments).unwrap_or(Value::Null);
    let result = match tc.name.as_str() {
        "wait" => {
            let secs = args["seconds"].as_f64().unwrap_or(2.0).clamp(0.1, 30.0);
            // Sleep in short slices so a stop request is honored quickly.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(secs);
            while std::time::Instant::now() < deadline {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    return Some("(wait cancelled)".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            format!("waited {secs:.1}s")
        }
        "shell" => {
            let cmd = args["command"].as_str().unwrap_or("");
            match ai_shell {
                Some(sh) => run_in_shell(sh, cmd, cancel),
                None => "(ai shell unavailable)".into(),
            }
        }
        "shell_bg" => {
            let cmd = args["command"].as_str().unwrap_or("");
            match ai_shell {
                Some(sh) => {
                    sh.write(format!("{cmd}\n").as_bytes());
                    format!("started in background: {cmd}\n(use wait + read_shell to monitor)")
                }
                None => "(ai shell unavailable)".into(),
            }
        }
        "read_shell" => {
            let lines = args["lines"].as_i64().unwrap_or(80).clamp(1, 400) as usize;
            match ai_shell {
                Some(sh) => crate::ai::tools::truncate_tokens(&sh.scrollback_text(lines), 1500),
                None => "(ai shell unavailable)".into(),
            }
        }
        // Browser tools talk to the broker; they may block for a navigation, so
        // running them here (agent thread) keeps the UI responsive.
        "browser_open" => {
            let url = args["url"].as_str().unwrap_or("");
            if url.is_empty() {
                "error: no url provided".into()
            } else {
                crate::ai::tools::truncate_tokens(&crate::browser::browser_open(url), 2000)
            }
        }
        "browser_read" => {
            crate::ai::tools::truncate_tokens(&crate::browser::browser_read(), 2000)
        }
        "browser_request_login" => {
            crate::browser::surface_for_login(args["url"].as_str())
        }
        "browser_save_login" => crate::browser::save_login_state(),
        "browser_setup" => {
            let mode = match args["mode"].as_str().unwrap_or("on") {
                "off" => crate::browser::BrowserMode::Off,
                _ => crate::browser::BrowserMode::Shared,
            };
            match crate::browser::set_mode(mode) {
                Ok(msg) => msg,
                Err(e) => format!("failed: {e}"),
            }
        }
        _ => return None,
    };
    Some(result)
}

/// Run a command in the agent's private shell, capturing output via sentinel
/// markers. Runs on the agent thread, so blocking here never freezes the UI.
fn run_in_shell(
    shell: &mut crate::pty::PtyTerm,
    cmd: &str,
    cancel: &std::sync::atomic::AtomicBool,
) -> String {
    let marker = format!(
        "__GT_{}__",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let begin = format!("{marker}BEGIN");
    let end = format!("{marker}END");
    shell.write(format!("echo {begin}; {cmd}; echo {end}$?\n").as_bytes());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return "(command cancelled)".into();
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
        let text = shell.scrollback_text(4000);
        if let (Some(bpos), Some(epos)) = (text.rfind(&begin), text.rfind(&end)) {
            if epos > bpos {
                let out = text[bpos + begin.len()..epos].trim_matches('\n');
                return crate::ai::tools::truncate_tokens(out, 1500);
            }
        }
        if std::time::Instant::now() > deadline {
            return format!(
                "(timed out after 30s; partial output)\n{}",
                crate::ai::tools::truncate_tokens(&shell.scrollback_text(200), 1000)
            );
        }
    }
}

/// Run assistant turns, executing tool calls (via the UI) until the model
/// produces a final answer with no tool calls.
fn run_until_settled(
    cfg: &AiConfig,
    tools: &Value,
    messages: &mut Vec<ChatMessage>,
    tx: &Sender<AiEvent>,
    rx: &Receiver<AgentInput>,
    wake: &(dyn Fn() + Send + Sync),
    cancel: &std::sync::atomic::AtomicBool,
    ai_shell: &mut Option<crate::pty::PtyTerm>,
) {
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = tx.send(AiEvent::Done);
            wake();
            return;
        }
        let (assistant_text, pending_tools) =
            match stream_turn(cfg, messages, tools, tx, wake, cancel) {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(AiEvent::Error(e));
                    wake();
                    return;
                }
            };

        messages.push(ChatMessage {
            role: "assistant".into(),
            content: assistant_text,
            tool_calls: pending_tools.clone(),
            tool_call_id: None,
        });

        if pending_tools.is_empty() {
            let _ = tx.send(AiEvent::Done);
            wake();
            return;
        }

        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = tx.send(AiEvent::Done);
            wake();
            return;
        }

        // Execute each tool. Local tools (shell/wait) run here on the agent
        // thread so blocking never freezes the UI; pane tools round-trip to the
        // UI which owns the terminals.
        for tc in pending_tools {
            // Always show the action in the chat.
            let _ = tx.send(AiEvent::ToolCall(tc.clone()));
            wake();

            let result = if let Some(local) = execute_local_tool(&tc, ai_shell, cancel) {
                local
            } else {
                // Pane tool: ask the UI to run it and await the result.
                let _ = tx.send(AiEvent::ExecPaneTool(tc.clone()));
                wake();
                let mut r = String::from("(no result)");
                while let Ok(input) = rx.recv() {
                    if let AgentInput::ToolResult { id, content } = input {
                        if id == tc.id {
                            r = content;
                            break;
                        }
                    }
                }
                r
            };
            messages.push(ChatMessage::tool_result(tc.id, result));
        }
    }
}
