// Command palette (⌘K) — extracted verbatim from App.tsx (pure structural move).
import { useState, useEffect } from "react";

export type CmdItem = { id: string; group: string; label: string; sublabel?: string; icon?: React.ReactNode; run: () => void };
export function CommandPalette({ open, items, onClose }: { open: boolean; items: CmdItem[]; onClose: () => void }) {
  const [q, setQ] = useState("");
  const [sel, setSel] = useState(0);
  useEffect(() => { if (open) { setQ(""); setSel(0); } }, [open]);
  const ql = q.trim().toLowerCase();
  const matches = ql ? items.filter((it) => `${it.label} ${it.sublabel ?? ""} ${it.group}`.toLowerCase().includes(ql)) : items;
  useEffect(() => { setSel(0); }, [ql]);
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "ArrowDown") { e.preventDefault(); setSel((s) => Math.min(matches.length - 1, s + 1)); }
      else if (e.key === "ArrowUp") { e.preventDefault(); setSel((s) => Math.max(0, s - 1)); }
      else if (e.key === "Enter") { e.preventDefault(); const m = matches[sel]; if (m) { m.run(); onClose(); } }
      else if (e.key === "Escape") { e.preventDefault(); onClose(); }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [open, matches, sel, onClose]);
  if (!open) return null;
  const groups: string[] = [];
  matches.forEach((m) => { if (!groups.includes(m.group)) groups.push(m.group); });
  let idx = -1;
  return (
    <>
      <div onClick={onClose} className="fixed inset-0 z-40 bg-[rgba(0,0,0,0.62)] animate-bd-in" />
      <div role="dialog" aria-modal="true" aria-label="Command menu" className="fixed left-1/2 top-[11vh] -translate-x-1/2 z-50 w-[580px] max-w-[92vw] bg-surface rounded-[13px] shadow-modal overflow-hidden animate-ov-in border border-rule">
        <div className="flex items-center gap-2.5 px-4 h-[50px] border-b border-rule2">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="text-muted flex-none"><circle cx="11" cy="11" r="8" /><line x1="21" y1="21" x2="16.65" y2="16.65" /></svg>
          <input autoFocus value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search or jump to a repo, issue, pull request, or action…" className="flex-1 bg-transparent outline-none font-sans text-[14px] text-ink placeholder:text-faint" />
          <span className="text-[11px] font-semibold text-dim border border-rule rounded-[5px] px-1.5 py-0.5 bg-paper">esc</span>
        </div>
        <div className="max-h-[54vh] overflow-y-auto py-1.5">
          {matches.length === 0 && <div className="px-4 py-8 text-[13px] text-muted text-center">no matches</div>}
          {groups.map((g) => (
            <div key={g}>
              <div className="px-4 pt-2.5 pb-1 text-[11px] font-semibold uppercase tracking-[0.08em] text-muted">{g}</div>
              {matches.filter((m) => m.group === g).map((m) => {
                idx++;
                const i = idx;
                return (
                  <button key={m.id} onMouseEnter={() => setSel(i)} onClick={() => { m.run(); onClose(); }}
                    className={`w-full text-left flex items-center gap-2.5 px-4 py-2 cursor-pointer ${sel === i ? "bg-steel-wash" : "hover:bg-paper"}`}>
                    <span className="flex-none w-4 grid place-items-center text-muted">{m.icon ?? <span className="text-faint">›</span>}</span>
                    <span className={`text-[13.5px] flex-1 truncate ${sel === i ? "text-steel-text font-medium" : "text-ink"}`}>{m.label}</span>
                    {m.sublabel && <span className="text-[12px] text-faint truncate flex-none max-w-[40%]">{m.sublabel}</span>}
                  </button>
                );
              })}
            </div>
          ))}
        </div>
      </div>
    </>
  );
}
