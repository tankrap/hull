// Menu/overlay atoms extracted verbatim from App.tsx (pure structural move).
// Popover portals a fixed-positioned panel to <body>; Picker + SplitButton build on it.
import { useState, useRef, useEffect, useLayoutEffect } from "react";
import { createPortal } from "react-dom";

// A click-to-open popover anchored under its trigger. Closes on outside-click or Escape. Used for the
// header "checks" summary, so the landing gate is reachable from the top of the page.
export function Popover({ trigger, children, align = "left", width = 300, direction = "down", block = false, onToggle }: { trigger: (open: boolean) => React.ReactNode; children: React.ReactNode; align?: "left" | "right"; width?: number; direction?: "down" | "up"; block?: boolean; onToggle?: (open: boolean) => void }) {
  const [open, setOpen] = useState(false);
  const [rect, setRect] = useState<DOMRect | null>(null);
  const wrapRef = useRef<HTMLSpanElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);
  const set = (v: boolean) => { setOpen(v); onToggle?.(v); };
  const measure = () => { if (wrapRef.current) setRect(wrapRef.current.getBoundingClientRect()); };
  useLayoutEffect(() => { if (open) measure(); /* eslint-disable-next-line */ }, [open]);
  useEffect(() => {
    if (!open) return;
    // Close on an outside click — but NOT when the click lands inside ANY popover panel (panels are
    // portaled to <body>, so a nested menu's panel is a DOM sibling that would otherwise read as
    // "outside" and wrongly close its parent). Menu items still close via the panel's own onClick.
    const onDoc = (e: MouseEvent) => {
      const t = e.target as HTMLElement;
      if (wrapRef.current?.contains(t)) return;
      if (t.closest?.("[data-popover-panel]")) return;
      set(false);
    };
    const onEsc = (e: KeyboardEvent) => { if (e.key === "Escape") set(false); };
    const reflow = () => measure();
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onEsc);
    window.addEventListener("scroll", reflow, true);
    window.addEventListener("resize", reflow);
    return () => { document.removeEventListener("mousedown", onDoc); document.removeEventListener("keydown", onEsc); window.removeEventListener("scroll", reflow, true); window.removeEventListener("resize", reflow); };
  }, [open]);
  // The panel renders in a body portal with FIXED positioning, so it escapes every ancestor's
  // overflow-hidden / stacking context and always paints on top — no clipping, no z-index fights.
  const style: React.CSSProperties = { position: "fixed", zIndex: 60, width: block ? (rect?.width ?? width) : width };
  if (rect) {
    if (direction === "up") { style.bottom = window.innerHeight - rect.top + 6; style.maxHeight = rect.top - 16; }
    else { style.top = rect.bottom + 6; style.maxHeight = window.innerHeight - rect.bottom - 16; }
    if (align === "right") style.right = window.innerWidth - rect.right; else style.left = rect.left;
  }
  return (
    <span ref={wrapRef} className={`relative inline-flex ${block ? "w-full" : ""}`}>
      {/* stopPropagation so a Popover nested inside another Popover's panel doesn't bubble its trigger
          click up to the outer panel's close-on-click handler (which would shut everything). */}
      <button type="button" onClick={(e) => { e.stopPropagation(); set(!open); }} className={`inline-flex ${block ? "w-full" : ""}`}>{trigger(open)}</button>
      {open && rect && createPortal(
        <div ref={panelRef} style={style} data-popover-panel onClick={() => set(false)}
          className="bg-surface border border-rule rounded-card shadow-menu overflow-y-auto overflow-x-hidden animate-[bd-in_120ms_ease-out]">
          {children}
        </div>,
        document.body,
      )}
    </span>
  );
}

// Styled select (replaces native <select>). options: {value,label}[]. When value is "" it shows the
// placeholder — used both for bound selects and "pick to act" menus.
type PickerOption = { value: string; label: string; sub?: string; avatar?: React.ReactNode };
export function Picker({ value, onChange, options, placeholder = "Select…", width = 220, size = "md", block = false, direction = "down", className = "", searchable: searchableProp }: { value: string; onChange: (v: string) => void; options: PickerOption[]; placeholder?: string; width?: number; size?: "sm" | "md"; block?: boolean; direction?: "down" | "up"; className?: string; searchable?: boolean }) {
  const cur = options.find((o) => o.value === value);
  const h = size === "sm" ? "h-ctl-sm text-xs" : "h-ctl text-[13px]";
  const [q, setQ] = useState("");
  const searchable = searchableProp ?? options.length >= 8;
  const ql = q.trim().toLowerCase();
  const filtered = ql ? options.filter((o) => o.label.toLowerCase().includes(ql) || (o.sub ?? "").toLowerCase().includes(ql)) : options;
  const rich = options.some((o) => o.avatar || o.sub);
  return (
    <Popover align="left" width={width} block={block} direction={direction} onToggle={(o) => { if (!o) setQ(""); }} trigger={(open) => (
      <span className={`inline-flex items-center justify-between gap-2 ${h} px-3 rounded-ctl border transition-[background-color,border-color,box-shadow] duration-200 bg-[var(--btn-neu-bg)] ${block ? "w-full" : ""} ${open ? "border-transparent shadow-[0_0_0_1px_var(--paper),0_0_0_3px_var(--steel)]" : "border-[var(--btn-neu-border)] hover:bg-[var(--btn-neu-bg-hover)] hover:border-[var(--btn-neu-border-hover)]"} ${className}`}>
        <span className={`truncate ${cur ? "text-ink" : "text-faint"}`}>{cur?.label ?? placeholder}</span>
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className={`text-muted flex-none transition-transform ${open ? "rotate-180" : ""}`}><polyline points="6 9 12 15 18 9" /></svg>
      </span>
    )}>
      {searchable && (
        <div className="p-1.5 border-b border-rule2 sticky top-0 bg-surface" onClick={(e) => e.stopPropagation()}>
          <input autoFocus value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search…" className="w-full box-border h-ctl-sm px-2 rounded-ctl-sm border border-[var(--field-border)] bg-[var(--field-bg)] font-sans text-[12.5px] text-ink outline-none focus:border-steel placeholder:text-faint" />
        </div>
      )}
      <div className="py-1 max-h-[280px] overflow-y-auto overflow-x-hidden">
        {filtered.length === 0 && <div className="px-3 py-1.5 text-[12.5px] text-muted">{ql ? "no matches" : "none available"}</div>}
        {filtered.map((o) => (rich ? (
          <button key={o.value} type="button" title={o.label} onClick={() => onChange(o.value)} className={`w-full text-left px-2.5 py-1.5 flex items-center gap-2.5 hover:bg-rule2 ${o.value === value ? "bg-rule" : ""}`}>
            {o.avatar ? <span className="flex-none">{o.avatar}</span> : null}
            <span className="min-w-0 flex-1">
              <span className={`block text-[13px] truncate ${o.value === value ? "font-medium text-ink" : "text-body"}`}>{o.label}</span>
              {o.sub && <span className="block text-[11.5px] text-muted truncate">{o.sub}</span>}
            </span>
            {o.value === value && <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round" className="text-ink flex-none"><polyline points="20 6 9 17 4 12" /></svg>}
          </button>
        ) : (
          <button key={o.value} type="button" title={o.label} onClick={() => onChange(o.value)} className={`w-full text-left px-3 py-1.5 text-[13px] truncate hover:bg-rule2 ${o.value === value ? "bg-rule font-medium text-ink" : "text-body"}`}>{o.label}</button>
        )))}
      </div>
    </Popover>
  );
}

// A cohesive split button: a primary action + a dropdown, sharing ONE dark/muted skin so a disabled
// submit never leaves a stray bright chevron. The chevron stays clickable while the submit is disabled
// (to pick a verdict / mode / secondary action). Shared by every composer — issues AND pull requests.
export function SplitButton({ label, icon, disabled, onSubmit, menu, menuWidth = 252 }: { label: React.ReactNode; icon?: React.ReactNode; disabled?: boolean; onSubmit: () => void; menu: React.ReactNode; menuWidth?: number }) {
  return (
    <div className={`flex-none inline-flex h-ctl rounded-ctl overflow-hidden border ${disabled ? "border-[var(--btn-dis-border)]" : "border-transparent"}`}>
      <button type="button" disabled={disabled} onClick={onSubmit}
        className={`inline-flex items-center gap-1.5 px-3 text-[13px] font-normal whitespace-nowrap transition-colors duration-200 ${disabled ? "bg-[var(--btn-dis-bg)] text-[var(--btn-dis-fg)] cursor-not-allowed" : "bg-[var(--btn-cta-bg)] text-[var(--btn-cta-fg)] hover:bg-[var(--btn-cta-bg-hover)]"}`}>
        {icon}{label}
      </button>
      <Popover align="right" width={menuWidth} direction="up" trigger={(open) => (
        <span className={`inline-flex items-center h-full px-1.5 border-l cursor-pointer transition-[background-color] duration-200 ${disabled ? "bg-[var(--btn-dis-bg)] text-[var(--btn-dis-fg)] border-[var(--btn-dis-border)] hover:text-ink" : "bg-[var(--btn-cta-bg)] text-[var(--btn-cta-fg)] border-l-[color-mix(in_oklab,var(--btn-cta-fg)_25%,transparent)] hover:bg-[var(--btn-cta-bg-hover)]"} ${open ? "opacity-90" : ""}`}>
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" className={`transition-transform ${open ? "rotate-180" : ""}`}><polyline points="6 9 12 15 18 9" /></svg>
        </span>
      )}>
        {menu}
      </Popover>
    </div>
  );
}
