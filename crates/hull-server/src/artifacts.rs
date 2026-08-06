//! Content-addressed review **audit artifacts** (D8).
//!
//! Per review, a complete, immutable record of *why the reviewer decided what it did*: the input
//! facts (intent, files, ops, verification, secrets), the models used per pass, the ledger and
//! findings as emitted, and the verdict. The artifact is **content-addressed** (BLAKE3 of its
//! canonical JSON — serde_json sorts object keys, so the hash is stable), so it can't be altered
//! after the fact and anchors the outcome loop + eval data (Epic E).
//!
//! Stored in a side-store (persisted JSON, id → artifact) — immutable: an id maps to exactly one
//! artifact forever.

use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

pub struct ArtifactStore {
    path: PathBuf,
    map: Mutex<HashMap<String, Value>>,
}

impl ArtifactStore {
    pub fn from_env() -> Self {
        let path = std::env::var("HULL_ARTIFACTS").map(PathBuf::from).unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(format!("{home}/.hull/review-artifacts.json"))
        });
        let map = crate::jsonstore::load_json(&path);
        ArtifactStore { path, map: Mutex::new(map) }
    }

    /// Content-address and store an artifact; returns its id (BLAKE3 hex). Idempotent — the same
    /// artifact always yields the same id and overwrites identically.
    pub fn put(&self, artifact: Value) -> String {
        let bytes = serde_json::to_vec(&artifact).unwrap_or_default();
        let id = blake3::hash(&bytes).to_hex().to_string();
        let mut m = self.map.lock().unwrap();
        m.insert(id.clone(), artifact);
        crate::jsonstore::persist_json_atomic(&self.path, &*m);
        id
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        self.map.lock().unwrap().get(id).cloned()
    }
}
