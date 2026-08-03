import { useRef, useState } from "react";
import { Markdown } from "../markdown";

// A markdown-aware text editor: a Write/Preview toggle + a formatting toolbar that inserts markdown,
// matching the familiar comment-box pattern. Controlled via value/onChange.
export function RichText({ value, onChange, placeholder, rows = 4, linkBase = null, autoFocus, onSubmit, minimal }:
  { value: string; onChange: (v: string) => void; placeholder?: string; rows?: number; linkBase?: string | null; autoFocus?: boolean; onSubmit?: () => void; minimal?: boolean }) {
  const [tab, setTab] = useState<"write" | "preview">("write");
  const ref = useRef<HTMLTextAreaElement>(null);
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
    <div className="border border-ctl rounded-ctl overflow-hidden bg-surface focus-within:border-body transition-colors">
      <div className="flex items-center justify-between gap-2 border-b border-rule2 bg-paper px-1.5 py-1 flex-wrap">
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
        <textarea ref={ref} autoFocus={autoFocus} rows={rows} value={value} onChange={(e) => onChange(e.target.value)} placeholder={placeholder}
          onKeyDown={(e) => { if (onSubmit && e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); onSubmit(); } }}
          className="w-full box-border px-2.5 py-2 bg-surface font-sans text-[13.5px] text-ink outline-none resize-y leading-[1.5] placeholder:text-faint" />
      ) : (
        <div className="px-2.5 py-2 min-h-[72px] text-[13.5px] text-body">{value.trim() ? <Markdown text={value} linkBase={linkBase} /> : <span className="text-faint">Nothing to preview</span>}</div>
      )}
      {!minimal && (
        <div className="px-2.5 py-1 border-t border-rule2 text-[11px] text-faint flex items-center gap-1.5">
          <svg width="13" height="13" viewBox="0 0 16 16" fill="currentColor"><path d="M14.85 3H1.15C.52 3 0 3.52 0 4.15v7.69C0 12.48.52 13 1.15 13h13.69c.64 0 1.15-.52 1.15-1.15v-7.7C16 3.52 15.48 3 14.85 3zM9 11H7V8L5.5 9.92 4 8v3H2V5h2l1.5 2L7 5h2v6zm2.99.5L9.5 8H11V5h2v3h1.5l-2.51 3.5z" /></svg>
          Markdown supported
        </div>
      )}
    </div>
  );
}
