//! The Hull server as a **library**, so both the OSS binary and a private hosted binary reuse it.
//!
//! The open-core seam is [`run`]'s `register_plugins` argument: the OSS binary passes a no-op; a
//! hosted binary (in a separate private repo) passes a closure that registers its closed plugins —
//! `hull_server::run(opts, |reg| hull_hosted::register(reg))`. The core never names a hosted crate.
//!
//! Endpoints: `/health` · `/api/home` · `/api/feed` (SSE) · `/api/repos` ·
//! `/api/repos/:repo/issues` · `/api/scan` · `/api/plugins`.

pub mod activity;
pub mod ci;
pub mod ingress;
pub mod keeld;
pub mod mirror;
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
use hull_plugin::{NotifyEvent, Notifier};
use std::collections::HashMap;
use std::sync::Mutex;
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

/// A delivered notification, captured for `/api/notifications`. In a hosted deployment a `Notifier`
/// plugin would ALSO fan this out over email/Slack/nostr; the core records + logs it.
#[derive(Clone, serde::Serialize)]
struct Notification {
    kind: String,
    to: Vec<String>,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    change: Option<String>,
    ts: u64,
}

/// A core [`Notifier`] capability that records recent notifications in memory so the UI can show
/// them — demonstrating the plugin seam end-to-end (the registry fans out to every notifier).
struct RecordingNotifier(Arc<Mutex<Vec<Notification>>>);
impl Notifier for RecordingNotifier {
    fn notify(&self, e: &NotifyEvent) {
        let mut buf = self.0.lock().unwrap();
        buf.push(Notification {
            kind: e.kind.clone(),
            to: e.to.clone(),
            summary: e.summary.clone(),
            change: e.change.clone(),
            ts: now(),
        });
        let n = buf.len();
        if n > 100 {
            buf.drain(0..n - 100);
        }
    }
}

/// Login challenges (nonce → issue time) and issued session tokens (token → actor id). In-memory
/// (crash-only); a hosted deployment would back this with the domain store / a cache.
#[derive(Default)]
struct AuthState {
    challenges: HashMap<String, u64>,
    tokens: HashMap<String, String>,
}

#[derive(Clone)]
struct App {
    store: Arc<dyn Store>,
    hub: Arc<ActivityHub>,
    registry: Arc<Registry>,
    repos: repos::RepoHost,
    notifications: Arc<Mutex<Vec<Notification>>>,
    auth: Arc<Mutex<AuthState>>,
    ci: Arc<ci::CiMemo>,
    ci_config: Arc<ci::CiConfig>,
    /// Outbound HTTP for dispatching CI jobs to a repo's configured endpoint. Cheap to clone.
    http: reqwest::Client,
    /// Hull's own public base URL, used to build the clone + callback URLs in a dispatch payload.
    public_url: Arc<str>,
    mirror: Arc<mirror::MirrorLedger>,
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

fn build_app(mut registry: Registry, hub: Arc<ActivityHub>, store: Arc<dyn Store>) -> App {
    // Register a core recording notifier so notifications are observable; the registry fans out to
    // this plus the log notifier plus any hosted plugin notifier.
    let notifications: Arc<Mutex<Vec<Notification>>> = Arc::new(Mutex::new(Vec::new()));
    registry.add_notifier(Arc::new(RecordingNotifier(notifications.clone())));
    App {
        store,
        hub,
        registry: Arc::new(registry),
        repos: repos::RepoHost::from_env(),
        notifications,
        auth: Arc::new(Mutex::new(AuthState::default())),
        ci: Arc::new(ci::CiMemo::from_env()),
        ci_config: Arc::new(ci::CiConfig::from_env()),
        http: reqwest::Client::new(),
        public_url: std::env::var("HULL_PUBLIC_URL").unwrap_or_else(|_| "http://127.0.0.1:8930".into()).into(),
        mirror: Arc::new(mirror::MirrorLedger::from_env()),
    }
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
    backfill_members(store);
}

/// Idempotent migration: an account persisted before memberships existed comes back with an empty
/// `members` list. Backfill the canonical org members (the human `justin` as Owner, `agent:reviewer`
/// as Write) by handle, without wiping the durable demo store or sweeping in every actor ever
/// registered. Skips a handle that isn't present.
fn backfill_members(store: &dyn Store) {
    use hull_core::{Membership, Role};
    const CANONICAL: &[(&str, Role)] = &[("justin", Role::Owner), ("agent:reviewer", Role::Write)];
    for mut acct in store.accounts() {
        if !acct.members.is_empty() {
            continue;
        }
        for (handle, role) in CANONICAL {
            if let Some(actor) = store.actors().into_iter().find(|a| &a.handle == handle) {
                acct.members.push(Membership { actor: actor.id, role: *role });
            }
        }
        if !acct.members.is_empty() {
            store.put_account(acct);
        }
    }
}

fn make_router(app: App) -> Router {
    eprintln!("hull-server: hosting keel repos under {}", app.repos.root().display());
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/home", get(home))
        .route("/api/feed", get(feed))
        .route("/api/actors", get(actors_list).post(register_actor))
        .route("/api/accounts", get(accounts_list))
        .route("/api/accounts/:id/members", post(add_member))
        .route("/api/auth/challenge", get(auth_challenge))
        .route("/api/auth/login", post(auth_login))
        .route("/api/auth/me", get(auth_me))
        .route("/api/notifications", get(notifications_list))
        .route("/api/repos", get(repos_list))
        .route("/api/repos/:tenant/:repo/issues", get(issues).post(create_issue))
        .route("/api/repos/:tenant/:repo/issues/:number", axum::routing::patch(update_issue))
        .route("/api/repos/:tenant/:repo/why", get(why))
        .route("/api/repos/:tenant/:repo/prs", get(prs).post(create_pr))
        .route("/api/repos/:tenant/:repo/prs/:number/merge", post(merge_pr))
        .route("/api/repos/:tenant/:repo/prs/:number/auto-review", post(auto_review))
        .route("/api/repos/:tenant/:repo/mirror", get(mirror_status))
        .route("/api/repos/:tenant/:repo/mirror/inbound", post(mirror_inbound))
        .route("/api/repos/:tenant/:repo/reviews", get(reviews).post(create_review))
        .route("/api/repos/:tenant/:repo/change/:id", get(change_info))
        .route("/api/repos/:tenant/:repo/change/:id/diff", get(change_diff))
        .route("/api/repos/:tenant/:repo/change/:id/ledger", get(change_ledger))
        .route("/api/repos/:tenant/:repo/change/:id/check", post(run_check_handler))
        .route("/api/repos/:tenant/:repo/change/:id/ci-result", post(ci_result))
        .route("/api/repos/:tenant/:repo/ci-config", get(get_ci_config).put(set_ci_config))
        .route("/api/repos/:tenant/:repo/security", get(repo_security))
        .route("/api/repos/:tenant/:repo/owners", get(owners_list).post(set_owners))
        .route("/api/repos/:tenant/:repo/change/:id/verify", post(verify_change))
        .route("/api/repos/:tenant/:repo/change/:id/session", post(ingest_session))
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
    // Persist the situation-room ranking next to the domain store so it survives restarts.
    let activity_path = data_path().with_file_name("activity.json");
    let hub = Arc::new(ActivityHub::with_persistence(activity_path));
    wire_sources(&hub);
    {
        // Flush the ranking to disk on a timer (crash-only; the timer is the durability point).
        let hub_flush = hub.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                hub_flush.flush();
            }
        });
    }
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

/// Recent notifications recorded by the core `Notifier` capability (newest first). Demonstrates the
/// plugin seam: these were fanned out by `registry.notify`, and a hosted plugin would also deliver
/// them over a real channel.
async fn notifications_list(
    State(app): State<App>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let mut n = app.notifications.lock().unwrap().clone();
    n.reverse();
    // Scope to an actor when `?actor=<id>` is given: deliver notifications addressed to them plus
    // broadcasts (empty `to`, e.g. CI results / mirror pushes). No actor → the full firehose.
    if let Some(actor) = q.get("actor").filter(|a| !a.is_empty()) {
        n.retain(|x| x.to.is_empty() || x.to.contains(actor));
    }
    // Resolve recipient handles for display.
    let items: Vec<Value> = n
        .iter()
        .map(|x| {
            let to_handles: Vec<String> = x.to.iter().map(|id| app.store.actor(id).map(|a| a.handle).unwrap_or_else(|| id.chars().take(8).collect())).collect();
            json!({ "kind": x.kind, "summary": x.summary, "change": x.change, "ts": x.ts, "to": to_handles, "broadcast": x.to.is_empty() })
        })
        .collect();
    Json(json!({ "notifications": items }))
}

/// Accounts (orgs / personal) with their members (handle + role) and owned repos.
async fn accounts_list(State(app): State<App>) -> Json<Value> {
    let repos = app.store.repos();
    let accounts: Vec<Value> = app
        .store
        .accounts()
        .into_iter()
        .map(|a| {
            let members: Vec<Value> = a
                .members
                .iter()
                .map(|m| {
                    json!({
                        "actor": m.actor,
                        "handle": app.store.actor(&m.actor).map(|x| x.handle).unwrap_or_default(),
                        "role": m.role,
                    })
                })
                .collect();
            let owned: Vec<String> = repos.iter().filter(|r| r.owner == a.id).map(|r| r.name.clone()).collect();
            json!({ "id": a.id, "handle": a.handle, "kind": a.kind, "members": members, "repos": owned })
        })
        .collect();
    Json(json!({ "accounts": accounts }))
}

/// Add/update an org member (`POST /api/accounts/:id/members` with `{actor, role}`). Authz uses
/// membership: only an **Owner or Admin** of the org may do it.
async fn add_member(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let acting = match require_actor(&app, &headers, body.get("by").and_then(Value::as_str).unwrap_or("")) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let Some(mut acct) = app.store.accounts().into_iter().find(|a| a.id == id) else {
        return (StatusCode::NOT_FOUND, "no such account").into_response();
    };
    let is_admin = acct.members.iter().any(|m| m.actor == acting.id && matches!(m.role, Role::Owner | Role::Admin));
    if !is_admin {
        return (StatusCode::FORBIDDEN, "only an org owner/admin can manage members").into_response();
    }
    let actor = body.get("actor").and_then(Value::as_str).unwrap_or("").to_string();
    if app.store.actor(&actor).is_none() {
        return (StatusCode::UNPROCESSABLE_ENTITY, "unknown actor").into_response();
    }
    let role = match body.get("role").and_then(Value::as_str) {
        Some("owner") => Role::Owner,
        Some("admin") => Role::Admin,
        Some("read") => Role::Read,
        _ => Role::Write,
    };
    acct.members.retain(|m| m.actor != actor);
    acct.members.push(Membership { actor, role });
    app.store.put_account(acct.clone());
    (StatusCode::CREATED, Json(json!({ "account": acct }))).into_response()
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

// ── auth: prove you hold an actor's Ed25519 key, get a session token ────────────────────────────

/// `GET /api/auth/challenge` — a one-time nonce to sign. Old nonces are pruned.
async fn auth_challenge(State(app): State<App>) -> Json<Value> {
    let nonce = identity::random_hex(16);
    let now = now();
    let mut a = app.auth.lock().unwrap();
    a.challenges.retain(|_, ts| now.saturating_sub(*ts) < 300); // 5-minute TTL
    a.challenges.insert(nonce.clone(), now);
    Json(json!({ "nonce": nonce }))
}

/// `POST /api/auth/login` — `{actor, nonce, signature}`. Verifies the Ed25519 signature over
/// `hull-login:<nonce>` against the actor's id (public key). On success, mints a session token.
async fn auth_login(State(app): State<App>, Json(body): Json<Value>) -> Response {
    let actor = body.get("actor").and_then(Value::as_str).unwrap_or("").to_string();
    let nonce = body.get("nonce").and_then(Value::as_str).unwrap_or("").to_string();
    let signature = body.get("signature").and_then(Value::as_str).unwrap_or("");
    if app.store.actor(&actor).is_none() {
        return (StatusCode::UNAUTHORIZED, "unknown actor").into_response();
    }
    {
        // consume the nonce (one-time)
        let mut a = app.auth.lock().unwrap();
        if a.challenges.remove(&nonce).is_none() {
            return (StatusCode::UNAUTHORIZED, "invalid or expired challenge").into_response();
        }
    }
    let message = format!("hull-login:{nonce}");
    if !identity::verify(&actor, message.as_bytes(), signature) {
        return (StatusCode::UNAUTHORIZED, "signature verification failed").into_response();
    }
    let token = identity::random_hex(24);
    app.auth.lock().unwrap().tokens.insert(token.clone(), actor.clone());
    (StatusCode::CREATED, Json(json!({ "token": token, "actor": actor }))).into_response()
}

/// `GET /api/auth/me` (Bearer token) — the authenticated actor, or 401.
async fn auth_me(State(app): State<App>, headers: axum::http::HeaderMap) -> Response {
    match authed_actor(&app, &headers) {
        Some(a) => Json(json!({ "id": a.id, "handle": a.handle, "kind": a.kind, "accountable": a.is_accountable() })).into_response(),
        None => (StatusCode::UNAUTHORIZED, "not signed in").into_response(),
    }
}

/// Resolve the `Authorization: Bearer <token>` header to its actor, if valid.
fn authed_actor(app: &App, headers: &axum::http::HeaderMap) -> Option<Actor> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))?;
    let actor_id = app.auth.lock().unwrap().tokens.get(token).cloned()?;
    app.store.actor(&actor_id)
}

/// The authoring identity: the **authenticated** actor (Bearer token) when signed in, else the
/// body-supplied `actor_id` (for curl/scripts). Either way it must be accountable.
#[allow(clippy::result_large_err)]
fn require_actor(app: &App, headers: &axum::http::HeaderMap, actor_id: &str) -> Result<Actor, Response> {
    if let Some(a) = authed_actor(app, headers) {
        return if a.is_accountable() {
            Ok(a)
        } else {
            Err((StatusCode::FORBIDDEN, "authenticated actor is not accountable").into_response())
        };
    }
    require_accountable(app, actor_id)
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

/// A repo's code-owner rules (`GET /api/repos/:tenant/:repo/owners`).
async fn owners_list(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>) -> Json<Value> {
    Json(json!({ "owners": app.store.owners(&format!("{tenant}/{repo}")) }))
}

/// Set a repo's code-owner rules (`POST …/owners` with `{rules: [{glob, owners:[actorId]}]}`),
/// gated to an accountable actor.
async fn set_owners(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(resp) = require_actor(&app, &headers, body.get("actor").and_then(Value::as_str).unwrap_or("")) {
        return resp;
    }
    let rules: Vec<OwnerRule> = body
        .get("rules")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    Some(OwnerRule {
                        glob: r.get("glob").and_then(Value::as_str)?.to_string(),
                        owners: r
                            .get("owners")
                            .and_then(Value::as_array)
                            .map(|o| o.iter().filter_map(Value::as_str).map(str::to_string).collect())
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    app.store.set_owners(&format!("{tenant}/{repo}"), rules.clone());
    (StatusCode::CREATED, Json(json!({ "owners": rules }))).into_response()
}

/// Resolve the code owners whose globs match any of `files`, deduped.
fn owners_for(app: &App, repo_key: &str, files: &[String]) -> Vec<String> {
    let mut set: Vec<String> = Vec::new();
    for rule in app.store.owners(repo_key) {
        if files.iter().any(|f| hull_core::store::glob_match(&rule.glob, f)) {
            for o in rule.owners {
                if !set.contains(&o) {
                    set.push(o);
                }
            }
        }
    }
    set
}

/// Secret findings from the server-side push scan (`GET /api/repos/:tenant/:repo/security`).
async fn repo_security(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>) -> Json<Value> {
    Json(json!({ "secrets": app.repos.secrets(&format!("{tenant}/{repo}")) }))
}

/// The diff of a change (`GET /api/repos/:tenant/:repo/change/:id/diff`): per-file line hunks plus a
/// semantic-operations summary — the review's diff viewer.
async fn change_diff(State(app): State<App>, Path((tenant, repo, id)): Path<(String, String, String)>) -> Json<Value> {
    Json(json!({ "files": app.repos.diff(&tenant, &repo, &id) }))
}

/// The **reconciliation ledger** for a change (`GET …/change/:id/ledger`): the claims extracted from
/// the change's narrative (intent + session lesson), each judged **supported / contradicted /
/// unsupported** against the real facts of the change (touched files, semantic ops, keel
/// verification, secret scan). This is the substance of a Hull review — does the code do what its
/// author said it does — computed the same way every time (pure, content-addressable).
async fn change_ledger(State(app): State<App>, Path((tenant, repo, id)): Path<(String, String, String)>) -> Json<Value> {
    let Some(info) = app.repos.change_info(&tenant, &repo, &id) else {
        return Json(json!({ "ledger": null }));
    };
    // Narrative: the change intent, plus the lesson from a native or ingested session.
    let lesson = info
        .session
        .as_ref()
        .map(|s| s.lesson.clone())
        .or_else(|| app.store.session_record(&format!("{tenant}/{repo}"), &id).map(|s| s.lesson))
        .unwrap_or_default();
    let facts = app.repos.facts(&tenant, &repo, &id);
    let ledger = hull_core::reconcile::reconcile(&id, &info.intent, &lesson, &facts);
    Json(json!({ "ledger": ledger }))
}

/// Expand a keel change (`GET /api/repos/:tenant/:repo/change/:id`): intent, author, and the files
/// it changed vs its parent — the keel-native "what does this touch" that anchors a review.
async fn change_info(State(app): State<App>, Path((tenant, repo, id)): Path<(String, String, String)>) -> Json<Value> {
    match app.repos.change_info(&tenant, &repo, &id) {
        Some(mut info) => {
            // If the change carries no NATIVE keel session (e.g. it arrived over git), fall back to
            // a session ingested for it (`keel capture` → POST …/session) — the session-carrying
            // bridge across the git boundary.
            if info.session.is_none() {
                if let Some(sr) = app.store.session_record(&format!("{tenant}/{repo}"), &id) {
                    info.session = Some(repos::SessionSummary {
                        task: sr.task,
                        model: sr.model,
                        lesson: sr.lesson,
                        tool_calls: sr.tool_calls,
                        tokens_in: sr.tokens_in,
                        tokens_out: sr.tokens_out,
                    });
                }
            }
            Json(json!({ "change": info }))
        }
        None => Json(json!({ "change": null })),
    }
}

/// Run the change's checks (`POST …/change/:id/check`) — the reviewer runtime. Checks out the
/// change's keel tree, runs its detected test command (or a hosted runner), memoizes the verdict by
/// tree id, and writes the resulting green/red to keel verification. Accountable-only. Body may set
/// `{"force": true}` to bypass the content-addressed memo. Returns `{status, summary, memoized}`.
async fn run_check_handler(
    State(app): State<App>,
    Path((tenant, repo, id)): Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(resp) = require_actor(&app, &headers, body.get("actor").and_then(Value::as_str).unwrap_or("")) {
        return resp;
    }
    let force = body.get("force").and_then(Value::as_bool).unwrap_or(false);
    match resolve_check(&app, &tenant, &repo, &id, force).await {
        CiResolution::Done(o) => {
            let status = ci_status_str(o.status);
            if matches!(o.status, hull_plugin::CiStatus::Green | hull_plugin::CiStatus::Red) {
                notify_ci(&app, &tenant, &repo, &id, status, &o.summary);
            }
            Json(json!({ "status": status, "summary": o.summary, "memoized": o.memoized })).into_response()
        }
        CiResolution::Dispatched { url } => {
            Json(json!({ "status": "dispatched", "summary": format!("job posted to {url}; awaiting result"), "memoized": false })).into_response()
        }
        CiResolution::Pending => {
            Json(json!({ "status": "pending", "summary": "a check for this tree is already running", "memoized": false })).into_response()
        }
        CiResolution::Failed(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

/// Outcome of triggering a check: either resolved now (memo hit or the built-in local runner) or
/// handed off to an external CI, whose verdict arrives later via the `ci-result` callback.
enum CiResolution {
    Done(hull_plugin::CiOutcome),
    Dispatched { url: String },
    Pending,
    Failed(String),
}

/// Trigger a change's checks. If the repo (or the instance) configures an external CI endpoint, POST
/// the standard job payload there and return — Hull owns no queue and waits for a callback.
/// Otherwise run the built-in local runner inline. A content-addressed memo hit short-circuits both.
async fn resolve_check(app: &App, tenant: &str, repo: &str, change: &str, force: bool) -> CiResolution {
    let Some(tree) = app.repos.change_tree(tenant, repo, change) else {
        return CiResolution::Failed("unknown change".into());
    };
    // Memo hit: an identical tree already has a verdict — no dispatch, no run.
    if !force {
        if let Some(o) = app.ci.get_memoized(&tree) {
            app.repos.set_verification(tenant, repo, change, o.status == hull_plugin::CiStatus::Green);
            return CiResolution::Done(o);
        }
    }
    let key = format!("{tenant}/{repo}");
    match app.ci_config.resolve(&key).0 {
        Some(cfg) => {
            // Don't re-dispatch a tree whose verdict is still outstanding (not a queue — just dedupe).
            if !force && app.ci_config.is_inflight(&tree) {
                return CiResolution::Pending;
            }
            let (intent, author) = app
                .repos
                .change_info(tenant, repo, change)
                .map(|i| (i.intent, i.author))
                .unwrap_or_default();
            let payload = ci::dispatch_body(tenant, repo, change, &tree, &intent, &author, &app.public_url);
            app.ci_config.mark_inflight(&tree);
            let mut req = app.http.post(&cfg.url).json(&payload);
            if !cfg.secret.is_empty() {
                req = req.header("X-Hull-CI-Secret", &cfg.secret);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => CiResolution::Dispatched { url: cfg.url },
                Ok(resp) => {
                    app.ci_config.clear_inflight(&tree);
                    CiResolution::Failed(format!("CI endpoint returned {}", resp.status()))
                }
                Err(e) => {
                    app.ci_config.clear_inflight(&tree);
                    CiResolution::Failed(format!("could not reach CI endpoint: {e}"))
                }
            }
        }
        None => {
            // No external CI configured: the built-in local runner (blocking — keep the runtime free).
            let (repos, registry, ci) = (app.repos.clone(), app.registry.clone(), app.ci.clone());
            let (t, r, c) = (tenant.to_string(), repo.to_string(), change.to_string());
            let outcome = tokio::task::spawn_blocking(move || ci::run_check(&repos, &registry, &ci, &t, &r, &c, force))
                .await
                .unwrap_or(hull_plugin::CiOutcome { status: hull_plugin::CiStatus::Errored, summary: "runner panicked".into(), memoized: false });
            CiResolution::Done(outcome)
        }
    }
}

fn ci_status_str(s: hull_plugin::CiStatus) -> &'static str {
    match s {
        hull_plugin::CiStatus::Green => "green",
        hull_plugin::CiStatus::Red => "red",
        hull_plugin::CiStatus::Errored => "errored",
    }
}

fn notify_ci(app: &App, tenant: &str, repo: &str, change: &str, status: &str, summary: &str) {
    app.registry.notify(&hull_plugin::NotifyEvent {
        kind: if status == "green" { "ci_passed".into() } else { "ci_failed".into() },
        to: vec![],
        summary: format!("checks {status} for {tenant}/{repo}@{}: {}", &change[..change.len().min(12)], summary),
        change: Some(change.to_string()),
    });
}

/// The CI system's verdict callback (`POST …/change/:id/ci-result`) — the other half of the standard
/// contract. Authenticated by the shared secret (`X-Hull-CI-Secret`). Body: `{status, summary}`.
/// Hull memoizes the verdict by tree, writes keel verification, and notifies.
async fn ci_result(
    State(app): State<App>,
    Path((tenant, repo, id)): Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let key = format!("{tenant}/{repo}");
    let (cfg, _src) = app.ci_config.resolve(&key);
    // If a secret is configured, the callback must present it.
    if let Some(ci::RepoCi { secret, .. }) = cfg.as_ref() {
        if !secret.is_empty() {
            let presented = headers.get("X-Hull-CI-Secret").and_then(|v| v.to_str().ok()).unwrap_or("");
            if presented != secret {
                return (StatusCode::UNAUTHORIZED, "bad or missing X-Hull-CI-Secret").into_response();
            }
        }
    }
    let status = body.get("status").and_then(Value::as_str).unwrap_or("").to_string();
    if !matches!(status.as_str(), "green" | "red" | "errored") {
        return (StatusCode::BAD_REQUEST, "status must be green | red | errored").into_response();
    }
    let summary = body.get("summary").and_then(Value::as_str).unwrap_or("").to_string();
    let st = ci::finalize(&app.repos, &app.ci, &app.ci_config, &tenant, &repo, &id, &status, &summary);
    if matches!(st, hull_plugin::CiStatus::Green | hull_plugin::CiStatus::Red) {
        notify_ci(&app, &tenant, &repo, &id, ci_status_str(st), &summary);
    }
    Json(json!({ "recorded": status })).into_response()
}

/// A repo's CI endpoint config (`GET/PUT …/ci-config`). GET reports the effective endpoint and where
/// it comes from (repo / instance default / none), never leaking the secret. PUT (owner-gated) sets
/// or clears the repo's own endpoint.
async fn get_ci_config(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>) -> Json<Value> {
    let key = format!("{tenant}/{repo}");
    let (cfg, src) = app.ci_config.resolve(&key);
    let source = match src {
        ci::CiSource::Repo => "repo",
        ci::CiSource::Instance => "instance",
        ci::CiSource::None => "none (built-in local runner)",
    };
    Json(json!({
        "url": cfg.as_ref().map(|c| c.url.clone()),
        "has_secret": cfg.as_ref().map(|c| !c.secret.is_empty()).unwrap_or(false),
        "source": source,
    }))
}

async fn set_ci_config(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let key = format!("{tenant}/{repo}");
    let acting = match require_actor(&app, &headers, body.get("by").and_then(Value::as_str).unwrap_or("")) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // Owner-gated: only an owner/admin of the repo's account may point it at a CI system.
    if !is_repo_admin(&app, &tenant, &repo, &acting.id) {
        return (StatusCode::FORBIDDEN, "only a repo owner/admin can set the CI endpoint").into_response();
    }
    let url = body.get("url").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let secret = body.get("secret").and_then(Value::as_str).unwrap_or("").to_string();
    app.ci_config.set(&key, ci::RepoCi { url: url.clone(), secret });
    Json(json!({ "url": url, "cleared": url.is_empty() })).into_response()
}

/// Is `actor` an Owner/Admin of the account that owns `tenant/repo`?
fn is_repo_admin(app: &App, tenant: &str, repo: &str, actor: &str) -> bool {
    let name = format!("{tenant}/{repo}");
    let Some(owner) = app.store.repos().into_iter().find(|r| r.name == name || r.name == repo).map(|r| r.owner) else {
        // No repo record — fall back to: any owner/admin of the tenant org.
        return app.store.accounts().iter().any(|a| a.handle == tenant && a.members.iter().any(|m| m.actor == actor && matches!(m.role, Role::Owner | Role::Admin)));
    };
    app.store
        .accounts()
        .into_iter()
        .find(|a| a.id == owner)
        .map(|a| a.members.iter().any(|m| m.actor == actor && matches!(m.role, Role::Owner | Role::Admin)))
        .unwrap_or(false)
}

/// Ingest a keel session for a change (`POST …/change/:id/session`) — the output of `keel capture`,
/// associated by change id. Gated to an accountable actor. Fills the review package for a change
/// that crossed the git boundary (where the native `Change.session` was lost).
async fn ingest_session(
    State(app): State<App>,
    Path((tenant, repo, id)): Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(resp) = require_actor(&app, &headers, body.get("actor").and_then(Value::as_str).unwrap_or("")) {
        return resp;
    }
    // The task is authoritative from the CHANGE's own intent, never the caller — so a session
    // captured from a long multi-task agent run can't mislabel what a specific change did.
    let Some(info) = app.repos.change_info(&tenant, &repo, &id) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "unknown change").into_response();
    };
    let record = SessionRecord {
        repo: format!("{tenant}/{repo}"),
        change: id,
        task: info.intent,
        model: body.get("model").and_then(Value::as_str).unwrap_or("").to_string(),
        lesson: body.get("lesson").and_then(Value::as_str).unwrap_or("").to_string(),
        tool_calls: body.get("tool_calls").and_then(Value::as_u64).unwrap_or(0) as usize,
        tokens_in: body.get("tokens_in").and_then(Value::as_u64).unwrap_or(0),
        tokens_out: body.get("tokens_out").and_then(Value::as_u64).unwrap_or(0),
    };
    app.store.put_session_record(record.clone());
    (StatusCode::CREATED, Json(json!({ "session": record }))).into_response()
}

/// Merge a PR (`POST /api/repos/:tenant/:repo/prs/:number/merge`). The review gate: the acting actor
/// must be accountable, the change must be keel-verify **green**, and there must be an **approve**
/// review by someone **other than the author** (independent — no self-merge). Records who merged.
async fn merge_pr(
    State(app): State<App>,
    Path((tenant, repo, number)): Path<(String, String, u64)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let key = format!("{tenant}/{repo}");
    let actor = match require_actor(&app, &headers, body.get("actor").and_then(Value::as_str).unwrap_or("")) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let Some(mut pr) = app.store.prs(&key).into_iter().find(|p| p.number == number) else {
        return (StatusCode::NOT_FOUND, "no such PR").into_response();
    };
    if pr.state == PrState::Merged {
        return (StatusCode::CONFLICT, "already merged").into_response();
    }
    // green keel verification of the proposed change
    let green = pr
        .changes
        .first()
        .and_then(|c| app.repos.verification(&tenant, &repo, c))
        .map(|v| v == "green")
        .unwrap_or(false);
    if !green {
        return (StatusCode::CONFLICT, "cannot merge: change is not keel-verify green").into_response();
    }
    // an independent approving review (approver != PR author)
    let has_independent_approval = app.store.reviews(&key).iter().any(|r| {
        r.target == format!("pr:{number}") && r.verdict == Verdict::Approve && r.reviewer != pr.author
    });
    if !has_independent_approval {
        return (StatusCode::CONFLICT, "cannot merge: needs an approving review from someone other than the author").into_response();
    }
    pr.state = PrState::Merged;
    pr.merged_by = Some(actor.id.clone());
    app.store.replace_pr(pr.clone());
    app.hub.publish(
        &tenant,
        ActivityEvent::Push { actor: actor.handle, repo: repo.clone(), change: pr.changes.first().cloned().unwrap_or_default(), ts: now() },
    );
    // Outbound mirror on change-land — guarded by loop prevention + idempotency.
    if let Some(change) = pr.changes.first() {
        mirror_out(&app, &tenant, &repo, change);
    }
    // Auto-close the issues this PR fixes, stamping the resolving keel change as provenance.
    let resolving = pr.changes.first().cloned();
    let mut closed: Vec<u64> = Vec::new();
    for num in closing_issue_numbers(&pr.title, &[]) {
        if let Some(mut issue) = app.store.issues(&pr.repo).into_iter().find(|i| i.number == num) {
            if matches!(issue.status, hull_core::IssueStatus::Open) {
                issue.status = hull_core::IssueStatus::Closed { reason: hull_core::CloseReason::Completed };
                issue.resolved_by = resolving.clone();
                if !issue.linked_prs.contains(&pr.id) {
                    issue.linked_prs.push(pr.id.clone());
                }
                let assignees = issue.assignees.clone();
                let author = issue.author.clone();
                app.store.replace_issue(issue);
                closed.push(num);
                let mut to = assignees;
                if !to.contains(&author) {
                    to.push(author);
                }
                app.registry.notify(&NotifyEvent {
                    kind: "issue_closed".into(),
                    to,
                    summary: format!("issue #{num} closed by merging PR !{number}"),
                    change: resolving.clone(),
                });
            }
        }
    }
    Json(json!({ "pr": pr, "closed_issues": closed })).into_response()
}

/// Issue numbers a PR closes: from closing keywords in the title (`fixes #12`, `closes #3`,
/// `resolves #7`) plus any explicit `closes` list. Deduped.
fn closing_issue_numbers(title: &str, explicit: &[u64]) -> Vec<u64> {
    let mut out: Vec<u64> = explicit.to_vec();
    let lower = title.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| c.is_whitespace() || c == ':' || c == ',' || c == '(').collect();
    const KW: &[&str] = &["fix", "fixes", "fixed", "close", "closes", "closed", "resolve", "resolves", "resolved"];
    for pair in words.windows(2) {
        if KW.contains(&pair[0]) {
            if let Some(n) = pair[1].strip_prefix('#').and_then(|s| s.parse::<u64>().ok()) {
                out.push(n);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Push a landed change out to the external forge, if the repo is mirrored — but only if the change
/// didn't originate on the other side (loop prevention) and hasn't already been pushed (idempotency).
/// A no-op when no mirror is configured. Returns whether a push happened (for the inbound test path).
fn mirror_out(app: &App, tenant: &str, repo: &str, change: &str) -> bool {
    let key = format!("{tenant}/{repo}");
    let Some(target) = app.registry.mirror_target(&key) else { return false };
    if !app.mirror.should_push_out(change) {
        return false; // originated on the forge — pushing it back would loop
    }
    if !app.mirror.mark_processed(&format!("out:{change}")) {
        return false; // already pushed this change
    }
    let info = app.repos.change_info(tenant, repo, change);
    let (intent, author) = info.map(|i| (i.intent, i.author)).unwrap_or_default();
    let result = app.registry.mirror_push(&hull_plugin::MirrorPush {
        repo: key.clone(),
        change: change.to_string(),
        intent,
        author,
    });
    if result.ok {
        app.mirror.set_origin(change, "hull");
        app.mirror.record_outbound(mirror::Outbound {
            repo: key,
            change: change.to_string(),
            target: target.clone(),
            external_ref: result.external_ref.unwrap_or_default(),
            ts: now(),
        });
        app.registry.notify(&NotifyEvent {
            kind: "mirror_pushed".into(),
            to: vec![],
            summary: format!("mirrored {}/{} @ {} → {target}", tenant, repo, &change[..change.len().min(12)]),
            change: Some(change.to_string()),
        });
    }
    result.ok
}

/// The repo's mirror status (`GET /api/repos/:tenant/:repo/mirror`): the external target it's linked
/// to (if any) and the outbound pushes recorded, for the UI's mirror panel.
async fn mirror_status(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>) -> Json<Value> {
    let key = format!("{tenant}/{repo}");
    Json(json!({
        "target": app.registry.mirror_target(&key),
        "outbound": app.mirror.outbound_for(&key),
    }))
}

/// Inbound mirror (`POST /api/repos/:tenant/:repo/mirror/inbound`) — a forge → Hull delivery
/// (GitHub webhook, in production). Body: `{external_id, change, intent?, author?}`. Idempotent on
/// `external_id` (redelivery is a no-op) and it stamps the change's origin as `github`, so the
/// change-land trigger will **not** push it back out (loop prevention). Accountable-only (the mirror
/// service's actor stands in for the webhook secret in this scaffold).
async fn mirror_inbound(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(resp) = require_actor(&app, &headers, body.get("actor").and_then(Value::as_str).unwrap_or("")) {
        return resp;
    }
    let external_id = body.get("external_id").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let change = body.get("change").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if external_id.is_empty() || change.is_empty() {
        return (StatusCode::BAD_REQUEST, "external_id and change are required").into_response();
    }
    // Idempotency: a redelivered webhook is a no-op.
    if !app.mirror.mark_processed(&format!("in:{external_id}")) {
        return Json(json!({ "processed": false, "duplicate": true, "change": change })).into_response();
    }
    // Loop prevention: mark this change as forge-originated so it is never pushed back out.
    app.mirror.set_origin(&change, "github");
    let author = body.get("author").and_then(Value::as_str).unwrap_or("mirror").to_string();
    app.hub.publish(
        &tenant,
        ActivityEvent::Push { actor: author, repo: repo.clone(), change: change.clone(), ts: now() },
    );
    (StatusCode::CREATED, Json(json!({ "processed": true, "duplicate": false, "change": change, "origin": "github" }))).into_response()
}

/// List reviews for a hosted repo (`GET /api/repos/:tenant/:repo/reviews`); the client filters by
/// target (e.g. `pr:1`).
async fn reviews(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>) -> Json<Value> {
    Json(json!({ "reviews": app.store.reviews(&format!("{tenant}/{repo}")) }))
}

/// Post a review (`POST /api/repos/:tenant/:repo/reviews`) — `{target, reviewer, verdict, summary}`.
/// The reviewer must be an accountable actor. Review is independent by construction: a PR's author
/// cannot approve their own PR (matches the "never self-merge / human review gate" rule).
async fn create_review(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let key = format!("{tenant}/{repo}");
    let reviewer = match require_actor(&app, &headers, body.get("reviewer").and_then(Value::as_str).unwrap_or("")) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let target = body.get("target").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if target.is_empty() {
        return (StatusCode::BAD_REQUEST, "target is required (e.g. 'pr:1')").into_response();
    }
    let verdict = match body.get("verdict").and_then(Value::as_str) {
        Some("approve") => Verdict::Approve,
        Some("request_changes") => Verdict::RequestChanges,
        Some("reject") => Verdict::Reject,
        _ => Verdict::Comment,
    };
    // Independent-review rule: you can't approve your own PR.
    if verdict == Verdict::Approve {
        if let Some(num) = target.strip_prefix("pr:").and_then(|s| s.parse::<u64>().ok()) {
            if let Some(pr) = app.store.prs(&key).into_iter().find(|p| p.number == num) {
                if pr.author == reviewer.id {
                    return (StatusCode::CONFLICT, "a PR author cannot approve their own PR — review must be independent").into_response();
                }
            }
        }
    }
    let findings: Vec<ReviewFinding> = body
        .get("findings")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    let path = f.get("path").and_then(Value::as_str)?.to_string();
                    Some(ReviewFinding {
                        path,
                        line: f.get("line").and_then(Value::as_u64).map(|n| n as u32),
                        severity: f.get("severity").and_then(Value::as_str).unwrap_or("info").to_string(),
                        note: f.get("note").and_then(Value::as_str).unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let count = app.store.reviews(&key).len();
    let review = Review {
        id: format!("rv_{}_{}", key.replace('/', "_"), count + 1),
        repo: key,
        target,
        reviewer: reviewer.id.clone(),
        verdict,
        summary: body.get("summary").and_then(Value::as_str).unwrap_or("").to_string(),
        findings,
        ledger: None,
        created_unix: now(),
    };
    app.store.put_review(review.clone());
    // Notify the PR's author via the Notifier plugin capability (core records + logs it; a hosted
    // plugin would also deliver over Slack/email/nostr).
    if let Some(num) = review.target.strip_prefix("pr:").and_then(|s| s.parse::<u64>().ok()) {
        if let Some(pr) = app.store.prs(&review.repo).into_iter().find(|p| p.number == num) {
            app.registry.notify(&NotifyEvent {
                kind: "review_posted".into(),
                to: vec![pr.author.clone()],
                summary: format!("{} posted a {:?} review on PR !{num}", reviewer.handle, review.verdict),
                change: pr.changes.first().cloned(),
            });
        }
    }
    (StatusCode::CREATED, Json(json!({ "review": review }))).into_response()
}

/// **Agent auto-review** (`POST /api/repos/:tenant/:repo/prs/:number/auto-review`) — the reviewer
/// runtime producing an accountable review. An **agent** actor runs the change's checks, reconciles
/// its claims against the facts, and posts a real [`Review`]: findings for every contradicted claim,
/// and a verdict (request-changes if anything is contradicted; approve only if checks are green and
/// claims hold up). Gated: the reviewer must be an agent, accountable, and independent of the PR
/// author — an agent can never rubber-stamp its own work.
async fn auto_review(
    State(app): State<App>,
    Path((tenant, repo, number)): Path<(String, String, u64)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let reviewer = match require_actor(&app, &headers, body.get("reviewer").and_then(Value::as_str).unwrap_or("")) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    match perform_auto_review(&app, &tenant, &repo, number, &reviewer).await {
        Ok(review) => (StatusCode::CREATED, Json(json!({ "review": review }))).into_response(),
        Err((code, msg)) => (code, msg).into_response(),
    }
}

/// The reviewer runtime: run a PR's checks, reconcile its change, and post an accountable agent
/// review. Shared by the explicit endpoint and the on-open agent flow. Enforces the gate (agent,
/// independent of author) itself, so every caller is safe.
async fn perform_auto_review(
    app: &App,
    tenant: &str,
    repo: &str,
    number: u64,
    reviewer: &hull_core::Actor,
) -> Result<Review, (StatusCode, String)> {
    let key = format!("{tenant}/{repo}");
    if reviewer.kind != hull_core::ActorKind::Agent {
        return Err((StatusCode::FORBIDDEN, "auto-review must be performed by an agent actor".into()));
    }
    let Some(pr) = app.store.prs(&key).into_iter().find(|p| p.number == number) else {
        return Err((StatusCode::NOT_FOUND, "no such PR".into()));
    };
    if pr.author == reviewer.id {
        return Err((StatusCode::CONFLICT, "an agent cannot auto-review its own PR — review must be independent".into()));
    }
    let Some(change) = pr.changes.first().cloned() else {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "PR has no change to review".into()));
    };

    // 1. Trigger the change's checks — local run resolves now; an external CI is dispatched and its
    //    verdict lands later via callback (the review posts on current verification either way).
    let check_label = match resolve_check(app, tenant, repo, &change, false).await {
        CiResolution::Done(o) => ci_status_str(o.status).to_string(),
        CiResolution::Dispatched { .. } => "dispatched".into(),
        CiResolution::Pending => "running".into(),
        CiResolution::Failed(_) => "not-run".into(),
    };
    let verification = app.repos.verification(tenant, repo, &change).unwrap_or_else(|| "unverified".into());

    // 2. Reconcile the change's narrative against its facts.
    let Some(info) = app.repos.change_info(tenant, repo, &change) else {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "cannot resolve change".into()));
    };
    let lesson = app.store.session_record(&key, &change).map(|s| s.lesson).unwrap_or_default();
    let facts = app.repos.facts(tenant, repo, &change);
    let ledger = hull_core::reconcile::reconcile(&change, &info.intent, &lesson, &facts);

    // 3. Synthesize a blocker finding for every contradicted claim.
    let anchor = info.files.first().map(|f| f.path.clone()).unwrap_or_else(|| format!("change:{}", &change[..change.len().min(12)]));
    let mut findings: Vec<ReviewFinding> = Vec::new();
    for claim in &ledger.claims {
        use hull_core::reconcile::ClaimStatus;
        if claim.status == ClaimStatus::Contradicted {
            let ev = claim.evidence.iter().find(|e| !e.supports).map(|e| e.detail.clone()).unwrap_or_default();
            findings.push(ReviewFinding {
                path: anchor.clone(),
                line: None,
                severity: "blocker".into(),
                note: format!("Claim not supported by the change: “{}” — {ev}", claim.text),
            });
        }
    }

    // 4. Verdict.
    let contradicted = ledger.contradicted();
    let verdict = if contradicted > 0 {
        Verdict::RequestChanges
    } else if verification == "green" && ledger.supported() > 0 {
        Verdict::Approve
    } else {
        Verdict::Comment
    };
    let summary = format!(
        "Agent reconciliation review: {} claims — {} supported, {} contradicted; checks {}. {}",
        ledger.claims.len(),
        ledger.supported(),
        contradicted,
        check_label,
        match verdict {
            Verdict::Approve => "Narrative matches the change and checks are green — approving.",
            Verdict::RequestChanges => "The change contradicts its own stated intent — requesting changes.",
            _ => "Checks are not green or nothing corroborates the intent — leaving a comment for a human.",
        }
    );

    let count = app.store.reviews(&key).len();
    let review = Review {
        id: format!("rv_{}_{}", key.replace('/', "_"), count + 1),
        repo: key.clone(),
        target: format!("pr:{number}"),
        reviewer: reviewer.id.clone(),
        verdict,
        summary,
        findings,
        ledger: Some(ledger),
        created_unix: now(),
    };
    app.store.put_review(review.clone());
    app.registry.notify(&NotifyEvent {
        kind: "review_posted".into(),
        to: vec![pr.author.clone()],
        summary: format!("{} auto-reviewed PR !{number}: {:?}", reviewer.handle, review.verdict),
        change: Some(change),
    });
    Ok(review)
}

/// Pick an agent actor that may independently review a PR by `author` — the reviewer for the on-open
/// agent flow. `None` if no agent other than the author is registered.
fn independent_agent_reviewer(app: &App, author: &str) -> Option<hull_core::Actor> {
    app.store
        .actors()
        .into_iter()
        .find(|a| a.kind == hull_core::ActorKind::Agent && a.id != author)
}

/// List pull requests for a hosted repo (`GET /api/repos/:tenant/:repo/prs`). Each PR's verification
/// is refreshed live from keel (the change's verify state), so an approving review + green keel
/// verify shows on the badge.
async fn prs(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>) -> Json<Value> {
    let mut list = app.store.prs(&format!("{tenant}/{repo}"));
    for pr in &mut list {
        if let Some(c) = pr.changes.first() {
            if let Some(v) = app.repos.verification(&tenant, &repo, c) {
                pr.verification = match v.as_str() {
                    "green" => Verification::Green,
                    "red" => Verification::Red,
                    _ => Verification::Unverified,
                };
            }
        }
    }
    Json(json!({ "prs": list }))
}

/// Set a change's keel verification (`POST /api/repos/:tenant/:repo/change/:id/verify` with
/// `{"green": true|false}`) — the same side table `keel verify` writes. Gated to an accountable
/// actor. PR badges refresh from this on the next list.
async fn verify_change(
    State(app): State<App>,
    Path((tenant, repo, id)): Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(resp) = require_actor(&app, &headers, body.get("actor").and_then(Value::as_str).unwrap_or("")) {
        return resp;
    }
    let green = body.get("green").and_then(Value::as_bool).unwrap_or(true);
    if app.repos.set_verification(&tenant, &repo, &id, green) {
        Json(json!({ "verification": if green { "green" } else { "red" } })).into_response()
    } else {
        (StatusCode::UNPROCESSABLE_ENTITY, "unknown change").into_response()
    }
}

/// Open a PR (`POST /api/repos/:tenant/:repo/prs`). It proposes real keel changes: `changes` may be
/// given explicitly, else it anchors to the repo's current HEAD change (content-addressed). Author
/// is gated by the accountability rule. Verification mirrors keel and starts Unverified.
async fn create_pr(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let key = format!("{tenant}/{repo}");
    let actor = match require_actor(&app, &headers, body.get("author").and_then(Value::as_str).unwrap_or("")) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let title = body.get("title").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if title.is_empty() {
        return (StatusCode::BAD_REQUEST, "title is required").into_response();
    }
    let changes: Vec<String> = match body.get("changes").and_then(Value::as_array) {
        Some(arr) => arr.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        None => app.repos.head_change(&tenant, &repo).into_iter().collect(),
    };
    if changes.is_empty() {
        return (StatusCode::UNPROCESSABLE_ENTITY, "no changes to propose (empty repo?)").into_response();
    }
    let number = app.store.prs(&key).iter().map(|p| p.number).max().unwrap_or(0) + 1;
    // Code owners: resolve the owners of any file this change touches — they become requested
    // reviewers and get notified.
    let files: Vec<String> = changes
        .first()
        .and_then(|c| app.repos.change_info(&tenant, &repo, c))
        .map(|ci| ci.files.into_iter().map(|f| f.path).collect())
        .unwrap_or_default();
    let owners = owners_for(&app, &key, &files);
    let pr = PullRequest {
        id: format!("pr_{}_{number}", key.replace('/', "_")),
        repo: key,
        number,
        title,
        author: actor.id,
        changes,
        verification: Verification::Unverified,
        reviewers: owners.clone(),
        state: PrState::Open,
        merged_by: None,
        created_unix: now(),
    };
    if !owners.is_empty() {
        app.registry.notify(&NotifyEvent {
            kind: "code_owner_referenced".into(),
            to: owners,
            summary: format!("your code is in PR !{number}: {}", pr.title),
            change: pr.changes.first().cloned(),
        });
    }
    app.store.put_pr(pr.clone());
    // Link the issues this PR closes (from `fixes #N` in the title, or an explicit `closes` list) so
    // they show the incoming PR now and auto-close when it merges.
    let explicit: Vec<u64> = body.get("closes").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
    for num in closing_issue_numbers(&pr.title, &explicit) {
        if let Some(mut issue) = app.store.issues(&pr.repo).into_iter().find(|i| i.number == num) {
            if !issue.linked_prs.contains(&pr.id) {
                issue.linked_prs.push(pr.id.clone());
                app.store.replace_issue(issue);
            }
        }
    }
    app.hub.publish(
        &tenant,
        ActivityEvent::Push { actor: actor.handle, repo: repo.clone(), change: pr.changes[0].clone(), ts: now() },
    );
    // Agent flow (M6): when a PR opens, an independent agent reviewer runs the reconciliation review
    // automatically — the fully autonomous loop (agent commits → PR → agent reviews → gate enforced).
    // Fire-and-forget so the response isn't blocked on the test run; the review lands in the feed.
    if let Some(agent) = independent_agent_reviewer(&app, &pr.author) {
        let (app2, t2, r2, n2) = (app.clone(), tenant.clone(), repo.clone(), number);
        tokio::spawn(async move {
            let _ = perform_auto_review(&app2, &t2, &r2, n2, &agent).await;
        });
    }
    (StatusCode::CREATED, Json(json!({ "pr": pr }))).into_response()
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
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let key = format!("{tenant}/{repo}");
    // Accountability gate: the author is the signed-in actor (or the body actor for scripts).
    let author_id = body.get("author").and_then(Value::as_str).unwrap_or("");
    let actor = match require_actor(&app, &headers, author_id) {
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
    if !issue.assignees.is_empty() {
        app.registry.notify(&NotifyEvent {
            kind: "issue_assigned".into(),
            to: issue.assignees.clone(),
            summary: format!("{} assigned issue #{number}: {}", actor.handle, issue.title),
            change: None,
        });
    }
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
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let key = format!("{tenant}/{repo}");
    // A transition is an authoring action — the acting actor must chain to a human.
    let acting = match require_actor(&app, &headers, body.get("actor").and_then(Value::as_str).unwrap_or("")) {
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
    for name in ["keel", "hull"] {
        store.put_repo(Repo {
            id: format!("repo_{name}"),
            owner: "acct_tankrap".into(),
            name: name.into(),
            default_branch: "main".into(),
        });
    }
    // A human root + an agent it delegated — both real Ed25519 identities, both org members.
    let human = identity::mint_human("justin").actor;
    store.put_actor(human.clone());
    let mut members = vec![Membership { actor: human.id.clone(), role: Role::Owner }];
    if let Some(agent) =
        identity::mint_agent("agent:reviewer", &human, "issues:*", Lifetime::Static)
    {
        members.push(Membership { actor: agent.actor.id.clone(), role: Role::Write });
        // agent:reviewer owns the server crate — it'll be auto-requested on PRs touching it.
        store.set_owners(
            "tankrap/hull",
            vec![OwnerRule { glob: "crates/hull-server/**".into(), owners: vec![agent.actor.id.clone()] }],
        );
        store.put_actor(agent.actor);
    }
    store.put_account(Account {
        id: "acct_tankrap".into(),
        kind: AccountKind::Organization,
        handle: "tankrap".into(),
        members,
    });
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
