// keel/hull form controls — Tailwind. Focus = border darkens to ink. NO focus rings.
import React, { useEffect, useRef, useState } from 'react';

const cx = (...a) => a.filter(Boolean).join(' ');
const FIELD = 'w-full box-border h-ctl px-2.5 rounded-ctl border bg-surface font-sans text-[13.5px] text-ink outline-none transition-colors duration-150';
const Label = ({ children }) => <div className="text-[12.5px] font-semibold text-body mb-1.5">{children}</div>;

export function TextField({ label, help, error, className, ...rest }) {
  return (
    <div>
      {label && <Label>{label}</Label>}
      <input className={cx(FIELD, error ? 'border-fault' : 'border-ctl focus:border-body', className)} {...rest} />
      {(error || help) && <div className={cx('text-xs mt-[5px]', error ? 'text-fault-text' : 'text-muted')}>{error || help}</div>}
    </div>
  );
}

// Framed composer: borderless textarea + footer (helper left, count right).
export function Composer({ label, helper, max = 280, defaultValue = '' }) {
  const [val, setVal] = useState(defaultValue);
  return (
    <div>
      {label && <Label>{label}</Label>}
      <div className="border border-ctl rounded-ctl bg-surface transition-colors duration-150 focus-within:border-body">
        <textarea rows={4} value={val} onChange={(e) => setVal(e.target.value.slice(0, max))}
          className="w-full box-border block px-[11px] pt-[9px] pb-1 border-none rounded-t-ctl font-sans text-[13.5px] leading-[1.55] text-ink outline-none resize-none bg-transparent" />
        <div className="flex justify-between items-center px-[11px] pt-[5px] pb-[7px]">
          <span className="text-[11.5px] text-faint">{helper}</span>
          <span className="text-[11.5px] text-faint tabular-nums">{val.length} / {max}</span>
        </div>
      </div>
    </div>
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

const Option = ({ label, selected, onPick, bold }) => (
  <div onClick={onPick} className={cx('px-2.5 py-[7px] rounded-chip text-[13.5px] cursor-pointer',
    selected ? 'font-semibold text-steel-text bg-steel-wash' : 'text-body hover:bg-[oklch(0.96_0.003_250)]')}>
    {bold ? <><b>{bold}</b>{label.slice(bold.length)}</> : label}
  </div>
);

export function Select({ label, options, value, onChange }) {
  const [open, setOpen] = useState(false);
  const ref = useClickAway(() => setOpen(false));
  return (
    <div ref={ref} className="relative">
      {label && <Label>{label}</Label>}
      <div onClick={() => setOpen(!open)} className={cx(FIELD, 'border-ctl flex items-center justify-between gap-2 cursor-pointer hover:border-[oklch(0.6_0.015_250)]')}>
        <span>{value}</span>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"
          className={cx('text-muted flex-none transition-transform duration-150', open && 'rotate-180')}><polyline points="6 9 12 15 18 9" /></svg>
      </div>
      {open && (
        <div className="absolute top-[calc(100%+4px)] inset-x-0 z-30 bg-surface border border-rule rounded-[10px] shadow-menu p-1">
          {options.map((o) => <Option key={o} label={o} selected={o === value} onPick={() => { onChange(o); setOpen(false); }} />)}
        </div>
      )}
    </div>
  );
}

// Filtering combobox: type-ahead, highlighted match, keyboard optional.
export function Combobox({ label, options, value, onChange, placeholder }) {
  const [open, setOpen] = useState(false);
  const [q, setQ] = useState('');
  const ref = useClickAway(() => setOpen(false));
  const hits = options.filter((o) => o.toLowerCase().includes(q.toLowerCase()));
  return (
    <div ref={ref} className="relative">
      {label && <Label>{label}</Label>}
      <input value={open ? q : value || ''} placeholder={placeholder}
        onFocus={() => { setOpen(true); setQ(''); }}
        onChange={(e) => { setQ(e.target.value); setOpen(true); }}
        className={cx(FIELD, 'border-ctl focus:border-body', open && hits.length && 'rounded-b-none')} />
      {open && hits.length > 0 && (
        <div className="absolute top-full inset-x-0 z-30 bg-surface border border-t-0 border-rule rounded-b-[10px] shadow-menu p-1">
          {hits.map((o) => (
            <Option key={o} label={o} bold={o.toLowerCase().startsWith(q.toLowerCase()) && q ? o.slice(0, q.length) : ''} selected={o === value}
              onPick={() => { onChange(o); setOpen(false); }} />
          ))}
        </div>
      )}
    </div>
  );
}

// Search input with leading icon and trailing shortcut chip.
export function SearchInput({ shortcut = '⌘K', ...rest }) {
  return (
    <div className="flex items-center gap-2 h-ctl px-2.5 rounded-ctl border border-ctl bg-surface transition-colors duration-150 focus-within:border-body">
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="text-muted flex-none">
        <circle cx="11" cy="11" r="8" /><line x1="21" y1="21" x2="16.65" y2="16.65" />
      </svg>
      <input className="flex-1 min-w-0 border-none outline-none bg-transparent font-sans text-[13.5px] text-ink placeholder:text-faint" {...rest} />
      {shortcut && <span className="text-[11px] font-semibold text-dim border border-rule rounded-[5px] px-[5px] py-0.5 bg-paper flex-none">{shortcut}</span>}
    </div>
  );
}

export function Checkbox({ label, checked, onChange }) {
  return (
    <div onClick={() => onChange(!checked)} className="flex items-center gap-2 cursor-pointer py-1">
      <span className={cx('w-4 h-4 rounded flex-none flex items-center justify-center border transition-colors duration-150',
        checked ? 'bg-steel border-steel' : 'bg-surface border-[oklch(0.78_0.01_250)]')}>
        {checked && <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="#fff" strokeWidth="3.5" strokeLinecap="round" strokeLinejoin="round"><polyline points="20 6 9 17 4 12" /></svg>}
      </span>
      <span className="text-[13.5px]">{label}</span>
    </div>
  );
}

export function Radio({ label, checked, onChange }) {
  return (
    <div onClick={onChange} className="flex items-center gap-2 cursor-pointer py-1">
      <span className={cx('w-4 h-4 rounded-full flex-none box-border bg-surface transition-all duration-150',
        checked ? 'border-[5px] border-steel' : 'border border-[oklch(0.78_0.01_250)]')} />
      <span className="text-[13.5px]">{label}</span>
    </div>
  );
}

export function Switch({ on, onChange }) {
  return (
    <div onClick={() => onChange(!on)}
      className={cx('w-9 h-5 rounded-full p-0.5 box-border cursor-pointer flex-none transition-colors duration-150', on ? 'bg-steel' : 'bg-[oklch(0.85_0.008_250)]')}>
      <div className={cx('w-4 h-4 rounded-full bg-white shadow-[0_1px_2px_rgba(25,28,33,0.2)] transition-transform duration-150', on && 'translate-x-4')} />
    </div>
  );
}
