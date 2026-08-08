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
pub mod ci_sandbox;
pub mod connections;
pub mod claims;
pub mod reviewcache;
pub mod ingress;
pub mod jsonstore;
pub mod keeld;
pub mod mirror;
pub mod nostr;
pub mod observability;
pub mod passkey;
pub mod reposettings;
pub mod plugins;
pub mod quic;
pub mod repos;

/// Convenience re-export: the CI sandbox helper dispatch, called as the first line of `main` (and by
/// a hosted binary's `main`) so a `ci-sandbox` re-exec is intercepted before the runtime boots.
pub use ci_sandbox::dispatch_if_invoked;

use activity::{ActivityEvent, ActivityHub};
use axum::{
    extract::{Path, Query, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, Sse},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use hull_plugin::{NotifyEvent, Notifier};
use std::collections::HashMap;
use std::sync::Mutex;
use futures::stream::Stream;
use hull_core::store::{FileStore, InMemory, PostgresStore, Store};
use hull_core::*;
use hull_plugin::Registry;
use serde_json::{json, Value};
use webauthn_rs::prelude::{Passkey, PublicKeyCredential, RegisterPublicKeyCredential, Uuid};
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

// ── HTTP hardening knobs ──────────────────────────────────────────────────────────────────────────
/// Max time any single non-streaming API request may run before it's aborted (408). The SSE feed,
/// git smart-HTTP, and the tar download are exempt (they legitimately run long / stream).
const REQUEST_TIMEOUT_SECS: u64 = 30;
/// Timeout for the handful of long **synchronous** handlers that `.await` a real agent-CLI or
/// local-CI subprocess inline and return the result in the HTTP response (auto-review, fix-finding,
/// run-check, and the LLM "ask agent" path of create-comment). These legitimately run for minutes, so
/// the 30s cap would 408 them mid-run. 600s sits comfortably above both wall-clocks: `HULL_CI_TIMEOUT`
/// (default 600s) and the agent-CLI timeout (~180s). They aren't streams — just slow — so they keep
/// the body-cap and concurrency layers.
const SLOW_REQUEST_TIMEOUT_SECS: u64 = 600;
/// Request-body cap for the normal JSON API surface. git push (packfiles) and the tar endpoint are
/// exempt — they carry legitimately large bodies. A few MiB is ample for every JSON handler.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Global cap on concurrently in-flight (non-streaming) requests — coarse backpressure so a flood
/// can't spawn unbounded work. High enough to never bind a single-user dogfood.
const MAX_CONCURRENT_REQUESTS: usize = 1024;
/// Outbound-HTTP timeouts (CI dispatch / mirror / GitHub) so a hung endpoint can't pin a task.
const HTTP_CONNECT_TIMEOUT_SECS: u64 = 10;
const HTTP_REQUEST_TIMEOUT_SECS: u64 = 30;
/// Count of handler panics caught by the panic guard (observability only).
static PANIC_COUNT: AtomicU64 = AtomicU64::new(0);

/// One shared, timeout-bounded outbound HTTP client, built once and cloned (a `reqwest::Client` is
/// internally ref-counted, so clones share the pool). Without connect + read timeouts a hung CI or
/// GitHub endpoint would pin the calling task forever.
fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(HTTP_CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(HTTP_REQUEST_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|e| {
            eprintln!("hull: WARN could not build the configured HTTP client ({e}); using an un-timed default");
            reqwest::Client::new()
        })
}

/// The panic guard's response: log + count the panic and return a plain 500, instead of letting the
/// worker unwind and reset the connection.
fn handle_panic(err: Box<dyn std::any::Any + Send + 'static>) -> Response {
    let detail = err
        .downcast_ref::<String>()
        .map(|s| s.as_str())
        .or_else(|| err.downcast_ref::<&str>().copied())
        .unwrap_or("<non-string panic payload>");
    let n = PANIC_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    eprintln!("hull: ERROR handler panicked (count={n}): {detail}");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal server error").into_response()
}
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
    /// The `"tenant/repo"` key this notification is about, so the inbox can link to the right repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    /// Structured link target within the repo (`"pr"` / `"issue"`), paired with `target_number`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_kind: Option<String>,
    /// The PR / issue number this notification links to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_number: Option<u64>,
}

/// A core [`Notifier`] capability that records recent notifications in memory so the UI can show
/// them — demonstrating the plugin seam end-to-end (the registry fans out to every notifier).
struct RecordingNotifier(Arc<Mutex<Vec<Notification>>>);
#[async_trait::async_trait]
impl Notifier for RecordingNotifier {
    async fn notify(&self, e: &NotifyEvent) {
        let mut buf = self.0.lock().unwrap();
        buf.push(Notification {
            kind: e.kind.clone(),
            to: e.to.clone(),
            summary: e.summary.clone(),
            change: e.change.clone(),
            ts: now(),
            repo: e.repo.clone(),
            target_kind: e.target_kind.clone(),
            target_number: e.target_number,
        });
        let n = buf.len();
        if n > 100 {
            buf.drain(0..n - 100);
        }
    }
}

/// How long an issued session token stays valid. A token older than this is rejected on use and
/// dropped — bounding both the blast radius of a leaked token and the unbounded growth of the token
/// map (nothing else ever evicts a token besides expiry and explicit logout).
const SESSION_TTL_SECS: u64 = 30 * 24 * 60 * 60; // 30 days
/// TTL for an in-flight passkey ceremony (register / add-passkey / authenticate). Abandoned flows are
/// pruned on the next insert into the same map, so an unauthenticated `.../start` loop cannot grow the
/// flow maps without bound. 5 minutes, matching the login-challenge TTL.
const CEREMONY_TTL_SECS: u64 = 300;
/// TTL for a feed ticket. Short (the browser mints one right before opening the stream) but long
/// enough to survive EventSource's auto-reconnect; the client re-mints on expiry.
const FEED_TICKET_TTL_SECS: u64 = 300;

/// Login challenges (nonce → issue time) and issued session tokens (token → (actor id, issued time)).
/// In-memory (crash-only); a hosted deployment would back this with the domain store / a cache.
#[derive(Default)]
struct AuthState {
    challenges: HashMap<String, u64>,
    /// token → (actor id, issued-at unix seconds). The issued time drives TTL expiry (see
    /// [`SESSION_TTL_SECS`] and [`authed_actor`]).
    tokens: HashMap<String, (String, u64)>,
    /// In-flight passkey ceremonies, keyed by an opaque flow id handed to the client.
    reg_flows: HashMap<String, passkey::RegFlow>,
    add_flows: HashMap<String, passkey::AddFlow>,
    auth_flows: HashMap<String, passkey::AuthFlow>,
    /// In-flight GitHub App install handoffs: opaque state → (account id, expiry). Minted only for an
    /// authed admin of that account, single-use, so the setup callback can connect the installation
    /// WITHOUT the browser redirect carrying a session — and nobody can connect an org they don't admin.
    gh_pending: HashMap<String, (String, u64)>,
    /// Short-lived tickets for the SSE `/api/feed` stream (which can't carry an Authorization header):
    /// ticket → (actor id, issued-at). Minted by the authenticated `POST /api/feed/ticket`; the feed
    /// resolves the ticket to the actor and streams only that actor's member accounts.
    feed_tickets: HashMap<String, (String, u64)>,
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
    /// Serializes issue/PR number allocation (the `MAX(number)+1` read-then-insert in `create_issue`,
    /// `create_pr`, and the review auto-triage path) so two concurrent creates in the same repo can't
    /// be handed the same number. One global async lock is ample for this single-process server's
    /// create volume; the `UNIQUE(repo, number)` index is the database-level backstop.
    number_lock: Arc<tokio::sync::Mutex<()>>,
    /// Decentralized ref transport: if a nostr key + relays are configured, a repo's branch pointer is
    /// published as a signed event (kind 31900) each time it lands, so history isn't hostage to one
    /// host. `None` unless configured (OSS default is off).
    nostr_refs: Option<Arc<nostr::NostrRefs>>,
}

impl repos::HasRepoHost for App {
    fn repo_host(&self) -> &repos::RepoHost {
        &self.repos
    }
}

impl App {
    /// Re-key the in-memory notification buffer on repo rename: every notification about `old` now
    /// points at `new`, so the inbox's repo link still resolves after the rename.
    fn rekey_notifications(&self, old: &str, new: &str) {
        let mut buf = self.notifications.lock().unwrap();
        for n in buf.iter_mut() {
            if n.repo.as_deref() == Some(old) {
                n.repo = Some(new.to_string());
            }
        }
    }

    /// Drop every notification about `repo` on repo delete, so the inbox doesn't linger with links to
    /// a repo that no longer exists.
    fn purge_notifications(&self, repo: &str) {
        let mut buf = self.notifications.lock().unwrap();
        buf.retain(|n| n.repo.as_deref() != Some(repo));
    }
}

/// Build the router with an already-assembled registry (handy for tests / embedding). Wires a
/// coordination source (real keeld bridge or the demo) but NOT the QUIC ingress — [`run`] starts
/// that, so tests don't bind a UDP port.
pub async fn router(registry: Registry) -> Router {
    let hub = Arc::new(ActivityHub::new());
    wire_sources(&hub);
    // Tests/embedding use an ephemeral in-memory store; `run` uses the durable FileStore.
    let store: Arc<dyn Store> = Arc::new(InMemory::new());
    seed_if_empty(&*store).await;
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
    // Decentralized ref transport: publish each landed branch pointer as a signed kind:31900 event.
    let nostr_refs = nostr::NostrRefs::from_env().map(|r| {
        eprintln!("nostr: ref transport enabled → {} relay(s)", r.relays().len());
        Arc::new(r)
    });
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
        http: build_http_client(),
        public_url: std::env::var("HULL_PUBLIC_URL").unwrap_or_else(|_| "http://127.0.0.1:8930".into()).into(),
        mirror: Arc::new(mirror::MirrorLedger::from_env()),
        webauthn: Arc::new(passkey::build()),
        repo_settings: Arc::new(reposettings::RepoSettingsStore::from_env()),
        connections: Arc::new(connections::ForgeConnections::from_env()),
        number_lock: Arc::new(tokio::sync::Mutex::new(())),
        nostr_refs,
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

/// One-shot migration entrypoint: import the on-disk `store.json` snapshot into Postgres, then exit.
/// Reads `HULL_DATABASE_URL` (required) and the same `store.json` path `run` would use. Idempotent —
/// the domain tables are replaced with the snapshot. Invoked by the `import-postgres` subcommand.
/// Returns `Err` (never panics on the operator's behalf) so the binary can print + exit non-zero.
pub async fn import_postgres() -> Result<(), String> {
    // The async `tokio-postgres` client runs directly on the caller's runtime (the bin's
    // `#[tokio::main]`), so no dedicated OS thread / nested runtime is needed anymore — just `.await`.
    let url = std::env::var("HULL_DATABASE_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .ok_or("hull: set HULL_DATABASE_URL to the target Postgres before importing")?;
    let path = data_path();
    let pg = PostgresStore::connect(&url).await?;
    let stats = hull_core::store::import_store_json(&pg, &path).await?;
    eprintln!("hull: imported {stats} from {} into Postgres", path.display());
    Ok(())
}

async fn seed_if_empty(store: &dyn Store) {
    if store.accounts().await.is_empty() {
        seed(store).await;
    }
    // The demo owner is a PUBLISHED-key backdoor (anyone can sign in as owner of every org with it),
    // so it is gated behind an explicit `HULL_DEMO_MODE` opt-in and is OFF by default. Same for the
    // `backfill_accountability` re-rooting, which signs delegations with that same public demo key.
    if demo_mode_enabled() {
        ensure_demo_owner(store).await;
    }
    backfill_members(store).await;
    backfill_accountability(store).await;
    normalize_account_handles(store).await;
}

/// One-time, idempotent repair: an account handle persisted before [`sanitize_handle`] was
/// strengthened (or written by an older client) can contain characters that [`repos::safe_segment`]
/// rejects (`"new org"`, `"n;kkjkjk"`, …). Such an org can never create a repo, because
/// [`repos::RepoHost::create_repo`] runs `safe_segment` on the tenant (= the handle). Rewrite every
/// invalid handle to its `sanitize_handle` form, disambiguating with a short numeric suffix if that
/// collides with an existing account. Repos are keyed by `account.id` (not handle) and the on-disk
/// tenant dir is derived from the handle only at request time, so no repo dir is orphaned — and these
/// broken orgs have no repos anyway (creation was impossible). No-op once all handles are valid.
///
/// Scoped to organizations on purpose. A personal account's handle is one leg of the load-bearing
/// `User.username == Actor.handle == Account.handle` invariant that [`account_update`] keeps in sync;
/// rewriting the account handle alone would desync the actor/username (breaking `@mention` lookups)
/// and be silently reverted the next time the user saves their settings. Personal handles are
/// repaired through the username path instead, not here.
async fn normalize_account_handles(store: &dyn Store) {
    let accounts = store.accounts().await;
    // Handles already in use (lowercased), so a repaired handle doesn't collide with a valid one.
    // Includes personal-account handles (which share the handle namespace) even though we don't
    // rewrite them, so an org repair never lands on a name a personal account already holds.
    let mut taken: std::collections::HashSet<String> = accounts.iter().map(|a| a.handle.to_lowercase()).collect();
    for mut acct in accounts {
        if acct.kind != hull_core::AccountKind::Organization || repos::safe_segment(&acct.handle) {
            continue;
        }
        let base = sanitize_handle(&acct.handle);
        let base = if base.is_empty() { format!("org-{}", acct.id) } else { base };
        // Free the old (invalid) handle from the taken set so it doesn't block its own replacement.
        taken.remove(&acct.handle.to_lowercase());
        let mut candidate = base.clone();
        let mut n = 2;
        while taken.contains(&candidate.to_lowercase()) {
            candidate = format!("{base}-{n}");
            n += 1;
        }
        taken.insert(candidate.to_lowercase());
        eprintln!("hull: normalized invalid account handle {:?} -> {:?} (id {})", acct.handle, candidate, acct.id);
        acct.handle = candidate;
        store.put_account(acct).await;
    }
}

/// Whether the local/demo affordances are enabled. Truthy values (`enforce`/`on`/`true`/`1`/`yes`,
/// case-insensitive) turn it ON; unset or a falsey value keeps it OFF — the safe production default.
/// When OFF, the published [`DEMO_OWNER_SECRET`] has ZERO effect: no auto-owner, no demo-key
/// delegation re-rooting. Mirrors the truthy parsing of [`git_auth_enforced`].
fn demo_mode_enabled() -> bool {
    match std::env::var("HULL_DEMO_MODE") {
        Ok(v) => {
            let v = v.trim();
            ["enforce", "on", "true", "1", "yes"].iter().any(|t| v.eq_ignore_ascii_case(t))
        }
        Err(_) => false,
    }
}

/// Migration for the crypto-delegation milestone (NEW-1166): any agent whose delegation doesn't
/// cryptographically verify — including legacy agents minted before hops were signed, or with no
/// delegation at all — is re-rooted at the demo human with a **signed** hop. Without this, enforcing
/// [`Delegation::verify`] at the authoring gate would lock out agents seeded by an earlier build.
/// Idempotent: already-verifiable agents are skipped. Only the demo human can be signed for here (its
/// key is known); a real deployment re-delegates through the owning human instead.
async fn backfill_accountability(store: &dyn Store) {
    use hull_core::{ActorKind, Delegation, DelegationHop};
    // Re-rooting on the published demo key is a demo-only affordance (see `seed_if_empty`). In prod
    // there are no pre-crypto legacy agents to migrate, and we must not sign anything with a public
    // key, so this is a no-op unless demo mode is explicitly enabled.
    if !demo_mode_enabled() {
        return;
    }
    let Some(demo) = identity::human_from_secret("demo", DEMO_OWNER_SECRET) else { return };
    let demo_id = demo.actor.id;
    let no_rev = |_: &str| false;
    for mut a in store.actors().await {
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
        store.put_actor(a).await;
    }
}

/// A published demo credential: a fixed Ed25519 secret so a local/demo instance has a **known** human
/// you can log into (through the real signature flow) and exercise owner-only features. This is not a
/// backdoor — login still verifies the signature; it's just a demo account whose key is public. The
/// frontend's "Sign in as demo" uses the same secret. Never ship this key on a real deployment.
const DEMO_OWNER_SECRET: &str = "68756c6c2d64656d6f2d6f776e65722d6b65792d64656d6f2d6f6e6c79212121";

/// Ensure the demo owner exists and owns every org, so a fresh login lands on a usable account.
/// Idempotent.
async fn ensure_demo_owner(store: &dyn Store) {
    use hull_core::{Membership, Role};
    let Some(minted) = identity::human_from_secret("demo", DEMO_OWNER_SECRET) else { return };
    let id = minted.actor.id.clone();
    if store.actor(&id).await.is_none() {
        store.put_actor(minted.actor).await;
    }
    for mut acct in store.accounts().await {
        if !acct.members.iter().any(|m| m.actor == id) {
            acct.members.push(Membership { actor: id.clone(), role: Role::Owner });
            store.put_account(acct).await;
        }
    }
}

/// Idempotent migration: an account persisted before memberships existed comes back with an empty
/// `members` list. Backfill the canonical org members (the human `justin` as Owner, `agent:reviewer`
/// as Write) by handle, without wiping the durable demo store or sweeping in every actor ever
/// registered. Skips a handle that isn't present.
async fn backfill_members(store: &dyn Store) {
    use hull_core::{Membership, Role};
    const CANONICAL: &[(&str, Role)] = &[("justin", Role::Owner), ("agent:reviewer", Role::Write)];
    for mut acct in store.accounts().await {
        if !acct.members.is_empty() {
            continue;
        }
        for (handle, role) in CANONICAL {
            if let Some(actor) = store.actors().await.into_iter().find(|a| &a.handle == handle) {
                acct.members.push(Membership { actor: actor.id, role: *role });
            }
        }
        if !acct.members.is_empty() {
            store.put_account(acct).await;
        }
    }
}

/// `/health` — 200 `ok` normally, 503 `degraded` when persistence has failed (see
/// [`hull_core::PERSISTENCE_DEGRADED`]) so an orchestrator's health probe pulls the instance out of
/// rotation instead of serving from state that has silently diverged from disk.
async fn health() -> Response {
    if hull_core::persistence_degraded() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "degraded: persistence is failing — in-memory state has diverged from disk",
        )
            .into_response();
    }
    (StatusCode::OK, "ok").into_response()
}

/// `/ready` — readiness probe (distinct from `/health` liveness). 200 `{"ready":true}` when the store
/// backend can serve (Postgres: a live pooled connection + `SELECT 1`; local backends: always/true),
/// else 503 `{"ready":false}`. Unauthenticated and held OUT of the heavy middleware so orchestrators
/// (k8s/compose) can poll it cheaply and frequently.
async fn ready(State(app): State<App>) -> Response {
    observability::ready_response(app.store.ready().await)
}

fn make_router(app: App) -> Router {
    eprintln!("hull-server: hosting keel repos under {}", app.repos.root().display());
    // Long-lived / large-body routes are held OUT of the timeout, body-cap, and concurrency layers:
    // the SSE feed streams indefinitely (a 30s timeout or a held concurrency permit would kill it and
    // exhaust the pool), git smart-HTTP carries large packfiles on push, and the tar download can be
    // large/slow. They still get the global panic guard (applied to the merged router below).
    let streaming = Router::new()
        .route("/api/feed", get(feed))
        .route("/api/repos/:tenant/:repo/tree/:tree/tar", get(tree_archive))
        .route("/:tenant/:repo/info/refs", get(info_refs_handler))
        .route("/:tenant/:repo/git-upload-pack", post(upload_pack_handler))
        .route("/:tenant/:repo/git-receive-pack", post(receive_pack_handler))
        // These routes are exempt from the 8 MiB `MAX_BODY_BYTES` cap because a git push carries a
        // packfile — but "no cap" let an anonymous client OOM the server with a huge (or gzip-bombed)
        // body. Apply a generous, operator-tunable git cap instead (`HULL_GIT_MAX_BODY_MB`, default
        // 512). The GET routes (feed/tar/info-refs) carry no request body, so the cap is a no-op there;
        // `maybe_gunzip` caps the *decompressed* size to the same limit to stop gzip bombs.
        .layer(RequestBodyLimitLayer::new(repos::git_max_body_bytes()));

    let api = Router::new()
        .route("/health", get(health))
        .route("/api/home", get(home))
        .route("/api/feed/ticket", post(feed_ticket))
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
        .route("/api/auth/session", axum::routing::delete(auth_logout))
        // passkey (WebAuthn) accounts — passwordless signup + login
        .route("/api/auth/register/start", post(register_start))
        .route("/api/auth/register/finish", post(register_finish))
        .route("/api/auth/passkey/start", post(passkey_start))
        .route("/api/auth/passkey/finish", post(passkey_finish))
        // sovereign (non-custodial) accounts: register a client-held key, fetch its wrapped bundle
        .route("/api/auth/sovereign/register", post(sovereign_register))
        .route("/api/auth/sovereign/wrapped", get(sovereign_wrapped))
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
        .route("/api/repos/:tenant/:repo", axum::routing::patch(rename_repo_handler).delete(delete_repo_handler))
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
        .route("/api/repos/:tenant/:repo/prs/:number/reviewers", post(request_reviewer))
        .route("/api/repos/:tenant/:repo/mirror", get(mirror_status))
        .route("/api/repos/:tenant/:repo/mirror/push", post(mirror_push_now))
        .route("/api/repos/:tenant/:repo/mirror/inbound", post(mirror_inbound))
        .route("/api/repos/:tenant/:repo/mirror/github", post(mirror_github_webhook))
        .route("/api/repos/:tenant/:repo/reviews", get(reviews).post(create_review))
        .route("/api/repos/:tenant/:repo/artifacts/:id", get(get_artifact))
        .route("/api/repos/:tenant/:repo/comments/:id", axum::routing::patch(edit_comment).delete(delete_comment))
        .route("/api/repos/:tenant/:repo/change/:id", get(change_info))
        .route("/api/repos/:tenant/:repo/change/:id/diff", get(change_diff))
        .route("/api/repos/:tenant/:repo/change/:id/file", get(change_file))
        .route("/api/repos/:tenant/:repo/change/:id/semantic", get(change_semantic))
        .route("/api/repos/:tenant/:repo/change/:id/ledger", get(change_ledger))
        .route("/api/repos/:tenant/:repo/change/:id/claims/:claim/resolve", post(resolve_claim))
        .route("/api/repos/:tenant/:repo/change/:id/ci-result", post(ci_result))
        .route("/api/repos/:tenant/:repo/ci-config", get(get_ci_config).put(set_ci_config))
        .route("/api/repos/:tenant/:repo/autonomy", get(get_repo_autonomy).put(set_repo_autonomy))
        .route("/api/accounts/:id/autonomy", put(set_account_autonomy))
        .route("/api/repos/:tenant/:repo/security", get(repo_security))
        .route("/api/repos/:tenant/:repo/owners", get(owners_list).post(set_owners))
        .route("/api/repos/:tenant/:repo/settings", get(get_repo_settings).put(set_repo_settings))
        .route("/api/repos/:tenant/:repo/substrate", get(substrate_view))
        .route("/api/repos/:tenant/:repo/labels", get(repo_labels))
        .route("/api/repos/:tenant/:repo/change/:id/verify", post(verify_change))
        .route("/api/repos/:tenant/:repo/change/:id/session", post(ingest_session))
        .route("/api/scan", post(scan))
        .route("/api/plugins", get(plugins_list))
        // The normal API surface gets the request-body cap, per-request timeout, and a global
        // concurrency ceiling. (Applied only here so the `streaming` routes stay exempt.)
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, Duration::from_secs(REQUEST_TIMEOUT_SECS)))
        .layer(GlobalConcurrencyLimitLayer::new(MAX_CONCURRENT_REQUESTS));

    // Long **synchronous** handlers that `.await` an agent-CLI / local-CI subprocess inline and return
    // the result in the response — they legitimately run for minutes, so the fast 30s timeout would
    // 408 them mid-run and break the core review/CI loop. They aren't streams (unlike `streaming`),
    // just slow, so they keep the body-cap and concurrency layers but get a generous 600s timeout.
    let slow = Router::new()
        .route("/api/repos/:tenant/:repo/prs/:number/auto-review", post(auto_review))
        .route("/api/repos/:tenant/:repo/prs/:number/fix", post(fix_finding))
        .route("/api/repos/:tenant/:repo/change/:id/check", post(run_check_handler))
        .route("/api/repos/:tenant/:repo/comments", get(comments_list).post(create_comment))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, Duration::from_secs(SLOW_REQUEST_TIMEOUT_SECS)))
        .layer(GlobalConcurrencyLimitLayer::new(MAX_CONCURRENT_REQUESTS));

    // Request observability (one structured log line + counter/gauge updates per request). It's a
    // `from_fn` layer that emits on the response HEAD, so it does NOT buffer streaming bodies — SSE,
    // git smart-HTTP, and the tar download keep streaming (see `observability::observe`). Applied to
    // the real API surface (api + slow + streaming) but NOT to `/ready`/`/metrics` below, so a
    // health/metrics scrape isn't logged or counted (avoids self-referential metric churn).
    let observed = api
        .merge(slow)
        .merge(streaming)
        .layer(axum::middleware::from_fn(observability::observe));

    // Operational probes: unauthenticated, and deliberately OUTSIDE the timeout/body/concurrency AND
    // the observability layers so they stay cheap under load and don't count their own scrapes.
    // `/ready` reads the store; `/metrics` renders atomics only.
    let probes = Router::new()
        .route("/ready", get(ready))
        .route("/metrics", get(observability::metrics_handler));

    observed
        .merge(probes)
        // The panic guard wraps EVERY route (streaming + probes included): a handler panic becomes a
        // logged, counted 500 instead of a reset connection.
        .layer(CatchPanicLayer::custom(handle_panic))
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
    // Install the tracing subscriber ONCE, before anything logs. Idempotent (guards double-init).
    observability::init_tracing();
    // Harden the server process before it serves any request: on Linux, make it non-dumpable (so a
    // same-uid CI child can't read the server's own /proc/<pid>/environ secrets) and a child-subreaper
    // (so a CI descendant that escapes its process group reparents here to be reaped). No-op elsewhere.
    ci_sandbox::harden_server_process();
    // Fail fast in the prod profile: refuse to boot with an unsafe/missing security config rather than
    // silently running open. A no-op unless `HULL_PROFILE=prod` / `HULL_PROD=1` is set (the default).
    enforce_prod_profile();
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
    // Kept for a final flush on graceful shutdown (the timer below only flushes every 5s).
    let hub_shutdown = hub.clone();
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
        // `HULL_INGRESS_TOKEN`, when set, is required in each daemon's header frame; unset (default)
        // leaves the ingress open exactly as before.
        let ingress_token = std::env::var("HULL_INGRESS_TOKEN").ok().filter(|t| !t.is_empty());
        ingress::spawn(addr, hub.clone(), ingress_token); // daemons dial in via hull-agent
    }
    // Backend selection. `HULL_DATABASE_URL` set → Postgres (build the pool, run migrations); UNSET
    // (the default) → the durable FileStore exactly as before, so the current dogfood is unchanged.
    let store: Arc<dyn Store> = match std::env::var("HULL_DATABASE_URL") {
        Ok(url) if !url.is_empty() => {
            eprintln!("hull-server: domain store = Postgres");
            Arc::new(PostgresStore::connect(&url).await.expect("hull-server: Postgres store"))
        }
        _ => {
            let store = Arc::new(FileStore::open(data_path()));
            eprintln!("hull-server: domain store at {}", data_path().display());
            store
        }
    };
    seed_if_empty(&*store).await;
    let router = make_router(build_app(registry, hub, store));
    let listener = tokio::net::TcpListener::bind(&opts.addr).await.expect("bind");
    tracing::info!(addr = %opts.addr, "hull-server listening on http://{}", opts.addr);
    // Graceful shutdown on SIGTERM/SIGINT: stop accepting, let in-flight requests drain, then flush
    // the activity ranking one last time so a redeploy doesn't drop the last (≤5s) window of events.
    // The drain is BOUNDED: long-lived SSE (`/api/feed`) connections never close on their own, so an
    // unbounded `with_graceful_shutdown` would block until the orchestrator SIGKILLs us and the final
    // flush would never run. Cap the drain at 20s, then flush unconditionally whether it completed or
    // hit the cap.
    // The 20s cap bounds ONLY the drain — it starts when a shutdown signal fires, not at boot. (A
    // previous version wrapped the whole `server` future in `timeout(20s, …)`, which killed every
    // server 20s after startup, since `with_graceful_shutdown` only resolves after a signal.)
    let draining = std::sync::Arc::new(tokio::sync::Notify::new());
    let draining_signal = draining.clone();
    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
        shutdown_signal().await;
        draining_signal.notify_one(); // a signal arrived → the drain begins now
    });
    let drain_cap = async move {
        draining.notified().await; // block until a shutdown signal has actually fired…
        tokio::time::sleep(Duration::from_secs(20)).await; // …then cap the drain at 20s
    };
    tokio::select! {
        r = server => match r {
            Ok(()) => eprintln!("hull-server: draining complete — flushing activity hub before exit"),
            Err(e) => eprintln!("hull-server: serve error: {e} — flushing activity hub before exit"),
        },
        _ = drain_cap => eprintln!("hull-server: drain cap (20s) hit — some connections still open; flushing anyway"),
    }
    hub_shutdown.flush();
}

/// Resolve when the process is asked to stop: SIGINT (Ctrl-C) or SIGTERM (the orchestrator's stop
/// signal on a redeploy). On non-Unix, only Ctrl-C is wired.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    eprintln!("hull-server: shutdown signal received");
}

// ── prod-profile fail-fast config validation ─────────────────────────────────────────────────────

/// Whether the prod profile is active: `HULL_PROFILE=prod` or a truthy `HULL_PROD`. Off by default,
/// so nothing below changes behavior for the dogfood.
fn prod_profile_active() -> bool {
    let profile_prod = std::env::var("HULL_PROFILE").map(|v| v.trim().eq_ignore_ascii_case("prod")).unwrap_or(false);
    let prod_flag = std::env::var("HULL_PROD")
        .map(|v| {
            let v = v.trim();
            ["1", "true", "on", "yes", "enforce"].iter().any(|t| v.eq_ignore_ascii_case(t))
        })
        .unwrap_or(false);
    profile_prod || prod_flag
}

/// Pure check of the security-critical config for a prod deployment. Returns the list of problems
/// (empty ⇒ safe to start). Kept pure over its inputs (not env) so it is straightforward to unit-test.
fn prod_config_problems(git_auth_enforced: bool, ingress_token: Option<&str>, session_key: Option<&str>, demo_mode: bool) -> Vec<String> {
    let mut problems = Vec::new();
    if !git_auth_enforced {
        problems.push("HULL_GIT_AUTH must be `enforce` (anonymous git push/fetch is unsafe in prod)".to_string());
    }
    if ingress_token.map(|t| t.trim().is_empty()).unwrap_or(true) {
        problems.push("HULL_INGRESS_TOKEN must be set (an unauthenticated coordination ingress is unsafe in prod)".to_string());
    }
    // No on-disk key fallback in prod: the AEAD session key must be supplied explicitly (64 hex chars).
    let key_ok = session_key
        .map(|k| {
            let k = k.trim();
            k.len() == 64 && k.chars().all(|c| c.is_ascii_hexdigit())
        })
        .unwrap_or(false);
    if !key_ok {
        problems.push("HULL_SESSION_KEY must be set to 64 hex chars (no on-disk key fallback in prod)".to_string());
    }
    if demo_mode {
        problems.push("HULL_DEMO_MODE must be off in prod (it enables a published-key owner backdoor)".to_string());
    }
    problems
}

/// Enforce the prod profile's requirements at startup. A no-op unless the prod profile is active
/// (default). When active, any missing/unsafe setting aborts startup with a clear message rather than
/// booting an insecure server.
fn enforce_prod_profile() {
    if !prod_profile_active() {
        return;
    }
    let ingress = std::env::var("HULL_INGRESS_TOKEN").ok();
    let session = std::env::var("HULL_SESSION_KEY").ok();
    let problems = prod_config_problems(git_auth_enforced(), ingress.as_deref(), session.as_deref(), demo_mode_enabled());
    if !problems.is_empty() {
        eprintln!("hull: FATAL refusing to start under the prod profile — insecure configuration:");
        for p in &problems {
            eprintln!("  - {p}");
        }
        panic!("hull: prod-profile config validation failed ({} problem(s)); see the log above", problems.len());
    }
    eprintln!("hull-server: prod-profile security config validated");
}

/// Home for a tenant: `GET /api/home?tenant=acme` (defaults to `local`). The tenant will come from
/// the authenticated session once auth lands (NEW-1166); until then it's an explicit param.
async fn home(State(app): State<App>, headers: axum::http::HeaderMap, Query(_q): Query<HashMap<String, String>>) -> Json<Value> {
    // Personalized: the signed-in user's repos across EVERY account they belong to, ranked by
    // activity (active repos first, then their quiet repos). Not a global tenant. Logged out → empty.
    let Some(actor) = authed_actor(&app, &headers).await else {
        return Json(json!({ "repos": [], "accounts": [] }));
    };
    let accts = member_accounts(&app, &actor.id).await;
    let mut items: Vec<Value> = Vec::new();
    for acct in &accts {
        let ranked = app.hub.home(&acct.handle);
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in &ranked {
            seen.insert(r.repo.clone());
            items.push(json!({ "tenant": acct.handle, "repo": r.repo, "score": r.score, "last_ts": r.last_ts, "active_actors": r.active_actors, "hot_files": r.hot_files }));
        }
        // Include the account's repos that have no recent activity, so created/imported repos show.
        for repo in app.store.repos().await.into_iter().filter(|rp| rp.owner == acct.id) {
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
        for repo in app.store.repos().await.into_iter().filter(|rp| rp.owner == acct.id) {
            let key = format!("{}/{}", acct.handle, repo.name);
            for is in app.store.issues(&key).await {
                if !matches!(is.status, IssueStatus::Open) { continue; }
                let reason = if is.author == me { "you opened" } else if is.assignees.iter().any(|a| a == me) { "assigned to you" } else if is.referenced_actors.iter().any(|a| a == me) { "you're mentioned" } else { continue };
                rel_issues.push(json!({ "tenant": acct.handle, "repo": repo.name, "number": is.number, "title": is.title, "author": is.author, "reason": reason, "ts": is.created_unix }));
            }
            for pr in app.store.prs(&key).await {
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
    let Some(me) = authed_actor(&app, &headers).await else {
        return (StatusCode::UNAUTHORIZED, "not signed in").into_response();
    };
    let bio = app.store.user_by_actor(&me.id).await.map(|u| u.bio).unwrap_or_default();
    let since = now().saturating_sub(371 * 86_400);
    // A change's author is the git author string ("handle <email> ..."), so we attribute by HANDLE:
    // mine = my own handle + every agent handle whose accountability roots at me.
    let mut mine: std::collections::HashSet<String> = std::collections::HashSet::new();
    mine.insert(me.handle.clone());
    let mut agent_handles: std::collections::HashSet<String> = std::collections::HashSet::new();
    for a in app.store.actors().await {
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
    for acct in member_accounts(&app, &me.id).await {
        for repo in app.store.repos().await.into_iter().filter(|r| r.owner == acct.id) {
            let key = format!("{}/{}", acct.handle, repo.name);
            let roots: Vec<String> = app.store.prs(&key).await.into_iter().flat_map(|p| p.changes).collect();
            for (author, ts, id) in app.repos.history(&acct.handle, &repo.name, &roots, since) {
                let h = handle_of(&author);
                if mine.contains(&h) {
                    let e = days.entry(ts / 86_400).or_default();
                    if agent_handles.contains(&h) { e.1 += 1; } else { e.0 += 1; }
                    *by_handle.entry(h).or_default() += 1;
                    total += 1;
                    if let Some(sr) = app.store.session_record(&key, &id).await {
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
    let Some(acct) = app.store.accounts().await.into_iter().find(|a| a.handle == handle) else {
        return (StatusCode::NOT_FOUND, "no such organization").into_response();
    };
    let since = now().saturating_sub(371 * 86_400);
    // An author handle counts as an agent if any actor bearing that handle is an agent.
    let mut agent_handles: std::collections::HashSet<String> = std::collections::HashSet::new();
    for a in app.store.actors().await {
        if a.kind == hull_core::ActorKind::Agent { agent_handles.insert(a.handle.clone()); }
    }
    let handle_of = |author: &str| -> String { author.split_once(" <").map(|(n, _)| n.trim().to_string()).unwrap_or_else(|| author.trim().to_string()) };
    let mut days: HashMap<u64, (u64, u64)> = HashMap::new();
    let mut by_handle: HashMap<String, u64> = HashMap::new();
    let mut total = 0u64;
    let mut tok_days: HashMap<u64, (u64, u64)> = HashMap::new();
    let (mut tok_in, mut tok_out) = (0u64, 0u64);
    for repo in app.store.repos().await.into_iter().filter(|r| r.owner == acct.id) {
        let key = format!("{}/{}", acct.handle, repo.name);
        let roots: Vec<String> = app.store.prs(&key).await.into_iter().flat_map(|p| p.changes).collect();
        for (author, ts, id) in app.repos.history(&acct.handle, &repo.name, &roots, since) {
            let h = handle_of(&author);
            let e = days.entry(ts / 86_400).or_default();
            if agent_handles.contains(&h) { e.1 += 1; } else { e.0 += 1; }
            *by_handle.entry(h.clone()).or_default() += 1;
            total += 1;
            if let Some(sr) = app.store.session_record(&key, &id).await {
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
    let repo_names: Vec<String> = app.store.repos().await.into_iter()
        .filter(|r| r.owner == acct.id && !app.repo_settings.get(&format!("{}/{}", acct.handle, r.name)).private)
        .map(|r| r.name).collect();
    Json(json!({ "handle": acct.handle, "members": acct.members.len(), "repos": repo_names.len(), "repo_names": repo_names,
        "total": total, "days": days_v, "contributors": contributors,
        "tokens": { "in": tok_in, "out": tok_out, "series": tok_v } })).into_response()
}

/// The accounts an actor is a member of.
async fn member_accounts(app: &App, actor_id: &str) -> Vec<Account> {
    app.store.accounts().await.into_iter().filter(|a| a.members.iter().any(|m| m.actor == actor_id)).collect()
}

/// Visibility gate: may `actor` (or an anonymous caller) read this repo? Public repos are readable by
/// anyone; a private repo only by a member of its owning account, or a member of a team the repo
/// grants access to.
async fn can_read_repo(app: &App, actor_id: Option<&str>, tenant: &str, repo: &str) -> bool {
    let key = format!("{tenant}/{repo}");
    let settings = app.repo_settings.get(&key);
    if !settings.private {
        return true;
    }
    let Some(aid) = actor_id else { return false };
    // Resolve the owning account the same way the write-side gate does (repo record's owner, falling
    // back to handle==tenant), so read-side and write-side membership evaluate the same account.
    let Some(acct) = repo_owner_account(app, tenant, repo).await else { return false };
    if acct.members.iter().any(|m| m.actor == aid) {
        return true;
    }
    app.store
        .teams(&acct.id)
        .await
        .into_iter()
        .any(|t| settings.team_access.iter().any(|ta| ta.team == t.id) && t.members.iter().any(|m| m.actor == aid))
}

/// A 404 for repos the caller can't see — used so a private repo doesn't even reveal its existence.
#[allow(clippy::result_large_err)]
async fn require_repo_read(app: &App, headers: &axum::http::HeaderMap, tenant: &str, repo: &str) -> Result<(), Response> {
    let actor = authed_actor(app, headers).await.map(|a| a.id);
    if can_read_repo(app, actor.as_deref(), tenant, repo).await {
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
    Json(json!({ "hosted": app.repos.list(), "repos": app.store.repos().await }))
}

/// Recent notifications recorded by the core `Notifier` capability (newest first). Demonstrates the
/// plugin seam: these were fanned out by `registry.notify`, and a hosted plugin would also deliver
/// them over a real channel.
async fn notifications_list(State(app): State<App>, headers: axum::http::HeaderMap) -> Response {
    // The inbox is the AUTHENTICATED actor's, derived from the bearer token — never a spoofable
    // `?actor=` query param, and never the whole-server firehose to an anonymous caller.
    let Some(actor) = authed_actor(&app, &headers).await else {
        return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
    };
    let mut n = app.notifications.lock().unwrap().clone();
    n.reverse();
    // Deliver notifications addressed to this actor, plus broadcasts (empty `to`, e.g. CI results /
    // mirror pushes) — but gate a broadcast about a PRIVATE repo on read access, so private-repo
    // activity doesn't leak to non-members. Addressed notifications are for this actor, so kept as-is.
    let mut items: Vec<Value> = Vec::new();
    for x in n.iter() {
        let addressed = x.to.contains(&actor.id);
        let broadcast = x.to.is_empty();
        if !addressed && !broadcast {
            continue;
        }
        // A repo-scoped broadcast is delivered only if the actor can read that repo. A malformed repo
        // key (present but not `tenant/repo`) is treated as unreadable — default-deny, so a future
        // producer that mis-sets `repo` can't fail open and leak private activity. A repo-less
        // broadcast (`None`) is a genuine server-wide notice and is delivered.
        if broadcast {
            if let Some(rk) = x.repo.as_deref() {
                let visible = match rk.split_once('/') {
                    Some((t, r)) => can_read_repo(&app, Some(&actor.id), t, r).await,
                    None => false,
                };
                if !visible {
                    continue;
                }
            }
        }
        // Resolve recipient handles for display. Explicit loops rather than `.map(async)` — the handle
        // lookups now `.await`.
        let mut to_handles: Vec<String> = Vec::new();
        for id in &x.to {
            to_handles.push(app.store.actor(id).await.map(|a| a.handle).unwrap_or_else(|| id.chars().take(8).collect()));
        }
        items.push(json!({ "kind": x.kind, "summary": x.summary, "change": x.change, "ts": x.ts, "to": to_handles, "broadcast": x.to.is_empty(), "repo": x.repo, "target_kind": x.target_kind, "target_number": x.target_number }));
    }
    Json(json!({ "notifications": items })).into_response()
}

/// Accounts (orgs / personal) with their members (handle + role) and owned repos.
async fn accounts_list(State(app): State<App>, headers: axum::http::HeaderMap) -> Json<Value> {
    let repos = app.store.repos().await;
    // Only the accounts the caller belongs to — an org you're not a member of is not yours to see.
    let visible = match authed_actor(&app, &headers).await {
        Some(a) => member_accounts(&app, &a.id).await,
        None => Vec::new(),
    };
    // Explicit loops rather than `.map(async)` — the member-handle lookups now `.await`.
    let mut accounts: Vec<Value> = Vec::new();
    for a in visible.into_iter() {
        let mut members: Vec<Value> = Vec::new();
        for m in a.members.iter() {
            members.push(json!({
                "actor": m.actor,
                "handle": app.store.actor(&m.actor).await.map(|x| x.handle).unwrap_or_default(),
                "role": m.role,
            }));
        }
        let owned: Vec<String> = repos.iter().filter(|r| r.owner == a.id).map(|r| r.name.clone()).collect();
        accounts.push(json!({ "id": a.id, "handle": a.handle, "kind": a.kind, "members": members, "repos": owned }));
    }
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
    let acting = match require_actor(&app, &headers, body.get("by").and_then(Value::as_str).unwrap_or("")).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let Some(mut acct) = app.store.accounts().await.into_iter().find(|a| a.id == id) else {
        return (StatusCode::NOT_FOUND, "no such account").into_response();
    };
    let is_admin = acct.members.iter().any(|m| m.actor == acting.id && matches!(m.role, Role::Owner | Role::Admin));
    if !is_admin {
        return (StatusCode::FORBIDDEN, "only an org owner/admin can manage members").into_response();
    }
    let Some(actor) = resolve_actor_ref(&app, &body).await else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "unknown actor or username").into_response();
    };
    let role = parse_role(body.get("role").and_then(Value::as_str));
    acct.members.retain(|m| m.actor != actor);
    acct.members.push(Membership { actor, role });
    app.store.put_account(acct.clone()).await;
    (StatusCode::CREATED, Json(json!({ "account": acct }))).into_response()
}

/// Gate an org-management action: the caller must be an Owner or Admin of the account. Returns the
/// loaded account + the acting actor.
#[allow(clippy::result_large_err)]
async fn require_account_admin(app: &App, headers: &axum::http::HeaderMap, account_id: &str) -> Result<(Account, Actor), Response> {
    let acting = require_actor(app, headers, "").await?;
    let Some(acct) = app.store.accounts().await.into_iter().find(|a| a.id == account_id) else {
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
async fn resolve_actor_ref(app: &App, body: &Value) -> Option<String> {
    if let Some(a) = body.get("actor").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        if app.store.actor(a).await.is_some() {
            return Some(a.to_string());
        }
    }
    if let Some(un) = body.get("username").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        return app.store.user_by_username(un).await.map(|u| u.actor);
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

/// Normalize a user/org/repo handle to a safe path segment. Keep only `[A-Za-z0-9._-]`; map any run
/// of other characters (whitespace, punctuation, non-ASCII, emoji) to a single `_`; collapse `..` to
/// `_` (no path traversal); and strip leading dots/underscores and trailing separators. The result
/// ALWAYS satisfies [`repos::safe_segment`], or is empty (which every caller rejects with a
/// "handle/name is required" error). This is the canonical form the UI shows while typing and the
/// server stores, so a tenant path derived from a stored handle can never fail `safe_segment`.
fn sanitize_handle(s: &str) -> String {
    // Map every disallowed char to a space, then treat runs of space as one `_` boundary.
    let mut out = String::with_capacity(s.len());
    let mut pending_sep = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' {
            if pending_sep && !out.is_empty() {
                out.push('_');
            }
            pending_sep = false;
            out.push(ch);
        } else {
            // A literal `_` is allowed but, like any disallowed char (whitespace, punctuation,
            // non-ASCII, emoji), is treated as a separator so runs collapse to a single `_`.
            pending_sep = true;
        }
    }
    // `pending_sep` left set means trailing separators — drop them (no trailing `_`).
    // Collapse any `..` (would fail `safe_segment`) to a single `_`.
    while out.contains("..") {
        out = out.replace("..", "_");
    }
    // Strip leading dots (dotfiles are rejected) / underscores, and trailing separators. After this the
    // result cannot start with `.` nor be `.`/`..`, so it satisfies `safe_segment` (or is empty).
    out.trim_start_matches(['.', '_']).trim_end_matches(['.', '_', '-']).to_string()
}

/// `GET /api/accounts/available?handle=X` — is an org/account handle free? Returns the sanitized form.
async fn account_available(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let handle = sanitize_handle(q.get("handle").map(String::as_str).unwrap_or(""));
    let taken = handle.is_empty() || app.store.accounts().await.iter().any(|a| a.handle.eq_ignore_ascii_case(&handle));
    Json(json!({ "handle": handle, "available": !handle.is_empty() && !taken }))
}

/// `GET /api/repos/available?account=X&name=Y` — is a repo name free under that account?
async fn repo_available(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let account = q.get("account").map(String::as_str).unwrap_or("");
    let name = sanitize_handle(q.get("name").map(String::as_str).unwrap_or(""));
    let taken = match app.store.accounts().await.into_iter().find(|a| a.handle.eq_ignore_ascii_case(account)) {
        Some(acct) => app.store.repos().await.into_iter().any(|r| r.owner == acct.id && r.name.eq_ignore_ascii_case(&name)),
        None => false,
    };
    Json(json!({ "name": name, "available": !name.is_empty() && !taken }))
}

/// `GET /api/auth/available?username=X` — is a username free? Returns the sanitized form.
async fn username_available(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let username = sanitize_handle(q.get("username").map(String::as_str).unwrap_or(""));
    let taken = username.is_empty() || app.store.user_by_username(&username).await.is_some();
    Json(json!({ "username": username, "available": !username.is_empty() && !taken }))
}

/// `POST /api/accounts` — create an organization (the caller becomes its Owner).
async fn create_account(State(app): State<App>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let acting = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let handle = sanitize_handle(body.get("handle").and_then(Value::as_str).unwrap_or(""));
    if handle.is_empty() {
        return (StatusCode::BAD_REQUEST, "handle is required").into_response();
    }
    if app.store.accounts().await.iter().any(|a| a.handle.eq_ignore_ascii_case(&handle)) {
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
    app.store.put_account(acct.clone()).await;
    (StatusCode::CREATED, Json(json!({ "account": acct }))).into_response()
}

/// `DELETE /api/accounts/:id/members/:actor` — remove a member (never the last owner).
async fn remove_member(State(app): State<App>, Path((id, actor)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    let (mut acct, _) = match require_account_admin(&app, &headers, &id).await {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let owners = acct.members.iter().filter(|m| matches!(m.role, Role::Owner)).count();
    let removing_owner = acct.members.iter().any(|m| m.actor == actor && matches!(m.role, Role::Owner));
    if removing_owner && owners <= 1 {
        return (StatusCode::BAD_REQUEST, "cannot remove the last owner").into_response();
    }
    acct.members.retain(|m| m.actor != actor);
    app.store.put_account(acct.clone()).await;
    Json(json!({ "account": acct })).into_response()
}

/// `GET /api/accounts/:id/teams` — the org's teams and their members (public read).
async fn teams_list(State(app): State<App>, Path(id): Path<String>) -> Json<Value> {
    // Explicit loops rather than `.map(async)` — resolving each member's handle now `.await`s.
    let mut teams: Vec<Value> = Vec::new();
    for t in app.store.teams(&id).await.into_iter() {
        let mut members: Vec<Value> = Vec::new();
        for m in t.members.iter() {
            members.push(json!({ "actor": m.actor, "handle": app.store.actor(&m.actor).await.map(|a| a.handle).unwrap_or_default(), "role": m.role }));
        }
        teams.push(json!({ "id": t.id, "name": t.name, "members": members }));
    }
    Json(json!({ "teams": teams }))
}

/// `POST /api/accounts/:id/teams` — create a team (`{name}`).
async fn create_team(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(resp) = require_account_admin(&app, &headers, &id).await {
        return resp;
    }
    let name = body.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "team name is required").into_response();
    }
    let team = Team { id: format!("team_{}", identity::random_hex(8)), account: id, name, members: vec![] };
    app.store.put_team(team.clone()).await;
    (StatusCode::CREATED, Json(json!({ "team": team }))).into_response()
}

/// `DELETE /api/accounts/:id/teams/:team` — remove a team.
async fn delete_team(State(app): State<App>, Path((id, team)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(resp) = require_account_admin(&app, &headers, &id).await {
        return resp;
    }
    if app.store.team(&team).await.map(|t| t.account != id).unwrap_or(true) {
        return (StatusCode::NOT_FOUND, "no such team").into_response();
    }
    app.store.delete_team(&team).await;
    Json(json!({ "ok": true })).into_response()
}

/// `POST /api/accounts/:id/teams/:team/members` — add a member (`{actor|username, role}`).
async fn team_add_member(State(app): State<App>, Path((id, team)): Path<(String, String)>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(resp) = require_account_admin(&app, &headers, &id).await {
        return resp;
    }
    let Some(mut t) = app.store.team(&team).await.filter(|t| t.account == id) else {
        return (StatusCode::NOT_FOUND, "no such team").into_response();
    };
    let Some(actor) = resolve_actor_ref(&app, &body).await else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "unknown actor or username").into_response();
    };
    let role = parse_role(body.get("role").and_then(Value::as_str));
    t.members.retain(|m| m.actor != actor);
    t.members.push(Membership { actor, role });
    app.store.put_team(t.clone()).await;
    (StatusCode::CREATED, Json(json!({ "team": t }))).into_response()
}

/// `DELETE /api/accounts/:id/teams/:team/members/:actor` — remove a team member.
async fn team_remove_member(State(app): State<App>, Path((id, team, actor)): Path<(String, String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(resp) = require_account_admin(&app, &headers, &id).await {
        return resp;
    }
    let Some(mut t) = app.store.team(&team).await.filter(|t| t.account == id) else {
        return (StatusCode::NOT_FOUND, "no such team").into_response();
    };
    t.members.retain(|m| m.actor != actor);
    app.store.put_team(t.clone()).await;
    Json(json!({ "team": t })).into_response()
}

/// Build the settings JSON (no auth — callers gate).
async fn repo_settings_value(app: &App, tenant: &str, repo: &str) -> Value {
    settings_value(app, &app.repo_settings.get(&format!("{tenant}/{repo}"))).await
}

/// Serialize a RepoSettings to the JSON the UI reads (shared by repo settings + org defaults).
async fn settings_value(app: &App, s: &reposettings::RepoSettings) -> Value {
    // Explicit loop rather than `.map(async)` — resolving each reviewer's handle now `.await`s.
    let mut reviewers: Vec<Value> = Vec::new();
    for id in &s.default_reviewers {
        let handle = app.store.actor(id).await.map(|a| a.handle).unwrap_or_default();
        reviewers.push(json!({ "actor": id, "handle": handle }));
    }
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
    if let Err(resp) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return resp;
    }
    let s = app.repo_settings.get(&format!("{tenant}/{repo}"));
    let labels: Vec<Value> = s.labels.iter().map(|l| json!({ "name": l.name, "color": l.color, "icon": l.icon })).collect();
    Json(json!({ "labels": labels })).into_response()
}

/// `GET /api/repos/:tenant/:repo/settings` — repo settings. **Owner/admin only** (settings expose
/// access grants, so they are not public).
async fn get_repo_settings(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    let acting = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if !is_repo_admin(&app, &tenant, &repo, &acting.id).await {
        return (StatusCode::FORBIDDEN, "only a repo owner/admin can view settings").into_response();
    }
    Json(repo_settings_value(&app, &tenant, &repo).await).into_response()
}

/// `PUT /api/repos/:tenant/:repo/settings` — update repo settings (owner/admin only).
async fn set_repo_settings(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let acting = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if !is_repo_admin(&app, &tenant, &repo, &acting.id).await {
        return (StatusCode::FORBIDDEN, "only a repo owner/admin can change settings").into_response();
    }
    let key = format!("{tenant}/{repo}");
    let mut s = app.repo_settings.get(&key);
    apply_settings_patch(&app, &mut s, &body).await;
    app.repo_settings.set(&key, s);
    Json(repo_settings_value(&app, &tenant, &repo).await).into_response()
}

/// Apply a settings JSON patch (shared by repo settings + org defaults).
async fn apply_settings_patch(app: &App, s: &mut reposettings::RepoSettings, body: &Value) {
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
        // Explicit loop rather than `.filter(async)` — the existence check now `.await`s.
        let mut ids = Vec::new();
        for id in arr.iter().filter_map(|v| v.as_str()) {
            if app.store.actor(id).await.is_some() {
                ids.push(id.to_string());
            }
        }
        s.default_reviewers = ids;
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
async fn require_account_admin_ref(app: &App, headers: &axum::http::HeaderMap, acct_ref: &str) -> Result<(Account, Actor), Response> {
    let acting = require_actor(app, headers, "").await?;
    let Some(acct) = app.store.accounts().await.into_iter().find(|a| a.id == acct_ref || a.handle.eq_ignore_ascii_case(acct_ref)) else {
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
    let (acct, _) = match require_account_admin_ref(&app, &headers, acct_ref).await {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let name = sanitize_handle(body.get("name").and_then(Value::as_str).unwrap_or(""));
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "name is required").into_response();
    }
    let tenant = acct.handle.clone();
    // Defense: if the org's own handle isn't a safe path segment, `create_repo` would fail with the
    // misleading "invalid repo name". Startup normalization repairs such handles, so this shouldn't
    // trigger for existing orgs, but return a message that points at the real problem.
    if !repos::safe_segment(&tenant) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("this organization's handle (\"{tenant}\") is invalid — rename the org"),
        )
            .into_response();
    }
    // Case-INSENSITIVE, matching the `rename` guard and the PG `repos_owner_lower_name` unique
    // index — so a case-variant duplicate (`web` next to `Web`) is rejected here with CONFLICT
    // rather than passing the guard and panicking on the index violation under Postgres.
    if app.store.repos().await.iter().any(|r| r.owner == acct.id && r.name.eq_ignore_ascii_case(&name)) {
        return (StatusCode::CONFLICT, "a repo with that name already exists").into_response();
    }
    if let Err(e) = app.repos.create_repo(&tenant, &name) {
        return (StatusCode::UNPROCESSABLE_ENTITY, format!("could not create repo: {e}")).into_response();
    }
    let repo = Repo { id: format!("repo_{tenant}_{name}"), owner: acct.id.clone(), name: name.clone(), default_branch: "main".into() };
    app.store.put_repo(repo.clone()).await;
    // Inherit the org's default repo settings, if it has configured any.
    let defaults = app.repo_settings.get(&repo_defaults_key(&acct.id));
    app.repo_settings.set(&format!("{tenant}/{name}"), defaults);
    (StatusCode::CREATED, Json(json!({ "repo": repo, "tenant": tenant, "name": name }))).into_response()
}

/// `PATCH /api/repos/:tenant/:repo` `{name}` — rename a repo. Owner/admin only. Sanitizes the new
/// name, rejects a collision under the same owner, then re-keys the repo record and its domain state
/// (issues, PRs, reviews, comments, sessions, code-owners) plus the per-repo side stores (settings,
/// autonomy, CI) and the on-disk keel repo, plus the other `"{tenant}/{repo}"`-keyed stores: claim
/// resolutions (durable human judgments, re-keyed so they follow the repo), the mirror-status ledger,
/// and the in-memory notification buffer (so inbox links resolve under the new name). Cache-only state
/// NOT re-keyed: the review cache and artifact store (both content-addressed / recomputed).
async fn rename_repo_handler(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let acting = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if !is_repo_admin(&app, &tenant, &repo, &acting.id).await {
        return (StatusCode::FORBIDDEN, "only a repo owner/admin can rename this repo").into_response();
    }
    let Some(existing) = find_repo(&app, &tenant, &repo).await else {
        return (StatusCode::NOT_FOUND, "no such repo").into_response();
    };
    let new_name = sanitize_handle(body.get("name").and_then(Value::as_str).unwrap_or(""));
    if new_name.is_empty() {
        return (StatusCode::BAD_REQUEST, "name is required").into_response();
    }
    if new_name == existing.name {
        // No-op rename — nothing to move.
        return Json(json!({ "repo": existing, "tenant": tenant, "name": new_name })).into_response();
    }
    if app.store.repos().await.iter().any(|r| r.owner == existing.owner && r.name.eq_ignore_ascii_case(&new_name)) {
        return (StatusCode::CONFLICT, "a repo with that name already exists").into_response();
    }
    // Move the on-disk keel repo first — if that fails, leave all domain state untouched.
    if let Err(e) = app.repos.rename_repo(&tenant, &repo, &new_name) {
        return (StatusCode::UNPROCESSABLE_ENTITY, format!("could not rename repo on disk: {e}")).into_response();
    }
    let old_key = format!("{tenant}/{repo}");
    let new_key = format!("{tenant}/{new_name}");
    // Re-key the repo record (its id embeds the name) and all domain state.
    let renamed = Repo { id: format!("repo_{tenant}_{new_name}"), name: new_name.clone(), ..existing.clone() };
    app.store.remove_repo(&existing.id).await;
    app.store.put_repo(renamed.clone()).await;
    app.store.rekey_repo_data(&old_key, &new_key).await;
    app.repo_settings.rename(&old_key, &new_key);
    app.autonomy.rename_repo(&tenant, &repo, &new_name);
    app.ci_config.rename(&old_key, &new_key);
    // Other `"{tenant}/{repo}"`-keyed stores that must follow the rename too.
    app.claims.rekey(&old_key, &new_key);
    app.mirror.rekey_repo(&old_key, &new_key);
    app.rekey_notifications(&old_key, &new_key);
    Json(json!({ "repo": renamed, "tenant": tenant, "name": new_name })).into_response()
}

/// `DELETE /api/repos/:tenant/:repo` — delete a repo. Owner/admin only. Removes the repo record, its
/// domain state (issues, PRs, reviews, comments, sessions, code-owners), the per-repo side stores
/// (settings, autonomy, CI endpoint), and the on-disk keel repo. Also purges the other
/// `"{tenant}/{repo}"`-keyed stores: claim resolutions (so stale judgments can't resurface if the name
/// is recreated), the mirror-status ledger, and the notification buffer (so the inbox drops links to
/// the dead repo). Cache-only state (review cache, artifacts) is left to age out — none of it is
/// reachable once the repo record is gone.
async fn delete_repo_handler(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    let acting = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if !is_repo_admin(&app, &tenant, &repo, &acting.id).await {
        return (StatusCode::FORBIDDEN, "only a repo owner/admin can delete this repo").into_response();
    }
    let Some(existing) = find_repo(&app, &tenant, &repo).await else {
        return (StatusCode::NOT_FOUND, "no such repo").into_response();
    };
    let key = format!("{tenant}/{repo}");
    app.store.remove_repo(&existing.id).await;
    app.store.purge_repo_data(&key).await;
    app.repo_settings.delete(&key);
    app.autonomy.delete_repo(&tenant, &repo);
    app.ci_config.delete(&key);
    // Other `"{tenant}/{repo}"`-keyed stores that must be purged too.
    app.claims.remove_repo(&key);
    app.mirror.remove_repo(&key);
    app.purge_notifications(&key);
    if let Err(e) = app.repos.delete_repo(&tenant, &repo) {
        // Domain state is already gone; report the on-disk failure so an operator can clean up.
        return (StatusCode::UNPROCESSABLE_ENTITY, format!("repo record removed but on-disk delete failed: {e}")).into_response();
    }
    (StatusCode::OK, Json(json!({ "deleted": true, "tenant": tenant, "name": repo }))).into_response()
}

/// The RepoSettingsStore key holding an account's default repo settings (inherited by new repos).
fn repo_defaults_key(acct_id: &str) -> String {
    format!("@defaults/{acct_id}")
}

/// `GET /api/accounts/:id/repo-defaults` — the org's default repo settings (owner/admin only).
async fn repo_defaults_get(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id).await {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    Json(settings_value(&app, &app.repo_settings.get(&repo_defaults_key(&acct.id))).await).into_response()
}

/// `PUT /api/accounts/:id/repo-defaults` — set the org's default repo settings (owner/admin only).
/// These are copied into every repo created afterward.
async fn repo_defaults_set(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id).await {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let key = repo_defaults_key(&acct.id);
    let mut s = app.repo_settings.get(&key);
    apply_settings_patch(&app, &mut s, &body).await;
    app.repo_settings.set(&key, s.clone());
    Json(settings_value(&app, &s).await).into_response()
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
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id).await {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    // Explicit loop rather than `.map(async)` — each connection's usage tally now `.await`s.
    let mut conns: Vec<Value> = Vec::new();
    for c in app.store.ai_connections(&acct.id).await.into_iter() {
        let kind = match &c.auth { hull_core::AiAuth::Key { .. } => "key", hull_core::AiAuth::AgentCli { .. } => "agent" };
        let u = app.store.ai_usage(&c.id).await;
        conns.push(json!({
            "id": c.id, "provider": c.provider, "label": c.label, "base_url": c.base_url, "auth_kind": kind,
            "hint": c.auth.hint(), "created_unix": c.created_unix, "token_expires_unix": c.token_expires_unix,
            "usage": { "input_tokens": u.input_tokens, "output_tokens": u.output_tokens, "cost_micros": u.cost_micros, "runs": u.runs, "updated_unix": u.updated_unix },
        }));
    }
    Json(json!({ "connections": conns, "rotate": app.store.ai_rotate(&acct.id).await })).into_response()
}

/// `POST /api/accounts/:id/ai` — connect a backend. Either an API key
/// (`{provider: openai|anthropic|openrouter, api_key, label?, base_url?}`) or a locally-installed
/// **agent CLI** run with the user's own subscription (`{provider: claude-code|codex, label?}`).
/// Owner/admin only.
async fn ai_connection_add(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id).await {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let provider = body.get("provider").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
    let n = app.store.ai_connections(&acct.id).await.len();
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
    app.store.put_ai_connection(conn.clone()).await;
    (StatusCode::CREATED, Json(json!({ "id": conn.id }))).into_response()
}

/// `GET /api/ai/agents` — which local agent CLIs are installed on this Hull host (so the settings UI
/// only offers the ones present). Any signed-in actor may query.
async fn ai_agents_detect(State(app): State<App>, headers: axum::http::HeaderMap) -> Response {
    if authed_actor(&app, &headers).await.is_none() {
        return (StatusCode::UNAUTHORIZED, "sign in required").into_response();
    }
    let agents: Vec<Value> = [("claude-code", "claude", "Claude Code"), ("codex", "codex", "Codex")]
        .iter()
        .map(|(kind, cmd, label)| {
            // Scrub the env + make the child non-dumpable like the other agent-CLI spawns (less
            // sensitive — no token — but same treatment for consistency; still inherits HULL_*/GITHUB_*
            // otherwise).
            let mut c = std::process::Command::new(cmd);
            c.arg("--version");
            ci_sandbox::harden_agent_cli(&mut c);
            let installed = c.output().map(|o| o.status.success()).unwrap_or(false);
            json!({ "kind": kind, "command": cmd, "label": label, "installed": installed })
        })
        .collect();
    Json(json!({ "agents": agents })).into_response()
}

/// `DELETE /api/accounts/:id/ai/:cid` — remove a connection (owner/admin only).
async fn ai_connection_delete(State(app): State<App>, Path((id, cid)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id).await {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    // Wipe the per-user credential bundle for an agent session, if this connection had one.
    if let Some(hull_core::AiAuth::AgentCli { session, .. }) = app.store.ai_connections(&acct.id).await.into_iter().find(|c| c.id == cid).map(|c| c.auth) {
        agentsession::remove(&session);
    }
    Json(json!({ "deleted": app.store.remove_ai_connection(&acct.id, &cid).await })).into_response()
}

/// `POST /api/accounts/:id/ai/agent/start` — `{provider: claude-code|codex}`: begin a per-user
/// subscription login. Provisions the user's bundle, drives `<cli> setup-token` under a PTY, and
/// returns `{session, login_url}` — the browser opens `login_url`, the user approves and pastes the
/// code back to `/complete`. Owner/admin only. Runs the PTY work off the async runtime.
async fn ai_agent_login_start(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let (_acct, _) = match require_account_admin_ref(&app, &headers, &id).await {
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
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id).await {
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
    let n = app.store.ai_connections(&acct.id).await.len();
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
    app.store.put_ai_connection(conn.clone()).await;
    (StatusCode::CREATED, Json(json!({ "id": conn.id, "identity": identity }))).into_response()
}

/// `POST /api/accounts/:id/ai/agent/cancel` — `{session}`: discard an in-flight login (owner/admin).
async fn ai_agent_login_cancel(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(resp) = require_account_admin_ref(&app, &headers, &id).await {
        return resp;
    }
    let session = body.get("session").and_then(Value::as_str).unwrap_or("").trim().to_string();
    // Guard the filesystem sink: `agentsession::remove` joins `session` into a path and
    // `remove_dir_all`s it, so a `..`-laden value from the body could delete a directory outside the
    // sessions root. Session ids are server-generated UUIDs; anything that isn't a safe path segment
    // is not a real session — ignore it rather than let it reach the path helpers.
    if !session.is_empty() && repos::safe_segment(&session) {
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
    // Scrub the inherited server env to a curated allow-list + make the child non-dumpable, so this
    // spawn can't leak HULL_*/GITHUB_*/*_API_KEY (and its config dir isn't readable) via /proc/environ.
    // `harden_agent_cli` must precede the call-specific `.env` (it `env_clear`s).
    let mut cmd = std::process::Command::new(command);
    cmd.args(["auth", "status", "--json"]);
    ci_sandbox::harden_agent_cli(&mut cmd);
    cmd.env("CLAUDE_CONFIG_DIR", dir);
    let out = cmd.output().map_err(|e| format!("{command} auth status: {e}"))?;
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
    // Scrub the env to a curated allow-list + make the child non-dumpable BEFORE attaching the victim's
    // OAuth token: without this the token (and every HULL_*/GITHUB_*/*_API_KEY) would be readable via
    // this child's /proc/<pid>/environ by a same-uid CI job. `harden_agent_cli` `env_clear`s, so it must
    // run before the token/config `.env` calls below.
    let mut cmd = std::process::Command::new("claude");
    cmd.args(["-p", "Reply with the single word: ok"]);
    ci_sandbox::harden_agent_cli(&mut cmd);
    cmd.env("CLAUDE_CODE_OAUTH_TOKEN", token)
        .env("CLAUDE_CONFIG_DIR", &scratch)
        .stdin(std::process::Stdio::null());
    let out = cmd.output();
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
    // Scrub the env to a curated allow-list + make the child non-dumpable (before the CODEX_HOME `.env`,
    // which `harden_agent_cli`'s `env_clear` would otherwise drop), so the codex identity check can't
    // leak HULL_*/GITHUB_*/*_API_KEY via /proc/<pid>/environ.
    let mut cmd = std::process::Command::new("codex");
    cmd.args(["login", "status"]);
    ci_sandbox::harden_agent_cli(&mut cmd);
    cmd.env("CODEX_HOME", dir);
    let status = cmd.output().map_err(|e| format!("codex login status: {e}"))?;
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
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id).await {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let on = body.get("rotate").and_then(Value::as_bool).unwrap_or(false);
    app.store.set_ai_rotate(&acct.id, on).await;
    Json(json!({ "rotate": on })).into_response()
}

/// `GET /api/accounts/:id/github` — the account's GitHub connection status (admin only). Never
/// exposes anything unless the caller administers the account.
async fn github_status(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id).await {
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
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id).await {
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
    let handle = app.store.accounts().await.into_iter().find(|a| a.id == acct_id).map(|a| a.handle).unwrap_or_default();
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
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id).await {
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
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id).await {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    app.connections.remove(&acct.id);
    Json(json!({ "connected": false })).into_response()
}

/// `GET /api/accounts/:id/github/importable` — repos importable via THIS account's connection. Admin
/// only, and only when the account has explicitly connected — never a global list.
async fn github_importable(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap) -> Response {
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id).await {
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
    let (acct, _) = match require_account_admin_ref(&app, &headers, &id).await {
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
    // Sanitize the destination name exactly like `create_repo_handler`: it becomes a filesystem path
    // segment (the import shells `git` into `{tenant}/{name}`) and a store key, so an unsanitized
    // `..`/`/`/dotfile name would bypass the safe-segment guarantee every other create path enforces.
    let name = sanitize_handle(&name);
    if name.is_empty() || !repos::safe_segment(&name) {
        return (StatusCode::UNPROCESSABLE_ENTITY, "invalid repository name").into_response();
    }
    let tenant = acct.handle.clone();
    if app.store.repos().await.iter().any(|r| r.owner == acct.id && r.name.eq_ignore_ascii_case(&name)) {
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
    app.store.put_repo(repo.clone()).await;
    (StatusCode::CREATED, Json(json!({ "repo": repo, "tenant": tenant, "name": name, "detail": res.detail }))).into_response()
}

/// Registered actors (public — no secret keys), each with its accountability root.
async fn actors_list(State(app): State<App>) -> Json<Value> {
    // Explicit loop rather than `.map(async)` — the email lookup + accountability check now `.await`.
    let mut actors: Vec<Value> = Vec::new();
    for a in app.store.actors().await.into_iter() {
        let email = app.store.user_by_actor(&a.id).await.map(|u| u.email).unwrap_or_default();
        // Reflect the real gate: cryptographic verification + revocation, not just structure.
        let is_accountable = accountable(&app, &a).await.is_ok();
        actors.push(json!({
            "id": a.id,
            "handle": a.handle,
            "kind": a.kind,
            "email": email,
            "accountable": is_accountable,
            "revoked": a.revoked,
            "human_root": a.human_principal(),
            "github": app.mirror.github_for(&a.id),
        }));
    }
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
            let parent = match require_actor(&app, &headers, "").await {
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
                        app.store.put_actor(actor.clone()).await;
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
            } else if let Some(user) = app.store.user_by_actor(&parent.id).await {
                if user.secret_key.is_empty() {
                    // SOVEREIGN (non-custodial) account: Hull holds no signing key for this user and must
                    // NOT sign. The client signs the delegation itself and re-sends the pair (verified
                    // by the client-signed branch above) — the whole point of a sovereign identity.
                    return (StatusCode::UNPROCESSABLE_ENTITY, "sovereign account: sign the delegation client-side and send { child_pub, delegation_sig } (Hull holds no key for you)").into_response();
                }
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
    app.store.put_actor(minted.actor.clone()).await;
    (StatusCode::CREATED, Json(json!({ "actor": minted.actor, "secret_key": minted.secret_key }))).into_response()
}

/// Revoke an actor (`POST /api/actors/:id/revoke`). Only an **ancestor** may revoke — the caller must
/// be the target itself or appear as a principal in the target's delegation chain (its human root or
/// an intermediate agent). Revocation propagates: because the revoked id sits in every descendant's
/// chain, [`accountable`] then rejects the whole subtree. Blast radius = the subtree.
async fn revoke_actor(State(app): State<App>, headers: axum::http::HeaderMap, Path(id): Path<String>) -> Response {
    let caller = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let Some(mut target) = app.store.actor(&id).await else {
        return (StatusCode::NOT_FOUND, "no such actor").into_response();
    };
    let ancestor = target.id == caller.id
        || target.delegation.as_ref().map(|d| d.chain.iter().any(|h| h.principal == caller.id)).unwrap_or(false);
    if !ancestor {
        return (StatusCode::FORBIDDEN, "you may only revoke an actor in your own delegation subtree").into_response();
    }
    target.revoked = true;
    app.store.put_actor(target).await;
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
    let caller = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let Some(mut target) = app.store.actor(&id).await else {
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
    app.store.put_actor(target).await;
    Json(json!({ "renewed": id, "expires_unix": expires_unix })).into_response()
}

/// Link your forge (GitHub) login to your hull actor (`POST /api/actors/:id/github` `{login}`).
/// Self-only. This is the accountability map across the mirror (NEW-1176): git commits you author on
/// GitHub, imported into Hull, then resolve to **you** (an accountable hull actor) instead of an
/// anonymous external identity. `login: ""` clears the link.
async fn link_github(State(app): State<App>, headers: axum::http::HeaderMap, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let caller = match require_actor(&app, &headers, "").await {
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
    let caller = match require_actor(&app, &headers, "").await {
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
    let Some(mut actor) = app.store.actor(&id).await else {
        return (StatusCode::NOT_FOUND, "no such actor").into_response();
    };
    actor.nostr_pubkey = (!pubkey.is_empty()).then_some(pubkey);
    app.store.put_actor(actor).await;
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
    match app.store.actor(&actor).await {
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
    app.auth.lock().unwrap().tokens.insert(token.clone(), (actor.clone(), now()));
    (StatusCode::CREATED, Json(json!({ "token": token, "actor": actor }))).into_response()
}

/// `GET /api/auth/me` (Bearer token) — the authenticated actor, or 401.
async fn auth_me(State(app): State<App>, headers: axum::http::HeaderMap) -> Response {
    match authed_actor(&app, &headers).await {
        Some(a) => {
            let user = app.store.user_by_actor(&a.id).await;
            Json(json!({
                "id": a.id, "handle": a.handle, "kind": a.kind, "accountable": a.is_accountable(),
                "username": user.as_ref().map(|u| u.username.clone()),
                "email": user.as_ref().map(|u| u.email.clone()),
            })).into_response()
        }
        None => (StatusCode::UNAUTHORIZED, "not signed in").into_response(),
    }
}

/// `DELETE /api/auth/session` (Bearer token) — log out by dropping the presented session token, so a
/// client can proactively invalidate its own credential rather than wait for TTL expiry. Idempotent:
/// an already-absent (or missing) token still returns success. No 401 gate — logging out with a bad
/// token is a no-op, not an error.
async fn auth_logout(State(app): State<App>, headers: axum::http::HeaderMap) -> Response {
    if let Some(token) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        app.auth.lock().unwrap().tokens.remove(token);
    }
    StatusCode::NO_CONTENT.into_response()
}

/// The signed-in actor's full profile (`GET /api/me`): identity, **accountability chain** (for an
/// agent, the delegation hops back to its human root), and org memberships + roles. The mirror of
/// "who am I and what am I allowed to be" — read-only; there is no key rotation because the actor id
/// *is* the public key (rotating it would be a different actor).
async fn me_profile(State(app): State<App>, headers: axum::http::HeaderMap) -> Response {
    let Some(a) = authed_actor(&app, &headers).await else {
        return (StatusCode::UNAUTHORIZED, "not signed in").into_response();
    };
    // Explicit loop rather than a `.map` over a closure that `.await`s the handle lookup.
    let mut chain: Vec<Value> = Vec::new();
    if let Some(d) = a.delegation.as_ref() {
        for h in d.chain.iter() {
            let handle = app.store.actor(&h.principal).await.map(|x| x.handle).unwrap_or_else(|| h.principal.chars().take(10).collect());
            chain.push(json!({ "principal": h.principal, "handle": handle, "kind": h.kind, "scope": h.scope }));
        }
    }
    let memberships: Vec<Value> = app
        .store
        .accounts()
        .await
        .into_iter()
        .filter_map(|acct| {
            acct.members.iter().find(|m| m.actor == a.id).map(|m| json!({ "account": acct.handle, "role": m.role }))
        })
        .collect();
    let user = app.store.user_by_actor(&a.id).await;
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
    if app.store.user_by_username(&username).await.is_some() {
        return (StatusCode::CONFLICT, "that username is taken").into_response();
    }
    let uuid = Uuid::new_v4();
    match app.webauthn.start_passkey_registration(uuid, &username, &username, None) {
        Ok((ccr, state)) => {
            let flow = identity::random_hex(16);
            {
                let mut a = app.auth.lock().unwrap();
                a.reg_flows.retain(|_, f| now().saturating_sub(f.created_unix) < CEREMONY_TTL_SECS);
                a.reg_flows.insert(flow.clone(), passkey::RegFlow { username, email, uuid, state, created_unix: now() });
            }
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
    // Serialize the passkey up front — if it can't be serialized we must NOT save the user with a Null
    // credential (which would return success yet lock them out); fail the registration instead.
    let pk_data = match serde_json::to_value(&pk) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("could not persist passkey: {e}")).into_response(),
    };
    // Re-check username uniqueness at FINISH: `register_start`'s check is TOCTOU (a flow started while
    // the name was free could otherwise create a second user + personal account with a duplicate
    // handle, or panic on Postgres's `users_lower_username` UNIQUE index). Reject gracefully instead.
    if app.store.user_by_username(&flow.username).await.is_some() {
        return (StatusCode::CONFLICT, "that username was taken while you were registering").into_response();
    }
    let user = User {
        id: flow.uuid.to_string(),
        username: flow.username.clone(),
        email: flow.email,
        actor: minted.actor.id.clone(),
        secret_key: minted.secret_key,
        wrapped_key: None,
        passkeys: vec![PasskeyCred { id: cred_id, name: "passkey".into(), created_unix: now(), data: pk_data }],
        created_unix: now(),
        bio: String::new(),
    };
    app.store.put_actor(minted.actor.clone()).await;
    app.store.put_user(user.clone()).await;
    app.store.put_account(Account {
        id: format!("acct_{}", flow.uuid),
        kind: AccountKind::Personal,
        handle: user.username.clone(),
        members: vec![Membership { actor: user.actor.clone(), role: Role::Owner }],
    }).await;
    let token = identity::random_hex(24);
    app.auth.lock().unwrap().tokens.insert(token.clone(), (user.actor.clone(), now()));
    (StatusCode::CREATED, Json(json!({ "token": token, "actor": user.actor, "username": user.username }))).into_response()
}

/// `POST /api/auth/sovereign/register` — `{username, email, pubkey, wrapped_key, signature}` creates a
/// SOVEREIGN (non-custodial) account. The human's Ed25519 key lives client-side; Hull stores only the
/// public key (as the actor id) and the passphrase-encrypted secret bundle (`wrapped_key`, opaque to
/// Hull — it holds no passphrase). `signature` is the client's Ed25519 signature over
/// `hull-sovereign:v1\nusername=<u>\npubkey=<pk>` — proof it holds the secret, so no one can bind a key
/// (or squat a username) they don't control. Login afterward uses the normal challenge→sign flow (the
/// client decrypts its key with the passphrase to sign the nonce); Hull never signs for this account.
async fn sovereign_register(State(app): State<App>, Json(body): Json<Value>) -> Response {
    let username = sanitize_handle(body.get("username").and_then(Value::as_str).unwrap_or("").trim());
    let email = body.get("email").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let pubkey = body.get("pubkey").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let wrapped = body.get("wrapped_key").and_then(Value::as_str).unwrap_or("").to_string();
    let signature = body.get("signature").and_then(Value::as_str).unwrap_or("");
    if username.is_empty() {
        return (StatusCode::BAD_REQUEST, "username must contain at least one letter or digit").into_response();
    }
    if wrapped.is_empty() {
        return (StatusCode::BAD_REQUEST, "wrapped_key is required (the passphrase-encrypted secret)").into_response();
    }
    // Validate the public key and build the human actor from it.
    let Some(actor) = identity::human_from_pubkey(&username, &pubkey) else {
        return (StatusCode::BAD_REQUEST, "pubkey must be a 32-byte hex Ed25519 public key").into_response();
    };
    // Proof of possession: the client signs the exact (username, pubkey) binding with the secret, so a
    // caller can't register a key it doesn't hold or squat a username against someone else's key.
    let msg = format!("hull-sovereign:v1\nusername={username}\npubkey={pubkey}");
    // STRICT verify: rejects small-order / non-canonical keys, so nobody can bind a "public" id whose
    // signatures anyone can forge (a self-verifying small-order key would otherwise satisfy the PoP).
    if !identity::verify_strict(&pubkey, msg.as_bytes(), signature) {
        return (StatusCode::UNAUTHORIZED, "signature does not prove possession of the secret key").into_response();
    }
    if app.store.user_by_username(&username).await.is_some() {
        return (StatusCode::CONFLICT, "that username is taken").into_response();
    }
    if app.store.actor(&actor.id).await.is_some() {
        return (StatusCode::CONFLICT, "that key is already registered").into_response();
    }
    let uuid = identity::random_hex(16);
    let user = User {
        id: uuid.clone(),
        username: username.clone(),
        email,
        actor: actor.id.clone(),
        secret_key: String::new(), // NON-CUSTODIAL: Hull holds no signing key for this user
        wrapped_key: Some(wrapped),
        passkeys: vec![],
        created_unix: now(),
        bio: String::new(),
    };
    app.store.put_actor(actor.clone()).await;
    app.store.put_user(user.clone()).await;
    app.store
        .put_account(Account {
            id: format!("acct_{uuid}"),
            kind: AccountKind::Personal,
            handle: username.clone(),
            members: vec![Membership { actor: actor.id.clone(), role: Role::Owner }],
        })
        .await;
    let token = identity::random_hex(24);
    app.auth.lock().unwrap().tokens.insert(token.clone(), (actor.id.clone(), now()));
    (StatusCode::CREATED, Json(json!({ "token": token, "actor": actor.id, "username": username }))).into_response()
}

/// `GET /api/auth/sovereign/wrapped?username=X` — the passphrase-encrypted key bundle for a sovereign
/// account, so the client can decrypt it (with the passphrase) and sign the login challenge from any
/// device. The bundle is passphrase-protected — Hull can't read it — so serving it pre-auth is the
/// standard encrypted-vault model. 404 for unknown or custodial accounts (no custodial-vs-missing
/// oracle: both are 404).
///
/// SECURITY: because this is unauthenticated, an attacker can harvest a username's bundle and brute-
/// force the passphrase OFFLINE — so the whole account's security rests on the CLIENT KDF. The browser
/// MUST wrap with a strong memory-hard KDF (Argon2id, high params); Hull can't verify that on an opaque
/// blob. Deployment follow-up: rate-limit this route (and it enumerates sovereign usernames: 200 vs 404).
async fn sovereign_wrapped(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Response {
    let username = sanitize_handle(q.get("username").map(String::as_str).unwrap_or(""));
    match app.store.user_by_username(&username).await.and_then(|u| u.wrapped_key.map(|w| (u.actor, w))) {
        Some((actor, wrapped)) => Json(json!({ "actor": actor, "wrapped_key": wrapped })).into_response(),
        None => (StatusCode::NOT_FOUND, "no sovereign account with that username").into_response(),
    }
}

/// `POST /api/auth/passkey/start` — `{username}` → a WebAuthn assertion challenge for that account.
async fn passkey_start(State(app): State<App>, Json(body): Json<Value>) -> Response {
    let username = body.get("username").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let Some(user) = app.store.user_by_username(&username).await else {
        return (StatusCode::NOT_FOUND, "no account with that username").into_response();
    };
    let passkeys: Vec<Passkey> = user.passkeys.iter().filter_map(|p| serde_json::from_value(p.data.clone()).ok()).collect();
    if passkeys.is_empty() {
        return (StatusCode::BAD_REQUEST, "this account has no passkeys").into_response();
    }
    match app.webauthn.start_passkey_authentication(&passkeys) {
        Ok((rcr, state)) => {
            let flow = identity::random_hex(16);
            {
                let mut a = app.auth.lock().unwrap();
                a.auth_flows.retain(|_, f| now().saturating_sub(f.created_unix) < CEREMONY_TTL_SECS);
                a.auth_flows.insert(flow.clone(), passkey::AuthFlow { user_id: user.id.clone(), state, created_unix: now() });
            }
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
    let Some(mut user) = app.store.user(&flow.user_id).await else {
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
    app.store.put_user(user.clone()).await;
    let token = identity::random_hex(24);
    app.auth.lock().unwrap().tokens.insert(token.clone(), (user.actor.clone(), now()));
    Json(json!({ "token": token, "actor": user.actor, "username": user.username })).into_response()
}

/// `GET /api/account` — the signed-in user's hosted account (username, email, passkeys).
async fn account_get(State(app): State<App>, headers: axum::http::HeaderMap) -> Response {
    let Some(a) = authed_actor(&app, &headers).await else { return (StatusCode::UNAUTHORIZED, "not signed in").into_response(); };
    let Some(user) = app.store.user_by_actor(&a.id).await else {
        return (StatusCode::NOT_FOUND, "this actor is a legacy key login, not a hosted account").into_response();
    };
    let passkeys: Vec<Value> = user.passkeys.iter().map(|p| json!({ "id": p.id, "name": p.name, "created_unix": p.created_unix })).collect();
    Json(json!({ "username": user.username, "email": user.email, "bio": user.bio, "actor": user.actor, "passkeys": passkeys, "created_unix": user.created_unix })).into_response()
}

/// `PUT /api/account` — change username and/or email. Keeps the actor + personal-account handle in sync.
async fn account_update(State(app): State<App>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let Some(a) = authed_actor(&app, &headers).await else { return (StatusCode::UNAUTHORIZED, "not signed in").into_response(); };
    let Some(mut user) = app.store.user_by_actor(&a.id).await else {
        return (StatusCode::NOT_FOUND, "not a hosted account").into_response();
    };
    if let Some(raw) = body.get("username").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        // The username doubles as the actor + personal-account HANDLE (a path segment / tenant), so it
        // must be sanitized like every other create/rename path — a raw value here would otherwise
        // corrupt the handle and could lock the user out of passkey login.
        let un = sanitize_handle(raw);
        if un.is_empty() {
            return (StatusCode::BAD_REQUEST, "username must contain at least one letter or digit").into_response();
        }
        if let Some(other) = app.store.user_by_username(&un).await {
            if other.id != user.id {
                return (StatusCode::CONFLICT, "that username is taken").into_response();
            }
        }
        user.username = un.clone();
        // keep the display handle on the actor + personal account aligned with the username
        if let Some(mut actor) = app.store.actor(&user.actor).await {
            actor.handle = un.clone();
            app.store.put_actor(actor).await;
        }
        let acct_id = format!("acct_{}", user.id);
        if let Some(mut acct) = app.store.accounts().await.into_iter().find(|x| x.id == acct_id) {
            acct.handle = un.clone();
            app.store.put_account(acct).await;
        }
    }
    if let Some(em) = body.get("email").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        user.email = em.to_string();
    }
    if let Some(bio) = body.get("bio").and_then(Value::as_str) {
        user.bio = bio.chars().take(280).collect();
    }
    app.store.put_user(user.clone()).await;
    Json(json!({ "username": user.username, "email": user.email, "bio": user.bio })).into_response()
}

/// `POST /api/account/passkeys/start` — begin adding another passkey to the signed-in account.
async fn account_passkey_start(State(app): State<App>, headers: axum::http::HeaderMap) -> Response {
    let Some(a) = authed_actor(&app, &headers).await else { return (StatusCode::UNAUTHORIZED, "not signed in").into_response(); };
    let Some(user) = app.store.user_by_actor(&a.id).await else { return (StatusCode::NOT_FOUND, "not a hosted account").into_response(); };
    let Ok(uuid) = Uuid::parse_str(&user.id) else { return (StatusCode::INTERNAL_SERVER_ERROR, "bad user id").into_response(); };
    let exclude: Vec<_> = user.passkeys.iter().filter_map(|p| serde_json::from_value::<Passkey>(p.data.clone()).ok().map(|pk| pk.cred_id().clone())).collect();
    match app.webauthn.start_passkey_registration(uuid, &user.username, &user.username, Some(exclude)) {
        Ok((ccr, state)) => {
            let flow = identity::random_hex(16);
            {
                let mut a = app.auth.lock().unwrap();
                a.add_flows.retain(|_, f| now().saturating_sub(f.created_unix) < CEREMONY_TTL_SECS);
                a.add_flows.insert(flow.clone(), passkey::AddFlow { user_id: user.id.clone(), state, created_unix: now() });
            }
            Json(json!({ "flow_id": flow, "options": ccr })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("could not start: {e}")).into_response(),
    }
}

/// `POST /api/account/passkeys/finish` — `{flow_id, credential, name?}` → store the new passkey.
async fn account_passkey_finish(State(app): State<App>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let Some(a) = authed_actor(&app, &headers).await else { return (StatusCode::UNAUTHORIZED, "not signed in").into_response(); };
    let flow_id = body.get("flow_id").and_then(Value::as_str).unwrap_or("").to_string();
    let name = body.get("name").and_then(Value::as_str).unwrap_or("passkey").trim().to_string();
    let cred = body.get("credential").cloned().unwrap_or(Value::Null);
    let Some(flow) = app.auth.lock().unwrap().add_flows.remove(&flow_id) else {
        return (StatusCode::BAD_REQUEST, "unknown or expired flow").into_response();
    };
    let Some(mut user) = app.store.user_by_actor(&a.id).await else { return (StatusCode::NOT_FOUND, "not a hosted account").into_response(); };
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
    // Serialize up front and fail on error — mirror `register_finish`. Storing `Null` here (the old
    // `unwrap_or`) returned 200 with a passkey that `passkey_start` silently drops, so the user thinks
    // they added a working credential but can never authenticate with it.
    let pk_data = match serde_json::to_value(&pk) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("could not persist passkey: {e}")).into_response(),
    };
    user.passkeys.push(PasskeyCred {
        id: b64u(pk.cred_id().as_ref()),
        name: if name.is_empty() { "passkey".into() } else { name },
        created_unix: now(),
        data: pk_data,
    });
    app.store.put_user(user.clone()).await;
    let passkeys: Vec<Value> = user.passkeys.iter().map(|p| json!({ "id": p.id, "name": p.name, "created_unix": p.created_unix })).collect();
    Json(json!({ "passkeys": passkeys })).into_response()
}

/// `DELETE /api/account/passkeys/:cred` — remove a passkey (never the last one).
async fn account_passkey_delete(State(app): State<App>, headers: axum::http::HeaderMap, Path(cred): Path<String>) -> Response {
    let Some(a) = authed_actor(&app, &headers).await else { return (StatusCode::UNAUTHORIZED, "not signed in").into_response(); };
    let Some(mut user) = app.store.user_by_actor(&a.id).await else { return (StatusCode::NOT_FOUND, "not a hosted account").into_response(); };
    if user.passkeys.len() <= 1 {
        return (StatusCode::BAD_REQUEST, "cannot remove your only passkey").into_response();
    }
    let before = user.passkeys.len();
    user.passkeys.retain(|p| p.id != cred);
    if user.passkeys.len() == before {
        return (StatusCode::NOT_FOUND, "no such passkey").into_response();
    }
    app.store.put_user(user.clone()).await;
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
            // Compare fixed-size SHA-256 digests so the check is length-independent: a plain
            // `len() == len()` short-circuit (or a byte-zip) leaks the secret's length via timing.
            use sha2::{Digest, Sha256};
            let ph = Sha256::digest(presented.as_bytes());
            let sh = Sha256::digest(s.as_bytes());
            let ok = ct_eq(&ph, &sh);
            if ok {
                Ok(())
            } else {
                Err((StatusCode::UNAUTHORIZED, format!("bad or missing {header}")).into_response())
            }
        }
        _ => Err((StatusCode::FORBIDDEN, "endpoint disabled: no shared secret configured").into_response()),
    }
}

/// Resolve the `Authorization: Bearer <token>` header to its actor, if valid. A token older than
/// [`SESSION_TTL_SECS`] is rejected and dropped; expired entries are pruned opportunistically on the
/// same lock so the map can't grow without bound.
async fn authed_actor(app: &App, headers: &axum::http::HeaderMap) -> Option<Actor> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))?;
    actor_for_token(app, token).await
}

/// Resolve a **raw** session token (no scheme prefix) to its actor, applying the same expiry prune
/// as [`authed_actor`]. Factored out so non-`Bearer` credential paths (git HTTP Basic, where the
/// token arrives as the Basic password) resolve identity through the exact same token map.
async fn actor_for_token(app: &App, token: &str) -> Option<Actor> {
    let now = now();
    let actor_id = {
        let mut a = app.auth.lock().unwrap();
        // Opportunistic prune: drop every expired token while we hold the lock.
        a.tokens.retain(|_, (_, issued)| now.saturating_sub(*issued) < SESSION_TTL_SECS);
        // The presented token survives the prune iff it's present and unexpired.
        a.tokens.get(token).map(|(actor, _)| actor.clone())?
    };
    app.store.actor(&actor_id).await
}

/// The authoring identity: the **authenticated** actor (Bearer token) when signed in, else the
/// body-supplied `actor_id` (for curl/scripts). Either way it must be accountable.
#[allow(clippy::result_large_err)]
/// The accountable actor for a mutating request. **Identity comes only from a valid session token**
/// (proven Ed25519 key possession) — never from a client-supplied actor id. No token ⇒ 401. This is
/// what makes "act as anyone" impossible: you are whoever you signed in as, nobody else. The
/// `_actor_id` argument (a body field some handlers still pass) is ignored, kept only so call sites
/// don't churn.
async fn require_actor(app: &App, headers: &axum::http::HeaderMap, _actor_id: &str) -> Result<Actor, Response> {
    match authed_actor(app, headers).await {
        Some(a) => match accountable(app, &a).await {
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
async fn accountable(app: &App, a: &Actor) -> Result<(), String> {
    if a.revoked {
        return Err("this actor has been revoked".into());
    }
    match a.kind {
        hull_core::ActorKind::Human => Ok(()),
        hull_core::ActorKind::Agent => {
            let deleg = a.delegation.as_ref().ok_or("agent carries no delegation — unaccountable")?;
            // `Delegation::verify` takes a SYNC revocation-check closure, but the store is now async.
            // Pre-fetch the revoked status of every principal in the chain (the only ids `verify`
            // queries), then hand `verify` a closure that reads from that set — identical semantics.
            let mut revoked = std::collections::HashSet::new();
            for hop in &deleg.chain {
                if app.store.actor(&hop.principal).await.map(|x| x.revoked).unwrap_or(false) {
                    revoked.insert(hop.principal.clone());
                }
            }
            let is_revoked = |id: &str| revoked.contains(id);
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
    headers: axum::http::HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    let path = q.get("path").map(String::as_str).unwrap_or("");
    let prov = app.repos.why(&tenant, &repo, path, 10);
    Json(json!({ "path": path, "provenance": prov })).into_response()
}

/// Branch names for a repo (`GET /api/repos/:tenant/:repo/branches`).
async fn repo_branches(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    Json(json!({ "branches": app.repos.branches(&tenant, &repo) })).into_response()
}

/// A directory listing at a branch (`GET /api/repos/:tenant/:repo/tree?ref=<branch>&path=<dir>`).
async fn repo_tree(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    let ref_name = q.get("ref").map(String::as_str).filter(|s| !s.is_empty()).unwrap_or("main");
    // `?flat=1` → every file path in the branch (for the full file-tree view).
    if q.get("flat").is_some_and(|v| v == "1" || v == "true") {
        return Json(json!({ "ref": ref_name, "paths": app.repos.all_paths(&tenant, &repo, ref_name) })).into_response();
    }
    let path = q.get("path").map(String::as_str).unwrap_or("");
    Json(json!({ "ref": ref_name, "path": path, "entries": app.repos.list_tree(&tenant, &repo, ref_name, path) })).into_response()
}

/// A file's contents at a branch (`GET /api/repos/:tenant/:repo/blob?ref=<branch>&path=<file>`).
/// Returns text when decodable, plus a `binary` flag and byte size so the UI can render sensibly.
async fn repo_blob(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    let ref_name = q.get("ref").map(String::as_str).filter(|s| !s.is_empty()).unwrap_or("main");
    let path = q.get("path").map(String::as_str).unwrap_or("");
    match app.repos.read_file_at(&tenant, &repo, ref_name, path) {
        Some(bytes) => {
            let binary = bytes.iter().take(8000).any(|&b| b == 0);
            let size = bytes.len();
            let text = if binary { String::new() } else { String::from_utf8_lossy(&bytes).into_owned() };
            Json(json!({ "path": path, "ref": ref_name, "size": size, "binary": binary, "text": text })).into_response()
        }
        None => Json(json!({ "path": path, "ref": ref_name, "missing": true })).into_response(),
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
    if let Err(resp) = require_repo_read(&app, &headers, &tenant, &repo).await {
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
    headers: axum::http::HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    let ref_name = q.get("ref").map(String::as_str).filter(|s| !s.is_empty()).unwrap_or("main");
    let query = q.get("q").map(String::as_str).unwrap_or("");
    Json(json!({ "q": query, "ref": ref_name, "hits": app.repos.search(&tenant, &repo, ref_name, query) })).into_response()
}

/// A repo's code-owner rules (`GET /api/repos/:tenant/:repo/owners`).
async fn owners_list(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    Json(json!({ "owners": app.store.owners(&format!("{tenant}/{repo}")).await })).into_response()
}

/// Set a repo's code-owner rules (`POST …/owners` with `{rules: [{glob, owners:[actorId]}]}`),
/// gated to an accountable actor.
async fn set_owners(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let actor = match require_actor(&app, &headers, body.get("actor").and_then(Value::as_str).unwrap_or("")).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // Code owners drive review routing and the merge gate — only a repo owner/admin may rewrite them.
    if !is_repo_admin(&app, &tenant, &repo, &actor.id).await {
        return (StatusCode::FORBIDDEN, "only a repo owner/admin can set code owners").into_response();
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
    app.store.set_owners(&format!("{tenant}/{repo}"), rules.clone()).await;
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

async fn owners_for(app: &App, repo_key: &str, files: &[String]) -> Vec<String> {
    let mut set: Vec<String> = Vec::new();
    for rule in app.store.owners(repo_key).await {
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
            let actors = app.store.actors().await;
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
async fn repo_security(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    Json(json!({ "secrets": app.repos.secrets(&format!("{tenant}/{repo}")) })).into_response()
}

/// The diff of a change (`GET /api/repos/:tenant/:repo/change/:id/diff`): per-file line hunks plus a
/// semantic-operations summary — the review's diff viewer.
async fn change_diff(State(app): State<App>, Path((tenant, repo, id)): Path<(String, String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    Json(json!({ "files": app.repos.diff(&tenant, &repo, &id) })).into_response()
}

/// Full old + new text of one file at a change (`GET …/change/:id/file?path=…`). The diff viewer
/// calls this to *expand unmodified lines* — the patch alone only carries the hunks' context, so
/// pierre needs the whole file to reveal the rest. `null` on either side = a pure add or delete.
async fn change_file(
    State(app): State<App>,
    Path((tenant, repo, id)): Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
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
async fn change_semantic(State(app): State<App>, Path((tenant, repo, id)): Path<(String, String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    Json(json!({ "semantic": app.repos.semantic_summary(&tenant, &repo, &id) })).into_response()
}

/// **keel-native content-addressed source fetch** (`GET …/tree/:tree/tar`): the change's keel tree,
/// addressed by its `tree_id`, materialized and streamed as a tar archive. This is how a CI or
/// reviewer runner obtains source — by content address, over keel, **not** `git clone`. (Hull's git
/// smart-HTTP endpoints exist only for interop/mirroring, never as the runner fetch path.) The
/// archive is verifiable: re-hashing the tree reproduces `tree`.
async fn tree_archive(State(app): State<App>, Path((tenant, repo, tree)): Path<(String, String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
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
async fn change_ledger(State(app): State<App>, Path((tenant, repo, id)): Path<(String, String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    let Some(info) = app.repos.change_info(&tenant, &repo, &id) else {
        return Json(json!({ "ledger": null })).into_response();
    };
    // Narrative: the change intent, plus the lesson from a native or ingested session.
    let lesson = match info.session.as_ref().map(|s| s.lesson.clone()) {
        Some(l) => l,
        None => app.store.session_record(&format!("{tenant}/{repo}"), &id).await.map(|s| s.lesson).unwrap_or_default(),
    };
    let facts = facts_with_independence(&app, &tenant, &repo, &id).await;
    // C1 — fold in the acceptance criteria of any issue a PR proposing this change closes, so the
    // standalone ledger matches what the review reconciled against.
    let key = format!("{tenant}/{repo}");
    let review_intent = {
        let issues = app.store.issues(&key).await;
        let acceptance: Vec<String> = app
            .store
            .prs(&key)
            .await
            .into_iter()
            .filter(|p| p.changes.contains(&id))
            .flat_map(|p| {
                let body = p.changes.iter().filter_map(|c| app.repos.change_info(&tenant, &repo, c).map(|i| i.intent)).collect::<Vec<_>>().join("\n");
                closing_issue_numbers(&p.title, &body, &[])
            })
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
                    let handle = app.store.actor(&r.by).await.map(|a| a.handle).unwrap_or_else(|| r.by.chars().take(8).collect());
                    claim["resolution"] = json!({ "judgment": r.judgment, "note": r.note, "by": handle, "ts": r.ts });
                }
            }
        }
    }
    Json(json!({ "ledger": val })).into_response()
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
    let actor = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // Resolving a reconciliation claim is a review judgment on the repo — repo members only.
    if !is_repo_member(&app, &tenant, &repo, &actor.id).await {
        return (StatusCode::FORBIDDEN, "only a repo member may resolve claims").into_response();
    }
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
async fn change_info(State(app): State<App>, Path((tenant, repo, id)): Path<(String, String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    match app.repos.change_info(&tenant, &repo, &id) {
        Some(mut info) => {
            // If the change carries no NATIVE keel session (e.g. it arrived over git), fall back to
            // a session ingested for it (`keel capture` → POST …/session) — the session-carrying
            // bridge across the git boundary.
            if info.session.is_none() {
                if let Some(sr) = app.store.session_record(&format!("{tenant}/{repo}"), &id).await {
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
            Json(json!({ "change": info })).into_response()
        }
        None => Json(json!({ "change": null })).into_response(),
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
    let actor = match require_actor(&app, &headers, body.get("actor").and_then(Value::as_str).unwrap_or("")).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // Running checks executes the change's own test command on the host — gate to repo members so an
    // outsider can't drive the runner (or force-bust its memo) on a repo they don't belong to.
    if !is_repo_member(&app, &tenant, &repo, &actor.id).await {
        return (StatusCode::FORBIDDEN, "only a repo member may run checks").into_response();
    }
    let force = body.get("force").and_then(Value::as_bool).unwrap_or(false);
    match resolve_check(&app, &tenant, &repo, &id, force).await {
        CiResolution::Done(o) => {
            let status = ci_status_str(o.status);
            if matches!(o.status, hull_plugin::CiStatus::Green | hull_plugin::CiStatus::Red) {
                notify_ci(&app, &tenant, &repo, &id, status, &o.summary).await;
            }
            // Auto-triage a failed check: an independent agent reviews the failing change and posts
            // findings, so a red result isn't a dead end. Runs in the background; the memoized red
            // verdict makes the agent's own check run instant.
            if matches!(o.status, hull_plugin::CiStatus::Red) {
                let key = format!("{tenant}/{repo}");
                if let Some(pr) = app.store.prs(&key).await.into_iter().find(|p| p.changes.iter().any(|c| c.starts_with(&id) || id.starts_with(c.as_str()))) {
                    if let Some(agent) = independent_agent_reviewer(&app, &tenant, &repo, &pr.author).await {
                        let (app2, t2, r2, n2) = (app.clone(), tenant.clone(), repo.clone(), pr.number);
                        tokio::spawn(async move { let _ = perform_auto_review(&app2, &t2, &r2, n2, &agent, 0).await; });
                    } else {
                        eprintln!(
                            "CI-red auto-review skipped for {tenant}/{repo} PR !{}: no org-member agent reviewer is registered",
                            pr.number
                        );
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

/// RAII release of a built-in-local-runner inflight slot. The local runner executes on a
/// `spawn_blocking` task and the handler `.await`s its completion; if the axum handler future is
/// cancelled (client disconnects while parked on that `.await`), the blocking task keeps running to
/// completion but the handler code after the `.await` never runs. Clearing the slot in `Drop`
/// guarantees release on EVERY exit — success, error, panic, and cancellation — so a cancelled run
/// can no longer wedge the tree in the inflight set until process restart. The cancelled run's
/// orphan simply finishes into its own unique `CI_SEQ` dir, so releasing the slot for a subsequent
/// run causes no collision. `active` is set to whether THIS invocation actually claimed the slot
/// (`mark_inflight` returned true): a `force` run that ran while another run already held the claim
/// must not clear that other run's slot.
struct InflightGuard {
    ci_config: Arc<ci::CiConfig>,
    tree: String,
    active: bool,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if self.active {
            self.ci_config.clear_inflight(&self.tree);
        }
    }
}

/// Trigger a change's checks. If the repo (or the instance) configures an external CI endpoint, POST
/// the standard job payload there and return — Hull owns no queue and waits for a callback.
/// Otherwise run the built-in local runner inline. A content-addressed memo hit short-circuits both.
async fn resolve_check(app: &App, tenant: &str, repo: &str, change: &str, force: bool) -> CiResolution {
    let Some(tree) = app.repos.change_tree(tenant, repo, change) else {
        return CiResolution::Failed("unknown change".into());
    };
    // Memo hit: an identical tree THIS tenant already judged — no dispatch, no run.
    if !force {
        if let Some(o) = app.ci.get_memoized(tenant, &tree) {
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
            // Guard against duplicate concurrent runs of the same (repo,tree) the same way the dispatch
            // path does: `mark_inflight` atomically claims the tree, so a second inline run that races
            // in gets `Pending` instead of doing redundant work. `force` still runs even when the tree
            // is already claimed by another run, but must never release that other run's slot.
            let claimed = app.ci_config.mark_inflight(&tree);
            if !force && !claimed {
                return CiResolution::Pending;
            }
            // Release the slot via RAII so it clears on EVERY exit path — including cancellation of
            // this handler future while parked on the `.await` below. `active = claimed` means the
            // guard only clears the slot when THIS invocation was the claimer (a forced run that
            // piggybacked on another run's outstanding claim leaves that claim intact).
            let _inflight = InflightGuard { ci_config: app.ci_config.clone(), tree: tree.clone(), active: claimed };
            let (repos, registry, ci) = (app.repos.clone(), app.registry.clone(), app.ci.clone());
            let (t, r, c) = (tenant.to_string(), repo.to_string(), change.to_string());
            let outcome = tokio::task::spawn_blocking(move || ci::run_check(&repos, &registry, &ci, &t, &r, &c, force))
                .await
                .unwrap_or(hull_plugin::CiOutcome { status: hull_plugin::CiStatus::Errored, summary: "runner panicked".into(), memoized: false });
            // `_inflight` drops here (or when the future is cancelled), clearing the slot iff claimed.
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

async fn notify_ci(app: &App, tenant: &str, repo: &str, change: &str, status: &str, summary: &str) {
    app.registry.notify(&hull_plugin::NotifyEvent {
        kind: if status == "green" { "ci_passed".into() } else { "ci_failed".into() },
        to: vec![],
        summary: format!("checks {status} for {tenant}/{repo}@{}: {}", change.chars().take(12).collect::<String>(), summary),
        change: Some(change.to_string()),
        repo: Some(format!("{tenant}/{repo}")),
        target_kind: None,
        target_number: None,
    })
    .await;
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
    // The `ci-result` callback is the EXTERNAL-CI half of the contract. The built-in local runner
    // reports its verdict in-process (see `resolve_check`), so when no external CI is configured for
    // this repo there is NO legitimate caller — refuse. Otherwise an anonymous request could POST
    // `{status:"green"}` for any real change and drive `set_verification`, poisoning the merge gate
    // that `verify_change` otherwise reserves to owners/admins.
    let Some(ci::RepoCi { secret, .. }) = cfg.as_ref() else {
        return (StatusCode::FORBIDDEN, "no external CI is configured for this repo; the built-in runner reports its own verdicts").into_response();
    };
    // A configured endpoint that set a secret must present it. Compare fixed-size SHA-256 digests so
    // the check is length-independent — a raw `ct_eq` short-circuits on length, leaking the secret's
    // length via timing (matching `verify_service_secret`).
    if !secret.is_empty() {
        let presented = headers.get("X-Hull-CI-Secret").and_then(|v| v.to_str().ok()).unwrap_or("");
        use sha2::{Digest, Sha256};
        if !ct_eq(&Sha256::digest(presented.as_bytes()), &Sha256::digest(secret.as_bytes())) {
            return (StatusCode::UNAUTHORIZED, "bad or missing X-Hull-CI-Secret").into_response();
        }
    }
    let status = body.get("status").and_then(Value::as_str).unwrap_or("").to_string();
    if !matches!(status.as_str(), "green" | "red" | "errored") {
        return (StatusCode::BAD_REQUEST, "status must be green | red | errored").into_response();
    }
    let summary = body.get("summary").and_then(Value::as_str).unwrap_or("").to_string();
    let st = ci::finalize(&app.repos, &app.ci, &app.ci_config, &tenant, &repo, &id, &status, &summary);
    if matches!(st, hull_plugin::CiStatus::Green | hull_plugin::CiStatus::Red) {
        notify_ci(&app, &tenant, &repo, &id, ci_status_str(st), &summary).await;
    }
    Json(json!({ "recorded": status })).into_response()
}

/// A repo's CI endpoint config (`GET/PUT …/ci-config`). GET reports the effective endpoint and where
/// it comes from (repo / instance default / none), never leaking the secret. PUT (owner-gated) sets
/// or clears the repo's own endpoint.
async fn get_ci_config(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
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
    })).into_response()
}

async fn set_ci_config(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let key = format!("{tenant}/{repo}");
    let acting = match require_actor(&app, &headers, body.get("by").and_then(Value::as_str).unwrap_or("")).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // Owner-gated: only an owner/admin of the repo's account may point it at a CI system.
    if !is_repo_admin(&app, &tenant, &repo, &acting.id).await {
        return (StatusCode::FORBIDDEN, "only a repo owner/admin can set the CI endpoint").into_response();
    }
    let url = body.get("url").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let secret = body.get("secret").and_then(Value::as_str).unwrap_or("").to_string();
    app.ci_config.set(&key, ci::RepoCi { url: url.clone(), secret });
    Json(json!({ "url": url, "cleared": url.is_empty() })).into_response()
}

/// Is `actor` an Owner/Admin of the account that owns `tenant/repo`?
/// Resolve the repo record for `tenant/repo`, matched by its **fully-qualified** identity: the bare
/// `name` under the account that owns `tenant`. There is deliberately no bare-name fallback — a
/// same-named repo under a *different* owner must never match (cross-tenant confusion).
async fn find_repo(app: &App, tenant: &str, repo: &str) -> Option<Repo> {
    let tenant_acct = app.store.accounts().await.into_iter().find(|a| a.handle == tenant)?.id;
    app.store.repos().await.into_iter().find(|r| r.name == repo && r.owner == tenant_acct)
}

async fn is_repo_admin(app: &App, tenant: &str, repo: &str, actor: &str) -> bool {
    let Some(owner) = find_repo(app, tenant, repo).await.map(|r| r.owner) else {
        // No repo record — fall back to: any owner/admin of the tenant org.
        return app.store.accounts().await.iter().any(|a| a.handle == tenant && a.members.iter().any(|m| m.actor == actor && matches!(m.role, Role::Owner | Role::Admin)));
    };
    app.store
        .accounts()
        .await
        .into_iter()
        .find(|a| a.id == owner)
        .map(|a| a.members.iter().any(|m| m.actor == actor && matches!(m.role, Role::Owner | Role::Admin)))
        .unwrap_or(false)
}

/// The account that owns `tenant/repo`: the repo record's owner, or (no repo record yet) the org whose
/// handle == `tenant`. Same resolution as `repo_account_id`, returning the full [`Account`].
async fn repo_owner_account(app: &App, tenant: &str, repo: &str) -> Option<Account> {
    let owner = repo_account_id(app, tenant, repo).await?;
    app.store.accounts().await.into_iter().find(|a| a.id == owner)
}

/// The write-side membership gate: does `actor` belong to the account that owns `tenant/repo` — in ANY
/// role (Owner/Admin/Write/Read) — or to a team that account has granted access to this repo? Unlike
/// `can_read_repo`, this never short-circuits on visibility: a *public* repo is still only mutated by
/// its members. `is_repo_admin` (Owner/Admin only) is the stricter peer used for repo-config changes.
async fn is_repo_member(app: &App, tenant: &str, repo: &str, actor: &str) -> bool {
    let Some(acct) = repo_owner_account(app, tenant, repo).await else { return false };
    if acct.members.iter().any(|m| m.actor == actor) {
        return true;
    }
    // A member of a team the repo grants a WRITE-CAPABLE role (admin/write, not read) counts as a
    // write-side member. This is the write gate, so a read-only team grant must NOT pass it — reads
    // go through `can_read_repo`, which (correctly) accepts any team grant. Ignoring the role here
    // silently escalated a read grant to push.
    let settings = app.repo_settings.get(&format!("{tenant}/{repo}"));
    app.store
        .teams(&acct.id)
        .await
        .into_iter()
        .any(|t| {
            settings.team_access.iter().any(|ta| ta.team == t.id && matches!(ta.role.as_str(), "admin" | "write"))
                && t.members.iter().any(|m| m.actor == actor)
        })
}

// ── git smart-HTTP authorization (closes authz-hardening area D) ────────────────────────────────
//
// Enforcement is CONFIG-GATED by `HULL_GIT_AUTH` so the credential-free dogfood keeps working: the
// DEFAULT (`off`, or unset) is a byte-for-byte no-op — every request is `Allow` and the handlers run
// exactly as before. Set `HULL_GIT_AUTH=enforce` to require credentials:
//   · FETCH (upload-pack): anonymous OK for public/unlisted repos (`can_read_repo(None).await`), a private
//     repo needs a token whose actor can read it — else 401 (so git prompts / a credential helper runs).
//   · PUSH  (receive-pack): always needs a token whose actor is a repo member; non-member → 403,
//     missing/invalid creds → 401. This also gates auto-create-on-push, because `is_repo_member`
//     resolves the owning account from the tenant handle, so only a member of that account can
//     provision a new repo by pushing.
// Git presents credentials natively via HTTP Basic on an HTTP remote; we treat the Basic **password**
// as a hull session token (username ignored). A `Bearer` header is accepted as a fallback.

/// Whether git smart-HTTP auth is enforced. Enabled by any common truthy value
/// (`enforce`/`on`/`true`/`1`/`yes`); unset or a falsey value means `off` — fully anonymous git,
/// exactly as before this change. An UNRECOGNIZED non-empty value logs a warning and defaults to off
/// rather than silently failing open on a typo (a security control must not quietly stay disabled).
fn git_auth_enforced() -> bool {
    match std::env::var("HULL_GIT_AUTH") {
        Ok(v) => {
            let v = v.trim();
            if ["enforce", "on", "true", "1", "yes"].iter().any(|t| v.eq_ignore_ascii_case(t)) {
                true
            } else if v.is_empty() || ["off", "false", "0", "no"].iter().any(|t| v.eq_ignore_ascii_case(t)) {
                false
            } else {
                eprintln!("hull: HULL_GIT_AUTH=\"{v}\" not recognized — git auth stays OFF (use `enforce` to enable)");
                false
            }
        }
        Err(_) => false,
    }
}

/// Extract a hull session token from a git request's `Authorization` header. Accepts HTTP Basic
/// (git's native scheme for HTTP remotes — the **password** field is the token, username ignored) and
/// a `Bearer` fallback. `None` if there is no usable credential.
fn git_token_from_headers(headers: &HeaderMap) -> Option<String> {
    let auth = headers.get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok())?;
    if let Some(b64) = auth.strip_prefix("Basic ") {
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()?;
        let text = String::from_utf8(decoded).ok()?;
        // `user:password` — the password (everything after the first colon) is the token.
        let (_user, pass) = text.split_once(':')?;
        let pass = pass.trim();
        return (!pass.is_empty()).then(|| pass.to_string());
    }
    if let Some(bearer) = auth.strip_prefix("Bearer ") {
        let bearer = bearer.trim();
        return (!bearer.is_empty()).then(|| bearer.to_string());
    }
    None
}

/// The authorization verdict for one git smart-HTTP request.
#[derive(Debug, PartialEq, Eq)]
enum GitAuthDecision {
    /// Proceed to the handler.
    Allow,
    /// 401 + `WWW-Authenticate: Basic` — missing/insufficient creds on a gated request.
    Unauthorized,
    /// 403 — a valid actor that isn't a member (push).
    Forbidden,
}

/// Decide whether a git request may proceed. Pure over (`enforce`, repo visibility/membership,
/// service, token) so it can be unit-tested exhaustively. When `enforce` is false this is ALWAYS
/// `Allow` — the config-off no-op. `service` is `git-upload-pack` (fetch) or `git-receive-pack`
/// (push); `token` is the raw session token extracted from the request, if any.
async fn git_auth_decision(app: &App, enforce: bool, tenant: &str, repo: &str, service: &str, token: Option<&str>) -> GitAuthDecision {
    if !enforce {
        return GitAuthDecision::Allow;
    }
    // Resolve the token to an actor, but honor it only if the actor is still ACCOUNTABLE (not revoked;
    // an agent's delegation chain valid + unexpired). The REST mutating path enforces this via
    // `require_actor`; the git path did not, so a revoked actor's still-unexpired session token kept
    // fetch/push working for up to the token TTL (~30 days). A non-accountable token is treated as no
    // credential — anonymous rules then apply (public fetch still works; private/push is refused).
    let actor = match token {
        Some(t) => actor_for_token(app, t).await,
        None => None,
    };
    let actor = match actor {
        Some(a) if accountable(app, &a).await.is_ok() => Some(a.id),
        _ => None,
    };
    if service == "git-receive-pack" {
        // Push: always require a member. This also gates auto-create-on-push (an anonymous or
        // non-member actor can neither push to nor provision a repo).
        match actor {
            None => GitAuthDecision::Unauthorized,
            Some(aid) if is_repo_member(app, tenant, repo, &aid).await => GitAuthDecision::Allow,
            Some(_) => GitAuthDecision::Forbidden,
        }
    } else {
        // Fetch: public/unlisted stays anonymous; a private repo needs a read-authorized actor.
        // Any shortfall is 401 (not 403) so git re-tries with a credential helper.
        if can_read_repo(app, actor.as_deref(), tenant, repo).await {
            GitAuthDecision::Allow
        } else {
            GitAuthDecision::Unauthorized
        }
    }
}

/// Run the git-auth gate for a request. `Some(resp)` = reject with that response; `None` = proceed.
async fn git_gate(app: &App, tenant: &str, repo: &str, service: &str, headers: &HeaderMap) -> Option<Response> {
    let token = git_token_from_headers(headers);
    match git_auth_decision(app, git_auth_enforced(), tenant, repo, service, token.as_deref()).await {
        GitAuthDecision::Allow => None,
        GitAuthDecision::Unauthorized => Some(
            (StatusCode::UNAUTHORIZED, [(axum::http::header::WWW_AUTHENTICATE, "Basic realm=\"hull\"")], "authentication required").into_response(),
        ),
        GitAuthDecision::Forbidden => Some((StatusCode::FORBIDDEN, "not a repo member").into_response()),
    }
}

/// The git service named by an `info/refs?service=…` query, or `""` if absent/unrecognized.
fn info_refs_service(query: Option<&str>) -> String {
    query
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("service=")))
        .unwrap_or("")
        .to_string()
}

/// `GET /{tenant}/{repo}/info/refs` — auth pre-check in front of [`repos::info_refs`]. The gate only
/// fires for the two real services; an unrecognized service falls through to the handler's own 403.
async fn info_refs_handler(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let service = info_refs_service(query.as_deref());
    if service == "git-upload-pack" || service == "git-receive-pack" {
        if let Some(resp) = git_gate(&app, &tenant, &repo, &service, &headers).await {
            return resp;
        }
    }
    repos::info_refs(State(app), Path((tenant, repo)), RawQuery(query)).await
}

/// `POST /{tenant}/{repo}/git-upload-pack` — auth pre-check in front of [`repos::upload_pack`].
async fn upload_pack_handler(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(resp) = git_gate(&app, &tenant, &repo, "git-upload-pack", &headers).await {
        return resp;
    }
    repos::upload_pack(State(app), Path((tenant, repo)), headers, body).await
}

/// Git push endpoint, wrapped so that **every successful push runs CI** on the new HEAD change —
/// independent of autonomy tier (CI is a mechanical check, not an autonomous action). Fire-and-forget
/// (memoized by tree, so an unchanged tree is a no-op); dispatched to the configured CI or the local
/// runner. Also carries the push auth gate (receive-pack), enforced BEFORE the handler provisions or
/// mutates the repo.
/// The git receive-pack **report-status** stream that rejects a push to a protected branch, so
/// `git push` prints a clean `! [remote rejected] main -> main (protected: …)`. keel-git advertises
/// no side-band, so the report goes in the response body directly (no sideband framing). We report
/// `unpack ok` (the pack is simply never ingested — nothing is written) followed by an `ng` for the
/// protected ref; git treats the `ng` as a rejection and fails the push, leaving the branch untouched.
///
/// `rejected` is the list of `(refname, reason)` pairs to reject — one per ref the client tried to push
/// that we refuse. The pack is never ingested regardless, so every un-mentioned ref is also left
/// untouched; the `ng` lines exist only so `git push` prints a clean per-ref rejection.
fn reject_push(rejected: &[(String, &str)]) -> Response {
    fn pkt(line: &str) -> Vec<u8> {
        // A git pkt-line: a 4-hex length prefix (covering the 4 bytes) followed by the payload.
        let mut v = format!("{:04x}", line.len() + 4).into_bytes();
        v.extend_from_slice(line.as_bytes());
        v
    }
    let mut body = Vec::new();
    body.extend(pkt("unpack ok\n"));
    for (refname, reason) in rejected {
        body.extend(pkt(&format!("ng {refname} {reason}\n")));
    }
    body.extend_from_slice(b"0000"); // flush-pkt
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "application/x-git-receive-pack-result"),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// Reject a push that targets the protected default branch (the branch is known).
fn protected_push_rejection(default_branch: &str) -> Response {
    reject_push(&[(format!("refs/heads/{default_branch}"), "protected: land changes via a reviewed PR")])
}

/// Fail-closed rejection for a protected repo whose real default branch could NOT be resolved: since we
/// can't tell which ref to guard, we reject EVERY ref the client tried to push (falling back to
/// `refs/heads/main` if the command list is empty). The pack is never ingested, so nothing advances.
fn protected_push_rejection_unresolved(commands: &[(String, String)]) -> Response {
    const REASON: &str = "protected: repo default branch could not be resolved; retry the push";
    let rejected: Vec<(String, &str)> = if commands.is_empty() {
        vec![("refs/heads/main".to_string(), REASON)]
    } else {
        commands.iter().map(|(_new, refname)| (refname.clone(), REASON)).collect()
    };
    reject_push(&rejected)
}

async fn receive_pack_handler(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(resp) = git_gate(&app, &tenant, &repo, "git-receive-pack", &headers).await {
        return resp;
    }
    // Branch protection: on a repo that requires review-to-land, a direct push that UPDATES the
    // protected default branch is rejected — the branch may only advance through a reviewed, merge-
    // verified land (`perform_merge`). Ref *creation* (a fresh repo) and every other branch push are
    // unaffected. When protection is off this whole block is skipped — the config-off no-op.
    let key = format!("{tenant}/{repo}");
    if app.repo_settings.get(&key).protects_default_branch() {
        let commands = repos::parse_receive_refs(&repos::maybe_gunzip(&headers, body.to_vec(), repos::git_max_body_bytes()));
        // Fail CLOSED: on a protected repo we must know the *real* default branch before deciding. If
        // `find_repo` can't resolve it (e.g. a transient store read), reject rather than fall back to a
        // literal "main" — a repo whose default branch isn't "main" would otherwise let a push to its
        // true protected branch slip past the gate while we guarded the wrong ref.
        let Some(default_branch) = find_repo(&app, &tenant, &repo).await.map(|r| r.default_branch) else {
            eprintln!("hull: ⚠ receive-pack on protected {tenant}/{repo} but default branch unresolved; failing closed");
            return protected_push_rejection_unresolved(&commands);
        };
        if repos::touches_protected(&commands, &default_branch) {
            return protected_push_rejection(&default_branch);
        }
    }
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
async fn repo_account_id(app: &App, tenant: &str, repo: &str) -> Option<String> {
    match find_repo(app, tenant, repo).await.map(|r| r.owner) {
        Some(owner) => Some(owner),
        None => app.store.accounts().await.into_iter().find(|a| a.handle == tenant).map(|a| a.id),
    }
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

/// `GET /api/repos/:tenant/:repo/substrate` — read the repo's DECENTRALIZED substrate state back from
/// the nostr relays and return a FULLY-VERIFIED view: the current ref (instance-signed, own-authored)
/// and provenance attestations (schnorr + Ed25519 verified against the SIGNED claim, not relay-supplied
/// tags), each annotated with whether `claim.actor` is an accountable hull actor (delegation chain to a
/// human, per the local store) and authorized on this repo. This is the CONSUMER that makes the
/// substrate load-bearing — proof that history isn't hostage to one host: it's readable + verifiable
/// off public relays without trusting any single relay or even this instance's DB.
async fn substrate_view(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    let Some(refs) = app.nostr_refs.clone() else {
        return Json(json!({ "enabled": false })).into_response();
    };
    let key = format!("{tenant}/{repo}");
    let default_branch = find_repo(&app, &tenant, &repo).await.map(|r| r.default_branch).unwrap_or_else(|| "main".into());
    // Relay round-trips are blocking I/O — keep them off the async runtime.
    let (rf, k, br) = (refs.clone(), key.clone(), default_branch.clone());
    let (commit, provenance) =
        tokio::task::spawn_blocking(move || (rf.fetch_ref(&k, &br), rf.fetch_provenance(&k))).await.unwrap_or((None, Vec::new()));
    // Annotate each signature-verified attestation with LOCAL accountability + repo authority. Use
    // Hull's real `accountable()` gate, which rejects a revoked actor AND verifies the delegation chain
    // — not the structural `is_accountable()`, which would let a revoked or compromised key's forged
    // provenance read as trustworthy. Authority mirrors the real write gate (`is_repo_member`, honoring
    // team write grants), checked for the acting actor or its human root.
    let mut prov: Vec<Value> = Vec::new();
    for sp in provenance {
        let actor = app.store.actor(&sp.claim.actor).await;
        let human = actor.as_ref().and_then(|a| a.human_principal().cloned());
        let acct_ok = match &actor {
            Some(a) => accountable(&app, a).await.is_ok(),
            None => false,
        };
        let authorized = acct_ok
            && (is_repo_member(&app, &tenant, &repo, &sp.claim.actor).await
                || match &human {
                    Some(h) => is_repo_member(&app, &tenant, &repo, h).await,
                    None => false,
                });
        prov.push(json!({
            "change": sp.claim.change,
            "actor": sp.claim.actor,
            "actor_handle": actor.as_ref().map(|a| a.handle.clone()),
            "human_root": human,
            "intent": sp.claim.intent,
            "ts": sp.claim.ts,
            // signatures_valid: schnorr + Ed25519 both checked in fetch_provenance. It means "a valid
            // signature by claim.actor", NOT "trustworthy" — read it with accountable/authorized.
            "signatures_valid": true,
            "accountable": acct_ok,
            "authorized": authorized,
        }));
    }
    prov.sort_by(|a, b| b["ts"].as_u64().cmp(&a["ts"].as_u64())); // newest first
    Json(json!({
        "enabled": true,
        "relays": refs.relays(),
        "ref": commit.map(|c| json!({ "branch": default_branch, "commit": c, "source": "nostr" })),
        "provenance": prov,
    }))
    .into_response()
}

/// The effective autonomy policy for a repo (`GET …/autonomy`) — the resolved tier, where it comes
/// from, and the protected paths that always require a human.
async fn get_repo_autonomy(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    let acct = repo_account_id(&app, &tenant, &repo).await;
    let e = app.autonomy.effective(&tenant, &repo, acct.as_deref());
    Json(json!({
        "tier": e.tier, "source": e.source, "protected_paths": e.protected_paths,
        "repo_override": app.autonomy.get_repo(&tenant, &repo).map(|p| p.tier),
        "account_tier": acct.as_deref().and_then(|a| app.autonomy.get_account(a)).map(|p| p.tier),
    })).into_response()
}

/// Set the repo's autonomy tier (`PUT …/autonomy` `{tier, protected_paths?}`) — owner/admin only.
async fn set_repo_autonomy(
    State(app): State<App>,
    Path((tenant, repo)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let actor = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if !is_repo_admin(&app, &tenant, &repo, &actor.id).await {
        return (StatusCode::FORBIDDEN, "only a repo owner/admin can set autonomy").into_response();
    }
    let Some(tier) = body.get("tier").and_then(Value::as_str).and_then(tier_from_str) else {
        return (StatusCode::BAD_REQUEST, "tier must be t0 | t1 | t2 | t3").into_response();
    };
    // Patch-merge (atomic under the store lock): overwrite protected_paths only when the key is
    // PRESENT. A tier-only change (the UI sends just {tier}) PRESERVES the existing protected paths,
    // not silently wiping the human-gated set — dropping them would quietly widen agent auto-merge.
    let paths = body
        .get("protected_paths")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect());
    app.autonomy.set_repo_tier(&tenant, &repo, tier, paths);
    Json(json!({ "tier": tier })).into_response()
}

/// Set an account's autonomy tier (`PUT /api/accounts/:id/autonomy`) — account owner/admin only.
async fn set_account_autonomy(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let actor = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let is_admin = app
        .store
        .accounts()
        .await
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
    // Patch-merge (see set_repo_autonomy): preserve existing protected_paths on a tier-only change.
    let paths = body
        .get("protected_paths")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect());
    app.autonomy.set_account_tier(&id, tier, paths);
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
    let actor = match require_actor(&app, &headers, body.get("actor").and_then(Value::as_str).unwrap_or("")).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // A session record is provenance attached to the repo's history — repo members only.
    if !is_repo_member(&app, &tenant, &repo, &actor.id).await {
        return (StatusCode::FORBIDDEN, "only a repo member may ingest a session").into_response();
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
    app.store.put_session_record(record.clone()).await;
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
    let actor = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let Some(mut pr) = app.store.prs(&key).await.into_iter().find(|p| p.number == number) else {
        return (StatusCode::NOT_FOUND, "no such PR").into_response();
    };
    if pr.state == PrState::Merged {
        return (StatusCode::CONFLICT, "a merged PR can't be closed or reopened").into_response();
    }
    if pr.author != actor.id && !is_repo_admin(&app, &tenant, &repo, &actor.id).await {
        return (StatusCode::FORBIDDEN, "only the PR author or a repo owner/admin can close it").into_response();
    }
    let reopen = body.get("reopen").and_then(Value::as_bool).unwrap_or(false);
    pr.state = if reopen { PrState::Open } else { PrState::Closed };
    app.store.replace_pr(pr.clone()).await;
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
    let requester = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // Repo-op gate: requesting a reviewer is a repo operation, not public participation — members only.
    if !is_repo_member(&app, &tenant, &repo, &requester.id).await {
        return (StatusCode::FORBIDDEN, "only a repo member can request a reviewer").into_response();
    }
    let reviewer = body.get("reviewer").and_then(Value::as_str).unwrap_or("").to_string();
    if app.store.actor(&reviewer).await.is_none() {
        return (StatusCode::UNPROCESSABLE_ENTITY, "reviewer must be a registered actor").into_response();
    }
    let Some(mut pr) = app.store.prs(&key).await.into_iter().find(|p| p.number == number) else {
        return (StatusCode::NOT_FOUND, "no such PR").into_response();
    };
    if !pr.reviewers.contains(&reviewer) {
        pr.reviewers.push(reviewer.clone());
        app.store.replace_pr(pr.clone()).await;
    }
    app.registry.notify(&NotifyEvent {
        kind: "review_requested".into(),
        to: vec![reviewer.clone()],
        summary: format!("{} requested your review on PR !{number}", requester.handle),
        change: pr.changes.first().cloned(),
        repo: Some(key.clone()),
        target_kind: Some("pr".into()),
        target_number: Some(number),
    }).await;
    Json(json!({ "pr": pr })).into_response()
}

/// review by someone **other than the author** (independent — no self-merge). Records who merged.
async fn merge_pr(
    State(app): State<App>,
    Path((tenant, repo, number)): Path<(String, String, u64)>,
    headers: axum::http::HeaderMap,
    Json(_body): Json<Value>,
) -> Response {
    let actor = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // Repo-op gate: merging is a repo operation — members only. The independent-approval / green-verify
    // gate inside `perform_merge` is preserved and still enforced on top of this.
    if !is_repo_member(&app, &tenant, &repo, &actor.id).await {
        return (StatusCode::FORBIDDEN, "only a repo member can merge").into_response();
    }
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
    let Some(mut pr) = app.store.prs(&key).await.into_iter().find(|p| p.number == number) else {
        return Err((StatusCode::NOT_FOUND, "no such PR".into()));
    };
    if pr.state == PrState::Merged {
        return Err((StatusCode::CONFLICT, "already merged".into()));
    }
    // An owner/admin may override the gate (merge despite red/unrun checks or no approval) — the
    // human-admin escape hatch for a wedged or misconfigured check.
    let override_ok = force && is_repo_admin(app, tenant, repo, actor.id.as_str()).await;
    // green keel verification of EVERY proposed change — a multi-change PR must not smuggle an
    // unverified change at index ≥1 past a check that only inspected the first.
    let green = !pr.changes.is_empty()
        && pr
            .changes
            .iter()
            .all(|c| app.repos.verification(tenant, repo, c).map(|v| v == "green").unwrap_or(false));
    if !green && !override_ok {
        return Err((StatusCode::CONFLICT, "cannot merge: change is not keel-verify green".into()));
    }
    // Independent approving reviews (approver != PR author unless the repo allows self-approval),
    // split by actor kind.
    let allow_self = app.repo_settings.get(&key).allow_self_approve;
    let approvals: Vec<ActorId> = app
        .store
        .reviews(&key)
        .await
        .into_iter()
        .filter(|r| r.target == format!("pr:{number}") && r.verdict == Verdict::Approve && (allow_self || r.reviewer != pr.author))
        .map(|r| r.reviewer)
        .collect();
    // Explicit loop rather than `.any(async)` — the actor-kind lookup now `.await`s.
    let mut human_approval = false;
    for a in approvals.iter() {
        if app.store.actor(a).await.map(|x| x.kind == hull_core::ActorKind::Human).unwrap_or(false) {
            human_approval = true;
            break;
        }
    }
    // Explicit loop rather than `.any(async)` — the actor-kind lookup now `.await`s.
    let mut agent_approval = false;
    for a in approvals.iter() {
        if app.store.actor(a).await.map(|x| x.kind == hull_core::ActorKind::Agent).unwrap_or(false) {
            agent_approval = true;
            break;
        }
    }

    // Autonomy policy: when may an AGENT's approve stand in for a human's?
    let acct = repo_account_id(app, tenant, repo).await;
    let eff = app.autonomy.effective(tenant, repo, acct.as_deref());
    // Inspect EVERY change in the PR, not just the first: protected-path and ledger gating must see a
    // protected or un-reconciled change wherever it sits in the PR.
    let files: Vec<String> = pr
        .changes
        .iter()
        .flat_map(|c| {
            app.repos
                .change_info(tenant, repo, c)
                .map(|i| i.files.into_iter().map(|f| f.path).collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect();
    let protected = autonomy::touches_protected(&files, &eff.protected_paths);
    let (contradicted, phantom) = {
        let mut contradicted = false;
        let mut phantom = false;
        for change in &pr.changes {
            let lesson = app.store.session_record(&key, change).await.map(|s| s.lesson).unwrap_or_default();
            let intent = app.repos.change_info(tenant, repo, change).map(|i| i.intent).unwrap_or_default();
            let facts = facts_with_independence(app, tenant, repo, change).await;
            let ledger = hull_core::reconcile::reconcile(change, &intent, &lesson, &facts);
            contradicted |= ledger.contradicted() > 0;
            phantom |= ledger.phantom() > 0;
        }
        (contradicted, phantom)
    };
    // A change that did work its narrative never claimed (C5 phantom work) is NOT low-risk: it
    // includes unreviewed operations, so an agent's approve must not auto-merge it — a human looks.
    let low_risk = !protected && !contradicted && !phantom; // green is already required above
    let agent_approve_counts = match eff.tier {
        hull_core::AutonomyTier::T0 | hull_core::AutonomyTier::T1 => false,
        hull_core::AutonomyTier::T2 => low_risk,
        // T3 is the most autonomous tier, but a RED independent verification (`contradicted` — the
        // change broke or weakened a pre-existing on-disk test) is a hard signal that must block an
        // agent-only auto-merge at EVERY tier; otherwise a change that self-approves by weakening a
        // real test would land on one agent approval. (Protected paths ALWAYS need a human — D11.)
        hull_core::AutonomyTier::T3 => !protected && !contradicted,
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
    // Landing. On a PROTECTED default branch this advances the branch through a synthesized,
    // speculatively re-verified merge (the merge queue). When protection is off it stays exactly
    // today's metadata-only flip — the config-off no-op that keeps every un-opted repo unchanged.
    let default_branch = find_repo(app, tenant, repo).await.map(|r| r.default_branch).unwrap_or_else(|| "main".into());
    let mut landed_change: Option<String> = None;
    if app.repo_settings.get(&key).protects_default_branch() {
        let Some(head) = app.repos.pr_head(tenant, repo, &pr.changes) else {
            return Err((StatusCode::UNPROCESSABLE_ENTITY, "PR has no landable change".into()));
        };
        let intent = format!("Merge PR !{number}: {}", pr.title);
        let mut done = false;
        // Serialize land+export per (repo, branch). The git-mirror export runs INSIDE `land_merge`
        // AFTER the keel CAS commits, so two concurrent lands to the same protected branch would
        // otherwise race `mirror::refs`-read → `set_ref`, the last writer dropping the other's landed
        // change from git `main`. Holding this per-branch async lock across the whole plan → verify →
        // land → export critical section fully orders them (the keel CAS still guards correctness; this
        // also orders the git side). No deadlock: it is the outermost lock, acquired only here and
        // released at end of scope, and no store lock is held while awaiting it.
        let land_lock = app.repos.land_lock(tenant, repo, &default_branch);
        let _land_guard = land_lock.lock().await;
        // CAS-advance with bounded retry: a concurrent land moves the branch, so we re-read the base
        // and re-plan (and re-verify the fresh merged tree) rather than clobber the other land.
        for _ in 0..8 {
            let base = app.repos.branch_head(tenant, repo, &default_branch);
            match app.repos.plan_merge(tenant, repo, &default_branch, base.as_deref(), &head) {
                Ok(repos::MergeOutcome::AlreadyMerged) => {
                    done = true; // head already on the branch — nothing to advance
                    break;
                }
                Ok(repos::MergeOutcome::Tree { tree, fast_forward }) => {
                    // Speculative verify of the MERGED tree — catches semantic conflicts two
                    // independently-green changes create. An admin override may bypass the green gate
                    // (a wedged/misconfigured check) but STILL lands through this merge path — there is
                    // no git-push escape hatch. A real content conflict already errored out of `plan_merge`.
                    if !override_ok {
                        let (rh, registry, ci) = (app.repos.clone(), app.registry.clone(), app.ci.clone());
                        let (t, r, tr) = (tenant.to_string(), repo.to_string(), tree.clone());
                        let outcome = tokio::task::spawn_blocking(move || ci::run_check_tree(&rh, &registry, &ci, &t, &r, &tr))
                            .await
                            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "merge verification panicked".to_string()))?;
                        if !matches!(outcome.status, hull_plugin::CiStatus::Green) {
                            return Err((StatusCode::CONFLICT, "CONFLICT: merged result fails checks".into()));
                        }
                    }
                    match app.repos.land_merge(tenant, repo, &default_branch, base.as_deref(), &head, &tree, fast_forward, &intent, &actor.id, now()) {
                        Ok(Some(new_change)) => {
                            landed_change = Some(new_change);
                            done = true;
                            break;
                        }
                        Ok(None) => continue, // CAS miss: branch moved — re-read + re-plan
                        Err(repos::MergeError::Conflict(m)) => return Err((StatusCode::CONFLICT, m)),
                        Err(repos::MergeError::Internal(m)) => return Err((StatusCode::INTERNAL_SERVER_ERROR, m)),
                    }
                }
                Err(repos::MergeError::Conflict(m)) => return Err((StatusCode::CONFLICT, m)),
                Err(repos::MergeError::Internal(m)) => return Err((StatusCode::INTERNAL_SERVER_ERROR, m)),
            }
        }
        if !done {
            return Err((StatusCode::CONFLICT, "branch moved during merge; retry".into()));
        }
    }
    pr.state = PrState::Merged;
    pr.merged_by = Some(actor.id.clone());
    app.store.replace_pr(pr.clone()).await;
    // Announce the change that now sits at the branch tip — the synthesized merge change on a
    // protected land, else the PR's head change (today's behavior).
    let announced = landed_change.clone().or_else(|| pr.changes.first().cloned()).unwrap_or_default();
    app.hub.publish(
        tenant,
        ActivityEvent::Push { actor: actor.handle.clone(), repo: repo.to_string(), change: announced.clone(), ts: now() },
    );
    // Decentralized ref transport: publish the new branch tip as a signed nostr event so the repo's
    // history lives on public relays, not just this host. Best-effort + off-thread — a relay must never
    // block or fail a merge. No-op unless nostr ref transport is configured.
    if let (Some(refs), false) = (app.nostr_refs.clone(), announced.is_empty()) {
        let (repo_key, branch, commit) = (key.clone(), default_branch.clone(), announced.clone());
        std::thread::spawn(move || {
            if let Some(ev) = refs.publish_ref(&repo_key, &branch, &commit, None) {
                eprintln!("nostr: published ref {repo_key}#{branch} → {}… ({}…)", &commit[..commit.len().min(12)], &ev.id[..12]);
            }
        });
    }
    // Actor-signed provenance (kind 1900): attest the landed change under the AUTHOR's Ed25519 key so
    // the substrate carries verifiable delegated authority, not just the instance's word. Only when the
    // instance holds that key (custodial/demo accounts); a SOVEREIGN author's key is client-held, so
    // its provenance must be signed client-side — a follow-up (the same primitives, signed in-browser).
    if let (Some(refs), false) = (app.nostr_refs.clone(), announced.is_empty()) {
        let demo_id = identity::human_from_secret("demo", DEMO_OWNER_SECRET).map(|m| m.actor.id).unwrap_or_default();
        let author_secret = if pr.author == demo_id {
            Some(DEMO_OWNER_SECRET.to_string())
        } else {
            app.store.user_by_actor(&pr.author).await.map(|u| u.secret_key).filter(|s| !s.is_empty())
        };
        if let Some(secret) = author_secret {
            let (author, repo_key, change, intent) = (pr.author.clone(), key.clone(), announced.clone(), pr.title.clone());
            std::thread::spawn(move || {
                if let Some(ev) = refs.publish_provenance(&secret, &change, &author, &repo_key, &intent) {
                    eprintln!("nostr: published provenance for {change} by {}… ({}…)", &author[..author.len().min(12)], &ev.id[..12]);
                }
            });
        }
    }
    // Outbound mirror on change-land — guarded by loop prevention + idempotency.
    if let Some(change) = landed_change.as_ref().or_else(|| pr.changes.first()) {
        mirror_out(app, tenant, repo, change).await;
    }
    // Auto-close the issues this PR fixes, stamping the resolving keel change as provenance.
    let resolving = pr.changes.first().cloned();
    // Scan the change intent/body as well as the title for closing keywords.
    let intent_body: String = pr
        .changes
        .iter()
        .filter_map(|c| app.repos.change_info(tenant, repo, c).map(|i| i.intent))
        .collect::<Vec<_>>()
        .join("\n");
    let mut closed: Vec<u64> = Vec::new();
    for num in closing_issue_numbers(&pr.title, &intent_body, &[]) {
        if let Some(mut issue) = app.store.issues(&pr.repo).await.into_iter().find(|i| i.number == num) {
            if matches!(issue.status, hull_core::IssueStatus::Open) {
                issue.status = hull_core::IssueStatus::Closed { reason: hull_core::CloseReason::Completed };
                issue.resolved_by = resolving.clone();
                if !issue.linked_prs.contains(&pr.id) {
                    issue.linked_prs.push(pr.id.clone());
                }
                let assignees = issue.assignees.clone();
                let author = issue.author.clone();
                app.store.replace_issue(issue).await;
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
                    repo: Some(key.clone()),
                    target_kind: Some("issue".into()),
                    target_number: Some(num),
                }).await;
            }
        }
    }
    Ok((pr, closed))
}

/// Issue numbers a PR closes: from closing keywords (`fixes #12`, `closes #3`, `resolves #7`) in the
/// title **and** the change intent/body, plus any explicit `closes` list. Deduped. Scanning the body
/// too means a `Closes #12` written in the change message isn't silently ignored.
fn closing_issue_numbers(title: &str, body: &str, explicit: &[u64]) -> Vec<u64> {
    let mut out: Vec<u64> = explicit.to_vec();
    const KW: &[&str] = &["fix", "fixes", "fixed", "close", "closes", "closed", "resolve", "resolves", "resolved"];
    for text in [title, body] {
        let lower = text.to_lowercase();
        let words: Vec<&str> = lower.split(|c: char| c.is_whitespace() || c == ':' || c == ',' || c == '(').collect();
        for pair in words.windows(2) {
            if KW.contains(&pair[0]) {
                if let Some(n) = pair[1].strip_prefix('#').and_then(|s| s.parse::<u64>().ok()) {
                    out.push(n);
                }
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
    // Bind the delivery to THIS repo's connection. The App's webhook secret is global (one per App),
    // so a validly-signed delivery for one tenant's repo could otherwise be replayed to another
    // tenant's `.../mirror/github` endpoint and force an unrelated re-sync. Require the payload's
    // installation id to match the GitHub connection registered for the repo's owning account (which
    // also rejects deliveries for a repo that was never connected).
    let payload_inst = v.get("installation").and_then(|i| i.get("id")).and_then(Value::as_i64).map(|n| n.to_string());
    let conn_inst = repo_owner_account(&app, &tenant, &repo).await.and_then(|acct| app.connections.get(&acct.id)).map(|c| c.installation);
    match (conn_inst, payload_inst) {
        (Some(want), Some(got)) if !want.is_empty() && want == got => {}
        _ => return (StatusCode::FORBIDDEN, "webhook installation does not match this repo's GitHub connection").into_response(),
    }
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

    // Branch protection: the protected default branch may only advance through the reviewed, merge-
    // verified merge queue (`perform_merge`). Mirror-inbound is secret-gated (not attacker-arbitrary),
    // but importing a forge push here would move the protected branch OUTSIDE review — so on a protected
    // repo we refuse to advance the protected branch and log it. (Non-default branches were already
    // ignored above; this path only ever reaches `refs/heads/main`.)
    if app.repo_settings.get(&format!("{tenant}/{repo}")).protects_default_branch() {
        let default_branch = find_repo(&app, &tenant, &repo).await.map(|r| r.default_branch).unwrap_or_else(|| "main".into());
        if git_ref == format!("refs/heads/{default_branch}") {
            eprintln!("hull: ⚠ mirror-inbound push to {tenant}/{repo} targets protected '{default_branch}'; skipped (advance only via a reviewed PR)");
            return Json(json!({ "skipped_protected": default_branch, "change": after })).into_response();
        }
    }

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
    let actor = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // Repo-op gate: pushing to the mirror is a repo operation — members only.
    if !is_repo_member(&app, &tenant, &repo, &actor.id).await {
        return (StatusCode::FORBIDDEN, "only a repo member can push to the mirror").into_response();
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

async fn mirror_out(app: &App, tenant: &str, repo: &str, change: &str) -> bool {
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
            repo: Some(format!("{tenant}/{repo}")),
            target_kind: None,
            target_number: None,
        }).await;
    }
    result.ok
}

/// The repo's mirror status (`GET /api/repos/:tenant/:repo/mirror`): the external target it's linked
/// to (if any) and the outbound pushes recorded, for the UI's mirror panel.
async fn mirror_status(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
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
    })).into_response()
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
    let attributed_handle = match attributed.as_ref() {
        Some(id) => app.store.actor(id).await.map(|a| a.handle),
        None => None,
    };
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
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    Json(json!({ "reviews": app.store.reviews(&format!("{tenant}/{repo}")).await })).into_response()
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
async fn comments_list(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    Json(json!({ "comments": app.store.comments(&format!("{tenant}/{repo}")).await })).into_response()
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
    let author = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // Participation gate: a public/unlisted repo stays open to any authed actor; a private repo only
    // lets people who can read it (its members) comment.
    if !can_read_repo(&app, Some(&author.id), &tenant, &repo).await {
        return (StatusCode::FORBIDDEN, "not a member of this repo").into_response();
    }
    let target = body.get("target").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let text = body.get("body").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if target.is_empty() || text.is_empty() {
        return (StatusCode::BAD_REQUEST, "target and body are required").into_response();
    }
    let count = app.store.comments(&key).await.len();
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
        edited_unix: None,
    };
    app.store.put_comment(comment.clone()).await;
    // Notify the people watching the target (not the commenter): a PR's author + reviewers, or an
    // issue's author + assignees.
    let (mut to, summary, change): (Vec<String>, String, Option<String>) =
        if let Some(num) = target.strip_prefix("pr:").and_then(|s| s.parse::<u64>().ok()) {
            match app.store.prs(&key).await.into_iter().find(|p| p.number == num) {
                Some(pr) => {
                    let mut to = pr.reviewers.clone();
                    to.push(pr.author.clone());
                    (to, format!("{} commented on PR !{num}", author.handle), pr.changes.first().cloned())
                }
                None => (vec![], String::new(), None),
            }
        } else if let Some(num) = target.strip_prefix("issue:").and_then(|s| s.parse::<u64>().ok()) {
            match app.store.issues(&key).await.into_iter().find(|i| i.number == num) {
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
        let (target_kind, target_number) = notify_target(&target);
        app.registry.notify(&NotifyEvent { kind: "comment_posted".into(), to, summary, change, repo: Some(key.clone()), target_kind, target_number }).await;
    }
    // @mentions in a comment add the mentioned actor as a reviewer (on a PR) or assignee (on an issue).
    let mentioned = parse_mentions(&comment.body);
    if !mentioned.is_empty() {
        let actors = app.store.actors().await;
        let ids: Vec<String> = mentioned.iter().filter_map(|h| actors.iter().find(|a| &a.handle == h).map(|a| a.id.clone())).collect();
        if let Some(num) = target.strip_prefix("pr:").and_then(|s| s.parse::<u64>().ok()) {
            if let Some(mut pr) = app.store.prs(&key).await.into_iter().find(|p| p.number == num) {
                let mut added = vec![];
                for id in &ids {
                    if id != &pr.author && !pr.reviewers.contains(id) {
                        pr.reviewers.push(id.clone());
                        added.push(id.clone());
                    }
                }
                if !added.is_empty() {
                    app.store.replace_pr(pr.clone()).await;
                    app.registry.notify(&NotifyEvent { kind: "review_requested".into(), to: added, summary: format!("{} mentioned you as a reviewer on PR !{num}", author.handle), change: pr.changes.first().cloned(), repo: Some(key.clone()), target_kind: Some("pr".into()), target_number: Some(num) }).await;
                }
            }
        } else if let Some(num) = target.strip_prefix("issue:").and_then(|s| s.parse::<u64>().ok()) {
            if let Some(mut issue) = app.store.issues(&key).await.into_iter().find(|i| i.number == num) {
                let mut added = vec![];
                for id in &ids {
                    if !issue.assignees.contains(id) {
                        issue.assignees.push(id.clone());
                        added.push(id.clone());
                    }
                }
                if !added.is_empty() {
                    app.store.replace_issue(issue.clone()).await;
                    app.registry.notify(&NotifyEvent { kind: "issue_assigned".into(), to: added, summary: format!("{} mentioned you on issue #{num}", author.handle), change: None, repo: Some(key.clone()), target_kind: Some("issue".into()), target_number: Some(num) }).await;
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
                    app.store.put_comment(reply).await;
                }
            }
        }
    }
    (StatusCode::CREATED, Json(json!({ "comment": comment }))).into_response()
}

/// Parse a comment/notification `target` string (`"pr:1"` / `"issue:2"`) into the structured
/// `(target_kind, target_number)` pair carried on a [`NotifyEvent`] so the inbox can link to it.
fn notify_target(target: &str) -> (Option<String>, Option<u64>) {
    if let Some(n) = target.strip_prefix("pr:").and_then(|s| s.parse::<u64>().ok()) {
        (Some("pr".into()), Some(n))
    } else if let Some(n) = target.strip_prefix("issue:").and_then(|s| s.parse::<u64>().ok()) {
        (Some("issue".into()), Some(n))
    } else {
        (None, None)
    }
}

/// Delete a comment (`DELETE …/comments/:id`). Only the comment's **author** or a repo **owner/admin**
/// may delete it — you can't erase someone else's words.
async fn delete_comment(State(app): State<App>, Path((tenant, repo, id)): Path<(String, String, String)>, headers: axum::http::HeaderMap) -> Response {
    let key = format!("{tenant}/{repo}");
    let actor = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let Some(comment) = app.store.comments(&key).await.into_iter().find(|c| c.id == id) else {
        return (StatusCode::NOT_FOUND, "no such comment").into_response();
    };
    if comment.author != actor.id && !is_repo_admin(&app, &tenant, &repo, &actor.id).await {
        return (StatusCode::FORBIDDEN, "only the comment's author or a repo owner/admin can delete it").into_response();
    }
    let removed = app.store.remove_comment(&key, &id).await;
    Json(json!({ "deleted": removed, "id": id })).into_response()
}

/// Edit a comment (`PATCH …/comments/:id` with `{body}`). Only the comment's **author** may edit it —
/// unlike delete, a repo admin can't rewrite someone else's words, only remove them. Updates the body
/// and stamps `edited_unix`.
async fn edit_comment(State(app): State<App>, Path((tenant, repo, id)): Path<(String, String, String)>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Response {
    let key = format!("{tenant}/{repo}");
    let actor = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let new_body = body.get("body").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if new_body.is_empty() {
        return (StatusCode::UNPROCESSABLE_ENTITY, "comment body must not be empty").into_response();
    }
    let Some(comment) = app.store.comments(&key).await.into_iter().find(|c| c.id == id) else {
        return (StatusCode::NOT_FOUND, "no such comment").into_response();
    };
    if comment.author != actor.id {
        return (StatusCode::FORBIDDEN, "only the comment's author can edit it").into_response();
    }
    app.store.update_comment_body(&key, &id, &new_body, now()).await;
    let updated = app.store.comments(&key).await.into_iter().find(|c| c.id == id);
    Json(json!({ "comment": updated })).into_response()
}

/// Build the code context around a commented line, ask the AI reviewer, and return its reply as a
/// [`Comment`] authored by the agent reviewer (anchored to the same line). `None` if anything is
/// missing (no change, no reviewer actor, model declined).
async fn ai_answer_comment(app: &App, key: &str, pr_num: u64, path: &str, line: u32, line_end: u32, question: &str) -> Option<Comment> {
    let (tenant, repo) = key.split_once('/')?;
    let pr = app.store.prs(key).await.into_iter().find(|p| p.number == pr_num)?;
    let change = pr.changes.first().cloned()?;
    // An accountable agent to author the reply — the org's reviewer, never the PR author. Same-org
    // only: the selected agent must be a member of this repo (matching `independent_agent_reviewer`),
    // so an agent from another tenant can't be picked to answer on a repo it isn't a member of.
    // Explicit loops rather than `.find(async)` — the predicate `.await`s (is_repo_member +
    // accountable). Prefer the named `agent:reviewer`; fall back to any accountable member agent.
    let reviewer = {
        let mut found = None;
        for a in app.store.actors().await {
            if a.kind == hull_core::ActorKind::Agent && a.handle == "agent:reviewer" && a.id != pr.author && is_repo_member(app, tenant, repo, &a.id).await {
                found = Some(a);
                break;
            }
        }
        if found.is_none() {
            for a in app.store.actors().await {
                if a.kind == hull_core::ActorKind::Agent && a.id != pr.author && accountable(app, &a).await.is_ok() && is_repo_member(app, tenant, repo, &a.id).await {
                    found = Some(a);
                    break;
                }
            }
        }
        found
    }?;
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
    let count = app.store.comments(key).await.len();
    Some(Comment {
        id: format!("cm_{}_{}", key.replace('/', "_"), count + 1),
        repo: key.to_string(),
        target: format!("pr:{pr_num}"),
        author: reviewer.id,
        body: answer,
        created_unix: now(),
        path: Some(path.to_string()),
        line: Some(line),
        edited_unix: None,
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
    let reviewer = match require_actor(&app, &headers, body.get("reviewer").and_then(Value::as_str).unwrap_or("")).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // A review is an accountable act on the repo — only a member of the owning account may post one.
    if !is_repo_member(&app, &tenant, &repo, &reviewer.id).await {
        return (StatusCode::FORBIDDEN, "only a repo member may review").into_response();
    }
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
            if let Some(pr) = app.store.prs(&key).await.into_iter().find(|p| p.number == num) {
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
    let count = app.store.reviews(&key).await.len();
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
    app.store.put_review(review.clone()).await;
    // Notify the PR's author via the Notifier plugin capability (core records + logs it; a hosted
    // plugin would also deliver over Slack/email/nostr).
    if let Some(num) = review.target.strip_prefix("pr:").and_then(|s| s.parse::<u64>().ok()) {
        if let Some(pr) = app.store.prs(&review.repo).await.into_iter().find(|p| p.number == num) {
            app.registry.notify(&NotifyEvent {
                kind: "review_posted".into(),
                to: vec![pr.author.clone()],
                summary: format!("{} posted a {:?} review on PR !{num}", reviewer.handle, review.verdict),
                change: pr.changes.first().cloned(),
                repo: Some(review.repo.clone()),
                target_kind: Some("pr".into()),
                target_number: Some(num),
            }).await;
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
    let actor = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // Repo-op gate: auto-review spawns agent compute — members only, not open participation.
    if !is_repo_member(&app, &tenant, &repo, &actor.id).await {
        return (StatusCode::FORBIDDEN, "only a repo member can request an auto-review").into_response();
    }
    let key = format!("{tenant}/{repo}");
    let Some(pr) = app.store.prs(&key).await.into_iter().find(|p| p.number == number) else {
        return (StatusCode::NOT_FOUND, "no such PR").into_response();
    };
    let Some(agent) = independent_agent_reviewer(&app, &tenant, &repo, &pr.author).await else {
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
    if let Some(org) = app.store.accounts().await.into_iter().find(|a| a.handle == tenant) {
        let c = app.store.ai_connections(&org.id).await;
        if !c.is_empty() { owner = Some(org.id.clone()); conns = c; }
    }
    if conns.is_empty() {
        if let Some(aid) = fallback_actor {
            if let Some(pa) = app.store.accounts().await.into_iter().find(|a| a.kind == hull_core::AccountKind::Personal && a.members.iter().any(|m| m.actor == aid)) {
                let c = app.store.ai_connections(&pa.id).await;
                if !c.is_empty() { owner = Some(pa.id.clone()); conns = c; }
            }
        }
    }
    let Some(owner) = owner else { return (None, None) };
    let idx = if app.store.ai_rotate(&owner).await { next_ai_index(&owner, conns.len()) } else { 0 };
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
    let Some(pr) = app.store.prs(&key).await.into_iter().find(|p| p.number == number) else {
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
    let session = app.store.session_record(&key, &change).await;
    let lesson = session.as_ref().map(|s| s.lesson.clone()).unwrap_or_default();
    let author_model = session.as_ref().map(|s| s.model.clone()).unwrap_or_default();
    // C1 — claim extraction from the issue's acceptance criteria: when this PR closes an issue, fold
    // that issue's title + body into the reviewed narrative, so the reconciliation verifies the change
    // against **what the issue asked for**, not just what the change's own message claims.
    let review_intent = {
        let issues = app.store.issues(&key).await;
        let acceptance: Vec<String> = closing_issue_numbers(&pr.title, &info.intent, &[])
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
        let acct = repo_account_id(app, tenant, repo).await;
        !autonomy::touches_protected(&touched, &app.autonomy.effective(tenant, repo, acct.as_deref()).protected_paths)
    };
    let (verdict, findings, ledger, base_summary, from_cache) = if mechanical {
        let ledger = hull_core::reconcile::reconcile(&change, &review_intent, &lesson, &facts);
        let n = semantic.moves.len();
        (Verdict::Approve, Vec::new(), Some(ledger), format!("pure move — {n} file{} relocated with byte-identical content (verified by content address); no behavioral review needed", if n == 1 { "" } else { "s" }), false)
    } else {
    let (cred, _bundle) = resolve_ai_credential(app, &key, Some(&pr.author)).await;
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
    // Namespace the key by repo AND a hash of the review intent, not just the content-addressed tree.
    // Trees are content-addressed, so two repos (or a fork) with byte-identical trees share a `tree`
    // id — a cached `approve` warmed in a throwaway repo would otherwise be served in a victim repo,
    // skipping the model's intent reconciliation. The intent (which folds in the closing issue's
    // acceptance criteria) is a real verdict input, so a different intent must re-review.
    let intent_hash = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(review_intent.as_bytes()))
    };
    let cache_key = format!("{tenant}/{repo}:{tree}:{}:{intent_hash}", app.repos.verification(tenant, repo, &change).unwrap_or_default());
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
                app.store.add_ai_usage(cid, u.input_tokens, u.output_tokens, u.cost_micros, now()).await;
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
    // (the AI reviewer). Only an **Approve** is an affirmative sign-off that the change does what it
    // claims — so only then do we resolve the outstanding needs-judgment claims as verified, attributed
    // to the agent (accountable; a human can still override). A Comment verdict is not sign-off: it
    // must NOT silently verify every claim. RequestChanges/Reject obviously leave the problems standing.
    if reviewer.kind == hull_core::ActorKind::Agent && verdict == Verdict::Approve {
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
    let count = app.store.reviews(&key).await.len();
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
    app.store.put_review(review.clone()).await;
    app.registry.notify(&NotifyEvent {
        kind: "review_posted".into(),
        to: vec![pr.author.clone()],
        summary: format!("{} auto-reviewed PR !{number}: {:?}", reviewer.handle, review.verdict),
        change: Some(change.clone()),
        repo: Some(key.clone()),
        target_kind: Some("pr".into()),
        target_number: Some(number),
    }).await;

    // Auto-triage (T2+): a review that requests changes turns its blocker findings into a triaged
    // issue — automatic issue triage out of reviews. Gated by the repo's autonomy tier.
    let acct = repo_account_id(app, tenant, repo).await;
    let tier = app.autonomy.effective(tenant, repo, acct.as_deref()).tier;
    if tier >= hull_core::AutonomyTier::T2 && review.verdict == Verdict::RequestChanges {
        let blockers: Vec<&ReviewFinding> = review.findings.iter().filter(|f| f.severity == "blocker").collect();
        if !blockers.is_empty() {
            // Don't re-triage the same PR: skip if an open from-review issue already links it.
            let already = app
                .store
                .issues(&key)
                .await
                .into_iter()
                .any(|i| i.labels.iter().any(|l| l == "from-review") && i.linked_prs.contains(&pr.id) && matches!(i.status, IssueStatus::Open));
            if !already {
                let alloc_guard = app.number_lock.lock().await;
                let inum = app.store.issues(&key).await.iter().map(|i| i.number).max().unwrap_or(0) + 1;
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
                    edited_unix: None,
                };
                app.store.put_issue(issue).await;
                drop(alloc_guard); // number persisted; release before notify/publish
                app.registry.notify(&NotifyEvent {
                    kind: "issue_triaged".into(),
                    to: vec![pr.author.clone()],
                    summary: format!("auto-triaged issue #{inum} from the review of PR !{number}"),
                    change: Some(change.clone()),
                    repo: Some(key.clone()),
                    target_kind: Some("issue".into()),
                    target_number: Some(inum),
                }).await;
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
                    repo: Some(key.clone()),
                    target_kind: Some("pr".into()),
                    target_number: Some(number),
                }).await;
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
    let pr = app.store.prs(&key).await.into_iter().find(|p| p.number == number)?;
    let change = pr.changes.first().cloned()?;
    let tree = app.repos.change_tree(tenant, repo, &change).unwrap_or_default();
    let source_url = format!("{}/api/repos/{tenant}/{repo}/tree/{tree}/tar", app.public_url.trim_end_matches('/'));
    let (cred, _bundle) = resolve_ai_credential(app, &key, Some(&pr.author)).await;
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
        app.store.add_ai_usage(cid, u.input_tokens, u.output_tokens, u.cost_micros, now()).await;
    }
    if res.ok && !res.edits.is_empty() {
        let intent = format!("fix: {}", res.explanation);
        let edits: Vec<(String, String, String)> = res.edits.iter().map(|e| (e.path.clone(), e.search.clone(), e.replace.clone())).collect();
        // Materialize the fix as a NEW keel change parented on the PR's change.
        match app.repos.apply_fix(tenant, repo, &change, &edits, &intent, &agent.handle, now()) {
            Some(fix_change) => {
                // Point the PR at the fixed change and run its checks.
                if let Some(mut pr2) = app.store.prs(&key).await.into_iter().find(|p| p.number == number) {
                    pr2.changes = vec![fix_change.clone()];
                    app.store.replace_pr(pr2).await;
                }
                let _ = resolve_check(app, tenant, repo, &fix_change, false).await;
                let diff = res.edits.iter().map(|e| format!("--- {}\n- {}\n+ {}", e.path, e.search.lines().next().unwrap_or(""), e.replace.lines().next().unwrap_or(""))).collect::<Vec<_>>().join("\n");
                let count = app.store.comments(&key).await.len();
                app.store.put_comment(Comment {
                    id: format!("cm_{}_{}", key.replace('/', "_"), count + 1),
                    repo: key.clone(),
                    target: format!("pr:{number}"),
                    author: agent.id.clone(),
                    body: format!("🔧 **Applied fix** as change ⬡{} — {}\n\n```diff\n{diff}\n```", &fix_change[..12], res.explanation),
                    created_unix: now(),
                    path: None,
                    line: None,
                    edited_unix: None,
                }).await;
                app.registry.notify(&NotifyEvent {
                    kind: "fix_applied".into(),
                    to: vec![pr.author.clone()],
                    summary: format!("{} applied a fix to PR !{number} (new change {})", agent.handle, &fix_change[..12]),
                    change: Some(fix_change),
                    repo: Some(key.clone()),
                    target_kind: Some("pr".into()),
                    target_number: Some(number),
                }).await;
            }
            None => {
                // The fix didn't apply cleanly — record it as a proposal instead of a silent drop.
                let count = app.store.comments(&key).await.len();
                app.store.put_comment(Comment {
                    id: format!("cm_{}_{}", key.replace('/', "_"), count + 1),
                    repo: key.clone(),
                    target: format!("pr:{number}"),
                    author: agent.id.clone(),
                    body: format!("🔧 **Proposed fix** for `{path}` (couldn't apply cleanly — the code moved): {}", res.explanation),
                    created_unix: now(),
                    path: None,
                    line: None,
                    edited_unix: None,
                }).await;
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
    let actor = match require_actor(&app, &headers, "").await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // Repo-op gate: requesting an AI fix spawns agent compute — members only, not open participation.
    if !is_repo_member(&app, &tenant, &repo, &actor.id).await {
        return (StatusCode::FORBIDDEN, "only a repo member can request an AI fix").into_response();
    }
    let key = format!("{tenant}/{repo}");
    let Some(pr) = app.store.prs(&key).await.into_iter().find(|p| p.number == number) else {
        return (StatusCode::NOT_FOUND, "no such PR").into_response();
    };
    let Some(agent) = independent_agent_reviewer(&app, &tenant, &repo, &pr.author).await else {
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
async fn independent_agent_reviewer(app: &App, tenant: &str, repo: &str, author: &str) -> Option<hull_core::Actor> {
    // Explicit loop rather than `.find(async closure)` — the predicate now `.await`s (accountable +
    // is_repo_member), which an iterator adapter closure can't express. Same first-match semantics.
    for a in app.store.actors().await {
        if a.kind == hull_core::ActorKind::Agent
            && a.id != author
            && accountable(app, &a).await.is_ok()
            // Same-org only: an agent from another tenant must never be selected to review — and,
            // at T3, auto-merge — a repo it isn't a member of (cross-tenant escalation).
            && is_repo_member(app, tenant, repo, &a.id).await
        {
            return Some(a);
        }
    }
    None
}

/// List pull requests for a hosted repo (`GET /api/repos/:tenant/:repo/prs`). Each PR's verification
/// is refreshed live from keel (the change's verify state), so an approving review + green keel
/// verify shows on the badge.
async fn prs(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    let mut list = app.store.prs(&format!("{tenant}/{repo}")).await;
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
    let actor = match require_actor(&app, &headers, body.get("actor").and_then(Value::as_str).unwrap_or("")).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // Setting a change green/red IS the merge gate — a non-admin flipping it to green would defeat CI.
    // The legitimate CI path writes verification through the secret-authed `ci-result` callback (and
    // the local runner writes it directly), never this endpoint, so restrict manual overrides to a
    // repo owner/admin.
    if !is_repo_admin(&app, &tenant, &repo, &actor.id).await {
        return (StatusCode::FORBIDDEN, "only a repo owner/admin may set verification directly (CI reports via the ci-result callback)").into_response();
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
    let actor = match require_actor(&app, &headers, body.get("author").and_then(Value::as_str).unwrap_or("")).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // Participation gate: a public/unlisted repo stays open to any authed actor; a private repo only
    // lets people who can read it (its members) open a PR.
    if !can_read_repo(&app, Some(&actor.id), &tenant, &repo).await {
        return (StatusCode::FORBIDDEN, "not a member of this repo").into_response();
    }
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
    // Hold the allocation lock across the read-max-then-insert (`put_pr` below) so a concurrent
    // create can't be handed the same number. The intervening code-owner resolution/notify runs
    // under the lock too; on this single-process, low-create-rate server that contention is trivial.
    let alloc_guard = app.number_lock.lock().await;
    let number = app.store.prs(&key).await.iter().map(|p| p.number).max().unwrap_or(0) + 1;
    // Code owners: resolve the owners of any file this change touches — they become requested
    // reviewers and get notified.
    let files: Vec<String> = changes
        .first()
        .and_then(|c| app.repos.change_info(&tenant, &repo, c))
        .map(|ci| ci.files.into_iter().map(|f| f.path).collect())
        .unwrap_or_default();
    let owners = owners_for(&app, &key, &files).await;
    // Code owners plus any repo-configured default reviewers are auto-requested on the new voyage.
    let mut reviewers = owners.clone();
    for r in app.repo_settings.get(&key).default_reviewers {
        if !reviewers.contains(&r) {
            reviewers.push(r);
        }
    }
    let pr = PullRequest {
        id: format!("pr_{}_{number}", key.replace('/', "_")),
        repo: key.clone(),
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
            repo: Some(key.clone()),
            target_kind: Some("pr".into()),
            target_number: Some(number),
        }).await;
    }
    app.store.put_pr(pr.clone()).await;
    drop(alloc_guard); // number persisted; release before the remaining link/publish work
    // Link the issues this PR closes (from `fixes #N` in the title, or an explicit `closes` list) so
    // they show the incoming PR now and auto-close when it merges.
    let explicit: Vec<u64> = body.get("closes").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
    let intent_body: String = pr.changes.iter().filter_map(|c| app.repos.change_info(&tenant, &repo, c).map(|i| i.intent)).collect::<Vec<_>>().join("\n");
    for num in closing_issue_numbers(&pr.title, &intent_body, &explicit) {
        if let Some(mut issue) = app.store.issues(&pr.repo).await.into_iter().find(|i| i.number == num) {
            if !issue.linked_prs.contains(&pr.id) {
                issue.linked_prs.push(pr.id.clone());
                app.store.replace_issue(issue).await;
            }
        }
    }
    app.hub.publish(
        &tenant,
        ActivityEvent::Push { actor: actor.handle, repo: repo.clone(), change: pr.changes[0].clone(), ts: now() },
    );
    // Agent flow (M6): when a PR opens, an independent agent reviewer auto-reviews it — but only if
    // the repo's autonomy tier permits autonomous action (T1+). At T0 (observe-only), nothing fires.
    let acct = repo_account_id(&app, &tenant, &repo).await;
    let tier = app.autonomy.effective(&tenant, &repo, acct.as_deref()).tier;
    if tier >= hull_core::AutonomyTier::T1 {
        if let Some(agent) = independent_agent_reviewer(&app, &tenant, &repo, &pr.author).await {
            let (app2, t2, r2, n2) = (app.clone(), tenant.clone(), repo.clone(), number);
            tokio::spawn(async move {
                let _ = perform_auto_review(&app2, &t2, &r2, n2, &agent, 0).await;
            });
        } else {
            eprintln!(
                "auto-review skipped for {tenant}/{repo} PR !{number}: no org-member agent reviewer is registered"
            );
        }
    }
    (StatusCode::CREATED, Json(json!({ "pr": pr }))).into_response()
}

/// List issues for a hosted repo (`GET /api/repos/:tenant/:repo/issues`).
async fn issues(State(app): State<App>, Path((tenant, repo)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_repo_read(&app, &headers, &tenant, &repo).await {
        return r;
    }
    Json(json!({ "issues": app.store.issues(&format!("{tenant}/{repo}")).await })).into_response()
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
    let actor = match require_actor(&app, &headers, author_id).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // Participation gate: a public/unlisted repo stays open to any authed actor; a private repo only
    // lets people who can read it (its members) open an issue.
    if !can_read_repo(&app, Some(&actor.id), &tenant, &repo).await {
        return (StatusCode::FORBIDDEN, "not a member of this repo").into_response();
    }
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
    // Assignees must themselves be registered accountable actors (unknown ids are dropped). Explicit
    // loop rather than `.filter(async)` — the accountability lookup now `.await`s.
    let mut assignees: Vec<String> = Vec::new();
    if let Some(arr) = body.get("assignees").and_then(Value::as_array) {
        for id in arr.iter().filter_map(Value::as_str) {
            if app.store.actor(id).await.map(|a| a.is_accountable()).unwrap_or(false) {
                assignees.push(id.to_string());
            }
        }
    }
    // @mentions in the title or body also assign the mentioned actor.
    let mention_text = format!("{} {}", title, body.get("body").and_then(Value::as_str).unwrap_or(""));
    let actors = app.store.actors().await;
    for h in parse_mentions(&mention_text) {
        if let Some(a) = actors.iter().find(|a| a.handle == h && a.is_accountable()) {
            if !assignees.contains(&a.id) {
                assignees.push(a.id.clone());
            }
        }
    }
    // Hold the allocation lock across the read-max-then-insert so a concurrent create can't be handed
    // the same number (the read and the `put_issue` below must be one critical section).
    let alloc_guard = app.number_lock.lock().await;
    let number = app.store.issues(&key).await.iter().map(|i| i.number).max().unwrap_or(0) + 1;
    let author = actor.id.clone();
    let issue = Issue {
        id: format!("iss_{}_{number}", key.replace('/', "_")),
        repo: key.clone(),
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
        edited_unix: None,
    };
    app.store.put_issue(issue.clone()).await;
    drop(alloc_guard); // number is persisted; release before the (unrelated) notify/publish work
    if !issue.assignees.is_empty() {
        app.registry.notify(&NotifyEvent {
            kind: "issue_assigned".into(),
            to: issue.assignees.clone(),
            summary: format!("{} assigned issue #{number}: {}", actor.handle, issue.title),
            change: None,
            repo: Some(key.clone()),
            target_kind: Some("issue".into()),
            target_number: Some(number),
        }).await;
    }
    app.hub.publish(
        &tenant,
        ActivityEvent::Issue { repo, number, action: "opened".into(), actor: author, ts: now() },
    );
    (StatusCode::CREATED, Json(json!({ "issue": issue }))).into_response()
}

/// Transition an issue (`PATCH /api/repos/:tenant/:repo/issues/:number`) with
/// `{"action":"close","reason":"completed|not_planned|cancelled|duplicate"}` or
/// `{"action":"reopen"}`. Also `{"action":"edit","title":…,"body":…}` to rewrite the words —
/// **author-only**, unlike close/label/assign which any accountable actor may do. Emits a
/// tenant-scoped event so the change shows live.
async fn update_issue(
    State(app): State<App>,
    Path((tenant, repo, number)): Path<(String, String, u64)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let key = format!("{tenant}/{repo}");
    // A transition is an authoring action — the acting actor must chain to a human.
    let acting = match require_actor(&app, &headers, body.get("actor").and_then(Value::as_str).unwrap_or("")).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // Participation gate: a public/unlisted repo stays open to any authed actor; a private repo only
    // lets people who can read it (its members) transition an issue. The per-action author/admin
    // checks below (edit = author-only, etc.) are still enforced ON TOP of this.
    if !can_read_repo(&app, Some(&acting.id), &tenant, &repo).await {
        return (StatusCode::FORBIDDEN, "not a member of this repo").into_response();
    }
    let Some(mut issue) = app.store.issues(&key).await.into_iter().find(|i| i.number == number) else {
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
            if who.is_empty() || app.store.actor(&who).await.is_none() {
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
        "edit" => {
            // Rewriting the title/body is **author-only** — unlike close/reopen/label/assign (any
            // accountable actor), a repo admin can't rewrite the author's words, only manage state.
            // Mirrors `edit_comment`.
            if issue.author != acting.id {
                return (StatusCode::FORBIDDEN, "only the issue's author can edit its title or body").into_response();
            }
            let new_title = body.get("title").and_then(Value::as_str).map(|t| t.trim().to_string());
            let new_body = body.get("body").and_then(Value::as_str).map(str::to_string);
            if let Some(t) = &new_title {
                if t.is_empty() {
                    return (StatusCode::UNPROCESSABLE_ENTITY, "issue title must not be empty").into_response();
                }
            }
            if new_title.is_none() && new_body.is_none() {
                return (StatusCode::BAD_REQUEST, "edit requires a title and/or body").into_response();
            }
            app.store.update_issue_content(&key, number, new_title.as_deref(), new_body.as_deref(), now()).await;
            let updated = app.store.issues(&key).await.into_iter().find(|i| i.number == number);
            app.hub.publish(
                &tenant,
                ActivityEvent::Issue { repo, number, action: "edited".into(), actor: acting.handle, ts: now() },
            );
            return Json(json!({ "issue": updated })).into_response();
        }
        _ => return (StatusCode::BAD_REQUEST, "action must be close | reopen | assign | unassign | label | unlabel | edit").into_response(),
    }
    app.store.replace_issue(issue.clone()).await;
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
/// `POST /api/feed/ticket` — mint a short-lived ticket the browser passes to the SSE `/api/feed`
/// stream, which cannot carry an Authorization header. Bound to the signed-in actor; the feed then
/// restricts events to that actor's member accounts. Reusable within its TTL so EventSource's
/// auto-reconnect keeps working.
async fn feed_ticket(State(app): State<App>, headers: axum::http::HeaderMap) -> Response {
    let Some(a) = authed_actor(&app, &headers).await else {
        return (StatusCode::UNAUTHORIZED, "not signed in").into_response();
    };
    let ticket = identity::random_hex(24);
    {
        let mut auth = app.auth.lock().unwrap();
        auth.feed_tickets.retain(|_, (_, iss)| now().saturating_sub(*iss) < FEED_TICKET_TTL_SECS);
        auth.feed_tickets.insert(ticket.clone(), (a.id.clone(), now()));
    }
    Json(json!({ "ticket": ticket, "ttl": FEED_TICKET_TTL_SECS })).into_response()
}

/// Resolve a feed ticket to its actor id, pruning expired tickets. `None` if unknown/expired.
fn resolve_feed_ticket(app: &App, ticket: &str) -> Option<String> {
    let mut auth = app.auth.lock().unwrap();
    auth.feed_tickets.retain(|_, (_, iss)| now().saturating_sub(*iss) < FEED_TICKET_TTL_SECS);
    auth.feed_tickets.get(ticket).map(|(aid, _)| aid.clone())
}

async fn feed(
    State(app): State<App>,
    Query(q): Query<HashMap<String, String>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // SSE can't send an Authorization header, so the browser passes a short-lived `?ticket=` minted by
    // the authenticated `POST /api/feed/ticket`. Resolve it to the caller's actor, then restrict the
    // requested accounts (`?accounts=a,b,c`, or a single `?tenant=`) to the ones that actor is a member
    // of. No/expired ticket → nobody → an empty stream. This stops anyone streaming another org's live
    // coordination activity (which `/api/home` already gates but the feed did not).
    let allowed: std::collections::HashSet<String> = match q.get("ticket").and_then(|t| resolve_feed_ticket(&app, t)) {
        Some(aid) => member_accounts(&app, &aid).await.into_iter().map(|a| a.handle).collect(),
        None => std::collections::HashSet::new(),
    };
    let tenants: Vec<String> = q
        .get("accounts")
        .map(|s| s.split(',').filter(|x| !x.is_empty()).map(str::to_string).collect())
        .or_else(|| q.get("tenant").map(|t| vec![t.clone()]))
        .unwrap_or_default()
        .into_iter()
        .filter(|t| allowed.contains(t))
        .collect();
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
async fn seed(store: &dyn Store) {
    for name in ["keel", "hull"] {
        store.put_repo(Repo {
            id: format!("repo_{name}"),
            owner: "acct_tankrap".into(),
            name: name.into(),
            default_branch: "main".into(),
        }).await;
    }
    // A human root + an agent it delegated — both real Ed25519 identities, both org members. The
    // human signs the agent's delegation hop (it holds the key at mint), so the chain is
    // cryptographically verifiable, not merely asserted.
    let human_minted = identity::mint_human("justin");
    let human = human_minted.actor.clone();
    store.put_actor(human.clone()).await;
    let mut members = vec![Membership { actor: human.id.clone(), role: Role::Owner }];
    if let Some(agent) =
        identity::mint_agent("agent:reviewer", &human, &human_minted.secret_key, "*", Lifetime::Static)
    {
        members.push(Membership { actor: agent.actor.id.clone(), role: Role::Write });
        // agent:reviewer owns the server crate — it'll be auto-requested on PRs touching it.
        store.set_owners(
            "tankrap/hull",
            vec![OwnerRule { glob: "crates/hull-server/**".into(), owners: vec![agent.actor.id.clone()] }],
        ).await;
        store.put_actor(agent.actor).await;
    }
    store.put_account(Account {
        id: "acct_tankrap".into(),
        kind: AccountKind::Organization,
        handle: "tankrap".into(),
        members,
    }).await;
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
        edited_unix: None,
    }).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inflight_guard_releases_slot_on_drop_only_when_active() {
        let cfg = Arc::new(ci::CiConfig::from_env());
        // A live claim released by an active guard drop (models normal completion AND cancellation:
        // both simply drop the guard).
        assert!(cfg.mark_inflight("tree-a"), "first claim should succeed");
        assert!(cfg.is_inflight("tree-a"));
        {
            let _g = InflightGuard { ci_config: cfg.clone(), tree: "tree-a".into(), active: true };
        }
        assert!(!cfg.is_inflight("tree-a"), "active guard drop must clear the slot");

        // A force run that did NOT claim (another run holds the slot) must leave the slot intact.
        assert!(cfg.mark_inflight("tree-b"), "owner claims tree-b");
        {
            let _g = InflightGuard { ci_config: cfg.clone(), tree: "tree-b".into(), active: false };
        }
        assert!(cfg.is_inflight("tree-b"), "inactive guard must not clear another run's slot");
        cfg.clear_inflight("tree-b");
    }

    #[test]
    fn closing_issue_numbers_scans_title_and_body_with_keywords() {
        // Keyword variants (fixes/closes/resolves) in the title.
        assert_eq!(closing_issue_numbers("fixes #1 and closes #2", "", &[]), vec![1, 2]);
        assert_eq!(closing_issue_numbers("resolves #7", "", &[]), vec![7]);
        // A closing keyword in the BODY (change intent) is honored, not just the title.
        assert_eq!(closing_issue_numbers("some title", "Closes #12", &[]), vec![12]);
        // Title + body combine and dedup; the explicit list folds in too.
        assert_eq!(closing_issue_numbers("fix #3", "also resolves #3 and fixes #4", &[5]), vec![3, 4, 5]);
    }

    #[test]
    fn closing_issue_numbers_ignores_non_matches() {
        // A bare "#9" with no keyword, and a keyword with no issue ref, must not match.
        assert!(closing_issue_numbers("mentions #9 in passing", "nothing here", &[]).is_empty());
        assert!(closing_issue_numbers("fixes the bug", "closes the loop", &[]).is_empty());
        // "affixes #1" — the keyword must be its own word, not a suffix of another.
        assert!(closing_issue_numbers("prefixes #1", "", &[]).is_empty());
    }

    #[test]
    fn session_tokens_expire_after_ttl() {
        // Mirrors the opportunistic prune in `authed_actor`: a token is kept iff its age is under
        // `SESSION_TTL_SECS`. Drive the exact retain predicate against a synthetic token map so the
        // TTL boundary is pinned without standing up a full `App`.
        let now = 1_000_000_000u64;
        let mut tokens: HashMap<String, (String, u64)> = HashMap::new();
        tokens.insert("fresh".into(), ("actor_a".into(), now)); // issued now
        tokens.insert("recent".into(), ("actor_b".into(), now - (SESSION_TTL_SECS - 1))); // just inside TTL
        tokens.insert("stale".into(), ("actor_c".into(), now - SESSION_TTL_SECS)); // exactly at TTL → expired
        tokens.insert("ancient".into(), ("actor_d".into(), now - (SESSION_TTL_SECS + 10_000))); // well past

        tokens.retain(|_, (_, issued)| now.saturating_sub(*issued) < SESSION_TTL_SECS);

        assert!(tokens.contains_key("fresh"), "a just-issued token must survive");
        assert!(tokens.contains_key("recent"), "a token one second inside the TTL must survive");
        assert!(!tokens.contains_key("stale"), "a token exactly at the TTL boundary must be dropped");
        assert!(!tokens.contains_key("ancient"), "a long-expired token must be dropped");
        assert_eq!(tokens.len(), 2, "only unexpired tokens remain — the map cannot grow unbounded");
    }

    #[test]
    fn sanitize_handle_always_yields_a_safe_segment() {
        // Every nasty input must sanitize to something `safe_segment` accepts (or empty, which every
        // caller rejects with "handle/name is required"). This is the invariant that keeps a stored
        // handle from ever failing `safe_segment` at repo-create time (the org-page bug).
        let nasty = [
            "n;kkjkjk", "new org", ".hidden", "a..b", "foo/bar", "café", "  ", "--", "🚀🚀",
            "UPPER_case-1", "...", "__leading", "trailing__", "a b c", "path\\to\\thing",
            "semi;colon;chain", "tab\there", "\u{200b}zero-width", "..", ".",
        ];
        for input in nasty {
            let out = sanitize_handle(input);
            assert!(
                out.is_empty() || repos::safe_segment(&out),
                "sanitize_handle({input:?}) = {out:?} is neither empty nor a safe_segment",
            );
        }
        // Spot-check the two handles that broke the user's orgs resolve to the expected safe forms.
        assert_eq!(sanitize_handle("new org"), "new_org");
        assert_eq!(sanitize_handle("n;kkjkjk"), "n_kkjkjk");
        // A leading dot is stripped (no dotfiles); `..` collapses (no traversal).
        assert_eq!(sanitize_handle(".hidden"), "hidden");
        assert_eq!(sanitize_handle("a..b"), "a_b");
        // An already-valid handle is unchanged (idempotent on good input).
        assert_eq!(sanitize_handle("UPPER_case-1"), "UPPER_case-1");
        assert_eq!(sanitize_handle("tankrap"), "tankrap");
    }

    #[tokio::test]
    async fn normalize_account_handles_repairs_invalid_and_disambiguates() {
        use hull_core::{Account, AccountKind};
        let store = InMemory::new();
        // Two broken orgs (the user's real ones) plus a valid one and a would-be collision target.
        for (id, handle) in [
            ("acct_a", "new org"),
            ("acct_b", "n;kkjkjk"),
            ("acct_c", "new_org"), // already occupies the sanitized form of "new org" -> forces a suffix
            ("acct_d", "tankrap"), // already valid — must be left untouched
        ] {
            store.put_account(Account { id: id.into(), kind: AccountKind::Organization, handle: handle.into(), members: vec![] }).await;
        }
        // A personal account with an equally invalid handle: it must be LEFT UNTOUCHED, because its
        // handle is one leg of the User.username/Actor.handle invariant and rewriting it here alone
        // would desync those. Personal handles are repaired via the username path, not this migration.
        store.put_account(Account { id: "acct_p".into(), kind: AccountKind::Personal, handle: "bad;personal".into(), members: vec![] }).await;
        normalize_account_handles(&store).await;
        let handles = store.accounts().await;
        let by_id = |id: &str| handles.iter().find(|a| a.id == id).unwrap().handle.clone();
        // "new org" wanted "new_org" but that's taken by acct_c, so it disambiguates.
        assert_eq!(by_id("acct_a"), "new_org-2");
        assert_eq!(by_id("acct_b"), "n_kkjkjk");
        assert_eq!(by_id("acct_c"), "new_org"); // valid handle, unchanged
        assert_eq!(by_id("acct_d"), "tankrap"); // valid handle, unchanged
        assert_eq!(by_id("acct_p"), "bad;personal"); // personal — untouched despite being invalid
        // Every ORG handle now passes safe_segment — repos can be created under all of them.
        for a in store.accounts().await.into_iter().filter(|a| a.kind == AccountKind::Organization) {
            assert!(repos::safe_segment(&a.handle), "{:?} still not a safe segment", a.handle);
        }
        // Idempotent: a second pass changes nothing.
        let before: Vec<_> = store.accounts().await.into_iter().map(|a| (a.id, a.handle)).collect();
        normalize_account_handles(&store).await;
        let after: Vec<_> = store.accounts().await.into_iter().map(|a| (a.id, a.handle)).collect();
        assert_eq!(before, after, "second normalization pass must be a no-op");
    }

    // ── merge gate (`perform_merge`) ──────────────────────────────────────────────────────────────
    //
    // Drives the real gate end-to-end against an isolated in-memory `App`: a temp-dir RepoHost with a
    // real keel change, a domain store with the PR/reviews/actors, and a per-repo autonomy tier.

    // Serialize the env-var writes that isolate each `App`'s file-backed side stores under a fresh HOME.
    static MERGE_ENV_LOCK: Mutex<()> = Mutex::new(());

    // The env-isolation guard is deliberately held across the awaited seed so the HULL_DEMO_MODE
    // mutation stays in effect for the whole operation; the same sync-locked static also backs
    // `build_test_app`, so an async mutex is not a clean swap.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn demo_owner_backdoor_is_gated_off_by_default() {
        // Serialize the HULL_DEMO_MODE env mutation against the other env-touching tests.
        let _g = MERGE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let demo_id = identity::human_from_secret("demo", DEMO_OWNER_SECRET).expect("demo id").actor.id;

        // Default (unset): demo mode OFF — the published-key demo owner is neither minted nor granted
        // ownership of any account. This is the whole point: reading the source can't make you owner.
        std::env::remove_var("HULL_DEMO_MODE");
        assert!(!demo_mode_enabled(), "demo mode is OFF by default");
        let store = InMemory::new();
        seed_if_empty(&store).await;
        assert!(store.actor(&demo_id).await.is_none(), "demo actor is not minted when demo mode is off");
        assert!(
            store.accounts().await.iter().all(|a| !a.members.iter().any(|m| m.actor == demo_id)),
            "demo owner must NOT be a member of any account when demo mode is off",
        );

        // Explicit opt-in: demo mode ON — the demo owner IS added as Owner on every account (the dev
        // flow still works when you ask for it).
        std::env::set_var("HULL_DEMO_MODE", "on");
        assert!(demo_mode_enabled(), "truthy HULL_DEMO_MODE turns demo mode on");
        let store2 = InMemory::new();
        seed_if_empty(&store2).await;
        assert!(
            store2.accounts().await.iter().all(|a| a.members.iter().any(|m| m.actor == demo_id && matches!(m.role, Role::Owner))),
            "demo owner is Owner on every account when demo mode is on",
        );
        std::env::remove_var("HULL_DEMO_MODE");
    }

    #[test]
    fn prod_config_validation_flags_missing_and_unsafe_settings() {
        let key = "a".repeat(64); // a valid 64-hex session key
        // A complete, safe prod config passes with zero problems.
        assert!(
            prod_config_problems(true, Some("ingress-tok"), Some(&key), false).is_empty(),
            "a complete, safe config must pass",
        );
        // Each requirement, violated in isolation, is caught.
        assert!(!prod_config_problems(false, Some("t"), Some(&key), false).is_empty(), "anonymous git is rejected");
        assert!(!prod_config_problems(true, None, Some(&key), false).is_empty(), "missing ingress token is rejected");
        assert!(!prod_config_problems(true, Some("  "), Some(&key), false).is_empty(), "blank ingress token is rejected");
        assert!(!prod_config_problems(true, Some("t"), None, false).is_empty(), "missing session key is rejected");
        assert!(!prod_config_problems(true, Some("t"), Some("tooshort"), false).is_empty(), "a short session key is rejected");
        assert!(!prod_config_problems(true, Some("t"), Some(&"z".repeat(64)), false).is_empty(), "a non-hex session key is rejected");
        assert!(!prod_config_problems(true, Some("t"), Some(&key), true).is_empty(), "demo mode on is rejected in prod");
        // Fail-fast reports every problem at once, not just the first.
        assert_eq!(prod_config_problems(false, None, None, true).len(), 4, "all four problems reported together");
    }

    fn build_test_app(tag: &str) -> (App, std::path::PathBuf) {
        let g = MERGE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("hull-merge-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // Point every `from_env` side store (and the repo host) at the throwaway dir so nothing touches
        // the real ~/.hull, then build the app while still holding the lock (env is captured at build).
        std::env::set_var("HOME", &tmp);
        std::env::set_var("HULL_REPOS_ROOT", tmp.join("repos"));
        std::env::remove_var("HULL_DEFAULT_AUTONOMY");
        let app = build_app(Registry::new(), Arc::new(ActivityHub::new()), Arc::new(InMemory::new()));
        drop(g);
        (app, tmp)
    }

    fn actor(id: &str, kind: ActorKind) -> Actor {
        Actor { id: id.into(), kind, lifetime: Lifetime::Static, handle: id.into(), delegation: None, nostr_pubkey: None, revoked: false }
    }

    async fn put_pr(app: &App, key: &str, number: u64, author: &str, change: &str) {
        app.store.put_pr(PullRequest {
            id: format!("pr-{number}"),
            repo: key.into(),
            number,
            title: "a change".into(),
            author: author.into(),
            changes: vec![change.into()],
            verification: Verification::Unverified,
            reviewers: vec![],
            state: PrState::Open,
            merged_by: None,
            created_unix: 0,
        }).await;
    }

    async fn put_approval(app: &App, key: &str, number: u64, reviewer: &str) {
        app.store.put_review(Review {
            id: format!("rev-{reviewer}-{number}"),
            repo: key.into(),
            target: format!("pr:{number}"),
            reviewer: reviewer.into(),
            verdict: Verdict::Approve,
            summary: "lgtm".into(),
            findings: vec![],
            ledger: None,
            artifact_id: None,
            created_unix: 0,
        }).await;
    }

    async fn is_merged(app: &App, key: &str, number: u64) -> bool {
        app.store.prs(key).await.into_iter().find(|p| p.number == number).map(|p| p.state == PrState::Merged).unwrap_or(false)
    }

    #[tokio::test]
    async fn merge_blocked_when_change_not_green() {
        let (app, tmp) = build_test_app("notgreen");
        let change = app.repos.test_commit("t", "r", "", None, &[("notes.txt", "hi\n")]);
        // verification left unset (unverified). A human reviewer approves — but green is required.
        app.store.put_actor(actor("human", ActorKind::Human)).await;
        put_pr(&app, "t/r", 1, "author", &change).await;
        put_approval(&app, "t/r", 1, "human").await;
        let acting = actor("author", ActorKind::Human);
        let res = perform_merge(&app, "t", "r", 1, &acting, false).await;
        let (code, msg) = res.expect_err("un-green change must be blocked");
        assert_eq!(code, StatusCode::CONFLICT);
        assert!(msg.contains("not keel-verify green"), "msg: {msg}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn merge_rejects_self_approval_unless_repo_allows_it() {
        let (app, tmp) = build_test_app("selfapprove");
        let change = app.repos.test_commit("t", "r", "", None, &[("notes.txt", "hi\n")]);
        app.repos.set_verification("t", "r", &change, true);
        app.store.put_actor(actor("author", ActorKind::Human)).await;
        put_pr(&app, "t/r", 1, "author", &change).await;
        put_approval(&app, "t/r", 1, "author").await; // the author approves their OWN pr
        let acting = actor("author", ActorKind::Human);

        // Default: self-approval doesn't count ⇒ no independent approval ⇒ blocked.
        let (code, msg) = perform_merge(&app, "t", "r", 1, &acting, false).await.expect_err("self-approval blocked by default");
        assert_eq!(code, StatusCode::CONFLICT);
        assert!(msg.contains("someone other than the author"), "msg: {msg}");

        // Opt in to self-approval ⇒ the author's own approve now satisfies the gate.
        app.repo_settings.set("t/r", crate::reposettings::RepoSettings { allow_self_approve: true, ..Default::default() });
        perform_merge(&app, "t", "r", 1, &acting, false).await.expect("self-approval allowed once opted in");
        assert!(is_merged(&app, "t/r", 1).await);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn merge_allows_independent_human_approval() {
        let (app, tmp) = build_test_app("humanok");
        let change = app.repos.test_commit("t", "r", "", None, &[("notes.txt", "hi\n")]);
        app.repos.set_verification("t", "r", &change, true);
        app.store.put_actor(actor("author", ActorKind::Human)).await;
        app.store.put_actor(actor("reviewer", ActorKind::Human)).await;
        put_pr(&app, "t/r", 1, "author", &change).await;
        put_approval(&app, "t/r", 1, "reviewer").await;
        let acting = actor("author", ActorKind::Human);
        perform_merge(&app, "t", "r", 1, &acting, false).await.expect("green + independent human approval merges");
        assert!(is_merged(&app, "t/r", 1).await);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn merge_agent_approval_blocked_at_t1_but_merges_at_t3() {
        let (app, tmp) = build_test_app("agenttier");
        // A clean, non-protected change with a claimed narrative (no phantom ops from notes.txt).
        let change = app.repos.test_commit("t", "r", "", None, &[("notes.txt", "hi\n")]);
        app.repos.set_verification("t", "r", &change, true);
        app.store.put_actor(actor("author", ActorKind::Human)).await;
        app.store.put_actor(actor("agent", ActorKind::Agent)).await;
        put_pr(&app, "t/r", 1, "author", &change).await;
        put_approval(&app, "t/r", 1, "agent").await;
        let acting = actor("author", ActorKind::Human);

        // T1: an agent's approve is advisory ⇒ still needs a human.
        app.autonomy.set_repo("t", "r", AutonomyPolicy { tier: AutonomyTier::T1, protected_paths: vec![] });
        let (code, msg) = perform_merge(&app, "t", "r", 1, &acting, false).await.expect_err("agent approve doesn't count at T1");
        assert_eq!(code, StatusCode::CONFLICT);
        assert!(msg.contains("autonomy tier doesn't let an agent approve"), "msg: {msg}");

        // T3: an agent's approve counts for a non-protected change.
        app.autonomy.set_repo("t", "r", AutonomyPolicy { tier: AutonomyTier::T3, protected_paths: vec![] });
        perform_merge(&app, "t", "r", 1, &acting, false).await.expect("agent approve counts at T3 for a non-protected change");
        assert!(is_merged(&app, "t/r", 1).await);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn merge_agent_approval_blocked_on_protected_path_even_at_t3() {
        let (app, tmp) = build_test_app("protected");
        // Touches a protected path (auth/…) → D11: always needs a human, even at the top tier.
        let change = app.repos.test_commit("t", "r", "", None, &[("auth/token.rs", "x\n")]);
        app.repos.set_verification("t", "r", &change, true);
        app.store.put_actor(actor("author", ActorKind::Human)).await;
        app.store.put_actor(actor("agent", ActorKind::Agent)).await;
        put_pr(&app, "t/r", 1, "author", &change).await;
        put_approval(&app, "t/r", 1, "agent").await;
        app.autonomy.set_repo("t", "r", AutonomyPolicy { tier: AutonomyTier::T3, protected_paths: vec![] });
        let acting = actor("author", ActorKind::Human);
        let (code, msg) = perform_merge(&app, "t", "r", 1, &acting, false).await.expect_err("protected path blocks agent auto-merge at T3");
        assert_eq!(code, StatusCode::CONFLICT);
        assert!(msg.contains("protected path"), "msg: {msg}");
        assert!(!is_merged(&app, "t/r", 1).await);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn merge_agent_approval_at_t2_requires_low_risk() {
        // T2 auto-approves only LOW-RISK changes (green, uncontradicted, no phantom, no protected path).
        // A change whose narrative (empty intent) doesn't account for the `fn` it added is phantom work
        // ⇒ not low-risk ⇒ an agent's approve must not auto-merge it.
        let (app, tmp) = build_test_app("t2phantom");
        let phantom = app.repos.test_commit("t", "r", "", None, &[("helper.rs", "fn secret_backdoor() {}\n")]);
        app.repos.set_verification("t", "r", &phantom, true);
        app.store.put_actor(actor("author", ActorKind::Human)).await;
        app.store.put_actor(actor("agent", ActorKind::Agent)).await;
        put_pr(&app, "t/r", 1, "author", &phantom).await;
        put_approval(&app, "t/r", 1, "agent").await;
        app.autonomy.set_repo("t", "r", AutonomyPolicy { tier: AutonomyTier::T2, protected_paths: vec![] });
        let acting = actor("author", ActorKind::Human);
        let (code, _msg) = perform_merge(&app, "t", "r", 1, &acting, false).await.expect_err("phantom work is not low-risk ⇒ blocked at T2");
        assert_eq!(code, StatusCode::CONFLICT);

        // A clean low-risk change (no phantom ops) DOES auto-merge at T2.
        let clean = app.repos.test_commit("t", "r", "", None, &[("notes.txt", "just prose\n")]);
        app.repos.set_verification("t", "r", &clean, true);
        put_pr(&app, "t/r", 2, "author", &clean).await;
        put_approval(&app, "t/r", 2, "agent").await;
        perform_merge(&app, "t", "r", 2, &acting, false).await.expect("clean low-risk change auto-merges at T2");
        assert!(is_merged(&app, "t/r", 2).await);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn merge_force_overrides_gate_only_for_a_repo_admin() {
        let (app, tmp) = build_test_app("force");
        // An org account whose handle == tenant, with `boss` as Owner (⇒ repo admin via the fallback).
        app.store.put_account(Account {
            id: "acct-t".into(),
            kind: AccountKind::Organization,
            handle: "t".into(),
            members: vec![Membership { actor: "boss".into(), role: Role::Owner }],
        }).await;
        app.store.put_actor(actor("boss", ActorKind::Human)).await;
        app.store.put_actor(actor("rando", ActorKind::Human)).await;
        // Un-green, no approvals — the gate would normally block.
        let change = app.repos.test_commit("t", "r", "", None, &[("notes.txt", "hi\n")]);
        put_pr(&app, "t/r", 1, "author", &change).await;

        // A non-admin can't override even with force.
        let rando = actor("rando", ActorKind::Human);
        assert!(perform_merge(&app, "t", "r", 1, &rando, true).await.is_err(), "force by a non-admin does not override the gate");
        assert!(!is_merged(&app, "t/r", 1).await);

        // An owner/admin CAN force-merge past red/unrun checks and missing approval.
        let boss = actor("boss", ActorKind::Human);
        perform_merge(&app, "t", "r", 1, &boss, true).await.expect("admin force override merges despite the gate");
        assert!(is_merged(&app, "t/r", 1).await);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── branch protection: default-off regression + merge-queue land ──────────────────────────────

    #[test]
    fn default_settings_do_not_protect_the_branch() {
        // The config-off switch: a repo that hasn't opted in is unprotected, so the receive-pack gate
        // is skipped entirely and `perform_merge` stays metadata-only.
        assert!(!crate::reposettings::RepoSettings::default().protects_default_branch());
    }

    /// REGRESSION: with `require_review_to_land` OFF (the default), an approved+green merge behaves
    /// exactly as today — the PR flips `Merged` and `main` does NOT move (metadata-only). This is the
    /// hard guarantee that the whole feature is a no-op until a repo opts in.
    #[tokio::test]
    async fn protection_off_land_is_metadata_only_and_does_not_move_main() {
        let (app, tmp) = build_test_app("protoff");
        let change = app.repos.test_commit("t", "r", "", None, &[("notes.txt", "hi\n")]);
        app.repos.set_verification("t", "r", &change, true);
        app.store.put_actor(actor("author", ActorKind::Human)).await;
        app.store.put_actor(actor("reviewer", ActorKind::Human)).await;
        put_pr(&app, "t/r", 1, "author", &change).await;
        put_approval(&app, "t/r", 1, "reviewer").await;
        let main_before = app.repos.head_change("t", "r");
        let acting = actor("author", ActorKind::Human);
        perform_merge(&app, "t", "r", 1, &acting, false).await.expect("merges with default (unprotected) settings");
        assert!(is_merged(&app, "t/r", 1).await, "PR flips Merged");
        let main_after = app.repos.head_change("t", "r");
        assert_eq!(main_before, main_after, "protection OFF ⇒ perform_merge must NOT advance main");
        assert_eq!(main_after.as_deref(), Some(change.as_str()), "main still at the original change (no merge commit synthesized)");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// On a PROTECTED branch a land ADVANCES `main` through a synthesized two-parent merge change (not
    /// a metadata flip). Uses the admin force-override so the ref-advance is exercised without a live CI
    /// runner — the override bypasses the green gate but STILL lands through the merge queue (§4).
    #[tokio::test]
    async fn protected_land_advances_main_to_a_two_parent_merge_change() {
        let (app, tmp) = build_test_app("protadvance");
        app.store.put_account(Account {
            id: "acct-t".into(),
            kind: AccountKind::Organization,
            handle: "t".into(),
            members: vec![Membership { actor: "boss".into(), role: Role::Owner }],
        }).await;
        app.store.put_actor(actor("boss", ActorKind::Human)).await;
        app.repo_settings.set("t/r", crate::reposettings::RepoSettings { require_review_to_land: true, ..Default::default() });
        // main = base; the PR's head is a child of base sitting OFF main (a real fast-forward to land).
        let base = app.repos.test_commit("t", "r", "base", None, &[("a.txt", "1\n")]);
        let head = app.repos.test_change("t", "r", Some(&base), &[("a.txt", "1\n"), ("b.txt", "2\n")]);
        put_pr(&app, "t/r", 1, "author", &head).await;

        let boss = actor("boss", ActorKind::Human);
        perform_merge(&app, "t", "r", 1, &boss, true).await.expect("admin force lands through the merge queue");
        assert!(is_merged(&app, "t/r", 1).await);
        let tip = app.repos.head_change("t", "r").expect("main has a tip");
        assert_ne!(tip, base, "main advanced off the base");
        assert_ne!(tip, head, "main is a synthesized merge change, not the PR head itself");
        // The new tip is a two-parent merge of [base, head].
        let info = app.repos.change_info("t", "r", &tip).expect("tip resolves");
        assert!(info.files.iter().any(|f| f.path == "b.txt"), "the merged tree carries head's new file");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// On a protected branch, the SPECULATIVE verify of the merged tree gates the land: a red merged
    /// tree is rejected and `main` does not move. Drives the built-in local CI via `HULL_CI_CMD`.
    // Holds the env-isolation guard across the awaited merge so HULL_CI_CMD stays set for the
    // speculative verify; same reason as above the async mutex is not a clean swap.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn protected_land_blocked_when_merged_tree_fails_checks() {
        let (app, tmp) = build_test_app("protred");
        app.store.put_actor(actor("author", ActorKind::Human)).await;
        app.store.put_actor(actor("reviewer", ActorKind::Human)).await;
        app.repo_settings.set("t/r", crate::reposettings::RepoSettings { require_review_to_land: true, ..Default::default() });
        let base = app.repos.test_commit("t", "r", "base", None, &[("a.txt", "1\n")]);
        let head = app.repos.test_change("t", "r", Some(&base), &[("a.txt", "1\n"), ("b.txt", "2\n")]);
        // The per-change green gate is satisfied directly; the MERGED-tree verify is what we exercise.
        app.repos.set_verification("t", "r", &head, true);
        put_pr(&app, "t/r", 1, "author", &head).await;
        put_approval(&app, "t/r", 1, "reviewer").await;

        let acting = actor("author", ActorKind::Human);
        let (code, msg) = {
            // Serialize the env mutation; make the local CI runner fail on the merged tree.
            let _g = MERGE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("HULL_CI_CMD", "exit 1");
            let r = perform_merge(&app, "t", "r", 1, &acting, false).await;
            std::env::remove_var("HULL_CI_CMD");
            r.expect_err("a red merged tree blocks the land")
        };
        assert_eq!(code, StatusCode::CONFLICT);
        assert!(msg.contains("merged result fails checks"), "msg: {msg}");
        assert!(!is_merged(&app, "t/r", 1).await, "PR stays open");
        assert_eq!(app.repos.head_change("t", "r").as_deref(), Some(base.as_str()), "main did not advance");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// On a protected branch, a green merged tree lands: `main` advances and the PR flips Merged.
    // Holds the env-isolation guard across the awaited merge so HULL_CI_CMD stays set for the
    // speculative verify; same reason as above the async mutex is not a clean swap.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn protected_land_succeeds_when_merged_tree_is_green() {
        let (app, tmp) = build_test_app("protgreen");
        app.store.put_actor(actor("author", ActorKind::Human)).await;
        app.store.put_actor(actor("reviewer", ActorKind::Human)).await;
        app.repo_settings.set("t/r", crate::reposettings::RepoSettings { require_review_to_land: true, ..Default::default() });
        let base = app.repos.test_commit("t", "r", "base", None, &[("a.txt", "1\n")]);
        let head = app.repos.test_change("t", "r", Some(&base), &[("a.txt", "1\n"), ("b.txt", "2\n")]);
        app.repos.set_verification("t", "r", &head, true);
        put_pr(&app, "t/r", 1, "author", &head).await;
        put_approval(&app, "t/r", 1, "reviewer").await;

        let acting = actor("author", ActorKind::Human);
        {
            let _g = MERGE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("HULL_CI_CMD", "exit 0"); // merged tree passes speculative verify
            let r = perform_merge(&app, "t", "r", 1, &acting, false).await;
            std::env::remove_var("HULL_CI_CMD");
            r.expect("green merged tree lands");
        }
        assert!(is_merged(&app, "t/r", 1).await);
        let tip = app.repos.head_change("t", "r").expect("main has a tip");
        assert_ne!(tip, base, "main advanced to the merge change");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── edit-comment authorization (`edit_comment`) ───────────────────────────────────────────────

    /// Mint a valid session token for `actor_id` so a handler's `require_actor` accepts it.
    fn mint_token(app: &App, token: &str, actor_id: &str) {
        app.auth.lock().unwrap().tokens.insert(token.into(), (actor_id.into(), now()));
    }

    fn bearer(token: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        h
    }

    #[tokio::test]
    async fn import_rejects_a_name_that_sanitizes_to_empty() {
        // The GitHub import destination name becomes a filesystem segment + store key, so it runs
        // through `sanitize_handle` like every other create path; a name that sanitizes to empty
        // (here ".") is rejected with 422 before any git shelling.
        let (app, tmp) = build_test_app("import-name");
        setup_org_repo(&app, "acme", "web", false, &[("owner", Role::Owner)]).await;
        app.store.put_actor(actor("owner", ActorKind::Human)).await;
        mint_token(&app, "tok", "owner");
        app.connections.set("acct-acme", crate::connections::Connection { provider: "github".into(), installation: "1".into(), login: "acme".into(), connected_unix: 0 });
        let resp = import_repo_handler(
            axum::extract::State(app.clone()),
            axum::extract::Path("acct-acme".to_string()),
            bearer("tok"),
            axum::Json(serde_json::json!({ "source": "evil/x", "name": "." })),
        ).await;
        assert_eq!(resp.status(), axum::http::StatusCode::UNPROCESSABLE_ENTITY, "an unsafe/empty destination name is refused");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn feed_ticket_requires_auth_binds_to_actor_and_expires() {
        // The SSE feed is gated by a short-lived ticket (SSE can't send an auth header). Minting
        // requires auth; the ticket resolves to the minting actor and stops resolving once expired.
        // Regression: `/api/feed` was fully unauthenticated — anyone could stream any org's activity.
        let (app, tmp) = build_test_app("feed-ticket");
        app.store.put_actor(actor("member", ActorKind::Human)).await;
        mint_token(&app, "tok", "member");
        // Unauthenticated mint → 401.
        let resp = feed_ticket(axum::extract::State(app.clone()), axum::http::HeaderMap::new()).await;
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED, "no session → no ticket");
        // Authenticated mint → 200, and the ticket resolves to that actor.
        let resp = feed_ticket(axum::extract::State(app.clone()), bearer("tok")).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let ticket = app.auth.lock().unwrap().feed_tickets.keys().next().cloned().unwrap();
        assert_eq!(resolve_feed_ticket(&app, &ticket).as_deref(), Some("member"), "ticket → minting actor");
        assert_eq!(resolve_feed_ticket(&app, "bogus"), None, "unknown ticket → nobody");
        // Backdate past the TTL → no longer resolves (and is pruned).
        for v in app.auth.lock().unwrap().feed_tickets.values_mut() {
            v.1 = now().saturating_sub(FEED_TICKET_TTL_SECS + 1);
        }
        assert_eq!(resolve_feed_ticket(&app, &ticket), None, "expired ticket → nobody");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn account_update_sanitizes_the_username_into_a_safe_handle() {
        // Regression (H1): PUT /api/account only trimmed the username, then copied it verbatim onto
        // the actor + personal-account HANDLE (a path segment / tenant) — a raw value could corrupt the
        // handle and lock the user out of passkey login. It must sanitize like every other handle path.
        let (app, tmp) = build_test_app("account-sanitize");
        app.store.put_actor(actor("u1actor", ActorKind::Human)).await;
        app.store
            .put_user(User {
                id: "u1".into(),
                username: "old".into(),
                email: "u@x.co".into(),
                actor: "u1actor".into(),
                secret_key: "00".into(),
                wrapped_key: None,
                passkeys: vec![],
                created_unix: 0,
                bio: String::new(),
            })
            .await;
        mint_token(&app, "tok", "u1actor");

        let call = |app: App, un: &str| {
            let body = serde_json::json!({ "username": un });
            async move { account_update(axum::extract::State(app), bearer("tok"), axum::Json(body)).await }
        };
        // a messy username is sanitized, not stored raw — and the actor handle is kept aligned + safe.
        assert_eq!(call(app.clone(), "new org!!").await.status(), axum::http::StatusCode::OK);
        assert_eq!(app.store.user_by_actor("u1actor").await.unwrap().username, "new_org", "username sanitized");
        assert_eq!(app.store.actor("u1actor").await.unwrap().handle, "new_org", "actor handle sanitized + aligned");
        // an all-symbol username sanitizes to empty → rejected, never stored as a broken handle.
        assert_eq!(call(app.clone(), "!!!").await.status(), axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(app.store.user_by_actor("u1actor").await.unwrap().username, "new_org", "rejected update left it unchanged");
        // email updates independently of the username block.
        let resp = account_update(
            axum::extract::State(app.clone()),
            bearer("tok"),
            axum::Json(serde_json::json!({ "email": "fresh@x.co" })),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let u = app.store.user_by_actor("u1actor").await.unwrap();
        assert_eq!((u.email.as_str(), u.username.as_str()), ("fresh@x.co", "new_org"), "email updated, username untouched");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn set_repo_autonomy_preserves_protected_paths_on_a_tier_only_change() {
        // Regression (H2): a tier-only PUT …/autonomy (the UI sends just {tier}) rebuilt the policy with
        // an empty protected_paths, silently WIPING the human-gated set and widening what agents may
        // auto-merge. It must patch-merge: preserve protected_paths unless the key is present.
        let (app, tmp) = build_test_app("autonomy-merge");
        setup_org_repo(&app, "acme", "web", false, &[("admin", Role::Admin)]).await;
        app.store.put_actor(actor("admin", ActorKind::Human)).await;
        mint_token(&app, "tok", "admin");
        app.autonomy.set_repo(
            "acme",
            "web",
            hull_core::AutonomyPolicy { tier: hull_core::AutonomyTier::T2, protected_paths: vec!["src/secret.rs".into()] },
        );

        let call = |app: App, body: Value| async move {
            set_repo_autonomy(
                axum::extract::State(app),
                axum::extract::Path(("acme".to_string(), "web".to_string())),
                bearer("tok"),
                axum::Json(body),
            )
            .await
        };
        // tier-only change → protected paths PRESERVED
        assert_eq!(call(app.clone(), serde_json::json!({ "tier": "t3" })).await.status(), axum::http::StatusCode::OK);
        let pol = app.autonomy.get_repo("acme", "web").unwrap();
        assert_eq!(pol.tier, hull_core::AutonomyTier::T3, "tier updated");
        assert_eq!(pol.protected_paths, vec!["src/secret.rs".to_string()], "protected paths preserved on a tier-only change");
        // explicitly sending protected_paths DOES replace them
        assert_eq!(
            call(app.clone(), serde_json::json!({ "tier": "t3", "protected_paths": ["a", "b"] })).await.status(),
            axum::http::StatusCode::OK
        );
        assert_eq!(app.autonomy.get_repo("acme", "web").unwrap().protected_paths, vec!["a".to_string(), "b".to_string()]);

        // same patch-merge behavior for the account-level setter (identical code path).
        app.autonomy.set_account(
            "acct-acme",
            hull_core::AutonomyPolicy { tier: hull_core::AutonomyTier::T1, protected_paths: vec!["acct/keep.rs".into()] },
        );
        let resp = set_account_autonomy(
            axum::extract::State(app.clone()),
            axum::extract::Path("acct-acme".to_string()),
            bearer("tok"),
            axum::Json(serde_json::json!({ "tier": "t2" })),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let apol = app.autonomy.get_account("acct-acme").unwrap();
        assert_eq!(apol.tier, hull_core::AutonomyTier::T2);
        assert_eq!(apol.protected_paths, vec!["acct/keep.rs".to_string()], "account protected paths preserved on tier-only change");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn notifications_require_auth_and_scope_to_the_authenticated_actor() {
        // Regression (B2): `/api/notifications` was unauthenticated and trusted a `?actor=` query
        // param — any anon caller could read any inbox, or (no param) the whole-server firehose. It
        // must require a token, derive the actor from it, and gate broadcasts about a PRIVATE repo on
        // read access so private activity doesn't leak.
        let (app, tmp) = build_test_app("notifications-auth");
        app.store.put_actor(actor("alice", ActorKind::Human)).await;
        app.store.put_actor(actor("bob", ActorKind::Human)).await;
        mint_token(&app, "atok", "alice");
        // a private repo alice is NOT a member of (bob is)
        setup_org_repo(&app, "acme", "secret", true, &[("bob", Role::Write)]).await;
        {
            let mk = |kind: &str, to: Vec<String>, summary: &str, ts: u64, repo: Option<String>| Notification {
                kind: kind.into(), to, summary: summary.into(), change: None, ts, repo, target_kind: None, target_number: None,
            };
            let mut n = app.notifications.lock().unwrap();
            n.push(mk("mention", vec!["alice".into()], "for-alice", 1, None));
            n.push(mk("mention", vec!["bob".into()], "for-bob", 2, None));
            n.push(mk("ci", vec![], "public-broadcast", 3, Some("acme/pubrepo".into())));
            n.push(mk("ci", vec![], "private-broadcast", 4, Some("acme/secret".into())));
            n.push(mk("ci", vec![], "malformed-broadcast", 5, Some("no-slash-here".into())));
            n.push(mk("sys", vec![], "server-wide", 6, None));
        }

        // No token → 401, no inbox.
        let resp = notifications_list(axum::extract::State(app.clone()), axum::http::HeaderMap::new()).await;
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED, "no token → no inbox");

        // Alice: her own + the public broadcast; NOT bob's inbox, NOT the private-repo broadcast.
        let resp = notifications_list(axum::extract::State(app.clone()), bearer("atok")).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let seen: Vec<&str> =
            body["notifications"].as_array().unwrap().iter().map(|x| x["summary"].as_str().unwrap()).collect();
        assert!(seen.contains(&"for-alice"), "own notification delivered; got {seen:?}");
        assert!(seen.contains(&"public-broadcast"), "public broadcast delivered; got {seen:?}");
        assert!(seen.contains(&"server-wide"), "repo-less server-wide broadcast delivered; got {seen:?}");
        assert!(!seen.contains(&"for-bob"), "must NOT leak another actor's inbox; got {seen:?}");
        assert!(!seen.contains(&"private-broadcast"), "must NOT leak a private repo's broadcast; got {seen:?}");
        assert!(!seen.contains(&"malformed-broadcast"), "a broadcast with an unparseable repo is default-denied; got {seen:?}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn substrate_view_returns_verified_and_annotated_state() {
        // The consumer: read a repo's ref + provenance back from the (loopback) relay through the
        // handler and confirm the verified view + accountability/authority annotation — including that a
        // valid signature from a REVOKED member does NOT read as accountable/authorized.
        let (mut app, tmp) = build_test_app("substrate");
        let url = crate::nostr::spawn_loopback_relay();
        let instance_sk = "0000000000000000000000000000000000000000000000000000000000000001";
        app.nostr_refs = Some(std::sync::Arc::new(crate::nostr::NostrRefs::new(instance_sk.into(), vec![url])));
        // an accountable human member...
        let author = hull_core::identity::mint_human("mira");
        app.store.put_actor(author.actor.clone()).await;
        // ...and a member whose key was compromised, then revoked.
        let gone = hull_core::identity::mint_human("gone");
        let gone_secret = gone.secret_key.clone();
        let gone_id = gone.actor.id.clone();
        let mut gone_actor = gone.actor.clone();
        gone_actor.revoked = true;
        app.store.put_actor(gone_actor).await;
        setup_org_repo(&app, "acme", "web", false, &[(&author.actor.id, Role::Write), (&gone_id, Role::Write)]).await;

        let refs = app.nostr_refs.clone().unwrap();
        refs.publish_ref("acme/web", "main", "blake3:tip", None).unwrap();
        refs.publish_provenance(&author.secret_key, "blake3:good", &author.actor.id, "acme/web", "landed it").unwrap();
        // a genuinely-valid signature by the revoked member — the attacker holds the leaked key.
        refs.publish_provenance(&gone_secret, "blake3:revoked", &gone_id, "acme/web", "forged after revocation").unwrap();

        let resp = substrate_view(
            axum::extract::State(app.clone()),
            axum::extract::Path(("acme".to_string(), "web".to_string())),
            axum::http::HeaderMap::new(), // public repo → readable without auth
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["enabled"], true);
        assert_eq!(v["ref"]["commit"], "blake3:tip", "ref reads back from the relay");
        let prov = v["provenance"].as_array().unwrap();

        let good = prov.iter().find(|p| p["change"] == "blake3:good").expect("author's row");
        assert_eq!(good["signatures_valid"], true);
        assert_eq!(good["accountable"], true, "live human member is accountable");
        assert_eq!(good["authorized"], true, "member of the owning account is authorized");

        let bad = prov.iter().find(|p| p["change"] == "blake3:revoked").expect("revoked member's row is present");
        assert_eq!(bad["signatures_valid"], true, "the signature really is valid (attacker holds the key)");
        assert_eq!(bad["accountable"], false, "but a REVOKED actor must not read as accountable");
        assert_eq!(bad["authorized"], false, "and therefore not authorized");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn sovereign_account_is_non_custodial_end_to_end() {
        // A sovereign (non-custodial) account: Hull stores the PUBLIC key + a passphrase-encrypted
        // bundle, never a plaintext secret, and refuses to sign for it — yet the account still logs in
        // via the normal challenge→sign flow (the client holds the key). This drives the whole path.
        let (app, tmp) = build_test_app("sovereign");
        // the client's keypair (in reality generated + kept in the browser)
        let kp = identity::mint_human("whoever");
        let pubkey = kp.actor.id.clone();
        let secret = kp.secret_key.clone();

        // register: sign the (username,pubkey) binding to prove possession, send only the pubkey + a
        // (here opaque) wrapped bundle.
        let proof = identity::sign(&secret, format!("hull-sovereign:v1\nusername=nomad\npubkey={pubkey}").as_bytes()).unwrap();
        let reg = |app: App, body: Value| async move { sovereign_register(axum::extract::State(app), axum::Json(body)).await };
        let good = serde_json::json!({ "username": "nomad", "email": "n@x.co", "pubkey": pubkey, "wrapped_key": "ENC(...)", "signature": proof });
        assert_eq!(reg(app.clone(), good.clone()).await.status(), axum::http::StatusCode::CREATED);

        // stored NON-custodially: no plaintext secret, but the wrapped bundle is kept, and the actor is
        // the client's public key.
        let u = app.store.user_by_username("nomad").await.unwrap();
        assert!(u.secret_key.is_empty(), "Hull must hold NO plaintext secret for a sovereign account");
        assert_eq!(u.wrapped_key.as_deref(), Some("ENC(...)"));
        assert_eq!(u.actor, pubkey, "actor id is the client's public key");

        // a bad proof-of-possession is rejected (can't bind a key you don't hold).
        let forged = serde_json::json!({ "username": "imposter", "email": "", "pubkey": pubkey, "wrapped_key": "x", "signature": "00" });
        assert_eq!(reg(app.clone(), forged).await.status(), axum::http::StatusCode::UNAUTHORIZED);

        // Hull REFUSES to server-sign a delegation for the sovereign parent (the core invariant).
        mint_token(&app, "ntok", &pubkey);
        let resp = register_actor(
            axum::extract::State(app.clone()),
            bearer("ntok"),
            axum::Json(serde_json::json!({ "kind": "agent", "handle": "bot" })),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::UNPROCESSABLE_ENTITY, "Hull must not sign for a sovereign account");
        let msg = String::from_utf8(axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
        assert!(msg.contains("sovereign"), "error should explain the client must sign; got {msg:?}");

        // yet the account LOGS IN through the normal flow — the client decrypts its key and signs the nonce.
        let nonce = auth_challenge(axum::extract::State(app.clone())).await.0["nonce"].as_str().unwrap().to_string();
        let login_sig = identity::sign(&secret, format!("hull-login:{nonce}").as_bytes()).unwrap();
        let resp = auth_login(
            axum::extract::State(app.clone()),
            axum::Json(serde_json::json!({ "actor": pubkey, "nonce": nonce, "signature": login_sig })),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::CREATED, "sovereign login works via the existing challenge→sign flow");

        // the wrapped bundle is fetchable (for cross-device login) by username.
        let resp = sovereign_wrapped(
            axum::extract::State(app.clone()),
            axum::extract::Query(std::collections::HashMap::from([("username".to_string(), "nomad".to_string())])),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap()["wrapped_key"].as_str(), Some("ENC(...)"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn abandoned_passkey_flows_are_pruned_on_next_start() {
        // Regression: the register/add/auth ceremony maps had no TTL prune, so an unauthenticated
        // `.../start` loop grew memory without bound. Start a real ceremony, backdate it past the TTL,
        // then start another — the abandoned flow must be pruned, leaving only the fresh one.
        let (app, tmp) = build_test_app("passkey-ttl");
        let start = |app: App, name: &str| {
            let body = serde_json::json!({ "username": name, "email": format!("{name}@example.com") });
            async move { register_start(axum::extract::State(app), axum::Json(body)).await.status() }
        };
        assert_eq!(start(app.clone(), "alice").await, axum::http::StatusCode::OK);
        // Backdate the in-flight flow to simulate an abandoned ceremony.
        {
            let mut a = app.auth.lock().unwrap();
            assert_eq!(a.reg_flows.len(), 1, "one ceremony in flight");
            for f in a.reg_flows.values_mut() {
                f.created_unix = now().saturating_sub(CEREMONY_TTL_SECS + 1);
            }
        }
        assert_eq!(start(app.clone(), "bob").await, axum::http::StatusCode::OK);
        assert_eq!(app.auth.lock().unwrap().reg_flows.len(), 1, "the abandoned flow was pruned; only the fresh one remains");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_issue_creates_get_distinct_contiguous_numbers() {
        // The number is allocated as MAX(number)+1 then inserted; without a lock across that
        // read-then-insert, concurrent creates can be handed the same number. Fire many at once and
        // assert every issue got a distinct, contiguous number (1..=N) — deterministic once the
        // allocation is serialized.
        let (app, tmp) = build_test_app("concurrent-issues");
        setup_org_repo(&app, "acme", "web", false, &[("member", Role::Write)]).await;
        app.store.put_actor(actor("member", ActorKind::Human)).await;
        mint_token(&app, "tok", "member");
        const N: u64 = 24;
        let mut tasks = Vec::new();
        for i in 0..N {
            let app = app.clone();
            tasks.push(tokio::spawn(async move {
                create_issue(
                    axum::extract::State(app),
                    axum::extract::Path(("acme".to_string(), "web".to_string())),
                    bearer("tok"),
                    axum::Json(serde_json::json!({ "title": format!("issue {i}") })),
                ).await.status()
            }));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap(), axum::http::StatusCode::CREATED, "each create succeeds");
        }
        let mut nums: Vec<u64> = app.store.issues("acme/web").await.iter().map(|i| i.number).collect();
        nums.sort_unstable();
        assert_eq!(nums, (1..=N).collect::<Vec<_>>(), "N concurrent creates → distinct, contiguous numbers");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn edit_comment_is_author_only() {
        let (app, tmp) = build_test_app("edit-comment");
        app.store.put_actor(actor("author", ActorKind::Human)).await;
        app.store.put_actor(actor("intruder", ActorKind::Human)).await;
        mint_token(&app, "tok-author", "author");
        mint_token(&app, "tok-intruder", "intruder");
        app.store.put_comment(Comment {
            id: "cm_1".into(),
            repo: "acme/web".into(),
            target: "pr:1".into(),
            author: "author".into(),
            body: "original".into(),
            created_unix: 0,
            path: None,
            line: None,
            edited_unix: None,
        }).await;

        // A non-author is rejected server-side (not just hidden in the UI) and the body is untouched.
        let resp = edit_comment(
            State(app.clone()),
            Path(("acme".into(), "web".into(), "cm_1".into())),
            bearer("tok-intruder"),
            Json(json!({ "body": "hijacked" })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "a non-author must not be allowed to edit");
        assert_eq!(app.store.comments("acme/web").await[0].body, "original", "a rejected edit leaves the body unchanged");

        // The author can edit: the body updates and `edited_unix` is stamped.
        let resp = edit_comment(
            State(app.clone()),
            Path(("acme".into(), "web".into(), "cm_1".into())),
            bearer("tok-author"),
            Json(json!({ "body": "revised" })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "the author may edit their own comment");
        let c = app.store.comments("acme/web").await.into_iter().find(|c| c.id == "cm_1").unwrap();
        assert_eq!(c.body, "revised");
        assert!(c.edited_unix.is_some(), "an accepted edit stamps edited_unix");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── edit-issue authorization (`update_issue` action:"edit") ───────────────────────────────────

    async fn put_issue_fixture(app: &App, key: &str, number: u64, author: &str) {
        app.store.put_issue(Issue {
            id: format!("iss_{number}"),
            repo: key.into(),
            number,
            title: "original title".into(),
            body: "original body".into(),
            author: author.into(),
            assignees: vec![],
            labels: vec![],
            projects: vec![],
            status: IssueStatus::Open,
            code_refs: vec![],
            referenced_actors: vec![],
            linked_prs: vec![],
            resolved_by: None,
            created_unix: 0,
            edited_unix: None,
        }).await;
    }

    #[tokio::test]
    async fn edit_issue_title_and_body_is_author_only() {
        let (app, tmp) = build_test_app("edit-issue");
        // An org account with `intruder` as Owner (⇒ a repo admin) — to prove even an admin can't
        // rewrite the author's words, unlike close/label which an admin may do.
        app.store.put_account(Account {
            id: "acct-acme".into(),
            kind: AccountKind::Organization,
            handle: "acme".into(),
            members: vec![Membership { actor: "intruder".into(), role: Role::Owner }],
        }).await;
        app.store.put_actor(actor("author", ActorKind::Human)).await;
        app.store.put_actor(actor("intruder", ActorKind::Human)).await;
        mint_token(&app, "tok-author", "author");
        mint_token(&app, "tok-intruder", "intruder");
        put_issue_fixture(&app, "acme/web", 1, "author").await;

        // A non-author (even a repo admin) is rejected server-side, and the title/body are untouched.
        let resp = update_issue(
            State(app.clone()),
            Path(("acme".into(), "web".into(), 1)),
            bearer("tok-intruder"),
            Json(json!({ "action": "edit", "title": "hijacked", "body": "hijacked" })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "a non-author (even an admin) must not edit an issue's words");
        let i = app.store.issues("acme/web").await.into_iter().find(|i| i.number == 1).unwrap();
        assert_eq!(i.title, "original title", "a rejected edit leaves the title unchanged");
        assert_eq!(i.body, "original body", "a rejected edit leaves the body unchanged");

        // An empty title is rejected even for the author.
        let resp = update_issue(
            State(app.clone()),
            Path(("acme".into(), "web".into(), 1)),
            bearer("tok-author"),
            Json(json!({ "action": "edit", "title": "   " })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "an empty title must be rejected");
        assert_eq!(app.store.issues("acme/web").await[0].title, "original title", "a rejected edit leaves the title unchanged");

        // The author can edit: title + body update and `edited_unix` is stamped.
        let resp = update_issue(
            State(app.clone()),
            Path(("acme".into(), "web".into(), 1)),
            bearer("tok-author"),
            Json(json!({ "action": "edit", "title": "revised title", "body": "revised body" })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "the author may edit their own issue");
        let i = app.store.issues("acme/web").await.into_iter().find(|i| i.number == 1).unwrap();
        assert_eq!(i.title, "revised title");
        assert_eq!(i.body, "revised body");
        assert!(i.edited_unix.is_some(), "an accepted edit stamps edited_unix");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── repo danger zone (rename / delete) is owner/admin only ────────────────────────────────────

    /// Seed a tenant account (`handle`, id `acct_<handle>`) with `boss` as Owner, and a repo record
    /// `<handle>/<name>`. `boss` and `rando` are accountable humans with session tokens minted.
    async fn seed_repo_admin_fixture(app: &App, handle: &str, name: &str) {
        app.store.put_actor(actor("boss", ActorKind::Human)).await;
        app.store.put_actor(actor("rando", ActorKind::Human)).await;
        mint_token(app, "tok-boss", "boss");
        mint_token(app, "tok-rando", "rando");
        app.store.put_account(Account {
            id: format!("acct_{handle}"),
            kind: AccountKind::Organization,
            handle: handle.into(),
            members: vec![Membership { actor: "boss".into(), role: Role::Owner }],
        }).await;
        app.store.put_repo(Repo {
            id: format!("repo_{handle}_{name}"),
            owner: format!("acct_{handle}"),
            name: name.into(),
            default_branch: "main".into(),
        }).await;
    }

    #[tokio::test]
    async fn repo_delete_is_admin_only() {
        let (app, tmp) = build_test_app("repo-delete");
        seed_repo_admin_fixture(&app, "acme", "web").await;
        let key = "acme/web";
        put_pr(&app, key, 1, "boss", "deadbeef").await;
        // A durable human judgment on a claim under this repo — it must be purged on delete so it
        // can't resurface if the name is recreated.
        app.claims.set(key, "chg1", "claimA", claims::ClaimResolution { by: "boss".into(), judgment: "verified".into(), note: "checked".into(), ts: 1 });

        // A non-admin actor is rejected server-side — and nothing is removed.
        let resp = delete_repo_handler(State(app.clone()), Path(("acme".into(), "web".into())), bearer("tok-rando")).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "a non-admin must not delete a repo");
        assert!(find_repo(&app, "acme", "web").await.is_some(), "a rejected delete leaves the repo intact");
        assert_eq!(app.store.prs(key).await.len(), 1, "a rejected delete leaves PRs intact");
        assert!(app.claims.for_change(key, "chg1").contains_key("claimA"), "a rejected delete leaves claim resolutions intact");

        // The owner may delete: the repo record and its domain state are gone.
        let resp = delete_repo_handler(State(app.clone()), Path(("acme".into(), "web".into())), bearer("tok-boss")).await;
        assert_eq!(resp.status(), StatusCode::OK, "an owner may delete the repo");
        assert!(find_repo(&app, "acme", "web").await.is_none(), "the repo record is removed");
        assert!(app.store.prs(key).await.is_empty(), "the repo's PRs are purged");
        assert!(app.claims.for_change(key, "chg1").is_empty(), "the repo's claim resolutions are purged");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn repo_rename_is_admin_only_and_rekeys_state() {
        let (app, tmp) = build_test_app("repo-rename");
        seed_repo_admin_fixture(&app, "acme", "web").await;
        put_pr(&app, "acme/web", 1, "boss", "deadbeef").await;
        // A durable human judgment on a claim under the old name — it must follow the rename.
        app.claims.set("acme/web", "chg1", "claimA", claims::ClaimResolution { by: "boss".into(), judgment: "verified".into(), note: "checked".into(), ts: 1 });

        // A non-admin actor is rejected server-side — the name is unchanged.
        let resp = rename_repo_handler(
            State(app.clone()),
            Path(("acme".into(), "web".into())),
            bearer("tok-rando"),
            Json(json!({ "name": "site" })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "a non-admin must not rename a repo");
        assert!(find_repo(&app, "acme", "web").await.is_some(), "a rejected rename leaves the old name");

        // The owner may rename: the repo record and its PRs re-key to the new name.
        let resp = rename_repo_handler(
            State(app.clone()),
            Path(("acme".into(), "web".into())),
            bearer("tok-boss"),
            Json(json!({ "name": "site" })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "an owner may rename the repo");
        assert!(find_repo(&app, "acme", "web").await.is_none(), "the old name no longer resolves");
        assert!(find_repo(&app, "acme", "site").await.is_some(), "the new name resolves");
        assert!(app.store.prs("acme/web").await.is_empty(), "PRs no longer sit under the old key");
        assert_eq!(app.store.prs("acme/site").await.len(), 1, "PRs re-key to the new name");
        // The human claim judgment follows the rename: gone under the old repo, visible under the new.
        assert!(app.claims.for_change("acme/web", "chg1").is_empty(), "claim resolutions no longer sit under the old key");
        let resolved = app.claims.for_change("acme/site", "chg1");
        assert_eq!(resolved.get("claimA").map(|r| r.judgment.as_str()), Some("verified"), "the claim resolution re-keys to the new name");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── authz-hardening: membership helpers + read/mutation gates ──────────────────────────────────
    //
    // Truth tables for the shared helpers (area A), the private-repo read gate (area B) driven through
    // a real handler, and the mutation gates (area C) on `verify_change` / `set_owners` / a member-only
    // endpoint plus the same-org scoping of `independent_agent_reviewer`.

    /// Seed an org whose handle == `tenant`, owning `repo`, with the given members and visibility — the
    /// exact shape `find_repo` / `repo_account_id` / `can_read_repo` read.
    async fn setup_org_repo(app: &App, tenant: &str, repo: &str, private: bool, members: &[(&str, Role)]) {
        let acct_id = format!("acct-{tenant}");
        app.store.put_account(Account {
            id: acct_id.clone(),
            kind: AccountKind::Organization,
            handle: tenant.into(),
            members: members.iter().map(|(a, r)| Membership { actor: (*a).into(), role: *r }).collect(),
        }).await;
        app.store.put_repo(Repo { id: format!("repo-{tenant}-{repo}"), owner: acct_id, name: repo.into(), default_branch: "main".into() }).await;
        app.repo_settings.set(&format!("{tenant}/{repo}"), crate::reposettings::RepoSettings { private, ..Default::default() });
    }

    fn set_private(app: &App, key: &str, private: bool) {
        app.repo_settings.set(key, crate::reposettings::RepoSettings { private, ..Default::default() });
    }

    #[tokio::test]
    async fn can_read_repo_truth_table() {
        let (app, tmp) = build_test_app("can-read");
        setup_org_repo(&app, "acme", "web", true, &[("member", Role::Write)]).await;
        // PRIVATE: only members read; anonymous and outsiders are refused.
        assert!(!can_read_repo(&app, None, "acme", "web").await, "anonymous cannot read a private repo");
        assert!(!can_read_repo(&app, Some("outsider"), "acme", "web").await, "a non-member cannot read a private repo");
        assert!(can_read_repo(&app, Some("member"), "acme", "web").await, "a member can read a private repo");
        // PUBLIC (and unlisted, which is `!private`): readable by anyone, including anonymous.
        set_private(&app, "acme/web", false);
        assert!(can_read_repo(&app, None, "acme", "web").await, "anonymous CAN read a public repo");
        assert!(can_read_repo(&app, Some("outsider"), "acme", "web").await, "a non-member can read a public repo");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn is_repo_member_and_admin_truth_table() {
        let (app, tmp) = build_test_app("is-member");
        setup_org_repo(&app, "acme", "web", true, &[("owner", Role::Owner), ("dev", Role::Write), ("reader", Role::Read)]).await;
        // is_repo_member: ANY role of the owning account.
        assert!(is_repo_member(&app, "acme", "web", "owner").await);
        assert!(is_repo_member(&app, "acme", "web", "dev").await);
        assert!(is_repo_member(&app, "acme", "web", "reader").await, "even a Read role is a member");
        assert!(!is_repo_member(&app, "acme", "web", "outsider").await);
        // Visibility is irrelevant to membership: a public repo is still only *mutated* by members.
        set_private(&app, "acme/web", false);
        assert!(!is_repo_member(&app, "acme", "web", "outsider").await, "a public repo does not make an outsider a member");
        // is_repo_admin: Owner/Admin only, a strict subset of members.
        assert!(is_repo_admin(&app, "acme", "web", "owner").await);
        assert!(!is_repo_admin(&app, "acme", "web", "dev").await, "Write is a member but not an admin");
        assert!(!is_repo_admin(&app, "acme", "web", "reader").await, "Read is a member but not an admin");
        assert!(!is_repo_admin(&app, "acme", "web", "outsider").await);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn git_auth_denies_a_revoked_actor() {
        // A revoked actor's still-unexpired session token must not keep git access — the git path now
        // runs the same `accountable` check the REST path does. Regression: revoke set the flag but the
        // git gate ignored it, so the token kept pushing until it expired (~30 days).
        let (app, tmp) = build_test_app("git-revoked");
        setup_org_repo(&app, "acme", "web", true, &[("member", Role::Write)]).await;
        let mut a = actor("member", ActorKind::Human);
        a.revoked = true;
        app.store.put_actor(a).await;
        mint_token(&app, "tok", "member");
        // The token resolves to a repo member, but revocation makes it unaccountable → denied.
        assert_eq!(
            git_auth_decision(&app, true, "acme", "web", "git-receive-pack", Some("tok")).await,
            GitAuthDecision::Unauthorized, "revoked actor cannot push",
        );
        assert_eq!(
            git_auth_decision(&app, true, "acme", "web", "git-upload-pack", Some("tok")).await,
            GitAuthDecision::Unauthorized, "revoked actor cannot fetch a private repo",
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn read_only_team_grant_is_not_write_membership() {
        // A team granted a "read" role can READ the repo but must NOT pass the write-side gate
        // (`is_repo_member`, which feeds git push and every mutation). A "write"/"admin" grant does.
        let (app, tmp) = build_test_app("team-role");
        setup_org_repo(&app, "acme", "web", true, &[("owner", Role::Owner)]).await;
        app.store.put_actor(actor("reader", ActorKind::Human)).await;
        app.store.put_actor(actor("writer", ActorKind::Human)).await;
        app.store.put_team(hull_core::Team { id: "team_r".into(), account: "acct-acme".into(), name: "readers".into(), members: vec![Membership { actor: "reader".into(), role: Role::Read }] }).await;
        app.store.put_team(hull_core::Team { id: "team_w".into(), account: "acct-acme".into(), name: "writers".into(), members: vec![Membership { actor: "writer".into(), role: Role::Write }] }).await;
        app.repo_settings.set("acme/web", crate::reposettings::RepoSettings {
            private: true,
            team_access: vec![
                crate::reposettings::TeamAccess { team: "team_r".into(), role: "read".into() },
                crate::reposettings::TeamAccess { team: "team_w".into(), role: "write".into() },
            ],
            ..Default::default()
        });
        // Read grant: can read, but is NOT a write-side member.
        assert!(can_read_repo(&app, Some("reader"), "acme", "web").await, "read grant → readable");
        assert!(!is_repo_member(&app, "acme", "web", "reader").await, "read grant must not confer push");
        // Write grant: both.
        assert!(can_read_repo(&app, Some("writer"), "acme", "web").await);
        assert!(is_repo_member(&app, "acme", "web", "writer").await, "write grant confers push");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn ci_result_callback_requires_configured_external_ci_and_secret() {
        // Regression for the authz bypass: the `ci-result` callback is the EXTERNAL-CI half of the
        // contract. With the default built-in local runner (no external CI configured) there is no
        // legitimate caller, so an anonymous callback MUST be refused — otherwise anyone could POST
        // `{status:"green"}` for a real change and drive `set_verification`, defeating the merge gate.
        let (app, tmp) = build_test_app("ci-result-auth");
        setup_org_repo(&app, "acme", "web", false, &[("owner", Role::Owner)]).await;
        let route = |id: &str| axum::extract::Path((String::from("acme"), String::from("web"), id.to_string()));
        let call = |app: App, hdrs: axum::http::HeaderMap| async move {
            ci_result(axum::extract::State(app), route("deadbeefcafebabe"), hdrs, axum::Json(serde_json::json!({ "status": "green" }))).await
        };

        // (1) No external CI configured → refuse anonymous callbacks (the bypass).
        let resp = call(app.clone(), axum::http::HeaderMap::new()).await;
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN, "no external CI configured → callback refused");

        // Configure an external endpoint WITH a secret for this repo.
        app.ci_config.set("acme/web", crate::ci::RepoCi { url: "http://ci.example".into(), secret: "s3cr3t".into() });

        // (2) Configured, no secret header → 401.
        let resp = call(app.clone(), axum::http::HeaderMap::new()).await;
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED, "missing secret → 401");

        // (3) Configured, wrong secret → 401.
        let mut wrong = axum::http::HeaderMap::new();
        wrong.insert("X-Hull-CI-Secret", "nope".parse().unwrap());
        let resp = call(app.clone(), wrong).await;
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED, "wrong secret → 401");

        // (4) Configured, correct secret → accepted (200). (The fake change id resolves to no tree, so
        // no verification is written, but the callback is authorized and records the verdict.)
        let mut ok = axum::http::HeaderMap::new();
        ok.insert("X-Hull-CI-Secret", "s3cr3t".parse().unwrap());
        let resp = call(app.clone(), ok).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK, "correct secret → accepted");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn git_basic_and_bearer_token_extraction() {
        use base64::Engine;
        // HTTP Basic: git sends base64("user:password"); the PASSWORD is the token, username ignored.
        let creds = base64::engine::general_purpose::STANDARD.encode("x-access-token:tok-123");
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::AUTHORIZATION, format!("Basic {creds}").parse().unwrap());
        assert_eq!(git_token_from_headers(&h).as_deref(), Some("tok-123"), "Basic password is the token");
        // A token with no username still resolves (git-credential emits ":<token>" shapes too).
        let creds = base64::engine::general_purpose::STANDARD.encode(":only-pass");
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::AUTHORIZATION, format!("Basic {creds}").parse().unwrap());
        assert_eq!(git_token_from_headers(&h).as_deref(), Some("only-pass"));
        // Bearer fallback.
        assert_eq!(git_token_from_headers(&bearer("tok-b")).as_deref(), Some("tok-b"));
        // No/empty credential → None.
        assert_eq!(git_token_from_headers(&axum::http::HeaderMap::new()), None);
        let empty = base64::engine::general_purpose::STANDARD.encode("user:");
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::AUTHORIZATION, format!("Basic {empty}").parse().unwrap());
        assert_eq!(git_token_from_headers(&h), None, "empty password is not a token");
    }

    #[tokio::test]
    async fn git_auth_decision_matrix() {
        // Area D: the pure decision over public/private × fetch/push × member/non-member/anon, with the
        // config-off case proven a total no-op (always Allow) regardless of visibility/creds.
        let (app, tmp) = build_test_app("git-auth");
        setup_org_repo(&app, "acme", "web", true, &[("member", Role::Write)]).await;
        app.store.put_actor(actor("member", ActorKind::Human)).await;
        app.store.put_actor(actor("outsider", ActorKind::Human)).await;
        mint_token(&app, "tok-member", "member");
        mint_token(&app, "tok-outsider", "outsider");
        let fetch = "git-upload-pack";
        let push = "git-receive-pack";
        macro_rules! d { ($e:expr, $s:expr, $t:expr) => { git_auth_decision(&app, $e, "acme", "web", $s, $t).await }; }

        // ── enforce OFF: ALWAYS Allow, whatever the repo/service/creds. The no-op guarantee. ──
        for service in [fetch, push] {
            for token in [None, Some("tok-member"), Some("tok-outsider"), Some("bad")] {
                assert_eq!(d!(false, service, token), GitAuthDecision::Allow, "config off is always Allow ({service}, {token:?})");
            }
        }

        // ── enforce ON, PRIVATE repo ──
        // Fetch: anon/non-member/invalid → 401 (so git prompts); a read-authorized member → Allow.
        assert_eq!(d!(true, fetch, None), GitAuthDecision::Unauthorized, "anon fetch of a private repo → 401");
        assert_eq!(d!(true, fetch, Some("bad")), GitAuthDecision::Unauthorized, "invalid token fetch of private → 401");
        assert_eq!(d!(true, fetch, Some("tok-outsider")), GitAuthDecision::Unauthorized, "non-member fetch of private → 401");
        assert_eq!(d!(true, fetch, Some("tok-member")), GitAuthDecision::Allow, "member fetch of a private repo → Allow");
        // Push: anon/invalid → 401; a valid non-member → 403; a member → Allow.
        assert_eq!(d!(true, push, None), GitAuthDecision::Unauthorized, "anon push → 401");
        assert_eq!(d!(true, push, Some("bad")), GitAuthDecision::Unauthorized, "invalid token push → 401");
        assert_eq!(d!(true, push, Some("tok-outsider")), GitAuthDecision::Forbidden, "non-member push → 403");
        assert_eq!(d!(true, push, Some("tok-member")), GitAuthDecision::Allow, "member push → Allow");

        // ── enforce ON, PUBLIC repo ── anonymous clone MUST still work; push still needs a member.
        set_private(&app, "acme/web", false);
        assert_eq!(d!(true, fetch, None), GitAuthDecision::Allow, "anon fetch of a PUBLIC repo → Allow even when enforcing");
        assert_eq!(d!(true, fetch, Some("tok-outsider")), GitAuthDecision::Allow, "non-member fetch of a public repo → Allow");
        assert_eq!(d!(true, push, None), GitAuthDecision::Unauthorized, "anon push to a public repo still → 401");
        assert_eq!(d!(true, push, Some("tok-outsider")), GitAuthDecision::Forbidden, "non-member push to a public repo → 403");
        assert_eq!(d!(true, push, Some("tok-member")), GitAuthDecision::Allow, "member push to a public repo → Allow");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn git_auth_create_on_push_requires_account_member() {
        // Area D: auto-create-on-push must not let an anon/outsider provision a repo. For a not-yet-
        // existent repo under account handle `acme`, the push decision is Allow only for an `acme`
        // member (via `is_repo_member`'s tenant→account resolution), else 401 (anon) / 403 (outsider).
        let (app, tmp) = build_test_app("git-create");
        // Account exists (handle == tenant) but the repo record/dir does not yet.
        setup_org_repo(&app, "acme", "placeholder", false, &[("member", Role::Write)]).await;
        app.store.put_actor(actor("member", ActorKind::Human)).await;
        app.store.put_actor(actor("outsider", ActorKind::Human)).await;
        mint_token(&app, "tok-member", "member");
        mint_token(&app, "tok-outsider", "outsider");
        let push = "git-receive-pack";
        assert_eq!(git_auth_decision(&app, true, "acme", "brand-new", push, None).await, GitAuthDecision::Unauthorized, "anon cannot provision a new repo by push");
        assert_eq!(git_auth_decision(&app, true, "acme", "brand-new", push, Some("tok-outsider")).await, GitAuthDecision::Forbidden, "an outsider cannot provision a new repo under acme");
        assert_eq!(git_auth_decision(&app, true, "acme", "brand-new", push, Some("tok-member")).await, GitAuthDecision::Allow, "an acme member may provision a new repo by push");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn private_repo_read_is_gated_but_public_stays_open() {
        // Area B: drive a real gated handler (`change_diff`) — a private repo is hidden (404) from
        // anonymous and non-members, visible to a member, and flips fully open once public.
        let (app, tmp) = build_test_app("read-gate");
        setup_org_repo(&app, "acme", "web", true, &[("member", Role::Write)]).await;
        app.store.put_actor(actor("member", ActorKind::Human)).await;
        app.store.put_actor(actor("outsider", ActorKind::Human)).await;
        mint_token(&app, "tok-member", "member");
        mint_token(&app, "tok-outsider", "outsider");
        let t = ("acme".to_string(), "web".to_string(), "deadbeef".to_string());

        let r = change_diff(State(app.clone()), Path(t.clone()), axum::http::HeaderMap::new()).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "anonymous read of a private repo is 404");
        let r = change_diff(State(app.clone()), Path(t.clone()), bearer("tok-outsider")).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "a non-member (valid token) still gets 404 on a private repo");
        let r = change_diff(State(app.clone()), Path(t.clone()), bearer("tok-member")).await;
        assert_eq!(r.status(), StatusCode::OK, "a member reads a private repo");

        set_private(&app, "acme/web", false);
        let r = change_diff(State(app.clone()), Path(t.clone()), axum::http::HeaderMap::new()).await;
        assert_eq!(r.status(), StatusCode::OK, "a public repo is readable by anyone, incl. anonymous");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn verify_change_is_repo_admin_only() {
        // Area C: setting verification green/red directly is the merge gate — a plain member (Write) is
        // refused; only an Owner/Admin may override. (CI reports via the secret-authed ci-result path.)
        let (app, tmp) = build_test_app("verify-gate");
        setup_org_repo(&app, "acme", "web", false, &[("boss", Role::Owner), ("dev", Role::Write)]).await;
        let change = app.repos.test_commit("acme", "web", "", None, &[("notes.txt", "hi\n")]);
        app.store.put_actor(actor("boss", ActorKind::Human)).await;
        app.store.put_actor(actor("dev", ActorKind::Human)).await;
        mint_token(&app, "tok-boss", "boss");
        mint_token(&app, "tok-dev", "dev");
        let t = ("acme".to_string(), "web".to_string(), change.clone());

        // A member who is not an admin cannot flip verification.
        let r = verify_change(State(app.clone()), Path(t.clone()), bearer("tok-dev"), Json(json!({ "green": true }))).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "a non-admin member cannot set verification");
        assert_ne!(app.repos.verification("acme", "web", &change).as_deref(), Some("green"), "the change stays un-green after a refused verify");
        // An owner/admin may.
        let r = verify_change(State(app.clone()), Path(t.clone()), bearer("tok-boss"), Json(json!({ "green": true }))).await;
        assert_eq!(r.status(), StatusCode::OK, "an owner/admin may set verification");
        assert_eq!(app.repos.verification("acme", "web", &change).as_deref(), Some("green"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn set_owners_is_repo_admin_only() {
        // Area C: code owners drive routing + the merge gate — admin-only.
        let (app, tmp) = build_test_app("owners-gate");
        setup_org_repo(&app, "acme", "web", false, &[("boss", Role::Admin), ("dev", Role::Write)]).await;
        app.store.put_actor(actor("boss", ActorKind::Human)).await;
        app.store.put_actor(actor("dev", ActorKind::Human)).await;
        mint_token(&app, "tok-boss", "boss");
        mint_token(&app, "tok-dev", "dev");
        let t = ("acme".to_string(), "web".to_string());
        let rules = json!({ "rules": [{ "glob": "*.rs", "owners": ["boss"] }] });

        let r = set_owners(State(app.clone()), Path(t.clone()), bearer("tok-dev"), Json(rules.clone())).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "a non-admin member cannot rewrite code owners");
        assert!(app.store.owners("acme/web").await.is_empty(), "no owners were written by a refused call");
        let r = set_owners(State(app.clone()), Path(t.clone()), bearer("tok-boss"), Json(rules)).await;
        assert_eq!(r.status(), StatusCode::CREATED, "an owner/admin may set code owners");
        assert_eq!(app.store.owners("acme/web").await.len(), 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn create_review_requires_repo_membership() {
        // Area C: a review is an accountable act on the repo — a non-member (even a valid, accountable
        // actor) is refused; a member may review.
        let (app, tmp) = build_test_app("review-gate");
        setup_org_repo(&app, "acme", "web", false, &[("member", Role::Write)]).await;
        app.store.put_actor(actor("member", ActorKind::Human)).await;
        app.store.put_actor(actor("outsider", ActorKind::Human)).await;
        mint_token(&app, "tok-member", "member");
        mint_token(&app, "tok-outsider", "outsider");
        put_pr(&app, "acme/web", 1, "author", "chg").await;
        let t = ("acme".to_string(), "web".to_string());
        let body = json!({ "target": "pr:1", "verdict": "comment", "summary": "looks fine" });

        let r = create_review(State(app.clone()), Path(t.clone()), bearer("tok-outsider"), Json(body.clone())).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "a non-member cannot review");
        assert!(app.store.reviews("acme/web").await.is_empty(), "no review persisted for a refused call");
        let r = create_review(State(app.clone()), Path(t.clone()), bearer("tok-member"), Json(body)).await;
        assert_eq!(r.status(), StatusCode::CREATED, "a repo member may review");
        assert_eq!(app.store.reviews("acme/web").await.len(), 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn independent_agent_reviewer_is_scoped_to_repo_members() {
        // Area C: the auto-reviewer must be a MEMBER of the repo's owning account — never an agent from
        // another tenant (which, at T3, could otherwise auto-merge cross-tenant). Uses real accountable
        // agents (minted delegations), since an unaccountable agent is filtered out regardless.
        let (app, tmp) = build_test_app("reviewer-scope");
        let human = hull_core::identity::mint_human("boss");
        let in_agent = hull_core::identity::mint_agent("agent:in", &human.actor, &human.secret_key, "*", Lifetime::Static).expect("mint in-org agent");
        let out_agent = hull_core::identity::mint_agent("agent:out", &human.actor, &human.secret_key, "*", Lifetime::Static).expect("mint outsider agent");
        app.store.put_actor(human.actor.clone()).await;
        app.store.put_actor(in_agent.actor.clone()).await;
        app.store.put_actor(out_agent.actor.clone()).await;
        // acme owns web; the human author + the in-org agent are members. `out_agent` is accountable but
        // not a member of acme.
        setup_org_repo(&app, "acme", "web", false, &[(human.actor.id.as_str(), Role::Owner), (in_agent.actor.id.as_str(), Role::Write)]).await;

        let picked = independent_agent_reviewer(&app, "acme", "web", &human.actor.id).await;
        assert_eq!(picked.map(|a| a.id), Some(in_agent.actor.id.clone()), "only the in-org accountable agent is eligible");

        // Drop the in-org agent's membership ⇒ no eligible reviewer (the accountable outsider is never chosen).
        app.store.put_account(Account {
            id: "acct-acme".into(),
            kind: AccountKind::Organization,
            handle: "acme".into(),
            members: vec![Membership { actor: human.actor.id.clone(), role: Role::Owner }],
        }).await;
        assert!(independent_agent_reviewer(&app, "acme", "web", &human.actor.id).await.is_none(), "an out-of-org agent is never selected as reviewer");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn participation_is_gated_on_read_not_membership() {
        // Area C (participation): comment/issue open uses `can_read_repo`, NOT `is_repo_member`. On a
        // PRIVATE repo a non-reader (valid, accountable, but not a member) is refused; the SAME actor is
        // ALLOWED once the repo is public — a public repo stays open to any authed actor (no regression).
        let (app, tmp) = build_test_app("participation-gate");
        setup_org_repo(&app, "acme", "web", true, &[("member", Role::Write)]).await;
        app.store.put_actor(actor("outsider", ActorKind::Human)).await;
        mint_token(&app, "tok-outsider", "outsider");
        let t = ("acme".to_string(), "web".to_string());
        let comment = json!({ "target": "pr:1", "body": "hello" });
        let issue = json!({ "title": "a bug" });

        // PRIVATE: the non-reader is denied both.
        let r = create_comment(State(app.clone()), Path(t.clone()), bearer("tok-outsider"), Json(comment.clone())).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "a non-reader cannot comment on a PRIVATE repo");
        assert!(app.store.comments("acme/web").await.is_empty(), "no comment persisted for a refused call");
        let r = create_issue(State(app.clone()), Path(t.clone()), bearer("tok-outsider"), Json(issue.clone())).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "a non-reader cannot open an issue on a PRIVATE repo");
        assert!(app.store.issues("acme/web").await.is_empty(), "no issue persisted for a refused call");

        // PUBLIC: the very same non-member actor may now participate freely.
        set_private(&app, "acme/web", false);
        let r = create_comment(State(app.clone()), Path(t.clone()), bearer("tok-outsider"), Json(comment)).await;
        assert_eq!(r.status(), StatusCode::CREATED, "any authed actor may comment on a PUBLIC repo");
        assert_eq!(app.store.comments("acme/web").await.len(), 1, "the public comment persisted");
        let r = create_issue(State(app.clone()), Path(t.clone()), bearer("tok-outsider"), Json(issue)).await;
        assert_eq!(r.status(), StatusCode::CREATED, "any authed actor may open an issue on a PUBLIC repo");
        assert_eq!(app.store.issues("acme/web").await.len(), 1, "the public issue persisted");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn repo_ops_require_membership_even_on_public() {
        // Area C (repo-ops): auto-review + request-reviewer are repo operations gated on `is_repo_member`
        // — a non-member is refused even on a PUBLIC repo (unlike participation). A member is allowed
        // past the gate (request_reviewer succeeds; auto_review reaches the "no agent" stage, i.e. it is
        // NOT refused for membership).
        let (app, tmp) = build_test_app("repo-op-gate");
        setup_org_repo(&app, "acme", "web", false, &[("member", Role::Write)]).await;
        app.store.put_actor(actor("member", ActorKind::Human)).await;
        app.store.put_actor(actor("outsider", ActorKind::Human)).await;
        app.store.put_actor(actor("rev", ActorKind::Human)).await;
        mint_token(&app, "tok-member", "member");
        mint_token(&app, "tok-outsider", "outsider");
        put_pr(&app, "acme/web", 1, "author", "chg").await;
        let t = ("acme".to_string(), "web".to_string(), 1u64);

        // request_reviewer: non-member refused, member allowed.
        let rr = json!({ "reviewer": "rev" });
        let r = request_reviewer(State(app.clone()), Path(t.clone()), bearer("tok-outsider"), Json(rr.clone())).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "a non-member cannot request a reviewer, even on a public repo");
        let r = request_reviewer(State(app.clone()), Path(t.clone()), bearer("tok-member"), Json(rr)).await;
        assert_eq!(r.status(), StatusCode::OK, "a repo member may request a reviewer");

        // auto_review: non-member refused with FORBIDDEN (membership), member passes the membership gate
        // (then stops at UNPROCESSABLE_ENTITY because no independent agent is registered — not a 403).
        let r = auto_review(State(app.clone()), Path(t.clone()), bearer("tok-outsider"), Json(json!({}))).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "a non-member cannot request an auto-review");
        let r = auto_review(State(app.clone()), Path(t.clone()), bearer("tok-member"), Json(json!({}))).await;
        assert_ne!(r.status(), StatusCode::FORBIDDEN, "a member passes the auto-review membership gate");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn missed_reads_are_gated_on_private_but_open_on_public() {
        // Area B (missed reads): `mirror_status` + `get_repo_autonomy` join the read sweep — a private
        // repo is hidden (404) from anonymous/non-members and visible to a member; a public repo is open.
        let (app, tmp) = build_test_app("missed-reads");
        setup_org_repo(&app, "acme", "web", true, &[("member", Role::Write)]).await;
        app.store.put_actor(actor("member", ActorKind::Human)).await;
        app.store.put_actor(actor("outsider", ActorKind::Human)).await;
        mint_token(&app, "tok-member", "member");
        mint_token(&app, "tok-outsider", "outsider");
        let t = ("acme".to_string(), "web".to_string());

        for (label, run) in [
            ("mirror_status", 0u8),
            ("get_repo_autonomy", 1u8),
        ] {
            let call = |h: axum::http::HeaderMap| {
                let (app, t) = (app.clone(), t.clone());
                async move {
                    if run == 0 { mirror_status(State(app), Path(t), h).await }
                    else { get_repo_autonomy(State(app), Path(t), h).await }
                }
            };
            assert_eq!(call(axum::http::HeaderMap::new()).await.status(), StatusCode::NOT_FOUND, "{label}: anonymous read of a private repo is 404");
            assert_eq!(call(bearer("tok-outsider")).await.status(), StatusCode::NOT_FOUND, "{label}: a non-member still gets 404 on a private repo");
            assert_eq!(call(bearer("tok-member")).await.status(), StatusCode::OK, "{label}: a member reads a private repo");
        }

        set_private(&app, "acme/web", false);
        assert_eq!(mirror_status(State(app.clone()), Path(t.clone()), axum::http::HeaderMap::new()).await.status(), StatusCode::OK, "mirror_status: public repo is open to anonymous");
        assert_eq!(get_repo_autonomy(State(app.clone()), Path(t.clone()), axum::http::HeaderMap::new()).await.status(), StatusCode::OK, "get_repo_autonomy: public repo is open to anonymous");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── observability: probes reachable without auth; metrics counter increments ────────────────

    /// Extract the `hull_http_requests_total{class="2xx"}` value from a Prometheus scrape body.
    fn scrape_2xx(body: &str) -> u64 {
        body.lines()
            .find_map(|l| l.strip_prefix("hull_http_requests_total{class=\"2xx\"} "))
            .and_then(|n| n.trim().parse::<u64>().ok())
            .expect("2xx counter line present in /metrics")
    }

    #[tokio::test]
    async fn probes_reachable_without_auth_and_metrics_increment() {
        let (app, tmp) = build_test_app("obs");
        let router = make_router(app);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let base = format!("http://{addr}");
        let client = reqwest::Client::new();

        // /health (liveness), /ready (readiness), /metrics — all reachable with NO auth header.
        let health = client.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(health.status(), 200, "/health is unauthenticated 200");

        let ready = client.get(format!("{base}/ready")).send().await.unwrap();
        assert_eq!(ready.status(), 200, "/ready is unauthenticated 200 (InMemory backend is always ready)");
        let ready_body: serde_json::Value = ready.json().await.unwrap();
        assert_eq!(ready_body["ready"], serde_json::Value::Bool(true));

        let m1 = client.get(format!("{base}/metrics")).send().await.unwrap();
        assert_eq!(m1.status(), 200, "/metrics is unauthenticated 200");
        let m1txt = m1.text().await.unwrap();
        // Valid exposition: HELP/TYPE lines + the three required metric families.
        assert!(m1txt.contains("# TYPE hull_http_requests_total counter"), "counter TYPE line");
        assert!(m1txt.contains("hull_http_requests_total{class=\"2xx\"}"), "2xx series");
        assert!(m1txt.contains("# TYPE hull_http_requests_in_flight gauge"), "in-flight gauge");
        assert!(m1txt.contains("# TYPE hull_process_uptime_seconds gauge"), "uptime gauge");

        // Metrics itself is NOT observed, so scraping doesn't inflate the counter. Hitting an OBSERVED
        // route (/health) must bump the 2xx total.
        let before = scrape_2xx(&m1txt);
        let _ = client.get(format!("{base}/health")).send().await.unwrap();
        let m2txt = client.get(format!("{base}/metrics")).send().await.unwrap().text().await.unwrap();
        let after = scrape_2xx(&m2txt);
        assert!(after > before, "an observed 2xx request must increment the counter: {before} -> {after}");

        handle.abort();
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
