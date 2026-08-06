// keel/hull PinInput — Tailwind. Segmented code entry, paste-aware.
import { useRef, useState } from 'react';

const cx = (...a: (string | false | null | undefined)[]) => a.filter(Boolean).join(' ');

// mode: 'digits' | 'alnum'
export function PinInput({ length = 6, mode = 'digits', onComplete }: {
  length?: number;
  mode?: 'digits' | 'alnum';
  onComplete?: (code: string) => void;
}) {
  const [vals, setVals] = useState<string[]>(Array(length).fill(''));
  const refs = useRef<(HTMLInputElement | null)[]>([]);
  const ok = (ch: string) => (mode === 'digits' ? /^[0-9]$/ : /^[a-zA-Z0-9]$/).test(ch);
  const norm = (ch: string) => (mode === 'alnum' ? ch.toUpperCase() : ch);
  const commit = (next: string[]) => { setVals(next); if (next.every(Boolean)) onComplete?.(next.join('')); };

  const handleKey = (i: number, e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Backspace') {
      e.preventDefault();
      const next = [...vals];
      if (next[i]) { next[i] = ''; commit(next); }
      else if (i > 0) { next[i - 1] = ''; commit(next); refs.current[i - 1]?.focus(); }
    } else if (e.key === 'ArrowLeft' && i > 0) refs.current[i - 1]?.focus();
    else if (e.key === 'ArrowRight' && i < length - 1) refs.current[i + 1]?.focus();
    else if (ok(e.key)) {
      e.preventDefault();
      const next = [...vals]; next[i] = norm(e.key); commit(next);
      if (i < length - 1) refs.current[i + 1]?.focus();
    }
  };

  const handlePaste = (i: number, e: React.ClipboardEvent<HTMLInputElement>) => {
    e.preventDefault();
    const chars = [...e.clipboardData.getData('text')].filter(ok).map(norm);
    if (!chars.length) return;
    const next = [...vals];
    for (let j = 0; j < chars.length && i + j < length; j++) next[i + j] = chars[j];
    commit(next);
    refs.current[Math.min(i + chars.length, length - 1)]?.focus();
  };

  return (
    <div className="flex gap-2">
      {vals.map((v, i) => (
        <input key={i} ref={(el) => { refs.current[i] = el; }} value={v}
          inputMode={mode === 'digits' ? 'numeric' : 'text'} onChange={() => {}}
          onKeyDown={(e) => handleKey(i, e)} onPaste={(e) => handlePaste(i, e)}
          onFocus={(e) => e.target.select()}
          className={cx('w-[38px] h-11 text-center box-border font-sans text-lg font-semibold text-ink rounded-ctl outline-none caret-transparent border border-ctl focus:border-body transition-colors duration-150',
            v ? 'bg-paper' : 'bg-surface')} />
      ))}
    </div>
  );
}
