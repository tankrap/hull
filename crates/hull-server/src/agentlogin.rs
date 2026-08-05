//! Web-relayed **agent subscription login** — drive `claude setup-token` (Codex analog) from the
//! browser so a user connects their OWN Claude/ChatGPT subscription to a *remote* Hull without a CLI.
//!
//! The CLI's login is a copy-paste-code OAuth flow: it prints an authorize URL whose `redirect_uri` is
//! Anthropic-hosted (`platform.claude.com/oauth/code/callback`), the user approves in their browser
//! and copies back a short code. There is no `localhost` callback, so it works across a network. But
//! the CLI only emits the URL / reads the code over a **terminal**, so we run it under a pseudo-tty:
//!
//!   1. [`begin`] provisions the user's bundle dir, spawns `<cli> setup-token` on a PTY pointed at it,
//!      scrapes the authorize URL from the terminal stream, and parks the live child in a registry.
//!   2. The browser opens that URL; the user approves and copies the code back into Hull.
//!   3. [`finish`] writes the code to the parked child's tty, waits for it to persist credentials into
//!      the bundle dir and exit, then the caller verifies with `<cli> auth status --json`.
//!
//! Hull never sees or stores the token — only the CLI's own credential files, in the user's bundle.

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Max time to wait for the CLI to emit its authorize URL.
const URL_DEADLINE: Duration = Duration::from_secs(25);
/// Max time to wait for the CLI to exchange the pasted code and exit.
const EXCHANGE_DEADLINE: Duration = Duration::from_secs(60);
/// A parked login expires if never completed, so a bundle+child can't leak forever.
const PENDING_TTL: Duration = Duration::from_secs(900);

/// A login ceremony in flight between [`begin`] and [`finish`].
struct Pending {
    command: String,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    started: Instant,
}

fn registry() -> &'static Mutex<HashMap<String, Pending>> {
    static R: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Reap any parked logins older than the TTL (kills the child, so no orphaned `setup-token`).
fn sweep() {
    let mut map = registry().lock().unwrap();
    let stale: Vec<String> = map.iter().filter(|(_, p)| p.started.elapsed() > PENDING_TTL).map(|(k, _)| k.clone()).collect();
    for k in stale {
        if let Some(mut p) = map.remove(&k) {
            let _ = p.child.kill();
        }
    }
}

/// Start a login: spawn `<command> setup-token` on a PTY writing credentials into `dir`, scrape the
/// authorize URL, and park the live child under `session`. Returns the URL to open in the browser.
pub fn begin(command: &str, session: &str, dir: &Path) -> Result<String, String> {
    sweep();
    let pty = native_pty_system();
    let pair = pty.openpty(PtySize { rows: 40, cols: 140, pixel_width: 0, pixel_height: 0 }).map_err(|e| format!("openpty: {e}"))?;

    let mut cmd = CommandBuilder::new(command);
    cmd.arg("setup-token");
    // Point the CLI at THIS user's bundle so credentials land in isolation.
    cmd.env(config_env(command), dir.to_string_lossy().to_string());
    // Don't let it try to launch a browser on the (headless) server; it falls back to printing the URL.
    cmd.env("BROWSER", "true");
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    }
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }

    let child = pair.slave.spawn_command(cmd).map_err(|e| format!("spawn {command} setup-token: {e} (installed?)"))?;
    drop(pair.slave); // release the slave fd; the child holds its own
    let reader = pair.master.try_clone_reader().map_err(|e| format!("pty reader: {e}"))?;
    let writer = pair.master.take_writer().map_err(|e| format!("pty writer: {e}"))?;

    // Read the terminal stream on a thread (a blocking read would wedge, since the child stays alive
    // waiting for the code); collect chunks until the authorize URL appears or the deadline passes.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut r = reader;
        let mut buf = [0u8; 4096];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let deadline = Instant::now() + URL_DEADLINE;
    let mut acc: Vec<u8> = Vec::new();
    let url = loop {
        if let Some(u) = scrape_url(&acc) {
            break u;
        }
        let remaining = deadline.checked_duration_since(Instant::now());
        match remaining.and_then(|d| rx.recv_timeout(d).ok()) {
            Some(chunk) => acc.extend_from_slice(&chunk),
            None => return Err("timed out waiting for the sign-in URL from the agent CLI".into()),
        }
    };

    registry().lock().unwrap().insert(
        session.to_string(),
        Pending { command: command.to_string(), master: pair.master, writer, child, started: Instant::now() },
    );
    Ok(url)
}

/// Finish a login: feed the pasted `code` to the parked child's tty and wait for it to persist
/// credentials and exit. Returns Ok when the CLI exits 0 (credentials written into the bundle).
pub fn finish(session: &str, code: &str) -> Result<(), String> {
    let mut pending = registry().lock().unwrap().remove(session).ok_or("no pending login for this session (it may have expired)")?;
    let line = format!("{}\r", code.trim());
    pending.writer.write_all(line.as_bytes()).map_err(|e| format!("write code to tty: {e}"))?;
    pending.writer.flush().ok();

    let deadline = Instant::now() + EXCHANGE_DEADLINE;
    loop {
        match pending.child.try_wait() {
            Ok(Some(status)) => {
                // Keep the master alive until here so the tty isn't torn down mid-exchange.
                drop(pending.master);
                return if status.success() { Ok(()) } else { Err(format!("{} setup-token rejected the code (exit {:?})", pending.command, status)) };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = pending.child.kill();
                    return Err("timed out exchanging the code".into());
                }
                std::thread::sleep(Duration::from_millis(150));
            }
            Err(e) => return Err(format!("wait: {e}")),
        }
    }
}

/// Discard a parked login (user cancelled / error), killing the child.
pub fn abort(session: &str) {
    if let Some(mut p) = registry().lock().unwrap().remove(session) {
        let _ = p.child.kill();
    }
}

/// The config-home env var for a CLI: Codex keys off `CODEX_HOME`, Claude Code off `CLAUDE_CONFIG_DIR`.
fn config_env(command: &str) -> &'static str {
    if command == "codex" {
        "CODEX_HOME"
    } else {
        "CLAUDE_CONFIG_DIR"
    }
}

/// Pull the OAuth authorize URL out of a raw terminal byte stream. The CLI prints it both as an OSC-8
/// hyperlink target and as (escape-chunked) visible text; the hyperlink target is one contiguous run,
/// so we take the first `https://…` up to the next control byte and sanity-check it's an authorize URL.
fn scrape_url(buf: &[u8]) -> Option<String> {
    let hay = String::from_utf8_lossy(buf);
    let mut search_from = 0;
    while let Some(rel) = hay[search_from..].find("https://") {
        let start = search_from + rel;
        let end = hay[start..].find(|c: char| (c as u32) < 0x20 || c == '"' || c == '\'').map(|o| start + o).unwrap_or(hay.len());
        let candidate = &hay[start..end];
        if candidate.contains("oauth/authorize") || candidate.contains("oauth") && candidate.contains("code_challenge") {
            return Some(candidate.to_string());
        }
        search_from = end.max(start + 8);
    }
    None
}
