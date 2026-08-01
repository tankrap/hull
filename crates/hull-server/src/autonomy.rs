//! Autonomy policy storage + resolution (per tenant/account and per repo).
//!
//! A scope's policy says **how much an agent may do autonomously** — auto-review PRs, have its
//! approve count toward a merge, etc. Policy resolves **repo → account → instance default**: the most
//! specific set wins for the tier; protected paths accumulate (a path protected at any level stays
//! protected). Default tier is **T1** (agents review but a human approves) until a human raises it.
//!
//! A side-store (persisted JSON) keyed by scope — `repo:<tenant>/<repo>` or `account:<id>`.

use hull_core::{AutonomyPolicy, AutonomyTier};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// Protected paths every instance enforces unless overridden — sensitive areas that always need a
/// human (D10: auth/, migrations always deep).
pub const DEFAULT_PROTECTED: &[&str] = &["auth/", "migrations/", ".hull/", "**/auth/**", "**/migrations/**"];

pub struct AutonomyStore {
    path: PathBuf,
    map: Mutex<HashMap<String, AutonomyPolicy>>,
}

/// The effective policy for a repo, with where the tier came from.
pub struct Effective {
    pub tier: AutonomyTier,
    pub source: &'static str, // "repo" | "account" | "instance"
    pub protected_paths: Vec<String>,
}

fn repo_scope(tenant: &str, repo: &str) -> String {
    format!("repo:{tenant}/{repo}")
}
fn account_scope(account: &str) -> String {
    format!("account:{account}")
}

impl AutonomyStore {
    pub fn from_env() -> Self {
        let path = std::env::var("HULL_AUTONOMY").map(PathBuf::from).unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(format!("{home}/.hull/autonomy.json"))
        });
        let map = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        AutonomyStore { path, map: Mutex::new(map) }
    }

    fn persist(&self, m: &HashMap<String, AutonomyPolicy>) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(j) = serde_json::to_string_pretty(m) {
            let _ = std::fs::write(&self.path, j);
        }
    }

    pub fn get_repo(&self, tenant: &str, repo: &str) -> Option<AutonomyPolicy> {
        self.map.lock().unwrap().get(&repo_scope(tenant, repo)).cloned()
    }
    pub fn get_account(&self, account: &str) -> Option<AutonomyPolicy> {
        self.map.lock().unwrap().get(&account_scope(account)).cloned()
    }
    pub fn set_repo(&self, tenant: &str, repo: &str, p: AutonomyPolicy) {
        let mut m = self.map.lock().unwrap();
        m.insert(repo_scope(tenant, repo), p);
        self.persist(&m);
    }
    pub fn set_account(&self, account: &str, p: AutonomyPolicy) {
        let mut m = self.map.lock().unwrap();
        m.insert(account_scope(account), p);
        self.persist(&m);
    }

    /// The instance-default tier from `HULL_DEFAULT_AUTONOMY` (t0..t3), default T1.
    fn instance_tier() -> AutonomyTier {
        match std::env::var("HULL_DEFAULT_AUTONOMY").unwrap_or_default().to_lowercase().as_str() {
            "t0" => AutonomyTier::T0,
            "t2" => AutonomyTier::T2,
            "t3" => AutonomyTier::T3,
            _ => AutonomyTier::T1,
        }
    }

    /// Resolve the effective policy for a repo. `account` is the id of the repo's owning account (for
    /// the account-level fallback); pass `None` if unknown.
    pub fn effective(&self, tenant: &str, repo: &str, account: Option<&str>) -> Effective {
        let mut protected: Vec<String> = DEFAULT_PROTECTED.iter().map(|s| s.to_string()).collect();
        let acct_pol = account.and_then(|a| self.get_account(a));
        if let Some(p) = &acct_pol {
            protected.extend(p.protected_paths.iter().cloned());
        }
        if let Some(p) = self.get_repo(tenant, repo) {
            protected.extend(p.protected_paths.iter().cloned());
            protected.sort();
            protected.dedup();
            return Effective { tier: p.tier, source: "repo", protected_paths: protected };
        }
        if let Some(p) = acct_pol {
            protected.sort();
            protected.dedup();
            return Effective { tier: p.tier, source: "account", protected_paths: protected };
        }
        protected.sort();
        protected.dedup();
        Effective { tier: Self::instance_tier(), source: "instance", protected_paths: protected }
    }
}

/// Does any changed path match a protected glob? Uses hull-core's glob matcher.
pub fn touches_protected(files: &[String], protected: &[String]) -> bool {
    files.iter().any(|f| protected.iter().any(|g| hull_core::store::glob_match(g, f)))
}
