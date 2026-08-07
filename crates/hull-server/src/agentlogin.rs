//! Web-relayed **agent subscription login** — connect a user's OWN Claude/ChatGPT subscription to a
//! *remote* Hull from the browser, no CLI on their machine. Two shapes, both driven under a PTY:
//!
//!   - **Claude** (`setup-token`, [`Mode::PasteCode`]): prints an authorize URL (Anthropic-hosted
//!     redirect, no localhost callback), the user approves and pastes a code back, and the CLI then
//!     **prints a long-lived OAuth token** (`sk-ant-oat…`) — which we capture and store. At run time
//!     the token is passed as `CLAUDE_CODE_OAUTH_TOKEN`. (This is Anthropic's sanctioned headless-token
//!     feature, used to run Claude Code itself — not third-party token reuse.)
//!   - **Codex** (`login --device-auth`, [`Mode::DevicePoll`]): prints a URL + a user code and
//!     self-polls until approved, writing `auth.json` into its `CODEX_HOME` bundle — nothing pasted.
//!
//! Hull only ever holds the token/credentials the CLI itself issues, sealed in the user's bundle.

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Max time to wait for the CLI to emit its authorize URL.
const URL_DEADLINE: Duration = Duration::from_secs(25);
/// Max time to wait for the CLI to exchange the pasted code (and print the token).
const EXCHANGE_DEADLINE: Duration = Duration::from_secs(60);
/// A parked login expires if never completed, so a bundle+child can't leak forever.
const PENDING_TTL: Duration = Duration::from_secs(900);

/// How the CLI's login completes.
#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    /// Claude `setup-token`: paste a code back, then the CLI prints a long-lived token we capture.
    PasteCode,
    /// Codex `login --device-auth`: enter the shown code on the site; the CLI self-polls and writes
    /// its own credential bundle — nothing is pasted or captured.
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
    /// Login complete. `token` is the captured `CLAUDE_CODE_OAUTH_TOKEN` (PasteCode); `None` for a
    /// device flow, which persisted its own bundle instead. `email`/`plan`/`ttl_days` are scraped from
    /// the CLI's success screen when it prints them (best-effort).
    Done { token: Option<String>, email: Option<String>, plan: Option<String>, ttl_days: Option<u64> },
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
    /// Everything the CLI has printed to its tty so far (URL, then — for Claude — the token).
    buf: Arc<Mutex<Vec<u8>>>,
    started: Instant,
}

fn registry() -> &'static Mutex<HashMap<String, Pending>> {
    static R: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Reap any parked logins older than the TTL (kills the child, so no orphaned CLI process).
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
    // A very wide terminal so long strings (notably the printed token) aren't wrapped across lines,
    // which would defeat scraping.
    let pair = pty.openpty(PtySize { rows: 60, cols: 1000, pixel_width: 0, pixel_height: 0 }).map_err(|e| format!("openpty: {e}"))?;

    let mut cmd = CommandBuilder::new(command);
    match mode {
        Mode::PasteCode => {
            cmd.arg("setup-token");
        }
        Mode::DevicePoll => {
            cmd.arg("login");
            cmd.arg("--device-auth");
        }
    }
    // Codex keys credentials off its bundle dir; Claude's setup-token doesn't use it (it prints a
    // token), but we still isolate its scratch state there.
    cmd.env(config_env(command), dir.to_string_lossy().to_string());
    cmd.env("BROWSER", "true"); // never try to launch a browser on a headless server
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    }
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }

    let child = pair.slave.spawn_command(cmd).map_err(|e| format!("spawn {command} login: {e} (installed?)"))?;
    drop(pair.slave);
    let reader = pair.master.try_clone_reader().map_err(|e| format!("pty reader: {e}"))?;
    let writer = pair.master.take_writer().map_err(|e| format!("pty writer: {e}"))?;

    // Drain the tty into a shared buffer on a thread (a blocking read would wedge, since the child
    // stays alive waiting for the code / polling). `begin` reads the URL from it; `finish` the token.
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    {
        let buf = buf.clone();
        std::thread::spawn(move || {
            let mut r = reader;
            let mut chunk = [0u8; 4096];
            loop {
                match r.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.lock().unwrap().extend_from_slice(&chunk[..n]),
                }
            }
        });
    }

    let deadline = Instant::now() + URL_DEADLINE;
    let (url, user_code) = loop {
        {
            let g = buf.lock().unwrap();
            let url = scrape_url(&g);
            let code = if mode == Mode::DevicePoll { scrape_device_code(&g) } else { None };
            if let Some(u) = url {
                if mode != Mode::DevicePoll || code.is_some() {
                    break (u, code);
                }
            }
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for the sign-in details from the agent CLI".into());
        }
        std::thread::sleep(Duration::from_millis(150));
    };

    registry().lock().unwrap().insert(
        session.to_string(),
        Pending { command: command.to_string(), mode, master: pair.master, writer, child, buf, started: Instant::now() },
    );
    Ok(Begun { url, user_code, mode })
}

/// Advance a login. **PasteCode**: feed the pasted `code`, then capture the long-lived token the CLI
/// prints. **DevicePoll**: `code` is ignored — check whether the self-polling CLI has completed yet
/// (returns [`Finish::Pending`] if the user hasn't approved).
pub fn finish(session: &str, code: &str) -> Result<Finish, String> {
    let mut pending = registry().lock().unwrap().remove(session).ok_or("no pending login for this session (it may have expired)")?;
    match pending.mode {
        Mode::PasteCode => {
            // Fill the field, then submit Enter as a SEPARATE keypress after a beat. The TUI (Ink)
            // batches a paste immediately followed by Enter and drops the submit, so the code sits in
            // the field unexchanged — which is exactly what we saw.
            pending.writer.write_all(code.trim().as_bytes()).map_err(|e| format!("write code to tty: {e}"))?;
            pending.writer.flush().ok();
            std::thread::sleep(Duration::from_millis(500));
            pending.writer.write_all(b"\r").map_err(|e| format!("submit code: {e}"))?;
            pending.writer.flush().ok();
            eprintln!("hull agentlogin: submitted code to {} setup-token (session {session})", pending.command);
            let deadline = Instant::now() + EXCHANGE_DEADLINE;
            let mut resubmit_at = Instant::now() + Duration::from_secs(4);
            loop {
                // If nothing has happened after a few seconds, nudge Enter once more (some builds want
                // a second submit after the field re-renders).
                if Instant::now() >= resubmit_at {
                    let _ = pending.writer.write_all(b"\r");
                    let _ = pending.writer.flush();
                    resubmit_at = Instant::now() + Duration::from_secs(3600);
                }
                // The token appears in the tty output once the code is exchanged.
                if scrape_token(&pending.buf.lock().unwrap()).is_some() {
                    // Let the success screen finish rendering (it may print the account/plan around the
                    // token), then capture token + identity together.
                    std::thread::sleep(Duration::from_millis(1500));
                    let buf = pending.buf.lock().unwrap().clone();
                    let _ = pending.child.kill();
                    drop(pending.master);
                    let tok = scrape_token(&buf);
                    let (email, plan) = scrape_account(&buf);
                    let ttl_days = scrape_validity_days(&buf);
                    dump_debug(session, &buf); // so we can inspect what the success screen offered
                    eprintln!("hull agentlogin: captured token (session {session}); email={email:?} plan={plan:?} ttl_days={ttl_days:?}");
                    return Ok(Finish::Done { token: tok, email, plan, ttl_days });
                }
                match pending.child.try_wait() {
                    Ok(Some(status)) => {
                        let buf = pending.buf.lock().unwrap().clone();
                        drop(pending.master);
                        // Exited without a token ⇒ the code was rejected (or an error was printed).
                        return match scrape_token(&buf) {
                            Some(t) => { let (email, plan) = scrape_account(&buf); Ok(Finish::Done { token: Some(t), email, plan, ttl_days: scrape_validity_days(&buf) }) }
                            None => Err(format!("{} did not return a token — check the code (exit {:?})", pending.command, status)),
                        };
                    }
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            dump_debug(session, &pending.buf.lock().unwrap());
                            let _ = pending.child.kill();
                            return Err("timed out exchanging the code".into());
                        }
                        std::thread::sleep(Duration::from_millis(250));
                    }
                    Err(e) => return Err(format!("wait: {e}")),
                }
            }
        }
        Mode::DevicePoll => match pending.child.try_wait() {
            Ok(Some(status)) => {
                drop(pending.master);
                if status.success() {
                    Ok(Finish::Done { token: None, email: None, plan: None, ttl_days: None })
                } else {
                    Err(format!("{} device login failed (exit {:?})", pending.command, status))
                }
            }
            Ok(None) => {
                registry().lock().unwrap().insert(session.to_string(), pending);
                Ok(Finish::Pending)
            }
            Err(e) => Err(format!("wait: {e}")),
        },
    }
}

/// Diagnostic: on a failed paste-code exchange, dump the (ANSI-stripped, token-redacted) terminal
/// output so we can see what `setup-token` actually printed. Written to `<data-root>/agentlogin-last.txt`.
fn dump_debug(session: &str, buf: &[u8]) {
    let text = strip_ansi(&String::from_utf8_lossy(buf));
    // Redact anything token-shaped so the dump is safe to read: keep `sk-ant-` then mask the rest.
    let mut redacted = text.clone();
    while let Some(pos) = redacted.find("sk-ant-") {
        let tail: usize = redacted[pos + 7..].bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_').count();
        redacted.replace_range(pos..pos + 7 + tail, "[TOKEN-REDACTED]");
    }
    // Also list the staging dir — setup-token might persist the token to a file rather than print it.
    let dir = crate::agentsession::dir_for(session);
    let mut listing = String::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let sz = e.metadata().map(|m| m.len()).unwrap_or(0);
            listing.push_str(&format!("  {} ({sz} bytes)\n", e.file_name().to_string_lossy()));
        }
    }
    let path = crate::agentsession::sessions_root().parent().map(|p| p.join("agentlogin-last.txt")).unwrap_or_else(|| std::path::PathBuf::from("agentlogin-last.txt"));
    let _ = std::fs::write(&path, format!("session {session}\n--- staging dir {} ---\n{listing}\n--- tty output (redacted) ---\n{redacted}\n", dir.display()));
    let tail: String = redacted.chars().rev().take(400).collect::<String>().chars().rev().collect();
    eprintln!("hull agentlogin: setup-token output tail (redacted): …{tail}");
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

/// Pull the OAuth authorize / device URL out of a raw terminal byte stream. The URL is emitted as an
/// OSC-8 hyperlink target (one contiguous run), so we take the first `https://…` up to the next
/// control byte and sanity-check it looks like an auth URL.
fn scrape_url(buf: &[u8]) -> Option<String> {
    let hay = String::from_utf8_lossy(buf);
    let mut search_from = 0;
    while let Some(rel) = hay[search_from..].find("https://") {
        let start = search_from + rel;
        let end = hay[start..].find(|c: char| (c as u32) < 0x20 || c == '"' || c == '\'').map(|o| start + o).unwrap_or(hay.len());
        let candidate = &hay[start..end];
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

/// Best-effort scrape of an account email and plan tier from a CLI success screen. Returns
/// `(email, plan)`; either may be `None` if the screen doesn't print it.
fn scrape_account(buf: &[u8]) -> (Option<String>, Option<String>) {
    let hay = strip_ansi(&String::from_utf8_lossy(buf));
    // Email: a `local@domain.tld` run.
    let email = hay.split(|c: char| c.is_whitespace() || "()<>\"',".contains(c)).find(|w| {
        let at = w.find('@');
        matches!(at, Some(i) if i > 0 && w[i + 1..].contains('.') && !w.ends_with('.'))
    }).map(str::to_string);
    // Plan tier: look for a known Claude plan word (case-insensitive), preferring one near "plan".
    let low = hay.to_lowercase();
    let plan = ["max", "pro", "team", "enterprise", "free"].iter().find(|p| {
        low.contains(&format!("{p} plan")) || low.contains(&format!("claude {p}")) || low.contains(&format!("{p} subscription"))
    }).map(|p| p.to_string());
    (email, plan)
}

/// Scrape a token validity window from the success screen (e.g. "valid for 1 year") → days.
fn scrape_validity_days(buf: &[u8]) -> Option<u64> {
    let low = strip_ansi(&String::from_utf8_lossy(buf)).to_lowercase();
    let idx = low.find("valid for")?;
    let after = &low[idx + 9..];
    // First number after "valid for", then the unit word.
    let num: u64 = after.split(|c: char| !c.is_ascii_digit()).find(|s| !s.is_empty())?.parse().ok()?;
    let unit_span: String = after.chars().skip_while(|c| !c.is_ascii_alphabetic()).take(8).collect();
    let per = if unit_span.starts_with("year") { 365 } else if unit_span.starts_with("month") { 30 } else if unit_span.starts_with("week") { 7 } else if unit_span.starts_with("day") { 1 } else { return None };
    Some(num * per)
}

/// Capture a long-lived Claude token (`sk-ant-oat…`) from the (wide, so unwrapped) terminal stream.
fn scrape_token(buf: &[u8]) -> Option<String> {
    let hay = strip_ansi(&String::from_utf8_lossy(buf));
    let start = hay.find("sk-ant-")?;
    let tok: String = hay[start..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    // A real token is long and includes the `oat` (OAuth) marker; guard against a short false hit.
    if tok.len() >= 40 && tok.contains("oat") {
        Some(tok)
    } else {
        None
    }
}

/// Pull the device **user code** from a device-auth terminal stream: an `XXXX-XXXX`-style token.
fn scrape_device_code(buf: &[u8]) -> Option<String> {
    let hay = strip_ansi(&String::from_utf8_lossy(buf));
    let bytes = hay.as_bytes();
    let is_grp = |c: u8| c.is_ascii_uppercase() || c.is_ascii_digit();
    let mut i = 0;
    while i < bytes.len() {
        let a0 = i;
        while i < bytes.len() && is_grp(bytes[i]) {
            i += 1;
        }
        let a_len = i - a0;
        if (3..=8).contains(&a_len) && i < bytes.len() && bytes[i] == b'-' {
            let dash = i;
            i += 1;
            let b0 = i;
            while i < bytes.len() && is_grp(bytes[i]) {
                i += 1;
            }
            let b_len = i - b0;
            if (3..=8).contains(&b_len) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrape_url_finds_claude_authorize_url() {
        // Emitted as an OSC-8 hyperlink target (BEL-terminated) — scrape_url reads the raw stream and
        // takes the https run up to the next control byte.
        let buf = b"\x1b]8;;https://claude.ai/oauth/authorize?code=abc&code_challenge=xyz\x07click here\x1b]8;;\x07";
        assert_eq!(
            scrape_url(buf).as_deref(),
            Some("https://claude.ai/oauth/authorize?code=abc&code_challenge=xyz")
        );
    }

    #[test]
    fn scrape_url_finds_codex_device_url() {
        // A scraped URL runs until the next control byte/quote (a wide-terminal OSC target has no
        // embedded spaces), so terminate with a newline like the real stream.
        let buf = b"Open https://auth.openai.com/oauth/device?user_code=WDJB-MJHT\nin your browser\n";
        assert_eq!(scrape_url(buf).as_deref(), Some("https://auth.openai.com/oauth/device?user_code=WDJB-MJHT"));
        // A bare `/device` path (no openai host) also qualifies.
        let buf2 = b"visit https://example.test/device/activate\n";
        assert_eq!(scrape_url(buf2).as_deref(), Some("https://example.test/device/activate"));
    }

    #[test]
    fn scrape_url_rejects_non_auth_urls_and_skips_junk_to_the_real_one() {
        // A plain link with none of the auth markers is not returned.
        assert_eq!(scrape_url(b"see https://example.com/docs\n"), None);
        assert_eq!(scrape_url(b"no url here at all"), None);
        // The first https is junk (no marker); the scraper keeps scanning and returns the real one.
        let buf = b"https://example.com\nhttps://claude.ai/oauth/authorize?code_challenge=z\n";
        assert_eq!(scrape_url(buf).as_deref(), Some("https://claude.ai/oauth/authorize?code_challenge=z"));
    }

    #[test]
    fn scrape_device_code_extracts_grouped_code() {
        assert_eq!(scrape_device_code(b"Enter code WDJB-MJHT to continue").as_deref(), Some("WDJB-MJHT"));
        // Digits are allowed in a group.
        assert_eq!(scrape_device_code(b"code: A1B2-9Z0Q done").as_deref(), Some("A1B2-9Z0Q"));
    }

    #[test]
    fn scrape_device_code_ignores_lowercase_words_and_bare_prose() {
        // A lowercase hyphenated word ("sign-in") is not an uppercase/digit device code.
        assert_eq!(scrape_device_code(b"please sign-in on the web page"), None);
        assert_eq!(scrape_device_code(b"nothing code-like present"), None);
    }

    #[test]
    fn scrape_token_captures_wide_unwrapped_oauth_token() {
        // A real `sk-ant-oat…` token: long, contains the `oat` OAuth marker, printed unwrapped on a
        // wide terminal. It's captured whole and stops at the trailing whitespace.
        let tok = "sk-ant-oat01-AbCdEf0123456789_AbCdEf0123456789-AbCdEf0123456789";
        let buf = format!("Your token:\n{tok}\nKeep it safe");
        assert_eq!(scrape_token(buf.as_bytes()).as_deref(), Some(tok));
    }

    #[test]
    fn scrape_token_rejects_short_or_non_oat_shaped_hits() {
        // Right prefix but no `oat` marker and too short → not a token.
        assert_eq!(scrape_token(b"sk-ant-api03-short"), None);
        // Contains `oat` but is far too short to be a real token.
        assert_eq!(scrape_token(b"sk-ant-oat01-x"), None);
        assert_eq!(scrape_token(b"no token here"), None);
    }

    #[test]
    fn scrape_validity_days_converts_units() {
        assert_eq!(scrape_validity_days(b"This token is valid for 1 year."), Some(365));
        assert_eq!(scrape_validity_days(b"valid for 6 months"), Some(180));
        assert_eq!(scrape_validity_days(b"valid for 2 weeks or so"), Some(14));
        assert_eq!(scrape_validity_days(b"valid for 30 days"), Some(30));
        // No validity phrase, or an unrecognized unit → None.
        assert_eq!(scrape_validity_days(b"token issued successfully"), None);
        assert_eq!(scrape_validity_days(b"valid for 5 fortnights"), None);
    }

    #[test]
    fn strip_ansi_removes_csi_and_osc_sequences() {
        // CSI color codes are removed, plain text kept.
        assert_eq!(strip_ansi("\x1b[1;32mHELLO\x1b[0m world"), "HELLO world");
        // OSC-8 hyperlink wrappers (BEL-terminated) are removed, the visible label kept.
        assert_eq!(strip_ansi("a\x1b]8;;http://x\x07shown\x1b]8;;\x07b"), "ashownb");
        // No escapes → identity.
        assert_eq!(strip_ansi("plain text"), "plain text");
    }
}
