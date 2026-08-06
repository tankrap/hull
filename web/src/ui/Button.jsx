// keel/hull buttons — Tailwind. Every clickable presses: active:translate-y-px active:scale-[0.99].
import React from 'react';

const cx = (...a) => a.filter(Boolean).join(' ');

// Every button: centered content, consistent icon spacing, a soft press, and a smooth hover.
const BASE = 'inline-flex items-center justify-center gap-1.5 font-medium cursor-pointer select-none whitespace-nowrap focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-steel focus-visible:ring-offset-2 focus-visible:ring-offset-surface';
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
