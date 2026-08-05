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

/// How the CLI's login completes.
#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    /// Claude `setup-token`: user copies a code off the approval page and pastes it back to the CLI.
    PasteCode,
    /// Codex `login --device-auth`: user enters a shown code on the site; the CLI **self-polls** the
    /// token endpoint until approved, then exits — nothing is pasted back.
    DevicePoll,
}

/// What [`begin`] surfaces to the browser.
pub struct Begun {
    pub url: String,
    /// The device user-code to enter on the site (DevicePoll only).
    pub user_code: Option<String>,
    pub mode: Mode,
}

/// Result of a [`finish`] attempt.
pub enum Finish {
    /// Credentials written to the bundle.
    Done,
    /// DevicePoll: the user hasn't approved yet — poll again.
    Pending,
}

/// A login ceremony in flight between [`begin`] and [`finish`].
struct Pending {
    command: String,
    mode: Mode,
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

/// Start a login: spawn the CLI's login on a PTY writing credentials into `dir`, scrape the sign-in
/// URL (and, for a device flow, the user code), and park the live child under `session`.
pub fn begin(command: &str, session: &str, dir: &Path) -> Result<Begun, String> {
    sweep();
    let mode = if command == "codex" { Mode::DevicePoll } else { Mode::PasteCode };
    let pty = native_pty_system();
    let pair = pty.openpty(PtySize { rows: 40, cols: 140, pixel_width: 0, pixel_height: 0 }).map_err(|e| format!("openpty: {e}"))?;

    let mut cmd = CommandBuilder::new(command);
    match mode {
        // Claude: mint a long-lived token, paste the code back.
        Mode::PasteCode => {
            cmd.arg("setup-token");
        }
        // Codex: device-authorization grant — prints URL + user code and self-polls.
        Mode::DevicePoll => {
            cmd.arg("login");
            cmd.arg("--device-auth");
        }
    }
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

    let child = pair.slave.spawn_command(cmd).map_err(|e| format!("spawn {command} login: {e} (installed?)"))?;
    drop(pair.slave); // release the slave fd; the child holds its own
    let reader = pair.master.try_clone_reader().map_err(|e| format!("pty reader: {e}"))?;
    let writer = pair.master.take_writer().map_err(|e| format!("pty writer: {e}"))?;

    // Read the terminal stream on a thread (a blocking read would wedge, since the child stays alive
    // waiting for the code / polling); collect chunks until the URL (and code) appear or we time out.
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
    let (url, user_code) = loop {
        let url = scrape_url(&acc);
        let code = if mode == Mode::DevicePoll { scrape_device_code(&acc) } else { None };
        // For a device flow we need BOTH the URL and the code; for paste-code, just the URL.
        if let Some(u) = url {
            if mode != Mode::DevicePoll || code.is_some() {
                break (u, code);
            }
        }
        let remaining = deadline.checked_duration_since(Instant::now());
        match remaining.and_then(|d| rx.recv_timeout(d).ok()) {
            Some(chunk) => acc.extend_from_slice(&chunk),
            None => return Err("timed out waiting for the sign-in details from the agent CLI".into()),
        }
    };

    registry().lock().unwrap().insert(
        session.to_string(),
        Pending { command: command.to_string(), mode, master: pair.master, writer, child, started: Instant::now() },
    );
    Ok(Begun { url, user_code, mode })
}

/// Advance a login. **PasteCode**: feed the pasted `code` to the CLI's tty and wait for it to persist
/// credentials and exit. **DevicePoll**: `code` is ignored — check whether the self-polling CLI has
/// completed yet (returns [`Finish::Pending`] if the user hasn't approved, so the caller polls again).
pub fn finish(session: &str, code: &str) -> Result<Finish, String> {
    let mut pending = registry().lock().unwrap().remove(session).ok_or("no pending login for this session (it may have expired)")?;
    match pending.mode {
        Mode::PasteCode => {
            let line = format!("{}\r", code.trim());
            pending.writer.write_all(line.as_bytes()).map_err(|e| format!("write code to tty: {e}"))?;
            pending.writer.flush().ok();
            let deadline = Instant::now() + EXCHANGE_DEADLINE;
            loop {
                match pending.child.try_wait() {
                    Ok(Some(status)) => {
                        drop(pending.master); // keep the tty alive until the exchange finishes
                        return if status.success() { Ok(Finish::Done) } else { Err(format!("{} rejected the code (exit {:?})", pending.command, status)) };
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
        Mode::DevicePoll => match pending.child.try_wait() {
            Ok(Some(status)) => {
                drop(pending.master);
                if status.success() {
                    Ok(Finish::Done)
                } else {
                    Err(format!("{} device login failed (exit {:?})", pending.command, status))
                }
            }
            // Not approved yet — park it again and tell the caller to poll.
            Ok(None) => {
                registry().lock().unwrap().insert(session.to_string(), pending);
                Ok(Finish::Pending)
            }
            Err(e) => Err(format!("wait: {e}")),
        },
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
        // Claude authorize URL, or a device-authorization verification URL (Codex → auth.openai.com/…/device).
        if candidate.contains("oauth/authorize")
            || (candidate.contains("oauth") && candidate.contains("code_challenge"))
            || candidate.contains("/device")
            || candidate.contains("auth.openai.com")
        {
            return Some(candidate.to_string());
        }
        search_from = end.max(start + 8);
    }
    None
}

/// Pull the device **user code** from a device-auth terminal stream. After stripping ANSI, find a
/// `XXXX-XXXX`-style token (upper-alphanumeric groups joined by a dash) — the format the CLI shows for
/// "enter this one-time code".
fn scrape_device_code(buf: &[u8]) -> Option<String> {
    let hay = strip_ansi(&String::from_utf8_lossy(buf));
    let bytes = hay.as_bytes();
    let is_grp = |c: u8| c.is_ascii_uppercase() || c.is_ascii_digit();
    let mut i = 0;
    while i < bytes.len() {
        // A run of code chars, a dash, another run.
        let a0 = i;
        while i < bytes.len() && is_grp(bytes[i]) {
            i += 1;
        }
        let a_len = i - a0;
        if a_len >= 3 && a_len <= 8 && i < bytes.len() && bytes[i] == b'-' {
            let dash = i;
            i += 1;
            let b0 = i;
            while i < bytes.len() && is_grp(bytes[i]) {
                i += 1;
            }
            let b_len = i - b0;
            if b_len >= 3 && b_len <= 8 {
                // Not part of a longer word (e.g. an uppercase URL fragment).
                let after_ok = i >= bytes.len() || !bytes[i].is_ascii_alphanumeric();
                if after_ok {
                    return Some(hay[a0..i].to_string());
                }
            }
            i = dash + 1;
        } else if i == a0 {
            i += 1;
        }
    }
    None
}

/// Remove ANSI/OSC escape sequences so scraping sees plain text.
fn strip_ansi(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b {
            // CSI (ESC [ … final) or OSC (ESC ] … BEL/ST) — skip to the terminator.
            i += 1;
            if i < b.len() && b[i] == b'[' {
                i += 1;
                while i < b.len() && !(0x40..=0x7e).contains(&b[i]) {
                    i += 1;
                }
                i += 1;
            } else if i < b.len() && b[i] == b']' {
                i += 1;
                while i < b.len() && b[i] != 0x07 && b[i] != 0x1b {
                    i += 1;
                }
                i += 1;
            }
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}
