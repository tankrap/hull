//! Two-way mirror bookkeeping: loop prevention + idempotency (NEW-1173).
//!
//! A mirror can loop: a change pushed **out** to GitHub comes back **in** via GitHub's webhook, which
//! would push it out again, forever. And forges redeliver webhooks, so the same inbound event can
//! arrive twice. This module is the ledger that makes both safe, independent of who does the actual
//! pushing (the [`Mirror`](hull_plugin::Mirror) capability):
//!
//! * **Origin tracking** — every change is stamped with where it first entered (`hull` or `github`).
//!   A change that originated on GitHub is never pushed back to GitHub. First writer wins, so a
//!   round-trip can't relabel it.
//! * **Idempotency keys** — every outbound push and inbound delivery is guarded by a key
//!   (`out:<change>`, `in:<external-id>`); a repeat is a no-op. Redelivery is safe.
//!
//! Persisted to JSON so the guarantees survive restarts.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

/// One recorded outbound push (for the repo's mirror status / UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outbound {
    pub repo: String,
    pub change: String,
    pub target: String,
    pub external_ref: String,
    pub ts: u64,
}

/// One recorded inbound import from the forge, with its **accountability mapping** (NEW-1176). A git
/// commit carries a git identity (name/email + optional login), not a hull Ed25519 actor. We resolve
/// that identity to a hull actor when the person has linked their GitHub login, else record it as an
/// **external, un-natively-signed** author — never passed off as accountable hull authorship.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inbound {
    pub repo: String,
    pub change: String,
    pub external_id: String,
    /// The git author string (`Name <email>`), preserved verbatim as provenance.
    pub git_author: String,
    /// The committer's forge login, if the webhook provided it.
    pub github_login: String,
    /// The hull actor this maps to (a linked, accountable actor), or `None` = external/unmapped.
    pub attributed_actor: Option<String>,
    pub ts: u64,
}

impl Inbound {
    /// True iff this import resolved to a linked hull actor (so it carries hull accountability);
    /// false = an external git identity with no hull actor behind it.
    pub fn accountable(&self) -> bool {
        self.attributed_actor.is_some()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    /// change id → origin (`hull` | `github`). First writer wins.
    origin: HashMap<String, String>,
    /// idempotency keys already handled.
    processed: HashSet<String>,
    outbound: Vec<Outbound>,
    /// forge login → hull actor id (self-linked). The mirror's accountability map.
    #[serde(default)]
    links: HashMap<String, String>,
    #[serde(default)]
    inbound: Vec<Inbound>,
}

/// The persisted mirror ledger.
pub struct MirrorLedger {
    path: PathBuf,
    inner: Mutex<Ledger>,
}

impl MirrorLedger {
    /// Load from `HULL_MIRROR_LEDGER` (default `~/.hull/mirror.json`).
    pub fn from_env() -> Self {
        let path = std::env::var("HULL_MIRROR_LEDGER").map(PathBuf::from).unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(format!("{home}/.hull/mirror.json"))
        });
        let inner = crate::jsonstore::load_json(&path);
        MirrorLedger { path, inner: Mutex::new(inner) }
    }

    fn persist(&self, l: &Ledger) {
        crate::jsonstore::persist_json_atomic(&self.path, l);
    }

    /// Where a change first entered, if known.
    pub fn origin(&self, change: &str) -> Option<String> {
        self.inner.lock().unwrap().origin.get(change).cloned()
    }

    /// Stamp a change's origin; first writer wins, so a round-trip can't relabel it.
    pub fn set_origin(&self, change: &str, origin: &str) {
        let mut l = self.inner.lock().unwrap();
        l.origin.entry(change.to_string()).or_insert_with(|| origin.to_string());
        self.persist(&l);
    }

    /// Loop prevention: a change may be pushed out only if it did **not** originate on the other
    /// side. A GitHub-originated change is never pushed back to GitHub.
    pub fn should_push_out(&self, change: &str) -> bool {
        self.inner.lock().unwrap().origin.get(change).map(|o| o != "github").unwrap_or(true)
    }

    /// Idempotency: mark a key handled. Returns `true` if it was newly recorded, `false` if this is
    /// a repeat (the caller should then no-op).
    pub fn mark_processed(&self, key: &str) -> bool {
        let mut l = self.inner.lock().unwrap();
        let fresh = l.processed.insert(key.to_string());
        if fresh {
            self.persist(&l);
        }
        fresh
    }

    /// Record an outbound push for the mirror status.
    pub fn record_outbound(&self, o: Outbound) {
        let mut l = self.inner.lock().unwrap();
        l.outbound.push(o);
        self.persist(&l);
    }

    /// Outbound pushes recorded for a repo (newest first).
    pub fn outbound_for(&self, repo: &str) -> Vec<Outbound> {
        let l = self.inner.lock().unwrap();
        let mut v: Vec<Outbound> = l.outbound.iter().filter(|o| o.repo == repo).cloned().collect();
        v.reverse();
        v
    }

    /// Link a forge login to a hull actor (self-service). `""` clears the link. This is the mapping
    /// that lets imported git commits carry hull accountability instead of being anonymous externals.
    pub fn link_github(&self, login: &str, actor_id: &str) {
        let mut l = self.inner.lock().unwrap();
        if actor_id.is_empty() {
            l.links.remove(login);
        } else {
            l.links.insert(login.to_string(), actor_id.to_string());
        }
        self.persist(&l);
    }

    /// The hull actor a forge login maps to, if linked.
    pub fn resolve_github(&self, login: &str) -> Option<String> {
        self.inner.lock().unwrap().links.get(login).cloned()
    }

    /// The forge login linked to a hull actor (reverse lookup, for display).
    pub fn github_for(&self, actor_id: &str) -> Option<String> {
        self.inner.lock().unwrap().links.iter().find(|(_, v)| v.as_str() == actor_id).map(|(k, _)| k.clone())
    }

    /// Record an inbound import + its accountability mapping.
    pub fn record_inbound(&self, i: Inbound) {
        let mut l = self.inner.lock().unwrap();
        l.inbound.push(i);
        self.persist(&l);
    }

    /// Inbound imports recorded for a repo (newest first).
    pub fn inbound_for(&self, repo: &str) -> Vec<Inbound> {
        let l = self.inner.lock().unwrap();
        let mut v: Vec<Inbound> = l.inbound.iter().filter(|i| i.repo == repo).cloned().collect();
        v.reverse();
        v
    }

    /// Re-key mirror-status history from `old` to `new` repo key (on repo rename). Only the
    /// outbound/inbound records carry a `tenant/repo` key; origin/idempotency are keyed by change id
    /// and stay put. Keeps the repo's mirror-status history reachable under its new name.
    pub fn rekey_repo(&self, old: &str, new: &str) {
        let mut l = self.inner.lock().unwrap();
        let mut changed = false;
        for o in l.outbound.iter_mut().filter(|o| o.repo == old) {
            o.repo = new.to_string();
            changed = true;
        }
        for i in l.inbound.iter_mut().filter(|i| i.repo == old) {
            i.repo = new.to_string();
            changed = true;
        }
        if changed {
            self.persist(&l);
        }
    }

    /// Drop a repo's mirror-status history (on repo delete), so it isn't orphaned pointing at a dead
    /// repo. Origin/idempotency entries (keyed by change id) are left to age out.
    pub fn remove_repo(&self, repo: &str) {
        let mut l = self.inner.lock().unwrap();
        let before = l.outbound.len() + l.inbound.len();
        l.outbound.retain(|o| o.repo != repo);
        l.inbound.retain(|i| i.repo != repo);
        if l.outbound.len() + l.inbound.len() != before {
            self.persist(&l);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> MirrorLedger {
        // A ledger whose path is in a temp dir unique to the test process (no RNG in this runtime).
        let path = std::env::temp_dir().join(format!("hull-mirror-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        MirrorLedger { path, inner: Mutex::new(Ledger::default()) }
    }

    #[test]
    fn github_originated_change_is_not_pushed_back() {
        let l = ledger();
        l.set_origin("c1", "github");
        assert!(!l.should_push_out("c1"), "a github-originated change must not loop back out");
    }

    #[test]
    fn hull_originated_change_pushes_out() {
        let l = ledger();
        l.set_origin("c2", "hull");
        assert!(l.should_push_out("c2"));
    }

    #[test]
    fn unknown_change_defaults_to_pushable() {
        assert!(ledger().should_push_out("never-seen"));
    }

    #[test]
    fn first_writer_wins_on_origin() {
        let l = ledger();
        l.set_origin("c3", "github");
        l.set_origin("c3", "hull"); // a round-trip trying to relabel
        assert_eq!(l.origin("c3").as_deref(), Some("github"));
        assert!(!l.should_push_out("c3"));
    }

    #[test]
    fn idempotency_key_is_only_fresh_once() {
        let l = ledger();
        assert!(l.mark_processed("in:evt-1"), "first delivery is fresh");
        assert!(!l.mark_processed("in:evt-1"), "redelivery is a no-op");
    }

    #[test]
    fn github_link_maps_imports_to_an_accountable_actor() {
        let l = ledger();
        l.link_github("octocat", "actor_abc");
        assert_eq!(l.resolve_github("octocat").as_deref(), Some("actor_abc"));
        assert_eq!(l.github_for("actor_abc").as_deref(), Some("octocat"));
        // a mapped import carries hull accountability; an unmapped one is external.
        let mapped = Inbound { repo: "t/r".into(), change: "c1".into(), external_id: "e1".into(), git_author: "Mona <mo@x>".into(), github_login: "octocat".into(), attributed_actor: l.resolve_github("octocat"), ts: 0 };
        let external = Inbound { repo: "t/r".into(), change: "c2".into(), external_id: "e2".into(), git_author: "Rando <r@x>".into(), github_login: "rando".into(), attributed_actor: l.resolve_github("rando"), ts: 0 };
        assert!(mapped.accountable());
        assert!(!external.accountable());
        l.link_github("octocat", ""); // clearing unlinks
        assert!(l.resolve_github("octocat").is_none());
    }
}
