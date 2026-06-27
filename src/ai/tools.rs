use serde_json::{json, Value};

/// The JSON tool/function schema advertised to the model. Tool execution that
/// touches panes happens in the UI thread (it owns the terminals).
pub fn tool_schema() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "run_command",
                "description": "Type a command into a terminal pane and run it (presses Enter). Use this to execute shell commands, run code, install things, etc. Output appears in that pane; read it back with read_pane.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pane": { "type": "integer", "description": "Pane index (0-based) to run in." },
                        "command": { "type": "string", "description": "The exact shell command to run." }
                    },
                    "required": ["pane", "command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_pane",
                "description": "Read the recent output of a terminal pane, including scrollback. Use after run_command to see results.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pane": { "type": "integer", "description": "Pane index (0-based)." },
                        "lines": { "type": "integer", "description": "How many recent lines to read (default 80, max 400)." }
                    },
                    "required": ["pane"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "send_keys",
                "description": "Send raw keystrokes to a pane WITHOUT pressing Enter (e.g. to answer an interactive prompt, send Ctrl-C as \\u0003, or type 'y'). Use control chars directly.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pane": { "type": "integer" },
                        "keys": { "type": "string", "description": "Raw bytes to send (may include control chars)." }
                    },
                    "required": ["pane", "keys"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "grep_pane",
                "description": "Search a pane's visible+scrollback content for a substring (case-insensitive) and return matching lines.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pane": { "type": "integer" },
                        "pattern": { "type": "string" }
                    },
                    "required": ["pane", "pattern"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_panes",
                "description": "List all open terminal panes with their index, grid position, and current working directory if known.",
                "parameters": { "type": "object", "properties": {} }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Run a command in YOUR OWN private shell (not visible to the user, not one of their panes) and wait for it to finish (up to 30s). Use this for your own investigation: grep, find, cat, ls, running scripts, checking git, etc. Returns the command's output directly. Prefer this for read-only/inspection work so you don't clutter the user's terminals.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Shell command to run in your private shell." }
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "shell_bg",
                "description": "Start a long-running or never-ending command in your private shell WITHOUT waiting (returns immediately). Use for things you want to monitor over time: starting a server, tailing logs, watching a build. Combine with wait + read_shell to poll progress.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Command to start in the background." }
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_shell",
                "description": "Read the recent output of your private shell (after shell_bg or to recheck). Returns the last N lines.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "lines": { "type": "integer", "description": "Recent lines to read (default 80, max 400)." }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "wait",
                "description": "Pause for a number of seconds before continuing. Use this to let a command/server/build make progress, then read_pane or read_shell to observe. Lets you monitor things over time.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "seconds": { "type": "number", "description": "How long to wait (1-30)." }
                    },
                    "required": ["seconds"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "set_grid",
                "description": "Change the terminal grid layout to cols x rows. GROWING the grid is safe and keeps all existing terminals alive (new empty panes are added). SHRINKING the grid CLOSES the terminals (and their running processes) that no longer fit — only do this when the user explicitly asks to close/remove panes, and you must pass allow_close=true to confirm. Index is row-major, 0-based.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "cols": { "type": "integer", "description": "Number of columns (1-12)." },
                        "rows": { "type": "integer", "description": "Number of rows (1-12)." },
                        "allow_close": { "type": "boolean", "description": "Must be true to allow shrinking the grid (which closes terminals). Defaults false." }
                    },
                    "required": ["cols", "rows"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "usage_report",
                "description": "Get the captured token usage and estimated cost (USD) per terminal pane and in total, for coding agents (Claude Code, Codex) running in the panes. Use when the user asks what something has cost, how many tokens were used, or to summarize spend.",
                "parameters": { "type": "object", "properties": {} }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "browser_setup",
                "description": "Configure browser access for the coding agents. 'on' = each agent gets its own isolated browser session, all sharing the saved logins (headless, lazy, low-RAM). 'off' = disable. On by default.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "mode": { "type": "string", "enum": ["on", "off"] }
                    },
                    "required": ["mode"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "browser_request_login",
                "description": "Call this when a browser action is blocked by a login/auth wall. It opens a browser window so the user can sign in once. Pass the URL that needs login. After calling, tell the user to log in and then say when they're done so you can save the session.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "The URL that requires login (optional)." }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "browser_save_login",
                "description": "Call this AFTER the user confirms they have logged in (following browser_request_login). It captures the logged-in session and shares it with all agents (each keeps an isolated session but inherits the credentials), so they can browse authenticated sites headlessly afterward.",
                "parameters": { "type": "object", "properties": {} }
            }
        }
    ])
}

/// The agent's system prompt.
pub fn system_prompt(cols: usize, rows: usize) -> String {
    format!(
        "You are an AI agent embedded directly inside a fast grid terminal emulator on macOS. \
The user has multiple terminal panes arranged in a grid; each pane runs a real shell. \
You can SEE the panes (their content is provided as context) and you can DRIVE them via tools: \
run_command runs a shell command in a chosen pane, read_pane reads its output, send_keys sends raw \
keystrokes (for interactive prompts), grep_pane searches a pane, and list_panes lists them. \
You ALSO have your own private `shell` tool — a separate hidden shell only you can use — for your own \
investigation (grep, find, cat, git, running scripts) without cluttering the user's visible terminals.\n\n\
Operating rules:\n\
- You have full autonomy: run commands as needed to accomplish the user's goal without asking for confirmation.\n\
- DEFAULT TO YOUR PRIVATE `shell` for everything you do yourself: inspecting, grepping, reading files, \
running scripts, checking things. It does not disturb the user's terminals.\n\
- ONLY use run_command (which types into the user's VISIBLE panes) when the user explicitly wants \
something to run in a specific pane they can see, or asks you to start/drive a program in a pane. \
If in doubt, use your private `shell`, not their panes.\n\
- To MONITOR something over time (a build, a server, an agent like claude code running in a pane): \
start it with shell_bg (or run_command in a pane), then loop: wait(a few seconds) -> read_shell / read_pane \
-> decide. Repeat until done. This lets you watch progress without blocking.\n\
- The user may be running coding agents (e.g. claude code) inside the panes. You can read those panes, \
grep them, and react to what the agents are doing.\n\
- You can reshape the grid: use add_pane to add a terminal without disturbing existing ones, or set_grid \
to grow the layout (safe). NEVER shrink the grid (which closes terminals and kills their processes) unless \
the user explicitly asks to close panes — and only then with allow_close=true.\n\
- The agents share ONE persistent Chrome that keeps your logins like a normal browser — sign in once, it \
stays signed in across runs. So agents can test/navigate web apps behind login (your apps, Gmail, AWS). \
Manage with browser_setup (on/off).\n\
- If a browser action is blocked by a LOGIN: call browser_request_login (with the URL) to open the \
browser; tell the user to sign in. They stay signed in afterward (persistent profile), so you can \
continue. browser_save_login just confirms the session.\n\
- After running a command in a pane, ALWAYS read_pane to observe the result before concluding.\n\
- Prefer the pane the user is focused on unless they specify another. Pane indices are 0-based, row-major.\n\
- Be concise. Explain what you're doing in one line, then act. Summarize results briefly.\n\
- Each pane is about {cols}x{rows} cells. Long output is truncated; use grep_pane or targeted commands.\n\
- When a command might be long-running or interactive, account for that and use send_keys / read_pane.\n\
- Never fabricate command output; always read it from the pane."
    )
}

/// Smart-truncate text to a rough token budget (chars/4 heuristic), keeping the
/// most relevant tail (recent output) plus a head sample.
pub fn truncate_tokens(text: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens * 4;
    if text.len() <= max_chars {
        return text.to_string();
    }
    let head = max_chars / 4;
    let tail = max_chars - head;
    let head_str: String = text.chars().take(head).collect();
    let tail_str: String = text
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!(
        "{head_str}\n\n... [{} chars truncated] ...\n\n{tail_str}",
        text.len().saturating_sub(max_chars)
    )
}
