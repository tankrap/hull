#!/usr/bin/env bash
# Generate a webhook shared secret for Hull's CI / mirror integrations (see CI-SPEC.md).
# One secret is used on BOTH sides: Hull sends it as X-Hull-CI-Secret on dispatch, and your CI
# echoes it on the callback (same for X-Hull-Mirror-Secret on mirror inbound).
#
#   scripts/gen-secret.sh                 # print a fresh secret
#   HULL_CI_SECRET=$(scripts/gen-secret.sh) hull-server   # use as the instance default
#
# Then either set it instance-wide (env: HULL_CI_SECRET / HULL_MIRROR_SECRET) or per-repo via the
# API:  PUT /api/repos/:tenant/:repo/ci-config { "by": "<owner>", "url": "...", "secret": "<this>" }
set -euo pipefail

if command -v openssl >/dev/null 2>&1; then
  openssl rand -hex 32
elif [ -r /dev/urandom ]; then
  head -c 32 /dev/urandom | xxd -p -c 64
else
  echo "no openssl or /dev/urandom available" >&2
  exit 1
fi
