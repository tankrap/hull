// Substrate tab: the repo's decentralized state read back from nostr relays — the branch ref, the
// verified authorship provenance (schnorr transport + Ed25519 actor sig, annotated with LOCAL
// accountability/authority), the federated view across peer instances, and where blobs are mirrored.
// Read-only over GET …/substrate (#47) and …/federation (#50). Empty unless the instance has relays
// configured (HULL_NOSTR_*).
import { useState, useEffect } from "react";
import { Card, SectionHeader } from "./primitives";

type Prov = {
  change: string;
  actor: string;
  actor_handle?: string | null;
  human_root?: string | null;
  intent: string;
  ts: number;
  signatures_valid: boolean;
  accountable: boolean;
  authorized: boolean;
};
type Substrate = {
  enabled: boolean;
  relays?: string[];
  blob_servers?: string[];
  ref?: { branch: string; commit: string; source: string } | null;
  provenance?: Prov[];
  _error?: number; // set client-side when the fetch failed (status, or 0 for network) — distinct from enabled:false
};
type Instance = { instance: string; commit: string; self: boolean };
type Federation = {
  enabled: boolean;
  branch?: string;
  peers?: string[];
  instances?: Instance[];
  responded?: number;
  expected?: number;
  missing?: string[];
  diverged?: boolean;
};

const short = (s: string, n = 12) => (s.length > n ? s.slice(0, n) + "…" : s);
const Pill = ({ ok, label, warn = false }: { ok: boolean; label: string; warn?: boolean }) => (
  <span
    className={`inline-flex items-center gap-1 text-[11px] font-medium px-1.5 py-[2px] rounded-badge ${
      ok ? "bg-clear-wash text-clear-text" : warn ? "bg-brass-wash text-brass-text" : "bg-fault-wash text-fault-text"
    }`}
  >
    <span className={`w-1.5 h-1.5 rounded-full ${ok ? "bg-clear" : warn ? "bg-brass" : "bg-fault"}`} />
    {label}
  </span>
);

export function SubstrateView({ tenant, repo, authHeaders, handleOf }: {
  tenant: string;
  repo: string;
  authHeaders: () => Record<string, string>;
  handleOf: (id: string) => string;
}) {
  const base = `/api/repos/${encodeURIComponent(tenant)}/${repo}`;
  const [sub, setSub] = useState<Substrate | null>(null);
  const [fed, setFed] = useState<Federation | null>(null);
  const [loading, setLoading] = useState(true);
  useEffect(() => {
    setLoading(true);
    // Preserve a fetch failure as `_error` instead of collapsing it to enabled:false — a 403 (can't read
    // the repo) or a 500 is not the same as "the operator didn't configure nostr".
    const load = (path: string) =>
      fetch(`${base}${path}`, { headers: authHeaders() })
        .then((r) => (r.ok ? r.json() : { _error: r.status }))
        .catch(() => ({ _error: 0 }));
    Promise.all([load("/substrate"), load("/federation")])
      .then(([s, f]) => { setSub(s); setFed(f); })
      .finally(() => setLoading(false));
  }, [tenant, repo]);

  if (loading) return <div className="py-16 text-center text-[13px] text-muted">reading the substrate…</div>;

  if (sub?._error != null) {
    return (
      <Card>
        <div className="px-6 py-10 grid gap-2 max-w-[640px]">
          <h2 className="text-[16px] font-semibold text-ink">Couldn't load the substrate</h2>
          <p className="text-[13.5px] text-muted">
            {sub._error === 403 ? "You don't have access to this repo's substrate." : sub._error === 0 ? "The request failed — check your connection and retry." : `The server returned an error (${sub._error}).`}
          </p>
        </div>
      </Card>
    );
  }

  if (!sub?.enabled) {
    return (
      <Card>
        <div className="px-6 py-10 grid gap-2 max-w-[640px]">
          <h2 className="text-[16px] font-semibold text-ink">Substrate not configured</h2>
          <p className="text-[13.5px] text-muted">
            This instance publishes refs and authorship provenance to nostr relays so a repo's history and its signed authorship live off this host, verifiable without trusting it. Set <code className="text-body">HULL_NOSTR_SECRET</code> and <code className="text-body">HULL_NOSTR_RELAYS</code> to enable it.
          </p>
        </div>
      </Card>
    );
  }

  const prov = sub.provenance ?? [];
  return (
    <div className="grid gap-6 max-w-[1000px]">
      {/* Branch ref, read back from relays */}
      <Card>
        <SectionHeader label="Branch ref" right={<span className="text-[12px] text-muted">from nostr</span>} />
        <div className="px-5 py-4">
          {sub.ref ? (
            <div className="flex items-baseline gap-3 text-[13px]">
              <span className="font-medium text-body">{sub.ref.branch}</span>
              <span className="text-muted">→</span>
              <code className="text-body tabular-nums" title={sub.ref.commit}>{short(sub.ref.commit, 20)}</code>
            </div>
          ) : (
            <p className="text-[13px] text-muted">no ref published for the default branch yet</p>
          )}
        </div>
      </Card>

      {/* Federation: what each trusted instance says this branch points at */}
      {fed?.enabled && (
        <Card>
          <SectionHeader
            label="Federation"
            right={<span className="text-[12px] text-muted">{fed.responded ?? 0}/{fed.expected ?? 0} instances</span>}
          />
          <div className="px-5 py-4 grid gap-3">
            {fed.diverged && (
              <div className="text-[12.5px] text-fault-text bg-fault-wash rounded-ctl px-3 py-2">
                Instances disagree on the commit for <b>{fed.branch}</b> — a fork, a stale mirror, or a rewritten ref.
              </div>
            )}
            {(fed.instances ?? []).length === 0 ? (
              <p className="text-[13px] text-muted">no instance has published this branch</p>
            ) : (
              <div className="grid gap-1.5">
                {(fed.instances ?? []).map((i) => (
                  <div key={i.instance} className="flex items-center gap-3 text-[13px]">
                    <code className="text-muted tabular-nums" title={i.instance}>{short(i.instance, 10)}</code>
                    {i.self && <Pill ok label="this instance" />}
                    <span className="text-muted">→</span>
                    <code className="text-body tabular-nums" title={i.commit}>{short(i.commit, 18)}</code>
                  </div>
                ))}
              </div>
            )}
            {(fed.missing ?? []).length > 0 && (
              <div className="text-[12px] text-muted">
                silent: {(fed.missing ?? []).map((m) => short(m, 10)).join(", ")}
              </div>
            )}
          </div>
        </Card>
      )}

      {/* Authorship provenance — signatures verified; accountability/authority resolved locally */}
      <Card>
        <SectionHeader label="Authorship provenance" right={<span className="text-[12px] text-muted">{prov.length}</span>} />
        {prov.length === 0 ? (
          <div className="px-5 py-4 text-[13px] text-muted">no provenance attestations on the relays yet</div>
        ) : (
          <div>
            {prov.map((p, i) => (
              <div key={`${p.change}:${i}`} className="px-5 py-3 border-t border-rule2 first:border-0 grid gap-1.5">
                <div className="flex items-center gap-2 flex-wrap">
                  <code className="text-body tabular-nums" title={p.change}>{short(p.change, 18)}</code>
                  <span className="text-muted text-[12.5px]">by</span>
                  <span className="text-[12.5px] font-medium text-body">{p.actor_handle || handleOf(p.actor)}</span>
                  {p.human_root && p.human_root !== p.actor && (
                    <span className="text-[12px] text-muted">(for {handleOf(p.human_root)})</span>
                  )}
                </div>
                {p.intent && <div className="text-[12.5px] text-muted truncate" title={p.intent}>{p.intent}</div>}
                <div className="flex items-center gap-1.5 flex-wrap">
                  <Pill ok={p.signatures_valid} label={p.signatures_valid ? "signatures valid" : "signatures invalid"} />
                  <Pill ok={p.accountable} warn={!p.accountable} label={p.accountable ? "accountable" : "not accountable"} />
                  <Pill ok={p.authorized} warn={!p.authorized} label={p.authorized ? "authorized" : "not a member"} />
                </div>
              </div>
            ))}
          </div>
        )}
      </Card>

      {/* Where this repo's state lives off-host */}
      <div className="grid gap-2 text-[12px] text-muted">
        {(sub.relays ?? []).length > 0 && <div>relays: {(sub.relays ?? []).join(", ")}</div>}
        {(sub.blob_servers ?? []).length > 0 && <div>blob servers: {(sub.blob_servers ?? []).join(", ")}</div>}
      </div>
    </div>
  );
}
