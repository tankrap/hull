# Verification: Reconciliation-Based Review design vs. the keel + Hull stack

Cross-check of `ai-code-review-platform-design.md` against what the stack actually provides today
(keel core + Hull), what's roadmapped, and where each piece lands on the open-core boundary.

**Verdict:** strongly compatible. The design's stated structural advantage — *"owns the VCS, the
hosting, the CI results, and (via the CLI) the generation-time provenance"* — is precisely keel (VCS
+ CLI provenance) + Hull (hosting + CI). No component conflicts with the stack; the main net-new
work is the **reconciliation engine / claim ledger** and **semantic diff**, both of which have
natural homes and, for the outcome loop, an existing mechanism (keel's flywheel) to build on.

## Component-by-component

| Design section | Stack status | Notes |
|---|---|---|
| §4 Provenance manifest (session, model, prompt, tool-calls) | **Have (partial)** | keel's `Session` object already carries task/model/prompt/tool_calls/tool_results/tokens/verification, and `keel capture` maps Claude Code / Aider transcripts into it (NEW-1088). |
| §4 …the **plan** (revisions + per-step status) + **human interventions** | **Gap** | keel's `Session` has no structured plan or interventions. This is the design's own "new capture requirement." → extend `Session` + the `capture` adapters. |
| §4 Signed manifest attached to the push; triage floor ignores manifest for gating | **Have the primitive** | keel already has Ed25519 provenance/delegation signing; Hull `Actor`s are Ed25519. Reuse it (answers design open-Q #5) — don't build new signing. Wire "sign the session/manifest" + "manifest never lowers the triage floor". |
| §4 Multi-session PRs (ordered manifest list, union extraction) | **Have** | A keel change history carries multiple sessions; capturing several and unioning is natural. |
| §7 Semantic diff at the **storage layer** (moves/renames/format churn) | **Gap (roadmapped) + partial free win** | keel diff is line-level (`textdiff`) today. The AST/semantic ladder is roadmapped (keel NEW-1011, 1017–1020). **Content-addressing gives pure-move detection for free** — a moved file keeps its blob id, so "same blob, new path = move" needs no AST. Start there; add tree-sitter for renames/format. |
| §3/§5 Reconciliation engine + **claim ledger** | **Net-new** | New Hull component. Its four inputs already exist: intent (keel `Session`/`why`), semantic diff (keel), CI (Hull M5), **repo context = keel's fused brief / live graph** (callers, contracts — already computed). |
| §5 Statuses incl. "verified mechanically" | **Have the primitive** | keel `verify` (green/red) + coverage-on-changed-lines (Hull CI) → the mechanical status. |
| §6.1 Sandbox reviewer that **runs code** + writes independent probe tests | **Gap → Hull CI (M5) + hosted** | The sandbox is Hull's memoized CI runner (M5); the probe-writing reviewer is a hosted agent flow. "Budget compute for it" = the managed, paid tier. |
| §6.2 Model tiering; §6.4 provider allowlists / first-party tier | **Have the pattern → hosted** | The forge lineage already did provider-agnostic model routing (OpenRouter + Anthropic). Becomes hosted `Reviewer`/provider-control plugins. |
| §6.3 Reviewer independence (different model family than author) | **Have the input** | Author model is in the `Session` manifest (`Session.model`) — switch the reviewer accordingly. |
| §6.5 Prompt-injection defense (data-not-instructions, constrained-schema verdict) | **Aligned** | Matches the never-merge / gated / structured-output posture already used across the agent flows. Implement in the reviewer plugin. |
| §6.6 Review **artifact** (auditability) | **Have a better home** | Store the artifact as a **content-addressed keel object** — immutable, addressable, deduped, provenance-linked — rather than a DB blob. keel-native win. |
| §6.7 Incremental re-review (invalidate only changed-evidence claims) | **Have the substrate** | The design calls this "platform-native, bolt-on tools can't match" — keel's content-addressing delivers exactly diff-aware invalidation (changed blob ids ⇒ which claims to re-verify). |
| §8 Outcome feedback loop → risk priors → depth policy | **Have the mechanism (keel flywheel)** | keel already joins provenance (sessions) with outcomes (`verify` green/red) and computes feedback-weighted priors (lesson-help scores). The design's risk-priors is a **second policy consumer of the same substrate** — unify, don't build a parallel loop. |

## Open-core placement (where each piece lives)

This maps cleanly onto the boundary we just set up:

- **keel core (open):** manifest capture incl. **plan + interventions + signing**; semantic-diff ladder; content-addressed **review artifacts**; the incremental-invalidation substrate.
- **Hull core (open):** the **review package** (ledger + findings + guided reading order), the reconciliation engine v1, deterministic gates, and **read-only** ("verified by agent, read-only") verification — a complete, self-hostable reviewer.
- **Hosted plugins (closed, `tankrap/hull-hosted`):** the expensive/managed moat — **sandbox execution + probe tests** (empirical verification), **model tiering + provider allowlists + first-party tier**, and the **outcome-priors depth policy**. These are exactly the "budget compute for it" parts, and each is a capability trait (`Reviewer`/`AgentFlow`, `CiRunner`, provider-control, `Metering`) on the existing plugin registry.

That split is faithful to the design's own emphasis: the OSS core gives you the ledger and read-only review; the hosted tier adds the empirical, compute-heavy verification and the priors loop.

## Gaps to build (in the design's own build order §11)

1. **Extend keel `Session` + `capture`**: structured `plan` (steps + status), `interventions`, and signing. *(keel core; builds on NEW-1088.)*
2. **Semantic diff**: start with free content-addressed move detection, then tree-sitter renames/format. *(keel core; NEW-1011/1017–1020.)*
3. **Reconciliation engine v1 + claim ledger + package UI**: plan-first extraction, read-only verification, the three-section package. *(Hull core; new milestone, sits with M2 issues/PRs and M6 review flows.)*
4. **Sandbox + probe tests** → empirical verification. *(Hull CI M5 + a hosted reviewer plugin.)*
5. **Tiering / incremental re-review / injection hardening.** *(mix; re-review substrate is keel-native.)*
6. **Outcome priors** as a second consumer of the keel flywheel. *(extend, don't duplicate.)*

## Recommendations / tensions

- **Unify the two feedback loops.** keel's flywheel (retrieval ranking) and §8 (review-depth priors) are the *same* provenance + verification + priors substrate with two policy consumers. Build one loop, two readouts. This strengthens the "durable moat" the design names in §8 — it's literally keel's thesis.
- **Reuse keel signing** for the manifest (design open-Q #5): the Ed25519 provenance/delegation chain already exists; don't stand up a second signing system.
- **Semantic diff is the critical-path dependency** (§3, §5, §7 all need it). De-risk by shipping content-addressed move/format classification first — it's cheap in keel and unlocks the "83% mechanical, here are the 120 behavioral lines" claim without the full AST ladder.
- **Review artifacts and the ledger should be keel objects**, not just DB rows — immutability + addressability + dedup + provenance links come for free and make §6.6 auditability and §6.7 invalidation trivial.
- **Manifest-as-context-not-gate** (§4 signing note) aligns with the never-merge/gated posture already used; the triage model setting the depth *floor* is the right call and matches "reviewer verdict alone never satisfies protected-path merge."

**Bottom line:** nothing in the design fights the stack; it reads like a natural next layer on it. The two real builds are semantic diff (roadmapped, with a free head-start from content-addressing) and the reconciliation engine/ledger (net-new Hull core). Everything compute-heavy lands cleanly as closed hosted plugins on the boundary we just established.
