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
    /// Allow an author to approve + merge their own change (turns OFF the author-independence gate).
    /// Off by default (`false`) so independence is enforced everywhere unless an owner opts out — and
    /// `Default`/missing settings stay safe.
    #[serde(default)]
    pub allow_self_approve: bool,
    /// The repo's configured issue labels — the ONLY labels issues can carry (not free-form).
    #[serde(default)]
    pub labels: Vec<Label>,
}

/// A configurable issue label: a name, a color (hex, e.g. `#d73a4a`), and an optional emoji icon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Label {
    pub name: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub icon: String,
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
        let inner = crate::jsonstore::load_json(&path);
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

    /// Drop a repo's settings entirely (on repo delete).
    pub fn delete(&self, key: &str) {
        let mut g = self.inner.lock().unwrap();
        if g.remove(key).is_some() {
            self.persist(&g);
        }
    }

    /// Move a repo's settings from `old` to `new` key (on repo rename).
    pub fn rename(&self, old: &str, new: &str) {
        let mut g = self.inner.lock().unwrap();
        if let Some(v) = g.remove(old) {
            g.insert(new.to_string(), v);
            self.persist(&g);
        }
    }

    fn persist(&self, map: &HashMap<String, RepoSettings>) {
        crate::jsonstore::persist_json_atomic(&self.path, map);
    }
}
