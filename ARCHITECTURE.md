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

Crate layout:
- `crates/hull-core` — domain model + storage + keel integration (the seam to `keel-*`).
- `crates/hull-server` — axum HTTP/JSON API, auth, the reactive event bridge.
- `crates/hull-scan` — secret scanning, **shared with the keel CLI** so a scan can run client-side.
- `web/` — the Vite/React frontend.

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
- *My addition:* because identity is a keypair, an agent can be a **code owner** and be notified over
  **nostr** when its code is touched — the requested code-owner feature *requires* this identity
  layer, so build them together.

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
