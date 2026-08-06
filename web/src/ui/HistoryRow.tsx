// keel/hull HistoryRow — Tailwind. Flat list row (no card). Hover swaps trailing
// +/- metadata for an animated open-link icon. Clickable only because it opens a detail.
import { StatusBadge, DiffStat, Tag } from './Badge';

type Voyage = {
  title: React.ReactNode;
  tag?: React.ReactNode;
  status?: string;
  id: React.ReactNode;
  crates: React.ReactNode;
  files: React.ReactNode;
  author: React.ReactNode;
  when: React.ReactNode;
  add: React.ReactNode;
  del: React.ReactNode;
};

export function HistoryRow({ voyage, onOpen }: { voyage: Voyage; onOpen?: () => void }) {
  return (
    <div onClick={onOpen}
      className="group px-3.5 py-3 border-b border-rule2 flex gap-2.5 items-start cursor-pointer transition-colors duration-150 hover:bg-paper">
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-2.5">
          <span className="text-[15px] font-medium">{voyage.title}</span>
          {voyage.tag && <Tag>{voyage.tag}</Tag>}
          {voyage.status && <StatusBadge kind={voyage.status} />}
        </div>
        <div className="text-xs text-muted mt-1.5 flex gap-3.5 flex-wrap tabular-nums">
          <span>{voyage.id}</span><span>{voyage.crates} crates</span><span>{voyage.files} files</span>
          <span>{voyage.author}</span><span>{voyage.when}</span>
        </div>
      </div>
      {/* trailing swap: crossfade + 6px x-shift, 180ms ease-out */}
      <div className="relative flex-none h-5 min-w-[84px]">
        <div className="absolute right-0 top-0 transition-all duration-[180ms] ease-out opacity-100 group-hover:opacity-0 group-hover:-translate-x-1.5">
          <DiffStat add={voyage.add} del={voyage.del} />
        </div>
        <div className="absolute right-0 top-0 transition-all duration-[180ms] ease-out opacity-0 translate-x-1.5 group-hover:opacity-100 group-hover:translate-x-0">
          <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"
            strokeLinecap="round" strokeLinejoin="round" className="text-steel-text">
            <path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" />
            <polyline points="15 3 21 3 21 9" /><line x1="10" y1="14" x2="21" y2="3" />
          </svg>
        </div>
      </div>
    </div>
  );
}
