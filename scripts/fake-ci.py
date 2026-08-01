#!/usr/bin/env python3
"""Reference CI integration for Hull — see CI-SPEC.md.

A minimal, conforming stand-in: receives Hull's dispatch, (pretends to) run checks, and posts a
verdict back to the callback URL. Real systems replace `run_checks` with a clone + sandboxed run on
their own runners; the HTTP contract is all Hull cares about.

Usage:
    python3 scripts/fake-ci.py <port> [green|red|errored] [shared-secret]

Then point a repo (or the instance) at it:
    HULL_CI_URL=http://127.0.0.1:<port> HULL_CI_SECRET=<secret> hull-server
    # or: PUT /api/repos/:tenant/:repo/ci-config { by, url, secret }
"""
import json
import sys
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 9099
VERDICT = sys.argv[2] if len(sys.argv) > 2 else "green"   # what this stand-in reports
SECRET = sys.argv[3] if len(sys.argv) > 3 else ""


def run_checks(job):
    """Where a real CI clones job['git_url'] @ job['ref'] and runs tests on a sandboxed runner.
    Returns (status, summary). This stand-in just reports a fixed verdict."""
    return VERDICT, f"fake-ci reporting {VERDICT} for {job['change'][:12]}"


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        # §8 — verify the shared secret on the inbound dispatch.
        if SECRET and self.headers.get("X-Hull-CI-Secret") != SECRET:
            self.send_response(401)
            self.end_headers()
            return

        job = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        version = self.headers.get("X-Hull-CI-Version", "?")
        print(f"[fake-ci] dispatch v{version}: {job['repo']} change={job['change'][:12]} "
              f"tree={job['tree_id'][:12]} clone={job['git_url']}@{job['ref'][:12]}", flush=True)

        # §5 — acknowledge receipt immediately; the verdict comes later via the callback.
        self.send_response(202)
        self.end_headers()
        self.wfile.write(b'{"accepted":true}')

        # ...run the job (your concern)...
        status, summary = run_checks(job)

        # §7 — post the verdict to the exact callback_url, echoing the secret.
        headers = {"Content-Type": "application/json"}
        if SECRET:
            headers["X-Hull-CI-Secret"] = SECRET
        req = urllib.request.Request(
            job["callback_url"],
            data=json.dumps({"status": status, "summary": summary}).encode(),
            headers=headers,
            method="POST",
        )
        try:
            resp = urllib.request.urlopen(req)
            print(f"[fake-ci] posted {status} → callback [{resp.status}]", flush=True)
        except Exception as e:  # noqa: BLE001 — reference tool, surface any failure
            print(f"[fake-ci] callback failed: {e}", flush=True)


if __name__ == "__main__":
    print(f"[fake-ci] listening on :{PORT}, will report '{VERDICT}'"
          f"{' (secret set)' if SECRET else ''}", flush=True)
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
