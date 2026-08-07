# syntax=docker/dockerfile:1

# ── Builder ──────────────────────────────────────────────────────────────────────────────────────
# Pin a recent stable Rust on Debian bookworm (the repo has no rust-toolchain.toml, so we choose).
# bookworm matches the slim runtime base below, so the glibc/OpenSSL ABIs line up.
FROM rust:1-bookworm AS builder

# Native build deps beyond the C toolchain the rust image already ships (gcc, for the bundled C in
# `secp256k1-sys` and `ring`):
#   - pkg-config + libssl-dev: `webauthn-rs` (passkey accounts, always compiled) links `openssl-sys`,
#     which discovers and dynamically links the system OpenSSL. Without these the build fails.
# tokio-postgres is pure Rust (no libpq), and git/keel hosting is keel-native (no libgit2), so nothing
# else is required.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy the whole workspace and build. A cargo-chef / manifest-first split was considered but the
# server depends on pinned keel git revs; a straightforward copy-all is the robust choice here (the
# .dockerignore keeps the context small). Caches for the registry, git deps, and target dir are
# mounted so repeat builds are fast without baking the cache into the image layer.
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -p hull-server --bin hull-server \
    && cp /build/target/release/hull-server /usr/local/bin/hull-server

# ── Runtime ──────────────────────────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# Runtime libs the binary dlopen's / dynamically links:
#   - ca-certificates: outbound TLS (reqwest → CI endpoints, mirroring, nostr relays).
#   - libssl3: the OpenSSL shared lib that `openssl-sys` (via webauthn-rs) links against.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Non-root. The user's HOME is the data root: many on-disk stores default to `$HOME/.hull/...`
# (repos, artifacts, autonomy, review-cache, agent-sessions, and the activity ranking), so pointing
# HOME at a mounted volume persists all of them with one mount. Creating + chowning the dir in the
# image means a fresh Docker named volume inherits this ownership on first mount.
RUN useradd --system --uid 10001 --create-home --home-dir /var/lib/hull hull
ENV HOME=/var/lib/hull
WORKDIR /var/lib/hull

COPY --from=builder /usr/local/bin/hull-server /usr/local/bin/hull-server

# 8930 = HTTP/JSON API + SSE. 8931/udp = QUIC coordination ingress (daemons dial in via hull-agent).
EXPOSE 8930
EXPOSE 8931/udp

USER hull

# `hull-server import-postgres` is a one-shot subcommand that migrates an existing on-disk
# store.json into the Postgres named by HULL_DATABASE_URL, then exits. Run it once before first boot
# only if you are migrating a FileStore deployment; a fresh install does not need it.
#   docker compose run --rm hull-server import-postgres
ENTRYPOINT ["hull-server"]
