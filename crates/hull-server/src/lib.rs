//! The Hull server as a **library**, so both the OSS binary and a private hosted binary reuse it.
//!
//! The open-core seam is [`run`]'s `register_plugins` argument: the OSS binary passes a no-op; a
//! hosted binary (in a separate private repo) passes a closure that registers its closed plugins —
//! `hull_server::run(opts, |reg| hull_hosted::register(reg))`. The core never names a hosted crate.
//!
//! Endpoints: `/health` · `/api/home` · `/api/feed` (SSE) · `/api/repos` ·
//! `/api/repos/:repo/issues` · `/api/scan` · `/api/plugins`.

pub mod activity;
pub mod keeld;
pub mod plugins;
pub mod repos;

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
use hull_plugin::Registry;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

/// Server options.
pub struct Options {
    pub addr: String,
}

impl Default for Options {
    fn default() -> Self {
        Options { addr: std::env::var("HULL_ADDR").unwrap_or_else(|_| "127.0.0.1:8930".into()) }
    }
}

#[derive(Clone)]
struct App {
    store: Arc<InMemory>,
    hub: Arc<ActivityHub>,
    registry: Arc<Registry>,
    repos: repos::RepoHost,
}

impl repos::HasRepoHost for App {
    fn repo_host(&self) -> &repos::RepoHost {
        &self.repos
    }
}

/// Build the router with an already-assembled registry (handy for tests / embedding).
pub fn router(registry: Registry) -> Router {
    let store = Arc::new(InMemory::new());
    seed(&store);
    let hub = Arc::new(ActivityHub::new());
    // Real keeld QUIC bridge when HULL_KEELD names one or more daemons; otherwise the demo source
    // keeps the scaffold alive end-to-end. (Set e.g. HULL_KEELD=hull@127.0.0.1:9000.)
    let endpoints = keeld::endpoints_from_env();
    if endpoints.is_empty() {
        spawn_fake_source(hub.clone());
    } else {
        eprintln!("hull-server: bridging {} keeld daemon(s) over QUIC", endpoints.len());
        keeld::spawn_keeld_sources(hub.clone(), endpoints);
    }
    let app = App { store, hub, registry: Arc::new(registry), repos: repos::RepoHost::from_env() };
    eprintln!("hull-server: hosting keel repos under {}", app.repos.root().display());
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/home", get(home))
        .route("/api/feed", get(feed))
        .route("/api/repos", get(repos_list))
        .route("/api/repos/:repo/issues", get(issues))
        .route("/api/scan", post(scan))
        .route("/api/plugins", get(plugins_list))
        // git smart-HTTP: host N keel repos at /{tenant}/{repo} (clone / fetch / push).
        .route("/:tenant/:repo/info/refs", get(repos::info_refs::<App>))
        .route("/:tenant/:repo/git-upload-pack", post(repos::upload_pack::<App>))
        .route("/:tenant/:repo/git-receive-pack", post(repos::receive_pack::<App>))
        .with_state(app)
}

/// Run the server. `register_plugins` is the open-core hook: core built-ins are installed first,
/// then this closure runs to add any extra (hosted) plugins.
pub async fn run(opts: Options, register_plugins: impl FnOnce(&mut Registry)) {
    let registry = plugins::build_registry(register_plugins);
    eprintln!(
        "hull-server: {} plugin(s) loaded: {}",
        registry.plugins().len(),
        registry.plugins().iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
    );
    let router = router(registry);
    let listener = tokio::net::TcpListener::bind(&opts.addr).await.expect("bind");
    eprintln!("hull-server listening on http://{}", opts.addr);
    axum::serve(listener, router).await.expect("serve");
}

async fn home(State(app): State<App>) -> Json<Value> {
    Json(json!({ "repos": app.hub.home() }))
}

/// The repos actually hosted on disk (the filesystem registry), plus the seeded domain repos.
async fn repos_list(State(app): State<App>) -> Json<Value> {
    Json(json!({ "hosted": app.repos.list(), "repos": app.store.repos() }))
}

async fn issues(State(app): State<App>, Path(repo): Path<String>) -> Json<Value> {
    Json(json!({ "issues": app.store.issues(&repo) }))
}

/// Server-side secret scan (the backstop) — built-in engine **plus** any plugin rulesets.
async fn scan(State(app): State<App>, Json(body): Json<Value>) -> Json<Value> {
    let text = body.get("text").and_then(Value::as_str).unwrap_or("");
    let findings = app.registry.scan_secrets(text);
    Json(json!({ "ok": findings.is_empty(), "findings": findings }))
}

/// The loaded plugins (core built-ins + any hosted plugins) — makes the open-core seam observable.
async fn plugins_list(State(app): State<App>) -> Json<Value> {
    Json(json!({ "plugins": app.registry.plugins() }))
}

/// SSE: stream live activity events as they arrive.
async fn feed(State(app): State<App>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(app.hub.subscribe())
        .filter_map(|ev| ev.ok().and_then(|ev| Event::default().json_data(&ev).ok()).map(Ok));
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
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn stamp(ev: &mut ActivityEvent, t: u64) {
    match ev {
        ActivityEvent::AgentBrief { ts, .. }
        | ActivityEvent::Lesson { ts, .. }
        | ActivityEvent::Push { ts, .. }
        | ActivityEvent::Issue { ts, .. } => *ts = t,
    }
}
