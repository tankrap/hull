// keel/hull form controls — Tailwind. Focus = border darkens to ink. NO focus rings.

const cx = (...a: (string | false | null | undefined)[]) => a.filter(Boolean).join(' ');
const FIELD = 'w-full box-border h-ctl px-2.5 rounded-ctl border bg-surface font-sans text-[13.5px] text-ink outline-none transition-colors duration-150';
const Label = ({ children }: { children?: React.ReactNode }) => <div className="text-[12.5px] font-semibold text-body mb-1.5">{children}</div>;

type TextFieldProps = React.InputHTMLAttributes<HTMLInputElement> & {
  label?: React.ReactNode;
  help?: React.ReactNode;
  error?: React.ReactNode;
};

export function TextField({ label, help, error, className, ...rest }: TextFieldProps) {
  return (
    <div>
      {label && <Label>{label}</Label>}
      <input className={cx(FIELD, error ? 'border-fault' : 'border-ctl focus:border-steel focus:ring-2 focus:ring-steel/30', className)} {...rest} />
      {(error || help) && <div className={cx('text-xs mt-[5px]', error ? 'text-fault-text' : 'text-muted')}>{error || help}</div>}
    </div>
  );
}

// Search input with leading icon and trailing shortcut chip.
export function SearchInput({ shortcut = '⌘K', ...rest }: React.InputHTMLAttributes<HTMLInputElement> & { shortcut?: React.ReactNode }) {
  return (
    <div className="flex items-center gap-2 h-ctl px-2.5 rounded-ctl border border-ctl bg-surface transition-colors duration-150 focus-within:border-steel focus-within:ring-2 focus-within:ring-steel/30">
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="text-muted flex-none">
        <circle cx="11" cy="11" r="8" /><line x1="21" y1="21" x2="16.65" y2="16.65" />
      </svg>
      <input className="flex-1 min-w-0 border-none outline-none bg-transparent font-sans text-[13.5px] text-ink placeholder:text-faint" {...rest} />
      {shortcut && <span className="text-[11px] font-semibold text-dim border border-rule rounded-[5px] px-[5px] py-0.5 bg-paper flex-none">{shortcut}</span>}
    </div>
  );
}

export function Switch({ on, onChange }: { on?: boolean; onChange: (v: boolean) => void }) {
  return (
    <div onClick={() => onChange(!on)}
      className={cx('w-9 h-5 rounded-full p-0.5 box-border cursor-pointer flex-none transition-colors duration-150', on ? 'bg-steel' : 'bg-ctl')}>
      <div className={cx('w-4 h-4 rounded-full bg-white shadow-[0_1px_2px_rgba(25,28,33,0.2)] transition-transform duration-150', on && 'translate-x-4')} />
    </div>
  );
}
