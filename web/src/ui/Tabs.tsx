// keel/hull selection system — Tailwind. ONE system, two orientations.
// Horizontal: detached 3px steel bar UNDER the active label.
// Vertical: detached 3px keel-line LEFT of a raised chip. Never an attached colored border.

const cx = (...a: (string | number | false | null | undefined)[]) => a.filter(Boolean).join(' ');

export function HTabs({ items, value, onChange }: {
  items: string[];
  value?: number;
  onChange?: (i: number) => void;
}) {
  return (
    <div className="flex gap-5 text-[13.5px]">
      {items.map((label, i) => (
        <span key={label} onClick={() => onChange?.(i)} className="flex flex-col gap-1 pt-1.5 cursor-pointer group">
          <span className={cx('transition-colors duration-150', value === i ? 'font-semibold text-ink' : 'text-dim group-hover:text-ink')}>{label}</span>
          <span className={cx('h-[3px] rounded-sm transition-colors duration-150', value === i ? 'bg-steel' : 'bg-transparent')} />
        </span>
      ))}
    </div>
  );
}

type RailItem = { label: string; count?: number | null };

// items: [{ label, count? }]
export function VRail({ items, value, onChange }: {
  items: RailItem[];
  value?: number;
  onChange?: (i: number) => void;
}) {
  return (
    <div className="grid gap-[3px] text-[13.5px]">
      {items.map((it, i) => (
        <div key={it.label} onClick={() => onChange?.(i)} className="flex items-stretch gap-[7px]">
          <div className={cx('w-[3px] rounded-sm flex-none transition-colors duration-150', value === i ? 'bg-steel' : 'bg-transparent')} />
          <div className={cx('flex-1 min-w-0 flex justify-between items-center gap-2 px-2.5 py-1.5 rounded-chip cursor-pointer transition-colors duration-150 border',
            value === i ? 'bg-surface border-rule2 font-medium text-ink' : 'border-transparent text-body hover:bg-surface')}>
            <span className="truncate">{it.label}</span>
            {it.count != null && <span className="text-xs text-muted tabular-nums">{it.count}</span>}
          </div>
        </div>
      ))}
    </div>
  );
}

export function Segmented({ items, value, onChange }: {
  items: string[];
  value?: number;
  onChange?: (i: number) => void;
}) {
  return (
    <div className="inline-flex h-ctl rounded-ctl overflow-hidden border border-ctl bg-paper">
      {items.map((label, i) => (
        <span key={label} onClick={() => onChange?.(i)}
          className={cx('text-[13px] px-3.5 flex items-center cursor-pointer select-none transition-colors duration-150',
            i && 'border-l border-ctl',
            value === i ? 'bg-surface text-ink font-semibold' : 'text-dim font-medium hover:bg-rule2 hover:text-body')}>
          {label}
        </span>
      ))}
    </div>
  );
}

// Chart-style pill period switcher (F3F4F6 track, sliding white thumb).
// Thumb slide: springy in the reference. transition-all with ease-out is an acceptable port.
export function PillSwitcher({ items, value, onChange }: {
  items: string[];
  value?: number;
  onChange?: (i: number) => void;
}) {
  return (
    <div className="relative inline-flex bg-[#F3F4F6] rounded-full p-[3px]">
      {items.map((label, i) => (
        <button key={label} onClick={() => onChange?.(i)}
          className={cx('relative z-[1] px-3.5 py-[5px] rounded-full text-xs cursor-pointer transition-all duration-200 ease-out',
            value === i
              ? 'font-semibold text-[#171717] bg-white shadow-[0_0_0_1px_rgba(25,28,33,0.05),0_1px_2px_rgba(25,28,33,0.08)]'
              : 'font-medium text-[#8A90A0]')}>
          {label}
        </button>
      ))}
    </div>
  );
}
