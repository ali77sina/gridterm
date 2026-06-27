use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Path to the crash/diagnostics log (next to the binary's working dir).
fn log_path() -> PathBuf {
    // Prefer a stable, user-visible location.
    if let Some(home) = std::env::var_os("HOME") {
        let dir = PathBuf::from(home).join(".gridterm");
        let _ = std::fs::create_dir_all(&dir);
        return dir.join("crash.log");
    }
    PathBuf::from("gridterm-crash.log")
}

fn now_str() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

/// Append a labeled block to the crash log (best-effort).
pub fn append(label: &str, body: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(
            f,
            "\n===== {label} @ {} =====\n{body}",
            now_str()
        );
    }
}

/// Install a panic hook that captures a full backtrace to the crash log (and
/// stderr) before the process aborts/continues. Call once at startup.
pub fn install_panic_hook() {
    // Ensure backtraces are captured even if the user didn't set the env var.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".into());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".into());
        let bt = std::backtrace::Backtrace::force_capture();
        let thread = std::thread::current();
        let tname = thread.name().unwrap_or("<unnamed>");

        let body = format!(
            "thread: {tname}\nlocation: {location}\nmessage: {msg}\n\nbacktrace:\n{bt}"
        );
        append("PANIC", &body);
        eprintln!("\n[gridterm panic] {msg} at {location}\n(full backtrace in {})", log_path().display());

        // Preserve default behavior (prints to stderr too).
        default(info);
    }));

    append("STARTUP", &format!("gridterm started (pid {})", std::process::id()));
}

/// Tracks whether the UI thread is currently *inside* event/render processing.
/// Freezes here come from blocking calls during handling, so we flag only when
/// processing has been active too long — never during healthy idle waits.
#[derive(Clone)]
pub struct Heartbeat {
    /// Monotonic ms timestamp when current processing began (0 = idle).
    processing_since_ms: Arc<AtomicU64>,
    /// Label of the operation currently running on the UI thread (breadcrumb),
    /// so a freeze report can say WHAT was running, not just that it stalled.
    current_op: Arc<Mutex<String>>,
    start: Instant,
}

impl Heartbeat {
    pub fn new() -> Self {
        Self {
            processing_since_ms: Arc::new(AtomicU64::new(0)),
            current_op: Arc::new(Mutex::new(String::from("idle"))),
            start: Instant::now(),
        }
    }

    /// Record what the UI thread is about to do (a breadcrumb). Cheap; called
    /// at the start of notable operations (event kind, tool name, render).
    pub fn mark(&self, op: impl Into<String>) {
        if let Ok(mut g) = self.current_op.lock() {
            *g = op.into();
        }
    }

    /// Mark the start of event/render processing on the UI thread.
    pub fn begin(&self) {
        let ms = (self.start.elapsed().as_millis() as u64).max(1);
        self.processing_since_ms.store(ms, Ordering::Relaxed);
    }

    /// Mark the end of processing (back to healthy idle).
    pub fn end(&self) {
        self.processing_since_ms.store(0, Ordering::Relaxed);
    }

    /// RAII guard: marks processing active for its lifetime, clears on drop —
    /// robust against the many early `return`s in event handlers. Owns a clone
    /// so it doesn't borrow the caller (Heartbeat is Arc-backed, cheap).
    pub fn guard(&self) -> HeartbeatGuard {
        self.begin();
        HeartbeatGuard { hb: self.clone() }
    }

    /// Spawn a watchdog that logs if processing stays active beyond `threshold`
    /// (a real freeze). Idle waits (processing == 0) never trigger it.
    pub fn spawn_watchdog(&self, threshold: Duration) {
        let proc_since = self.processing_since_ms.clone();
        let current_op = self.current_op.clone();
        let start = self.start;
        let thresh_ms = threshold.as_millis() as u64;
        std::thread::Builder::new()
            .name("gridterm-watchdog".into())
            .spawn(move || {
                let mut warned_at = 0u64;
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    let since = proc_since.load(Ordering::Relaxed);
                    if since == 0 {
                        continue; // idle, healthy
                    }
                    let now = start.elapsed().as_millis() as u64;
                    let stalled = now.saturating_sub(since);
                    if stalled >= thresh_ms && since != warned_at {
                        warned_at = since;
                        let op = current_op
                            .lock()
                            .map(|g| g.clone())
                            .unwrap_or_else(|_| "<lock poisoned>".into());
                        append(
                            "FREEZE",
                            &format!(
                                "UI thread blocked for {:.1}s during operation: [{op}]. \
A blocking call is running on the UI thread (long tool call, lock contention, or deadlock).",
                                stalled as f64 / 1000.0
                            ),
                        );
                        eprintln!(
                            "[gridterm] WARNING: UI blocked {:.1}s — logged to {}",
                            stalled as f64 / 1000.0,
                            log_path().display()
                        );
                    }
                }
            })
            .ok();
    }
}

/// Drops back to idle when it goes out of scope.
pub struct HeartbeatGuard {
    hb: Heartbeat,
}

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        self.hb.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_writes_to_log() {
        append("TEST", "hello from test");
        let contents = std::fs::read_to_string(log_path()).unwrap_or_default();
        assert!(contents.contains("TEST"), "log missing TEST block");
        assert!(contents.contains("hello from test"));
    }

    #[test]
    fn watchdog_flags_long_processing() {
        let hb = Heartbeat::new();
        hb.spawn_watchdog(Duration::from_millis(300));
        hb.begin();
        std::thread::sleep(Duration::from_millis(700));
        hb.end();
        let contents = std::fs::read_to_string(log_path()).unwrap_or_default();
        assert!(contents.contains("FREEZE"), "watchdog did not log FREEZE");
    }
}
