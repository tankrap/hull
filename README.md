# Hull

A hosted [keel](https://github.com/tankrap/keel) — the agent-native VCS platform. keel is the
substrate (content-addressed store, git-compatible wire, the fused code+session graph, the flywheel,
coordination over QUIC). Hull is the hosted layer: accounts, issues, projects, CI/CD, cryptographic
agent identity, security scanning, and a home page that reacts to the work actually happening.

**Agent-native first.** Humans and agents are peers — both have Ed25519 identity, both can own code,
both appear in the same live coordination stream. Every object links back to keel provenance, so
references are content-addressed (stable across edits), authorship is signed, and the home page is a
live "situation room" of the fleet.

See **[ARCHITECTURE.md](./ARCHITECTURE.md)** for the design and the feature map (including where the
requested features land and where I've added to them).

## Layout

```
crates/
  hull-core     domain model (accounts, actors, repos, issues, projects) + storage seam
  hull-scan     secret scanning — shared with the keel CLI (client-side) and Hull (server-side)
  hull-server   axum HTTP/JSON API + the reactive activity feed (keeld QUIC → SSE)
web/            React + TypeScript + Vite frontend (the situation-room home page)
```

## Run (M0 scaffold)

```bash
cargo run -p hull-server            # http://127.0.0.1:8930
# GET /health · /api/home · /api/feed (SSE) · /api/repos · /api/repos/:repo/issues · POST /api/scan

cd web && npm install && npm run dev # the home page (proxies the API)
```

## Status

M0 scaffold: domain model, server skeleton with the reactive feed seam, a real+tested secret-scan
engine, and the web shell. Builds and runs. Milestones M1–M6 are in ARCHITECTURE.md.
