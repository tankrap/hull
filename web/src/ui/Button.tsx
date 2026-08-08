// keel/hull buttons — Tailwind. Every clickable presses: active:translate-y-px active:scale-[0.99].

const cx = (...a: (string | false | null | undefined)[]) => a.filter(Boolean).join(' ');

// Every button: centered content, consistent icon spacing, a soft press, and a smooth hover.
const BASE = 'inline-flex items-center justify-center gap-1.5 font-medium cursor-pointer select-none whitespace-nowrap focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-steel focus-visible:ring-offset-2 focus-visible:ring-offset-surface';
const PRESS = 'transition-[transform,box-shadow,background-color,border-color,color,filter] duration-150 active:translate-y-px active:scale-[0.99]';

const VARIANT = {
  // GitLab "confirm": solid blue, white label, a darker-blue hairline for a crisp edge on any ground.
  primary: 'bg-steel text-white border border-[color-mix(in_oklab,var(--steel)_78%,black)] font-semibold shadow-[0_1px_2px_rgba(0,0,0,0.16)] hover:brightness-[1.08] active:brightness-95 active:shadow-none',
  // GitLab "default": a SOLID neutral face (a step above the card) with a defined border — reads as a
  // tactile control, not a flat label. Fills lighter on hover.
  secondary: 'bg-ctlfill text-ink border border-ctl shadow-[0_1px_1px_rgba(0,0,0,0.06)] hover:bg-ctlfill-hover hover:border-dim active:bg-ctlfill-hover active:shadow-none',
  ghost: 'bg-transparent text-steel-text border border-transparent hover:bg-steel-wash active:bg-steel-wash/80',
  // GitLab "danger (secondary)": the neutral control face, red label + red-tinted border.
  destructive: 'bg-ctlfill text-fault-text border border-fault/50 shadow-[0_1px_1px_rgba(0,0,0,0.06)] hover:bg-fault-wash hover:border-fault/75 active:bg-fault-wash active:shadow-none',
};

const SIZE = {
  sm: 'h-ctl-sm px-2.5 text-[12.5px] rounded-ctl-sm',
  md: 'h-ctl px-3.5 text-sm rounded-ctl',
  lg: 'h-ctl-lg px-[18px] text-[15px] rounded-[9px]',
};

type ButtonProps = React.ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: keyof typeof VARIANT;
  size?: keyof typeof SIZE;
};

export function Button({ variant = 'primary', size = 'md', className, disabled, children, ...rest }: ButtonProps) {
  if (disabled) {
    return (
      <button disabled className={cx(BASE, 'bg-ctlfill/50 text-faint border border-ctl/60 cursor-not-allowed shadow-none', SIZE[size], className)} {...rest}>
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
