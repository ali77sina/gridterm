// gridterm-browser-shim
//
// A panel agent (Claude Code, Codex, etc.) is configured to launch THIS binary
// as its Playwright MCP server. Instead of starting a whole Node + Chromium per
// agent, the shim just connects to the single long-lived browser broker over a
// Unix domain socket and pipes the MCP stdio stream through it. The broker hands
// this connection its own browser tab inside the one shared (logged-in) Chrome.
//
// Cost: one tiny native process per panel (a few hundred KB resident, two parked
// I/O threads), versus ~40-250MB for a Node/Chromium per panel. This is the
// whole point of the broker architecture.
//
// If the broker socket is not up yet, the shim tries to start it (via the
// bootstrap command in GRIDTERM_BROKER_CMD, if provided) and waits briefly.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn sock_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".gridterm/browser-broker.sock")
}

fn try_connect(deadline: Instant) -> Option<UnixStream> {
    let path = sock_path();
    loop {
        if let Ok(s) = UnixStream::connect(&path) {
            return Some(s);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(120));
    }
}

fn main() {
    // Give the broker a moment to be up (gridterm starts it lazily). If a
    // bootstrap command is provided and the socket is missing, kick it off.
    if UnixStream::connect(sock_path()).is_err() {
        if let Ok(cmd) = std::env::var("GRIDTERM_BROKER_CMD") {
            // cmd is a shell line that starts the broker detached.
            let _ = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&cmd)
                .spawn();
        }
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut sock = match try_connect(deadline) {
        Some(s) => s,
        None => {
            eprintln!("gridterm-browser-shim: broker socket unavailable");
            std::process::exit(1);
        }
    };

    // Tell the broker this is a PANEL agent's MCP stream (drives the headless
    // worker browser, never a visible window).
    if sock.write_all(b"MCP PANEL\n").is_err() {
        std::process::exit(1);
    }
    let _ = sock.flush();

    // Pipe stdin -> socket on one thread, socket -> stdout on another. When
    // either side closes, we exit (the agent host treats that as the MCP server
    // going away, which is correct).
    let mut sock_in = sock.try_clone().expect("clone socket");

    let writer = std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 16 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if sock_in.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = sock_in.flush();
                }
                Err(_) => break,
            }
        }
        // Half-close so the broker sees EOF on its read side.
        let _ = sock_in.shutdown(std::net::Shutdown::Write);
    });

    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = stdout.flush();
            }
            Err(_) => break,
        }
    }
    let _ = writer.join();
}
