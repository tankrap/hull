// keel/hull badges, tags & chips — Tailwind, matched to GitLab's gl-badge:
// 12px / weight-400 / NORMAL case, 2px×6px, ~4px radius. Light = a tint-100 fill + tint-700 text;
// dark = a BRIGHT tint-300 fill + near-black tint-950 text (per-theme --badge-* tokens).
// Color never carries state alone (the label word does).

const cx = (...a: (string | false | null | undefined)[]) => a.filter(Boolean).join(' ');

const LOZENGE = 'inline-flex items-center gap-1 text-[12px] font-normal leading-4 px-1.5 py-[2px] rounded-badge whitespace-nowrap max-w-[220px] truncate';

const V = {
  success: 'bg-[var(--badge-success-bg)] text-[var(--badge-success-fg)]',
  danger: 'bg-[var(--badge-danger-bg)] text-[var(--badge-danger-fg)]',
  warning: 'bg-[var(--badge-warning-bg)] text-[var(--badge-warning-fg)]',
  info: 'bg-[var(--badge-info-bg)] text-[var(--badge-info-fg)]',
  neutral: 'bg-[var(--badge-neutral-bg)] text-[var(--badge-neutral-fg)]',
};

const STATUS: Record<string, string> = {
  // CI / run state
  passed: V.success,
  failed: V.danger,
  running: V.warning,
  queued: V.neutral,
  // issue / MR lifecycle — GitLab: open=green, merged=blue, closed=neutral
  open: V.success,
  merged: V.info,
  closed: V.neutral,
  // actor kind
  agent: V.info,
  human: V.neutral,
  verified: V.success,
};

// kind: passed | failed | running | queued | open | merged | closed | agent | human | verified
export function StatusBadge({ kind, children }: { kind: string; children?: React.ReactNode }) {
  return <span className={cx(LOZENGE, STATUS[kind] ?? V.neutral)}>{children ?? kind}</span>;
}

// Operation-kind lozenges for the diff rail (rename/signature/behavior/chart)
const OP: Record<string, string> = {
  rename: V.info,
  signature: V.warning,
  behavior: V.danger,
  chart: V.success,
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

// GitLab "muted" badge — a subtle neutral count/category chip (mixed-case).
export function Tag({ children }: { children?: React.ReactNode }) {
  return (
    <span className="inline-flex items-center text-[12px] font-normal leading-4 px-1.5 py-[2px] rounded-badge bg-rule2 text-dim">
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
