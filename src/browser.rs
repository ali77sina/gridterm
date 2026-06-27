use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Browser access mode for the coding agents in the panes.
#[derive(Clone, Copy, PartialEq)]
pub enum BrowserMode {
    /// One shared persistent Chrome (your real logins), brokered so that every
    /// panel agent gets its OWN tab in that one browser. Cookies/logins are
    /// shared across panels (sign in once); tabs are isolated like two browser
    /// tabs. One browser engine + one Node broker total -> low RAM/CPU/GPU.
    /// This is the default and only "on" model.
    Shared,
    /// No browser MCP configured.
    Off,
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

fn gt_dir() -> PathBuf {
    home_dir().join(".gridterm")
}

fn broker_dir() -> PathBuf {
    if let Ok(d) = std::env::var("GRIDTERM_BROKER_DIR") {
        return PathBuf::from(d);
    }
    gt_dir().join("broker")
}

fn broker_sock() -> PathBuf {
    gt_dir().join("browser-broker.sock")
}

/// The broker's source files, embedded so the shipped binary is self-contained.
/// They are materialized to `~/.gridterm/broker` on first use, then `npm
/// install`ed there (once).
const BROKER_JS: &str = include_str!("../browser/broker.mjs");
const BROKER_PKG: &str = include_str!("../browser/package.json");

/// Path to the shim binary we hand to panel agents as their MCP `command`.
/// Prefer a sibling of the running executable; fall back to a copy in
/// `~/.gridterm`.
fn shim_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sib = dir.join("gridterm-browser-shim");
            if sib.exists() {
                return sib;
            }
        }
    }
    gt_dir().join("gridterm-browser-shim")
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
            // Bring the broker (and thus the one shared Chrome) up in the
            // background so the config write returns instantly and the UI never
            // blocks on a first-run `npm install`.
            spawn_broker_bringup();
            // USER-SCOPE config: write the browser MCP + permissions into Claude
            // Code's user-level files so `claude` in ANY directory has the browser
            // tools (not just gridterm's launch dir). This is what makes it
            // dynamic across projects.
            write_user_mcp_server(shim_server_entry())?;
            ensure_user_autoapprove()?;
            Ok("Browser access ready: every panel agent gets its own tab in one shared Chrome \
that keeps your logins (sign in once, stays signed in like a normal browser). Works for agents \
started in ANY directory. If a site needs login, an agent surfaces the window for you to sign in \
once; all panels then share it.".into())
        }
    }
}

/// The MCP server entry handed to panel agents: run our tiny native shim, which
/// pipes the MCP stream to the single broker. We pass GRIDTERM_BROKER_CMD so the
/// shim can cold-start the broker if it raced ahead of gridterm.
fn shim_server_entry() -> Value {
    let shim = shim_path();
    json!({
        "command": shim.to_string_lossy(),
        "args": [],
        "env": {
            "GRIDTERM_BROKER_CMD": broker_bootstrap_cmd()
        }
    })
}

/// A shell line that starts the broker detached (used by the shim as a fallback
/// if gridterm hasn't started it yet).
fn broker_bootstrap_cmd() -> String {
    let dir = broker_dir();
    format!(
        "cd {} >/dev/null 2>&1 && nohup node broker.mjs >/dev/null 2>&1 &",
        shell_quote(&dir.to_string_lossy())
    )
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Claude Code won't run an MCP server's tools without permission, and prompts
/// per-tool otherwise. To make this seamless in EVERY directory (user scope),
/// we add our playwright server to the user-level allow list in
/// `~/.claude/settings.json`. Non-destructive: preserves the user's existing
/// env/permissions/settings.
fn ensure_user_autoapprove() -> std::io::Result<()> {
    let home = home_dir();
    let claude_dir = home.join(".claude");
    std::fs::create_dir_all(&claude_dir)?;
    let path = claude_dir.join("settings.json");
    let mut root = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({}));
    if !root.is_object() {
        root = json!({});
    }
    let obj = root.as_object_mut().unwrap();

    // Pre-approve every tool from our playwright server so the agent NEVER stops
    // to ask the user for permission per-tool. The bare server name
    // "mcp__playwright" allows all of that server's tools (wildcards in the allow
    // list are unreliable). Merge non-destructively.
    let perms = obj.entry("permissions").or_insert_with(|| json!({}));
    if !perms.is_object() {
        *perms = json!({});
    }
    let allow = perms
        .as_object_mut()
        .unwrap()
        .entry("allow")
        .or_insert_with(|| json!([]));
    if let Some(arr) = allow.as_array_mut() {
        if !arr.iter().any(|v| v == "mcp__playwright") {
            arr.push(json!("mcp__playwright"));
        }
    } else {
        *allow = json!(["mcp__playwright"]);
    }

    std::fs::write(&path, serde_json::to_string_pretty(&root)?)
}

/// Path to Claude Code's user-scope config that holds top-level `mcpServers`.
fn user_claude_json() -> PathBuf {
    home_dir().join(".claude.json")
}

/// Write our `playwright` server into the USER-scope mcpServers so it's available
/// in every directory. Non-destructive to other servers and all other keys.
fn write_user_mcp_server(entry: Value) -> std::io::Result<()> {
    let path = user_claude_json();
    let mut root = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({}));
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

/// Ensure the browser is configured with the CURRENT scheme at USER scope.
/// Upgrades a stale gridterm-managed entry but never touches other MCP servers.
/// Returns true if it wrote.
pub fn ensure_default() -> std::io::Result<bool> {
    let path = user_claude_json();
    let root = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({}));
    let existing = root.get("mcpServers").and_then(|s| s.get("playwright"));

    if let Some(entry) = existing {
        let cmd = entry.get("command").and_then(|c| c.as_str()).unwrap_or("");
        let is_current = cmd.ends_with("gridterm-browser-shim");
        let is_old_ours = entry
            .get("args")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().any(|v| v == "@playwright/mcp@latest"))
            .unwrap_or(false);
        if is_current {
            // Already current; ensure broker is up and approval is in place.
            spawn_broker_bringup();
            let _ = ensure_user_autoapprove();
            return Ok(false);
        }
        if !is_old_ours {
            // A user's custom playwright entry: leave it alone.
            return Ok(false);
        }
        // Stale gridterm entry -> upgrade below.
    }
    set_mode(BrowserMode::Shared)?;
    Ok(true)
}

// ---- broker lifecycle -------------------------------------------------------

/// True if the broker socket is accepting connections.
fn broker_up() -> bool {
    UnixStream::connect(broker_sock()).is_ok()
}

/// Kick off broker bring-up on a background thread (so first-run `npm install`
/// never blocks the UI). Cheap to call repeatedly: it no-ops if already up.
pub fn spawn_broker_bringup() {
    if broker_up() {
        return;
    }
    std::thread::spawn(|| {
        if let Err(e) = ensure_broker_running() {
            crate::crashlog::append("BROWSER_BROKER_ERR", &format!("{e}"));
        }
    });
}

/// Materialize the broker files and run `npm install` once (idempotent).
fn ensure_broker_installed() -> std::io::Result<()> {
    let dir = broker_dir();
    std::fs::create_dir_all(&dir)?;
    let js = dir.join("broker.mjs");
    let pkg = dir.join("package.json");
    // (Re)write if missing or changed, so upgrades take effect.
    if std::fs::read_to_string(&js).ok().as_deref() != Some(BROKER_JS) {
        std::fs::write(&js, BROKER_JS)?;
    }
    if std::fs::read_to_string(&pkg).ok().as_deref() != Some(BROKER_PKG) {
        std::fs::write(&pkg, BROKER_PKG)?;
    }
    let installed = dir.join("node_modules/@playwright/mcp");
    if !installed.exists() {
        // Blocking, but only happens once (first ever browser use). The caller
        // runs this off the UI thread.
        let status = std::process::Command::new("npm")
            .arg("install")
            .arg("--no-audit")
            .arg("--no-fund")
            .current_dir(&dir)
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                return Err(std::io::Error::other(format!("npm install failed ({s})")))
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Ensure the broker process is running (lazy). Installs deps on first use,
/// spawns the broker detached, and waits for its socket to come up.
pub fn ensure_broker_running() -> std::io::Result<()> {
    if broker_up() {
        return Ok(());
    }
    ensure_broker_installed()?;
    let dir = broker_dir();
    std::process::Command::new("node")
        .arg("broker.mjs")
        .current_dir(&dir)
        // Tell the broker which app to re-focus after Chrome launches, so the
        // browser never steals focus from the user. Allow an override via env.
        .env(
            "GRIDTERM_FOCUS_APP",
            std::env::var("GRIDTERM_FOCUS_APP").unwrap_or_else(|_| "gridterm".into()),
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if broker_up() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Err(std::io::Error::other("broker did not come up in time"))
}

// ---- control channel (talk to the broker) -----------------------------------

/// Send one control command to the broker and read its JSON reply.
fn control(cmd: Value) -> Result<Value, String> {
    let mut sock = UnixStream::connect(broker_sock()).map_err(|e| format!("broker offline: {e}"))?;
    sock.set_read_timeout(Some(Duration::from_secs(35))).ok();
    let line = format!("CTL {}\n", cmd);
    sock.write_all(line.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    sock.flush().ok();
    let mut buf = String::new();
    sock.read_to_string(&mut buf).map_err(|e| format!("read: {e}"))?;
    let first = buf.lines().next().unwrap_or("").trim();
    serde_json::from_str(first).map_err(|e| format!("bad reply: {e}: {first}"))
}

// ---- user-scope config helpers (non-destructive merge) ----

pub fn remove_browser_mcp() -> std::io::Result<bool> {
    let path = user_claude_json();
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

/// Surface the one shared Chrome for the user to log in once. Brings the window
/// to the front (optionally at `url`). After the user logs in, every panel
/// inherits the session automatically (same shared profile).
pub fn surface_for_login(url: Option<&str>) -> String {
    // The broker is normally already up (started at launch). If not, kick off
    // bring-up in the background and wait briefly for the socket rather than
    // blocking the UI thread on a possible first-run install.
    if !broker_up() {
        spawn_broker_bringup();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !broker_up() {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if !broker_up() {
        return "The browser is still starting up (first run installs a small helper). \
Give it a few seconds and ask again.".into();
    }
    let mut cmd = json!({ "cmd": "surface_login" });
    if let Some(u) = url {
        cmd["url"] = json!(u);
    }
    match control(cmd) {
        Ok(v) if v.get("ok").and_then(|b| b.as_bool()) == Some(true) => {
            let target = url.unwrap_or("the site you need");
            format!(
                "Opened a browser window at {target}. Please sign in there. Because all panels \
share this one browser, you only need to log in once; every agent (and future runs) will \
stay signed in, just like your normal Chrome. Tell me when you're done."
            )
        }
        Ok(v) => format!("couldn't surface the login window: {v}"),
        Err(e) => format!("couldn't surface the login window: {e}"),
    }
}

/// Confirm the shared session is live and report cookie count. With the shared
/// persistent profile, logins already persist automatically; this just
/// reassures the user (and optionally hides the window again).
pub fn save_login_state() -> String {
    let count = match control(json!({ "cmd": "cookie_count" })) {
        Ok(v) => v.get("count").and_then(|c| c.as_u64()).unwrap_or(0),
        Err(e) => return format!("couldn't read the session: {e}"),
    };
    // Push the window back out of the way now that login is done.
    let _ = control(json!({ "cmd": "hide" }));
    format!(
        "You're signed in ({count} cookies in the shared session). Every panel agent shares this \
one persistent Chrome, so your logins STAY signed in across runs, just like your normal \
browser. The agents can use those sites now."
    )
}

/// Navigate the visible identity browser and return what the page says. This is
/// the main (cmd+J) agent's real browsing tool: it actually opens the URL and
/// reads the page text back, so the agent can answer "am I logged in?", check a
/// site, etc. If a login is needed, the window is already visible for the user.
pub fn browser_open(url: &str) -> String {
    if !broker_up() {
        spawn_broker_bringup();
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline && !broker_up() {
            std::thread::sleep(Duration::from_millis(150));
        }
    }
    if !broker_up() {
        return "The browser is still starting (first run installs a small helper); try again in a few seconds.".into();
    }
    match control(json!({ "cmd": "nav", "url": url })) {
        Ok(v) if v.get("ok").and_then(|b| b.as_bool()) == Some(true) => {
            let title = v.get("title").and_then(|t| t.as_str()).unwrap_or("");
            let final_url = v.get("url").and_then(|u| u.as_str()).unwrap_or(url);
            let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
            format!("Opened {final_url}\nTitle: {title}\n\nPage text:\n{text}")
        }
        Ok(v) => format!("couldn't open {url}: {}", v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown error")),
        Err(e) => format!("couldn't open {url}: {e}"),
    }
}

/// Re-read the current identity-browser page (no navigation).
pub fn browser_read() -> String {
    if !broker_up() {
        return "No browser is open yet. Open a URL first.".into();
    }
    match control(json!({ "cmd": "read" })) {
        Ok(v) if v.get("ok").and_then(|b| b.as_bool()) == Some(true) => {
            let title = v.get("title").and_then(|t| t.as_str()).unwrap_or("");
            let final_url = v.get("url").and_then(|u| u.as_str()).unwrap_or("");
            let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
            format!("{final_url}\nTitle: {title}\n\nPage text:\n{text}")
        }
        Ok(v) => format!("couldn't read the page: {}", v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown error")),
        Err(e) => format!("couldn't read the page: {e}"),
    }
}

/// Report broker/browser status (for diagnostics / the main agent).
#[allow(dead_code)]
pub fn status_line() -> String {
    match control(json!({ "cmd": "status" })) {
        Ok(v) => format!(
            "browser: identity={} worker={} pages={} clients={}",
            v.get("identity").and_then(|b| b.as_bool()).unwrap_or(false),
            v.get("worker").and_then(|b| b.as_bool()).unwrap_or(false),
            v.get("pages").and_then(|n| n.as_u64()).unwrap_or(0),
            v.get("clients").and_then(|n| n.as_u64()).unwrap_or(0),
        ),
        Err(_) => "browser broker: not running".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Tests here mutate process-global env (HOME). They share one process-wide
    // lock with other env-reading tests (crashlog) so nothing races.
    use crate::test_env_lock;

    fn tmp_home(tag: &str) -> PathBuf {
        let h = std::env::temp_dir().join(format!("gt_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        std::fs::create_dir_all(&h).unwrap();
        std::env::set_var("HOME", &h);
        h
    }

    // RAII guard that restores HOME when dropped, so we never leak a temp HOME
    // into other tests (e.g. crashlog) that also read HOME.
    struct HomeGuard(Option<std::ffi::OsString>);
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }
    fn save_home() -> HomeGuard {
        HomeGuard(std::env::var_os("HOME"))
    }

    #[test]
    fn user_mcp_merge_without_clobbering() {
        let _g = test_env_lock().lock().unwrap();
        let _h = save_home();
        let home = tmp_home("mcp2");
        std::fs::write(
            home.join(".claude.json"),
            r#"{"theme":"dark","mcpServers":{"mine":{"command":"x","args":[]}}}"#,
        )
        .unwrap();

        write_user_mcp_server(shim_server_entry()).unwrap();
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(home.join(".claude.json")).unwrap(),
        )
        .unwrap();
        // Preserve unrelated keys and other servers.
        assert_eq!(v["theme"], "dark");
        assert!(v["mcpServers"]["mine"].is_object());
        assert!(v["mcpServers"]["playwright"]["command"]
            .as_str()
            .unwrap()
            .ends_with("gridterm-browser-shim"));

        assert!(remove_browser_mcp().unwrap());
        let v2: Value = serde_json::from_str(
            &std::fs::read_to_string(home.join(".claude.json")).unwrap(),
        )
        .unwrap();
        assert!(v2["mcpServers"]["mine"].is_object());
        assert!(v2["mcpServers"].get("playwright").is_none());
    }

    #[test]
    fn upgrades_stale_npx_entry() {
        let _g = test_env_lock().lock().unwrap();
        let _h = save_home();
        let home = tmp_home("mcp3");
        std::fs::write(
            home.join(".claude.json"),
            r#"{"mcpServers":{"playwright":{"command":"npx","args":["-y","@playwright/mcp@latest","--cdp-endpoint","http://127.0.0.1:9222"]}}}"#,
        )
        .unwrap();
        // Simulate the upgrade write path without launching the broker.
        write_user_mcp_server(shim_server_entry()).unwrap();
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(home.join(".claude.json")).unwrap(),
        )
        .unwrap();
        assert!(v["mcpServers"]["playwright"]["command"]
            .as_str()
            .unwrap()
            .ends_with("gridterm-browser-shim"));
    }

    #[test]
    fn user_autoapprove_is_nondestructive() {
        let _g = test_env_lock().lock().unwrap();
        let _h = save_home();
        let home = tmp_home("claude");
        let claude = home.join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        // Pre-existing user settings we must preserve (e.g. env, other perms).
        std::fs::write(
            claude.join("settings.json"),
            r#"{"env":{"FOO":"bar"},"permissions":{"allow":["WebFetch(domain:example.com)"]}}"#,
        )
        .unwrap();

        ensure_user_autoapprove().unwrap();

        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(claude.join("settings.json")).unwrap(),
        )
        .unwrap();
        // Preserved the user's env + existing permission.
        assert_eq!(v["env"]["FOO"], "bar");
        assert_eq!(v["permissions"]["allow"][0], "WebFetch(domain:example.com)");
        // Our server is auto-allowed so the agent never prompts per-tool.
        assert!(v["permissions"]["allow"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "mcp__playwright"));
    }
}
