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

use serde::Serialize;
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
#[derive(Debug, Clone, Serialize)]
pub struct RepoActivity {
    pub repo: String,
    pub score: f64,
    pub last_ts: u64,
    /// Actors seen active recently (deduped).
    pub active_actors: Vec<String>,
    /// Most recent files touched.
    pub hot_files: Vec<String>,
}

/// Central hub: broadcasts events to SSE subscribers and maintains per-repo activity.
pub struct ActivityHub {
    tx: broadcast::Sender<ActivityEvent>,
    ranker: RwLock<Ranker>,
}

impl ActivityHub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        ActivityHub { tx, ranker: RwLock::new(Ranker::default()) }
    }

    /// Subscribe to the live event stream (one receiver per SSE connection).
    pub fn subscribe(&self) -> broadcast::Receiver<ActivityEvent> {
        self.tx.subscribe()
    }

    /// Ingest an event: update the ranking and fan it out to subscribers.
    pub fn publish(&self, ev: ActivityEvent) {
        self.ranker.write().unwrap().observe(&ev);
        let _ = self.tx.send(ev); // no subscribers is fine
    }

    /// The home page: repos ranked by live activity (busy first), then recency.
    pub fn home(&self) -> Vec<RepoActivity> {
        self.ranker.read().unwrap().ranked()
    }
}

impl Default for ActivityHub {
    fn default() -> Self {
        Self::new()
    }
}

/// Decaying activity accounting. Each event adds weight to its repo; `ranked()` orders by score.
/// (Scaffold uses simple accumulation + recency; M3 adds time-decay tied to wall clock.)
#[derive(Default)]
struct Ranker {
    repos: HashMap<String, RepoActivity>,
}

impl Ranker {
    fn observe(&mut self, ev: &ActivityEvent) {
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
