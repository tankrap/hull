//! Multi-repo / multi-tenant git serving (M4 / NEW-1117).
//!
//! One Hull server hosts N keel repos, routing `/{tenant}/{repo}/…` to the right store — the core
//! hosting gap (`keel serve` is one repo per process; `keeld` is one repo per machine). It speaks
//! the git smart-HTTP protocol via keel-git, so a plain `git clone` / `git push` works, and a push
//! is **bridged to native keel history** (Change/Tree DAG) so brief/provenance/status light up even
//! though the client used vanilla git. Auth + per-tenant authorization is the next slice (ties to
//! NEW-1166); this lands the routing + store lifecycle.

use axum::{
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use keel_store::Store;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Hosts keel repos under a root dir at `<root>/{tenant}/{repo}/.keel/store`. Opened stores are
/// cached — an LMDB env is cheap to clone (shared handle) but expensive to open per request.
#[derive(Clone)]
pub struct RepoHost {
    root: PathBuf,
    open: Arc<Mutex<HashMap<String, Store>>>,
}

impl RepoHost {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        RepoHost { root: root.into(), open: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Root from `HULL_REPOS_ROOT`, defaulting to `~/.hull/repos`.
    pub fn from_env() -> Self {
        let root = std::env::var("HULL_REPOS_ROOT").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            format!("{home}/.hull/repos")
        });
        RepoHost::new(root)
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// The repos currently on disk, as `"tenant/repo"` — a filesystem-backed registry.
    pub fn list(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(tenants) = std::fs::read_dir(&self.root) else { return out };
        for t in tenants.flatten() {
            if !t.file_type().map(|f| f.is_dir()).unwrap_or(false) {
                continue;
            }
            let tenant = t.file_name().to_string_lossy().into_owned();
            if let Ok(repos) = std::fs::read_dir(t.path()) {
                for r in repos.flatten() {
                    if r.path().join(".keel/store").exists() {
                        out.push(format!("{tenant}/{}", r.file_name().to_string_lossy()));
                    }
                }
            }
        }
        out.sort();
        out
    }

    /// Open (and cache) a repo's store. `create` makes it if missing — used by receive-pack so a
    /// first `git push` to a new name provisions the repo. Returns `None` if the name is invalid
    /// or (when `create` is false) the repo doesn't exist.
    fn store(&self, tenant: &str, repo: &str, create: bool) -> io::Result<Option<Store>> {
        if !safe_segment(tenant) || !safe_segment(repo) {
            return Ok(None);
        }
        let key = format!("{tenant}/{repo}");
        if let Some(s) = self.open.lock().unwrap().get(&key) {
            return Ok(Some(s.clone()));
        }
        let path = self.root.join(tenant).join(repo).join(".keel/store");
        if !create && !path.exists() {
            return Ok(None);
        }
        std::fs::create_dir_all(&path)?;
        let store = Store::open(&path).map_err(|e| io::Error::other(e.to_string()))?;
        self.open.lock().unwrap().insert(key, store.clone());
        Ok(Some(store))
    }
}

/// A repo/tenant path segment must be a plain name — no traversal, separators, or dotfiles.
fn safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.starts_with('.')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !s.contains("..")
}

// ── axum handlers (wired by `router` in lib.rs) ─────────────────────────────────────────────────

/// The `RepoHost` is pulled from the app state via this trait so the handlers stay decoupled from
/// the concrete `App` struct.
pub trait HasRepoHost {
    fn repo_host(&self) -> &RepoHost;
}

/// `GET /{tenant}/{repo}/info/refs?service=git-(upload|receive)-pack` — the ref advertisement.
pub async fn info_refs<S: HasRepoHost + Clone + Send + Sync + 'static>(
    State(app): State<S>,
    Path((tenant, repo)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Response {
    let service = query
        .as_deref()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("service=")))
        .unwrap_or("")
        .to_string();
    if service != "git-upload-pack" && service != "git-receive-pack" {
        return (StatusCode::FORBIDDEN, "unsupported service").into_response();
    }
    // A push begins with this handshake, so provision the repo here for receive-pack (so a first
    // `git push` to a new name works); a clone/fetch (upload-pack) 404s on a missing repo.
    let create = service == "git-receive-pack";
    let store = match app.repo_host().store(&tenant, &repo, create) {
        Ok(Some(s)) => s,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such repo").into_response(),
        Err(e) => return server_error(e),
    };
    match keel_git::smart_http::advertisement(&store, &service) {
        Ok(adv) => with_type(&format!("application/x-{service}-advertisement"), adv),
        Err(e) => server_error(e),
    }
}

/// `POST /{tenant}/{repo}/git-upload-pack` — clone/fetch. git gzips this body by default.
pub async fn upload_pack<S: HasRepoHost + Clone + Send + Sync + 'static>(
    State(app): State<S>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body = maybe_gunzip(&headers, body.to_vec());
    let store = match app.repo_host().store(&tenant, &repo, false) {
        Ok(Some(s)) => s,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such repo").into_response(),
        Err(e) => return server_error(e),
    };
    match keel_git::smart_http::upload_pack(&store, &body) {
        Ok(resp) => with_type("application/x-git-upload-pack-result", resp),
        Err(e) => server_error(e),
    }
}

/// `POST /{tenant}/{repo}/git-receive-pack` — push. Provisions the repo on first push and bridges
/// the pushed objects into native keel history so brief/provenance work.
pub async fn receive_pack<S: HasRepoHost + Clone + Send + Sync + 'static>(
    State(app): State<S>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body = maybe_gunzip(&headers, body.to_vec());
    let store = match app.repo_host().store(&tenant, &repo, true) {
        Ok(Some(s)) => s,
        Ok(None) => return (StatusCode::BAD_REQUEST, "invalid repo name").into_response(),
        Err(e) => return server_error(e),
    };
    match keel_git::smart_http::receive_pack(&store, &body) {
        Ok(resp) => {
            let _ = keel_git::bridge::bridge(&store); // git → native keel history
            with_type("application/x-git-receive-pack-result", resp)
        }
        Err(e) => server_error(e),
    }
}

fn with_type(content_type: &str, body: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::CACHE_CONTROL, "no-cache".to_string()),
        ],
        body,
    )
        .into_response()
}

fn server_error(e: io::Error) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("git error: {e}")).into_response()
}

/// Decompress the body if the request declares `Content-Encoding: gzip` (git's default for the
/// upload-pack POST). On any decode failure fall back to the raw bytes.
fn maybe_gunzip(headers: &HeaderMap, body: Vec<u8>) -> Vec<u8> {
    let is_gzip = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().contains("gzip"))
        .unwrap_or(false);
    if !is_gzip {
        return body;
    }
    use std::io::Read;
    let mut out = Vec::new();
    match flate2::read::GzDecoder::new(&body[..]).read_to_end(&mut out) {
        Ok(_) => out,
        Err(_) => body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_validation_blocks_traversal() {
        assert!(safe_segment("tankrap"));
        assert!(safe_segment("hull-server_2"));
        assert!(!safe_segment(".."));
        assert!(!safe_segment("a/b"));
        assert!(!safe_segment(".hidden"));
        assert!(!safe_segment(""));
        assert!(!safe_segment("a..b"));
    }

    #[test]
    fn list_reports_repos_on_disk() {
        let tmp = std::env::temp_dir().join(format!("hull-repos-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("acme/web/.keel/store")).unwrap();
        std::fs::create_dir_all(tmp.join("acme/api/.keel/store")).unwrap();
        std::fs::create_dir_all(tmp.join("acme/not-a-repo")).unwrap(); // no .keel/store → skipped
        let host = RepoHost::new(&tmp);
        assert_eq!(host.list(), vec!["acme/api".to_string(), "acme/web".to_string()]);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
