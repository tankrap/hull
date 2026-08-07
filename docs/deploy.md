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
(nginx/Caddy/Traefik/a cloud LB) in front of port 8930 and forward to it. See
[TLS / reverse proxy](#tls--reverse-proxy) for concrete nginx and Caddy configs (and the one buffering
gotcha that would otherwise break the SSE feed).

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

## TLS / reverse proxy

Run a TLS-terminating reverse proxy in front of `:8930` and forward plain HTTP to the server. The one
**critical** detail: the SSE activity feed (`GET /api/feed`) is a long-lived streaming response, so the
proxy must **not buffer** that location — a buffering proxy holds the whole response and the live feed
never arrives. nginx buffers by default and needs `proxy_buffering off;` on the feed; Caddy's
`reverse_proxy` streams by default (nothing extra needed). Git smart-HTTP (`/*/git-upload-pack`,
`/*/git-receive-pack`) and the tar download also stream/carry large bodies — don't impose a small body
cap or aggressive buffering on the proxy for those either.

### nginx

```nginx
server {
    listen 443 ssl;
    server_name hull.example.com;

    ssl_certificate     /etc/letsencrypt/live/hull.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/hull.example.com/privkey.pem;

    # git push can carry large packfiles; the tar endpoint can be large too. Don't cap the body.
    client_max_body_size 0;

    # Everything → the server. Buffering ON here is fine for the normal JSON API.
    location / {
        proxy_pass http://127.0.0.1:8930;
        proxy_http_version 1.1;
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    # SSE feed: MUST NOT be buffered, or the live activity stream never flushes to the client.
    location = /api/feed {
        proxy_pass http://127.0.0.1:8930;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header Connection "";     # keep the upstream connection alive
        proxy_buffering off;                # ← the important line for SSE
        proxy_cache off;
        proxy_read_timeout 1h;              # the feed is long-lived; don't time it out
    }
}

# Redirect plain HTTP → HTTPS.
server {
    listen 80;
    server_name hull.example.com;
    return 301 https://$host$request_uri;
}
```

### Caddy

Caddy fetches/renews certs automatically and its `reverse_proxy` streams responses (SSE works with no
extra config):

```caddy
hull.example.com {
    reverse_proxy 127.0.0.1:8930 {
        # flush_interval -1 forces immediate flushing (belt-and-suspenders for SSE; Caddy already
        # streams, but this guarantees no response buffering on /api/feed).
        flush_interval -1
    }
}
```

## Health, readiness & metrics

The server exposes three **unauthenticated** operational endpoints (all cheap; poll them freely, and
point your orchestrator's probes at them):

| Endpoint   | Kind      | 200 when…                          | Non-200                        | Use it for |
|------------|-----------|------------------------------------|--------------------------------|------------|
| `/health`  | liveness  | the process is up and persistence is not degraded | 503 `degraded` when the FileStore can't persist (in-memory state has diverged from disk) | "is the process alive / should it be restarted" |
| `/ready`   | readiness | `{"ready":true}` — the store backend can serve (Postgres: a live pooled connection + `SELECT 1`; local backends: always) | 503 `{"ready":false}` | "should traffic be routed here yet" (k8s readiness gate, LB pool membership) |
| `/metrics` | scrape    | always 200, Prometheus text v0.0.4 | —                              | Prometheus scraping |

`/metrics` exposes `hull_http_requests_total` (counter, labeled `class="2xx".."5xx"`),
`hull_http_requests_in_flight` (gauge), and `hull_process_uptime_seconds` (gauge). The `/ready` and
`/metrics` scrapes are themselves exempt from the request-log and metrics counters, so polling them
doesn't inflate the numbers.

Liveness vs readiness: a liveness failure means *restart me*; a readiness failure means *hold traffic
off me but don't kill me* (e.g. Postgres briefly unreachable). Wire `/health` to the liveness probe and
`/ready` to the readiness probe.

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

### Logging / observability

| Var               | Purpose                                                          | Default |
|-------------------|------------------------------------------------------------------|---------|
| `RUST_LOG`        | Log level / filter (standard `env-filter` syntax, e.g. `info`, `hull_server=debug`). | `info` |
| `HULL_LOG_FORMAT` | `json` emits machine-parseable structured logs; anything else → compact human logs. | (dev: compact) |

Logs are JSON automatically under `HULL_PROFILE=prod` (override with `HULL_LOG_FORMAT`). One structured
line is emitted per request (`method`, `path`, `status`, `latency_ms`). See **Health, readiness &
metrics** below for the `/ready` and `/metrics` endpoints.

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
