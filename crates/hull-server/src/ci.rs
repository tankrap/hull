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
use std::collections::HashMap;
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

    // Fresh run: materialize the tree and hand a checkout to the runner. The temp dir is unique per
    // (tree, pid) so concurrent checks don't collide; cleaned up after. (No RNG available in this
    // runtime — the tree id + pid is uniqueness enough.)
    let dir = std::env::temp_dir().join(format!("hull-ci-{}-{}", &tree[..tree.len().min(16)], std::process::id()));
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
