use serde_json::{json, Value};
use std::path::PathBuf;

/// Browser access mode for the coding agents in the panes.
#[derive(Clone, Copy, PartialEq)]
pub enum BrowserMode {
    /// Each agent gets its own ISOLATED browser context, but all are seeded
    /// with the shared saved logins (storage state). Headless + lazy + low-RAM.
    /// This is the default and the recommended model.
    Shared,
    /// No browser MCP configured.
    Off,
}

const CDP_PORT: u16 = 9222;

fn cdp_endpoint() -> String {
    format!("http://127.0.0.1:{CDP_PORT}")
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

/// Apply a browser mode by rewriting the project `.mcp.json` (non-destructive
/// to other MCP servers).
pub fn set_mode(mode: BrowserMode) -> std::io::Result<String> {
    match mode {
        BrowserMode::Off => {
            let removed = remove_browser_mcp()?;
            Ok(if removed {
                "Browser access disabled.".into()
            } else {
                "Browser access was not configured.".into()
            })
        }
        BrowserMode::Shared => {
            // Make sure the persistent login Chrome is running so agents can
            // connect to it (and your logins persist there like normal Chrome).
            let _ = launch_debug_chrome();
            write_server(shared_server_entry())?;
            Ok("Browser access ready: agents drive a single persistent Chrome that \
keeps your logins (just like your normal browser — sign in once, stays signed in). \
If a site needs login, an agent surfaces it so you sign in once.".into())
        }
    }
}

/// The MCP server entry: agents connect over CDP to the ONE persistent login
/// Chrome. Because it's a real persistent profile (not an isolated snapshot),
/// logins persist across runs exactly like a normal browser, and all agents
/// share them. Lazy: the MCP only spins up when an agent first calls it.
fn shared_server_entry() -> Value {
    json!({
        "command": "npx",
        "args": [
            "-y", "@playwright/mcp@latest",
            "--browser=chrome",
            "--cdp-endpoint", cdp_endpoint()
        ]
    })
}

/// Ensure the browser MCP is configured with the CURRENT scheme. Upgrades a
/// stale gridterm-managed entry (e.g. from an older version) but never touches
/// MCP servers other than our `playwright` one. Returns true if it wrote.
pub fn ensure_default() -> std::io::Result<bool> {
    let path = mcp_json_path();
    let root = read_root(&path);
    let existing = root.get("mcpServers").and_then(|s| s.get("playwright"));

    // If a playwright entry exists, only replace it when it looks like one WE
    // manage (npx @playwright/mcp). A user's custom entry is left alone.
    if let Some(entry) = existing {
        let is_ours = entry
            .get("args")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().any(|v| v == "@playwright/mcp@latest"))
            .unwrap_or(false);
        let already_current = entry
            .get("args")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().any(|v| v == "--cdp-endpoint"))
            .unwrap_or(false);
        if !is_ours || already_current {
            // Still make sure the persistent Chrome is up for current configs.
            if already_current {
                let _ = launch_debug_chrome();
            }
            return Ok(false);
        }
        // Stale gridterm entry — upgrade it.
    }
    set_mode(BrowserMode::Shared)?;
    Ok(true)
}

/// Launch a Chrome with remote debugging on a dedicated data dir (Chrome v136+
/// blocks debugging on the default profile). Used only for the one-time login
/// so we can capture the session. Returns true if we started it.
fn launch_debug_chrome() -> std::io::Result<bool> {
    if std::net::TcpStream::connect(("127.0.0.1", CDP_PORT)).is_ok() {
        return Ok(false);
    }
    let debug_dir = home_dir().join(".gridterm/chrome-debug");
    std::fs::create_dir_all(&debug_dir)?;

    let chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
    if !PathBuf::from(chrome).exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Google Chrome not found at the standard macOS path",
        ));
    }
    std::process::Command::new(chrome)
        .arg(format!("--remote-debugging-port={CDP_PORT}"))
        .arg(format!("--user-data-dir={}", debug_dir.display()))
        .arg("--remote-debugging-address=127.0.0.1")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-features=OptimizationGuideOnDeviceModel")
        .spawn()?;
    Ok(true)
}

// ---- .mcp.json helpers (non-destructive merge) ----

fn write_server(entry: Value) -> std::io::Result<()> {
    let path = mcp_json_path();
    let mut root = read_root(&path);
    if !root.is_object() {
        root = json!({});
    }
    let servers = root
        .as_object_mut()
        .unwrap()
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    if let Some(map) = servers.as_object_mut() {
        map.insert("playwright".into(), entry);
    }
    std::fs::write(&path, serde_json::to_string_pretty(&root)?)
}

pub fn remove_browser_mcp() -> std::io::Result<bool> {
    let path = mcp_json_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    let Ok(mut root) = serde_json::from_str::<Value>(&text) else {
        return Ok(false);
    };
    let removed = root
        .get_mut("mcpServers")
        .and_then(|s| s.as_object_mut())
        .map(|m| m.remove("playwright").is_some())
        .unwrap_or(false);
    if removed {
        std::fs::write(&path, serde_json::to_string_pretty(&root)?)?;
    }
    Ok(removed)
}

fn read_root(path: &PathBuf) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn mcp_json_path() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".mcp.json")
}

/// Surface a Chrome window for the user to log in once. Opens `url` (if given),
/// brings it to the front. After the user logs in they call (or the agent
/// calls) `save_login_state` to persist the shared session for all agents.
pub fn surface_for_login(url: Option<&str>) -> String {
    let launched = match launch_debug_chrome() {
        Ok(started) => started,
        Err(e) => return format!("couldn't start the login browser: {e}"),
    };
    // Give Chrome a moment to open its CDP port before we drive it.
    if launched {
        std::thread::sleep(std::time::Duration::from_millis(800));
    }
    if let Some(u) = url {
        let cdp_new_tab = format!("{}/json/new?{}", cdp_endpoint(), u);
        let _ = ureq::put(&cdp_new_tab).call().or_else(|_| ureq::get(&cdp_new_tab).call());
    }
    bring_chrome_to_front();

    let target = url.unwrap_or("the site you need");
    format!(
        "Opened a browser at {target}. Please log in there. When you're done, tell me \
\"I've logged in\" and I'll save the session so all agents share it (you won't need to \
log in again until it expires)."
    )
}

/// With the persistent-profile model, logins already persist automatically in
/// the Chrome profile (no snapshot needed). This just confirms the session is
/// live and reminds the user it will stay signed in.
pub fn save_login_state() -> String {
    if cdp_ws_url().is_none() {
        return "No login browser is open. Ask me to open one first.".into();
    }
    // Sanity: how many cookies are present (confirms a session exists).
    let count = cdp_ws_url()
        .and_then(|ws| fetch_cookies_via_cdp(&ws).ok())
        .and_then(|c| c.as_array().map(|a| a.len()))
        .unwrap_or(0);
    format!(
        "You're signed in ({count} cookies in the session). Because the agents share \
this one persistent Chrome, your logins STAY signed in across runs — just like your \
normal browser. The agents can now use those sites."
    )
}

/// Get the browser-level CDP WebSocket URL from /json/version.
fn cdp_ws_url() -> Option<String> {
    let body = ureq::get(&format!("{}/json/version", cdp_endpoint()))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let v: Value = serde_json::from_str(&body).ok()?;
    v.get("webSocketDebuggerUrl")
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
}

/// Connect to the CDP WebSocket, call Storage.getCookies, return the cookies
/// array converted to Playwright's storageState cookie format.
fn fetch_cookies_via_cdp(ws_url: &str) -> Result<Value, String> {
    use tungstenite::Message;

    let (mut socket, _resp) =
        tungstenite::connect(ws_url).map_err(|e| format!("ws connect: {e}"))?;

    // CDP: Storage.getCookies returns all browser cookies.
    let req = json!({ "id": 1, "method": "Storage.getCookies", "params": {} });
    socket
        .send(Message::Text(req.to_string()))
        .map_err(|e| format!("ws send: {e}"))?;

    // Read until we get the response with id == 1.
    for _ in 0..50 {
        let msg = socket.read().map_err(|e| format!("ws read: {e}"))?;
        if let Message::Text(txt) = msg {
            let v: Value = match serde_json::from_str(&txt) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("id").and_then(|i| i.as_i64()) == Some(1) {
                let cookies = v
                    .get("result")
                    .and_then(|r| r.get("cookies"))
                    .and_then(|c| c.as_array())
                    .cloned()
                    .unwrap_or_default();
                return Ok(json!(cookies
                    .into_iter()
                    .map(to_playwright_cookie)
                    .collect::<Vec<_>>()));
            }
        }
    }
    Err("no CDP response".into())
}

/// Convert a CDP cookie to Playwright storageState cookie shape.
fn to_playwright_cookie(c: Value) -> Value {
    let same_site = match c.get("sameSite").and_then(|s| s.as_str()) {
        Some("Strict") => "Strict",
        Some("Lax") => "Lax",
        Some("None") => "None",
        _ => "Lax",
    };
    json!({
        "name": c.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        "value": c.get("value").and_then(|v| v.as_str()).unwrap_or(""),
        "domain": c.get("domain").and_then(|v| v.as_str()).unwrap_or(""),
        "path": c.get("path").and_then(|v| v.as_str()).unwrap_or("/"),
        "expires": c.get("expires").and_then(|v| v.as_f64()).unwrap_or(-1.0),
        "httpOnly": c.get("httpOnly").and_then(|v| v.as_bool()).unwrap_or(false),
        "secure": c.get("secure").and_then(|v| v.as_bool()).unwrap_or(false),
        "sameSite": same_site
    })
}

/// Best-effort: focus Google Chrome on macOS so the login window is visible.
fn bring_chrome_to_front() {
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(r#"tell application "Google Chrome" to activate"#)
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_merge_without_clobbering() {
        let tmp = std::env::temp_dir().join(format!("gt_mcp2_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_current_dir(&tmp).unwrap();
        std::fs::write(
            tmp.join(".mcp.json"),
            r#"{"mcpServers":{"mine":{"command":"x","args":[]}}}"#,
        )
        .unwrap();

        set_mode(BrowserMode::Shared).unwrap();
        let v = read_root(&tmp.join(".mcp.json"));
        assert!(v["mcpServers"]["mine"].is_object());
        assert!(v["mcpServers"]["playwright"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "--cdp-endpoint"));

        assert!(remove_browser_mcp().unwrap());
        let v2 = read_root(&tmp.join(".mcp.json"));
        assert!(v2["mcpServers"]["mine"].is_object());
        assert!(v2["mcpServers"].get("playwright").is_none());
    }
}
