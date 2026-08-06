// keel/hull buttons — Tailwind. Every clickable presses: active:translate-y-px active:scale-[0.99].
import React, { useEffect, useRef, useState } from 'react';

const cx = (...a) => a.filter(Boolean).join(' ');

// Every button: centered content, consistent icon spacing, a soft press, and a smooth hover.
const BASE = 'inline-flex items-center justify-center gap-1.5 font-medium cursor-pointer select-none whitespace-nowrap';
const PRESS = 'transition-[transform,box-shadow,background-color,border-color,color,filter] duration-150 active:translate-y-px active:scale-[0.99]';

const VARIANT = {
  // Solid ink with a little lift — a subtle shadow gives it presence; hover brightens, press flattens.
  primary: 'bg-ink text-surface border border-ink font-semibold shadow-[0_1px_2px_rgba(15,23,42,0.14)] hover:brightness-[1.12] hover:shadow-[0_3px_10px_-3px_rgba(15,23,42,0.32)] active:shadow-none',
  // ADS default: translucent-neutral fill with a hairline for crispness.
  secondary: 'bg-ink/[0.05] text-body border border-ink/[0.07] hover:bg-ink/[0.09] hover:border-ink/[0.11] active:bg-ink/[0.15]',
  ghost: 'bg-transparent text-steel-text border border-transparent hover:bg-steel-wash active:bg-steel-wash/80',
  destructive: 'bg-fault/[0.08] text-fault-text border border-fault/[0.12] hover:bg-fault/[0.15] hover:border-fault/20 active:bg-fault/20',
};

const SIZE = {
  sm: 'h-ctl-sm px-2.5 text-[12.5px] rounded-ctl-sm',
  md: 'h-ctl px-3.5 text-sm rounded-ctl',
  lg: 'h-ctl-lg px-[18px] text-[15px] rounded-[9px]',
};

export function Button({ variant = 'primary', size = 'md', className, disabled, children, ...rest }) {
  if (disabled) {
    return (
      <button disabled className={cx(BASE, 'bg-ink/[0.05] text-faint border border-ink/[0.05] cursor-not-allowed shadow-none', SIZE[size], className)} {...rest}>
        {children}
      </button>
    );
  }
  return (
    <button className={cx(BASE, PRESS, VARIANT[variant], SIZE[size], className)} {...rest}>
      {children}
    </button>
  );
}

export function LinkButton({ className, children, ...rest }) {
  return (
    <button className={cx('bg-transparent border-none p-0 text-[13.5px] font-medium text-steel-text cursor-pointer hover:underline hover:text-[oklch(0.42_0.12_248)] active:text-[oklch(0.35_0.12_248)]', className)} {...rest}>
      {children}
    </button>
  );
}

function useClickAway(onAway) {
  const ref = useRef(null);
  useEffect(() => {
    const h = (e) => { if (ref.current && !ref.current.contains(e.target)) onAway(); };
    document.addEventListener('pointerdown', h, true);
    return () => document.removeEventListener('pointerdown', h, true);
  }, [onAway]);
  return ref;
}

export function Menu({ items, onPick }) {
  return (
    <div className="absolute top-[calc(100%+4px)] left-0 min-w-[180px] z-30 bg-surface border border-rule rounded-[10px] shadow-menu p-1">
      {items.map((it) => (
        <div key={it.label} onClick={() => onPick(it)}
          className={cx('px-2.5 py-[7px] rounded-chip text-[13.5px] cursor-pointer',
            it.danger ? 'text-fault-text hover:bg-fault-wash' : 'text-body hover:bg-[oklch(0.96_0.003_250)]')}>
          {it.label}
        </div>
      ))}
    </div>
  );
}

const Chevron = ({ className }) => (
  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" className={className}><polyline points="6 9 12 15 18 9" /></svg>
);

export function SplitButton({ label, onPrimary, items, onPick }) {
  const [open, setOpen] = useState(false);
  const ref = useClickAway(() => setOpen(false));
  return (
    <div ref={ref} className="relative inline-flex">
      <Button className="rounded-r-none" onClick={onPrimary}>{label}</Button>
      <Button className="rounded-l-none w-[30px] !px-0 border-l border-l-[oklch(0.38_0.02_250)] flex items-center justify-center" aria-label="More actions" onClick={() => setOpen(!open)}>
        <Chevron />
      </Button>
      {open && <Menu items={items} onPick={(it) => { setOpen(false); onPick?.(it); }} />}
    </div>
  );
}

export function ComboButton({ label = 'Actions', items, onPick }) {
  const [open, setOpen] = useState(false);
  const ref = useClickAway(() => setOpen(false));
  return (
    <div ref={ref} className="relative inline-flex">
      <Button variant="secondary" className="inline-flex items-center gap-[7px]" onClick={() => setOpen(!open)}>
        {label} <Chevron className="text-dim" />
      </Button>
      {open && <Menu items={items} onPick={(it) => { setOpen(false); onPick?.(it); }} />}
    </div>
  );
}

export function ToggleButton({ on, children, ...rest }) {
  return <Button variant={on ? 'primary' : 'secondary'} className="font-semibold" {...rest}>{children}</Button>;
}

// Bordered group of exclusive views (Chart | List | Timeline)
export function ButtonGroup({ items, value, onChange }) {
  return (
    <div className="inline-flex border border-ctl rounded-ctl overflow-hidden">
      {items.map((label, i) => (
        <button key={label} onClick={() => onChange(i)}
          className={cx('h-[30px] px-[13px] text-[13.5px] font-medium cursor-pointer transition-colors duration-150',
            i && 'border-l border-rule',
            value === i ? 'bg-ink text-surface' : 'bg-surface text-dim hover:bg-[oklch(0.97_0.003_250)] active:bg-[oklch(0.94_0.004_250)]')}>
          {label}
        </button>
      ))}
    </div>
  );
}
