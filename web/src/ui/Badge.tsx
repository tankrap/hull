// keel/hull badges, tags & chips — Tailwind.
// Atlassian-style lozenges: compact rounded rectangles (3px), 11px BOLD UPPERCASE, subtle tinted
// ground + accessible text, no border. Color never carries state alone (the label word does).

const cx = (...a: (string | false | null | undefined)[]) => a.filter(Boolean).join(' ');

const LOZENGE = 'inline-flex items-center text-[11px] font-bold uppercase tracking-[0.03em] leading-none px-1.5 py-[3px] rounded-badge whitespace-nowrap max-w-[200px] truncate';

const STATUS: Record<string, string> = {
  passed: 'bg-clear-wash text-clear-text',
  failed: 'bg-fault-wash text-fault-text',
  running: 'bg-brass-wash text-brass-text',
  queued: 'bg-rule2 text-dim',
  agent: 'bg-steel-wash text-steel-text',
  human: 'bg-rule2 text-dim',
  verified: 'bg-ink text-surface',
};

// kind: passed | failed | running | queued | agent | human | verified
export function StatusBadge({ kind, children }: { kind: string; children?: React.ReactNode }) {
  return <span className={cx(LOZENGE, STATUS[kind])}>{children ?? kind}</span>;
}

// Operation-kind lozenges for the diff rail (rename/signature/behavior/chart)
const OP: Record<string, string> = {
  rename: 'bg-steel-wash text-steel-text',
  signature: 'bg-brass-wash text-brass-text',
  behavior: 'bg-fault-wash text-fault-text',
  chart: 'bg-clear-wash text-clear-text',
};
export function OpBadge({ kind }: { kind: string }) {
  return <span className={cx(LOZENGE, OP[kind])}>{kind}</span>;
}

// Voyage / crate id chip — a subtle code chip, mixed-case + tabular (not a lozenge)
export function IdChip({ children }: { children?: React.ReactNode }) {
  return (
    <span className="inline-flex items-center text-xs px-2 py-1 rounded-chip bg-paper border border-rule text-body tabular-nums">
      {children}
    </span>
  );
}

// Neutral count/category tag — subtle neutral, mixed-case (counts read better un-capsed)
export function Tag({ children }: { children?: React.ReactNode }) {
  return (
    <span className="inline-flex items-center text-[11.5px] font-semibold px-1.5 py-[2px] rounded-badge bg-rule2 text-dim">
      {children}
    </span>
  );
}

// +N / −N line stats
export function DiffStat({ add, del }: { add: React.ReactNode; del: React.ReactNode }) {
  return (
    <span className="inline-flex gap-2 text-xs font-semibold tabular-nums">
      <span className="text-clear-text">+{add}</span>
      <span className="text-fault-text">−{del}</span>
    </span>
  );
}
