//! Human resolutions of reconciliation claims (C4 → the "needs-judgment" action).
//!
//! A reconciliation claim the engine can't verify is left **needs-judgment** — the machine defers to
//! a human. This is where that human judgment is recorded: for a `(repo, change, claim)` a reviewer
//! marks the claim **verified** ("I checked it — it holds") or raises a **concern** ("this is a real
//! problem"), with a note and their accountable identity. The ledger overlays these so a resolved
//! claim stops reading as an open question.
//!
//! A side-store (persisted JSON) keyed by content — the claim id is a stable hash of the claim text,
//! so a resolution follows the claim across re-derivations of the ledger.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimResolution {
    /// Accountable actor id who recorded the judgment.
    pub by: String,
    /// `verified` (a human confirmed it) or `concern` (a human flags it as a real issue).
    pub judgment: String,
    #[serde(default)]
    pub note: String,
    pub ts: u64,
}

pub struct ClaimResolutions {
    path: PathBuf,
    map: Mutex<HashMap<String, ClaimResolution>>,
}

fn key(repo: &str, change: &str, claim: &str) -> String {
    format!("{repo}|{change}|{claim}")
}

impl ClaimResolutions {
    pub fn from_env() -> Self {
        let path = std::env::var("HULL_CLAIM_RESOLUTIONS").map(PathBuf::from).unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(format!("{home}/.hull/claim-resolutions.json"))
        });
        let map = crate::jsonstore::load_json(&path);
        ClaimResolutions { path, map: Mutex::new(map) }
    }

    pub fn set(&self, repo: &str, change: &str, claim: &str, r: ClaimResolution) {
        let mut m = self.map.lock().unwrap();
        m.insert(key(repo, change, claim), r);
        crate::jsonstore::persist_json_atomic(&self.path, &*m);
    }

    /// All resolutions for a change, keyed by claim id — for overlaying onto the ledger.
    pub fn for_change(&self, repo: &str, change: &str) -> HashMap<String, ClaimResolution> {
        let prefix = format!("{repo}|{change}|");
        self.map
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(k, v)| k.strip_prefix(&prefix).map(|claim| (claim.to_string(), v.clone())))
            .collect()
    }

    /// Re-key every resolution belonging to `old_repo` under `new_repo` (on repo rename). Keys are
    /// `"{repo}|{change}|{claim}"`, so we move each entry whose key starts with `"{old_repo}|"` to the
    /// same key with the prefix swapped — preserving the durable human verified/concern judgments
    /// across the rename.
    pub fn rekey(&self, old_repo: &str, new_repo: &str) {
        let old_prefix = format!("{old_repo}|");
        let mut m = self.map.lock().unwrap();
        let moved: Vec<(String, ClaimResolution)> = m
            .iter()
            .filter_map(|(k, v)| k.strip_prefix(&old_prefix).map(|rest| (format!("{new_repo}|{rest}"), v.clone())))
            .collect();
        if moved.is_empty() {
            return;
        }
        m.retain(|k, _| !k.starts_with(&old_prefix));
        for (k, v) in moved {
            m.insert(k, v);
        }
        crate::jsonstore::persist_json_atomic(&self.path, &*m);
    }

    /// Drop every resolution for a repo (on repo delete), so stale judgments can't resurface if the
    /// name is later recreated. Keys are `"{repo}|{change}|{claim}"` — remove those with the prefix.
    pub fn remove_repo(&self, repo: &str) {
        let prefix = format!("{repo}|");
        let mut m = self.map.lock().unwrap();
        let before = m.len();
        m.retain(|k, _| !k.starts_with(&prefix));
        if m.len() != before {
            crate::jsonstore::persist_json_atomic(&self.path, &*m);
        }
    }
}
