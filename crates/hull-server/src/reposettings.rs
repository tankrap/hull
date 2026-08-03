//! Per-repo settings not covered by autonomy / CI / code-owners: visibility, default reviewers, and
//! team access grants. A self-contained JSON side-store keyed by `<tenant>/<repo>` (same pattern as
//! the autonomy + CI stores).

use hull_core::ActorId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// A team's access grant on a repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamAccess {
    pub team: String,
    /// "admin" | "write" | "read".
    pub role: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RepoSettings {
    /// Private repos are hidden from non-members (advisory in this build; the flag is authoritative).
    #[serde(default)]
    pub private: bool,
    /// Unlisted: readable by anyone with the link (not private), but excluded from public listings.
    #[serde(default)]
    pub unlisted: bool,
    /// Reviewers auto-requested on every new voyage.
    #[serde(default)]
    pub default_reviewers: Vec<ActorId>,
    /// Teams granted access, with their role on this repo.
    #[serde(default)]
    pub team_access: Vec<TeamAccess>,
    /// Require an approving review to land (on top of the built-in author-independence gate).
    #[serde(default)]
    pub require_review_to_land: bool,
}

pub struct RepoSettingsStore {
    path: PathBuf,
    inner: Mutex<HashMap<String, RepoSettings>>,
}

impl RepoSettingsStore {
    pub fn from_env() -> Self {
        let path = std::env::var("HULL_REPO_SETTINGS").map(PathBuf::from).unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(format!("{home}/.hull/repo-settings.json"))
        });
        let inner = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        RepoSettingsStore { path, inner: Mutex::new(inner) }
    }

    pub fn get(&self, key: &str) -> RepoSettings {
        self.inner.lock().unwrap().get(key).cloned().unwrap_or_default()
    }

    pub fn set(&self, key: &str, settings: RepoSettings) {
        let mut g = self.inner.lock().unwrap();
        g.insert(key.to_string(), settings);
        self.persist(&g);
    }

    fn persist(&self, map: &HashMap<String, RepoSettings>) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(bytes) = serde_json::to_string_pretty(map) {
            let _ = std::fs::write(&self.path, bytes);
        }
    }
}
