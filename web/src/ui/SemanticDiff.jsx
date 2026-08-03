// keel/hull SemanticDiff — Tailwind. The flagship review surface.
// Structure + state machine; op detail bodies are data-driven (see building blocks below).
import React, { useState, useEffect } from 'react';
import { Button } from './Button.jsx';
import { OpBadge } from './Badge.jsx';

const cx = (...a) => a.filter(Boolean).join(' ');

// Reviewed-state persistence. Keyed by the change so a reload (or navigating away and back) keeps
// the "mark reviewed" checkmarks the reviewer already earned.
const loadReviewed = (key, n) => {
  if (!key) return null;
  try { const a = JSON.parse(localStorage.getItem(key) || 'null'); return Array.isArray(a) && a.length === n ? a : null; } catch { return null; }
};

// op: { kind, title, meta, body: ReactNode | (ctx)=>ReactNode, proposals?: [...] }
export function SemanticDiff({ voyage, ops, rawDiff, onMerge, showMerge = true, storageKey }) {
  const [sel, setSel] = useState(0);
  const [view, setView] = useState('sem');
  const [full, setFull] = useState(false);
  const [railW, setRailW] = useState(236);
  // Drag the divider to resize the file/findings rail.
  const startResize = (e) => {
    e.preventDefault();
    const startX = e.clientX, startW = railW;
    const onMove = (ev) => setRailW(Math.max(170, Math.min(520, startW + ev.clientX - startX)));
    const onUp = () => { window.removeEventListener('mousemove', onMove); window.removeEventListener('mouseup', onUp); document.body.style.userSelect = ''; };
    document.body.style.userSelect = 'none';
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
  };
  const [reviewed, setReviewed] = useState(() => loadReviewed(storageKey, ops.length) ?? ops.map(() => false));
  const [accepted, setAccepted] = useState(ops.map((o) => (o.proposals || []).map(() => false)));
  const done = reviewed.filter(Boolean).length;
  const allDone = done === ops.length;

  // Persist reviewed state per change.
  useEffect(() => { if (storageKey) try { localStorage.setItem(storageKey, JSON.stringify(reviewed)); } catch { /* quota */ } }, [reviewed, storageKey]);
  // Esc leaves fullscreen.
  useEffect(() => {
    if (!full) return;
    const onKey = (e) => { if (e.key === 'Escape') setFull(false); };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [full]);

  const markAndNext = () => {
    setReviewed((r) => r.map((v, i) => (i === sel ? true : v)));
    setSel((s) => Math.min(ops.length - 1, s + 1));
  };
  const acceptProposal = (opIdx, pIdx) => {
    setAccepted((a) => {
      const next = a.map((row) => [...row]);
      next[opIdx][pIdx] = true;
      if (next[opIdx].every(Boolean)) setReviewed((r) => r.map((v, i) => (i === opIdx ? true : v)));
      return next;
    });
  };

  const body = view === 'sem' ? (
    <div className={cx('grid', full && 'flex-1 min-h-0')} style={{ gridTemplateColumns: `${railW}px 7px 1fr` }}>
      {/* rail */}
      <div className={cx('border-r border-rule2 bg-paper px-2 py-2.5 grid gap-0.5 content-start overflow-x-hidden', full && 'overflow-y-auto')}>
        {ops.map((op, i) => (
          <button key={i} onClick={() => setSel(i)}
            className={cx('flex items-center gap-2 min-w-0 px-2.5 py-2 rounded-ctl text-left transition-colors duration-150 border',
              sel === i ? 'bg-surface border-rule2 shadow-[0_1px_2px_rgba(15,23,42,0.04)]' : 'border-transparent hover:bg-surface/70')}>
            <span className={cx('w-4 h-4 rounded-full grid place-items-center flex-none text-[10px]',
              reviewed[i] ? 'bg-clear text-white' : 'border border-ctl text-transparent')}>
              <svg width="9" height="9" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="4" strokeLinecap="round" strokeLinejoin="round"><polyline points="20 6 9 17 4 12" /></svg>
            </span>
            <span className="min-w-0 flex-1">
              <span className="block text-[13px] font-semibold truncate leading-tight">{op.title}</span>
              <span className="block text-[11.5px] text-muted truncate leading-tight mt-0.5">{op.meta}</span>
            </span>
            <OpBadge kind={op.kind} />
          </button>
        ))}
      </div>
      {/* resize handle */}
      <div onMouseDown={startResize} title="Drag to resize" className="group cursor-col-resize flex items-center justify-center hover:bg-steel-wash/60 transition-colors">
        <span className="w-px h-8 bg-rule2 group-hover:bg-steel" />
      </div>
      {/* detail */}
      <div className={cx('flex flex-col min-w-0', full ? 'overflow-auto' : '')}>
        <div className={cx('flex-1 min-w-0', full ? 'px-6 py-5' : 'px-5 py-4')}>
          {typeof ops[sel].body === 'function'
            ? ops[sel].body({ accepted: accepted[sel], accept: (p) => acceptProposal(sel, p) })
            : ops[sel].body}
        </div>
        <div className="flex justify-between items-center gap-3 px-5 py-3 border-t border-rule2 bg-paper/60 sticky bottom-0">
          <Button size="sm" variant="secondary" onClick={() => setSel((s) => Math.max(0, s - 1))}
            className={sel === 0 ? 'invisible' : ''}>← Prev</Button>
          <div className="flex items-center gap-3">
            <span className="text-xs text-muted tabular-nums">{sel + 1} / {ops.length}</span>
            <Button size="sm" onClick={markAndNext}
              className={cx('font-semibold', reviewed[sel] && '!bg-clear-wash !text-clear-text !border-transparent')}>
              {reviewed[sel] ? (sel < ops.length - 1 ? 'Next →' : 'Reviewed ✓') : (sel < ops.length - 1 ? 'Reviewed · next →' : 'Mark reviewed')}
            </Button>
          </div>
        </div>
      </div>
    </div>
  ) : (
    <div className={cx(full && 'flex-1 min-h-0 overflow-auto')}>
      <RawDiff diff={rawDiff} onJump={(opIdx) => { setView('sem'); setSel(opIdx); }} />
    </div>
  );

  const card = (
    <div className={cx('bg-surface border border-rule rounded-card overflow-hidden',
      full ? 'fixed inset-4 z-50 shadow-modal flex flex-col' : '')}>
      {/* header */}
      <div className="px-4 py-3 border-b border-rule2 flex justify-between items-center gap-4 flex-wrap flex-none">
        <div className="flex items-baseline gap-2.5 min-w-0">
          <span className="text-[13.5px] font-semibold truncate">{voyage.title}</span>
          <span className="text-[12px] text-muted tabular-nums flex-none">{voyage.id}</span>
        </div>
        <div className="flex gap-2 items-center">
          <div className="inline-flex border border-ctl rounded-ctl overflow-hidden">
            {[['sem', 'Semantic'], ['raw', 'Unified']].map(([id, label], i) => (
              <button key={id} onClick={() => setView(id)}
                className={cx('text-xs font-semibold px-2.5 py-[5px] transition-colors duration-150',
                  i && 'border-l border-ctl',
                  view === id ? 'bg-ink text-surface' : 'text-dim hover:text-ink')}>{label}</button>
            ))}
          </div>
          <span className="text-[11.5px] font-semibold text-muted tabular-nums">{allDone ? 'all reviewed' : `${ops.length - done} left`}</span>
          <button onClick={() => setFull((f) => !f)} title={full ? 'Exit full screen (Esc)' : 'Full screen'}
            className="w-8 h-8 grid place-items-center rounded-ctl border border-ctl text-dim hover:text-ink hover:bg-paper transition-colors">
            {full ? (
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M9 3v6H3M21 9h-6V3M3 15h6v6M15 21v-6h6" /></svg>
            ) : (
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M15 3h6v6M9 21H3v-6M21 3l-7 7M3 21l7-7" /></svg>
            )}
          </button>
        </div>
      </div>

      {body}

      {/* footer: review progress (+ merge gate when this surface owns landing) */}
      <div className="px-4 py-2.5 border-t border-rule2 bg-paper flex justify-between items-center gap-3 flex-wrap flex-none">
        <span className="text-[12px] text-muted">{voyage.meta}</span>
        <div className="flex items-center gap-3">
          <span className="text-[12px] text-muted tabular-nums">{done} of {ops.length} reviewed</span>
          {showMerge && <Button size="sm" disabled={!allDone} onClick={onMerge} className="font-semibold">Merge voyage</Button>}
        </div>
      </div>
    </div>
  );

  return full ? (
    <>
      <div className="fixed inset-0 z-40 bg-ink/40 animate-bd-in" onClick={() => setFull(false)} />
      {card}
    </>
  ) : card;
}

// ---- building blocks for op detail bodies --------------------------------

export function LocationBar({ crumbs, right }) {
  return (
    <div className="flex items-center gap-[7px] text-[12px] text-muted bg-paper border border-rule2 rounded-t-ctl border-b-0 px-3 py-[6px]">
      <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="text-faint flex-none">
        <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" /><polyline points="14 2 14 8 20 8" />
      </svg>
      {crumbs.map((c, i) => (
        <React.Fragment key={i}>
          {i > 0 && <span className="opacity-40">/</span>}
          <span className={i === crumbs.length - 1 ? 'font-semibold text-ink' : ''}>{c}</span>
        </React.Fragment>
      ))}
      <span className="ml-auto text-[11px] tabular-nums">{right}</span>
    </div>
  );
}

// lines: [{ n, code(ReactNode), sign?: '+'|'-', note?: ReactNode, marker?: {tone,onClick,title} }]
// A row with `note` renders a full-width annotation (inline finding / line comment / composer).
// A row with `marker` shows a labelled pill in the gutter to reopen a collapsed finding.
// filePath + selectedLine/onSelectLine/onCommentLine drive line selection and line-level comments.
const MARK = { bad: 'bg-fault', warn: 'bg-brass', info: 'bg-steel' };
const CommentIcon = () => (
  <svg width="11" height="11" viewBox="0 0 24 24" fill="currentColor"><path d="M20 2H4a2 2 0 0 0-2 2v18l4-4h14a2 2 0 0 0 2-2V4a2 2 0 0 0-2-2z" /></svg>
);
export function CodePanel({ lines, filePath, selectedLine, onSelectLine, onCommentLine, highlightLine }) {
  return (
    <div className="border border-rule2 rounded-b-ctl overflow-hidden text-[12.5px] leading-[1.6]">
      {lines.map((l, i) => l.note ? (
        <div key={i} className="border-y border-rule3">{l.note}</div>
      ) : (() => {
        const ln = typeof l.n === 'number' ? l.n : null;
        const canComment = ln != null && l.sign !== '-' && onCommentLine;
        const selected = ln != null && selectedLine === ln;
        const highlighted = ln != null && highlightLine === ln;
        return (
          <div key={i} id={filePath && ln != null ? `L-${filePath}-${ln}` : undefined}
            className={cx('group grid items-stretch scroll-mt-20', l.sign ? 'grid-cols-[28px_54px_1fr]' : 'grid-cols-[54px_1fr]',
              highlighted ? 'line-highlight' : selected ? 'bg-steel-wash' : l.sign === '-' ? 'bg-fault-wash' : l.sign === '+' ? 'bg-clear-wash' : 'hover:bg-paper/60')}>
            {l.sign && <span className={cx('pl-3 py-1 font-bold select-none', l.sign === '-' ? 'text-fault-text' : 'text-clear-text')}>{l.sign}</span>}
            <span className={cx('relative flex items-center justify-end gap-1 pr-2 py-1 text-faint bg-paper/50 border-r border-rule3 text-[11px] select-none tabular-nums', canComment && 'cursor-pointer')}
              onClick={canComment ? () => onSelectLine(selected ? null : ln) : undefined}>
              {l.marker ? (
                <button onClick={(e) => { e.stopPropagation(); l.marker.onClick(); }} title={l.marker.title}
                  className={cx('absolute left-1 top-1/2 -translate-y-1/2 inline-flex items-center gap-0.5 h-[15px] px-1 rounded-[3px] text-white text-[9px] font-bold ring-1 ring-surface hover:brightness-110 transition', MARK[l.marker.tone] || 'bg-brass')}>
                  <CommentIcon />
                </button>
              ) : canComment && (
                <button onClick={(e) => { e.stopPropagation(); onCommentLine(ln); }} title="Comment on this line (or press C when selected)"
                  className="absolute left-1 top-1/2 -translate-y-1/2 w-[17px] h-[17px] grid place-items-center rounded-[4px] bg-steel text-white opacity-0 group-hover:opacity-100 hover:brightness-110 transition shadow-sm">
                  <CommentIcon />
                </button>
              )}
              {l.n}
            </span>
            <span className="px-3 py-1 text-body whitespace-pre-wrap break-words">{l.code}</span>
          </div>
        );
      })())}
    </div>
  );
}

export const OldTok = ({ children }) => (
  <span className="bg-fault/25 text-fault-text line-through rounded-[3px] px-[3px]">{children}</span>
);
export const NewTok = ({ children }) => (
  <span className="bg-clear/25 text-clear-text rounded-[3px] px-[3px] font-semibold">{children}</span>
);

export function ProposalRow({ file, before, arg, accepted, onAccept, first }) {
  return (
    <div className={cx('flex items-center gap-3 px-3.5 py-[9px] text-[13px] font-medium', !first && 'border-t border-rule3')}>
      <span className="text-[11.5px] font-semibold px-2 py-0.5 rounded-full border border-rule text-dim flex-none">{file}</span>
      <span className="text-body flex-1 min-w-0">
        {before}
        <span className={cx('px-[5px] rounded-[5px] font-semibold transition-colors duration-150',
          accepted ? 'bg-clear-wash text-clear-text' : 'bg-brass-wash text-brass-text')}>{arg}</span>)
      </span>
      {accepted ? (
        <span className="text-[11.5px] font-semibold px-2.5 py-[3px] rounded-full bg-clear-wash text-clear-text flex-none">Accepted ✓</span>
      ) : (
        <Button size="sm" variant="secondary" onClick={onAccept}
          className="!h-auto !px-2.5 !py-[3px] !text-[11.5px] !rounded-full font-semibold">Accept</Button>
      )}
    </div>
  );
}

// ---- raw view -------------------------------------------------------------
// diff: { files: [{ name, add, del, hunks: [{ header, lines: [{ o, n, sign, code, opIdx?, opTag? }] }] }], hiddenNote? }
function RawDiff({ diff, onJump }) {
  const gut = 'px-2 py-0.5 text-right text-faint text-[11px] bg-paper/50 border-r border-rule3 self-stretch flex items-center justify-end tabular-nums select-none';
  return (
    <div className="text-[12.5px] leading-[1.6]">
      {diff.files.map((f, fi) => (
        <React.Fragment key={f.name}>
          <div className={cx('flex items-center gap-2.5 px-4 py-2 bg-paper border-b border-rule2 sticky top-0 z-[1]', fi && 'border-t')}>
            <span className="text-[12px] font-bold">{f.name}</span>
            <span className="text-[11.5px] font-semibold text-clear-text tabular-nums">+{f.add}</span>
            <span className="text-[11.5px] font-semibold text-fault-text tabular-nums">−{f.del}</span>
          </div>
          {f.hunks.map((h) => (
            <React.Fragment key={h.header}>
              <div className="px-4 py-[3px] text-[11px] font-semibold text-steel-text bg-steel-wash border-b border-rule3 tabular-nums">{h.header}</div>
              {h.lines.map((l, li) => (
                <div key={li} className={cx('grid grid-cols-[44px_44px_20px_1fr_auto] items-stretch',
                  l.sign === '-' ? 'bg-fault-wash' : l.sign === '+' ? 'bg-clear-wash' : 'hover:bg-paper/50')}>
                  <span className={gut}>{l.o}</span><span className={gut}>{l.n}</span>
                  <span className={cx('pl-2 py-0.5 font-bold select-none', l.sign === '-' ? 'text-fault-text' : l.sign === '+' ? 'text-clear-text' : 'text-transparent')}>{l.sign || ' '}</span>
                  <span className="px-3 py-0.5 text-body whitespace-pre-wrap break-words">{l.code}</span>
                  {l.opIdx != null ? (
                    <button onClick={() => onJump(l.opIdx)}
                      className="text-[10.5px] font-bold px-2 py-0.5 rounded-full mr-3 my-0.5 whitespace-nowrap bg-steel-wash text-steel-text hover:underline self-center">
                      {l.opTag} →</button>
                  ) : <span />}
                </div>
              ))}
            </React.Fragment>
          ))}
        </React.Fragment>
      ))}
      {diff.hiddenNote && <div className="px-4 py-2.5 text-xs text-muted border-t border-rule3">{diff.hiddenNote}</div>}
    </div>
  );
}
