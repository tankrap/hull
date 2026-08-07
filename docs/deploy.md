# Deploying Hull (self-host)

This guide covers running the OSS `hull-server` binary against Postgres, in Docker. The image and
`docker-compose.yml` at the repo root are the supported bring-up.

Hull has two storage tiers:

- **Domain store** — accounts, repos, sessions, reviews, etc. Postgres when `HULL_DATABASE_URL` is
  set; otherwise an on-disk `FileStore` (`store.json`). For a deployment, use Postgres.
- **On-disk state** — the hosted git/keel repos themselves, per-user agent credential bundles, and a
  handful of JSON side-stores (CI memo, autonomy, review cache, mirror ledger, activity ranking).
  These always live on the filesystem, under `$HOME/.hull/...` by default. Mount a volume for them.

The server speaks **plain HTTP** — there is no built-in TLS. Terminate TLS at a reverse proxy
(nginx/Caddy/Traefik/a cloud LB) in front of port 8930 and forward to it.

## Quickstart

```bash
# 1. Edit docker-compose.yml and replace every CHANGEME_* value (see the security section below).
#    Generate the two secrets with:  openssl rand -hex 32
# 2. Bring it up (first run builds the image — a Rust workspace build, 10-20 min):
docker compose up --build
```

The API is then on `http://localhost:8930`. Put your TLS-terminating proxy in front of it.

## Ports

| Port       | Proto   | Purpose                                                            |
|------------|---------|-------------------------------------------------------------------|
| `8930`     | TCP     | HTTP/JSON API + SSE (`/api/feed`). This is what the proxy fronts.  |
| `8931`     | UDP     | QUIC coordination ingress — daemons dial IN via `hull-agent` and stream events up. Requires `HULL_INGRESS_TOKEN` in prod. |

Note: the binary defaults its bind addresses to `127.0.0.1`, which is unreachable across the
container boundary. The compose file overrides `HULL_ADDR`/`HULL_INGRESS_ADDR` to `0.0.0.0` — keep
that if you write your own manifests.

## The prod profile (fail-fast security gate)

Setting `HULL_PROFILE=prod` (or a truthy `HULL_PROD`) makes the server **refuse to boot** unless the
security-critical config is complete. The checks (`enforce_prod_profile`) are:

| Requirement            | Why                                                                  |
|------------------------|----------------------------------------------------------------------|
| `HULL_GIT_AUTH=enforce`| Anonymous git push/fetch is unsafe in prod.                          |
| `HULL_INGRESS_TOKEN` set (non-empty) | An unauthenticated coordination ingress is unsafe.    |
| `HULL_SESSION_KEY` = exactly 64 hex chars | AEAD key sealing per-user credential bundles; no on-disk fallback in prod. |
| `HULL_DEMO_MODE` off   | Demo mode enables a **published-key owner backdoor** — never in prod. |

The compose file satisfies all four; you only need to fill in the two `CHANGEME` secrets
(`HULL_INGRESS_TOKEN`, `HULL_SESSION_KEY`), each `openssl rand -hex 32`, and the Postgres password.

## First run

**Fresh install:** nothing special — `docker compose up` seeds an empty store and starts. Do **not**
set `HULL_DEMO_MODE`.

**Migrating an existing on-disk `store.json` into Postgres:** run the one-shot subcommand once,
before starting the server, with the same `HULL_DATABASE_URL` and `HULL_DATA_DIR` the server will
use. It reads `$HULL_DATA_DIR/store.json` and replaces the Postgres domain tables with that snapshot
(idempotent):

```bash
docker compose run --rm hull-server import-postgres
```

Then `docker compose up` as normal.

## Environment variable reference

### Core / store

| Var                 | Purpose                                                        | Default |
|---------------------|----------------------------------------------------------------|---------|
| `HULL_DATABASE_URL` | Postgres connection string. Unset → on-disk `FileStore`.       | (unset) |
| `HULL_DATA_DIR`     | Dir holding `store.json` (FileStore) + `activity.json`.        | `$HOME/.hull/data` |
| `HULL_ADDR`         | HTTP API bind address.                                         | `127.0.0.1:8930` |
| `HULL_INGRESS_ADDR` | QUIC ingress bind address; `off` disables the ingress.        | `127.0.0.1:8931` |
| `HULL_PUBLIC_URL`   | Externally reachable base URL (used in generated links).      | `http://127.0.0.1:8930` |

### Prod profile / security

| Var                 | Purpose                                                        | Default |
|---------------------|----------------------------------------------------------------|---------|
| `HULL_PROFILE`      | `prod` activates the fail-fast security gate.                 | (unset → off) |
| `HULL_PROD`         | Alternative truthy flag (`1`/`true`/`on`/`yes`/`enforce`) to activate the prod profile. | (unset → off) |
| `HULL_GIT_AUTH`     | `enforce` requires a session token for git smart-HTTP; else anonymous. | off |
| `HULL_INGRESS_TOKEN`| Token daemons must present on the QUIC ingress header frame.  | (unset → open) |
| `HULL_SESSION_KEY`  | 64-hex AEAD key sealing per-user credential bundles at rest. Required in prod. | (dev: on-disk fallback) |
| `HULL_DEMO_MODE`    | Truthy enables the published-key demo owner + demo delegation re-rooting. **Keep off.** | off |

### Auth / passkey (WebAuthn)

| Var                   | Purpose                                    | Default |
|-----------------------|--------------------------------------------|---------|
| `HULL_WEBAUTHN_RP_ID` | WebAuthn relying-party ID (your host).     | `localhost` |
| `HULL_WEBAUTHN_ORIGIN`| WebAuthn origin (scheme + host + port).    | `http://localhost:5931` |

### On-disk state paths (all default under `$HOME/.hull`)

Setting `HOME` (the image sets `HOME=/var/lib/hull`) and mounting a volume there persists all of
these at once; override individually only if you want them elsewhere.

| Var                     | Purpose                                              | Default |
|-------------------------|------------------------------------------------------|---------|
| `HULL_REPOS_ROOT`       | Root holding hosted git/keel repos.                  | `$HOME/.hull/repos` |
| `HULL_AGENT_SESSIONS`   | Root for per-user agent credential bundles.          | `<HULL_DATA_DIR sibling>/agent-sessions` (`$HOME/.hull/agent-sessions`) |
| `HULL_ARTIFACTS`        | Review audit-artifact index.                         | `$HOME/.hull/review-artifacts.json` |
| `HULL_AUTONOMY`         | Per-repo autonomy settings.                          | `$HOME/.hull/autonomy.json` |
| `HULL_DEFAULT_AUTONOMY` | Fallback autonomy level when a repo has none set.    | (built-in default) |
| `HULL_CLAIM_RESOLUTIONS`| Claim-resolution ledger.                             | `$HOME/.hull/claim-resolutions.json` |
| `HULL_CONNECTIONS`      | Forge connection records.                            | `$HOME/.hull/connections.json` |
| `HULL_REPO_SETTINGS`    | Per-repo settings store.                             | `$HOME/.hull/repo-settings.json` |
| `HULL_REVIEW_CACHE`     | Cached review verdicts.                              | `$HOME/.hull/review-cache.json` |
| `HULL_MIRROR_LEDGER`    | Mirror push ledger.                                  | `$HOME/.hull/mirror.json` |
| `HULL_CI_MEMO`          | Last-seen CI verdicts.                               | `$HOME/.hull/ci-memo.json` |
| `HULL_CI_CONFIG`        | Per-repo CI config.                                  | `$HOME/.hull/ci-config.json` |

### CI dispatch

| Var               | Purpose                                                     | Default |
|-------------------|-------------------------------------------------------------|---------|
| `HULL_CI_URL`     | External CI endpoint to POST job payloads to. Unset → built-in local runner. | (unset) |
| `HULL_CI_SECRET`  | Shared secret for signing/verifying CI callbacks.           | (empty) |
| `HULL_CI_CMD`     | Override test command for the built-in runner (via `sh -c`).| auto-detect (Cargo/npm) |
| `HULL_CI_TIMEOUT` | Built-in runner timeout, seconds.                           | `600` |

### Mirroring to a forge

| Var                  | Purpose                                                   | Default |
|----------------------|-----------------------------------------------------------|---------|
| `HULL_MIRROR_TARGET` | Mirror one repo to a forge, e.g. `github:tenant/repo`. Unset → off. | (unset) |
| `HULL_MIRROR_SECRET` | Credential for the mirror push.                           | (unset) |

### Nostr bridge

| Var                 | Purpose                                             | Default |
|---------------------|-----------------------------------------------------|---------|
| `HULL_NOSTR_SECRET` | Nostr secret key (hex). Unset → nostr off.          | (unset) |
| `HULL_NOSTR_RELAYS` | Comma-separated relay URLs.                         | (unset) |

### Reviewer models (resolved via the plugin config chain)

| Var                     | Purpose                                  | Default |
|-------------------------|------------------------------------------|---------|
| `HULL_REVIEW_MODEL`     | Model ID for standard AI review.         | (unset; hosted reviewer only) |
| `HULL_REVIEW_MODEL_DEEP`| Model ID for deep AI review.             | (unset; hosted reviewer only) |

### Coordination / dev

| Var          | Purpose                                                              | Default |
|--------------|----------------------------------------------------------------------|---------|
| `HULL_KEELD` | Comma-separated `[tenant/]repo@host:port` keeld daemons to bridge outbound (dev). | (unset) |

### Config / secret plumbing

| Var                    | Purpose                                                         | Default |
|------------------------|-----------------------------------------------------------------|---------|
| `HULL_ENV_FILE`        | Path to a dotenv file the config chain loads.                   | `.env` |
| `HULL_SECRET_FILE_<KEY>`| Path to a file holding secret `<KEY>` (e.g. `HULL_SECRET_FILE_OPENROUTER`). Tried after process env, before dotenv. | (unset) |
| `GITHUB_APP_SLUG`      | GitHub App slug enabling the GitHub integration. Unset → GitHub disabled. | (empty) |
| `HOME`                 | Base for all `$HOME/.hull/...` default paths.                   | (OS) / `/var/lib/hull` in the image |

> `HULL_TEST_DATABASE_URL` also exists but is read only by the test suite, not the running server.
