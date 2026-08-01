//! The real keeld → activity bridge (M3 / NEW-1116): connect to a local `keeld`'s QUIC coordination
//! channel, subscribe, and feed every event into the [`ActivityHub`] — the same shape the scaffold's
//! `spawn_fake_source` faked, but sourced from a live daemon.
//!
//! Each keeld serves ONE repo, so a Hull server aggregates N daemons: one [`KeeldEndpoint`] per
//! `(repo, addr)`. The bridge tags every event with that repo (keeld's own events don't carry a
//! repo name — the association is which daemon they came from) and reconnects with backoff if a
//! daemon restarts. Tenant scoping (so one org can't see another's stream) is NEW-1169.

use crate::activity::{ActivityEvent, ActivityHub};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// One local keeld to aggregate: the repo it serves and its QUIC address.
#[derive(Debug, Clone)]
pub struct KeeldEndpoint {
    pub repo: String,
    pub addr: SocketAddr,
}

/// Parse `HULL_KEELD` — a comma-separated list of `repo@host:port` (e.g. `hull@127.0.0.1:9000`).
/// Empty/unset → no real sources (the caller falls back to the demo source).
pub fn endpoints_from_env() -> Vec<KeeldEndpoint> {
    let Ok(spec) = std::env::var("HULL_KEELD") else { return Vec::new() };
    spec.split(',')
        .filter_map(|s| {
            let (repo, addr) = s.trim().split_once('@')?;
            let addr = addr.trim().parse().ok()?;
            Some(KeeldEndpoint { repo: repo.trim().to_string(), addr })
        })
        .collect()
}

/// Spawn one background task per endpoint, each streaming its keeld's events into `hub`.
pub fn spawn_keeld_sources(hub: Arc<ActivityHub>, endpoints: Vec<KeeldEndpoint>) {
    for ep in endpoints {
        let hub = hub.clone();
        tokio::spawn(async move { keeld_loop(hub, ep).await });
    }
}

/// Connect → subscribe → forward, reconnecting with capped exponential backoff so a daemon
/// restart (or a not-yet-started daemon) is transparent.
async fn keeld_loop(hub: Arc<ActivityHub>, ep: KeeldEndpoint) {
    let mut backoff = 1u64;
    loop {
        match stream_once(&hub, &ep).await {
            Ok(()) => backoff = 1, // clean end of stream → reconnect promptly
            Err(e) => eprintln!("hull: keeld '{}' ({}) unavailable: {e}", ep.repo, ep.addr),
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

async fn stream_once(
    hub: &ActivityHub,
    ep: &KeeldEndpoint,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = keel_net::Client::connect(ep.addr).await?;
    let mut events = client.subscribe().await?;
    eprintln!("hull: subscribed to keeld for repo '{}' at {}", ep.repo, ep.addr);
    while let Some(bytes) = events.next().await {
        if let Some(ev) = map_event(&bytes, &ep.repo) {
            hub.publish(ev);
        }
    }
    Ok(())
}

/// Map a keeld coordination event (JSON) to an [`ActivityEvent`], tagging it with `repo`. Unknown
/// kinds are skipped rather than erroring, so new keeld event types don't break the bridge.
fn map_event(bytes: &[u8], repo: &str) -> Option<ActivityEvent> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let ts = v.get("ts").and_then(serde_json::Value::as_u64).unwrap_or(0);
    let str_of = |k: &str| v.get(k).and_then(serde_json::Value::as_str).unwrap_or("").to_string();
    match v.get("kind").and_then(serde_json::Value::as_str)? {
        "brief" => Some(ActivityEvent::AgentBrief {
            actor: v.get("agent").and_then(serde_json::Value::as_str).unwrap_or("unknown").to_string(),
            repo: repo.to_string(),
            file: str_of("file"),
            task: str_of("task"),
            ts,
        }),
        "lesson" => Some(ActivityEvent::Lesson {
            repo: repo.to_string(),
            file: str_of("file"),
            lesson: str_of("lesson"),
            ts,
        }),
        "commit" | "push" | "change" => Some(ActivityEvent::Push {
            actor: v.get("agent").and_then(serde_json::Value::as_str).unwrap_or("unknown").to_string(),
            repo: repo.to_string(),
            change: str_of("change"),
            ts,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_endpoint_spec() {
        std::env::set_var("HULL_KEELD", "hull@127.0.0.1:9000, keel@127.0.0.1:9001");
        let eps = endpoints_from_env();
        assert_eq!(eps.len(), 2);
        assert_eq!(eps[0].repo, "hull");
        assert_eq!(eps[1].addr.port(), 9001);
        std::env::remove_var("HULL_KEELD");
    }

    #[test]
    fn maps_brief_and_lesson_events() {
        let brief = br#"{"kind":"brief","agent":"agent:opus","file":"src/lib.rs","task":"fix","ts":42}"#;
        match map_event(brief, "hull") {
            Some(ActivityEvent::AgentBrief { actor, repo, file, ts, .. }) => {
                assert_eq!((actor.as_str(), repo.as_str(), file.as_str(), ts), ("agent:opus", "hull", "src/lib.rs", 42));
            }
            other => panic!("expected AgentBrief, got {other:?}"),
        }
        let lesson = br#"{"kind":"lesson","file":"a.rs","lesson":"wrap errors","ts":7}"#;
        assert!(matches!(map_event(lesson, "keel"), Some(ActivityEvent::Lesson { .. })));
        // unknown kind is skipped, not an error
        assert!(map_event(br#"{"kind":"weather","temp":72}"#, "keel").is_none());
    }
}
