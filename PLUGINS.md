# Hull plugins & the open-core model

**Hull's core server is open source (Apache-2.0) and fully functional on its own.** The hosted
product adds value through **closed plugins** that extend the core via a stable SDK. This document is
the contract that makes the split clean: the OSS core (this repo) never depends on any closed plugin
— it only defines the plugin **SDK** (`hull-plugin`) and a **registration hook**. The closed plugins
live in a **separate private repo** (`tankrap/hull-hosted`).

## How it works

- **`hull-plugin`** (SDK, in this repo) defines the extension points as traits — `SecretRuleset`,
  `Notifier`, `AuthProvider`, … — plus a `Registry` and the `Plugin` trait.
- **`hull-server`** is a **library** (`hull_server::run`) plus a thin OSS binary. `run` takes a
  `register_plugins` closure — the seam. Core built-ins are installed first (so the OSS server is
  self-sufficient), then the closure adds any extra plugins.
- The OSS binary (`crates/hull-server/src/main.rs`) passes a **no-op** closure. A hosted binary (in
  the private repo) passes a closure that registers its closed plugins. **The core never names a
  hosted crate.**

The server asks the registry for each capability and always gets a usable answer (a built-in default
if no plugin provides one), so the same code runs with 0, 1, or N plugins.

## The two repos

| Repo | Visibility | Contents |
|---|---|---|
| `tankrap/hull` (this) | **public, Apache-2.0** | core: `hull-core`, `hull-scan`, `hull-plugin` SDK, `hull-server` (lib + OSS binary) |
| `tankrap/hull-hosted` | **private** | `hull-hosted-plugins` (closed capabilities) + `hull-hosted-server` (the hosted binary) |

The private repo depends on this one (path dep for local dev; a pinned git rev in production). The
entire hosted wiring is one closure:

```rust
// crates/hull-hosted-server/src/main.rs (private repo)
hull_server::run(hull_server::Options::default(), |reg| {
    hull_hosted_plugins::register(reg);
}).await;
```

Because the public core only ever names the SDK, this repo builds and runs with no access to any
private code, and the hosted binary is just `public core + private plugins` at build time.

```bash
# OSS core (this repo):
cargo run -p hull-server

# Hosted (private repo, with a sibling ../hull checkout):
cd ../hull-hosted && cargo run -p hull-hosted-server
```

## Writing a plugin

1. New crate depending on `hull-plugin`.
2. Implement one or more capability traits (`SecretRuleset`, `Notifier`, `AuthProvider`, …).
3. Implement `Plugin` — `name()`, `description()`, `register()` (add your capabilities to the
   `Registry`).
4. Expose `pub fn register(reg: &mut Registry) { reg.install(&MyPlugin); }`.
5. From a server binary, pass it to the hook: `hull_server::run(opts, |reg| my_plugin::register(reg))`.

`tankrap/hull-hosted`'s `hull-hosted-plugins` is the reference — an open-source plugin looks the same
(the only difference is where the crate lives and whether it's public).

### Config & secrets (pluggable)

Capabilities that need config or secrets (a model key, a webhook secret) resolve them through
`reg.config("KEY")` — never by reading a hardcoded path. The core installs three `ConfigProvider`s,
tried in order:

1. **env** — process environment (`KEY`).
2. **file-secret** — a value stored in a file whose *path* is given by env: `HULL_SECRET_FILE_<KEY>`
   (e.g. `HULL_SECRET_FILE_OPENROUTER_API_KEY=~/.openrouter`). The file's trimmed contents are the value.
3. **dotenv** — `KEY=VALUE` lines from `HULL_ENV_FILE` (default `.env`).

A hosted plugin adds Infisical / Vault / a cloud secret manager with `reg.add_config_provider(...)`,
tried ahead of these — same seam, no core change.

## Two plugin classes: in-process policy vs out-of-process execution

Not every capability may run in the server's address space. **A capability that executes untrusted
code (a PR's tests, a reviewer's probe scripts) MUST NOT be an in-process trait object** — a
prompt-injected or malicious change could escape into the core server.

- **In-process capability plugins** (trait objects, called directly): `AuthProvider`, `Notifier`,
  `SecretRuleset`, `Metering`, depth-policy. Pure logic, no untrusted execution.
- **Out-of-process execution backends**: `CiRunner`, `Reviewer`. The trait is a **dispatch client**
  — it hands work to an **isolated, sandboxed runner** (separate process/VM, no core-server memory,
  no credentials, egress limited to the model API, ephemeral) and streams results back. The plugin
  never runs the job in-process. This isolation IS the review design's §6.1 sandbox.

When adding a capability, decide its class first; if it runs repo code, it's out-of-process.

## Connecting a CI system

Hull is a **dumb dispatcher**: it does not run a queue, schedule runners, or know anything about the
CI on the other side. On a check it POSTs a **standard job payload** to a configured HTTP endpoint;
that system (queue, runners, whatever) posts the verdict back. There are two integration shapes —
the HTTP contract is the primary one.

### A. External CI over HTTP (recommended — language-agnostic)

> **The full normative contract is [`CI-SPEC.md`](./CI-SPEC.md)** (field reference, status semantics,
> auth, versioning, conformance checklist) with a reference implementation at
> [`scripts/fake-ci.py`](./scripts/fake-ci.py). The summary below is orientation.

**Configure the endpoint** — per repo, or an instance default, else the built-in local runner:

```
PUT /api/repos/:tenant/:repo/ci-config     { "by": "<owner>", "url": "https://ci.example/hull", "secret": "…" }
GET /api/repos/:tenant/:repo/ci-config     → { url, has_secret, source: "repo" | "instance" | "none …" }
```

Resolution order per check: **repo config → instance default (`HULL_CI_URL` / `HULL_CI_SECRET`) →
built-in local runner** (so a bare OSS instance still runs CI). Setting the endpoint is owner/admin
gated.

**Dispatch — Hull → your CI** (POST to the configured `url`; `X-Hull-CI-Secret` header if a secret is
set):

```json
{
  "repo": "tankrap/hull",
  "change": "<keel change id>",
  "tree_id": "<keel tree content-address>",
  "intent": "<change intent>",
  "author": "<author>",
  "git_url": "https://hull.example/tankrap/hull",   // clone this…
  "ref": "<keel change id>",                          // …and check out this ref
  "callback_url": "https://hull.example/api/repos/tankrap/hull/change/<id>/ci-result"
}
```

Your CI clones `git_url` at `ref`, runs whatever it wants on its own sandboxed runners, then:

**Callback — your CI → Hull** (POST `callback_url`, echo the `X-Hull-CI-Secret`):

```json
{ "status": "green" | "red" | "errored", "summary": "42 tests, 0 failed" }
```

Hull then memoizes the verdict by `tree_id`, writes keel verification (which the reconciliation
ledger + merge gate consume), and notifies. That's the whole contract — **two HTTP calls**.

**What Hull handles so your CI doesn't:** change→tree resolution; **content-addressed memoization**
(an identical tree is an instant hit — Hull never dispatches it again); a de-dupe guard so a tree
already in flight isn't dispatched twice; verification write-back; `ci_passed`/`ci_failed`
notifications; the `/check` endpoint + `{force}` bypass. Return **`errored`** (not `red`) for infra
failures so a blip isn't cached as a failing tree.

### B. In-process `CiRunner` (self-host convenience)

For a runner compiled into the server, implement `CiRunner::run(&CiRequest) -> CiOutcome` (given a
materialized `req.workdir`) and `reg.set_ci_runner(...)`. The built-in [`default_local_ci`]
(`cargo test`/`npm test`, or `HULL_CI_CMD`) is exactly this — the no-config fallback. **Because it
runs untrusted PR code in-process, this shape is only for local/self-host use; anything hosted uses
shape A** and runs tests on isolated, sandboxed runners it controls.

## Building an AI reviewer (Epic D)

`Reviewer` is the seam for the review *judgment* (distinct from `CiRunner`, which is the *execution*).
The OSS core ships the **reconciliation** reviewer as the default (`default_review` — deterministic
claims-vs-facts, Epic C). A hosted plugin swaps in the **model-backed** reviewer.

A working one ships in this repo: **`hull-review-openrouter`** reviews the change with a model over
[OpenRouter](https://openrouter.ai) and returns a constrained-schema verdict, falling back to
reconciliation on any error. Its API key + model come from the **pluggable config** (below) —
`OPENROUTER_API_KEY`, `HULL_REVIEW_MODEL` (default `anthropic/claude-sonnet-5`) — never a hardcoded
path or secret. The `hull-server` binary activates it only when a key resolves, so the OSS core stays
model-free; in the real open-core split it moves to the hosted binary. Your own reviewer is the same
shape:

```rust
impl Reviewer for OpencodeReviewer {
    fn review(&self, req: &ReviewRequest) -> ReviewPackage {
        // req.source_url → keel-native content-addressed tree tar (CI-SPEC §6); NOT git.
        // req.intent / req.lesson / req.author → the narrative. req.facts → files/ops/verify/secrets.
        //
        // Fetch source into an ISOLATED sandbox, drive the model/agent (opencode) over the change,
        // and return a CONSTRAINED-SCHEMA verdict — never free-text parsed for approval.
        let v = self.run_in_sandbox(&req.source_url, &req.intent); // your harness
        ReviewPackage {
            verdict: if v.ok { ReviewVerdict::Approve } else { ReviewVerdict::RequestChanges },
            summary: v.summary,
            findings: v.findings,   // {path, line?, severity, note}
            ledger: None,           // or attach your own evidence
        }
    }
}
// reg.set_reviewer(Arc::new(OpencodeReviewer::new()))
```

The non-negotiables from the design (Epic D):
- **Isolation (D2):** the reviewer runs the change's code — untrusted. Fetch/run in a separate
  process/VM, no core credentials, egress limited to the model API, ephemeral. Never in-process.
- **Constrained-schema verdict (D7):** the verdict/findings are structured output; repo content is
  *data, never instructions*. Don't parse free text for an approval — a prompt-injected diff must not
  be able to talk the reviewer into "approve".
- **Independence:** review under a model family independent of the author; an agent never reviews its
  own PR (the core already enforces author≠reviewer).
- **Advisory only (D11):** a `Reviewer` verdict is input to the merge gate, **not** a merge
  authorization. Hull's gate still requires keel-verify **green** + an **independent approval**; a
  reviewer's "approve" never satisfies a protected-path merge by itself.

`source_url` is the keel-native, content-addressed source (a `…/tree/:tree_id/tar` — see CI-SPEC §6),
so an AI reviewer and a CI runner fetch source the exact same way. No git.

## Capability roadmap

The SDK starts with the in-process trio `SecretRuleset`, `Notifier`, `AuthProvider`. Planned:
`StorageBackend` (sqlite/embedded → managed Postgres + object store, in-process), `Metering`/`Billing`
(no-op in OSS, in-process), `ActivitySource` (single keeld → multi-region aggregation, in-process),
and the two **out-of-process** backends `CiRunner` (local sandbox → autoscaled runners) and
`Reviewer`/`AgentFlow` (BYO-key → managed AI review).

## Open-core integrity — the core must stay genuinely useful

Open core degrades into "open-washing" if the free core is a hollow shell that forces the paid tier.
**Rule: the OSS core must remain a complete, self-hostable product.** Every capability has a real
built-in default — read-only review, local CI, built-in secret rules, keypair auth. Hosted plugins
add **scale / managed / empirical** value; they never gate basic function. A capability whose absence
makes the core non-functional is not allowed.

## Licensing

- Core server + SDK: **Apache-2.0** (see `LICENSE`). Apache (not a copyleft license) is the
  deliberate open-core choice — it lets the hosted plugins stay proprietary while the core is freely
  usable and self-hostable.
- Closed hosted plugins: proprietary, separately licensed, in the private `tankrap/hull-hosted` repo.
