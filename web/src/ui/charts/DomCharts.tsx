// keel/hull DOM charts — Tailwind: Gauges, Heatmap, Composition.
// (Donut / stacked bars / area field / line are canvas engines; see engines.md.)
import { useState } from 'react';
import { ChartCard, RAMP, REST } from './ChartCard';

const cx = (...a: (string | false | null | undefined)[]) => a.filter(Boolean).join(' ');

// rows: [{ label, value (0-100) }], threshold: 0-100
export function Gauges({ rows, threshold = 85 }: { rows: { label: string; value: number }[]; threshold?: number }) {
  return (
    <div className="grid gap-3.5 mt-[18px]">
      {rows.map((r, i) => (
        <div key={r.label} className="grid grid-cols-[118px_1fr_44px] gap-2.5 items-center">
          <span className="text-[12.5px] text-[#5F6675] truncate">{r.label}</span>
          <div className="relative h-2 rounded bg-[#EEF0F3]">
            {/* width animates on a spring in the reference; transition-all is an acceptable port */}
            <div className="absolute inset-y-0 left-0 rounded transition-all duration-500 ease-out"
              style={{ width: `${r.value}%`, background: RAMP[i % RAMP.length] }} />
            <div className="absolute inset-y-0 w-0.5 rounded-full bg-[#171717]/30" style={{ left: `${threshold}%` }} />
          </div>
          <span className="text-xs font-semibold text-[#171717] text-right tabular-nums">{r.value}%</span>
        </div>
      ))}
    </div>
  );
}

// GitHub-style activity grid. levels: number[weeks][7], 0-4.
export function Heatmap({ levels, onHover }: { levels: number[][]; onHover?: (w: number, d: number) => void }) {
  const ramp = [REST, ...RAMP.slice(0, 4).reverse()];
  const [hov, setHov] = useState<[number, number] | null>(null);
  return (
    <div className="flex gap-[3px] mt-4">
      {levels.map((week, w) => (
        <div key={w} className="flex flex-col gap-[3px]">
          {week.map((lvl, d) => (
            <div key={d}
              onMouseEnter={() => { setHov([w, d]); onHover?.(w, d); }}
              onMouseLeave={() => setHov(null)}
              className={cx('w-[13px] h-[13px] rounded-[3px] transition-transform duration-150',
                hov && hov[0] === w && hov[1] === d && 'outline outline-1 outline-ink scale-110')}
              style={{ background: ramp[lvl] }} />
          ))}
        </div>
      ))}
    </div>
  );
}

// GitHub-style segmented breakdown line + legend chips. items: [{ name, pct }]
export function Composition({ title, items }: { title: React.ReactNode; items: { name: string; pct: number }[] }) {
  const [hov, setHov] = useState<number | null>(null);
  return (
    <div>
      <div className="text-[12.5px] font-semibold text-[#171717]">{title}</div>
      <div className="flex h-2.5 rounded-[5px] overflow-hidden gap-0.5 mt-2">
        {items.map((it, i) => (
          <div key={it.name} className="transition-opacity duration-150"
            style={{ width: `${it.pct}%`, background: RAMP[i % RAMP.length], opacity: hov != null && hov !== i ? 0.3 : 1 }} />
        ))}
      </div>
      <div className="flex flex-wrap gap-x-4 gap-y-1.5 mt-2.5" onMouseLeave={() => setHov(null)}>
        {items.map((it, i) => (
          <span key={it.name} onMouseEnter={() => setHov(i)}
            className={cx('inline-flex items-center gap-1.5 text-xs font-medium text-[#5F6675] cursor-default transition-opacity duration-150',
              hov != null && hov !== i && 'opacity-40')}>
            <span className="w-2 h-2 rounded-full flex-none" style={{ background: RAMP[i % RAMP.length] }} />
            {it.name} <span className="text-[#8A90A0] font-normal">{it.pct.toFixed(1)}%</span>
          </span>
        ))}
      </div>
    </div>
  );
}

// Example composition card:
export function CompositionCard() {
  return (
    <ChartCard title="Composition" context="acme/pipeline"
      icon={<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="#8A90A0" strokeWidth="2" strokeLinecap="round"><line x1="3" y1="12" x2="21" y2="12" /><circle cx="8" cy="12" r="2" fill="#8A90A0" stroke="none" /></svg>}>
      <div className="mt-4 grid gap-4">
        <Composition title="Languages" items={[
          { name: 'Rust', pct: 58.2 }, { name: 'TypeScript', pct: 24.1 }, { name: 'Go', pct: 9.4 },
          { name: 'Shell', pct: 5.1 }, { name: 'Other', pct: 3.2 }]} />
        <div className="border-t border-rule2" />
        <Composition title="Models" items={[
          { name: 'claude-4.6', pct: 46.0 }, { name: 'claude-haiku', pct: 27.5 }, { name: 'gpt-5', pct: 14.8 },
          { name: 'gemini-3', pct: 8.2 }, { name: 'human', pct: 3.5 }]} />
      </div>
    </ChartCard>
  );
}
