//! The reactive activity hub — the engine behind Hull's "situation room" home page.
//!
//! keeld already broadcasts coordination events over QUIC ("agent working on file X", lessons). The
//! hub ingests that stream, keeps a decaying **activity score per repo**, and re-broadcasts events
//! to connected browsers over SSE. The home page ranks repos by live activity: an agent starting
//! work on repo X floats X to the front.
//!
//! Scaffold note: [`spawn_fake_source`] synthesizes events so the UI is alive end-to-end. The real
//! [`KeeldSource`] (M3) will `keel_net::Client::connect(addr).subscribe()` and feed the same hub —
//! the ingestion shape is identical, only the source changes.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;
use tokio::sync::broadcast;

/// One thing that happened in the fleet. Mirrors keeld's QUIC coordination events plus Hull-level
/// object events (issues/PRs), so the home feed is a single unified stream.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivityEvent {
    /// An agent is working on a file (keeld brief-presence).
    AgentBrief { actor: String, repo: String, file: String, task: String, ts: u64 },
    /// A lesson was learned (keeld / `keel learn`).
    Lesson { repo: String, file: String, lesson: String, ts: u64 },
    /// A change landed on a repo.
    Push { actor: String, repo: String, change: String, ts: u64 },
    /// A Hull object event.
    Issue { repo: String, number: u64, action: String, actor: String, ts: u64 },
}

impl ActivityEvent {
    pub fn repo(&self) -> &str {
        match self {
            ActivityEvent::AgentBrief { repo, .. }
            | ActivityEvent::Lesson { repo, .. }
            | ActivityEvent::Push { repo, .. }
            | ActivityEvent::Issue { repo, .. } => repo,
        }
    }
    pub fn ts(&self) -> u64 {
        match self {
            ActivityEvent::AgentBrief { ts, .. }
            | ActivityEvent::Lesson { ts, .. }
            | ActivityEvent::Push { ts, .. }
            | ActivityEvent::Issue { ts, .. } => *ts,
        }
    }
}

/// A repo's current standing on the home page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoActivity {
    pub repo: String,
    pub score: f64,
    pub last_ts: u64,
    /// Actors seen active recently (deduped).
    pub active_actors: Vec<String>,
    /// Most recent files touched.
    pub hot_files: Vec<String>,
}

/// An event tagged with the tenant it belongs to — the unit on the broadcast bus. SSE subscribers
/// filter by their own tenant so one org never sees another's fleet (NEW-1169).
#[derive(Debug, Clone)]
pub struct TenantEvent {
    pub tenant: String,
    pub event: ActivityEvent,
}

/// Central hub: broadcasts tenant-tagged events to SSE subscribers and maintains **per-tenant**
/// repo activity, so the situation room is scoped to the viewing org. Optionally persisted to disk
/// so the home page survives a restart (NEW-1169).
pub struct ActivityHub {
    tx: broadcast::Sender<TenantEvent>,
    /// tenant → its repo ranking.
    rankers: RwLock<HashMap<String, Ranker>>,
    persist: Option<std::path::PathBuf>,
}

impl ActivityHub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        ActivityHub { tx, rankers: RwLock::new(HashMap::new()), persist: None }
    }

    /// Like [`new`](Self::new) but loads any persisted ranking from `path` and remembers it for
    /// [`flush`](Self::flush), so the situation room survives restarts.
    pub fn with_persistence(path: std::path::PathBuf) -> Self {
        let rankers = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<HashMap<String, Ranker>>(&b).ok())
            .unwrap_or_default();
        let (tx, _) = broadcast::channel(1024);
        ActivityHub { tx, rankers: RwLock::new(rankers), persist: Some(path) }
    }

    /// Write the current ranking to disk (no-op if persistence is off). Called on a timer.
    pub fn flush(&self) {
        let Some(path) = &self.persist else { return };
        let snap = self.rankers.read().unwrap();
        if let Ok(bytes) = serde_json::to_vec(&*snap) {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = path.with_extension("json.tmp");
            let _ = std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, path));
        }
    }

    /// Subscribe to the raw (tenant-tagged) stream; the caller filters to its tenant.
    pub fn subscribe(&self) -> broadcast::Receiver<TenantEvent> {
        self.tx.subscribe()
    }

    /// Ingest an event for `tenant`: update that tenant's ranking and fan out on the bus.
    pub fn publish(&self, tenant: &str, ev: ActivityEvent) {
        {
            let mut rankers = self.rankers.write().unwrap();
            // Cap the number of distinct tenants ranked. The ingress is anonymous by default, so an
            // attacker could otherwise open many connections with unique tenant headers and grow this
            // map (which is periodically flushed to disk) without bound. A brand-new tenant beyond the
            // cap is not ranked; the event still broadcasts to live subscribers below.
            if rankers.contains_key(tenant) || rankers.len() < MAX_RANKED_TENANTS {
                rankers.entry(tenant.to_string()).or_default().observe(&ev);
            }
        }
        let _ = self.tx.send(TenantEvent { tenant: tenant.to_string(), event: ev }); // no subscribers is fine
    }

    /// The home page for one tenant: its repos ranked by live activity (busy first), then recency.
    pub fn home(&self, tenant: &str) -> Vec<RepoActivity> {
        self.rankers.read().unwrap().get(tenant).map(Ranker::ranked).unwrap_or_default()
    }
}

impl Default for ActivityHub {
    fn default() -> Self {
        Self::new()
    }
}

/// Upper bounds on the ranker maps, so the (anonymous-by-default) ingress can't grow them without
/// limit. Generous — a real deployment has far fewer tenants/repos; these only stop an abuse flood.
const MAX_RANKED_TENANTS: usize = 4096;
const MAX_RANKED_REPOS_PER_TENANT: usize = 1024;

/// Decaying activity accounting. Each event adds weight to its repo; `ranked()` orders by score.
/// (Scaffold uses simple accumulation + recency; M3 adds time-decay tied to wall clock.)
#[derive(Default, Serialize, Deserialize)]
struct Ranker {
    repos: HashMap<String, RepoActivity>,
}

impl Ranker {
    fn observe(&mut self, ev: &ActivityEvent) {
        // Cap distinct repos per tenant for the same reason `publish` caps tenants: a flood of unique
        // repo names must not grow this map without bound. A new repo beyond the cap is skipped.
        let repo = ev.repo();
        if !self.repos.contains_key(repo) && self.repos.len() >= MAX_RANKED_REPOS_PER_TENANT {
            return;
        }
        let entry = self.repos.entry(ev.repo().to_string()).or_insert_with(|| RepoActivity {
            repo: ev.repo().to_string(),
            score: 0.0,
            last_ts: 0,
            active_actors: Vec::new(),
            hot_files: Vec::new(),
        });
        let (weight, actor, file) = match ev {
            ActivityEvent::AgentBrief { actor, file, .. } => (3.0, Some(actor), Some(file)),
            ActivityEvent::Push { actor, .. } => (5.0, Some(actor), None),
            ActivityEvent::Lesson { file, .. } => (2.0, None, Some(file)),
            ActivityEvent::Issue { actor, .. } => (1.5, Some(actor), None),
        };
        entry.score += weight;
        entry.last_ts = entry.last_ts.max(ev.ts());
        if let Some(a) = actor {
            dedup_push_front(&mut entry.active_actors, a, 5);
        }
        if let Some(f) = file {
            dedup_push_front(&mut entry.hot_files, f, 5);
        }
    }

    fn ranked(&self) -> Vec<RepoActivity> {
        let mut v: Vec<RepoActivity> = self.repos.values().cloned().collect();
        v.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.last_ts.cmp(&a.last_ts))
        });
        v
    }
}

/// Push `item` to the front (most-recent-first), dedup, cap length.
fn dedup_push_front(list: &mut Vec<String>, item: &str, cap: usize) {
    list.retain(|x| x != item);
    list.insert(0, item.to_string());
    list.truncate(cap);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brief(repo: &str) -> ActivityEvent {
        ActivityEvent::AgentBrief {
            actor: "agent:x".into(),
            repo: repo.into(),
            file: "f.rs".into(),
            task: "t".into(),
            ts: 1,
        }
    }

    #[test]
    fn ranker_caps_distinct_tenants() {
        let hub = ActivityHub::new();
        for i in 0..(MAX_RANKED_TENANTS + 25) {
            hub.publish(&format!("t{i}"), brief("r"));
        }
        assert_eq!(hub.rankers.read().unwrap().len(), MAX_RANKED_TENANTS, "tenant map stops growing at the cap");
    }

    #[test]
    fn ranker_caps_distinct_repos_per_tenant() {
        let hub = ActivityHub::new();
        for i in 0..(MAX_RANKED_REPOS_PER_TENANT + 25) {
            hub.publish("solo", brief(&format!("repo{i}")));
        }
        assert_eq!(hub.rankers.read().unwrap().get("solo").unwrap().repos.len(), MAX_RANKED_REPOS_PER_TENANT, "per-tenant repo map capped");
    }

    #[test]
    fn home_is_scoped_per_tenant() {
        let hub = ActivityHub::new();
        hub.publish("acme", brief("acme-web"));
        hub.publish("acme", brief("acme-api"));
        hub.publish("globex", brief("globex-svc"));

        let acme: Vec<_> = hub.home("acme").into_iter().map(|r| r.repo).collect();
        let globex: Vec<_> = hub.home("globex").into_iter().map(|r| r.repo).collect();

        assert_eq!(acme.len(), 2);
        assert!(acme.contains(&"acme-web".to_string()) && acme.contains(&"acme-api".to_string()));
        assert!(!acme.contains(&"globex-svc".to_string()), "acme must not see globex's repos");
        assert_eq!(globex, vec!["globex-svc".to_string()]);
        assert!(hub.home("unknown").is_empty()); // a tenant with no activity sees nothing
    }
}
