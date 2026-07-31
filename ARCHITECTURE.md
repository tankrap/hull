# Hull — a hosted keel

Hull is the hosted layer on top of [keel](https://github.com/tankrap/keel), the agent-native VCS.
keel is the substrate — content-addressed object store, git-compatible wire, the fused code+session
graph, the flywheel, and coordination over QUIC. Hull adds what a hosted product needs: accounts and
tenancy, issues and projects, CI/CD, agent identity, security scanning, and a UI that reacts to the
work actually happening.

The guiding principle: **Hull is agent-native first.** GitHub was built for humans and bolted agents
on. Hull assumes agents and humans are peers — both have cryptographic identity, both can own code,
both show up in the same coordination stream — and the whole product is shaped by keel's provenance
(every issue, PR, and review links to real keel changes with real verification status).

---

## Stack (decided)

| Layer | Choice | Why |
|---|---|---|
| Backend | **Rust + axum** | Hull embeds keel's crates directly (`keel-store`, `keel-git`, `keel-net`, `keel-brief`) — no serialization boundary between the host and the substrate. Issues can reference keel blobs/changes by content address; the reactive feed subscribes to keeld's QUIC event channel natively. |
| Domain storage | **SQLite (dev) → Postgres (prod)** via `sqlx` | Accounts / issues / projects / labels are mutable relational data — a SQL store fits. **Content and provenance live in keel**, referenced by id. (A later dogfood option: version Hull's own domain objects *in* keel.) |
| Realtime | **keeld QUIC → server bridge → SSE/WebSocket** | The daemon already broadcasts "agent working on file X" + lessons. Hull aggregates that fleet stream and pushes it to the reactive home page. |
| Frontend | **React + TypeScript + Vite** | Rich, real-time, generative UI. |
| Agent identity | **Ed25519** (matches keel provenance) + **nostr (secp256k1) bridge** for notifications | Static or ephemeral agent keypairs; nostr for code-owner notification fan-out. |

Crate layout (this **public** repo):
- `crates/hull-core` — domain model + storage + keel integration (the seam to `keel-*`).
- `crates/hull-plugin` — the **plugin SDK**: extension-point traits + registry (the open-core seam).
- `crates/hull-server` — axum HTTP/JSON API as a **library** (`run` with a plugin hook) + OSS binary.
- `crates/hull-scan` — secret scanning, **shared with the keel CLI** so a scan can run client-side.
- `web/` — the Vite/React frontend.

Closed hosted plugins live in the **separate private repo `tankrap/hull-hosted`**.

## Open core (Apache-2.0 core + closed hosted plugins)

**The entire server is open source and fully functional on its own.** The hosted product's extra
value ships as **closed plugins** in a separate private repo (`tankrap/hull-hosted`) that extend the
core through the `hull-plugin` SDK — and the core never depends on them, so it can be given away while
the hosted plugins stay private. Capabilities (`SecretRuleset`, `Notifier`, `AuthProvider`, and a
roadmap of `StorageBackend` / `CiRunner` / `AgentFlow` / `Metering` …) are trait objects registered
into a `Registry`; the server always falls back to a built-in default, so 0, 1, or N plugins run the
same code. `hull-server` is a library whose `run(opts, register_plugins)` hook is the seam: the OSS
binary passes a no-op; the hosted binary (private repo) passes a closure that registers its plugins.
See **[PLUGINS.md](./PLUGINS.md)**.

---

## Feature map (requested + my input)

### Issues
Requested: assignees, labels, projects, **reference/attach specific lines of code**, reference agents,
link PRs, rich statuses (open · closed · closed/not-planned · closed/cancelled …).

- **Line references are content-addressed, not fragile.** A GitHub line link (`file#L42`) rots the
  moment the file changes. Hull anchors a line reference to a keel **blob id + line**, so it stays
  correct across edits, and `keel why` can resolve *which change and which agent/session* last
  touched that line. **This is a keel-native advantage GitHub structurally can't match** — lean into
  it.
- Assignees and "referenced agents" are the same primitive: an **actor** (human or agent identity).
- Status is a small state machine with typed close-reasons; a status change is an event on the
  reactive stream.
- *My addition:* issues carry an optional **`verified` provenance badge** — if the change that
  closed an issue is `keel verify`-green, show it. Verification is first-class in keel; surface it.

### Projects
Requested: multiple views (kanban, list).
- A project is a saved view over a filtered issue set; **views are projections**, not separate data
  (kanban / list / roadmap all read the same issues). *My addition:* a **"live" view** grouped by
  what agents are touching right now (fed by the coordination stream).

### Agent identity
Requested: cryptographic identities, **ephemeral or static** (froots/buzz-style), linked to issues /
PRs / objects.
- Every actor has an **Ed25519 keypair**. **Static** = registered, long-lived (a named agent or a
  human). **Ephemeral** = minted for one session, attenuated scope + TTL, auto-expiring — exactly
  keel's delegation model. An action (comment, review, commit, close) is **signed**, so authorship is
  cryptographic, not a claim.
- **Hard invariant — every agent chains to a human (see Accountability below).**
- *My addition:* because identity is a keypair, an agent can be a **code owner** and be notified over
  **nostr** when its code is touched — the requested code-owner feature *requires* this identity
  layer, so build them together.

## Adoption: git-compatible, and mirror GitHub both ways

Two properties make adoption incremental instead of all-or-nothing (the biggest risk for anything
that competes with git/GitHub):

- **Never reject a git-native client.** A plain `git push` to a hosted keel repo is accepted and
  **bridges to native keel history** — the fused brief / provenance / status all work, and the user
  never has to know keel is underneath. (Verified end-to-end at the `keel serve` level; platform
  routing + auth = Hull M4.)
- **Two-way GitHub mirroring** (`push to Hull → GitHub`, `push to GitHub → Hull`). A repo lives on
  both at once, so teams keep GitHub's integrations/CI/network while adopting Hull incrementally. The
  hard part is already built in keel (byte-identical git codec + `mirror-in`/`mirror-out` both
  directions + receive-pack/bridge); the remaining work is the remote-sync + GitHub-App layer (loop
  prevention, conflict policy, webhooks, tokens, accountability mapping). High priority — Linear
  NEW-1170.

## Accountability — every agent cryptographically chains to a human

**Non-negotiable invariant: no agent is ever an unaccountable actor.** Every agent's authority is a
cryptographically verifiable **delegation chain that roots at a human** — an ephemeral reviewer, a
CI-triage bot, a fix agent, all of it. "Nothing is authored anonymously."

This is not new to build — **forge already implements it**, and Hull reuses that scheme rather than
inventing a parallel one:

- **Attenuation-only delegation** with **Ed25519 / biscuit** tokens. The **accountability chain
  roots at a natural person**: `human → machine → session / agent-run`. Each hop can only *narrow*
  scope, TTL, and ref-glob, and is **depth-capped** — a child never holds more authority than its
  parent.
- **Tenancy is orthogonal, not an ancestor.** Org / account membership is *scope* (where a principal
  may act), carried alongside the chain — **never above the human in it**. An org authors nothing on
  its own; only a human, or an agent that human delegated, authors within an org. (So "human at the
  root" and the org→account naming hierarchy are two different axes; don't conflate them.)
- **No service/system escape hatch for authors.** Automation may exist, but an *authoring* agent
  (one that produces a change, review verdict, comment, or issue transition) **must** root at a
  human. There is no "service agent" that authors code without a human behind it.
- The **delegation chain is carried on every authored artifact**, so any action resolves to the
  human it acts for.
- Agent work always enters the **human review gate** (`human_required`) — it **never self-merges**
  (exactly the review system's protected-path rule, §D11).
- **Standing/scheduled agents** (nightly triage, a cron reviewer) root at the human who *authorized*
  them, via a **short-TTL delegation auto-renewed** by a machine credential that itself chains to
  that human — never an eternal token. **Revocation propagates**: revoking a human or machine kills
  every descendant agent credential (blast-radius = the subtree). Reuse forge's revocation +
  provenance-bundle work.

In the domain model this is a hard type-level + runtime gate: an `Actor` is accountable iff
`human_principal()` resolves — a human is its own root; an agent MUST carry a `Delegation` whose root
hop is a human, or it is rejected at mint and at every authoring boundary (`hull-core`, tested). The
cryptographic verification (signatures + attenuation-subset + depth-cap + **TTL/revocation** checks)
is the M1 identity layer, wiring to forge's existing biscuit/Ed25519 delegation issuer.

### CI/CD
Requested: custom runners, speed.
- *My strong input:* **memoized, content-addressed CI.** keel addresses every tree/blob by hash, so
  Hull can **skip any job whose inputs are content-identical to a previous green run** — the biggest
  real speed win, and unique to a content-addressed substrate. Warm caches shared across the fleet.
- Runners are declarative (a `.hull/ci.yml`), sandboxed, never raw shell on the host.
- Agent CI (below) triages failures instead of just reporting them.

### Agent integration
Requested: CI/CD triage, security review of PRs/commits, code review.
- These already exist as model-backed flows in the forge lineage (ci-triage, ai-review, fix). Hull
  wires them to **keel's fused brief** so a reviewing agent gets task-relevant context + prior
  lessons automatically — better reviews at lower token cost (the flywheel, proven: real-corpus
  75→94). Every agent action is gated + provenance-signed + **never auto-merges**.

### Accounts
Requested: personal + organization.
- Standard: users, orgs, membership/roles. Repos belong to an owner (user or org). Agents are
  first-class members with scoped grants.

### Secret scanning (two layers, as requested)
- **In the keel CLI (client-side) — cut it off before it leaves the machine.** `hull-scan` is a
  shared crate; keel gains a `keel scan` + a **pre-push guard** that blocks a push containing a
  detected secret. This is the important one — a secret that never leaves the laptop can't leak.
- **In Hull (server-side) — backstop.** receive-pack runs the same scanner; a hit quarantines the
  push and alerts. Same engine both sides, so parity is guaranteed.

### Code owners
Requested: owners pulled into PRs/issues when their code is referenced by a human **or agent**;
agents can be owners, notified via nostr.
- `.hull/owners` maps path globs → actors (human or agent identity). When a human/agent references
  owned code (a PR touches it, an issue line-refs it), owners are auto-subscribed; **agent owners get
  a nostr notification** carrying the keel change id so they can act autonomously.

### The reactive home page
Requested: layout reacts to daemon/cli data — agents touching repo X pushes X to the front; a
dynamic/generative UI reflecting real work. This is the home page.
- The home page is a **live projection of the coordination stream.** keeld already emits
  brief-presence and lesson events over QUIC; `hull-server` bridges that to the client. Repos, PRs,
  and issues **rank by live activity** — an agent starting work on repo X floats X up, its active
  files surface, its in-flight reviews show. Quiet → recency; busy → activity-ranked.
- *My framing:* this is a **"situation room,"** not a dashboard — it answers "what is the fleet doing
  right now," which is the question that matters when agents outnumber humans.

---

## Provenance is the spine (my overarching input)

Every Hull object links back to keel: issues ↔ the changes that resolve them, PRs ↔ keel changes with
verification status, reviews ↔ the session + brief that produced them, line-refs ↔ blob ids. This
gives Hull three things GitHub can't cheaply have: **stable references** (content-addressed),
**cryptographic authorship** (signed by Ed25519 identities), and **a live coordination view** (the
QUIC stream). Build every feature through that spine rather than beside it.

---

## Phased plan

**M0 — scaffold (this commit):** workspace, domain model, axum skeleton with health + the reactive
feed seam, the `hull-scan` secret engine (real + tested), the web shell. Builds and runs.

**M1 — identity + accounts:** Ed25519 actors (static/ephemeral), personal/org accounts, signed
actions.

**M2 — issues + projects:** the full issue model (line-refs to keel blobs, statuses, actors), project
views (kanban/list/live).

**M3 — reactive home:** real keeld-QUIC bridge → SSE → activity-ranked home.

**M4 — repos + git serving:** host keel repos (multi-repo `keeld` routing), client/server push with
client-side secret scanning.

**M5 — CI/CD:** memoized content-addressed runners + agent CI triage.

**M6 — agent review + code owners + nostr:** brief-fed review flows, owners, nostr notification.
