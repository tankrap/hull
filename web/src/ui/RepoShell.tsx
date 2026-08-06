// keel/hull RepoShell — Tailwind. The hull repo page frame: top bar (mark, breadcrumb,
// nav tabs), left sidebar (VRail filters + fusion stats), content slot.
import { HTabs, VRail } from './Tabs';
import { SearchInput } from './Field';
import { Button } from './Button';

export function RepoShell({ org, repo, nav, onNav, filters, filter, onFilter, stats, children }: {
  org?: React.ReactNode;
  repo?: React.ReactNode;
  nav: string[];
  onNav?: (i: number) => void;
  filters: { label: string; count?: number | null }[];
  filter?: number;
  onFilter?: (i: number) => void;
  stats?: string[];
  children?: React.ReactNode;
}) {
  return (
    <div className="border border-rule bg-surface rounded-card overflow-hidden">
      {/* top bar */}
      <div className="flex items-center gap-4 px-5 h-[52px] border-b border-rule2">
        <div className="flex items-center gap-2.5">
          <div className="w-5 h-5 rounded-chip bg-brass" /> {/* hull mark slot */}
          <span className="text-[19px] font-extrabold tracking-tight">hull</span>
        </div>
        <div className="text-[13px] flex gap-1.5 tabular-nums">
          <span className="text-faint">{org}</span><span className="text-rule">/</span>
          <span className="font-medium">{repo}</span>
        </div>
        <div className="ml-auto">
          {/* original source had a duplicate `value` prop; object-spread last-wins made the
              effective value exactly `onNav ? undefined : 0` — preserved verbatim here. */}
          <HTabs items={nav} value={onNav ? undefined : 0} />
        </div>
      </div>
      <div className="grid grid-cols-[210px_1fr] min-h-[260px]">
        {/* sidebar */}
        <div className="border-r border-rule2 px-4 py-[18px] bg-paper">
          <div className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted mb-3">Filter</div>
          <VRail items={filters} value={filter} onChange={onFilter} />
          {stats && (
            <>
              <div className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted mt-[22px] mb-3">Fusion</div>
              <div className="text-[12.5px] leading-[1.9] text-dim tabular-nums">
                {stats.map((s) => <div key={s}>{s}</div>)}
              </div>
            </>
          )}
        </div>
        {/* content */}
        <div>
          <div className="flex items-center gap-2.5 px-5 py-3 border-b border-rule2">
            <div className="flex-1"><SearchInput placeholder="author:agent state:open" shortcut="⌘K" /></div>
            <Button size="sm">New voyage</Button>
          </div>
          {children}
        </div>
      </div>
    </div>
  );
}

// NOTE: wire HTabs value/onChange to your router. Kept minimal here on purpose:
// the shell is layout, the tabs/rail components own the selection visuals.
