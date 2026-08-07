//! Per-user **agent credential bundles** — the on-disk home for a user's own Claude Code / Codex
//! subscription login, populated by the web-relayed `setup-token` flow and pointed at by the CLI's
//! `CLAUDE_CONFIG_DIR` / `CODEX_HOME` at review time.
//!
//! Each connected user gets an isolated bundle (verified: a fresh config dir reports
//! `loggedIn: false`, so credentials are dir-scoped). The CLI writes/refreshes its own credentials, so
//! the bundle IS the durable store — no token is parsed or reused by Hull.
//!
//! **At rest the bundle is encrypted** (Phase 1b): it lives as a single AEAD-sealed tar.gz
//! (`<session>.enc`, ChaCha20-Poly1305 under the server key). It is only ever plaintext transiently:
//!   - during **login**, in `dir_for(session)` while `setup-token` writes into it, then [`seal`]ed;
//!   - during a **run**, [`open`]ed into a throwaway dir that is wiped when the [`BundleGuard`] drops.
//!
//! A run is **not** read-only against the bundle: the CLI rotates its own access/refresh tokens as it
//! runs, and those rotations must survive to the next run or the credential goes stale. So on drop the
//! guard **re-seals** the (possibly mutated) dir back to `<session>.enc`, then wipes the plaintext.
//! A per-session lock (see [`lock_for`]) serializes runs against the same bundle, so two runs never
//! decrypt-mutate-reseal concurrently and clobber each other's rotated refresh token. If a re-seal
//! ever fails, the guard logs loudly and keeps the *prior* sealed bundle (a stale-but-valid credential
//! is better than none) while still wiping the plaintext, so unencrypted creds never linger on disk.

use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
use chacha20poly1305::{AeadCore, ChaCha20Poly1305, Key, Nonce};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

/// Root holding every user's bundle: `HULL_AGENT_SESSIONS` or `<HULL_DATA_DIR|~/.hull>/agent-sessions`.
pub fn sessions_root() -> PathBuf {
    if let Ok(p) = std::env::var("HULL_AGENT_SESSIONS") {
        return PathBuf::from(p);
    }
    let base = std::env::var("HULL_DATA_DIR").map(PathBuf::from).unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".hull").join("data")
    });
    base.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from(".")).join("agent-sessions")
}

/// Filename inside a Claude bundle holding the captured long-lived OAuth token (`CLAUDE_CODE_OAUTH_TOKEN`).
/// Codex bundles have no such file — they hold the CLI's own `auth.json` instead.
pub const OAUTH_TOKEN_FILE: &str = "hull-oauth-token";

/// The **plaintext** login-staging directory for a session (exists only between provision and seal).
pub fn dir_for(session: &str) -> PathBuf {
    sessions_root().join(session)
}

/// Read the stored Claude OAuth token from an open (decrypted) bundle dir, if present.
pub fn read_oauth_token(dir: &Path) -> Option<String> {
    let t = std::fs::read_to_string(dir.join(OAUTH_TOKEN_FILE)).ok()?;
    let t = t.trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// The **encrypted** at-rest bundle path.
fn enc_path(session: &str) -> PathBuf {
    sessions_root().join(format!("{session}.enc"))
}

/// Create a fresh, private (`0700`) staging directory and return `(session_id, path)` for a login to
/// write credentials into (sealed afterwards by [`seal`]).
pub fn provision() -> std::io::Result<(String, PathBuf)> {
    let session = uuid::Uuid::new_v4().simple().to_string();
    let dir = dir_for(&session);
    std::fs::create_dir_all(&dir)?;
    harden(&dir);
    Ok((session, dir))
}

/// Best-effort tighten to owner-only (`0700`). Credentials must never be group/world-readable.
pub fn harden(dir: &Path) {
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

/// A session has a populated (sealed) bundle iff its `.enc` exists.
pub fn exists(session: &str) -> bool {
    enc_path(session).is_file()
}

/// Delete a session's bundle — both the sealed `.enc` and any leftover plaintext staging dir.
pub fn remove(session: &str) {
    if session.is_empty() {
        return;
    }
    let _ = std::fs::remove_file(enc_path(session));
    let _ = std::fs::remove_dir_all(dir_for(session));
}

/// Seal an arbitrary plaintext dir into the encrypted at-rest bundle for `session` (atomic rename).
fn seal_dir(session: &str, src: &Path) -> Result<(), String> {
    let tar_gz = pack(src).map_err(|e| format!("pack bundle: {e}"))?;
    let sealed = encrypt(&tar_gz).map_err(|e| format!("encrypt bundle: {e}"))?;
    let path = enc_path(session);
    let tmp = path.with_extension("enc.tmp");
    std::fs::write(&tmp, &sealed).map_err(|e| format!("write bundle: {e}"))?;
    harden_file(&tmp);
    std::fs::rename(&tmp, &path).map_err(|e| format!("commit bundle: {e}"))?;
    Ok(())
}

/// Seal the plaintext *staging* dir into the encrypted at-rest bundle, then wipe it. Called once a
/// login has written credentials into `dir_for(session)`.
pub fn seal(session: &str) -> Result<(), String> {
    let dir = dir_for(session);
    if !dir.is_dir() {
        return Err("nothing to seal (no staging dir)".into());
    }
    seal_dir(session, &dir)?;
    let _ = std::fs::remove_dir_all(&dir); // drop the plaintext staging copy
    Ok(())
}

/// Per-session lock: serialize runs against the same bundle so two never refresh from — and rotate —
/// the same refresh token concurrently (a double-spend that would invalidate the credential).
fn lock_for(session: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    map.lock().unwrap().entry(session.to_string()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
}

/// Decrypt a sealed bundle into a throwaway private directory for use as the CLI's config home,
/// holding the per-session lock for the duration. The returned guard, on drop, **re-seals** the dir
/// (persisting any credential refresh the run wrote — access/refresh tokens rotate) and then wipes it,
/// so plaintext credentials never outlive the run and a rotated refresh token is never lost.
pub async fn open(session: &str) -> Result<BundleGuard, String> {
    let lock = lock_for(session).lock_owned().await;
    let sealed = std::fs::read(enc_path(session)).map_err(|e| format!("read bundle: {e}"))?;
    let tar_gz = decrypt(&sealed).map_err(|e| format!("decrypt bundle: {e}"))?;
    let dir = sessions_root().join(".open").join(uuid::Uuid::new_v4().simple().to_string());
    std::fs::create_dir_all(&dir).map_err(|e| format!("open dir: {e}"))?;
    harden(&dir);
    unpack(&tar_gz, &dir).map_err(|e| format!("unpack bundle: {e}"))?;
    Ok(BundleGuard { session: session.to_string(), dir, _lock: lock })
}

/// A live, decrypted bundle directory; re-sealed (to persist token refresh) then wiped on drop, with
/// the per-session lock held throughout.
pub struct BundleGuard {
    session: String,
    dir: PathBuf,
    _lock: tokio::sync::OwnedMutexGuard<()>,
}

impl BundleGuard {
    pub fn dir_string(&self) -> String {
        self.dir.to_string_lossy().into_owned()
    }
}

impl Drop for BundleGuard {
    fn drop(&mut self) {
        // Persist whatever the run left (refreshed access token / rotated refresh token) — unless the
        // dir was somehow emptied, in which case keep the last good sealed bundle rather than clobber.
        let populated = std::fs::read_dir(&self.dir).map(|mut it| it.next().is_some()).unwrap_or(false);
        if populated {
            // Re-seal atomically (seal_dir writes to a temp file then renames), so a failure leaves the
            // PRIOR sealed bundle intact — never a half-written one. On failure we can only log loudly:
            // the run's token rotation is lost and the next run decrypts the stale-but-valid bundle. We
            // still wipe the plaintext below regardless, so a failed re-seal never leaves unencrypted
            // credentials on disk (exposing them would be worse than losing a refresh).
            if let Err(e) = seal_dir(&self.session, &self.dir) {
                eprintln!("hull: could not re-seal agent bundle {} (keeping prior sealed bundle; token refresh lost): {e}", self.session);
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ── crypto + archive helpers ─────────────────────────────────────────────────────────

/// The 32-byte AEAD key. From `HULL_SESSION_KEY` (64 hex chars) if set, else a per-install key file
/// (`<root>/.bundle-key`, `0600`) generated once so restarts can still decrypt. For production, set
/// `HULL_SESSION_KEY` from a secret manager rather than relying on the on-disk key.
fn key() -> &'static Key {
    static KEY: OnceLock<Key> = OnceLock::new();
    KEY.get_or_init(|| {
        if let Ok(hex) = std::env::var("HULL_SESSION_KEY") {
            if let Ok(bytes) = hex_decode(hex.trim()) {
                if bytes.len() == 32 {
                    return *Key::from_slice(&bytes);
                }
            }
            eprintln!("hull: HULL_SESSION_KEY must be 64 hex chars (32 bytes) — falling back to the key file");
        }
        let path = sessions_root().join(".bundle-key");
        if let Ok(bytes) = std::fs::read(&path) {
            if bytes.len() == 32 {
                return *Key::from_slice(&bytes);
            }
        }
        // Generate once and persist (0600).
        let k = ChaCha20Poly1305::generate_key(&mut OsRng);
        let _ = std::fs::create_dir_all(sessions_root());
        if std::fs::write(&path, k.as_slice()).is_ok() {
            harden_file(&path);
        }
        k
    })
}

/// Encrypt: `[12-byte nonce][ciphertext+tag]`.
fn encrypt(plain: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = ChaCha20Poly1305::new(key());
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ct = cipher.encrypt(&nonce, plain).map_err(|_| "aead encrypt".to_string())?;
    let mut out = Vec::with_capacity(12 + ct.len());
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(&ct);
    Ok(out)
}

fn decrypt(sealed: &[u8]) -> Result<Vec<u8>, String> {
    if sealed.len() < 12 {
        return Err("sealed bundle too short".into());
    }
    let cipher = ChaCha20Poly1305::new(key());
    let nonce = Nonce::from_slice(&sealed[..12]);
    cipher.decrypt(nonce, &sealed[12..]).map_err(|_| "aead decrypt (wrong key or corrupt bundle)".into())
}

/// tar.gz the *contents* of `dir` (paths relative to it).
fn pack(dir: &Path) -> std::io::Result<Vec<u8>> {
    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all(".", dir)?;
    let enc = tar.into_inner()?;
    enc.finish()
}

fn unpack(tar_gz: &[u8], dir: &Path) -> std::io::Result<()> {
    let dec = flate2::read::GzDecoder::new(tar_gz);
    let mut ar = tar::Archive::new(dec);
    ar.set_preserve_permissions(true);
    ar.unpack(dir)
}

fn harden_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

fn hex_decode(s: &str) -> Result<Vec<u8>, ()> {
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_wipes_plaintext_and_persists_refresh() {
        let root = std::env::temp_dir().join(format!("hull-agent-test-{}", uuid::Uuid::new_v4().simple()));
        std::env::set_var("HULL_AGENT_SESSIONS", &root);
        std::env::set_var("HULL_SESSION_KEY", "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff");
        let rt = tokio::runtime::Runtime::new().unwrap();

        // A login writes a credential file into the staging dir…
        let (session, dir) = provision().unwrap();
        std::fs::write(dir.join("auth.json"), b"{\"refresh_token\":\"secret-xyz\"}").unwrap();
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("nested/state"), b"deep").unwrap();

        // …then it's sealed: encrypted blob appears, plaintext staging is gone, and the raw bytes
        // on disk do NOT contain the secret.
        seal(&session).unwrap();
        assert!(exists(&session));
        assert!(!dir.exists(), "plaintext staging dir must be wiped after seal");
        let raw = std::fs::read(sessions_root().join(format!("{session}.enc"))).unwrap();
        assert!(!raw.windows(10).any(|w| w == b"secret-xyz"), "secret must not appear in the sealed bytes");

        // A run opens the bundle, mutates a credential (a token refresh), then drops the guard.
        let open_dir;
        {
            let g = rt.block_on(open(&session)).unwrap();
            open_dir = g.dir_string();
            assert_eq!(std::fs::read(PathBuf::from(&open_dir).join("auth.json")).unwrap(), b"{\"refresh_token\":\"secret-xyz\"}");
            assert_eq!(std::fs::read(PathBuf::from(&open_dir).join("nested/state")).unwrap(), b"deep");
            std::fs::write(PathBuf::from(&open_dir).join("auth.json"), b"{\"refresh_token\":\"rotated-abc\"}").unwrap();
        }
        assert!(!PathBuf::from(&open_dir).exists(), "decrypted dir must be wiped when the guard drops");

        // Re-opening sees the rotated token (drop re-sealed it), proving refresh persists.
        {
            let g = rt.block_on(open(&session)).unwrap();
            assert_eq!(std::fs::read(PathBuf::from(&g.dir_string()).join("auth.json")).unwrap(), b"{\"refresh_token\":\"rotated-abc\"}");
        }

        remove(&session);
        assert!(!exists(&session));
        let _ = std::fs::remove_dir_all(&root);
    }
}
