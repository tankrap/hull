//! Memoized, content-addressed CI runs (M5) over the reviewer runtime (D2).
//!
//! Checks are keyed by the change's keel **tree id**, not the change id — two changes with identical
//! trees (a rebase, a no-op merge, a re-push) share one verdict, and re-running an unchanged tree is
//! an instant memo hit instead of a fresh test run. That content-addressing is the whole speed
//! story: you pay for a given tree's checks exactly once.
//!
//! The plumbing here is core (Apache-2.0): resolve the change to its tree, materialize a checkout,
//! ask the [`Registry`](hull_plugin::Registry)'s CI runner for a verdict (the built-in local runner
//! by default; a hosted plugin swaps in autoscaled runners), memoize by tree id, and write the
//! green/red back to keel verification so the reconciliation ledger reflects it.

use hull_plugin::{CiOutcome, CiRequest, CiStatus, Registry};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::repos::RepoHost;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoEntry {
    status: String, // green | red | errored
    summary: String,
}

/// The content-addressed CI memo: tree id → verdict, persisted to JSON so it survives restarts.
/// Errored runs are never memoized (they are not a verdict about the tree).
pub struct CiMemo {
    path: PathBuf,
    map: Mutex<HashMap<String, MemoEntry>>,
}

impl CiMemo {
    /// Load the memo from `HULL_CI_MEMO` (default `~/.hull/ci-memo.json`).
    pub fn from_env() -> Self {
        let path = std::env::var("HULL_CI_MEMO").map(PathBuf::from).unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(format!("{home}/.hull/ci-memo.json"))
        });
        let map = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        CiMemo { path, map: Mutex::new(map) }
    }

    fn get(&self, tree: &str) -> Option<MemoEntry> {
        self.map.lock().unwrap().get(tree).cloned()
    }

    /// The memoized verdict for a tree, as a [`CiOutcome`] (with `memoized: true`), if any.
    pub fn get_memoized(&self, tree: &str) -> Option<CiOutcome> {
        self.get(tree).map(|hit| {
            let status = match hit.status.as_str() {
                "green" => CiStatus::Green,
                "red" => CiStatus::Red,
                _ => CiStatus::Errored,
            };
            CiOutcome { status, summary: hit.summary, memoized: true }
        })
    }

    fn put(&self, tree: &str, entry: MemoEntry) {
        let mut m = self.map.lock().unwrap();
        m.insert(tree.to_string(), entry);
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&*m) {
            let _ = std::fs::write(&self.path, json);
        }
    }
}

/// Process-local sequence appended to CI checkout dir names so every run gets a unique directory,
/// even when two concurrent checks materialize the same tree (see `run_check` / `run_check_tree`).
static CI_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn status_str(s: CiStatus) -> &'static str {
    match s {
        CiStatus::Green => "green",
        CiStatus::Red => "red",
        CiStatus::Errored => "errored",
    }
}

/// Run (or serve from memo) the checks for a change, and persist the green/red to keel verification.
/// `force` bypasses the memo for a fresh run.
pub fn run_check(
    repos: &RepoHost,
    registry: &Registry,
    memo: &CiMemo,
    tenant: &str,
    repo: &str,
    change: &str,
    force: bool,
) -> CiOutcome {
    let Some(tree) = repos.change_tree(tenant, repo, change) else {
        return CiOutcome { status: CiStatus::Errored, summary: "unknown change".into(), memoized: false };
    };

    // Content-addressed memo hit: an identical tree has already been judged.
    if !force {
        if let Some(hit) = memo.get(&tree) {
            let status = match hit.status.as_str() {
                "green" => CiStatus::Green,
                "red" => CiStatus::Red,
                _ => CiStatus::Errored,
            };
            apply_verification(repos, tenant, repo, change, status);
            return CiOutcome { status, summary: hit.summary, memoized: true };
        }
    }

    // Fresh run: materialize the tree and hand a checkout to the runner. The temp dir must be unique
    // **per run**, not per (tree, pid): two concurrent checks of the same tree would otherwise share a
    // directory and `remove_dir_all` each other's checkout mid-run. A process-local atomic counter is
    // enough and stays deterministic (no RNG in this runtime). Cleaned up after.
    let seq = CI_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("hull-ci-{}-{}-{seq}", &tree[..tree.len().min(16)], std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    if !repos.checkout_change(tenant, repo, change, &dir) {
        return CiOutcome { status: CiStatus::Errored, summary: "checkout failed".into(), memoized: false };
    }
    let req = CiRequest { repo: format!("{tenant}/{repo}"), change: change.to_string(), tree_id: tree.clone(), workdir: dir.clone() };
    let outcome = registry.run_ci(&req);
    let _ = std::fs::remove_dir_all(&dir);

    // Memoize only real verdicts (green/red), and reflect them in keel verification.
    if matches!(outcome.status, CiStatus::Green | CiStatus::Red) {
        memo.put(&tree, MemoEntry { status: status_str(outcome.status).into(), summary: outcome.summary.clone() });
        apply_verification(repos, tenant, repo, change, outcome.status);
    }
    outcome
}

/// Run (or serve from memo) checks for a **bare tree** — no associated change, no keel-verification
/// write. This is how the independence check runs its composed tree (new code + pre-existing tests).
/// Memoized by tree id like any other run, so an identical composed tree is judged once.
pub fn run_check_tree(repos: &RepoHost, registry: &Registry, memo: &CiMemo, tenant: &str, repo: &str, tree: &str) -> CiOutcome {
    if let Some(hit) = memo.get(tree) {
        let status = match hit.status.as_str() {
            "green" => CiStatus::Green,
            "red" => CiStatus::Red,
            _ => CiStatus::Errored,
        };
        return CiOutcome { status, summary: hit.summary, memoized: true };
    }
    let seq = CI_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("hull-ci-{}-{}-{seq}", &tree[..tree.len().min(16)], std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    if !repos.checkout_tree(tenant, repo, tree, &dir) {
        return CiOutcome { status: CiStatus::Errored, summary: "checkout failed".into(), memoized: false };
    }
    let req = CiRequest { repo: format!("{tenant}/{repo}"), change: String::new(), tree_id: tree.to_string(), workdir: dir.clone() };
    let outcome = registry.run_ci(&req);
    let _ = std::fs::remove_dir_all(&dir);
    if matches!(outcome.status, CiStatus::Green | CiStatus::Red) {
        memo.put(tree, MemoEntry { status: status_str(outcome.status).into(), summary: outcome.summary.clone() });
    }
    outcome
}

// ── external CI dispatch ─────────────────────────────────────────────────────────────────────────
//
// Hull is a dumb dispatcher: it POSTs a standard job payload to the CI endpoint a repo configures
// (or the instance default), and the CI system — queue, runners, whatever — posts the verdict back
// to the callback URL. Hull owns no queue and knows nothing about the CI's internals.

/// A repo's CI endpoint: where to POST the job, and the shared secret that authenticates both the
/// dispatch and the callback.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoCi {
    pub url: String,
    #[serde(default)]
    pub secret: String,
}

/// Persisted per-repo CI endpoints + an in-memory guard against double-dispatching the same tree
/// while its verdict is outstanding.
pub struct CiConfig {
    path: PathBuf,
    map: Mutex<HashMap<String, RepoCi>>,
    inflight: Mutex<HashSet<String>>, // tree ids currently dispatched, awaiting a callback
}

/// Where an effective CI config came from — for the `GET …/ci-config` response.
pub enum CiSource {
    Repo,
    Instance,
    None,
}

impl CiConfig {
    /// Load per-repo config from `HULL_CI_CONFIG` (default `~/.hull/ci-config.json`).
    pub fn from_env() -> Self {
        let path = std::env::var("HULL_CI_CONFIG").map(PathBuf::from).unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(format!("{home}/.hull/ci-config.json"))
        });
        let map = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        CiConfig { path, map: Mutex::new(map), inflight: Mutex::new(HashSet::new()) }
    }

    /// The repo's own configured endpoint (not the instance default).
    pub fn get(&self, repo: &str) -> Option<RepoCi> {
        self.map.lock().unwrap().get(repo).cloned().filter(|c| !c.url.is_empty())
    }

    /// Set (or clear, with an empty url) a repo's CI endpoint.
    pub fn set(&self, repo: &str, cfg: RepoCi) {
        let mut m = self.map.lock().unwrap();
        if cfg.url.is_empty() {
            m.remove(repo);
        } else {
            m.insert(repo.to_string(), cfg);
        }
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(j) = serde_json::to_string_pretty(&*m) {
            let _ = std::fs::write(&self.path, j);
        }
    }

    /// The effective endpoint for a repo: its own config, else the hull-instance default
    /// (`HULL_CI_URL` / `HULL_CI_SECRET`), else none (fall back to the built-in local runner).
    pub fn resolve(&self, repo: &str) -> (Option<RepoCi>, CiSource) {
        if let Some(c) = self.get(repo) {
            return (Some(c), CiSource::Repo);
        }
        if let Ok(url) = std::env::var("HULL_CI_URL") {
            if !url.is_empty() {
                return (Some(RepoCi { url, secret: std::env::var("HULL_CI_SECRET").unwrap_or_default() }), CiSource::Instance);
            }
        }
        (None, CiSource::None)
    }

    pub fn mark_inflight(&self, tree: &str) -> bool {
        self.inflight.lock().unwrap().insert(tree.to_string())
    }
    pub fn is_inflight(&self, tree: &str) -> bool {
        self.inflight.lock().unwrap().contains(tree)
    }
    pub fn clear_inflight(&self, tree: &str) {
        self.inflight.lock().unwrap().remove(tree);
    }
}

/// The version of the CI integration contract Hull speaks (sent as `X-Hull-CI-Version`). See
/// `CI-SPEC.md`. Bump only on a breaking change to the dispatch/callback shape.
pub const CONTRACT_VERSION: &str = "1";

/// The **standard dispatch payload** Hull POSTs to a CI endpoint. This is the contract a CI system
/// integrates against — stable regardless of what's on the other side.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_body(tenant: &str, repo: &str, change: &str, tree: &str, intent: &str, author: &str, base_url: &str) -> Value {
    let base = base_url.trim_end_matches('/');
    json!({
        "repo": format!("{tenant}/{repo}"),
        "change": change,
        "tree_id": tree,
        "intent": intent,
        "author": author,
        // keel-native, content-addressed source: GET this to obtain the change's tree (by tree_id) as
        // a tar archive. NOT git — git smart-HTTP is interop/mirroring only.
        "source_url": format!("{base}/api/repos/{tenant}/{repo}/tree/{tree}/tar"),
        // Where the CI system POSTs its verdict when done.
        "callback_url": format!("{base}/api/repos/{tenant}/{repo}/change/{change}/ci-result"),
    })
}

/// Finalize a verdict delivered by the CI system's callback: memoize by tree (green/red only), write
/// keel verification, and release the in-flight guard. Returns the parsed status.
#[allow(clippy::too_many_arguments)]
pub fn finalize(repos: &RepoHost, memo: &CiMemo, config: &CiConfig, tenant: &str, repo: &str, change: &str, status: &str, summary: &str) -> CiStatus {
    let st = match status {
        "green" => CiStatus::Green,
        "red" => CiStatus::Red,
        _ => CiStatus::Errored,
    };
    if let Some(tree) = repos.change_tree(tenant, repo, change) {
        if matches!(st, CiStatus::Green | CiStatus::Red) {
            memo.put(&tree, MemoEntry { status: status_str(st).into(), summary: summary.to_string() });
            apply_verification(repos, tenant, repo, change, st);
        }
        config.clear_inflight(&tree);
    }
    st
}

fn apply_verification(repos: &RepoHost, tenant: &str, repo: &str, change: &str, status: CiStatus) {
    match status {
        CiStatus::Green => {
            repos.set_verification(tenant, repo, change, true);
        }
        CiStatus::Red => {
            repos.set_verification(tenant, repo, change, false);
        }
        CiStatus::Errored => {}
    }
}
