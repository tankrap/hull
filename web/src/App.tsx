import { Component, lazy, Suspense, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
// Code-split the heavy Shiki-powered @pierre viewers into their own chunk (kept out of the initial bundle).
const PierreFile = lazy(() => import("@pierre/diffs/react").then((m) => ({ default: m.File })));
const RepoTree = lazy(() => import("./RepoTree"));
import * as ed from "@noble/ed25519";
import { Button, LinkButton } from "./ui/Button";
import { HTabs, Segmented } from "./ui/Tabs";
import { SearchInput, Switch, Select } from "./ui/Field";
import { StatusBadge, Tag } from "./ui/Badge";
import { Drawer, Dialog, PromptModal } from "./ui/Overlay";
import { SemanticDiff, CodePanel, LocationBar, OldTok, NewTok } from "./ui/SemanticDiff";
import { createPasskey, getPasskey } from "./webauthn";
import { hlToHtml, wordDiff, type Seg } from "./highlight";
import { Markdown } from "./markdown";
import { RichText } from "./ui/RichText";

// Syntax-highlighted code fragment (hljs HTML). Used across the diff viewer.
const Hl = ({ text, path }: { text: string; path: string }) => <span dangerouslySetInnerHTML={{ __html: hlToHtml(text, path) }} />;
// Render word-diff segments: unchanged parts syntax-highlighted, changed parts tinted (removed/added).
const wdRender = (segs: Seg[], path: string, side: "old" | "new") =>
  segs.map((s, i) => s.changed
    ? <span key={i} className={side === "old" ? "bg-fault-wash text-fault-text rounded-[3px]" : "bg-clear-wash text-clear-text font-semibold rounded-[3px]"} style={{ padding: "1px 2px" }}>{s.text}</span>
    : <Hl key={i} text={s.text} path={path} />);

const hexToBytes = (h: string) => Uint8Array.from((h.match(/../g) ?? []).map((x) => parseInt(x, 16)));
const bytesToHex = (b: Uint8Array) => [...b].map((x) => x.toString(16).padStart(2, "0")).join("");

// ── in-app modal system (replaces window.prompt/confirm/alert) ───────────────
// Promise-based so call sites stay simple: `await uiPrompt(...)`, `await uiConfirm(...)`, `uiAlert(...)`.
// A module-level bridge lets both App and ReviewPage reach the one modal host that App renders.
type ModalReq =
  | { kind: "prompt"; title: string; label?: string; placeholder?: string; initial?: string; sanitize?: (s: string) => string; check?: "account" | "username"; confirmLabel?: string; optional?: boolean }
  | { kind: "confirm"; title: string; body?: string; danger?: boolean; confirmLabel?: string }
  | { kind: "alert"; title: string; body?: string };
let _pushModal: ((req: ModalReq, resolve: (v: unknown) => void) => void) | null = null;
const uiPrompt = (o: Omit<Extract<ModalReq, { kind: "prompt" }>, "kind">) =>
  new Promise<string | null>((res) => (_pushModal ? _pushModal({ kind: "prompt", ...o }, (v) => res(v as string | null)) : res(null)));
const uiConfirm = (o: Omit<Extract<ModalReq, { kind: "confirm" }>, "kind">) =>
  new Promise<boolean>((res) => (_pushModal ? _pushModal({ kind: "confirm", ...o }, (v) => res(!!v)) : res(false)));
const uiAlert = (title: string, body?: string) =>
  new Promise<void>((res) => (_pushModal ? _pushModal({ kind: "alert", title, body }, () => res()) : res()));
const sanitizeHandle = (s: string) => s.replace(/\s+/g, "_");

// Compact relative time ("3h ago") from a unix seconds timestamp.
const timeAgo = (unix: number) => {
  if (!unix) return "";
  const s = Math.max(1, Math.floor(Date.now() / 1000 - unix));
  const steps: [number, string][] = [[60, "s"], [60, "m"], [24, "h"], [30, "d"], [12, "mo"], [Infinity, "y"]];
  let v = s, u = "s";
  for (const [div, unit] of steps) { if (v < div) { u = unit; break; } v = Math.floor(v / div); u = unit; }
  return `${v}${u} ago`;
};

// Parse a path into view state. Used both to seed initial state synchronously (so the first render —
// and its data fetches — already match the URL, no default-tenant race on deep links) and by the
// router on navigate/popstate.
type RouteState = {
  view: "home" | "repo";
  authPage: "login" | "signup" | "account" | "profile" | null;
  orgHandle: string | null;
  tenant: string;
  issueRepo: string;
  tab: RepoTab;
  openIssue: number | null;
  openPr: number | null;
};
type RepoTab = "issues" | "prs" | "files" | "graph" | "settings";
function parseRoute(path: string): RouteState {
  const seg = path.split("?")[0].split("/").filter(Boolean).map(decodeURIComponent);
  const r: RouteState = { view: "home", authPage: null, orgHandle: null, tenant: "", issueRepo: "", tab: "issues", openIssue: null, openPr: null };
  if (seg[0] === "login" || seg[0] === "signup") { r.authPage = seg[0]; return r; }
  if (seg[0] === "settings") { r.authPage = "account"; return r; }
  if (seg[0] === "me") { r.authPage = "profile"; return r; }
  if (seg[0] === "orgs" && seg[1]) { r.orgHandle = seg[1]; return r; }
  const [t, rp, s, n] = seg;
  if (t && rp) {
    r.view = "repo"; r.tenant = t; r.issueRepo = rp;
    if (s === "settings") r.tab = "settings";
    else if (s === "files" || s === "tree" || s === "code") r.tab = "files";
    else if (s === "graph") r.tab = "graph";
    else if (s === "issues" && n) { r.openIssue = Number(n); r.tab = "issues"; }
    else if (s === "voyages" && n) { r.openPr = Number(n); r.tab = "prs"; }
    else if (s === "voyages") r.tab = "prs";
  }
  return r;
}

// The published demo-owner secret (mirrors hull-server's DEMO_OWNER_SECRET). "Sign in as demo" signs
// the login challenge with this key — real signature auth, just a publicly-known demo credential.
const DEMO_OWNER_SECRET = "68756c6c2d64656d6f2d6f776e65722d6b65792d64656d6f2d6f6e6c79212121";

// Mirrors hull-server's activity model.
type RepoActivity = {
  tenant: string;
  repo: string;
  score: number;
  last_ts: number;
  active_actors: string[];
  hot_files: string[];
};

type ActivityEvent =
  | { kind: "agent_brief"; actor: string; repo: string; file: string; task: string; ts: number }
  | { kind: "lesson"; repo: string; file: string; lesson: string; ts: number }
  | { kind: "push"; actor: string; repo: string; change: string; ts: number }
  | { kind: "issue"; repo: string; number: number; action: string; actor: string; ts: number };

type Actor = { id: string; handle: string; kind: "human" | "agent"; accountable: boolean; human_root: string | null; email?: string };
type PR = { number: number; title: string; author: string; changes: string[]; verification: string; state: string; reviewers: string[] };
type Finding = { path: string; line?: number; severity: string; note: string };
type ClaimEv = { kind: string; detail: string; supports: boolean };
type LedgerSnap = { change: string; claims: { id: string; text: string; source: string; status: string; evidence: ClaimEv[] }[]; unclaimed?: string[] };
type Review = { id: string; target: string; reviewer: string; verdict: string; summary: string; findings: Finding[]; ledger?: LedgerSnap; artifact_id?: string; created_unix?: number };
type CodeRef = { repo: string; blob: string; path: string; line_start: number; line_end?: number };
type Issue = {
  number: number;
  title: string;
  body: string;
  author: string;
  assignees: string[];
  status: { state: string; reason?: string };
  code_refs: CodeRef[];
  labels: string[];
  resolved_by?: string;
  linked_prs?: string[];
};

// Deterministic GitHub-style identicon: a symmetric 5×5 pixel pattern (left half mirrored) in a hue
// derived from the id, on a neutral tile. Shape by kind (humans round, agents/orgs a rounded square).
const Avatar = ({ id, handle, kind, size = 22 }: { id?: string; handle?: string; kind?: string; size?: number }) => {
  const seed = id || handle || "?";
  let h = 2166136261;
  for (let i = 0; i < seed.length; i++) { h ^= seed.charCodeAt(i); h = Math.imul(h, 16777619); }
  const hash = h >>> 0;
  const color = `hsl(${hash % 360} 58% ${kind === "agent" ? 52 : 46}%)`;
  const radius = kind === "agent" ? size * 0.28 : kind === "organization" ? size * 0.22 : "50%";
  const pad = size * 0.12;
  const cell = (size - pad * 2) / 5;
  const rects: React.ReactNode[] = [];
  // 3 unique columns × 5 rows → 15 bits from the hash; columns 3,4 mirror columns 1,0.
  for (let col = 0; col < 3; col++) for (let row = 0; row < 5; row++) {
    if ((hash >> (col * 5 + row)) & 1) {
      const y = pad + row * cell;
      rects.push(<rect key={`${col}-${row}`} x={pad + col * cell} y={y} width={cell + 0.5} height={cell + 0.5} fill={color} />);
      if (col < 2) rects.push(<rect key={`m${col}-${row}`} x={pad + (4 - col) * cell} y={y} width={cell + 0.5} height={cell + 0.5} fill={color} />);
    }
  }
  return (
    <span className="inline-block bg-rule2 shrink-0 select-none overflow-hidden align-middle" style={{ width: size, height: size, borderRadius: radius }} title={handle} aria-hidden>
      <svg width={size} height={size} viewBox={`0 0 ${size} ${size}`}>{rects}</svg>
    </span>
  );
};
// Renders children; on any render error, shows `fallback` instead (used to guard the third-party
// @pierre/diffs renderer so a failure degrades to our built-in viewer rather than crashing the page).
class Boundary extends Component<{ fallback: React.ReactNode; children: React.ReactNode }, { err: boolean }> {
  state = { err: false };
  static getDerivedStateFromError() { return { err: true }; }
  render() { return this.state.err ? this.props.fallback : this.props.children; }
}
// A rich Picker option for a user/actor — avatar + username + email (or kind), so you pick the right person.
const actorOption = (a: { id: string; handle: string; kind?: string; email?: string }) => ({
  value: a.id,
  label: a.handle,
  sub: (a.email && a.email.trim()) || (a.kind === "agent" ? "agent" : "human"),
  avatar: <Avatar id={a.id} handle={a.handle} kind={a.kind} size={24} />,
});

// An issue label rendered as a neutral tag (like the rest of the UI) with a small colour dot for
// identity — hue deterministically derived from the label text.
// Perceived luminance of a #rrggbb colour → pick black or white text so a label always reads clearly.
const hexLum = (hex: string): number => {
  const m = /^#?([0-9a-fA-F]{6})$/.exec(hex || "");
  if (!m) return 0.5;
  const n = parseInt(m[1], 16);
  return (0.299 * ((n >> 16) & 255) + 0.587 * ((n >> 8) & 255) + 0.114 * (n & 255)) / 255;
};
const contrastText = (hex: string) => (hexLum(hex) > 0.6 ? "#111827" : "#ffffff");
const randomHexColor = () => "#" + Math.floor(Math.random() * 0xffffff).toString(16).padStart(6, "0");
const Label = ({ name, color, icon }: { name: string; color?: string; icon?: string }) => {
  const c = color || "#8b949e";
  return (
    <span className="inline-flex items-center gap-1 text-[12px] font-semibold px-1.5 py-[2px] rounded-badge" style={{ background: c, color: contrastText(c) }}>
      {icon ? <span className="leading-none">{icon}</span> : null}{name}
    </span>
  );
};
// Presets + emoji icons for configuring labels; custom hex + a random roll are also offered.
const LABEL_COLORS = ["#d73a4a", "#e99695", "#fbca04", "#0e8a16", "#006b75", "#1d76db", "#0052cc", "#5319e7", "#b60205", "#c5def5", "#bfdadc", "#8b949e"];
const LABEL_ICONS = ["🐛", "✨", "📝", "🔥", "⚠️", "🚀", "🧹", "🔒", "💡", "📦", "🎨", "⚡", "❓", "🚧"];
type RepoLabel = { name: string; color: string; icon?: string };
// Shared label editor — used by BOTH repo settings and org defaults. Pick an emoji icon, a preset OR
// custom OR random colour, name it, preview it live, add/remove.
function LabelEditor({ labels, onChange }: { labels: RepoLabel[]; onChange: (l: RepoLabel[]) => void }) {
  const [draft, setDraft] = useState<RepoLabel>({ name: "", color: LABEL_COLORS[0], icon: "" });
  const add = () => { const n = draft.name.trim(); if (!n || labels.some((x) => x.name === n)) return; onChange([...labels, { name: n, color: draft.color, icon: draft.icon }]); setDraft({ name: "", color: LABEL_COLORS[0], icon: "" }); };
  const chip = "w-7 h-7 rounded-ctl border grid place-items-center text-[14px] transition-colors";
  return (
    <div className="grid gap-3">
      <div className="flex flex-wrap gap-1.5">
        {labels.map((l) => (
          <span key={l.name} className="group inline-flex items-center gap-0.5"><Label name={l.name} color={l.color} icon={l.icon} /><button className="text-muted hover:text-fault-text cursor-pointer opacity-0 group-hover:opacity-100 transition-opacity px-0.5" title="remove" onClick={() => onChange(labels.filter((x) => x.name !== l.name))}>×</button></span>
        ))}
        {labels.length === 0 && <span className="text-[12.5px] text-muted">none yet</span>}
      </div>
      <div className="grid gap-2.5 border border-rule2 rounded-ctl p-3 bg-paper/40 max-w-[560px]">
        <div className="flex items-center gap-1.5 flex-wrap">
          <span className="text-[11.5px] text-muted w-10 flex-none">Icon</span>
          <button type="button" title="no icon" onClick={() => setDraft((d) => ({ ...d, icon: "" }))} className={`${chip} text-[10px] ${draft.icon === "" ? "border-body text-ink" : "border-rule text-muted hover:border-dim"}`}>—</button>
          {LABEL_ICONS.map((ic) => <button key={ic} type="button" onClick={() => setDraft((d) => ({ ...d, icon: ic }))} className={`${chip} ${draft.icon === ic ? "border-body bg-surface" : "border-rule hover:border-dim"}`}>{ic}</button>)}
        </div>
        <div className="flex items-center gap-1.5 flex-wrap">
          <span className="text-[11.5px] text-muted w-10 flex-none">Color</span>
          {LABEL_COLORS.map((c) => <button key={c} type="button" onClick={() => setDraft((d) => ({ ...d, color: c }))} className={`w-6 h-6 rounded-full transition-transform ${draft.color.toLowerCase() === c ? "ring-2 ring-offset-1 ring-body scale-110" : "hover:scale-110"}`} style={{ background: c }} />)}
          <label className="w-7 h-7 rounded-ctl border border-rule overflow-hidden cursor-pointer relative" title="custom color" style={{ background: draft.color }}><input type="color" value={draft.color} onChange={(e) => setDraft((d) => ({ ...d, color: e.target.value }))} className="absolute inset-0 opacity-0 cursor-pointer" /></label>
          <button type="button" onClick={() => setDraft((d) => ({ ...d, color: randomHexColor() }))} className="h-7 px-2 rounded-ctl border border-rule text-[12px] text-dim hover:text-ink hover:border-dim inline-flex items-center gap-1">🎲 random</button>
        </div>
        <div className="flex items-center gap-2 flex-wrap">
          <input className="box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[13px] text-ink outline-none focus:border-body placeholder:text-faint w-[180px]" placeholder="label name" value={draft.name} onChange={(e) => setDraft((d) => ({ ...d, name: e.target.value }))} onKeyDown={(e) => { if (e.key === "Enter") add(); }} />
          {draft.name.trim() && <Label name={draft.name.trim()} color={draft.color} icon={draft.icon} />}
          <Button size="sm" className="ml-auto" disabled={!draft.name.trim()} onClick={add}>Add label</Button>
        </div>
      </div>
    </div>
  );
}

// ── small token-only layout atoms (not controls — controls come from ./ui) ──────────
const Card = ({ children, className = "", id }: { children: React.ReactNode; className?: string; id?: string }) => (
  <div id={id} className={`bg-surface border border-rule rounded-card overflow-hidden ${className}`}>{children}</div>
);
const SectionHeader = ({ label, right }: { label: string; right?: React.ReactNode }) => (
  <div className="flex items-center justify-between gap-3 px-5 py-3.5 border-b border-rule2">
    <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted">{label}</span>
    {right}
  </div>
);
// An eyebrow label that sits directly on the page ground (no card), above a section.
const Eyebrow = ({ label, right }: { label: string; right?: React.ReactNode }) => (
  <div className="flex items-baseline justify-between gap-3 mb-2.5">
    <span className="text-[11px] font-semibold uppercase tracking-[0.09em] text-muted">{label}</span>
    {right && <span className="text-[12.5px] text-muted">{right}</span>}
  </div>
);
// A compact sidebar metadata module (About / Delivery / Autonomy …).
const Module = ({ title, children, tone = "" }: { title: string; children: React.ReactNode; tone?: string }) => (
  <div className="bg-surface border border-rule rounded-card">
    <div className="px-4 py-2.5 border-b border-rule2 flex items-center justify-between">
      <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted">{title}</span>
      {tone && <span className="w-1.5 h-1.5 rounded-full" style={{ background: tone }} />}
    </div>
    <div className="px-4 py-3.5 grid gap-2.5 text-[13px]">{children}</div>
  </div>
);
// label-left / value-right stat line inside a Module.
const Stat = ({ k, v }: { k: React.ReactNode; v: React.ReactNode }) => (
  <div className="flex items-baseline justify-between gap-3">
    <span className="text-muted">{k}</span>
    <span className="font-medium text-body tabular-nums text-right">{v}</span>
  </div>
);

// A click-to-open popover anchored under its trigger. Closes on outside-click or Escape. Used for the
// header "checks" summary, so the landing gate is reachable from the top of the page.
function Popover({ trigger, children, align = "left", width = 300, direction = "down", block = false, onToggle }: { trigger: (open: boolean) => React.ReactNode; children: React.ReactNode; align?: "left" | "right"; width?: number; direction?: "down" | "up"; block?: boolean; onToggle?: (open: boolean) => void }) {
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
function Picker({ value, onChange, options, placeholder = "Select…", width = 220, size = "md", block = false, direction = "down", className = "", searchable: searchableProp }: { value: string; onChange: (v: string) => void; options: PickerOption[]; placeholder?: string; width?: number; size?: "sm" | "md"; block?: boolean; direction?: "down" | "up"; className?: string; searchable?: boolean }) {
  const cur = options.find((o) => o.value === value);
  const h = size === "sm" ? "h-ctl-sm text-xs" : "h-ctl text-[13px]";
  const [q, setQ] = useState("");
  const searchable = searchableProp ?? options.length >= 8;
  const ql = q.trim().toLowerCase();
  const filtered = ql ? options.filter((o) => o.label.toLowerCase().includes(ql) || (o.sub ?? "").toLowerCase().includes(ql)) : options;
  const rich = options.some((o) => o.avatar || o.sub);
  return (
    <Popover align="left" width={width} block={block} direction={direction} onToggle={(o) => { if (!o) setQ(""); }} trigger={(open) => (
      <span className={`inline-flex items-center justify-between gap-2 ${h} px-2.5 rounded-ctl border bg-surface transition-colors ${block ? "w-full" : ""} ${open ? "border-body" : "border-ctl hover:border-dim"} ${className}`}>
        <span className={`truncate ${cur ? "text-ink" : "text-faint"}`}>{cur?.label ?? placeholder}</span>
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className={`text-muted flex-none transition-transform ${open ? "rotate-180" : ""}`}><polyline points="6 9 12 15 18 9" /></svg>
      </span>
    )}>
      {searchable && (
        <div className="p-1.5 border-b border-rule2 sticky top-0 bg-surface" onClick={(e) => e.stopPropagation()}>
          <input autoFocus value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search…" className="w-full box-border h-ctl-sm px-2 rounded-ctl-sm border border-ctl bg-surface font-sans text-[12.5px] text-ink outline-none focus:border-body placeholder:text-faint" />
        </div>
      )}
      <div className="py-1 max-h-[280px] overflow-y-auto overflow-x-hidden">
        {filtered.length === 0 && <div className="px-3 py-1.5 text-[12.5px] text-muted">{ql ? "no matches" : "none available"}</div>}
        {filtered.map((o) => (rich ? (
          <button key={o.value} type="button" title={o.label} onClick={() => onChange(o.value)} className={`w-full text-left px-2.5 py-1.5 flex items-center gap-2.5 hover:bg-paper ${o.value === value ? "bg-paper" : ""}`}>
            {o.avatar ? <span className="flex-none">{o.avatar}</span> : null}
            <span className="min-w-0 flex-1">
              <span className={`block text-[13px] truncate ${o.value === value ? "font-medium text-ink" : "text-body"}`}>{o.label}</span>
              {o.sub && <span className="block text-[11.5px] text-muted truncate">{o.sub}</span>}
            </span>
            {o.value === value && <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round" className="text-steel-text flex-none"><polyline points="20 6 9 17 4 12" /></svg>}
          </button>
        ) : (
          <button key={o.value} type="button" title={o.label} onClick={() => onChange(o.value)} className={`w-full text-left px-3 py-1.5 text-[13px] truncate hover:bg-paper ${o.value === value ? "bg-paper font-medium text-ink" : "text-body"}`}>{o.label}</button>
        )))}
      </div>
    </Popover>
  );
}

// A cohesive split button: a primary action + a dropdown, sharing ONE dark/muted skin so a disabled
// submit never leaves a stray bright chevron. The chevron stays clickable while the submit is disabled
// (to pick a verdict / mode / secondary action). Shared by every composer — issues AND pull requests.
function SplitButton({ label, icon, disabled, onSubmit, menu, menuWidth = 252 }: { label: React.ReactNode; icon?: React.ReactNode; disabled?: boolean; onSubmit: () => void; menu: React.ReactNode; menuWidth?: number }) {
  return (
    <div className={`flex-none inline-flex h-ctl rounded-ctl overflow-hidden border ${disabled ? "border-ctl" : "border-ink"}`}>
      <button type="button" disabled={disabled} onClick={onSubmit}
        className={`inline-flex items-center gap-1.5 px-3.5 text-[13px] font-semibold whitespace-nowrap transition-colors ${disabled ? "bg-paper text-faint cursor-not-allowed" : "bg-ink text-surface hover:brightness-110"}`}>
        {icon}{label}
      </button>
      <Popover align="right" width={menuWidth} direction="up" trigger={(open) => (
        <span className={`inline-flex items-center h-full px-1.5 border-l cursor-pointer transition-[filter,background-color] ${disabled ? "bg-paper text-muted border-ctl hover:text-ink" : "bg-ink text-surface border-l-white/25 hover:brightness-110"} ${open ? "brightness-110" : ""}`}>
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" className={`transition-transform ${open ? "rotate-180" : ""}`}><polyline points="6 9 12 15 18 9" /></svg>
        </span>
      )}>
        {menu}
      </Popover>
    </div>
  );
}

// Centered modal shell (backdrop + card + header). Closes on backdrop click / ✕ / Escape.
function ModalShell({ title, onClose, children, width = 480 }: { title: string; onClose: () => void; children: React.ReactNode; width?: number }) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);
  return (
    <>
      <div className="fixed inset-0 z-40 bg-ink/40 animate-bd-in" onClick={onClose} />
      <div style={{ width }} className="fixed left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 z-50 max-w-[93vw] max-h-[88vh] overflow-auto bg-surface rounded-card shadow-modal animate-ov-in">
        <div className="flex items-center justify-between px-5 py-3.5 border-b border-rule2 sticky top-0 bg-surface">
          <h2 className="text-[15px] font-semibold">{title}</h2>
          <button onClick={onClose} className="w-6 h-6 grid place-items-center rounded-ctl text-muted hover:text-ink hover:bg-paper" aria-label="close">✕</button>
        </div>
        <div className="px-5 py-4">{children}</div>
      </div>
    </>
  );
}
// A labelled field wrapper for modals.
const Field = ({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) => (
  <div className="grid gap-1.5">
    <label className="text-[12.5px] font-semibold text-body">{label}{hint && <span className="text-faint font-normal"> · {hint}</span>}</label>
    {children}
  </div>
);
const modalInput = "w-full box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[13.5px] text-ink outline-none focus:border-body placeholder:text-faint";

// New repository modal: owner/name shown inline (owner ∕ name), a live name-availability check like
// org handles, a Public/Unlisted/Private dropdown, and the default branch.
function NewRepoModal({ accounts, onClose, onCreate }: { accounts: string[]; onClose: () => void; onCreate: (p: { account: string; name: string; visibility: "public" | "private" | "unlisted"; branch: string }) => Promise<boolean> }) {
  const [account, setAccount] = useState(accounts[0] ?? "");
  const [name, setName] = useState("");
  const [visibility, setVisibility] = useState<"public" | "private" | "unlisted">("public");
  const [branch, setBranch] = useState("main");
  const [busy, setBusy] = useState(false);
  const [avail, setAvail] = useState<boolean | null>(null);
  const [checking, setChecking] = useState(false);
  useEffect(() => {
    const n = name.trim();
    if (!n || !account) { setAvail(null); setChecking(false); return; }
    setChecking(true);
    const t = setTimeout(() => {
      fetch(`/api/repos/available?account=${encodeURIComponent(account)}&name=${encodeURIComponent(n)}`)
        .then((r) => r.json()).then((d) => setAvail(!!d.available)).catch(() => setAvail(null)).finally(() => setChecking(false));
    }, 300);
    return () => clearTimeout(t);
  }, [name, account]);
  const ok = !!account && !!name.trim() && avail !== false;
  const submit = async () => { if (!ok || busy) return; setBusy(true); const done = await onCreate({ account, name: name.trim(), visibility, branch: branch.trim() || "main" }); setBusy(false); if (done) onClose(); };
  return (
    <ModalShell title="New repository" onClose={onClose} width={480}>
      <div className="grid gap-4">
        <Field label="Repository">
          <div className="flex items-stretch gap-1.5">
            <div className="flex-none min-w-[130px] max-w-[190px]"><Picker block value={account} onChange={setAccount} options={accounts.map((a) => ({ value: a, label: a }))} placeholder="owner" searchable /></div>
            <span className="flex items-center text-[16px] text-faint px-0.5">/</span>
            <input autoFocus className={`${modalInput} flex-1`} value={name} onChange={(e) => setName(sanitizeHandle(e.target.value))} onKeyDown={(e) => e.key === "Enter" && submit()} placeholder="my-service" spellCheck={false} />
          </div>
          {name.trim() && (
            <div className={`text-[12px] mt-1.5 ${checking ? "text-muted" : avail ? "text-clear-text" : "text-fault-text"}`}>
              {checking ? "checking…" : avail ? `✓ ${account}/${name.trim()} is available` : `✗ ${account}/${name.trim()} is taken`}
            </div>
          )}
        </Field>
        <Field label="Default branch"><input className={modalInput} value={branch} onChange={(e) => setBranch(e.target.value)} placeholder="main" spellCheck={false} /></Field>
        <Field label="Visibility">
          <Picker block value={visibility} onChange={(v) => setVisibility(v as "public" | "private" | "unlisted")}
            options={[{ value: "public", label: "Public · anyone can find it" }, { value: "unlisted", label: "Unlisted · anyone with the link" }, { value: "private", label: `Private · members of ${account || "the owner"}` }]} />
        </Field>
        <div className="flex justify-end gap-2 pt-1">
          <Button variant="secondary" size="sm" onClick={onClose}>Cancel</Button>
          <Button size="sm" disabled={!ok || busy} onClick={submit}>{busy ? "Creating…" : "Create repository"}</Button>
        </div>
      </div>
    </ModalShell>
  );
}

// New issue modal: repo, title, description, labels, assignees (item 11). agentActors listed first.
function NewIssueModal({ repos, defaultRepo, actors, onClose, onCreate }: { repos: { tenant: string; repo: string }[]; defaultRepo: string; actors: Actor[]; onClose: () => void; onCreate: (p: { repo: string; title: string; body: string; labels: string[]; assignees: string[] }) => Promise<boolean> }) {
  const [repo, setRepo] = useState(defaultRepo || (repos[0] ? `${repos[0].tenant}/${repos[0].repo}` : ""));
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [labels, setLabels] = useState<string[]>([]);
  const [repoLabels, setRepoLabels] = useState<{ name: string; color: string }[]>([]);
  const [assignees, setAssignees] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  // Labels are the repo's configured set (not free-form) — refetch when the repo changes.
  useEffect(() => {
    setLabels([]); setRepoLabels([]);
    const [t, r] = repo.split("/");
    if (t && r) fetch(`/api/repos/${encodeURIComponent(t)}/${r}/labels`).then((res) => (res.ok ? res.json() : { labels: [] })).then((d) => setRepoLabels(d.labels ?? [])).catch(() => {});
  }, [repo]);
  const ok = !!repo && !!title.trim();
  const submit = async () => { if (!ok || busy) return; setBusy(true); const done = await onCreate({ repo, title: title.trim(), body: body.trim(), labels, assignees }); setBusy(false); if (done) onClose(); };
  const sorted = [...actors].sort((a, b) => Number(b.kind === "agent") - Number(a.kind === "agent"));
  return (
    <ModalShell title="New issue" onClose={onClose} width={540}>
      <div className="grid gap-4">
        <Field label="Repository"><Picker block value={repo} onChange={setRepo} options={repos.map((r) => ({ value: `${r.tenant}/${r.repo}`, label: `${r.tenant}/${r.repo}` }))} placeholder="Choose a repository…" /></Field>
        <Field label="Title"><input autoFocus className={modalInput} value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Something an agent (or human) should fix" /></Field>
        <Field label="Description"><RichText value={body} onChange={setBody} rows={5} mentions={actors.map((a) => ({ handle: a.handle, kind: a.kind, email: a.email, avatar: <Avatar id={a.id} handle={a.handle} kind={a.kind} size={22} /> }))} placeholder="What's wrong, where, and how you'd know it's fixed. Agents read this first." /></Field>
        <Field label="Labels" hint="configured by the repo">
          {repoLabels.length === 0 ? (
            <div className="text-[12.5px] text-muted">This repo has no labels yet. A repo admin adds them in Settings → Labels.</div>
          ) : (
            <div className="flex flex-wrap gap-1.5">
              {repoLabels.map((l) => {
                const on = labels.includes(l.name);
                return (
                  <button key={l.name} type="button" onClick={() => setLabels((s) => (on ? s.filter((x) => x !== l.name) : [...s, l.name]))}
                    className={`inline-flex items-center gap-1 text-[12px] font-medium px-1.5 py-[3px] rounded-badge border transition-colors ${on ? "border-body bg-rule2 text-ink" : "border-rule text-dim hover:border-dim"}`}>
                    <span className="w-1.5 h-1.5 rounded-full flex-none" style={{ background: l.color || "#8b949e" }} />{l.name}
                    {on && <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round"><polyline points="20 6 9 17 4 12" /></svg>}
                  </button>
                );
              })}
            </div>
          )}
        </Field>
        <Field label="Assignees">
          <div className="flex flex-wrap items-center gap-1.5">
            {assignees.map((id) => { const a = actors.find((x) => x.id === id); return (
              <span key={id} className="inline-flex items-center gap-1 text-[12px] px-1.5 py-1 rounded-badge bg-rule2 text-dim">
                <Avatar id={id} handle={a?.handle ?? id} kind={a?.kind} size={14} />{a?.handle ?? id.slice(0, 7)}
                <button className="hover:text-fault-text" onClick={() => setAssignees((s) => s.filter((x) => x !== id))}>✕</button>
              </span>
            ); })}
            <Picker size="sm" width={240} direction="up" value="" placeholder="Add assignee…" onChange={(v) => setAssignees((s) => s.includes(v) ? s : [...s, v])}
              options={sorted.filter((a) => !assignees.includes(a.id)).map(actorOption)} />
          </div>
        </Field>
        <div className="flex justify-end gap-2 pt-1">
          <Button variant="secondary" size="sm" onClick={onClose}>Cancel</Button>
          <Button size="sm" disabled={!ok || busy} onClick={submit}>{busy ? "Creating…" : "Create issue"}</Button>
        </div>
      </div>
    </ModalShell>
  );
}

// A long list that only shows a slice until expanded, with a search box once it's big enough. Keeps
// a "705 unclaimed changes" list from swamping the page.
function SearchableList<T>({ items, renderItem, searchOf, initial = 15, searchThreshold = 20, placeholder = "Search…" }: { items: T[]; renderItem: (it: T, i: number) => React.ReactNode; searchOf: (it: T) => string; initial?: number; searchThreshold?: number; placeholder?: string }) {
  const [q, setQ] = useState("");
  const [expanded, setExpanded] = useState(false);
  const ql = q.trim().toLowerCase();
  const filtered = ql ? items.filter((it) => searchOf(it).toLowerCase().includes(ql)) : items;
  const shown = expanded ? filtered : filtered.slice(0, initial);
  return (
    <div className="grid gap-2">
      {items.length >= searchThreshold && (
        <div className="relative">
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="absolute left-2 top-1/2 -translate-y-1/2 text-faint pointer-events-none"><circle cx="11" cy="11" r="8" /><line x1="21" y1="21" x2="16.65" y2="16.65" /></svg>
          <input value={q} onChange={(e) => setQ(e.target.value)} placeholder={placeholder} className="w-full box-border h-ctl-sm pl-7 pr-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[12.5px] text-ink outline-none focus:border-body placeholder:text-faint" />
        </div>
      )}
      <div className="grid gap-1">{shown.map(renderItem)}</div>
      <div className="flex items-center gap-3">
        {filtered.length > shown.length && <button onClick={() => setExpanded(true)} className="text-[12px] font-medium text-steel-text hover:underline">Show all {filtered.length}{ql ? " matches" : ""} ↓</button>}
        {expanded && filtered.length > initial && <button onClick={() => setExpanded(false)} className="text-[12px] text-muted hover:text-ink">Show less ↑</button>}
        {ql && filtered.length === 0 && <span className="text-[12px] text-muted">no matches</span>}
      </div>
    </div>
  );
}

// Small semantic status glyph — a filled dot/check/x/question in a soft disc. Calmer than a full
// pill when a whole list of statuses is shown together.
const StatusDot = ({ tone, size = 18 }: { tone: "ok" | "bad" | "warn" | "wait" | "info"; size?: number }) => {
  const map = { ok: "bg-clear text-white", bad: "bg-fault text-white", warn: "bg-brass text-[#1D2125]", wait: "border border-ctl text-muted", info: "bg-steel text-white" } as const;
  return (
    <span className={`rounded-full grid place-items-center flex-none ${map[tone]}`} style={{ width: size, height: size }}>
      {tone === "ok" ? <svg width={size * 0.6} height={size * 0.6} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="4" strokeLinecap="round" strokeLinejoin="round"><polyline points="20 6 9 17 4 12" /></svg>
        : tone === "bad" ? <svg width={size * 0.55} height={size * 0.55} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="4" strokeLinecap="round"><line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" /></svg>
          : tone === "warn" ? <span className="font-bold leading-none" style={{ fontSize: size * 0.65 }}>!</span>
            : tone === "info" ? <span className="font-bold leading-none lowercase" style={{ fontSize: size * 0.62 }}>i</span>
              : <span className="font-bold leading-none" style={{ fontSize: size * 0.6 }}>?</span>}
    </span>
  );
};

// Small stroked line-icons — used in place of emoji so the UI reads as a product, not a chat message.
const Ico = ({ path, size = 14, fill = false }: { path: React.ReactNode; size?: number; fill?: boolean }) => (
  <svg width={size} height={size} viewBox="0 0 24 24" fill={fill ? "currentColor" : "none"} stroke={fill ? "none" : "currentColor"} strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="flex-none">{path}</svg>
);
const IcoCheck = ({ size = 14 }: { size?: number }) => <Ico size={size} path={<polyline points="20 6 9 17 4 12" />} />;
const IcoX = ({ size = 14 }: { size?: number }) => <Ico size={size} path={<><line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" /></>} />;
const IcoFlag = ({ size = 14 }: { size?: number }) => <Ico size={size} path={<><path d="M4 15s1-1 4-1 5 2 8 2 4-1 4-1V3s-1 1-4 1-5-2-8-2-4 1-4 1z" /><line x1="4" y1="22" x2="4" y2="15" /></>} />;
const IcoSparkle = ({ size = 14 }: { size?: number }) => <Ico size={size} path={<path d="M12 3l1.9 5.1L19 10l-5.1 1.9L12 17l-1.9-5.1L5 10l5.1-1.9z" />} />;
const IcoSearch = ({ size = 14 }: { size?: number }) => <Ico size={size} path={<><circle cx="11" cy="11" r="7" /><line x1="21" y1="21" x2="16.65" y2="16.65" /></>} />;
const IcoBulb = ({ size = 14 }: { size?: number }) => <Ico size={size} path={<><path d="M9 18h6" /><path d="M10 22h4" /><path d="M12 2a7 7 0 0 0-4 12.7c.6.5 1 1.3 1 2.1v.2h6v-.2c0-.8.4-1.6 1-2.1A7 7 0 0 0 12 2z" /></>} />;
const IcoGit = ({ size = 13 }: { size?: number }) => <Ico size={size} path={<><circle cx="6" cy="6" r="2.5" /><circle cx="6" cy="18" r="2.5" /><circle cx="18" cy="8" r="2.5" /><path d="M18 10.5c0 3-3 4-6 4H6M6 8.5v7" /></>} />;
const IcoExpand = ({ size = 13 }: { size?: number }) => <Ico size={size} path={<><polyline points="15 3 21 3 21 9" /><polyline points="9 21 3 21 3 15" /><line x1="21" y1="3" x2="14" y2="10" /><line x1="3" y1="21" x2="10" y2="14" /></>} />;

// The issue discussion thread + composer, as a STABLE top-level component so typing doesn't remount
// it (which would steal focus on every keystroke). All state is passed in from App.
type ThreadComment = { id: string; target: string; author: string; body: string; created_unix: number };
function IssueThread({ target, comments, issues, commentDraft, setCommentDraft, issueMode, setIssueMode, runIssueMode, canAct, tenant, repo, handleOf, kindOf, boxRef, mentions }: {
  target: string; comments: ThreadComment[]; issues: Issue[]; commentDraft: Record<string, string>; setCommentDraft: React.Dispatch<React.SetStateAction<Record<string, string>>>;
  issueMode: Record<string, string>; setIssueMode: React.Dispatch<React.SetStateAction<Record<string, string>>>; runIssueMode: (target: string, num: number, mode: string) => void;
  canAct: boolean; tenant: string; repo: string; handleOf: (id: string) => string; kindOf: (id: string) => string | undefined; boxRef: React.MutableRefObject<HTMLDivElement | null>; mentions: { handle: string; kind?: string }[];
}) {
  const msgs = comments.filter((c) => c.target === target).sort((a, b) => a.created_unix - b.created_unix);
  const num = Number(target.split(":")[1]);
  const issue = issues.find((i) => i.number === num);
  const isOpen = (issue?.status.state ?? "open") === "open";
  const draft = (commentDraft[target] ?? "").trim();
  const ic = (d: React.ReactNode) => <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">{d}</svg>;
  const modes: { id: string; label: string; hint: string; sub?: string; color: string; icon: React.ReactNode }[] = isOpen
    ? [
        { id: "comment", label: "Comment", hint: "Add a note", color: "text-dim", icon: ic(<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" />) },
        { id: "close", label: draft ? "Close with comment" : "Close issue", hint: "Resolved / completed", color: "text-clear-text", icon: ic(<><path d="M22 11.08V12a10 10 0 1 1-5.93-9.14" /><polyline points="22 4 12 14.01 9 11.01" /></>) },
        { id: "close_np", label: draft ? "Close with comment" : "Close issue", sub: "not planned", hint: "Won't fix / not planned", color: "text-muted", icon: ic(<><circle cx="12" cy="12" r="10" /><line x1="4.93" y1="4.93" x2="19.07" y2="19.07" /></>) },
      ]
    : [
        { id: "comment", label: "Comment", hint: "Add a note", color: "text-dim", icon: ic(<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" />) },
        { id: "reopen", label: draft ? "Comment & reopen" : "Reopen issue", hint: "Back to open", color: "text-clear-text", icon: ic(<><path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" /><path d="M3 3v5h5" /></>) },
      ];
  const mode = modes.find((m) => m.id === (issueMode[target] ?? "comment")) ?? modes[0];
  const disabled = !canAct || (mode.id === "comment" && !draft);
  return (
    <div className="grid gap-3">
      {msgs.map((c) => (
        <div className="flex gap-2.5" key={c.id}>
          <Avatar id={c.author} handle={handleOf(c.author)} kind={kindOf(c.author)} size={26} />
          <div className="flex-1 min-w-0 border border-rule2 rounded-ctl overflow-hidden">
            <div className="flex items-center gap-2 px-3 py-1.5 bg-paper border-b border-rule3 text-[12.5px]">
              <b className={kindOf(c.author) === "agent" ? "text-steel-text" : ""}>{handleOf(c.author)}</b>
              <span className="text-faint tabular-nums" title={new Date(c.created_unix * 1000).toLocaleString()}>{timeAgo(c.created_unix)}</span>
            </div>
            <Markdown text={c.body} linkBase={`/${encodeURIComponent(tenant)}/${repo}`} className="px-3 py-2 text-[13.5px] text-body" />
          </div>
        </div>
      ))}
      {msgs.length === 0 && <div className="text-[13px] text-muted">no comments yet</div>}
      <div className="mt-1 grid gap-2" ref={boxRef}>
        {canAct ? <RichText value={commentDraft[target] ?? ""} onChange={(v) => setCommentDraft((d) => ({ ...d, [target]: v }))} rows={3} mentions={mentions} linkBase={`/${encodeURIComponent(tenant)}/${repo}`} onSubmit={() => !disabled && runIssueMode(target, num, mode.id)} placeholder="Leave a comment…" />
          : <div className="border border-ctl rounded-ctl px-2.5 py-2 text-[13px] text-faint">sign in to comment</div>}
        <div className="flex justify-end">
          {canAct ? (
            <SplitButton disabled={disabled} onSubmit={() => runIssueMode(target, num, mode.id)}
              icon={mode.icon} label={`${mode.label}${mode.sub ? ` — ${mode.sub}` : ""}`}
              menu={
                <div className="py-1">
                  {modes.map((m) => (
                    <button key={m.id} type="button" onClick={() => setIssueMode((s) => ({ ...s, [target]: m.id }))}
                      className={`w-full text-left px-3 py-2 flex items-start gap-2.5 hover:bg-paper ${m.id === mode.id ? "bg-paper" : ""}`}>
                      <span className={`mt-[1px] flex-none ${m.color}`}>{m.icon}</span>
                      <span className="min-w-0 flex-1"><span className="block text-[13px] font-medium text-body leading-tight">{m.label}{m.sub ? ` — ${m.sub}` : ""}</span><span className="block text-[11.5px] text-muted leading-tight mt-0.5">{m.hint}</span></span>
                      {m.id === mode.id && <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round" className="text-steel-text mt-0.5 flex-none"><polyline points="20 6 9 17 4 12" /></svg>}
                    </button>
                  ))}
                </div>
              } />
          ) : <Button size="md" disabled>Comment</Button>}
        </div>
      </div>
    </div>
  );
}

// ── ⌘K command palette: search or jump to repos / issues / voyages / actions ──────
type CmdItem = { id: string; group: string; label: string; sublabel?: string; icon?: React.ReactNode; run: () => void };
function CommandPalette({ open, items, onClose }: { open: boolean; items: CmdItem[]; onClose: () => void }) {
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
      <div onClick={onClose} className="fixed inset-0 z-40 bg-ink/30 animate-bd-in" />
      <div className="fixed left-1/2 top-[11vh] -translate-x-1/2 z-50 w-[580px] max-w-[92vw] bg-surface rounded-[13px] shadow-modal overflow-hidden animate-ov-in border border-rule">
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
// verification/state → StatusBadge kind
/**
 * The home page IS a live projection of the fleet's coordination stream: repos rank by activity
 * (an agent starting work floats a repo up), and the event ticker shows what's happening now.
 */
export function App() {
  const [repos, setRepos] = useState<RepoActivity[]>([]);
  type HomeItem = { tenant: string; repo: string; number: number; title: string; author: string; reason: string; verification?: string };
  const [homeIssues, setHomeIssues] = useState<HomeItem[]>([]);
  const [homePrs, setHomePrs] = useState<HomeItem[]>([]);
  const [tenant, setTenant] = useState<string>(() => parseRoute(location.pathname).tenant || "tankrap");
  const feedRef = useRef<EventSource | null>(null);

  // ⌘K command palette + keyboard shortcuts
  const [cmdOpen, setCmdOpen] = useState(false);
  const [showShortcuts, setShowShortcuts] = useState(false);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") { e.preventDefault(); setCmdOpen((o) => !o); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // modal host: one place renders whatever uiPrompt/uiConfirm/uiAlert asked for
  const [modalReq, setModalReq] = useState<{ req: ModalReq; resolve: (v: unknown) => void } | null>(null);
  useEffect(() => {
    _pushModal = (req, resolve) => setModalReq({ req, resolve });
    return () => { _pushModal = null; };
  }, []);
  const closeModal = (v: unknown) => { modalReq?.resolve(v); setModalReq(null); };
  const checkAvail = async (kind: "account" | "username", v: string) => {
    const url = kind === "account" ? `/api/accounts/available?handle=${encodeURIComponent(v)}` : `/api/auth/available?username=${encodeURIComponent(v)}`;
    const d = await fetch(url).then((r) => r.json()).catch(() => ({ available: false }));
    return { available: !!d.available, hint: "taken" };
  };
  const uiModalNode = (() => {
    if (!modalReq) return null;
    const req = modalReq.req;
    if (req.kind === "prompt") {
      const check = req.check;
      return (
        <PromptModal open title={req.title} label={req.label} placeholder={req.placeholder} initial={req.initial ?? ""} sanitize={req.sanitize} optional={req.optional} confirmLabel={req.confirmLabel ?? "Confirm"}
          validate={check ? (v: string) => checkAvail(check, v) : undefined}
          onCancel={() => closeModal(null)} onConfirm={(v: string) => closeModal(v)} />
      );
    }
    if (req.kind === "confirm") {
      return <Dialog open title={req.title} body={req.body} cancelLabel="Cancel" actionLabel={req.confirmLabel ?? "Confirm"} onClose={() => closeModal(false)} onAction={() => closeModal(true)} />;
    }
    return <Dialog open title={req.title} body={req.body} cancelLabel={null} actionLabel="OK" onClose={() => closeModal(undefined)} onAction={() => closeModal(undefined)} />;
  })();

  // Theme: light-first (the design's default), dark via [data-theme] on <html>. Persisted.
  const [theme, setTheme] = useState<string>(
    () => localStorage.getItem("hull_theme") || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"),
  );
  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
    localStorage.setItem("hull_theme", theme);
  }, [theme]);

  // Issues for the selected repo under the selected tenant (M2). Click a repo card to switch.
  const [issueRepo, setIssueRepo] = useState<string>(() => parseRoute(location.pathname).issueRepo || "hull");
  const [issues, setIssues] = useState<Issue[]>([]);
  const [prov, setProv] = useState<Record<string, { change: string; intent: string; author: string }[]>>({});

  // Notifications recorded by the core Notifier plugin capability (poll).
  const [notifs, setNotifs] = useState<{ kind: string; to: string[]; summary: string; ts: number; broadcast?: boolean }[]>([]);
  const [showNotifs, setShowNotifs] = useState(false);
  // Unread tracking: the badge counts only notifications newer than what you've last opened.
  const [seenTs, setSeenTs] = useState<number>(() => Number(localStorage.getItem("hull_notif_seen") ?? 0));
  const openNotifs = () => {
    if (notifs.length) {
      const maxTs = Math.max(...notifs.map((n) => n.ts));
      setSeenTs(maxTs);
      localStorage.setItem("hull_notif_seen", String(maxTs));
    }
    setShowNotifs(true);
  };

  // Auth: sign in by proving possession of an actor's Ed25519 key → session token.
  const [token, setToken] = useState<string>(() => localStorage.getItem("hull_token") ?? "");
  // Your Ed25519 secret, held in memory for the session only (never localStorage) so you can sign
  // agent delegations locally. Set on sign-in, cleared on sign-out.
  const sessionSecret = useRef<string>("");
  const [me, setMe] = useState<{ id: string; handle: string; kind: string } | null>(null);
  const [secretInput, setSecretInput] = useState("");
  const authHeaders = (): Record<string, string> => (token ? { authorization: `Bearer ${token}` } : {});
  useEffect(() => {
    if (!token) {
      setMe(null);
      return;
    }
    fetch("/api/auth/me", { headers: authHeaders() })
      .then((r) => (r.ok ? r.json() : null))
      .then((m) => setMe(m))
      .catch(() => setMe(null));
  }, [token]);
  // Full profile — identity, accountability chain, org memberships.
  type Profile = {
    id: string; handle: string; kind: string; accountable: boolean; human_root: string | null;
    delegation: { principal: string; handle: string; kind: string; scope: string }[];
    memberships: { account: string; role: string }[];
  };
  const [profile, setProfile] = useState<Profile | null>(null);
  useEffect(() => {
    if (!token) { setProfile(null); return; }
    fetch("/api/me", { headers: { authorization: `Bearer ${token}` } })
      .then((r) => (r.ok ? r.json() : null))
      .then(setProfile)
      .catch(() => setProfile(null));
  }, [token, me]);
  // Register a fresh human identity and sign in with it — one click to a usable session.
  const registerAndSignIn = async () => {
    const handle = (await uiPrompt({ title: "New identity", label: "handle", initial: "you", sanitize: sanitizeHandle })) ?? "";
    if (!handle.trim()) return;
    const res = await fetch("/api/actors", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ handle: handle.trim(), kind: "human" }),
    });
    if (!res.ok) {
      uiAlert(await res.text());
      return;
    }
    const { secret_key } = await res.json();
    await signInWith(secret_key);
    uiAlert("Your secret key (save it to sign in again):\n\n" + secret_key);
  };
  const signIn = () => signInWith(secretInput.trim());
  const signInWith = async (secret: string) => {
    if (!secret) return;
    try {
      const skBytes = hexToBytes(secret);
      const actor = bytesToHex(await ed.getPublicKeyAsync(skBytes));
      const { nonce } = await fetch("/api/auth/challenge").then((r) => r.json());
      const sig = await ed.signAsync(new TextEncoder().encode(`hull-login:${nonce}`), skBytes);
      const res = await fetch("/api/auth/login", {
        method: "POST",
        headers: { "content-type": "application/json", ...authHeaders() },
        body: JSON.stringify({ actor, nonce, signature: bytesToHex(sig) }),
      });
      if (!res.ok) {
        uiAlert(await res.text());
        return;
      }
      const { token: t } = await res.json();
      localStorage.setItem("hull_token", t);
      setToken(t);
      // Keep the key in memory (not localStorage) for the session, so you can sign delegations
      // (createAgent) without Hull ever holding it. Cleared on sign-out.
      sessionSecret.current = secret;
      setSecretInput("");
    } catch (e) {
      uiAlert("bad secret key");
    }
  };
  const signOut = () => {
    localStorage.removeItem("hull_token");
    setToken("");
    setMe(null);
    sessionSecret.current = "";
  };
  // Mint an agent that cryptographically chains to you. The delegation hop is signed **client-side**
  // with your key (kept in memory for the session, never persisted), so Hull never sees the agent's
  // secret — it only stores a signed delegation it can verify. Matches the server's canonical
  // hop_message (identity::hop_message) exactly.
  const createAgent = async () => {
    if (!me) return uiAlert("Sign in to delegate an agent.");
    const handle = (await uiPrompt({ title: "Delegate an agent", label: "handle (chains to you)", initial: "agent:mine" })) ?? "";
    if (!handle.trim()) return;
    const scope = "*";
    const refresh = () => fetch("/api/actors").then((r) => r.json()).then((d) => setActors(d.actors ?? []));
    if (sessionSecret.current) {
      // Legacy key login: sign the delegation client-side so Hull never sees the agent's secret.
      const childSk = ed.utils.randomSecretKey();
      const childPub = bytesToHex(await ed.getPublicKeyAsync(childSk));
      const msg = new TextEncoder().encode(`hull-delegation:v1\nparent=${me.id}\nchild=${childPub}\nkind=agent\nscope=${scope}\nexpires=0`);
      const sig = bytesToHex(await ed.signAsync(msg, hexToBytes(sessionSecret.current)));
      const res = await fetch("/api/actors", {
        method: "POST",
        headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
        body: JSON.stringify({ handle: handle.trim(), kind: "agent", child_pub: childPub, scope, delegation_sig: sig }),
      });
      if (!res.ok) return uiAlert(await res.text());
      refresh();
      uiAlert(`Agent created, cryptographically delegated by you (Hull never saw this key).\n\nIts secret key — save it, the agent signs in with this:\n\n${bytesToHex(childSk)}`);
    } else {
      // Hosted account: Hull signs the delegation with your held key and returns the agent's secret.
      const res = await fetch("/api/actors", {
        method: "POST",
        headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
        body: JSON.stringify({ handle: handle.trim(), kind: "agent", scope }),
      });
      if (!res.ok) return uiAlert(await res.text());
      const d = await res.json();
      refresh();
      uiAlert(`Agent created — Hull signed the delegation on your behalf.\n\nIts secret key — save it, the agent signs in with this:\n\n${d.secret_key ?? "(stored)"}`);
    }
  };
  // ── passkey auth (signup / login) + account settings ─────────────────────
  // Full-screen auth/account pages, orthogonal to the home/repo views. null = normal app.
  const [authPage, setAuthPage] = useState<"login" | "signup" | "account" | "profile" | null>(() => parseRoute(location.pathname).authPage);
  const [authForm, setAuthForm] = useState({ username: "", email: "" });
  const [authBusy, setAuthBusy] = useState(false);
  const [authError, setAuthError] = useState("");
  // Live username availability on the signup form.
  const [usernameAvail, setUsernameAvail] = useState<{ available: boolean } | null>(null);
  useEffect(() => {
    if (authPage !== "signup") { setUsernameAvail(null); return; }
    const u = authForm.username.trim();
    if (!u) { setUsernameAvail(null); return; }
    let cancelled = false;
    const t = setTimeout(() => {
      fetch(`/api/auth/available?username=${encodeURIComponent(u)}`).then((r) => r.json()).then((d) => { if (!cancelled) setUsernameAvail({ available: !!d.available }); }).catch(() => {});
    }, 300);
    return () => { cancelled = true; clearTimeout(t); };
  }, [authForm.username, authPage]);
  const finishSession = (t: string) => {
    localStorage.setItem("hull_token", t);
    setToken(t);
    sessionSecret.current = "";
  };
  const signupPasskey = async () => {
    setAuthError("");
    if (!authForm.username.trim() || !authForm.email.trim()) { setAuthError("username and email are required"); return; }
    setAuthBusy(true);
    try {
      const start = await fetch("/api/auth/register/start", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(authForm) });
      if (!start.ok) { setAuthError(await start.text()); return; }
      const { flow_id, options } = await start.json();
      const credential = await createPasskey(options);
      const fin = await fetch("/api/auth/register/finish", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ flow_id, credential }) });
      if (!fin.ok) { setAuthError(await fin.text()); return; }
      const { token: t } = await fin.json();
      finishSession(t);
      setAuthForm({ username: "", email: "" });
      navigate("/");
    } catch (e: any) {
      setAuthError(e?.message || "passkey creation was cancelled");
    } finally {
      setAuthBusy(false);
    }
  };
  const loginPasskey = async (username: string) => {
    setAuthError("");
    if (!username.trim()) { setAuthError("enter your username"); return; }
    setAuthBusy(true);
    try {
      const start = await fetch("/api/auth/passkey/start", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ username }) });
      if (!start.ok) { setAuthError(await start.text()); return; }
      const { flow_id, options } = await start.json();
      const credential = await getPasskey(options);
      const fin = await fetch("/api/auth/passkey/finish", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ flow_id, credential }) });
      if (!fin.ok) { setAuthError(await fin.text()); return; }
      const { token: t } = await fin.json();
      finishSession(t);
      navigate("/");
    } catch (e: any) {
      setAuthError(e?.message || "passkey login was cancelled");
    } finally {
      setAuthBusy(false);
    }
  };
  // Account settings data
  type AccountInfo = { username: string; email: string; actor: string; passkeys: { id: string; name: string; created_unix: number }[] };
  const [account, setAccount] = useState<AccountInfo | null>(null);
  const loadAccount = () => {
    if (!token) return;
    fetch("/api/account", { headers: { authorization: `Bearer ${token}` } })
      .then((r) => (r.ok ? r.json() : null))
      .then(setAccount)
      .catch(() => {});
  };
  useEffect(() => { if (authPage === "account") loadAccount(); }, [authPage, token]);
  // Profile page: bio + a year of contributions (mine + my agents'), for the heatmap.
  type ProfileStats = { handle: string; bio: string; total: number; human_count: number; days: HeatDay[]; agents: { handle: string; count: number }[] };
  const [profileStats, setProfileStats] = useState<ProfileStats | null>(null);
  const [profileRepoQ, setProfileRepoQ] = useState("");
  const [profileTab, setProfileTab] = useState<"overview" | "repos" | "orgs">("overview");
  const [profileReadme, setProfileReadme] = useState<string | null>(null); // README of <me>/<me>, if any
  const [bioDraft, setBioDraft] = useState<string | null>(null); // non-null = editing
  const loadProfile = () => { if (token) fetch("/api/profile", { headers: authHeaders() }).then((r) => (r.ok ? r.json() : null)).then(setProfileStats).catch(() => {}); };
  useEffect(() => {
    if (authPage !== "profile" || !me) { setProfileReadme(null); return; }
    loadProfile();
    // GitHub-style "special repo": <username>/<username>. If it exists with a README.md, show it.
    const h = me.handle;
    fetch(`/api/repos/${encodeURIComponent(h)}/${encodeURIComponent(h)}/blob?path=README.md`, { headers: authHeaders() })
      .then((r) => (r.ok ? r.json() : null)).then((d) => setProfileReadme(d && !d.missing && !d.binary ? (d.text || "") : null)).catch(() => setProfileReadme(null));
    /* eslint-disable-next-line */
  }, [authPage, token, me?.handle]);
  const saveBio = async (bio: string) => {
    await fetch("/api/account", { method: "PUT", headers: { "content-type": "application/json", authorization: `Bearer ${token}` }, body: JSON.stringify({ bio }) });
    setBioDraft(null); loadProfile();
  };
  const saveAccount = async (patch: { username?: string; email?: string }) => {
    const res = await fetch("/api/account", { method: "PUT", headers: { "content-type": "application/json", authorization: `Bearer ${token}` }, body: JSON.stringify(patch) });
    if (!res.ok) return uiAlert(await res.text());
    loadAccount();
    fetch("/api/auth/me", { headers: { authorization: `Bearer ${token}` } }).then((r) => (r.ok ? r.json() : null)).then((m) => setMe(m)).catch(() => {});
  };
  const addPasskey = async () => {
    const name = (await uiPrompt({ title: "Add a passkey", label: "name", initial: "my device" })) ?? "";
    try {
      const start = await fetch("/api/account/passkeys/start", { method: "POST", headers: { authorization: `Bearer ${token}` } });
      if (!start.ok) return uiAlert(await start.text());
      const { flow_id, options } = await start.json();
      const credential = await createPasskey(options);
      const fin = await fetch("/api/account/passkeys/finish", { method: "POST", headers: { "content-type": "application/json", authorization: `Bearer ${token}` }, body: JSON.stringify({ flow_id, credential, name }) });
      if (!fin.ok) return uiAlert(await fin.text());
      loadAccount();
    } catch (e: any) {
      uiAlert(e?.message || "cancelled");
    }
  };
  const removePasskey = async (id: string) => {
    if (!(await uiConfirm({ title: "Remove passkey", body: "Remove this passkey?", danger: true, confirmLabel: "Remove" }))) return;
    const res = await fetch(`/api/account/passkeys/${encodeURIComponent(id)}`, { method: "DELETE", headers: { authorization: `Bearer ${token}` } });
    if (!res.ok) return uiAlert(await res.text());
    loadAccount();
  };

  // ── org management (members + teams) + repo settings ──────────────────────
  const [orgHandle, setOrgHandle] = useState<string | null>(() => parseRoute(location.pathname).orgHandle);
  type TeamT = { id: string; name: string; members: { actor: string; handle: string; role: string }[] };
  const [teams, setTeams] = useState<TeamT[]>([]);
  const [memberPick, setMemberPick] = useState({ actor: "", role: "write" });
  const [teamDraft, setTeamDraft] = useState("");
  const loadTeams = (accountId: string) => {
    fetch(`/api/accounts/${encodeURIComponent(accountId)}/teams`).then((r) => r.json()).then((d) => setTeams(d.teams ?? [])).catch(() => {});
  };
  const reloadAccounts = () => fetch("/api/accounts", { headers: authHeaders() }).then((r) => (r.ok ? r.json() : { accounts: [] })).then((d) => setAccounts(d.accounts ?? [])).catch(() => {});
  const orgApi = async (path: string, method: string, body?: unknown) => {
    const res = await fetch(`/api/accounts/${path}`, { method, headers: { "content-type": "application/json", authorization: `Bearer ${token}` }, body: body ? JSON.stringify(body) : undefined });
    if (!res.ok) { uiAlert(await res.text()); return false; }
    return true;
  };
  const addOrgMember = async (accountId: string, ref: { actor?: string; username?: string }, role: string) => {
    if (await orgApi(`${accountId}/members`, "POST", { ...ref, role })) reloadAccounts();
  };
  const removeOrgMember = async (accountId: string, actor: string) => {
    if (await orgApi(`${accountId}/members/${encodeURIComponent(actor)}`, "DELETE")) reloadAccounts();
  };
  const createTeam = async (accountId: string, name: string) => {
    if (!name.trim()) return;
    if (await orgApi(`${accountId}/teams`, "POST", { name: name.trim() })) loadTeams(accountId);
  };
  const deleteTeam = async (accountId: string, teamId: string) => {
    if (!(await uiConfirm({ title: "Delete team", body: "Delete this team?", danger: true, confirmLabel: "Delete" }))) return;
    if (await orgApi(`${accountId}/teams/${teamId}`, "DELETE")) loadTeams(accountId);
  };
  const teamAddMember = async (accountId: string, teamId: string, ref: { actor?: string; username?: string }, role: string) => {
    if (await orgApi(`${accountId}/teams/${teamId}/members`, "POST", { ...ref, role })) loadTeams(accountId);
  };
  const teamRemoveMember = async (accountId: string, teamId: string, actor: string) => {
    if (await orgApi(`${accountId}/teams/${teamId}/members/${encodeURIComponent(actor)}`, "DELETE")) loadTeams(accountId);
  };
  const createOrg = async () => {
    const handle = (await uiPrompt({ title: "New organization", label: "handle", placeholder: "e.g. acme", sanitize: sanitizeHandle, check: "account", confirmLabel: "Create" })) ?? "";
    if (!handle.trim()) return;
    const res = await fetch("/api/accounts", { method: "POST", headers: { "content-type": "application/json", authorization: `Bearer ${token}` }, body: JSON.stringify({ handle: handle.trim(), kind: "organization" }) });
    if (!res.ok) return uiAlert(await res.text());
    await reloadAccounts();
    navigate(`/orgs/${encodeURIComponent(handle.trim())}`);
  };
  // The "+" navbar menu opens these modals (New repo / New issue); New org still uses the prompt.
  const [newRepoOpen, setNewRepoOpen] = useState(false);
  const [newIssueOpen, setNewIssueOpen] = useState(false);
  const [importOpen, setImportOpen] = useState(false);
  const [importAcct, setImportAcct] = useState("");
  const doCreateRepo = async (p: { account: string; name: string; visibility: "public" | "private" | "unlisted"; branch: string }): Promise<boolean> => {
    if (!canAct) { uiAlert("Sign in to act."); return false; }
    const res = await fetch("/api/repos", { method: "POST", headers: { "content-type": "application/json", authorization: `Bearer ${token}` }, body: JSON.stringify({ account: p.account, name: sanitizeHandle(p.name), default_branch: p.branch }) });
    if (!res.ok) { uiAlert(await res.text()); return false; }
    const d = await res.json();
    if (p.visibility !== "public") await fetch(`/api/repos/${encodeURIComponent(d.tenant)}/${encodeURIComponent(d.name)}/settings`, { method: "PUT", headers: { "content-type": "application/json", authorization: `Bearer ${token}` }, body: JSON.stringify({ visibility: p.visibility }) });
    navigate(`/${encodeURIComponent(d.tenant)}/${encodeURIComponent(d.name)}`);
    return true;
  };
  const doCreateIssue = async (p: { repo: string; title: string; body: string; labels: string[]; assignees: string[] }): Promise<boolean> => {
    if (!canAct) { uiAlert("Sign in to act."); return false; }
    const slash = p.repo.indexOf("/");
    const t = p.repo.slice(0, slash), r = p.repo.slice(slash + 1);
    const res = await fetch(`/api/repos/${encodeURIComponent(t)}/${encodeURIComponent(r)}/issues`, { method: "POST", headers: { "content-type": "application/json", ...authHeaders() }, body: JSON.stringify({ title: p.title, author: actingAs, body: p.body, labels: p.labels, assignees: p.assignees }) });
    if (!res.ok) { uiAlert(await res.text()); return false; }
    navigate(`/${encodeURIComponent(t)}/${encodeURIComponent(r)}`);
    return true;
  };
  // GitHub connection is per-account (org). Import lives under an org you administer + have connected.
  type GhStatus = { connected: boolean; login?: string; provider?: string };
  const [ghStatus, setGhStatus] = useState<GhStatus | null>(null);
  const [importList, setImportList] = useState<string[] | null>(null);
  const [importBusy, setImportBusy] = useState("");
  const loadGh = (acctId: string) => {
    fetch(`/api/accounts/${encodeURIComponent(acctId)}/github`, { headers: authHeaders() })
      .then((r) => (r.ok ? r.json() : { connected: false }))
      .then(setGhStatus)
      .catch(() => setGhStatus({ connected: false }));
  };
  // Connect = redirect the admin to GitHub's install page, where THEY pick their org + the repos to
  // grant. We never list the App's installations, so you can't see or connect an org you don't admin.
  const connectGh = async (acctId: string) => {
    const res = await fetch(`/api/accounts/${encodeURIComponent(acctId)}/github/connect-url`, { method: "POST", headers: authHeaders() });
    if (!res.ok) return uiAlert(await res.text());
    const { url } = await res.json();
    window.location.href = url; // GitHub → /api/github/setup → back here, connected
  };
  const disconnectGh = async (acctId: string) => {
    if (!(await uiConfirm({ title: "Disconnect GitHub", body: "Disconnect GitHub from this org?", danger: true, confirmLabel: "Disconnect" }))) return;
    await fetch(`/api/accounts/${encodeURIComponent(acctId)}/github`, { method: "DELETE", headers: authHeaders() });
    setImportList(null);
    loadGh(acctId);
  };
  const openImport = async (acctId: string) => {
    setImportList([]);
    const d = await fetch(`/api/accounts/${encodeURIComponent(acctId)}/github/importable`, { headers: authHeaders() }).then((r) => (r.ok ? r.json() : { repos: [] })).catch(() => ({ repos: [] }));
    setImportList(d.repos ?? []);
  };
  const importRepo = async (acctId: string, source: string) => {
    const name = (await uiPrompt({ title: `Import ${source}`, label: "as (repository name)", initial: source.split("/").pop() ?? "", sanitize: sanitizeHandle, confirmLabel: "Import" })) ?? "";
    if (!name.trim()) return;
    setImportBusy(source);
    try {
      const res = await fetch(`/api/accounts/${encodeURIComponent(acctId)}/repos/import`, { method: "POST", headers: { "content-type": "application/json", authorization: `Bearer ${token}` }, body: JSON.stringify({ source, name: name.trim() }) });
      if (!res.ok) { uiAlert(await res.text()); return; }
      const d = await res.json();
      setImportList(null);
      navigate(`/${encodeURIComponent(d.tenant)}/${encodeURIComponent(d.name)}`);
    } finally {
      setImportBusy("");
    }
  };
  // repo settings
  type RepoSettings = { private: boolean; unlisted?: boolean; visibility?: "public" | "private" | "unlisted"; require_review_to_land: boolean; author_independence?: boolean; default_reviewers: { actor: string; handle: string }[]; team_access: { team: string; role: string }[]; labels?: RepoLabel[] };
  const [repoSettings, setRepoSettings] = useState<RepoSettings | null>(null);
  const [ownerRules, setOwnerRules] = useState<{ glob: string; owners: string[] }[]>([]);
  // The repo's configured labels (readable by any member) — drives issue label colors + the pickers.
  const [repoLabels, setRepoLabels] = useState<RepoLabel[]>([]);
  const labelColor = (name: string) => repoLabels.find((l) => l.name === name)?.color;
  const loadRepoSettings = () => {
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/settings`, { headers: authHeaders() }).then((r) => (r.ok ? r.json() : null)).then((d) => d && setRepoSettings(d)).catch(() => {});
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/owners`, { headers: authHeaders() }).then((r) => r.json()).then((d) => setOwnerRules(d.owners ?? [])).catch(() => {});
    if (orgAccountFor(tenant)) loadTeams(orgAccountFor(tenant)!.id);
  };
  const orgAccountFor = (handle: string) => accounts.find((a) => a.handle === handle);
  const saveRepoSettings = async (patch: Partial<{ private: boolean; visibility: "public" | "private" | "unlisted"; require_review_to_land: boolean; author_independence: boolean; default_reviewers: string[]; team_access: { team: string; role: string }[]; labels: RepoLabel[] }>) => {
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/settings`, { method: "PUT", headers: { "content-type": "application/json", authorization: `Bearer ${token}` }, body: JSON.stringify(patch) });
    if (!res.ok) return uiAlert(await res.text());
    setRepoSettings(await res.json());
  };

  // Registered actors (for display / handle resolution only — you cannot *act* as any of them).
  const [actors, setActors] = useState<Actor[]>([]);
  useEffect(() => {
    fetch("/api/actors")
      .then((r) => r.json())
      .then((d) => setActors(d.actors ?? []))
      .catch(() => {});
  }, []);
  const handleOf = (id: string) => actors.find((a) => a.id === id)?.handle ?? id.slice(0, 8);
  const kindOf = (id: string) => actors.find((a) => a.id === id)?.kind;
  // Human-readable label for an actor token that may be a known id, a bare pubkey, or a raw git-author
  // string ("Rando <rando@x>"). Resolves handles, strips emails, shortens naked pubkeys.
  const actorLabel = (a: string) => {
    const found = actors.find((x) => x.id === a);
    if (found) return found.handle;
    const named = a.replace(/\s*<[^>]*>/, "").trim();
    if (/^[0-9a-f]{16,}$/i.test(named)) return named.slice(0, 7);
    return named || a.slice(0, 7);
  };
  // You act only as your signed-in self. No token ⇒ no identity ⇒ writes are blocked (server 401s).
  const actingAs = me?.id ?? "";
  const canAct = !!me;
  // Quick filter across the current repo's issues / PRs (title, body, #number).
  const [q, setQ] = useState("");
  const matchQ = (s: string) => q.trim() === "" || s.toLowerCase().includes(q.trim().toLowerCase());

  // Notifications inbox, scoped to the acting actor (addressed-to-them + broadcasts). Polled.
  useEffect(() => {
    const load = () => {
      const url = actingAs ? `/api/notifications?actor=${encodeURIComponent(actingAs)}` : "/api/notifications";
      fetch(url).then((r) => r.json()).then((d) => setNotifs(d.notifications ?? [])).catch(() => {});
    };
    load();
    const t = setInterval(load, 4000);
    return () => clearInterval(t);
  }, [actingAs]);

  // Two views: Home (situation room) and a focused Repo view with Issues / PRs tabs.
  const [view, setView] = useState<"home" | "repo">(() => parseRoute(location.pathname).view);
  const [tab, setTab] = useState<RepoTab>(() => parseRoute(location.pathname).tab);
  useEffect(() => { if (tab === "settings") loadRepoSettings(); }, [tab, tenant, issueRepo]);
  const [issueView, setIssueView] = useState<"list" | "board">("list");
  type StateFilter = "open" | "closed" | "all";
  const [issueFilter, setIssueFilter] = useState<StateFilter>("open");
  const [prFilter, setPrFilter] = useState<StateFilter>("open");
  const [openIssue, setOpenIssue] = useState<number | null>(() => parseRoute(location.pathname).openIssue);
  // Default the issues/PRs repo to whatever's actually active, so it's never stuck on a stale name.
  useEffect(() => {
    if (repos.length && !repos.some((r) => r.repo === issueRepo)) setIssueRepo(repos[0].repo);
  }, [repos]);

  // Toggle keel-native provenance ("who/what touched this path") under a code-ref.
  const showWhy = async (key: string, path: string) => {
    if (prov[key]) {
      setProv(({ [key]: _drop, ...rest }) => rest);
      return;
    }
    const d = await fetch(
      `/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/why?path=${encodeURIComponent(path)}`,
    ).then((r) => r.json());
    setProv((p) => ({ ...p, [key]: d.provenance ?? [] }));
  };
  const loadIssues = () =>
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/issues`, { headers: authHeaders() })
      .then((r) => r.json())
      .then((d) => setIssues(d.issues ?? []))
      .catch(() => {});
  useEffect(() => {
    loadIssues();
    if (tenant && issueRepo) fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/labels`, { headers: authHeaders() }).then((r) => (r.ok ? r.json() : { labels: [] })).then((d) => setRepoLabels(d.labels ?? [])).catch(() => {});
  }, [tenant, issueRepo]);

  const issueAction = async (number: number, action: string, extra: Record<string, unknown> = {}) => {
    if (!canAct) return uiAlert("Sign in to act.");
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/issues/${number}`, {
      method: "PATCH",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ action, ...(action === "close" ? { reason: "completed" } : {}), ...extra }),
    });
    if (res.ok) loadIssues();
    else uiAlert(await res.text());
  };
  // Issue comment composer mode per target (Comment / Close with comment / Close as not planned / Reopen).
  const [issueMode, setIssueMode] = useState<Record<string, string>>({});
  const runIssueMode = async (target: string, num: number, mode: string) => {
    const hasDraft = (commentDraft[target] ?? "").trim().length > 0;
    // Post the typed comment first regardless of the action — reopen used to drop it.
    if (hasDraft) await postComment(target);
    if (mode === "reopen") return issueAction(num, "reopen");
    if (mode === "close") issueAction(num, "close", { reason: "completed" });
    else if (mode === "close_np") issueAction(num, "close", { reason: "not_planned" });
  };

  // Accounts / orgs (membership + roles).
  type Account = { id: string; handle: string; kind: string; repos: string[]; members: { actor: string; handle: string; role: string }[] };
  const [accounts, setAccounts] = useState<Account[]>([]);
  const orgAccount = accounts.find((a) => a.handle === (orgHandle ?? " "));
  // Org-level DEFAULT repo settings — inherited by every repo created afterward.
  const [orgDefaults, setOrgDefaults] = useState<RepoSettings | null>(null);
  const loadOrgDefaults = (acctId: string) => fetch(`/api/accounts/${encodeURIComponent(acctId)}/repo-defaults`, { headers: authHeaders() }).then((r) => (r.ok ? r.json() : null)).then((d) => d && setOrgDefaults(d)).catch(() => {});
  const saveOrgDefaults = async (acctId: string, patch: Partial<{ visibility: string; require_review_to_land: boolean; labels: RepoLabel[] }>) => {
    const res = await fetch(`/api/accounts/${encodeURIComponent(acctId)}/repo-defaults`, { method: "PUT", headers: { "content-type": "application/json", authorization: `Bearer ${token}` }, body: JSON.stringify(patch) });
    if (res.ok) setOrgDefaults(await res.json()); else uiAlert(await res.text());
  };
  useEffect(() => { if (orgAccount) { loadTeams(orgAccount.id); loadGh(orgAccount.id); loadOrgDefaults(orgAccount.id); setImportList(null); } }, [orgAccount?.id]);
  useEffect(() => {
    if (!token) { setAccounts([]); return; }
    fetch("/api/accounts", { headers: authHeaders() }).then((r) => (r.ok ? r.json() : { accounts: [] })).then((d) => setAccounts(d.accounts ?? [])).catch(() => {});
  }, [token]);

  // Server-side secret-scan findings for the selected repo.
  const [secrets, setSecrets] = useState<{ path: string; line: number; title: string; redacted: string }[]>([]);
  useEffect(() => {
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/security`, { headers: authHeaders() })
      .then((r) => r.json())
      .then((d) => setSecrets(d.secrets ?? []))
      .catch(() => {});
  }, [tenant, issueRepo, view]);

  // Pull requests for the selected repo.
  const [prs, setPrs] = useState<PR[]>([]);
  const loadPrs = () =>
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/prs`, { headers: authHeaders() })
      .then((r) => r.json())
      .then((d) => setPrs(d.prs ?? []))
      .catch(() => {});
  useEffect(() => {
    loadPrs();
  }, [tenant, issueRepo]);

  // Mirror status for the selected repo (external target + outbound pushes). Refreshes with prs so a
  // merge that mirrors out shows up.
  type Mirror = { target: string | null; outbound: { change: string; target: string; external_ref: string; ts: number }[] };
  const [mirror, setMirror] = useState<Mirror | null>(null);
  useEffect(() => {
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/mirror`, { headers: authHeaders() })
      .then((r) => r.json())
      .then((d) => setMirror(d))
      .catch(() => {});
  }, [tenant, issueRepo, view, prs]);

  const isTenantOwner = !!profile?.memberships.some((m) => m.account === tenant && (m.role === "owner" || m.role === "admin"));

  // Autonomy policy for the selected repo (tier T0–T3, resolved repo → account → instance).
  type Autonomy = { tier: string; source: string; protected_paths: string[] };
  const [autonomy, setAutonomy] = useState<Autonomy | null>(null);
  const loadAutonomy = () =>
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/autonomy`, { headers: authHeaders() })
      .then((r) => r.json())
      .then((d) => setAutonomy(d))
      .catch(() => {});
  useEffect(() => { loadAutonomy(); }, [tenant, issueRepo, view]);
  const setTier = async (tier: string) => {
    if (!canAct) return uiAlert("Sign in to act.");
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/autonomy`, {
      method: "PUT",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ tier }),
    });
    if (res.ok) loadAutonomy();
    else uiAlert(await res.text());
  };
  const TIERS: Record<string, string> = {
    t0: "Observe — no autonomous action",
    t1: "Review-required — agents auto-review; a human approves",
    t2: "Auto-approve low-risk — agent approve merges green, uncontradicted, non-protected changes",
    t3: "Autonomous — agent approve counts broadly (never protected paths)",
  };

  // Reviews (first-class), loaded per repo and filtered to a PR target.
  const [reviews, setReviews] = useState<Review[]>([]);
  const [openPr, setOpenPr] = useState<number | null>(() => parseRoute(location.pathname).openPr);

  // ── client-side routing: real URLs, back/forward, deep links ──────────────
  // Parse a path into view state. Routes:
  //   /                              situation room
  //   /:tenant/:repo                 repo · issues
  //   /:tenant/:repo/voyages         repo · voyages
  //   /:tenant/:repo/issues/:n       issue page
  //   /:tenant/:repo/voyages/:n      voyage page
  const applyPath = (path: string) => {
    const r = parseRoute(path);
    setAuthPage(r.authPage);
    setOrgHandle(r.orgHandle);
    setView(r.view);
    setTab(r.tab);
    setOpenIssue(r.openIssue);
    setOpenPr(r.openPr);
    if (r.tenant) setTenant(r.tenant);
    if (r.issueRepo) setIssueRepo(r.issueRepo);
  };
  const navigate = (path: string) => {
    if (path !== location.pathname) history.pushState({}, "", path);
    applyPath(path);
  };
  const repoBase = () => `/${encodeURIComponent(tenant)}/${encodeURIComponent(issueRepo)}`;
  // initial deep-link parse + browser back/forward. All in-app navigation goes through navigate()
  // (which pushState's), so no reactive URL-sync effect is needed — and having one would clobber the
  // deep-linked path on mount (stale state → replaceState("/")).
  useEffect(() => {
    applyPath(location.pathname);
    const onPop = () => applyPath(location.pathname);
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);
  const loadReviews = () =>
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/reviews`, { headers: authHeaders() })
      .then((r) => r.json())
      .then((d) => setReviews(d.reviews ?? []))
      .catch(() => {});
  useEffect(() => {
    loadReviews();
  }, [tenant, issueRepo]);

  // Discussion comments (the conversation layer over reviews).
  type Comment = { id: string; target: string; author: string; body: string; created_unix: number };
  const [comments, setComments] = useState<Comment[]>([]);
  const [commentDraft, setCommentDraft] = useState<Record<string, string>>({});
  const loadComments = () =>
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/comments`, { headers: authHeaders() })
      .then((r) => r.json())
      .then((d) => setComments(d.comments ?? []))
      .catch(() => {});
  useEffect(() => { loadComments(); }, [tenant, issueRepo]);
  // Post to any target — `pr:N` or `issue:N` — keyed by the target string so drafts don't collide.
  const postComment = async (target: string) => {
    if (!canAct) return uiAlert("Sign in to act.");
    const body = (commentDraft[target] ?? "").trim();
    if (!body) return;
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/comments`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ target, body }),
    });
    if (res.ok) { setCommentDraft((d) => ({ ...d, [target]: "" })); loadComments(); }
    else uiAlert(await res.text());
  };
  // The issue discussion thread is a stable top-level component (IssueThread) so typing in its
  // composer doesn't remount and steal focus. This closure just binds the current App state to it.
  const commentBoxRef = useRef<HTMLDivElement | null>(null);
  const threadProps = { comments, issues, commentDraft, setCommentDraft, issueMode, setIssueMode, runIssueMode, canAct, tenant, repo: issueRepo, handleOf, kindOf, boxRef: commentBoxRef, mentions: actors.map((a) => ({ handle: a.handle, kind: a.kind, email: a.email, avatar: <Avatar id={a.id} handle={a.handle} kind={a.kind} size={22} /> })) };
  const [autoReviewing, setAutoReviewing] = useState<number | null>(null);
  const requestReviewer = async (prNumber: number, reviewer: string) => {
    if (!canAct || !reviewer) return;
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/prs/${prNumber}/reviewers`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ reviewer }),
    });
    if (res.ok) loadPrs();
    else uiAlert(await res.text());
  };
  const autoReview = async (prNumber: number) => {
    if (!canAct) return uiAlert("Sign in to act.");
    setAutoReviewing(prNumber);
    try {
      // The server picks an independent agent reviewer — the client never names one (no impersonation).
      const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/prs/${prNumber}/auto-review`, {
        method: "POST",
        headers: { "content-type": "application/json", ...authHeaders() },
        body: JSON.stringify({}),
      });
      if (res.ok) {
        loadReviews();
        loadPrs();
      } else {
        uiAlert(await res.text());
      }
    } finally {
      setAutoReviewing(null);
    }
  };

  const closePr = async (number: number, reopen: boolean) => {
    if (!canAct) return uiAlert("Sign in to act.");
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/prs/${number}/close`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ reopen }),
    });
    if (res.ok) loadPrs();
    else uiAlert(await res.text());
  };
  const mergePr = async (number: number, force = false) => {
    if (!canAct) return uiAlert("Sign in to act.");
    if (force && !(await uiConfirm({ title: "Merge without a green gate?", body: "As an owner you can override the checks/approval gate. This bypasses a safety signal — use it only for a wedged or misconfigured check.", danger: true, confirmLabel: "Merge anyway" }))) return;
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/prs/${number}/merge`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ actor: actingAs, force }),
    });
    if (res.ok) loadPrs();
    else uiAlert(await res.text());
  };
  // What AI capabilities this instance can fulfill — hide AI actions it can't run.
  const [caps, setCaps] = useState<{ ai_fix: boolean; ai_review: boolean }>({ ai_fix: true, ai_review: true });
  useEffect(() => { fetch("/api/capabilities").then((r) => r.json()).then(setCaps).catch(() => {}); }, []);

  // Personalized home: the signed-in user's repos across every org they belong to, ranked by
  // activity. Not tied to a single tenant. `myAccounts` also scopes the live feed.
  const [myAccounts, setMyAccounts] = useState<string[]>([]);
  useEffect(() => {
    if (!token) { setRepos([]); setMyAccounts([]); setHomeIssues([]); setHomePrs([]); return; }
    const load = () =>
      fetch("/api/home", { headers: authHeaders() })
        .then((r) => (r.ok ? r.json() : { repos: [], accounts: [] }))
        .then((d) => { setRepos(d.repos ?? []); setMyAccounts(d.accounts ?? []); setHomeIssues(d.issues ?? []); setHomePrs(d.prs ?? []); })
        .catch(() => {});
    load();
    const t = setInterval(load, 3000);
    return () => clearInterval(t);
  }, [token]);

  // Live event stream over SSE, scoped to the user's accounts — drives live refresh of the open lists.
  useEffect(() => {
    if (myAccounts.length === 0) return;
    const es = new EventSource(`/api/feed?accounts=${encodeURIComponent(myAccounts.join(","))}`);
    feedRef.current = es;
    es.onmessage = (m) => {
      try {
        const ev = JSON.parse(m.data) as ActivityEvent;
        if (ev.kind === "issue") loadIssues(); // reflect new issues live
      } catch {
        /* ignore keep-alives */
      }
    };
    return () => es.close();
  }, [myAccounts.join(",")]);

  // ── full-screen auth / account pages ──────────────────────────────────────
  // keyboard shortcuts: g→h/i/p navigation, c to comment, ? for help, / for the palette
  useEffect(() => {
    let lastG = 0;
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.tagName === "SELECT" || t.isContentEditable)) return;
      if (e.metaKey || e.ctrlKey || e.altKey) return;
      if (e.key === "?") { e.preventDefault(); setShowShortcuts((s) => !s); return; }
      if (e.key === "/") { e.preventDefault(); setCmdOpen(true); return; }
      const now = Date.now();
      if (e.key === "g") { lastG = now; return; }
      if (now - lastG < 900 && lastG) {
        lastG = 0;
        if (e.key === "h") { e.preventDefault(); navigate("/"); }
        else if (e.key === "i" && view === "repo") { e.preventDefault(); navigate(repoBase()); }
        else if (e.key === "p" && view === "repo") { e.preventDefault(); navigate(`${repoBase()}/voyages`); }
        else if (e.key === "s" && view === "repo" && isTenantOwner) { e.preventDefault(); navigate(`${repoBase()}/settings`); }
        return;
      }
      if (e.key === "c" && commentBoxRef.current) { const t = commentBoxRef.current.querySelector("textarea"); if (t) { e.preventDefault(); t.focus(); } }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [view, tenant, issueRepo, isTenantOwner]);
  const shortcutsNode = showShortcuts ? (
    <>
      <div onClick={() => setShowShortcuts(false)} className="fixed inset-0 z-40 bg-ink/30 animate-bd-in" />
      <div className="fixed left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 z-50 w-[440px] max-w-[92vw] bg-surface rounded-[13px] shadow-modal border border-rule animate-ov-in overflow-hidden">
        <div className="px-5 py-3.5 border-b border-rule2 flex items-center justify-between">
          <span className="text-[14.5px] font-semibold">Keyboard shortcuts</span>
          <span onClick={() => setShowShortcuts(false)} className="text-muted cursor-pointer hover:text-ink">×</span>
        </div>
        <div className="px-5 py-4 grid gap-2.5 text-[13px]">
          {([["⌘K  /  /", "Open the command palette"], ["g h", "Go home"], ["g i", "Go to issues"], ["g p", "Go to pull requests"], ["g s", "Go to repo settings"], ["c", "Focus the comment box"], ["?", "Toggle this help"]] as [string, string][]).map(([k, d]) => (
            <div key={k} className="flex items-center justify-between gap-4">
              <span className="text-body">{d}</span>
              <span className="flex gap-1">{k.split("  ").map((part, i) => <kbd key={i} className="text-[11px] font-semibold text-dim border border-rule rounded-[5px] px-[6px] py-0.5 bg-paper">{part}</kbd>)}</span>
            </div>
          ))}
        </div>
      </div>
    </>
  ) : null;

  // command-palette items (built from what's loaded) + the palette node, rendered in every branch
  const cmdItems: CmdItem[] = [];
  cmdItems.push({ id: "go-home", group: "Go to", label: "Home", run: () => navigate("/") });
  if (me) cmdItems.push({ id: "go-settings", group: "Go to", label: "Account settings", run: () => navigate("/settings") });
  myAccounts.forEach((h) => cmdItems.push({ id: `org-${h}`, group: "Organizations", label: h, run: () => navigate(`/orgs/${encodeURIComponent(h)}`) }));
  repos.forEach((r) => cmdItems.push({ id: `repo-${r.tenant}/${r.repo}`, group: "Repositories", label: `${r.tenant}/${r.repo}`, sublabel: r.score > 0 ? "active" : undefined, run: () => navigate(`/${encodeURIComponent(r.tenant)}/${encodeURIComponent(r.repo)}`) }));
  if (view === "repo") {
    issues.forEach((it) => cmdItems.push({ id: `issue-${it.number}`, group: "Issues", label: `#${it.number}  ${it.title}`, sublabel: it.status.state, run: () => navigate(`${repoBase()}/issues/${it.number}`) }));
    prs.forEach((p) => cmdItems.push({ id: `voyage-${p.number}`, group: "Voyages", label: `v${p.number}  ${p.title}`, sublabel: p.state, run: () => navigate(`${repoBase()}/voyages/${p.number}`) }));
  }
  if (me) {
    cmdItems.push({ id: "act-neworg", group: "Actions", label: "New organization", run: createOrg });
    cmdItems.push({ id: "act-newrepo", group: "Actions", label: "New repository", run: () => setNewRepoOpen(true) });
  }
  const cmdNode = <CommandPalette open={cmdOpen} items={cmdItems} onClose={() => setCmdOpen(false)} />;
  const createModalsNode = (
    <>
      {newRepoOpen && <NewRepoModal accounts={myAccounts} onClose={() => setNewRepoOpen(false)} onCreate={doCreateRepo} />}
      {newIssueOpen && <NewIssueModal repos={repos.map((r) => ({ tenant: r.tenant, repo: r.repo }))} defaultRepo={view === "repo" ? `${tenant}/${issueRepo}` : ""} actors={actors} onClose={() => setNewIssueOpen(false)} onCreate={doCreateIssue} />}
      {importOpen && (() => {
        const adminAccounts = accounts.filter((a) => me && a.members.some((m) => m.actor === me.id && (m.role === "owner" || m.role === "admin")));
        return (
          <ModalShell title="Import from GitHub" onClose={() => { setImportOpen(false); setImportList(null); setGhStatus(null); setImportAcct(""); }} width={540}>
            <div className="grid gap-4">
              {adminAccounts.length === 0 ? (
                <p className="text-[13px] text-muted leading-[1.5]">You need to be an owner/admin of an organization to import repositories. Create one from the <b>+</b> menu first.</p>
              ) : (
                <>
                  <Field label="Organization" hint="you must be an admin">
                    <Picker block value={importAcct} onChange={(v) => { setImportAcct(v); setImportList(null); loadGh(v); }} options={adminAccounts.map((a) => ({ value: a.id, label: a.handle }))} placeholder="Pick an organization…" />
                  </Field>
                  {importAcct && (ghStatus?.connected ? (
                    <>
                      <div className="text-[12.5px] text-muted flex items-center gap-2 flex-wrap">
                        Connected as <b className="text-body">{ghStatus.login}</b>
                        <LinkButton onClick={() => openImport(importAcct)}>refresh</LinkButton>
                        <LinkButton onClick={() => disconnectGh(importAcct)}>disconnect</LinkButton>
                      </div>
                      <div className="border border-rule2 rounded-ctl overflow-hidden max-h-[340px] overflow-y-auto">
                        {importList === null && <div className="px-4 py-6 text-center"><Button size="sm" onClick={() => openImport(importAcct)}>List importable repos</Button></div>}
                        {importList?.length === 0 && <div className="px-4 py-6 text-[13px] text-muted">No repositories available from this connection.</div>}
                        {importList?.map((full) => (
                          <div key={full} className="flex items-center gap-3 px-4 py-2.5 border-b border-rule3 last:border-0 text-[13px]">
                            <span className="flex-1 truncate">{full}</span>
                            <Button size="sm" disabled={importBusy === full} onClick={() => importRepo(importAcct, full)}>{importBusy === full ? "importing…" : "Import"}</Button>
                          </div>
                        ))}
                      </div>
                    </>
                  ) : (
                    <div className="grid gap-2">
                      <p className="text-[12.5px] text-muted leading-[1.5]">Connect this org to GitHub. You'll be taken to GitHub to sign in, choose <b className="text-body">{adminAccounts.find((a) => a.id === importAcct)?.handle}</b>'s GitHub organization, and pick exactly which repositories to grant — you only ever connect an org you administer.</p>
                      <div><Button size="sm" className="inline-flex items-center gap-2" onClick={() => connectGh(importAcct)}>
                        <svg width="15" height="15" viewBox="0 0 24 24" fill="currentColor"><path d="M12 .5A11.5 11.5 0 0 0 .5 12a11.5 11.5 0 0 0 7.86 10.92c.58.1.79-.25.79-.56v-2c-3.2.7-3.88-1.37-3.88-1.37-.53-1.34-1.3-1.7-1.3-1.7-1.06-.72.08-.71.08-.71 1.17.08 1.79 1.2 1.79 1.2 1.04 1.79 2.73 1.27 3.4.97.1-.76.4-1.27.74-1.56-2.56-.29-5.26-1.28-5.26-5.7 0-1.26.45-2.29 1.19-3.1-.12-.29-.52-1.46.11-3.05 0 0 .97-.31 3.18 1.18a11 11 0 0 1 5.8 0c2.2-1.49 3.17-1.18 3.17-1.18.63 1.59.23 2.76.11 3.05.74.81 1.19 1.84 1.19 3.1 0 4.43-2.7 5.4-5.28 5.69.42.36.79 1.07.79 2.16v3.2c0 .31.21.67.8.56A11.5 11.5 0 0 0 23.5 12 11.5 11.5 0 0 0 12 .5z" /></svg>
                        Continue to GitHub
                      </Button></div>
                    </div>
                  ))}
                </>
              )}
            </div>
          </ModalShell>
        );
      })()}
    </>
  );

  if (authPage) {
    const shell = (title: string, children: React.ReactNode, wide = false) => (
      <div className="bg-paper min-h-screen text-ink">
        {uiModalNode}
        {cmdNode}
        {shortcutsNode}
        {createModalsNode}

        <header className="h-14 border-b border-rule2 bg-surface flex items-center px-6">
          <button className="flex items-center gap-2.5 cursor-pointer" onClick={() => navigate("/")}>
            <span className="w-[22px] h-[22px] rounded-chip bg-brass" aria-hidden />
            <span className="text-[19px] font-extrabold tracking-tight">hull</span>
          </button>
        </header>
        <div className={`mx-auto px-6 py-12 ${wide ? "max-w-[720px]" : "max-w-[420px]"}`}>
          {title && <h1 className="text-[24px] font-semibold tracking-tight mb-6">{title}</h1>}
          {children}
        </div>
      </div>
    );
    const errBox = authError ? <div className="text-[13px] text-fault-text bg-fault-wash border border-fault/30 rounded-ctl px-3 py-2 mb-3">{authError}</div> : null;

    if (authPage === "signup") {
      return shell("Create your hull account", (
        <Card>
          <div className="px-6 py-6 grid gap-4">
            {errBox}
            <div className="grid gap-1.5">
              <label className="text-[12.5px] font-semibold text-body">username</label>
              <input className={`box-border h-ctl px-2.5 rounded-ctl border bg-surface font-sans text-[13.5px] text-ink outline-none placeholder:text-faint transition-colors ${usernameAvail && !usernameAvail.available ? "border-fault" : "border-ctl focus:border-body"}`} placeholder="e.g. mira" value={authForm.username} onChange={(e) => setAuthForm({ ...authForm, username: sanitizeHandle(e.target.value) })} autoFocus />
              {authForm.username.trim() && usernameAvail && (
                <div className={`text-[12px] ${usernameAvail.available ? "text-clear-text" : "text-fault-text"}`}>{usernameAvail.available ? `✓ ${authForm.username} is available` : "✗ that username is taken"}</div>
              )}
            </div>
            <div className="grid gap-1.5">
              <label className="text-[12.5px] font-semibold text-body">email</label>
              <input className="box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[13.5px] text-ink outline-none focus:border-body placeholder:text-faint" placeholder="you@example.com" value={authForm.email} onChange={(e) => setAuthForm({ ...authForm, email: e.target.value.trim() })} onKeyDown={(e) => e.key === "Enter" && signupPasskey()} />
            </div>
            <Button disabled={authBusy || (!!usernameAvail && !usernameAvail.available)} onClick={signupPasskey}>{authBusy ? "waiting for passkey…" : "Create account with a passkey"}</Button>
            <p className="text-[12.5px] text-muted leading-[1.55]">No passwords. Your device (Touch ID, Windows Hello, a security key, or your phone) creates a passkey and that is your login.</p>
            <div className="text-[13px] text-muted pt-1 border-t border-rule2">Already have an account? <LinkButton onClick={() => { setAuthError(""); navigate("/login"); }}>Log in</LinkButton></div>
          </div>
        </Card>
      ));
    }

    if (authPage === "login") {
      return shell("Log in to hull", (
        <Card>
          <div className="px-6 py-6 grid gap-4">
            {errBox}
            <div className="grid gap-1.5">
              <label className="text-[12.5px] font-semibold text-body">username</label>
              <input className="box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[13.5px] text-ink outline-none focus:border-body placeholder:text-faint" placeholder="your username" value={authForm.username} onChange={(e) => setAuthForm({ ...authForm, username: sanitizeHandle(e.target.value) })} onKeyDown={(e) => e.key === "Enter" && loginPasskey(authForm.username)} autoFocus />
            </div>
            <Button disabled={authBusy} onClick={() => loginPasskey(authForm.username)}>{authBusy ? "waiting for passkey…" : "Continue with a passkey"}</Button>
            <div className="text-[13px] text-muted pt-1 border-t border-rule2">New here? <LinkButton onClick={() => { setAuthError(""); navigate("/signup"); }}>Create an account</LinkButton></div>
            <details className="text-[12.5px]">
              <summary className="text-muted cursor-pointer">Advanced: key login</summary>
              <div className="grid gap-2 mt-2.5">
                <input type="password" className="box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[13px] text-ink outline-none focus:border-body placeholder:text-faint" placeholder="ed25519 secret key (hex)" value={secretInput} onChange={(e) => setSecretInput(e.target.value)} onKeyDown={(e) => e.key === "Enter" && signIn()} />
                <div className="flex gap-2 items-center">
                  <Button size="sm" variant="secondary" onClick={signIn}>Sign in with key</Button>
                  <LinkButton onClick={registerAndSignIn}>new raw identity</LinkButton>
                  <LinkButton onClick={() => signInWith(DEMO_OWNER_SECRET)}>demo</LinkButton>
                </div>
              </div>
            </details>
          </div>
        </Card>
      ));
    }

    // account settings
    if (authPage === "profile") {
      const ranked = [...repos].sort((a, b) => b.score - a.score);
      const q = profileRepoQ.trim().toLowerCase();
      return shell("", (
        <div className="grid gap-6">
          {/* Full-bleed banner behind the page — breaks out of the container to the viewport edges. */}
          <div className="relative left-1/2 right-1/2 -mx-[50vw] w-screen -mt-12 -z-0 h-[200px] bg-gradient-to-br from-steel-wash via-paper to-brass-wash pointer-events-none" aria-hidden />
          {me && (
            <div className="-mt-[104px] relative">
              <div className="flex items-end gap-4">
                <span className="rounded-full ring-4 ring-surface bg-surface shadow-modal"><Avatar id={me.id} handle={me.handle} kind={me.kind} size={104} /></span>
                <div className="flex-1 min-w-0 pb-1">
                  <div className="text-[24px] font-semibold leading-tight">{me.handle}</div>
                  <div className="text-[13px] text-muted">{me.kind}{profile?.accountable ? " · accountable" : ""} · member of {myAccounts.length} org{myAccounts.length === 1 ? "" : "s"}</div>
                </div>
                <button onClick={() => navigate("/settings")} className="text-[13px] text-steel-text hover:underline flex-none pb-1">Account settings →</button>
              </div>
              <div className="mt-4 max-w-[560px]">
                {bioDraft !== null ? (
                  <div className="grid gap-2">
                    <textarea autoFocus value={bioDraft} onChange={(e) => setBioDraft(e.target.value.slice(0, 280))} rows={2} placeholder="Tell people what you work on…" className="w-full box-border px-2.5 py-2 rounded-ctl border border-ctl bg-surface font-sans text-[14px] text-ink outline-none focus:border-body resize-y" />
                    <div className="flex items-center gap-2"><Button size="sm" onClick={() => saveBio(bioDraft)}>Save</Button><LinkButton onClick={() => setBioDraft(null)}>cancel</LinkButton><span className="text-[12px] text-faint ml-auto">{bioDraft.length}/280</span></div>
                  </div>
                ) : (profileStats?.bio || "").trim() ? (
                  <p className="text-[14px] text-body leading-[1.55]">{profileStats!.bio} <button onClick={() => setBioDraft(profileStats!.bio)} className="text-[12.5px] text-steel-text hover:underline">edit</button></p>
                ) : (
                  <button onClick={() => setBioDraft("")} className="text-[14px] text-muted hover:text-steel-text">+ Add a bio</button>
                )}
              </div>
            </div>
          )}

          {/* Profile tabs */}
          <div className="border-b border-rule2 -mt-1">
            <HTabs items={["Overview", `Repositories ${repos.length}`, `Organizations ${myAccounts.length}`]} value={profileTab === "overview" ? 0 : profileTab === "repos" ? 1 : 2} onChange={(i: number) => setProfileTab(i === 0 ? "overview" : i === 1 ? "repos" : "orgs")} />
          </div>

          {profileTab === "overview" && (
            <div className="grid gap-6">
              {profileReadme !== null && (
                <Card>
                  <SectionHeader label={`${me?.handle} / ${me?.handle}`} right={<span className="text-[12px] text-muted">README.md</span>} />
                  <div className="px-6 py-5"><Markdown text={profileReadme || "_This README is empty._"} linkBase={`/${encodeURIComponent(me?.handle ?? "")}/${encodeURIComponent(me?.handle ?? "")}`} className="text-[14px] text-body leading-[1.6]" /></div>
                </Card>
              )}
              <Card>
                <SectionHeader label={`${profileStats?.total ?? 0} contribution${(profileStats?.total ?? 0) === 1 ? "" : "s"} in the last year`} />
                <div className="px-5 py-4">
                  <ContributionHeatmap days={profileStats?.days ?? []} />
                  <div className="mt-4 grid gap-1.5 pt-3 border-t border-rule3">
                    <div className="flex items-center gap-2 text-[14px]">
                      <Avatar id={me?.id} handle={me?.handle} kind="human" size={18} />
                      <span className="font-medium">{me?.handle}</span>
                      <span className="text-muted">directly</span>
                      <span className="ml-auto tabular-nums text-body">{profileStats?.human_count ?? 0}</span>
                    </div>
                    {(profileStats?.agents ?? []).map((a) => (
                      <div key={a.handle} className="flex items-center gap-2 text-[14px] pl-5">
                        <span className="text-faint">↳</span>
                        <Avatar handle={a.handle} kind="agent" size={16} />
                        <span className="text-steel-text">{a.handle}</span>
                        <span className="ml-auto tabular-nums text-muted">{a.count}</span>
                      </div>
                    ))}
                    {(profileStats?.agents ?? []).length === 0 && <div className="text-[12.5px] text-faint pl-5">no agents accountable to you have contributed yet</div>}
                  </div>
                </div>
              </Card>
            </div>
          )}

          {profileTab === "repos" && (() => {
            const grid = q ? ranked.filter((r) => `${r.tenant}/${r.repo}`.toLowerCase().includes(q)) : ranked;
            return (
              <div className="grid gap-4">
                <div className="flex justify-end"><div className="w-[260px]"><SearchInput placeholder="Find a repository" shortcut="" value={profileRepoQ} onChange={(e: React.ChangeEvent<HTMLInputElement>) => setProfileRepoQ(e.target.value)} /></div></div>
                {repos.length === 0 && <div className="py-8 text-[13px] text-muted">No repositories yet.</div>}
                <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4">
                  {grid.map((r) => (
                    <button key={`${r.tenant}/${r.repo}`} onClick={() => navigate(`/${encodeURIComponent(r.tenant)}/${encodeURIComponent(r.repo)}`)} className="group text-left bg-surface border border-rule rounded-card p-4 hover:border-ctl hover:shadow-[0_2px_10px_-4px_rgba(15,23,42,0.12)] transition-all">
                      <div className="flex items-center gap-2 min-w-0">
                        <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor" className="text-muted flex-none"><path d="M2 2.5A2.5 2.5 0 0 1 4.5 0h8.75a.75.75 0 0 1 .75.75v12.5a.75.75 0 0 1-.75.75h-2.5a.75.75 0 0 1 0-1.5h1.75v-2h-8a1 1 0 0 0-.714 1.7.75.75 0 1 1-1.072 1.05A2.495 2.495 0 0 1 2 11.5Zm10.5-1h-8a1 1 0 0 0-1 1v6.708A2.486 2.486 0 0 1 4.5 9h8ZM5 12.25a.25.25 0 0 1 .25-.25h3.5a.25.25 0 0 1 .25.25v3.25a.25.25 0 0 1-.4.2l-1.45-1.087a.25.25 0 0 0-.3 0L5.4 15.7a.25.25 0 0 1-.4-.2Z" /></svg>
                        <span className="font-semibold text-[14.5px] truncate group-hover:text-steel-text transition-colors">{r.repo}</span>
                        {r.score > 0 && <span className="ml-auto flex-none w-2 h-2 rounded-full bg-clear" title="active" />}
                      </div>
                      <div className="text-[12px] text-faint mt-0.5 truncate">{r.tenant}/{r.repo}</div>
                      <div className="mt-3 flex items-center gap-2 text-[12px] text-muted min-w-0">
                        {r.active_actors.length > 0 ? (
                          <>
                            <span className="flex -space-x-1.5 flex-none">{r.active_actors.slice(0, 3).map((a) => <span key={a} className="ring-2 ring-surface rounded-full"><Avatar id={a} handle={actorLabel(a)} kind={kindOf(a)} size={16} /></span>)}</span>
                            <span className="truncate">{r.active_actors.length} active</span>
                          </>
                        ) : <span className="text-faint">quiet</span>}
                        {r.score > 0 && <span className="ml-auto tabular-nums flex-none" title="live activity">{r.score.toFixed(0)}</span>}
                      </div>
                    </button>
                  ))}
                </div>
                {q && grid.length === 0 && <div className="py-6 text-[13px] text-muted">No repositories match “{profileRepoQ}”.</div>}
              </div>
            );
          })()}

          {profileTab === "orgs" && (
            <div>
              {myAccounts.length === 0 && <div className="py-8 text-[13px] text-muted">You're not a member of any organizations yet.</div>}
              <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4">
                {myAccounts.map((h) => {
                  const role = profile?.memberships.find((m) => m.account === h)?.role;
                  return (
                    <button key={h} onClick={() => navigate(`/orgs/${encodeURIComponent(h)}`)} className="group text-left bg-surface border border-rule rounded-card p-4 hover:border-ctl hover:shadow-[0_2px_10px_-4px_rgba(15,23,42,0.12)] transition-all flex items-center gap-3">
                      <Avatar id={h} handle={h} kind="organization" size={40} />
                      <div className="min-w-0 flex-1">
                        <div className="font-semibold text-[14.5px] truncate group-hover:text-steel-text transition-colors">{h}</div>
                        {role && <div className="text-[12px] text-muted capitalize">{role}</div>}
                      </div>
                      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="text-faint opacity-0 group-hover:opacity-100 -translate-x-1 group-hover:translate-x-0 transition-all flex-none"><polyline points="9 18 15 12 9 6" /></svg>
                    </button>
                  );
                })}
              </div>
            </div>
          )}
        </div>
      ), true);
    }
    return shell("Account settings", (
      !me ? (
        <Card><div className="px-6 py-6 grid gap-3"><p className="text-[13.5px] text-body">You are not signed in.</p><div className="flex gap-2"><Button size="sm" onClick={() => navigate("/login")}>Log in</Button><Button size="sm" variant="secondary" onClick={() => navigate("/signup")}>Sign up</Button></div></div></Card>
      ) : (
        <div className="grid gap-5">
          {!account && <Card><div className="px-6 py-6 text-[13px] text-muted">This session is a legacy key login, not a hosted passkey account, so there are no account settings to manage. Your identity is <code className="text-body">{me.handle}</code>.</div></Card>}
          {account && (
            <>
              <Card>
                <SectionHeader label="Profile" />
                <div className="px-6 py-5 grid gap-4 max-w-[420px]">
                  <div className="grid gap-1.5">
                    <label className="text-[12.5px] font-semibold text-body">username</label>
                    <input className="box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[13.5px] text-ink outline-none focus:border-body" value={account.username} onChange={(e) => setAccount({ ...account, username: e.target.value })} />
                  </div>
                  <div className="grid gap-1.5">
                    <label className="text-[12.5px] font-semibold text-body">email</label>
                    <input className="box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[13.5px] text-ink outline-none focus:border-body" value={account.email} onChange={(e) => setAccount({ ...account, email: e.target.value })} />
                  </div>
                  <div><Button size="sm" onClick={() => saveAccount({ username: account.username, email: account.email })}>Save</Button></div>
                </div>
              </Card>
              <Card>
                <SectionHeader label="Passkeys" right={<Button size="sm" variant="secondary" onClick={addPasskey}>Add a passkey</Button>} />
                <div>
                  {account.passkeys.length === 0 && <div className="px-6 py-5 text-[13px] text-muted">no passkeys</div>}
                  {account.passkeys.map((p) => (
                    <div key={p.id} className="px-6 py-3.5 border-b border-rule2 last:border-0 flex items-center gap-3">
                      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="text-steel-text"><path d="M21 2l-2 2m-7.61 7.61a5.5 5.5 0 1 1-7.778 7.778 5.5 5.5 0 0 1 7.777-7.777zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4" /></svg>
                      <div className="flex-1 min-w-0">
                        <div className="text-[14px] font-medium">{p.name}</div>
                        <div className="text-[12px] text-muted tabular-nums">{p.id.slice(0, 20)}… · added {timeAgo(p.created_unix)}</div>
                      </div>
                      <Button size="sm" variant="destructive" disabled={account.passkeys.length <= 1} onClick={() => removePasskey(p.id)}>Remove</Button>
                    </div>
                  ))}
                </div>
              </Card>
              <Card>
                <SectionHeader label="Identity" right={<span className="text-[12.5px] text-muted">accountability</span>} />
                <div className="px-6 py-5 grid gap-3">
                  <Stat k="handle" v={me.handle} />
                  <Stat k="kind" v={me.kind} />
                  <div className="grid gap-1">
                    <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted">actor id (public key)</span>
                    <code className="text-[11.5px] text-body break-all tabular-nums">{account.actor}</code>
                  </div>
                  <div className="pt-1"><Button size="sm" variant="secondary" onClick={createAgent}>+ delegate an agent</Button></div>
                  <p className="text-[12px] text-muted leading-[1.55]">Agents you delegate chain to you cryptographically. Hull signs the delegation with your held key, so agents are accountable to your identity.</p>
                </div>
              </Card>
            </>
          )}
          <Card>
            <div className="px-6 py-4 flex items-center justify-between">
              <span className="text-[13px] text-muted">Signed in as <b className="text-body">{me.handle}</b></span>
              <Button size="sm" variant="secondary" onClick={() => { signOut(); navigate("/"); }}>Sign out</Button>
            </div>
          </Card>
        </div>
      )
    ), true);
  }


  const unread = notifs.filter((n) => n.ts > seenTs).length;
  const openIssues = issues.filter((i) => i.status.state === "open").length;

  // ── shared chrome (top bar + notifications drawer) ────────────────────────
  const topBar = (
    <header className="h-14 border-b border-rule2 bg-surface flex items-center gap-5 px-6 sticky top-0 z-40">
      <button className="flex items-center gap-2.5 cursor-pointer shrink-0" onClick={() => navigate("/")} title="situation room">
        <span className="w-[22px] h-[22px] rounded-chip bg-brass" aria-hidden />
        <span className="text-[19px] font-extrabold tracking-tight">hull</span>
      </button>
      {view === "repo" && (
        <div className="hidden md:flex text-[13px] gap-1.5 items-center tabular-nums shrink-0">
          <button className="text-faint hover:text-ink cursor-pointer" onClick={() => navigate("/")}>{tenant}</button>
          <span className="text-rule">/</span>
          <button className="font-medium hover:text-steel-text cursor-pointer" onClick={() => navigate(repoBase())}>{issueRepo}</button>
        </div>
      )}
      <div className="flex-1 max-w-[440px] mx-auto">
        <button onClick={() => setCmdOpen(true)} className="w-full flex items-center gap-2 h-ctl px-2.5 rounded-ctl border border-ctl bg-surface hover:border-[oklch(0.6_0.015_250)] transition-colors cursor-pointer text-left">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="text-muted flex-none"><circle cx="11" cy="11" r="8" /><line x1="21" y1="21" x2="16.65" y2="16.65" /></svg>
          <span className="flex-1 text-[13.5px] text-faint">Search or jump to…</span>
          <span className="text-[11px] font-semibold text-dim border border-rule rounded-[5px] px-[5px] py-0.5 bg-paper flex-none">⌘K</span>
        </button>
      </div>
      <div className="flex items-center gap-2 shrink-0">
        <button onClick={() => setTheme(theme === "dark" ? "light" : "dark")} title={theme === "dark" ? "Switch to light" : "Switch to dark"} aria-label="toggle theme"
          className="h-ctl w-ctl grid place-items-center rounded-ctl border border-ctl bg-surface text-dim hover:text-ink hover:border-dim transition-colors">
          {theme === "dark" ? (
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" /></svg>
          ) : (
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden><circle cx="12" cy="12" r="5" /><line x1="12" y1="1" x2="12" y2="3" /><line x1="12" y1="21" x2="12" y2="23" /><line x1="4.22" y1="4.22" x2="5.64" y2="5.64" /><line x1="18.36" y1="18.36" x2="19.78" y2="19.78" /><line x1="1" y1="12" x2="3" y2="12" /><line x1="21" y1="12" x2="23" y2="12" /><line x1="4.22" y1="19.78" x2="5.64" y2="18.36" /><line x1="18.36" y1="5.64" x2="19.78" y2="4.22" /></svg>
          )}
        </button>
        {me && (
          <Popover align="right" width={230} trigger={(open) => (
            <span className={`h-ctl w-ctl grid place-items-center rounded-ctl border bg-surface cursor-pointer transition-colors text-dim hover:text-ink ${open ? "border-body" : "border-ctl hover:border-dim"}`} title="Create new…" aria-label="create">
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><line x1="12" y1="5" x2="12" y2="19" /><line x1="5" y1="12" x2="19" y2="12" /></svg>
            </span>
          )}>
            <div className="py-1">
              {[
                { label: "New issue", hint: view === "repo" ? `in ${issueRepo}` : "pick a repository", run: () => setNewIssueOpen(true), icon: <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><circle cx="12" cy="12" r="10" /><line x1="12" y1="8" x2="12" y2="16" /><line x1="8" y1="12" x2="16" y2="12" /></svg> },
                { label: "New repository", hint: "", run: () => setNewRepoOpen(true), icon: <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M2 3h6a4 4 0 0 1 4 4v14a3 3 0 0 0-3-3H2z" /><path d="M22 3h-6a4 4 0 0 0-4 4v14a3 3 0 0 1 3-3h7z" /></svg> },
                { label: "New organization", hint: "", run: createOrg, icon: <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M3 21h18M5 21V7l8-4v18M19 21V11l-6-4" /></svg> },
                { label: "Import from GitHub", hint: "into an org you admin", run: () => setImportOpen(true), icon: <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" /><polyline points="7 10 12 15 17 10" /><line x1="12" y1="15" x2="12" y2="3" /></svg> },
              ].map((it) => (
                <button key={it.label} type="button" onClick={it.run} className="w-full text-left px-3 py-2 flex items-center gap-2.5 hover:bg-paper">
                  <span className="text-muted flex-none">{it.icon}</span>
                  <span className="min-w-0"><span className="block text-[13px] font-medium text-body leading-tight">{it.label}</span>{it.hint && <span className="block text-[11.5px] text-muted leading-tight">{it.hint}</span>}</span>
                </button>
              ))}
            </div>
          </Popover>
        )}
        <button
          className="relative h-ctl w-ctl grid place-items-center rounded-ctl border border-ctl bg-surface text-dim hover:text-ink hover:border-dim transition-colors cursor-pointer"
          onClick={openNotifs}
          title="notifications"
          aria-label="notifications"
        >
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <path d="M18 8A6 6 0 0 0 6 8c0 7-3 9-3 9h18s-3-2-3-9" /><path d="M13.73 21a2 2 0 0 1-3.46 0" />
          </svg>
          {unread > 0 && <span className="absolute -top-1 -right-1 min-w-[16px] h-4 px-1 grid place-items-center rounded-full bg-fault text-white text-[10px] font-bold tabular-nums">{unread}</span>}
        </button>
        <span className="w-px h-6 bg-rule2 mx-0.5" aria-hidden />
        {me ? (
          <Popover align="right" width={240} trigger={(open) => (
            <span className={`flex items-center gap-1.5 h-ctl px-2.5 rounded-ctl border bg-surface cursor-pointer text-[13px] transition-colors ${open ? "border-body" : "border-ctl hover:border-dim"}`} title={me.handle}>
              <Avatar id={me.id} handle={me.handle} kind={me.kind} size={18} />
              <span className="font-medium">{me.handle}</span>
              <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" className={`text-muted transition-transform ${open ? "rotate-180" : ""}`}><polyline points="6 9 12 15 18 9" /></svg>
            </span>
          )}>
            <div className="p-1.5 grid gap-0.5">
              <button onClick={() => navigate("/me")} className="flex items-center gap-2.5 px-2.5 py-2 rounded-ctl hover:bg-paper text-left">
                <Avatar id={me.id} handle={me.handle} kind={me.kind} size={34} />
                <span className="min-w-0">
                  <span className="block text-[13.5px] font-semibold truncate">{me.handle}</span>
                  <span className="block text-[11.5px] text-muted">{me.kind}{profile?.accountable ? " · accountable" : ""}</span>
                </span>
              </button>
              <div className="border-t border-rule2 my-1" />
              <button onClick={() => navigate("/me")} className="flex items-center gap-2.5 px-2.5 py-1.5 rounded-ctl hover:bg-paper text-left text-[13px] text-body">
                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="text-muted"><path d="M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2" /><circle cx="12" cy="7" r="4" /></svg>Your profile
              </button>
              <button onClick={() => navigate("/settings")} className="flex items-center gap-2.5 px-2.5 py-1.5 rounded-ctl hover:bg-paper text-left text-[13px] text-body">
                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="text-muted"><circle cx="12" cy="12" r="3" /><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" /></svg>Settings
              </button>
              <div className="border-t border-rule2 my-1" />
              <button onClick={signOut} className="flex items-center gap-2.5 px-2.5 py-1.5 rounded-ctl hover:bg-fault-wash text-left text-[13px] text-fault-text">
                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" /><polyline points="16 17 21 12 16 7" /><line x1="21" y1="12" x2="9" y2="12" /></svg>Sign out
              </button>
            </div>
          </Popover>
        ) : (
          <div className="flex items-center gap-2">
            <Button size="sm" variant="secondary" onClick={() => navigate("/login")}>Log in</Button>
            <Button size="sm" onClick={() => navigate("/signup")}>Sign up</Button>
          </div>
        )}
      </div>
    </header>
  );

  const notifDrawer = (
    <Drawer open={showNotifs} onClose={() => setShowNotifs(false)} title={`inbox · ${handleOf(actingAs)}`}>
      {notifs.length === 0 && <div className="text-[13px] text-muted">nothing yet</div>}
      {notifs.slice(0, 20).map((n, i) => (
        <div key={`${n.ts}-${n.kind}-${i}`} className="flex items-start gap-2 py-2 border-b border-rule3 last:border-0">
          <span className={`w-1.5 h-1.5 rounded-full mt-1.5 flex-none ${n.ts > seenTs ? "bg-steel" : "bg-rule"}`} />
          <div className="min-w-0">
            <div className="flex items-center gap-1.5">
              <span className="text-[12.5px] font-semibold text-body">{n.kind.replace(/_/g, " ")}</span>
              {n.broadcast && <Tag>team</Tag>}
            </div>
            <div className="text-[12.5px] text-muted mt-0.5">{n.summary}</div>
          </div>
        </div>
      ))}
      <div className="text-[11px] text-faint pt-1">via Notifier plugin</div>
    </Drawer>
  );

  // repo sidebar — only surfaces when there's something worth flagging (e.g. leaked secrets); the
  // former "About" metadata module was redundant with the header and has been removed.
  const hasRepoSidebar = secrets.length > 0;
  const repoSidebar = (
    <aside className="grid gap-5 content-start">
      {secrets.length > 0 && (
        <Module title="Security" tone="var(--fault)">
          <p className="text-[12.5px] text-fault-text font-medium">{secrets.length} secret{secrets.length > 1 ? "s" : ""} detected on push</p>
          {secrets.slice(0, 5).map((s, i) => (
            <div key={i} className="text-[12px] text-muted">{s.title} · <code className="text-body">{s.path}:{s.line}</code></div>
          ))}
        </Module>
      )}
    </aside>
  );

  const currentIssue = openIssue != null ? issues.find((i) => i.number === openIssue) ?? null : null;
  const currentPr = openPr != null ? prs.find((p) => p.number === openPr) ?? null : null;

  return (
    <div className="bg-paper min-h-screen text-ink">
      {uiModalNode}
      {cmdNode}
      {shortcutsNode}
      {createModalsNode}

      {topBar}
      {notifDrawer}

      {/* ── HOME · your work ──────────────────────────────────────────────── */}
      {view === "home" && !orgHandle && !me && (
        <div className="max-w-[560px] mx-auto px-6 py-20 text-center">
          <h1 className="text-[28px] font-semibold tracking-tight">Welcome to hull</h1>
          <p className="text-[14px] text-muted mt-3 leading-[1.6]">The hosted layer for keel — where humans and accountable agents review and land changes together. Sign in to see the work across your organizations.</p>
          <div className="flex items-center justify-center gap-2.5 mt-6">
            <Button onClick={() => navigate("/login")}>Log in</Button>
            <Button variant="secondary" onClick={() => navigate("/signup")}>Create an account</Button>
          </div>
        </div>
      )}
      {view === "home" && !orgHandle && me && (() => {
        // Home has no search box of its own — don't filter by the shared `q` (it's bound to the
        // Issues/PRs filters and would otherwise hide repos here based on a term typed elsewhere).
        const activeRepos = repos.filter((r) => r.score > 0);
        return (
        <div className="max-w-[1180px] mx-auto px-6 sm:px-8 py-9">
          <div className="flex flex-wrap items-end justify-between gap-4 mb-8">
            <div>
              <h1 className="text-[27px] font-semibold tracking-tight leading-none">Your work</h1>
              <p className="text-[13.5px] text-muted mt-2.5">
                {activeRepos.length} active {activeRepos.length === 1 ? "repo" : "repos"} · {homePrs.length + homeIssues.length} {homePrs.length + homeIssues.length === 1 ? "item" : "items"} needing you
              </p>
            </div>
          </div>

          <div className="grid grid-cols-1 lg:grid-cols-[1fr_340px] gap-x-12 gap-y-9">
            <section className="min-w-0 grid gap-8 content-start">
              {(homePrs.length > 0 || homeIssues.length > 0) && (
                <div>
                  <Eyebrow label="Needs your attention" right={`${homePrs.length + homeIssues.length}`} />
                  <div>
                    {homePrs.map((p) => (
                      <button key={`pr-${p.tenant}/${p.repo}#${p.number}`} onClick={() => navigate(`/${encodeURIComponent(p.tenant)}/${encodeURIComponent(p.repo)}/voyages/${p.number}`)} className="group w-full text-left block border-b border-rule2">
                        <div className="flex items-start gap-3 py-3 -mx-3 px-3 rounded-ctl group-hover:bg-surface transition-colors">
                          <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor" className="text-clear-text mt-0.5 flex-none"><path d="M1.5 3.25a2.25 2.25 0 1 1 3 2.122v5.256a2.251 2.251 0 1 1-1.5 0V5.372A2.25 2.25 0 0 1 1.5 3.25Zm5.677-.177L9.573.677A.25.25 0 0 1 10 .854V2.5h1A2.5 2.5 0 0 1 13.5 5v5.628a2.251 2.251 0 1 1-1.5 0V5a1 1 0 0 0-1-1h-1v1.646a.25.25 0 0 1-.427.177L7.177 3.427a.25.25 0 0 1 0-.354Z" /></svg>
                          <div className="flex-1 min-w-0">
                            <div className="text-[14px] font-medium group-hover:text-steel-text transition-colors truncate">{p.title}</div>
                            <div className="text-[12px] text-muted mt-0.5 tabular-nums"><span className="text-faint">{p.tenant}/{p.repo}</span> · #{p.number} · <span className="text-steel-text">{p.reason}</span></div>
                          </div>
                        </div>
                      </button>
                    ))}
                    {homeIssues.map((it) => (
                      <button key={`is-${it.tenant}/${it.repo}#${it.number}`} onClick={() => navigate(`/${encodeURIComponent(it.tenant)}/${encodeURIComponent(it.repo)}/issues/${it.number}`)} className="group w-full text-left block border-b border-rule2">
                        <div className="flex items-start gap-3 py-3 -mx-3 px-3 rounded-ctl group-hover:bg-surface transition-colors">
                          <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor" className="text-clear-text mt-0.5 flex-none"><path d="M8 9.5a1.5 1.5 0 1 0 0-3 1.5 1.5 0 0 0 0 3Z" /><path fillRule="evenodd" d="M8 0a8 8 0 1 0 0 16A8 8 0 0 0 8 0ZM1.5 8a6.5 6.5 0 1 1 13 0 6.5 6.5 0 0 1-13 0Z" /></svg>
                          <div className="flex-1 min-w-0">
                            <div className="text-[14px] font-medium group-hover:text-steel-text transition-colors truncate">{it.title}</div>
                            <div className="text-[12px] text-muted mt-0.5 tabular-nums"><span className="text-faint">{it.tenant}/{it.repo}</span> · #{it.number} · <span className="text-steel-text">{it.reason}</span></div>
                          </div>
                        </div>
                      </button>
                    ))}
                  </div>
                </div>
              )}
              <div>
              <Eyebrow label="Active repositories" right="by live activity" />
              {activeRepos.length === 0 && (
                <div className="py-8 text-[13px] text-muted">
                  {repos.length === 0
                    ? <>No repos yet. Create one, import from GitHub, or <code className="text-body">git push http://localhost:8930/&lt;org&gt;/&lt;repo&gt; main</code>.</>
                    : <>No active repositories right now — <button className="text-steel-text hover:underline" onClick={() => navigate("/me")}>see all {repos.length} on your profile →</button></>}
                </div>
              )}
              <div>
                {activeRepos.map((r) => (
                  <button key={`${r.tenant}/${r.repo}`} onClick={() => navigate(`/${encodeURIComponent(r.tenant)}/${encodeURIComponent(r.repo)}`)} className="group w-full text-left block">
                    <div className="flex items-start gap-4 py-4 -mx-3 px-3 rounded-ctl border-b border-rule2 group-hover:bg-surface transition-colors">
                      <div className="flex-1 min-w-0">
                        <div className="flex items-center gap-2.5">
                          <span className="text-[16px] font-medium group-hover:text-steel-text transition-colors"><span className="text-faint font-normal">{r.tenant}/</span>{r.repo}</span>
                          {r.active_actors.some((a) => kindOf(a) === "agent" || a.startsWith("agent")) && <StatusBadge kind="agent">agents active</StatusBadge>}
                        </div>
                        <div className="flex items-center gap-2 mt-2 text-[12.5px] text-muted min-w-0">
                          {r.active_actors.length > 0 ? (
                            <>
                              <span className="flex -space-x-1.5 flex-none">
                                {r.active_actors.slice(0, 4).map((a) => <span key={a} className="ring-2 ring-paper rounded-full group-hover:ring-surface transition-colors"><Avatar id={a} handle={actorLabel(a)} kind={kindOf(a)} size={18} /></span>)}
                              </span>
                              <span className="truncate">{r.active_actors.slice(0, 3).map((a) => actorLabel(a)).join(", ")}{r.active_actors.length > 3 ? ` +${r.active_actors.length - 3}` : ""}</span>
                              {r.hot_files.length > 0 && <span className="text-faint flex-none">· {r.hot_files.length} hot file{r.hot_files.length > 1 ? "s" : ""}</span>}
                            </>
                          ) : <span className="text-faint">no recent activity</span>}
                        </div>
                      </div>
                      <div className="flex items-center gap-3 shrink-0 pt-0.5">
                        {r.score > 0 && <span className="text-[12.5px] text-muted tabular-nums" title="live activity score">{r.score.toFixed(0)}</span>}
                        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="text-steel-text opacity-0 -translate-x-1.5 group-hover:opacity-100 group-hover:translate-x-0 transition-all duration-150">
                          <path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" /><polyline points="15 3 21 3 21 9" /><line x1="10" y1="14" x2="21" y2="3" />
                        </svg>
                      </div>
                    </div>
                  </button>
                ))}
              </div>
              {repos.length > 0 && (
                <button onClick={() => navigate("/me")} className="mt-3 inline-flex items-center gap-1.5 text-[13px] font-medium text-steel-text hover:underline">
                  All {repos.length} {repos.length === 1 ? "repository" : "repositories"} on your profile
                  <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><line x1="5" y1="12" x2="19" y2="12" /><polyline points="12 5 19 12 12 19" /></svg>
                </button>
              )}
              </div>
            </section>

            <aside className="grid gap-5 content-start">
              <Module title="Your organizations">
                {myAccounts.length === 0 && <span className="text-[13px] text-muted">none yet</span>}
                {myAccounts.map((h) => (
                  <button key={h} className="text-left text-[13.5px] text-body hover:text-steel-text cursor-pointer flex items-center justify-between" onClick={() => navigate(`/orgs/${encodeURIComponent(h)}`)}>
                    <span>{h}</span><span className="text-faint">→</span>
                  </button>
                ))}
              </Module>
            </aside>
          </div>
        </div>
        );
      })()}

      {/* ── ISSUE detail page ─────────────────────────────────────────────── */}
      {view === "repo" && currentIssue && (() => {
        const it = currentIssue;
        return (
          <div className="max-w-[1180px] mx-auto px-6 sm:px-8 py-8">
            <button className="flex items-center gap-1.5 text-[13px] font-medium text-dim hover:text-ink cursor-pointer mb-5" onClick={() => navigate(repoBase())}>
              <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><line x1="19" y1="12" x2="5" y2="12" /><polyline points="12 19 5 12 12 5" /></svg>
              {issueRepo} · issues
            </button>
            <div className="flex items-center gap-3 flex-wrap mb-1.5">
              <StatusBadge kind={it.status.state === "open" ? "running" : "queued"}>{it.status.state === "open" ? "open" : it.status.reason ?? "closed"}</StatusBadge>
              <h1 className="text-[24px] font-semibold tracking-tight">{it.title}</h1>
            </div>
            <p className="text-[13px] text-muted mb-7">
              <span className="tabular-nums">#{it.number}</span> · opened by <b className={kindOf(it.author) === "agent" ? "text-steel-text" : "text-body"}>{handleOf(it.author)}</b>
            </p>
            <div className="grid grid-cols-1 lg:grid-cols-[1fr_300px] gap-x-12 gap-y-9">
              <section className="min-w-0 grid gap-6">
                {it.body ? <Markdown text={it.body} linkBase={`/${encodeURIComponent(tenant)}/${issueRepo}`} className="text-[14px] text-body" /> : <p className="text-[13px] text-muted">no description</p>}
                {it.code_refs.length > 0 && (
                  <div className="grid gap-2">
                    <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted">Code references</span>
                    {it.code_refs.map((c, i) => {
                      const key = `${it.number}:${c.path}`;
                      return (
                        <div key={i} className="border border-rule2 rounded-ctl overflow-hidden">
                          <button className="w-full flex items-center gap-2 px-3 py-2.5 bg-paper hover:bg-surface transition-colors text-left" onClick={() => showWhy(key, c.path)}>
                            <code className="text-[12.5px] text-body flex-1">{c.path}:{c.line_start}{c.line_end ? `-${c.line_end}` : ""}</code>
                            <span className="text-[11.5px] text-steel-text" title={`content-addressed → keel blob ${c.blob}`}>⬡ {c.blob.slice(0, 10)}</span>
                            <span className="text-[12px] text-muted">{prov[key] ? "hide ▾" : "provenance ▸"}</span>
                          </button>
                          {prov[key] && (
                            <div className="border-t border-rule3">
                              {prov[key].length === 0 && <div className="px-3 py-2 text-[12px] text-muted">no recorded history</div>}
                              {prov[key].map((p, j) => (
                                <div key={j} className="px-3 py-2 border-b border-rule3 last:border-0 flex gap-3 text-[12.5px]">
                                  <code className="text-steel-text tabular-nums">{p.change.slice(0, 8)}</code>
                                  <span className="text-body flex-1">{p.intent}</span>
                                  <span className="text-muted">{p.author}</span>
                                </div>
                              ))}
                            </div>
                          )}
                        </div>
                      );
                    })}
                  </div>
                )}
                <div className="grid gap-2.5">
                  <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted">Discussion</span>
                  <IssueThread target={`issue:${it.number}`} {...threadProps} />
                </div>
              </section>

              <aside className="grid gap-5 content-start">
                <Module title="Details">
                  <Stat k="status" v={
                    <span className={`inline-flex items-center gap-1 text-[12px] font-medium px-1.5 py-[2px] rounded-badge ${it.status.state === "open" ? "bg-clear-wash text-clear-text" : it.status.reason === "completed" || !it.status.reason ? "bg-steel-wash text-steel-text" : "bg-rule2 text-dim"}`}>
                      <span className={`w-1.5 h-1.5 rounded-full ${it.status.state === "open" ? "bg-clear" : it.status.reason === "completed" || !it.status.reason ? "bg-steel" : "bg-muted"}`} />
                      {it.status.state === "open" ? "open" : it.status.reason ?? "closed"}
                    </span>
                  } />
                  <Stat k="author" v={handleOf(it.author)} />
                  <div className="grid gap-1.5 pt-1">
                    <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted">assignees</span>
                    <div className="flex flex-wrap gap-1.5">
                      {it.assignees.map((id) => (
                        <span key={id} className="inline-flex items-center gap-1 text-xs px-2 py-1 rounded-chip bg-paper border border-rule">
                          {handleOf(id)}
                          {canAct && <button className="text-muted hover:text-fault-text cursor-pointer" title="unassign" onClick={() => issueAction(it.number, "unassign", { assignee: id })}>×</button>}
                        </span>
                      ))}
                      {it.assignees.length === 0 && <span className="text-[12.5px] text-muted">none</span>}
                    </div>
                    {canAct && me && !it.assignees.includes(me.id) && <button className="text-[11.5px] text-muted hover:text-ink inline-flex items-center gap-1 w-fit" onClick={() => issueAction(it.number, "assign", { assignee: me.id })}><svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><line x1="19" y1="8" x2="19" y2="14" /><line x1="22" y1="11" x2="16" y2="11" /></svg>assign me</button>}
                  </div>
                  <div className="grid gap-1.5 pt-1">
                    <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted">labels</span>
                    <div className="flex flex-wrap gap-1.5">
                      {it.labels.map((l) => (
                        <span key={l} className="group inline-flex items-center gap-0.5">
                          <Label name={l} color={labelColor(l)} />
                          {canAct && <button className="text-muted hover:text-fault-text cursor-pointer text-xs opacity-0 group-hover:opacity-100 transition-opacity px-0.5" title="remove" onClick={() => issueAction(it.number, "unlabel", { label: l })}>×</button>}
                        </span>
                      ))}
                      {it.labels.length === 0 && <span className="text-[12.5px] text-muted">none</span>}
                    </div>
                    {canAct && (repoLabels.filter((l) => !it.labels.includes(l.name)).length > 0 ? (
                      <div className="max-w-[220px]"><Picker block size="sm" value="" placeholder="Add a label…" onChange={(v) => v && issueAction(it.number, "label", { label: v })}
                        options={repoLabels.filter((l) => !it.labels.includes(l.name)).map((l) => ({ value: l.name, label: l.name }))} /></div>
                    ) : repoLabels.length === 0 ? (
                      <span className="text-[11.5px] text-faint">no labels configured — a repo admin can add them in Settings</span>
                    ) : null)}
                  </div>
                  {it.resolved_by && <Stat k="resolved by" v={<code className="text-[12px] text-steel-text">⬡ {it.resolved_by.slice(0, 8)}</code>} />}
                </Module>
              </aside>
            </div>
          </div>
        );
      })()}

      {/* ── VOYAGE page = the review package, front and center ───────────── */}
      {view === "repo" && currentPr && (() => {
        const p = currentPr;
        const prReviews = reviews.filter((r) => r.target === `pr:${p.number}`);
        const primary = (prReviews[0] ?? { id: "live", target: `pr:${p.number}`, reviewer: "", verdict: "comment", summary: "", findings: [] }) as Review;
        const checksOk = p.verification === "green";
        const changesRequested = prReviews.some((r) => r.verdict === "request_changes" || r.verdict === "reject");
        const hasApproval = prReviews.some((r) => r.verdict === "approve" && r.reviewer !== p.author);
        const blockers = prReviews.reduce((n, r) => n + (r.findings ?? []).filter((f) => f.severity === "blocker").length, 0);
        // The merge gate must agree with the checklist below: an unresolved blocker blocks landing.
        const canLand = checksOk && hasApproval && !changesRequested && blockers === 0;
        const gateChecks: { tone: "ok" | "bad" | "wait"; label: string; detail: string }[] = [
          { tone: checksOk ? "ok" : p.verification === "red" ? "bad" : "wait", label: "keel verify", detail: checksOk ? "Build & tests passed" : p.verification === "red" ? "Build or tests failed" : "Not run yet" },
          { tone: blockers > 0 ? "bad" : "ok", label: "No blocking findings", detail: blockers > 0 ? `${blockers} blocker${blockers > 1 ? "s" : ""}` : "None raised" },
          { tone: changesRequested ? "bad" : hasApproval ? "ok" : "wait", label: "Independent approval", detail: changesRequested ? "Changes requested" : hasApproval ? "Approved by a non-author" : "Awaiting review" },
        ];
        const okN = gateChecks.filter((c) => c.tone === "ok").length;
        const badN = gateChecks.filter((c) => c.tone === "bad").length;
        const overall = badN > 0 ? "bad" : okN === gateChecks.length ? "ok" : "wait";
        const mergeGlyph = (
          <svg width="17" height="17" viewBox="0 0 16 16" fill="currentColor"><path d="M5.45 5.154A4.25 4.25 0 0 0 9.25 7.5h1.378a2.251 2.251 0 1 1 0 1.5H9.25A5.734 5.734 0 0 1 5 7.123v3.505a2.25 2.25 0 1 1-1.5 0V5.372a2.25 2.25 0 1 1 1.95-.218ZM4.25 13.5a.75.75 0 1 0 0-1.5.75.75 0 0 0 0 1.5Zm8.5-4.5a.75.75 0 1 0 0-1.5.75.75 0 0 0 0 1.5ZM5 3.25a.75.75 0 1 0-1.5 0 .75.75 0 0 0 1.5 0Z" /></svg>
        );
        const gate = p.state === "merged" ? (
          <div className="flex gap-3">
            <div className="w-9 h-9 rounded-ctl bg-clear text-white grid place-items-center flex-none mt-0.5">{mergeGlyph}</div>
            <Card className="flex-1"><div className="px-4 py-3.5"><div className="text-[14.5px] font-semibold text-clear-text">Merged</div><div className="text-[12.5px] text-muted tabular-nums mt-0.5">v{p.number} · ⬡ {(p.changes[0] ?? "").slice(0, 8)} · by {handleOf(p.author)}</div></div></Card>
          </div>
        ) : p.state === "closed" ? (
          <div className="flex gap-3">
            <div className="w-9 h-9 rounded-ctl bg-dim text-white grid place-items-center flex-none mt-0.5">{mergeGlyph}</div>
            <Card className="flex-1"><div className="px-4 py-3.5 flex items-center gap-3"><div className="flex-1"><div className="text-[14.5px] font-semibold">Closed without merging</div><div className="text-[12.5px] text-muted tabular-nums mt-0.5">v{p.number} · by {handleOf(p.author)}</div></div>{canAct && <Button size="sm" variant="secondary" onClick={() => closePr(p.number, true)}>Reopen</Button>}</div></Card>
          </div>
        ) : (
          <div className="flex gap-3">
            <div className="w-9 h-9 rounded-ctl bg-ink text-surface grid place-items-center flex-none mt-0.5">{mergeGlyph}</div>
            <Card className="flex-1">
              {/* overall status header */}
              <div className="px-4 py-3 flex items-center gap-3 border-b border-rule2">
                {overall === "wait"
                  ? <span className="w-[22px] h-[22px] rounded-full border-[3px] border-brass/30 border-t-brass flex-none" />
                  : <StatusDot tone={overall === "ok" ? "ok" : "bad"} size={22} />}
                <div className="flex-1 min-w-0">
                  <div className="text-[14.5px] font-semibold">{overall === "ok" ? "All checks have passed" : overall === "bad" ? "Some checks did not pass" : "Some checks haven't completed yet"}</div>
                  <div className="text-[12.5px] text-muted">{okN} passed{badN > 0 ? ` · ${badN} failing` : ""}{overall === "wait" ? ` · ${gateChecks.length - okN} pending` : ""}</div>
                </div>
              </div>
              {/* individual checks */}
              <div className="border-b border-rule2">
                {gateChecks.map((c, i) => (
                  <div key={i} className="px-4 py-2.5 flex items-center gap-2.5 border-b border-rule3 last:border-0">
                    <StatusDot tone={c.tone} size={16} />
                    <span className="text-[13px] font-medium text-body flex-1 min-w-0 truncate">{c.label}</span>
                    <span className="text-[12px] text-muted flex-none">{c.detail}</span>
                  </div>
                ))}
              </div>
              {/* conflicts */}
              <div className="px-4 py-3 flex items-center gap-2.5 border-b border-rule2">
                <StatusDot tone="ok" size={16} />
                <div className="min-w-0"><div className="text-[13px] font-medium">No conflicts with the base branch</div><div className="text-[12px] text-muted">keel changes are content-addressed — merging is automatic.</div></div>
              </div>
              {/* action */}
              <div className="px-4 py-3 bg-paper flex items-center gap-3 flex-wrap">
                <Button size="sm" disabled={!canLand} onClick={() => mergePr(p.number)} className={canLand ? "!bg-clear !border-clear !text-white font-semibold hover:!bg-[oklch(0.5_0.11_150)]" : ""}>Merge</Button>
                {!canLand && <span className="text-[12.5px] text-muted">{isTenantOwner ? "Or override as an owner:" : "Every check must pass before merging."}</span>}
                {!canLand && isTenantOwner && <Button size="sm" variant="secondary" className="ml-auto !text-fault-text" onClick={() => mergePr(p.number, true)}>Merge without checks</Button>}
              </div>
            </Card>
          </div>
        );
        // Secondary review actions (the primary comment/approve/request-changes flow lives in the
        // conversation composer's split button). Agent auto-review + request-a-reviewer only.
        const reviewTools = p.state === "open" ? (
          <div className="flex items-center gap-2 flex-wrap">
            {caps.ai_review && <Button size="sm" variant="secondary" disabled={autoReviewing === p.number} onClick={() => autoReview(p.number)}>{autoReviewing === p.number ? "agent reviewing…" : "⬡ Agent auto-review"}</Button>}
            {canAct && (
              <Picker size="sm" width={220} placeholder="Request a reviewer…" value="" onChange={(v) => requestReviewer(p.number, v)}
                options={actors.filter((a) => a.id !== p.author && !p.reviewers?.includes(a.id)).map(actorOption)} />
            )}
          </div>
        ) : null;
        return (
          <ReviewPage
            review={primary}
            reviews={prReviews}
            landGate={gate}
            reviewTools={reviewTools}
            onReviewsChanged={loadReviews}
            canFix={caps.ai_fix}
            canReview={caps.ai_review}
            onTriage={() => autoReview(p.number)}
            triaging={autoReviewing === p.number}
            pr={p}
            actors={actors}
            tenant={tenant}
            repo={issueRepo}
            token={token}
            me={me}
            onBack={() => navigate(`${repoBase()}/voyages`)}
          />
        );
      })()}

      {/* ── ORG · members + teams ─────────────────────────────────────────── */}
      {orgHandle && (() => {
        const acct = orgAccount;
        if (!acct) return <div className="max-w-[1180px] mx-auto px-6 sm:px-8 py-16 text-[13px] text-muted">no organization <b className="text-body">{orgHandle}</b></div>;
        const amAdmin = !!me && acct.members.some((m) => m.actor === me.id && (m.role === "owner" || m.role === "admin"));
        const candidates = actors.filter((a) => !acct.members.some((m) => m.actor === a.id));
        return (
          <div className="max-w-[1180px] mx-auto px-6 sm:px-8 py-9">
            <div className="flex items-center gap-3 flex-wrap mb-1.5">
              <Avatar id={acct.id} handle={acct.handle} kind="organization" size={32} />
              <h1 className="text-[25px] font-semibold tracking-tight">{acct.handle}</h1>
              <span className="text-[11px] font-bold uppercase tracking-[0.03em] px-1.5 py-[3px] rounded-badge bg-rule2 text-dim">{acct.kind}</span>
            </div>
            <p className="text-[13px] text-muted mb-6">{acct.members.length} member{acct.members.length === 1 ? "" : "s"} · {teams.length} team{teams.length === 1 ? "" : "s"} · {acct.repos.length} repo{acct.repos.length === 1 ? "" : "s"}</p>
            <div className="grid grid-cols-1 lg:grid-cols-[1fr_320px] gap-x-12 gap-y-9">
              <section className="min-w-0 grid gap-8">
                {amAdmin && (
                  <div>
                    <Eyebrow label="Default repository settings" right="inherited by new repos" />
                    <Card>
                      <div className="px-5 py-4 grid gap-4">
                        <div className="flex items-center justify-between gap-4">
                          <div><div className="text-[14px] font-medium">Default visibility</div><div className="text-[12.5px] text-muted">New repos in {acct.handle} start with this.</div></div>
                          <Picker width={200} value={orgDefaults?.visibility ?? "public"} onChange={(v) => saveOrgDefaults(acct.id, { visibility: v })} options={[{ value: "public", label: "Public" }, { value: "unlisted", label: "Unlisted" }, { value: "private", label: "Private" }]} />
                        </div>
                        <div className="flex items-center justify-between gap-4">
                          <div><div className="text-[14px] font-medium">Require a review to merge</div><div className="text-[12.5px] text-muted">Default merge gate for new repos.</div></div>
                          <Switch on={!!orgDefaults?.require_review_to_land} onChange={(on: boolean) => saveOrgDefaults(acct.id, { require_review_to_land: on })} />
                        </div>
                        <div className="grid gap-2 pt-1 border-t border-rule3">
                          <div className="text-[14px] font-medium">Default labels</div>
                          <LabelEditor labels={orgDefaults?.labels ?? []} onChange={(l) => saveOrgDefaults(acct.id, { labels: l })} />
                        </div>
                      </div>
                    </Card>
                  </div>
                )}
                <div>
                  <Eyebrow label="Members" />
                  <Card>
                    {acct.members.map((m) => (
                      <div key={m.actor} className="px-5 py-3 border-b border-rule2 last:border-0 flex items-center gap-3">
                        <Avatar id={m.actor} handle={m.handle} kind={kindOf(m.actor)} size={30} />
                        <div className="flex-1 min-w-0">
                          <div className="text-[14px] font-medium">{m.handle || m.actor.slice(0, 10)}</div>
                          <div className="text-[12px] text-muted">{kindOf(m.actor) === "agent" ? "agent" : "human"}</div>
                        </div>
                        <span className="text-[11px] font-bold uppercase tracking-[0.03em] px-1.5 py-[3px] rounded-badge bg-rule2 text-dim">{m.role}</span>
                        {amAdmin && me?.id !== m.actor && <Button size="sm" variant="destructive" onClick={() => removeOrgMember(acct.id, m.actor)}>Remove</Button>}
                      </div>
                    ))}
                    {amAdmin && (
                      <div className="px-5 py-3 flex gap-2 items-center bg-paper flex-wrap">
                        <div className="flex-1 min-w-[200px]"><Picker block value={memberPick.actor} placeholder="Add a member…" onChange={(v) => setMemberPick({ ...memberPick, actor: v })}
                          options={candidates.map(actorOption)} /></div>
                        <Picker width={140} value={memberPick.role} onChange={(v) => setMemberPick({ ...memberPick, role: v })}
                          options={["read", "write", "admin", "owner"].map((r) => ({ value: r, label: r }))} />
                        <Button size="sm" disabled={!memberPick.actor} onClick={() => { if (memberPick.actor) { addOrgMember(acct.id, { actor: memberPick.actor }, memberPick.role); setMemberPick({ actor: "", role: "write" }); } }}>Add</Button>
                      </div>
                    )}
                  </Card>
                </div>
                <div>
                  <Eyebrow label="Teams" right={amAdmin ? <span className="flex gap-2 items-center"><input className="box-border h-ctl-sm px-2 rounded-ctl-sm border border-ctl bg-surface font-sans text-xs text-ink outline-none focus:border-body placeholder:text-faint" placeholder="new team…" value={teamDraft} onChange={(e) => setTeamDraft(e.target.value)} onKeyDown={(e) => { if (e.key === "Enter") { createTeam(acct.id, teamDraft); setTeamDraft(""); } }} /><Button size="sm" onClick={() => { createTeam(acct.id, teamDraft); setTeamDraft(""); }}>Create</Button></span> : undefined} />
                  {teams.length === 0 && <div className="text-[13px] text-muted py-4">no teams yet</div>}
                  <div className="grid gap-4">
                    {teams.map((t) => (
                      <Card key={t.id}>
                        <div className="px-5 py-3 border-b border-rule2 flex items-center justify-between">
                          <span className="text-[14px] font-semibold">{t.name}</span>
                          {amAdmin && <Button size="sm" variant="destructive" onClick={() => deleteTeam(acct.id, t.id)}>Delete team</Button>}
                        </div>
                        {t.members.map((m) => (
                          <div key={m.actor} className="px-5 py-2.5 border-b border-rule2 last:border-0 flex items-center gap-3">
                            <Avatar id={m.actor} handle={m.handle} kind={kindOf(m.actor)} size={20} />
                            <span className="flex-1 text-[13.5px]">{m.handle || m.actor.slice(0, 10)}</span>
                            <span className="text-[11px] font-bold uppercase tracking-[0.03em] px-1.5 py-[2px] rounded-badge bg-rule2 text-dim">{m.role}</span>
                            {amAdmin && <button className="text-muted hover:text-fault-text cursor-pointer text-sm" title="remove" onClick={() => teamRemoveMember(acct.id, t.id, m.actor)}>×</button>}
                          </div>
                        ))}
                        {amAdmin && (
                          <div className="px-5 py-2.5 bg-paper">
                            <Picker size="sm" block value="" placeholder="Add to team…" onChange={(v) => { if (v) teamAddMember(acct.id, t.id, { actor: v }, "write"); }}
                              options={acct.members.filter((m) => !t.members.some((x) => x.actor === m.actor)).map((m) => ({ value: m.actor, label: m.handle || m.actor.slice(0, 10) }))} />
                          </div>
                        )}
                      </Card>
                    ))}
                  </div>
                </div>
              </section>
              <aside className="grid gap-5 content-start">
                <Module title="Repositories">
                  {acct.repos.length === 0 && <span className="text-[13px] text-muted">none</span>}
                  {acct.repos.map((rp: string) => (
                    <button key={rp} className="text-left text-[13.5px] text-body hover:text-steel-text cursor-pointer" onClick={() => navigate(`/${encodeURIComponent(acct.handle)}/${encodeURIComponent(rp)}`)}>{rp} →</button>
                  ))}
                  {amAdmin && <div className="pt-1"><LinkButton onClick={() => setNewRepoOpen(true)}>+ new repo</LinkButton></div>}
                </Module>
                {!amAdmin && <Module title="Access"><span className="text-[12.5px] text-muted">You are viewing as a {me ? "member" : "guest"}. Owner/admin rights are needed to manage members and teams.</span></Module>}
              </aside>
            </div>
          </div>
        );
      })()}

      {/* ── REPO · list (issues / voyages) ────────────────────────────────── */}
      {view === "repo" && !currentIssue && !currentPr && (
        <div className="max-w-[1180px] mx-auto px-6 sm:px-8 py-9">
          <div className="flex flex-wrap items-center gap-x-3 gap-y-2 mb-6">
            <h1 className="text-[25px] font-semibold tracking-tight"><button className="text-muted font-normal hover:text-ink transition-colors" onClick={() => navigate(`/orgs/${encodeURIComponent(tenant)}`)}>{tenant}</button><span className="text-faint font-normal mx-1.5">/</span>{issueRepo}</h1>
            {autonomy && <span className="text-[11px] font-bold uppercase tracking-[0.03em] px-1.5 py-[3px] rounded-badge bg-rule2 text-dim" title={TIERS[autonomy.tier]}>{autonomy.tier.toUpperCase()}</span>}
            {secrets.length > 0 && <StatusBadge kind="failed">{secrets.length} secret{secrets.length > 1 ? "s" : ""}</StatusBadge>}
            <div className="ml-auto flex items-center gap-3">
              {!canAct && <span className="text-[12.5px] text-muted">read-only</span>}
            </div>
          </div>

          <div className="flex items-end justify-between gap-4 border-b border-rule2 mb-7">
            <HTabs
              items={[`Issues ${openIssues}`, `Pull requests ${prs.length}`, "Files", "Graph", ...(isTenantOwner ? ["Settings"] : [])]}
              value={tab === "issues" ? 0 : tab === "prs" ? 1 : tab === "files" ? 2 : tab === "graph" ? 3 : 4}
              onChange={(i: number) => navigate([repoBase(), `${repoBase()}/voyages`, `${repoBase()}/files`, `${repoBase()}/graph`, `${repoBase()}/settings`][i])} />
          </div>

          {(tab === "issues" || tab === "prs") && (
          <div className={`grid grid-cols-1 gap-x-12 gap-y-9 ${hasRepoSidebar ? "lg:grid-cols-[1fr_320px]" : ""}`}>
            <section className="min-w-0">
              {tab === "issues" && (
                <>
                  <div className="flex items-center justify-between gap-3 mb-6 flex-wrap">
                    <div className="flex items-center gap-3 min-w-0">
                      <div className="inline-flex items-center gap-0.5 text-[13px] tabular-nums flex-none">
                        {(["open", "closed", "all"] as const).map((fk) => (
                          <button key={fk} onClick={() => setIssueFilter(fk)} className={`px-2.5 py-1 rounded-ctl-sm capitalize transition-colors ${issueFilter === fk ? "bg-rule2 text-ink font-medium" : "text-muted hover:text-ink"}`}>
                            {fk === "open" ? `${openIssues} open` : fk === "closed" ? `${issues.length - openIssues} closed` : "all"}
                          </button>
                        ))}
                      </div>
                      <div className="w-[240px] hidden sm:block"><SearchInput placeholder="Filter issues" shortcut="" value={q} onChange={(e: React.ChangeEvent<HTMLInputElement>) => setQ(e.target.value)} /></div>
                    </div>
                    <div className="flex items-center gap-2.5">
                      <Segmented items={["List", "Board"]} value={issueView === "list" ? 0 : 1} onChange={(i: number) => setIssueView(i === 0 ? "list" : "board")} />
                      {canAct && <Button size="sm" variant="ghost" className="inline-flex items-center gap-1.5" onClick={() => setNewIssueOpen(true)}><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><line x1="12" y1="5" x2="12" y2="19" /><line x1="5" y1="12" x2="19" y2="12" /></svg>New issue</Button>}
                    </div>
                  </div>
                  {issueView === "list" ? (
                    <div>
                      {issues.length === 0 && <div className="py-8 text-[13px] text-muted">no issues yet — open one with the New issue button</div>}
                      {[...issues]
                        .filter((it) => issueFilter === "all" || (issueFilter === "open" ? it.status.state === "open" : it.status.state !== "open"))
                        .filter((it) => matchQ(`${it.title} ${it.body} #${it.number} ${it.labels.join(" ")}`))
                        .sort((a, b) => Number(a.status.state !== "open") - Number(b.status.state !== "open") || b.number - a.number)
                        .map((it) => (
                          <button key={it.number} onClick={() => navigate(`${repoBase()}/issues/${it.number}`)} className="group w-full text-left block border-b border-rule2">
                            <div className="py-3 -mx-3 px-3 rounded-ctl group-hover:bg-surface transition-colors flex items-start gap-3">
                              <span className="mt-0.5 flex-none">
                                {it.status.state === "open" ? (
                                  <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor" className="text-clear-text"><path d="M8 9.5a1.5 1.5 0 1 0 0-3 1.5 1.5 0 0 0 0 3Z" /><path fillRule="evenodd" d="M8 0a8 8 0 1 0 0 16A8 8 0 0 0 8 0ZM1.5 8a6.5 6.5 0 1 1 13 0 6.5 6.5 0 0 1-13 0Z" /></svg>
                                ) : (
                                  <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor" className={it.status.reason === "completed" || !it.status.reason ? "text-steel-text" : "text-muted"}><path fillRule="evenodd" d="M11.28 6.78a.75.75 0 0 0-1.06-1.06L7.25 8.69 5.78 7.22a.75.75 0 0 0-1.06 1.06l2 2a.75.75 0 0 0 1.06 0l3.5-3.5ZM8 1.5a6.5 6.5 0 1 0 0 13 6.5 6.5 0 0 0 0-13ZM0 8a8 8 0 1 1 16 0A8 8 0 0 1 0 8Z" /></svg>
                                )}
                              </span>
                              <div className="flex-1 min-w-0">
                                <div className="flex items-center gap-2 flex-wrap">
                                  <span className="text-[15px] font-semibold group-hover:text-steel-text transition-colors">{it.title}</span>
                                  {it.code_refs.length > 0 && <span className="text-[11.5px] text-steel-text">⬡ {it.code_refs.length}</span>}
                                  {it.resolved_by && <Tag>⬡ resolved</Tag>}
                                  {!it.resolved_by && (it.linked_prs?.length ?? 0) > 0 && <Tag>⇄ {it.linked_prs!.length} PR{it.linked_prs!.length > 1 ? "s" : ""}</Tag>}
                                </div>
                                <div className="text-[12.5px] text-muted mt-1 flex items-center gap-1.5 flex-wrap tabular-nums">
                                  <span>#{it.number}</span>
                                  <span className="text-faint">·</span>
                                  <span>opened by</span>
                                  <Avatar id={it.author} handle={handleOf(it.author)} kind={kindOf(it.author)} size={15} />
                                  <span className={kindOf(it.author) === "agent" ? "text-steel-text" : ""}>{handleOf(it.author)}</span>
                                  {it.assignees.length > 0 && <span className="flex items-center gap-1 ml-1">assigned {it.assignees.map((id) => <Avatar key={id} id={id} handle={handleOf(id)} kind={kindOf(id)} size={15} />)}</span>}
                                </div>
                              </div>
                            </div>
                          </button>
                        ))}
                    </div>
                  ) : (
                    <div className="grid grid-flow-col auto-cols-[minmax(210px,1fr)] gap-4 overflow-x-auto pb-2">
                      {[
                        { k: "open", label: "Open" },
                        { k: "completed", label: "Completed" },
                        { k: "not_planned", label: "Not planned" },
                        { k: "cancelled", label: "Cancelled" },
                        { k: "duplicate", label: "Duplicate" },
                      ].map((col) => {
                        const inCol = issues.filter((i) => (i.status.state === "open" ? "open" : i.status.reason) === col.k && matchQ(`${i.title} ${i.body} #${i.number} ${i.labels.join(" ")}`));
                        if (col.k !== "open" && inCol.length === 0) return null;
                        return (
                          <div key={col.k} className="grid gap-2 content-start">
                            <div className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted flex gap-1.5 mb-1">{col.label} <span className="text-faint">{inCol.length}</span></div>
                            {inCol.map((it) => (
                              <button key={it.number} className="text-left bg-surface border border-rule rounded-ctl p-3 cursor-pointer hover:border-ctl transition-colors" onClick={() => navigate(`${repoBase()}/issues/${it.number}`)}>
                                <div className="text-xs text-faint tabular-nums">#{it.number}</div>
                                <div className="text-[13.5px] font-medium mt-0.5 leading-snug">{it.title}</div>
                                {it.assignees.length > 0 && <div className="text-[11.5px] text-muted mt-1.5">◎ {it.assignees.map((id) => handleOf(id)).join(", ")}</div>}
                              </button>
                            ))}
                          </div>
                        );
                      })}
                    </div>
                  )}
                </>
              )}

              {tab === "prs" && (
                <>
                  <div className="flex items-center gap-3 mb-6 flex-wrap">
                    <div className="inline-flex items-center gap-0.5 text-[13px] tabular-nums flex-none">
                      {(["open", "closed", "all"] as const).map((fk) => {
                        const n = fk === "open" ? prs.filter((p) => p.state === "open").length : fk === "closed" ? prs.filter((p) => p.state !== "open").length : prs.length;
                        return <button key={fk} onClick={() => setPrFilter(fk)} className={`px-2.5 py-1 rounded-ctl-sm capitalize transition-colors ${prFilter === fk ? "bg-rule2 text-ink font-medium" : "text-muted hover:text-ink"}`}>{fk === "all" ? `all ${n}` : `${n} ${fk}`}</button>;
                      })}
                    </div>
                    <div className="w-[240px] hidden sm:block"><SearchInput placeholder="Filter pull requests" shortcut="" value={q} onChange={(e: React.ChangeEvent<HTMLInputElement>) => setQ(e.target.value)} /></div>
                  </div>
                  <div>
                    {prs.length === 0 && <div className="py-8 text-[13px] text-muted">no pull requests yet — agents open these when they push a change for review</div>}
                    {[...prs].filter((p) => prFilter === "all" || (prFilter === "open" ? p.state === "open" : p.state !== "open")).filter((p) => matchQ(`${p.title} #${p.number}`)).sort((a, b) => b.number - a.number).map((p) => {
                      const prReviews = reviews.filter((r) => r.target === `pr:${p.number}`);
                      return (
                        <button key={p.number} onClick={() => navigate(`${repoBase()}/voyages/${p.number}`)} className="group w-full text-left block border-b border-rule2">
                          <div className="py-3 -mx-3 px-3 rounded-ctl group-hover:bg-surface transition-colors flex items-start gap-3">
                            <span className="mt-0.5 flex-none">
                              {p.state === "merged" ? (
                                <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor" className="text-steel-text"><path d="M5.45 5.154A4.25 4.25 0 0 0 9.25 7.5h1.378a2.251 2.251 0 1 1 0 1.5H9.25A5.734 5.734 0 0 1 5 7.123v3.505a2.25 2.25 0 1 1-1.5 0V5.372a2.25 2.25 0 1 1 1.95-.218ZM4.25 13.5a.75.75 0 1 0 0-1.5.75.75 0 0 0 0 1.5Zm8.5-4.5a.75.75 0 1 0 0-1.5.75.75 0 0 0 0 1.5ZM5 3.25a.75.75 0 1 0-1.5 0 .75.75 0 0 0 1.5 0Z" /></svg>
                              ) : p.state === "closed" ? (
                                <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor" className="text-muted"><path d="M3.25 1A2.25 2.25 0 0 1 4 5.372v5.256a2.251 2.251 0 1 1-1.5 0V5.372A2.251 2.251 0 0 1 3.25 1Zm9.5 5.5a.75.75 0 0 1-.75-.75V4.31l-1.97 1.97a.75.75 0 0 1-1.06-1.06L10.94 3.25H9.5a.75.75 0 0 1 0-1.5h3.25a.75.75 0 0 1 .75.75V6.5a.75.75 0 0 1-.75.75ZM3.25 3.5a.75.75 0 1 0 0-1.5.75.75 0 0 0 0 1.5Zm0 9a.75.75 0 1 0 0-1.5.75.75 0 0 0 0 1.5Z" /></svg>
                              ) : (
                                <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor" className="text-clear-text"><path d="M1.5 3.25a2.25 2.25 0 1 1 3 2.122v5.256a2.251 2.251 0 1 1-1.5 0V5.372A2.25 2.25 0 0 1 1.5 3.25Zm5.677-.177L9.573.677A.25.25 0 0 1 10 .854V2.5h1A2.5 2.5 0 0 1 13.5 5v5.628a2.251 2.251 0 1 1-1.5 0V5a1 1 0 0 0-1-1h-1v1.646a.25.25 0 0 1-.427.177L7.177 3.427a.25.25 0 0 1 0-.354ZM3.75 2.5a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5Zm0 9.5a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5Zm8.25.75a.75.75 0 1 0 1.5 0 .75.75 0 0 0-1.5 0Z" /></svg>
                              )}
                            </span>
                            <div className="flex-1 min-w-0">
                              <div className="flex items-center gap-2 flex-wrap">
                                <span className="text-[15px] font-semibold group-hover:text-steel-text transition-colors">{p.title}</span>
                                {p.state === "open" && (() => {
                                  const vChanges = prReviews.some((r) => r.verdict === "request_changes" || r.verdict === "reject");
                                  const vApprove = prReviews.some((r) => r.verdict === "approve" && r.reviewer !== p.author);
                                  const vBlockers = prReviews.some((r) => (r.findings ?? []).some((f) => f.severity === "blocker"));
                                  const cks = [p.verification === "green", !vBlockers, vApprove && !vChanges];
                                  const pass = cks.filter(Boolean).length;
                                  const bad = p.verification === "red" || vChanges || vBlockers;
                                  const dot = bad ? "bg-fault" : pass === cks.length ? "bg-clear" : "bg-brass";
                                  const cls = bad ? "bg-fault-wash text-fault-text" : pass === cks.length ? "bg-clear-wash text-clear-text" : "bg-brass-wash text-brass-text";
                                  return <span className={`inline-flex items-center gap-1 text-[11px] font-bold uppercase tracking-[0.03em] px-1.5 py-[3px] rounded-badge ${cls}`}><span className={`w-1.5 h-1.5 rounded-full ${dot}`} />{pass}/{cks.length} checks</span>;
                                })()}
                                {prReviews.length > 0 && <Tag>{prReviews.length} review{prReviews.length > 1 ? "s" : ""}</Tag>}
                              </div>
                              <div className="text-[12.5px] text-muted mt-1 flex items-center gap-1.5 flex-wrap tabular-nums">
                                <span>v{p.number}</span>
                                <span className="text-faint">·</span>
                                <span className="text-steel-text" title={`proposes keel change ${p.changes[0]}`}>⬡ {(p.changes[0] ?? "").slice(0, 8)}</span>
                                <span className="text-faint">·</span>
                                <Avatar id={p.author} handle={handleOf(p.author)} kind={kindOf(p.author)} size={15} />
                                <span className={kindOf(p.author) === "agent" ? "text-steel-text" : ""}>{handleOf(p.author)}</span>
                                {p.reviewers?.length > 0 && <span className="flex items-center gap-1 ml-1">reviewers {p.reviewers.map((id) => <Avatar key={id} id={id} handle={handleOf(id)} kind={kindOf(id)} size={15} />)}</span>}
                              </div>
                            </div>
                          </div>
                        </button>
                      );
                    })}
                  </div>
                </>
              )}
            </section>

            {hasRepoSidebar && repoSidebar}
          </div>
          )}

          {tab === "files" && (
            <RepoFiles tenant={tenant} repo={issueRepo} authHeaders={authHeaders} theme={theme} />
          )}

          {tab === "graph" && (
            <RepoGraph tenant={tenant} repo={issueRepo} authHeaders={authHeaders} />
          )}

          {tab === "settings" && !isTenantOwner && (
            <Card><div className="px-6 py-8 grid gap-2"><h2 className="text-[16px] font-semibold">Not authorized</h2><p className="text-[13.5px] text-muted">Repository settings are visible only to owners and admins of <b className="text-body">{tenant}</b>. {!me && <>You are not signed in.</>}</p></div></Card>
          )}
          {tab === "settings" && isTenantOwner && (() => {
            const s = repoSettings;
            const reviewerCandidates = actors;
            return (
            <div className="grid gap-5 max-w-[760px]">
              <Card>
                <SectionHeader label="General" />
                <div className="px-5 py-4 grid gap-4">
                  <div className="flex items-center justify-between gap-4">
                    <div>
                      <div className="text-[13.5px] font-medium">Visibility</div>
                      <div className="text-[12.5px] text-muted">{(s?.visibility ?? (s?.private ? "private" : "public")) === "private" ? "Only members can see it." : (s?.visibility === "unlisted" ? "Anyone with the link can see it, but it's hidden from listings." : "Anyone can find and view it.")}</div>
                    </div>
                    <Picker width={200} value={s?.visibility ?? (s?.private ? "private" : "public")} onChange={(v) => isTenantOwner && saveRepoSettings({ visibility: v as "public" | "private" | "unlisted" })}
                      options={[{ value: "public", label: "Public" }, { value: "unlisted", label: "Unlisted" }, { value: "private", label: "Private" }]} />
                  </div>
                  <div className="flex items-center justify-between gap-4">
                    <div><div className="text-[13.5px] font-medium">Require a review to merge</div><div className="text-[12.5px] text-muted">An approving review is needed to land.</div></div>
                    <Switch on={!!s?.require_review_to_land} onChange={(on: boolean) => isTenantOwner && saveRepoSettings({ require_review_to_land: on })} />
                  </div>
                  <div className="flex items-center justify-between gap-4">
                    <div><div className="text-[13.5px] font-medium">Author-independence gate</div><div className="text-[12.5px] text-muted">An approval must come from someone other than the author (no self-merge). Turn off for a solo repo.</div></div>
                    <Switch on={s?.author_independence ?? true} onChange={(on: boolean) => isTenantOwner && saveRepoSettings({ author_independence: on })} />
                  </div>
                </div>
              </Card>
              {(() => {
                const coOwners = [...new Set(ownerRules.flatMap((r) => r.owners))];
                const isOwner = (actor: string) => coOwners.includes(actor);
                const missingOwners = coOwners.filter((o) => !(s?.default_reviewers ?? []).some((x) => x.actor === o));
                return (
                <Card>
                <SectionHeader label="Default reviewers" right={<span className="text-[12.5px] text-muted">auto-requested on new pull requests · code owners are pre-suggested</span>} />
                <div className="px-5 py-4 grid gap-3">
                  <div className="flex flex-wrap gap-1.5">
                    {(s?.default_reviewers ?? []).map((r) => (
                      <span key={r.actor} className="inline-flex items-center gap-1 text-xs px-2 py-1 rounded-chip bg-paper border border-rule">
                        {r.handle || r.actor.slice(0, 8)}
                        {isOwner(r.actor) && <span className="text-[9.5px] font-bold uppercase tracking-[0.05em] text-steel-text bg-steel-wash rounded-[3px] px-1" title="also a code owner">owner</span>}
                        {isTenantOwner && <button className="text-muted hover:text-fault-text cursor-pointer" onClick={() => saveRepoSettings({ default_reviewers: (s!.default_reviewers.filter((x) => x.actor !== r.actor)).map((x) => x.actor) })}>×</button>}
                      </span>
                    ))}
                    {(s?.default_reviewers ?? []).length === 0 && <span className="text-[12.5px] text-muted">none</span>}
                  </div>
                  {isTenantOwner && (
                    <div className="max-w-[280px]"><Picker block value="" placeholder="Add a reviewer…" onChange={(v) => { if (v && s) saveRepoSettings({ default_reviewers: [...new Set([...s.default_reviewers.map((x) => x.actor), v])] }); }}
                      options={reviewerCandidates.filter((a) => !(s?.default_reviewers ?? []).some((x) => x.actor === a.id)).map((a) => ({ ...actorOption(a), sub: `${(a.email && a.email.trim()) || a.kind}${isOwner(a.id) ? " · code owner" : ""}` }))} /></div>
                  )}
                  {isTenantOwner && coOwners.length > 0 && (
                    <div className="text-[12px] text-muted pt-2 border-t border-rule3 flex flex-wrap items-center gap-x-1.5 gap-y-1">
                      <span>Code owners:</span>
                      <span className="text-body">{coOwners.map((o) => handleOf(o)).join(", ")}.</span>
                      {missingOwners.length > 0 && s && <button className="text-steel-text font-medium hover:underline" onClick={() => saveRepoSettings({ default_reviewers: [...new Set([...s.default_reviewers.map((x) => x.actor), ...coOwners])] })}>Add {missingOwners.length} as default reviewer{missingOwners.length > 1 ? "s" : ""} →</button>}
                    </div>
                  )}
                </div>
              </Card>
                );
              })()}
              <Card>
                <SectionHeader label="Team access" />
                <div className="px-5 py-4 grid gap-3">
                  {(s?.team_access ?? []).map((ta, i) => {
                    const tm = teams.find((t) => t.id === ta.team);
                    return (
                      <div key={i} className="flex items-center gap-3">
                        <span className="flex-1 text-[13.5px]">{tm?.name ?? ta.team}</span>
                        <span className="text-[11px] font-bold uppercase tracking-[0.03em] px-1.5 py-[3px] rounded-badge bg-rule2 text-dim">{ta.role}</span>
                        {isTenantOwner && <button className="text-muted hover:text-fault-text cursor-pointer" onClick={() => saveRepoSettings({ team_access: (s!.team_access.filter((_, j) => j !== i)) })}>×</button>}
                      </div>
                    );
                  })}
                  {(s?.team_access ?? []).length === 0 && <span className="text-[12.5px] text-muted">no teams have explicit access</span>}
                  {isTenantOwner && teams.length > 0 && (
                    <div className="max-w-[280px]"><Picker block value="" placeholder="Grant a team access…" onChange={(v) => { if (v && s) saveRepoSettings({ team_access: [...s.team_access, { team: v, role: "write" }] }); }}
                      options={teams.filter((t) => !(s?.team_access ?? []).some((x) => x.team === t.id)).map((t) => ({ value: t.id, label: t.name }))} /></div>
                  )}
                  {teams.length === 0 && <span className="text-[12px] text-faint">create teams in the org page to grant team access</span>}
                </div>
              </Card>
              <Card>
                <SectionHeader label="Labels" right={<span className="text-[12.5px] text-muted">the only labels issues can use</span>} />
                <div className="px-5 py-4">
                  {isTenantOwner
                    ? <LabelEditor labels={s?.labels ?? []} onChange={(l) => saveRepoSettings({ labels: l })} />
                    : <div className="flex flex-wrap gap-1.5">{(s?.labels ?? []).map((l) => <Label key={l.name} name={l.name} color={l.color} icon={l.icon} />)}{(s?.labels ?? []).length === 0 && <span className="text-[12.5px] text-muted">none</span>}</div>}
                </div>
              </Card>
              <Card>
                <SectionHeader label="Automation" right={<span className="text-[12.5px] text-muted">autonomy tier</span>} />
                <div className="px-5 py-4 grid gap-3">
                  {autonomy && (
                    <>
                      <div className="flex items-center gap-2">
                        <StatusBadge kind={autonomy.tier === "t0" ? "queued" : "agent"}>{autonomy.tier.toUpperCase()}</StatusBadge>
                        <span className="text-[12.5px] text-muted">{TIERS[autonomy.tier]}</span>
                      </div>
                      {isTenantOwner && <div className="max-w-[160px]"><Select options={["t0", "t1", "t2", "t3"].map((t) => t.toUpperCase())} value={autonomy.tier.toUpperCase()} onChange={(v: string) => setTier(v.toLowerCase())} /></div>}
                    </>
                  )}
                </div>
              </Card>
              {mirror?.target && (
                <Card>
                  <SectionHeader label="Mirror" right={<span className="text-[12.5px] text-muted">outbound delivery</span>} />
                  <div className="px-5 py-4 grid gap-1.5 text-[13px]">
                    <Stat k="mirror" v={<code className="text-[12px]">{mirror.target}</code>} />
                    <Stat k="pushed outbound" v={`${mirror.outbound.length}`} />
                    <p className="text-[11.5px] text-faint leading-[1.5] pt-1">Loop-safe: forge-originated changes are never pushed back.</p>
                  </div>
                </Card>
              )}
              <Card>
                <SectionHeader label="Code owners" right={<span className="text-[12.5px] text-muted">path → owners · also .hull/CODEOWNERS</span>} />
                <div className="px-5 py-4 grid gap-2">
                  {ownerRules.length === 0 && <span className="text-[12.5px] text-muted">no code-owner rules</span>}
                  {ownerRules.map((r, i) => (
                    <div key={i} className="flex items-center gap-3 text-[13px]">
                      <code className="text-body">{r.glob}</code>
                      <span className="text-muted flex-1">{r.owners.map((o) => handleOf(o)).join(", ")}</span>
                    </div>
                  ))}
                </div>
              </Card>
            </div>
            );
          })()}
        </div>
      )}
    </div>
  );
}
// A GitHub-style contribution heatmap: ~53 weeks × 7 days. Each cell is split into TWO triangles —
// top-left = your own contributions, bottom-right = your agents' — each shaded by its own intensity.
// Columns flex to fill the width, so there's never a scrollbar.
type HeatDay = { day: number; human: number; agent: number };
const HEAT_EMPTY = "var(--rule2)"; // theme-aware neutral for days with no contributions
const HEAT_YOU = [HEAT_EMPTY, "#9be9a8", "#40c463", "#30a14e", "#216e39"]; // green
const HEAT_AGENT = [HEAT_EMPTY, "#a9c9f5", "#5a9bd4", "#2f6fb0", "#1b4d80"]; // blue
function ContributionHeatmap({ days }: { days: HeatDay[] }) {
  const byDay = new Map(days.map((d) => [d.day, d]));
  const today = Math.floor(Date.now() / 86_400_000);
  const start = today - 371;
  const dow = (e: number) => ((e % 7) + 4) % 7; // epoch day 0 = Thursday; 0=Sun … 6=Sat
  const gridStart = start - dow(start);
  const maxH = Math.max(1, ...days.map((d) => d.human));
  const maxA = Math.max(1, ...days.map((d) => d.agent));
  const lvl = (c: number, m: number) => (c <= 0 ? 0 : c <= m * 0.25 ? 1 : c <= m * 0.5 ? 2 : c <= m * 0.75 ? 3 : 4);
  const weeks: number[][] = [];
  for (let w = 0; gridStart + w * 7 <= today; w++) {
    const col: number[] = [];
    for (let d = 0; d < 7; d++) { const e = gridStart + w * 7 + d; col.push(e >= start && e <= today ? e : -1); }
    weeks.push(col);
  }
  const fmt = (e: number) => new Date(e * 86_400_000).toLocaleDateString(undefined, { month: "short", day: "numeric" });
  return (
    <div className="grid gap-2">
      <div className="flex gap-[2px] w-full">
        {weeks.map((col, i) => (
          <div key={i} className="grid gap-[2px] flex-1 min-w-0 content-start">
            {col.map((e, j) => {
              if (e < 0) return <span key={j} className="aspect-square" />;
              const d = byDay.get(e); const h = d?.human ?? 0; const a = d?.agent ?? 0;
              const bg = h === 0 && a === 0 ? HEAT_EMPTY : `linear-gradient(to bottom right, ${HEAT_YOU[lvl(h, maxH)]} 0 50%, ${HEAT_AGENT[lvl(a, maxA)]} 50% 100%)`;
              return <span key={j} title={`${h + a} contribution${h + a === 1 ? "" : "s"} — ${h} you, ${a} agents · ${fmt(e)}`} className="aspect-square rounded-[2px]" style={{ background: bg }} />;
            })}
          </div>
        ))}
      </div>
      <div className="flex items-center gap-4 text-[11.5px] text-muted self-end">
        <span className="inline-flex items-center gap-1.5"><span className="w-3 h-3 rounded-[2px]" style={{ background: HEAT_YOU[3] }} />you</span>
        <span className="inline-flex items-center gap-1.5"><span className="w-3 h-3 rounded-[2px]" style={{ background: HEAT_AGENT[3] }} />agents</span>
      </div>
    </div>
  );
}

// ── Graph tab: the codebase as a force-directed import graph, searchable ─────────────────────────
type GNode = { path: string; dir: string; lang: string; size: number; deg: number };
type GEdge = { from: string; to: string };
const LANG_COLOR: Record<string, string> = { rust: "#dea584", ts: "#3178c6", js: "#f1e05a", python: "#3572A5", go: "#00ADD8" };
function RepoGraph({ tenant, repo, authHeaders }: { tenant: string; repo: string; authHeaders: () => Record<string, string> }) {
  const base = `/api/repos/${encodeURIComponent(tenant)}/${repo}`;
  const [branch, setBranch] = useState("main");
  const [branches, setBranches] = useState<string[]>([]);
  const [data, setData] = useState<{ nodes: GNode[]; edges: GEdge[] } | null>(null);
  const [loading, setLoading] = useState(true);
  const [q, setQ] = useState("");
  const [hover, setHover] = useState<string | null>(null);
  useEffect(() => { fetch(`${base}/branches`, { headers: authHeaders() }).then((r) => r.json()).then((d) => { const bs = d.branches ?? []; setBranches(bs); setBranch((b) => (bs.length && !bs.includes(b) ? bs[0] : b)); }).catch(() => {}); }, [tenant, repo]);
  useEffect(() => {
    setLoading(true);
    fetch(`${base}/graph?ref=${encodeURIComponent(branch)}`, { headers: authHeaders() }).then((r) => r.json()).then((d) => setData({ nodes: d.nodes ?? [], edges: d.edges ?? [] })).catch(() => setData({ nodes: [], edges: [] })).finally(() => setLoading(false));
  }, [branch, tenant, repo]);

  // Deterministic force-directed layout (seeded on a circle by index; no RNG). O(n²)·iters, capped.
  const W = 900, H = 560;
  const laid = useMemo(() => {
    const nodes = data?.nodes ?? []; const edges = data?.edges ?? [];
    const n = nodes.length; if (n === 0) return { pos: [] as { x: number; y: number }[], vb: `0 0 ${W} ${H}` };
    const idx = new Map(nodes.map((nd, i) => [nd.path, i]));
    const E = edges.map((e) => [idx.get(e.from), idx.get(e.to)]).filter(([a, b]) => a != null && b != null) as [number, number][];
    const pos = nodes.map((_, i) => { const a = (i / n) * Math.PI * 2; return { x: W / 2 + Math.cos(a) * 200, y: H / 2 + Math.sin(a) * 160, vx: 0, vy: 0 }; });
    const iters = n > 400 ? 120 : n > 150 ? 220 : 320;
    for (let it = 0; it < iters; it++) {
      for (let i = 0; i < n; i++) for (let j = i + 1; j < n; j++) { const dx = pos[i].x - pos[j].x, dy = pos[i].y - pos[j].y; const d2 = dx * dx + dy * dy + 0.01; const d = Math.sqrt(d2); const f = 2600 / d2; pos[i].vx += (dx / d) * f; pos[i].vy += (dy / d) * f; pos[j].vx -= (dx / d) * f; pos[j].vy -= (dy / d) * f; }
      for (const [a, b] of E) { const dx = pos[b].x - pos[a].x, dy = pos[b].y - pos[a].y; const d = Math.sqrt(dx * dx + dy * dy) + 0.01; const f = (d - 90) * 0.02; pos[a].vx += (dx / d) * f; pos[a].vy += (dy / d) * f; pos[b].vx -= (dx / d) * f; pos[b].vy -= (dy / d) * f; }
      for (let i = 0; i < n; i++) { pos[i].vx += (W / 2 - pos[i].x) * 0.006; pos[i].vy += (H / 2 - pos[i].y) * 0.006; pos[i].x += pos[i].vx * 0.85; pos[i].y += pos[i].vy * 0.85; pos[i].vx *= 0.82; pos[i].vy *= 0.82; }
    }
    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    for (const p of pos) { minX = Math.min(minX, p.x); minY = Math.min(minY, p.y); maxX = Math.max(maxX, p.x); maxY = Math.max(maxY, p.y); }
    const pad = 40;
    return { pos, vb: `${minX - pad} ${minY - pad} ${maxX - minX + pad * 2} ${maxY - minY + pad * 2}` };
  }, [data]);

  const nodes = data?.nodes ?? []; const edges = data?.edges ?? [];
  const ql = q.trim().toLowerCase();
  const match = (p: string) => ql !== "" && p.toLowerCase().includes(ql);
  const anyMatch = ql !== "" && nodes.some((n) => match(n.path));
  const posOf = new Map(nodes.map((n, i) => [n.path, laid.pos[i]]));
  const base2 = (p: string) => p.split("/").pop() ?? p;

  return (
    <div className="grid gap-4">
      <div className="flex items-center gap-3 flex-wrap">
        <div className="min-w-[220px]"><Picker value={branch} onChange={setBranch} options={branches.map((b) => ({ value: b, label: b }))} placeholder="branch" width={340} size="sm" block searchable /></div>
        <span className="text-[12.5px] text-muted">{nodes.length} files · {edges.length} imports</span>
        <div className="flex-1 min-w-[220px] max-w-[420px] ml-auto"><SearchInput placeholder="Search the graph…" shortcut="" value={q} onChange={(e: React.ChangeEvent<HTMLInputElement>) => setQ(e.target.value)} /></div>
      </div>
      <Card>
        <div className="relative">
          {loading && <div className="absolute inset-0 grid place-items-center text-[13px] text-muted z-10">Building graph…</div>}
          {!loading && nodes.length === 0 && <div className="py-16 text-center text-[13px] text-muted">No source files with resolvable imports on this branch.</div>}
          {nodes.length > 0 && (
            <svg viewBox={laid.vb} className="w-full" style={{ height: 620 }} preserveAspectRatio="xMidYMid meet">
              {edges.map((e, i) => { const a = posOf.get(e.from), b = posOf.get(e.to); if (!a || !b) return null; const dim = anyMatch && !(match(e.from) || match(e.to)); return <line key={i} x1={a.x} y1={a.y} x2={b.x} y2={b.y} stroke="currentColor" className={dim ? "text-rule3" : "text-rule"} strokeWidth={1} opacity={dim ? 0.3 : 0.7} />; })}
              {nodes.map((n, i) => {
                const p = laid.pos[i]; if (!p) return null;
                const r = 4 + Math.min(9, n.deg * 1.4);
                const on = !anyMatch || match(n.path);
                const showLabel = hover === n.path || n.deg >= 4 || match(n.path);
                return (
                  <g key={n.path} transform={`translate(${p.x} ${p.y})`} onMouseEnter={() => setHover(n.path)} onMouseLeave={() => setHover(null)} style={{ cursor: "pointer" }} opacity={on ? 1 : 0.25}>
                    <circle r={r} fill={LANG_COLOR[n.lang] || "#8b949e"} stroke={match(n.path) ? "#111827" : "#ffffff"} strokeWidth={match(n.path) ? 2 : 1} />
                    {showLabel && <text x={r + 3} y={4} className="fill-body" style={{ fontSize: 11, paintOrder: "stroke", stroke: "var(--surface)", strokeWidth: 3 }}>{base2(n.path)}</text>}
                    <title>{n.path} · {n.deg} imports</title>
                  </g>
                );
              })}
            </svg>
          )}
        </div>
      </Card>
      <div className="flex items-center gap-4 text-[12px] text-muted flex-wrap">
        {Object.entries(LANG_COLOR).map(([l, c]) => nodes.some((n) => n.lang === l) && <span key={l} className="inline-flex items-center gap-1.5"><span className="w-2.5 h-2.5 rounded-full" style={{ background: c }} />{l}</span>)}
        <span className="text-faint">· node size = how many files import it · drag-free force layout</span>
      </div>
    </div>
  );
}

// ── Files tab: a branch-aware file browser with fuzzy/full-text search ──────────────────────────
type SearchHit = { path: string; line: number; text: string; kind: "path" | "content" };
function RepoFiles({ tenant, repo, authHeaders, theme }: { tenant: string; repo: string; authHeaders: () => Record<string, string>; theme: string }) {
  const base = `/api/repos/${encodeURIComponent(tenant)}/${repo}`;
  const [branches, setBranches] = useState<string[]>([]);
  const [branch, setBranch] = useState("main");
  const [file, setFile] = useState<{ path: string; text: string; binary: boolean; size: number } | null>(null);
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<SearchHit[] | null>(null);
  const [allPaths, setAllPaths] = useState<string[] | null>(null);

  useEffect(() => {
    fetch(`${base}/branches`, { headers: authHeaders() }).then((r) => r.json()).then((d) => {
      const bs: string[] = d.branches ?? [];
      setBranches(bs);
      setBranch((b) => (bs.length && !bs.includes(b) ? bs[0] : b));
    }).catch(() => {});
  }, [tenant, repo]);

  // Every file path in the branch — feeds the @pierre/trees sidebar.
  useEffect(() => {
    setAllPaths(null);
    fetch(`${base}/tree?ref=${encodeURIComponent(branch)}&flat=1`, { headers: authHeaders() })
      .then((r) => r.json()).then((d) => setAllPaths(d.paths ?? [])).catch(() => setAllPaths([]));
  }, [branch, tenant, repo]);

  const openFile = (p: string) => {
    setResults(null); setQuery("");
    fetch(`${base}/blob?ref=${encodeURIComponent(branch)}&path=${encodeURIComponent(p)}`, { headers: authHeaders() })
      .then((r) => r.json()).then((d) => setFile({ path: p, text: d.text ?? "", binary: !!d.binary, size: d.size ?? 0 })).catch(() => {});
  };
  // Clear any open file / search when the branch changes (the tree reloads via the paths effect).
  useEffect(() => { setResults(null); setQuery(""); setFile(null); /* eslint-disable-next-line */ }, [branch]);
  // Debounced fuzzy/full-text search.
  useEffect(() => {
    if (!query.trim()) { setResults(null); return; }
    const t = setTimeout(() => {
      fetch(`${base}/search?ref=${encodeURIComponent(branch)}&q=${encodeURIComponent(query)}`, { headers: authHeaders() })
        .then((r) => r.json()).then((d) => setResults(d.hits ?? [])).catch(() => setResults([]));
    }, 250);
    return () => clearTimeout(t);
  }, [query, branch]);

  const FileIcon = () => <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" className="text-muted flex-none"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" /><polyline points="14 2 14 8 20 8" /></svg>;
  const fmtSize = (n: number) => (n < 1024 ? `${n} B` : n < 1024 * 1024 ? `${(n / 1024).toFixed(1)} KB` : `${(n / 1024 / 1024).toFixed(1)} MB`);
  const LINE_CAP = 1500;
  const lines = file ? file.text.replace(/\n$/, "").split("\n") : [];

  return (
    <div className="grid gap-4">
      <div className="flex items-center gap-3 flex-wrap">
        <div className="min-w-[240px]"><Picker value={branch} onChange={setBranch} options={branches.map((b) => ({ value: b, label: b }))} placeholder="branch" width={340} size="sm" block searchable /></div>
        <div className="flex-1 min-w-[220px] max-w-[440px] ml-auto">
          <SearchInput placeholder="Search files & content…" shortcut="" value={query} onChange={(e: React.ChangeEvent<HTMLInputElement>) => setQuery(e.target.value)} />
        </div>
      </div>

      {results !== null ? (
        <Card>
          <SectionHeader label="Search" right={<span className="text-[12.5px] text-muted">{results.length} hit{results.length === 1 ? "" : "s"} · {branch}</span>} />
          <div className="max-h-[560px] overflow-y-auto">
            {results.length === 0 && <div className="px-5 py-6 text-[13px] text-muted">No matches for “{query}”.</div>}
            {results.map((h, i) => (
              <button key={i} onClick={() => openFile(h.path)} className="w-full text-left px-5 py-2 border-b border-rule3 hover:bg-paper/60 flex items-start gap-3">
                <span className="text-[11px] font-bold uppercase tracking-[0.05em] mt-0.5 flex-none w-[52px] text-right">{h.kind === "path" ? <span className="text-steel-text">name</span> : <span className="text-faint tabular-nums">L{h.line}</span>}</span>
                <span className="min-w-0">
                  <span className="block text-[12.5px] text-body font-medium truncate">{h.path}</span>
                  {h.kind === "content" && <span className="block text-[12px] text-muted truncate"><Hl text={h.text} path={h.path} /></span>}
                </span>
              </button>
            ))}
          </div>
        </Card>
      ) : (
        <div className="grid grid-cols-[minmax(190px,270px)_1fr] gap-4 items-start">
          <Card className="overflow-hidden">
            <SectionHeader label="Files" right={<span className="text-[11.5px] text-faint truncate max-w-[110px]">{branch}</span>} />
            <div className="py-1.5 pl-1.5 pr-1">
              {allPaths === null ? (
                <div className="px-3 py-4 text-[12.5px] text-muted">Loading…</div>
              ) : allPaths.length === 0 ? (
                <div className="px-3 py-4 text-[12.5px] text-muted">No files.</div>
              ) : (
                <Suspense fallback={<div className="px-3 py-4 text-[12.5px] text-muted">Loading tree…</div>}>
                  <RepoTree paths={allPaths} selected={file?.path ?? null} onSelect={openFile} />
                </Suspense>
              )}
            </div>
          </Card>
          <div className="min-w-0">
      {file ? (
        <Card>
          <div className="flex items-center justify-between gap-3 px-5 py-3 border-b border-rule2">
            <div className="flex items-center gap-1.5 text-[13px] min-w-0">
              <FileIcon /><span className="font-medium truncate">{file.path}</span>
            </div>
            <span className="text-[12px] text-faint flex-none tabular-nums">{fmtSize(file.size)}</span>
          </div>
          {file.binary ? (
            <div className="px-5 py-8 text-[13px] text-muted">Binary file · {fmtSize(file.size)} — not shown.</div>
          ) : (
            // Primary: Shiki-powered @pierre/diffs viewer. Falls back to our built-in line viewer.
            <Boundary fallback={
              <div className="text-[12.5px] leading-[1.6] overflow-x-auto">
                {lines.slice(0, LINE_CAP).map((ln, i) => (
                  <div key={i} className="grid grid-cols-[52px_1fr] hover:bg-paper/50">
                    <span className="pr-3 py-0.5 text-right text-faint bg-paper/40 border-r border-rule3 select-none tabular-nums text-[11px]">{i + 1}</span>
                    <span className="px-3 py-0.5 whitespace-pre-wrap break-words"><Hl text={ln} path={file.path} /></span>
                  </div>
                ))}
                {lines.length > LINE_CAP && <div className="px-5 py-3 text-[12.5px] text-muted border-t border-rule2">{lines.length - LINE_CAP} more lines not shown ({fmtSize(file.size)} file).</div>}
              </div>
            }>
              <Suspense fallback={<div className="px-5 py-8 text-[13px] text-muted">Loading viewer…</div>}>
                {/* @pierre/diffs `--diffs-*` vars cascade into its shadow DOM — map them onto hull tokens so
                    the gutter, separators and numbers read as native chrome and follow the theme toggle. */}
                <div className="text-[13px] overflow-x-auto max-h-[72vh] overflow-y-auto" style={{
                  "--diffs-bg": "var(--surface)",
                  "--diffs-fg-number": "var(--faint)",
                  "--diffs-font-size": "12.5px",
                  "--diffs-line-height": "1.7",
                  "--diffs-min-number-column-width": "2.75rem",
                } as React.CSSProperties}>
                  <PierreFile file={{ name: file.path, contents: file.text }} disableWorkerPool
                    options={{ theme: { light: "github-light", dark: "github-dark" }, themeType: theme === "dark" ? "dark" : "light", overflow: "scroll", disableFileHeader: true, lineHoverHighlight: "line", tokenizeMaxLength: 400_000,
                      // The gutter/number bg is a theme-computed var we can't override cleanly, so tint it
                      // directly in the shadow DOM (inherited hull tokens resolve inside `unsafeCSS`).
                      unsafeCSS: "[data-gutter]{background:var(--paper);border-right:1px solid var(--rule2)}[data-line-number-content]{padding-right:14px;opacity:.85}" }} />
                </div>
              </Suspense>
            </Boundary>
          )}
        </Card>
      ) : (
        <Card className="grid place-items-center min-h-[420px] text-center">
          <div className="px-6 py-10 max-w-[320px]">
            <div className="mx-auto w-11 h-11 grid place-items-center rounded-full bg-paper text-dim mb-3">
              <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" /><polyline points="14 2 14 8 20 8" /></svg>
            </div>
            <div className="text-[14px] font-medium text-body">Select a file</div>
            <div className="text-[12.5px] text-muted mt-1 leading-[1.5]">Pick a file from the tree on the left to view its contents, or search across the branch above.</div>
          </div>
        </Card>
      )}
          </div>
        </div>
      )}
    </div>
  );
}
/** The review "package" — a dedicated page synthesizing what a reviewer needs, not a one-liner. */
function ReviewPage({
  review,
  reviews = [],
  landGate,
  reviewTools,
  onReviewsChanged,
  canFix = true,
  canReview = false,
  onTriage,
  triaging = false,
  pr,
  actors,
  tenant,
  repo,
  token,
  me,
  onBack,
}: {
  review: Review;
  reviews?: Review[];
  landGate?: React.ReactNode;
  reviewTools?: React.ReactNode;
  onReviewsChanged?: () => void;
  canFix?: boolean;
  canReview?: boolean;
  onTriage?: () => void;
  triaging?: boolean;
  pr: PR | null;
  actors: Actor[];
  tenant: string;
  repo: string;
  token: string;
  me: { id: string; handle: string; kind: string } | null;
  onBack: () => void;
}) {
  const authHeaders = (): Record<string, string> => (token ? { authorization: `Bearer ${token}` } : {});
  const canAct = !!me;
  // Which verdict's lens we're viewing the package through (default: the primary review). The
  // synthesized package itself — diff, reconciliation, checks — is the page; verdicts are a strip.
  const [activeId, setActiveId] = useState<string | null>(null);
  const active = reviews.find((r) => r.id === activeId) ?? review;
  type Session = { task: string; model: string; lesson: string; tool_calls: number; tokens_in: number; tokens_out: number };
  type ChangeInfo = {
    id: string;
    intent: string;
    author: string;
    verification: string;
    files: { path: string; status: string }[];
    session?: Session;
  };
  type DiffLine = { tag: string; text: string };
  type FileDiff = { path: string; status: string; ops: string[]; hunks: { old_start: number; new_start: number; lines: DiffLine[] }[]; too_large?: boolean };
  type Evidence = { kind: string; detail: string; supports: boolean };
  type Claim = { id: string; text: string; source: string; status: string; evidence: Evidence[] };
  type Ledger = { change: string; claims: Claim[]; unclaimed?: string[] };
  const [change, setChange] = useState<ChangeInfo | null>(null);
  const mentions = actors.map((a) => ({ handle: a.handle, kind: a.kind, email: a.email, avatar: <Avatar id={a.id} handle={a.handle} kind={a.kind} size={22} /> }));
  const [diff, setDiff] = useState<FileDiff[]>([]);
  type Semantic = { moves: { from: string; to: string; blob: string }[]; added: string[]; deleted: string[]; modified: string[]; whitespace_only: string[]; behavioral: string[]; pure_move: boolean; mechanical: boolean };
  const [semantic, setSemantic] = useState<Semantic | null>(null);
  const [ledger, setLedger] = useState<Ledger | null>(null);
  const handleOf = (id: string) => actors.find((a) => a.id === id)?.handle ?? id.slice(0, 8);
  const changeId = pr?.changes[0];
  const loadChange = () => {
    if (!changeId) return;
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}`, { headers: authHeaders() })
      .then((r) => r.json())
      .then((d) => setChange(d.change))
      .catch(() => {});
  };
  useEffect(loadChange, [changeId, tenant, repo]);
  useEffect(() => {
    if (!changeId) return;
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}/diff`, { headers: authHeaders() })
      .then((r) => r.json())
      .then((d) => setDiff(d.files ?? []))
      .catch(() => {});
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}/semantic`, { headers: authHeaders() })
      .then((r) => r.json())
      .then((d) => setSemantic(d.semantic ?? null))
      .catch(() => {});
  }, [changeId, tenant, repo]);
  // If this review carries an immutable ledger snapshot (an agent reconciliation review), show that
  // — it's the evidence the verdict was actually based on. Otherwise reconcile live.
  const snapshot = active.ledger ?? null;
  const loadLedger = () => {
    if (snapshot || !changeId) return;
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}/ledger`, { headers: authHeaders() })
      .then((r) => r.json())
      .then((d) => setLedger(d.ledger))
      .catch(() => {});
  };
  // Reconcile after verification is known, so a green/red signal is reflected in the claim statuses.
  useEffect(loadLedger, [changeId, tenant, repo, change?.verification]);
  const shownLedger = snapshot ?? ledger;

  // Human resolutions of claims (the needs-judgment action). Fetched from the live ledger (which
  // overlays them), keyed by claim id, so they show on the snapshot too.
  type Res = { judgment: string; note: string; by: string };
  const [resolutions, setResolutions] = useState<Record<string, Res>>({});
  const loadResolutions = () => {
    if (!changeId) return;
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}/ledger`, { headers: authHeaders() })
      .then((r) => r.json())
      .then((d) => {
        const m: Record<string, Res> = {};
        (d.ledger?.claims ?? []).forEach((c: { id: string; resolution?: Res }) => { if (c.resolution) m[c.id] = c.resolution; });
        setResolutions(m);
      })
      .catch(() => {});
  };
  useEffect(loadResolutions, [changeId, tenant, repo]);
  const resolveClaim = async (claimId: string, judgment: "verified" | "concern") => {
    if (!canAct) return uiAlert("Sign in to act.");
    const note = (await uiPrompt({ title: judgment === "verified" ? "Mark verified" : "Raise a concern", label: "note (optional)", optional: true, confirmLabel: judgment === "verified" ? "Verify" : "Raise concern" })) ?? "";
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}/claims/${claimId}/resolve`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ judgment, note }),
    });
    if (res.ok) loadResolutions();
    else uiAlert(await res.text());
  };
  // Bulk-verify the intent claims at once — the common case is "I read the diff, these all hold",
  // which shouldn't be 20 separate clicks (and prompts).
  const [verifyingAll, setVerifyingAll] = useState(false);
  const verifyAllClaims = async (ids: string[]) => {
    if (!canAct || ids.length === 0) return;
    if (!(await uiConfirm({ title: `Verify ${ids.length} claim${ids.length > 1 ? "s" : ""} as read?`, body: "Marks these intent claims as checked by you — use it once you've read the diff and they hold up.", confirmLabel: "Verify all" }))) return;
    setVerifyingAll(true);
    try {
      for (const id of ids) {
        await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}/claims/${id}/resolve`, {
          method: "POST", headers: { "content-type": "application/json", ...authHeaders() },
          body: JSON.stringify({ judgment: "verified", note: "" }),
        });
      }
      loadResolutions();
    } finally { setVerifyingAll(false); }
  };

  // "Fix with AI": ask the fixer to propose a patch for a finding; it posts to the PR thread.
  const [fixing, setFixing] = useState<number | null>(null);
  const fixWithAI = async (idx: number, f: Finding) => {
    if (!canAct || !pr) return uiAlert("Sign in to act.");
    setFixing(idx);
    try {
      const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/prs/${pr.number}/fix`, {
        method: "POST",
        headers: { "content-type": "application/json", ...authHeaders() },
        body: JSON.stringify({ path: f.path, note: f.note, severity: f.severity }),
      });
      if (res.ok) { const d = await res.json(); uiAlert("AI fix applied as a new change (re-verified):\n\n" + (d.fix?.explanation ?? "")); loadThread(); loadChange(); }
      else uiAlert(await res.text());
    } finally {
      setFixing(null);
    }
  };
  // Fix-with-AI for a reconciliation claim: hand the claim text to the fixer against the change's files.
  const [fixingClaim, setFixingClaim] = useState<string | null>(null);
  const fixClaim = async (c: Claim) => {
    if (!canAct || !pr) return uiAlert("Sign in to act.");
    setFixingClaim(c.id);
    try {
      const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/prs/${pr.number}/fix`, {
        method: "POST",
        headers: { "content-type": "application/json", ...authHeaders() },
        body: JSON.stringify({ path: change?.files[0]?.path ?? "", note: `Reconcile claim: ${c.text}`, severity: c.status === "contradicted" ? "blocker" : "warn" }),
      });
      if (res.ok) { const d = await res.json(); uiAlert("AI fix applied as a new change (re-verified):\n\n" + (d.fix?.explanation ?? "")); loadThread(); loadChange(); }
      else uiAlert(await res.text());
    } finally { setFixingClaim(null); }
  };

  const [checking, setChecking] = useState(false);
  const [checkResult, setCheckResult] = useState<{ status: string; summary: string; memoized: boolean } | null>(null);
  const runChecks = async (force: boolean) => {
    if (!changeId) return;
    if (!canAct) return uiAlert("Sign in to act.");
    setChecking(true);
    setCheckResult(null);
    try {
      const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}/check`, {
        method: "POST",
        headers: { "content-type": "application/json", ...authHeaders() },
        body: JSON.stringify({ force }),
      });
      setCheckResult(await res.json());
      loadChange(); // verification was written back by the runner
    } catch {
      setCheckResult({ status: "errored", summary: "request failed", memoized: false });
    } finally {
      setChecking(false);
    }
  };

  const verification = change?.verification ?? "unverified";
  // ── review brief metrics: the synthesized "what changed / what to review" ──
  const changedFiles = diff.length || (change?.files.length ?? 0);
  const addN = diff.reduce((s, f) => s + f.hunks.reduce((x, h) => x + h.lines.filter((l) => l.tag === "add").length, 0), 0);
  const delN = diff.reduce((s, f) => s + f.hunks.reduce((x, h) => x + h.lines.filter((l) => l.tag === "del").length, 0), 0);
  const behN = semantic?.behavioral.length ?? 0;
  const briefClaims = shownLedger?.claims ?? [];
  // A human resolution (verify / raise concern) overrides the raw ledger status, so the checks and
  // brief reflect what the reviewer actually decided — not "needs judgment" forever.
  const needsN = briefClaims.filter((c) => c.status === "needs_judgment" && !resolutions[c.id]).length;
  const contraN = briefClaims.filter((c) => c.status === "contradicted" && resolutions[c.id]?.judgment !== "verified").length;
  const concernN = briefClaims.filter((c) => resolutions[c.id]?.judgment === "concern").length;
  const allFindings = reviews.flatMap((r) => r.findings ?? []);
  const blockerN = allFindings.filter((f) => f.severity === "blocker").length;
  // Every finding flattened with its reviewer + a stable key + a global index (for the fix-with-AI
  // busy state), so findings can be rendered inline in the diff at their line.
  type FindingRow = { f: Finding; reviewer: string; key: string; idx: number };
  let _fidx = 0;
  const findingRows: FindingRow[] = reviews.flatMap((r, ri) => (r.findings ?? []).map((f, fi) => ({ f, reviewer: r.reviewer, key: `${ri}:${fi}`, idx: _fidx++ })));
  const findingsByFile = new Map<string, FindingRow[]>();
  for (const x of findingRows) { if (!x.f.path) continue; const arr = findingsByFile.get(x.f.path) ?? []; arr.push(x); findingsByFile.set(x.f.path, arr); }
  const diffPaths = new Set(diff.map((f) => f.path));
  // Findings we can't anchor in the diff (no line, or a file that isn't in this diff) still need a home.
  const unmappedFindings = findingRows.filter((x) => !x.f.line || !diffPaths.has(x.f.path));
  const sevTone = (s: string): "bad" | "warn" | "info" => (s === "blocker" ? "bad" : s === "warn" ? "warn" : "info");
  // Risk that reflects the whole picture, not just the check status: a green build with 4 unresolved
  // claims is not "low risk". Note phantom/unclaimed count is NOT a risk input — it's really just
  // "the diff is bigger than the prose enumerates", which fires on almost every non-trivial change,
  // so it would peg everything to "elevated" and drown the real signal.
  const riskLevel =
    contraN > 0 || blockerN > 0 || verification === "red" ? "high"
      : needsN > 0 || (change ? change.files.length > 10 : false) ? "elevated"
        : verification === "green" ? "low"
          : "moderate";
  // "What's happening": prefer the change's own description (the why) over the title, which the
  // heading already shows.
  const intentFull = (change?.intent ?? pr?.title ?? active.target).trim();
  const intentLine = intentFull.split("\n")[0].trim();

  // ── landing checks: the gate, surfaced as a checklist reachable from the top badge ──
  const supportedN = briefClaims.filter((c) => ["verified_mechanically", "verified_read_only", "self_attested"].includes(c.status)).length;
  const hasApproval = reviews.some((r) => r.verdict === "approve" && (!pr || r.reviewer !== pr.author));
  const changesRequested = reviews.some((r) => r.verdict === "request_changes" || r.verdict === "reject");
  type Check = { label: string; tone: "ok" | "bad" | "warn" | "wait"; detail: string };
  const checks: Check[] = [
    { label: "keel verify", tone: verification === "green" ? "ok" : verification === "red" ? "bad" : "wait", detail: verification === "green" ? "build & tests pass" : verification === "red" ? "build or tests failing" : "not run yet" },
    ...(briefClaims.length > 0 ? [{ label: "Claims reconciled", tone: (contraN > 0 || concernN > 0 ? "bad" : needsN > 0 ? "warn" : "ok") as Check["tone"], detail: concernN > 0 ? `${concernN} concern${concernN > 1 ? "s" : ""} raised` : contraN > 0 ? `${contraN} contradicted` : needsN > 0 ? `${needsN} need a human's judgment` : `${supportedN}/${briefClaims.length} reconciled` }] : []),
    { label: "No blocking findings", tone: blockerN > 0 ? "bad" : "ok", detail: blockerN > 0 ? `${blockerN} blocker${blockerN > 1 ? "s" : ""}` : "none raised" },
    { label: "Independent approval", tone: changesRequested ? "bad" : hasApproval ? "ok" : "wait", detail: changesRequested ? "changes requested" : hasApproval ? "approved by a non-author" : "awaiting review" },
  ];
  const checksPass = checks.filter((c) => c.tone === "ok").length;
  const checksBad = checks.some((c) => c.tone === "bad");
  // The page opens as a calm digest — the detail (claims, diff) is folded beneath it. Contradictions
  // are the exception: a claim the change's own facts contradict always deserves eyes, so auto-open.
  const [showChanges, setShowChanges] = useState(false);
  const [showClaims, setShowClaims] = useState(false);
  const [attnIdx, setAttnIdx] = useState(0); // cursor into the one-by-one attention stepper
  const [taskModal, setTaskModal] = useState(false); // "view full task" overlay
  // When the big title scrolls out of view, condense it into the sticky top bar so the reviewer
  // always knows which PR + verdict they're looking at.
  const titleSentinel = useRef<HTMLDivElement>(null);
  const [condensed, setCondensed] = useState(false);
  useEffect(() => {
    const el = titleSentinel.current;
    if (!el) return;
    const io = new IntersectionObserver(([e]) => setCondensed(!e.isIntersecting), { rootMargin: "-56px 0px 0px 0px" });
    io.observe(el);
    return () => io.disconnect();
  }, []);

  // Discussion thread — the same PR thread as the compact view, followed into the deep review page.
  type Cmt = { id: string; target: string; author: string; body: string; created_unix: number; path?: string; line?: number };
  const [thread, setThread] = useState<Cmt[]>([]);
  const [draft, setDraft] = useState("");
  const loadThread = () =>
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/comments`, { headers: authHeaders() })
      .then((r) => r.json())
      .then((d) => setThread((d.comments ?? []).filter((c: Cmt) => pr && c.target === `pr:${pr.number}`)))
      .catch(() => {});
  useEffect(() => { loadThread(); }, [tenant, repo, pr?.number]);
  const postThreadComment = async () => {
    if (!canAct || !pr || !draft.trim()) return;
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/comments`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ target: `pr:${pr.number}`, body: draft.trim() }),
    });
    if (res.ok) { setDraft(""); loadThread(); }
    else uiAlert(await res.text());
  };
  // The composer's split button posts a verdict-carrying review (approve / request changes / reject),
  // with the draft as its summary. "Comment" alone routes to postThreadComment instead.
  const [composerBusy, setComposerBusy] = useState(false);
  const postReview = async (verdict: "approve" | "request_changes" | "reject") => {
    if (!canAct || !pr) return uiAlert("Sign in to act.");
    setComposerBusy(true);
    try {
      const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/reviews`, {
        method: "POST",
        headers: { "content-type": "application/json", ...authHeaders() },
        body: JSON.stringify({ target: `pr:${pr.number}`, reviewer: me?.id ?? "", verdict, summary: draft.trim(), findings: [] }),
      });
      if (res.ok) { setDraft(""); onReviewsChanged?.(); loadThread(); }
      else uiAlert(await res.text());
    } finally { setComposerBusy(false); }
  };
  const closeOrReopenPr = async (reopen: boolean) => {
    if (!canAct || !pr) return uiAlert("Sign in to act.");
    if (!reopen && !(await uiConfirm({ title: "Close this pull request?", body: "It won't be merged. You can reopen it later from the pull requests list.", danger: true, confirmLabel: "Close pull request" }))) return;
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/prs/${pr.number}/close`, {
      method: "POST", headers: { "content-type": "application/json", ...authHeaders() }, body: JSON.stringify({ reopen }),
    });
    if (res.ok) { onReviewsChanged?.(); if (!reopen) onBack(); } else uiAlert(await res.text());
  };
  type ComposerMode = "comment" | "approve" | "request_changes" | "reject";
  const [composerMode, setComposerMode] = useState<ComposerMode>("comment");
  const composerHasDraft = draft.trim().length > 0;
  // Labels reflect whether a comment is present: "Approve with comment" vs "Approve".
  const MODES: { id: ComposerMode; label: string; hint: string; color: string; icon: React.ReactNode }[] = [
    { id: "comment", label: "Comment", hint: "Leave a note, no verdict", color: "text-dim", icon: <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /></svg> },
    { id: "approve", label: composerHasDraft ? "Approve with comment" : "Approve", hint: "Good to merge", color: "text-clear-text", icon: <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M22 11.08V12a10 10 0 1 1-5.93-9.14" /><polyline points="22 4 12 14.01 9 11.01" /></svg> },
    { id: "request_changes", label: composerHasDraft ? "Request changes with comment" : "Request changes", hint: "Needs work before merging", color: "text-brass-text", icon: <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M3 7v6h6" /><path d="M21 17a9 9 0 0 0-15-6.7L3 13" /></svg> },
    { id: "reject", label: composerHasDraft ? "Reject with comment" : "Reject", hint: "Do not merge", color: "text-fault-text", icon: <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><circle cx="12" cy="12" r="10" /><line x1="4.93" y1="4.93" x2="19.07" y2="19.07" /></svg> },
  ];
  const runMode = (m: ComposerMode) => { if (m === "comment") postThreadComment(); else postReview(m); };
  const composerDisabled = composerBusy || (composerMode === "comment" && !draft.trim());
  // Inline findings: which are collapsed (hidden inline, reopenable from a gutter marker).
  // Findings start collapsed (shown as a gutter marker) so they don't crowd the diff; click to open.
  // Reviews load async, so collapse each finding once when first seen (without undoing manual expands).
  // Raw diff bodies are minified (just the "What changed here" summary) until expanded — click a
  // summary line or the Show-diff toggle to reveal the lines.
  // Diffs open on demand — "Show diff" expands one; "Hide diff" collapses it again.
  const [expandedDiffs, setExpandedDiffs] = useState<Set<string>>(() => new Set());
  // A big diff only ever RENDERS a bounded window (never thousands of rows). diffFocus[path] centres
  // the window on a line (set by a "What changed here" click); diffExpand[path] grows it on "show more".
  const [diffFocus, setDiffFocus] = useState<Record<string, number>>({});
  const [diffExpand, setDiffExpand] = useState<Record<string, number>>({});
  // Open a file's diff and jump to a line — the line stays highlighted (diffFocus drives CodePanel's
  // persistent `highlightLine`) so you can see exactly which line the "What changed here" click meant.
  const revealDiff = (path: string, line?: number) => {
    setExpandedDiffs((s) => (s.has(path) ? s : new Set(s).add(path)));
    if (line != null) {
      setDiffFocus((f) => ({ ...f, [path]: line }));
      setTimeout(() => document.getElementById(`L-${path}-${line}`)?.scrollIntoView({ block: "center", behavior: "smooth" }), 180);
    }
  };
  const [collapsedFindings, setCollapsedFindings] = useState<Set<string>>(() => new Set());
  const seenFindingsRef = useRef<Set<string>>(new Set());
  useEffect(() => {
    setCollapsedFindings((prev) => {
      const n = new Set(prev);
      reviews.forEach((r, ri) => (r.findings ?? []).forEach((_, fi) => {
        const k = `${ri}:${fi}`;
        if (!seenFindingsRef.current.has(k)) { seenFindingsRef.current.add(k); n.add(k); }
      }));
      return n;
    });
  }, [reviews]);
  const toggleFinding = (key: string) => setCollapsedFindings((s) => { const n = new Set(s); n.has(key) ? n.delete(key) : n.add(key); return n; });
  // Line-level review comments: select a diff line, press C (or click the gutter ✎) to comment on it.
  const [selLine, setSelLine] = useState<{ path: string; line: number } | null>(null);
  const [commenting, setCommenting] = useState<{ path: string; line: number } | null>(null);
  const [lineDraft, setLineDraft] = useState("");
  const postLineComment = async () => {
    if (!canAct || !pr || !commenting || !lineDraft.trim()) return;
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/comments`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ target: `pr:${pr.number}`, body: lineDraft.trim(), path: commenting.path, line: commenting.line }),
    });
    if (res.ok) { setLineDraft(""); setCommenting(null); loadThread(); }
    else uiAlert(await res.text());
  };
  const openLineComment = (path: string, line: number) => { setSelLine({ path, line }); setCommenting({ path, line }); setLineDraft(""); };
  // Press "c" to comment on the currently-selected line.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement;
      if ((e.key === "c" || e.key === "C") && selLine && !commenting && !/^(INPUT|TEXTAREA)$/.test(t.tagName)) { e.preventDefault(); setCommenting(selLine); setLineDraft(""); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [selLine, commenting]);
  // Line-anchored comments render in the diff; general comments render in the conversation.
  const lineCommentsByFile = new Map<string, Cmt[]>();
  for (const c of thread) { if (c.path && c.line) { const arr = lineCommentsByFile.get(c.path) ?? []; arr.push(c); lineCommentsByFile.set(c.path, arr); } }
  const kindOf = (id: string) => actors.find((a) => a.id === id)?.kind;


  return (
    <div className="bg-paper min-h-screen text-ink">
      <header className="h-[52px] border-b border-rule2 bg-surface flex items-center gap-3 px-6 sticky top-14 z-20">
        <button className="flex items-center gap-1.5 text-[13px] font-medium text-dim hover:text-ink cursor-pointer flex-none" onClick={onBack}>
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><line x1="19" y1="12" x2="5" y2="12" /><polyline points="12 19 5 12 12 5" /></svg>
          <span className="hidden sm:inline">{repo} · pull requests</span>
        </button>
        {condensed ? (
          <div className="flex items-center gap-2 min-w-0 flex-1">
            <span className="text-[13.5px] font-semibold text-ink truncate">{pr ? pr.title : active.target}</span>
            {pr && <span className="text-[12px] text-faint tabular-nums flex-none">#{pr.number}</span>}
            <span className={`ml-auto flex-none inline-flex items-center gap-1.5 text-[11.5px] font-semibold px-2 py-[3px] rounded-badge ${checksBad ? "bg-fault-wash text-fault-text" : checksPass === checks.length ? "bg-clear-wash text-clear-text" : "bg-brass-wash text-brass-text"}`}>
              <span className={`w-1.5 h-1.5 rounded-full ${checksBad ? "bg-fault" : checksPass === checks.length ? "bg-clear" : "bg-brass"}`} />
              {checksBad ? "Not ready" : checksPass === checks.length ? "Ready to merge" : "Awaiting review"} · {checksPass}/{checks.length}
            </span>
          </div>
        ) : (
          <span className="text-[11.5px] font-semibold px-[9px] py-[3px] rounded-full border border-rule text-dim">review package</span>
        )}
      </header>

      {/* Full-task overlay — the session summary shows a clipped task; this is the whole thing. */}
      {taskModal && change?.session && createPortal(
        <div className="fixed inset-0 z-[60] flex items-center justify-center p-4" onClick={() => setTaskModal(false)}>
          <div className="absolute inset-0 bg-ink/40" />
          <div className="relative bg-surface border border-rule rounded-card shadow-modal max-w-[680px] w-full max-h-[80vh] flex flex-col" onClick={(e) => e.stopPropagation()}>
            <div className="flex items-center justify-between gap-3 px-5 py-3 border-b border-rule2">
              <span className="text-[13.5px] font-semibold text-ink">Agent task</span>
              <button onClick={() => setTaskModal(false)} className="text-dim hover:text-ink"><IcoX size={16} /></button>
            </div>
            <div className="px-5 py-4 overflow-y-auto text-[13.5px] text-body leading-[1.6] whitespace-pre-wrap">{change.session.task}</div>
          </div>
        </div>, document.body)}

      <div className="max-w-[1320px] mx-auto px-6 sm:px-8 py-8">
        <h1 className="text-[23px] font-semibold tracking-tight">{pr ? `${pr.title}` : active.target}</h1>
        <div ref={titleSentinel} aria-hidden />
        <div className="flex items-center gap-2 flex-wrap text-[12.5px] text-muted mt-2 mb-5 tabular-nums">
          {pr && <span className="font-medium text-body">PR #{pr.number}</span>}
          <span className="text-faint">·</span>
          {pr && <><Avatar id={pr.author} handle={handleOf(pr.author)} kind={kindOf(pr.author)} size={16} /><span className={kindOf(pr.author) === "agent" ? "text-steel-text" : ""}>{handleOf(pr.author)}</span></>}
          <span className="text-faint">·</span>
          <span className="text-steel-text inline-flex items-center gap-1" title={changeId}><IcoGit size={12} />{(changeId ?? "").slice(0, 8)}</span>
          <span className="text-faint">·</span>
          <span>{changedFiles} file{changedFiles === 1 ? "" : "s"}</span>
          <span className="text-clear-text font-semibold">+{addN}</span>
          <span className="text-fault-text font-semibold">−{delN}</span>
          <span className="text-faint">·</span>
          {/* Checks — clickable, opens the full checklist so the gate is reachable from the top */}
          <Popover align="left" width={296} trigger={(open) => (
            <span className={`inline-flex items-center gap-1.5 text-[11.5px] font-semibold px-2 py-[3px] rounded-badge transition-colors ${checksBad ? "bg-fault-wash text-fault-text" : checksPass === checks.length ? "bg-clear-wash text-clear-text" : "bg-brass-wash text-brass-text"} ${open ? "ring-2 ring-steel/25" : ""}`}>
              <span className={`w-1.5 h-1.5 rounded-full ${checksBad ? "bg-fault" : checksPass === checks.length ? "bg-clear" : "bg-brass"}`} />
              {checksPass}/{checks.length} checks
              <svg width="9" height="9" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round" className={open ? "rotate-180 transition-transform" : "transition-transform"}><polyline points="6 9 12 15 18 9" /></svg>
            </span>
          )}>
            <div>
              {checks.map((c, i) => (
                <div key={i} className="flex items-start gap-2.5 px-4 py-2.5 border-b border-rule2 last:border-0">
                  <StatusDot tone={c.tone} />
                  <div className="min-w-0 flex-1">
                    <div className="text-[13px] font-medium text-body leading-tight">{c.label}</div>
                    <div className="text-[12px] text-muted mt-0.5">{c.detail}</div>
                  </div>
                </div>
              ))}
              {canAct && (
                <div className="px-4 py-2.5 flex items-center gap-2 bg-paper">
                  <Popover align="left" width={260} trigger={(open) => (
                    <span className={`inline-flex items-center gap-1.5 h-ctl-sm px-2.5 rounded-ctl-sm border bg-surface text-[12.5px] font-medium cursor-pointer transition-colors ${checking ? "opacity-60" : ""} ${open ? "border-body text-ink" : "border-ctl text-dim hover:text-ink hover:border-dim"}`}>
                      {checking ? "Running…" : "Rerun failed pipelines"}
                      <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" className={`text-muted transition-transform ${open ? "rotate-180" : ""}`}><polyline points="6 9 12 15 18 9" /></svg>
                    </span>
                  )}>
                    <div className="py-1">
                      {[
                        { label: "Rerun failed pipelines", hint: "reuses cached results that are still valid", run: () => runChecks(false) },
                        { label: "Force rerun (ignore cache)", hint: "re-execute every check from scratch", run: () => runChecks(true) },
                      ].map((o) => (
                        <button key={o.label} type="button" disabled={checking} onClick={o.run} className="w-full text-left px-3 py-2 hover:bg-paper disabled:opacity-50">
                          <span className="block text-[13px] font-medium text-body leading-tight">{o.label}</span>
                          <span className="block text-[11.5px] text-muted leading-tight mt-0.5">{o.hint}</span>
                        </button>
                      ))}
                    </div>
                  </Popover>
                  {checkResult && <span className="text-[12px] text-muted">{checkResult.status}{checkResult.memoized ? " · memoized" : ""}</span>}
                </div>
              )}
            </div>
          </Popover>
        </div>

        {/* Digest — the page's calm headline: a plain-English summary of the change (an agent
            reviewer's own words when one has reviewed, else the change's subject line), a one-line
            verdict, and what — if anything — needs a human. The claims + diff fold beneath it, so the
            reviewer meets a verdict, not a wall. */}
        {(() => {
          const loz = "inline-flex items-center text-[11px] font-bold uppercase tracking-[0.03em] leading-none px-1.5 py-[3px] rounded-badge";
          // Prefer an agent reviewer's prose (the AI layer's own summary) over raw commit text; skip the
          // templated mechanical reconciliation line — we want a real summary, not "N claims supported".
          const aiSummary = [...reviews]
            .filter((r) => actors.find((a) => a.id === r.reviewer)?.kind === "agent" && (r.summary ?? "").trim() && !/^reconciliation review:/i.test((r.summary ?? "").trim()))
            .sort((a, b) => (b.created_unix ?? 0) - (a.created_unix ?? 0))[0]?.summary?.trim();
          // Drop a leading conventional-commit prefix (feat:, fix(ui):, …) so the headline reads as prose.
          const digestText = aiSummary || intentLine.replace(/^\w+(\([^)]+\))?!?:\s*/, "");
          const addN = diff.reduce((s, f) => s + f.hunks.reduce((x, h) => x + h.lines.filter((l) => l.tag === "add").length, 0), 0);
          const delN = diff.reduce((s, f) => s + f.hunks.reduce((x, h) => x + h.lines.filter((l) => l.tag === "del").length, 0), 0);
          const fileN = change?.files.length ?? diff.length;
          const verdict = checksBad ? "Not ready to merge" : checks.length > 0 && checksPass === checks.length ? "Ready to merge" : "Awaiting review";
          const vTone = checksBad ? "text-fault-text" : verdict === "Ready to merge" ? "text-clear-text" : "text-brass-text";
          const blocking = checks.filter((c) => c.tone === "bad");
          // Where does clicking a blocking reason take you? Claims/findings → the attention card;
          // approval → the conversation; a red build isn't jumpable (rerun lives in the top bar).
          const jumpTarget = (label: string) => /approval/i.test(label) ? "pr-conversation" : /claim|finding/i.test(label) ? "needs-attention" : null;
          const jumpTo = (id: string) => document.getElementById(id)?.scrollIntoView({ behavior: "smooth", block: "start" });
          // The non-blocking claims line (only shown when nothing is actually blocking the merge).
          const check = needsN > 0 ? { c: "text-brass-text", t: `${needsN} claim${needsN > 1 ? "s" : ""} worth a spot-check.` }
            : briefClaims.length > 0 ? { c: "text-clear-text", t: "Nothing needs your judgment." }
            : { c: "text-muted", t: "" };
          const claimsLine = contraN > 0 ? `${briefClaims.length} claims · ${contraN} contradicted`
            : needsN > 0 ? `${briefClaims.length} claims · ${needsN} to judge`
            : `${briefClaims.length} claims reconciled — all verified`;
          const chev = (open: boolean) => <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" className={`text-faint flex-none transition-transform ${open ? "rotate-90" : ""}`}><polyline points="9 18 15 12 9 6" /></svg>;
          const Toggle = ({ open, onClick, children }: { open: boolean; onClick: () => void; children: React.ReactNode }) => (
            <button onClick={onClick} className="w-full flex items-center gap-2 px-5 py-2.5 text-[13px] text-body hover:bg-paper/40 transition-colors border-t border-rule2 text-left">{chev(open)}{children}</button>
          );
          return (
            <Card className="mb-6">
              <div className="px-5 py-4">
                <div className="flex items-start gap-4">
                  <div className="min-w-0 flex-1">
                    <div className={`text-[15px] font-semibold ${vTone} mb-1`}>{verdict}</div>
                    <p className="text-[14px] text-body leading-[1.55]">{digestText.length > 260 ? digestText.slice(0, 260).trimEnd() + "…" : digestText}</p>
                    {checksBad ? (
                      <div className="mt-2.5 grid gap-1">
                        <div className="text-[11px] font-bold uppercase tracking-[0.06em] text-fault-text">Blocking the merge</div>
                        {blocking.map((c, i) => {
                          const target = jumpTarget(c.label);
                          return (
                            <button key={i} disabled={!target} onClick={() => target && jumpTo(target)} className={`text-[13px] flex items-start gap-1.5 text-left text-fault-text ${target ? "hover:underline cursor-pointer" : "cursor-default"}`}>
                              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" className="mt-[3px] flex-none"><line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" /></svg>
                              <span><b>{c.label}</b> — {c.detail}{target ? " ↓" : ""}</span>
                            </button>
                          );
                        })}
                      </div>
                    ) : check.t ? (
                      <div className={`text-[13px] mt-2 flex items-center gap-1.5 ${check.c}`}>{needsN > 0 ? null : <IcoCheck size={13} />}{check.t}</div>
                    ) : null}
                    {/* Neutral metadata chips — no decorative color at the top of the page. */}
                    <div className="flex flex-wrap gap-1.5 mt-3 text-dim">
                      {behN > 0 && <span className={`${loz} bg-rule2 text-dim`}>{behN} behavioral change{behN > 1 ? "s" : ""}</span>}
                      {semantic?.pure_move && <span className={`${loz} bg-rule2 text-dim`}>pure move</span>}
                      {!change?.session && <span className={`${loz} bg-rule2 text-dim`}>no plan captured</span>}
                      {aiSummary && <span className={`${loz} bg-rule2 text-dim inline-flex items-center gap-1`}><IcoSparkle size={11} />agent-summarized</span>}
                    </div>
                  </div>
                  {/* Risk as a quiet dot + label, not a saturated pill. */}
                  <span className="flex-none inline-flex items-center gap-1.5 text-[11.5px] font-medium text-muted">
                    <span className={`w-1.5 h-1.5 rounded-full ${riskLevel === "low" ? "bg-clear" : riskLevel === "high" ? "bg-fault" : "bg-brass"}`} />{riskLevel} risk
                  </span>
                </div>
              </div>
              {briefClaims.length > 0 && <Toggle open={showClaims} onClick={() => { const o = !showClaims; setShowClaims(o); if (o) setTimeout(() => document.getElementById("reconciliation-section")?.scrollIntoView({ behavior: "smooth", block: "start" }), 80); }}>{claimsLine}</Toggle>}
              {(diff.length > 0 || (semantic?.moves.length ?? 0) > 0) && <Toggle open={showChanges} onClick={() => { const o = !showChanges; setShowChanges(o); if (o) setTimeout(() => document.getElementById("changes-section")?.scrollIntoView({ behavior: "smooth", block: "start" }), 80); }}>Changes · {fileN} file{fileN === 1 ? "" : "s"} · <span className="text-clear-text tabular-nums">+{addN}</span> <span className="text-fault-text tabular-nums">−{delN}</span></Toggle>}
            </Card>
          );
        })()}

        {/* Merge decision — at the TOP now (and it re-appears in the sticky bar on scroll), so the
            reviewer never has to hunt to the bottom of the page to land. */}
        <div className="mb-6">{landGate}</div>

        {/* Session — surfaced up here (was buried at the bottom): a summary of what the agent set out
            to do, with the full task one click away, its carried-forward lesson, and the run metrics. */}
        {change?.session && (() => {
          const task = change.session.task || "";
          const long = task.length > 240;
          return (
            <Card className="mb-6">
              <SectionHeader label="Session" right={pr && <span className="inline-flex items-center gap-1.5 text-[12.5px] text-muted"><Avatar id={pr.author} handle={handleOf(pr.author)} kind={kindOf(pr.author)} size={16} />{handleOf(pr.author)}</span>} />
              <div className="px-5 py-4 grid gap-3.5">
                <div>
                  <div className="text-[11px] font-bold uppercase tracking-[0.06em] text-muted mb-1">What the agent set out to do</div>
                  <p className="text-[13.5px] text-body leading-[1.55]">{long ? task.slice(0, 240).trimEnd() + "…" : task}</p>
                  {long && <button onClick={() => setTaskModal(true)} className="text-[12.5px] text-steel-text hover:underline mt-1.5 inline-flex items-center gap-1"><IcoExpand size={12} />View full task</button>}
                </div>
                {change.session.lesson && (
                  <div className="rounded-ctl bg-paper/60 border border-rule2 px-3.5 py-2.5">
                    <div className="text-[11px] font-bold uppercase tracking-[0.06em] text-dim mb-1 flex items-center gap-1.5"><IcoBulb size={13} />Lesson carried forward</div>
                    <p className="text-[13px] text-body leading-[1.5]">{change.session.lesson}</p>
                  </div>
                )}
                <div className="flex flex-wrap gap-2">
                  {[
                    { k: "Model", v: change.session.model || "—" },
                    ...(change.session.tool_calls > 0 ? [{ k: "Tool calls", v: String(change.session.tool_calls) }] : []),
                    ...(change.session.tokens_out > 0 ? [{ k: "Tokens (in → out)", v: `${change.session.tokens_in.toLocaleString()} → ${change.session.tokens_out.toLocaleString()}` }] : []),
                  ].map((m) => (
                    <div key={m.k} className="rounded-ctl border border-rule2 bg-paper/40 px-3 py-2 min-w-[110px]">
                      <div className="text-[10.5px] text-muted uppercase tracking-[0.04em]">{m.k}</div>
                      <div className="text-[13.5px] font-semibold text-ink tabular-nums mt-0.5 break-all">{m.v}</div>
                    </div>
                  ))}
                </div>
              </div>
            </Card>
          );
        })()}

        {/* Needs attention — a one-at-a-time stepper so review FLOWS: each item that wants a human
            (a contradicted claim, a raised concern, a blocker/warning finding, or an unverifiable
            intent claim) gets the full frame with big, obvious controls. Acting on it advances to the
            next; a bulk "verify all intent claims" clears the long tail without 20 clicks. */}
        {(() => {
          const contradictions = (shownLedger?.claims ?? []).filter((c) => c.status === "contradicted" && resolutions[c.id]?.judgment !== "verified");
          const concerns = (shownLedger?.claims ?? []).filter((c) => resolutions[c.id]?.judgment === "concern");
          const needs = (shownLedger?.claims ?? []).filter((c) => (c.status === "needs_judgment" || c.status === "self_attested") && !resolutions[c.id]);
          const findAtt = findingRows.filter((x) => x.f.severity !== "info");
          type Item = { kind: "contra" | "concern" | "finding" | "needs"; key: string; claim?: Claim; row?: FindingRow };
          const items: Item[] = [
            ...contradictions.map((c): Item => ({ kind: "contra", key: c.id, claim: c })),
            ...concerns.map((c): Item => ({ kind: "concern", key: c.id, claim: c })),
            ...findAtt.map((x): Item => ({ kind: "finding", key: x.key, row: x })),
            ...needs.map((c): Item => ({ kind: "needs", key: c.id, claim: c })),
          ];
          if (items.length === 0) return null;
          const idx = Math.min(attnIdx, items.length - 1);
          const cur = items[idx];
          const kindLoz = "text-[10px] font-semibold uppercase tracking-[0.05em] px-1.5 py-[1px] rounded flex-none";
          const tone = cur.kind === "needs" ? "wait" : cur.kind === "finding" ? (cur.row!.f.severity === "blocker" ? "bad" : "warn") : "bad";
          const label = cur.kind === "contra" ? "Contradicted claim" : cur.kind === "concern" ? "Concern raised" : cur.kind === "finding" ? (cur.row!.f.severity === "blocker" ? "Blocker" : "Warning") : "Intent — needs your eyes";
          const labelColor = tone === "bad" ? "text-fault-text" : tone === "warn" ? "text-brass-text" : "text-brass-text";
          const needsLeft = needs.length;
          const lbl = (icon: React.ReactNode, text: string) => <span className="inline-flex items-center gap-1.5">{icon}{text}</span>;
          return (
            <div id="needs-attention" className="scroll-mt-4">
            <Card className="mb-6">
              <div className="flex items-center justify-between gap-3 px-5 py-3 border-b border-rule">
                <div className="flex items-center gap-2.5">
                  <span className="text-fault-text"><IcoFlag size={15} /></span>
                  <span className="text-[13.5px] font-semibold text-ink">Needs your attention</span>
                  <span className="text-[12px] text-muted tabular-nums">{idx + 1} / {items.length}</span>
                </div>
                {canReview && onTriage && <Button size="sm" variant="secondary" disabled={triaging || !canAct} onClick={onTriage}>{triaging ? "agent triaging…" : lbl(<IcoSearch size={13} />, "Let an agent triage")}</Button>}
              </div>
              <div className="h-[2px] bg-rule2"><div className="h-full bg-dim/50 transition-all" style={{ width: `${((idx + 1) / items.length) * 100}%` }} /></div>

              <div className="px-5 py-4 min-h-[128px]">
                <div className={`text-[11px] font-bold uppercase tracking-[0.05em] mb-2 ${labelColor}`}>{label}</div>
                {cur.kind === "finding" ? (
                  <>
                    <div className="text-[14px] text-body leading-snug">{cur.row!.f.note}</div>
                    {cur.row!.f.path && <div className="text-[12.5px] text-muted mt-1 tabular-nums">{cur.row!.f.path}{cur.row!.f.line ? `:${cur.row!.f.line}` : ""}</div>}
                    <div className="flex items-center gap-2 mt-3.5 flex-wrap">
                      {canFix && pr && cur.row!.f.path ? <Button size="sm" disabled={!canAct || fixing === cur.row!.idx} onClick={() => fixWithAI(cur.row!.idx, cur.row!.f)}>{fixing === cur.row!.idx ? "fixing…" : lbl(<IcoSparkle size={13} />, "Fix with AI")}</Button>
                        : !canFix && <span className="text-[12px] text-muted">Set <code className="text-body">OPENROUTER_API_KEY</code> to auto-fix.</span>}
                    </div>
                  </>
                ) : (
                  <>
                    <div className="text-[14px] text-body leading-snug">{cur.claim!.text}</div>
                    <div className={`text-[12.5px] mt-1 ${cur.kind === "needs" ? "text-muted" : "text-fault-text"}`}>
                      {cur.kind === "contra" ? "The change's own facts contradict this claim." : cur.kind === "concern" ? <>Concern raised by <b>{resolutions[cur.claim!.id]?.by}</b>{resolutions[cur.claim!.id]?.note ? ` — ${resolutions[cur.claim!.id]?.note}` : ""}.</> : "Can't be checked mechanically — read the diff and confirm."}
                    </div>
                    {(cur.claim!.evidence ?? []).slice(0, 2).map((e, i) => (
                      <div key={i} className={`text-[12px] mt-1 flex gap-1.5 items-baseline ${e.supports ? "text-dim" : "text-fault-text"}`}>
                        <span className={`${kindLoz} ${e.supports ? "bg-paper text-muted" : "bg-fault-wash text-fault-text"}`}>{e.kind}</span><span className="leading-snug">{e.detail}</span>
                      </div>
                    ))}
                    <div className="flex items-center gap-2 mt-3.5 flex-wrap">
                      <Button size="sm" disabled={!canAct} onClick={() => resolveClaim(cur.claim!.id, "verified")}>{lbl(<IcoCheck size={13} />, cur.kind === "needs" ? "Verified" : "Looks fine")}</Button>
                      {cur.kind !== "concern" && <Button size="sm" variant="destructive" disabled={!canAct} onClick={() => resolveClaim(cur.claim!.id, "concern")}>{lbl(<IcoFlag size={13} />, "Raise concern")}</Button>}
                      {canFix && change && pr && <Button size="sm" variant="secondary" disabled={!canAct || fixingClaim === cur.claim!.id} onClick={() => fixClaim(cur.claim!)}>{fixingClaim === cur.claim!.id ? "fixing…" : lbl(<IcoSparkle size={13} />, "Fix with AI")}</Button>}
                    </div>
                  </>
                )}
              </div>

              <div className="flex items-center justify-between gap-3 px-5 py-2.5 border-t border-rule bg-paper/40">
                <button disabled={idx === 0} onClick={() => setAttnIdx(Math.max(0, idx - 1))} className="text-[13px] text-dim hover:text-ink disabled:opacity-40 inline-flex items-center gap-1"><Ico size={14} path={<polyline points="15 18 9 12 15 6" />} />Prev</button>
                {needsLeft > 1 && <Button size="sm" variant="secondary" disabled={!canAct || verifyingAll} onClick={() => verifyAllClaims(needs.map((c) => c.id))}>{verifyingAll ? "verifying…" : lbl(<IcoCheck size={13} />, `Verify all ${needsLeft} intent claims`)}</Button>}
                <button disabled={idx >= items.length - 1} onClick={() => setAttnIdx(Math.min(items.length - 1, idx + 1))} className="text-[13px] text-dim hover:text-ink disabled:opacity-40 inline-flex items-center gap-1">Skip<Ico size={14} path={<polyline points="9 18 15 12 9 6" />} /></button>
              </div>
            </Card>
            </div>
          );
        })()}

        <div className="grid gap-6 mt-3">
          <div className="min-w-0 grid gap-6">
        {/* changes — the flagship semantic-diff surface, revealed (and scrolled to) from the digest. */}
        <div id="changes-section" className="scroll-mt-20" />
        {showChanges && (() => {
          if (diff.length === 0 && !(semantic?.moves.length)) {
            return (
              <Card>
                <SectionHeader label="Changes" />
                <div className="px-5 py-6 text-[13px] text-muted">no textual changes (or binary)</div>
              </Card>
            );
          }
          const cls = (p: string) => (semantic?.behavioral.includes(p) ? "behavioral" : semantic?.whitespace_only.includes(p) ? "reformatted" : "changed");
          const rank: Record<string, number> = { behavioral: 0, changed: 1, reformatted: 2 };
          const ordered = [...diff].sort((a, b) => (rank[cls(a.path)] ?? 1) - (rank[cls(b.path)] ?? 1));
          const count = (f: FileDiff, tag: string) => f.hunks.reduce((s, h) => s + h.lines.filter((l) => l.tag === tag).length, 0);
          const base = (p: string) => p.split("/").pop() ?? p;
          const opKind = (f: FileDiff): string => (f.ops.some((o) => /fn |struct |enum |signature|impl /.test(o)) ? "signature" : "behavior");

          // Collapse long runs of unchanged context so the eye lands on what actually changed.
          // Keeps 3 lines of context on each side of a change; folds the middle into one marker row.
          const CTX = 3;
          function fold<T extends { sign?: string }>(rows: T[], marker: (hidden: number) => T, keep?: (r: T) => boolean): T[] {
            const isCtx = (r: T) => r.sign === undefined && !(keep?.(r));
            const out: T[] = [];
            for (let i = 0; i < rows.length;) {
              if (isCtx(rows[i])) {
                let j = i; while (j < rows.length && isCtx(rows[j])) j++;
                const head = i === 0 ? 0 : CTX, tail = j === rows.length ? 0 : CTX;
                if (j - i > head + tail + 1) {
                  for (let k = i; k < i + head; k++) out.push(rows[k]);
                  out.push(marker(j - i - head - tail));
                  for (let k = j - tail; k < j; k++) out.push(rows[k]);
                } else for (let k = i; k < j; k++) out.push(rows[k]);
                i = j;
              } else { out.push(rows[i]); i++; }
            }
            return out;
          }
          const foldNote = (hidden: number) => <span className="text-faint italic text-[12px]">⋯ {hidden} unchanged {hidden === 1 ? "line" : "lines"}</span>;

          // The inline finding annotation shown right under its line in the diff (collapsible).
          const findingNote = (x: FindingRow) => {
            const { f, reviewer, key, idx } = x;
            const sevColor = f.severity === "blocker" ? "text-fault-text" : f.severity === "warn" ? "text-brass-text" : "text-steel-text";
            return (
              <div className="flex gap-2.5 px-4 py-3 bg-brass-wash/25">
                <StatusDot tone={sevTone(f.severity)} size={16} />
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2 flex-wrap">
                    <span className={`text-[11px] font-bold uppercase tracking-[0.03em] ${sevColor}`}>{f.severity}</span>
                    <span className="inline-flex items-center gap-1 text-[11.5px] text-muted"><Avatar id={reviewer} handle={handleOf(reviewer)} kind={kindOf(reviewer)} size={14} />{handleOf(reviewer)}</span>
                    <button onClick={() => toggleFinding(key)} className="ml-auto text-[11.5px] text-muted hover:text-ink inline-flex items-center gap-1" title="collapse this finding out of the diff">
                      collapse <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round"><polyline points="18 15 12 9 6 15" /></svg>
                    </button>
                  </div>
                  <p className="text-[13px] text-body mt-1 leading-snug">{f.note}</p>
                  {f.severity !== "info" && pr && f.path && canFix && (
                    <div className="mt-2"><Button size="sm" variant="secondary" disabled={!canAct || fixing === idx} onClick={() => fixWithAI(idx, f)}>{fixing === idx ? "fixing…" : "✨ Fix with AI"}</Button></div>
                  )}
                </div>
              </div>
            );
          };
          // A line-level review comment, shown under the line it references.
          const lineCommentNote = (c: Cmt) => (
            <div className="flex gap-2.5 px-4 py-3 bg-surface">
              <Avatar id={c.author} handle={handleOf(c.author)} kind={kindOf(c.author)} size={22} />
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-2 text-[12.5px]">
                  <b className={kindOf(c.author) === "agent" ? "text-steel-text" : ""}>{handleOf(c.author)}</b>
                  <span className="text-faint tabular-nums" title={new Date(c.created_unix * 1000).toLocaleString()}>{timeAgo(c.created_unix)}</span>
                  <span className="text-[11px] text-faint">on line {c.line}</span>
                </div>
                <Markdown text={c.body} linkBase={`/${encodeURIComponent(tenant)}/${repo}`} className="text-[13.5px] text-body mt-0.5" />
              </div>
            </div>
          );
          // The inline composer that opens when you comment on a line.
          const composerNote = () => (
            <div className="px-4 py-3 bg-steel-wash/50 grid gap-2">
              <div className="text-[11.5px] text-muted">Commenting on <b className="text-body">{commenting!.path.split("/").pop()}:{commenting!.line}</b></div>
              <RichText value={lineDraft} onChange={setLineDraft} rows={2} autoFocus minimal mentions={mentions} onSubmit={postLineComment} linkBase={`/${encodeURIComponent(tenant)}/${repo}`} placeholder="Leave a comment on this line…  (⌘↵ to submit)" />
              <div className="flex gap-2">
                <Button size="sm" disabled={!lineDraft.trim()} onClick={postLineComment}>Comment</Button>
                <Button size="sm" variant="secondary" onClick={() => { setCommenting(null); setSelLine(null); }}>Cancel</Button>
              </div>
            </div>
          );
          type Row = { n: number | string; sign?: string; code: React.ReactNode; note?: React.ReactNode; marker?: { tone: string; onClick: () => void; title: string } };
          // Interleave findings, line comments, and the open composer under their lines.
          const annotate = (rows: Row[], f: FileDiff): Row[] => {
            const fs = findingsByFile.get(f.path) ?? [];
            const lcs = lineCommentsByFile.get(f.path) ?? [];
            const res: Row[] = [];
            for (const row of rows) {
              if (row.sign !== "-" && typeof row.n === "number") {
                const ln = row.n;
                const here = fs.filter((x) => x.f.line === ln);
                const collapsed = here.filter((x) => collapsedFindings.has(x.key));
                res.push(collapsed.length ? { ...row, marker: { tone: sevTone(collapsed[0].f.severity), onClick: () => toggleFinding(collapsed[0].key), title: `reopen ${collapsed[0].f.severity} finding` } } : row);
                for (const x of here) if (!collapsedFindings.has(x.key)) res.push({ n: "", code: null, note: findingNote(x) });
                for (const c of lcs.filter((c) => c.line === ln)) res.push({ n: "", code: null, note: lineCommentNote(c) });
                if (commenting && commenting.path === f.path && commenting.line === ln) res.push({ n: "", code: null, note: composerNote() });
                continue;
              }
              res.push(row);
            }
            return res;
          };

          // Grouped (semantic) ops: behavioral/changed files first, then renames, then reformatted.
          const codeBody = (f: FileDiff) => {
            const fs = findingsByFile.get(f.path) ?? [];
            const lcs = lineCommentsByFile.get(f.path) ?? [];
            // Keep the diff open when it carries comments/an active composer, so they're never hidden.
            const forceOpen = lcs.length > 0 || commenting?.path === f.path;
            const anchoredLines = new Set<number>([...fs.map((x) => x.f.line ?? -1), ...lcs.map((c) => c.line ?? -1)]);
            if (commenting?.path === f.path) anchoredLines.add(commenting.line);
            const keepAnchored = (r: Row) => typeof r.n === "number" && anchoredLines.has(r.n);
            // A huge diff only ever RENDERS a bounded window of rows, so one enormous file can never
            // swamp the page. The window centres on diffFocus[path] (set by a "What changed here"
            // click) and grows by diffExpand[path] on "show more". Annotated files render in full.
            const expander = (hidden: number, where: "above" | "below"): Row => ({
              n: "", code: null, note: (
                <button onClick={() => setDiffExpand((e) => ({ ...e, [f.path]: (e[f.path] ?? 0) + 200 }))}
                  className="w-full py-2 text-[12.5px] font-medium text-steel-text hover:bg-steel-wash/50 flex items-center justify-center gap-1.5 bg-paper/40">
                  <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><line x1="12" y1="5" x2="12" y2="19" /><line x1="5" y1="12" x2="19" y2="12" /></svg>
                  Show {Math.min(hidden, 200)} more {where} · {hidden} line{hidden > 1 ? "s" : ""} hidden
                </button>
              ),
            });
            const capRows = (rows: Row[]): Row[] => {
              if (forceOpen) return rows;
              const grow = diffExpand[f.path] ?? 0;
              const RAD = 24 + grow;
              if (rows.length <= RAD * 2 + 8) return rows;
              const focus = diffFocus[f.path];
              const idx = focus != null ? rows.findIndex((r) => typeof r.n === "number" && r.n === focus) : -1;
              let lo: number, hi: number;
              if (idx >= 0) { lo = Math.max(0, idx - RAD); hi = Math.min(rows.length, idx + RAD + 1); }
              else { lo = 0; hi = Math.min(rows.length, RAD * 2 + 1); }
              const parts: Row[] = [];
              if (lo > 0) parts.push(expander(lo, "above"));
              parts.push(...rows.slice(lo, hi));
              if (hi < rows.length) parts.push(expander(rows.length - hi, "below"));
              return parts;
            };
            const isOpen = expandedDiffs.has(f.path) || forceOpen;
            // Take the span from first-changed to last-changed token so inner spaces/punctuation are
            // preserved (otherwise "walk_all ()" → "slice ( task )" collapses to "slicetask").
            const span = (segs: Seg[]) => {
              const first = segs.findIndex((s) => s.changed);
              if (first === -1) return "";
              let last = segs.length - 1; while (last >= 0 && !segs[last].changed) last--;
              return segs.slice(first, last + 1).map((s) => s.text).join("").trim();
            };
            // A "What changed here" row is noise if its content is only punctuation/brackets (e.g. "};",
            // "})", ",") or a comment — those aren't behaviourally interesting, so keep them out of the
            // summary. A transform is dropped only when BOTH sides are noise.
            const noise = (s: string) => {
              const t = s.trim();
              if (!t) return true;
              if (/^[\s{}()[\];,.:<>+\-|&?*]+$/.test(t)) return true;
              if (/^(\/\/|\/\*|\*\/|\*|#|--|<!--)/.test(t)) return true;
              return false;
            };
            // Cheap always-on pass: extract the "What changed here" summary WITHOUT building any React
            // elements. The heavy row/syntax-highlight build happens only for an OPEN diff (below), so a
            // huge collapsed file costs nothing per render — it no longer swamps the page.
            const transforms: { old: string; next: string; ln: number }[] = [];
            for (const h of f.hunks) {
              let n = h.new_start;
              const L = h.lines;
              for (let k = 0; k < L.length;) {
                const l = L[k];
                if (l.tag === "del") {
                  let d = k; while (L[d] && L[d].tag === "del") d++;
                  let a = d; while (L[a] && L[a].tag === "add") a++;
                  const dels = L.slice(k, d), adds = L.slice(d, a);
                  const pairs = Math.min(dels.length, adds.length);
                  for (let p = 0; p < pairs; p++) {
                    const w = wordDiff(dels[p].text, adds[p].text);
                    const oldS = span(w.old), nextS = span(w.next);
                    if (oldS || nextS) transforms.push({ old: oldS, next: nextS, ln: n + p });
                  }
                  for (let p = pairs; p < dels.length; p++) { const t = dels[p].text.trim(); if (t) transforms.push({ old: t, next: "", ln: n }); }
                  for (let p = pairs; p < adds.length; p++) { const t = adds[p].text.trim(); if (t) transforms.push({ old: "", next: t, ln: n + p }); }
                  n += adds.length; k = a;
                } else if (l.tag === "add") {
                  const t = l.text.trim(); if (t) transforms.push({ old: "", next: t, ln: n });
                  n++; k++;
                } else { n++; k++; }
              }
            }
            const renderHunk = (h: FileDiff["hunks"][number], hi: number) => {
              let o = h.old_start, n = h.new_start;
              const L = h.lines;
              const out: Row[] = [];
              for (let k = 0; k < L.length;) {
                const l = L[k];
                if (l.tag === "del") {
                  let d = k; while (L[d] && L[d].tag === "del") d++;
                  let a = d; while (L[a] && L[a].tag === "add") a++;
                  const dels = L.slice(k, d), adds = L.slice(d, a);
                  const pairs = Math.min(dels.length, adds.length);
                  const wds = Array.from({ length: pairs }, (_, p) => wordDiff(dels[p].text, adds[p].text));
                  dels.forEach((dl, p) => { out.push({ n: o, sign: "-", code: p < pairs ? <>{wdRender(wds[p].old, f.path, "old")}</> : <Hl text={dl.text} path={f.path} /> }); o++; });
                  adds.forEach((al, p) => { out.push({ n, sign: "+", code: p < pairs ? <>{wdRender(wds[p].next, f.path, "new")}</> : <Hl text={al.text} path={f.path} /> }); n++; });
                  k = a;
                } else if (l.tag === "add") {
                  out.push({ n, sign: "+", code: <Hl text={l.text} path={f.path} /> }); n++; k++;
                } else {
                  out.push({ n, sign: undefined, code: <Hl text={l.text} path={f.path} /> }); o++; n++; k++;
                }
              }
              return (
                <div key={hi}>
                  <LocationBar crumbs={f.path.split("/")} right={`@@ -${h.old_start} +${h.new_start} @@`} />
                  <CodePanel lines={capRows(annotate(fold(out, (hidden) => ({ n: "⋯", code: foldNote(hidden) }), keepAnchored), f))}
                    filePath={f.path} selectedLine={selLine?.path === f.path ? selLine.line : null}
                    highlightLine={diffFocus[f.path] ?? null}
                    onSelectLine={(ln: number | null) => setSelLine(ln == null ? null : { path: f.path, line: ln })}
                    onCommentLine={(ln: number) => openLineComment(f.path, ln)} />
                </div>
              );
            };
            // Render the whole file's diff; the clicked "What changed here" line stays highlighted
            // (diffFocus → highlightLine) and you can select/comment on any line.
            const indexed = f.hunks.map((h, hi) => ({ h, hi }));
            const hunkNodes = isOpen ? <>{indexed.map(({ h, hi }) => renderHunk(h, hi))}</> : null;
            // Drop punctuation/comment-only noise, then dedupe so a repeated edit isn't listed twice.
            const seen = new Set<string>();
            const uniq = transforms
              .filter((t) => !(noise(t.old) && noise(t.next)))
              .filter((t) => { const k = t.old + "→" + t.next; if (seen.has(k)) return false; seen.add(k); return true; });
            return (
              <>
                {uniq.length > 0 && (() => {
                  const clip = (s: string) => (s.length > 52 ? s.slice(0, 51).trimEnd() + "…" : s);
                  return (
                    <div className="mb-3 rounded-ctl border border-rule2 overflow-hidden">
                      <div className="px-3.5 py-2 bg-paper border-b border-rule2 flex items-center justify-between">
                        <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted">What changed here</span>
                        <span className="text-[11px] text-faint tabular-nums">{uniq.length} edit{uniq.length > 1 ? "s" : ""} · click to jump</span>
                      </div>
                      <div className="py-1 max-h-[300px] overflow-y-auto">
                        {uniq.map((t, i) => (
                          <button key={i} onClick={() => revealDiff(f.path, t.ln)} title={`Jump to line ${t.ln}`}
                            className="group w-full grid grid-cols-[auto_1fr] items-center gap-x-2.5 text-[13px] text-left px-3.5 py-1.5 hover:bg-steel-wash/60 transition-colors">
                            <span className="inline-flex items-center h-[19px] px-1.5 rounded-[3px] bg-rule2 text-dim text-[11px] font-semibold tabular-nums group-hover:bg-steel group-hover:text-white transition-colors flex-none">L{t.ln}</span>
                            <span className="flex items-center gap-2 flex-wrap min-w-0 leading-relaxed">
                              {t.old ? <OldTok>{clip(t.old)}</OldTok> : <span className="text-muted italic text-[12.5px]">added</span>}
                              <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="text-faint flex-none"><line x1="5" y1="12" x2="19" y2="12" /><polyline points="12 5 19 12 12 19" /></svg>
                              {t.next ? <NewTok>{clip(t.next)}</NewTok> : <span className="text-muted italic text-[12.5px]">removed</span>}
                            </span>
                          </button>
                        ))}
                      </div>
                    </div>
                  );
                })()}
                {isOpen ? (() => {
                  // A compact review bar top + bottom of the open diff, so you can act right after reading.
                  const reviewBar = !forceOpen ? (
                    <div className="flex justify-end py-1">
                      <button onClick={() => setExpandedDiffs((s) => { const n = new Set(s); n.delete(f.path); return n; })} className="text-[12px] text-muted hover:text-ink inline-flex items-center gap-1">Hide diff <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round"><polyline points="18 15 12 9 6 15" /></svg></button>
                    </div>
                  ) : null;
                  return (
                    <>
                      {reviewBar}
                      {hunkNodes}
                      {reviewBar}
                    </>
                  );
                })() : f.too_large ? (
                  <div className="w-full py-2.5 rounded-ctl border border-dashed border-rule text-[12.5px] text-muted flex items-center justify-center gap-2">
                    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><circle cx="12" cy="12" r="10" /><line x1="12" y1="8" x2="12" y2="12" /><line x1="12" y1="16" x2="12.01" y2="16" /></svg>
                    File too large to diff inline — open it in the Files tab to view.
                  </div>
                ) : (
                  <button onClick={() => revealDiff(f.path)} className="w-full py-2.5 rounded-ctl border border-dashed border-rule text-[12.5px] text-muted hover:text-ink hover:border-ctl hover:bg-paper/50 transition-colors flex items-center justify-center gap-2">
                    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><polyline points="6 9 12 15 18 9" /></svg>
                    Show diff · {f.hunks.reduce((acc, h) => acc + h.lines.length, 0)} lines
                  </button>
                )}
              </>
            );
          };
          const fileOps = ordered.filter((f) => cls(f.path) !== "reformatted").map((f) => ({
            kind: opKind(f),
            title: base(f.path),
            meta: `${f.path}  ·  +${count(f, "add")} −${count(f, "del")}`,
            body: codeBody(f),
          }));
          const moveOps = (semantic?.moves ?? []).map((m) => ({
            kind: "rename",
            title: `Move ${base(m.from)}`,
            meta: `exact move · ⬡ ${m.blob.slice(0, 8)}`,
            body: (
              <div className="text-[13px] text-body">
                <div className="flex items-center gap-2 flex-wrap"><OldTok>{m.from}</OldTok><span className="text-muted">→</span><NewTok>{m.to}</NewTok></div>
                <p className="text-[12.5px] text-muted mt-2.5">Byte-identical relocation (proven by content address). Behavior-preserving.</p>
              </div>
            ),
          }));
          const reformatOps = (semantic?.whitespace_only ?? []).map((p) => ({
            kind: "chart",
            title: base(p),
            meta: `${p} · whitespace only`,
            body: <p className="text-[13px] text-muted">Reformatted only — no semantic change (whitespace/layout). Low risk.</p>,
          }));
          const ops = [...fileOps, ...moveOps, ...reformatOps];
          if (ops.length === 0) ops.push({ kind: "behavior", title: "changes", meta: "", body: <p className="text-[13px] text-muted">no semantic operations detected — see line-by-line.</p> });

          // Line-by-line (raw) view with computed old/new line numbers.
          const rawFiles = ordered.map((f) => ({
            name: f.path,
            add: count(f, "add"),
            del: count(f, "del"),
            hunks: f.hunks.map((h) => {
              let o = h.old_start, n = h.new_start;
              const lines = h.lines.map((l) => {
                const code = <Hl text={l.text} path={f.path} />;
                let row: { o: number | string; n: number | string; sign?: string; code: React.ReactNode };
                if (l.tag === "add") { row = { o: "", n, sign: "+", code }; n++; }
                else if (l.tag === "del") { row = { o, n: "", sign: "-", code }; o++; }
                else { row = { o, n, code }; o++; n++; }
                return row;
              });
              return { header: `@@ -${h.old_start} +${h.new_start} @@`, lines: fold(lines, (hidden) => ({ o: "", n: "⋯", code: foldNote(hidden) })) };
            }),
          }));
          const mechanical = (semantic?.moves.length ?? 0) + (semantic?.whitespace_only.length ?? 0);
          const rawDiff = { files: rawFiles, hiddenNote: mechanical > 0 ? `${semantic!.moves.length} move${semantic!.moves.length === 1 ? "" : "s"} + ${semantic!.whitespace_only.length} reformatted (mechanical) are grouped in the semantic view.` : undefined };

          const voyageMeta = semantic
            ? `${semantic.behavioral.length} behavior change${semantic.behavioral.length === 1 ? "" : "s"} · ${semantic.moves.length} moved · ${semantic.whitespace_only.length} reformatted`
            : `${diff.length} file${diff.length === 1 ? "" : "s"} changed`;
          const voyage = { title: change?.intent ? change.intent.split("\n")[0] : (pr ? pr.title : "changes"), id: (changeId ?? "").slice(0, 8), meta: voyageMeta };
          return <SemanticDiff voyage={voyage} ops={ops} rawDiff={rawDiff} showMerge={false} storageKey={changeId ? `hull_reviewed_${changeId}` : undefined} />;
        })()}

        {/* reconciliation — revealed (and scrolled to) from the digest's claims toggle. */}
        <div id="reconciliation-section" className="scroll-mt-20" />
        {showClaims && shownLedger && shownLedger.claims.length > 0 && (() => {
          const ledger = shownLedger;
          const POSITIVE = ["verified_mechanically", "verified_read_only", "self_attested"];
          // Count against human resolutions: a resolved claim is no longer "to judge".
          const supported = ledger.claims.filter((c) => POSITIVE.includes(c.status) || resolutions[c.id]?.judgment === "verified").length;
          const contradicted = ledger.claims.filter((c) => c.status === "contradicted" && resolutions[c.id]?.judgment !== "verified").length;
          const needs = ledger.claims.filter((c) => c.status === "needs_judgment" && !resolutions[c.id]).length;
          const label: Record<string, string> = { verified_mechanically: "verified", verified_read_only: "read-only verified", self_attested: "self-attested", contradicted: "contradicted", needs_judgment: "needs judgment" };
          const dotTone = (s: string): "ok" | "bad" | "warn" | "wait" => s === "contradicted" ? "bad" : s === "needs_judgment" ? "wait" : s === "self_attested" ? "warn" : "ok";
          const labelColor = (s: string) => s === "contradicted" ? "text-fault-text" : (s === "needs_judgment" || s === "self_attested") ? "text-brass-text" : "text-clear-text";
          const isVerified = (s: string) => s === "verified_mechanically" || s === "verified_read_only";
          const verified = ledger.claims.filter((c) => isVerified(c.status));
          const Row = (c: Claim) => (
            <div key={c.id} className="px-5 py-3.5 border-b border-rule2 last:border-0 flex gap-3">
              <StatusDot tone={dotTone(c.status)} />
              <div className="min-w-0 flex-1">
                <div className="flex items-baseline gap-2 flex-wrap">
                  <span className="text-[13.5px] text-body flex-1 min-w-[200px] leading-snug">{c.text}</span>
                  <span className={`text-[11px] font-semibold ${labelColor(c.status)}`}>{label[c.status] ?? c.status}</span>
                  <span className="text-[11px] text-faint">{c.source}</span>
                </div>
                {c.evidence.length > 0 && (
                  <div className="grid gap-1 mt-2">
                    {c.evidence.map((e, i) => (
                      <div key={i} className={`text-[12.5px] flex gap-2 items-baseline ${e.supports ? "text-dim" : "text-fault-text"}`}>
                        <span className={`text-[10px] font-semibold uppercase tracking-[0.05em] px-1.5 py-[1px] rounded flex-none ${e.supports ? "bg-paper text-muted" : "bg-fault-wash text-fault-text"}`}>{e.kind}</span>
                        <span className="leading-snug">{e.detail}</span>
                      </div>
                    ))}
                  </div>
                )}
                {resolutions[c.id] ? (
                  <div className={`text-[12.5px] mt-2.5 ${resolutions[c.id].judgment === "verified" ? "text-clear-text" : "text-fault-text"}`}>
                    {resolutions[c.id].judgment === "verified" ? "✓ verified by a human" : "⚑ concern raised"} · <b>{resolutions[c.id].by}</b>
                    {resolutions[c.id].note && <span className="text-muted"> — {resolutions[c.id].note}</span>}
                  </div>
                ) : (c.status === "needs_judgment" || c.status === "self_attested") ? (
                  <div className="flex items-center gap-2 mt-2.5 flex-wrap">
                    <Button size="sm" variant="secondary" disabled={!canAct} onClick={() => resolveClaim(c.id, "verified")}>✓ I checked — verified</Button>
                    <Button size="sm" variant="destructive" disabled={!canAct} onClick={() => resolveClaim(c.id, "concern")}>⚑ Raise concern</Button>
                    {pr && change && canFix && <Button size="sm" variant="secondary" disabled={!canAct || fixingClaim === c.id} onClick={() => fixClaim(c)}>{fixingClaim === c.id ? "fixing…" : "✨ Fix with AI"}</Button>}
                  </div>
                ) : c.status === "contradicted" ? (
                  <div className="flex items-center gap-2 mt-2.5 flex-wrap">
                    {pr && change && canFix && <Button size="sm" variant="secondary" disabled={!canAct || fixingClaim === c.id} onClick={() => fixClaim(c)}>{fixingClaim === c.id ? "fixing…" : "✨ Fix with AI"}</Button>}
                  </div>
                ) : null}
              </div>
            </div>
          );
          return (
            <Card>
              <SectionHeader label="Reconciliation" right={<span className="text-[12.5px] text-muted tabular-nums">{supported} supported{needs ? ` · ${needs} to judge` : ""}{contradicted ? ` · ${contradicted} contradicted` : ""}</span>} />
              <div className="px-5 py-2.5 bg-paper/40 border-b border-rule2 text-[12.5px] text-muted leading-snug">
                Each claim the change makes, checked against what the code actually does — the evidence
                behind the verdict. Anything needing a human is lifted to “Needs your attention” above.
              </div>
              {(ledger.unclaimed?.length ?? 0) > 0 && (
                <details className="group border-b border-rule2">
                  <summary className="px-5 py-3.5 flex gap-3 cursor-pointer select-none list-none items-start hover:bg-paper/40">
                    <StatusDot tone="warn" />
                    <div className="min-w-0 flex-1 text-[13.5px] text-body leading-snug"><b>{ledger.unclaimed!.length} unclaimed change{ledger.unclaimed!.length > 1 ? "s" : ""}</b> — semantic edits the narrative never names (usually just a diff larger than the summary).</div>
                    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" className="text-faint mt-0.5 flex-none group-open:rotate-90 transition-transform"><polyline points="9 18 15 12 9 6" /></svg>
                  </summary>
                  <div className="px-5 pb-3.5 pl-[52px]">
                    <SearchableList items={ledger.unclaimed!} searchOf={(op) => op} placeholder="Search unclaimed changes…"
                      renderItem={(op, i) => <div key={i} className="text-[12.5px] text-fault-text flex gap-2 items-baseline"><span className="text-[10px] font-semibold uppercase tracking-[0.05em] px-1.5 py-[1px] rounded bg-fault-wash flex-none">phantom</span><span className="leading-snug break-all">{op}</span></div>} />
                  </div>
                </details>
              )}
              {verified.length > 0 ? verified.map(Row) : (
                <div className="px-5 py-4 text-[13px] text-dim flex items-center gap-2.5"><StatusDot tone="ok" /> No mechanically-verified claims.</div>
              )}
            </Card>
          );
        })()}

        {/* Findings live inline in the diff now (at their line, collapsible). Only findings we can't
            anchor to a diff line get a residual card here. */}
        {unmappedFindings.length > 0 && (() => {
          const sevColor = (s: string) => s === "blocker" ? "text-fault-text" : s === "warn" ? "text-brass-text" : "text-steel-text";
          return (
            <Card>
              <SectionHeader label="Other findings" right={<span className="text-[12.5px] text-muted">not tied to a diff line</span>} />
              <div>
                {unmappedFindings.map(({ f, reviewer, idx }) => (
                  <div key={idx} className="px-5 py-3.5 border-b border-rule2 last:border-0 flex gap-3">
                    <StatusDot tone={sevTone(f.severity)} />
                    <div className="min-w-0 flex-1">
                      <div className="flex items-baseline gap-2 flex-wrap">
                        <span className={`text-[11px] font-bold uppercase tracking-[0.03em] ${sevColor(f.severity)}`}>{f.severity}</span>
                        {f.path && <code className="text-[12px] text-dim">{f.path}{f.line ? `:${f.line}` : ""}</code>}
                      </div>
                      <p className="text-[13.5px] text-body mt-1 leading-snug">{f.note}</p>
                      <div className="flex items-center gap-2 mt-2 flex-wrap">
                        <span className="inline-flex items-center gap-1.5 text-[11.5px] text-muted"><Avatar id={reviewer} handle={handleOf(reviewer)} kind={kindOf(reviewer)} size={16} />{handleOf(reviewer)}</span>
                        {f.severity !== "info" && pr && f.path && canFix && (<><span className="text-faint">·</span><Button size="sm" variant="secondary" disabled={!canAct || fixing === idx} onClick={() => fixWithAI(idx, f)}>{fixing === idx ? "fixing…" : "✨ Fix with AI"}</Button></>)}
                      </div>
                    </div>
                  </div>
                ))}
              </div>
            </Card>
          );
        })()}

        {/* conversation timeline — reviews + comments, one accountable thread */}
        {pr && (
          <Card id="pr-conversation" className="scroll-mt-4">
            <SectionHeader label="Conversation" right={reviewTools ?? <span className="text-[12.5px] text-muted">reviews and comments, humans and agents</span>} />
            <div className="px-5 py-4 grid gap-3.5">
              {(() => {
                const verb: Record<string, string> = { approve: "approved", request_changes: "requested changes on", reject: "rejected", comment: "reviewed" };
                const vcolor = (v: string) => v === "approve" ? "bg-clear text-white" : v === "reject" ? "bg-fault text-white" : v === "request_changes" ? "bg-brass text-white" : "border border-rule text-muted";
                const items = [
                  ...reviews.map((r) => ({ ts: r.created_unix ?? 0, kind: "review" as const, r })),
                  ...thread.filter((c) => !(c.path && c.line)).map((c) => ({ ts: c.created_unix, kind: "comment" as const, c })),
                ].sort((a, b) => a.ts - b.ts);
                if (items.length === 0) return <div className="text-[13px] text-muted">no activity yet — add a review or comment below.</div>;
                return items.map((e) => e.kind === "review" ? (
                  <div key={`r${e.r.id}`} className="grid gap-1.5">
                    <div className="flex items-center gap-2.5 text-[13px] flex-wrap">
                      <span className={`grid place-items-center w-[24px] h-[24px] rounded-full flex-none text-[12px] ${vcolor(e.r.verdict)}`}>{e.r.verdict === "approve" ? "✓" : e.r.verdict === "reject" || e.r.verdict === "request_changes" ? "!" : "◍"}</span>
                      <Avatar id={e.r.reviewer} handle={handleOf(e.r.reviewer)} kind={kindOf(e.r.reviewer)} size={18} />
                      <b className={kindOf(e.r.reviewer) === "agent" ? "text-steel-text" : ""}>{handleOf(e.r.reviewer)}</b>
                      <span className="text-muted">{verb[e.r.verdict] ?? "reviewed"} this pull request</span>
                      {e.r.findings.length > 0 && <button onClick={() => setActiveId(e.r.id)} className="cursor-pointer"><Tag>{e.r.findings.length} finding{e.r.findings.length > 1 ? "s" : ""}</Tag></button>}
                      <span className="text-faint tabular-nums ml-auto" title={new Date(e.ts * 1000).toLocaleString()}>{timeAgo(e.ts)}</span>
                    </div>
                    {e.r.summary?.trim() && <div className="ml-[34px] border border-rule2 rounded-ctl px-3 py-2 bg-surface"><Markdown text={e.r.summary} linkBase={`/${encodeURIComponent(tenant)}/${repo}`} className="text-[13.5px] text-body" /></div>}
                  </div>
                ) : (
                  <div className="flex gap-2.5" key={`c${e.c.id}`}>
                    <Avatar id={e.c.author} handle={handleOf(e.c.author)} kind={kindOf(e.c.author)} size={26} />
                    <div className="flex-1 min-w-0 border border-rule2 rounded-ctl overflow-hidden">
                      <div className="flex items-center gap-2 px-3 py-1.5 bg-paper border-b border-rule3 text-[12.5px]">
                        <b className={kindOf(e.c.author) === "agent" ? "text-steel-text" : ""}>{handleOf(e.c.author)}</b>
                        <span className="text-faint tabular-nums" title={new Date(e.c.created_unix * 1000).toLocaleString()}>{timeAgo(e.c.created_unix)}</span>
                      </div>
                      <Markdown text={e.c.body} linkBase={`/${encodeURIComponent(tenant)}/${repo}`} className="px-3 py-2 text-[13.5px] text-body" />
                    </div>
                  </div>
                ));
              })()}
              <div className="mt-1 grid gap-2 scroll-mt-20" id="pr-composer">
                {canAct ? <RichText value={draft} onChange={setDraft} rows={3} mentions={mentions} linkBase={`/${encodeURIComponent(tenant)}/${repo}`} onSubmit={() => !composerDisabled && runMode(composerMode)} placeholder="Leave a comment…" />
                  : <div className="border border-ctl rounded-ctl px-2.5 py-2 text-[13px] text-faint">sign in to comment</div>}
                <div className="flex justify-end">
                {canAct ? (
                  <SplitButton disabled={composerDisabled} onSubmit={() => runMode(composerMode)} menuWidth={256}
                    icon={MODES.find((m) => m.id === composerMode)!.icon} label={MODES.find((m) => m.id === composerMode)!.label}
                    menu={
                      <div className="py-1">
                        {/* Selecting only changes what kind of reply this is — it doesn't submit. */}
                        {MODES.map((m) => (
                          <button key={m.id} type="button" onClick={() => setComposerMode(m.id)}
                            className={`w-full text-left px-3 py-2 flex items-start gap-2.5 hover:bg-paper ${m.id === composerMode ? "bg-paper" : ""}`}>
                            <span className={`mt-[1px] flex-none ${m.color}`}>{m.icon}</span>
                            <span className="min-w-0 flex-1">
                              <span className="block text-[13px] font-medium text-body leading-tight">{m.label}</span>
                              <span className="block text-[11.5px] text-muted leading-tight mt-0.5">{m.hint}</span>
                            </span>
                            {m.id === composerMode && <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round" className="text-steel-text mt-0.5 flex-none"><polyline points="20 6 9 17 4 12" /></svg>}
                          </button>
                        ))}
                        {pr && (
                          <>
                            <div className="my-1 border-t border-rule2" />
                            {pr.state === "open" ? (
                              <button type="button" onClick={() => closeOrReopenPr(false)} className="w-full text-left px-3 py-2 flex items-start gap-2.5 hover:bg-fault-wash">
                                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="text-fault-text mt-[1px] flex-none"><circle cx="12" cy="12" r="10" /><line x1="15" y1="9" x2="9" y2="15" /><line x1="9" y1="9" x2="15" y2="15" /></svg>
                                <span className="min-w-0 flex-1"><span className="block text-[13px] font-medium text-fault-text leading-tight">Close pull request</span><span className="block text-[11.5px] text-muted leading-tight mt-0.5">Close without merging</span></span>
                              </button>
                            ) : (
                              <button type="button" onClick={() => closeOrReopenPr(true)} className="w-full text-left px-3 py-2 flex items-start gap-2.5 hover:bg-paper">
                                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="text-dim mt-[1px] flex-none"><path d="M3 12a9 9 0 1 0 9-9 9 9 0 0 0-6.36 2.64L3 8" /><polyline points="3 3 3 8 8 8" /></svg>
                                <span className="min-w-0 flex-1"><span className="block text-[13px] font-medium text-body leading-tight">Reopen pull request</span><span className="block text-[11.5px] text-muted leading-tight mt-0.5">Put it back in review</span></span>
                              </button>
                            )}
                          </>
                        )}
                      </div>
                    } />
                ) : <Button size="md" disabled>Comment</Button>}
                </div>
              </div>
            </div>
          </Card>
        )}

          </div>
        </div>

      </div>
    </div>
  );
}



