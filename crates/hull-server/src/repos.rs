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
use keel_store::{diff_lines, Object, ObjectId, Store, Tag, Verification};
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Hosts keel repos under a root dir at `<root>/{tenant}/{repo}/.keel/store`. Opened stores are
/// cached — an LMDB env is cheap to clone (shared handle) but expensive to open per request.
/// A secret detected by the server-side scan of a push (the backstop layer).
#[derive(Clone, serde::Serialize)]
pub struct SecretHit {
    pub repo: String,
    pub change: String,
    pub path: String,
    pub rule: String,
    pub title: String,
    pub line: usize,
    pub redacted: String,
}

#[derive(Clone)]
pub struct RepoHost {
    root: PathBuf,
    open: Arc<Mutex<HashMap<String, Store>>>,
    secrets: Arc<Mutex<Vec<SecretHit>>>,
}

impl RepoHost {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        RepoHost {
            root: root.into(),
            open: Arc::new(Mutex::new(HashMap::new())),
            secrets: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Scan the changed blobs of a repo's HEAD change for secrets (the server-side backstop; the
    /// same `hull-scan` engine runs client-side too). Records hits and returns how many were found.
    /// Called after a push bridges git → keel.
    pub fn scan_head(&self, tenant: &str, repo: &str) -> usize {
        let Ok(Some(store)) = self.store(tenant, repo, false) else { return 0 };
        let Some(head) = store.get_ref("main").ok().flatten() else { return 0 };
        let change = match store.get(&head).ok().flatten() {
            Some(Object::Change(c)) => c,
            _ => return 0,
        };
        let mut head_files = HashMap::new();
        flatten_tree(&store, change.tree, "", &mut head_files, 0);
        let mut parent_files = HashMap::new();
        if let Some(p) = change.parents.first() {
            if let Some(Object::Change(pc)) = store.get(p).ok().flatten() {
                flatten_tree(&store, pc.tree, "", &mut parent_files, 0);
            }
        }
        let key = format!("{tenant}/{repo}");
        let change_hex = head.to_hex();
        let mut hits = Vec::new();
        for (path, blob) in &head_files {
            if parent_files.get(path) == Some(blob) {
                continue; // unchanged
            }
            if let Some(Object::Blob(b)) = store.get(blob).ok().flatten() {
                if b.len() > MAX_BLOB_FOR_DIFF {
                    continue;
                }
                let text = String::from_utf8_lossy(&b);
                for f in hull_scan::scan(&text) {
                    hits.push(SecretHit {
                        repo: key.clone(),
                        change: change_hex.clone(),
                        path: path.clone(),
                        rule: f.rule,
                        title: f.title,
                        line: f.line,
                        redacted: f.redacted,
                    });
                }
            }
        }
        let n = hits.len();
        if n > 0 {
            self.secrets.lock().unwrap().extend(hits);
        }
        n
    }

    /// Secret hits recorded for a repo (server-side scan backstop).
    pub fn secrets(&self, repo: &str) -> Vec<SecretHit> {
        self.secrets.lock().unwrap().iter().filter(|s| s.repo == repo).cloned().collect()
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

/// A resolved content-addressed anchor: the keel blob id a path currently maps to at HEAD, plus the
/// change id that HEAD is — so an issue's line-ref survives edits (the blob stays valid) and can be
/// traced to the change/agent that produced it (`keel why`).
pub struct BlobAnchor {
    pub blob: String,
    pub change: String,
}

impl RepoHost {
    /// The keel change id at HEAD of a hosted repo (hex), or `None` if the repo/ref is missing.
    /// A PR proposes real keel changes, so it anchors to this content address.
    pub fn head_change(&self, tenant: &str, repo: &str) -> Option<String> {
        let store = self.store(tenant, repo, false).ok()??;
        store.get_ref("main").ok()?.map(|id| id.to_hex())
    }

    /// Resolve `path` in a hosted repo to the keel blob it points at in HEAD's tree. This is what
    /// makes a Hull code-ref content-addressed rather than a fragile `file#L42`. `None` if the repo
    /// or path doesn't exist. Reuses the cached store (no second LMDB open).
    pub fn resolve_blob(&self, tenant: &str, repo: &str, path: &str) -> Option<BlobAnchor> {
        let store = self.store(tenant, repo, false).ok()??;
        let head = store.get_ref("main").ok()??;
        let tree = match store.get(&head).ok()?? {
            Object::Change(c) => c.tree,
            _ => return None,
        };
        let blob = resolve_path_in_tree(&store, tree, path)?;
        Some(BlobAnchor { blob: blob.to_hex(), change: head.to_hex() })
    }
}

/// One change that touched a path — the keel-native provenance behind a code-ref.
#[derive(serde::Serialize)]
pub struct Provenance {
    pub change: String,
    pub intent: String,
    pub author: String,
}

/// A file changed by a keel change (vs its first parent) — the "what does this touch" for a review.
#[derive(serde::Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: String, // added | modified | deleted
}

/// The keel session behind a change (task/reasoning/operations) — populated when the change was
/// committed with `--session` / `keel capture`. Absent for a plain `git push`.
#[derive(serde::Serialize)]
pub struct SessionSummary {
    pub task: String,
    pub model: String,
    pub lesson: String,
    pub tool_calls: usize,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// The keel change a PR proposes, expanded for the review page — with real keel verification and,
/// when present, the session behind it.
#[derive(serde::Serialize)]
pub struct ChangeInfo {
    pub id: String,
    pub intent: String,
    pub author: String,
    /// keel verification state: `green` | `red` | `unverified` (the "tests & CI" signal).
    pub verification: String,
    pub files: Vec<ChangedFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionSummary>,
}

fn verification_str(v: Verification) -> String {
    match v {
        Verification::Green => "green",
        Verification::Red => "red",
        Verification::Unverified => "unverified",
    }
    .to_string()
}

const MODE_DIR: u32 = 0o040000;

#[derive(serde::Serialize)]
pub struct DiffLineOut {
    pub tag: String, // add | del | ctx
    pub text: String,
}

#[derive(serde::Serialize)]
pub struct HunkOut {
    pub old_start: usize,
    pub new_start: usize,
    pub lines: Vec<DiffLineOut>,
}

/// A file's diff for the review viewer: the line hunks plus a best-effort **semantic operations**
/// summary derived from them (added/removed functions/types/imports) — "what changed" as operations,
/// not just text (the review-should-show-the-operation idea; full semantic diff is roadmapped).
#[derive(serde::Serialize)]
pub struct FileDiff {
    pub path: String,
    pub status: String,
    pub ops: Vec<String>,
    pub hunks: Vec<HunkOut>,
}

const MAX_DIFF_FILES: usize = 40;
const MAX_BLOB_FOR_DIFF: usize = 256 * 1024; // skip huge/binary blobs

/// The leading identifier after a keyword (`fn foo` → `foo`).
fn ident_after(s: &str) -> String {
    s.trim_start().chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect()
}

/// Best-effort semantic operations from hunks: detect definitions added/removed by signature across
/// Rust / TS-JS / Python / CSS. Heuristic, not a real semantic diff (which is roadmapped), but it
/// turns "+40 lines" into "added fn `verify`".
fn semantic_ops(hunks: &[keel_store::Hunk]) -> Vec<String> {
    // keyword → label, for `KEYWORD name …`
    let kinds = [
        ("fn ", "fn"), ("function ", "fn"), ("def ", "fn"),
        ("struct ", "struct"), ("enum ", "enum"), ("class ", "class"),
        ("trait ", "trait"), ("interface ", "interface"), ("type ", "type"),
    ];
    let mut ops = Vec::new();
    for h in hunks {
        for l in &h.lines {
            let verb = match l.tag {
                Tag::Add => "added",
                Tag::Del => "removed",
                Tag::Context => continue,
            };
            let mut t = l.text.trim_start();
            for p in ["pub ", "async ", "export ", "default ", "public ", "private ", "static "] {
                t = t.strip_prefix(p).unwrap_or(t);
            }
            // imports
            if t.starts_with("use ") || t.starts_with("import ") || t.starts_with("from ") {
                ops.push(format!("{verb} import"));
                continue;
            }
            // `KEYWORD name`
            let mut matched = false;
            for (pat, label) in kinds {
                if let Some(rest) = t.strip_prefix(pat) {
                    let name = ident_after(rest);
                    if !name.is_empty() {
                        ops.push(format!("{verb} {label} `{name}`"));
                    }
                    matched = true;
                    break;
                }
            }
            if matched {
                continue;
            }
            // TS/JS: `const Name = (…) =>` / `= async`, and destructured hooks `const [x, setX] = useState`
            if let Some(rest) = t.strip_prefix("const ").or_else(|| t.strip_prefix("let ")) {
                if rest.trim_start().starts_with('[') {
                    if rest.contains("useState") || rest.contains("useRef") || rest.contains("useReducer") {
                        let name = ident_after(rest.trim_start().trim_start_matches('['));
                        if !name.is_empty() {
                            ops.push(format!("{verb} state `{name}`"));
                        }
                    }
                    continue;
                }
                let name = ident_after(rest);
                let looks_fn = rest.contains("=>") || rest.contains("= (") || rest.contains("=(") || rest.contains("= async");
                if !name.is_empty() && looks_fn {
                    let label = if name.chars().next().is_some_and(char::is_uppercase) { "component" } else { "fn" };
                    ops.push(format!("{verb} {label} `{name}`"));
                }
                continue;
            }
            // CSS rule: a line with a selector before `{` (single-line rules included)
            if let Some(brace) = t.find('{') {
                let sel = t[..brace].trim();
                let ok = sel
                    .chars()
                    .next()
                    .is_some_and(|c| c == '.' || c == '#' || c == ':' || c == '*' || c == '&' || c == '-' || c.is_alphabetic());
                if !sel.is_empty() && sel.len() < 60 && ok && !sel.contains('(') && !sel.contains('=') && !sel.contains(';') {
                    ops.push(format!("{verb} style `{sel}`"));
                }
            }
        }
    }
    ops.sort();
    ops.dedup();
    ops
}

/// Recursively flatten a tree to `path -> blob id`.
fn flatten_tree(store: &Store, tree: ObjectId, prefix: &str, out: &mut HashMap<String, ObjectId>, depth: u32) {
    if depth > 64 {
        return;
    }
    let entries = match store.get(&tree).ok().flatten() {
        Some(Object::Tree(t)) => t.entries,
        _ => return,
    };
    for e in entries {
        let path = if prefix.is_empty() { e.name.clone() } else { format!("{prefix}/{}", e.name) };
        if e.mode == MODE_DIR {
            flatten_tree(store, e.id, &path, out, depth + 1);
        } else {
            out.insert(path, e.id);
        }
    }
}

impl RepoHost {
    /// Expand the keel change `hex` in a hosted repo: its intent/author and the files it changed vs
    /// its first parent (keel-native — "what does this change touch"). `None` if not found.
    pub fn change_info(&self, tenant: &str, repo: &str, hex: &str) -> Option<ChangeInfo> {
        let store = self.store(tenant, repo, false).ok()??;
        let cid = ObjectId::from_hex(hex)?;
        let change = match store.get(&cid).ok()?? {
            Object::Change(c) => c,
            _ => return None,
        };
        let mut head = HashMap::new();
        flatten_tree(&store, change.tree, "", &mut head, 0);
        let mut parent = HashMap::new();
        if let Some(p) = change.parents.first() {
            if let Some(Object::Change(pc)) = store.get(p).ok().flatten() {
                flatten_tree(&store, pc.tree, "", &mut parent, 0);
            }
        }
        let mut files = Vec::new();
        for (path, blob) in &head {
            match parent.get(path) {
                None => files.push(ChangedFile { path: path.clone(), status: "added".into() }),
                Some(pb) if pb != blob => files.push(ChangedFile { path: path.clone(), status: "modified".into() }),
                _ => {}
            }
        }
        for path in parent.keys() {
            if !head.contains_key(path) {
                files.push(ChangedFile { path: path.clone(), status: "deleted".into() });
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        // Real keel verification (tests & CI signal), and the session behind the change if it links one.
        let verification = verification_str(store.verification(&cid).unwrap_or(Verification::Unverified));
        let session = change.session.and_then(|sid| match store.get(&sid).ok().flatten() {
            Some(Object::Session(s)) => Some(SessionSummary {
                task: s.task,
                model: s.model,
                lesson: s.lesson,
                tool_calls: s.tool_calls.len(),
                tokens_in: s.tokens_in,
                tokens_out: s.tokens_out,
            }),
            _ => None,
        });
        Some(ChangeInfo { id: hex.to_string(), intent: change.intent, author: change.author, verification, files, session })
    }

    /// The per-file diff of a change vs its first parent — line hunks + a semantic-ops summary.
    pub fn diff(&self, tenant: &str, repo: &str, hex: &str) -> Vec<FileDiff> {
        let Ok(Some(store)) = self.store(tenant, repo, false) else { return Vec::new() };
        let Some(cid) = ObjectId::from_hex(hex) else { return Vec::new() };
        let change = match store.get(&cid).ok().flatten() {
            Some(Object::Change(c)) => c,
            _ => return Vec::new(),
        };
        let mut head = HashMap::new();
        flatten_tree(&store, change.tree, "", &mut head, 0);
        let mut parent = HashMap::new();
        if let Some(p) = change.parents.first() {
            if let Some(Object::Change(pc)) = store.get(p).ok().flatten() {
                flatten_tree(&store, pc.tree, "", &mut parent, 0);
            }
        }
        let read = |id: &ObjectId| -> Option<String> {
            match store.get(id).ok().flatten() {
                Some(Object::Blob(b)) if b.len() <= MAX_BLOB_FOR_DIFF => Some(String::from_utf8_lossy(&b).into_owned()),
                _ => None,
            }
        };
        // union of paths, changed only
        let mut paths: Vec<&String> = head.keys().chain(parent.keys()).collect();
        paths.sort();
        paths.dedup();
        let mut out = Vec::new();
        for path in paths {
            let (h, p) = (head.get(path), parent.get(path));
            let status = match (p, h) {
                (None, Some(_)) => "added",
                (Some(_), None) => "deleted",
                (Some(a), Some(b)) if a != b => "modified",
                _ => continue,
            };
            let old = p.and_then(read).unwrap_or_default();
            let new = h.and_then(read).unwrap_or_default();
            let hunks = diff_lines(&old, &new);
            let ops = semantic_ops(&hunks);
            let hunks_out = hunks
                .into_iter()
                .map(|hk| HunkOut {
                    old_start: hk.old_start,
                    new_start: hk.new_start,
                    lines: hk
                        .lines
                        .into_iter()
                        .map(|l| DiffLineOut {
                            tag: match l.tag {
                                Tag::Add => "add",
                                Tag::Del => "del",
                                Tag::Context => "ctx",
                            }
                            .to_string(),
                            text: l.text,
                        })
                        .collect(),
                })
                .collect();
            out.push(FileDiff { path: path.clone(), status: status.to_string(), ops, hunks: hunks_out });
            if out.len() >= MAX_DIFF_FILES {
                break;
            }
        }
        out
    }

    /// The keel verification state of a change (`green`/`red`/`unverified`), or `None` if missing.
    pub fn verification(&self, tenant: &str, repo: &str, hex: &str) -> Option<String> {
        let store = self.store(tenant, repo, false).ok()??;
        let cid = ObjectId::from_hex(hex)?;
        Some(verification_str(store.verification(&cid).ok()?))
    }

    /// Set a change's keel verification (`green` or `red`) — the same side table `keel verify` writes.
    pub fn set_verification(&self, tenant: &str, repo: &str, hex: &str, green: bool) -> bool {
        let Ok(Some(store)) = self.store(tenant, repo, false) else { return false };
        let Some(cid) = ObjectId::from_hex(hex) else { return false };
        let v = if green { Verification::Green } else { Verification::Red };
        store.set_verification(&cid, v).is_ok()
    }
}

impl RepoHost {
    /// First-parent history where `path`'s content changed vs its parent — i.e. the changes (and
    /// authors/agents) that actually touched it. This is `keel why` over a hosted repo, the spine
    /// that lets a Hull code-ref resolve to who produced it. Newest first, capped at `limit`.
    pub fn why(&self, tenant: &str, repo: &str, path: &str, limit: usize) -> Vec<Provenance> {
        let Ok(Some(store)) = self.store(tenant, repo, false) else { return Vec::new() };
        let mut out = Vec::new();
        let mut cur = store.get_ref("main").ok().flatten();
        while let Some(cid) = cur {
            let change = match store.get(&cid).ok().flatten() {
                Some(Object::Change(c)) => c,
                _ => break,
            };
            let here = resolve_path_in_tree(&store, change.tree, path);
            let parent = change.parents.first().copied();
            let there = parent
                .and_then(|p| match store.get(&p).ok().flatten() {
                    Some(Object::Change(pc)) => resolve_path_in_tree(&store, pc.tree, path),
                    _ => None,
                });
            if here != there {
                out.push(Provenance {
                    change: cid.to_hex(),
                    intent: change.intent,
                    author: change.author,
                });
                if out.len() >= limit {
                    break;
                }
            }
            cur = parent;
        }
        out
    }
}

/// Walk `tree` down `path` (`/`-separated) to the blob id at the leaf.
fn resolve_path_in_tree(store: &Store, tree: ObjectId, path: &str) -> Option<ObjectId> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }
    let mut cur = tree;
    for (i, part) in parts.iter().enumerate() {
        let entries = match store.get(&cur).ok()?? {
            Object::Tree(t) => t.entries,
            _ => return None,
        };
        let entry = entries.into_iter().find(|e| e.name == *part)?;
        if i == parts.len() - 1 {
            return Some(entry.id);
        }
        cur = entry.id;
    }
    None
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
            // Server-side secret-scan backstop: flag any secret in the pushed change.
            let n = app.repo_host().scan_head(&tenant, &repo);
            if n > 0 {
                eprintln!("hull: ⚠ {n} secret finding(s) in push to {tenant}/{repo} (see /api/repos/…/security)");
            }
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
