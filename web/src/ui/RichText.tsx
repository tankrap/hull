import { useRef, useState } from "react";
import { Markdown } from "../markdown";

export type Mention = { handle: string; kind?: string; email?: string; avatar?: React.ReactNode };

// A markdown-aware text editor: a Write/Preview toggle + a formatting toolbar that inserts markdown,
// matching the familiar comment-box pattern. Controlled via value/onChange. Typing "@" opens a
// mention autocomplete drawn from `mentions` (↑/↓ to move, ↵/Tab to insert, Esc to dismiss).
export function RichText({ value, onChange, placeholder, rows = 4, linkBase = null, autoFocus, onSubmit, minimal, mentions }:
  { value: string; onChange: (v: string) => void; placeholder?: string; rows?: number; linkBase?: string | null; autoFocus?: boolean; onSubmit?: () => void; minimal?: boolean; mentions?: Mention[] }) {
  const [tab, setTab] = useState<"write" | "preview">("write");
  const ref = useRef<HTMLTextAreaElement>(null);
  // Active @mention query: `at` is the index of the '@', `q` the text typed after it.
  const [men, setMen] = useState<{ q: string; at: number } | null>(null);
  const [mIdx, setMIdx] = useState(0);

  const matches = (() => {
    if (!men || !mentions || mentions.length === 0) return [] as Mention[];
    const q = men.q.toLowerCase();
    return mentions
      .filter((m) => m.handle.toLowerCase().includes(q))
      .sort((a, b) => Number(b.handle.toLowerCase().startsWith(q)) - Number(a.handle.toLowerCase().startsWith(q)))
      .slice(0, 7);
  })();

  // Look back from the caret for an "@token" (token chars: word/:.-, no whitespace) whose '@' sits at a
  // word boundary. Opens/updates the mention menu, or closes it when the caret isn't in a mention.
  const syncMention = (text: string, caret: number) => {
    if (!mentions || mentions.length === 0) return;
    let i = caret - 1;
    while (i >= 0 && /[\w:.\-]/.test(text[i])) i--;
    if (i < 0 || text[i] !== "@") { setMen(null); return; }
    const prev = i > 0 ? text[i - 1] : "";
    if (i !== 0 && !/[\s([{]/.test(prev)) { setMen(null); return; }
    const q = text.slice(i + 1, caret);
    if (/\s/.test(q)) { setMen(null); return; }
    setMen((prevMen) => (prevMen && prevMen.at === i && prevMen.q === q ? prevMen : (setMIdx(0), { q, at: i })));
  };

  const pick = (m: Mention) => {
    const ta = ref.current; if (!ta || !men) return;
    const caret = ta.selectionStart;
    const insert = "@" + m.handle + " ";
    const next = value.slice(0, men.at) + insert + value.slice(caret);
    onChange(next);
    setMen(null);
    const pos = men.at + insert.length;
    requestAnimationFrame(() => { ta.focus(); ta.setSelectionRange(pos, pos); });
  };

  // Wrap the current selection (or caret) with markers, keeping focus + selection.
  const wrap = (before: string, after = before, placeholder = "") => {
    const ta = ref.current; if (!ta) return;
    const s = ta.selectionStart, e = ta.selectionEnd;
    const sel = value.slice(s, e) || placeholder;
    const next = value.slice(0, s) + before + sel + after + value.slice(e);
    onChange(next);
    requestAnimationFrame(() => { ta.focus(); ta.setSelectionRange(s + before.length, s + before.length + sel.length); });
  };
  // Prefix each selected line (list / quote / heading).
  const prefixLines = (prefix: string) => {
    const ta = ref.current; if (!ta) return;
    const s = ta.selectionStart, e = ta.selectionEnd;
    const lineStart = value.lastIndexOf("\n", s - 1) + 1;
    const block = value.slice(lineStart, e);
    const prefixed = block.split("\n").map((l) => prefix + l).join("\n");
    const next = value.slice(0, lineStart) + prefixed + value.slice(e);
    onChange(next);
    requestAnimationFrame(() => { ta.focus(); ta.setSelectionRange(lineStart, lineStart + prefixed.length); });
  };
  const T = ({ title, onClick, children }: { title: string; onClick: () => void; children: React.ReactNode }) => (
    <button type="button" title={title} onClick={onClick} className="w-8 h-8 grid place-items-center rounded-ctl text-dim hover:text-ink hover:bg-rule2 transition-colors">{children}</button>
  );
  const ico = (d: React.ReactNode) => <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">{d}</svg>;
  return (
    <div className="relative">
    <div className="border border-[var(--field-border)] rounded-ctl overflow-hidden bg-[var(--field-bg)] focus-within:border-[var(--field-border-focus)] focus-within:ring-2 focus-within:ring-steel/40 transition-[border-color,box-shadow]">
      <div className="flex items-center justify-between gap-2 border-b border-rule2 px-1.5 py-1 flex-wrap">
        <div className="inline-flex rounded-ctl-sm overflow-hidden border border-rule2">
          {(["write", "preview"] as const).map((t) => (
            <button key={t} type="button" onClick={() => setTab(t)} className={`px-2.5 py-[3px] text-[12px] font-medium capitalize ${tab === t ? "bg-surface text-ink" : "text-muted hover:text-ink"}`}>{t}</button>
          ))}
        </div>
        <div className="flex items-center gap-0.5">
          <T title="Heading" onClick={() => prefixLines("### ")}><span className="text-[13px] font-bold">H</span></T>
          <T title="Bold" onClick={() => wrap("**", "**", "bold")}><span className="text-[13px] font-bold">B</span></T>
          <T title="Italic" onClick={() => wrap("_", "_", "italic")}><span className="text-[13px] italic font-semibold">i</span></T>
          <span className="w-px h-4 bg-rule2 mx-0.5" />
          <T title="Code" onClick={() => wrap("`", "`", "code")}>{ico(<><polyline points="16 18 22 12 16 6" /><polyline points="8 6 2 12 8 18" /></>)}</T>
          <T title="Link" onClick={() => wrap("[", "](url)", "text")}>{ico(<><path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" /><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" /></>)}</T>
          <T title="Bulleted list" onClick={() => prefixLines("- ")}>{ico(<><line x1="8" y1="6" x2="21" y2="6" /><line x1="8" y1="12" x2="21" y2="12" /><line x1="8" y1="18" x2="21" y2="18" /><line x1="3" y1="6" x2="3.01" y2="6" /><line x1="3" y1="12" x2="3.01" y2="12" /><line x1="3" y1="18" x2="3.01" y2="18" /></>)}</T>
          <T title="Quote" onClick={() => prefixLines("> ")}>{ico(<><path d="M3 21c3 0 7-1 7-8V5c0-1.25-.756-2.017-2-2H4c-1.25 0-2 .75-2 1.972V11c0 1.25.75 2 2 2 1 0 1 0 1 1v1c0 1-1 2-2 2s-1 .008-1 1.031V20c0 1 0 1 1 1z" /></>)}</T>
        </div>
      </div>
      {tab === "write" ? (
        <textarea ref={ref} autoFocus={autoFocus} rows={rows} value={value}
          onChange={(e) => { onChange(e.target.value); syncMention(e.target.value, e.target.selectionStart); }}
          onClick={(e) => syncMention(value, (e.target as HTMLTextAreaElement).selectionStart)}
          onKeyUp={(e) => { if (!/^Arrow(Up|Down)$|^Enter$|^Tab$|^Escape$/.test(e.key)) syncMention(value, (e.target as HTMLTextAreaElement).selectionStart); }}
          onBlur={() => setTimeout(() => setMen(null), 120)}
          onKeyDown={(e) => {
            if (men && matches.length > 0) {
              if (e.key === "ArrowDown") { e.preventDefault(); setMIdx((i) => (i + 1) % matches.length); return; }
              if (e.key === "ArrowUp") { e.preventDefault(); setMIdx((i) => (i - 1 + matches.length) % matches.length); return; }
              if (e.key === "Enter" || e.key === "Tab") { e.preventDefault(); pick(matches[Math.min(mIdx, matches.length - 1)]); return; }
              if (e.key === "Escape") { e.preventDefault(); setMen(null); return; }
            }
            if (onSubmit && e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); onSubmit(); }
          }}
          placeholder={placeholder}
          className="w-full box-border px-2.5 py-2 bg-[var(--field-bg)] font-sans text-[13.5px] text-ink outline-none resize-y leading-[1.5] placeholder:text-faint" />
      ) : (
        <div className="px-2.5 py-2 min-h-[72px] text-[13.5px] text-body">{value.trim() ? <Markdown text={value} linkBase={linkBase} /> : <span className="text-faint">Nothing to preview</span>}</div>
      )}
      {!minimal && (
        <div className="px-2.5 py-1 border-t border-rule2 text-[11px] text-faint flex items-center gap-1.5">
          <svg width="13" height="13" viewBox="0 0 16 16" fill="currentColor"><path d="M14.85 3H1.15C.52 3 0 3.52 0 4.15v7.69C0 12.48.52 13 1.15 13h13.69c.64 0 1.15-.52 1.15-1.15v-7.7C16 3.52 15.48 3 14.85 3zM9 11H7V8L5.5 9.92 4 8v3H2V5h2l1.5 2L7 5h2v6zm2.99.5L9.5 8H11V5h2v3h1.5l-2.51 3.5z" /></svg>
          Markdown supported · type <span className="font-semibold">@</span> to mention
        </div>
      )}
    </div>
      {tab === "write" && men && matches.length > 0 && (
        <div className="absolute left-2 right-2 top-full mt-1 z-50 max-h-[220px] overflow-y-auto rounded-ctl border border-ctl bg-surface shadow-modal py-1">
          {matches.map((m, i) => (
            <button key={m.handle} type="button"
              onMouseDown={(e) => { e.preventDefault(); pick(m); }}
              onMouseEnter={() => setMIdx(i)}
              className={`w-full flex items-center gap-2.5 px-2.5 py-1.5 text-left ${i === mIdx ? "bg-steel-wash" : "hover:bg-paper/60"}`}>
              {m.avatar ? <span className="flex-none">{m.avatar}</span> : <span className={`w-1.5 h-1.5 rounded-full flex-none ${m.kind === "agent" ? "bg-steel" : "bg-brass"}`} />}
              <span className="min-w-0 flex-1">
                <span className="flex items-center gap-1.5"><span className="text-[13px] font-medium text-ink truncate">@{m.handle}</span>{m.kind === "agent" && <span className="text-[10px] font-bold uppercase tracking-[0.06em] text-steel-text bg-steel-wash rounded-[3px] px-1 py-px flex-none">agent</span>}</span>
                {m.email && <span className="block text-[11.5px] text-muted truncate">{m.email}</span>}
              </span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
