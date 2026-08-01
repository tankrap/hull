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

#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    /// change id → origin (`hull` | `github`). First writer wins.
    origin: HashMap<String, String>,
    /// idempotency keys already handled.
    processed: HashSet<String>,
    outbound: Vec<Outbound>,
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
        let inner = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        MirrorLedger { path, inner: Mutex::new(inner) }
    }

    fn persist(&self, l: &Ledger) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(l) {
            let _ = std::fs::write(&self.path, json);
        }
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
}
