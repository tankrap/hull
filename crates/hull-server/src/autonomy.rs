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
        let map = crate::jsonstore::load_json(&path);
        AutonomyStore { path, map: Mutex::new(map) }
    }

    fn persist(&self, m: &HashMap<String, AutonomyPolicy>) {
        crate::jsonstore::persist_json_atomic(&self.path, m);
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
    /// Atomically set a scope's tier, replacing `protected_paths` only when `paths` is `Some` — else
    /// the existing paths are PRESERVED. The read-merge-write happens under a single lock, so a
    /// concurrent tier-only change can't clobber a concurrent path-replace (or vice-versa).
    fn update_tier(&self, scope: String, tier: AutonomyTier, paths: Option<Vec<String>>) {
        let mut m = self.map.lock().unwrap();
        let protected_paths = paths.unwrap_or_else(|| m.get(&scope).map(|p| p.protected_paths.clone()).unwrap_or_default());
        m.insert(scope, AutonomyPolicy { tier, protected_paths });
        self.persist(&m);
    }
    /// Set a repo's tier, preserving its protected paths unless `paths` is given. See [`Self::update_tier`].
    pub fn set_repo_tier(&self, tenant: &str, repo: &str, tier: AutonomyTier, paths: Option<Vec<String>>) {
        self.update_tier(repo_scope(tenant, repo), tier, paths);
    }
    /// Set an account's tier, preserving its protected paths unless `paths` is given.
    pub fn set_account_tier(&self, account: &str, tier: AutonomyTier, paths: Option<Vec<String>>) {
        self.update_tier(account_scope(account), tier, paths);
    }
    /// Drop a repo's autonomy override (on repo delete).
    pub fn delete_repo(&self, tenant: &str, repo: &str) {
        let mut m = self.map.lock().unwrap();
        if m.remove(&repo_scope(tenant, repo)).is_some() {
            self.persist(&m);
        }
    }
    /// Move a repo's autonomy override to a new name (on repo rename).
    pub fn rename_repo(&self, tenant: &str, old: &str, new: &str) {
        let mut m = self.map.lock().unwrap();
        if let Some(p) = m.remove(&repo_scope(tenant, old)) {
            m.insert(repo_scope(tenant, new), p);
            self.persist(&m);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Vec<String> {
        DEFAULT_PROTECTED.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_protected_matches_real_files() {
        let p = defaults();
        // Root-level protected dirs.
        assert!(touches_protected(&["auth/login.rs".into()], &p));
        assert!(touches_protected(&[".hull/CODEOWNERS".into()], &p));
        assert!(touches_protected(&["migrations/0001.sql".into()], &p));
        // Nested `auth` / `migrations` dirs, caught by the `**/…/**` entries.
        assert!(touches_protected(&["db/migrations/1.sql".into()], &p));
        assert!(touches_protected(&["crates/api/auth/token.rs".into()], &p));
    }

    #[test]
    fn default_protected_does_not_match_ordinary_files() {
        let p = defaults();
        assert!(!touches_protected(&["src/main.rs".into()], &p));
        assert!(!touches_protected(&["README.md".into()], &p));
        // A same-prefix-but-different word must not match.
        assert!(!touches_protected(&["authz/policy.rs".into()], &p));
        assert!(!touches_protected(&["src/migration_helper.rs".into()], &p));
    }

    #[test]
    fn touches_protected_is_false_when_nothing_matches() {
        assert!(!touches_protected(&["a.rs".into(), "b/c.rs".into()], &defaults()));
    }

    // A store with an isolated temp persist path (constructed directly — the test module sees the
    // private fields), so set_repo/set_account don't touch the real ~/.hull.
    fn store_at(tag: &str) -> AutonomyStore {
        let path = std::env::temp_dir().join(format!("hull-autonomy-test-{}-{tag}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        AutonomyStore { path, map: Mutex::new(HashMap::new()) }
    }

    fn policy(tier: AutonomyTier, protected: &[&str]) -> AutonomyPolicy {
        AutonomyPolicy { tier, protected_paths: protected.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn effective_repo_overrides_account_overrides_instance_for_tier() {
        let s = store_at("prec");
        s.set_account("acct", policy(AutonomyTier::T2, &["acct/"]));
        s.set_repo("tnt", "repo", policy(AutonomyTier::T3, &["repo/"]));
        // Repo policy present ⇒ its tier wins, and `source` says so.
        let eff = s.effective("tnt", "repo", Some("acct"));
        assert_eq!(eff.tier, AutonomyTier::T3);
        assert_eq!(eff.source, "repo");
    }

    #[test]
    fn effective_falls_back_to_account_then_instance() {
        let s = store_at("fallback");
        s.set_account("acct", policy(AutonomyTier::T2, &["acct/"]));
        // No repo policy ⇒ account tier + source.
        let acc = s.effective("tnt", "repo", Some("acct"));
        assert_eq!(acc.tier, AutonomyTier::T2);
        assert_eq!(acc.source, "account");
        // Neither repo nor account ⇒ instance default (env-derived; compared to the same source so the
        // assertion is independent of HULL_DEFAULT_AUTONOMY in the test environment).
        let inst = s.effective("other", "repo", None);
        assert_eq!(inst.source, "instance");
        assert_eq!(inst.tier, AutonomyStore::instance_tier());
        // With no policies, protected paths are exactly the instance defaults (sorted + deduped).
        let mut want: Vec<String> = DEFAULT_PROTECTED.iter().map(|s| s.to_string()).collect();
        want.sort();
        want.dedup();
        assert_eq!(inst.protected_paths, want);
    }

    #[test]
    fn effective_protected_paths_accumulate_across_all_three_levels() {
        let s = store_at("union");
        s.set_account("acct", policy(AutonomyTier::T1, &["account-only/"]));
        s.set_repo("tnt", "repo", policy(AutonomyTier::T3, &["repo-only/"]));
        let eff = s.effective("tnt", "repo", Some("acct"));
        // The repo tier won, but protected paths are the UNION of instance defaults + account + repo:
        // a path protected at ANY level stays protected regardless of which level set the tier (D11).
        assert!(eff.protected_paths.contains(&"repo-only/".to_string()), "repo-level protected kept");
        assert!(eff.protected_paths.contains(&"account-only/".to_string()), "account-level protected kept even though repo won the tier");
        assert!(eff.protected_paths.contains(&"auth/".to_string()), "instance default protected kept");
        // Deduped + sorted.
        let mut sorted = eff.protected_paths.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(eff.protected_paths, sorted);
    }
}
