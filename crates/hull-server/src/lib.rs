//! The Hull server as a **library**, so both the OSS binary and a private hosted binary reuse it.
//!
//! The open-core seam is [`run`]'s `register_plugins` argument: the OSS binary passes a no-op; a
//! hosted binary (in a separate private repo) passes a closure that registers its closed plugins —
//! `hull_server::run(opts, |reg| hull_hosted::register(reg))`. The core never names a hosted crate.
//!
//! Endpoints: `/health` · `/api/home` · `/api/feed` (SSE) · `/api/repos` ·
//! `/api/repos/:repo/issues` · `/api/scan` · `/api/plugins`.

pub mod activity;
pub mod artifacts;
pub mod autonomy;
pub mod ci;
pub mod claims;
pub mod reviewcache;
pub mod ingress;
pub mod keeld;
pub mod mirror;
pub mod nostr;
pub mod plugins;
pub mod quic;
pub mod repos;

use activity::{ActivityEvent, ActivityHub};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    response::{IntoResponse, Response},
    routing::{get, post, put},
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
    claims: Arc<claims::ClaimResolutions>,
    artifacts: Arc<artifacts::ArtifactStore>,
    review_cache: Arc<reviewcache::ReviewCache>,
    autonomy: Arc<autonomy::AutonomyStore>,
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
    // Decentralized fan-out: if a nostr publisher key + relays are configured, code-owner pings are
    // also published as signed nostr events to opted-in actors. Off by default (OSS stays log-only).
    if let Some(n) = nostr::NostrNotifier::from_env(store.clone()) {
        eprintln!("nostr: code-owner notifications enabled → {} relay(s)", n.relays().len());
        registry.add_notifier(Arc::new(n));
    }
    App {
        store,
        hub,
        registry: Arc::new(registry),
        repos: repos::RepoHost::from_env(),
        notifications,
        auth: Arc::new(Mutex::new(AuthState::default())),
        ci: Arc::new(ci::CiMemo::from_env()),
        claims: Arc::new(claims::ClaimResolutions::from_env()),
        artifacts: Arc::new(artifacts::ArtifactStore::from_env()),
        review_cache: Arc::new(reviewcache::ReviewCache::from_env()),
        autonomy: Arc::new(autonomy::AutonomyStore::from_env()),
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
    ensure_demo_owner(store);
    backfill_members(store);
    backfill_accountability(store);
}

/// Migration for the crypto-delegation milestone (NEW-1166): any agent whose delegation doesn't
/// cryptographically verify — including legacy agents minted before hops were signed, or with no
/// delegation at all — is re-rooted at the demo human with a **signed** hop. Without this, enforcing
/// [`Delegation::verify`] at the authoring gate would lock out agents seeded by an earlier build.
/// Idempotent: already-verifiable agents are skipped. Only the demo human can be signed for here (its
/// key is known); a real deployment re-delegates through the owning human instead.
fn backfill_accountability(store: &dyn Store) {
    use hull_core::{ActorKind, Delegation, DelegationHop};
    let Some(demo) = identity::human_from_secret("demo", DEMO_OWNER_SECRET) else { return };
    let demo_id = demo.actor.id;
    let no_rev = |_: &str| false;
    for mut a in store.actors() {
        if a.kind != ActorKind::Agent {
            continue;
        }
        let verified = a.delegation.as_ref().map(|d| d.verify(&a.id, 0, &no_rev).is_ok()).unwrap_or(false);
        if verified {
            continue;
        }
        let Some(sig) = identity::sign_hop(DEMO_OWNER_SECRET, &demo_id, &a.id, ActorKind::Agent, "*", 0) else { continue };
        a.delegation = Some(Delegation {
            chain: vec![
                DelegationHop { principal: demo_id.clone(), kind: ActorKind::Human, scope: "*".into(), expires_unix: 0, signature: vec![] },
                DelegationHop { principal: a.id.clone(), kind: ActorKind::Agent, scope: "*".into(), expires_unix: 0, signature: sig },
            ],
        });
        eprintln!("hull: backfilled a signed delegation for agent {} (rooted at demo)", a.handle);
        store.put_actor(a);
    }
}

/// A published demo credential: a fixed Ed25519 secret so a local/demo instance has a **known** human
/// you can log into (through the real signature flow) and exercise owner-only features. This is not a
/// backdoor — login still verifies the signature; it's just a demo account whose key is public. The
/// frontend's "Sign in as demo" uses the same secret. Never ship this key on a real deployment.
const DEMO_OWNER_SECRET: &str = "68756c6c2d64656d6f2d6f776e65722d6b65792d64656d6f2d6f6e6c79212121";

/// Ensure the demo owner exists and owns every org, so a fresh login lands on a usable account.
/// Idempotent.
fn ensure_demo_owner(store: &dyn Store) {
    use hull_core::{Membership, Role};
    let Some(minted) = identity::human_from_secret("demo", DEMO_OWNER_SECRET) else { return };
    let id = minted.actor.id.clone();
    if store.actor(&id).is_none() {
        store.put_actor(minted.actor);
    }
    for mut acct in store.accounts() {
        if !acct.members.iter().any(|m| m.actor == id) {
            acct.members.push(Membership { actor: id.clone(), role: Role::Owner });
            store.put_account(acct);
        }
    }
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
        .route("/api/actors/:id/revoke", post(revoke_actor))
        .route("/api/actors/:id/renew", post(renew_delegation))
        .route("/api/actors/:id/nostr", post(set_nostr_key))
        .route("/api/actors/:id/github", post(link_github))
        .route("/api/accounts", get(accounts_list))
        .route("/api/accounts/:id/members", post(add_member))
        .route("/api/auth/challenge", get(auth_challenge))
        .route("/api/auth/login", post(auth_login))
        .route("/api/auth/me", get(auth_me))
        .route("/api/me", get(me_profile))
        .route("/api/notifications", get(notifications_list))
        .route("/api/repos", get(repos_list))
        .route("/api/repos/:tenant/:repo/issues", get(issues).post(create_issue))
        .route("/api/repos/:tenant/:repo/issues/:number", axum::routing::patch(update_issue))
        .route("/api/repos/:tenant/:repo/why", get(why))
        .route("/api/repos/:tenant/:repo/prs", get(prs).post(create_pr))
        .route("/api/repos/:tenant/:repo/prs/:number/merge", post(merge_pr))
        .route("/api/repos/:tenant/:repo/prs/:number/close", post(close_pr))
        .route("/api/repos/:tenant/:repo/prs/:number/auto-review", post(auto_review))
        .route("/api/repos/:tenant/:repo/prs/:number/reviewers", post(request_reviewer))
        .route("/api/repos/:tenant/:repo/prs/:number/fix", post(fix_finding))
        .route("/api/repos/:tenant/:repo/mirror", get(mirror_status))
        .route("/api/repos/:tenant/:repo/mirror/inbound", post(mirror_inbound))
        .route("/api/repos/:tenant/:repo/reviews", get(reviews).post(create_review))
        .route("/api/repos/:tenant/:repo/artifacts/:id", get(get_artifact))
        .route("/api/repos/:tenant/:repo/comments", get(comments_list).post(create_comment))
        .route("/api/repos/:tenant/:repo/change/:id", get(change_info))
        .route("/api/repos/:tenant/:repo/change/:id/diff", get(change_diff))
        .route("/api/repos/:tenant/:repo/change/:id/semantic", get(change_semantic))
        .route("/api/repos/:tenant/:repo/change/:id/ledger", get(change_ledger))
        .route("/api/repos/:tenant/:repo/change/:id/claims/:claim/resolve", post(resolve_claim))
        .route("/api/repos/:tenant/:repo/tree/:tree/tar", get(tree_archive))
        .route("/api/repos/:tenant/:repo/change/:id/check", post(run_check_handler))
        .route("/api/repos/:tenant/:repo/change/:id/ci-result", post(ci_result))
        .route("/api/repos/:tenant/:repo/ci-config", get(get_ci_config).put(set_ci_config))
        .route("/api/repos/:tenant/:repo/autonomy", get(get_repo_autonomy).put(set_repo_autonomy))
        .route("/api/accounts/:id/autonomy", put(set_account_autonomy))
        .route("/api/repos/:tenant/:repo/security", get(repo_security))
        .route("/api/repos/:tenant/:repo/owners", get(owners_list).post(set_owners))
        .route("/api/repos/:tenant/:repo/change/:id/verify", post(verify_change))
        .route("/api/repos/:tenant/:repo/change/:id/session", post(ingest_session))
        .route("/api/scan", post(scan))
        .route("/api/plugins", get(plugins_list))
        // git smart-HTTP: host N keel repos at /{tenant}/{repo} (clone / fetch / push).
        .route("/:tenant/:repo/info/refs", get(repos::info_refs::<App>))
        .route("/:tenant/:repo/git-upload-pack", post(repos::upload_pack::<App>))
        .route("/:tenant/:repo/git-receive-pack", post(receive_pack_handler))
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
                // Reflect the real gate: cryptographic verification + revocation, not just structure.
                "accountable": accountable(&app, &a).is_ok(),
                "revoked": a.revoked,
                "human_root": a.human_principal(),
                "github": app.mirror.github_for(&a.id),
            })
        })
        .collect();
    Json(json!({ "actors": actors }))
}

/// Register (mint) an actor with a real Ed25519 keypair (`POST /api/actors`). A `human` is its own
/// root; an `agent` must name `delegated_by` (an existing accountable actor) and gets a delegation
/// chain rooting at that human — enforcing "no unaccountable agents" at mint. The secret key is
/// returned ONCE and never stored.
async fn register_actor(State(app): State<App>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let handle = body.get("handle").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if handle.is_empty() {
        return (StatusCode::BAD_REQUEST, "handle is required").into_response();
    }
    let minted = match body.get("kind").and_then(Value::as_str).unwrap_or("human") {
        // Creating a *new* human identity is open self-serve onboarding — you're a new person, not
        // claiming to be an existing one.
        "human" => identity::mint_human(&handle),
        // An agent is delegated by its parent — the **authenticated caller**, never a body field, so a
        // delegation can't be forged in someone else's name. The delegation hop is signed by the
        // parent's Ed25519 key (NEW-1166): preferably client-side (the caller generates the child key
        // and signs `child_pub`, so Hull never sees the agent's secret), with a demo-owner fallback
        // where Hull holds the key.
        "agent" => {
            let parent = match require_actor(&app, &headers, "") {
                Ok(a) => a,
                Err(resp) => return resp,
            };
            let scope = body.get("scope").and_then(Value::as_str).unwrap_or("*");
            let child_pub = body.get("child_pub").and_then(Value::as_str).unwrap_or("").trim();
            let sig_hex = body.get("delegation_sig").and_then(Value::as_str).unwrap_or("").trim();
            // A standing agent may be minted with a short TTL (`expires_unix`) — "never an eternal
            // token"; the client signs the same expiry into the hop. 0 = no expiry.
            let expires_unix = body.get("expires_unix").and_then(Value::as_u64).unwrap_or(0);
            let lifetime = if expires_unix > 0 { Lifetime::Ephemeral { expires_unix } } else { Lifetime::Static };
            if !child_pub.is_empty() && !sig_hex.is_empty() {
                // Client-signed: verify the parent's signature by assembling + verifying the chain.
                let Ok(sig) = hex::decode(sig_hex) else { return (StatusCode::BAD_REQUEST, "delegation_sig must be hex").into_response() };
                match identity::delegate(&handle, &parent, child_pub, scope, lifetime, sig) {
                    Some(actor) => {
                        app.store.put_actor(actor.clone());
                        return (StatusCode::CREATED, Json(json!({ "actor": actor }))).into_response();
                    }
                    None => return (StatusCode::UNPROCESSABLE_ENTITY, "delegation did not verify — bad signature, widened scope, or unaccountable parent").into_response(),
                }
            }
            // Fallback: only the demo owner's key is known server-side, so Hull can sign for it.
            let demo_id = identity::human_from_secret("demo", DEMO_OWNER_SECRET).map(|m| m.actor.id).unwrap_or_default();
            if parent.id == demo_id {
                match identity::mint_agent(&handle, &parent, DEMO_OWNER_SECRET, scope, lifetime) {
                    Some(m) => m,
                    None => return (StatusCode::UNPROCESSABLE_ENTITY, "could not mint — parent is not accountable").into_response(),
                }
            } else {
                return (StatusCode::UNPROCESSABLE_ENTITY, "sign the delegation client-side: send { child_pub, delegation_sig } (Hull never holds an agent's secret)").into_response();
            }
        }
        _ => return (StatusCode::BAD_REQUEST, "kind must be 'human' or 'agent'").into_response(),
    };
    app.store.put_actor(minted.actor.clone());
    (StatusCode::CREATED, Json(json!({ "actor": minted.actor, "secret_key": minted.secret_key }))).into_response()
}

/// Revoke an actor (`POST /api/actors/:id/revoke`). Only an **ancestor** may revoke — the caller must
/// be the target itself or appear as a principal in the target's delegation chain (its human root or
/// an intermediate agent). Revocation propagates: because the revoked id sits in every descendant's
/// chain, [`accountable`] then rejects the whole subtree. Blast radius = the subtree.
async fn revoke_actor(State(app): State<App>, headers: axum::http::HeaderMap, Path(id): Path<String>) -> Response {
    let caller = match require_actor(&app, &headers, "") {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let Some(mut target) = app.store.actor(&id) else {
        return (StatusCode::NOT_FOUND, "no such actor").into_response();
    };
    let ancestor = target.id == caller.id
        || target.delegation.as_ref().map(|d| d.chain.iter().any(|h| h.principal == caller.id)).unwrap_or(false);
    if !ancestor {
        return (StatusCode::FORBIDDEN, "you may only revoke an actor in your own delegation subtree").into_response();
    }
    target.revoked = true;
    app.store.put_actor(target);
    Json(json!({ "revoked": id, "by": caller.handle })).into_response()
}

/// Renew a standing agent's short-TTL delegation (`POST /api/actors/:id/renew` `{expires_unix,
/// delegation_sig}`). "Never an eternal token": a standing agent holds a short-lived delegation that
/// its **delegating parent** (a machine credential that itself chains to the human) re-issues before
/// it lapses. Only that immediate parent may renew — it signs a fresh hop over the same scope with a
/// new expiry; Hull swaps in the new leaf hop and re-verifies. Revocation of any ancestor still kills
/// it, and the parent can simply stop renewing to let it expire.
async fn renew_delegation(State(app): State<App>, headers: axum::http::HeaderMap, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    use hull_core::{ActorKind, Delegation, DelegationHop};
    let caller = match require_actor(&app, &headers, "") {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let Some(mut target) = app.store.actor(&id) else {
        return (StatusCode::NOT_FOUND, "no such actor").into_response();
    };
    let Some(chain) = target.delegation.as_ref().map(|d| d.chain.clone()) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "actor has no delegation to renew").into_response();
    };
    if chain.len() < 2 {
        return (StatusCode::UNPROCESSABLE_ENTITY, "delegation has no delegating parent").into_response();
    }
    // Only the leaf's immediate parent may renew it (the machine credential that issued it).
    if chain[chain.len() - 2].principal != caller.id {
        return (StatusCode::FORBIDDEN, "only the delegating parent may renew this agent").into_response();
    }
    let scope = chain[chain.len() - 1].scope.clone(); // renewal keeps the same (already-attenuated) scope
    let expires_unix = body.get("expires_unix").and_then(Value::as_u64).unwrap_or(0);
    let sig_hex = body.get("delegation_sig").and_then(Value::as_str).unwrap_or("").trim();
    let Ok(sig) = hex::decode(sig_hex) else { return (StatusCode::BAD_REQUEST, "delegation_sig must be hex").into_response() };
    // The parent must have signed the fresh hop (over its own id → this leaf, the same scope, new TTL).
    let msg = identity::hop_message(&caller.id, &id, ActorKind::Agent, &scope, expires_unix);
    if !identity::verify_bytes(&caller.id, &msg, &sig) {
        return (StatusCode::UNPROCESSABLE_ENTITY, "renewal signature does not verify").into_response();
    }
    let mut new_chain = chain;
    let last = new_chain.len() - 1;
    new_chain[last] = DelegationHop { principal: id.clone(), kind: ActorKind::Agent, scope, expires_unix, signature: sig };
    let deleg = Delegation { chain: new_chain };
    if deleg.verify(&id, 0, &|_| false).is_err() {
        return (StatusCode::UNPROCESSABLE_ENTITY, "renewed delegation does not verify").into_response();
    }
    target.lifetime = if expires_unix > 0 { Lifetime::Ephemeral { expires_unix } } else { Lifetime::Static };
    target.delegation = Some(deleg);
    app.store.put_actor(target);
    Json(json!({ "renewed": id, "expires_unix": expires_unix })).into_response()
}

/// Link your forge (GitHub) login to your hull actor (`POST /api/actors/:id/github` `{login}`).
/// Self-only. This is the accountability map across the mirror (NEW-1176): git commits you author on
/// GitHub, imported into Hull, then resolve to **you** (an accountable hull actor) instead of an
/// anonymous external identity. `login: ""` clears the link.
async fn link_github(State(app): State<App>, headers: axum::http::HeaderMap, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let caller = match require_actor(&app, &headers, "") {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if caller.id != id {
        return (StatusCode::FORBIDDEN, "you may only link your own GitHub login").into_response();
    }
    let login = body.get("login").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if login.is_empty() {
        if let Some(existing) = app.mirror.github_for(&id) {
            app.mirror.link_github(&existing, ""); // clear the caller's current link
        }
        return Json(json!({ "id": id, "linked": false })).into_response();
    }
    app.mirror.link_github(&login, &id);
    Json(json!({ "id": id, "github_login": login, "linked": true })).into_response()
}

/// Opt an actor into nostr notifications (`POST /api/actors/:id/nostr` `{pubkey}`). Self-only: you
/// set your **own** nostr pubkey (32-byte x-only hex). Code you own then pings you over nostr.
async fn set_nostr_key(State(app): State<App>, headers: axum::http::HeaderMap, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let caller = match require_actor(&app, &headers, "") {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if caller.id != id {
        return (StatusCode::FORBIDDEN, "you may only set your own nostr key").into_response();
    }
    let pubkey = body.get("pubkey").and_then(Value::as_str).unwrap_or("").trim().to_string();
    // A nostr x-only pubkey is 32 bytes → 64 hex chars. Empty clears the opt-in.
    if !pubkey.is_empty() && (pubkey.len() != 64 || hex::decode(&pubkey).is_err()) {
        return (StatusCode::BAD_REQUEST, "pubkey must be a 32-byte hex nostr key (or empty to clear)").into_response();
    }
    let Some(mut actor) = app.store.actor(&id) else {
        return (StatusCode::NOT_FOUND, "no such actor").into_response();
    };
    actor.nostr_pubkey = (!pubkey.is_empty()).then_some(pubkey);
    app.store.put_actor(actor);
    Json(json!({ "id": id, "nostr": true })).into_response()
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
    match app.store.actor(&actor) {
        None => return (StatusCode::UNAUTHORIZED, "unknown actor").into_response(),
        Some(a) if a.revoked => return (StatusCode::UNAUTHORIZED, "this actor has been revoked").into_response(),
        Some(_) => {}
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

/// The signed-in actor's full profile (`GET /api/me`): identity, **accountability chain** (for an
/// agent, the delegation hops back to its human root), and org memberships + roles. The mirror of
/// "who am I and what am I allowed to be" — read-only; there is no key rotation because the actor id
/// *is* the public key (rotating it would be a different actor).
async fn me_profile(State(app): State<App>, headers: axum::http::HeaderMap) -> Response {
    let Some(a) = authed_actor(&app, &headers) else {
        return (StatusCode::UNAUTHORIZED, "not signed in").into_response();
    };
    let handle_of = |id: &str| app.store.actor(id).map(|x| x.handle).unwrap_or_else(|| id.chars().take(10).collect());
    let chain: Vec<Value> = a
        .delegation
        .as_ref()
        .map(|d| {
            d.chain
                .iter()
                .map(|h| json!({ "principal": h.principal, "handle": handle_of(&h.principal), "kind": h.kind, "scope": h.scope }))
                .collect()
        })
        .unwrap_or_default();
    let memberships: Vec<Value> = app
        .store
        .accounts()
        .into_iter()
        .filter_map(|acct| {
            acct.members.iter().find(|m| m.actor == a.id).map(|m| json!({ "account": acct.handle, "role": m.role }))
        })
        .collect();
    Json(json!({
        "id": a.id,
        "handle": a.handle,
        "kind": a.kind,
        "accountable": a.is_accountable(),
        "human_root": a.human_principal(),
        "delegation": chain,
        "memberships": memberships,
    }))
    .into_response()
}

/// Verify a service-to-service shared secret from a request header — for webhook-style endpoints
/// (mirror inbound, CI callbacks) invoked by other *systems*, not signed-in users. If no secret is
/// configured the endpoint is **disabled** (refuse rather than run unauthenticated). Length-safe
/// constant-time-ish comparison.
#[allow(clippy::result_large_err)]
fn verify_service_secret(headers: &axum::http::HeaderMap, header: &str, expected: Option<&str>) -> Result<(), Response> {
    match expected {
        Some(s) if !s.is_empty() => {
            let presented = headers.get(header).and_then(|v| v.to_str().ok()).unwrap_or("");
            let ok = presented.len() == s.len()
                && presented.bytes().zip(s.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0;
            if ok {
                Ok(())
            } else {
                Err((StatusCode::UNAUTHORIZED, format!("bad or missing {header}")).into_response())
            }
        }
        _ => Err((StatusCode::FORBIDDEN, "endpoint disabled: no shared secret configured").into_response()),
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
/// The accountable actor for a mutating request. **Identity comes only from a valid session token**
/// (proven Ed25519 key possession) — never from a client-supplied actor id. No token ⇒ 401. This is
/// what makes "act as anyone" impossible: you are whoever you signed in as, nobody else. The
/// `_actor_id` argument (a body field some handlers still pass) is ignored, kept only so call sites
/// don't churn.
fn require_actor(app: &App, headers: &axum::http::HeaderMap, _actor_id: &str) -> Result<Actor, Response> {
    match authed_actor(app, headers) {
        Some(a) => match accountable(app, &a) {
            Ok(()) => Ok(a),
            Err(why) => Err((StatusCode::FORBIDDEN, why).into_response()),
        },
        None => Err((StatusCode::UNAUTHORIZED, "sign in required — no valid session token").into_response()),
    }
}

/// The **cryptographic** accountability gate (NEW-1166), run at every authoring boundary. A human is
/// its own root (must not be revoked); an agent's delegation must fully verify — every hop signed by
/// its parent, scope only narrowing, within the depth cap and TTL, and no principal in the chain
/// revoked. Revocation of any ancestor propagates here automatically. `Ok(())` means "may author".
fn accountable(app: &App, a: &Actor) -> Result<(), String> {
    if a.revoked {
        return Err("this actor has been revoked".into());
    }
    match a.kind {
        hull_core::ActorKind::Human => Ok(()),
        hull_core::ActorKind::Agent => {
            let deleg = a.delegation.as_ref().ok_or("agent carries no delegation — unaccountable")?;
            let is_revoked = |id: &str| app.store.actor(id).map(|x| x.revoked).unwrap_or(false);
            deleg.verify(&a.id, now(), &is_revoked).map(|_| ()).map_err(|e| format!("agent delegation does not verify: {e}"))
        }
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

/// The **content-addressed semantic summary** of a change (`GET …/change/:id/semantic`, B1): files
/// purely moved (proven by an unchanged blob id, not guessed by similarity) vs really added/deleted/
/// modified, and whether the whole change is a behavior-preserving `pure_move`.
async fn change_semantic(State(app): State<App>, Path((tenant, repo, id)): Path<(String, String, String)>) -> Json<Value> {
    Json(json!({ "semantic": app.repos.semantic_summary(&tenant, &repo, &id) }))
}

/// **keel-native content-addressed source fetch** (`GET …/tree/:tree/tar`): the change's keel tree,
/// addressed by its `tree_id`, materialized and streamed as a tar archive. This is how a CI or
/// reviewer runner obtains source — by content address, over keel, **not** `git clone`. (Hull's git
/// smart-HTTP endpoints exist only for interop/mirroring, never as the runner fetch path.) The
/// archive is verifiable: re-hashing the tree reproduces `tree`.
async fn tree_archive(State(app): State<App>, Path((tenant, repo, tree)): Path<(String, String, String)>) -> Response {
    let dir = std::env::temp_dir().join(format!("hull-tree-{}-{}", &tree[..tree.len().min(16)], std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    if !app.repos.checkout_tree(&tenant, &repo, &tree, &dir) {
        return (StatusCode::NOT_FOUND, "no such tree").into_response();
    }
    let result = tokio::task::spawn_blocking({
        let dir = dir.clone();
        move || {
            let mut buf = Vec::new();
            {
                let mut ar = tar::Builder::new(&mut buf);
                ar.mode(tar::HeaderMode::Deterministic);
                ar.append_dir_all(".", &dir)?;
                ar.finish()?;
            }
            Ok::<Vec<u8>, std::io::Error>(buf)
        }
    })
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    match result {
        Ok(Ok(bytes)) => (
            [(axum::http::header::CONTENT_TYPE, "application/x-tar"), (axum::http::header::CONTENT_DISPOSITION, "attachment; filename=\"tree.tar\"")],
            bytes,
        )
            .into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "could not archive tree").into_response(),
    }
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
    let facts = facts_with_independence(&app, &tenant, &repo, &id).await;
    let ledger = hull_core::reconcile::reconcile(&id, &info.intent, &lesson, &facts);
    // Overlay human resolutions onto the claims (a resolved needs-judgment claim stops being an open
    // question). Serialize the ledger, then attach `resolution` per claim by id.
    let resolutions = app.claims.for_change(&format!("{tenant}/{repo}"), &id);
    let mut val = serde_json::to_value(&ledger).unwrap_or(json!({}));
    if let Some(arr) = val.get_mut("claims").and_then(|c| c.as_array_mut()) {
        for claim in arr {
            if let Some(cid) = claim.get("id").and_then(Value::as_str) {
                if let Some(r) = resolutions.get(cid) {
                    let handle = app.store.actor(&r.by).map(|a| a.handle).unwrap_or_else(|| r.by.chars().take(8).collect());
                    claim["resolution"] = json!({ "judgment": r.judgment, "note": r.note, "by": handle, "ts": r.ts });
                }
            }
        }
    }
    Json(json!({ "ledger": val }))
}

/// Record a human judgment on a reconciliation claim (`POST …/change/:id/claims/:claim/resolve`) —
/// the action for a **needs-judgment** item. Body `{judgment: "verified"|"concern", note?}`.
/// Accountable-only; the resolution is attributed to the signed-in actor and overlaid on the ledger.
async fn resolve_claim(
    State(app): State<App>,
    Path((tenant, repo, id, claim)): Path<(String, String, String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let actor = match require_actor(&app, &headers, "") {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let judgment = match body.get("judgment").and_then(Value::as_str) {
        Some("verified") => "verified",
        Some("concern") => "concern",
        _ => return (StatusCode::BAD_REQUEST, "judgment must be 'verified' or 'concern'").into_response(),
    };
    let note = body.get("note").and_then(Value::as_str).unwrap_or("").to_string();
    app.claims.set(
        &format!("{tenant}/{repo}"),
        &id,
        &claim,
        claims::ClaimResolution { by: actor.id.clone(), judgment: judgment.into(), note, ts: now() },
    );
    Json(json!({ "resolved": claim, "judgment": judgment, "by": actor.handle })).into_response()
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
            // `X-Hull-CI-Version` lets a CI integration branch on the contract version (see CI-SPEC.md).
            let mut req = app.http.post(&cfg.url).header("X-Hull-CI-Version", ci::CONTRACT_VERSION).json(&payload);
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

/// The **independence-filtered** verification for a change: re-run its checks with every test it
/// added or modified neutralized (restored to the parent's version, or dropped if newly added), so a
/// change can't approve itself by writing or weakening its own passing test. `None` when the change
/// touched no tests (the whole suite is already independent) — reconciliation then uses the plain
/// verification. Runs off the async runtime; the composed tree is content-addressed and memoized.
async fn independent_verification(app: &App, tenant: &str, repo: &str, change: &str) -> Option<hull_core::reconcile::Independent> {
    use hull_core::reconcile::Independent;
    // Cheap gate first: no touched tests ⇒ nothing to neutralize ⇒ no separate run.
    if app.repos.changed_test_files(tenant, repo, change).is_empty() {
        return None;
    }
    let tree = app.repos.compose_independence_tree(tenant, repo, change)?;
    let (repos, registry, ci) = (app.repos.clone(), app.registry.clone(), app.ci.clone());
    let (t, r, tr) = (tenant.to_string(), repo.to_string(), tree);
    let outcome = tokio::task::spawn_blocking(move || ci::run_check_tree(&repos, &registry, &ci, &t, &r, &tr)).await.ok()?;
    Some(match outcome.status {
        hull_plugin::CiStatus::Green => Independent::Green,
        hull_plugin::CiStatus::Red => Independent::Red,
        hull_plugin::CiStatus::Errored => Independent::None,
    })
}

/// A change's [`facts`](RepoHost::facts) enriched with the independence-filtered verification — the
/// version reconciliation and the reviewer should use everywhere a verdict is derived.
async fn facts_with_independence(app: &App, tenant: &str, repo: &str, change: &str) -> hull_core::reconcile::ChangeFacts {
    let mut f = app.repos.facts(tenant, repo, change);
    f.independent_verification = independent_verification(app, tenant, repo, change).await;
    f
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

/// Git push endpoint, wrapped so that **every successful push runs CI** on the new HEAD change —
/// independent of autonomy tier (CI is a mechanical check, not an autonomous action). Fire-and-forget
/// (memoized by tree, so an unchanged tree is a no-op); dispatched to the configured CI or the local
/// runner.
async fn receive_pack_handler(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let resp = repos::receive_pack(State(app.clone()), Path((tenant.clone(), repo.clone())), headers, body).await;
    if resp.status().is_success() {
        if let Some(change) = app.repos.head_change(&tenant, &repo) {
            let (app2, t, r) = (app.clone(), tenant.clone(), repo.clone());
            tokio::spawn(async move {
                let _ = resolve_check(&app2, &t, &r, &change, false).await;
            });
        }
    }
    resp
}

/// The id of the account that owns `tenant/repo` (for the account-level policy fallback).
fn repo_account_id(app: &App, tenant: &str, repo: &str) -> Option<String> {
    let name = format!("{tenant}/{repo}");
    app.store
        .repos()
        .into_iter()
        .find(|r| r.name == name || r.name == repo)
        .map(|r| r.owner)
        .or_else(|| app.store.accounts().into_iter().find(|a| a.handle == tenant).map(|a| a.id))
}

fn tier_from_str(s: &str) -> Option<hull_core::AutonomyTier> {
    match s.to_lowercase().as_str() {
        "t0" => Some(hull_core::AutonomyTier::T0),
        "t1" => Some(hull_core::AutonomyTier::T1),
        "t2" => Some(hull_core::AutonomyTier::T2),
        "t3" => Some(hull_core::AutonomyTier::T3),
        _ => None,
    }
}

/// The effective autonomy policy for a repo (`GET …/autonomy`) — the resolved tier, where it comes
/// from, and the protected paths that always require a human.
async fn get_repo_autonomy(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>) -> Json<Value> {
    let acct = repo_account_id(&app, &tenant, &repo);
    let e = app.autonomy.effective(&tenant, &repo, acct.as_deref());
    Json(json!({
        "tier": e.tier, "source": e.source, "protected_paths": e.protected_paths,
        "repo_override": app.autonomy.get_repo(&tenant, &repo).map(|p| p.tier),
        "account_tier": acct.as_deref().and_then(|a| app.autonomy.get_account(a)).map(|p| p.tier),
    }))
}

/// Set the repo's autonomy tier (`PUT …/autonomy` `{tier, protected_paths?}`) — owner/admin only.
async fn set_repo_autonomy(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let actor = match require_actor(&app, &headers, "") {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if !is_repo_admin(&app, &tenant, &repo, &actor.id) {
        return (StatusCode::FORBIDDEN, "only a repo owner/admin can set autonomy").into_response();
    }
    let Some(tier) = body.get("tier").and_then(Value::as_str).and_then(tier_from_str) else {
        return (StatusCode::BAD_REQUEST, "tier must be t0 | t1 | t2 | t3").into_response();
    };
    let protected_paths = body
        .get("protected_paths")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    app.autonomy.set_repo(&tenant, &repo, hull_core::AutonomyPolicy { tier, protected_paths });
    Json(json!({ "tier": tier })).into_response()
}

/// Set an account's autonomy tier (`PUT /api/accounts/:id/autonomy`) — account owner/admin only.
async fn set_account_autonomy(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let actor = match require_actor(&app, &headers, "") {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let is_admin = app
        .store
        .accounts()
        .into_iter()
        .find(|a| a.id == id)
        .map(|a| a.members.iter().any(|m| m.actor == actor.id && matches!(m.role, Role::Owner | Role::Admin)))
        .unwrap_or(false);
    if !is_admin {
        return (StatusCode::FORBIDDEN, "only an account owner/admin can set autonomy").into_response();
    }
    let Some(tier) = body.get("tier").and_then(Value::as_str).and_then(tier_from_str) else {
        return (StatusCode::BAD_REQUEST, "tier must be t0 | t1 | t2 | t3").into_response();
    };
    let protected_paths = body
        .get("protected_paths")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    app.autonomy.set_account(&id, hull_core::AutonomyPolicy { tier, protected_paths });
    Json(json!({ "tier": tier })).into_response()
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
/// Close a PR without merging, or reopen a closed one (`POST …/prs/:number/close` with
/// `{"reopen": bool}`). A merged PR can't be closed/reopened. Only the author or an org owner/admin.
async fn close_pr(
    State(app): State<App>,
    Path((tenant, repo, number)): Path<(String, String, u64)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let key = format!("{tenant}/{repo}");
    let actor = match require_actor(&app, &headers, "") {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let Some(mut pr) = app.store.prs(&key).into_iter().find(|p| p.number == number) else {
        return (StatusCode::NOT_FOUND, "no such PR").into_response();
    };
    if pr.state == PrState::Merged {
        return (StatusCode::CONFLICT, "a merged PR can't be closed or reopened").into_response();
    }
    if pr.author != actor.id && !is_repo_admin(&app, &tenant, &repo, &actor.id) {
        return (StatusCode::FORBIDDEN, "only the PR author or a repo owner/admin can close it").into_response();
    }
    let reopen = body.get("reopen").and_then(Value::as_bool).unwrap_or(false);
    pr.state = if reopen { PrState::Open } else { PrState::Closed };
    app.store.replace_pr(pr.clone());
    Json(json!({ "pr": pr })).into_response()
}

/// Request a review from an actor (`POST …/prs/:number/reviewers` with `{reviewer}`). Adds them to
/// the PR's reviewers and notifies them (`review_requested`). Any signed-in actor may request.
async fn request_reviewer(
    State(app): State<App>,
    Path((tenant, repo, number)): Path<(String, String, u64)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let key = format!("{tenant}/{repo}");
    let requester = match require_actor(&app, &headers, "") {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let reviewer = body.get("reviewer").and_then(Value::as_str).unwrap_or("").to_string();
    if app.store.actor(&reviewer).is_none() {
        return (StatusCode::UNPROCESSABLE_ENTITY, "reviewer must be a registered actor").into_response();
    }
    let Some(mut pr) = app.store.prs(&key).into_iter().find(|p| p.number == number) else {
        return (StatusCode::NOT_FOUND, "no such PR").into_response();
    };
    if !pr.reviewers.contains(&reviewer) {
        pr.reviewers.push(reviewer.clone());
        app.store.replace_pr(pr.clone());
    }
    app.registry.notify(&NotifyEvent {
        kind: "review_requested".into(),
        to: vec![reviewer.clone()],
        summary: format!("{} requested your review on PR !{number}", requester.handle),
        change: pr.changes.first().cloned(),
    });
    Json(json!({ "pr": pr })).into_response()
}

/// review by someone **other than the author** (independent — no self-merge). Records who merged.
async fn merge_pr(
    State(app): State<App>,
    Path((tenant, repo, number)): Path<(String, String, u64)>,
    headers: axum::http::HeaderMap,
    Json(_body): Json<Value>,
) -> Response {
    let actor = match require_actor(&app, &headers, "") {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    match perform_merge(&app, &tenant, &repo, number, &actor).await {
        Ok((pr, closed)) => Json(json!({ "pr": pr, "closed_issues": closed })).into_response(),
        Err((code, msg)) => (code, msg).into_response(),
    }
}

/// The merge gate + merge, shared by the endpoint and the T3 auto-merge flow. Enforces: green
/// keel-verify, an independent approval (human always counts; an agent's counts per the autonomy
/// tier and never for a protected path, D11). Returns the merged PR + the issues it auto-closed.
#[allow(clippy::result_large_err)]
async fn perform_merge(
    app: &App,
    tenant: &str,
    repo: &str,
    number: u64,
    actor: &hull_core::Actor,
) -> Result<(PullRequest, Vec<u64>), (StatusCode, String)> {
    let key = format!("{tenant}/{repo}");
    let Some(mut pr) = app.store.prs(&key).into_iter().find(|p| p.number == number) else {
        return Err((StatusCode::NOT_FOUND, "no such PR".into()));
    };
    if pr.state == PrState::Merged {
        return Err((StatusCode::CONFLICT, "already merged".into()));
    }
    // green keel verification of the proposed change
    let green = pr
        .changes
        .first()
        .and_then(|c| app.repos.verification(tenant, repo, c))
        .map(|v| v == "green")
        .unwrap_or(false);
    if !green {
        return Err((StatusCode::CONFLICT, "cannot merge: change is not keel-verify green".into()));
    }
    // Independent approving reviews (approver != PR author), split by actor kind.
    let approvals: Vec<ActorId> = app
        .store
        .reviews(&key)
        .into_iter()
        .filter(|r| r.target == format!("pr:{number}") && r.verdict == Verdict::Approve && r.reviewer != pr.author)
        .map(|r| r.reviewer)
        .collect();
    let human_approval = approvals.iter().any(|a| app.store.actor(a).map(|x| x.kind == hull_core::ActorKind::Human).unwrap_or(false));
    let agent_approval = approvals.iter().any(|a| app.store.actor(a).map(|x| x.kind == hull_core::ActorKind::Agent).unwrap_or(false));

    // Autonomy policy: when may an AGENT's approve stand in for a human's?
    let acct = repo_account_id(app, tenant, repo);
    let eff = app.autonomy.effective(tenant, repo, acct.as_deref());
    let change = pr.changes.first().cloned().unwrap_or_default();
    let files: Vec<String> = app.repos.change_info(tenant, repo, &change).map(|i| i.files.into_iter().map(|f| f.path).collect()).unwrap_or_default();
    let protected = autonomy::touches_protected(&files, &eff.protected_paths);
    let contradicted = {
        let lesson = app.store.session_record(&key, &change).map(|s| s.lesson).unwrap_or_default();
        let intent = app.repos.change_info(tenant, repo, &change).map(|i| i.intent).unwrap_or_default();
        let facts = facts_with_independence(app, tenant, repo, &change).await;
        hull_core::reconcile::reconcile(&change, &intent, &lesson, &facts).contradicted() > 0
    };
    let low_risk = !protected && !contradicted; // green is already required above
    let agent_approve_counts = match eff.tier {
        hull_core::AutonomyTier::T0 | hull_core::AutonomyTier::T1 => false,
        hull_core::AutonomyTier::T2 => low_risk,
        hull_core::AutonomyTier::T3 => !protected, // protected paths ALWAYS need a human (D11)
    };
    let approved = human_approval || (agent_approval && agent_approve_counts);
    if !approved {
        let why = if agent_approval && protected {
            "an agent approved, but this change touches a protected path — a human approval is required (D11)"
        } else if agent_approval {
            "an agent approved, but the repo's autonomy tier doesn't let an agent approve this — needs a human approval"
        } else {
            "needs an approving review from someone other than the author"
        };
        return Err((StatusCode::CONFLICT, format!("cannot merge: {why}")));
    }
    pr.state = PrState::Merged;
    pr.merged_by = Some(actor.id.clone());
    app.store.replace_pr(pr.clone());
    app.hub.publish(
        tenant,
        ActivityEvent::Push { actor: actor.handle.clone(), repo: repo.to_string(), change: pr.changes.first().cloned().unwrap_or_default(), ts: now() },
    );
    // Outbound mirror on change-land — guarded by loop prevention + idempotency.
    if let Some(change) = pr.changes.first() {
        mirror_out(app, tenant, repo, change);
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
    Ok((pr, closed))
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
    let inbound = app.mirror.inbound_for(&key);
    Json(json!({
        "target": app.registry.mirror_target(&key),
        "outbound": app.mirror.outbound_for(&key),
        // Imported changes with their accountability mapping (NEW-1176).
        "inbound": inbound.iter().map(|i| json!({
            "change": i.change, "git_author": i.git_author, "github_login": i.github_login,
            "attributed_actor": i.attributed_actor, "accountable": i.accountable(), "ts": i.ts,
        })).collect::<Vec<_>>(),
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
    // Webhook auth: a forge calls this, not a signed-in user — gate on the shared mirror secret
    // (`HULL_MIRROR_SECRET`), like the CI callback. No secret configured ⇒ endpoint disabled.
    let secret = std::env::var("HULL_MIRROR_SECRET").ok();
    if let Err(resp) = verify_service_secret(&headers, "X-Hull-Mirror-Secret", secret.as_deref()) {
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

    // Accountability mapping across the mirror (NEW-1176). A git commit carries a git identity, not a
    // hull key. Resolve the committer's forge login to a linked hull actor when we can; otherwise the
    // import is an **external, un-natively-signed** author — recorded honestly, never presented as
    // accountable hull authorship (an imported change stays out of the auto-merge path just like any
    // unverified one).
    let git_author = body.get("author").and_then(Value::as_str).unwrap_or("mirror").to_string();
    let github_login = body.get("github_login").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let attributed = if github_login.is_empty() { None } else { app.mirror.resolve_github(&github_login) };
    let attributed_handle = attributed.as_ref().and_then(|id| app.store.actor(id).map(|a| a.handle));
    let key = format!("{tenant}/{repo}");
    app.mirror.record_inbound(mirror::Inbound {
        repo: key.clone(),
        change: change.clone(),
        external_id,
        git_author: git_author.clone(),
        github_login: github_login.clone(),
        attributed_actor: attributed.clone(),
        ts: now(),
    });
    // Surface the mapped (accountable) actor in the activity stream when known; else the git identity.
    let actor_label = attributed_handle.clone().unwrap_or_else(|| format!("{git_author} (external)"));
    app.hub.publish(
        &tenant,
        ActivityEvent::Push { actor: actor_label, repo: repo.clone(), change: change.clone(), ts: now() },
    );
    (StatusCode::CREATED, Json(json!({
        "processed": true, "duplicate": false, "change": change, "origin": "github",
        "attributed_actor": attributed, "attributed_handle": attributed_handle, "accountable": attributed.is_some(),
    }))).into_response()
}

/// List reviews for a hosted repo (`GET /api/repos/:tenant/:repo/reviews`); the client filters by
/// target (e.g. `pr:1`).
async fn reviews(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>) -> Json<Value> {
    Json(json!({ "reviews": app.store.reviews(&format!("{tenant}/{repo}")) }))
}

/// The content-addressed review **audit artifact** (`GET …/artifacts/:id`) — the immutable record of
/// why a review reached its verdict (inputs, models, ledger, findings). The `:id` is a BLAKE3
/// content address, so the answer to "why did the reviewer pass this?" can't be altered after.
async fn get_artifact(State(app): State<App>, Path((tenant, repo, id)): Path<(String, String, String)>) -> Response {
    // Scope to the requesting repo — the store is content-addressed and global, so we must check the
    // artifact belongs here rather than serving any id cross-tenant. (Fix from the dogfood review of
    // PR !2.)
    match app.artifacts.get(&id) {
        Some(a) if a.get("repo").and_then(Value::as_str) == Some(&format!("{tenant}/{repo}")) => {
            Json(json!({ "artifact_id": id, "artifact": a })).into_response()
        }
        _ => (StatusCode::NOT_FOUND, "no such artifact").into_response(),
    }
}

/// Discussion comments for a repo (`GET /api/repos/:tenant/:repo/comments`); the client filters by
/// `target` (e.g. `pr:1`). The conversation layer over the structured review.
async fn comments_list(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>) -> Json<Value> {
    Json(json!({ "comments": app.store.comments(&format!("{tenant}/{repo}")) }))
}

/// Post a comment (`POST /api/repos/:tenant/:repo/comments`) — `{target, body}`. Authored by the
/// signed-in actor (human or agent), so a review thread is one accountable conversation. Notifies the
/// PR's author and reviewers.
async fn create_comment(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let key = format!("{tenant}/{repo}");
    let author = match require_actor(&app, &headers, "") {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let target = body.get("target").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let text = body.get("body").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if target.is_empty() || text.is_empty() {
        return (StatusCode::BAD_REQUEST, "target and body are required").into_response();
    }
    let count = app.store.comments(&key).len();
    let comment = Comment {
        id: format!("cm_{}_{}", key.replace('/', "_"), count + 1),
        repo: key.clone(),
        target: target.clone(),
        author: author.id.clone(),
        body: text,
        created_unix: now(),
    };
    app.store.put_comment(comment.clone());
    // Notify the people watching the target (not the commenter): a PR's author + reviewers, or an
    // issue's author + assignees.
    let (mut to, summary, change): (Vec<String>, String, Option<String>) =
        if let Some(num) = target.strip_prefix("pr:").and_then(|s| s.parse::<u64>().ok()) {
            match app.store.prs(&key).into_iter().find(|p| p.number == num) {
                Some(pr) => {
                    let mut to = pr.reviewers.clone();
                    to.push(pr.author.clone());
                    (to, format!("{} commented on PR !{num}", author.handle), pr.changes.first().cloned())
                }
                None => (vec![], String::new(), None),
            }
        } else if let Some(num) = target.strip_prefix("issue:").and_then(|s| s.parse::<u64>().ok()) {
            match app.store.issues(&key).into_iter().find(|i| i.number == num) {
                Some(issue) => {
                    let mut to = issue.assignees.clone();
                    to.push(issue.author.clone());
                    (to, format!("{} commented on issue #{num}", author.handle), None)
                }
                None => (vec![], String::new(), None),
            }
        } else {
            (vec![], String::new(), None)
        };
    to.retain(|a| a != &author.id);
    to.sort();
    to.dedup();
    if !to.is_empty() {
        app.registry.notify(&NotifyEvent { kind: "comment_posted".into(), to, summary, change });
    }
    (StatusCode::CREATED, Json(json!({ "comment": comment }))).into_response()
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
        artifact_id: None,
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
    Json(_body): Json<Value>,
) -> Response {
    // Any signed-in accountable actor may *ask* for an agent review; the reviewer is never supplied
    // by the client (no impersonation) — the server picks an agent independent of the PR author.
    if let Err(resp) = require_actor(&app, &headers, "") {
        return resp;
    }
    let key = format!("{tenant}/{repo}");
    let Some(pr) = app.store.prs(&key).into_iter().find(|p| p.number == number) else {
        return (StatusCode::NOT_FOUND, "no such PR").into_response();
    };
    let Some(agent) = independent_agent_reviewer(&app, &pr.author) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "no independent agent reviewer is registered").into_response();
    };
    match perform_auto_review(&app, &tenant, &repo, number, &agent, 0).await {
        Ok(review) => (StatusCode::CREATED, Json(json!({ "review": review }))).into_response(),
        Err((code, msg)) => (code, msg).into_response(),
    }
}

/// The reviewer runtime: run a PR's checks, reconcile its change, and post an accountable agent
/// review. Shared by the explicit endpoint and the on-open agent flow. Enforces the gate (agent,
/// independent of author) itself, so every caller is safe.
/// Max review→fix→re-review cycles at T3, so the autonomous loop always terminates.
const MAX_FIX_DEPTH: u8 = 2;

async fn perform_auto_review(
    app: &App,
    tenant: &str,
    repo: &str,
    number: u64,
    reviewer: &hull_core::Actor,
    depth: u8,
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
    // 2. Produce the review package through the Reviewer capability (Epic D). The OSS default
    //    reconciles the narrative against the facts (Epic C); a hosted plugin swaps in the sandbox +
    //    model-backed AI reviewer. Either way the output is a constrained-schema verdict/findings.
    let Some(info) = app.repos.change_info(tenant, repo, &change) else {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "cannot resolve change".into()));
    };
    let session = app.store.session_record(&key, &change);
    let lesson = session.as_ref().map(|s| s.lesson.clone()).unwrap_or_default();
    let author_model = session.as_ref().map(|s| s.model.clone()).unwrap_or_default();
    let facts = facts_with_independence(app, tenant, repo, &change).await;
    let tree = app.repos.change_tree(tenant, repo, &change).unwrap_or_default();
    let source_url = format!("{}/api/repos/{tenant}/{repo}/tree/{tree}/tar", app.public_url.trim_end_matches('/'));
    // Capture the reviewer's INPUTS for the audit artifact before `facts` moves into the request.
    let artifact_inputs = json!({
        "intent": info.intent, "author": info.author, "author_model": author_model,
        "files": facts.files, "ops": facts.ops, "verification": facts.verification, "secrets": facts.secrets,
    });
    // B6 — pure-move fast-track: a byte-identical relocation has no behavioral logic to review, so
    // approve it mechanically and skip the (expensive) model review. CI-green is still required by
    // the merge gate — a move can break the build via path changes, which CI catches. Protected
    // paths (auth/, migrations/, .hull/) are never fast-tracked; they always get a full review.
    let semantic = app.repos.semantic_summary(tenant, repo, &change);
    let touched: Vec<String> = semantic.moves.iter().flat_map(|m| [m.from.clone(), m.to.clone()]).chain(semantic.added.iter().cloned()).chain(semantic.deleted.iter().cloned()).chain(semantic.modified.iter().cloned()).collect();
    let mechanical = semantic.pure_move && {
        let acct = repo_account_id(app, tenant, repo);
        !autonomy::touches_protected(&touched, &app.autonomy.effective(tenant, repo, acct.as_deref()).protected_paths)
    };
    let (verdict, findings, ledger, base_summary, from_cache) = if mechanical {
        let ledger = hull_core::reconcile::reconcile(&change, &info.intent, &lesson, &facts);
        let n = semantic.moves.len();
        (Verdict::Approve, Vec::new(), Some(ledger), format!("pure move — {n} file{} relocated with byte-identical content (verified by content address); no behavioral review needed", if n == 1 { "" } else { "s" }), false)
    } else {
    let review_req = hull_plugin::ReviewRequest {
        repo: key.clone(),
        change: change.clone(),
        intent: info.intent.clone(),
        lesson,
        author: info.author.clone(),
        author_model,
        source_url,
        facts,
    };
    // D9 — incremental re-review: reuse the cached verdict when nothing that feeds it changed. The
    // key is tree **+ verification** (a review's inputs are the diff AND the green/red signal), so a
    // changed diff OR a flipped verification re-reviews; an identical (tree, verification) is cached.
    // (Fix from the dogfood review of PR !1: keying on tree alone would serve a stale verdict after a
    // red→green flip on the same tree.)
    let cache_key = format!("{tree}:{}", app.repos.verification(tenant, repo, &change).unwrap_or_default());
    match app.review_cache.get(&cache_key) {
        Some(cr) => {
            let v = match cr.verdict.as_str() {
                "approve" => Verdict::Approve,
                "request_changes" => Verdict::RequestChanges,
                _ => Verdict::Comment,
            };
            let f: Vec<ReviewFinding> = cr.findings.into_iter().map(|f| ReviewFinding { path: f.path, line: f.line, severity: f.severity, note: f.note }).collect();
            (v, f, cr.ledger, cr.summary, true)
        }
        None => {
            // The reviewer may make a blocking model call (the OpenRouter reviewer); keep the runtime free.
            let registry = app.registry.clone();
            let pkg = tokio::task::spawn_blocking(move || registry.review(&review_req))
                .await
                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "reviewer panicked".to_string()))?;
            let v = match pkg.verdict {
                hull_plugin::ReviewVerdict::Approve => Verdict::Approve,
                hull_plugin::ReviewVerdict::RequestChanges => Verdict::RequestChanges,
                hull_plugin::ReviewVerdict::Comment => Verdict::Comment,
            };
            let f: Vec<ReviewFinding> = pkg.findings.into_iter().map(|f| ReviewFinding { path: f.path, line: f.line, severity: f.severity, note: f.note }).collect();
            let verdict_str = match v {
                Verdict::Approve => "approve",
                Verdict::RequestChanges => "request_changes",
                _ => "comment",
            };
            app.review_cache.put(&cache_key, reviewcache::CachedReview {
                verdict: verdict_str.into(),
                summary: pkg.summary.clone(),
                findings: f.iter().map(|x| reviewcache::CachedFinding { path: x.path.clone(), line: x.line, severity: x.severity.clone(), note: x.note.clone() }).collect(),
                ledger: pkg.ledger.clone(),
            });
            (v, f, pkg.ledger, pkg.summary, false)
        }
        }
    };
    let count = app.store.reviews(&key).len();
    let rid = format!("rv_{}_{}", key.replace('/', "_"), count + 1);
    // D8 — content-addressed audit artifact: the full record of why this verdict was reached.
    let artifact = json!({
        "review_id": rid,
        "repo": key, "change": change, "tree_id": tree,
        "reviewer": reviewer.id, "reviewer_handle": reviewer.handle,
        "verdict": format!("{verdict:?}"),
        "summary": base_summary.clone(),
        "checks": check_label,
        "cached": from_cache,
        "findings": findings.clone(),
        "ledger": ledger.clone(),
        "inputs": artifact_inputs,
        "models": {
            "screen": app.registry.config("HULL_REVIEW_MODEL"),
            "deep": app.registry.config("HULL_REVIEW_MODEL_DEEP"),
        },
        "created_unix": now(),
    });
    let artifact_id = app.artifacts.put(artifact);
    let review = Review {
        id: rid,
        repo: key.clone(),
        target: format!("pr:{number}"),
        reviewer: reviewer.id.clone(),
        verdict,
        summary: format!("{base_summary} · checks {check_label}{}", if from_cache { " · reused (unchanged tree)" } else { "" }),
        findings,
        ledger,
        artifact_id: Some(artifact_id),
        created_unix: now(),
    };
    app.store.put_review(review.clone());
    app.registry.notify(&NotifyEvent {
        kind: "review_posted".into(),
        to: vec![pr.author.clone()],
        summary: format!("{} auto-reviewed PR !{number}: {:?}", reviewer.handle, review.verdict),
        change: Some(change.clone()),
    });

    // Auto-triage (T2+): a review that requests changes turns its blocker findings into a triaged
    // issue — automatic issue triage out of reviews. Gated by the repo's autonomy tier.
    let acct = repo_account_id(app, tenant, repo);
    let tier = app.autonomy.effective(tenant, repo, acct.as_deref()).tier;
    if tier >= hull_core::AutonomyTier::T2 && review.verdict == Verdict::RequestChanges {
        let blockers: Vec<&ReviewFinding> = review.findings.iter().filter(|f| f.severity == "blocker").collect();
        if !blockers.is_empty() {
            // Don't re-triage the same PR: skip if an open from-review issue already links it.
            let already = app
                .store
                .issues(&key)
                .into_iter()
                .any(|i| i.labels.iter().any(|l| l == "from-review") && i.linked_prs.contains(&pr.id) && matches!(i.status, IssueStatus::Open));
            if !already {
                let inum = app.store.issues(&key).iter().map(|i| i.number).max().unwrap_or(0) + 1;
                let body = blockers.iter().map(|f| format!("- {} ({})", f.note, f.path)).collect::<Vec<_>>().join("\n");
                let issue = Issue {
                    id: format!("iss_{}_{inum}", key.replace('/', "_")),
                    repo: key.clone(),
                    number: inum,
                    title: format!("Review flagged {} blocker(s) on PR !{number}", blockers.len()),
                    body: format!("Auto-triaged from {}'s review of PR !{number}:\n\n{body}", reviewer.handle),
                    author: reviewer.id.clone(),
                    assignees: vec![pr.author.clone()],
                    labels: vec!["from-review".into()],
                    projects: vec![],
                    status: IssueStatus::Open,
                    code_refs: vec![],
                    referenced_actors: vec![],
                    linked_prs: vec![pr.id.clone()],
                    resolved_by: None,
                    created_unix: now(),
                };
                app.store.put_issue(issue);
                app.registry.notify(&NotifyEvent {
                    kind: "issue_triaged".into(),
                    to: vec![pr.author.clone()],
                    summary: format!("auto-triaged issue #{inum} from the review of PR !{number}"),
                    change: Some(change.clone()),
                });
                app.hub.publish(
                    tenant,
                    ActivityEvent::Issue { repo: repo.to_string(), number: inum, action: "opened".into(), actor: reviewer.handle.clone(), ts: now() },
                );
            }
        }
    }

    // AI auto-fix (T3): the agent handles its own findings — it applies a fix for every non-info
    // finding as a new keel change, then RE-REVIEWS the fixed change. Bounded by MAX_FIX_DEPTH so the
    // loop terminates; if the re-review is clean the auto-merge below fires. A fix that doesn't apply
    // (or the fixer declines) leaves the PR flagged for a human.
    if tier >= hull_core::AutonomyTier::T3
        && depth < MAX_FIX_DEPTH
        && review.findings.iter().any(|f| f.severity != "info" && !f.path.is_empty())
    {
        let mut applied = false;
        for f in review.findings.iter().filter(|f| f.severity != "info" && !f.path.is_empty()) {
            if let Some(res) = post_fix(app, tenant, repo, number, reviewer, &f.path, &f.note, &f.severity).await {
                applied |= res.ok;
            }
        }
        if applied {
            // The PR now points at the fixed change — re-review it (one step deeper).
            return Box::pin(perform_auto_review(app, tenant, repo, number, reviewer, depth + 1)).await;
        }
    }

    // AI merge (T3): if the agent approved and the change is mergeable (green, non-protected), the
    // agent merges it autonomously — the top of the autonomy ladder. The merge gate still runs, so a
    // protected path or a non-green change is refused even here (D11).
    if tier >= hull_core::AutonomyTier::T3 && review.verdict == Verdict::Approve {
        match perform_merge(app, tenant, repo, number, reviewer).await {
            Ok((_, closed)) => {
                app.registry.notify(&NotifyEvent {
                    kind: "auto_merged".into(),
                    to: vec![pr.author.clone()],
                    summary: format!("{} auto-merged PR !{number} (autonomy T3){}", reviewer.handle, if closed.is_empty() { String::new() } else { format!(", closed #{:?}", closed) }),
                    change: Some(change.clone()),
                });
            }
            Err((_, why)) => eprintln!("hull: T3 auto-merge of PR !{number} declined: {why}"),
        }
    }
    Ok(review)
}

/// Ask the AI fixer to propose a fix for a finding, and post it as a comment on the PR (authored by
/// `agent`). Returns the fixer's result, or `None` if no fixer is configured. Fetches the change's
/// keel-native source for the fixer to patch.
#[allow(clippy::too_many_arguments)]
async fn post_fix(app: &App, tenant: &str, repo: &str, number: u64, agent: &hull_core::Actor, path: &str, note: &str, severity: &str) -> Option<hull_plugin::FixResult> {
    let key = format!("{tenant}/{repo}");
    let pr = app.store.prs(&key).into_iter().find(|p| p.number == number)?;
    let change = pr.changes.first().cloned()?;
    let tree = app.repos.change_tree(tenant, repo, &change).unwrap_or_default();
    let source_url = format!("{}/api/repos/{tenant}/{repo}/tree/{tree}/tar", app.public_url.trim_end_matches('/'));
    let req = hull_plugin::FixRequest {
        repo: key.clone(),
        change,
        source_url,
        path: path.to_string(),
        note: note.to_string(),
        severity: severity.to_string(),
    };
    let change = req.change.clone();
    let registry = app.registry.clone();
    let res = tokio::task::spawn_blocking(move || registry.fix(&req)).await.ok()??;
    if res.ok && !res.edits.is_empty() {
        let intent = format!("fix: {}", res.explanation);
        let edits: Vec<(String, String, String)> = res.edits.iter().map(|e| (e.path.clone(), e.search.clone(), e.replace.clone())).collect();
        // Materialize the fix as a NEW keel change parented on the PR's change.
        match app.repos.apply_fix(tenant, repo, &change, &edits, &intent, &agent.handle, now()) {
            Some(fix_change) => {
                // Point the PR at the fixed change and run its checks.
                if let Some(mut pr2) = app.store.prs(&key).into_iter().find(|p| p.number == number) {
                    pr2.changes = vec![fix_change.clone()];
                    app.store.replace_pr(pr2);
                }
                let _ = resolve_check(app, tenant, repo, &fix_change, false).await;
                let diff = res.edits.iter().map(|e| format!("--- {}\n- {}\n+ {}", e.path, e.search.lines().next().unwrap_or(""), e.replace.lines().next().unwrap_or(""))).collect::<Vec<_>>().join("\n");
                let count = app.store.comments(&key).len();
                app.store.put_comment(Comment {
                    id: format!("cm_{}_{}", key.replace('/', "_"), count + 1),
                    repo: key.clone(),
                    target: format!("pr:{number}"),
                    author: agent.id.clone(),
                    body: format!("🔧 **Applied fix** as change ⬡{} — {}\n\n```diff\n{diff}\n```", &fix_change[..12], res.explanation),
                    created_unix: now(),
                });
                app.registry.notify(&NotifyEvent {
                    kind: "fix_applied".into(),
                    to: vec![pr.author.clone()],
                    summary: format!("{} applied a fix to PR !{number} (new change {})", agent.handle, &fix_change[..12]),
                    change: Some(fix_change),
                });
            }
            None => {
                // The fix didn't apply cleanly — record it as a proposal instead of a silent drop.
                let count = app.store.comments(&key).len();
                app.store.put_comment(Comment {
                    id: format!("cm_{}_{}", key.replace('/', "_"), count + 1),
                    repo: key.clone(),
                    target: format!("pr:{number}"),
                    author: agent.id.clone(),
                    body: format!("🔧 **Proposed fix** for `{path}` (couldn't apply cleanly — the code moved): {}", res.explanation),
                    created_unix: now(),
                });
            }
        }
    }
    Some(res)
}

/// Request an AI fix for a finding (`POST …/prs/:number/fix` with `{path, note, severity}`). Any
/// signed-in actor may ask; the fix is proposed by an agent as a PR comment. 501 if no fixer is
/// configured (OSS core has none — it's a hosted capability).
async fn fix_finding(
    State(app): State<App>,
    Path((tenant, repo, number)): Path<(String, String, u64)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(resp) = require_actor(&app, &headers, "") {
        return resp;
    }
    let key = format!("{tenant}/{repo}");
    let Some(pr) = app.store.prs(&key).into_iter().find(|p| p.number == number) else {
        return (StatusCode::NOT_FOUND, "no such PR").into_response();
    };
    let Some(agent) = independent_agent_reviewer(&app, &pr.author) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "no agent available to fix").into_response();
    };
    let path = body.get("path").and_then(Value::as_str).unwrap_or("").to_string();
    let note = body.get("note").and_then(Value::as_str).unwrap_or("").to_string();
    let severity = body.get("severity").and_then(Value::as_str).unwrap_or("warn").to_string();
    match post_fix(&app, &tenant, &repo, number, &agent, &path, &note, &severity).await {
        Some(res) if res.ok => (StatusCode::CREATED, Json(json!({ "fix": res }))).into_response(),
        Some(res) => (StatusCode::UNPROCESSABLE_ENTITY, if res.explanation.is_empty() { "the fixer could not produce a fix".into() } else { res.explanation }).into_response(),
        None => (StatusCode::NOT_IMPLEMENTED, "no AI fixer configured on this instance").into_response(),
    }
}

/// Pick an agent actor that may independently review a PR by `author` — the reviewer for the on-open
/// agent flow. `None` if no **accountable** agent other than the author is registered: an agent whose
/// delegation doesn't cryptographically verify (NEW-1166) must not author, so it's never selected.
fn independent_agent_reviewer(app: &App, author: &str) -> Option<hull_core::Actor> {
    app.store
        .actors()
        .into_iter()
        .find(|a| a.kind == hull_core::ActorKind::Agent && a.id != author && accountable(app, a).is_ok())
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
    // Agent flow (M6): when a PR opens, an independent agent reviewer auto-reviews it — but only if
    // the repo's autonomy tier permits autonomous action (T1+). At T0 (observe-only), nothing fires.
    let acct = repo_account_id(&app, &tenant, &repo);
    let tier = app.autonomy.effective(&tenant, &repo, acct.as_deref()).tier;
    if tier >= hull_core::AutonomyTier::T1 {
        if let Some(agent) = independent_agent_reviewer(&app, &pr.author) {
            let (app2, t2, r2, n2) = (app.clone(), tenant.clone(), repo.clone(), number);
            tokio::spawn(async move {
                let _ = perform_auto_review(&app2, &t2, &r2, n2, &agent, 0).await;
            });
        }
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
        "assign" | "unassign" => {
            let who = body.get("assignee").and_then(Value::as_str).unwrap_or("").to_string();
            if who.is_empty() || app.store.actor(&who).is_none() {
                return (StatusCode::UNPROCESSABLE_ENTITY, "assignee must be a registered actor").into_response();
            }
            issue.assignees.retain(|a| a != &who);
            if action == "assign" {
                issue.assignees.push(who);
            }
        }
        "label" | "unlabel" => {
            let label = body.get("label").and_then(Value::as_str).unwrap_or("").trim().to_string();
            if label.is_empty() {
                return (StatusCode::BAD_REQUEST, "label is required").into_response();
            }
            issue.labels.retain(|l| l != &label);
            if action == "label" {
                issue.labels.push(label);
            }
        }
        _ => return (StatusCode::BAD_REQUEST, "action must be close | reopen | assign | unassign | label | unlabel").into_response(),
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
    // A human root + an agent it delegated — both real Ed25519 identities, both org members. The
    // human signs the agent's delegation hop (it holds the key at mint), so the chain is
    // cryptographically verifiable, not merely asserted.
    let human_minted = identity::mint_human("justin");
    let human = human_minted.actor.clone();
    store.put_actor(human.clone());
    let mut members = vec![Membership { actor: human.id.clone(), role: Role::Owner }];
    if let Some(agent) =
        identity::mint_agent("agent:reviewer", &human, &human_minted.secret_key, "*", Lifetime::Static)
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
