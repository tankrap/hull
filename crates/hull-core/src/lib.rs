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
pub mod store;

use serde::{Deserialize, Serialize};

/// An Ed25519 public key, hex-encoded — the stable id of an [`Actor`].
pub type ActorId = String;
/// A keel object address (BLAKE3, hex) — a change, tree, or blob.
pub type KeelId = String;

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
    /// Ed25519 signature by the PARENT principal over this hop (empty until the crypto layer, M1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signature: Vec<u8>,
}

impl Delegation {
    /// The human principal at the root, or `None` if the chain doesn't start at a human.
    pub fn human_root(&self) -> Option<&ActorId> {
        match self.chain.first() {
            Some(h) if h.kind == ActorKind::Human => Some(&h.principal),
            _ => None,
        }
    }
}

/// Static (registered, long-lived) or ephemeral (session-scoped, auto-expiring). The delegation
/// chain on [`Actor`] carries who delegated it; lifetime carries only how long it lives.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
        Actor { id: id.into(), kind: ActorKind::Human, lifetime: Lifetime::Static, handle: id.into(), delegation: None, nostr_pubkey: None }
    }
    fn agent(id: &str, delegation: Option<Delegation>) -> Actor {
        Actor { id: id.into(), kind: ActorKind::Agent, lifetime: Lifetime::Ephemeral { expires_unix: 0 }, handle: id.into(), delegation, nostr_pubkey: None }
    }
    fn hop(id: &str, kind: ActorKind) -> DelegationHop {
        DelegationHop { principal: id.into(), kind, scope: "*".into(), signature: vec![] }
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
