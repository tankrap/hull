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

/// Process-local sequence appended to scratch checkout dir names (`hull-fix-*`, `hull-indep-*`) so
/// every operation gets a unique directory, even when two concurrent requests materialize the same
/// change/tree — otherwise they would share a dir and `remove_dir_all` each other's checkout mid-run.
/// Deterministic (no RNG in this runtime); the tree/change id + pid + seq is uniqueness enough.
static SCRATCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    /// Per-`tenant/repo/branch` land+export serialization. The git-mirror export runs *after* the
    /// keel CAS commits, so two concurrent lands to the same protected branch would otherwise race
    /// `mirror::refs`-read → `set_ref` and the last writer would drop the other landed change from
    /// git `main`. `perform_merge` holds the branch's async mutex across the whole land+export
    /// critical section to fully order them (see [`RepoHost::land_lock`]).
    land_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl RepoHost {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        RepoHost {
            root: root.into(),
            open: Arc::new(Mutex::new(HashMap::new())),
            secrets: Arc::new(Mutex::new(Vec::new())),
            land_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The per-branch land+export lock for `tenant/repo/branch`, created on first use. The caller
    /// (`perform_merge`) holds the returned mutex's guard across `plan_merge` → verify → `land_merge`
    /// (which runs the git-mirror export) so concurrent same-branch lands are serialized as a unit.
    /// No deadlock: this lock is acquired only in that one place and is the outermost lock held —
    /// nothing else is awaited while a task waits to acquire it, and no store lock is held across it.
    pub(crate) fn land_lock(&self, tenant: &str, repo: &str, branch: &str) -> Arc<tokio::sync::Mutex<()>> {
        let key = format!("{tenant}/{repo}/{branch}");
        self.land_locks
            .lock()
            .unwrap()
            .entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
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
        // Hold the cache lock across the whole check-open-insert. Releasing it between the miss and the
        // `Store::open` would let two concurrent first-touches of the same repo each open the LMDB
        // environment — opening one env twice in a process is undefined behavior and can corrupt it.
        // A second caller that arrives mid-open waits here, then hits the cache. Opens are one-time
        // per repo, so the added serialization is a rare, brief stall; nothing under this section
        // re-enters `store()`/locks `open`, so there is no deadlock.
        let mut open = self.open.lock().unwrap();
        if let Some(s) = open.get(&key) {
            return Ok(Some(s.clone()));
        }
        let path = self.root.join(tenant).join(repo).join(".keel/store");
        if !create && !path.exists() {
            return Ok(None);
        }
        std::fs::create_dir_all(&path)?;
        let store = Store::open(&path).map_err(|e| io::Error::other(e.to_string()))?;
        open.insert(key, store.clone());
        Ok(Some(store))
    }

    /// Provision an empty repo (dir + keel store) so it can be cloned and pushed to. Returns `true`
    /// if newly created, `false` if it already existed. Errors on an invalid name.
    pub fn create_repo(&self, tenant: &str, repo: &str) -> io::Result<bool> {
        if !safe_segment(tenant) || !safe_segment(repo) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid repo name"));
        }
        let existed = self.root.join(tenant).join(repo).join(".keel/store").exists();
        self.store(tenant, repo, true)?;
        Ok(!existed)
    }

    /// Delete a repo's on-disk directory (keel store + working tree) and evict it from the open-store
    /// cache. Returns `true` if a directory was removed, `false` if nothing was on disk. Errors on an
    /// invalid name.
    pub fn delete_repo(&self, tenant: &str, repo: &str) -> io::Result<bool> {
        if !safe_segment(tenant) || !safe_segment(repo) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid repo name"));
        }
        // Drop any cached store handle first so no LMDB mmap keeps the dir alive.
        self.open.lock().unwrap().remove(&format!("{tenant}/{repo}"));
        let dir = self.root.join(tenant).join(repo);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Rename a repo's on-disk directory (tenant unchanged), evicting cached store handles for both
    /// names. A repo that was never pushed to (no dir yet) is a no-op. Errors on an invalid name or if
    /// the destination already exists.
    pub fn rename_repo(&self, tenant: &str, old: &str, new: &str) -> io::Result<()> {
        if !safe_segment(tenant) || !safe_segment(old) || !safe_segment(new) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid repo name"));
        }
        {
            let mut open = self.open.lock().unwrap();
            open.remove(&format!("{tenant}/{old}"));
            open.remove(&format!("{tenant}/{new}"));
        }
        let from = self.root.join(tenant).join(old);
        let to = self.root.join(tenant).join(new);
        if !from.exists() {
            return Ok(()); // never pushed — nothing on disk to move
        }
        if to.exists() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "destination repo already exists"));
        }
        std::fs::rename(&from, &to)?;
        Ok(())
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

    /// The change a named `branch` ref points at (hex), or `None` if the repo/branch is absent. Unlike
    /// [`Self::head_change`] this isn't hard-wired to `main` — the protected-branch land reads the
    /// repo's configured default branch through it.
    pub fn branch_head(&self, tenant: &str, repo: &str, branch: &str) -> Option<String> {
        let store = self.store(tenant, repo, false).ok()??;
        store.get_ref(branch).ok()?.map(|id| id.to_hex())
    }

    /// Resolve a pushed git commit (full 40-hex SHA-1) to the keel change it was bridged into — the
    /// glue that lets a voyage be opened from a pushed branch's HEAD, not just `main`.
    /// The `gchange` aux namespace (git-commit-oid(20) → keel change id(32)) is written by the bridge
    /// for every pushed commit, so a branch's changes are resolvable even before it's merged.
    pub fn change_for_commit(&self, tenant: &str, repo: &str, sha_hex: &str) -> Option<String> {
        let store = self.store(tenant, repo, false).ok()??;
        let s = sha_hex.trim();
        if s.len() != 40 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let oid: Vec<u8> = (0..40).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect();
        let cid = store.aux_get("gchange", &oid).ok()??;
        Some(cid.iter().map(|b| format!("{b:02x}")).collect())
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

    /// Read a file's bytes from the repo's HEAD tree by path (content-addressed). Used to read the
    /// in-repo `.hull/CODEOWNERS`. `None` if the repo or path doesn't exist.
    pub fn read_file(&self, tenant: &str, repo: &str, path: &str) -> Option<Vec<u8>> {
        let store = self.store(tenant, repo, false).ok()??;
        let head = store.get_ref("main").ok()??;
        let tree = match store.get(&head).ok()?? {
            Object::Change(c) => c.tree,
            _ => return None,
        };
        let blob = resolve_path_in_tree(&store, tree, path)?;
        match store.get(&blob).ok()?? {
            Object::Blob(bytes) => Some(bytes),
            _ => None,
        }
    }
}

impl RepoHost {
    /// `(author, unix)` for every change reachable from ANY ref (all branches) newer than `since` —
    /// the raw material for a contribution heatmap. Each change is counted once. Stops descending a
    /// branch once it predates `since` (parents are older still) and caps total work.
    pub fn history(&self, tenant: &str, repo: &str, extra_roots: &[String], since: u64) -> Vec<(String, u64, String)> {
        let Some(store) = self.store(tenant, repo, false).ok().flatten() else { return vec![] };
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<keel_store::ObjectId> = store.list_refs().unwrap_or_default().into_iter().map(|(_, id)| id).collect();
        // Feature branches aren't keel refs, but every PR/voyage points at its change — seed those too
        // so work on unmerged branches still counts.
        for hex in extra_roots {
            if let Some(oid) = keel_store::ObjectId::from_hex(hex) {
                stack.push(oid);
            }
        }
        while let Some(id) = stack.pop() {
            if out.len() > 50_000 {
                break;
            }
            if seen.contains(&id) {
                continue;
            }
            seen.insert(id);
            let Ok(Some(Object::Change(c))) = store.get(&id) else { continue };
            if c.timestamp >= since {
                out.push((c.author.clone(), c.timestamp, id.to_hex()));
                for p in c.parents {
                    stack.push(p);
                }
            }
        }
        out
    }
}

/// A node (file) + edges (imports) for the codebase graph.
#[derive(serde::Serialize)]
pub struct GraphNode {
    pub path: String,
    pub dir: String,
    pub lang: String,
    pub size: u64,
    pub deg: usize,
}
#[derive(serde::Serialize)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
}

impl RepoHost {
    /// Build a codebase import graph at a branch: nodes are source files, edges are resolved in-repo
    /// imports (TS/JS relative `import`/`require`, Rust `mod`). Self-contained (no resolver sidecars).
    pub fn code_graph(&self, tenant: &str, repo: &str, ref_name: &str) -> (Vec<GraphNode>, Vec<GraphEdge>) {
        let Some(store) = self.store(tenant, repo, false).ok().flatten() else { return (vec![], vec![]) };
        let Some(root) = self.root_tree(&store, ref_name) else { return (vec![], vec![]) };
        // Collect all source files (path -> text).
        let mut files: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut stack = vec![(String::new(), root)];
        let lang_of = |p: &str| -> Option<&'static str> {
            if p.ends_with(".tsx") || p.ends_with(".ts") { Some("ts") }
            else if p.ends_with(".jsx") || p.ends_with(".js") || p.ends_with(".mjs") { Some("js") }
            else if p.ends_with(".rs") { Some("rust") }
            else if p.ends_with(".py") { Some("python") }
            else if p.ends_with(".go") { Some("go") }
            else { None }
        };
        while let Some((prefix, tid)) = stack.pop() {
            if files.len() > 6000 { break; }
            let entries = match store.get(&tid) { Ok(Some(Object::Tree(t))) => t.entries, _ => continue };
            for e in entries {
                let full = if prefix.is_empty() { e.name.clone() } else { format!("{prefix}/{}", e.name) };
                match store.get(&e.id) {
                    Ok(Some(Object::Tree(_))) => stack.push((full, e.id)),
                    Ok(Some(Object::Blob(b))) if lang_of(&full).is_some() && b.len() < 400_000 => {
                        files.insert(full, String::from_utf8_lossy(&b).into_owned());
                    }
                    _ => {}
                }
            }
        }
        let paths: std::collections::HashSet<String> = files.keys().cloned().collect();
        // Resolve a relative TS/JS import spec from `dir` to an existing repo file.
        let resolve_rel = |dir: &str, spec: &str| -> Option<String> {
            let mut parts: Vec<String> = if dir.is_empty() { vec![] } else { dir.split('/').map(str::to_string).collect() };
            for seg in spec.split('/') {
                match seg {
                    "." | "" => {}
                    ".." => { parts.pop(); }
                    s => parts.push(s.to_string()),
                }
            }
            let base = parts.join("/");
            for ext in ["", ".ts", ".tsx", ".js", ".jsx", ".mjs"] {
                let cand = format!("{base}{ext}");
                if paths.contains(&cand) { return Some(cand); }
            }
            for idx in ["/index.ts", "/index.tsx", "/index.js", "/index.jsx"] {
                let cand = format!("{base}{idx}");
                if paths.contains(&cand) { return Some(cand); }
            }
            None
        };
        let mut edges: Vec<GraphEdge> = Vec::new();
        let mut deg: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut seen_edge: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
        for (path, text) in &files {
            let dir = path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
            let lang = lang_of(path).unwrap_or("");
            for line in text.lines() {
                let t = line.trim();
                let mut targets: Vec<String> = Vec::new();
                if lang == "ts" || lang == "js" {
                    // from "X" / require("X") / import "X"
                    for marker in ["from \"", "from '", "require(\"", "require('", "import \"", "import '"] {
                        if let Some(i) = t.find(marker) {
                            let rest = &t[i + marker.len()..];
                            let end = rest.find(['"', '\'']).unwrap_or(rest.len());
                            let spec = &rest[..end];
                            if spec.starts_with('.') { if let Some(r) = resolve_rel(&dir, spec) { targets.push(r); } }
                        }
                    }
                } else if lang == "rust" {
                    // mod x;  → sibling x.rs or x/mod.rs
                    if let Some(rest) = t.strip_prefix("mod ").or_else(|| t.strip_prefix("pub mod ")) {
                        if let Some(name) = rest.split(&[';', ' '][..]).next().filter(|s| !s.is_empty()) {
                            for cand in [format!("{dir}/{name}.rs"), format!("{dir}/{name}/mod.rs")] {
                                let c = cand.trim_start_matches('/').to_string();
                                if paths.contains(&c) { targets.push(c); }
                            }
                        }
                    }
                }
                for to in targets {
                    if &to != path && seen_edge.insert((path.clone(), to.clone())) {
                        *deg.entry(path.clone()).or_default() += 1;
                        *deg.entry(to.clone()).or_default() += 1;
                        edges.push(GraphEdge { from: path.clone(), to });
                    }
                }
            }
        }
        let mut nodes: Vec<GraphNode> = files
            .iter()
            .map(|(p, txt)| GraphNode {
                path: p.clone(),
                dir: p.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default(),
                lang: lang_of(p).unwrap_or("").to_string(),
                size: txt.len() as u64,
                deg: deg.get(p).copied().unwrap_or(0),
            })
            .collect();
        nodes.sort_by(|a, b| a.path.cmp(&b.path));
        (nodes, edges)
    }
}

/// One entry in a directory listing for the file browser.
#[derive(serde::Serialize)]
pub struct TreeItem {
    pub name: String,
    pub path: String,
    pub dir: bool,
    pub size: u64,
}

/// A search hit: a filename match (`kind:"path"`, line 0) or a content-line match (`kind:"content"`).
#[derive(serde::Serialize)]
pub struct SearchHit {
    pub path: String,
    pub line: u32,
    pub text: String,
    pub kind: &'static str,
}

impl RepoHost {
    /// Branch names that resolve to a keel change, `main` first then alphabetical. The keel store
    /// keys refs by bare branch name (`main`, `feat/x`), so this is just the ref table filtered.
    pub fn branches(&self, tenant: &str, repo: &str) -> Vec<String> {
        let Some(store) = self.store(tenant, repo, false).ok().flatten() else { return vec![] };
        let mut names: Vec<String> = store
            .list_refs()
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, id)| matches!(store.get(id), Ok(Some(Object::Change(_)))))
            .map(|(n, _)| n)
            .collect();
        names.sort();
        names.dedup();
        if let Some(pos) = names.iter().position(|n| n == "main") {
            let m = names.remove(pos);
            names.insert(0, m);
        }
        names
    }

    /// The root tree of a branch head (`None` if the repo/ref is missing or not a change).
    fn root_tree(&self, store: &Store, ref_name: &str) -> Option<ObjectId> {
        let head = store.get_ref(ref_name).ok()??;
        match store.get(&head).ok()?? {
            Object::Change(c) => Some(c.tree),
            _ => None,
        }
    }

    /// List a directory (`path`, "" = root) at a branch. Directories first, then files, both by name.
    /// `None` if the repo/ref/path is missing or the path isn't a directory.
    pub fn list_tree(&self, tenant: &str, repo: &str, ref_name: &str, path: &str) -> Option<Vec<TreeItem>> {
        let store = self.store(tenant, repo, false).ok()??;
        let root = self.root_tree(&store, ref_name)?;
        let base = path.trim_matches('/');
        let tree_id = if base.is_empty() { root } else { resolve_path_in_tree(&store, root, base)? };
        let entries = match store.get(&tree_id).ok()?? {
            Object::Tree(t) => t.entries,
            _ => return None,
        };
        let mut out: Vec<TreeItem> = entries
            .into_iter()
            .map(|e| {
                let (dir, size) = match store.get(&e.id) {
                    Ok(Some(Object::Tree(_))) => (true, 0),
                    Ok(Some(Object::Blob(b))) => (false, b.len() as u64),
                    _ => (e.mode == 0o040000, 0),
                };
                let full = if base.is_empty() { e.name.clone() } else { format!("{base}/{}", e.name) };
                TreeItem { name: e.name, path: full, dir, size }
            })
            .collect();
        out.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
        Some(out)
    }

    /// Every file path in a branch's tree (recursive), for a full file-tree view. Capped.
    pub fn all_paths(&self, tenant: &str, repo: &str, ref_name: &str) -> Vec<String> {
        let Some(store) = self.store(tenant, repo, false).ok().flatten() else { return vec![] };
        let Some(root) = self.root_tree(&store, ref_name) else { return vec![] };
        let mut out = Vec::new();
        let mut stack = vec![(String::new(), root)];
        while let Some((prefix, tid)) = stack.pop() {
            if out.len() > 50_000 {
                break;
            }
            let entries = match store.get(&tid) { Ok(Some(Object::Tree(t))) => t.entries, _ => continue };
            for e in entries {
                let full = if prefix.is_empty() { e.name.clone() } else { format!("{prefix}/{}", e.name) };
                match store.get(&e.id) {
                    Ok(Some(Object::Tree(_))) => stack.push((full, e.id)),
                    Ok(Some(Object::Blob(_))) => out.push(full),
                    _ => {}
                }
            }
        }
        out.sort();
        out
    }

    /// Read a file's bytes at a specific branch (`None` if repo/ref/path missing or not a blob).
    pub fn read_file_at(&self, tenant: &str, repo: &str, ref_name: &str, path: &str) -> Option<Vec<u8>> {
        let store = self.store(tenant, repo, false).ok()??;
        let root = self.root_tree(&store, ref_name)?;
        let blob = resolve_path_in_tree(&store, root, path)?;
        match store.get(&blob).ok()?? {
            Object::Blob(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// Fuzzy filename + full-text content search across a branch's tree. Filename hits first, then
    /// content-line hits; capped so a huge repo can't run away. (Semantic/vector search is a
    /// follow-up — this is the fuzzy/full-text tier.)
    pub fn search(&self, tenant: &str, repo: &str, ref_name: &str, query: &str) -> Vec<SearchHit> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return vec![];
        }
        let Some(store) = self.store(tenant, repo, false).ok().flatten() else { return vec![] };
        let Some(root) = self.root_tree(&store, ref_name) else { return vec![] };
        // Walk the whole tree, collecting (path, blob id).
        let mut files: Vec<(String, ObjectId)> = Vec::new();
        let mut stack = vec![(String::new(), root)];
        while let Some((prefix, tid)) = stack.pop() {
            if files.len() > 50_000 {
                break;
            }
            let entries = match store.get(&tid) {
                Ok(Some(Object::Tree(t))) => t.entries,
                _ => continue,
            };
            for e in entries {
                let full = if prefix.is_empty() { e.name.clone() } else { format!("{prefix}/{}", e.name) };
                match store.get(&e.id) {
                    Ok(Some(Object::Tree(_))) => stack.push((full, e.id)),
                    Ok(Some(Object::Blob(_))) => files.push((full, e.id)),
                    _ => {}
                }
            }
        }
        let mut hits: Vec<SearchHit> = Vec::new();
        for (path, _) in &files {
            if path.to_lowercase().contains(&q) {
                hits.push(SearchHit { path: path.clone(), line: 0, text: String::new(), kind: "path" });
            }
        }
        for (path, id) in &files {
            if hits.len() > 300 {
                break;
            }
            let bytes = match store.get(id) {
                Ok(Some(Object::Blob(b))) => b,
                _ => continue,
            };
            if bytes.len() > 512 * 1024 || bytes.iter().take(8000).any(|&b| b == 0) {
                continue; // skip huge or binary files
            }
            let text = String::from_utf8_lossy(&bytes);
            for (i, line) in text.lines().enumerate() {
                if line.to_lowercase().contains(&q) {
                    hits.push(SearchHit {
                        path: path.clone(),
                        line: (i + 1) as u32,
                        text: line.trim().chars().take(200).collect(),
                        kind: "content",
                    });
                    if hits.len() > 300 {
                        break;
                    }
                }
            }
        }
        hits
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
    /// The file changed but is past [`MAX_BLOB_FOR_DIFF`], so no line hunks were computed. Lets the
    /// UI say "too large to diff" instead of silently rendering an empty ("0 lines") diff.
    #[serde(default)]
    pub too_large: bool,
}

const MAX_DIFF_FILES: usize = 40;
const MAX_BLOB_FOR_DIFF: usize = 1024 * 1024; // skip only genuinely huge/binary blobs (line-diffing 1MB of text is cheap)

/// A file relocated with byte-identical content — a **pure move**, detected exactly by content
/// address (the blob id is unchanged), not guessed by similarity like git.
#[derive(serde::Serialize)]
pub struct Move {
    pub from: String,
    pub to: String,
    /// The shared blob content-address that proves the two paths are the same bytes.
    pub blob: String,
}

/// A content-addressed classification of a change (B1 + B4): which files were purely moved,
/// reformatted (whitespace-only), or really changed. `pure_move` = every change is a move (the
/// strongest behavior-preserving case). `mechanical` = the whole change is moves and/or
/// whitespace-only edits with no added/deleted/behavioral content — low-risk, though only
/// `pure_move` is *provably* behavior-preserving (whitespace can be semantic, e.g. Python).
#[derive(serde::Serialize)]
pub struct SemanticSummary {
    pub moves: Vec<Move>,
    pub added: Vec<String>,
    pub deleted: Vec<String>,
    /// All content-differing files (the union; `whitespace_only` + `behavioral` partition it).
    pub modified: Vec<String>,
    /// Modified files whose only change is whitespace (reindent, trailing space, blank lines).
    pub whitespace_only: Vec<String>,
    /// Modified files with a real (non-whitespace) content change — the behavioral set.
    pub behavioral: Vec<String>,
    pub pure_move: bool,
    pub mechanical: bool,
}

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

/// Like [`flatten_tree`] but carries each path's **mode** (`MODE_FILE`/`MODE_EXEC`/`MODE_SYMLINK`)
/// alongside the blob id. The 3-way merge uses this so a merged executable keeps its `+x` bit and a
/// merged symlink is re-created as a symlink — a plain blob-id flatten drops the mode and silently
/// materializes both as ordinary files ("green but wrong").
fn flatten_tree_modes(store: &Store, tree: ObjectId, prefix: &str, out: &mut HashMap<String, (ObjectId, u32)>, depth: u32) {
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
            flatten_tree_modes(store, e.id, &path, out, depth + 1);
        } else {
            out.insert(path, (e.id, e.mode));
        }
    }
}

/// Write a merged tree entry to disk honoring its git mode: a `MODE_SYMLINK` blob is materialized as
/// a real symlink whose target IS the blob content (git's symlink encoding); a `MODE_EXEC` blob is
/// written and `chmod +x`'d; everything else is a plain file. Matches how `keel_store::snapshot`
/// checks out and re-reads modes, so the subsequent `snapshot_uncached` captures the mode exactly.
fn write_entry(fp: &std::path::Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    use keel_store::snapshot::{MODE_EXEC, MODE_SYMLINK};
    if mode == MODE_SYMLINK {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            return std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(bytes), fp);
        }
        #[cfg(not(unix))]
        {
            return std::fs::write(fp, bytes); // no cheap symlinks — fall back to the target as content
        }
    }
    std::fs::write(fp, bytes)?;
    if mode == MODE_EXEC {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(fp)?.permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(fp, perm)?;
        }
    }
    Ok(())
}

/// Reject a merged-edit `path` whose *ancestor* directories, as materialized under the checkout `dir`,
/// include a symlink. The checkout-then-patch design (`materialize_merge`, `compose_independence_tree`)
/// writes each edit via `dir.join(path)`; keel's `checkout` already rejects `..`/absolute entry names,
/// so the only residual way a write could land OUTSIDE `dir` is a symlinked path *prefix* — either
/// pre-existing in the base tree, or created by an earlier symlink edit in the same merge. We walk each
/// ancestor component under `dir` and return the offending path if any existing one is a symlink; a
/// component that doesn't exist yet is fine (`create_dir_all` only ever makes real directories).
fn symlinked_prefix(dir: &std::path::Path, path: &str) -> Option<std::path::PathBuf> {
    let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    let mut cur = dir.to_path_buf();
    // Only ancestor components (all but the leaf) can be a *traversed* prefix; the leaf itself is
    // `remove_file`'d before the write, so a leaf symlink is replaced, not followed.
    for comp in &comps[..comps.len().saturating_sub(1)] {
        cur.push(comp);
        match std::fs::symlink_metadata(&cur) {
            Ok(md) if md.file_type().is_symlink() => return Some(cur),
            _ => {} // real dir, or not present yet
        }
    }
    None
}

/// Committer stamped on a synthesized git merge commit — the land was performed by Hull's merge
/// queue, not authored by a person. Stable so a re-export is deterministic.
const HULL_COMMITTER: &str = "Hull Merge Queue <merge@hull.local>";

/// Invert keel's mode → the git tree-entry mode bytes. Note keel folds a git symlink into file
/// content on ingest ([`keel_git::bridge`]), so a symlink that arrived via `git push` is `MODE_FILE`
/// here and re-exports as `100644` (a v1 fidelity caveat); a NATIVE keel symlink is `MODE_SYMLINK`
/// and re-exports faithfully as `120000`. `MODE_DIR` is handled by the recursive caller, not here.
fn git_mode_bytes(mode: u32) -> Vec<u8> {
    use keel_store::snapshot::{MODE_EXEC, MODE_SYMLINK};
    if mode == MODE_EXEC {
        b"100755".to_vec()
    } else if mode == MODE_SYMLINK {
        b"120000".to_vec()
    } else {
        b"100644".to_vec()
    }
}

/// Build a git identity (`Name <email>`) for a commit header from a keel change author. If the author
/// already looks like a git identity (`Name <email>`), use it verbatim; otherwise synthesize a stable
/// placeholder address so the header is well-formed.
fn git_identity(author: &str) -> String {
    let a = author.trim();
    if a.contains('<') && a.contains('>') {
        a.to_string()
    } else if a.is_empty() {
        "Hull <noreply@hull.local>".to_string()
    } else {
        format!("{a} <{}@hull.local>", a.replace(char::is_whitespace, "-"))
    }
}

impl RepoHost {
    /// The blob content of change `hex`'s OWN tree (`path` → bytes), read straight from the change —
    /// NOT from any branch ref — so it always reflects that exact change regardless of where `main`
    /// points or whether the branch was advanced. Sorted by path, capped at `max_files` (files beyond
    /// the cap are dropped and the `bool` returns `true`) and skipping any blob over `max_bytes`. For
    /// mirroring a landed change's content to an external blob store.
    pub fn change_blobs(&self, tenant: &str, repo: &str, hex: &str, max_files: usize, max_bytes: usize) -> (Vec<(String, Vec<u8>)>, bool) {
        let Ok(Some(store)) = self.store(tenant, repo, false) else { return (Vec::new(), false) };
        let Some(cid) = ObjectId::from_hex(hex) else { return (Vec::new(), false) };
        let tree = match store.get(&cid).ok().flatten() {
            Some(Object::Change(c)) => c.tree,
            _ => return (Vec::new(), false),
        };
        let mut map = HashMap::new();
        flatten_tree(&store, tree, "", &mut map, 0);
        let over_cap = map.len() > max_files;
        let mut entries: Vec<(String, ObjectId)> = map.into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out = Vec::new();
        for (path, blob) in entries.into_iter().take(max_files) {
            if let Ok(Some(Object::Blob(bytes))) = store.get(&blob) {
                if bytes.len() <= max_bytes {
                    out.push((path, bytes));
                }
            }
        }
        (out, over_cap)
    }

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
    /// The full old + new text of one file at a change — powers the diff viewer's "expand unmodified
    /// lines" (pierre's `loadDiffFiles`), which needs the whole file, not just the patch context.
    /// `None` text = the file is absent on that side (a pure add or delete). Returns `None` if the
    /// change/file can't be resolved or the blob is over the diff cap.
    pub fn file_pair(&self, tenant: &str, repo: &str, hex: &str, path: &str) -> Option<(Option<String>, Option<String>)> {
        let store = self.store(tenant, repo, false).ok().flatten()?;
        let cid = ObjectId::from_hex(hex)?;
        let Some(Object::Change(change)) = store.get(&cid).ok().flatten() else { return None };
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
        let old = parent.get(path).and_then(read);
        let new = head.get(path).and_then(read);
        if old.is_none() && new.is_none() { return None; }
        Some((old, new))
    }

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
        // Read a blob as text; `too_large` distinguishes "over the diff cap" from "missing/non-blob".
        let read = |id: &ObjectId| -> (Option<String>, bool) {
            match store.get(id).ok().flatten() {
                Some(Object::Blob(b)) if b.len() <= MAX_BLOB_FOR_DIFF => (Some(String::from_utf8_lossy(&b).into_owned()), false),
                Some(Object::Blob(_)) => (None, true),
                _ => (None, false),
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
            let (old_txt, old_big) = p.map(&read).unwrap_or((None, false));
            let (new_txt, new_big) = h.map(&read).unwrap_or((None, false));
            // A changed file past the cap: report it as changed, but flag it instead of diffing.
            if old_big || new_big {
                out.push(FileDiff { path: path.clone(), status: status.to_string(), ops: Vec::new(), hunks: Vec::new(), too_large: true });
                if out.len() >= MAX_DIFF_FILES {
                    break;
                }
                continue;
            }
            let (old, new) = (old_txt.unwrap_or_default(), new_txt.unwrap_or_default());
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
            out.push(FileDiff { path: path.clone(), status: status.to_string(), ops, hunks: hunks_out, too_large: false });
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

    /// The keel **tree id** (content address) of a change — what makes a CI result memoizable: the
    /// same tree always yields the same verdict.
    pub fn change_tree(&self, tenant: &str, repo: &str, hex: &str) -> Option<String> {
        let store = self.store(tenant, repo, false).ok()??;
        let cid = ObjectId::from_hex(hex)?;
        match store.get(&cid).ok()?? {
            Object::Change(c) => Some(c.tree.to_hex()),
            _ => None,
        }
    }

    /// Materialize a change's tree onto `dir` (a fresh checkout to run checks against).
    pub fn checkout_change(&self, tenant: &str, repo: &str, hex: &str, dir: &std::path::Path) -> bool {
        let Ok(Some(store)) = self.store(tenant, repo, false) else { return false };
        let Some(cid) = ObjectId::from_hex(hex) else { return false };
        let tree = match store.get(&cid) {
            Ok(Some(Object::Change(c))) => c.tree,
            _ => return false,
        };
        keel_store::snapshot::checkout(&store, tree, dir).is_ok()
    }

    /// Materialize a **tree** (by its content-address `tree_id`) onto `dir` — the keel-native way a
    /// runner obtains source: addressed by content, not a git ref.
    pub fn checkout_tree(&self, tenant: &str, repo: &str, tree_hex: &str, dir: &std::path::Path) -> bool {
        let Ok(Some(store)) = self.store(tenant, repo, false) else { return false };
        let Some(tid) = ObjectId::from_hex(tree_hex) else { return false };
        keel_store::snapshot::checkout(&store, tid, dir).is_ok()
    }

    /// Apply search/replace `edits` to a change's tree and commit the result as a **new keel change**
    /// parented on it — how an AI fix becomes real code. Returns the new change id, or `None` if any
    /// edit's `search` isn't found verbatim (the fix doesn't apply cleanly — we never write a partial
    /// or guessed patch). A ref keeps the new change reachable so GC can't sweep it.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_fix(
        &self,
        tenant: &str,
        repo: &str,
        change_hex: &str,
        edits: &[(String, String, String)],
        intent: &str,
        author: &str,
        timestamp: u64,
    ) -> Option<String> {
        let store = self.store(tenant, repo, false).ok()??;
        let cid = ObjectId::from_hex(change_hex)?;
        let tree = match store.get(&cid).ok()?? {
            Object::Change(c) => c.tree,
            _ => return None,
        };
        let seq = SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hull-fix-{}-{}-{seq}", &change_hex[..change_hex.len().min(12)], std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        if keel_store::snapshot::checkout(&store, tree, &dir).is_err() {
            return None;
        }
        // Apply each edit; abort (clean up, return None) if any search string isn't present.
        for (path, search, replace) in edits {
            // The edit `path` comes from the fixer (LLM output), so — unlike the keel-derived paths in
            // `materialize_merge`/`compose_independence_tree` — it is untrusted. Guard the checkout
            // boundary: reject an absolute path (`join` would replace `dir`), any `..`/root traversal,
            // and an empty `search` (which `replacen` would blindly prepend to the file). Then, like
            // the siblings, refuse to write through a symlinked path prefix that could escape `dir`.
            let rel = std::path::Path::new(path.as_str());
            let unsafe_path = search.is_empty()
                || rel.is_absolute()
                || rel.components().any(|c| !matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir))
                || symlinked_prefix(&dir, path).is_some();
            if unsafe_path {
                let _ = std::fs::remove_dir_all(&dir);
                return None;
            }
            let fp = dir.join(path);
            let Ok(content) = std::fs::read_to_string(&fp) else {
                let _ = std::fs::remove_dir_all(&dir);
                return None;
            };
            if !content.contains(search.as_str()) {
                let _ = std::fs::remove_dir_all(&dir);
                return None;
            }
            let updated = content.replacen(search.as_str(), replace.as_str(), 1);
            if std::fs::write(&fp, updated).is_err() {
                let _ = std::fs::remove_dir_all(&dir);
                return None;
            }
        }
        let new_tree = keel_store::snapshot::snapshot_uncached(&store, &dir).ok();
        let _ = std::fs::remove_dir_all(&dir);
        let new_tree = new_tree?;
        if new_tree == tree {
            return None; // no-op edit
        }
        let change = keel_store::Change {
            parents: vec![cid],
            tree: new_tree,
            session: None,
            intent: intent.to_string(),
            author: author.to_string(),
            timestamp,
            verification: keel_store::Verification::Unverified,
        };
        let id = store.put(&Object::Change(change)).ok()?;
        // Keep it reachable (unreferenced changes can be GC'd).
        let _ = store.set_ref(&format!("hull/fix/{}", id.to_hex()), &id);
        Some(id.to_hex())
    }

    /// Content-addressed semantic summary of a change (B1): which files were **purely moved** (same
    /// blob id, new path) vs genuinely added/deleted/modified. Because keel addresses blobs by
    /// content, a move is detected **exactly** — a removed path's blob id reappearing at an added
    /// path is provably the same bytes — where git can only guess by similarity. A change whose every
    /// file-level change is a move is `pure_move`: mechanical and behavior-preserving.
    pub fn semantic_summary(&self, tenant: &str, repo: &str, hex: &str) -> SemanticSummary {
        let empty = || SemanticSummary { moves: vec![], added: vec![], deleted: vec![], modified: vec![], whitespace_only: vec![], behavioral: vec![], pure_move: false, mechanical: false };
        let Ok(Some(store)) = self.store(tenant, repo, false) else { return empty() };
        let Some(cid) = ObjectId::from_hex(hex) else { return empty() };
        let change = match store.get(&cid).ok().flatten() {
            Some(Object::Change(c)) => c,
            _ => return empty(),
        };
        let mut head = HashMap::new();
        flatten_tree(&store, change.tree, "", &mut head, 0);
        let mut parent = HashMap::new();
        if let Some(p) = change.parents.first() {
            if let Some(Object::Change(pc)) = store.get(p).ok().flatten() {
                flatten_tree(&store, pc.tree, "", &mut parent, 0);
            }
        }
        // Partition by path: added (new only), deleted (parent only), modified (both, blob differs).
        let mut added: Vec<(String, ObjectId)> = head.iter().filter(|(p, _)| !parent.contains_key(*p)).map(|(p, id)| (p.clone(), *id)).collect();
        let mut deleted: Vec<(String, ObjectId)> = parent.iter().filter(|(p, _)| !head.contains_key(*p)).map(|(p, id)| (p.clone(), *id)).collect();
        let mut modified: Vec<String> = head.iter().filter(|(p, id)| parent.get(*p).is_some_and(|pid| pid != *id)).map(|(p, _)| p.clone()).collect();
        added.sort();
        deleted.sort();
        modified.sort();
        // Pair identical blobs across deleted↔added: same content-address ⇒ a pure move. Greedy per
        // blob so duplicated content still pairs one-to-one.
        let mut moves = Vec::new();
        let mut used_del = vec![false; deleted.len()];
        let mut leftover_added = Vec::new();
        for (to, aid) in added.into_iter() {
            if let Some(k) = deleted.iter().enumerate().find(|(i, (_, did))| !used_del[*i] && *did == aid).map(|(i, _)| i) {
                used_del[k] = true;
                moves.push(Move { from: deleted[k].0.clone(), to, blob: aid.to_hex() });
            } else {
                leftover_added.push(to);
            }
        }
        let leftover_deleted: Vec<String> = deleted.iter().enumerate().filter(|(i, _)| !used_del[*i]).map(|(_, (p, _))| p.clone()).collect();
        moves.sort_by(|a, b| a.to.cmp(&b.to));
        leftover_added.sort();
        // B4 — split modified files into whitespace-only (reformat) vs behavioral (real content). Two
        // blobs whose contents match once whitespace is normalized differ only in formatting.
        let read = |id: &ObjectId| -> Option<String> {
            match store.get(id).ok().flatten() {
                Some(Object::Blob(b)) if b.len() <= MAX_BLOB_FOR_DIFF => Some(String::from_utf8_lossy(&b).into_owned()),
                _ => None,
            }
        };
        // Whitespace-insensitive comparison: drop every whitespace char (catches reindent, blank-line
        // and around-symbol spacing changes from a formatter). A hint only — never an auto-approve
        // signal (whitespace can be semantic, e.g. Python), so the rare merge like `a b`→`ab` is safe.
        let normalized = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        let mut whitespace_only = Vec::new();
        for p in &modified {
            if let (Some(o), Some(n)) = (parent.get(p).and_then(&read), head.get(p).and_then(&read)) {
                if normalized(&o) == normalized(&n) {
                    whitespace_only.push(p.clone());
                }
            }
        }
        whitespace_only.sort();
        let behavioral: Vec<String> = modified.iter().filter(|p| !whitespace_only.contains(*p)).cloned().collect();
        // Pure move: at least one move, and nothing else changed at all.
        let pure_move = !moves.is_empty() && leftover_added.is_empty() && leftover_deleted.is_empty() && modified.is_empty();
        // Mechanical: only moves and/or whitespace edits — no added/deleted/behavioral content.
        let mechanical = leftover_added.is_empty() && leftover_deleted.is_empty() && behavioral.is_empty() && (!moves.is_empty() || !whitespace_only.is_empty());
        SemanticSummary { moves, added: leftover_added, deleted: leftover_deleted, modified, whitespace_only, behavioral, pure_move, mechanical }
    }

    /// The observable facts of a change — touched files, semantic operations, verification, and
    /// secret findings — the ground truth a [`reconcile`](hull_core::reconcile) run judges claims
    /// against.
    pub fn facts(&self, tenant: &str, repo: &str, hex: &str) -> hull_core::reconcile::ChangeFacts {
        let diff = self.diff(tenant, repo, hex);
        let files = diff.iter().map(|f| f.path.clone()).collect();
        let ops = diff.iter().flat_map(|f| f.ops.iter().cloned()).collect();
        // The literal added lines (lower-cased), so a claim can be corroborated by the code the diff
        // introduced even when no named op captures it. Bounded so a huge diff can't blow memory.
        let mut added_text = String::new();
        'outer: for f in &diff {
            for h in &f.hunks {
                for line in &h.lines {
                    if line.tag == "add" {
                        added_text.push_str(&line.text.to_lowercase());
                        added_text.push('\n');
                        if added_text.len() > 256 * 1024 {
                            break 'outer;
                        }
                    }
                }
            }
        }
        let verification = self.verification(tenant, repo, hex).unwrap_or_else(|| "unverified".into());
        let key = format!("{tenant}/{repo}");
        let secrets = self
            .secrets(&key)
            .into_iter()
            .filter(|s| s.change.is_empty() || s.change == hex)
            .map(|s| s.title)
            .collect();
        // Does the change add its own tests? (The coarse fallback when independence isn't computed:
        // green then reads as self-attested, not mechanical.) `independent_verification` is filled in
        // at the App layer, which can run the independence check; `facts()` stays cheap and I/O-light.
        let adds_tests = diff.iter().any(|f| f.status != "deleted" && hull_core::reconcile::is_test_path(&f.path));
        hull_core::reconcile::ChangeFacts { files, ops, verification, secrets, added_text, adds_tests, independent_verification: None }
    }

    /// The test files a change **added or modified** (deletions included — dropping a test is also a
    /// way to make a suite pass). Cheap: diff-only, no checkout. Empty ⇒ the whole suite is
    /// pre-existing, so the change can't have tampered with what verifies it.
    pub fn changed_test_files(&self, tenant: &str, repo: &str, hex: &str) -> Vec<String> {
        self.diff(tenant, repo, hex)
            .into_iter()
            .filter(|f| hull_core::reconcile::is_test_path(&f.path))
            .map(|f| (f.path, f.status))
            .map(|(p, _)| p)
            .collect()
    }

    /// Compose the **independence tree** for a change: its new code, but with every test file it
    /// touched *restored to the parent's version* (or dropped if the change newly added it). Running
    /// checks on this tree answers "does the change pass the tests it did **not** author?" — it can't
    /// approve itself by adding or weakening a test. Returns the composed tree id, or `None` if the
    /// change touched no tests (nothing to neutralize) or has no parent to restore from.
    pub fn compose_independence_tree(&self, tenant: &str, repo: &str, hex: &str) -> Option<String> {
        let changed_tests = self.changed_test_files(tenant, repo, hex);
        if changed_tests.is_empty() {
            return None;
        }
        let store = self.store(tenant, repo, false).ok()??;
        let cid = ObjectId::from_hex(hex)?;
        let change = match store.get(&cid).ok()?? {
            Object::Change(c) => c,
            _ => return None,
        };
        let parent = *change.parents.first()?; // need a baseline to restore pre-existing tests from
        let parent_tree = match store.get(&parent).ok()?? {
            Object::Change(c) => c.tree,
            _ => return None,
        };

        let seq = SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("hull-indep-{}-{}-{seq}", &hex[..hex.len().min(12)], std::process::id()));
        let newdir = base.join("new");
        let pdir = base.join("parent");
        let _ = std::fs::remove_dir_all(&base);
        if keel_store::snapshot::checkout(&store, change.tree, &newdir).is_err()
            || keel_store::snapshot::checkout(&store, parent_tree, &pdir).is_err()
        {
            let _ = std::fs::remove_dir_all(&base);
            return None;
        }
        // Neutralize each touched test: restore the parent's copy, or drop it if the change added it.
        for path in &changed_tests {
            // Same checkout-then-patch escape as `materialize_merge`: never copy/delete through a
            // symlinked path prefix. Best-effort compose — skip such a path rather than traverse it.
            if symlinked_prefix(&newdir, path).is_some() {
                continue;
            }
            let target = newdir.join(path);
            let parent_copy = pdir.join(path);
            if parent_copy.is_file() {
                let _ = std::fs::create_dir_all(target.parent().unwrap_or(&newdir));
                let _ = std::fs::copy(&parent_copy, &target); // restore pre-change (unweakened) test
            } else {
                let _ = std::fs::remove_file(&target); // change newly added this test → drop it
            }
        }
        let composed = keel_store::snapshot::snapshot_uncached(&store, &newdir).ok();
        let _ = std::fs::remove_dir_all(&base);
        let composed = composed?;
        // Keep the composed tree reachable so it survives GC between compose and CI run.
        let _ = store.set_ref(&format!("hull/indep/{}", composed.to_hex()), &composed);
        Some(composed.to_hex())
    }
}

// ── protected-branch merge queue ─────────────────────────────────────────────────────────────────
//
// When a repo's default branch is protected (`require_review_to_land`), a PR lands not by flipping
// metadata but by *synthesizing* the merge server-side (fast-forward or a real 3-way), speculatively
// re-verifying the merged tree, and advancing the branch by compare-and-swap. These helpers do the
// tree math; the App layer (`perform_merge`) drives verify + retry around them.

/// The plan for landing a PR onto a protected branch, produced by [`RepoHost::plan_merge`].
pub(crate) enum MergeOutcome {
    /// The PR's head is already contained in the branch — nothing to land (idempotent success).
    AlreadyMerged,
    /// A merged tree id, ready to speculatively verify and land. A fast-forward yields the head's own
    /// tree; a divergent land yields a synthesized 3-way tree. Pinned reachable (`hull/merge/<tree>`)
    /// so GC can't sweep it during the verify + advance window.
    ///
    /// `fast_forward` records the genuine ANCESTRY outcome (`base ∈ ancestors(head)`), not a
    /// tree-equality guess: a no-op/empty base (revert-of-revert) can make the synthesized 3-way tree
    /// equal the head's tree while the land is still divergent. The git-mirror export must take the
    /// FF shortcut only when this is `true`, or it would do a non-fast-forward ref move (CRITICAL-1).
    Tree { tree: String, fast_forward: bool },
}

/// Why a protected-branch land could not proceed.
#[derive(Debug)]
pub(crate) enum MergeError {
    /// A real content conflict — both sides changed the same path incompatibly (incl. add/add and
    /// delete/modify). v1 does no line-level auto-merge, so this **rejects** the land. User-facing.
    Conflict(String),
    /// A store/IO failure while synthesizing or committing the merge.
    Internal(String),
}

impl RepoHost {
    /// The tree id of a change, or `None` if it isn't a change / can't be read.
    fn change_tree_id(store: &Store, change: ObjectId) -> Option<ObjectId> {
        match store.get(&change).ok()?? {
            Object::Change(c) => Some(c.tree),
            _ => None,
        }
    }

    /// All ancestors of `id` (including `id`) over every parent edge, nearest-first (BFS). Used for
    /// fast-forward detection and picking a near merge-base.
    fn ancestors_bfs(store: &Store, id: ObjectId) -> Vec<ObjectId> {
        let mut seen = std::collections::HashSet::new();
        let mut order = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        seen.insert(id);
        queue.push_back(id);
        while let Some(x) = queue.pop_front() {
            order.push(x);
            if let Some(Object::Change(c)) = store.get(&x).ok().flatten() {
                for p in c.parents {
                    if seen.insert(p) {
                        queue.push_back(p);
                    }
                }
            }
        }
        order
    }

    /// The **head** change of a PR — the tip among its proposed `changes` (the one whose ancestry
    /// contains all the others). A single-change PR is trivially that change; diverging changes with no
    /// single tip fall back to the last proposed change. This is the change landed onto the branch.
    pub(crate) fn pr_head(&self, tenant: &str, repo: &str, changes: &[String]) -> Option<String> {
        match changes {
            [] => None,
            [only] => Some(only.clone()),
            _ => {
                let store = self.store(tenant, repo, false).ok()??;
                let ids: Vec<ObjectId> = changes.iter().filter_map(|h| ObjectId::from_hex(h)).collect();
                for &cand in &ids {
                    let anc: std::collections::HashSet<ObjectId> = Self::ancestors_bfs(&store, cand).into_iter().collect();
                    if ids.iter().all(|o| anc.contains(o)) {
                        return Some(cand.to_hex());
                    }
                }
                changes.last().cloned()
            }
        }
    }

    /// Pin a tree reachable (so a concurrent GC can't sweep it between synthesis and land) and wrap it.
    /// `fast_forward` is the genuine ancestry outcome, threaded to the exporter (see [`MergeOutcome`]).
    fn pin_merge_tree(store: &Store, tree: ObjectId, fast_forward: bool) -> MergeOutcome {
        let _ = store.set_ref(&format!("hull/merge/{}", tree.to_hex()), &tree);
        MergeOutcome::Tree { tree: tree.to_hex(), fast_forward }
    }

    /// Plan the land of `head_hex` onto the protected branch `branch` whose current tip is `base_hex`
    /// (`None` = an unborn branch — the first land is a trivial fast-forward). Fast-forward when the
    /// base is an ancestor of the head; already-contained when the head is an ancestor of the base;
    /// otherwise a per-path 3-way merge against a near common ancestor. Returns the merged tree id
    /// (pinned reachable) to verify + land, or a conflict/internal error. NO line-level merge: any
    /// path both sides changed incompatibly is a hard conflict.
    pub(crate) fn plan_merge(&self, tenant: &str, repo: &str, branch: &str, base_hex: Option<&str>, head_hex: &str) -> Result<MergeOutcome, MergeError> {
        let store = self
            .store(tenant, repo, false)
            .map_err(|e| MergeError::Internal(e.to_string()))?
            .ok_or_else(|| MergeError::Internal("no such repo".into()))?;
        let head = ObjectId::from_hex(head_hex).ok_or_else(|| MergeError::Internal("bad head change id".into()))?;
        let head_tree = Self::change_tree_id(&store, head).ok_or_else(|| MergeError::Internal("head change not found".into()))?;
        // Unborn branch: the first land is a trivial fast-forward to the head's tree.
        let Some(base_hex) = base_hex else {
            return Ok(Self::pin_merge_tree(&store, head_tree, true));
        };
        let base = ObjectId::from_hex(base_hex).ok_or_else(|| MergeError::Internal("bad base change id".into()))?;
        if base == head {
            return Ok(MergeOutcome::AlreadyMerged);
        }
        let base_anc: std::collections::HashSet<ObjectId> = Self::ancestors_bfs(&store, base).into_iter().collect();
        if base_anc.contains(&head) {
            return Ok(MergeOutcome::AlreadyMerged); // head already in the branch's history
        }
        let head_anc = Self::ancestors_bfs(&store, head);
        if head_anc.contains(&base) {
            // base ∈ ancestors(head) ⇒ fast-forward: the merged tree IS the head's tree, no synthesis.
            return Ok(Self::pin_merge_tree(&store, head_tree, true));
        }
        // 3-way merge. Pick the merge-base as a true LCA, not just the nearest common ancestor: in a
        // criss-cross / multi-base history there can be several *incomparable* common ancestors (none an
        // ancestor of another). Silently picking one produces a clean-but-wrong merge, so we detect that
        // and REJECT — a single unambiguous base (the common diamond) still auto-merges.
        let common: Vec<ObjectId> = head_anc.iter().copied().filter(|a| base_anc.contains(a)).collect();
        if common.is_empty() {
            return Err(MergeError::Internal("no common ancestor for 3-way merge".into()));
        }
        // Reduce to the *maximal* common ancestors (the merge bases): drop any common ancestor that is a
        // proper ancestor of another common ancestor. What's left are pairwise-incomparable LCAs.
        let common_set: std::collections::HashSet<ObjectId> = common.iter().copied().collect();
        let mut non_maximal: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
        for &c in &common {
            for a in Self::ancestors_bfs(&store, c) {
                if a != c && common_set.contains(&a) {
                    non_maximal.insert(a);
                }
            }
        }
        // `common` is BFS-nearest-first, so the surviving bases keep that order (nearest first).
        let merge_bases: Vec<ObjectId> = common.into_iter().filter(|c| !non_maximal.contains(c)).collect();
        if merge_bases.len() > 1 {
            return Err(MergeError::Conflict("CONFLICT: complex history (multiple merge bases); merge manually".into()));
        }
        let mergebase = merge_bases[0];
        let base_tree = Self::change_tree_id(&store, base).ok_or_else(|| MergeError::Internal("base change not found".into()))?;
        let mb_tree = Self::change_tree_id(&store, mergebase).ok_or_else(|| MergeError::Internal("merge-base change not found".into()))?;
        let merged = self.synthesize_three_way(&store, branch, mb_tree, base_tree, head_tree)?;
        // A synthesized 3-way is NEVER a fast-forward, even if `merged == head_tree` coincidentally
        // (no-op/empty base): the exporter must preserve base_git as a parent, not FF to head.
        Ok(Self::pin_merge_tree(&store, merged, false))
    }

    /// Per-path 3-way merge of `base_tree` (ours) and `head_tree` (theirs) against common ancestor
    /// `mb_tree`, producing a merged tree id. Per path: both sides equal ⇒ take it; a side unchanged
    /// from the ancestor ⇒ take the other side (covers add / modify / delete on one side); both sides
    /// changed the same path to different content (incl. add/add-differing and delete/modify) ⇒
    /// CONFLICT. The merge is applied on top of a base checkout so unchanged files keep their mode.
    fn synthesize_three_way(&self, store: &Store, branch: &str, mb_tree: ObjectId, base_tree: ObjectId, head_tree: ObjectId) -> Result<ObjectId, MergeError> {
        // Mode-aware: each side maps path → (blob, mode). Comparing the tuple means a change that ONLY
        // flips the exec bit (or file↔symlink) is a real change and is resolved/materialized as such.
        let mut mb = HashMap::new();
        flatten_tree_modes(store, mb_tree, "", &mut mb, 0);
        let mut ours = HashMap::new();
        flatten_tree_modes(store, base_tree, "", &mut ours, 0);
        let mut theirs = HashMap::new();
        flatten_tree_modes(store, head_tree, "", &mut theirs, 0);
        let mut paths: Vec<&String> = ours.keys().chain(theirs.keys()).chain(mb.keys()).collect();
        paths.sort();
        paths.dedup();
        // Only paths whose resolved (blob, mode) DIFFERS from `ours` become edits applied on top of the
        // base checkout — so unchanged files (the vast majority) retain their mode from the checkout.
        let mut edits: Vec<(String, Option<(ObjectId, u32)>)> = Vec::new();
        for p in paths {
            let b = mb.get(p).copied();
            let o = ours.get(p).copied();
            let t = theirs.get(p).copied();
            let resolved: Option<(ObjectId, u32)> = if o == t {
                o // both sides agree (incl. both-absent)
            } else if o == b {
                t // ours unchanged from the ancestor ⇒ take theirs (add / modify / delete)
            } else if t == b {
                o // theirs unchanged ⇒ take ours
            } else {
                return Err(MergeError::Conflict(format!("CONFLICT: merge conflict in {p}; update your change against {branch}")));
            };
            if resolved != o {
                edits.push((p.clone(), resolved));
            }
        }
        self.materialize_merge(store, base_tree, &edits)
    }

    /// Materialize a merged tree: check out `base_tree`, apply the resolved edits (write a blob at its
    /// mode — plain / executable / symlink — or delete a path), then snapshot. Mode-aware so a merged
    /// `+x` script stays executable and a merged symlink stays a symlink (see [`flatten_tree_modes`]);
    /// a plain `fs::write` for every edit is exactly what dropped the exec bit and turned symlinks into
    /// regular files. Mirrors [`RepoHost::compose_independence_tree`]'s checkout+patch.
    fn materialize_merge(&self, store: &Store, base_tree: ObjectId, edits: &[(String, Option<(ObjectId, u32)>)]) -> Result<ObjectId, MergeError> {
        let seq = SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hull-merge-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        if keel_store::snapshot::checkout(store, base_tree, &dir).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(MergeError::Internal("checkout of base tree failed".into()));
        }
        for (path, resolved) in edits {
            // Defense-in-depth: refuse to write/delete through a symlinked path prefix, which could
            // escape the checkout dir (a latent risk in the checkout-then-patch design). Reject the
            // whole merge rather than materialize a tree derived from a traversal outside `dir`.
            if let Some(link) = symlinked_prefix(&dir, path) {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(MergeError::Internal(format!(
                    "refusing merge: symlinked path prefix {} for edit {path}",
                    link.display()
                )));
            }
            let fp = dir.join(path);
            match resolved {
                Some((id, mode)) => {
                    let bytes = match store.get(id) {
                        Ok(Some(Object::Blob(b))) => b,
                        _ => {
                            let _ = std::fs::remove_dir_all(&dir);
                            return Err(MergeError::Internal(format!("missing merged blob for {path}")));
                        }
                    };
                    if let Some(parent) = fp.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    // Replace any existing entry first so file↔symlink transitions apply cleanly.
                    let _ = std::fs::remove_file(&fp);
                    if let Err(e) = write_entry(&fp, &bytes, *mode) {
                        let _ = std::fs::remove_dir_all(&dir);
                        return Err(MergeError::Internal(format!("write failed for {path}: {e}")));
                    }
                }
                None => {
                    let _ = std::fs::remove_file(&fp);
                }
            }
        }
        let tree = keel_store::snapshot::snapshot_uncached(store, &dir).ok();
        let _ = std::fs::remove_dir_all(&dir);
        tree.ok_or_else(|| MergeError::Internal("snapshot of merged tree failed".into()))
    }

    /// Commit the two-parent merge change (`parents = [base, head]`, or `[head]` on an unborn branch)
    /// carrying `merged_tree`, and advance `branch` to it by **compare-and-swap** from `base`. Returns
    /// `Some(change_hex)` on success, `None` if the branch moved under us (caller re-plans + retries),
    /// or an internal error.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn land_merge(
        &self,
        tenant: &str,
        repo: &str,
        branch: &str,
        base_hex: Option<&str>,
        head_hex: &str,
        merged_tree_hex: &str,
        fast_forward: bool,
        intent: &str,
        author: &str,
        timestamp: u64,
    ) -> Result<Option<String>, MergeError> {
        let store = self
            .store(tenant, repo, false)
            .map_err(|e| MergeError::Internal(e.to_string()))?
            .ok_or_else(|| MergeError::Internal("no such repo".into()))?;
        let merged_tree = ObjectId::from_hex(merged_tree_hex).ok_or_else(|| MergeError::Internal("bad merged tree id".into()))?;
        let head = ObjectId::from_hex(head_hex).ok_or_else(|| MergeError::Internal("bad head change id".into()))?;
        let expected = match base_hex {
            Some(h) => Some(ObjectId::from_hex(h).ok_or_else(|| MergeError::Internal("bad base change id".into()))?),
            None => None,
        };
        let parents: Vec<ObjectId> = match expected {
            Some(b) => vec![b, head],
            None => vec![head],
        };
        let change = keel_store::Change {
            parents,
            tree: merged_tree,
            session: None,
            intent: intent.to_string(),
            author: author.to_string(),
            timestamp,
            verification: keel_store::Verification::Unverified,
        };
        let obj = Object::Change(change);
        let id = obj.id();
        match store.apply_cas(std::slice::from_ref(&obj), branch, expected, id) {
            Ok(keel_store::store::Applied::Committed) => {
                // The keel-native `branch` ref advanced; the git mirror's `refs/heads/<branch>` did
                // NOT — so `git clone`/`ls-remote` would never see this gated merge. Advance it too.
                // Best-effort: the keel merge already committed and must not roll back on a mirror
                // hiccup, so an export failure is logged and swallowed.
                if let Err(e) = self.export_change_to_git_mirror(tenant, repo, &id.to_hex(), branch, fast_forward) {
                    eprintln!("hull: git-mirror export failed for {tenant}/{repo} change {}: {e}", id.to_hex());
                }
                Ok(Some(id.to_hex()))
            }
            Ok(keel_store::store::Applied::Conflict(_)) => Ok(None),
            Err(e) => Err(MergeError::Internal(e.to_string())),
        }
    }

    /// Advance the git mirror's `refs/heads/<branch>` to reflect a just-landed merge-queue change, so
    /// `git clone` / `git ls-remote` observe gated merges (the keel-native `<branch>` ref already
    /// advanced in [`land_merge`]; the git ref namespace, read by git clients via [`keel_git::mirror`],
    /// did not). Best-effort consistency — the caller has already committed the keel merge.
    ///
    /// **History must not diverge.** Git clients already hold commits for everything that arrived via
    /// push. Re-serializing a keel change into a *new* git commit with a different oid would rewrite
    /// history, so the parents of the exported merge always REUSE existing git commit oids:
    /// - parent[0] is the current git `refs/heads/<branch>` tip that clients already have (falling back
    ///   to the base change's existing git commit via a reverse `gchange` scan);
    /// - parent[1] is the head change's existing git commit (it arrived via a feature-branch push, so
    ///   it is present in `gchange`), found by reverse-scanning that map.
    ///
    /// If a required parent can't be resolved to an existing git commit, the export is SKIPPED (the git
    /// ref is left stale — exactly today's behavior — rather than diverging history) and `Ok(())` is
    /// returned after a warning. A **fast-forward** advances the ref straight to the head's existing git
    /// commit, synthesizing nothing. A **divergent** land synthesizes exactly ONE new git merge commit
    /// over those two existing parents.
    ///
    /// `fast_forward` is the genuine ancestry outcome threaded from [`RepoHost::plan_merge`]
    /// (`base ∈ ancestors(head)`) — NOT inferred from tree equality, which a no-op/empty base
    /// (revert-of-revert) would fool into a non-fast-forward ref move (CRITICAL-1). Even when the plan
    /// says fast-forward, the ref only advances if `head_git` genuinely descends from the current git
    /// tip in the GIT commit graph (belt-and-suspenders); otherwise it falls back to non-FF synthesis.
    ///
    /// On the non-FF path the export is **skipped** (git left stale, `Ok(())`) if either parent's real
    /// git tree uses a feature keel's bridge cannot round-trip — `160000` gitlinks/submodules, `120000`
    /// symlinks, or non-UTF-8 entry names — because a tree re-derived from keel would silently drop the
    /// submodule, fold the symlink to a plain file, or mangle the name (MED-HIGH-3). This guarantees the
    /// export only ever ADDS a faithful advance for ordinary-file repos; it never regresses content.
    pub(crate) fn export_change_to_git_mirror(&self, tenant: &str, repo: &str, change_hex: &str, branch: &str, fast_forward: bool) -> io::Result<()> {
        let store = self
            .store(tenant, repo, false)
            .map_err(|e| io::Error::other(e.to_string()))?
            .ok_or_else(|| io::Error::other("no such repo"))?;
        let change_id = ObjectId::from_hex(change_hex).ok_or_else(|| io::Error::other("bad change id"))?;
        let change = match store.get(&change_id).map_err(|e| io::Error::other(e.to_string()))? {
            Some(Object::Change(c)) => c,
            _ => return Err(io::Error::other("landed change not found")),
        };
        // The head change is the LAST parent (`[base, head]` on a real merge, `[head]` on an unborn
        // branch's first land); the base (parent[0]) exists only when there are two parents.
        let Some(&head_keel) = change.parents.last() else {
            return Err(io::Error::other("merge change has no parents"));
        };
        let base_keel: Option<ObjectId> = if change.parents.len() >= 2 { Some(change.parents[0]) } else { None };

        // Reverse the `gchange` map (git-commit-oid(20) → keel-change-id(32)) so parents resolve to the
        // EXISTING git commits clients already hold — the guarantee against history divergence.
        let mut rev: HashMap<[u8; 32], [u8; 20]> = HashMap::new();
        for (k, v) in store.aux_iter("gchange").map_err(|e| io::Error::other(e.to_string()))? {
            if k.len() == 20 && v.len() == 32 {
                let (mut goid, mut cid) = ([0u8; 20], [0u8; 32]);
                goid.copy_from_slice(&k);
                cid.copy_from_slice(&v);
                rev.entry(cid).or_insert(goid); // first wins — deterministic enough for v1
            }
        }
        let git_for = |cid: &ObjectId| -> Option<String> { rev.get(&cid.0).map(|o| keel_git::Oid::from_bytes(*o).to_hex()) };

        let refname = format!("refs/heads/{branch}");
        let current_git_tip: Option<String> = keel_git::mirror::refs(&store)?
            .into_iter()
            .find(|(n, _)| *n == refname)
            .map(|(_, v)| v)
            .filter(|v| v.len() == 40 && v.bytes().all(|b| b.is_ascii_hexdigit()));

        // parent[1]: the head's existing git commit. If the head never reached git (a purely keel-native
        // head), there is nothing to fast-forward to and nothing whose oid we may reuse — skip.
        let Some(head_git) = git_for(&head_keel) else {
            eprintln!(
                "hull: git-mirror export skipped for {tenant}/{repo} change {change_hex}: head change {} has no git commit in gchange (leaving {refname} stale, not diverging history)",
                head_keel.to_hex()
            );
            return Ok(());
        };

        // parse the head's existing git commit once — needed for the FF descendant proof and (on the
        // non-FF path) the fidelity guard and merge parent.
        let head_git_oid = keel_git::Oid::from_hex_str(&head_git).map_err(|e| io::Error::other(format!("{e:?}")))?;

        // Fast-forward? The decision is the plan's genuine ancestry outcome (`base ∈ ancestors(head)`),
        // NOT tree equality — a no-op/empty base can make a divergent land's merged tree equal the head's
        // while head_git is only a SIBLING of the git tip, so FF-ing there would be a non-fast-forward
        // ref move that drops base history. Belt-and-suspenders: even on a FF plan, only advance if the
        // GIT graph confirms head_git descends from the current tip; otherwise fall through to non-FF
        // synthesis (which reuses the tip as parent[0]). An unborn git branch (no tip) FF's trivially.
        if fast_forward {
            let ff_sound = match &current_git_tip {
                None => true,
                Some(tip) => match keel_git::Oid::from_hex_str(tip) {
                    Ok(tip_oid) => Self::git_commit_reaches(&store, &head_git_oid, &tip_oid)?,
                    Err(_) => false,
                },
            };
            if ff_sound {
                keel_git::mirror::set_ref(&store, &refname, &head_git)?;
                Self::ensure_head_symref(&store, &refname)?;
                return Ok(());
            }
            // plan said FF but the git graph disagrees (stale/ambiguous tip) — fall through to non-FF.
        }

        // Non-fast-forward: synthesize exactly ONE new git merge commit over the existing parents.
        // parent[0] prefers the git tip clients already have; else the base change's existing commit.
        let Some(parent0) = current_git_tip.or_else(|| base_keel.as_ref().and_then(git_for)) else {
            eprintln!(
                "hull: git-mirror export skipped for {tenant}/{repo} change {change_hex}: base parent has no existing git commit (leaving {refname} stale, not diverging history)"
            );
            return Ok(());
        };

        // Fidelity guard (never regress). keel's git bridge drops `160000` gitlinks/submodules, folds
        // `120000` symlinks to plain files, and lossily converts non-UTF-8 entry names at ingest — so a
        // synthesized non-FF merge tree (re-derived from keel) would silently remove a submodule, turn a
        // symlink into a regular file, or mangle a name that either parent carries. Rather than write an
        // unfaithful tree, inspect BOTH parents' REAL git trees (recursively); if either uses any of
        // those features, SKIP the export — leaving the git ref stale, identical to pre-feature behavior.
        let parent0_oid = keel_git::Oid::from_hex_str(&parent0).map_err(|e| io::Error::other(format!("{e:?}")))?;
        if Self::git_tree_has_unfaithful_entry(&store, &parent0_oid)? || Self::git_tree_has_unfaithful_entry(&store, &head_git_oid)? {
            eprintln!(
                "hull: git-mirror export skipped for {tenant}/{repo} change {change_hex}: repo uses submodules/symlinks/non-UTF-8 names not faithfully representable; {refname} left stale"
            );
            return Ok(());
        }

        let git_tree = Self::export_keel_tree_to_git(&store, change.tree)?;
        let ident = git_identity(&change.author);
        let mut headers = Vec::new();
        headers.extend_from_slice(format!("tree {}\n", git_tree.to_hex()).as_bytes());
        headers.extend_from_slice(format!("parent {parent0}\n").as_bytes());
        headers.extend_from_slice(format!("parent {head_git}\n").as_bytes());
        headers.extend_from_slice(format!("author {ident} {} +0000\n", change.timestamp).as_bytes());
        // A stable committer identity: the merge was landed by Hull's merge queue, not authored by hand.
        headers.extend_from_slice(format!("committer {HULL_COMMITTER} {} +0000\n", change.timestamp).as_bytes());
        let message = if change.intent.is_empty() {
            format!("Merge into {branch}\n").into_bytes()
        } else if change.intent.ends_with('\n') {
            change.intent.clone().into_bytes()
        } else {
            format!("{}\n", change.intent).into_bytes()
        };
        let payload = keel_git::serialize_headed(&keel_git::Headed { headers, message });
        let merge_oid = keel_git::hash(keel_git::Kind::Commit, &payload);
        keel_git::mirror::ingest_objects(&store, &[(keel_git::Kind::Commit, payload)])?;
        // Keep the FORWARD map consistent (git-commit → keel-change) so a later push/bridge over this
        // commit doesn't re-synthesize a divergent keel change.
        store.aux_put("gchange", merge_oid.as_bytes(), &change_id.0).map_err(|e| io::Error::other(e.to_string()))?;
        keel_git::mirror::set_ref(&store, &refname, &merge_oid.to_hex())?;
        Self::ensure_head_symref(&store, &refname)?;
        Ok(())
    }

    /// Recursively serialize a keel tree into git tree/blob objects in the mirror, returning the git
    /// tree oid. Each blob is ingested (a dedup no-op — the content already exists from the pushes that
    /// built it, so git oids match); each subdirectory recurses. Modes are inverted keel → git.
    fn export_keel_tree_to_git(store: &Store, tree_id: ObjectId) -> io::Result<keel_git::Oid> {
        let entries = match store.get(&tree_id).map_err(|e| io::Error::other(e.to_string()))? {
            Some(Object::Tree(t)) => t.entries,
            _ => return Err(io::Error::other("expected a keel tree")),
        };
        let mut git_entries = Vec::with_capacity(entries.len());
        for e in entries {
            let (mode, oid) = if e.mode == MODE_DIR {
                (b"40000".to_vec(), Self::export_keel_tree_to_git(store, e.id)?)
            } else {
                let bytes = match store.get(&e.id).map_err(|er| io::Error::other(er.to_string()))? {
                    Some(Object::Blob(b)) => b,
                    _ => return Err(io::Error::other("expected a keel blob")),
                };
                let oid = keel_git::hash(keel_git::Kind::Blob, &bytes);
                keel_git::mirror::ingest_objects(store, &[(keel_git::Kind::Blob, bytes)])?;
                (git_mode_bytes(e.mode), oid)
            };
            git_entries.push(keel_git::TreeEntry { mode, name: e.name.into_bytes(), oid });
        }
        keel_git::sort_tree(&mut git_entries);
        let payload = keel_git::serialize_tree(&git_entries);
        let oid = keel_git::hash(keel_git::Kind::Tree, &payload);
        keel_git::mirror::ingest_objects(store, &[(keel_git::Kind::Tree, payload)])?;
        Ok(oid)
    }

    /// Walk the GIT commit graph from `start` over parent edges: is `target` reachable — i.e. is
    /// `start` a descendant of `target`? Cycle-safe and bounded by the objects present; a missing or
    /// non-commit object just ends that branch. Used to PROVE a planned fast-forward is a genuine
    /// ancestry advance in git before moving the ref, so the export can never do a non-FF ref move.
    fn git_commit_reaches(store: &Store, start: &keel_git::Oid, target: &keel_git::Oid) -> io::Result<bool> {
        if start == target {
            return Ok(true);
        }
        let mut seen = std::collections::HashSet::new();
        seen.insert(*start);
        let mut stack = vec![*start];
        while let Some(oid) = stack.pop() {
            let Some((kind, payload)) = keel_git::mirror::get_object(store, &oid)? else { continue };
            if kind != keel_git::Kind::Commit {
                continue;
            }
            for p in keel_git::Headed::parse(&payload).parents().unwrap_or_default() {
                if p == *target {
                    return Ok(true);
                }
                if seen.insert(p) {
                    stack.push(p);
                }
            }
        }
        Ok(false)
    }

    /// Does `commit`'s real git tree (recursively) contain any entry keel's git bridge cannot faithfully
    /// round-trip — a `160000` gitlink/submodule, a `120000` symlink, or a non-UTF-8 entry name? Such an
    /// entry means a keel-re-derived merge tree would silently regress content, so the exporter skips
    /// rather than synthesize. A missing/non-commit object answers `false` (nothing to protect).
    fn git_tree_has_unfaithful_entry(store: &Store, commit: &keel_git::Oid) -> io::Result<bool> {
        let Some((kind, payload)) = keel_git::mirror::get_object(store, commit)? else { return Ok(false) };
        if kind != keel_git::Kind::Commit {
            return Ok(false);
        }
        let Ok(tree) = keel_git::Headed::parse(&payload).tree() else { return Ok(false) };
        Self::git_tree_entry_unfaithful_rec(store, &tree)
    }

    /// Recursive worker for [`RepoHost::git_tree_has_unfaithful_entry`], walking a git tree oid.
    fn git_tree_entry_unfaithful_rec(store: &Store, tree: &keel_git::Oid) -> io::Result<bool> {
        let Some((kind, payload)) = keel_git::mirror::get_object(store, tree)? else { return Ok(false) };
        if kind != keel_git::Kind::Tree {
            return Ok(false);
        }
        for e in keel_git::parse_tree(&payload).map_err(|err| io::Error::other(format!("{err:?}")))? {
            // A gitlink (submodule) or symlink is dropped/folded by the bridge; a non-UTF-8 name is
            // lossily converted. Any of these anywhere in the tree disqualifies faithful synthesis.
            if e.mode == b"160000" || e.mode == b"120000" || std::str::from_utf8(&e.name).is_err() {
                return Ok(true);
            }
            if e.mode == b"40000" && Self::git_tree_entry_unfaithful_rec(store, &e.oid)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Ensure the mirror has a symbolic `HEAD` (a fresh clone needs it to check out), mirroring what
    /// [`keel_git::smart_http`]'s receive-pack does after the first push.
    fn ensure_head_symref(store: &Store, refname: &str) -> io::Result<()> {
        if !keel_git::mirror::refs(store)?.iter().any(|(n, _)| n == "HEAD") {
            keel_git::mirror::set_ref(store, "HEAD", &format!("ref: {refname}"))?;
        }
        Ok(())
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
pub(crate) fn safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.starts_with('.')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !s.contains("..")
}

// ── axum handlers (wired by `router` in lib.rs) ─────────────────────────────────────────────────
//
// AUTHORIZATION (authz-hardening, area D — closed). These three git smart-HTTP handlers are the
// success-path wire protocol ONLY; the auth pre-check lives in front of them in lib.rs (where the full
// `App` — and thus `can_read_repo` / `is_repo_member` — is available): `info_refs_handler`,
// `upload_pack_handler` and `receive_pack_handler` run `git_gate` before delegating here. Enforcement
// is config-gated by `HULL_GIT_AUTH` (default `off` = fully anonymous, a byte-for-byte no-op; set
// `enforce` to require credentials). Credentials arrive as HTTP Basic (the password is a hull session
// token) or a `Bearer` fallback; see `git_auth_decision` in lib.rs for the fetch/push/create matrix.
// These handlers therefore assume authorization already happened — do not call them on a path that
// bypasses the lib.rs wrappers.

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
    let body = maybe_gunzip(&headers, body.to_vec(), git_max_body_bytes());
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
    let body = maybe_gunzip(&headers, body.to_vec(), git_max_body_bytes());
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

/// Maximum accepted git request-body size (raw and, after `maybe_gunzip`, decompressed), in bytes.
/// A git push carries a packfile, so this is generous; `HULL_GIT_MAX_BODY_MB` (default 512) tunes it.
/// Used both to build the git routes' `RequestBodyLimitLayer` and to bound gzip decompression.
pub(crate) fn git_max_body_bytes() -> usize {
    let mb: usize = std::env::var("HULL_GIT_MAX_BODY_MB").ok().and_then(|v| v.parse().ok()).filter(|&m| m > 0).unwrap_or(512);
    mb.saturating_mul(1024 * 1024)
}

/// Decompress the body if the request declares `Content-Encoding: gzip` (git's default for the
/// upload-pack POST). The decompressed size is capped at `max` bytes (callers pass
/// [`git_max_body_bytes`]) so a gzip bomb (small input, huge inflation) can't exhaust memory: on
/// over-cap OR any decode failure we hand back the raw bytes, which the git pkt-line parser then
/// rejects — a bounded, safe failure rather than an OOM.
pub(crate) fn maybe_gunzip(headers: &HeaderMap, body: Vec<u8>, max: usize) -> Vec<u8> {
    let is_gzip = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().contains("gzip"))
        .unwrap_or(false);
    if !is_gzip {
        return body;
    }
    use std::io::Read;
    // Read one byte past the cap so `out.len() > max` unambiguously signals overflow.
    let mut out = Vec::new();
    match flate2::read::GzDecoder::new(&body[..]).take(max as u64 + 1).read_to_end(&mut out) {
        Ok(_) if out.len() <= max => out,
        _ => body,
    }
}

/// Parse the **command section** of a (gunzipped) git-receive-pack request body into the
/// `(new-oid, refname)` pairs it will apply — using the **exact same reader and tokenization** that
/// keel-git's `smart_http::receive_pack` uses to actually move refs (`pktline::read_at` +
/// `split_whitespace`, capabilities dropped after the first `\0`). Sharing the one parser is the
/// point: hull's recognized command set is then *provably identical* to what keel-git applies, so a
/// double-space, a tab, a trailing token, or a `0001` delimiter can't make hull's view of "which refs
/// this push touches" diverge from what really gets written. Mirrors `smart_http.rs:118-132`.
///
/// The `new-oid` is retained only for parity with keel-git's `commands` shape; the protection
/// predicate ([`touches_protected`]) judges by refname alone (see its doc).
pub(crate) fn parse_receive_refs(body: &[u8]) -> Vec<(String, String)> {
    use keel_git::pktline::{read_at, Pkt};
    let mut out = Vec::new(); // (new-oid, refname)
    let mut pos = 0;
    while let Some(pkt) = read_at(body, &mut pos) {
        match pkt {
            Pkt::Flush => break,
            Pkt::Line(line) => {
                let line = line.split(|&b| b == 0).next().unwrap_or(line); // drop caps after NUL
                let s = String::from_utf8_lossy(line);
                let mut it = s.split_whitespace();
                if let (Some(_old), Some(new), Some(r)) = (it.next(), it.next(), it.next()) {
                    out.push((new.to_string(), r.to_string()));
                }
            }
            Pkt::Delim => {}
        }
    }
    out
}

/// Does any command **touch** the protected default branch (`refs/heads/<default_branch>`)? On a
/// protected repo we reject the push if any recorded command's refname equals the protected ref,
/// **regardless of old/new** — an overwrite (even one the client dresses up with a zeroed `old`), a
/// deletion (`new` = zeros), and a normal update are all caught. We deliberately do NOT exempt
/// "creation" based on the client-supplied `old`: on a protected repo the branch already exists
/// (protection is opt-in, enabled after the repo is created), and the only sanctioned way to advance
/// it is the server-side merge queue (`perform_merge` → `set_ref`), which never goes through
/// receive-pack. keel-git applies refs with no old-value check, so trusting the client's `old` was
/// exactly the zeroed-old bypass — judging by refname closes it.
pub(crate) fn touches_protected(commands: &[(String, String)], default_branch: &str) -> bool {
    let target = format!("refs/heads/{default_branch}");
    commands.iter().any(|(_new, refname)| *refname == target)
}

#[cfg(test)]
impl RepoHost {
    /// Test-only: ensure `tenant/repo` exists and commit `files` as a new keel change (parented on
    /// `parent`, or a root change if `None`) carrying `intent`; points `main` at it. Returns the new
    /// change id (hex). Shared by the cross-module CI / merge-gate tests that need real changes.
    pub(crate) fn test_commit(&self, tenant: &str, repo: &str, intent: &str, parent: Option<&str>, files: &[(&str, &str)]) -> String {
        let store = self.store(tenant, repo, true).unwrap().unwrap();
        let seq = SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hull-testcommit-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (p, c) in files {
            let fp = dir.join(p);
            std::fs::create_dir_all(fp.parent().unwrap()).unwrap();
            std::fs::write(&fp, c).unwrap();
        }
        let tree = keel_store::snapshot::snapshot_uncached(&store, &dir).unwrap();
        let parents: Vec<ObjectId> = parent.and_then(ObjectId::from_hex).into_iter().collect();
        let change = keel_store::Change {
            parents,
            tree,
            session: None,
            intent: intent.to_string(),
            author: "tester".into(),
            timestamp: 0,
            verification: keel_store::Verification::Unverified,
        };
        let id = store.put(&Object::Change(change)).unwrap();
        let _ = store.set_ref("main", &id);
        let _ = std::fs::remove_dir_all(&dir);
        id.to_hex()
    }

    /// Test-only: like [`Self::test_commit`] but does NOT move `main` — commits a change parented on
    /// `parent` and leaves the branch where it is. Used by the protected-branch merge tests to build a
    /// PR head that sits OFF the default branch (so a real land has something to fast-forward/advance).
    pub(crate) fn test_change(&self, tenant: &str, repo: &str, parent: Option<&str>, files: &[(&str, &str)]) -> String {
        let store = self.store(tenant, repo, true).unwrap().unwrap();
        let seq = SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hull-testchange-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (p, c) in files {
            let fp = dir.join(p);
            std::fs::create_dir_all(fp.parent().unwrap()).unwrap();
            std::fs::write(&fp, c).unwrap();
        }
        let tree = keel_store::snapshot::snapshot_uncached(&store, &dir).unwrap();
        let parents: Vec<ObjectId> = parent.and_then(ObjectId::from_hex).into_iter().collect();
        let change = keel_store::Change {
            parents,
            tree,
            session: None,
            intent: "child".into(),
            author: "tester".into(),
            timestamp: 0,
            verification: keel_store::Verification::Unverified,
        };
        let id = store.put(&Object::Change(change)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        id.to_hex()
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

    #[test]
    fn maybe_gunzip_caps_decompressed_size() {
        use std::io::Write;
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_ENCODING, "gzip".parse().unwrap());
        // A gzip bomb: 8 MiB of zeros compresses to a few KiB.
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(&vec![0u8; 8 * 1024 * 1024]).unwrap();
        let bomb = enc.finish().unwrap();
        assert!(bomb.len() < 256 * 1024, "the bomb is small compressed ({} bytes)", bomb.len());
        // With a 1 MiB cap the 8 MiB inflation is refused → raw (still-compressed) bytes handed back,
        // never the 8 MiB buffer. The git parser then rejects the raw bytes (bounded, safe failure).
        assert_eq!(maybe_gunzip(&headers, bomb.clone(), 1024 * 1024), bomb, "over-cap → raw bytes");
        // A within-cap payload decompresses normally.
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"hello git").unwrap();
        let small = enc.finish().unwrap();
        assert_eq!(maybe_gunzip(&headers, small, 1024 * 1024), b"hello git", "within-cap decompresses");
        // A non-gzip body is passed through untouched regardless of cap.
        assert_eq!(maybe_gunzip(&HeaderMap::new(), b"plain".to_vec(), 4), b"plain");
    }

    #[test]
    fn concurrent_first_open_caches_a_single_env() {
        // Two concurrent first-touches of the same repo must not each open the LMDB environment
        // (opening one env twice in a process is UB). Hammer store() from many threads for a
        // brand-new repo: every call succeeds and exactly one env ends up cached under the key.
        let tmp = std::env::temp_dir().join(format!("hull-repos-conc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let host = Arc::new(RepoHost::new(&tmp));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let h = host.clone();
            handles.push(std::thread::spawn(move || h.store("acme", "web", true).map(|o| o.is_some()).unwrap_or(false)));
        }
        for hnd in handles {
            assert!(hnd.join().unwrap(), "every concurrent first-open succeeds");
        }
        assert_eq!(host.open.lock().unwrap().len(), 1, "exactly one cached env for the repo");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Build a tree from a set of (path, contents) files and return its keel tree id.
    fn tree_from(store: &Store, base: &std::path::Path, tag: &str, files: &[(&str, &str)]) -> ObjectId {
        let dir = base.join(tag);
        let _ = std::fs::remove_dir_all(&dir);
        for (p, c) in files {
            let fp = dir.join(p);
            std::fs::create_dir_all(fp.parent().unwrap()).unwrap();
            std::fs::write(&fp, c).unwrap();
        }
        keel_store::snapshot::snapshot_uncached(store, &dir).unwrap()
    }

    fn commit(store: &Store, parents: Vec<ObjectId>, tree: ObjectId) -> ObjectId {
        let change = keel_store::Change {
            parents,
            tree,
            session: None,
            intent: "c".into(),
            author: "t".into(),
            timestamp: 0,
            verification: keel_store::Verification::Unverified,
        };
        store.put(&Object::Change(change)).unwrap()
    }

    #[test]
    fn pure_move_is_detected_by_content_address() {
        let tmp = std::env::temp_dir().join(format!("hull-move-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("t/r/.keel/store")).unwrap();
        let host = RepoHost::new(&tmp);
        let (parent_hex, moved_hex, mixed_hex) = {
            let store = host.store("t", "r", true).unwrap().unwrap();
            // parent: a file + an unrelated file
            let p_tree = tree_from(&store, &tmp, "p", &[("src/old.rs", "fn f() {}\n"), ("README", "hi\n")]);
            let parent = commit(&store, vec![], p_tree);
            // pure move: old.rs → new.rs, byte-identical; README untouched
            let m_tree = tree_from(&store, &tmp, "m", &[("src/new.rs", "fn f() {}\n"), ("README", "hi\n")]);
            let moved = commit(&store, vec![parent], m_tree);
            // mixed: same move, but README also edited → not a pure move
            let x_tree = tree_from(&store, &tmp, "x", &[("src/new.rs", "fn f() {}\n"), ("README", "changed\n")]);
            let mixed = commit(&store, vec![parent], x_tree);
            (parent.to_hex(), moved.to_hex(), mixed.to_hex())
        };
        let _ = parent_hex;

        let s = host.semantic_summary("t", "r", &moved_hex);
        assert!(s.pure_move, "a content-identical relocation is a pure move");
        assert_eq!(s.moves.len(), 1);
        assert_eq!((s.moves[0].from.as_str(), s.moves[0].to.as_str()), ("src/old.rs", "src/new.rs"));
        assert!(s.added.is_empty() && s.deleted.is_empty() && s.modified.is_empty(), "nothing else changed");

        let m = host.semantic_summary("t", "r", &mixed_hex);
        assert!(!m.pure_move, "a move alongside a content edit is NOT a pure move");
        assert_eq!(m.moves.len(), 1); // the move is still detected
        assert_eq!(m.modified, vec!["README".to_string()]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn whitespace_only_edit_is_mechanical_not_behavioral() {
        let tmp = std::env::temp_dir().join(format!("hull-ws-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("t/r/.keel/store")).unwrap();
        let host = RepoHost::new(&tmp);
        let (ws_hex, beh_hex) = {
            let store = host.store("t", "r", true).unwrap().unwrap();
            let p = commit(&store, vec![], tree_from(&store, &tmp, "p", &[("a.rs", "fn f(){x}\n")]));
            // reformat only — same tokens, different whitespace
            let ws = commit(&store, vec![p], tree_from(&store, &tmp, "ws", &[("a.rs", "fn f() {\n    x\n}\n")]));
            // real content change
            let beh = commit(&store, vec![p], tree_from(&store, &tmp, "beh", &[("a.rs", "fn f(){y}\n")]));
            (ws.to_hex(), beh.to_hex())
        };
        let w = host.semantic_summary("t", "r", &ws_hex);
        assert_eq!(w.whitespace_only, vec!["a.rs".to_string()]);
        assert!(w.behavioral.is_empty());
        assert!(w.mechanical && !w.pure_move, "a whitespace reformat is mechanical but not a pure move");

        let b = host.semantic_summary("t", "r", &beh_hex);
        assert_eq!(b.behavioral, vec!["a.rs".to_string()]);
        assert!(b.whitespace_only.is_empty());
        assert!(!b.mechanical, "a real content edit is behavioral");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── branch-protection: receive-pack command parser ──────────────────────────────────────────

    // Frame a git pkt-line (4-hex length prefix covering the 4 bytes + payload).
    fn pkt(line: &str) -> Vec<u8> {
        let mut v = format!("{:04x}", line.len() + 4).into_bytes();
        v.extend_from_slice(line.as_bytes());
        v
    }
    const ZERO: &str = "0000000000000000000000000000000000000000";

    const NEW: &str = "1111111111111111111111111111111111111111";
    const OLD: &str = "2222222222222222222222222222222222222222";

    // Is this raw receive-pack body rejected against a `main`-protected repo? (Mirrors the lib.rs gate:
    // parse the refs with the shared parser, then apply the protection predicate.)
    fn rejected(body: &[u8]) -> bool {
        touches_protected(&parse_receive_refs(body), "main")
    }

    // The command section of a well-formed receive-pack request: `cmd` pkt-lines (first carrying
    // `\0caps`), a flush, then a never-decoded PACK. `raw_prefix` lets a test splice bytes (e.g. a
    // `0001` delim) ahead of the commands.
    fn recv_body(cmds: &[&str]) -> Vec<u8> {
        let mut body = Vec::new();
        for (i, c) in cmds.iter().enumerate() {
            let line = if i == 0 { format!("{c}\0report-status side-band-64k\n") } else { format!("{c}\n") };
            body.extend(pkt(&line));
        }
        body.extend_from_slice(b"0000");
        body.extend_from_slice(b"PACK\x00\x00garbagepackdata");
        body
    }

    #[test]
    fn parse_receive_refs_reads_the_command_section() {
        let body = recv_body(&[&format!("{OLD} {NEW} refs/heads/main"), &format!("{ZERO} {NEW} refs/heads/feature")]);
        let cmds = parse_receive_refs(&body);
        assert_eq!(cmds.len(), 2, "both commands parsed, PACK ignored");
        assert_eq!(cmds[0], (NEW.to_string(), "refs/heads/main".to_string()));
        assert_eq!(cmds[1], (NEW.to_string(), "refs/heads/feature".to_string()));
    }

    // Parity: the recognizer mirrors keel-git's — it uses the SAME `pktline::read_at` reader and the
    // SAME `split_whitespace` tokenization, keeping caps after the first NUL. So the (new, ref) pairs
    // hull records are exactly the pairs keel-git's `receive_pack` would apply. This test documents
    // that by reproducing keel-git's loop inline and asserting byte-for-byte agreement.
    #[test]
    fn parse_receive_refs_mirrors_keel_git_tokenization() {
        let body = recv_body(&[
            &format!("{OLD}  {NEW} refs/heads/main"), // double space
            &format!("{OLD}\t{NEW} refs/heads/feature"), // tab
            &format!("{ZERO} {NEW} refs/heads/x y"), // trailing token
        ]);
        // keel-git's exact loop (smart_http.rs:118-132).
        let mut expected: Vec<(String, String)> = Vec::new();
        let mut pos = 0;
        while let Some(pkt) = keel_git::pktline::read_at(&body, &mut pos) {
            match pkt {
                keel_git::pktline::Pkt::Flush => break,
                keel_git::pktline::Pkt::Line(line) => {
                    let line = line.split(|&b| b == 0).next().unwrap_or(line);
                    let s = String::from_utf8_lossy(line);
                    let mut it = s.split_whitespace();
                    if let (Some(_o), Some(n), Some(r)) = (it.next(), it.next(), it.next()) {
                        expected.push((n.to_string(), r.to_string()));
                    }
                }
                keel_git::pktline::Pkt::Delim => {}
            }
        }
        assert_eq!(parse_receive_refs(&body), expected, "hull's recognizer must match keel-git's exactly");
    }

    // Every documented bypass of the old bespoke parser must now be REJECTED on a protected `main`.
    #[test]
    fn protected_push_bypasses_are_all_rejected() {
        // (a) zeroed-old overwrite: client lies that this is a "creation" — keel-git overwrites anyway.
        assert!(rejected(&recv_body(&[&format!("{ZERO} {NEW} refs/heads/main")])), "zeroed-old overwrite");
        // (b) delete (new = zeros).
        assert!(rejected(&recv_body(&[&format!("{OLD} {ZERO} refs/heads/main")])), "delete");
        // (c) double space between tokens — old splitn(3,' ') dropped it; split_whitespace doesn't.
        assert!(rejected(&recv_body(&[&format!("{OLD}  {NEW} refs/heads/main")])), "double space");
        // (d) tab between the oids.
        assert!(rejected(&recv_body(&[&format!("{OLD}\t{NEW} refs/heads/main")])), "tab");
        // (e) trailing token after the refname — old `!=` match missed it; 3rd whitespace token catches.
        assert!(rejected(&recv_body(&[&format!("{OLD} {NEW} refs/heads/main x")])), "trailing token");
        // (e') trailing space after the refname.
        assert!(rejected(&recv_body(&[&format!("{OLD} {NEW} refs/heads/main ")])), "trailing space");
        // (f) a `0001` delim pkt before the real command — old parser `break`d on len<4 and dropped it.
        let mut delim_body = Vec::new();
        delim_body.extend_from_slice(b"0001");
        delim_body.extend(pkt(&format!("{OLD} {NEW} refs/heads/main\0report-status\n")));
        delim_body.extend_from_slice(b"0000");
        assert!(rejected(&delim_body), "0001 delim before the command");
        // (g) a normal well-formed update.
        assert!(rejected(&recv_body(&[&format!("{OLD} {NEW} refs/heads/main")])), "normal update");
        // (h) a push touching ONLY a feature branch is NOT rejected.
        assert!(!rejected(&recv_body(&[&format!("{OLD} {NEW} refs/heads/feature")])), "feature-only push allowed");
        // And a multi-command push where main hides behind a feature command is still caught.
        assert!(
            rejected(&recv_body(&[&format!("{OLD} {NEW} refs/heads/feature"), &format!("{OLD} {NEW} refs/heads/main")])),
            "main touched alongside a feature branch"
        );
        // No commands at all → nothing protected.
        assert!(!rejected(&recv_body(&[])), "empty command section");
    }

    // ── branch-protection: 3-way merge synthesis ─────────────────────────────────────────────────

    // Flatten a merged tree id to a `path -> contents` map for assertions.
    fn tree_contents(store: &Store, tree_hex: &str) -> std::collections::BTreeMap<String, String> {
        let tid = ObjectId::from_hex(tree_hex).unwrap();
        let mut flat = HashMap::new();
        flatten_tree(store, tid, "", &mut flat, 0);
        flat.into_iter()
            .map(|(p, id)| {
                let content = match store.get(&id) {
                    Ok(Some(Object::Blob(b))) => String::from_utf8_lossy(&b).into_owned(),
                    _ => String::new(),
                };
                (p, content)
            })
            .collect()
    }

    // A repo host with a base→head fast-forward chain and a divergent (3-way) pair off a shared base.
    fn merge_host(tag: &str) -> (std::path::PathBuf, RepoHost) {
        let tmp = std::env::temp_dir().join(format!("hull-merge-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("t/r/.keel/store")).unwrap();
        let host = RepoHost::new(&tmp);
        (tmp, host)
    }

    #[test]
    fn plan_merge_fast_forwards_when_base_is_ancestor_of_head() {
        let (tmp, host) = merge_host("ff");
        let (base_hex, head_hex, head_tree_hex) = {
            let store = host.store("t", "r", true).unwrap().unwrap();
            let base = commit(&store, vec![], tree_from(&store, &tmp, "b", &[("a.txt", "1\n")]));
            let head = commit(&store, vec![base], tree_from(&store, &tmp, "h", &[("a.txt", "1\n"), ("b.txt", "2\n")]));
            let head_tree = match store.get(&head).unwrap().unwrap() {
                Object::Change(c) => c.tree.to_hex(),
                _ => unreachable!(),
            };
            (base.to_hex(), head.to_hex(), head_tree)
        };
        match host.plan_merge("t", "r", "main", Some(&base_hex), &head_hex) {
            Ok(MergeOutcome::Tree { tree, fast_forward }) => {
                assert_eq!(tree, head_tree_hex, "a fast-forward's merged tree IS the head's tree");
                assert!(fast_forward, "base ∈ ancestors(head) is a genuine fast-forward");
            }
            _ => panic!("expected a fast-forward Tree outcome"),
        }
        // Symmetric: head already contained in base ⇒ nothing to land.
        match host.plan_merge("t", "r", "main", Some(&head_hex), &base_hex) {
            Ok(MergeOutcome::AlreadyMerged) => {}
            _ => panic!("expected AlreadyMerged when head is an ancestor of base"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn three_way_merges_disjoint_edits() {
        let (tmp, host) = merge_host("disjoint");
        let (base_hex, head_hex) = {
            let store = host.store("t", "r", true).unwrap().unwrap();
            // ancestor: a=1, b=1. base edits a; head edits b (disjoint).
            let mb = commit(&store, vec![], tree_from(&store, &tmp, "mb", &[("a.txt", "1\n"), ("b.txt", "1\n")]));
            let base = commit(&store, vec![mb], tree_from(&store, &tmp, "base", &[("a.txt", "2\n"), ("b.txt", "1\n")]));
            let head = commit(&store, vec![mb], tree_from(&store, &tmp, "head", &[("a.txt", "1\n"), ("b.txt", "2\n")]));
            (base.to_hex(), head.to_hex())
        };
        let tree_hex = match host.plan_merge("t", "r", "main", Some(&base_hex), &head_hex) {
            Ok(MergeOutcome::Tree { tree, fast_forward }) => {
                assert!(!fast_forward, "a synthesized 3-way is never a fast-forward");
                tree
            }
            other => panic!("expected a synthesized 3-way Tree, got {}", matches!(other, Ok(MergeOutcome::AlreadyMerged))),
        };
        let store = host.store("t", "r", false).unwrap().unwrap();
        let c = tree_contents(&store, &tree_hex);
        assert_eq!(c.get("a.txt").map(String::as_str), Some("2\n"), "base's edit to a survives");
        assert_eq!(c.get("b.txt").map(String::as_str), Some("2\n"), "head's edit to b survives");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn three_way_conflicts_on_same_file_different_edits() {
        let (tmp, host) = merge_host("conflict");
        let (base_hex, head_hex) = {
            let store = host.store("t", "r", true).unwrap().unwrap();
            let mb = commit(&store, vec![], tree_from(&store, &tmp, "mb", &[("a.txt", "1\n")]));
            let base = commit(&store, vec![mb], tree_from(&store, &tmp, "base", &[("a.txt", "2\n")]));
            let head = commit(&store, vec![mb], tree_from(&store, &tmp, "head", &[("a.txt", "3\n")]));
            (base.to_hex(), head.to_hex())
        };
        match host.plan_merge("t", "r", "main", Some(&base_hex), &head_hex) {
            Err(MergeError::Conflict(m)) => assert!(m.contains("a.txt") && m.contains("CONFLICT"), "message names the path: {m}"),
            _ => panic!("expected a conflict on divergent edits to the same file"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn three_way_conflicts_on_add_add() {
        let (tmp, host) = merge_host("addadd");
        let (base_hex, head_hex) = {
            let store = host.store("t", "r", true).unwrap().unwrap();
            let mb = commit(&store, vec![], tree_from(&store, &tmp, "mb", &[("keep.txt", "x\n")]));
            let base = commit(&store, vec![mb], tree_from(&store, &tmp, "base", &[("keep.txt", "x\n"), ("new.txt", "A\n")]));
            let head = commit(&store, vec![mb], tree_from(&store, &tmp, "head", &[("keep.txt", "x\n"), ("new.txt", "B\n")]));
            (base.to_hex(), head.to_hex())
        };
        match host.plan_merge("t", "r", "main", Some(&base_hex), &head_hex) {
            Err(MergeError::Conflict(m)) => assert!(m.contains("new.txt"), "add/add differing is a conflict: {m}"),
            _ => panic!("expected an add/add conflict"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn three_way_conflicts_on_delete_vs_modify() {
        let (tmp, host) = merge_host("delmod");
        let (base_hex, head_hex) = {
            let store = host.store("t", "r", true).unwrap().unwrap();
            let mb = commit(&store, vec![], tree_from(&store, &tmp, "mb", &[("a.txt", "1\n"), ("k.txt", "0\n")]));
            // base deletes a.txt; head modifies it.
            let base = commit(&store, vec![mb], tree_from(&store, &tmp, "base", &[("k.txt", "0\n")]));
            let head = commit(&store, vec![mb], tree_from(&store, &tmp, "head", &[("a.txt", "2\n"), ("k.txt", "0\n")]));
            (base.to_hex(), head.to_hex())
        };
        match host.plan_merge("t", "r", "main", Some(&base_hex), &head_hex) {
            Err(MergeError::Conflict(m)) => assert!(m.contains("a.txt"), "delete-vs-modify is a conflict: {m}"),
            _ => panic!("expected a delete/modify conflict"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn three_way_takes_one_sided_edits_and_deletes_cleanly() {
        // ancestor: a,b,c. base edits a and deletes b; head adds d (all disjoint from base) → clean.
        let (tmp, host) = merge_host("onesided");
        let (base_hex, head_hex) = {
            let store = host.store("t", "r", true).unwrap().unwrap();
            let mb = commit(&store, vec![], tree_from(&store, &tmp, "mb", &[("a.txt", "1\n"), ("b.txt", "1\n"), ("c.txt", "1\n")]));
            let base = commit(&store, vec![mb], tree_from(&store, &tmp, "base", &[("a.txt", "2\n"), ("c.txt", "1\n")]));
            let head = commit(&store, vec![mb], tree_from(&store, &tmp, "head", &[("a.txt", "1\n"), ("b.txt", "1\n"), ("c.txt", "1\n"), ("d.txt", "9\n")]));
            (base.to_hex(), head.to_hex())
        };
        let tree_hex = match host.plan_merge("t", "r", "main", Some(&base_hex), &head_hex) {
            Ok(MergeOutcome::Tree { tree, .. }) => tree,
            _ => panic!("expected a clean 3-way merge"),
        };
        let store = host.store("t", "r", false).unwrap().unwrap();
        let c = tree_contents(&store, &tree_hex);
        assert_eq!(c.get("a.txt").map(String::as_str), Some("2\n"), "base's edit to a wins (head unchanged there)");
        assert!(!c.contains_key("b.txt"), "base's delete of b is honored (head unchanged there)");
        assert_eq!(c.get("d.txt").map(String::as_str), Some("9\n"), "head's added d is taken");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Flatten a merged tree id to `path -> git mode`.
    fn tree_modes(store: &Store, tree_hex: &str) -> std::collections::BTreeMap<String, u32> {
        let tid = ObjectId::from_hex(tree_hex).unwrap();
        let mut flat = HashMap::new();
        flatten_tree_modes(store, tid, "", &mut flat, 0);
        flat.into_iter().map(|(p, (_, m))| (p, m)).collect()
    }

    // MED 1: a merge that flips the exec bit or introduces a symlink must carry the MODE through — a
    // plain-`fs::write` materialize dropped +x and turned symlinks into regular files ("green but
    // wrong"). Unix-only: modes are a no-op on platforms without them.
    #[cfg(unix)]
    #[test]
    fn three_way_preserves_exec_bit_and_symlink_through_merge() {
        use keel_store::snapshot::{snapshot_uncached, MODE_EXEC, MODE_FILE, MODE_SYMLINK};
        use std::os::unix::fs::PermissionsExt;
        let (tmp, host) = merge_host("modes");
        // Write `bytes` to `dir/name` at `mode` (MODE_FILE or MODE_EXEC).
        let write_mode = |dir: &std::path::Path, name: &str, bytes: &[u8], mode: u32| {
            let fp = dir.join(name);
            std::fs::create_dir_all(fp.parent().unwrap()).unwrap();
            std::fs::write(&fp, bytes).unwrap();
            if mode == MODE_EXEC {
                let mut perm = std::fs::metadata(&fp).unwrap().permissions();
                perm.set_mode(0o755);
                std::fs::set_permissions(&fp, perm).unwrap();
            }
        };
        let script = b"#!/bin/sh\necho hi\n";
        let (base_hex, head_hex) = {
            let store = host.store("t", "r", true).unwrap().unwrap();
            // ancestor: a plain (non-exec) script + a text file.
            let mb_dir = tmp.join("mb");
            write_mode(&mb_dir, "script.sh", script, MODE_FILE);
            write_mode(&mb_dir, "other.txt", b"1\n", MODE_FILE);
            let mb = commit(&store, vec![], snapshot_uncached(&store, &mb_dir).unwrap());
            // base (ours): edits other.txt; leaves script.sh plain.
            let base_dir = tmp.join("base");
            write_mode(&base_dir, "script.sh", script, MODE_FILE);
            write_mode(&base_dir, "other.txt", b"2\n", MODE_FILE);
            let base = commit(&store, vec![mb], snapshot_uncached(&store, &base_dir).unwrap());
            // head (theirs): same script content but now EXECUTABLE, plus a NEW symlink `link -> other.txt`.
            let head_dir = tmp.join("head");
            write_mode(&head_dir, "script.sh", script, MODE_EXEC);
            write_mode(&head_dir, "other.txt", b"1\n", MODE_FILE);
            std::os::unix::fs::symlink("other.txt", head_dir.join("link")).unwrap();
            let head = commit(&store, vec![mb], snapshot_uncached(&store, &head_dir).unwrap());
            (base.to_hex(), head.to_hex())
        };
        let tree_hex = match host.plan_merge("t", "r", "main", Some(&base_hex), &head_hex) {
            Ok(MergeOutcome::Tree { tree, .. }) => tree,
            other => panic!("expected a synthesized 3-way tree, got AlreadyMerged={}", matches!(other, Ok(MergeOutcome::AlreadyMerged))),
        };
        let store = host.store("t", "r", false).unwrap().unwrap();
        let modes = tree_modes(&store, &tree_hex);
        assert_eq!(modes.get("script.sh"), Some(&MODE_EXEC), "the merged script keeps its +x bit");
        assert_eq!(modes.get("link"), Some(&MODE_SYMLINK), "the merged symlink stays a symlink, not a regular file");
        let c = tree_contents(&store, &tree_hex);
        assert_eq!(c.get("other.txt").map(String::as_str), Some("2\n"), "base's edit to other.txt survives");
        assert_eq!(c.get("link").map(String::as_str), Some("other.txt"), "the symlink's content is its target path");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // LOW hardening: a base tree whose directory-position entry is a SYMLINK must not let a later
    // merged write traverse it and escape the checkout dir. `materialize_merge` rejects the merge.
    #[cfg(unix)]
    #[test]
    fn materialize_merge_rejects_symlinked_path_prefix() {
        use keel_store::snapshot::{snapshot_uncached, MODE_FILE};
        let (tmp, host) = merge_host("symlinkprefix");
        let store = host.store("t", "r", true).unwrap().unwrap();
        // `escape` is an out-of-checkout directory the symlink points at; nothing may be written into it.
        let escape = tmp.join("escape-target");
        std::fs::create_dir_all(&escape).unwrap();
        // Base tree: a real file plus `d`, a symlink standing in a directory position.
        let base_dir = tmp.join("base");
        std::fs::create_dir_all(&base_dir).unwrap();
        std::fs::write(base_dir.join("keep.txt"), "1\n").unwrap();
        std::os::unix::fs::symlink(&escape, base_dir.join("d")).unwrap();
        let base_tree = snapshot_uncached(&store, &base_dir).unwrap();
        // An edit that writes UNDER the symlinked prefix `d/…`.
        let blob = store.put(&Object::Blob(b"pwned\n".to_vec())).unwrap();
        let edits = vec![("d/inner.txt".to_string(), Some((blob, MODE_FILE)))];
        let res = host.materialize_merge(&store, base_tree, &edits);
        assert!(res.is_err(), "a merge write through a symlinked prefix must be rejected");
        assert!(!escape.join("inner.txt").exists(), "nothing escaped into the symlink target");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // MED 2: a criss-cross history with two INCOMPARABLE merge bases must be rejected, not silently
    // merged against one arbitrarily-chosen base.
    #[test]
    fn plan_merge_rejects_multiple_incomparable_merge_bases() {
        let (tmp, host) = merge_host("crisscross");
        let (base_hex, head_hex) = {
            let store = host.store("t", "r", true).unwrap().unwrap();
            // o1, o2 are two independent roots (neither an ancestor of the other). base and head each
            // descend from BOTH → their common ancestors are {o1, o2}, two incomparable merge bases.
            let o1 = commit(&store, vec![], tree_from(&store, &tmp, "o1", &[("a.txt", "1\n")]));
            let o2 = commit(&store, vec![], tree_from(&store, &tmp, "o2", &[("b.txt", "1\n")]));
            let base = commit(&store, vec![o1, o2], tree_from(&store, &tmp, "base", &[("a.txt", "1\n"), ("b.txt", "1\n"), ("c.txt", "base\n")]));
            let head = commit(&store, vec![o1, o2], tree_from(&store, &tmp, "head", &[("a.txt", "1\n"), ("b.txt", "1\n"), ("d.txt", "head\n")]));
            (base.to_hex(), head.to_hex())
        };
        match host.plan_merge("t", "r", "main", Some(&base_hex), &head_hex) {
            Err(MergeError::Conflict(m)) => assert!(m.contains("multiple merge bases"), "message flags complex history: {m}"),
            other => panic!("expected a complex-history conflict; got ok={}", other.is_ok()),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn land_merge_advances_branch_by_cas_and_conflicts_on_stale_base() {
        let (tmp, host) = merge_host("land");
        let (base_hex, head_hex, merged_tree_hex) = {
            let store = host.store("t", "r", true).unwrap().unwrap();
            let base = commit(&store, vec![], tree_from(&store, &tmp, "base", &[("a.txt", "1\n")]));
            store.set_ref("main", &base).unwrap();
            let head = commit(&store, vec![base], tree_from(&store, &tmp, "head", &[("a.txt", "1\n"), ("b.txt", "2\n")]));
            let head_tree = match store.get(&head).unwrap().unwrap() {
                Object::Change(c) => c.tree.to_hex(),
                _ => unreachable!(),
            };
            (base.to_hex(), head.to_hex(), head_tree)
        };
        // Landing from the correct base advances main to a two-parent merge change.
        let landed = host
            .land_merge("t", "r", "main", Some(&base_hex), &head_hex, &merged_tree_hex, true, "Merge PR !1", "alice", 0)
            .expect("land ok");
        let landed = landed.expect("committed");
        let store = host.store("t", "r", false).unwrap().unwrap();
        assert_eq!(store.get_ref("main").unwrap().unwrap().to_hex(), landed, "main advanced to the merge change");
        match store.get(&ObjectId::from_hex(&landed).unwrap()).unwrap().unwrap() {
            Object::Change(c) => {
                assert_eq!(c.parents.len(), 2, "a merge change has two parents");
                assert_eq!(c.parents[0].to_hex(), base_hex);
                assert_eq!(c.parents[1].to_hex(), head_hex);
            }
            _ => panic!("landed object is not a change"),
        }
        // A second land from the now-STALE base is a CAS miss → None (caller re-plans + retries).
        let stale = host
            .land_merge("t", "r", "main", Some(&base_hex), &head_hex, &merged_tree_hex, true, "Merge PR !1 again", "alice", 0)
            .expect("land ok");
        assert!(stale.is_none(), "a stale-base land is rejected by CAS, not clobbered");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── git-mirror export of gated merges ────────────────────────────────────────────────────────
    // Build a git blob in the mirror, returning its git oid.
    fn gblob(store: &Store, content: &[u8]) -> keel_git::Oid {
        let oid = keel_git::hash(keel_git::Kind::Blob, content);
        keel_git::mirror::ingest_objects(store, &[(keel_git::Kind::Blob, content.to_vec())]).unwrap();
        oid
    }
    // Compute a git tree oid from `(mode, name, child-oid)` entries WITHOUT touching a store — the
    // canonical git-side answer to compare the exporter against.
    fn gtree_oid(entries: &[(&str, &str, keel_git::Oid)]) -> keel_git::Oid {
        let mut te: Vec<keel_git::TreeEntry> = entries
            .iter()
            .map(|(m, n, o)| keel_git::TreeEntry { mode: m.as_bytes().to_vec(), name: n.as_bytes().to_vec(), oid: *o })
            .collect();
        keel_git::sort_tree(&mut te);
        keel_git::hash(keel_git::Kind::Tree, &keel_git::serialize_tree(&te))
    }
    // Build (and ingest) a git tree in the mirror from `(mode, name, child-oid)` entries.
    fn gtree(store: &Store, entries: &[(&str, &str, keel_git::Oid)]) -> keel_git::Oid {
        let mut te: Vec<keel_git::TreeEntry> = entries
            .iter()
            .map(|(m, n, o)| keel_git::TreeEntry { mode: m.as_bytes().to_vec(), name: n.as_bytes().to_vec(), oid: *o })
            .collect();
        keel_git::sort_tree(&mut te);
        let payload = keel_git::serialize_tree(&te);
        let oid = keel_git::hash(keel_git::Kind::Tree, &payload);
        keel_git::mirror::ingest_objects(store, &[(keel_git::Kind::Tree, payload)]).unwrap();
        oid
    }
    // Build (and ingest) a git commit in the mirror.
    fn gcommit(store: &Store, tree: keel_git::Oid, parents: &[keel_git::Oid], msg: &str) -> keel_git::Oid {
        let mut headers = Vec::new();
        headers.extend_from_slice(format!("tree {}\n", tree.to_hex()).as_bytes());
        for p in parents {
            headers.extend_from_slice(format!("parent {}\n", p.to_hex()).as_bytes());
        }
        headers.extend_from_slice(b"author A <a@e> 1700000000 +0000\n");
        headers.extend_from_slice(b"committer A <a@e> 1700000000 +0000\n");
        let payload = keel_git::serialize_headed(&keel_git::Headed { headers, message: format!("{msg}\n").into_bytes() });
        let oid = keel_git::hash(keel_git::Kind::Commit, &payload);
        keel_git::mirror::ingest_objects(store, &[(keel_git::Kind::Commit, payload)]).unwrap();
        oid
    }
    // The value of a git ref in the mirror.
    fn mirror_ref(store: &Store, name: &str) -> Option<String> {
        keel_git::mirror::refs(store).unwrap().into_iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
    // Recursively resolve a git tree in the mirror to `path -> blob-content`.
    fn git_tree_contents(store: &Store, tree: &keel_git::Oid, prefix: &str, out: &mut std::collections::BTreeMap<String, String>) {
        let (kind, payload) = keel_git::mirror::get_object(store, tree).unwrap().unwrap();
        assert_eq!(kind, keel_git::Kind::Tree);
        for e in keel_git::parse_tree(&payload).unwrap() {
            let name = String::from_utf8_lossy(&e.name).into_owned();
            let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            if e.mode == b"40000" {
                git_tree_contents(store, &e.oid, &path, out);
            } else {
                let (_, bytes) = keel_git::mirror::get_object(store, &e.oid).unwrap().unwrap();
                out.insert(path, String::from_utf8_lossy(&bytes).into_owned());
            }
        }
    }
    // Reverse a git-commit-oid to its keel change id via the forward `gchange` map.
    fn keel_for_git(store: &Store, goid: &keel_git::Oid) -> ObjectId {
        let v = store.aux_get("gchange", goid.as_bytes()).unwrap().unwrap();
        let mut a = [0u8; 32];
        a.copy_from_slice(&v);
        ObjectId(a)
    }

    // UNIT: the keel-tree → git-tree exporter must produce the CANONICAL git tree oid — the exact oid a
    // real git client computes for the same content/modes. Covers nested dirs and the exec-bit inversion.
    #[test]
    fn export_keel_tree_reproduces_canonical_git_tree_oid() {
        let (tmp, host) = merge_host("exporttree");
        let store = host.store("t", "r", true).unwrap().unwrap();
        // A keel tree with a plain file, a nested dir, and (unix) an executable — built the normal way.
        #[cfg(unix)]
        let keel_tree = {
            use keel_store::snapshot::snapshot_uncached;
            use std::os::unix::fs::PermissionsExt;
            let dir = tmp.join("work");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("sub")).unwrap();
            std::fs::write(dir.join("a.txt"), "1\n").unwrap();
            std::fs::write(dir.join("sub/b.txt"), "2\n").unwrap();
            std::fs::write(dir.join("run.sh"), "#!/bin/sh\n").unwrap();
            let mut perm = std::fs::metadata(dir.join("run.sh")).unwrap().permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(dir.join("run.sh"), perm).unwrap();
            snapshot_uncached(&store, &dir).unwrap()
        };
        #[cfg(not(unix))]
        let keel_tree = tree_from(&store, &tmp, "work", &[("a.txt", "1\n"), ("sub/b.txt", "2\n")]);

        let got = RepoHost::export_keel_tree_to_git(&store, keel_tree).unwrap();

        // Independently compute the git oid the same content/modes must hash to.
        let ba = keel_git::hash(keel_git::Kind::Blob, b"1\n");
        let bb = keel_git::hash(keel_git::Kind::Blob, b"2\n");
        let sub = gtree_oid(&[("100644", "b.txt", bb)]);
        #[cfg(unix)]
        let expected = {
            let brun = keel_git::hash(keel_git::Kind::Blob, b"#!/bin/sh\n");
            gtree_oid(&[("100644", "a.txt", ba), ("100755", "run.sh", brun), ("40000", "sub", sub)])
        };
        #[cfg(not(unix))]
        let expected = gtree_oid(&[("100644", "a.txt", ba), ("40000", "sub", sub)]);
        assert_eq!(got, expected, "exporter must yield the canonical git tree oid (a real client's answer)");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // INTEGRATION (fast-forward): a gated land whose merge carries the head's tree verbatim must point
    // git `refs/heads/main` at the head's PRE-EXISTING git commit — no new object, no rewritten history.
    #[test]
    fn gated_merge_fast_forward_points_git_main_at_existing_head_commit() {
        let (tmp, host) = merge_host("ffexport");
        let store = host.store("t", "r", true).unwrap().unwrap();
        // A git base commit and a git head commit that fast-forwards it (adds b.txt). Both are already
        // in the mirror + bridged to keel — exactly the post-push state.
        let base_git = gcommit(&store, gtree(&store, &[("100644", "a.txt", gblob(&store, b"1\n"))]), &[], "base");
        let head_git = gcommit(
            &store,
            gtree(&store, &[("100644", "a.txt", gblob(&store, b"1\n")), ("100644", "b.txt", gblob(&store, b"2\n"))]),
            &[base_git],
            "head",
        );
        keel_git::mirror::set_ref(&store, "refs/heads/main", &base_git.to_hex()).unwrap();
        keel_git::bridge::bridge(&store).unwrap();
        let base_keel = keel_for_git(&store, &base_git);
        let head_keel = keel_for_git(&store, &head_git);
        store.set_ref("main", &base_keel).unwrap();

        // Drive the real merge-queue path: plan then land (land triggers the export).
        let (tree, ff) = match host.plan_merge("t", "r", "main", Some(&base_keel.to_hex()), &head_keel.to_hex()) {
            Ok(MergeOutcome::Tree { tree, fast_forward }) => (tree, fast_forward),
            other => panic!("expected a FF Tree outcome: {}", matches!(other, Ok(MergeOutcome::AlreadyMerged))),
        };
        assert!(ff, "base ∈ ancestors(head) plans a fast-forward");
        host.land_merge("t", "r", "main", Some(&base_keel.to_hex()), &head_keel.to_hex(), &tree, ff, "Merge PR !1", "alice", 0)
            .unwrap()
            .expect("committed");

        assert_eq!(
            mirror_ref(&store, "refs/heads/main").as_deref(),
            Some(head_git.to_hex().as_str()),
            "FF advances git main straight to the head's EXISTING commit (no synthesis)"
        );
        assert_eq!(mirror_ref(&store, "HEAD").as_deref(), Some("ref: refs/heads/main"), "HEAD symref present for clone");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // INTEGRATION (non-fast-forward): a gated land of two divergent branches must synthesize ONE new git
    // merge commit whose PARENTS are the pre-existing git commits (base git tip + head's pushed commit)
    // and whose TREE matches the merged content. This is the history-divergence guard, proven.
    #[test]
    fn gated_merge_non_ff_synthesizes_two_parent_commit_reusing_existing_parents() {
        let (tmp, host) = merge_host("nffexport");
        let store = host.store("t", "r", true).unwrap().unwrap();
        // mergebase a=1,b=1; base edits a→2 (on main); head edits b→2 (feature). Divergent, clean merge.
        let mb_git = gcommit(
            &store,
            gtree(&store, &[("100644", "a.txt", gblob(&store, b"1\n")), ("100644", "b.txt", gblob(&store, b"1\n"))]),
            &[],
            "mb",
        );
        let base_git = gcommit(
            &store,
            gtree(&store, &[("100644", "a.txt", gblob(&store, b"2\n")), ("100644", "b.txt", gblob(&store, b"1\n"))]),
            &[mb_git],
            "base",
        );
        let head_git = gcommit(
            &store,
            gtree(&store, &[("100644", "a.txt", gblob(&store, b"1\n")), ("100644", "b.txt", gblob(&store, b"2\n"))]),
            &[mb_git],
            "head",
        );
        keel_git::mirror::set_ref(&store, "refs/heads/main", &base_git.to_hex()).unwrap();
        keel_git::mirror::set_ref(&store, "HEAD", "ref: refs/heads/main").unwrap();
        keel_git::bridge::bridge(&store).unwrap();
        let base_keel = keel_for_git(&store, &base_git);
        let head_keel = keel_for_git(&store, &head_git);
        store.set_ref("main", &base_keel).unwrap();

        let (tree, ff) = match host.plan_merge("t", "r", "main", Some(&base_keel.to_hex()), &head_keel.to_hex()) {
            Ok(MergeOutcome::Tree { tree, fast_forward }) => (tree, fast_forward),
            other => panic!("expected a synthesized 3-way Tree: {}", matches!(other, Ok(MergeOutcome::AlreadyMerged))),
        };
        assert!(!ff, "divergent branches plan a non-fast-forward");
        let landed = host
            .land_merge("t", "r", "main", Some(&base_keel.to_hex()), &head_keel.to_hex(), &tree, ff, "Merge PR !7: land", "alice", 0)
            .unwrap()
            .expect("committed");

        // git main now points at a NEW merge commit (neither pre-existing parent).
        let main_hex = mirror_ref(&store, "refs/heads/main").expect("git main set");
        assert_ne!(main_hex, base_git.to_hex(), "a divergent land is not a FF to base");
        assert_ne!(main_hex, head_git.to_hex(), "a divergent land is not a FF to head");
        let merge_oid = keel_git::Oid::from_hex_str(&main_hex).unwrap();
        let (kind, payload) = keel_git::mirror::get_object(&store, &merge_oid).unwrap().unwrap();
        assert_eq!(kind, keel_git::Kind::Commit, "the new git main is a commit");
        let headed = keel_git::Headed::parse(&payload);

        // THE KEY INVARIANT: parents REUSE the pre-existing git commits — history is not rewritten.
        assert_eq!(
            headed.parents().unwrap(),
            vec![base_git, head_git],
            "merge parents must be the pre-existing git commits (base tip, pushed head), never re-synthesized"
        );

        // The synthesized tree matches the merged content (base's a=2 AND head's b=2).
        let mut contents = std::collections::BTreeMap::new();
        git_tree_contents(&store, &headed.tree().unwrap(), "", &mut contents);
        assert_eq!(contents.get("a.txt").map(String::as_str), Some("2\n"), "base's edit survives in the git merge tree");
        assert_eq!(contents.get("b.txt").map(String::as_str), Some("2\n"), "head's edit survives in the git merge tree");

        // Forward gchange stays consistent: the new git commit maps to the landed keel change.
        assert_eq!(keel_for_git(&store, &merge_oid).to_hex(), landed, "gchange maps the synthesized commit → landed keel change");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // CRITICAL-1 (FF only on TRUE ancestry): a NO-OP base (its tree equals the merge-base's) makes the
    // synthesized 3-way tree equal the HEAD's tree, yet the land is divergent — head_git is a SIBLING of
    // the git tip, not a descendant. The old tree-equality FF inference would point git `main` at head_git
    // (a non-fast-forward move that drops base history). The plan-threaded ancestry flag must instead take
    // the non-FF 2-parent path, preserving base_git as a parent.
    #[test]
    fn ff_shortcut_not_taken_for_noop_base_even_when_merged_tree_equals_head() {
        let (tmp, host) = merge_host("noopbaseff");
        let store = host.store("t", "r", true).unwrap().unwrap();
        // mb: a=1. base: a NO-OP commit off mb (same tree). head: a=1,b=2 off mb (adds b). base & head are
        // siblings; the 3-way merge of (base unchanged from mb) with (head adds b) yields exactly head's tree.
        let mb_tree = gtree(&store, &[("100644", "a.txt", gblob(&store, b"1\n"))]);
        let mb_git = gcommit(&store, mb_tree, &[], "mb");
        let base_git = gcommit(&store, mb_tree, &[mb_git], "noop base"); // tree IDENTICAL to mb
        let head_git = gcommit(
            &store,
            gtree(&store, &[("100644", "a.txt", gblob(&store, b"1\n")), ("100644", "b.txt", gblob(&store, b"2\n"))]),
            &[mb_git],
            "head",
        );
        keel_git::mirror::set_ref(&store, "refs/heads/main", &base_git.to_hex()).unwrap();
        keel_git::mirror::set_ref(&store, "HEAD", "ref: refs/heads/main").unwrap();
        keel_git::bridge::bridge(&store).unwrap();
        let base_keel = keel_for_git(&store, &base_git);
        let head_keel = keel_for_git(&store, &head_git);
        store.set_ref("main", &base_keel).unwrap();

        // The plan MUST report a non-fast-forward even though the merged tree equals the head's tree.
        let (tree, ff) = match host.plan_merge("t", "r", "main", Some(&base_keel.to_hex()), &head_keel.to_hex()) {
            Ok(MergeOutcome::Tree { tree, fast_forward }) => (tree, fast_forward),
            other => panic!("expected a divergent Tree, got AlreadyMerged={}", matches!(other, Ok(MergeOutcome::AlreadyMerged))),
        };
        assert!(!ff, "a no-op-base land is NOT a fast-forward, even though its merged tree equals the head's");
        let head_tree_keel = match store.get(&head_keel).unwrap().unwrap() {
            Object::Change(c) => c.tree.to_hex(),
            _ => unreachable!(),
        };
        assert_eq!(tree, head_tree_keel, "the merged tree coincidentally equals the head's tree (the trap)");

        let landed = host
            .land_merge("t", "r", "main", Some(&base_keel.to_hex()), &head_keel.to_hex(), &tree, ff, "Merge PR !1", "alice", 0)
            .unwrap()
            .expect("committed");

        // git main must be a NEW 2-parent merge over [base_git, head_git] — NOT a non-FF jump to head_git.
        let main_hex = mirror_ref(&store, "refs/heads/main").expect("git main set");
        assert_ne!(main_hex, head_git.to_hex(), "must NOT non-fast-forward git main to head's sibling commit (the CRITICAL-1 bug)");
        assert_ne!(main_hex, base_git.to_hex(), "the ref advanced (a merge was synthesized)");
        let merge_oid = keel_git::Oid::from_hex_str(&main_hex).unwrap();
        let (_, payload) = keel_git::mirror::get_object(&store, &merge_oid).unwrap().unwrap();
        assert_eq!(
            keel_git::Headed::parse(&payload).parents().unwrap(),
            vec![base_git, head_git],
            "base_git is preserved as parent[0] — base history is not dropped"
        );
        assert_eq!(keel_for_git(&store, &merge_oid).to_hex(), landed, "gchange maps the synthesized commit → landed change");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // CRITICAL-1 belt-and-suspenders: even if a caller forces `fast_forward = true`, the export must
    // verify in the GIT graph that head_git descends from the current tip before moving the ref. Here
    // head_git is a sibling of the tip, so the descendant check fails and the export falls back to a
    // non-FF 2-parent synthesis rather than a non-fast-forward ref move.
    #[test]
    fn export_ff_falls_back_when_git_graph_denies_descent() {
        let (tmp, host) = merge_host("ffdescent");
        let store = host.store("t", "r", true).unwrap().unwrap();
        let mb_tree = gtree(&store, &[("100644", "a.txt", gblob(&store, b"1\n"))]);
        let mb_git = gcommit(&store, mb_tree, &[], "mb");
        let base_git = gcommit(&store, mb_tree, &[mb_git], "base"); // git tip
        let head_git = gcommit(
            &store,
            gtree(&store, &[("100644", "a.txt", gblob(&store, b"1\n")), ("100644", "b.txt", gblob(&store, b"2\n"))]),
            &[mb_git],
            "head",
        ); // SIBLING of base_git, not a descendant
        keel_git::mirror::set_ref(&store, "refs/heads/main", &base_git.to_hex()).unwrap();
        keel_git::mirror::set_ref(&store, "HEAD", "ref: refs/heads/main").unwrap();
        keel_git::bridge::bridge(&store).unwrap();
        let base_keel = keel_for_git(&store, &base_git);
        let head_keel = keel_for_git(&store, &head_git);
        store.set_ref("main", &base_keel).unwrap();
        let head_tree_keel = match store.get(&head_keel).unwrap().unwrap() {
            Object::Change(c) => c.tree.to_hex(),
            _ => unreachable!(),
        };

        // Force fast_forward = true — the adversarial input the belt-and-suspenders defends against.
        host.land_merge("t", "r", "main", Some(&base_keel.to_hex()), &head_keel.to_hex(), &head_tree_keel, true, "Merge PR !1", "alice", 0)
            .unwrap()
            .expect("committed");

        let main_hex = mirror_ref(&store, "refs/heads/main").expect("git main set");
        assert_ne!(main_hex, head_git.to_hex(), "the git-graph descendant check blocks the non-FF ref move");
        let merge_oid = keel_git::Oid::from_hex_str(&main_hex).unwrap();
        let (_, payload) = keel_git::mirror::get_object(&store, &merge_oid).unwrap().unwrap();
        assert_eq!(
            keel_git::Headed::parse(&payload).parents().unwrap(),
            vec![base_git, head_git],
            "fell back to non-FF synthesis over [tip, head], preserving the tip as a parent"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // CRITICAL-2 (land+export serialized with keel commit order): two SEQUENTIAL lands to the same
    // protected branch must leave git `main` at the SECOND merge, containing BOTH landed changes — never
    // regressing to the first. This exercises the export chaining that the per-branch land lock protects
    // under concurrency (the lock orders concurrent lands into exactly this sequential shape).
    #[test]
    fn sequential_lands_leave_git_main_at_second_merge_with_both_changes() {
        let (tmp, host) = merge_host("seqlands");
        let store = host.store("t", "r", true).unwrap().unwrap();
        // b0 is the branch base. h1 (adds f1) fast-forwards it; h2 (adds f2) diverges off b0.
        let b0_git = gcommit(&store, gtree(&store, &[("100644", "a.txt", gblob(&store, b"1\n"))]), &[], "b0");
        let h1_git = gcommit(
            &store,
            gtree(&store, &[("100644", "a.txt", gblob(&store, b"1\n")), ("100644", "f1.txt", gblob(&store, b"1\n"))]),
            &[b0_git],
            "h1",
        );
        let h2_git = gcommit(
            &store,
            gtree(&store, &[("100644", "a.txt", gblob(&store, b"1\n")), ("100644", "f2.txt", gblob(&store, b"1\n"))]),
            &[b0_git],
            "h2",
        );
        keel_git::mirror::set_ref(&store, "refs/heads/main", &b0_git.to_hex()).unwrap();
        keel_git::mirror::set_ref(&store, "HEAD", "ref: refs/heads/main").unwrap();
        keel_git::bridge::bridge(&store).unwrap();
        let b0_keel = keel_for_git(&store, &b0_git);
        let h1_keel = keel_for_git(&store, &h1_git);
        let h2_keel = keel_for_git(&store, &h2_git);
        store.set_ref("main", &b0_keel).unwrap();

        // Land 1 (h1): a fast-forward. git main → h1's existing commit.
        let (t1, ff1) = match host.plan_merge("t", "r", "main", Some(&b0_keel.to_hex()), &h1_keel.to_hex()) {
            Ok(MergeOutcome::Tree { tree, fast_forward }) => (tree, fast_forward),
            other => panic!("land1 expected Tree, AlreadyMerged={}", matches!(other, Ok(MergeOutcome::AlreadyMerged))),
        };
        let mc1 = host
            .land_merge("t", "r", "main", Some(&b0_keel.to_hex()), &h1_keel.to_hex(), &t1, ff1, "Merge PR !1", "alice", 0)
            .unwrap()
            .expect("land1 committed");
        assert_eq!(mirror_ref(&store, "refs/heads/main").as_deref(), Some(h1_git.to_hex().as_str()), "land1 FF's git main to h1");

        // Land 2 (h2): diverges from the new keel tip (mc1). Non-FF synthesis chaining off h1's commit.
        let (t2, ff2) = match host.plan_merge("t", "r", "main", Some(&mc1), &h2_keel.to_hex()) {
            Ok(MergeOutcome::Tree { tree, fast_forward }) => (tree, fast_forward),
            other => panic!("land2 expected Tree, AlreadyMerged={}", matches!(other, Ok(MergeOutcome::AlreadyMerged))),
        };
        assert!(!ff2, "land2 is a divergent 3-way");
        let mc2 = host
            .land_merge("t", "r", "main", Some(&mc1), &h2_keel.to_hex(), &t2, ff2, "Merge PR !2", "alice", 0)
            .unwrap()
            .expect("land2 committed");

        // git main ends at the SECOND merge — NOT regressed to h1 — and its tree carries BOTH f1 and f2.
        let main_hex = mirror_ref(&store, "refs/heads/main").expect("git main set");
        assert_ne!(main_hex, h1_git.to_hex(), "git main must NOT regress to the first land");
        assert_ne!(main_hex, h2_git.to_hex(), "the second land is a synthesized merge, not a FF to h2");
        let merge_oid = keel_git::Oid::from_hex_str(&main_hex).unwrap();
        let (_, payload) = keel_git::mirror::get_object(&store, &merge_oid).unwrap().unwrap();
        let headed = keel_git::Headed::parse(&payload);
        assert_eq!(headed.parents().unwrap(), vec![h1_git, h2_git], "the second merge chains off the first land's tip (h1) and h2");
        let mut contents = std::collections::BTreeMap::new();
        git_tree_contents(&store, &headed.tree().unwrap(), "", &mut contents);
        assert_eq!(contents.get("f1.txt").map(String::as_str), Some("1\n"), "the FIRST landed change survives in git main");
        assert_eq!(contents.get("f2.txt").map(String::as_str), Some("1\n"), "the SECOND landed change is present in git main");
        assert_eq!(keel_for_git(&store, &merge_oid).to_hex(), mc2, "gchange maps the second merge → its keel change");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // MED-HIGH-3 (never regress content): keel's bridge cannot faithfully round-trip a `120000` symlink
    // (it folds to a plain file). When a non-FF land's parent tree carries one, the exporter must SKIP —
    // leaving git `main` stale (pre-feature behavior) rather than writing an unfaithful merge tree that
    // silently turns the symlink into a regular file. The keel land itself still succeeds.
    #[test]
    fn non_ff_export_skipped_when_parent_tree_has_symlink() {
        let (tmp, host) = merge_host("symlinkskip");
        let store = host.store("t", "r", true).unwrap().unwrap();
        let target = gblob(&store, b"a.txt"); // a symlink's blob is its target path
        // mb: a=1 + symlink `link`. base edits a→2 (main). head adds b=2 (feature). Divergent, clean.
        let mb_git = gcommit(
            &store,
            gtree(&store, &[("100644", "a.txt", gblob(&store, b"1\n")), ("120000", "link", target)]),
            &[],
            "mb",
        );
        let base_git = gcommit(
            &store,
            gtree(&store, &[("100644", "a.txt", gblob(&store, b"2\n")), ("120000", "link", target)]),
            &[mb_git],
            "base",
        );
        let head_git = gcommit(
            &store,
            gtree(&store, &[("100644", "a.txt", gblob(&store, b"1\n")), ("100644", "b.txt", gblob(&store, b"2\n")), ("120000", "link", target)]),
            &[mb_git],
            "head",
        );
        keel_git::mirror::set_ref(&store, "refs/heads/main", &base_git.to_hex()).unwrap();
        keel_git::mirror::set_ref(&store, "HEAD", "ref: refs/heads/main").unwrap();
        keel_git::bridge::bridge(&store).unwrap();
        let base_keel = keel_for_git(&store, &base_git);
        let head_keel = keel_for_git(&store, &head_git);
        store.set_ref("main", &base_keel).unwrap();

        let (tree, ff) = match host.plan_merge("t", "r", "main", Some(&base_keel.to_hex()), &head_keel.to_hex()) {
            Ok(MergeOutcome::Tree { tree, fast_forward }) => (tree, fast_forward),
            other => panic!("expected a divergent Tree, AlreadyMerged={}", matches!(other, Ok(MergeOutcome::AlreadyMerged))),
        };
        assert!(!ff, "divergent land");
        let landed = host
            .land_merge("t", "r", "main", Some(&base_keel.to_hex()), &head_keel.to_hex(), &tree, ff, "Merge PR !1", "alice", 0)
            .unwrap()
            .expect("keel land still succeeds");

        // The export was SKIPPED: git main is unchanged (still base_git), never an unfaithful merge tree.
        assert_eq!(
            mirror_ref(&store, "refs/heads/main").as_deref(),
            Some(base_git.to_hex().as_str()),
            "git main left stale (export skipped) — the symlink is not regressed to a regular file"
        );
        // But the keel land committed and advanced native `main`.
        assert_eq!(store.get_ref("main").unwrap().unwrap().to_hex(), landed, "keel main advanced to the landed merge change");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Build a single hunk from `(tag, text)` lines for driving `semantic_ops` directly.
    fn hunk(lines: &[(Tag, &str)]) -> keel_store::Hunk {
        keel_store::Hunk {
            old_start: 0,
            old_len: 0,
            new_start: 0,
            new_len: 0,
            lines: lines.iter().map(|(t, s)| keel_store::DiffLine { tag: *t, text: (*s).to_string() }).collect(),
        }
    }

    #[test]
    fn semantic_ops_detects_rust_definitions() {
        let h = hunk(&[
            (Tag::Add, "pub fn verify(x: u32) -> bool {"),
            (Tag::Add, "async fn spawn() {"),
            (Tag::Add, "pub struct Config {"),
            (Tag::Add, "enum State {"),
            (Tag::Add, "trait Runner {"),
            (Tag::Add, "type Id = u64;"),
            (Tag::Add, "use std::io::Read;"),
            (Tag::Del, "fn old_impl() {"),
        ]);
        let ops = semantic_ops(&[h]);
        assert!(ops.contains(&"added fn `verify`".to_string()));
        assert!(ops.contains(&"added fn `spawn`".to_string()), "async prefix is stripped");
        assert!(ops.contains(&"added struct `Config`".to_string()));
        assert!(ops.contains(&"added enum `State`".to_string()));
        assert!(ops.contains(&"added trait `Runner`".to_string()));
        assert!(ops.contains(&"added type `Id`".to_string()));
        assert!(ops.contains(&"added import".to_string()));
        assert!(ops.contains(&"removed fn `old_impl`".to_string()), "a deleted def is a removed op");
    }

    #[test]
    fn semantic_ops_detects_ts_js_forms_including_arrow_fns() {
        let h = hunk(&[
            (Tag::Add, "export function handleClick() {"),
            (Tag::Add, "const Widget = (props) => {"),       // uppercase ⇒ component
            (Tag::Add, "const add = (a, b) => a + b"),        // lowercase ⇒ fn
            (Tag::Add, "let load = async () => {}"),          // `= async` ⇒ fn
            (Tag::Add, "const [count, setCount] = useState(0)"), // destructured hook ⇒ state
            (Tag::Add, "interface Opts {"),
            (Tag::Add, "import { x } from 'y'"),
        ]);
        let ops = semantic_ops(&[h]);
        assert!(ops.contains(&"added fn `handleClick`".to_string()));
        assert!(ops.contains(&"added component `Widget`".to_string()), "Uppercase arrow-fn ⇒ component");
        assert!(ops.contains(&"added fn `add`".to_string()), "lowercase arrow-fn ⇒ fn");
        assert!(ops.contains(&"added fn `load`".to_string()));
        assert!(ops.contains(&"added state `count`".to_string()));
        assert!(ops.contains(&"added interface `Opts`".to_string()));
        assert!(ops.contains(&"added import".to_string()));
    }

    #[test]
    fn semantic_ops_detects_python_and_css() {
        let py = hunk(&[
            (Tag::Add, "def compute(n):"),
            (Tag::Add, "class Model:"),
            (Tag::Add, "from os import path"),
            (Tag::Add, "import sys"),
        ]);
        let ops = semantic_ops(&[py]);
        assert!(ops.contains(&"added fn `compute`".to_string()), "python def ⇒ fn");
        assert!(ops.contains(&"added class `Model`".to_string()));
        // both `from …` and `import …` are imports (deduped to one entry).
        assert_eq!(ops.iter().filter(|o| *o == "added import").count(), 1);

        let css = hunk(&[
            (Tag::Add, ".btn {"),
            (Tag::Add, "#header {"),
            (Tag::Add, ".card { color: red; }"), // single-line rule
        ]);
        let cops = semantic_ops(&[css]);
        assert!(cops.contains(&"added style `.btn`".to_string()));
        assert!(cops.contains(&"added style `#header`".to_string()));
        assert!(cops.contains(&"added style `.card`".to_string()));
    }

    #[test]
    fn semantic_ops_does_not_hallucinate_on_non_definition_lines() {
        let h = hunk(&[
            (Tag::Add, "let x = 5;"),          // plain binding, not an arrow-fn
            (Tag::Add, "return compute(y);"),
            (Tag::Add, "// a comment"),
            (Tag::Add, "    total += 1"),
            (Tag::Context, "fn should_be_ignored() {"), // context lines never count
            (Tag::Del, "x += 1;"),
        ]);
        assert!(semantic_ops(&[h]).is_empty(), "no ops from ordinary statements or context lines");
    }

    #[test]
    fn semantic_ops_sorts_and_dedups() {
        let h = hunk(&[
            (Tag::Add, "fn dup() {"),
            (Tag::Add, "fn dup() {"), // identical ⇒ one op
            (Tag::Add, "fn apex() {"),
        ]);
        let ops = semantic_ops(&[h]);
        assert_eq!(ops, vec!["added fn `apex`".to_string(), "added fn `dup`".to_string()], "sorted + deduped");
    }

    #[test]
    fn independence_tree_restores_modified_tests_and_drops_added_ones() {
        // A change that weakens its own test (and adds a new passing one) must not verify itself:
        // the composed independence tree restores the *parent's* test and drops the newly-added one.
        let tmp = std::env::temp_dir().join(format!("hull-indep-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("t/r/.keel/store")).unwrap();
        let host = RepoHost::new(&tmp);
        let composed_hex = {
            let store = host.store("t", "r", true).unwrap().unwrap();
            // Parent: real code + a strict test.
            let parent_tree = tree_from(&store, &tmp, "parent", &[
                ("src/lib.rs", "pub fn f() -> i32 { 2 }\n"),
                ("tests/check.rs", "#[test] fn strict() { assert_eq!(f(), 2); }\n"),
            ]);
            let parent = commit(&store, vec![], parent_tree);
            // Child: same code, but the test is WEAKENED, plus a NEW self-serving test.
            let child_tree = tree_from(&store, &tmp, "child", &[
                ("src/lib.rs", "pub fn f() -> i32 { 2 }\n"),
                ("tests/check.rs", "#[test] fn strict() { assert!(true); }\n"), // weakened
                ("tests/added.rs", "#[test] fn mine() { assert!(true); }\n"),   // newly added
            ]);
            let child = commit(&store, vec![parent], child_tree);
            host.compose_independence_tree("t", "r", &child.to_hex()).expect("composes when tests change")
        };
        // Materialize the composed tree and inspect it.
        let out = tmp.join("out");
        assert!(host.checkout_tree("t", "r", &composed_hex, &out));
        let restored = std::fs::read_to_string(out.join("tests/check.rs")).unwrap();
        assert!(restored.contains("assert_eq!(f(), 2)"), "modified test restored to parent's strict version");
        assert!(!out.join("tests/added.rs").exists(), "newly-added test dropped from the independence tree");
        assert!(out.join("src/lib.rs").exists(), "non-test code kept");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
