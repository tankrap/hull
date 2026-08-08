// keel/hull overlays — Tailwind. Modal (blocking, destructive confirm only),
// Dialog (one decision, two buttons, light scrim), Drawer (right edge only).
// All dismiss on backdrop click. Entrances: bd-in / ov-in / dw-in (tailwind.config).
import { useState, useEffect } from 'react';
import { Button } from './Button';
import { TextField } from './Field';

const cx = (...a: (string | false | null | undefined)[]) => a.filter(Boolean).join(' ');

function Backdrop({ onClose, strength = 'bg-[rgba(0,0,0,0.68)]', z = 'z-40' }: {
  onClose?: () => void;
  strength?: string;
  z?: string;
}) {
  return <div onClick={onClose} className={cx('fixed inset-0 animate-bd-in', z, strength)} />;
}

// Destructive confirm: action disabled until typed id matches confirmId.
export function ConfirmModal({ open, onClose, title, body, confirmId, actionLabel, onConfirm }: {
  open?: boolean;
  onClose?: () => void;
  title?: React.ReactNode;
  body?: React.ReactNode;
  confirmId?: string;
  actionLabel?: React.ReactNode;
  onConfirm: () => void;
}) {
  const [typed, setTyped] = useState('');
  if (!open) return null;
  const ok = typed.trim() === confirmId;
  return (
    <>
      <Backdrop onClose={onClose} />
      <div role="dialog" aria-modal="true" onClick={(e) => e.stopPropagation()}
        className="fixed left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 z-50 w-[420px] max-w-[90vw] bg-surface border border-rule rounded-card shadow-modal p-4 animate-ov-in">
        <div className="text-[14.5px] font-semibold">{title}</div>
        <div className="text-[12.5px] text-dim leading-[1.55] mt-1.5">{body}</div>
        <div className="mt-3">
          <TextField placeholder={`type ${confirmId} to confirm`} value={typed} onChange={(e) => setTyped(e.target.value)} />
        </div>
        <div className="flex justify-end gap-2 mt-3.5">
          <Button size="sm" variant="secondary" onClick={onClose}>Cancel</Button>
          <Button size="sm" disabled={!ok} onClick={() => { onConfirm(); setTyped(''); }}
            className={ok ? '!bg-fault !border-fault !text-white' : ''}>{actionLabel}</Button>
        </div>
      </div>
    </>
  );
}

// One decision, two buttons. Never a form inside.
export function Dialog({ open, onClose, icon, title, body, cancelLabel = 'Cancel', actionLabel, onAction }: {
  open?: boolean;
  onClose?: () => void;
  icon?: React.ReactNode;
  title?: React.ReactNode;
  body?: React.ReactNode;
  cancelLabel?: React.ReactNode;
  actionLabel?: React.ReactNode;
  onAction?: () => void;
}) {
  if (!open) return null;
  return (
    <>
      <Backdrop onClose={onClose} strength="bg-[rgba(0,0,0,0.55)]" z="z-[65]" />
      <div role="dialog" aria-modal="true" onClick={(e) => e.stopPropagation()}
        className="fixed left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 z-[70] w-[400px] max-w-[90vw] bg-surface border border-rule rounded-card shadow-modal p-4 animate-ov-in">
        <div className="flex gap-2.5 items-start">
          {icon && <div className="w-7 h-7 rounded-ctl bg-steel-wash flex items-center justify-center flex-none">{icon}</div>}
          <div>
            <div className="text-sm font-semibold">{title}</div>
            <div className="text-[12.5px] text-dim leading-[1.55] mt-1">{body}</div>
          </div>
        </div>
        <div className="flex justify-end gap-2 mt-3.5">
          {cancelLabel !== null && <Button size="sm" variant="secondary" onClick={onClose}>{cancelLabel}</Button>}
          <Button size="sm" onClick={onAction}>{actionLabel}</Button>
        </div>
      </div>
    </>
  );
}

type ValidateResult = { available?: boolean; hint?: string };

// A single text input in a modal (replaces window.prompt). Sanitizes on type and, if `validate` is
// given, shows live availability. `validate(value) -> { available, hint }` (async).
export function PromptModal({ open, title, label, placeholder, initial = '', sanitize, validate, optional = false, confirmLabel = 'Confirm', onCancel, onConfirm }: {
  open?: boolean;
  title?: React.ReactNode;
  label?: React.ReactNode;
  placeholder?: string;
  initial?: string;
  sanitize?: (s: string) => string;
  validate?: (v: string) => Promise<ValidateResult>;
  optional?: boolean;
  confirmLabel?: React.ReactNode;
  onCancel: () => void;
  onConfirm: (value: string) => void;
}) {
  const [val, setVal] = useState(initial);
  const [status, setStatus] = useState<ValidateResult | null>(null);
  const [checking, setChecking] = useState(false);
  useEffect(() => { if (open) { setVal(initial); setStatus(null); setChecking(false); } }, [open]);
  useEffect(() => {
    if (!open || !validate) { setStatus(null); return; }
    const v = val.trim();
    if (!v) { setStatus(null); setChecking(false); return; }
    let cancelled = false;
    setChecking(true);
    const t = setTimeout(async () => {
      try { const s = await validate(v); if (!cancelled) setStatus(s); }
      catch { if (!cancelled) setStatus(null); }
      finally { if (!cancelled) setChecking(false); }
    }, 300);
    return () => { cancelled = true; clearTimeout(t); };
  }, [val, open]);
  if (!open) return null;
  const blocked = !!validate && status && status.available === false;
  const ok = (optional || val.trim().length > 0) && !blocked;
  const submit = () => { if (ok) onConfirm(val.trim()); };
  return (
    <>
      <Backdrop onClose={onCancel} strength="bg-[rgba(0,0,0,0.55)]" z="z-[65]" />
      <div role="dialog" aria-modal="true" onClick={(e) => e.stopPropagation()}
        className="fixed left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 z-[70] w-[420px] max-w-[90vw] bg-surface border border-rule rounded-card shadow-modal p-4 animate-ov-in">
        <div className="text-[14.5px] font-semibold">{title}</div>
        <div className="mt-3">
          {label && <div className="text-[12.5px] font-semibold text-body mb-1.5">{label}</div>}
          <input autoFocus value={val} placeholder={placeholder}
            onChange={(e) => setVal(sanitize ? sanitize(e.target.value) : e.target.value)}
            onKeyDown={(e) => { if (e.key === 'Enter') submit(); if (e.key === 'Escape') onCancel(); }}
            className={cx('w-full box-border h-ctl px-2.5 rounded-ctl border bg-paper font-sans text-[13.5px] text-ink outline-none transition-colors duration-150', blocked ? 'border-fault' : 'border-ctl focus:border-body')} />
          {validate && val.trim() && (
            <div className={cx('text-[12px] mt-1.5', checking ? 'text-muted' : status?.available ? 'text-clear-text' : 'text-fault-text')}>
              {checking ? 'checking…' : status ? (status.available ? `✓ ${val.trim()} is available` : `✗ ${status.hint || 'taken'}`) : ''}
            </div>
          )}
        </div>
        <div className="flex justify-end gap-2 mt-4">
          <Button size="sm" variant="secondary" onClick={onCancel}>Cancel</Button>
          <Button size="sm" disabled={!ok} onClick={submit}>{confirmLabel}</Button>
        </div>
      </div>
    </>
  );
}

// Inspect without losing the list. Right edge, never bottom.
export function Drawer({ open, onClose, title, children, footer }: {
  open?: boolean;
  onClose?: () => void;
  title?: React.ReactNode;
  children?: React.ReactNode;
  footer?: React.ReactNode;
}) {
  if (!open) return null;
  return (
    <>
      <Backdrop onClose={onClose} strength="bg-[rgba(0,0,0,0.55)]" />
      <div role="dialog" aria-modal="true" onClick={(e) => e.stopPropagation()}
        className="fixed right-0 top-0 bottom-0 z-50 w-[380px] max-w-[85vw] bg-surface border-l border-rule2 shadow-drawer flex flex-col animate-dw-in">
        <div className="flex justify-between items-center px-3.5 py-3 border-b border-rule2">
          <span className="text-[13.5px] font-semibold">{title}</span>
          <span onClick={onClose} className="text-base text-muted cursor-pointer px-1 hover:text-ink">×</span>
        </div>
        <div className="px-3.5 py-3 grid gap-[9px] text-[12.5px] overflow-y-auto">{children}</div>
        {footer && <div className="mt-auto px-3.5 py-3 border-t border-rule2">{footer}</div>}
      </div>
    </>
  );
}

// Key-value row for drawer bodies
export function DrawerRow({ k, v, accent }: { k: React.ReactNode; v: React.ReactNode; accent?: string }) {
  return (
    <div className="flex justify-between">
      <span className="text-muted">{k}</span>
      <span className={cx('font-medium', accent)}>{v}</span>
    </div>
  );
}
