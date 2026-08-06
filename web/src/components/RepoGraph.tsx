// Graph tab: the codebase as a force-directed import graph — extracted verbatim from App.tsx.
import { useState, useEffect, useMemo } from "react";
import { SearchInput } from "../ui/Field";
import { Picker } from "./menus";
import { Card } from "./primitives";

type GNode = { path: string; dir: string; lang: string; size: number; deg: number };
type GEdge = { from: string; to: string };
const LANG_COLOR: Record<string, string> = { rust: "#dea584", ts: "#3178c6", js: "#f1e05a", python: "#3572A5", go: "#00ADD8" };
export function RepoGraph({ tenant, repo, authHeaders, onOpenFile }: { tenant: string; repo: string; authHeaders: () => Record<string, string>; onOpenFile: (path: string, branch: string) => void }) {
  const base = `/api/repos/${encodeURIComponent(tenant)}/${repo}`;
  const [branch, setBranch] = useState("main");
  const [branches, setBranches] = useState<string[]>([]);
  const [data, setData] = useState<{ nodes: GNode[]; edges: GEdge[] } | null>(null);
  const [loading, setLoading] = useState(true);
  const [q, setQ] = useState("");
  const [hover, setHover] = useState<string | null>(null);
  useEffect(() => { fetch(`${base}/branches`, { headers: authHeaders() }).then((r) => r.json()).then((d) => { const bs = d.branches ?? []; setBranches(bs); setBranch((b) => (bs.length && !bs.includes(b) ? bs[0] : b)); }).catch(() => {}); }, [tenant, repo]);
  useEffect(() => {
    setLoading(true);
    fetch(`${base}/graph?ref=${encodeURIComponent(branch)}`, { headers: authHeaders() }).then((r) => r.json()).then((d) => setData({ nodes: d.nodes ?? [], edges: d.edges ?? [] })).catch(() => setData({ nodes: [], edges: [] })).finally(() => setLoading(false));
  }, [branch, tenant, repo]);

  // Deterministic force-directed layout (seeded on a circle by index; no RNG). O(n²)·iters, capped.
  const W = 900, H = 560;
  const laid = useMemo(() => {
    const nodes = data?.nodes ?? []; const edges = data?.edges ?? [];
    const n = nodes.length; if (n === 0) return { pos: [] as { x: number; y: number }[], vb: `0 0 ${W} ${H}` };
    const idx = new Map(nodes.map((nd, i) => [nd.path, i]));
    const E = edges.map((e) => [idx.get(e.from), idx.get(e.to)]).filter(([a, b]) => a != null && b != null) as [number, number][];
    const pos = nodes.map((_, i) => { const a = (i / n) * Math.PI * 2; return { x: W / 2 + Math.cos(a) * 200, y: H / 2 + Math.sin(a) * 160, vx: 0, vy: 0 }; });
    const iters = n > 400 ? 120 : n > 150 ? 220 : 320;
    for (let it = 0; it < iters; it++) {
      for (let i = 0; i < n; i++) for (let j = i + 1; j < n; j++) { const dx = pos[i].x - pos[j].x, dy = pos[i].y - pos[j].y; const d2 = dx * dx + dy * dy + 0.01; const d = Math.sqrt(d2); const f = 2600 / d2; pos[i].vx += (dx / d) * f; pos[i].vy += (dy / d) * f; pos[j].vx -= (dx / d) * f; pos[j].vy -= (dy / d) * f; }
      for (const [a, b] of E) { const dx = pos[b].x - pos[a].x, dy = pos[b].y - pos[a].y; const d = Math.sqrt(dx * dx + dy * dy) + 0.01; const f = (d - 90) * 0.02; pos[a].vx += (dx / d) * f; pos[a].vy += (dy / d) * f; pos[b].vx -= (dx / d) * f; pos[b].vy -= (dy / d) * f; }
      for (let i = 0; i < n; i++) { pos[i].vx += (W / 2 - pos[i].x) * 0.006; pos[i].vy += (H / 2 - pos[i].y) * 0.006; pos[i].x += pos[i].vx * 0.85; pos[i].y += pos[i].vy * 0.85; pos[i].vx *= 0.82; pos[i].vy *= 0.82; }
    }
    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    for (const p of pos) { minX = Math.min(minX, p.x); minY = Math.min(minY, p.y); maxX = Math.max(maxX, p.x); maxY = Math.max(maxY, p.y); }
    const pad = 40;
    return { pos, vb: `${minX - pad} ${minY - pad} ${maxX - minX + pad * 2} ${maxY - minY + pad * 2}` };
  }, [data]);

  const nodes = data?.nodes ?? []; const edges = data?.edges ?? [];
  const ql = q.trim().toLowerCase();
  const match = (p: string) => ql !== "" && p.toLowerCase().includes(ql);
  const anyMatch = ql !== "" && nodes.some((n) => match(n.path));
  const posOf = new Map(nodes.map((n, i) => [n.path, laid.pos[i]]));
  const base2 = (p: string) => p.split("/").pop() ?? p;

  return (
    <div className="grid gap-4">
      <div className="flex items-center gap-3 flex-wrap">
        <div className="min-w-[220px]"><Picker value={branch} onChange={setBranch} options={branches.map((b) => ({ value: b, label: b }))} placeholder="branch" width={340} size="sm" block searchable /></div>
        <span className="text-[12.5px] text-muted">{nodes.length} files · {edges.length} imports</span>
        <div className="flex-1 min-w-[220px] max-w-[420px] ml-auto"><SearchInput placeholder="Search the graph…" shortcut="" value={q} onChange={(e: React.ChangeEvent<HTMLInputElement>) => setQ(e.target.value)} /></div>
      </div>
      <Card>
        <div className="relative">
          {loading && <div className="absolute inset-0 grid place-items-center text-[13px] text-muted z-10">Building graph…</div>}
          {!loading && nodes.length === 0 && <div className="py-16 text-center text-[13px] text-muted">No source files with resolvable imports on this branch.</div>}
          {nodes.length > 0 && (
            <svg viewBox={laid.vb} className="w-full" style={{ height: 620 }} preserveAspectRatio="xMidYMid meet">
              {edges.map((e, i) => { const a = posOf.get(e.from), b = posOf.get(e.to); if (!a || !b) return null; const dim = anyMatch && !(match(e.from) || match(e.to)); return <line key={i} x1={a.x} y1={a.y} x2={b.x} y2={b.y} stroke="currentColor" className={dim ? "text-rule3" : "text-rule"} strokeWidth={1} opacity={dim ? 0.3 : 0.7} />; })}
              {nodes.map((n, i) => {
                const p = laid.pos[i]; if (!p) return null;
                const r = 4 + Math.min(9, n.deg * 1.4);
                const on = !anyMatch || match(n.path);
                const showLabel = hover === n.path || n.deg >= 4 || match(n.path);
                return (
                  <g key={n.path} transform={`translate(${p.x} ${p.y})`} onMouseEnter={() => setHover(n.path)} onMouseLeave={() => setHover(null)} onClick={() => onOpenFile(n.path, branch)} style={{ cursor: "pointer" }} opacity={on ? 1 : 0.25}>
                    <circle r={r} fill={LANG_COLOR[n.lang] || "#8b949e"} stroke={match(n.path) ? "#111827" : "#ffffff"} strokeWidth={match(n.path) ? 2 : 1} />
                    {showLabel && <text x={r + 3} y={4} className="fill-body" style={{ fontSize: 11, paintOrder: "stroke", stroke: "var(--surface)", strokeWidth: 3 }}>{base2(n.path)}</text>}
                    <title>{n.path} · {n.deg} imports</title>
                  </g>
                );
              })}
            </svg>
          )}
        </div>
      </Card>
      <div className="flex items-center gap-4 text-[12px] text-muted flex-wrap">
        {Object.entries(LANG_COLOR).map(([l, c]) => nodes.some((n) => n.lang === l) && <span key={l} className="inline-flex items-center gap-1.5"><span className="w-2.5 h-2.5 rounded-full" style={{ background: c }} />{l}</span>)}
        <span className="text-faint">· node size = how many files import it · drag-free force layout</span>
      </div>
    </div>
  );
}
