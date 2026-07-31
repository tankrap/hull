//! Hull server (M0 scaffold): axum HTTP/JSON API + the reactive activity feed.
//!
//! Endpoints:
//!   GET  /health                     — liveness
//!   GET  /api/home                   — repos ranked by live fleet activity (the situation room)
//!   GET  /api/feed                   — SSE stream of live activity events
//!   GET  /api/repos                  — repos
//!   GET  /api/repos/:repo/issues     — issues in a repo
//!   POST /api/scan                   — secret-scan a blob of text (server-side backstop engine)
//!
//! The feed is fed by a synthetic source for now; M3 replaces it with a keeld QUIC subscription.

mod activity;

use activity::{ActivityEvent, ActivityHub};
use axum::{
    extract::{Path, State},
    response::sse::{Event, Sse},
    routing::{get, post},
    Json, Router,
};
use futures::stream::Stream;
use hull_core::store::{InMemory, Store};
use hull_core::*;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

#[derive(Clone)]
struct App {
    store: Arc<InMemory>,
    hub: Arc<ActivityHub>,
}

#[tokio::main]
async fn main() {
    let store = Arc::new(InMemory::new());
    seed(&store);
    let hub = Arc::new(ActivityHub::new());
    let app = App { store, hub: hub.clone() };

    // Scaffold: synthesize fleet activity so the home page is alive. M3 swaps this for a keeld
    // QUIC subscription (keel_net::Client::connect(addr).subscribe()).
    spawn_fake_source(hub);

    let router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/home", get(home))
        .route("/api/feed", get(feed))
        .route("/api/repos", get(repos))
        .route("/api/repos/:repo/issues", get(issues))
        .route("/api/scan", post(scan))
        .with_state(app);

    let addr = std::env::var("HULL_ADDR").unwrap_or_else(|_| "127.0.0.1:8930".into());
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    eprintln!("hull-server listening on http://{addr}");
    axum::serve(listener, router).await.expect("serve");
}

async fn home(State(app): State<App>) -> Json<Value> {
    Json(json!({ "repos": app.hub.home() }))
}

async fn repos(State(app): State<App>) -> Json<Value> {
    Json(json!({ "repos": app.store.repos() }))
}

async fn issues(State(app): State<App>, Path(repo): Path<String>) -> Json<Value> {
    Json(json!({ "issues": app.store.issues(&repo) }))
}

/// Server-side secret scan (the backstop). Body: `{ "text": "..." }`.
async fn scan(Json(body): Json<Value>) -> Json<Value> {
    let text = body.get("text").and_then(Value::as_str).unwrap_or("");
    let findings = hull_scan::scan(text);
    Json(json!({ "ok": findings.is_empty(), "findings": findings }))
}

/// SSE: stream live activity events as they arrive.
async fn feed(State(app): State<App>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(app.hub.subscribe()).filter_map(|ev| {
        ev.ok().and_then(|ev| Event::default().json_data(&ev).ok()).map(Ok)
    });
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

/// Seed a little sample data so the scaffold is explorable.
fn seed(store: &InMemory) {
    store.put_account(Account {
        id: "acct_tankrap".into(),
        kind: AccountKind::Organization,
        handle: "tankrap".into(),
        members: vec![],
    });
    for name in ["keel", "hull"] {
        store.put_repo(Repo {
            id: format!("repo_{name}"),
            owner: "acct_tankrap".into(),
            name: name.into(),
            default_branch: "main".into(),
        });
    }
    store.put_issue(Issue {
        id: "iss_1".into(),
        repo: "repo_keel".into(),
        number: 1,
        title: "Track symlinks in status".into(),
        body: "status/diff should match git on symlinks.".into(),
        author: "agent:opus-4-8".into(),
        assignees: vec!["agent:opus-4-8".into()],
        labels: vec!["status".into()],
        projects: vec![],
        status: IssueStatus::Closed { reason: CloseReason::Completed },
        code_refs: vec![],
        referenced_actors: vec![],
        linked_prs: vec![],
        resolved_by: Some("blake3:eb17068".into()),
        created_unix: 0,
    });
}

/// Synthesize fleet-coordination events on a timer (stand-in for the keeld QUIC stream).
fn spawn_fake_source(hub: Arc<ActivityHub>) {
    tokio::spawn(async move {
        let script = [
            ActivityEvent::AgentBrief {
                actor: "agent:reviewer-3".into(),
                repo: "hull".into(),
                file: "crates/hull-scan/src/lib.rs".into(),
                task: "review secret patterns".into(),
                ts: 0,
            },
            ActivityEvent::Lesson {
                repo: "keel".into(),
                file: "keel-store/src/snapshot.rs".into(),
                lesson: "Symlinks store the target as a mode-120000 blob.".into(),
                ts: 0,
            },
            ActivityEvent::Push {
                actor: "agent:opus-4-8".into(),
                repo: "keel".into(),
                change: "blake3:eb17068".into(),
                ts: 0,
            },
        ];
        let mut i = 0usize;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            let mut ev = script[i % script.len()].clone();
            stamp(&mut ev, now());
            hub.publish(ev);
            i += 1;
        }
    });
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn stamp(ev: &mut ActivityEvent, t: u64) {
    match ev {
        ActivityEvent::AgentBrief { ts, .. }
        | ActivityEvent::Lesson { ts, .. }
        | ActivityEvent::Push { ts, .. }
        | ActivityEvent::Issue { ts, .. } => *ts = t,
    }
}
