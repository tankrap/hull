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
