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
