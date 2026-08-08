//! Hull's domain model.
//!
//! Two ideas run through every type:
//! 1. **Actors are peers.** A human and an agent are the same primitive — an [`Actor`] with an
//!    Ed25519 identity. Anywhere GitHub would say "user" (author, assignee, reviewer, code owner),
//!    Hull says "actor", so an agent can do all of it, cryptographically.
//! 2. **References are content-addressed.** A [`CodeRef`] anchors to a keel **blob id + line**, not
//!    a mutable `file#L42`, so it survives edits and resolves through `keel why` to the change and
//!    session that produced it.
//!
//! This module is storage-agnostic: types + a [`Store`] trait, with an in-memory implementation for
//! the scaffold. M1+ swaps in a SQL-backed store (accounts/issues) that references keel by id.

pub mod identity;
pub mod reconcile;
pub mod store;

use serde::{Deserialize, Serialize};

/// An Ed25519 public key, hex-encoded — the stable id of an [`Actor`].
pub type ActorId = String;
/// A keel object address (BLAKE3, hex) — a change, tree, or blob.
pub type KeelId = String;

/// Process-global "persistence degraded" flag. Set when a durable write (the [`store::FileStore`]
/// snapshot, or a JSON side-store persist) fails — at which point the in-memory state has silently
/// **diverged from disk** and a restart would lose the un-persisted writes. The server's `/health`
/// handler reads this and returns 503 while it is set, so an orchestrator can pull the instance.
///
/// This is a stopgap: the [`store::Store`] trait methods are infallible (return `()`), so a persist
/// failure cannot be propagated to the caller. Fully surfacing the error needs a fallible `Store`
/// trait — tracked as a follow-up. Until then, "log loudly + flip this flag" is the safety net.
pub static PERSISTENCE_DEGRADED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Flip the process into the "persistence degraded" state (see [`PERSISTENCE_DEGRADED`]). Set on any
/// failed durable write; cleared by [`clear_persistence_degraded`] on the next fully-successful one.
pub fn mark_persistence_degraded() {
    PERSISTENCE_DEGRADED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Clear the "persistence degraded" state after a SUCCESSFUL durable write. Because every persist
/// rewrites the entire snapshot, one clean write fully re-syncs disk to memory — so a transient blip
/// (a full disk that's since freed, a brief I/O error) recovers and `/health` returns to 200 rather
/// than staying 503 for the process lifetime. Called only on the success path of a persist.
pub fn clear_persistence_degraded() {
    PERSISTENCE_DEGRADED.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Whether persistence has degraded this process (see [`PERSISTENCE_DEGRADED`]).
pub fn persistence_degraded() -> bool {
    PERSISTENCE_DEGRADED.load(std::sync::atomic::Ordering::Relaxed)
}

/// A human or an agent. Identity is a keypair, so authorship is signed, not asserted.
///
/// **Hard invariant — no unaccountable agents.** Every agent's authority is a cryptographically
/// verifiable **delegation chain that roots at a human**. A human *is* its own root; an agent MUST
/// carry a [`Delegation`] whose first hop is a human principal. An agent with no human root cannot
/// be minted and cannot author anything — "nothing is authored anonymously".
///
/// This mirrors the model forge already ships: **attenuation-only, Ed25519/biscuit** delegation.
/// The accountability chain roots at a **natural person** — `human → machine → session/run` — each
/// hop only narrowing scope/TTL/ref-glob and depth-capped. Tenancy (org / account) is orthogonal
/// *scope*, NOT an ancestor above the human. Hull reuses forge's scheme rather than inventing one.
/// The chain is carried on every authored artifact; agent work always enters the human review gate
/// and never self-merges; standing agents use short-TTL auto-renewed delegations with revocation
/// that propagates to descendants. See `ARCHITECTURE.md` §"Accountability".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Actor {
    pub id: ActorId,
    pub kind: ActorKind,
    pub lifetime: Lifetime,
    /// Display handle, e.g. `@justin` or `agent:reviewer-3`.
    pub handle: String,
    /// The attenuation chain rooting this actor's authority at a human. REQUIRED for agents; `None`
    /// only for a human (a human is the root). Enforced structurally by [`Actor::human_principal`]
    /// and cryptographically by the delegation verifier (M1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<Delegation>,
    /// secp256k1 nostr pubkey for notification fan-out (code-owner pings), if the actor opted in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nostr_pubkey: Option<String>,
    /// Revoked actors cannot authenticate or author, and — because their id sits in every descendant's
    /// delegation chain — revoking one **propagates** to its whole subtree via [`Delegation::verify`].
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Human,
    Agent,
}

impl Actor {
    /// The human this actor ultimately acts for: itself if human, else the root of its delegation
    /// chain. `None` means **unaccountable** — such an actor must be rejected at mint and at every
    /// authoring boundary. (Signature/attenuation verification is the crypto layer, M1; this is the
    /// structural gate that makes the invariant representable and testable.)
    pub fn human_principal(&self) -> Option<&ActorId> {
        match self.kind {
            ActorKind::Human => Some(&self.id),
            ActorKind::Agent => self.delegation.as_ref().and_then(Delegation::human_root),
        }
    }

    /// True iff this actor chains to a human — the gate every mint / write / review must pass.
    pub fn is_accountable(&self) -> bool {
        self.human_principal().is_some()
    }
}

/// A cryptographically verifiable delegation chain (attenuation-only, Ed25519 — biscuit-style, as
/// forge implements). Ordered hops from the human root (index 0) down to the actor; each hop only
/// narrows the parent's scope/TTL/ref-glob and is signed by the parent. Depth-capped. The chain
/// MUST terminate at a human, or the actor is unaccountable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Delegation {
    pub chain: Vec<DelegationHop>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DelegationHop {
    pub principal: ActorId,
    /// The root hop MUST be `Human`.
    pub kind: ActorKind,
    /// Attenuated scope for this hop — a subset of the parent's (scope + TTL + ref-glob).
    pub scope: String,
    /// Unix expiry for this hop's grant; `0` = no expiry (a standing delegation). A hop may not
    /// outlive its parent — the verifier enforces monotonic (non-increasing) TTL down the chain.
    #[serde(default)]
    pub expires_unix: u64,
    /// Ed25519 signature by the PARENT principal over this hop's canonical bytes
    /// ([`identity::hop_message`]). Empty on the root hop (a human is self-rooted) and on any
    /// structurally-minted (pre-crypto) hop; the [`Delegation::verify`] crypto gate rejects the
    /// latter at every authoring boundary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signature: Vec<u8>,
}

impl Delegation {
    /// The human principal at the root, or `None` if the chain doesn't start at a human. This is the
    /// **structural** check (kind only); [`Delegation::verify`] is the cryptographic gate.
    pub fn human_root(&self) -> Option<&ActorId> {
        match self.chain.first() {
            Some(h) if h.kind == ActorKind::Human => Some(&h.principal),
            _ => None,
        }
    }
}

/// Static (registered, long-lived) or ephemeral (session-scoped, auto-expiring). The delegation
/// chain on [`Actor`] carries who delegated it; lifetime carries only how long it lives.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Lifetime {
    Static,
    /// Minted for one session: expires at `expires_unix` (an ephemeral agent still chains to a
    /// human via [`Actor::delegation`]).
    Ephemeral { expires_unix: u64 },
}

/// A personal or organization account. Repos, issues, and projects belong to an account.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub kind: AccountKind,
    pub handle: String,
    /// Members and their role (orgs have many; a personal account has its owner). Agents can be
    /// members with scoped grants.
    #[serde(default)]
    pub members: Vec<Membership>,
}

/// A configured AI backend an account (personal or org) can lend to Hull's AI functions — reviews,
/// fixes, "ask agent". Several can be connected and rotated across; a repo's reviews use its owning
/// org's connections, falling back to the triggering user's own. The credential is an API key OR an
/// OAuth token from a `keel ai login` (a subscription sign-in).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiConnection {
    pub id: String,
    /// Owning account id (a personal account or an org).
    pub owner: String,
    /// `openai` | `anthropic` | `openrouter`.
    pub provider: String,
    /// Human label shown in settings (e.g. "Claude Pro (justin)").
    pub label: String,
    /// API root, e.g. `https://api.anthropic.com/v1`.
    pub base_url: String,
    pub auth: AiAuth,
    pub created_unix: u64,
    /// When a captured agent token expires (e.g. Claude `setup-token` is valid ~1 year). `None` for
    /// key connections / device sessions that self-refresh. Used to warn before reviews start failing.
    #[serde(default)]
    pub token_expires_unix: Option<u64>,
}

/// Rolling token-usage tally for one [`AiConnection`] — what the account's subscription has spent on
/// Hull's AI work (reviews/fixes/answers). `cost_micros` is USD × 1e6 (agents that report a cost).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AiUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cost_micros: u64,
    #[serde(default)]
    pub runs: u64,
    #[serde(default)]
    pub updated_unix: u64,
}

/// How an [`AiConnection`] authenticates. `Key` = a static API key. `AgentCli` = a locally-installed
/// agent (Claude Code / Codex) that Hull runs with the user's OWN subscription login — no token is
/// stored or reused; the CLI authenticates itself. (This is the ToS-compliant way to use a Pro/Max or
/// ChatGPT subscription: the actual agent uses it, not a third-party app reusing an OAuth token.)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AiAuth {
    Key { api_key: String },
    AgentCli {
        command: String,
        /// Storage key for this user's own credential bundle (the CLI's `CLAUDE_CONFIG_DIR` /
        /// `CODEX_HOME` contents, populated by a web-relayed subscription login). Empty ⇒ fall back to
        /// the Hull host's own agent login (single-tenant / self-hosted).
        #[serde(default)]
        session: String,
        /// Introspected identity for the settings UI (from `<cli> auth status`). Non-secret.
        #[serde(default)]
        account_email: String,
        /// e.g. `max`, `pro`, `plus`. Non-secret.
        #[serde(default)]
        plan: String,
    },
}

impl AiAuth {
    /// The bearer token to send now (an API key), or empty for an agent CLI (which self-authenticates).
    pub fn bearer(&self) -> &str {
        match self {
            AiAuth::Key { api_key } => api_key,
            AiAuth::AgentCli { .. } => "",
        }
    }
    /// A short, non-secret hint for the settings UI: the connected account/plan for a per-user
    /// session, else the bare command (host login), else the key's last chars.
    pub fn hint(&self) -> String {
        match self {
            AiAuth::AgentCli { command, account_email, plan, .. } => {
                if !account_email.is_empty() {
                    if plan.is_empty() { account_email.clone() } else { format!("{account_email} · {plan}") }
                } else {
                    command.clone()
                }
            }
            AiAuth::Key { api_key } => {
                let tail: String = api_key.chars().rev().take(3).collect::<String>().chars().rev().collect();
                format!("…{tail}")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    Personal,
    Organization,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Membership {
    pub actor: ActorId,
    pub role: Role,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Admin,
    Write,
    Read,
}

/// A login identity (a hosted account). Distinct from [`Actor`]: a `User` authenticates with
/// **passkeys** (WebAuthn) + email/username — no passwords — and *owns* a human [`Actor`] whose
/// Ed25519 signing key Hull holds server-side, so Hull can sign agent delegations on the user's
/// behalf. Passkeys can only sign server challenges (not arbitrary delegation messages), so login
/// and signing are deliberately separated here. Legacy key-import login (proving possession of an
/// actor keypair) still works and needs no `User`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    /// Stable id = the WebAuthn user handle (a uuid), independent of `username` (which may change).
    pub id: String,
    pub username: String,
    pub email: String,
    /// The human actor this user drives (its id is that actor's public key).
    pub actor: ActorId,
    /// Hex Ed25519 secret Hull holds to sign delegations for a CUSTODIAL (passkey) account. Plaintext
    /// in the dev store; a real deployment encrypts this at rest (KMS/enclave). This is the tradeoff
    /// the hosted-account model accepts in exchange for passkey login. **Empty for a SOVEREIGN
    /// (non-custodial) account** — Hull then holds no signing key and refuses to sign for it.
    pub secret_key: String,
    /// For a SOVEREIGN account: the user's Ed25519 secret, encrypted in-browser under their passphrase
    /// (opaque bundle — KDF params + salt + nonce + ciphertext). Hull stores only this ciphertext so the
    /// account is portable across devices; it can never decrypt it (no passphrase). `None` for custodial.
    #[serde(default)]
    pub wrapped_key: Option<String>,
    #[serde(default)]
    pub passkeys: Vec<PasskeyCred>,
    #[serde(default)]
    pub created_unix: u64,
    /// A short self-description shown on the profile page. Free text, user-editable.
    #[serde(default)]
    pub bio: String,
}

/// A registered WebAuthn passkey. `data` is the opaque `webauthn_rs::prelude::Passkey`, kept as JSON
/// so hull-core never has to depend on the WebAuthn crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PasskeyCred {
    /// Base64url credential id — the stable handle for display / removal.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub created_unix: u64,
    pub data: serde_json::Value,
}

/// A team within an organization account: a named group of members, used to grant repo access
/// collectively (see repo settings). Personal accounts have no teams.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Team {
    pub id: String,
    /// Owning account id.
    pub account: String,
    pub name: String,
    #[serde(default)]
    pub members: Vec<Membership>,
}

/// A hosted keel repository.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Repo {
    pub id: String,
    pub owner: String, // Account id
    pub name: String,
    #[serde(default)]
    pub default_branch: String,
}

/// A **content-addressed** reference to a span of code: a keel blob + line range, plus the path it
/// lived at (for display). Unlike `file#L42`, this stays correct across edits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeRef {
    pub repo: String,
    pub blob: KeelId,
    pub path: String,
    pub line_start: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_end: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    /// Hex color, e.g. `#2ec9bd`.
    pub color: String,
}

/// Issue lifecycle with typed close-reasons (the requested open · closed/not-planned ·
/// closed/cancelled · …).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum IssueStatus {
    Open,
    Closed { reason: CloseReason },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    Completed,
    NotPlanned,
    Cancelled,
    Duplicate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Issue {
    pub id: String,
    pub repo: String,
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub author: ActorId,
    #[serde(default)]
    pub assignees: Vec<ActorId>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub projects: Vec<String>,
    pub status: IssueStatus,
    /// Content-addressed code the issue points at.
    #[serde(default)]
    pub code_refs: Vec<CodeRef>,
    /// Actors (human or agent) explicitly referenced/mentioned.
    #[serde(default)]
    pub referenced_actors: Vec<ActorId>,
    /// Linked pull request ids.
    #[serde(default)]
    pub linked_prs: Vec<String>,
    /// If the resolving keel change is verify-green, surface it (provenance badge).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<KeelId>,
    pub created_unix: u64,
    /// When the author last edited this issue's title/body (unix seconds), if ever. Lets the UI show
    /// an "edited" marker. Absent on issues that have never been edited (and on pre-edit data).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_unix: Option<u64>,
}

/// Where a PR is in its lifecycle.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    #[default]
    Open,
    Merged,
    Closed,
}

/// A pull request — a keel change (or range) proposed for merge, carrying its verification status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PullRequest {
    pub id: String,
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub author: ActorId,
    /// The keel change(s) this PR proposes.
    #[serde(default)]
    pub changes: Vec<KeelId>,
    pub verification: Verification,
    #[serde(default)]
    pub reviewers: Vec<ActorId>,
    #[serde(default)]
    pub state: PrState,
    /// The accountable actor who merged it, once merged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_by: Option<ActorId>,
    /// Client-signed provenance for this PR's change(s): change id → serialized `SignedProvenance` JSON.
    /// A sovereign author's Ed25519 key is client-held, so the instance can't sign provenance for them;
    /// they submit it here (signed in the browser/CLI) and the server embeds it in the substrate at land.
    /// The value is opaque to hull-core (the crypto lives in hull-server::nostr); it is transport JSON.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub sovereign_provenance: std::collections::HashMap<KeelId, String>,
    pub created_unix: u64,
}

/// A keel session ingested for a hosted change (via `keel capture`) and associated by change id —
/// the session-carrying bridge across the git boundary, where the native `Change.session` is lost.
/// This is what fills a review's package with the task/reasoning/operations that produced the code.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRecord {
    pub repo: String,
    pub change: String,
    pub task: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub lesson: String,
    #[serde(default)]
    pub tool_calls: usize,
    #[serde(default)]
    pub tokens_in: u64,
    #[serde(default)]
    pub tokens_out: u64,
}

/// A review of a target (a PR, or a keel change/session) — first-class, like keel's own reviews.
/// The reviewer is an [`Actor`], so an **agent** can review (independent-by-construction: a model
/// from a different family than the author), and every verdict is accountable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Review {
    pub id: String,
    pub repo: String,
    /// What is under review, e.g. `pr:1` or a keel change id.
    pub target: String,
    pub reviewer: ActorId,
    pub verdict: Verdict,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<ReviewFinding>,
    /// For an agent reconciliation review: the claim ledger **as evaluated when the verdict was
    /// posted**. Immutable point-in-time evidence — verification can change later, but this records
    /// what the review actually decided on. `None` for a manual human review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger: Option<reconcile::ClaimLedger>,
    /// Content address (BLAKE3) of the full audit artifact for this review (inputs, models, ledger,
    /// findings) — "why did the reviewer pass this?" Fetchable at `…/artifacts/:id`. (D8)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    pub created_unix: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    RequestChanges,
    Reject,
    Comment,
}

/// A discussion comment on a target (a PR `pr:1`, or a review id) — the conversation layer over the
/// structured verdict/findings. Author is an [`Actor`], so a **human and an agent hold the same
/// thread**: an agent can post its reasoning and a human can reply, accountably, in one place.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Comment {
    pub id: String,
    pub repo: String,
    /// What this comments on, e.g. `pr:1`.
    pub target: String,
    pub author: ActorId,
    pub body: String,
    pub created_unix: u64,
    /// Optional file + line this comment is anchored to (a line-level review comment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// When the author last edited this comment's body (unix seconds), if ever. Lets the UI show an
    /// "edited" marker. Absent on comments that have never been edited (and on pre-edit data).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_unix: Option<u64>,
}

/// A specific issue a review raises, anchored to a file (and optionally a line). What turns a
/// review from a verdict into an actual review.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewFinding {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// `info` | `warn` | `blocker`.
    #[serde(default)]
    pub severity: String,
    pub note: String,
}

/// How much an agent may do **autonomously** in a scope (org account or repo). Tiers T0–T3 (§8.6):
/// the default cap is **T1** until a human raises it — "the token says *may*, the policy decides
/// *do*." Ordered so `tier >= AutonomyTier::T2` is a meaningful gate.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyTier {
    /// Observe-only. No autonomous action — no auto-review, no agent approvals.
    T0,
    /// Review-required (default). Agents auto-review PRs, but the verdict is **advisory**: a merge
    /// needs a human approval.
    #[default]
    T1,
    /// Auto-approve **low-risk** changes: an agent's approve satisfies the merge gate when the change
    /// is green, uncontradicted, and touches no protected path.
    T2,
    /// Autonomous: an agent's approve counts broadly. Still never auto-approves a **protected path**
    /// and stays within grant scope (D11 — a verdict never merges a protected path alone).
    T3,
}

/// The autonomy policy for a scope: the tier plus the protected-path globs that always require a
/// human, regardless of tier. Resolved repo → account → instance default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomyPolicy {
    pub tier: AutonomyTier,
    /// Path globs that always require human review (auth, migrations, the .hull policy dir …).
    #[serde(default)]
    pub protected_paths: Vec<String>,
}

/// Mirror of keel's verification (from `keel verify`) — first-class in Hull.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    Unverified,
    Green,
    Red,
}

/// A project is a saved set of **views** over a filtered issue set — the views are projections of
/// the same issues, not separate data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    pub id: String,
    pub owner: String,
    pub name: String,
    #[serde(default)]
    pub views: Vec<ProjectView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectView {
    pub name: String,
    pub kind: ViewKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ViewKind {
    Kanban,
    List,
    Roadmap,
    /// Grouped by what agents are touching right now (fed by the coordination stream).
    Live,
}

/// Path-glob → owners (human or agent). Owners are auto-pulled into PRs/issues that reference their
/// code; agent owners get a nostr ping carrying the keel change id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OwnerRule {
    pub glob: String,
    pub owners: Vec<ActorId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn human(id: &str) -> Actor {
        Actor { id: id.into(), kind: ActorKind::Human, lifetime: Lifetime::Static, handle: id.into(), delegation: None, nostr_pubkey: None , revoked: false }
    }
    fn agent(id: &str, delegation: Option<Delegation>) -> Actor {
        Actor { id: id.into(), kind: ActorKind::Agent, lifetime: Lifetime::Ephemeral { expires_unix: 0 }, handle: id.into(), delegation, nostr_pubkey: None , revoked: false }
    }
    fn hop(id: &str, kind: ActorKind) -> DelegationHop {
        DelegationHop { principal: id.into(), kind, scope: "*".into(), expires_unix: 0, signature: vec![] }
    }

    #[test]
    fn no_unaccountable_agents() {
        // a human is its own root
        assert_eq!(human("h").human_principal(), Some(&"h".to_string()));

        // an agent with NO delegation is unaccountable — rejected
        assert!(!agent("a", None).is_accountable(), "agent without a human root must be rejected");

        // an agent whose chain roots at a human is accountable, and resolves to that human
        let d = Delegation { chain: vec![hop("h", ActorKind::Human), hop("machine", ActorKind::Agent), hop("a", ActorKind::Agent)] };
        let acc = agent("a", Some(d));
        assert!(acc.is_accountable());
        assert_eq!(acc.human_principal(), Some(&"h".to_string()));

        // an agent whose chain does NOT start at a human is still unaccountable
        let bad = Delegation { chain: vec![hop("machine", ActorKind::Agent), hop("a", ActorKind::Agent)] };
        assert!(!agent("a", Some(bad)).is_accountable(), "a chain with no human root is unaccountable");
    }
}
