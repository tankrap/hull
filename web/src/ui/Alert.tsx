// keel/hull inline alerts — Tailwind. Tinted ground + matching border, icon at text
// size, bold lead-in, max ONE action, no dismiss on facts.
import { Button } from './Button';

type AlertKind = 'info' | 'success' | 'warning' | 'error';

const KINDS: Record<AlertKind, { box: string; fg: string }> = {
  info: { box: 'bg-steel-wash border-steel/25', fg: 'text-steel-text' },
  success: { box: 'bg-clear-wash border-clear/30', fg: 'text-clear-text' },
  warning: { box: 'bg-brass-wash border-brass/35', fg: 'text-brass-text' },
  error: { box: 'bg-fault-wash border-fault/30', fg: 'text-fault-text' },
};

const ICONS: Record<AlertKind, React.ReactNode> = {
  info: <><circle cx="12" cy="12" r="10" /><line x1="12" y1="16" x2="12" y2="12" /><line x1="12" y1="8" x2="12.01" y2="8" /></>,
  success: <><path d="M22 11.08V12a10 10 0 1 1-5.93-9.14" /><polyline points="22 4 12 14.01 9 11.01" /></>,
  warning: <><path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z" /><line x1="12" y1="9" x2="12" y2="13" /><line x1="12" y1="17" x2="12.01" y2="17" /></>,
  error: <><circle cx="12" cy="12" r="10" /><line x1="15" y1="9" x2="9" y2="15" /><line x1="9" y1="9" x2="15" y2="15" /></>,
};

export function Alert({ kind = 'info', lead, children, action, onAction }: {
  kind?: AlertKind;
  lead?: React.ReactNode;
  children?: React.ReactNode;
  action?: React.ReactNode;
  onAction?: () => void;
}) {
  const k = KINDS[kind];
  return (
    <div className={`flex items-start gap-2.5 px-3.5 py-3 rounded-[10px] border ${k.box}`}>
      <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"
        strokeLinecap="round" strokeLinejoin="round" className={`flex-none mt-px ${k.fg}`}>{ICONS[kind]}</svg>
      <div className="text-[13.5px] leading-[1.55] text-body flex-1"><b>{lead}</b> {children}</div>
      {action && (
        <Button size="sm" variant="secondary" onClick={onAction}
          className={`!bg-transparent font-semibold flex-none ${k.fg}`}>{action}</Button>
      )}
    </div>
  );
}
