# Plan review — red-teaming Hull + Reconciliation Review before build

A deliberate adversarial pass over the architecture we've committed to (open-core plugins, keel
embedding, the accountability invariant, the review design + its epics). Each item: the challenge,
severity, and resolution (✅ fixed now / 📝 flagged with a plan). Fixes applied in this pass are
noted with the file/issue they landed in.

---

## 1. Plugin model conflates *policy* plugins with *execution* plugins — **High** ✅

**Challenge.** The plugin registry hands the server trait objects it calls **in-process**. That's
right for auth / notifier / secret-rules / metering / depth-policy. It is *wrong* for the two most
important hosted capabilities — the **CI runner** and the **sandbox reviewer** — which execute
**untrusted code**. Running those as in-process trait objects would put arbitrary PR code in the
core server's address space. A prompt-injected or malicious change could escape.

**Resolution ✅.** Split plugins into two classes, documented in ARCHITECTURE + PLUGINS:
- **In-process capability plugins** (trait objects): `AuthProvider`, `Notifier`, `SecretRuleset`,
  `Metering`, depth-policy. Pure logic, no untrusted execution.
- **Out-of-process execution backends**: `CiRunner` / `Reviewer` are *client* traits that **dispatch
  to an isolated, sandboxed runner** (separate process/VM, no core-server memory, no credentials,
  ephemeral) and stream results back. The plugin never runs the job in-process.
- Epic D scope corrected accordingly (D1/D2): the trait is the dispatch client; the sandbox is the
  isolation boundary the design already mandates (§6.1). Sub-issue added: **run-isolation contract**.

## 2. "Human at the root" vs "org → account → machine → session" — the model was self-contradictory — **High** ✅

**Challenge.** ARCHITECTURE said both "delegation chain **roots at a human**" and "**org → account →
machine → session/run**." Those disagree: an **org is not a human**. If an org/account sat *above*
the human in the chain, the accountable root wouldn't be a person. The `human_root()` check
(`chain[0].kind == Human`) is only correct if the human really is index 0.

**Resolution ✅.** Separated two orthogonal things:
- **Accountability delegation chain** (what `Delegation` models): roots at a **natural person** —
  `human → machine → session/agent-run`. Index 0 is the human. This is what `human_principal()`
  resolves, and it's the invariant.
- **Tenancy/scope hierarchy** (org → account membership): *where* a principal is allowed to act, an
  attribute carried alongside — **never a delegation ancestor**. An org authors nothing on its own;
  only a human, or an agent that human delegated, authors within the org.
- Also closed: an *agent* has **no service/system escape hatch** — "service" automation is not an
  author. Only humans and human-delegated agents can author code changes. (forge has human /
  operator / service principal kinds; Hull's rule: an **authoring** agent must root at a human.)
- Fixed in ARCHITECTURE §Accountability and the `Delegation` doc; NEW-1166 updated.

## 3. Accountability ignored revocation + standing/scheduled agents — **High** ✅

**Challenge.** "Has a human root" is necessary but not sufficient. A nightly triage bot has no human
present at runtime; a long-lived agent token is a standing liability; and a compromised human/machine
must be able to kill *all* descendant agents. "Chains to a human" with an eternal token is
accountability in name only.

**Resolution ✅ (design) / 📝 (crypto = M1).** Strengthened the invariant:
- The root human is the **authorizing** human (who stood the agent up), not a runtime presence.
- Standing agents get **short-TTL delegations auto-renewed** by a machine credential that itself
  chains to the human — never an eternal token.
- **Revocation propagates**: revoking a human/machine invalidates every descendant agent credential
  (blast-radius = the subtree). This is forge's revocation + provenance-bundle work — reuse it.
- NEW-1166 updated to require TTL-renewal + revocation propagation, not just "has a root."

## 4. keel is embedded by **path dependency** — no versioning, three-repo fragility — **Med-High** 📝

**Challenge.** Hull path-deps `../keel`; hull-hosted path-deps `../hull` → `../keel`. keel changes
daily (we merge PRs constantly). A keel breaking change silently breaks Hull, and there's no pinned,
reproducible build. "Embed keel" also quietly couples their release cycles.

**Resolution 📝.** Define keel's **embedding API** as a versioned surface and pin by **git rev**, not
path (path only for local co-dev). Decide per-crate whether Hull embeds the *library* (store / brief
/ diff — pure logic, fine to embed) vs. talks to **keeld as a service** for multi-tenant repo
*serving* (which shouldn't be in Hull's process anyway). New issue filed: **"Define + pin keel's
embedding API (git-rev, semver the embedded crates)."**

## 5. "Pure-move detection is free via content-addressing" over-claimed — **Med** ✅

**Challenge.** Same-blob-id-at-new-path detects *exact-content relocation only*, and can
**false-positive** on coincidental identical content (boilerplate, empty files dedup to one blob). A
move **+ a one-char edit** yields a different blob id → not detected at all. "Free move detection" is
really "free exact-duplicate detection," a subset with false positives.

**Resolution ✅.** B1 re-scoped precisely: a move = a **paired delete@old + add@new of the same blob
id** (one-to-one; unpaired identical blobs are *not* moves), and near-moves (move+edit) are
explicitly **B3's** similarity job, not B1's. Removes the false "free" claim; B1 stays a cheap,
correct first pass.

## 6. "Outcome loop = the flywheel, unify don't duplicate" over-stated — **Med** ✅

**Challenge.** keel's flywheel ranks *lessons for retrieval* ("which lesson led to green"). The
review outcome loop computes *review-depth risk priors* ("which author-config is risky where").
Same substrate, **different questions and consumers**. "Unify, don't build a parallel loop" wrongly
implied one model.

**Resolution ✅.** Precise version: unify the **event substrate** (one provenance + outcome event
log), keep **two policy models** reading it (retrieval-help ranking vs review-depth priors). Epic E
+ the verification doc reworded.

## 7. "Review package is a keel object" collides with mutability — **Med** ✅

**Challenge.** A review package *evolves* (claims verify over time, pushes invalidate entries, humans
comment). keel objects are **immutable**. "The package is a keel object" (singular, mutable) is a
model mismatch.

**Resolution ✅.** Model it exactly like keel's own change DAG: an **append-only series of immutable
review artifacts** (each pass = a new content-addressed object referencing the prior) with a
**mutable head ref**. Incremental re-review (D9) produces a new artifact that reuses unchanged
claims. C8 / D8 / D9 descriptions corrected.

## 8. Strict A → B → C sequencing delays a demoable product — **Med** ✅

**Challenge.** The build order front-loads keel-core (plan capture A, semantic diff B) before the
product-visible ledger (C). But a **thin end-to-end review** — description-based claims + plain
line-diff + read-only verification + minimal UI — can ship **without A/B** and proves the product
early. Provenance-first is right for *depth*, not for *first demo*.

**Resolution ✅.** Added **C0 — thin vertical slice** (Reconciliation Review): a working
description-based review package with no plan capture and no semantic diff, then deepen with A
(plan) and B (semantic diff). De-risks and gives an early demo; A/B upgrade it in place.

## 9. Reactive "situation room" leaks across tenants + doesn't survive scale — **Med** 📝

**Challenge.** Broadcasting "agent X on file Y" fleet-wide is per-server, in-memory, and
**unscoped** — at multi-tenant scale it would leak one org's activity to another and lose state on
restart / across instances.

**Resolution 📝.** Coordination events must be **tenant-scoped** (an org sees only its own fleet),
and the activity state must be **persisted + aggregated across instances** (not one process's RAM).
New issue filed under Hull M3.

## 10. Reviewer independence is a heuristic sold as a guarantee — **Low-Med** ✅

**Challenge.** "Different model family than the author" fails when the author model is unknown
(`plan:null`), when only one provider is allowed (first-party-only tier), and because different
families can still share blind spots. It's risk-reduction, not proof.

**Resolution ✅.** Reframed (D5 + verification): independence is **best-effort diversification**; the
**real** independence mechanism is **empirical probe execution** (D3 — runs code, shares no author
reasoning). Unknown author model → most-independent available reviewer **+ forced deep tier** (already
the provenance-gap rule). Documented as risk-reduction, not a guarantee.

## 11. Open-core integrity — guard against hollowing out the OSS core — **Principle** ✅

**Challenge.** Open-core degrades into "open-washing" if the free core is a hollow shell that forces
the paid tier. Nothing currently *enforces* that the OSS core stays genuinely useful.

**Resolution ✅.** Added an explicit principle to PLUGINS.md: **the OSS core must remain a complete,
self-hostable product** — every capability has a real built-in default (read-only review, local CI,
built-in secret rules, keypair auth); hosted plugins add **scale / managed / empirical** value, never
gate basic function. A capability that would make the core non-functional without a plugin is not
allowed.

---

## Net changes from this review

- **Corrected** the accountability model (human-person root vs tenancy; + revocation/TTL) — a real
  security-invariant fix, in code-adjacent docs + NEW-1166.
- **Corrected** the plugin model (in-process policy vs out-of-process execution) — a real isolation
  fix, ARCHITECTURE + PLUGINS + Epic D.
- **Corrected** the review-state model (immutable artifact series + head ref).
- **Tightened** three over-claims (free moves, unify-the-loop, reviewer independence).
- **Added** a thin vertical slice (C0) so the product is demoable before the deep provenance work.
- **Flagged** two hosting-real concerns (keel embedding API/pinning; tenant-scoped coordination) as
  new issues.

None of these blocks starting — they make the foundation we're about to build on correct.
