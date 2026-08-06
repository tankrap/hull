// Contribution heatmap (GitHub-style calendar) — extracted verbatim from App.tsx.
import { useState } from "react";

export type HeatDay = { day: number; human: number; agent: number };
const HEAT_EMPTY = "var(--rule2)"; // theme-aware neutral for days with no contributions
const HEAT_YOU = [HEAT_EMPTY, "#9be9a8", "#40c463", "#30a14e", "#216e39"]; // green
const HEAT_AGENT = [HEAT_EMPTY, "#a9c9f5", "#5a9bd4", "#2f6fb0", "#1b4d80"]; // blue
export function ContributionHeatmap({ days }: { days: HeatDay[] }) {
  const byDay = new Map(days.map((d) => [d.day, d]));
  const today = Math.floor(Date.now() / 86_400_000);
  const start = today - 371;
  const dow = (e: number) => ((e % 7) + 4) % 7; // epoch day 0 = Thursday; 0=Sun … 6=Sat
  const gridStart = start - dow(start);
  const maxH = Math.max(1, ...days.map((d) => d.human));
  const maxA = Math.max(1, ...days.map((d) => d.agent));
  const lvl = (c: number, m: number) => (c <= 0 ? 0 : c <= m * 0.25 ? 1 : c <= m * 0.5 ? 2 : c <= m * 0.75 ? 3 : 4);
  const weeks: number[][] = [];
  for (let w = 0; gridStart + w * 7 <= today; w++) {
    const col: number[] = [];
    for (let d = 0; d < 7; d++) { const e = gridStart + w * 7 + d; col.push(e >= start && e <= today ? e : -1); }
    weeks.push(col);
  }
  const fmtFull = (e: number) => new Date(e * 86_400_000).toLocaleDateString(undefined, { weekday: "short", month: "short", day: "numeric", year: "numeric" });
  // Hover reveals that day's specifics in a small cursor-anchored tooltip.
  const [hover, setHover] = useState<{ x: number; y: number; e: number; h: number; a: number } | null>(null);
  return (
    <div className="grid gap-2">
      <div className="flex gap-[2px] w-full">
        {weeks.map((col, i) => (
          <div key={i} className="grid gap-[2px] flex-1 min-w-0 content-start">
            {col.map((e, j) => {
              if (e < 0) return <span key={j} className="aspect-square" />;
              const d = byDay.get(e); const h = d?.human ?? 0; const a = d?.agent ?? 0;
              const bg = h === 0 && a === 0 ? HEAT_EMPTY : `linear-gradient(to bottom right, ${HEAT_YOU[lvl(h, maxH)]} 0 50%, ${HEAT_AGENT[lvl(a, maxA)]} 50% 100%)`;
              return <span key={j}
                onMouseEnter={(ev) => setHover({ x: ev.clientX, y: ev.clientY, e, h, a })}
                onMouseMove={(ev) => setHover((prev) => (prev && prev.e === e ? { ...prev, x: ev.clientX, y: ev.clientY } : { x: ev.clientX, y: ev.clientY, e, h, a }))}
                onMouseLeave={() => setHover((prev) => (prev && prev.e === e ? null : prev))}
                className="aspect-square rounded-[2px] hover:ring-2 hover:ring-ink/25 transition-shadow" style={{ background: bg }} />;
            })}
          </div>
        ))}
      </div>
      <div className="flex items-center gap-4 text-[11.5px] text-muted self-end">
        <span className="inline-flex items-center gap-1.5"><span className="w-3 h-3 rounded-[2px]" style={{ background: HEAT_YOU[3] }} />humans</span>
        <span className="inline-flex items-center gap-1.5"><span className="w-3 h-3 rounded-[2px]" style={{ background: HEAT_AGENT[3] }} />agents</span>
      </div>
      {hover && (
        <div className="fixed z-[80] pointer-events-none -translate-x-1/2 -translate-y-full" style={{ left: hover.x, top: hover.y - 10 }}>
          <div className="rounded-ctl-sm bg-surface border border-rule2 shadow-modal px-2.5 py-1.5 text-[11.5px] whitespace-nowrap">
            <div className="font-semibold text-body">{hover.h + hover.a === 0 ? "No contributions" : `${hover.h + hover.a} contribution${hover.h + hover.a === 1 ? "" : "s"}`}</div>
            {hover.h + hover.a > 0 && (
              <div className="mt-0.5 flex items-center gap-2 text-muted">
                <span className="inline-flex items-center gap-1"><span className="w-2 h-2 rounded-[2px]" style={{ background: HEAT_YOU[3] }} />{hover.h} human{hover.h === 1 ? "" : "s"}</span>
                <span className="inline-flex items-center gap-1"><span className="w-2 h-2 rounded-[2px]" style={{ background: HEAT_AGENT[3] }} />{hover.a} agents</span>
              </div>
            )}
            <div className={`text-faint ${hover.h + hover.a > 0 ? "mt-0.5" : "mt-0.5"}`}>{fmtFull(hover.e)}</div>
          </div>
        </div>
      )}
    </div>
  );
}
