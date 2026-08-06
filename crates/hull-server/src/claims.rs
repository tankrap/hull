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
}
