//! Incremental re-review via **diff-aware invalidation** (D9).
//!
//! A review's inputs are the change's keel **tree** (its content) plus verification. Re-reviewing an
//! unchanged tree should not re-run the (expensive) model — the verdict is memoized by `tree_id`.
//! When the tree changes (a new push = a different diff), the key changes, so the review is
//! automatically re-run. Content-addressing gives diff-aware invalidation for free: same tree ⇒ same
//! review, changed tree ⇒ fresh review.
//!
//! Persisted JSON, `tree_id → CachedReview`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFinding {
    pub path: String,
    pub line: Option<u32>,
    pub severity: String,
    pub note: String,
}

/// The reusable part of a review verdict — everything derived from the tree, minus per-PR wrapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedReview {
    /// `approve` | `request_changes` | `comment`.
    pub verdict: String,
    pub summary: String,
    pub findings: Vec<CachedFinding>,
    #[serde(default)]
    pub ledger: Option<hull_core::reconcile::ClaimLedger>,
}

pub struct ReviewCache {
    path: PathBuf,
    map: Mutex<HashMap<String, CachedReview>>,
}

impl ReviewCache {
    pub fn from_env() -> Self {
        let path = std::env::var("HULL_REVIEW_CACHE").map(PathBuf::from).unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(format!("{home}/.hull/review-cache.json"))
        });
        let map = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        ReviewCache { path, map: Mutex::new(map) }
    }

    pub fn get(&self, tree: &str) -> Option<CachedReview> {
        self.map.lock().unwrap().get(tree).cloned()
    }

    pub fn put(&self, tree: &str, r: CachedReview) {
        let mut m = self.map.lock().unwrap();
        m.insert(tree.to_string(), r);
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(j) = serde_json::to_string_pretty(&*m) {
            let _ = std::fs::write(&self.path, j);
        }
    }
}
