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

pub mod store;

use serde::{Deserialize, Serialize};

/// An Ed25519 public key, hex-encoded — the stable id of an [`Actor`].
pub type ActorId = String;
/// A keel object address (BLAKE3, hex) — a change, tree, or blob.
pub type KeelId = String;

/// A human or an agent. Identity is a keypair, so authorship is signed, not asserted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Actor {
    pub id: ActorId,
    pub kind: ActorKind,
    pub lifetime: Lifetime,
    /// Display handle, e.g. `@justin` or `agent:reviewer-3`.
    pub handle: String,
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

/// Static (registered, long-lived) or ephemeral (session-scoped, attenuated, auto-expiring) — the
/// froots/buzz distinction, expressed with keel's delegation semantics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Lifetime {
    Static,
    /// Minted for one session: expires at `expires_unix`, with an optional parent that delegated it.
    Ephemeral {
        expires_unix: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<ActorId>,
    },
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
