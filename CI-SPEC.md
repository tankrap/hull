# Hull CI Integration Standard

**Contract version: 1** &nbsp;·&nbsp; Status: stable &nbsp;·&nbsp; Transport: HTTP/JSON

This is the complete standard for building a CI system that runs checks for a Hull repository. If
your system speaks the two HTTP calls below, it works with Hull. Nothing else is required, and Hull
imposes nothing on how you queue, schedule, isolate, or scale — that is entirely yours.

The key words **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are used as in RFC 2119.

---

## 1. Design principles

1. **Hull is a dispatcher, not a scheduler.** Hull POSTs a job and waits for a result. It owns no
   queue, no runner pool, and no knowledge of your internals.
2. **The change is the unit of work, the *tree* is the identity.** Every job names a keel `change`
   and its content-addressed `tree_id`. Two changes with an identical tree are the same work.
3. **Asynchronous by default.** A verdict arrives whenever your system is done, via a callback. Hull
   never blocks on your runners.
4. **The source of truth is Hull.** Verification state, memoization, and de-duplication live in Hull.
   Your system is stateless from Hull's point of view: it receives a job and reports a verdict.

---

## 2. Terminology

| Term | Meaning |
|------|---------|
| **change** | A keel change id (hex). The revision under test. |
| **tree_id** | The keel tree content-address for that change. The cache key for a verdict. |
| **dispatch** | The request Hull sends your system to start a job (§5). |
| **callback** | The request your system sends Hull with the verdict (§6). |
| **verdict** | `green` \| `red` \| `errored` (§7). |
| **CI endpoint** | The HTTPS URL Hull POSTs dispatches to, per repo or per instance (§4). |

---

## 3. Sequence

```mermaid
sequenceDiagram
    participant U as Actor / PR-open
    participant H as Hull
    participant C as Your CI
    participant R as Your runner
    U->>H: POST …/change/:id/check
    Note over H: memo hit? → return verdict, no dispatch
    H->>C: POST <ci endpoint>  (dispatch payload, §5)
    C-->>H: 2xx (accepted)
    H-->>U: { status: "dispatched" }
    C->>H: GET source_url  (keel tree tar, by tree_id)
    H-->>C: repository objects
    C->>R: schedule + run tests (your concern)
    R-->>C: pass / fail
    C->>H: POST callback_url  (verdict, §6)
    Note over H: memoize by tree_id · write keel verification · notify
    H-->>C: 200 { recorded }
```

---

## 4. Configuration & endpoint resolution

A repository's CI endpoint is resolved in this order; the first hit wins:

1. **Per-repo** — set via the config API below.
2. **Instance default** — the `HULL_CI_URL` (and `HULL_CI_SECRET`) the Hull operator sets.
3. **None** — Hull falls back to its built-in local runner (self-host convenience). Your system is
   not involved.

```
GET  /api/repos/:tenant/:repo/ci-config
     → { "url": "https://ci.example/hull" | null,
         "has_secret": true,
         "source": "repo" | "instance" | "none (built-in local runner)" }

PUT  /api/repos/:tenant/:repo/ci-config          (owner/admin only)
     { "by": "<actor-id>", "url": "https://ci.example/hull", "secret": "<shared secret>" }
     → { "url": "...", "cleared": false }
```

Setting `url` to `""` clears the per-repo endpoint (falls back to the instance default). The `secret`
is write-only; `GET` never returns it, only `has_secret`.

---

## 5. Dispatch (Hull → your CI)

When a check is triggered for a change whose tree has no memoized verdict, Hull sends:

```
POST <ci endpoint>
Content-Type: application/json
X-Hull-CI-Version: 1
X-Hull-CI-Secret: <shared secret>          # present iff a secret is configured
```
```json
{
  "repo":         "tankrap/hull",
  "change":       "21ea2242186c99ff…",
  "tree_id":      "f7a2d47020c63c8e…",
  "intent":       "fixes #6 pagination off-by-one",
  "author":       "justin",
  "source_url":   "https://hull.example/api/repos/tankrap/hull/tree/f7a2d47020c63c8e…/tar",
  "callback_url": "https://hull.example/api/repos/tankrap/hull/change/21ea…/ci-result"
}
```

### Field reference (normative)

| Field | Type | Notes |
|-------|------|-------|
| `repo` | string | `tenant/repo`. Routing/logging only. |
| `change` | string | keel change id. Include it in your callback path (Hull derives it from `callback_url`; you MUST use `callback_url` verbatim). |
| `tree_id` | string | The keel **content-address** of the source. `source_url` resolves to exactly this tree; you **MAY** use it as your own cache key, and you **MAY** verify the fetched archive re-hashes to it. |
| `intent` | string | Human summary of the change. Display only. |
| `author` | string | Actor handle. Display only. |
| `source_url` | string | keel-native, content-addressed source: **GET** it to receive the change's tree (identified by `tree_id`) as a `tar` archive. This is the *only* fetch path — see §6. Treat as **opaque**. |
| `callback_url` | string | Where to POST the verdict (§6/§7). Treat as **opaque**; do not construct it yourself. |

**Your CI MUST:**
- Respond `2xx` promptly to acknowledge receipt (this is *accepted*, not *done*). A non-2xx response
  makes Hull treat the dispatch as failed and surface an error to the caller.
- Treat the body as **forward-compatible**: ignore unknown fields. Hull MAY add fields in later
  contract versions without bumping `X-Hull-CI-Version` for additive changes.

**Your CI SHOULD:**
- Verify `X-Hull-CI-Secret` on the dispatch if you configured one, and reject mismatches.

---

## 6. Fetching the source

Source is fetched **by content address, over keel — never with git.** keel is the substrate; git in
Hull exists only for interop/mirroring and is not part of this contract.

`GET source_url` returns the change's keel **tree** (the one named by `tree_id`) as a `tar` archive.
Extract it into your sandbox and run there:

```
curl -sL "$source_url" | tar -x -C work && cd work
```

Because the source is addressed by `tree_id`, the fetch is:
- **Content-addressed** — the same `tree_id` always yields the same bytes; cache on it freely.
- **Verifiable** — a conforming runner **MAY** re-hash the extracted tree and confirm it reproduces
  `tree_id`, rejecting a mismatch.
- **git-free** — do **not** `git clone`. A runner that shells out to git for source is not
  conforming; there is no ref to check out and no `.git` in the archive.

Your runners **SHOULD** fetch and extract inside the isolated sandbox, not on the control-plane host.

> **Private repositories:** `source_url` auth is out of scope for contract v1 (the archive is
> fetchable by the CI system's network identity). A scoped, short-lived fetch token in the dispatch
> payload is reserved for a future version; ignore its absence today.

---

## 7. Result callback (your CI → Hull)

When the job finishes — success, failure, or infrastructure error — POST the verdict to the exact
`callback_url` from the dispatch:

```
POST <callback_url>
Content-Type: application/json
X-Hull-CI-Secret: <shared secret>          # MUST echo it if the endpoint has a secret
```
```json
{ "status": "green", "summary": "42 tests, 0 failed, in 8.1s" }
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `status` | enum | yes | `green` \| `red` \| `errored`. Anything else → `400`. |
| `summary` | string | no | One-line human summary, shown in the UI and notifications. |

Response: `200 { "recorded": "<status>" }`.

### Status semantics (normative)

| `status` | Meaning | Hull's action |
|----------|---------|---------------|
| `green` | checks passed | memoize verdict by `tree_id`; set keel verification **green**; notify `ci_passed` |
| `red` | checks failed | memoize verdict by `tree_id`; set keel verification **red**; notify `ci_failed` |
| `errored` | could not produce a verdict (timeout, infra failure, no tests) | **not** memoized; verification unchanged |

Return **`errored`, not `red`,** for infrastructure problems. `red` is a statement about the code;
`errored` is a statement about your system. Only `green`/`red` are cached, so an outage never poisons
a tree's verdict — a later re-check will re-dispatch.

---

## 8. Authentication

- A repo (or the instance) **SHOULD** configure a shared `secret`.
- When set, Hull sends `X-Hull-CI-Secret` on the **dispatch**, and **requires** it on the
  **callback** — a missing or wrong secret on the callback is rejected `401`, and no verdict is
  recorded.
- The secret is symmetric and per-endpoint. Rotate by `PUT`-ing a new `ci-config`.
- Transport **SHOULD** be HTTPS in production; the secret is a bearer credential.

---

## 9. What Hull guarantees (so you don't implement it)

- **Content-addressed memoization.** A tree with an existing `green`/`red` verdict is **never
  re-dispatched** — the check returns the cached verdict instantly.
- **In-flight de-duplication.** While a tree's job is outstanding, a second check for the same tree
  does **not** dispatch again; it returns `pending`. (A caller MAY force a re-dispatch with
  `{"force": true}` on the check, which bypasses both the memo and this guard.)
- **Verification write-back.** A `green`/`red` verdict updates keel verification, which gates PR
  merges and feeds the reconciliation ledger.
- **Notifications.** Hull emits `ci_passed` / `ci_failed` to the relevant actors.

Because de-dup is best-effort (in-memory), your system **SHOULD** itself be idempotent per
`(tree_id)` or per `callback_url`: a duplicate dispatch **MUST** be safe to run, and a duplicate
callback for an already-recorded tree simply re-affirms the same verdict.

### 9.1 Test independence (a change may not verify itself)

A change that **adds or modifies its own tests** cannot use those tests as evidence for approval — a
green run over tests the change authored is circular. Hull enforces this, and it needs **nothing from
you**: it is a property of *which tree Hull dispatches*, not of your reporting.

When a change touches any test file, Hull composes an **independence tree** — the change's new code,
but with every touched test **restored to the parent's version** (or **dropped** if the change newly
added it) — and dispatches that tree as an ordinary job (§5). Because the tree is content-addressed,
it dispatches and memoizes exactly like any other:

- The composed tree is **green** ⇒ the tests the change *did not author* pass against its new code →
  `verified_mechanically` (genuine independent evidence; eligible for auto-approval).
- **red** ⇒ a pre-existing test fails against the new code — the change **broke or weakened** a test
  (restoring the real test surfaces the failure the change's edit hid) → **contradiction**, merge
  blocked.
- **errored / no tests** ⇒ no pre-existing test exercises the change → `self_attested`: green, but not
  independently verified → escalates to a human/independent reviewer.

Your CI does not special-case this: you receive a normal dispatch for a normal `tree_id` and run the
suite you find in the tree. The independence guarantee is entirely in what Hull chooses to send.

---

## 10. Timeouts, retries, lost callbacks

- Hull does not time out a dispatched job; the verdict is whatever your callback eventually says.
  Your system **SHOULD** enforce its own job timeout and report `errored` when it fires.
- If a callback never arrives (your system crashed), the tree stays unverified. A human (or an
  automated re-check with `force`) re-triggers it. Hull does not poll you.
- If Hull is unreachable when you call back, retry with backoff; the callback is idempotent.

---

## 11. Conformance checklist

A conforming CI integration:

- [ ] Accepts `POST` at its configured endpoint and returns `2xx` on receipt.
- [ ] Verifies `X-Hull-CI-Secret` on dispatch when a secret is configured.
- [ ] Fetches `source_url` (keel tree tar), extracts, runs its checks in isolation — no git.
- [ ] POSTs `{status, summary}` to the exact `callback_url`, echoing `X-Hull-CI-Secret`.
- [ ] Uses `errored` (not `red`) for infrastructure failures.
- [ ] Ignores unknown dispatch fields (forward-compatible).
- [ ] Is safe under duplicate dispatch and duplicate callback.

---

## 12. Reference implementation (illustrative)

A minimal, single-file CI stand-in — receive the dispatch, run, call back — lives at
`scripts/fake-ci.py`. In pseudocode:

```python
def on_dispatch(req):
    assert req.header("X-Hull-CI-Secret") == MY_SECRET      # §8
    job = req.json()
    ack(202)                                                # §5: acknowledge, don't block

    workdir = fetch_tar(job["source_url"])                  # §6 — content-addressed, not git
    result  = run_tests_in_sandbox(workdir)                 # your concern

    status  = "green" if result.ok else "red"               # §7
    if result.infra_error: status = "errored"
    post(job["callback_url"],
         json={"status": status, "summary": result.summary},
         headers={"X-Hull-CI-Secret": MY_SECRET})
```

---

## 13. Versioning

- The current contract is **version 1**, advertised on every dispatch as `X-Hull-CI-Version: 1`.
- **Additive** changes (new dispatch fields, new optional callback fields) do **not** bump the
  version; integrations MUST tolerate them.
- **Breaking** changes (renamed/removed fields, changed semantics) bump the version. Hull MAY, for a
  transition period, support multiple versions and let an endpoint negotiate via the header.
- Treat any field not defined in this document as reserved.

---

## 14. Security & isolation (running untrusted code)

**A CI job executes untrusted code.** The tree it runs — `build.rs`, proc-macros, test bodies,
`npm` lifecycle scripts, a `Makefile` — is written by whoever authored the change, which on an open
or multi-tenant instance is **not** someone you trust. A runner that executes a job on the
control-plane host, or on a box that holds any credential, is a full remote-code-execution and
credential-exfiltration hole. This section is normative for any runner that accepts changes from
untrusted authors.

### 14.1 Isolation boundary

- Each job **MUST** run in a **single-use, hardware-or-kernel-isolated sandbox** — a microVM
  (**Firecracker**, Cloud Hypervisor) or an equivalently strong boundary (gVisor, a locked-down
  container). A shared interpreter, a bare `chroot`, or a plain host subprocess is **NOT** sufficient.
- A sandbox **MUST NOT** be reused across jobs. Destroy the whole microVM/rootfs after each job so
  nothing (a planted binary, a poisoned cache, a lingering process) survives into the next job.
- The runner **MUST NEVER** execute job code on the control-plane host or on any host with access to
  Hull's secrets, the CI shared secret, or cloud-provider credentials.

### 14.2 Credentials & environment

- The job environment **MUST** be scrubbed: pass only an explicit allowlist of benign variables. It
  **MUST NOT** contain the `X-Hull-CI-Secret`, cloud keys, registry tokens, or `source_url` auth.
- The runner **MUST** block access to the **cloud metadata endpoint** (`169.254.169.254`, `fd00:ec2::254`)
  — a classic path from RCE to instance-role credentials.
- Fetch `source_url` and post the callback **from the control plane / a broker**, not from inside the
  sandbox, so the secret never enters untrusted territory. If the sandbox must fetch, use a
  single-use, source-scoped token (reserved; see §6) — never the CI secret.

### 14.3 Network

- Default **egress-deny**. A job **SHOULD** run with no outbound network. Where dependency resolution
  needs it, restrict egress to an allowlisted, authenticated **package proxy** — never the open
  internet, never Hull's internal network.
- No inbound network to the sandbox.

### 14.4 Privilege & resources

- Run as a **non-root**, unprivileged user; **read-only** root filesystem; a writable **tmpfs**
  scratch that dies with the job.
- **Drop all capabilities**, set `no-new-privileges`, apply a **default-deny seccomp** profile.
- Enforce **CPU, memory, PID, and disk** limits and a **wall-clock timeout** (report `errored` when
  it fires — §7). Cap captured output so a job can't OOM the runner by flooding logs.
- No host filesystem mounts into the sandbox beyond the extracted tree.

### 14.5 Reporting

- Treat everything from the job — exit status, stdout/stderr, any file it writes — as **untrusted
  data**. Truncate and sanitize `summary` before returning it (§7); never let job output smuggle
  control characters or forge additional fields.

> Hull's built-in local runner (§4, the "None" fallback) executes on the host **without** these
> protections. It exists
> for single-tenant self-hosting where every author is trusted. **Do not** point it at untrusted or
> multi-tenant input — configure a conforming sandboxed endpoint instead.
