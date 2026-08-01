//! The Hull server as a **library**, so both the OSS binary and a private hosted binary reuse it.
//!
//! The open-core seam is [`run`]'s `register_plugins` argument: the OSS binary passes a no-op; a
//! hosted binary (in a separate private repo) passes a closure that registers its closed plugins —
//! `hull_server::run(opts, |reg| hull_hosted::register(reg))`. The core never names a hosted crate.
//!
//! Endpoints: `/health` · `/api/home` · `/api/feed` (SSE) · `/api/repos` ·
//! `/api/repos/:repo/issues` · `/api/scan` · `/api/plugins`.

pub mod activity;
pub mod ingress;
pub mod keeld;
pub mod plugins;
pub mod quic;
pub mod repos;

use activity::{ActivityEvent, ActivityHub};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use std::collections::HashMap;
use futures::stream::Stream;
use hull_core::store::{FileStore, InMemory, Store};
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
    store: Arc<dyn Store>,
    hub: Arc<ActivityHub>,
    registry: Arc<Registry>,
    repos: repos::RepoHost,
}

impl repos::HasRepoHost for App {
    fn repo_host(&self) -> &repos::RepoHost {
        &self.repos
    }
}

/// Build the router with an already-assembled registry (handy for tests / embedding). Wires a
/// coordination source (real keeld bridge or the demo) but NOT the QUIC ingress — [`run`] starts
/// that, so tests don't bind a UDP port.
pub fn router(registry: Registry) -> Router {
    let hub = Arc::new(ActivityHub::new());
    wire_sources(&hub);
    // Tests/embedding use an ephemeral in-memory store; `run` uses the durable FileStore.
    let store: Arc<dyn Store> = Arc::new(InMemory::new());
    seed_if_empty(&*store);
    make_router(build_app(registry, hub, store))
}

fn build_app(registry: Registry, hub: Arc<ActivityHub>, store: Arc<dyn Store>) -> App {
    App { store, hub, registry: Arc::new(registry), repos: repos::RepoHost::from_env() }
}

/// Path to the durable domain snapshot (`HULL_DATA_DIR`/store.json, default `~/.hull/data`).
fn data_path() -> std::path::PathBuf {
    let dir = std::env::var("HULL_DATA_DIR").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.hull/data")
    });
    std::path::PathBuf::from(dir).join("store.json")
}

fn seed_if_empty(store: &dyn Store) {
    if store.accounts().is_empty() {
        seed(store);
    }
}

fn make_router(app: App) -> Router {
    eprintln!("hull-server: hosting keel repos under {}", app.repos.root().display());
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/home", get(home))
        .route("/api/feed", get(feed))
        .route("/api/actors", get(actors_list).post(register_actor))
        .route("/api/repos", get(repos_list))
        .route("/api/repos/:tenant/:repo/issues", get(issues).post(create_issue))
        .route("/api/repos/:tenant/:repo/issues/:number", axum::routing::patch(update_issue))
        .route("/api/repos/:tenant/:repo/why", get(why))
        .route("/api/scan", post(scan))
        .route("/api/plugins", get(plugins_list))
        // git smart-HTTP: host N keel repos at /{tenant}/{repo} (clone / fetch / push).
        .route("/:tenant/:repo/info/refs", get(repos::info_refs::<App>))
        .route("/:tenant/:repo/git-upload-pack", post(repos::upload_pack::<App>))
        .route("/:tenant/:repo/git-receive-pack", post(repos::receive_pack::<App>))
        .with_state(app)
}

/// Wire a coordination source into `hub`: the `hull-agent` ingress (below, started by `run`) is the
/// hosted path, but a local dev can also point hull OUT at a keeld with `HULL_KEELD`. With neither,
/// the demo source keeps the scaffold alive end-to-end.
fn wire_sources(hub: &Arc<ActivityHub>) {
    let endpoints = keeld::endpoints_from_env();
    if endpoints.is_empty() {
        spawn_fake_source(hub.clone());
    } else {
        eprintln!("hull-server: bridging {} keeld daemon(s) over QUIC (dev outbound)", endpoints.len());
        keeld::spawn_keeld_sources(hub.clone(), endpoints);
    }
}

/// The QUIC ingress bind address (`HULL_INGRESS_ADDR`, default `127.0.0.1:8931`); `off` disables it.
fn ingress_addr() -> Option<std::net::SocketAddr> {
    let s = std::env::var("HULL_INGRESS_ADDR").unwrap_or_else(|_| "127.0.0.1:8931".into());
    if s.eq_ignore_ascii_case("off") {
        return None;
    }
    match s.parse() {
        Ok(a) => Some(a),
        Err(_) => {
            eprintln!("hull-server: invalid HULL_INGRESS_ADDR '{s}' — ingress disabled");
            None
        }
    }
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
    let hub = Arc::new(ActivityHub::new());
    wire_sources(&hub);
    if let Some(addr) = ingress_addr() {
        ingress::spawn(addr, hub.clone()); // daemons dial in via hull-agent
    }
    let store: Arc<dyn Store> = Arc::new(FileStore::open(data_path()));
    eprintln!("hull-server: domain store at {}", data_path().display());
    seed_if_empty(&*store);
    let router = make_router(build_app(registry, hub, store));
    let listener = tokio::net::TcpListener::bind(&opts.addr).await.expect("bind");
    eprintln!("hull-server listening on http://{}", opts.addr);
    axum::serve(listener, router).await.expect("serve");
}

/// Home for a tenant: `GET /api/home?tenant=acme` (defaults to `local`). The tenant will come from
/// the authenticated session once auth lands (NEW-1166); until then it's an explicit param.
async fn home(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let tenant = q.get("tenant").map(String::as_str).unwrap_or("local");
    Json(json!({ "tenant": tenant, "repos": app.hub.home(tenant) }))
}

/// The repos actually hosted on disk (the filesystem registry), plus the seeded domain repos.
async fn repos_list(State(app): State<App>) -> Json<Value> {
    Json(json!({ "hosted": app.repos.list(), "repos": app.store.repos() }))
}

/// Registered actors (public — no secret keys), each with its accountability root.
async fn actors_list(State(app): State<App>) -> Json<Value> {
    let actors: Vec<Value> = app
        .store
        .actors()
        .into_iter()
        .map(|a| {
            json!({
                "id": a.id,
                "handle": a.handle,
                "kind": a.kind,
                "accountable": a.is_accountable(),
                "human_root": a.human_principal(),
            })
        })
        .collect();
    Json(json!({ "actors": actors }))
}

/// Register (mint) an actor with a real Ed25519 keypair (`POST /api/actors`). A `human` is its own
/// root; an `agent` must name `delegated_by` (an existing accountable actor) and gets a delegation
/// chain rooting at that human — enforcing "no unaccountable agents" at mint. The secret key is
/// returned ONCE and never stored.
async fn register_actor(State(app): State<App>, Json(body): Json<Value>) -> Response {
    let handle = body.get("handle").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if handle.is_empty() {
        return (StatusCode::BAD_REQUEST, "handle is required").into_response();
    }
    let minted = match body.get("kind").and_then(Value::as_str).unwrap_or("human") {
        "human" => identity::mint_human(&handle),
        "agent" => {
            let Some(parent_id) = body.get("delegated_by").and_then(Value::as_str) else {
                return (StatusCode::UNPROCESSABLE_ENTITY, "an agent must be 'delegated_by' a human actor").into_response();
            };
            let Some(parent) = app.store.actor(parent_id) else {
                return (StatusCode::UNPROCESSABLE_ENTITY, format!("delegated_by: unknown actor '{parent_id}'")).into_response();
            };
            let scope = body.get("scope").and_then(Value::as_str).unwrap_or("*");
            match identity::mint_agent(&handle, &parent, scope, Lifetime::Ephemeral { expires_unix: 0 }) {
                Some(m) => m,
                None => return (StatusCode::UNPROCESSABLE_ENTITY, "the delegating actor is not accountable (must chain to a human)").into_response(),
            }
        }
        _ => return (StatusCode::BAD_REQUEST, "kind must be 'human' or 'agent'").into_response(),
    };
    app.store.put_actor(minted.actor.clone());
    (StatusCode::CREATED, Json(json!({ "actor": minted.actor, "secret_key": minted.secret_key }))).into_response()
}

/// The accountability gate: an authoring action must be by a registered actor that chains to a
/// human. Returns the actor, or a 403 response to short-circuit the handler.
#[allow(clippy::result_large_err)] // the Err IS the HTTP response the caller returns
fn require_accountable(app: &App, actor_id: &str) -> Result<Actor, Response> {
    match app.store.actor(actor_id) {
        Some(a) if a.is_accountable() => Ok(a),
        Some(_) => Err((StatusCode::FORBIDDEN, "actor is not accountable (no human root)").into_response()),
        None => Err((
            StatusCode::FORBIDDEN,
            format!("unknown actor '{actor_id}' — register it at POST /api/actors first (nothing is authored anonymously)"),
        )
            .into_response()),
    }
}

/// Keel-native provenance for a path (`GET /api/repos/:tenant/:repo/why?path=…`): the changes and
/// authors/agents that touched it. This is the spine that makes a code-ref traceable, not just a
/// pointer — something GitHub has no representation for.
async fn why(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let path = q.get("path").map(String::as_str).unwrap_or("");
    let prov = app.repos.why(&tenant, &repo, path, 10);
    Json(json!({ "path": path, "provenance": prov }))
}

/// List issues for a hosted repo (`GET /api/repos/:tenant/:repo/issues`).
async fn issues(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>) -> Json<Value> {
    Json(json!({ "issues": app.store.issues(&format!("{tenant}/{repo}")) }))
}

/// Open an issue (`POST /api/repos/:tenant/:repo/issues`). An optional `code_ref` `{path, line_start,
/// line_end?}` is resolved to a keel **blob id** at HEAD, so the reference is content-addressed and
/// survives edits (the keel-native advantage over `file#L42`). Opening an issue also emits an event
/// to the tenant's situation room.
async fn create_issue(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    let key = format!("{tenant}/{repo}");
    // Accountability gate: the author must be a registered actor that chains to a human.
    let author_id = body.get("author").and_then(Value::as_str).unwrap_or("");
    let actor = match require_accountable(&app, author_id) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let title = body.get("title").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if title.is_empty() {
        return (StatusCode::BAD_REQUEST, "title is required").into_response();
    }
    let mut code_refs = Vec::new();
    if let Some(cr) = body.get("code_ref").filter(|v| !v.is_null()) {
        if let Some(path) = cr.get("path").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            // resolve to a content-addressed keel blob; None if the path isn't in HEAD
            let Some(anchor) = app.repos.resolve_blob(&tenant, &repo, path) else {
                return (StatusCode::UNPROCESSABLE_ENTITY, format!("path '{path}' not found in {key}@HEAD")).into_response();
            };
            code_refs.push(CodeRef {
                repo: key.clone(),
                blob: anchor.blob,
                path: path.to_string(),
                line_start: cr.get("line_start").and_then(Value::as_u64).unwrap_or(1) as u32,
                line_end: cr.get("line_end").and_then(Value::as_u64).map(|n| n as u32),
            });
        }
    }
    // Assignees must themselves be registered accountable actors (unknown ids are dropped).
    let assignees: Vec<String> = body
        .get("assignees")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .filter(|id| app.store.actor(id).map(|a| a.is_accountable()).unwrap_or(false))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let number = app.store.issues(&key).iter().map(|i| i.number).max().unwrap_or(0) + 1;
    let author = actor.id.clone();
    let issue = Issue {
        id: format!("iss_{}_{number}", key.replace('/', "_")),
        repo: key,
        number,
        title,
        body: body.get("body").and_then(Value::as_str).unwrap_or("").to_string(),
        author: author.clone(),
        assignees,
        labels: vec![],
        projects: vec![],
        status: IssueStatus::Open,
        code_refs,
        referenced_actors: vec![],
        linked_prs: vec![],
        resolved_by: None,
        created_unix: now(),
    };
    app.store.put_issue(issue.clone());
    app.hub.publish(
        &tenant,
        ActivityEvent::Issue { repo, number, action: "opened".into(), actor: author, ts: now() },
    );
    (StatusCode::CREATED, Json(json!({ "issue": issue }))).into_response()
}

/// Transition an issue (`PATCH /api/repos/:tenant/:repo/issues/:number`) with
/// `{"action":"close","reason":"completed|not_planned|cancelled|duplicate"}` or
/// `{"action":"reopen"}`. Emits a tenant-scoped event so the change shows live.
async fn update_issue(
    State(app): State<App>,
    Path((tenant, repo, number)): Path<(String, String, u64)>,
    Json(body): Json<Value>,
) -> Response {
    let key = format!("{tenant}/{repo}");
    // A transition is an authoring action — the acting actor must chain to a human.
    let acting = match require_accountable(&app, body.get("actor").and_then(Value::as_str).unwrap_or("")) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let Some(mut issue) = app.store.issues(&key).into_iter().find(|i| i.number == number) else {
        return (StatusCode::NOT_FOUND, "no such issue").into_response();
    };
    let action = body.get("action").and_then(Value::as_str).unwrap_or("");
    match action {
        "close" => {
            let reason = match body.get("reason").and_then(Value::as_str) {
                Some("not_planned") => CloseReason::NotPlanned,
                Some("cancelled") => CloseReason::Cancelled,
                Some("duplicate") => CloseReason::Duplicate,
                _ => CloseReason::Completed,
            };
            issue.status = IssueStatus::Closed { reason };
        }
        "reopen" => issue.status = IssueStatus::Open,
        _ => return (StatusCode::BAD_REQUEST, "action must be 'close' or 'reopen'").into_response(),
    }
    app.store.replace_issue(issue.clone());
    app.hub.publish(
        &tenant,
        ActivityEvent::Issue { repo, number, action: action.into(), actor: acting.handle, ts: now() },
    );
    Json(json!({ "issue": issue })).into_response()
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

/// SSE: stream live activity for one tenant — `GET /api/feed?tenant=acme` (defaults to `local`).
/// Events for other tenants are filtered out, so a subscriber only ever sees its own fleet.
async fn feed(
    State(app): State<App>,
    Query(q): Query<HashMap<String, String>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let tenant = q.get("tenant").cloned().unwrap_or_else(|| "local".into());
    let stream = BroadcastStream::new(app.hub.subscribe()).filter_map(move |ev| {
        let te = ev.ok()?;
        if te.tenant != tenant {
            return None; // not this subscriber's tenant
        }
        Event::default().json_data(&te.event).ok().map(Ok)
    });
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

/// Seed a little sample data so the scaffold is explorable — including real accountable actors: a
/// human, and an agent delegated by that human (so there's an accountable author to open issues).
fn seed(store: &dyn Store) {
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
    // A human root + an agent it delegated — both real Ed25519 identities.
    let human = identity::mint_human("justin").actor;
    store.put_actor(human.clone());
    if let Some(agent) =
        identity::mint_agent("agent:reviewer", &human, "issues:*", Lifetime::Static)
    {
        store.put_actor(agent.actor);
    }
    store.put_issue(Issue {
        id: "iss_1".into(),
        repo: "repo_keel".into(),
        number: 1,
        title: "Track symlinks in status".into(),
        body: "status/diff should match git on symlinks.".into(),
        author: human.id,
        assignees: vec![],
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
            hub.publish("local", ev); // demo events live under the `local` tenant
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
