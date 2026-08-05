//! Per-user **agent credential bundles** — the on-disk home for a user's own Claude Code / Codex
//! subscription login, populated by the web-relayed `setup-token` flow and pointed at by the CLI's
//! `CLAUDE_CONFIG_DIR` / `CODEX_HOME` at review time.
//!
//! Each connected user gets an isolated bundle directory (verified: a fresh config dir reports
//! `loggedIn: false`, so credentials are dir-scoped). The CLI reads, refreshes, and rewrites its own
//! credentials in place, so the directory IS the durable store — no token is parsed or reused by Hull.
//!
//! NOTE (Phase 1): bundles are stored as `0700` directories under the Hull data root. App-level
//! encryption-at-rest (decrypt-to-tmp per use, keyed by the server secret) is the immediate follow-up
//! (Phase 1b) that wraps this module; until then, protect the data root with volume encryption.

use std::path::PathBuf;

/// Root holding every user's bundle dir: `HULL_AGENT_SESSIONS` or `<HULL_DATA_DIR|~/.hull>/agent-sessions`.
pub fn sessions_root() -> PathBuf {
    if let Ok(p) = std::env::var("HULL_AGENT_SESSIONS") {
        return PathBuf::from(p);
    }
    let base = std::env::var("HULL_DATA_DIR").map(PathBuf::from).unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".hull").join("data")
    });
    // Sibling of the domain store dir, so all Hull state lives under one root.
    base.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from(".")).join("agent-sessions")
}

/// The bundle directory for a session id (not guaranteed to exist).
pub fn dir_for(session: &str) -> PathBuf {
    sessions_root().join(session)
}

/// Create a fresh, private (`0700`) bundle directory and return `(session_id, path)`.
pub fn provision() -> std::io::Result<(String, PathBuf)> {
    let session = uuid::Uuid::new_v4().simple().to_string();
    let dir = dir_for(&session);
    std::fs::create_dir_all(&dir)?;
    harden(&dir);
    Ok((session, dir))
}

/// Best-effort tighten to owner-only (`0700`). Credentials must never be group/world-readable.
pub fn harden(dir: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

/// Does this session have a populated bundle (i.e. a login landed)?
pub fn exists(session: &str) -> bool {
    let d = dir_for(session);
    d.is_dir() && std::fs::read_dir(&d).map(|mut it| it.next().is_some()).unwrap_or(false)
}

/// Delete a session's bundle (on connection removal). Best-effort.
pub fn remove(session: &str) {
    if session.is_empty() {
        return;
    }
    let _ = std::fs::remove_dir_all(dir_for(session));
}
