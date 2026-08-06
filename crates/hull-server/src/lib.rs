//! The Hull server as a **library**, so both the OSS binary and a private hosted binary reuse it.
//!
//! The open-core seam is [`run`]'s `register_plugins` argument: the OSS binary passes a no-op; a
//! hosted binary (in a separate private repo) passes a closure that registers its closed plugins —
//! `hull_server::run(opts, |reg| hull_hosted::register(reg))`. The core never names a hosted crate.
//!
//! Endpoints: `/health` · `/api/home` · `/api/feed` (SSE) · `/api/repos` ·
//! `/api/repos/:repo/issues` · `/api/scan` · `/api/plugins`.

pub mod activity;
pub mod agentlogin;
pub mod agentsession;
pub mod artifacts;
pub mod autonomy;
pub mod ci;
pub mod connections;
pub mod claims;
pub mod reviewcache;
pub mod ingress;
pub mod keeld;
pub mod mirror;
pub mod nostr;
pub mod passkey;
pub mod reposettings;
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
use webauthn_rs::prelude::{Passkey, PublicKeyCredential, RegisterPublicKeyCredential, Uuid};
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
    /// In-flight passkey ceremonies, keyed by an opaque flow id handed to the client.
    reg_flows: HashMap<String, passkey::RegFlow>,
    add_flows: HashMap<String, passkey::AddFlow>,
    auth_flows: HashMap<String, passkey::AuthFlow>,
    /// In-flight GitHub App install handoffs: opaque state → (account id, expiry). Minted only for an
    /// authed admin of that account, single-use, so the setup callback can connect the installation
    /// WITHOUT the browser redirect carrying a session — and nobody can connect an org they don't admin.
    gh_pending: HashMap<String, (String, u64)>,
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
    /// WebAuthn relying party for passkey accounts.
    webauthn: Arc<webauthn_rs::prelude::Webauthn>,
    /// Per-repo settings (visibility, default reviewers, team access).
    repo_settings: Arc<reposettings::RepoSettingsStore>,
    /// Per-account forge connections (GitHub App installations).
    connections: Arc<connections::ForgeConnections>,
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
        webauthn: Arc::new(passkey::build()),
        repo_settings: Arc::new(reposettings::RepoSettingsStore::from_env()),
        connections: Arc::new(connections::ForgeConnections::from_env()),
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
        .route("/api/capabilities", get(capabilities))
        .route("/api/actors/:id/revoke", post(revoke_actor))
        .route("/api/actors/:id/renew", post(renew_delegation))
        .route("/api/actors/:id/nostr", post(set_nostr_key))
        .route("/api/actors/:id/github", post(link_github))
        .route("/api/accounts", get(accounts_list).post(create_account))
        .route("/api/accounts/available", get(account_available))
        .route("/api/repos/available", get(repo_available))
        .route("/api/auth/available", get(username_available))
        .route("/api/accounts/:id/members", post(add_member))
        .route("/api/accounts/:id/members/:actor", axum::routing::delete(remove_member))
        .route("/api/accounts/:id/teams", get(teams_list).post(create_team))
        .route("/api/accounts/:id/teams/:team", axum::routing::delete(delete_team))
        .route("/api/accounts/:id/teams/:team/members", post(team_add_member))
        .route("/api/accounts/:id/teams/:team/members/:actor", axum::routing::delete(team_remove_member))
        .route("/api/auth/challenge", get(auth_challenge))
        .route("/api/auth/login", post(auth_login))
        .route("/api/auth/me", get(auth_me))
        // passkey (WebAuthn) accounts — passwordless signup + login
        .route("/api/auth/register/start", post(register_start))
        .route("/api/auth/register/finish", post(register_finish))
        .route("/api/auth/passkey/start", post(passkey_start))
        .route("/api/auth/passkey/finish", post(passkey_finish))
        // account self-service (settings): username/email + passkey management
        .route("/api/account", get(account_get).put(account_update))
        .route("/api/profile", get(profile))
        .route("/api/orgs/:handle/profile", get(org_profile))
        .route("/api/account/passkeys/start", post(account_passkey_start))
        .route("/api/account/passkeys/finish", post(account_passkey_finish))
        .route("/api/account/passkeys/:cred", axum::routing::delete(account_passkey_delete))
        .route("/api/me", get(me_profile))
        .route("/api/notifications", get(notifications_list))
        .route("/api/repos", get(repos_list).post(create_repo_handler))
        .route("/api/accounts/:id/repo-defaults", get(repo_defaults_get).put(repo_defaults_set))
        .route("/api/accounts/:id/ai", get(ai_connections_get).post(ai_connection_add))
        .route("/api/accounts/:id/ai/rotate", axum::routing::put(ai_rotate_set))
        .route("/api/accounts/:id/ai/:cid", axum::routing::delete(ai_connection_delete))
        .route("/api/accounts/:id/ai/agent/start", post(ai_agent_login_start))
        .route("/api/accounts/:id/ai/agent/complete", post(ai_agent_login_complete))
        .route("/api/accounts/:id/ai/agent/cancel", post(ai_agent_login_cancel))
        .route("/api/ai/agents", get(ai_agents_detect))
        .route("/api/accounts/:id/github", get(github_status).delete(github_disconnect))
        .route("/api/accounts/:id/github/connect", post(github_connect))
        .route("/api/accounts/:id/github/connect-url", post(github_connect_url))
        .route("/api/github/setup", get(github_setup))
        .route("/api/accounts/:id/github/importable", get(github_importable))
        .route("/api/accounts/:id/repos/import", post(import_repo_handler))
        .route("/api/repos/:tenant/:repo/issues", get(issues).post(create_issue))
        .route("/api/repos/:tenant/:repo/issues/:number", axum::routing::patch(update_issue))
        .route("/api/repos/:tenant/:repo/why", get(why))
        .route("/api/repos/:tenant/:repo/branches", get(repo_branches))
        .route("/api/repos/:tenant/:repo/tree", get(repo_tree))
        .route("/api/repos/:tenant/:repo/blob", get(repo_blob))
        .route("/api/repos/:tenant/:repo/search", get(repo_search))
        .route("/api/repos/:tenant/:repo/graph", get(repo_graph))
        .route("/api/repos/:tenant/:repo/prs", get(prs).post(create_pr))
        .route("/api/repos/:tenant/:repo/prs/:number/merge", post(merge_pr))
        .route("/api/repos/:tenant/:repo/prs/:number/close", post(close_pr))
        .route("/api/repos/:tenant/:repo/prs/:number/auto-review", post(auto_review))
        .route("/api/repos/:tenant/:repo/prs/:number/reviewers", post(request_reviewer))
        .route("/api/repos/:tenant/:repo/prs/:number/fix", post(fix_finding))
        .route("/api/repos/:tenant/:repo/mirror", get(mirror_status))
        .route("/api/repos/:tenant/:repo/mirror/push", post(mirror_push_now))
        .route("/api/repos/:tenant/:repo/mirror/inbound", post(mirror_inbound))
        .route("/api/repos/:tenant/:repo/mirror/github", post(mirror_github_webhook))
        .route("/api/repos/:tenant/:repo/reviews", get(reviews).post(create_review))
        .route("/api/repos/:tenant/:repo/artifacts/:id", get(get_artifact))
        .route("/api/repos/:tenant/:repo/comments", get(comments_list).post(create_comment))
        .route("/api/repos/:tenant/:repo/comments/:id", axum::routing::delete(delete_comment))
        .route("/api/repos/:tenant/:repo/change/:id", get(change_info))
        .route("/api/repos/:tenant/:repo/change/:id/diff", get(change_diff))
        .route("/api/repos/:tenant/:repo/change/:id/file", get(change_file))
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
        .route("/api/repos/:tenant/:repo/settings", get(get_repo_settings).put(set_repo_settings))
        .route("/api/repos/:tenant/:repo/labels", get(repo_labels))
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
async fn home(State(app): State<App>, headers: axum::http::HeaderMap, Query(_q): Query<HashMap<String, String>>) -> Json<Value> {
    // Personalized: the signed-in user's repos across EVERY account they belong to, ranked by
    // activity (active repos first, then their quiet repos). Not a global tenant. Logged out → empty.
    let Some(actor) = authed_actor(&app, &headers) else {
        return Json(json!({ "repos": [], "accounts": [] }));
    };
    let accts = member_accounts(&app, &actor.id);
    let mut items: Vec<Value> = Vec::new();
    for acct in &accts {
        let ranked = app.hub.home(&acct.handle);
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in &ranked {
            seen.insert(r.repo.clone());
            items.push(json!({ "tenant": acct.handle, "repo": r.repo, "score": r.score, "last_ts": r.last_ts, "active_actors": r.active_actors, "hot_files": r.hot_files }));
        }
        // Include the account's repos that have no recent activity, so created/imported repos show.
        for repo in app.store.repos().into_iter().filter(|rp| rp.owner == acct.id) {
            if !seen.contains(&repo.name) {
                items.push(json!({ "tenant": acct.handle, "repo": repo.name, "score": 0.0, "last_ts": 0, "active_actors": [], "hot_files": [] }));
            }
        }
    }
    items.sort_by(|a, b| {
        b.get("score").and_then(Value::as_f64).unwrap_or(0.0)
            .partial_cmp(&a.get("score").and_then(Value::as_f64).unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    // Relevant open work across the user's repos — issues they opened / are assigned / are mentioned in,
    // and open PRs they authored or were asked to review — so the home page surfaces the actual to-dos,
    // not just repos.
    let me = actor.id.as_str();
    let mut rel_issues: Vec<Value> = Vec::new();
    let mut rel_prs: Vec<Value> = Vec::new();
    for acct in &accts {
        for repo in app.store.repos().into_iter().filter(|rp| rp.owner == acct.id) {
            let key = format!("{}/{}", acct.handle, repo.name);
            for is in app.store.issues(&key) {
                if !matches!(is.status, IssueStatus::Open) { continue; }
                let reason = if is.author == me { "you opened" } else if is.assignees.iter().any(|a| a == me) { "assigned to you" } else if is.referenced_actors.iter().any(|a| a == me) { "you're mentioned" } else { continue };
                rel_issues.push(json!({ "tenant": acct.handle, "repo": repo.name, "number": is.number, "title": is.title, "author": is.author, "reason": reason, "ts": is.created_unix }));
            }
            for pr in app.store.prs(&key) {
                if pr.state != PrState::Open { continue; }
                let reason = if pr.reviewers.iter().any(|a| a == me) { "review requested" } else if pr.author == me { "you opened" } else { continue };
                rel_prs.push(json!({ "tenant": acct.handle, "repo": repo.name, "number": pr.number, "title": pr.title, "author": pr.author, "reason": reason, "verification": pr.verification }));
            }
        }
    }
    rel_issues.sort_by(|a, b| b["ts"].as_u64().unwrap_or(0).cmp(&a["ts"].as_u64().unwrap_or(0)));
    rel_issues.truncate(15);
    rel_prs.truncate(15);
    let handles: Vec<String> = accts.iter().map(|a| a.handle.clone()).collect();
    Json(json!({ "repos": items, "accounts": handles, "issues": rel_issues, "prs": rel_prs }))
}

/// `GET /api/profile` — the signed-in user's profile: bio + a year of contributions (their own and
/// their accountable agents'), bucketed by day, across every repo they belong to. Powers the heatmap.
async fn profile(State(app): State<App>, headers: axum::http::HeaderMap) -> Response {
    let Some(me) = authed_actor(&app, &headers) else {
        return (StatusCode::UNAUTHORIZED, "not signed in").into_response();
    };
    let bio = app.store.user_by_actor(&me.id).map(|u| u.bio).unwrap_or_default();
    let since = now().saturating_sub(371 * 86_400);
    // A change's author is the git author string ("handle <email> ..."), so we attribute by HANDLE:
    // mine = my own handle + every agent handle whose accountability roots at me.
    let mut mine: std::collections::HashSet<String> = std::collections::HashSet::new();
    mine.insert(me.handle.clone());
    let mut agent_handles: std::collections::HashSet<String> = std::collections::HashSet::new();
    for a in app.store.actors() {
        if a.id != me.id && a.human_principal().map(|h| h == &me.id).unwrap_or(false) {
            mine.insert(a.handle.clone());
            agent_handles.insert(a.handle.clone());
        }
    }
    // The git author header is "Name <email> unixtime tz"; take the name part as the handle.
    let handle_of = |author: &str| -> String { author.split_once(" <").map(|(n, _)| n.trim().to_string()).unwrap_or_else(|| author.trim().to_string()) };
    // Per day, split human (my own commits) vs agent (my agents') so each heatmap cell can render two
    // triangles.
    let mut days: HashMap<u64, (u64, u64)> = HashMap::new();
    let mut by_handle: HashMap<String, u64> = HashMap::new();
    let mut total = 0u64;
    // Token usage over time: per day, sum tokens in / out of the sessions behind my changes.
    let mut tok_days: HashMap<u64, (u64, u64)> = HashMap::new();
    let (mut tok_in, mut tok_out) = (0u64, 0u64);
    for acct in member_accounts(&app, &me.id) {
        for repo in app.store.repos().into_iter().filter(|r| r.owner == acct.id) {
            let key = format!("{}/{}", acct.handle, repo.name);
            let roots: Vec<String> = app.store.prs(&key).into_iter().flat_map(|p| p.changes).collect();
            for (author, ts, id) in app.repos.history(&acct.handle, &repo.name, &roots, since) {
                let h = handle_of(&author);
                if mine.contains(&h) {
                    let e = days.entry(ts / 86_400).or_default();
                    if agent_handles.contains(&h) { e.1 += 1; } else { e.0 += 1; }
                    *by_handle.entry(h).or_default() += 1;
                    total += 1;
                    if let Some(sr) = app.store.session_record(&key, &id) {
                        let te = tok_days.entry(ts / 86_400).or_default();
                        te.0 += sr.tokens_in; te.1 += sr.tokens_out;
                        tok_in += sr.tokens_in; tok_out += sr.tokens_out;
                    }
                }
            }
        }
    }
    let days_v: Vec<Value> = days.into_iter().map(|(d, (human, agent))| json!({ "day": d, "human": human, "agent": agent, "count": human + agent })).collect();
    let mut tok_v: Vec<Value> = tok_days.into_iter().map(|(d, (i, o))| json!({ "day": d, "in": i, "out": o })).collect();
    tok_v.sort_by_key(|v| v["day"].as_u64().unwrap_or(0));
    let mut agents_v: Vec<Value> = by_handle
        .iter()
        .filter(|(h, _)| agent_handles.contains(*h))
        .map(|(h, c)| json!({ "handle": h, "count": c }))
        .collect();
    agents_v.sort_by(|a, b| b["count"].as_u64().unwrap_or(0).cmp(&a["count"].as_u64().unwrap_or(0)));
    let human_count = by_handle.get(&me.handle).copied().unwrap_or(0);
    Json(json!({ "handle": me.handle, "bio": bio, "total": total, "human_count": human_count, "days": days_v, "agents": agents_v,
        "tokens": { "in": tok_in, "out": tok_out, "series": tok_v } })).into_response()
}

/// `GET /api/orgs/:handle/profile` — an organization's public profile: a year of contributions from
/// **everyone** who works in its repos (humans and their agents), bucketed by day, plus token usage
/// over time. The org page is public, so this needs no auth; it only reads repos the org owns.
async fn org_profile(State(app): State<App>, axum::extract::Path(handle): axum::extract::Path<String>) -> Response {
    let Some(acct) = app.store.accounts().into_iter().find(|a| a.handle == handle) else {
        return (StatusCode::NOT_FOUND, "no such organization").into_response();
    };
    let since = now().saturating_sub(371 * 86_400);
    // An author handle counts as an agent if any actor bearing that handle is an agent.
    let mut agent_handles: std::collections::HashSet<String> = std::collections::HashSet::new();
    for a in app.store.actors() {
        if a.kind == hull_core::ActorKind::Agent { agent_handles.insert(a.handle.clone()); }
    }
    let handle_of = |author: &str| -> String { author.split_once(" <").map(|(n, _)| n.trim().to_string()).unwrap_or_else(|| author.trim().to_string()) };
    let mut days: HashMap<u64, (u64, u64)> = HashMap::new();
    let mut by_handle: HashMap<String, u64> = HashMap::new();
    let mut total = 0u64;
    let mut tok_days: HashMap<u64, (u64, u64)> = HashMap::new();
    let (mut tok_in, mut tok_out) = (0u64, 0u64);
    for repo in app.store.repos().into_iter().filter(|r| r.owner == acct.id) {
        let key = format!("{}/{}", acct.handle, repo.name);
        let roots: Vec<String> = app.store.prs(&key).into_iter().flat_map(|p| p.changes).collect();
        for (author, ts, id) in app.repos.history(&acct.handle, &repo.name, &roots, since) {
            let h = handle_of(&author);
            let e = days.entry(ts / 86_400).or_default();
            if agent_handles.contains(&h) { e.1 += 1; } else { e.0 += 1; }
            *by_handle.entry(h.clone()).or_default() += 1;
            total += 1;
            if let Some(sr) = app.store.session_record(&key, &id) {
                let te = tok_days.entry(ts / 86_400).or_default();
                te.0 += sr.tokens_in; te.1 += sr.tokens_out;
                tok_in += sr.tokens_in; tok_out += sr.tokens_out;
            }
        }
    }
    let days_v: Vec<Value> = days.into_iter().map(|(d, (human, agent))| json!({ "day": d, "human": human, "agent": agent, "count": human + agent })).collect();
    let mut tok_v: Vec<Value> = tok_days.into_iter().map(|(d, (i, o))| json!({ "day": d, "in": i, "out": o })).collect();
    tok_v.sort_by_key(|v| v["day"].as_u64().unwrap_or(0));
    // Top contributors (agents and humans alike), busiest first.
    let mut contributors: Vec<Value> = by_handle
        .iter()
        .map(|(h, c)| json!({ "handle": h, "count": c, "agent": agent_handles.contains(h) }))
        .collect();
    contributors.sort_by(|a, b| b["count"].as_u64().unwrap_or(0).cmp(&a["count"].as_u64().unwrap_or(0)));
    // Public repo names, so a signed-out visitor can still browse the org (private repos are omitted).
    let repo_names: Vec<String> = app.store.repos().into_iter()
        .filter(|r| r.owner == acct.id && !app.repo_settings.get(&format!("{}/{}", acct.handle, r.name)).private)
        .map(|r| r.name).collect();
    Json(json!({ "handle": acct.handle, "members": acct.members.len(), "repos": repo_names.len(), "repo_names": repo_names,
        "total": total, "days": days_v, "contributors": contributors,
        "tokens": { "in": tok_in, "out": tok_out, "series": tok_v } })).into_response()
}

/// The accounts an actor is a member of.
fn member_accounts(app: &App, actor_id: &str) -> Vec<Account> {
    app.store.accounts().into_iter().filter(|a| a.members.iter().any(|m| m.actor == actor_id)).collect()
}

/// Visibility gate: may `actor` (or an anonymous caller) read this repo? Public repos are readable by
/// anyone; a private repo only by a member of its owning account, or a member of a team the repo
/// grants access to.
fn can_read_repo(app: &App, actor_id: Option<&str>, tenant: &str, repo: &str) -> bool {
    let key = format!("{tenant}/{repo}");
    let settings = app.repo_settings.get(&key);
    if !settings.private {
        return true;
    }
    let Some(aid) = actor_id else { return false };
    let Some(acct) = app.store.accounts().into_iter().find(|a| a.handle == tenant) else { return false };
    if acct.members.iter().any(|m| m.actor == aid) {
        return true;
    }
    app.store
        .teams(&acct.id)
        .into_iter()
        .any(|t| settings.team_access.iter().any(|ta| ta.team == t.id) && t.members.iter().any(|m| m.actor == aid))
}

/// A 404 for repos the caller can't see — used so a private repo doesn't even reveal its existence.
#[allow(clippy::result_large_err)]
fn require_repo_read(app: &App, headers: &axum::http::HeaderMap, tenant: &str, repo: &str) -> Result<(), Response> {
    let actor = authed_actor(app, headers).map(|a| a.id);
    if can_read_repo(app, actor.as_deref(), tenant, repo) {
        Ok(())
    } else {
        Err((StatusCode::NOT_FOUND, "not found").into_response())
    }
}

/// What AI capabilities this instance can actually fulfill, so the UI hides actions it can't run
/// (e.g. "Fix with AI" when no fixer is configured).
async fn capabilities(State(app): State<App>) -> Json<Value> {
    Json(json!({ "ai_fix": app.registry.has_fixer(), "ai_review": app.registry.has_reviewer() }))
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
async fn accounts_list(State(app): State<App>, headers: axum::http::HeaderMap) -> Json<Value> {
    let repos = app.store.repos();
    // Only the accounts the caller belongs to — an org you're not a member of is not yours to see.
    let visible = match authed_actor(&app, &headers) {
        Some(a) => member_accounts(&app, &a.id),
        None => Vec::new(),
    };
    let accounts: Vec<Value> = visible
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
    let Some(actor) = resolve_actor_ref(&app, &body) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "unknown actor or username").into_response();
    };
    let role = parse_role(body.get("role").and_then(Value::as_str));
    acct.members.retain(|m| m.actor != actor);
    acct.members.push(Membership { actor, role });
    app.store.put_account(acct.clone());
    (StatusCode::CREATED, Json(json!({ "account": acct }))).into_response()
}

/// Gate an org-management action: the caller must be an Owner or Admin of the account. Returns the
/// loaded account + the acting actor.
#[allow(clippy::result_large_err)]
fn require_account_admin(app: &App, headers: &axum::http::HeaderMap, account_id: &str) -> Result<(Account, Actor), Response> {
    let acting = require_actor(app, headers, "")?;
    let Some(acct) = app.store.accounts().into_iter().find(|a| a.id == account_id) else {
        return Err((StatusCode::NOT_FOUND, "no such account").into_response());
    };
    let is_admin = acct.members.iter().any(|m| m.actor == acting.id && matches!(m.role, Role::Owner | Role::Admin));
    if !is_admin {
        return Err((StatusCode::FORBIDDEN, "only an org owner/admin can do that").into_response());
    }
    Ok((acct, acting))
}

/// Resolve a member reference in a body to an actor id: `{actor}` (existing actor) or `{username}`
/// (a hosted account's driving actor).
fn resolve_actor_ref(app: &App, body: &Value) -> Option<String> {
    if let Some(a) = body.get("actor").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        if app.store.actor(a).is_some() {
            return Some(a.to_string());
        }
    }
    if let Some(un) = body.get("username").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        return app.store.user_by_username(un).map(|u| u.actor);
    }
    None
}

fn parse_role(s: Option<&str>) -> Role {
    match s {
        Some("owner") => Role::Owner,
        Some("admin") => Role::Admin,
        Some("read") => Role::Read,
        _ => Role::Write,
    }
}

/// Normalize a user/org/repo handle: trim, and collapse any run of whitespace to a single `_`. The
/// canonical form the UI shows while typing and the server stores.
fn sanitize_handle(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join("_")
}

/// `GET /api/accounts/available?handle=X` — is an org/account handle free? Returns the sanitized form.
async fn account_available(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let handle = sanitize_handle(q.get("handle").map(String::as_str).unwrap_or(""));
    let taken = handle.is_empty() || app.store.accounts().iter().any(|a| a.handle.eq_ignore_ascii_case(&handle));
    Json(json!({ "handle": handle, "available": !handle.is_empty() && !taken }))
}

/// `GET /api/repos/available?account=X&name=Y` — is a repo name free under that account?
async fn repo_available(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let account = q.get("account").map(String::as_str).unwrap_or("");
    let name = sanitize_handle(q.get("name").map(String::as_str).unwrap_or(""));
    let taken = app
        .store
        .accounts()
        .into_iter()
        .find(|a| a.handle.eq_ignore_ascii_case(account))
        .map(|acct| app.store.repos().into_iter().any(|r| r.owner == acct.id && r.name.eq_ignore_ascii_case(&name)))
        .unwrap_or(false);
    Json(json!({ "name": name, "available": !name.is_empty() && !taken }))
}

/// `GET /api/auth/available?username=X` — is a username free? Returns the sanitized form.
async fn username_available(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let username = sanitize_handle(q.get("username").map(String::as_str).unwrap_or(""));
    let taken = username.is_empty() || app.store.user_by_username(&username).is_some();
    Json(json!({ "username": username, "available": !username.is_empty() && !taken }))
}

/// `POST /api/accounts` — create an organization (the caller becomes its Owner).
async fn create_account(State(app): State<App>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let acting = match require_actor(&app, &headers, "") {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let handle = sanitize_handle(body.get("handle").and_then(Value::as_str).unwrap_or(""));
    if handle.is_empty() {
        return (StatusCode::BAD_REQUEST, "handle is required").into_response();
    }
    if app.store.accounts().iter().any(|a| a.handle.eq_ignore_ascii_case(&handle)) {
        return (StatusCode::CONFLICT, "that handle is taken").into_response();
    }
    let kind = match body.get("kind").and_then(Value::as_str) {
        Some("personal") => AccountKind::Personal,
        _ => AccountKind::Organization,
    };
    let acct = Account {
        id: format!("acct_{}", identity::random_hex(8)),
        kind,
        handle,
        members: vec![Membership { actor: acting.id.clone(), role: Role::Owner }],
    };
    app.store.put_account(acct.clone());
    (StatusCode::CREATED, Json(json!({ "account": acct }))).into_response()
}

/// `DELETE /api/accounts/:id/members/:actor` — remove a member (never the last owner).
async fn remove_member(State(app): State<App>, Path((id, actor)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    let (mut acct, _) = match require_account_admin(&app, &headers, &id) {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let owners = acct.members.iter().filter(|m| matches!(m.role, Role::Owner)).count();
    let removing_owner = acct.members.iter().any(|m| m.actor == actor && matches!(m.role, Role::Owner));
    if removing_owner && owners <= 1 {
        return (StatusCode::BAD_REQUEST, "cannot remove the last owner").into_response();
    }
    acct.members.retain(|m| m.actor != actor);
    app.store.put_account(acct.clone());
    Json(json!({ "account": acct })).into_response()
}

/// `GET /api/accounts/:id/teams` — the org's teams and their members (public read).
async fn teams_list(State(app): State<App>, Path(id): Path<String>) -> Json<Value> {
    let teams: Vec<Value> = app
        .store
        .teams(&id)
        .into_iter()
        .map(|t| {
            let members: Vec<Value> = t
                .members
                .iter()
                .map(|m| json!({ "actor": m.actor, "handle": app.store.actor(&m.actor).map(|a| a.handle).unwrap_or_default(), "role": m.role }))
                .collect();
            json!({ "id": t.id, "name": t.name, "members": members })
        })
        .collect();
    Json(json!({ "teams": teams }))
}

/// `POST /api/accounts/:id/teams` — create a team (`{name}`).
async fn create_team(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(resp) = require_account_admin(&app, &headers, &id) {
        return resp;
    }
    let name = body.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "team name is required").into_response();
    }
    let team = Team { id: format!("team_{}", identity::random_hex(8)), account: id, name, members: vec![] };
    app.store.put_team(team.clone());
    (StatusCode::CREATED, Json(json!({ "team": team }))).into_response()
}

/// `DELETE /api/accounts/:id/teams/:team` — remove a team.
async fn delete_team(State(app): State<App>, Path((id, team)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(resp) = require_account_admin(&app, &headers, &id) {
        return resp;
    }
    if app.store.team(&team).map(|t| t.account != id).unwrap_or(true) {
        return (StatusCode::NOT_FOUND, "no such team").into_response();
    }
    app.store.delete_team(&team);
    Json(json!({ "ok": true })).into_response()
}

/// `POST /api/accounts/:id/teams/:team/members` — add a member (`{actor|username, role}`).
async fn team_add_member(State(app): State<App>, Path((id, team)): Path<(String, String)>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(resp) = require_account_admin(&app, &headers, &id) {
        return resp;
    }
    let Some(mut t) = app.store.team(&team).filter(|t| t.account == id) else {
        return (StatusCode::NOT_FOUND, "no such team").into_response();
    };
    let Some(actor) = resolve_actor_ref(&app, &body) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "unknown actor or username").into_response();
    };
    let role = parse_role(body.get("role").and_then(Value::as_str));
    t.members.retain(|m| m.actor != actor);
    t.members.push(Membership { actor, role });
    app.store.put_team(t.clone());
    (StatusCode::CREATED, Json(json!({ "team": t }))).into_response()
}

/// `DELETE /api/accounts/:id/teams/:team/members/:actor` — remove a team member.
async fn team_remove_member(State(app): State<App>, Path((id, team, actor)): Path<(String, String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(resp) = require_account_admin(&app, &headers, &id) {
        return resp;
    }
    let Some(mut t) = app.store.team(&team).filter(|t| t.account == id) else {
        return (StatusCode::NOT_FOUND, "no such team").into_response();
    };
    t.members.retain(|m| m.actor != actor);
    app.store.put_team(t.clone());
    Json(json!({ "team": t })).into_response()
}

/// Build the settings JSON (no auth — callers gate).
fn repo_settings_value(app: &App, tenant: &str, repo: &str) -> Value {
    settings_value(app, &app.repo_settings.get(&format!("{tenant}/{repo}")))
}

/// Serialize a RepoSettings to the JSON the UI reads (shared by repo settings + org defaults).
fn settings_value(app: &App, s: &reposettings::RepoSettings) -> Value {
    let reviewers: Vec<Value> = s
        .default_reviewers
        .iter()
        .map(|id| json!({ "actor": id, "handle": app.store.actor(id).map(|a| a.handle).unwrap_or_default() }))
        .collect();
    let teams: Vec<Value> = s.team_access.iter().map(|t| json!({ "team": t.team, "role": t.role })).collect();
    let labels: Vec<Value> = s.labels.iter().map(|l| json!({ "name": l.name, "color": l.color, "icon": l.icon })).collect();
    json!({
        "private": s.private,
        "unlisted": s.unlisted,
        "visibility": if s.private { "private" } else if s.unlisted { "unlisted" } else { "public" },
        "require_review_to_land": s.require_review_to_land,
        "author_independence": !s.allow_self_approve,
        "default_reviewers": reviewers,
        "team_access": teams,
        "labels": labels,
    })
}

/// `GET /api/repos/:tenant/:repo/labels` — the repo's configured issue labels (name + color). Readable
/// by anyone who can read the repo, so the new-issue form can offer them.
async fn repo_labels(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(resp) = require_repo_read(&app, &headers, &tenant, &repo) {
        return resp;
    }
    let s = app.repo_settings.get(&format!("{tenant}/{repo}"));
    let labels: Vec<Value> = s.labels.iter().map(|l| json!({ "name": l.name, "color": l.color, "icon": l.icon })).collect();
    Json(json!({ "labels": labels })).into_response()
}

/// `GET /api/repos/:tenant/:repo/settings` — repo settings. **Owner/admin only** (settings expose
/// access grants, so they are not public).
async fn get_repo_settings(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    let acting = match require_actor(&app, &headers, "") {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if !is_repo_admin(&app, &tenant, &repo, &acting.id) {
        return (StatusCode::FORBIDDEN, "only a repo owner/admin can view settings").into_response();
    }
    Json(repo_settings_value(&app, &tenant, &repo)).into_response()
}

/// `PUT /api/repos/:tenant/:repo/settings` — update repo settings (owner/admin only).
async fn set_repo_settings(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let acting = match require_actor(&app, &headers, "") {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if !is_repo_admin(&app, &tenant, &repo, &acting.id) {
        return (StatusCode::FORBIDDEN, "only a repo owner/admin can change settings").into_response();
    }
    let key = format!("{tenant}/{repo}");
    let mut s = app.repo_settings.get(&key);
    apply_settings_patch(&app, &mut s, &body);
    app.repo_settings.set(&key, s);
    Json(repo_settings_value(&app, &tenant, &repo)).into_response()
}

/// Apply a settings JSON patch (shared by repo settings + org defaults).
fn apply_settings_patch(app: &App, s: &mut reposettings::RepoSettings, body: &Value) {
    // Accept either a `visibility` enum ("public"|"private"|"unlisted") or the legacy `private` bool.
    if let Some(vis) = body.get("visibility").and_then(Value::as_str) {
        s.private = vis == "private";
        s.unlisted = vis == "unlisted";
    }
    if let Some(p) = body.get("private").and_then(Value::as_bool) {
        s.private = p;
    }
    if let Some(u) = body.get("unlisted").and_then(Value::as_bool) {
        s.unlisted = u;
    }
    if let Some(r) = body.get("require_review_to_land").and_then(Value::as_bool) {
        s.require_review_to_land = r;
    }
    if let Some(ai) = body.get("author_independence").and_then(Value::as_bool) {
        s.allow_self_approve = !ai;
    }
    if let Some(arr) = body.get("default_reviewers").and_then(Value::as_array) {
        s.default_reviewers = arr.iter().filter_map(|v| v.as_str()).filter(|id| app.store.actor(id).is_some()).map(str::to_string).collect();
    }
    if let Some(arr) = body.get("team_access").and_then(Value::as_array) {
        s.team_access = arr
            .iter()
            .filter_map(|v| {
                let team = v.get("team").and_then(Value::as_str)?.to_string();
                let role = v.get("role").and_then(Value::as_str).unwrap_or("read").to_string();
                Some(reposettings::TeamAccess { team, role })
            })
            .collect();
    }
    if let Some(arr) = body.get("labels").and_then(Value::as_array) {
        s.labels = arr
            .iter()
            .filter_map(|v| {
                let name = v.get("name").and_then(Value::as_str)?.trim().to_string();
                if name.is_empty() { return None; }
                let color = v.get("color").and_then(Value::as_str).unwrap_or("#8b949e").to_string();
                let icon = v.get("icon").and_then(Value::as_str).unwrap_or("").chars().take(4).collect();
                Some(reposettings::Label { name, color, icon })
            })
            .collect();
    }
}

/// Resolve an account by id or handle and require the caller be its owner/admin.
#[allow(clippy::result_large_err)]
fn require_account_admin_ref(app: &App, headers: &axum::http::HeaderMap, acct_ref: &str) -> Result<(Account, Actor), Response> {
    let acting = require_actor(app, headers, "")?;
    let Some(acct) = app.store.accounts().into_iter().find(|a| a.id == acct_ref || a.handle.eq_ignore_ascii_case(acct_ref)) else {
        return Err((StatusCode::NOT_FOUND, "no such account").into_response());
    };
    let is_admin = acct.members.iter().any(|m| m.actor == acting.id && matches!(m.role, Role::Owner | Role::Admin));
    if !is_admin {
        return Err((StatusCode::FORBIDDEN, "only an org owner/admin can do that").into_response());
    }
    Ok((acct, acting))
}

/// `POST /api/repos` — create an empty repo under an account you administer. Body `{account, name}`
/// (`account` = an account id or handle). The repo can then be cloned + pushed to.
async fn create_repo_handler(State(app): State<App>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let acct_ref = body.get("account").and_then(Value::as_str).unwrap_or("").trim();
    let (acct, _) = match require_account_admin_ref(&app, &headers, acct_ref) {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let name = sanitize_handle(body.get("name").and_then(Value::as_str).unwrap_or(""));
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "name is required").into_response();
    }
    let tenant = acct.handle.clone();
    if app.store.repos().iter().any(|r| r.owner == acct.id && r.name == name) {
        return (StatusCode::CONFLICT, "a repo with that name already exists").into_response();
    }
    if let Err(e) = app.repos.create_repo(&tenant, &name) {
        return (StatusCode::UNPROCESSABLE_ENTITY, format!("could not create repo: {e}")).into_response();
    }
    let repo = Repo { id: format!("repo_{tenant}_{name}"), owner: acct.id.clone(), name: name.clone(), default_branch: "main".into() };
    app.store.put_repo(repo.clone());
    // Inherit the org's default repo settings, if it has configured any.
    let defaults = app.repo_settings.get(&repo_defaults_key(&acct.id));
    app.repo_settings.set(&format!("{tenant}/{name}"), defaults);
    (StatusCode::CREATED, Json(json!({ "repo": repo, "tenant": tenant, "name": name }))).into_response()
}

/// The RepoSettingsStore key holding an account's default repo settings (inherited by new repos).
fn repo_defaults_key(acct_id: &str) -> String {
    format!("@defaults/{acct_id}")
}

/// `GET /api/accounts/:id/repo-defaults` — the org's default repo settings (owner/admin only).
async fn repo_defaults_get(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id) {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    Json(settings_value(&app, &app.repo_settings.get(&repo_defaults_key(&acct.id)))).into_response()
}

/// `PUT /api/accounts/:id/repo-defaults` — set the org's default repo settings (owner/admin only).
/// These are copied into every repo created afterward.
async fn repo_defaults_set(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id) {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let key = repo_defaults_key(&acct.id);
    let mut s = app.repo_settings.get(&key);
    apply_settings_patch(&app, &mut s, &body);
    app.repo_settings.set(&key, s.clone());
    Json(settings_value(&app, &s)).into_response()
}

/// Default API base for a provider.
fn ai_default_base(provider: &str) -> String {
    match provider {
        "openai" => "https://api.openai.com/v1",
        "anthropic" => "https://api.anthropic.com/v1",
        _ => "https://openrouter.ai/api/v1",
    }
    .to_string()
}

/// `GET /api/accounts/:id/ai` — the account's connected AI backends (credentials redacted to a hint)
/// plus the rotation flag. Account owner/admin only. These lend the account's OpenAI/Claude/OpenRouter
/// access to Hull's AI functions; a repo's reviews use its owning org's connections (else the
/// triggerer's own), rotating when enabled.
async fn ai_connections_get(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id) {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let conns: Vec<Value> = app.store.ai_connections(&acct.id).into_iter().map(|c| {
        let kind = match &c.auth { hull_core::AiAuth::Key { .. } => "key", hull_core::AiAuth::AgentCli { .. } => "agent" };
        let u = app.store.ai_usage(&c.id);
        json!({
            "id": c.id, "provider": c.provider, "label": c.label, "base_url": c.base_url, "auth_kind": kind,
            "hint": c.auth.hint(), "created_unix": c.created_unix, "token_expires_unix": c.token_expires_unix,
            "usage": { "input_tokens": u.input_tokens, "output_tokens": u.output_tokens, "cost_micros": u.cost_micros, "runs": u.runs, "updated_unix": u.updated_unix },
        })
    }).collect();
    Json(json!({ "connections": conns, "rotate": app.store.ai_rotate(&acct.id) })).into_response()
}

/// `POST /api/accounts/:id/ai` — connect a backend. Either an API key
/// (`{provider: openai|anthropic|openrouter, api_key, label?, base_url?}`) or a locally-installed
/// **agent CLI** run with the user's own subscription (`{provider: claude-code|codex, label?}`).
/// Owner/admin only.
async fn ai_connection_add(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id) {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let provider = body.get("provider").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
    let n = app.store.ai_connections(&acct.id).len();
    // Agent-CLI connection: uses the user's own Claude Code / Codex login, no key.
    let (base_url, auth, def_label) = if let Some(cmd) = agent_command(&provider) {
        let command = body.get("command").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or(cmd).to_string();
        // A key-less agent connection created via this endpoint uses THIS Hull host's own CLI login
        // (self-hosted / single-tenant). Per-user subscription logins go through the relay endpoints
        // below, which populate `session` + identity.
        (String::new(), hull_core::AiAuth::AgentCli { command, session: String::new(), account_email: String::new(), plan: String::new() }, if provider == "codex" { "Codex (this host)".into() } else { "Claude Code (this host)".into() })
    } else if ["openai", "anthropic", "openrouter"].contains(&provider.as_str()) {
        let key = body.get("api_key").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if key.is_empty() {
            return (StatusCode::BAD_REQUEST, "api_key is required for a key connection").into_response();
        }
        let base = body.get("base_url").and_then(Value::as_str).map(str::to_string).filter(|s| !s.is_empty()).unwrap_or_else(|| ai_default_base(&provider));
        (base, hull_core::AiAuth::Key { api_key: key }, format!("{provider} key"))
    } else {
        return (StatusCode::BAD_REQUEST, "provider must be openai|anthropic|openrouter (key) or claude-code|codex (agent)").into_response();
    };
    let label = body.get("label").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).unwrap_or(def_label);
    let conn = hull_core::AiConnection { id: format!("ai_{}_{}", acct.id, n + 1), owner: acct.id.clone(), provider, label, base_url, auth, created_unix: now(), token_expires_unix: None };
    app.store.put_ai_connection(conn.clone());
    (StatusCode::CREATED, Json(json!({ "id": conn.id }))).into_response()
}

/// `GET /api/ai/agents` — which local agent CLIs are installed on this Hull host (so the settings UI
/// only offers the ones present). Any signed-in actor may query.
async fn ai_agents_detect(State(app): State<App>, headers: axum::http::HeaderMap) -> Response {
    if authed_actor(&app, &headers).is_none() {
        return (StatusCode::UNAUTHORIZED, "sign in required").into_response();
    }
    let agents: Vec<Value> = [("claude-code", "claude", "Claude Code"), ("codex", "codex", "Codex")]
        .iter()
        .map(|(kind, cmd, label)| {
            let installed = std::process::Command::new(cmd).arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
            json!({ "kind": kind, "command": cmd, "label": label, "installed": installed })
        })
        .collect();
    Json(json!({ "agents": agents })).into_response()
}

/// `DELETE /api/accounts/:id/ai/:cid` — remove a connection (owner/admin only).
async fn ai_connection_delete(State(app): State<App>, Path((id, cid)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id) {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    // Wipe the per-user credential bundle for an agent session, if this connection had one.
    if let Some(hull_core::AiAuth::AgentCli { session, .. }) = app.store.ai_connections(&acct.id).into_iter().find(|c| c.id == cid).map(|c| c.auth) {
        agentsession::remove(&session);
    }
    Json(json!({ "deleted": app.store.remove_ai_connection(&acct.id, &cid) })).into_response()
}

/// `POST /api/accounts/:id/ai/agent/start` — `{provider: claude-code|codex}`: begin a per-user
/// subscription login. Provisions the user's bundle, drives `<cli> setup-token` under a PTY, and
/// returns `{session, login_url}` — the browser opens `login_url`, the user approves and pastes the
/// code back to `/complete`. Owner/admin only. Runs the PTY work off the async runtime.
async fn ai_agent_login_start(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let (_acct, _) = match require_account_admin_ref(&app, &headers, &id) {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let provider = body.get("provider").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
    let Some(command) = agent_command(&provider).map(str::to_string) else {
        return (StatusCode::BAD_REQUEST, "provider must be claude-code or codex").into_response();
    };
    let (session, dir) = match agentsession::provision() {
        Ok(x) => x,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("could not provision bundle: {e}")).into_response(),
    };
    let session_for_cleanup = session.clone();
    let res = tokio::task::spawn_blocking(move || agentlogin::begin(&command, &session, &dir).map(|b| (session, b))).await;
    match res {
        Ok(Ok((session, b))) => Json(json!({
            "session": session,
            "login_url": b.url,
            "user_code": b.user_code,
            // PasteCode ⇒ the user pastes a code back here; DevicePoll ⇒ they enter user_code on the
            // site and we poll for approval.
            "needs_code": matches!(b.mode, agentlogin::Mode::PasteCode),
            "provider": provider,
        })).into_response(),
        Ok(Err(e)) => {
            agentsession::remove(&session_for_cleanup);
            (StatusCode::BAD_GATEWAY, e).into_response()
        }
        Err(_) => {
            agentsession::remove(&session_for_cleanup);
            (StatusCode::INTERNAL_SERVER_ERROR, "login task panicked").into_response()
        }
    }
}

/// `POST /api/accounts/:id/ai/agent/complete` — `{provider, session, code}`: finish the login by
/// feeding the pasted code to the parked CLI, then verify with `<cli> auth status --json` and persist
/// the connection with the introspected identity. Owner/admin only.
async fn ai_agent_login_complete(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id) {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let provider = body.get("provider").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
    let session = body.get("session").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let code = body.get("code").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let Some(command) = agent_command(&provider).map(str::to_string) else {
        return (StatusCode::BAD_REQUEST, "provider must be claude-code or codex").into_response();
    };
    if session.is_empty() {
        return (StatusCode::BAD_REQUEST, "session is required").into_response();
    }
    let sess = session.clone();
    let cmd = command.clone();
    let out = tokio::task::spawn_blocking(move || {
        let dir = agentsession::dir_for(&sess);
        match agentlogin::finish(&sess, &code) {
            // Device flow not approved yet — the client polls again.
            Ok(agentlogin::Finish::Pending) => Ok(None),
            // Claude paste-code: the CLI printed a long-lived token. Verify it works, write it into the
            // bundle, seal. The token is scoped to inference only (can't read the profile — 403), so the
            // account/plan is whatever the CLI's success screen printed, else just "subscription".
            Ok(agentlogin::Finish::Done { token: Some(tok), email, plan, ttl_days }) => {
                verify_claude_token(&tok)?;
                std::fs::write(dir.join(agentsession::OAUTH_TOKEN_FILE), tok.as_bytes()).map_err(|e| format!("store token: {e}"))?;
                let expires = ttl_days.map(|d| now() + d * 86_400);
                agentsession::seal(&sess).map(|_| Some(json!({
                    "email": email.unwrap_or_default(),
                    "plan": plan.unwrap_or_else(|| "subscription".into()),
                    "expires_unix": expires,
                })))
            }
            // Codex device flow: the CLI wrote its own bundle; verify + read identity, then seal.
            Ok(agentlogin::Finish::Done { token: None, .. }) => match agent_auth_identity(&cmd, &dir) {
                Ok(idy) => agentsession::seal(&sess).map(|_| Some(idy)),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        }
    })
    .await;
    if let Ok(Ok(Some(_))) = &out {
        eprintln!("hull agentlogin: connected {provider} for account {}", acct.id);
    } else if let Ok(Err(e)) = &out {
        eprintln!("hull agentlogin: complete failed for {provider}: {e}");
    }
    let identity = match out {
        Ok(Ok(Some(idy))) => idy,
        // Device flow not approved yet — tell the client to keep polling.
        Ok(Ok(None)) => return (StatusCode::ACCEPTED, Json(json!({ "pending": true }))).into_response(),
        Ok(Err(e)) => {
            agentlogin::abort(&session);
            agentsession::remove(&session);
            return (StatusCode::BAD_GATEWAY, e).into_response();
        }
        Err(_) => {
            agentlogin::abort(&session);
            agentsession::remove(&session);
            return (StatusCode::INTERNAL_SERVER_ERROR, "login task panicked").into_response();
        }
    };
    let n = app.store.ai_connections(&acct.id).len();
    let email = identity.get("email").and_then(Value::as_str).unwrap_or("").to_string();
    let plan = identity.get("plan").and_then(Value::as_str).unwrap_or("").to_string();
    let label = body.get("label").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(|| {
        if email.is_empty() { format!("{provider} (subscription)") } else { email.clone() }
    });
    let conn = hull_core::AiConnection {
        id: format!("ai_{}_{}", acct.id, n + 1),
        owner: acct.id.clone(),
        provider,
        label,
        base_url: String::new(),
        auth: hull_core::AiAuth::AgentCli { command, session: session.clone(), account_email: email, plan },
        created_unix: now(),
        token_expires_unix: identity.get("expires_unix").and_then(Value::as_u64),
    };
    app.store.put_ai_connection(conn.clone());
    (StatusCode::CREATED, Json(json!({ "id": conn.id, "identity": identity }))).into_response()
}

/// `POST /api/accounts/:id/ai/agent/cancel` — `{session}`: discard an in-flight login (owner/admin).
async fn ai_agent_login_cancel(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(resp) = require_account_admin_ref(&app, &headers, &id) {
        return resp;
    }
    let session = body.get("session").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if !session.is_empty() {
        agentlogin::abort(&session);
        agentsession::remove(&session);
    }
    Json(json!({ "cancelled": true })).into_response()
}

/// Verify a bundle is actually authenticated and pull a non-secret `{loggedIn, email, plan}` identity
/// from it, so a failed login never persists a dead connection. Codex has no `auth status --json`, so
/// its identity comes from the tokens it wrote (`auth.json`, email from the id_token claims).
fn agent_auth_identity(command: &str, dir: &std::path::Path) -> Result<Value, String> {
    if command == "codex" {
        return codex_identity(dir);
    }
    let out = std::process::Command::new(command)
        .args(["auth", "status", "--json"])
        .env("CLAUDE_CONFIG_DIR", dir)
        .output()
        .map_err(|e| format!("{command} auth status: {e}"))?;
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|_| format!("{command} auth status returned no JSON"))?;
    if !v.get("loggedIn").and_then(Value::as_bool).unwrap_or(false) {
        return Err("login did not complete — the bundle is not authenticated".into());
    }
    Ok(json!({
        "loggedIn": true,
        "email": v.get("email").and_then(Value::as_str).unwrap_or(""),
        "plan": v.get("subscriptionType").and_then(Value::as_str).unwrap_or(""),
    }))
}

/// Confirm a captured Claude long-lived token actually authenticates, by running a tiny `claude -p`
/// with it in a throwaway config dir. Cheap, and it means a bad capture never persists a dead
/// connection. A 401/invalid-token error fails; a working token (any normal reply) passes.
fn verify_claude_token(token: &str) -> Result<(), String> {
    if !token.starts_with("sk-ant-") {
        return Err("captured value is not a Claude token".into());
    }
    let scratch = agentsession::sessions_root().join(".verify").join(uuid::Uuid::new_v4().simple().to_string());
    let _ = std::fs::create_dir_all(&scratch);
    let out = std::process::Command::new("claude")
        .args(["-p", "Reply with the single word: ok"])
        .env("CLAUDE_CODE_OAUTH_TOKEN", token)
        .env("CLAUDE_CONFIG_DIR", &scratch)
        .stdin(std::process::Stdio::null())
        .output();
    let _ = std::fs::remove_dir_all(&scratch);
    let out = out.map_err(|e| format!("verify token: {e}"))?;
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() && !combined.to_lowercase().contains("invalid") && !combined.contains("401") {
        Ok(())
    } else {
        Err(format!("token did not authenticate: {}", combined.trim().chars().take(160).collect::<String>()))
    }
}

/// Codex identity from the credentials it persisted: confirm `login status` reports logged-in, then
/// read `auth.json` (email decoded from the id_token JWT's claims, plan from `auth_mode`).
fn codex_identity(dir: &std::path::Path) -> Result<Value, String> {
    let status = std::process::Command::new("codex")
        .args(["login", "status"])
        .env("CODEX_HOME", dir)
        .output()
        .map_err(|e| format!("codex login status: {e}"))?;
    let text = String::from_utf8_lossy(&status.stdout);
    if !text.to_lowercase().contains("logged in") {
        return Err("login did not complete — codex is not authenticated".into());
    }
    let auth: Value = std::fs::read(dir.join("auth.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}));
    let email = auth["tokens"]["id_token"].as_str().and_then(jwt_email).unwrap_or_default();
    let plan = auth["auth_mode"].as_str().unwrap_or("chatgpt").to_string();
    Ok(json!({ "loggedIn": true, "email": email, "plan": plan }))
}

/// Decode a JWT's payload (middle segment, base64url) and return its `email` claim, if any. The token
/// is never verified or used for auth here — only read for a display string.
fn jwt_email(jwt: &str) -> Option<String> {
    use base64::Engine;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("email").and_then(Value::as_str).map(str::to_string)
}

/// `PUT /api/accounts/:id/ai/rotate` — `{rotate: bool}`: cycle across the account's connections per
/// request instead of always using the first. Owner/admin only.
async fn ai_rotate_set(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id) {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let on = body.get("rotate").and_then(Value::as_bool).unwrap_or(false);
    app.store.set_ai_rotate(&acct.id, on);
    Json(json!({ "rotate": on })).into_response()
}

/// `GET /api/accounts/:id/github` — the account's GitHub connection status (admin only). Never
/// exposes anything unless the caller administers the account.
async fn github_status(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id) {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    match app.connections.get(&acct.id) {
        Some(c) => Json(json!({ "connected": true, "provider": c.provider, "login": c.login, "connected_unix": c.connected_unix })).into_response(),
        None => Json(json!({ "connected": false })).into_response(),
    }
}

/// `POST /api/accounts/:id/github/connect-url` — begin a GitHub App install for THIS account. Returns
/// the GitHub install URL the admin is redirected to, where THEY pick their org + the repos to grant.
/// A one-time signed `state` (stored server-side for this authed admin) rides along so the setup
/// callback can attach the resulting installation to this account — and to NO other. Admin only.
/// This is what makes it impossible to connect (or even see) an org you don't administer: we never
/// list the App's installations; you only ever get back the one YOU just authorized on GitHub.
async fn github_connect_url(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id) {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let slug = std::env::var("GITHUB_APP_SLUG").unwrap_or_default();
    if slug.trim().is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "GitHub isn't configured on this instance (set GITHUB_APP_SLUG).").into_response();
    }
    let state = identity::random_hex(24);
    {
        let mut a = app.auth.lock().unwrap();
        let cutoff = now().saturating_sub(1800);
        a.gh_pending.retain(|_, (_, exp)| *exp > cutoff);
        a.gh_pending.insert(state.clone(), (acct.id.clone(), now() + 900));
    }
    // GitHub appends ?installation_id=&setup_action=install&state= to this app's Setup URL after install.
    let url = format!("https://github.com/apps/{slug}/installations/new?state={state}");
    Json(json!({ "url": url })).into_response()
}

/// `GET /api/github/setup?installation_id=&state=` — GitHub redirects here after the admin installs the
/// App on THEIR org and selects repos. We consume the one-time `state` (proving an authed admin of a
/// specific account started this), verify the installation, and connect it to THAT account only.
async fn github_setup(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Response {
    let state = q.get("state").cloned().unwrap_or_default();
    let installation = q.get("installation_id").cloned().unwrap_or_default();
    let pending = {
        let mut a = app.auth.lock().unwrap();
        a.gh_pending.remove(&state)
    };
    let Some((acct_id, exp)) = pending else {
        return (StatusCode::BAD_REQUEST, "This GitHub install link is invalid or has expired — start the connect from your org again.").into_response();
    };
    if now() > exp || installation.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "This GitHub install link has expired.").into_response();
    }
    let handle = app.store.accounts().into_iter().find(|a| a.id == acct_id).map(|a| a.handle).unwrap_or_default();
    let reg = app.registry.clone();
    let inst = installation.clone();
    let login = tokio::task::spawn_blocking(move || reg.mirror_verify_connection(&inst)).await.ok().flatten();
    let Some(login) = login else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "Could not verify that GitHub installation.").into_response();
    };
    app.connections.set(&acct_id, connections::Connection { provider: "github".into(), installation, login, connected_unix: now() });
    // Back to the org page; the connection now shows as active.
    axum::response::Redirect::to(&format!("/orgs/{handle}?github=connected")).into_response()
}

/// `POST /api/accounts/:id/github/connect` — `{installation}`. Verifies the App installation is real
/// (returns its GitHub login) and stores it against the account. Admin only.
async fn github_connect(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id) {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let installation = body.get("installation").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if installation.is_empty() {
        return (StatusCode::BAD_REQUEST, "installation id is required").into_response();
    }
    let reg = app.registry.clone();
    let inst = installation.clone();
    let login = tokio::task::spawn_blocking(move || reg.mirror_verify_connection(&inst)).await.ok().flatten();
    let Some(login) = login else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "could not verify that installation (wrong id, or the GitHub App isn't installed / configured)").into_response();
    };
    app.connections.set(&acct.id, connections::Connection { provider: "github".into(), installation, login: login.clone(), connected_unix: now() });
    (StatusCode::CREATED, Json(json!({ "connected": true, "provider": "github", "login": login }))).into_response()
}

/// `DELETE /api/accounts/:id/github` — disconnect the account's GitHub connection (admin only).
async fn github_disconnect(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id) {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    app.connections.remove(&acct.id);
    Json(json!({ "connected": false })).into_response()
}

/// `GET /api/accounts/:id/github/importable` — repos importable via THIS account's connection. Admin
/// only, and only when the account has explicitly connected — never a global list.
async fn github_importable(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id) {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let Some(conn) = app.connections.get(&acct.id) else {
        return (StatusCode::FORBIDDEN, "this account is not connected to GitHub").into_response();
    };
    let reg = app.registry.clone();
    let repos = tokio::task::spawn_blocking(move || reg.mirror_importable(&conn.installation)).await.unwrap_or_default();
    Json(json!({ "repos": repos })).into_response()
}

/// `POST /api/accounts/:id/repos/import` — `{source, name?}`. Import a GitHub repo through the
/// account's own connection. Admin only, connection required.
async fn import_repo_handler(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id) {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let Some(conn) = app.connections.get(&acct.id) else {
        return (StatusCode::FORBIDDEN, "connect this account to GitHub first").into_response();
    };
    let source = body.get("source").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if source.is_empty() {
        return (StatusCode::BAD_REQUEST, "source (owner/name on GitHub) is required").into_response();
    }
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| source.rsplit('/').next().unwrap_or(&source).to_string());
    let tenant = acct.handle.clone();
    if app.store.repos().iter().any(|r| r.owner == acct.id && r.name == name) {
        return (StatusCode::CONFLICT, "a repo with that name already exists").into_response();
    }
    let dest = format!("{tenant}/{name}");
    // The import shells out to `git`, and it pushes back into THIS server's git endpoint — so it must
    // run off the async runtime (spawn_blocking), or it starves the workers that serve that push and
    // the whole thing deadlocks.
    let reg = app.registry.clone();
    let (inst, src_c, dst_c) = (conn.installation.clone(), source.clone(), dest.clone());
    let res = match tokio::task::spawn_blocking(move || reg.mirror_import(&inst, &src_c, &dst_c)).await {
        Ok(r) => r,
        Err(_) => hull_plugin::MirrorResult { ok: false, external_ref: None, detail: "import task failed".into() },
    };
    if !res.ok {
        return (StatusCode::UNPROCESSABLE_ENTITY, format!("import failed: {}", res.detail)).into_response();
    }
    let repo = Repo { id: format!("repo_{tenant}_{name}"), owner: acct.id.clone(), name: name.clone(), default_branch: "main".into() };
    app.store.put_repo(repo.clone());
    (StatusCode::CREATED, Json(json!({ "repo": repo, "tenant": tenant, "name": name, "detail": res.detail }))).into_response()
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
                "email": app.store.user_by_actor(&a.id).map(|u| u.email).unwrap_or_default(),
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
            // Server-signed fallback: Hull holds the signing key for (a) the demo owner and (b) any
            // hosted account (passkey users), so it can sign the delegation on their behalf — this is
            // how "delegate an agent" works from the web when the human logs in with a passkey and
            // never holds a raw key. A legacy key-login human (no stored secret) must sign client-side.
            let demo_id = identity::human_from_secret("demo", DEMO_OWNER_SECRET).map(|m| m.actor.id).unwrap_or_default();
            if parent.id == demo_id {
                match identity::mint_agent(&handle, &parent, DEMO_OWNER_SECRET, scope, lifetime) {
                    Some(m) => m,
                    None => return (StatusCode::UNPROCESSABLE_ENTITY, "could not mint — parent is not accountable").into_response(),
                }
            } else if let Some(user) = app.store.user_by_actor(&parent.id) {
                match identity::mint_agent(&handle, &parent, &user.secret_key, scope, lifetime) {
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
        Some(a) => {
            let user = app.store.user_by_actor(&a.id);
            Json(json!({
                "id": a.id, "handle": a.handle, "kind": a.kind, "accountable": a.is_accountable(),
                "username": user.as_ref().map(|u| u.username.clone()),
                "email": user.as_ref().map(|u| u.email.clone()),
            })).into_response()
        }
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
    let user = app.store.user_by_actor(&a.id);
    Json(json!({
        "id": a.id,
        "handle": a.handle,
        "kind": a.kind,
        "accountable": a.is_accountable(),
        "human_root": a.human_principal(),
        "delegation": chain,
        "memberships": memberships,
        "username": user.as_ref().map(|u| u.username.clone()),
        "email": user.as_ref().map(|u| u.email.clone()),
    }))
    .into_response()
}

/// base64url (no padding) — the encoding WebAuthn uses for credential ids.
fn b64u(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ── passkey (WebAuthn) accounts ──────────────────────────────────────────────
// Signup and login are two-step ceremonies: `/start` returns the browser challenge (and an opaque
// flow id we hold the server state under); `/finish` verifies the authenticator's response. No
// passwords ever touch Hull.

/// `POST /api/auth/register/start` — `{username, email}` → a WebAuthn creation challenge.
async fn register_start(State(app): State<App>, Json(body): Json<Value>) -> Response {
    let username = sanitize_handle(body.get("username").and_then(Value::as_str).unwrap_or(""));
    let email = body.get("email").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if username.is_empty() || email.is_empty() {
        return (StatusCode::BAD_REQUEST, "username and email are required").into_response();
    }
    if app.store.user_by_username(&username).is_some() {
        return (StatusCode::CONFLICT, "that username is taken").into_response();
    }
    let uuid = Uuid::new_v4();
    match app.webauthn.start_passkey_registration(uuid, &username, &username, None) {
        Ok((ccr, state)) => {
            let flow = identity::random_hex(16);
            app.auth.lock().unwrap().reg_flows.insert(flow.clone(), passkey::RegFlow { username, email, uuid, state });
            Json(json!({ "flow_id": flow, "options": ccr })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("could not start registration: {e}")).into_response(),
    }
}

/// `POST /api/auth/register/finish` — `{flow_id, credential}` → creates the user + a human actor Hull
/// signs for, gives them a personal account, and returns a session token.
async fn register_finish(State(app): State<App>, Json(body): Json<Value>) -> Response {
    let flow_id = body.get("flow_id").and_then(Value::as_str).unwrap_or("").to_string();
    let cred = body.get("credential").cloned().unwrap_or(Value::Null);
    let Some(flow) = app.auth.lock().unwrap().reg_flows.remove(&flow_id) else {
        return (StatusCode::BAD_REQUEST, "unknown or expired registration flow").into_response();
    };
    let reg: RegisterPublicKeyCredential = match serde_json::from_value(cred) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad credential: {e}")).into_response(),
    };
    let pk = match app.webauthn.finish_passkey_registration(&reg, &flow.state) {
        Ok(p) => p,
        Err(e) => return (StatusCode::UNAUTHORIZED, format!("passkey registration failed: {e}")).into_response(),
    };
    // The user drives a fresh human actor; Hull holds its key to sign delegations for them.
    let minted = identity::mint_human(&flow.username);
    let cred_id = b64u(pk.cred_id().as_ref());
    let user = User {
        id: flow.uuid.to_string(),
        username: flow.username.clone(),
        email: flow.email,
        actor: minted.actor.id.clone(),
        secret_key: minted.secret_key,
        passkeys: vec![PasskeyCred { id: cred_id, name: "passkey".into(), created_unix: now(), data: serde_json::to_value(&pk).unwrap_or(Value::Null) }],
        created_unix: now(),
        bio: String::new(),
    };
    app.store.put_actor(minted.actor.clone());
    app.store.put_user(user.clone());
    app.store.put_account(Account {
        id: format!("acct_{}", flow.uuid),
        kind: AccountKind::Personal,
        handle: user.username.clone(),
        members: vec![Membership { actor: user.actor.clone(), role: Role::Owner }],
    });
    let token = identity::random_hex(24);
    app.auth.lock().unwrap().tokens.insert(token.clone(), user.actor.clone());
    (StatusCode::CREATED, Json(json!({ "token": token, "actor": user.actor, "username": user.username }))).into_response()
}

/// `POST /api/auth/passkey/start` — `{username}` → a WebAuthn assertion challenge for that account.
async fn passkey_start(State(app): State<App>, Json(body): Json<Value>) -> Response {
    let username = body.get("username").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let Some(user) = app.store.user_by_username(&username) else {
        return (StatusCode::NOT_FOUND, "no account with that username").into_response();
    };
    let passkeys: Vec<Passkey> = user.passkeys.iter().filter_map(|p| serde_json::from_value(p.data.clone()).ok()).collect();
    if passkeys.is_empty() {
        return (StatusCode::BAD_REQUEST, "this account has no passkeys").into_response();
    }
    match app.webauthn.start_passkey_authentication(&passkeys) {
        Ok((rcr, state)) => {
            let flow = identity::random_hex(16);
            app.auth.lock().unwrap().auth_flows.insert(flow.clone(), passkey::AuthFlow { user_id: user.id.clone(), state });
            Json(json!({ "flow_id": flow, "options": rcr })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("could not start authentication: {e}")).into_response(),
    }
}

/// `POST /api/auth/passkey/finish` — `{flow_id, credential}` → verifies the assertion, issues a token.
async fn passkey_finish(State(app): State<App>, Json(body): Json<Value>) -> Response {
    let flow_id = body.get("flow_id").and_then(Value::as_str).unwrap_or("").to_string();
    let cred = body.get("credential").cloned().unwrap_or(Value::Null);
    let Some(flow) = app.auth.lock().unwrap().auth_flows.remove(&flow_id) else {
        return (StatusCode::BAD_REQUEST, "unknown or expired login flow").into_response();
    };
    let pkc: PublicKeyCredential = match serde_json::from_value(cred) {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad credential: {e}")).into_response(),
    };
    let res = match app.webauthn.finish_passkey_authentication(&pkc, &flow.state) {
        Ok(r) => r,
        Err(e) => return (StatusCode::UNAUTHORIZED, format!("passkey login failed: {e}")).into_response(),
    };
    let Some(mut user) = app.store.user(&flow.user_id) else {
        return (StatusCode::UNAUTHORIZED, "account no longer exists").into_response();
    };
    // Update the used credential's counter (clone/replay detection lives in the Passkey).
    for pc in user.passkeys.iter_mut() {
        if let Ok(mut pk) = serde_json::from_value::<Passkey>(pc.data.clone()) {
            if pk.cred_id() == res.cred_id() {
                pk.update_credential(&res);
                pc.data = serde_json::to_value(&pk).unwrap_or_else(|_| pc.data.clone());
            }
        }
    }
    app.store.put_user(user.clone());
    let token = identity::random_hex(24);
    app.auth.lock().unwrap().tokens.insert(token.clone(), user.actor.clone());
    Json(json!({ "token": token, "actor": user.actor, "username": user.username })).into_response()
}

/// `GET /api/account` — the signed-in user's hosted account (username, email, passkeys).
async fn account_get(State(app): State<App>, headers: axum::http::HeaderMap) -> Response {
    let Some(a) = authed_actor(&app, &headers) else { return (StatusCode::UNAUTHORIZED, "not signed in").into_response(); };
    let Some(user) = app.store.user_by_actor(&a.id) else {
        return (StatusCode::NOT_FOUND, "this actor is a legacy key login, not a hosted account").into_response();
    };
    let passkeys: Vec<Value> = user.passkeys.iter().map(|p| json!({ "id": p.id, "name": p.name, "created_unix": p.created_unix })).collect();
    Json(json!({ "username": user.username, "email": user.email, "bio": user.bio, "actor": user.actor, "passkeys": passkeys, "created_unix": user.created_unix })).into_response()
}

/// `PUT /api/account` — change username and/or email. Keeps the actor + personal-account handle in sync.
async fn account_update(State(app): State<App>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let Some(a) = authed_actor(&app, &headers) else { return (StatusCode::UNAUTHORIZED, "not signed in").into_response(); };
    let Some(mut user) = app.store.user_by_actor(&a.id) else {
        return (StatusCode::NOT_FOUND, "not a hosted account").into_response();
    };
    if let Some(un) = body.get("username").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(other) = app.store.user_by_username(un) {
            if other.id != user.id {
                return (StatusCode::CONFLICT, "that username is taken").into_response();
            }
        }
        user.username = un.to_string();
        // keep the display handle on the actor + personal account aligned with the username
        if let Some(mut actor) = app.store.actor(&user.actor) {
            actor.handle = un.to_string();
            app.store.put_actor(actor);
        }
        let acct_id = format!("acct_{}", user.id);
        if let Some(mut acct) = app.store.accounts().into_iter().find(|x| x.id == acct_id) {
            acct.handle = un.to_string();
            app.store.put_account(acct);
        }
    }
    if let Some(em) = body.get("email").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        user.email = em.to_string();
    }
    if let Some(bio) = body.get("bio").and_then(Value::as_str) {
        user.bio = bio.chars().take(280).collect();
    }
    app.store.put_user(user.clone());
    Json(json!({ "username": user.username, "email": user.email, "bio": user.bio })).into_response()
}

/// `POST /api/account/passkeys/start` — begin adding another passkey to the signed-in account.
async fn account_passkey_start(State(app): State<App>, headers: axum::http::HeaderMap) -> Response {
    let Some(a) = authed_actor(&app, &headers) else { return (StatusCode::UNAUTHORIZED, "not signed in").into_response(); };
    let Some(user) = app.store.user_by_actor(&a.id) else { return (StatusCode::NOT_FOUND, "not a hosted account").into_response(); };
    let Ok(uuid) = Uuid::parse_str(&user.id) else { return (StatusCode::INTERNAL_SERVER_ERROR, "bad user id").into_response(); };
    let exclude: Vec<_> = user.passkeys.iter().filter_map(|p| serde_json::from_value::<Passkey>(p.data.clone()).ok().map(|pk| pk.cred_id().clone())).collect();
    match app.webauthn.start_passkey_registration(uuid, &user.username, &user.username, Some(exclude)) {
        Ok((ccr, state)) => {
            let flow = identity::random_hex(16);
            app.auth.lock().unwrap().add_flows.insert(flow.clone(), passkey::AddFlow { user_id: user.id.clone(), state });
            Json(json!({ "flow_id": flow, "options": ccr })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("could not start: {e}")).into_response(),
    }
}

/// `POST /api/account/passkeys/finish` — `{flow_id, credential, name?}` → store the new passkey.
async fn account_passkey_finish(State(app): State<App>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let Some(a) = authed_actor(&app, &headers) else { return (StatusCode::UNAUTHORIZED, "not signed in").into_response(); };
    let flow_id = body.get("flow_id").and_then(Value::as_str).unwrap_or("").to_string();
    let name = body.get("name").and_then(Value::as_str).unwrap_or("passkey").trim().to_string();
    let cred = body.get("credential").cloned().unwrap_or(Value::Null);
    let Some(flow) = app.auth.lock().unwrap().add_flows.remove(&flow_id) else {
        return (StatusCode::BAD_REQUEST, "unknown or expired flow").into_response();
    };
    let Some(mut user) = app.store.user_by_actor(&a.id) else { return (StatusCode::NOT_FOUND, "not a hosted account").into_response(); };
    if user.id != flow.user_id {
        return (StatusCode::FORBIDDEN, "flow does not belong to you").into_response();
    }
    let reg: RegisterPublicKeyCredential = match serde_json::from_value(cred) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad credential: {e}")).into_response(),
    };
    let pk = match app.webauthn.finish_passkey_registration(&reg, &flow.state) {
        Ok(p) => p,
        Err(e) => return (StatusCode::UNAUTHORIZED, format!("failed: {e}")).into_response(),
    };
    user.passkeys.push(PasskeyCred {
        id: b64u(pk.cred_id().as_ref()),
        name: if name.is_empty() { "passkey".into() } else { name },
        created_unix: now(),
        data: serde_json::to_value(&pk).unwrap_or(Value::Null),
    });
    app.store.put_user(user.clone());
    let passkeys: Vec<Value> = user.passkeys.iter().map(|p| json!({ "id": p.id, "name": p.name, "created_unix": p.created_unix })).collect();
    Json(json!({ "passkeys": passkeys })).into_response()
}

/// `DELETE /api/account/passkeys/:cred` — remove a passkey (never the last one).
async fn account_passkey_delete(State(app): State<App>, headers: axum::http::HeaderMap, Path(cred): Path<String>) -> Response {
    let Some(a) = authed_actor(&app, &headers) else { return (StatusCode::UNAUTHORIZED, "not signed in").into_response(); };
    let Some(mut user) = app.store.user_by_actor(&a.id) else { return (StatusCode::NOT_FOUND, "not a hosted account").into_response(); };
    if user.passkeys.len() <= 1 {
        return (StatusCode::BAD_REQUEST, "cannot remove your only passkey").into_response();
    }
    let before = user.passkeys.len();
    user.passkeys.retain(|p| p.id != cred);
    if user.passkeys.len() == before {
        return (StatusCode::NOT_FOUND, "no such passkey").into_response();
    }
    app.store.put_user(user.clone());
    let passkeys: Vec<Value> = user.passkeys.iter().map(|p| json!({ "id": p.id, "name": p.name, "created_unix": p.created_unix })).collect();
    Json(json!({ "passkeys": passkeys })).into_response()
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

/// Branch names for a repo (`GET /api/repos/:tenant/:repo/branches`).
async fn repo_branches(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>) -> Json<Value> {
    Json(json!({ "branches": app.repos.branches(&tenant, &repo) }))
}

/// A directory listing at a branch (`GET /api/repos/:tenant/:repo/tree?ref=<branch>&path=<dir>`).
async fn repo_tree(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let ref_name = q.get("ref").map(String::as_str).filter(|s| !s.is_empty()).unwrap_or("main");
    // `?flat=1` → every file path in the branch (for the full file-tree view).
    if q.get("flat").is_some_and(|v| v == "1" || v == "true") {
        return Json(json!({ "ref": ref_name, "paths": app.repos.all_paths(&tenant, &repo, ref_name) }));
    }
    let path = q.get("path").map(String::as_str).unwrap_or("");
    Json(json!({ "ref": ref_name, "path": path, "entries": app.repos.list_tree(&tenant, &repo, ref_name, path) }))
}

/// A file's contents at a branch (`GET /api/repos/:tenant/:repo/blob?ref=<branch>&path=<file>`).
/// Returns text when decodable, plus a `binary` flag and byte size so the UI can render sensibly.
async fn repo_blob(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let ref_name = q.get("ref").map(String::as_str).filter(|s| !s.is_empty()).unwrap_or("main");
    let path = q.get("path").map(String::as_str).unwrap_or("");
    match app.repos.read_file_at(&tenant, &repo, ref_name, path) {
        Some(bytes) => {
            let binary = bytes.iter().take(8000).any(|&b| b == 0);
            let size = bytes.len();
            let text = if binary { String::new() } else { String::from_utf8_lossy(&bytes).into_owned() };
            Json(json!({ "path": path, "ref": ref_name, "size": size, "binary": binary, "text": text }))
        }
        None => Json(json!({ "path": path, "ref": ref_name, "missing": true })),
    }
}

/// The codebase import graph at a branch (`GET /api/repos/:tenant/:repo/graph?ref=<branch>`) —
/// nodes are source files, edges are resolved in-repo imports.
async fn repo_graph(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(resp) = require_repo_read(&app, &headers, &tenant, &repo) {
        return resp;
    }
    let ref_name = q.get("ref").map(String::as_str).filter(|s| !s.is_empty()).unwrap_or("main");
    let (nodes, edges) = app.repos.code_graph(&tenant, &repo, ref_name);
    Json(json!({ "ref": ref_name, "nodes": nodes, "edges": edges })).into_response()
}

/// Fuzzy filename + full-text content search (`GET /api/repos/:tenant/:repo/search?q=<query>&ref=<branch>`).
async fn repo_search(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let ref_name = q.get("ref").map(String::as_str).filter(|s| !s.is_empty()).unwrap_or("main");
    let query = q.get("q").map(String::as_str).unwrap_or("");
    Json(json!({ "q": query, "ref": ref_name, "hits": app.repos.search(&tenant, &repo, ref_name, query) }))
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
/// Extract `@handle` mentions from free text (handles allow `_ - :`, e.g. `@agent:reviewer`).
fn parse_mentions(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        if let Some(rest) = word.strip_prefix('@') {
            let h: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == ':').collect();
            if !h.is_empty() && !out.contains(&h) {
                out.push(h);
            }
        }
    }
    out
}

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
    // In-repo `.hull/CODEOWNERS` (GitHub-style: `<pattern> @handle …`), so owners live with the code,
    // not only in the UI. Handles resolve to accountable actors; unknown handles are ignored.
    if let Some((tenant, repo)) = repo_key.split_once('/') {
        if let Some(bytes) = app.repos.read_file(tenant, repo, ".hull/CODEOWNERS") {
            let text = String::from_utf8_lossy(&bytes);
            let actors = app.store.actors();
            for line in text.lines() {
                let line = line.split('#').next().unwrap_or("").trim();
                if line.is_empty() {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let Some(glob) = parts.next() else { continue };
                if files.iter().any(|f| hull_core::store::glob_match(glob, f)) {
                    for owner in parts {
                        let handle = owner.trim_start_matches('@');
                        if let Some(a) = actors.iter().find(|a| a.handle == handle) {
                            if !set.contains(&a.id) {
                                set.push(a.id.clone());
                            }
                        }
                    }
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

/// Full old + new text of one file at a change (`GET …/change/:id/file?path=…`). The diff viewer
/// calls this to *expand unmodified lines* — the patch alone only carries the hunks' context, so
/// pierre needs the whole file to reveal the rest. `null` on either side = a pure add or delete.
async fn change_file(
    State(app): State<App>,
    Path((tenant, repo, id)): Path<(String, String, String)>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(path) = q.get("path").map(|s| s.as_str()).filter(|s| !s.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "path is required").into_response();
    };
    match app.repos.file_pair(&tenant, &repo, &id, path) {
        Some((old, new)) => Json(json!({ "path": path, "old": old, "new": new })).into_response(),
        None => (StatusCode::NOT_FOUND, "no such file in this change").into_response(),
    }
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
    // The scratch path must be unique **per request**, not per (tree, pid): two concurrent fetches of
    // the same tree — an ordinary occurrence once a CI shards a job or a re-check races a first
    // dispatch — would otherwise share a directory and `remove_dir_all` each other's checkout out
    // from under the tar writer. A process-local counter is enough and stays deterministic (this
    // runtime has no RNG).
    static ARCHIVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = ARCHIVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "hull-tree-{}-{}-{seq}",
        &tree[..tree.len().min(16)],
        std::process::id()
    ));
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
                // **Must be false.** `tar::Builder` follows symlinks by default, which would pack a
                // link as a *copy of its target's contents*. keel addresses a symlink as
                // `MODE_SYMLINK` over a blob holding the target path, so a followed link changes the
                // tree's content address: the archive could never re-hash to `tree`, and every change
                // touching a symlink would fail verification — permanently, since `errored` is not
                // memoized and each re-check would fail the same way. A dangling link is worse still:
                // `append_dir_all` errors outright and the endpoint 500s.
                ar.follow_symlinks(false);
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
    // C1 — fold in the acceptance criteria of any issue a PR proposing this change closes, so the
    // standalone ledger matches what the review reconciled against.
    let key = format!("{tenant}/{repo}");
    let review_intent = {
        let issues = app.store.issues(&key);
        let acceptance: Vec<String> = app
            .store
            .prs(&key)
            .into_iter()
            .filter(|p| p.changes.contains(&id))
            .flat_map(|p| closing_issue_numbers(&p.title, &[]))
            .filter_map(|n| issues.iter().find(|i| i.number == n).map(|i| format!("Closes #{}: {}. {}", i.number, i.title, i.body)))
            .collect();
        if acceptance.is_empty() { info.intent.clone() } else { format!("{}\n{}", info.intent, acceptance.join("\n")) }
    };
    let ledger = hull_core::reconcile::reconcile(&id, &review_intent, &lesson, &facts);
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
            // Auto-triage a failed check: an independent agent reviews the failing change and posts
            // findings, so a red result isn't a dead end. Runs in the background; the memoized red
            // verdict makes the agent's own check run instant.
            if matches!(o.status, hull_plugin::CiStatus::Red) {
                let key = format!("{tenant}/{repo}");
                if let Some(pr) = app.store.prs(&key).into_iter().find(|p| p.changes.iter().any(|c| c.starts_with(&id) || id.starts_with(c.as_str()))) {
                    if let Some(agent) = independent_agent_reviewer(&app, &pr.author) {
                        let (app2, t2, r2, n2) = (app.clone(), tenant.clone(), repo.clone(), pr.number);
                        tokio::spawn(async move { let _ = perform_auto_review(&app2, &t2, &r2, n2, &agent, 0).await; });
                    }
                }
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
    let force = _body.get("force").and_then(Value::as_bool).unwrap_or(false);
    match perform_merge(&app, &tenant, &repo, number, &actor, force).await {
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
    force: bool,
) -> Result<(PullRequest, Vec<u64>), (StatusCode, String)> {
    let key = format!("{tenant}/{repo}");
    let Some(mut pr) = app.store.prs(&key).into_iter().find(|p| p.number == number) else {
        return Err((StatusCode::NOT_FOUND, "no such PR".into()));
    };
    if pr.state == PrState::Merged {
        return Err((StatusCode::CONFLICT, "already merged".into()));
    }
    // An owner/admin may override the gate (merge despite red/unrun checks or no approval) — the
    // human-admin escape hatch for a wedged or misconfigured check.
    let override_ok = force && is_repo_admin(app, tenant, repo, actor.id.as_str());
    // green keel verification of the proposed change
    let green = pr
        .changes
        .first()
        .and_then(|c| app.repos.verification(tenant, repo, c))
        .map(|v| v == "green")
        .unwrap_or(false);
    if !green && !override_ok {
        return Err((StatusCode::CONFLICT, "cannot merge: change is not keel-verify green".into()));
    }
    // Independent approving reviews (approver != PR author unless the repo allows self-approval),
    // split by actor kind.
    let allow_self = app.repo_settings.get(&key).allow_self_approve;
    let approvals: Vec<ActorId> = app
        .store
        .reviews(&key)
        .into_iter()
        .filter(|r| r.target == format!("pr:{number}") && r.verdict == Verdict::Approve && (allow_self || r.reviewer != pr.author))
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
    let (contradicted, phantom) = {
        let lesson = app.store.session_record(&key, &change).map(|s| s.lesson).unwrap_or_default();
        let intent = app.repos.change_info(tenant, repo, &change).map(|i| i.intent).unwrap_or_default();
        let facts = facts_with_independence(app, tenant, repo, &change).await;
        let ledger = hull_core::reconcile::reconcile(&change, &intent, &lesson, &facts);
        (ledger.contradicted() > 0, ledger.phantom() > 0)
    };
    // A change that did work its narrative never claimed (C5 phantom work) is NOT low-risk: it
    // includes unreviewed operations, so an agent's approve must not auto-merge it — a human looks.
    let low_risk = !protected && !contradicted && !phantom; // green is already required above
    let agent_approve_counts = match eff.tier {
        hull_core::AutonomyTier::T0 | hull_core::AutonomyTier::T1 => false,
        hull_core::AutonomyTier::T2 => low_risk,
        hull_core::AutonomyTier::T3 => !protected, // protected paths ALWAYS need a human (D11)
    };
    let approved = human_approval || (agent_approval && agent_approve_counts);
    if !approved && !override_ok {
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
/// Constant-time byte compare (don't leak signature validity via timing).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The **GitHub App push webhook** (`POST …/mirror/github`) — the inbound half of two-way mirroring.
/// GitHub calls this on every push. We verify the App's `X-Hub-Signature-256` HMAC, then (for
/// `main`, non-duplicate, not our own echoed-back push) fetch + bridge the forge's git into keel,
/// with the committer's forge login mapped to a hull actor (NEW-1176) and origin stamped `github`
/// so it never loops back out (NEW-1173).
async fn mirror_github_webhook(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // Auth: HMAC-SHA256 of the raw body with the App's webhook secret. No secret ⇒ endpoint disabled.
    let Some(secret) = app.registry.config("GITHUB_WEBHOOK_SECRET") else {
        return (StatusCode::NOT_IMPLEMENTED, "no GitHub webhook secret configured").into_response();
    };
    let sig = headers.get("X-Hub-Signature-256").and_then(|v| v.to_str().ok()).unwrap_or("");
    let expected = {
        use hmac::{Hmac, Mac};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
        mac.update(&body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    };
    if !ct_eq(sig.as_bytes(), expected.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, "invalid webhook signature").into_response();
    }
    // GitHub pings the endpoint with a `ping` event on setup — acknowledge it.
    if headers.get("X-GitHub-Event").and_then(|v| v.to_str().ok()) == Some("ping") {
        return Json(json!({ "pong": true })).into_response();
    }
    let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let git_ref = v.get("ref").and_then(Value::as_str).unwrap_or("");
    let after = v.get("after").and_then(Value::as_str).unwrap_or("").to_string();
    let login = v.get("sender").and_then(|s| s.get("login")).and_then(Value::as_str).unwrap_or("").to_string();
    let git_author = v.get("pusher").and_then(|p| p.get("name")).and_then(Value::as_str).unwrap_or("mirror").to_string();
    let delivery = headers.get("X-GitHub-Delivery").and_then(|d| d.to_str().ok()).unwrap_or(&after).to_string();

    if git_ref != "refs/heads/main" {
        return Json(json!({ "ignored": git_ref })).into_response();
    }
    // Idempotency: GitHub redelivers; a repeat delivery is a no-op.
    if !app.mirror.mark_processed(&format!("in:{delivery}")) {
        return Json(json!({ "duplicate": true, "change": after })).into_response();
    }
    // Loop prevention: if this commit originated on hull (our own outbound push echoed back), don't
    // re-import it.
    if app.mirror.origin(&after).as_deref() == Some("hull") {
        return Json(json!({ "loop_skipped": after })).into_response();
    }
    app.mirror.set_origin(&after, "github");

    // Import: fetch the forge's git and bridge into keel (off the async runtime — it shells git).
    let key = format!("{tenant}/{repo}");
    let result = tokio::task::spawn_blocking({
        let (registry, key2) = (app.registry.clone(), key.clone());
        move || registry.mirror_pull_in(&key2)
    })
    .await
    .unwrap_or(hull_plugin::MirrorResult { ok: false, external_ref: None, detail: "import task panicked".into() });

    // Accountability mapping across the mirror (NEW-1176): resolve the committer's forge login.
    let attributed = if login.is_empty() { None } else { app.mirror.resolve_github(&login) };
    app.mirror.record_inbound(mirror::Inbound {
        repo: key,
        change: after.clone(),
        external_id: delivery,
        git_author,
        github_login: login,
        attributed_actor: attributed.clone(),
        ts: now(),
    });
    (if result.ok { StatusCode::OK } else { StatusCode::BAD_GATEWAY }, Json(json!({
        "imported": result.ok, "change": after, "detail": result.detail, "origin": "github",
        "attributed_actor": attributed,
    }))).into_response()
}

/// Manually push a repo's current HEAD to its configured mirror (`POST …/mirror/push`) — an initial
/// sync / "sync now" for an accountable actor, independent of the on-land trigger. Bypasses the
/// per-change idempotency guard so a re-sync always runs; loop-origin is still recorded.
async fn mirror_push_now(State(app): State<App>, headers: axum::http::HeaderMap, Path((tenant, repo)): Path<(String, String)>) -> Response {
    let key = format!("{tenant}/{repo}");
    if let Err(resp) = require_actor(&app, &headers, "") {
        return resp;
    }
    let Some(target) = app.registry.mirror_target(&key) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "no mirror target configured for this repo").into_response();
    };
    let Some(change) = app.repos.head_change(&tenant, &repo) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "repo has no HEAD change to push").into_response();
    };
    let (intent, author) = app.repos.change_info(&tenant, &repo, &change).map(|i| (i.intent, i.author)).unwrap_or_default();
    let result = tokio::task::spawn_blocking({
        let (registry, key, change) = (app.registry.clone(), key.clone(), change.clone());
        move || registry.mirror_push(&hull_plugin::MirrorPush { repo: key, change, intent, author })
    })
    .await
    .unwrap_or(hull_plugin::MirrorResult { ok: false, external_ref: None, detail: "mirror task panicked".into() });
    if result.ok {
        app.mirror.set_origin(&change, "hull");
        // Also stamp the pushed git sha (external_ref) as hull-origin — that's the key the inbound
        // webhook sees (`after`), so our own push echoing back is recognized and not re-imported.
        if let Some(sha) = &result.external_ref {
            app.mirror.set_origin(sha, "hull");
        }
        app.mirror.record_outbound(mirror::Outbound { repo: key, change: change.clone(), target, external_ref: result.external_ref.clone().unwrap_or_default(), ts: now() });
    }
    (if result.ok { StatusCode::OK } else { StatusCode::BAD_GATEWAY }, Json(json!({ "ok": result.ok, "change": change, "detail": result.detail }))).into_response()
}

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
        // Stamp the pushed git sha as hull-origin too (the loop key the inbound webhook's `after` uses).
        if let Some(sha) = &result.external_ref {
            app.mirror.set_origin(sha, "hull");
        }
        app.mirror.record_outbound(mirror::Outbound {
            repo: key,
            change: change.to_string(),
            target: target.clone(),
            external_ref: result.external_ref.clone().unwrap_or_default(),
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
async fn reviews(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo) {
        return r;
    }
    Json(json!({ "reviews": app.store.reviews(&format!("{tenant}/{repo}")) })).into_response()
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
    let path = body.get("path").and_then(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let line = body.get("line").and_then(Value::as_u64).map(|n| n as u32);
    let line_end = body.get("line_end").and_then(Value::as_u64).map(|n| n as u32).unwrap_or(line.unwrap_or(0));
    let ask_ai = body.get("ask_ai").and_then(Value::as_bool).unwrap_or(false);
    let comment = Comment {
        id: format!("cm_{}_{}", key.replace('/', "_"), count + 1),
        repo: key.clone(),
        target: target.clone(),
        author: author.id.clone(),
        body: text,
        created_unix: now(),
        path,
        line,
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
    // @mentions in a comment add the mentioned actor as a reviewer (on a PR) or assignee (on an issue).
    let mentioned = parse_mentions(&comment.body);
    if !mentioned.is_empty() {
        let actors = app.store.actors();
        let ids: Vec<String> = mentioned.iter().filter_map(|h| actors.iter().find(|a| &a.handle == h).map(|a| a.id.clone())).collect();
        if let Some(num) = target.strip_prefix("pr:").and_then(|s| s.parse::<u64>().ok()) {
            if let Some(mut pr) = app.store.prs(&key).into_iter().find(|p| p.number == num) {
                let mut added = vec![];
                for id in &ids {
                    if id != &pr.author && !pr.reviewers.contains(id) {
                        pr.reviewers.push(id.clone());
                        added.push(id.clone());
                    }
                }
                if !added.is_empty() {
                    app.store.replace_pr(pr.clone());
                    app.registry.notify(&NotifyEvent { kind: "review_requested".into(), to: added, summary: format!("{} mentioned you as a reviewer on PR !{num}", author.handle), change: pr.changes.first().cloned() });
                }
            }
        } else if let Some(num) = target.strip_prefix("issue:").and_then(|s| s.parse::<u64>().ok()) {
            if let Some(mut issue) = app.store.issues(&key).into_iter().find(|i| i.number == num) {
                let mut added = vec![];
                for id in &ids {
                    if !issue.assignees.contains(id) {
                        issue.assignees.push(id.clone());
                        added.push(id.clone());
                    }
                }
                if !added.is_empty() {
                    app.store.replace_issue(issue.clone());
                    app.registry.notify(&NotifyEvent { kind: "issue_assigned".into(), to: added, summary: format!("{} mentioned you on issue #{num}", author.handle), change: None });
                }
            }
        }
    }
    // "Comment & ask agent": hand the code around this comment to the AI reviewer and post its reply
    // inline, authored by the agent reviewer. Best-effort — a failure never fails the human comment.
    if ask_ai && app.registry.has_reviewer() {
        if let (Some(p), Some(ln)) = (comment.path.clone(), line) {
            if let Some(num) = target.strip_prefix("pr:").and_then(|s| s.parse::<u64>().ok()) {
                if let Some(reply) = ai_answer_comment(&app, &key, num, &p, ln, line_end, &comment.body).await {
                    app.store.put_comment(reply);
                }
            }
        }
    }
    (StatusCode::CREATED, Json(json!({ "comment": comment }))).into_response()
}

/// Delete a comment (`DELETE …/comments/:id`). Only the comment's **author** or a repo **owner/admin**
/// may delete it — you can't erase someone else's words.
async fn delete_comment(State(app): State<App>, Path((tenant, repo, id)): Path<(String, String, String)>, headers: axum::http::HeaderMap) -> Response {
    let key = format!("{tenant}/{repo}");
    let actor = match require_actor(&app, &headers, "") {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let Some(comment) = app.store.comments(&key).into_iter().find(|c| c.id == id) else {
        return (StatusCode::NOT_FOUND, "no such comment").into_response();
    };
    if comment.author != actor.id && !is_repo_admin(&app, &tenant, &repo, &actor.id) {
        return (StatusCode::FORBIDDEN, "only the comment's author or a repo owner/admin can delete it").into_response();
    }
    let removed = app.store.remove_comment(&key, &id);
    Json(json!({ "deleted": removed, "id": id })).into_response()
}

/// Build the code context around a commented line, ask the AI reviewer, and return its reply as a
/// [`Comment`] authored by the agent reviewer (anchored to the same line). `None` if anything is
/// missing (no change, no reviewer actor, model declined).
async fn ai_answer_comment(app: &App, key: &str, pr_num: u64, path: &str, line: u32, line_end: u32, question: &str) -> Option<Comment> {
    let (tenant, repo) = key.split_once('/')?;
    let pr = app.store.prs(key).into_iter().find(|p| p.number == pr_num)?;
    let change = pr.changes.first().cloned()?;
    // An accountable agent to author the reply — the org's reviewer, never the PR author.
    let reviewer = app.store.actors().into_iter().find(|a| a.kind == hull_core::ActorKind::Agent && a.handle == "agent:reviewer" && a.id != pr.author)
        .or_else(|| app.store.actors().into_iter().find(|a| a.kind == hull_core::ActorKind::Agent && a.id != pr.author && accountable(app, a).is_ok()))?;
    // Code context from the change's diff hunks (the change is content-addressed, not a git ref):
    // walk the file's hunks tracking the NEW line number and keep the referenced span plus ~13 lines
    // of surrounding context, marking added lines with '+'.
    let (lo, hi) = (line.saturating_sub(13), line_end.max(line) + 13);
    let file = app.repos.diff(tenant, repo, &change).into_iter().find(|f| f.path == path)?;
    let mut code = String::new();
    for h in &file.hunks {
        let mut n = h.new_start as u32;
        for l in &h.lines {
            if l.tag == "del" { continue; }
            if n >= lo && n <= hi {
                let sign = if l.tag == "add" { "+" } else { " " };
                code.push_str(&format!("{n:>5} {sign} {}\n", l.text));
            }
            n += 1;
        }
    }
    if code.trim().is_empty() { return None; }
    let (cred, _bundle) = resolve_ai_credential(app, key, None).await;
    let req = hull_plugin::AskRequest {
        repo: key.to_string(), path: path.to_string(), line, line_end: line_end.max(line), code, question: question.to_string(),
        ai_credential: cred,
    };
    let app2 = app.clone();
    // `_bundle` (the decrypted per-user bundle, if any) stays alive across the call, then wipes.
    let answer = tokio::task::spawn_blocking(move || app2.registry.answer(&req)).await.ok().flatten()?;
    let count = app.store.comments(key).len();
    Some(Comment {
        id: format!("cm_{}_{}", key.replace('/', "_"), count + 1),
        repo: key.to_string(),
        target: format!("pr:{pr_num}"),
        author: reviewer.id,
        body: answer,
        created_unix: now(),
        path: Some(path.to_string()),
        line: Some(line),
    })
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

/// Round-robin cursor per owner for rotating across its AI connections (process-local; rotation just
/// spreads load, so it need not survive a restart).
fn next_ai_index(owner: &str, n: usize) -> usize {
    use std::sync::OnceLock;
    static C: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    let m = C.get_or_init(|| Mutex::new(HashMap::new()));
    let mut g = m.lock().unwrap();
    let e = g.entry(owner.to_string()).or_insert(0);
    let i = *e % n.max(1);
    *e = e.wrapping_add(1);
    i
}

/// Resolve which AI backend to use for a request touching `repo`: the repo's owning org's connected
/// credentials first, else the `fallback_actor`'s personal-account connections. With rotation on for
/// the owner, cycle across its connections; else use the first. `None` → the process-configured default.
async fn resolve_ai_credential(app: &App, repo: &str, fallback_actor: Option<&str>) -> (Option<hull_plugin::AiCredential>, Option<agentsession::BundleGuard>) {
    let tenant = repo.split_once('/').map(|(t, _)| t).unwrap_or(repo);
    let mut owner: Option<String> = None;
    let mut conns: Vec<hull_core::AiConnection> = Vec::new();
    if let Some(org) = app.store.accounts().into_iter().find(|a| a.handle == tenant) {
        let c = app.store.ai_connections(&org.id);
        if !c.is_empty() { owner = Some(org.id.clone()); conns = c; }
    }
    if conns.is_empty() {
        if let Some(aid) = fallback_actor {
            if let Some(pa) = app.store.accounts().into_iter().find(|a| a.kind == hull_core::AccountKind::Personal && a.members.iter().any(|m| m.actor == aid)) {
                let c = app.store.ai_connections(&pa.id);
                if !c.is_empty() { owner = Some(pa.id.clone()); conns = c; }
            }
        }
    }
    let Some(owner) = owner else { return (None, None) };
    let idx = if app.store.ai_rotate(&owner) { next_ai_index(&owner, conns.len()) } else { 0 };
    let c = &conns[idx % conns.len()];
    let (agent_cli, agent_config_dir, agent_token, guard) = match &c.auth {
        hull_core::AiAuth::AgentCli { command, session, .. } if !session.is_empty() => {
            // Per-user session: decrypt this user's bundle into a throwaway dir the CLI runs against;
            // the guard wipes it after. If it won't open, run no agent (degrade to the default
            // reviewer) rather than silently using the wrong identity (the host login).
            match agentsession::open(session).await {
                Ok(g) => {
                    let d = g.dir_string();
                    // A Claude bundle holds a captured OAuth token → run via CLAUDE_CODE_OAUTH_TOKEN, no
                    // config dir. A Codex bundle IS the config dir (CODEX_HOME).
                    match agentsession::read_oauth_token(std::path::Path::new(&d)) {
                        Some(tok) => (Some(command.clone()), None, tok, Some(g)),
                        None => (Some(command.clone()), Some(d), String::new(), Some(g)),
                    }
                }
                Err(e) => {
                    eprintln!("hull: agent bundle for session {session} won't open ({e}); skipping agent backend");
                    return (None, None);
                }
            }
        }
        // Host-login agent (empty session): no override — the CLI uses the Hull host's own login.
        hull_core::AiAuth::AgentCli { command, .. } => (Some(command.clone()), None, String::new(), None),
        _ => (None, None, c.auth.bearer().to_string(), None),
    };
    let token = if agent_cli.is_some() { agent_token } else { c.auth.bearer().to_string() };
    (Some(hull_plugin::AiCredential { provider: c.provider.clone(), base_url: c.base_url.clone(), token, agent_cli, agent_config_dir, connection_id: Some(c.id.clone()) }), guard)
}

/// Default command for an agent kind.
fn agent_command(kind: &str) -> Option<&'static str> {
    match kind {
        "claude-code" => Some("claude"),
        "codex" => Some("codex"),
        _ => None,
    }
}

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
    // C1 — claim extraction from the issue's acceptance criteria: when this PR closes an issue, fold
    // that issue's title + body into the reviewed narrative, so the reconciliation verifies the change
    // against **what the issue asked for**, not just what the change's own message claims.
    let review_intent = {
        let issues = app.store.issues(&key);
        let acceptance: Vec<String> = closing_issue_numbers(&pr.title, &[])
            .into_iter()
            .filter_map(|n| issues.iter().find(|i| i.number == n))
            .map(|i| format!("Closes #{}: {}. {}", i.number, i.title, i.body))
            .collect();
        if acceptance.is_empty() { info.intent.clone() } else { format!("{}\n{}", info.intent, acceptance.join("\n")) }
    };
    let facts = facts_with_independence(app, tenant, repo, &change).await;
    let tree = app.repos.change_tree(tenant, repo, &change).unwrap_or_default();
    let source_url = format!("{}/api/repos/{tenant}/{repo}/tree/{tree}/tar", app.public_url.trim_end_matches('/'));
    // Capture the reviewer's INPUTS for the audit artifact before `facts` moves into the request.
    let artifact_inputs = json!({
        "intent": review_intent, "author": info.author, "author_model": author_model,
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
        let ledger = hull_core::reconcile::reconcile(&change, &review_intent, &lesson, &facts);
        let n = semantic.moves.len();
        (Verdict::Approve, Vec::new(), Some(ledger), format!("pure move — {n} file{} relocated with byte-identical content (verified by content address); no behavioral review needed", if n == 1 { "" } else { "s" }), false)
    } else {
    let (cred, _bundle) = resolve_ai_credential(&app, &key, Some(&pr.author)).await;
    let usage_conn = cred.as_ref().and_then(|c| c.connection_id.clone());
    let review_req = hull_plugin::ReviewRequest {
        repo: key.clone(),
        change: change.clone(),
        intent: review_intent.clone(),
        lesson,
        author: info.author.clone(),
        author_model,
        source_url,
        facts,
        ai_credential: cred,
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
            // Meter this run's token usage against the connection that served it.
            if let (Some(cid), Some(u)) = (&usage_conn, &pkg.usage) {
                app.store.add_ai_usage(cid, u.input_tokens, u.output_tokens, u.cost_micros, now());
            }
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
    // "Put the claims through AI to cut down the numbers": an independent agent just read the diff
    // (the AI reviewer). Every intent claim it did NOT contradict or ask changes on is no longer an
    // open question for a human — resolve those needs-judgment claims as verified, attributed to the
    // agent (accountable; a human can still override). A request-changes/reject verdict, or a
    // contradicted claim, leaves the real problems standing. So after a triage the reviewer sees only
    // what genuinely remains, not a wall of mechanically-extracted intent restatements.
    if reviewer.kind == hull_core::ActorKind::Agent && !matches!(verdict, Verdict::RequestChanges | Verdict::Reject) {
        if let Some(l) = &ledger {
            for c in l.claims.iter().filter(|c| c.status == hull_core::reconcile::ClaimStatus::NeedsJudgment) {
                app.claims.set(&key, &change, &c.id, claims::ClaimResolution {
                    by: reviewer.id.clone(),
                    judgment: "verified".into(),
                    note: format!("verified by {}", reviewer.handle),
                    ts: now(),
                });
            }
        }
    }
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
        match perform_merge(app, tenant, repo, number, reviewer, false).await {
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
    let (cred, _bundle) = resolve_ai_credential(&app, &key, Some(&pr.author)).await;
    let usage_conn = cred.as_ref().and_then(|c| c.connection_id.clone());
    let req = hull_plugin::FixRequest {
        repo: key.clone(),
        change,
        source_url,
        path: path.to_string(),
        note: note.to_string(),
        severity: severity.to_string(),
        ai_credential: cred,
    };
    let change = req.change.clone();
    let registry = app.registry.clone();
    // `_bundle` (decrypted per-user bundle, if any) stays alive across the fixer call, then wipes.
    let res = tokio::task::spawn_blocking(move || registry.fix(&req)).await.ok()??;
    if let (Some(cid), Some(u)) = (&usage_conn, &res.usage) {
        app.store.add_ai_usage(cid, u.input_tokens, u.output_tokens, u.cost_micros, now());
    }
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
                    path: None,
                    line: None,
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
                    path: None,
                    line: None,
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
async fn prs(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo) {
        return r;
    }
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
    Json(json!({ "prs": list })).into_response()
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
    let changes: Vec<String> = if let Some(commit) = body.get("commit").and_then(Value::as_str) {
        // Open a voyage from a pushed branch's HEAD commit (resolved to its bridged keel change).
        app.repos.change_for_commit(&tenant, &repo, commit).into_iter().collect()
    } else if let Some(arr) = body.get("changes").and_then(Value::as_array) {
        arr.iter().filter_map(Value::as_str).map(str::to_string).collect()
    } else {
        app.repos.head_change(&tenant, &repo).into_iter().collect()
    };
    if changes.is_empty() {
        return (StatusCode::UNPROCESSABLE_ENTITY, "no changes to propose (unknown commit or empty repo?)").into_response();
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
    // Code owners plus any repo-configured default reviewers are auto-requested on the new voyage.
    let mut reviewers = owners.clone();
    for r in app.repo_settings.get(&key).default_reviewers {
        if !reviewers.contains(&r) {
            reviewers.push(r);
        }
    }
    let pr = PullRequest {
        id: format!("pr_{}_{number}", key.replace('/', "_")),
        repo: key,
        number,
        title,
        author: actor.id,
        changes,
        verification: Verification::Unverified,
        reviewers: reviewers.clone(),
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
async fn issues(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo) {
        return r;
    }
    Json(json!({ "issues": app.store.issues(&format!("{tenant}/{repo}")) })).into_response()
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
    let mut assignees: Vec<String> = body
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
    // @mentions in the title or body also assign the mentioned actor.
    let mention_text = format!("{} {}", title, body.get("body").and_then(Value::as_str).unwrap_or(""));
    let actors = app.store.actors();
    for h in parse_mentions(&mention_text) {
        if let Some(a) = actors.iter().find(|a| a.handle == h && a.is_accountable()) {
            if !assignees.contains(&a.id) {
                assignees.push(a.id.clone());
            }
        }
    }
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
        labels: body.get("labels").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default(),
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
    // Scoped to the caller's accounts (`?accounts=a,b,c`), falling back to a single `?tenant=` for
    // compatibility. Empty → no events (a logged-out viewer sees nothing personal).
    let tenants: Vec<String> = q
        .get("accounts")
        .map(|s| s.split(',').filter(|x| !x.is_empty()).map(str::to_string).collect())
        .or_else(|| q.get("tenant").map(|t| vec![t.clone()]))
        .unwrap_or_default();
    let stream = BroadcastStream::new(app.hub.subscribe()).filter_map(move |ev| {
        let te = ev.ok()?;
        if !tenants.contains(&te.tenant) {
            return None; // not one of this subscriber's accounts
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
