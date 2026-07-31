# Hull plugins & the open-core model

**Hull's core server is open source (Apache-2.0) and fully functional on its own.** The hosted
product adds value through **closed plugins** that extend the core via a stable SDK. This document is
the contract that makes that split clean: the OSS core never depends on any closed plugin — it
depends only on the plugin **SDK** (`hull-plugin`, also Apache-2.0).

## How it works

- **`hull-plugin`** (SDK, OSS) defines the extension points as traits — `SecretRuleset`, `Notifier`,
  `AuthProvider`, … — plus a `Registry` and the `Plugin` trait.
- The core server (`hull-server`) builds a `Registry`, installs its **built-in** capabilities (so
  the OSS server works with zero plugins), then registers any plugins **behind a build feature**.
  See `crates/hull-server/src/plugins.rs::build_registry` — that function is the seam.
- A plugin is a crate that depends only on `hull-plugin`, implements capability traits, and exposes
  a `register(&mut Registry)` entry point. `plugins/hull-plugin-example` is a working reference.

The server asks the registry for each capability and always gets a usable answer (built-in default
if no plugin provides one). So the same binary shape runs with 0, 1, or N plugins.

## Keeping hosted plugins closed while giving away the core

The core repo (`tankrap/hull`) contains the server, the SDK, and reference/example plugins. The
hosted plugins live in a **separate private repo** and are pulled in only for the hosted build:

```toml
# in the PRIVATE hosted build's hull-server Cargo.toml override / workspace:
hull-hosted = { git = "ssh://git@github.com/tankrap/hull-hosted", optional = true }

[features]
hosted = ["dep:hull-hosted"]
```

```rust
// build_registry(), behind the hosted feature — identical shape to the example:
#[cfg(feature = "hosted")]
hull_hosted::register(&mut reg);
```

The public repo ships the exact same seam wired to the **example** feature:

```bash
cargo run -p hull-server                          # OSS core only
cargo run -p hull-server --features example-plugins   # core + the reference plugin
```

Because the core only ever names the SDK (never a hosted crate), the OSS tree builds and runs with
no access to any private code, and the hosted binary is just `core + private plugins` at build time.

## Writing a plugin

1. New crate depending on `hull-plugin`.
2. Implement one or more capability traits (`SecretRuleset`, `Notifier`, `AuthProvider`, …).
3. Implement `Plugin` — `name()`, `description()`, and `register()` (add your capabilities to the
   `Registry`).
4. Expose `pub fn register(reg: &mut Registry) { reg.install(&MyPlugin); }`.
5. Wire it into a server build behind a feature.

See `plugins/hull-plugin-example/src/lib.rs` — a closed hosted plugin is structured identically.

## Capability roadmap

The SDK starts with `SecretRuleset`, `Notifier`, `AuthProvider`. Planned extension points (added as
the milestones land, each a natural closed/hosted seam): `StorageBackend` (sqlite/embedded → managed
Postgres + object store), `CiRunner` (local sandbox → autoscaled runners), `AgentFlow`/`Reviewer`
(BYO-key → managed AI review), `Metering`/`Billing` (no-op in OSS), `ActivitySource` (single keeld →
multi-region aggregation).

## Licensing

- Core server + SDK + reference plugins: **Apache-2.0** (see `LICENSE`). Apache (not a copyleft
  license) is the deliberate open-core choice — it lets the hosted plugins remain proprietary while
  the core is freely usable and self-hostable.
- Closed hosted plugins: proprietary, separately licensed, not distributed in this repo.
