// keel/hull buttons — Tailwind. Every clickable presses: active:translate-y-px active:scale-[0.99].

const cx = (...a: (string | false | null | undefined)[]) => a.filter(Boolean).join(' ');

// GitLab .gl-button: 400 weight, gl-text-base (14px), 1px border, sizing by min-height not padding,
// and a two-part focus ring (1px background gap + 3px blue) applied on :focus-visible.
const BASE = 'inline-flex items-center justify-center gap-1.5 font-normal cursor-pointer select-none whitespace-nowrap focus-visible:outline-none focus-visible:shadow-[0_0_0_1px_var(--paper),0_0_0_3px_var(--steel)]';
const PRESS = 'transition-[transform,box-shadow,background-color,border-color,color,filter] duration-200 active:translate-y-px active:scale-[0.99]';

const VARIANT = {
  // GitLab "confirm": NEUTRAL high-contrast — near-black in light, near-white in dark. Not blue.
  primary: 'bg-[var(--btn-cta-bg)] text-[var(--btn-cta-fg)] border border-transparent hover:bg-[var(--btn-cta-bg-hover)] active:bg-[var(--btn-cta-bg-hover)]',
  // GitLab "default": light = white + neutral-200 border → neutral-400 on hover; dark = a translucent
  // gray fill with NO border. The tokens carry the whole per-theme difference.
  secondary: 'bg-[var(--btn-neu-bg)] text-[var(--btn-neu-fg)] border border-[var(--btn-neu-border)] hover:bg-[var(--btn-neu-bg-hover)] hover:border-[var(--btn-neu-border-hover)] active:bg-[var(--btn-neu-bg-hover)]',
  // GitLab "tertiary": no fill, blue label, tints on hover.
  ghost: 'bg-transparent text-steel-text border border-transparent hover:bg-steel-wash active:bg-steel-wash/80',
  // GitLab "danger (secondary)": the neutral default face, red label + red border.
  destructive: 'bg-[var(--btn-neu-bg)] text-fault-text border border-fault/50 hover:bg-fault-wash hover:border-fault/75 active:bg-fault-wash',
};

const SIZE = {
  sm: 'h-ctl-sm px-2 text-[13px] rounded-ctl-sm',   // gl-btn-sm: 24px, gl-px-3 (8px)
  md: 'h-ctl px-3 text-sm rounded-ctl',             // gl-btn-md: 32px, gl-px-4 (12px)
  lg: 'h-ctl-lg px-4 text-[15px] rounded-ctl',
};

type ButtonProps = React.ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: keyof typeof VARIANT;
  size?: keyof typeof SIZE;
};

export function Button({ variant = 'primary', size = 'md', className, disabled, children, ...rest }: ButtonProps) {
  if (disabled) {
    return (
      <button disabled className={cx('inline-flex items-center justify-center gap-1.5 font-normal select-none whitespace-nowrap bg-[var(--btn-dis-bg)] text-[var(--btn-dis-fg)] border border-[var(--btn-dis-border)] cursor-not-allowed', SIZE[size], className)} {...rest}>
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

export function LinkButton({ className, children, ...rest }: React.ButtonHTMLAttributes<HTMLButtonElement>) {
  return (
    <button className={cx('bg-transparent border-none p-0 text-[13.5px] font-medium text-steel-text cursor-pointer hover:underline hover:text-[oklch(0.42_0.12_248)] active:text-[oklch(0.35_0.12_248)]', className)} {...rest}>
      {children}
    </button>
  );
}
