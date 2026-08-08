import { lazy, Suspense, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
// Code-split the heavy Shiki-powered @pierre viewers into their own chunk (kept out of the initial bundle).
const PierrePatch = lazy(() => import("@pierre/diffs/react").then((m) => ({ default: m.PatchDiff })));
import * as ed from "@noble/ed25519";
import { generateIdentity, wrapSecret, unwrapSecret, signMessage } from "./sovereign";
import { Button, LinkButton } from "./ui/Button";
import { HTabs, Segmented } from "./ui/Tabs";
import { SearchInput, Switch, TextField } from "./ui/Field";
import { StatusBadge, Tag } from "./ui/Badge";
import { Drawer, Dialog, PromptModal, ConfirmModal } from "./ui/Overlay";
import { SemanticDiff, OldTok, NewTok } from "./ui/SemanticDiff";
import { createPasskey, getPasskey } from "./webauthn";
import { wordDiff, type Seg } from "./highlight";
import { Markdown } from "./markdown";
import { RichText } from "./ui/RichText";
import { apiGet, apiPost, apiPut, apiPatch, apiDelete } from "./api";
import { Popover, Picker, SplitButton } from "./components/menus";
import { CommandPalette, type CmdItem } from "./components/CommandPalette";
import { ContributionHeatmap, type HeatDay } from "./components/ContributionHeatmap";
import { RepoGraph } from "./components/RepoGraph";
import { Boundary, Card, SectionHeader } from "./components/primitives";
import { RepoFiles } from "./components/RepoFiles";


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

// Turn a failed Response into human copy: map the common status codes to a friendly line, and fall
// back to the server's own message for anything else (validation errors etc. read better raw).
const FRIENDLY_STATUS: Record<number, string> = {
  401: "Please sign in again.",
  403: "You don't have permission to do that.",
  404: "Not found.",
  500: "Something went wrong on the server.",
  502: "Something went wrong on the server.",
  503: "Something went wrong on the server.",
};
// Set by the App so any 401 surfaced through `apiError` drops the (now-expired) session to the
// signed-out state, instead of leaving a half-authed UI whose write buttons keep 401-ing.
let onUnauthorized: (() => void) | null = null;
async function apiError(res: Response): Promise<string> {
  if (res.status === 401) onUnauthorized?.();
  if (FRIENDLY_STATUS[res.status]) return FRIENDLY_STATUS[res.status];
  const text = await res.text().catch(() => "");
  return text.trim() || "Something went wrong.";
}
// Mirror the backend `sanitize_handle` (crates/hull-server/src/lib.rs): keep only `[A-Za-z0-9._-]`,
// map any run of other characters (whitespace, punctuation, non-ASCII, emoji) to a single `_`,
// collapse `..` (no path traversal), and strip leading dots/underscores. The result satisfies the
// server's `safe_segment`, so a create-org/create-repo input can't show or submit an invalid handle.
// A trailing separator the user is still typing is intentionally kept (the backend strips it on
// submit) so multi-word handles like "new org" -> "new_org" type through naturally.
const sanitizeHandle = (s: string) =>
  s.replace(/[^A-Za-z0-9._-]+/g, "_").replace(/\.\.+/g, "_").replace(/^[._]+/, "");
// EXACT parity with the server's `sanitize_handle` (Rust): unlike the lenient `sanitizeHandle` above
// (which keeps `_` and trailing separators so a handle types through naturally), this treats `_` as a
// separator (collapsing runs) and strips trailing separators — the form the SERVER stores and verifies.
// Use it at submit for anything the server will re-sanitize AND cryptographically check (the sovereign
// proof-of-possession), so the value the client signs matches the handle the server computes. Idempotent.
const serverHandle = (s: string) =>
  s.replace(/[^A-Za-z0-9.-]+/g, "_").replace(/\.\.+/g, "_").replace(/^[._]+/, "").replace(/[._-]+$/, "");

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
  edited_unix?: number;
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
// Named line-icons for labels — SVGs, not emoji. `icon` stores the NAME (e.g. "bug"); a legacy value
// that isn't a known name (an old emoji) still renders as-is so existing labels don't break.
const LABEL_ICON_PATHS: Record<string, React.ReactNode> = {
  bug: <><path d="M8 2l1.5 2M16 2l-1.5 2" /><rect x="8" y="6" width="8" height="12" rx="4" /><path d="M8 10H4M8 14H4M8 18l-3 3M16 10h4M16 14h4M16 18l3 3M12 6V4" /></>,
  sparkle: <path d="M12 3l1.9 5.1L19 10l-5.1 1.9L12 17l-1.9-5.1L5 10l5.1-1.9z" />,
  note: <><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" /><polyline points="14 2 14 8 20 8" /><line x1="8" y1="13" x2="16" y2="13" /><line x1="8" y1="17" x2="13" y2="17" /></>,
  fire: <path d="M12 2c1 3-1 4-2 6-1 1.5-1 4 2 4s3-2.5 2-4c2 1.5 3 3.5 3 6a7 7 0 1 1-14 0c0-3 2-5 4-7 1 2 2 2 3 3 .5-3-1-5-1-8z" />,
  warning: <><path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z" /><line x1="12" y1="9" x2="12" y2="13" /><line x1="12" y1="17" x2="12.01" y2="17" /></>,
  rocket: <><path d="M4.5 16.5c-1.5 1.3-2 5-2 5s3.7-.5 5-2c.7-.8.7-2 0-2.8a2 2 0 0 0-3 0z" /><path d="M12 15l-3-3a12 12 0 0 1 3-7c2-2 5-3 7-3 0 2-1 5-3 7a12 12 0 0 1-4 6z" /><circle cx="15" cy="9" r="1" /></>,
  broom: <><path d="M19 3l-6 6M14 8l2 2M10 12l-4 8 8-4M8 14l2 2" /></>,
  lock: <><rect x="4" y="11" width="16" height="10" rx="2" /><path d="M8 11V7a4 4 0 0 1 8 0v4" /></>,
  bulb: <><path d="M9 18h6" /><path d="M10 22h4" /><path d="M12 2a7 7 0 0 0-4 12.7c.6.5 1 1.3 1 2.1v.2h6v-.2c0-.8.4-1.6 1-2.1A7 7 0 0 0 12 2z" /></>,
  box: <><path d="M21 8l-9-5-9 5v8l9 5 9-5z" /><path d="M3 8l9 5 9-5M12 13v8" /></>,
  palette: <><path d="M12 3a9 9 0 1 0 0 18c1 0 1.5-.8 1.5-1.5 0-1 .8-1.5 1.5-1.5H17a4 4 0 0 0 4-4c0-5-4-8-9-8z" /><circle cx="7.5" cy="10.5" r="1" /><circle cx="12" cy="7.5" r="1" /><circle cx="16.5" cy="10.5" r="1" /></>,
  bolt: <polygon points="13 2 3 14 12 14 11 22 21 10 12 10" />,
  question: <><circle cx="12" cy="12" r="10" /><path d="M9.1 9a3 3 0 0 1 5.8 1c0 2-3 3-3 3" /><line x1="12" y1="17" x2="12.01" y2="17" /></>,
  cone: <><path d="M10 3h4l4 18H6z" /><line x1="7.5" y1="12" x2="16.5" y2="12" /><line x1="6" y1="21" x2="18" y2="21" /></>,
};
const LABEL_ICONS = Object.keys(LABEL_ICON_PATHS);
const labelIco = (name: string | undefined, size = 11) => {
  if (!name) return null;
  const p = LABEL_ICON_PATHS[name];
  if (!p) return <span className="leading-none">{name}</span>; // legacy emoji fallback
  return <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="flex-none">{p}</svg>;
};
const Label = ({ name, color, icon }: { name: string; color?: string; icon?: string }) => {
  const c = color || "#8b949e";
  return (
    <span className="inline-flex items-center gap-1 text-[12px] font-semibold px-1.5 py-[2px] rounded-badge" style={{ background: c, color: contrastText(c) }}>
      {labelIco(icon)}{name}
    </span>
  );
};
const LABEL_COLORS = ["#d73a4a", "#e99695", "#fbca04", "#0e8a16", "#006b75", "#1d76db", "#0052cc", "#5319e7", "#b60205", "#c5def5", "#bfdadc", "#8b949e"];

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

// Compact large-number format for KPIs: 15021 → 15k, 9952890 → 9.95M.
const fmtNum = (n: number) => (n >= 1e9 ? (n / 1e9).toFixed(2) + "B" : n >= 1e6 ? (n / 1e6).toFixed(2) + "M" : n >= 1e3 ? (n / 1e3).toFixed(1) + "k" : String(Math.round(n)));
// A day's (or bucket's) worth of tokens for the bar chart. `dayEnd` is set only for bucketed bars.
type TokBar = { day: number; dayEnd?: number; value: number };
// Mini per-day bar chart — no chart lib. Hovering a bar reveals that day's exact token count.
const MiniBars = ({ bars, color }: { bars: TokBar[]; color: string }) => {
  const [hover, setHover] = useState<{ x: number; y: number; label: string; value: number } | null>(null);
  if (bars.every((b) => b.value === 0)) return <div className="h-[40px] grid place-items-center text-[11px] text-faint">no usage in this range</div>;
  const max = Math.max(...bars.map((b) => b.value), 1);
  const fmtDay = (d: number) => new Date(d * 86_400_000).toLocaleDateString(undefined, { month: "short", day: "numeric" });
  return (
    <div className="relative flex items-end gap-px h-[40px]">
      {bars.map((b, i) => (
        <div key={i} className="flex-1 min-w-px flex items-end h-full cursor-default"
          onMouseMove={(e) => setHover({ x: e.clientX, y: e.clientY, label: b.dayEnd && b.dayEnd !== b.day ? `${fmtDay(b.day)}–${fmtDay(b.dayEnd)}` : fmtDay(b.day), value: b.value })}
          onMouseLeave={() => setHover(null)}>
          <div className="w-full rounded-t-[2px] hover:brightness-110 transition-[filter]" style={{ height: b.value > 0 ? `${Math.max(9, (b.value / max) * 100)}%` : 0, background: color, opacity: b.value > 0 ? 1 : 0 }} />
        </div>
      ))}
      {hover && (
        <div className="fixed z-[80] pointer-events-none -translate-x-1/2 -translate-y-full" style={{ left: hover.x, top: hover.y - 10 }}>
          <div className="rounded-ctl-sm bg-surface border border-rule2 shadow-modal px-2.5 py-1.5 text-[11.5px] whitespace-nowrap">
            <div className="font-semibold tabular-nums" style={{ color }}>{hover.value.toLocaleString()} tokens</div>
            <div className="text-faint mt-0.5">{hover.label}</div>
          </div>
        </div>
      )}
    </div>
  );
};
// Token-usage KPIs — shared by the profile and org Overview. Per-day bar charts with a time-range
// dropdown (above the out box); hovering a box shows the exact total, hovering a bar shows that day.
const TOK_RANGES: { value: string; label: string; days: number }[] = [
  { value: "7", label: "Last 7 days", days: 7 },
  { value: "30", label: "Last 30 days", days: 30 },
  { value: "90", label: "Last 90 days", days: 90 },
  { value: "365", label: "Last year", days: 365 },
];
const TokenKpis = ({ tokens }: { tokens?: { in: number; out: number; series: { day: number; in: number; out: number }[] } }) => {
  const [range, setRange] = useState("30");
  if (!tokens || tokens.in + tokens.out === 0) return null;
  const days = TOK_RANGES.find((r) => r.value === range)?.days ?? 30;
  const today = Math.floor(Date.now() / 86_400_000);
  const start = today - days + 1;
  const byDay = new Map((tokens.series ?? []).map((s) => [s.day, s]));
  // Fill every day in the range (0 where idle), then bucket down to ≤45 bars so long ranges stay legible.
  const fill = (pick: (s: { in: number; out: number }) => number): TokBar[] => {
    const raw: TokBar[] = [];
    for (let d = start; d <= today; d++) { const s = byDay.get(d); raw.push({ day: d, value: s ? pick(s) : 0 }); }
    const MAX = 45;
    if (raw.length <= MAX) return raw;
    const size = Math.ceil(raw.length / MAX);
    const out: TokBar[] = [];
    for (let i = 0; i < raw.length; i += size) { const c = raw.slice(i, i + size); out.push({ day: c[0].day, dayEnd: c[c.length - 1].day, value: c.reduce((a, x) => a + x.value, 0) }); }
    return out;
  };
  const barsIn = fill((s) => s.in), barsOut = fill((s) => s.out);
  const sum = (b: TokBar[]) => b.reduce((a, x) => a + x.value, 0);
  const kpi = (label: string, total: number, bars: TokBar[], color: string) => (
    <div className="flex-1 min-w-[180px] rounded-card border border-rule bg-surface px-3.5 py-2.5" title={`${total.toLocaleString()} tokens ${label} · ${TOK_RANGES.find((r) => r.value === range)?.label.toLowerCase()}`}>
      <div className="flex items-baseline justify-between gap-2">
        <span className="text-[11px] font-semibold uppercase tracking-[0.05em] text-muted">{label}</span>
        <span className="text-[17px] font-semibold tabular-nums" style={{ color }}>{fmtNum(total)}</span>
      </div>
      <div className="mt-1.5"><MiniBars bars={bars} color={color} /></div>
    </div>
  );
  return (
    <div className="grid gap-2">
      <div className="flex items-center justify-end">
        <Picker size="sm" width={150} value={range} onChange={setRange} options={TOK_RANGES.map((r) => ({ value: r.value, label: r.label }))} />
      </div>
      <div className="flex flex-wrap gap-4">
        {kpi("in", sum(barsIn), barsIn, "var(--dim)")}
        {kpi("out", sum(barsOut), barsOut, "var(--steel)")}
      </div>
    </div>
  );
};
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
          {LABEL_ICONS.map((ic) => <button key={ic} type="button" title={ic} onClick={() => setDraft((d) => ({ ...d, icon: ic }))} className={`${chip} grid place-items-center text-dim ${draft.icon === ic ? "border-body bg-surface text-ink" : "border-rule hover:border-dim"}`}>{labelIco(ic, 15)}</button>)}
        </div>
        <div className="flex items-center gap-1.5 flex-wrap">
          <span className="text-[11.5px] text-muted w-10 flex-none">Color</span>
          {LABEL_COLORS.map((c) => <button key={c} type="button" onClick={() => setDraft((d) => ({ ...d, color: c }))} className={`w-6 h-6 rounded-full transition-transform ${draft.color.toLowerCase() === c ? "ring-2 ring-offset-1 ring-body scale-110" : "hover:scale-110"}`} style={{ background: c }} />)}
          <label className="w-7 h-7 rounded-ctl border border-rule overflow-hidden cursor-pointer relative" title="custom color" style={{ background: draft.color }}><input type="color" value={draft.color} onChange={(e) => setDraft((d) => ({ ...d, color: e.target.value }))} className="absolute inset-0 opacity-0 cursor-pointer" /></label>
          <button type="button" onClick={() => setDraft((d) => ({ ...d, color: randomHexColor() }))} className="h-7 px-2 rounded-ctl border border-rule text-[12px] text-dim hover:text-ink hover:border-dim inline-flex items-center gap-1.5"><svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><rect x="3" y="3" width="18" height="18" rx="3" /><circle cx="8.5" cy="8.5" r="1" fill="currentColor" /><circle cx="15.5" cy="15.5" r="1" fill="currentColor" /><circle cx="15.5" cy="8.5" r="1" fill="currentColor" /><circle cx="8.5" cy="15.5" r="1" fill="currentColor" /></svg>random</button>
        </div>
        <div className="flex items-center gap-2 flex-wrap">
          <input className="box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[13px] text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30 placeholder:text-faint w-[180px]" placeholder="label name" value={draft.name} onChange={(e) => setDraft((d) => ({ ...d, name: e.target.value }))} onKeyDown={(e) => { if (e.key === "Enter") add(); }} />
          {draft.name.trim() && <Label name={draft.name.trim()} color={draft.color} icon={draft.icon} />}
          <Button size="sm" className="ml-auto" disabled={!draft.name.trim()} onClick={add}>Add label</Button>
        </div>
      </div>
    </div>
  );
}

// Editor for a repo's code-owner rules (`glob → owners`), modeled on the default-reviewers editor.
// Holds a local draft synced from the server set; Save persists the whole rule set at once (the
// endpoint replaces, not merges). An empty set shows an actionable "add a rule" affordance, not dead
// text.
type OwnerRule = { glob: string; owners: string[] };
function CodeOwnersEditor({ rules, actors, handleOf, onSave }: { rules: OwnerRule[]; actors: Actor[]; handleOf: (id: string) => string; onSave: (rules: OwnerRule[]) => Promise<void> }) {
  const [draft, setDraft] = useState<OwnerRule[]>(rules);
  const [newGlob, setNewGlob] = useState("");
  const [newOwner, setNewOwner] = useState("");
  const [saving, setSaving] = useState(false);
  // Re-sync when the server set changes (settings reload / repo switch).
  useEffect(() => { setDraft(rules); }, [rules]);
  const dirty = JSON.stringify(draft) !== JSON.stringify(rules);
  const setRow = (i: number, patch: Partial<OwnerRule>) => setDraft((d) => d.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  const addRow = () => {
    const g = newGlob.trim();
    if (!g) return;
    setDraft((d) => [...d, { glob: g, owners: newOwner ? [newOwner] : [] }]);
    setNewGlob(""); setNewOwner("");
  };
  const save = async () => { setSaving(true); try { await onSave(draft.map((r) => ({ glob: r.glob.trim(), owners: r.owners })).filter((r) => r.glob)); } finally { setSaving(false); } };
  const ownerCandidates = (row: OwnerRule) => actors.filter((a) => !row.owners.includes(a.id)).map(actorOption);
  return (
    <div className="grid gap-3">
      {draft.length === 0 && <span className="text-[12.5px] text-muted">No code-owner rules yet. Add one below to auto-notify owners when matching paths change.</span>}
      {draft.map((r, i) => (
        <div key={i} className="grid gap-2 border border-rule2 rounded-ctl p-3 bg-paper/40">
          <div className="flex items-center gap-2">
            <input className="box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-mono text-[12.5px] text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30 placeholder:text-faint flex-1 min-w-0" placeholder="path glob, e.g. crates/** or *.rs" value={r.glob} onChange={(e) => setRow(i, { glob: e.target.value })} />
            <button className="text-muted hover:text-fault-text cursor-pointer px-1 flex-none" title="remove rule" onClick={() => setDraft((d) => d.filter((_, j) => j !== i))}>×</button>
          </div>
          <div className="flex flex-wrap items-center gap-1.5">
            {r.owners.map((o) => (
              <span key={o} className="inline-flex items-center gap-1 text-xs px-2 py-1 rounded-chip bg-paper border border-rule">
                {handleOf(o)}
                <button className="text-muted hover:text-fault-text cursor-pointer" onClick={() => setRow(i, { owners: r.owners.filter((x) => x !== o) })}>×</button>
              </span>
            ))}
            {r.owners.length === 0 && <span className="text-[12px] text-faint">no owners</span>}
            <div className="max-w-[220px]"><Picker size="sm" value="" placeholder="Add an owner…" onChange={(v) => { if (v && !r.owners.includes(v)) setRow(i, { owners: [...r.owners, v] }); }} options={ownerCandidates(r)} /></div>
          </div>
        </div>
      ))}
      <div className="flex items-center gap-2 flex-wrap border-t border-rule3 pt-3">
        <input className="box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-mono text-[12.5px] text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30 placeholder:text-faint w-[220px]" placeholder="add a rule: path glob…" value={newGlob} onChange={(e) => setNewGlob(e.target.value)} onKeyDown={(e) => { if (e.key === "Enter") addRow(); }} />
        <div className="w-[200px]"><Picker size="sm" block value={newOwner} placeholder="owner (optional)…" onChange={setNewOwner} options={actors.map(actorOption)} /></div>
        <Button size="sm" variant="secondary" disabled={!newGlob.trim()} onClick={addRow}>Add rule</Button>
        <Button size="sm" className="ml-auto" disabled={!dirty || saving} onClick={save}>{saving ? "Saving…" : "Save code owners"}</Button>
      </div>
    </div>
  );
}

// Repo "Danger zone": rename (text field + confirm) and delete (typed-confirmation). Owner/admin only
// — the server enforces the same gate, this UI is just the front door. Both callbacks hit the API and
// navigate on success.
function DangerZone({ repo, onRename, onDelete }: { repo: string; onRename: (name: string) => Promise<void>; onDelete: () => Promise<void> }) {
  const [name, setName] = useState(repo);
  const [renaming, setRenaming] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  useEffect(() => { setName(repo); }, [repo]);
  const trimmed = name.trim();
  const doRename = async () => { if (!trimmed || trimmed === repo) return; setRenaming(true); try { await onRename(trimmed); } finally { setRenaming(false); } };
  return (
    <Card className="!border-fault/50">
      <div className="px-5 py-3.5 border-b border-fault/25 bg-fault/[0.04]">
        <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-fault-text">Danger zone</span>
      </div>
      <div className="px-5 py-4 grid gap-5">
        <div className="flex items-end justify-between gap-4 flex-wrap">
          <div className="min-w-[240px] flex-1">
            <div className="text-[13.5px] font-medium">Rename repository</div>
            <div className="text-[12.5px] text-muted mb-2">Changes the repo's URL and clone path. Existing voyages, issues, and reviews move with it.</div>
            <TextField value={name} onChange={(e: React.ChangeEvent<HTMLInputElement>) => setName(e.target.value)} placeholder="new name" className="max-w-[280px]" />
          </div>
          <Button size="sm" variant="secondary" disabled={renaming || !trimmed || trimmed === repo} onClick={doRename}>{renaming ? "Renaming…" : "Rename"}</Button>
        </div>
        <div className="flex items-center justify-between gap-4 flex-wrap border-t border-rule3 pt-4">
          <div className="min-w-[240px] flex-1">
            <div className="text-[13.5px] font-medium">Delete this repository</div>
            <div className="text-[12.5px] text-muted">Permanently removes the repo, its voyages, issues, reviews, comments, and hosted git store. This cannot be undone.</div>
          </div>
          <Button size="sm" variant="destructive" onClick={() => setConfirmDelete(true)}>Delete repository</Button>
        </div>
      </div>
      <ConfirmModal open={confirmDelete} onClose={() => setConfirmDelete(false)} title="Delete this repository?" body={<>This permanently deletes <b>{repo}</b> and everything in it. Type the repo name to confirm.</>} confirmId={repo} actionLabel="Delete forever" onConfirm={() => { setConfirmDelete(false); onDelete(); }} />
    </Card>
  );
}

// AI connections — lend an account/org's OpenAI, Claude, or OpenRouter access to Hull's AI functions.
// Connect several and toggle rotation; a repo's reviews use its org's connections (else the caller's).
function AiConnections({ accountId, authHeaders, scopeLabel }: { accountId: string; authHeaders: () => Record<string, string>; scopeLabel?: string }) {
  type Usage = { input_tokens: number; output_tokens: number; cost_micros: number; runs: number; updated_unix: number };
  type Conn = { id: string; provider: string; label: string; base_url: string; auth_kind: string; hint: string; token_expires_unix?: number | null; usage?: Usage };
  type AgentInfo = { kind: string; command: string; label: string; installed: boolean };
  const fmtTok = (n: number) => (n >= 1e6 ? (n / 1e6).toFixed(1) + "M" : n >= 1e3 ? (n / 1e3).toFixed(1) + "k" : String(n));
  const fmtExpiry = (unix: number) => new Date(unix * 1000).toLocaleDateString(undefined, { month: "short", year: "numeric" });
  const enc = encodeURIComponent;
  const [conns, setConns] = useState<Conn[]>([]);
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  const [rotate, setRotate] = useState(false);
  const [provider, setProvider] = useState("anthropic");
  const [label, setLabel] = useState("");
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [agentBusy, setAgentBusy] = useState("");
  // An in-flight per-user subscription login. Two shapes: paste-code (Claude — user copies a code off
  // the approval page and pastes it back) or device (Codex — user enters `userCode` on the site and we
  // poll for approval).
  const [login, setLogin] = useState<{ provider: string; label: string; session: string; url: string; needsCode: boolean; userCode: string | null } | null>(null);
  const [code, setCode] = useState("");
  const [waiting, setWaiting] = useState(false);
  const load = () => { apiGet(`/api/accounts/${enc(accountId)}/ai`, authHeaders()).then((r) => (r.ok ? r.json() : null)).then((d) => { if (d) { setConns(d.connections || []); setRotate(!!d.rotate); } }).catch(() => {}); };
  useEffect(load, [accountId]); // eslint-disable-line
  useEffect(() => { apiGet(`/api/ai/agents`, authHeaders()).then((r) => (r.ok ? r.json() : null)).then((d) => { if (d) setAgents(d.agents || []); }).catch(() => {}); }, [accountId]); // eslint-disable-line
  const post = (body: Record<string, unknown>) => apiPost(`/api/accounts/${enc(accountId)}/ai`, body, authHeaders());
  const postPath = (path: string, body: Record<string, unknown>) => apiPost(`/api/accounts/${enc(accountId)}/ai/${path}`, body, authHeaders());
  const add = async () => {
    if (!key.trim()) return;
    setBusy(true);
    const r = await post({ provider, label: label.trim(), api_key: key.trim() });
    setBusy(false);
    if (r.ok) { setKey(""); setLabel(""); load(); } else uiAlert(await r.text());
  };
  // Begin a per-user subscription login: provision the bundle + drive setup-token, then hand the user
  // the sign-in URL. The connection is only created once they paste the code (completeAgentLogin).
  const startAgentLogin = async (kind: string, agentLabel: string) => {
    setAgentBusy(kind);
    const r = await postPath("agent/start", { provider: kind });
    setAgentBusy("");
    if (!r.ok) { uiAlert(await r.text()); return; }
    const d = await r.json();
    setCode("");
    setLogin({ provider: kind, label: agentLabel, session: d.session, url: d.login_url, needsCode: !!d.needs_code, userCode: d.user_code ?? null });
  };
  // One completion attempt. Returns "done" | "pending" | "error" so both the paste button and the
  // device-flow poller can share it.
  const attemptComplete = async (): Promise<"done" | "pending" | "error"> => {
    if (!login) return "error";
    const r = await postPath("agent/complete", { provider: login.provider, session: login.session, code: code.trim() });
    if (r.status === 202) return "pending";
    if (r.ok) { setLogin(null); setCode(""); setWaiting(false); load(); return "done"; }
    uiAlert(await r.text());
    return "error";
  };
  const completeAgentLogin = async () => {
    if (!login || !code.trim()) return;
    setBusy(true);
    await attemptComplete();
    setBusy(false);
  };
  // Device flow (Codex): once the user has the code, poll for approval every few seconds.
  useEffect(() => {
    if (!login || login.needsCode || !waiting) return;
    let live = true;
    const tick = async () => {
      const s = await attemptComplete();
      if (!live) return;
      if (s === "pending") timer = window.setTimeout(tick, 3000);
      else setWaiting(false);
    };
    let timer = window.setTimeout(tick, 2000);
    return () => { live = false; window.clearTimeout(timer); };
  }, [login, waiting]); // eslint-disable-line
  const cancelAgentLogin = async () => {
    if (login) await postPath("agent/cancel", { session: login.session }).catch(() => {});
    setLogin(null); setCode(""); setWaiting(false);
  };
  const remove = async (id: string) => {
    if (!(await uiConfirm({ title: "Remove connection", body: "Disconnect this AI backend from the account?", danger: true, confirmLabel: "Remove" }))) return;
    const r = await apiDelete(`/api/accounts/${enc(accountId)}/ai/${enc(id)}`, authHeaders());
    if (r.ok) load(); else uiAlert(await r.text());
  };
  const toggleRotate = async (on: boolean) => { setRotate(on); await apiPut(`/api/accounts/${enc(accountId)}/ai/rotate`, { rotate: on }, authHeaders()).catch(() => setRotate(!on)); };
  const PROVIDERS = [{ value: "anthropic", label: "Claude (Anthropic)" }, { value: "openai", label: "OpenAI" }, { value: "openrouter", label: "OpenRouter" }];
  const dot = (c: Conn) => (c.auth_kind === "agent" ? "bg-clear" : c.provider === "anthropic" ? "bg-brass" : c.provider === "openai" ? "bg-clear" : "bg-steel");
  const installed = agents.filter((a) => a.installed);
  return (
    <div>
      <Eyebrow label="AI connections" right={conns.length > 1 ? <span className="inline-flex items-center gap-2 text-[12px] text-muted">rotate across them <Switch on={rotate} onChange={toggleRotate} /></span> : undefined} />
      <Card>
        <div className="px-5 py-4 grid gap-3">
          <p className="text-[12.5px] text-muted leading-[1.5]">Power AI reviews, fixes, triage, and “ask agent” for {scopeLabel ?? "this account"}. Connect a coding agent to run on <b>your own Claude / ChatGPT subscription</b>, or add an API key. Connect several and turn on rotate to spread load; without any, Hull uses its built-in reconciliation reviewer.</p>
          {conns.length > 0 && (
            <div className="grid gap-1.5">
              {conns.map((c) => {
                const u = c.usage;
                const exp = c.token_expires_unix;
                const soon = exp ? exp * 1000 - Date.now() < 30 * 86400 * 1000 : false;
                return (
                <div key={c.id} className="px-3 py-2 rounded-ctl border border-rule2 bg-paper/40">
                  <div className="flex items-center gap-2.5">
                    <span className={`w-2 h-2 rounded-full flex-none ${dot(c)}`} />
                    <span className="text-[13.5px] font-medium truncate">{c.label}</span>
                    <span className="text-[11.5px] text-faint">{c.auth_kind === "agent" ? <>{c.provider} · {c.hint}</> : <>{c.provider} · key {c.hint}</>}</span>
                    <button onClick={() => remove(c.id)} className="ml-auto text-[12px] text-muted hover:text-fault-text">Remove</button>
                  </div>
                  {(!!(u && u.runs > 0) || !!exp) && (
                    <div className="flex flex-wrap items-center gap-x-3 gap-y-0.5 mt-1 pl-[18px] text-[11px] text-faint tabular-nums">
                      {u && u.runs > 0 && (
                        <span title={`${u.input_tokens.toLocaleString()} in · ${u.output_tokens.toLocaleString()} out over ${u.runs} run(s)`}>
                          ↑ {fmtTok(u.input_tokens)} in · ↓ {fmtTok(u.output_tokens)} out · {u.runs} run{u.runs === 1 ? "" : "s"}{u.cost_micros > 0 ? ` · $${(u.cost_micros / 1e6).toFixed(2)}` : ""}
                        </span>
                      )}
                      {exp && <span className={soon ? "text-fault-text" : ""}>token expires {fmtExpiry(exp)}{soon ? " — reconnect soon" : ""}</span>}
                    </div>
                  )}
                </div>
                );
              })}
            </div>
          )}
          {installed.length > 0 && !login && (
            <div className="grid gap-2 pt-1 border-t border-rule3">
              <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-faint">Connect a subscription</span>
              <div className="flex flex-wrap gap-2">
                {installed.map((a) => (
                  <Button key={a.kind} size="sm" variant="secondary" disabled={!!agentBusy} onClick={() => startAgentLogin(a.kind, a.label)}>
                    {agentBusy === a.kind ? "Starting…" : `Connect ${a.label}`}
                  </Button>
                ))}
              </div>
              <p className="text-[11.5px] text-faint leading-[1.5]">Sign in with your own Claude / ChatGPT subscription. The agent runs under your login — no API key, and Hull never sees your token (only the CLI’s own credentials, kept for this account).</p>
            </div>
          )}
          {login && (
            <div className="grid gap-3 pt-3 border-t border-rule3">
              <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-faint">Sign in to {login.label}</span>
              {login.needsCode ? (
                // Paste-code flow (Claude): approve, copy the code, paste it back.
                <ol className="grid gap-2.5 text-[13px] text-body">
                  <li className="flex items-center gap-2.5">
                    <span className="w-5 h-5 flex-none grid place-items-center rounded-full bg-rule2 text-[11px] font-semibold text-muted tabular-nums">1</span>
                    <a href={login.url} target="_blank" rel="noreferrer"><Button size="sm">Open sign-in page ↗</Button></a>
                    <span className="text-[12px] text-muted">approve access, then copy the code shown</span>
                  </li>
                  <li className="flex items-center gap-2.5">
                    <span className="w-5 h-5 flex-none grid place-items-center rounded-full bg-rule2 text-[11px] font-semibold text-muted tabular-nums">2</span>
                    <input value={code} onChange={(e) => setCode(e.target.value)} placeholder="paste code here" autoFocus className="box-border flex-1 min-w-[200px] h-ctl-sm px-2.5 rounded-ctl-sm border border-ctl bg-surface text-[13px] text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30 placeholder:text-faint" onKeyDown={(e) => { if (e.key === "Enter") completeAgentLogin(); }} />
                    <Button size="sm" disabled={!code.trim() || busy} onClick={completeAgentLogin}>{busy ? "Verifying…" : "Finish"}</Button>
                    <button onClick={cancelAgentLogin} className="text-[12px] text-muted hover:text-fault-text">Cancel</button>
                  </li>
                </ol>
              ) : (
                // Device flow (Codex): enter the code on the site; we poll for approval.
                <ol className="grid gap-2.5 text-[13px] text-body">
                  <li className="flex items-center gap-2.5">
                    <span className="w-5 h-5 flex-none grid place-items-center rounded-full bg-rule2 text-[11px] font-semibold text-muted tabular-nums">1</span>
                    <a href={login.url} target="_blank" rel="noreferrer"><Button size="sm">Open sign-in page ↗</Button></a>
                    {login.userCode && <span className="text-[12px] text-muted">and enter code <code className="px-1.5 py-0.5 rounded bg-rule2 text-ink font-semibold tracking-[0.06em] select-all">{login.userCode}</code></span>}
                  </li>
                  <li className="flex items-center gap-2.5">
                    <span className="w-5 h-5 flex-none grid place-items-center rounded-full bg-rule2 text-[11px] font-semibold text-muted tabular-nums">2</span>
                    {waiting ? (
                      <span className="text-[12.5px] text-muted inline-flex items-center gap-2"><span className="w-3.5 h-3.5 rounded-full border-2 border-rule2 border-t-body animate-spin" /> Waiting for you to approve…</span>
                    ) : (
                      <Button size="sm" onClick={() => setWaiting(true)}>I’ve entered the code</Button>
                    )}
                    <button onClick={cancelAgentLogin} className="text-[12px] text-muted hover:text-fault-text ml-1">Cancel</button>
                  </li>
                </ol>
              )}
            </div>
          )}
          <div className="grid gap-2 pt-1 border-t border-rule3">
            <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-faint">Or add an API key</span>
            <div className="flex flex-wrap items-end gap-2">
              <div className="w-[180px]"><Picker size="sm" block value={provider} onChange={setProvider} options={PROVIDERS} /></div>
              <input value={label} onChange={(e) => setLabel(e.target.value)} placeholder="label (optional)" className="box-border h-ctl-sm px-2.5 rounded-ctl-sm border border-ctl bg-surface text-[13px] text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30 placeholder:text-faint w-[160px]" />
              <input value={key} onChange={(e) => setKey(e.target.value)} type="password" placeholder="API key" className="box-border flex-1 min-w-[180px] h-ctl-sm px-2.5 rounded-ctl-sm border border-ctl bg-surface text-[13px] text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30 placeholder:text-faint" />
              <Button size="sm" disabled={!key.trim() || busy} onClick={add}>{busy ? "Connecting…" : "Connect"}</Button>
            </div>
          </div>
        </div>
      </Card>
    </div>
  );
}

// ── small token-only layout atoms (not controls — controls come from ./ui) ──────────
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


// Centered modal shell (backdrop + card + header). Closes on backdrop click / ✕ / Escape.
function ModalShell({ title, onClose, children, width = 480 }: { title: string; onClose: () => void; children: React.ReactNode; width?: number }) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);
  return (
    <>
      <div aria-hidden="true" className="fixed inset-0 z-40 bg-[rgba(0,0,0,0.68)] animate-bd-in" onClick={onClose} />
      <div role="dialog" aria-modal="true" style={{ width }} className="fixed left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 z-50 max-w-[93vw] max-h-[88vh] overflow-auto bg-surface border border-rule rounded-card shadow-modal animate-ov-in">
        <div className="flex items-center justify-between px-5 py-3.5 border-b border-rule2 sticky top-0 bg-surface">
          <h2 className="text-[15px] font-semibold">{title}</h2>
          <button onClick={onClose} className="w-6 h-6 grid place-items-center rounded-ctl text-muted hover:text-ink hover:bg-paper" aria-label="close"><IcoX size={15} /></button>
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
const modalInput = "w-full box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[13.5px] text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30 placeholder:text-faint";

// New repository modal: owner/name shown inline (owner ∕ name), a live name-availability check like
// org handles, a Public/Unlisted/Private dropdown, and the default branch.
function NewRepoModal({ accounts, defaultAccount, onClose, onCreate }: { accounts: string[]; defaultAccount?: string; onClose: () => void; onCreate: (p: { account: string; name: string; visibility: "public" | "private" | "unlisted"; branch: string }) => Promise<boolean> }) {
  // Default the owner to the org whose page you launched from, else your first account.
  const [account, setAccount] = useState((defaultAccount && accounts.includes(defaultAccount) ? defaultAccount : accounts[0]) ?? "");
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
              <span className="inline-flex items-center gap-1.5">{checking ? "checking…" : avail ? <><IcoCheck size={12} />{`${account}/${name.trim()} is available`}</> : <><IcoX size={12} />{`${account}/${name.trim()} is taken`}</>}</span>
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
          <input value={q} onChange={(e) => setQ(e.target.value)} placeholder={placeholder} className="w-full box-border h-ctl-sm pl-7 pr-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[12.5px] text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30 placeholder:text-faint" />
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

  // Theme: dark-first (the situation-room shell is designed for it), light via the toggle. A stored
  // preference always wins; new visitors get dark. [data-theme] on <html>. Persisted.
  const [theme, setTheme] = useState<string>(
    () => localStorage.getItem("hull_theme") || "dark",
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
  const [notifs, setNotifs] = useState<{ kind: string; to: string[]; summary: string; ts: number; broadcast?: boolean; repo?: string | null; target_kind?: string | null; target_number?: number | null }[]>([]);
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
      uiAlert(await apiError(res));
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
        uiAlert(await apiError(res));
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
  // Expose signOut to the module-level `apiError` so a mid-session token expiry (a 401 on any call)
  // drops us to the signed-out state and offers login, rather than stranding a half-authed UI.
  useEffect(() => {
    onUnauthorized = signOut;
    return () => {
      onUnauthorized = null;
    };
  });
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
      if (!res.ok) return uiAlert(await apiError(res));
      refresh();
      uiAlert(`Agent created, cryptographically delegated by you (Hull never saw this key).\n\nIts secret key — save it, the agent signs in with this:\n\n${bytesToHex(childSk)}`);
    } else {
      // Hosted account: Hull signs the delegation with your held key and returns the agent's secret.
      const res = await fetch("/api/actors", {
        method: "POST",
        headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
        body: JSON.stringify({ handle: handle.trim(), kind: "agent", scope }),
      });
      if (!res.ok) return uiAlert(await apiError(res));
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
  const [authPass, setAuthPass] = useState(""); // sovereign-account passphrase (never leaves the browser)
  const [sovereignMode, setSovereignMode] = useState(false); // toggle passkey ⇄ sovereign on the auth pages
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
      if (!start.ok) { setAuthError((await start.text()) || `request failed (${start.status}) — is the server running?`); return; }
      const { flow_id, options } = await start.json();
      const credential = await createPasskey(options);
      const fin = await fetch("/api/auth/register/finish", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ flow_id, credential }) });
      if (!fin.ok) { setAuthError((await fin.text()) || `request failed (${fin.status}) — is the server running?`); return; }
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
      if (!start.ok) { setAuthError((await start.text()) || `request failed (${start.status}) — is the server running?`); return; }
      const { flow_id, options } = await start.json();
      const credential = await getPasskey(options);
      const fin = await fetch("/api/auth/passkey/finish", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ flow_id, credential }) });
      if (!fin.ok) { setAuthError((await fin.text()) || `request failed (${fin.status}) — is the server running?`); return; }
      const { token: t } = await fin.json();
      finishSession(t);
      navigate("/");
    } catch (e: any) {
      setAuthError(e?.message || "passkey login was cancelled");
    } finally {
      setAuthBusy(false);
    }
  };
  // ── SOVEREIGN (non-custodial) accounts: the Ed25519 key is generated + kept in THIS browser, and
  // wrapped under the passphrase before it's sent. Hull stores only the public key + the encrypted
  // bundle and can never sign for you — the browser signs delegations itself. ──
  const signupSovereign = async () => {
    setAuthError("");
    const u = serverHandle(authForm.username);
    if (!u) { setAuthError("username is required"); return; }
    if (authPass.length < 10) { setAuthError("use a passphrase of at least 10 characters — it's the only thing protecting your key"); return; }
    setAuthBusy(true);
    try {
      const id = await generateIdentity();
      const wrapped = wrapSecret(id.secret, authPass); // Argon2id + XChaCha20, in-browser
      const signature = await signMessage(id.secret, `hull-sovereign:v1\nusername=${u}\npubkey=${id.pub}`);
      const res = await fetch("/api/auth/sovereign/register", {
        method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify({ username: u, email: authForm.email, pubkey: id.pub, wrapped_key: wrapped, signature }),
      });
      if (!res.ok) { setAuthError((await res.text()) || `request failed (${res.status}) — is the server running?`); return; }
      const { token: t } = await res.json();
      finishSession(t);
      sessionSecret.current = id.secret; // keep the key in memory so agent delegations can be signed this session
      setAuthForm({ username: "", email: "" }); setAuthPass("");
      navigate("/");
    } catch (e: any) {
      setAuthError(e?.message || "could not create the sovereign account");
    } finally {
      setAuthBusy(false);
    }
  };
  const loginSovereign = async () => {
    setAuthError("");
    const u = serverHandle(authForm.username);
    if (!u || !authPass) { setAuthError("username and passphrase are required"); return; }
    setAuthBusy(true);
    try {
      // fetch the (passphrase-protected) key bundle, decrypt it here, then sign the login challenge.
      const w = await fetch(`/api/auth/sovereign/wrapped?username=${encodeURIComponent(u)}`);
      if (!w.ok) { setAuthError("no sovereign account with that username"); return; }
      const { actor, wrapped_key } = await w.json();
      let secret: string;
      try { secret = unwrapSecret(wrapped_key, authPass); } catch { setAuthError("wrong passphrase"); return; }
      const ch = await fetch("/api/auth/challenge");
      const { nonce } = await ch.json();
      const signature = await signMessage(secret, `hull-login:${nonce}`);
      const res = await fetch("/api/auth/login", {
        method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify({ actor, nonce, signature }),
      });
      if (!res.ok) { setAuthError((await res.text()) || `login failed (${res.status})`); return; }
      const { token: t } = await res.json();
      finishSession(t);
      sessionSecret.current = secret;
      setAuthPass("");
      navigate("/");
    } catch (e: any) {
      setAuthError(e?.message || "sovereign login failed");
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
  type ProfileStats = { handle: string; bio: string; total: number; human_count: number; days: HeatDay[]; agents: { handle: string; count: number }[]; tokens?: { in: number; out: number; series: { day: number; in: number; out: number }[] } };
  const [profileStats, setProfileStats] = useState<ProfileStats | null>(null);
  const [profileRepoQ, setProfileRepoQ] = useState("");
  const [profileTab, setProfileTab] = useState<"overview" | "repos" | "orgs">("overview");
  const [profileReadme, setProfileReadme] = useState<string | null>(null); // README of <me>/<me>, if any
  const [bioDraft, setBioDraft] = useState<string | null>(null); // non-null = editing
  const loadProfile = () => { if (token) fetch("/api/profile", { headers: authHeaders() }).then((r) => (r.ok ? r.json() : null)).then(setProfileStats).catch(() => {}); };
  // The org page mirrors the user profile (banner, Overview/Repos tabs, contributions + tokens). It's
  // public, so these load for any visitor.
  type OrgStats = { handle: string; members: number; repos: number; repo_names: string[]; total: number; days: HeatDay[]; contributors: { handle: string; count: number; agent: boolean }[]; tokens?: { in: number; out: number; series: { day: number; in: number; out: number }[] } };
  const [orgStats, setOrgStats] = useState<OrgStats | null>(null);
  const [orgTab, setOrgTab] = useState<"overview" | "repos" | "people">("overview");
  const [orgReadme, setOrgReadme] = useState<string | null>(null);
  const [orgRepoQ, setOrgRepoQ] = useState("");
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
    if (!res.ok) return uiAlert(await apiError(res));
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
    if (!res.ok) return uiAlert(await apiError(res));
    loadAccount();
  };

  // ── org management (members + teams) + repo settings ──────────────────────
  const [orgHandle, setOrgHandle] = useState<string | null>(() => parseRoute(location.pathname).orgHandle);
  useEffect(() => {
    if (!orgHandle) { setOrgStats(null); setOrgReadme(null); return; }
    setOrgTab("overview");
    fetch(`/api/orgs/${encodeURIComponent(orgHandle)}/profile`).then((r) => (r.ok ? r.json() : null)).then(setOrgStats).catch(() => {});
    fetch(`/api/repos/${encodeURIComponent(orgHandle)}/${encodeURIComponent(orgHandle)}/blob?path=README.md`, { headers: authHeaders() })
      .then((r) => (r.ok ? r.json() : null)).then((d) => setOrgReadme(d && !d.missing && !d.binary ? (d.text || "") : null)).catch(() => setOrgReadme(null));
    /* eslint-disable-next-line */
  }, [orgHandle]);
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
    if (!res.ok) { uiAlert(await apiError(res)); return false; }
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
    if (!res.ok) return uiAlert(await apiError(res));
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
    if (!res.ok) { uiAlert(await apiError(res)); return false; }
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
    if (!res.ok) { uiAlert(await apiError(res)); return false; }
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
    if (!res.ok) return uiAlert(await apiError(res));
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
      if (!res.ok) { uiAlert(await apiError(res)); return; }
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
    apiGet(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/settings`, authHeaders()).then((r) => (r.ok ? r.json() : null)).then((d) => d && setRepoSettings(d)).catch(() => {});
    apiGet(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/owners`, authHeaders()).then((r) => r.json()).then((d) => setOwnerRules(d.owners ?? [])).catch(() => {});
    if (orgAccountFor(tenant)) loadTeams(orgAccountFor(tenant)!.id);
  };
  const orgAccountFor = (handle: string) => accounts.find((a) => a.handle === handle);
  const saveRepoSettings = async (patch: Partial<{ private: boolean; visibility: "public" | "private" | "unlisted"; require_review_to_land: boolean; author_independence: boolean; default_reviewers: string[]; team_access: { team: string; role: string }[]; labels: RepoLabel[] }>) => {
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/settings`, { method: "PUT", headers: { "content-type": "application/json", authorization: `Bearer ${token}` }, body: JSON.stringify(patch) });
    if (!res.ok) return uiAlert(await apiError(res));
    setRepoSettings(await res.json());
    // A labels edit changes repoLabels (issue label colors + the assign-label picker), which is
    // otherwise only refetched on repo change — refresh it now so the UI isn't stale until navigation.
    if (tenant && issueRepo) {
      fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/labels`, { headers: authHeaders() })
        .then((r) => (r.ok ? r.json() : { labels: [] }))
        .then((d) => setRepoLabels(d.labels ?? []))
        .catch(() => {});
    }
  };
  // Persist the full code-owner rule set (the endpoint replaces, not merges).
  const saveOwners = async (rules: OwnerRule[]) => {
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/owners`, { method: "POST", headers: { "content-type": "application/json", ...authHeaders() }, body: JSON.stringify({ rules, actor: actingAs }) });
    if (!res.ok) return uiAlert(await apiError(res));
    const d = await res.json();
    setOwnerRules(d.owners ?? rules);
  };
  // Danger zone: rename re-keys the repo — hard-navigate to the new URL so no stale repo-name state
  // lingers. Delete removes it and returns to the owning org's page.
  const renameRepo = async (newName: string) => {
    const res = await apiPatch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}`, { name: newName }, authHeaders());
    if (!res.ok) { uiAlert(await apiError(res)); return; }
    const d = await res.json();
    window.location.href = `/${encodeURIComponent(tenant)}/${encodeURIComponent(d.name ?? newName)}/settings`;
  };
  const deleteRepo = async () => {
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}`, { method: "DELETE", headers: authHeaders() });
    if (!res.ok) { uiAlert(await apiError(res)); return; }
    navigate(`/orgs/${encodeURIComponent(tenant)}`);
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
  // Does a PR have an independent approval that would actually let it LAND — mirroring the server gate
  // (perform_merge): a non-author HUMAN approval always counts; a non-author AGENT approval counts only
  // when the repo's autonomy tier lets an agent stand in for a human (T2/T3). Counting any agent
  // approve here (the old bug) lit the Merge button for an approval the server then 409s at T0/T1.
  const independentlyApproved = (revs: Review[], author: string): boolean => {
    const approvals = revs.filter((r) => r.verdict === "approve" && r.reviewer !== author);
    const human = approvals.some((r) => kindOf(r.reviewer) === "human");
    const agent = approvals.some((r) => kindOf(r.reviewer) === "agent");
    const tierAllowsAgent = autonomy?.tier === "t2" || autonomy?.tier === "t3";
    return human || (agent && tierAllowsAgent);
  };
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

  // Notifications inbox for the authenticated actor (addressed-to-them + visible broadcasts). The
  // server derives the actor from the bearer token, so there's no client-supplied ?actor= to spoof;
  // with no token there's no inbox to show.
  useEffect(() => {
    if (!token) { setNotifs([]); return; }
    const load = () => {
      fetch("/api/notifications", { headers: authHeaders() })
        .then((r) => (r.ok ? r.json() : { notifications: [] }))
        .then((d) => setNotifs(d.notifications ?? []))
        .catch(() => {});
    };
    load();
    const t = setInterval(load, 4000);
    return () => clearInterval(t);
  }, [token]);

  // Two views: Home (situation room) and a focused Repo view with Issues / PRs tabs.
  const [view, setView] = useState<"home" | "repo">(() => parseRoute(location.pathname).view);
  const [tab, setTab] = useState<RepoTab>(() => parseRoute(location.pathname).tab);
  useEffect(() => { if (tab === "settings") loadRepoSettings(); }, [tab, tenant, issueRepo]);
  const [issueView, setIssueView] = useState<"list" | "board">("list");
  // Mobile-only: reveal the (desktop-always-visible) filter input via a small toggle.
  const [filterOpen, setFilterOpen] = useState(false);
  // Kanban drag-to-restatus: which issue is being dragged and which column it's hovering.
  const [dragIssue, setDragIssue] = useState<number | null>(null);
  const [dragOverCol, setDragOverCol] = useState<string | null>(null);
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
    apiGet(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/issues`, authHeaders())
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
    else uiAlert(await apiError(res));
  };
  // Inline issue title/body editing (author-only, mirrors comment editing). `editingIssue` is the
  // number of the issue whose words are being edited, plus the working title/body drafts.
  const [editingIssue, setEditingIssue] = useState<number | null>(null);
  const [issueTitleDraft, setIssueTitleDraft] = useState("");
  const [issueBodyDraft, setIssueBodyDraft] = useState("");
  const startEditIssue = (it: Issue) => { setEditingIssue(it.number); setIssueTitleDraft(it.title); setIssueBodyDraft(it.body); };
  const cancelEditIssue = () => { setEditingIssue(null); setIssueTitleDraft(""); setIssueBodyDraft(""); };
  // Drop any in-progress issue edit when the open issue/repo changes, so stale drafts can't be saved
  // onto a different issue (numbers repeat across repos).
  useEffect(() => { setEditingIssue(null); setIssueTitleDraft(""); setIssueBodyDraft(""); }, [openIssue, issueRepo]);
  const saveEditIssue = async (number: number) => {
    if (!canAct) return uiAlert("Sign in to act.");
    const title = issueTitleDraft.trim();
    if (!title) return uiAlert("Title must not be empty.");
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/issues/${number}`, {
      method: "PATCH",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ action: "edit", title, body: issueBodyDraft }),
    });
    if (res.ok) { cancelEditIssue(); loadIssues(); }
    else uiAlert(await apiError(res));
  };
  // Board column → the issue action that lands a card in it. Every column maps to a supported
  // transition (reopen, or close with a reason), so every column is a valid drop target.
  const boardColAction = (k: string): [string, Record<string, unknown>] | null =>
    k === "open" ? ["reopen", {}]
      : k === "completed" ? ["close", { reason: "completed" }]
      : k === "not_planned" ? ["close", { reason: "not_planned" }]
      : k === "cancelled" ? ["close", { reason: "cancelled" }]
      : k === "duplicate" ? ["close", { reason: "duplicate" }]
      : null;
  const dropIssueOnCol = (colKey: string) => {
    const num = dragIssue;
    setDragOverCol(null);
    setDragIssue(null);
    if (num == null) return;
    const it = issues.find((i) => i.number === num);
    if (!it) return;
    const cur = it.status.state === "open" ? "open" : it.status.reason;
    if (cur === colKey) return; // same column — no-op
    const m = boardColAction(colKey);
    if (m) issueAction(num, m[0], m[1]);
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
  const loadOrgDefaults = (acctId: string) => apiGet(`/api/accounts/${encodeURIComponent(acctId)}/repo-defaults`, authHeaders()).then((r) => (r.ok ? r.json() : null)).then((d) => d && setOrgDefaults(d)).catch(() => {});
  const saveOrgDefaults = async (acctId: string, patch: Partial<{ visibility: string; require_review_to_land: boolean; labels: RepoLabel[] }>) => {
    const res = await fetch(`/api/accounts/${encodeURIComponent(acctId)}/repo-defaults`, { method: "PUT", headers: { "content-type": "application/json", authorization: `Bearer ${token}` }, body: JSON.stringify(patch) });
    if (res.ok) setOrgDefaults(await res.json()); else uiAlert(await apiError(res));
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
    else uiAlert(await apiError(res));
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
    else uiAlert(await apiError(res));
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
    else uiAlert(await apiError(res));
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
        uiAlert(await apiError(res));
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
    else uiAlert(await apiError(res));
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
    else uiAlert(await apiError(res));
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
  // SSE can't send an auth header, so we first mint a short-lived ticket (authenticated POST) and pass
  // it on the EventSource URL; the server streams only the caller's member accounts. On any drop or
  // ticket expiry we re-mint and reconnect (this replaces EventSource's native reconnect, which would
  // reuse the stale ticket).
  useEffect(() => {
    if (myAccounts.length === 0) return;
    let es: EventSource | null = null;
    let closed = false;
    let retry: ReturnType<typeof setTimeout> | null = null;
    const open = async () => {
      if (closed) return;
      try {
        const r = await fetch("/api/feed/ticket", { method: "POST", headers: authHeaders() });
        if (!r.ok) throw new Error("ticket");
        const { ticket } = await r.json();
        if (closed) return;
        es = new EventSource(`/api/feed?accounts=${encodeURIComponent(myAccounts.join(","))}&ticket=${encodeURIComponent(ticket)}`);
        feedRef.current = es;
        es.onmessage = (m) => {
          try {
            const ev = JSON.parse(m.data) as ActivityEvent;
            if (ev.kind === "issue") loadIssues(); // reflect new issues live
          } catch {
            /* ignore keep-alives */
          }
        };
        es.onerror = () => {
          es?.close();
          if (!closed && !retry) retry = setTimeout(() => { retry = null; open(); }, 3000);
        };
      } catch {
        if (!closed && !retry) retry = setTimeout(() => { retry = null; open(); }, 3000);
      }
    };
    open();
    return () => {
      closed = true;
      if (retry) clearTimeout(retry);
      es?.close();
    };
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
      <div onClick={() => setShowShortcuts(false)} className="fixed inset-0 z-40 bg-[rgba(0,0,0,0.62)] animate-bd-in" />
      <div className="fixed left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 z-50 w-[440px] max-w-[92vw] bg-surface border border-rule rounded-card shadow-modal border border-rule animate-ov-in overflow-hidden">
        <div className="px-5 py-3.5 border-b border-rule2 flex items-center justify-between">
          <span className="text-[14.5px] font-semibold">Keyboard shortcuts</span>
          <button type="button" aria-label="Close" onClick={() => setShowShortcuts(false)} className="text-muted hover:text-ink"><IcoX size={16} /></button>
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
      {newRepoOpen && <NewRepoModal accounts={myAccounts} defaultAccount={orgHandle ?? undefined} onClose={() => setNewRepoOpen(false)} onCreate={doCreateRepo} />}
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

  // ── left nav sidebar (chrome) — GitLab-style: global nav lives here, the top bar keeps search + user.
  // Defined here (before the authPage short-circuit) so the post-auth pages reuse the exact same nav. ──
  const sideCls = (active: boolean) =>
    `w-full flex items-center gap-2.5 px-2.5 h-8 rounded-ctl text-[13.5px] transition-colors ${active ? "bg-surface text-ink font-medium" : "text-body hover:bg-surface hover:text-ink"}`;
  const SIco = ({ d }: { d: string }) => (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round" className="flex-none text-muted"><path d={d} /></svg>
  );
  const sidebar = (
    <aside className="w-[228px] shrink-0 bg-shell hidden md:flex flex-col">
      <nav className="flex-1 overflow-y-auto px-3 pt-3 grid gap-0.5 content-start">
        <div className="px-2.5 pt-1 pb-2 text-[15px] font-semibold tracking-tight">Your work</div>
        <button className={sideCls(view === "home" && !orgHandle && !authPage)} onClick={() => navigate("/")}>
          <SIco d="M3 10.5 12 3l9 7.5M5 9.5V20a1 1 0 0 0 1 1h4v-6h4v6h4a1 1 0 0 0 1-1V9.5" /><span>Home</span>
        </button>
        {me && (
          <button className={sideCls(authPage === "profile")} onClick={() => navigate("/me")}>
            <SIco d="M4 4h6a2 2 0 0 1 2 2v14a2 2 0 0 0-2-2H4zM20 4h-6a2 2 0 0 0-2 2v14a2 2 0 0 1 2-2h6z" /><span>Your repositories</span>
          </button>
        )}
        {me && (
          <>
            <div className="px-2.5 pt-4 pb-1.5 text-[10.5px] font-semibold uppercase tracking-[0.09em] text-faint">Organizations</div>
            {myAccounts.length === 0 && <div className="px-2.5 pb-1 text-[12.5px] text-faint">none yet</div>}
            {myAccounts.map((h) => (
              <button key={h} className={sideCls(!!orgHandle && orgHandle === h)} onClick={() => navigate(`/orgs/${encodeURIComponent(h)}`)}>
                <SIco d="M3 21h18M6 21V7l6-4 6 4v14M10 9h.01M14 9h.01M10 13h.01M14 13h.01M10 17h4" /><span className="truncate">{h}</span>
              </button>
            ))}
          </>
        )}
        {!me && (
          <button className={sideCls(false)} onClick={() => navigate("/login")}>
            <SIco d="M15 3h4a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2h-4M10 17l5-5-5-5M15 12H3" /><span>Log in</span>
          </button>
        )}
      </nav>
      <div className="px-3 py-3 border-t border-rule2 grid gap-0.5">
        <button className={sideCls(authPage === "account")} onClick={() => me ? navigate("/settings") : setTheme(theme === "dark" ? "light" : "dark")}>
          {me
            ? <SIco d="M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
            : theme === "dark" ? <SIco d="M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9z" /> : <SIco d="M12 3v2M12 19v2M5 5l1.5 1.5M17.5 17.5 19 19M3 12h2M19 12h2M5 19l1.5-1.5M17.5 6.5 19 5M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8z" />}
          <span>{me ? "Account settings" : theme === "dark" ? "Light mode" : "Dark mode"}</span>
        </button>
        {me && (
          <button className={sideCls(false)} onClick={() => setTheme(theme === "dark" ? "light" : "dark")}>
            {theme === "dark" ? <SIco d="M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9z" /> : <SIco d="M12 3v2M12 19v2M5 5l1.5 1.5M17.5 17.5 19 19M3 12h2M19 12h2M5 19l1.5-1.5M17.5 6.5 19 5M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8z" />}
            <span>{theme === "dark" ? "Light mode" : "Dark mode"}</span>
          </button>
        )}
        <a className={sideCls(false)} href="https://github.com/tankrap/hull" target="_blank" rel="noreferrer">
          <SIco d="M9 18c-4.5 1.5-4.5-2.5-6-3m12 6v-3.5c0-1 .1-1.4-.5-2 2.8-.3 5.5-1.4 5.5-6a4.6 4.6 0 0 0-1.3-3.2 4.3 4.3 0 0 0-.1-3.2s-1-.3-3.4 1.3a11.6 11.6 0 0 0-6 0C7.3 1.3 6.3 1.6 6.3 1.6a4.3 4.3 0 0 0-.1 3.2A4.6 4.6 0 0 0 5 8c0 4.6 2.7 5.7 5.5 6-.6.6-.6 1.2-.5 2V20" /><span>Docs</span>
        </a>
      </div>
    </aside>
  );

  if (authPage) {
    // Pre-auth (login / signup) stays a clean centered page; once signed in, profile + account
    // settings render inside the SAME sidebar shell as the rest of the app.
    const shell = (title: string, children: React.ReactNode, wide = false) => {
      const inner = (
        <div className={`mx-auto px-6 py-10 ${wide ? "max-w-[860px]" : "max-w-[560px]"}`}>
          {title && <h1 className="text-[24px] font-semibold tracking-tight mb-6">{title}</h1>}
          {children}
        </div>
      );
      if (me) {
        return (
          <div className="bg-shell h-dvh flex flex-col text-ink overflow-hidden">
            {uiModalNode}{cmdNode}{shortcutsNode}{createModalsNode}
            <header className="h-14 shrink-0 bg-shell flex items-center gap-5 px-6">
              <button className="flex items-center gap-2.5 cursor-pointer shrink-0" onClick={() => navigate("/")}>
                <span className="w-[22px] h-[22px] rounded-chip bg-brass" aria-hidden />
                <span className="text-[19px] font-extrabold tracking-tight">hull</span>
              </button>
              <button onClick={() => setCmdOpen(true)} className="flex-1 max-w-[440px] mx-auto flex items-center gap-2 h-ctl px-2.5 rounded-ctl border border-ctl bg-surface hover:border-dim transition-colors cursor-pointer text-left">
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="text-muted flex-none"><circle cx="11" cy="11" r="8" /><line x1="21" y1="21" x2="16.65" y2="16.65" /></svg>
                <span className="flex-1 text-[13.5px] text-faint">Search or jump to…</span>
                <span className="text-[11px] font-semibold text-dim border border-rule rounded-[5px] px-[5px] py-0.5 bg-paper flex-none">⌘K</span>
              </button>
              <div className="w-[120px] shrink-0" />
            </header>
            <div className="flex flex-1 min-h-0">
              {sidebar}
              <main className="flex-1 min-w-0 overflow-y-auto overflow-x-clip bg-paper rounded-tl-[16px] border-l border-t border-rule2/70">
                {inner}
              </main>
            </div>
          </div>
        );
      }
      return (
      <div className="bg-shell min-h-screen text-ink">
        {uiModalNode}
        {cmdNode}
        {shortcutsNode}
        {createModalsNode}

        <header className="h-14 border-b border-rule2 bg-shell flex items-center px-6">
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
    };
    const errBox = authError ? <div className="text-[13px] text-fault-text bg-fault-wash border border-fault/30 rounded-ctl px-3 py-2 mb-3">{authError}</div> : null;

    if (authPage === "signup") {
      return shell("Create your hull account", (
        <Card>
          <div className="px-6 py-6 grid gap-4">
            {errBox}
            <div className="grid gap-1.5">
              <label className="text-[12.5px] font-semibold text-body">username</label>
              <input className={`box-border h-ctl px-2.5 rounded-ctl border bg-surface font-sans text-[13.5px] text-ink outline-none placeholder:text-faint transition-colors ${usernameAvail && !usernameAvail.available ? "border-fault" : "border-ctl focus:border-steel focus:ring-2 focus:ring-steel/30"}`} placeholder="e.g. mira" value={authForm.username} onChange={(e) => setAuthForm({ ...authForm, username: sanitizeHandle(e.target.value) })} autoFocus />
              {authForm.username.trim() && usernameAvail && (
                <div className={`text-[12px] ${usernameAvail.available ? "text-clear-text" : "text-fault-text"}`}><span className="inline-flex items-center gap-1.5">{usernameAvail.available ? <><IcoCheck size={12} />{`${authForm.username} is available`}</> : <><IcoX size={12} />that username is taken</>}</span></div>
              )}
            </div>
            <div className="grid gap-1.5">
              <label className="text-[12.5px] font-semibold text-body">email</label>
              <input className="box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[13.5px] text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30 placeholder:text-faint" placeholder="you@example.com" value={authForm.email} onChange={(e) => setAuthForm({ ...authForm, email: e.target.value.trim() })} onKeyDown={(e) => e.key === "Enter" && signupPasskey()} />
            </div>
            {!sovereignMode ? (
              <>
                <Button disabled={authBusy || (!!usernameAvail && !usernameAvail.available)} onClick={signupPasskey}>{authBusy ? "waiting for passkey…" : "Create account with a passkey"}</Button>
                <p className="text-[12.5px] text-muted leading-[1.55]">No passwords. Your device (Touch ID, Windows Hello, a security key, or your phone) creates a passkey and that is your login. Hull holds your signing key so it can act for you after login.</p>
                <LinkButton onClick={() => { setAuthError(""); setSovereignMode(true); }}>Or create a sovereign account — you hold the key →</LinkButton>
              </>
            ) : (
              <>
                <div className="grid gap-1.5">
                  <label className="text-[12.5px] font-semibold text-body">passphrase</label>
                  <input type="password" className="box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[13.5px] text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30 placeholder:text-faint" placeholder="a strong passphrase you'll remember" value={authPass} onChange={(e) => setAuthPass(e.target.value)} onKeyDown={(e) => e.key === "Enter" && signupSovereign()} />
                </div>
                <Button disabled={authBusy || (!!usernameAvail && !usernameAvail.available)} onClick={signupSovereign}>{authBusy ? "generating your key…" : "Create sovereign account"}</Button>
                <p className="text-[12.5px] text-muted leading-[1.55]">Your Ed25519 key is generated in this browser and encrypted with your passphrase — Hull only ever stores the public key and the encrypted bundle, and can never sign for you. There is no reset: lose the passphrase and the account is unrecoverable.</p>
                <LinkButton onClick={() => { setAuthError(""); setSovereignMode(false); }}>← Back to passkey signup</LinkButton>
              </>
            )}
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
              <input className="box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[13.5px] text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30 placeholder:text-faint" placeholder="your username" value={authForm.username} onChange={(e) => setAuthForm({ ...authForm, username: sanitizeHandle(e.target.value) })} onKeyDown={(e) => e.key === "Enter" && loginPasskey(authForm.username)} autoFocus />
            </div>
            {!sovereignMode ? (
              <>
                <Button disabled={authBusy} onClick={() => loginPasskey(authForm.username)}>{authBusy ? "waiting for passkey…" : "Continue with a passkey"}</Button>
                <LinkButton onClick={() => { setAuthError(""); setSovereignMode(true); }}>Log in to a sovereign account (passphrase) →</LinkButton>
              </>
            ) : (
              <>
                <div className="grid gap-1.5">
                  <label className="text-[12.5px] font-semibold text-body">passphrase</label>
                  <input type="password" className="box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[13.5px] text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30 placeholder:text-faint" placeholder="your passphrase" value={authPass} onChange={(e) => setAuthPass(e.target.value)} onKeyDown={(e) => e.key === "Enter" && loginSovereign()} />
                </div>
                <Button disabled={authBusy} onClick={loginSovereign}>{authBusy ? "unlocking your key…" : "Log in with your passphrase"}</Button>
                <p className="text-[12.5px] text-muted leading-[1.55]">Your key is decrypted in this browser; Hull never sees your passphrase.</p>
                <LinkButton onClick={() => { setAuthError(""); setSovereignMode(false); }}>← Back to passkey login</LinkButton>
              </>
            )}
            <div className="text-[13px] text-muted pt-1 border-t border-rule2">New here? <LinkButton onClick={() => { setAuthError(""); navigate("/signup"); }}>Create an account</LinkButton></div>
            {import.meta.env.DEV && (
              <details className="text-[12.5px]">
                <summary className="text-muted cursor-pointer">Advanced: key login</summary>
                <div className="grid gap-2 mt-2.5">
                  <input type="password" className="box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[13px] text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30 placeholder:text-faint" placeholder="ed25519 secret key (hex)" value={secretInput} onChange={(e) => setSecretInput(e.target.value)} onKeyDown={(e) => e.key === "Enter" && signIn()} />
                  <div className="flex gap-2 items-center">
                    <Button size="sm" variant="secondary" onClick={signIn}>Sign in with key</Button>
                    <LinkButton onClick={registerAndSignIn}>new raw identity</LinkButton>
                    <LinkButton onClick={() => signInWith(DEMO_OWNER_SECRET)}>demo</LinkButton>
                  </div>
                </div>
              </details>
            )}
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
                    <textarea autoFocus value={bioDraft} onChange={(e) => setBioDraft(e.target.value.slice(0, 280))} rows={2} placeholder="Tell people what you work on…" className="w-full box-border px-2.5 py-2 rounded-ctl border border-ctl bg-surface font-sans text-[14px] text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30 resize-y" />
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
              <TokenKpis tokens={profileStats?.tokens} />
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
                        <span className="text-faint"><Ico size={11} path={<polyline points="9 6 15 12 9 18" />} /></span>
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
                {repos.length === 0 && (
                  <div className="py-8 text-[13px] text-muted">
                    No repositories yet.{me && <> <button className="text-steel-text hover:underline" onClick={() => setNewRepoOpen(true)}>Create a repository →</button></>}
                  </div>
                )}
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
                    <input className="box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[13.5px] text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30" value={account.username} onChange={(e) => setAccount({ ...account, username: e.target.value })} />
                  </div>
                  <div className="grid gap-1.5">
                    <label className="text-[12.5px] font-semibold text-body">email</label>
                    <input className="box-border h-ctl px-2.5 rounded-ctl border border-ctl bg-surface font-sans text-[13.5px] text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30" value={account.email} onChange={(e) => setAccount({ ...account, email: e.target.value })} />
                  </div>
                  <div><Button size="sm" onClick={() => saveAccount({ username: account.username, email: account.email })}>Save</Button></div>
                </div>
              </Card>
              {(() => {
                const pa = accounts.find((a) => a.kind === "personal" && me && a.members.some((m) => m.actor === me.id));
                return pa ? <AiConnections accountId={pa.id} authHeaders={authHeaders} scopeLabel="your own reviews" /> : null;
              })()}
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
    <header className="h-14 shrink-0 bg-shell flex items-center gap-5 px-6 z-30">
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

  // The inbox is a global per-actor feed across all repos. A notification now carries its own
  // `repo` + structured target, so each row can link to the exact repo+PR/issue it's about — never
  // guessing from the current view. Rows without a resolvable target render static (no false link).
  const notifHref = (n: (typeof notifs)[number]): string | null => {
    if (!n.repo || !n.target_kind || n.target_number == null) return null;
    const [tn, rp] = n.repo.split("/");
    if (!tn || !rp) return null;
    const base = `/${encodeURIComponent(tn)}/${encodeURIComponent(rp)}`;
    if (n.target_kind === "pr") return `${base}/voyages/${n.target_number}`;
    if (n.target_kind === "issue") return `${base}/issues/${n.target_number}`;
    return null;
  };
  const notifDrawer = (
    <Drawer open={showNotifs} onClose={() => setShowNotifs(false)} title={`inbox · ${handleOf(actingAs)}`}>
      {notifs.length === 0 && <div className="text-[13px] text-muted">nothing yet</div>}
      {notifs.slice(0, 20).map((n, i) => {
        const href = notifHref(n);
        const body = (
          <>
            <span className={`w-1.5 h-1.5 rounded-full mt-1.5 flex-none ${n.ts > seenTs ? "bg-steel" : "bg-rule"}`} />
            <div className="min-w-0">
              <div className="flex items-center gap-1.5">
                <span className="text-[12.5px] font-semibold text-body">{n.kind.replace(/_/g, " ")}</span>
                {n.broadcast && <Tag>team</Tag>}
                {n.repo && <span className="text-[11px] text-faint truncate">{n.repo}</span>}
              </div>
              <div className="text-[12.5px] text-muted mt-0.5">{n.summary}</div>
            </div>
          </>
        );
        return href ? (
          <button key={`${n.ts}-${n.kind}-${i}`} onClick={() => { setShowNotifs(false); navigate(href); }} className="w-full text-left flex items-start gap-2 py-2 border-b border-rule3 last:border-0 hover:bg-paper cursor-pointer">
            {body}
          </button>
        ) : (
          <div key={`${n.ts}-${n.kind}-${i}`} className="flex items-start gap-2 py-2 border-b border-rule3 last:border-0">
            {body}
          </div>
        );
      })}
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
    <div className="bg-shell h-dvh flex flex-col text-ink overflow-hidden">
      {uiModalNode}
      {cmdNode}
      {shortcutsNode}
      {createModalsNode}

      {topBar}
      {notifDrawer}

      {/* GitLab app-shell: lighter chrome (top bar + left sidebar) frames a darker content "well"
          that is inset and rounded at the inner corner and scrolls on its own. */}
      <div className="flex flex-1 min-h-0">
        {sidebar}
        <main className="flex-1 min-w-0 overflow-y-auto overflow-x-clip bg-paper rounded-tl-[16px] border-l border-t border-rule2/70">

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
          {/* Greeting header — GitLab "Your work" dashboard style. */}
          <div className="flex items-center gap-4 mb-7">
            <Avatar id={me.id} handle={me.handle} kind={me.kind} size={52} />
            <div className="min-w-0">
              <h1 className="text-[22px] font-semibold tracking-tight leading-tight truncate">{me.handle}</h1>
              <p className="text-[13.5px] text-muted mt-1">Hey there — here's the work across your organizations.</p>
            </div>
          </div>
          {/* Stat tiles — the signature GitLab dashboard row. */}
          <div className="grid grid-cols-2 lg:grid-cols-4 gap-4 mb-7">
            {[
              { label: "Reviews", n: homePrs.length, sub: "Pull requests waiting on you", to: "" },
              { label: "Issues", n: homeIssues.length, sub: "Assigned to you", to: "" },
              { label: "Repositories", n: activeRepos.length, sub: "Active right now", to: "/me" },
              { label: "Organizations", n: myAccounts.length, sub: "You're a member of", to: "/me" },
            ].map((t) => (
              <button key={t.label} onClick={() => t.to && navigate(t.to)} className="text-left bg-surface border border-rule rounded-card px-4 py-3.5 hover:border-dim transition-colors">
                <div className="text-[12.5px] text-muted">{t.label}</div>
                <div className="text-[30px] font-semibold tracking-tight mt-1.5 leading-none tabular-nums">{t.n}</div>
                <div className="text-[12.5px] text-body mt-1.5 truncate">{t.sub}</div>
              </button>
            ))}
          </div>

          <div className="grid grid-cols-1 lg:grid-cols-[1fr_340px] gap-x-12 gap-y-9">
            <section className="min-w-0 grid gap-8 content-start">
              {(homePrs.length > 0 || homeIssues.length > 0) && (
                <div className="bg-surface border border-rule rounded-card p-5">
                  <Eyebrow label="Needs your attention" right={`${homePrs.length + homeIssues.length}`} />
                  <div className="[&>button:last-child]:border-b-0">
                    {homePrs.map((p) => (
                      <button key={`pr-${p.tenant}/${p.repo}#${p.number}`} onClick={() => navigate(`/${encodeURIComponent(p.tenant)}/${encodeURIComponent(p.repo)}/voyages/${p.number}`)} className="group w-full text-left block border-b border-rule2">
                        <div className="flex items-start gap-3 py-3 -mx-3 px-3 rounded-ctl group-hover:bg-shell transition-colors">
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
                        <div className="flex items-start gap-3 py-3 -mx-3 px-3 rounded-ctl group-hover:bg-shell transition-colors">
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
              <div className="bg-surface border border-rule rounded-card p-5">
              <Eyebrow label="Active repositories" right="by live activity" />
              {activeRepos.length === 0 && (
                <div className="py-8 text-[13px] text-muted">
                  {repos.length === 0
                    ? <>No repos yet. Create one, import from GitHub, or <code className="text-body">git push http://localhost:8930/&lt;org&gt;/&lt;repo&gt; main</code>.</>
                    : <>No active repositories right now — <button className="text-steel-text hover:underline" onClick={() => navigate("/me")}>see all {repos.length} on your profile →</button></>}
                </div>
              )}
              <div className="[&>button:last-child>div]:border-b-0">
                {activeRepos.map((r) => (
                  <button key={`${r.tenant}/${r.repo}`} onClick={() => navigate(`/${encodeURIComponent(r.tenant)}/${encodeURIComponent(r.repo)}`)} className="group w-full text-left block">
                    <div className="flex items-start gap-4 py-4 -mx-3 px-3 rounded-ctl border-b border-rule2 group-hover:bg-shell transition-colors">
                      <div className="flex-1 min-w-0">
                        <div className="flex items-center gap-2.5">
                          <span className="text-[16px] font-medium group-hover:text-steel-text transition-colors"><span className="text-faint font-normal">{r.tenant}/</span>{r.repo}</span>
                          {r.active_actors.some((a) => kindOf(a) === "agent" || a.startsWith("agent")) && <StatusBadge kind="agent">agents active</StatusBadge>}
                        </div>
                        <div className="flex items-center gap-2 mt-2 text-[12.5px] text-muted min-w-0">
                          {r.active_actors.length > 0 ? (
                            <>
                              <span className="flex -space-x-1.5 flex-none">
                                {r.active_actors.slice(0, 4).map((a) => <span key={a} className="ring-2 ring-surface rounded-full group-hover:ring-shell transition-colors"><Avatar id={a} handle={actorLabel(a)} kind={kindOf(a)} size={18} /></span>)}
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
            <div className="group flex items-center gap-3 flex-wrap mb-1.5">
              <StatusBadge kind={it.status.state === "open" ? "running" : "queued"}>{it.status.state === "open" ? "open" : it.status.reason ?? "closed"}</StatusBadge>
              {editingIssue === it.number
                ? <input autoFocus value={issueTitleDraft} onChange={(e) => setIssueTitleDraft(e.target.value)} placeholder="Issue title" className={`${modalInput} flex-1 min-w-[240px] !text-[20px] !h-auto py-1.5 font-semibold tracking-tight`} />
                : <h1 className="text-[24px] font-semibold tracking-tight">{it.title}</h1>}
              {me?.id === it.author && editingIssue !== it.number && (
                <button onClick={() => startEditIssue(it)} title="Edit title & description" className="ml-auto inline-flex items-center gap-1 text-[12.5px] text-faint hover:text-steel-text transition-opacity opacity-0 group-hover:opacity-100">
                  <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M12 20h9" /><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z" /></svg>Edit
                </button>
              )}
            </div>
            <p className="text-[13px] text-muted mb-7">
              <span className="tabular-nums">#{it.number}</span> · opened by <b className={kindOf(it.author) === "agent" ? "text-steel-text" : "text-body"}>{handleOf(it.author)}</b>
              {it.edited_unix ? <span className="text-faint" title={`edited ${new Date(it.edited_unix * 1000).toLocaleString()}`}> · edited</span> : null}
            </p>
            <div className="grid grid-cols-1 lg:grid-cols-[1fr_300px] gap-x-12 gap-y-9">
              <section className="min-w-0 grid gap-6">
                {editingIssue === it.number ? (
                  <div className="grid gap-2">
                    <RichText value={issueBodyDraft} onChange={setIssueBodyDraft} rows={6} mentions={actors.map((a) => ({ handle: a.handle, kind: a.kind, email: a.email, avatar: <Avatar id={a.id} handle={a.handle} kind={a.kind} size={22} /> }))} onSubmit={() => saveEditIssue(it.number)} linkBase={`/${encodeURIComponent(tenant)}/${issueRepo}`} placeholder="Describe the issue…  (⌘↵ to save)" />
                    <div className="flex gap-2">
                      <Button size="sm" disabled={!issueTitleDraft.trim()} onClick={() => saveEditIssue(it.number)}>Save</Button>
                      <Button size="sm" variant="ghost" onClick={cancelEditIssue}>Cancel</Button>
                    </div>
                  </div>
                ) : it.body ? <Markdown text={it.body} linkBase={`/${encodeURIComponent(tenant)}/${issueRepo}`} className="text-[14px] text-body" /> : <p className="text-[13px] text-muted">no description</p>}
                {it.code_refs.length > 0 && (
                  <div className="grid gap-2">
                    <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted">Code references</span>
                    {it.code_refs.map((c) => {
                      const key = `${it.number}:${c.path}:${c.line_start}:${c.line_end}`;
                      return (
                        <div key={key} className="border border-rule2 rounded-ctl overflow-hidden">
                          <button className="w-full flex items-center gap-2 px-3 py-2.5 bg-paper hover:bg-surface transition-colors text-left" onClick={() => showWhy(key, c.path)}>
                            <code className="text-[12.5px] text-body flex-1">{c.path}:{c.line_start}{c.line_end ? `-${c.line_end}` : ""}</code>
                            <span className="text-[11.5px] text-steel-text" title={`content-addressed → keel blob ${c.blob}`}><IcoGit size={12} /> {c.blob.slice(0, 10)}</span>
                            <span className="text-[12px] text-muted"><span className="inline-flex items-center gap-1">{prov[key] ? "hide" : "provenance"}<Ico size={11} path={<polyline points="6 9 12 15 18 9" />} /></span></span>
                          </button>
                          {prov[key] && (
                            <div className="border-t border-rule3">
                              {prov[key].length === 0 && <div className="px-3 py-2 text-[12px] text-muted">no recorded history</div>}
                              {prov[key].map((p) => (
                                <div key={p.change} className="px-3 py-2 border-b border-rule3 last:border-0 flex gap-3 text-[12.5px]">
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
                  {it.resolved_by && <Stat k="resolved by" v={<code className="text-[12px] text-steel-text"><IcoGit size={12} /> {it.resolved_by.slice(0, 8)}</code>} />}
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
        const hasApproval = independentlyApproved(prReviews, p.author);
        const blockers = prReviews.reduce((n, r) => n + (r.findings ?? []).filter((f) => f.severity === "blocker").length, 0);
        // The merge gate must agree with the checklist below: an unresolved blocker blocks landing.
        const canLand = checksOk && hasApproval && !changesRequested && blockers === 0;
        const mergeGlyph = (
          <svg width="17" height="17" viewBox="0 0 16 16" fill="currentColor"><path d="M5.45 5.154A4.25 4.25 0 0 0 9.25 7.5h1.378a2.251 2.251 0 1 1 0 1.5H9.25A5.734 5.734 0 0 1 5 7.123v3.505a2.25 2.25 0 1 1-1.5 0V5.372a2.25 2.25 0 1 1 1.95-.218ZM4.25 13.5a.75.75 0 1 0 0-1.5.75.75 0 0 0 0 1.5Zm8.5-4.5a.75.75 0 1 0 0-1.5.75.75 0 0 0 0 1.5ZM5 3.25a.75.75 0 1 0-1.5 0 .75.75 0 0 0 1.5 0Z" /></svg>
        );
        // Just the merge ACTION — no checks box (the digest already spells out the gate + what's
        // blocking). Merged/closed collapse to a compact inline badge.
        const gate = p.state === "merged" ? (
          <span className="inline-flex items-center gap-1.5 text-[13px] font-semibold text-clear-text"><span className="w-5 h-5 rounded-full bg-clear text-white grid place-items-center">{mergeGlyph}</span>Merged · v{p.number}</span>
        ) : p.state === "closed" ? (
          <div className="inline-flex items-center gap-3 text-[13px] text-muted"><span className="font-medium">Closed without merging</span>{canAct && <Button size="sm" variant="secondary" onClick={() => closePr(p.number, true)}>Reopen</Button>}</div>
        ) : (
          <div className="flex items-center gap-2.5 flex-wrap justify-end">
            <Button disabled={!canLand} onClick={() => mergePr(p.number)} className={canLand ? "!bg-clear !border-clear !text-white font-semibold hover:!bg-[oklch(0.5_0.11_150)]" : ""}><span className="inline-flex items-center gap-1.5">{mergeGlyph}Merge</span></Button>
            {/* The tenant owner's override is ALWAYS available — never hidden by the gate — so a wedged
                or misconfigured check (or an agent-only approval the tier won't accept) can't trap them. */}
            {isTenantOwner && <Button size="sm" variant="secondary" onClick={() => mergePr(p.number, true)}>Merge without checks</Button>}
          </div>
        );
        // Secondary review actions (the primary comment/approve/request-changes flow lives in the
        // conversation composer's split button). Agent auto-review + request-a-reviewer only.
        const reviewTools = p.state === "open" ? (
          <div className="flex items-center gap-2 flex-wrap">
            {caps.ai_review && <Button size="sm" variant="secondary" disabled={autoReviewing === p.number} onClick={() => autoReview(p.number)}>{autoReviewing === p.number ? "agent reviewing…" : <span className="inline-flex items-center gap-1.5"><IcoGit size={13} />Agent auto-review</span>}</Button>}
            {canAct && (
              <Picker size="sm" width={220} placeholder="Request a reviewer…" value="" onChange={(v) => requestReviewer(p.number, v)}
                options={actors.filter((a) => a.id !== p.author && !p.reviewers?.includes(a.id)).map(actorOption)} />
            )}
          </div>
        ) : null;
        return (
          <ReviewPage
            key={p.number}
            review={primary}
            reviews={prReviews}
            landGate={gate}
            independentApproval={hasApproval}
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
            theme={theme}
            onBack={() => navigate(`${repoBase()}/voyages`)}
          />
        );
      })()}

      {/* ── ORG · members + teams ─────────────────────────────────────────── */}
      {orgHandle && (() => {
        const acct = orgAccount;
        // The org page is public: a member sees the full page (incl. People management); a signed-out
        // visitor still gets Overview + Repositories, rendered from the public org-profile endpoint.
        if (!acct && !orgStats) return <div className="max-w-[1180px] mx-auto px-6 sm:px-8 py-16 text-[13px] text-muted">no organization <b className="text-body">{orgHandle}</b></div>;
        const amAdmin = !!me && !!acct && acct.members.some((m) => m.actor === me.id && (m.role === "owner" || m.role === "admin"));
        const candidates = actors.filter((a) => !acct || !acct.members.some((m) => m.actor === a.id));
        const oHandle = acct?.handle ?? orgStats?.handle ?? (orgHandle ?? "");
        const oMembers = acct ? acct.members.length : (orgStats?.members ?? 0);
        const oRepos: string[] = acct ? acct.repos : (orgStats?.repo_names ?? []);
        return (
          <div className="grid gap-6">
            {/* Full-bleed banner — the org page is public and mirrors the user profile. */}
            <div className="relative left-1/2 right-1/2 -mx-[50vw] w-screen -mt-12 -z-0 h-[200px] bg-gradient-to-br from-brass-wash via-paper to-steel-wash pointer-events-none" aria-hidden />
            <div className="max-w-[1180px] mx-auto w-full px-6 sm:px-8 grid gap-6">
              <div className="-mt-[104px] relative">
                <div className="flex items-end gap-4">
                  <span className="rounded-2xl ring-4 ring-surface bg-surface shadow-modal"><Avatar id={acct?.id} handle={oHandle} kind="organization" size={104} /></span>
                  <div className="flex-1 min-w-0 pb-1">
                    <div className="text-[24px] font-semibold leading-tight">{oHandle}</div>
                    <div className="text-[13px] text-muted">organization · {oMembers} member{oMembers === 1 ? "" : "s"}{acct ? ` · ${teams.length} team${teams.length === 1 ? "" : "s"}` : ""} · {oRepos.length} repo{oRepos.length === 1 ? "" : "s"}</div>
                  </div>
                  {amAdmin && <button onClick={() => setNewRepoOpen(true)} className="text-[13px] text-steel-text hover:underline flex-none pb-1">+ New repo</button>}
                </div>
              </div>

              {/* Org tabs — same shape as the profile page. People is members-only. */}
              <div className="border-b border-rule2 -mt-1">
                <HTabs items={["Overview", `Repositories ${oRepos.length}`, ...(acct ? ["Settings"] : [])]} value={orgTab === "overview" ? 0 : orgTab === "repos" ? 1 : 2} onChange={(i: number) => setOrgTab(i === 0 ? "overview" : i === 1 ? "repos" : "people")} />
              </div>

              {orgTab === "overview" && (
                <div className="grid gap-6">
                  {orgReadme !== null && (
                    <Card>
                      <SectionHeader label={`${oHandle} / ${oHandle}`} right={<span className="text-[12px] text-muted">README.md</span>} />
                      <div className="px-6 py-5"><Markdown text={orgReadme || "_This README is empty._"} linkBase={`/${encodeURIComponent(oHandle)}/${encodeURIComponent(oHandle)}`} className="text-[14px] text-body leading-[1.6]" /></div>
                    </Card>
                  )}
                  <TokenKpis tokens={orgStats?.tokens} />
                  <Card>
                    <SectionHeader label={`${orgStats?.total ?? 0} contribution${(orgStats?.total ?? 0) === 1 ? "" : "s"} in the last year`} />
                    <div className="px-5 py-4">
                      <ContributionHeatmap days={orgStats?.days ?? []} />
                      {(orgStats?.contributors ?? []).length > 0 && (
                        <div className="mt-4 grid gap-1.5 pt-3 border-t border-rule3">
                          {(orgStats?.contributors ?? []).slice(0, 8).map((c) => (
                            <div key={c.handle} className="flex items-center gap-2 text-[14px]">
                              <Avatar handle={c.handle} kind={c.agent ? "agent" : "human"} size={18} />
                              <span className={c.agent ? "text-steel-text" : "font-medium"}>{c.handle}</span>
                              {c.agent && <span className="text-[11px] text-faint">agent</span>}
                              <span className="ml-auto tabular-nums text-muted">{c.count}</span>
                            </div>
                          ))}
                        </div>
                      )}
                    </div>
                  </Card>
                </div>
              )}

              {orgTab === "repos" && (() => {
                const oq = orgRepoQ.trim().toLowerCase();
                const list = oq ? oRepos.filter((rp: string) => rp.toLowerCase().includes(oq)) : oRepos;
                return (
                  <div className="grid gap-4">
                    <div className="flex justify-end"><div className="w-[260px]"><SearchInput placeholder="Find a repository" shortcut="" value={orgRepoQ} onChange={(e: React.ChangeEvent<HTMLInputElement>) => setOrgRepoQ(e.target.value)} /></div></div>
                    {oRepos.length === 0 && (
                      <div className="py-8 text-[13px] text-muted">
                        No repositories yet.{amAdmin && <> <button className="text-steel-text hover:underline" onClick={() => setNewRepoOpen(true)}>Create a repository →</button></>}
                      </div>
                    )}
                    <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4">
                      {list.map((rp: string) => (
                        <button key={rp} onClick={() => navigate(`/${encodeURIComponent(oHandle)}/${encodeURIComponent(rp)}`)} className="group text-left bg-surface border border-rule rounded-card p-4 hover:border-ctl hover:shadow-[0_2px_10px_-4px_rgba(15,23,42,0.12)] transition-all">
                          <div className="flex items-center gap-2 min-w-0">
                            <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor" className="text-muted flex-none"><path d="M2 2.5A2.5 2.5 0 0 1 4.5 0h8.75a.75.75 0 0 1 .75.75v12.5a.75.75 0 0 1-.75.75h-2.5a.75.75 0 0 1 0-1.5h1.75v-2h-8a1 1 0 0 0-.714 1.7.75.75 0 1 1-1.072 1.05A2.495 2.495 0 0 1 2 11.5Zm10.5-1h-8a1 1 0 0 0-1 1v6.708A2.486 2.486 0 0 1 4.5 9h8ZM5 12.25a.25.25 0 0 1 .25-.25h3.5a.25.25 0 0 1 .25.25v3.25a.25.25 0 0 1-.4.2l-1.45-1.087a.25.25 0 0 0-.3 0L5.4 15.7a.25.25 0 0 1-.4-.2Z" /></svg>
                            <span className="font-semibold text-[14.5px] truncate group-hover:text-steel-text transition-colors">{rp}</span>
                          </div>
                          <div className="text-[12px] text-faint mt-0.5 truncate">{oHandle}/{rp}</div>
                        </button>
                      ))}
                    </div>
                    {oq && list.length === 0 && <div className="py-6 text-[13px] text-muted">No repositories match “{orgRepoQ}”.</div>}
                    {amAdmin && <div className="pt-1"><LinkButton onClick={() => setNewRepoOpen(true)}>+ new repo</LinkButton></div>}
                  </div>
                );
              })()}

              {acct && orgTab === "people" && (
              <div className="grid grid-cols-1 lg:grid-cols-[1fr_320px] gap-x-12 gap-y-9">
              <section className="min-w-0 grid gap-8">
                {amAdmin && <AiConnections accountId={acct.id} authHeaders={authHeaders} scopeLabel={`${acct.handle}'s repos`} />}
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
                  <Eyebrow label="Teams" right={amAdmin ? <span className="flex gap-2 items-center"><input className="box-border h-ctl-sm px-2 rounded-ctl-sm border border-ctl bg-surface font-sans text-xs text-ink outline-none focus:border-steel focus:ring-2 focus:ring-steel/30 placeholder:text-faint" placeholder="new team…" value={teamDraft} onChange={(e) => setTeamDraft(e.target.value)} onKeyDown={(e) => { if (e.key === "Enter") { createTeam(acct.id, teamDraft); setTeamDraft(""); } }} /><Button size="sm" onClick={() => { createTeam(acct.id, teamDraft); setTeamDraft(""); }}>Create</Button></span> : undefined} />
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
                {!amAdmin && <Module title="Access"><span className="text-[12.5px] text-muted">You are viewing as a {me ? "member" : "guest"}. Owner/admin rights are needed to manage members and teams.</span></Module>}
                <Module title="Quick links"><button className="text-left text-[13.5px] text-steel-text hover:underline cursor-pointer" onClick={() => setOrgTab("repos")}>Browse repositories →</button></Module>
              </aside>
              </div>
              )}
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
                      <button className={`sm:hidden inline-flex items-center gap-1.5 px-2.5 py-1 rounded-ctl-sm border border-rule text-[13px] transition-colors ${filterOpen || q ? "text-ink border-ctl" : "text-muted hover:text-ink"}`} aria-expanded={filterOpen} onClick={() => setFilterOpen((v) => !v)}><IcoSearch size={13} />Filter</button>
                    </div>
                    <div className="flex items-center gap-2.5">
                      <Segmented items={["List", "Board"]} value={issueView === "list" ? 0 : 1} onChange={(i: number) => setIssueView(i === 0 ? "list" : "board")} />
                      {canAct && <Button size="sm" variant="ghost" className="inline-flex items-center gap-1.5" onClick={() => setNewIssueOpen(true)}><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><line x1="12" y1="5" x2="12" y2="19" /><line x1="5" y1="12" x2="19" y2="12" /></svg>New issue</Button>}
                    </div>
                  </div>
                  {filterOpen && <div className="sm:hidden -mt-3 mb-5"><SearchInput placeholder="Filter issues" shortcut="" value={q} onChange={(e: React.ChangeEvent<HTMLInputElement>) => setQ(e.target.value)} /></div>}
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
                                  {it.code_refs.length > 0 && <span className="text-[11.5px] text-steel-text"><IcoGit size={12} /> {it.code_refs.length}</span>}
                                  {it.resolved_by && <Tag><span className="inline-flex items-center gap-1"><IcoGit size={11} />resolved</span></Tag>}
                                  {!it.resolved_by && (it.linked_prs?.length ?? 0) > 0 && <Tag><span className="inline-flex items-center gap-1"><IcoGit size={10} />{`${it.linked_prs!.length} PR${it.linked_prs!.length > 1 ? "s" : ""}`}</span></Tag>}
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
                        // Empty non-open columns are hidden to avoid clutter, but stay
                        // rendered while a drag is in progress so they remain valid drop
                        // targets (e.g. dragging an open card onto Completed to close it).
                        if (col.k !== "open" && inCol.length === 0 && !dragIssue) return null;
                        // A card can be dropped here only when the viewer can change status and the
                        // column maps to a real transition (all current columns do).
                        const droppable = canAct && !!boardColAction(col.k);
                        return (
                          <div key={col.k} className={`grid gap-2 content-start rounded-ctl transition-colors ${dragOverCol === col.k && droppable ? "bg-steel-wash ring-1 ring-steel-text/40" : ""}`}
                            onDragOver={droppable ? (e) => { e.preventDefault(); e.dataTransfer.dropEffect = "move"; } : undefined}
                            onDragEnter={droppable ? () => setDragOverCol(col.k) : undefined}
                            onDragLeave={droppable ? (e) => { if (!e.currentTarget.contains(e.relatedTarget as Node)) setDragOverCol(null); } : undefined}
                            onDrop={droppable ? (e) => { e.preventDefault(); dropIssueOnCol(col.k); } : undefined}>
                            <div className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted flex gap-1.5 mb-1">{col.label} <span className="text-faint">{inCol.length}</span></div>
                            {inCol.map((it) => (
                              <button key={it.number} draggable={canAct}
                                onDragStart={canAct ? (e) => { e.dataTransfer.effectAllowed = "move"; setDragIssue(it.number); } : undefined}
                                onDragEnd={canAct ? () => { setDragIssue(null); setDragOverCol(null); } : undefined}
                                className={`text-left bg-surface border border-rule rounded-ctl p-3 cursor-pointer hover:border-ctl transition-colors ${canAct ? "active:cursor-grabbing" : ""} ${dragIssue === it.number ? "opacity-50" : ""}`} onClick={() => navigate(`${repoBase()}/issues/${it.number}`)}>
                                <div className="text-xs text-faint tabular-nums">#{it.number}</div>
                                <div className="text-[13.5px] font-medium mt-0.5 leading-snug">{it.title}</div>
                                {it.assignees.length > 0 && <div className="text-[11.5px] text-muted mt-1.5 inline-flex items-center gap-1.5"><Ico size={11} path={<><circle cx="12" cy="8" r="3.5" /><path d="M5 21a7 7 0 0 1 14 0" /></>} />{it.assignees.map((id) => handleOf(id)).join(", ")}</div>}
                              </button>
                            ))}
                            {inCol.length === 0 && dragIssue && droppable && (
                              <div className="text-[12px] text-muted border border-dashed border-rule rounded-ctl p-3 text-center pointer-events-none">Drop to mark {col.label}</div>
                            )}
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
                    <button className={`sm:hidden inline-flex items-center gap-1.5 px-2.5 py-1 rounded-ctl-sm border border-rule text-[13px] transition-colors ${filterOpen || q ? "text-ink border-ctl" : "text-muted hover:text-ink"}`} aria-expanded={filterOpen} onClick={() => setFilterOpen((v) => !v)}><IcoSearch size={13} />Filter</button>
                  </div>
                  {filterOpen && <div className="sm:hidden -mt-3 mb-5"><SearchInput placeholder="Filter pull requests" shortcut="" value={q} onChange={(e: React.ChangeEvent<HTMLInputElement>) => setQ(e.target.value)} /></div>}
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
                                  const vApprove = independentlyApproved(prReviews, p.author);
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
                                <span className="text-steel-text" title={`proposes keel change ${p.changes[0]}`}><IcoGit size={12} /> {(p.changes[0] ?? "").slice(0, 8)}</span>
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
            <RepoGraph tenant={tenant} repo={issueRepo} authHeaders={authHeaders} onOpenFile={(p, branch) => navigate(`${repoBase()}/files?path=${encodeURIComponent(p)}&branch=${encodeURIComponent(branch)}`)} />
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
                      <div key={ta.team} className="flex items-center gap-3">
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
                      {isTenantOwner && <div className="max-w-[160px]"><Picker block options={["t0", "t1", "t2", "t3"].map((t) => ({ value: t.toUpperCase(), label: t.toUpperCase() }))} value={autonomy.tier.toUpperCase()} onChange={(v: string) => setTier(v.toLowerCase())} /></div>}
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
                <div className="px-5 py-4">
                  <CodeOwnersEditor rules={ownerRules} actors={actors} handleOf={handleOf} onSave={saveOwners} />
                </div>
              </Card>
              <DangerZone repo={issueRepo} onRename={renameRepo} onDelete={deleteRepo} />
            </div>
            );
          })()}
        </div>
      )}
        </main>
      </div>
    </div>
  );
}
// A GitHub-style contribution heatmap: ~53 weeks × 7 days. Each cell is split into TWO triangles —
// top-left = your own contributions, bottom-right = your agents' — each shaded by its own intensity.
// Columns flex to fill the width, so there's never a scrollbar.


// Annotation payload carried on a pierre diff line: an AI finding, a human line comment, or the open
// composer. renderAnnotation switches on `kind` to draw the right thing inline in the diff.
type DiffAnno =
  | { kind: "finding"; kf: unknown }
  | { kind: "comment"; c: unknown }
  | { kind: "composer" };
type PierreAnno = { side: "additions" | "deletions"; lineNumber: number; metadata: DiffAnno };

// One file's diff, rendered by @pierre/diffs (Shiki, GitHub-style context collapse). Findings and
// comments ride inline as line annotations; hovering a line reveals a gutter "+" that opens a comment
// there. This is the whole diff surface — no bespoke hunk machinery on top of it.
function PierreReviewDiff({ patch, filePath, theme, lineAnnotations, renderAnnotation, loadFile, selectedLines }: {
  patch: string;
  filePath: string;
  theme: string;
  lineAnnotations: PierreAnno[];
  renderAnnotation: (a: PierreAnno) => React.ReactNode;
  loadFile?: () => Promise<{ old: string | null; new: string | null }>;
  selectedLines?: { start: number; end: number; side?: "additions" | "deletions" } | null;
}) {
  return (
    <Boundary fallback={<div className="px-4 py-4 text-[12.5px] text-muted rounded-ctl border border-rule2">Diff viewer unavailable — reload to retry.</div>}>
      <Suspense fallback={<div className="px-4 py-6 text-[13px] text-muted rounded-ctl border border-rule2">Loading diff…</div>}>
        <div data-review-path={filePath} className="text-[13px] rounded-ctl border border-rule2 overflow-hidden" style={{
          "--diffs-bg": "var(--surface)",
          "--diffs-fg-number": "var(--faint)",
          "--diffs-font-size": "12.5px",
          "--diffs-line-height": "1.7",
          "--diffs-min-number-column-width": "2.75rem",
        } as React.CSSProperties}>
          <PierrePatch patch={patch} disableWorkerPool
            lineAnnotations={lineAnnotations as never}
            renderAnnotation={renderAnnotation as never}
            selectedLines={(selectedLines ?? null) as never}
            options={{
              theme: { light: "github-light", dark: "github-dark" }, themeType: theme === "dark" ? "dark" : "light",
              // Unified (single-column, inline +/−) rather than split — far more room per line in the
              // review pane, and calmer to read than two cramped columns.
              diffStyle: "unified",
              overflow: "wrap", disableFileHeader: true, lineHoverHighlight: "line", tokenizeMaxLength: 400_000,
              // Line selection stays OFF so the code is normal selectable text — highlighting it (a real
              // browser selection) is what offers a comment (handled in ReviewPage's mouseup).
              // Clicking a "N unmodified lines" band fetches the full file so pierre can reveal the
              // hidden context (the patch only carries the hunks' few lines).
              ...(loadFile ? { loadDiffFiles: async () => {
                const { old, new: nw } = await loadFile();
                return { oldFile: old != null ? { name: filePath, contents: old } : null, newFile: nw != null ? { name: filePath, contents: nw } : null };
              } } : {}),
              // Override pierre's user-select:none on code so a reviewer can highlight code text.
              unsafeCSS: "[data-gutter]{background:var(--paper);border-right:1px solid var(--rule2)}[data-line-number-content]{padding-right:14px;opacity:.85}[data-code],[data-code] *{user-select:text!important;-webkit-user-select:text!important;cursor:text}",
            } as never} />
        </div>
      </Suspense>
    </Boundary>
  );
}

/** The review "package" — a dedicated page synthesizing what a reviewer needs, not a one-liner. */
function ReviewPage({
  review,
  reviews = [],
  landGate,
  independentApproval,
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
  theme,
  onBack,
}: {
  review: Review;
  reviews?: Review[];
  landGate?: React.ReactNode;
  independentApproval?: boolean;
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
  theme: string;
  onBack: () => void;
}) {
  const authHeaders = (): Record<string, string> => (token ? { authorization: `Bearer ${token}` } : {});
  const canAct = !!me;
  // Which verdict's lens we're viewing the package through (default: the primary review). The
  // synthesized package itself — diff, reconciliation, checks — is the page; verdicts are a strip.
  const [activeId, setActiveId] = useState<string | null>(null);
  // Default to the NEWEST review that carries a ledger — so after a fresh agent review/triage the page
  // reflects the current reconciliation (fewer claims), not a stale snapshot from an older review.
  const latestLedgered = [...reviews].filter((r) => r.ledger).sort((a, b) => (b.created_unix ?? 0) - (a.created_unix ?? 0))[0];
  const active = reviews.find((r) => r.id === activeId) ?? latestLedgered ?? review;
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
  // Always load the LIVE ledger — it reflects the current reconcile AND any resolutions (e.g. an
  // agent triage that just verified the intent claims). Prefer it over an older review's frozen
  // snapshot so "cut down by AI" actually shows up; fall back to the snapshot only if the live one
  // isn't available yet.
  const snapshot = active.ledger ?? null;
  const loadLedger = () => {
    if (!changeId) return;
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}/ledger`, { headers: authHeaders() })
      .then((r) => r.json())
      .then((d) => setLedger(d.ledger))
      .catch(() => {});
  };
  // Reconcile after verification is known, so a green/red signal is reflected in the claim statuses.
  useEffect(loadLedger, [changeId, tenant, repo, change?.verification]);
  const shownLedger = ledger ?? snapshot;

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
    else uiAlert(await apiError(res));
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
      else uiAlert(await apiError(res));
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
      else uiAlert(await apiError(res));
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
  // Prefer the parent's gate-accurate value (human/agent + tier); fall back to the plain heuristic
  // only if a caller didn't pass it, so this checklist row can't say "approved" when the gate won't land.
  const hasApproval = independentApproval ?? reviews.some((r) => r.verdict === "approve" && (!pr || r.reviewer !== pr.author));
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
  // Changes and Claims are independent detail panels that open in place; opening one never hides the
  // rest of the page (session, attention, conversation all stay put).
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
  type Cmt = { id: string; target: string; author: string; body: string; created_unix: number; path?: string; line?: number; edited_unix?: number };
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
    else uiAlert(await apiError(res));
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
      else uiAlert(await apiError(res));
    } finally { setComposerBusy(false); }
  };
  const closeOrReopenPr = async (reopen: boolean) => {
    if (!canAct || !pr) return uiAlert("Sign in to act.");
    if (!reopen && !(await uiConfirm({ title: "Close this pull request?", body: "It won't be merged. You can reopen it later from the pull requests list.", danger: true, confirmLabel: "Close pull request" }))) return;
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/prs/${pr.number}/close`, {
      method: "POST", headers: { "content-type": "application/json", ...authHeaders() }, body: JSON.stringify({ reopen }),
    });
    if (res.ok) { onReviewsChanged?.(); if (!reopen) onBack(); } else uiAlert(await apiError(res));
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
  // diffFocus[path] = the line a "What changed here" click selected; it becomes pierre's highlighted
  // `selectedLines` in that file's diff so the reader sees exactly which line the summary meant.
  const [diffFocus, setDiffFocus] = useState<Record<string, number>>({});
  // A file's full diff stays closed until you ask for it: click a "What changed" row (jumps to that
  // line) or "Show diff". Keeps the page light — never a wall of code you didn't open.
  const [openDiff, setOpenDiff] = useState<Set<string>>(() => new Set());
  // Cache each file's full old/new text (fetched for "expand unmodified lines") so re-expanding never
  // re-hits the network.
  const fileCache = useRef<Record<string, { old: string | null; new: string | null }>>({});
  // "What changed here" defaults to the handful of edits that carry the most signal; the long tail of
  // trivial token tweaks stays folded until you ask for it (per file).
  const [allEdits, setAllEdits] = useState<Set<string>>(() => new Set());
  // Open a file's diff and pinpoint-scroll to a line, highlighted. Pierre renders into a shadow DOM
  // and (for a just-opened diff) tokenizes async, so we poll briefly for the line's number cell.
  const scrollToDiffLine = (line: number, tries = 0) => {
    const host = document.querySelector("#changes-section ~ * diffs-container, diffs-container");
    const root = (host as HTMLElement & { shadowRoot?: ShadowRoot })?.shadowRoot;
    const cell = root && Array.from(root.querySelectorAll("[data-line-number-content]")).find((c) => (c.textContent ?? "").trim() === String(line));
    if (cell) { cell.scrollIntoView({ block: "center", behavior: "smooth" }); return; }
    if (tries < 12) setTimeout(() => scrollToDiffLine(line, tries + 1), 120);
    else document.getElementById("changes-section")?.scrollIntoView({ block: "start", behavior: "smooth" });
  };
  const revealDiff = (path: string, line?: number) => {
    setOpenDiff((s) => (s.has(path) ? s : new Set(s).add(path)));
    if (line != null) { setDiffFocus((f) => ({ ...f, [path]: line })); setTimeout(() => scrollToDiffLine(line), 80); }
    else setTimeout(() => document.getElementById("changes-section")?.scrollIntoView({ block: "start", behavior: "smooth" }), 60);
  };
  // Line-level review comments over a line OR a selected range of lines. Click a line number for one
  // line; drag to select a chunk, then "Comment on lines X–Y". A comment can be sent to an AI agent,
  // which reads the code around it and replies inline.
  // Highlighting code TEXT in the diff offers a comment. We map the selection's vertical span onto the
  // line-number cells (`data-column-number`) to get the line range, and anchor a small tooltip at the
  // selection. A plain click makes a collapsed selection, so nothing pops just from clicking.
  const [selRange, setSelRange] = useState<{ path: string; from: number; to: number; x: number; y: number } | null>(null);
  const [commenting, setCommenting] = useState<{ path: string; from: number; to: number } | null>(null);
  useEffect(() => {
    const onUp = (e: MouseEvent) => {
      if ((e.target as HTMLElement)?.closest?.("[data-cmt-tooltip]")) return; // don't clear on tooltip clicks
      const wrap = document.querySelector("[data-review-path]") as HTMLElement | null;
      const host = wrap?.querySelector("diffs-container") as (HTMLElement & { shadowRoot?: ShadowRoot }) | null;
      const root = host?.shadowRoot as (ShadowRoot & { getSelection?: () => Selection | null }) | undefined;
      if (!wrap || !root) return;
      const sel = root.getSelection?.() ?? window.getSelection();
      if (!sel || sel.rangeCount === 0 || sel.isCollapsed) return setSelRange(null);
      const rect = sel.getRangeAt(0).getBoundingClientRect();
      if (rect.width < 1 && rect.height < 1) return setSelRange(null);
      const within = (Array.from(root.querySelectorAll("[data-column-number]")) as HTMLElement[])
        .map((c) => ({ n: Number(c.getAttribute("data-column-number")), r: c.getBoundingClientRect() }))
        .filter((x) => Number.isFinite(x.n) && x.r.bottom > rect.top + 2 && x.r.top < rect.bottom - 2);
      if (!within.length) return setSelRange(null);
      const from = Math.min(...within.map((x) => x.n)), to = Math.max(...within.map((x) => x.n));
      setSelRange({ path: wrap.getAttribute("data-review-path") || "", from, to, x: Math.max(120, rect.left + rect.width / 2), y: Math.max(96, rect.top) });
    };
    document.addEventListener("mouseup", onUp);
    return () => document.removeEventListener("mouseup", onUp);
  }, []);
  const [lineDraft, setLineDraft] = useState("");
  const [askingAI, setAskingAI] = useState(false);
  // Inline comment editing: the id of the comment currently being edited, plus its working draft.
  const [editingComment, setEditingComment] = useState<string | null>(null);
  const [editDraft, setEditDraft] = useState("");
  const postLineComment = async (askAI = false) => {
    if (!canAct || !pr || !commenting || !lineDraft.trim()) return;
    if (askAI) setAskingAI(true);
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/comments`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ target: `pr:${pr.number}`, body: lineDraft.trim(), path: commenting.path, line: commenting.from, line_end: commenting.to, ask_ai: askAI }),
    });
    setAskingAI(false);
    if (res.ok) { setLineDraft(""); setCommenting(null); setSelRange(null); loadThread(); }
    else uiAlert(await apiError(res));
  };
  const openLineComment = (path: string, from: number, to?: number) => { const t = to ?? from; setCommenting({ path, from, to: t }); setLineDraft(""); };
  const deleteComment = async (id: string) => {
    if (!(await uiConfirm({ title: "Delete comment", body: "Delete this comment? This can't be undone.", danger: true, confirmLabel: "Delete" }))) return;
    const r = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/comments/${encodeURIComponent(id)}`, { method: "DELETE", headers: authHeaders() });
    if (r.ok) loadThread(); else uiAlert(await r.text());
  };
  const startEditComment = (c: Cmt) => { setEditingComment(c.id); setEditDraft(c.body); };
  const cancelEditComment = () => { setEditingComment(null); setEditDraft(""); };
  const saveEditComment = async (id: string) => {
    const body = editDraft.trim();
    if (!body) return;
    const r = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/comments/${encodeURIComponent(id)}`, {
      method: "PATCH",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ body }),
    });
    if (r.ok) { setEditingComment(null); setEditDraft(""); loadThread(); } else uiAlert(await r.text());
  };
  // Press "c" to comment on the currently-selected line(s) — the pierre selection drives it.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement;
      if ((e.key === "c" || e.key === "C") && selRange && !commenting && !/^(INPUT|TEXTAREA)$/.test(t.tagName)) { e.preventDefault(); openLineComment(selRange.path, selRange.from, selRange.to); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line
  }, [selRange, commenting]);
  // Line-anchored comments render in the diff; general comments render in the conversation.
  const lineCommentsByFile = new Map<string, Cmt[]>();
  for (const c of thread) { if (c.path && c.line) { const arr = lineCommentsByFile.get(c.path) ?? []; arr.push(c); lineCommentsByFile.set(c.path, arr); } }
  const kindOf = (id: string) => actors.find((a) => a.id === id)?.kind;


  return (
    <div className="bg-paper min-h-screen text-ink">
      {/* Highlight-to-comment tooltip — a small pill at the selection with a comment button (or press c).
          Only shows for an actual highlight (a drag), never a plain line click. */}
      {selRange && !commenting && (
        <div data-cmt-tooltip className="fixed z-[75] -translate-x-1/2 -translate-y-full animate-ov-in" style={{ left: selRange.x, top: selRange.y - 10 }}>
          <div className="flex items-center gap-1 rounded-ctl bg-ink text-surface shadow-modal pl-1 pr-1.5 py-1">
            <button onClick={() => openLineComment(selRange.path, selRange.from, selRange.to)} title={`Comment on line${selRange.to > selRange.from ? `s ${selRange.from}–${selRange.to}` : ` ${selRange.from}`}`}
              className="w-6 h-6 grid place-items-center rounded-ctl-sm hover:bg-surface/15 transition-colors">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /></svg>
            </button>
            <span className="text-[10.5px] font-medium opacity-70 pr-0.5">or press <kbd className="font-semibold">C</kbd></span>
          </div>
        </div>
      )}
      <header className="h-[52px] border-b border-rule2 bg-surface flex items-center gap-3 px-6 sticky top-14 z-20">
        <button className="flex items-center gap-1.5 text-[13px] font-medium text-dim hover:text-ink cursor-pointer flex-none" onClick={onBack}>
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><line x1="19" y1="12" x2="5" y2="12" /><polyline points="12 19 5 12 12 5" /></svg>
          <span className="hidden sm:inline">{repo} · pull requests</span>
        </button>
        {condensed ? (
          <div className="flex items-center gap-2.5 min-w-0 flex-1">
            <span className="text-[13.5px] font-semibold text-ink truncate">{pr ? pr.title : active.target}</span>
            {pr && <span className="text-[12px] text-faint tabular-nums flex-none">#{pr.number}</span>}
            {pr?.state === "merged" ? (
              <span className="ml-auto flex-none inline-flex items-center gap-1.5 text-[11.5px] font-semibold px-2 py-[3px] rounded-badge bg-clear-wash text-clear-text"><span className="w-1.5 h-1.5 rounded-full bg-clear" />Merged</span>
            ) : pr?.state === "closed" ? (
              <span className="ml-auto flex-none inline-flex items-center gap-1.5 text-[11.5px] font-semibold px-2 py-[3px] rounded-badge bg-rule2 text-dim"><span className="w-1.5 h-1.5 rounded-full bg-muted" />Closed</span>
            ) : (
              <span className={`ml-auto flex-none inline-flex items-center gap-1.5 text-[11.5px] font-semibold px-2 py-[3px] rounded-badge ${checksBad ? "bg-brass-wash text-brass-text" : checksPass === checks.length ? "bg-clear-wash text-clear-text" : "bg-brass-wash text-brass-text"}`}>
                <span className={`w-1.5 h-1.5 rounded-full ${checksBad ? "bg-brass" : checksPass === checks.length ? "bg-clear" : "bg-brass"}`} />
                {checksBad ? "Not ready" : checksPass === checks.length ? "Ready to merge" : "Awaiting review"} · {checksPass}/{checks.length}
              </span>
            )}
            {/* Merge travels up here on scroll, so it's always one click away. */}
            <span className="flex-none">{landGate}</span>
          </div>
        ) : (
          <span className="text-[11.5px] font-semibold px-[9px] py-[3px] rounded-full border border-rule text-dim">review package</span>
        )}
      </header>

      {/* Full-task overlay — the session summary shows a clipped task; this is the whole thing. */}
      {taskModal && change?.session && createPortal(
        <div className="fixed inset-0 z-[60] flex items-center justify-center p-4" onClick={() => setTaskModal(false)}>
          <div className="absolute inset-0 bg-[rgba(0,0,0,0.68)]" />
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
          <span className="text-dim font-semibold">−{delN}</span>
          <span className="text-faint">·</span>
          {/* Checks — clickable, opens the full checklist so the gate is reachable from the top */}
          <Popover align="left" width={296} trigger={(open) => (
            <span className={`inline-flex items-center gap-1.5 text-[11.5px] font-semibold px-2 py-[3px] rounded-badge transition-colors ${checksBad ? "bg-brass-wash text-brass-text" : checksPass === checks.length ? "bg-clear-wash text-clear-text" : "bg-brass-wash text-brass-text"} ${open ? "ring-2 ring-steel/25" : ""}`}>
              <span className={`w-1.5 h-1.5 rounded-full ${checksBad ? "bg-brass" : checksPass === checks.length ? "bg-clear" : "bg-brass"}`} />
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

        {/* One consistent 24px rhythm for every top-level section on the page. */}
        <div className="grid gap-6">
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
          const verdict = pr?.state === "merged" ? "Merged" : pr?.state === "closed" ? "Closed" : checksBad ? "Not ready to merge" : checks.length > 0 && checksPass === checks.length ? "Ready to merge" : "Awaiting review";
          const vTone = pr?.state === "merged" ? "text-clear-text" : pr?.state === "closed" ? "text-muted" : checksBad ? "text-ink" : verdict === "Ready to merge" ? "text-clear-text" : "text-brass-text";
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
                        <div className="text-[11px] font-bold uppercase tracking-[0.06em] text-brass-text">Blocking the merge</div>
                        {blocking.map((c, i) => {
                          const target = jumpTarget(c.label);
                          return (
                            <button key={i} disabled={!target} onClick={() => target && jumpTo(target)} className={`text-[13px] flex items-start gap-2 text-left text-body ${target ? "hover:text-ink cursor-pointer group" : "cursor-default"}`}>
                              <span className="w-1.5 h-1.5 rounded-full bg-brass mt-1.5 flex-none" />
                              <span><b>{c.label}</b> — {c.detail}{target && <span className="text-steel-text group-hover:underline"> ↓</span>}</span>
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
                  {/* Merge action lives here (not a box at the bottom); risk sits quietly beneath it. */}
                  <div className="flex flex-col items-end gap-2.5 flex-none">
                    {landGate}
                    <span className="inline-flex items-center gap-1.5 text-[11.5px] font-medium text-muted">
                      <span className={`w-1.5 h-1.5 rounded-full ${riskLevel === "low" ? "bg-clear" : "bg-brass"}`} />{riskLevel} risk
                    </span>
                  </div>
                </div>
              </div>
              {briefClaims.length > 0 && <Toggle open={showClaims} onClick={() => { const o = !showClaims; setShowClaims(o); if (o) setTimeout(() => document.getElementById("reconciliation-section")?.scrollIntoView({ behavior: "smooth", block: "start" }), 80); }}>{claimsLine}</Toggle>}
              {(diff.length > 0 || (semantic?.moves.length ?? 0) > 0) && <Toggle open={showChanges} onClick={() => { const o = !showChanges; setShowChanges(o); if (o) setTimeout(() => document.getElementById("changes-section")?.scrollIntoView({ behavior: "smooth", block: "start" }), 80); }}>Changes · {fileN} file{fileN === 1 ? "" : "s"} · <span className="text-dim tabular-nums">+{addN} −{delN}</span></Toggle>}
            </Card>
          );
        })()}

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
          const needsLeft = needs.length;
          const lbl = (icon: React.ReactNode, text: string) => <span className="inline-flex items-center gap-1.5">{icon}{text}</span>;
          return (
            <div id="needs-attention" className="scroll-mt-4">
            <Card className="mb-6">
              <div className="flex items-center justify-between gap-3 px-5 py-3 border-b border-rule">
                <div className="flex items-center gap-2.5">
                  <span className="text-brass-text"><IcoFlag size={15} /></span>
                  <span className="text-[13.5px] font-semibold text-ink">Needs your attention</span>
                  <span className="text-[12px] text-muted tabular-nums">{idx + 1} / {items.length}</span>
                </div>
                {canReview && onTriage && <Button size="sm" variant="secondary" disabled={triaging || !canAct} onClick={async () => { await onTriage(); loadResolutions(); loadLedger(); }}>{triaging ? "agent triaging…" : lbl(<IcoSearch size={13} />, "Let an agent triage")}</Button>}
              </div>
              <div className="h-[2px] bg-rule2"><div className="h-full bg-dim/50 transition-all" style={{ width: `${((idx + 1) / items.length) * 100}%` }} /></div>

              <div className="px-5 py-4 min-h-[128px]">
                {cur.kind === "finding" ? (
                  <>
                    <div className="text-[15.5px] font-medium text-ink leading-snug">{cur.row!.f.note}</div>
                    {cur.row!.f.path && <div className="text-[12.5px] text-muted mt-1 tabular-nums">{cur.row!.f.path}{cur.row!.f.line ? `:${cur.row!.f.line}` : ""}</div>}
                    <div className="flex items-center gap-2 mt-3.5 flex-wrap">
                      {canFix && pr && cur.row!.f.path ? <Button size="sm" disabled={!canAct || fixing === cur.row!.idx} onClick={() => fixWithAI(cur.row!.idx, cur.row!.f)}>{fixing === cur.row!.idx ? "fixing…" : lbl(<IcoSparkle size={13} />, "Fix with AI")}</Button>
                        : !canFix && <span className="text-[12px] text-muted">Set <code className="text-body">OPENROUTER_API_KEY</code> to auto-fix.</span>}
                    </div>
                  </>
                ) : (
                  <>
                    <div className="text-[15.5px] font-medium text-ink leading-snug">{cur.claim!.text}</div>
                    <div className={`text-[12.5px] mt-1.5 ${cur.kind === "needs" ? "text-muted" : "text-brass-text"}`}>
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

        <div className="grid gap-6">
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

          // The inline finding annotation shown right under its line in the pierre diff.
          const findingNote = (x: FindingRow) => {
            const { f, reviewer, idx } = x;
            const sevColor = f.severity === "blocker" ? "text-fault-text" : f.severity === "warn" ? "text-brass-text" : "text-steel-text";
            return (
              <div className="flex gap-2.5 px-4 py-3 bg-brass-wash/25 border-l-2 border-brass">
                <StatusDot tone={sevTone(f.severity)} size={16} />
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2 flex-wrap">
                    <span className={`text-[11px] font-bold uppercase tracking-[0.03em] ${sevColor}`}>{f.severity}</span>
                    <span className="inline-flex items-center gap-1 text-[11.5px] text-muted"><Avatar id={reviewer} handle={handleOf(reviewer)} kind={kindOf(reviewer)} size={14} />{handleOf(reviewer)}</span>
                  </div>
                  <p className="text-[13px] text-body mt-1 leading-snug">{f.note}</p>
                  {f.severity !== "info" && pr && f.path && canFix && (
                    <div className="mt-2"><Button size="sm" variant="secondary" disabled={!canAct || fixing === idx} onClick={() => fixWithAI(idx, f)}>{fixing === idx ? "fixing…" : <span className="inline-flex items-center gap-1.5"><IcoSparkle size={13} />Fix with AI</span>}</Button></div>
                  )}
                </div>
              </div>
            );
          };
          // A line-level review comment, shown under the line it references.
          const lineCommentNote = (c: Cmt) => (
            <div className="group/cmt flex gap-2.5 px-4 py-3 bg-surface">
              <Avatar id={c.author} handle={handleOf(c.author)} kind={kindOf(c.author)} size={22} />
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-2 text-[12.5px]">
                  <b className={kindOf(c.author) === "agent" ? "text-steel-text" : ""}>{handleOf(c.author)}</b>
                  <span className="text-faint tabular-nums" title={new Date(c.created_unix * 1000).toLocaleString()}>{timeAgo(c.created_unix)}</span>
                  <span className="text-[11px] text-faint">on line {c.line}</span>
                  {c.edited_unix ? <span className="text-[11px] text-faint" title={`edited ${new Date(c.edited_unix * 1000).toLocaleString()}`}>· edited</span> : null}
                  {me?.id === c.author && editingComment !== c.id && (
                    <span className="ml-auto flex items-center gap-1.5">
                      <button onClick={() => startEditComment(c)} title="Edit this comment" className="opacity-0 group-hover/cmt:opacity-100 text-faint hover:text-steel-text transition-opacity"><svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="M12 20h9" /><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z" /></svg></button>
                      <button onClick={() => deleteComment(c.id)} title="Delete this comment" className="opacity-0 group-hover/cmt:opacity-100 text-faint hover:text-fault-text transition-opacity"><svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><polyline points="3 6 5 6 21 6" /><path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" /></svg></button>
                    </span>
                  )}
                </div>
                {editingComment === c.id ? (
                  <div className="mt-1 grid gap-2">
                    <RichText value={editDraft} onChange={setEditDraft} rows={2} autoFocus minimal mentions={mentions} onSubmit={() => saveEditComment(c.id)} linkBase={`/${encodeURIComponent(tenant)}/${repo}`} placeholder="Edit your comment…  (⌘↵ to save)" />
                    <div className="flex gap-2">
                      <Button size="sm" disabled={!editDraft.trim()} onClick={() => saveEditComment(c.id)}>Save</Button>
                      <Button size="sm" variant="ghost" onClick={cancelEditComment}>Cancel</Button>
                    </div>
                  </div>
                ) : (
                  <Markdown text={c.body} linkBase={`/${encodeURIComponent(tenant)}/${repo}`} className="text-[13.5px] text-body mt-0.5" />
                )}
              </div>
            </div>
          );
          // The inline composer that opens when you comment on a line or a selected range. "Ask agent"
          // posts the comment and hands the code + your question to an AI agent, which replies inline.
          const composerNote = () => {
            const span = commenting && commenting.to > commenting.from ? `${commenting.from}–${commenting.to}` : `${commenting?.from}`;
            return (
              <div className="px-4 py-3 bg-steel-wash/50 grid gap-2">
                <div className="text-[11.5px] text-muted">Commenting on <b className="text-body">{commenting!.path.split("/").pop()}:{span}</b>{commenting!.to > commenting!.from ? <span className="text-faint"> · {commenting!.to - commenting!.from + 1} lines</span> : null}</div>
                <RichText value={lineDraft} onChange={setLineDraft} rows={2} autoFocus minimal mentions={mentions} onSubmit={() => postLineComment(false)} linkBase={`/${encodeURIComponent(tenant)}/${repo}`} placeholder="Comment on this code…  (⌘↵ to submit)" />
                <div className="flex gap-2 flex-wrap">
                  <Button size="sm" disabled={!lineDraft.trim() || askingAI} onClick={() => postLineComment(false)}>Comment</Button>
                  {canReview && <Button size="sm" variant="secondary" disabled={!lineDraft.trim() || askingAI} onClick={() => postLineComment(true)}><span className="inline-flex items-center gap-1.5"><IcoSparkle size={13} />{askingAI ? "Asking agent…" : "Comment & ask agent"}</span></Button>}
                  <Button size="sm" variant="ghost" onClick={() => { setCommenting(null); setSelRange(null); }}>Cancel</Button>
                </div>
              </div>
            );
          };

          // Grouped (semantic) ops: behavioral/changed files first, then renames, then reformatted.
          // Convert a file's hunks into a unified patch for @pierre/diffs: sign each line and compute
          // the hunk counts pierre's parser expects from the add/del tags.
          // When `focusLine` is given (a "What changed" jump), render ONLY the hunk that holds it —
          // the reviewer lands on the change and a few lines of context, not the whole file. The rest
          // is one click away ("show all changes"), and pierre's expand still fills in context.
          const hunkHasNewLine = (h: FileDiff["hunks"][number], line: number) => {
            let n = h.new_start;
            for (const l of h.lines) { if (l.tag !== "del") { if (n === line) return true; n++; } }
            return false;
          };
          const toPatch = (f: FileDiff, focusLine?: number): string => {
            const head = `diff --git a/${f.path} b/${f.path}\n--- a/${f.path}\n+++ b/${f.path}\n`;
            let hunks = f.hunks;
            if (focusLine != null) {
              const one = f.hunks.filter((h) => hunkHasNewLine(h, focusLine));
              if (one.length) hunks = one;
            }
            const body = hunks.map((h) => {
              const oldN = h.lines.filter((l) => l.tag !== "add").length;
              const newN = h.lines.filter((l) => l.tag !== "del").length;
              const rows = h.lines.map((l) => (l.tag === "add" ? "+" : l.tag === "del" ? "-" : " ") + l.text).join("\n");
              return `@@ -${h.old_start},${oldN} +${h.new_start},${newN} @@\n${rows}`;
            }).join("\n");
            return head + body + "\n";
          };
          const codeBody = (f: FileDiff) => {
            const fs = findingsByFile.get(f.path) ?? [];
            const lcs = lineCommentsByFile.get(f.path) ?? [];
            // Take the span from first-changed to last-changed token so inner spaces/punctuation are
            // preserved (otherwise "walk_all ()" → "slice ( task )" collapses to "slicetask").
            const span = (segs: Seg[]) => {
              const first = segs.findIndex((s) => s.changed);
              if (first === -1) return "";
              let last = segs.length - 1; while (last >= 0 && !segs[last].changed) last--;
              return segs.slice(first, last + 1).map((s) => s.text).join("").trim();
            };
            // A "What changed here" row is *noise* — structural churn a reviewer doesn't need called
            // out — when it's only brackets/operators, a bare markup tag (</div>, <div>, <br/>), a lone
            // keyword (return, else, break), a comment, or a stray import. A transform is dropped only
            // when BOTH its old and new side are noise, so a real edit is never hidden.
            const noise = (s: string) => {
              const t = s.trim();
              if (!t) return true;
              if (t.length <= 2) return true;                                  // }, ), ;, =>, {}
              if (/^[\s{}()[\]<>;,.:+\-|&?*=/`'"]+$/.test(t)) return true;      // punctuation/operators only
              if (/^<\/?[A-Za-z][\w.-]*\s*\/?>$/.test(t)) return true;         // a bare markup tag, no attributes
              if (/^<\/?>$/.test(t)) return true;                              // <> </> fragments
              if (/^[)\]}]+[;,]?$/.test(t) || /^[({[]+$/.test(t)) return true; // lone close/open runs
              if (/^(\/\/|\/\*|\*\/|\*|#|--|<!--|-->)/.test(t)) return true;   // comments
              if (/^(return|break|continue|else|then|do|end|fi|done|\}|\{)[\s;{}()]*$/.test(t)) return true; // bare keyword
              if (/^(use|import|from|require|pub use)\b/.test(t)) return true; // import churn — rarely the point
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
            // Inline annotations that ride the pierre diff: AI findings and human line comments at
            // their line, plus the open composer. renderAnno draws each with the existing note UIs.
            const annos: PierreAnno[] = [];
            for (const x of fs) if (x.f.line) annos.push({ side: "additions", lineNumber: x.f.line, metadata: { kind: "finding", kf: x } });
            for (const c of lcs) if (c.line) annos.push({ side: "additions", lineNumber: c.line, metadata: { kind: "comment", c } });
            if (commenting?.path === f.path) annos.push({ side: "additions", lineNumber: commenting.to, metadata: { kind: "composer" } });
            const renderAnno = (a: PierreAnno) => a.metadata.kind === "finding" ? findingNote(a.metadata.kf as FindingRow) : a.metadata.kind === "comment" ? lineCommentNote(a.metadata.c as Cmt) : composerNote();
            const selLn = diffFocus[f.path];
            // A "What changed" jump focuses one hunk (selLn set); "show all changes" clears it.
            const focused = selLn != null && f.hunks.length > 1;
            // Drop punctuation/comment-only noise, then dedupe so a repeated edit isn't listed twice.
            const seen = new Set<string>();
            const uniq = transforms
              .filter((t) => !(noise(t.old) && noise(t.next)))
              .filter((t) => { const k = t.old + "→" + t.next; if (seen.has(k)) return false; seen.add(k); return true; });
            // Rank each edit by how much it's likely to *matter*: declarations and control flow beat a
            // renamed local, a whole added/removed line beats a one-token tweak, and longer edits beat
            // trivial ones. We show the top slice by default (in file order) and fold the long tail.
            const SIG_KW = /\b(fn|def|func|function|class|struct|enum|impl|trait|interface|type|const|let|var|return|if|else|elif|for|while|match|switch|case|await|async|throw|raise|yield|import|export|from|pub|public|private|protected|new|delete|=>|->)\b/;
            const editScore = (t: { old: string; next: string }) => {
              let s = Math.min(60, Math.max(t.old.length, t.next.length));
              if (t.old === "" || t.next === "") s += 20;
              if (SIG_KW.test(t.old) || SIG_KW.test(t.next)) s += 45;
              if (/[A-Za-z_][A-Za-z0-9_]{2,}\s*\(/.test(t.next || t.old)) s += 15; // a call/definition
              return s;
            };
            const EDIT_CAP = 8;
            const showAll = allEdits.has(f.path);
            // The CAP most-significant edits (as a set of indices), still rendered in file order.
            const topIdx = new Set(uniq.map((t, i) => [i, editScore(t)] as [number, number]).sort((a, b) => b[1] - a[1]).slice(0, EDIT_CAP).map(([i]) => i));
            const shownEdits = uniq.length > EDIT_CAP && !showAll ? uniq.filter((_, i) => topIdx.has(i)) : uniq;
            const hiddenEdits = uniq.length - shownEdits.length;
            return (
              <>
                {uniq.length > 0 && (() => {
                  const clip = (s: string) => (s.length > 52 ? s.slice(0, 51).trimEnd() + "…" : s);
                  return (
                    <div className="mb-3 rounded-ctl border border-rule2 overflow-hidden">
                      <div className="px-3.5 py-2 bg-paper border-b border-rule2 flex items-center justify-between">
                        <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted">What changed here</span>
                        <span className="text-[11px] text-faint tabular-nums">{hiddenEdits > 0 ? `top ${shownEdits.length} of ${uniq.length} edits` : `${uniq.length} edit${uniq.length > 1 ? "s" : ""} · click to jump`}</span>
                      </div>
                      <div className="py-1 max-h-[300px] overflow-y-auto">
                        {shownEdits.map((t, i) => (
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
                        {(hiddenEdits > 0 || showAll) && (
                          <button onClick={() => setAllEdits((s) => { const n = new Set(s); if (showAll) n.delete(f.path); else n.add(f.path); return n; })}
                            className="w-full text-[12px] font-medium text-steel-text hover:bg-steel-wash/50 px-3.5 py-1.5 flex items-center gap-1.5">
                            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" className={showAll ? "rotate-180 transition-transform" : "transition-transform"}><polyline points="6 9 12 15 18 9" /></svg>
                            {showAll ? "Show fewer" : `Show all ${uniq.length} edits`}
                          </button>
                        )}
                      </div>
                    </div>
                  );
                })()}
                {f.too_large ? (
                  <div className="w-full py-2.5 rounded-ctl border border-dashed border-rule text-[12.5px] text-muted flex items-center justify-center gap-2">
                    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><circle cx="12" cy="12" r="10" /><line x1="12" y1="8" x2="12" y2="12" /><line x1="12" y1="16" x2="12.01" y2="16" /></svg>
                    File too large to diff inline — open it in the Files tab to view.
                  </div>
                ) : openDiff.has(f.path) ? (
                  <div>
                    {/* Always-reachable hide bar — sticks to the top of the diff so you can collapse it
                        without scrolling to the bottom of a long file. */}
                    <div className="sticky top-0 z-[2] flex items-center justify-between gap-2 mb-1.5 px-3 py-1.5 rounded-ctl bg-paper border border-rule2">
                      {focused
                        ? <button onClick={() => setDiffFocus((s) => { const n = { ...s }; delete n[f.path]; return n; })} className="text-[12px] font-medium text-steel-text hover:underline inline-flex items-center gap-1"><svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round"><line x1="12" y1="5" x2="12" y2="19" /><line x1="5" y1="12" x2="19" y2="12" /></svg>Showing one change · show all {f.hunks.length} in this file</button>
                        : <span className="text-[12px] text-muted tabular-nums">{f.hunks.length} change{f.hunks.length === 1 ? "" : "s"} · highlight code to comment</span>}
                      <button onClick={() => { setOpenDiff((s) => { const n = new Set(s); n.delete(f.path); return n; }); setDiffFocus((s) => { const n = { ...s }; delete n[f.path]; return n; }); }} className="text-[12px] font-medium text-muted hover:text-ink inline-flex items-center gap-1">Hide diff<svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round"><polyline points="18 15 12 9 6 15" /></svg></button>
                    </div>
                    <PierreReviewDiff patch={toPatch(f, focused ? selLn : undefined)} filePath={f.path} theme={theme}
                      lineAnnotations={annos} renderAnnotation={renderAnno}
                      loadFile={changeId ? async () => {
                        const ck = `${changeId}:${f.path}`;
                        if (fileCache.current[ck]) return fileCache.current[ck];
                        const r = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}/file?path=${encodeURIComponent(f.path)}`, { headers: authHeaders() });
                        if (!r.ok) throw new Error("could not load full file");
                        const d = await r.json();
                        const res = { old: d.old ?? null, new: d.new ?? null };
                        fileCache.current[ck] = res;
                        return res;
                      } : undefined}
                      selectedLines={selLn != null ? { start: selLn, end: selLn, side: "additions" } : null} />
                    <div className="flex justify-end pt-1.5">
                      <button onClick={() => setOpenDiff((s) => { const n = new Set(s); n.delete(f.path); return n; })} className="text-[12px] text-muted hover:text-ink inline-flex items-center gap-1">Hide diff<svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round"><polyline points="18 15 12 9 6 15" /></svg></button>
                    </div>
                  </div>
                ) : (
                  <button onClick={() => revealDiff(f.path)} className="w-full py-2.5 rounded-ctl border border-dashed border-rule text-[12.5px] font-medium text-muted hover:text-ink hover:border-ctl hover:bg-paper/50 transition-colors flex items-center justify-center gap-2">
                    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><polyline points="6 9 12 15 18 9" /></svg>
                    Show full diff · {f.hunks.reduce((a, h) => a + h.lines.length, 0)} lines
                    {(fs.length > 0 || lcs.length > 0) && <span className="text-steel-text">· {[fs.length && `${fs.length} finding${fs.length > 1 ? "s" : ""}`, lcs.length && `${lcs.length} comment${lcs.length > 1 ? "s" : ""}`].filter(Boolean).join(", ")}</span>}
                  </button>
                )}
              </>
            );
          };
          // One op per changed file — a clean pierre diff, behavioral files first. No move/reformat
          // grouping. `body` is a THUNK so only the file you're actually looking at builds its diff
          // (and mounts pierre) — clicking around never constructs every file's diff at once.
          const fileOps = ordered.map((f) => ({
            kind: opKind(f),
            title: base(f.path),
            meta: `${f.path}  ·  +${count(f, "add")} −${count(f, "del")}`,
            body: () => codeBody(f),
          }));
          const ops = fileOps.length ? fileOps : [{ kind: "behavior", title: "changes", meta: "", body: <p className="text-[13px] text-muted">no textual changes.</p> }];

          const voyageMeta = semantic
            ? `${semantic.behavioral.length} behavior change${semantic.behavioral.length === 1 ? "" : "s"} · ${semantic.moves.length} moved · ${semantic.whitespace_only.length} reformatted`
            : `${diff.length} file${diff.length === 1 ? "" : "s"} changed`;
          const voyage = { title: change?.intent ? change.intent.split("\n")[0] : (pr ? pr.title : "changes"), id: (changeId ?? "").slice(0, 8), meta: voyageMeta };
          return <SemanticDiff voyage={voyage} ops={ops} showMerge={false} storageKey={changeId ? `hull_reviewed_${changeId}` : undefined} />;
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
                    {c.evidence.map((e) => (
                      <div key={`${e.kind}:${e.detail}`} className={`text-[12.5px] flex gap-2 items-baseline ${e.supports ? "text-dim" : "text-fault-text"}`}>
                        <span className={`text-[10px] font-semibold uppercase tracking-[0.05em] px-1.5 py-[1px] rounded flex-none ${e.supports ? "bg-paper text-muted" : "bg-fault-wash text-fault-text"}`}>{e.kind}</span>
                        <span className="leading-snug">{e.detail}</span>
                      </div>
                    ))}
                  </div>
                )}
                {resolutions[c.id] ? (
                  <div className={`text-[12.5px] mt-2.5 ${resolutions[c.id].judgment === "verified" ? "text-clear-text" : "text-fault-text"}`}>
                    {resolutions[c.id].judgment === "verified" ? "verified by a human" : "concern raised"} · <b>{resolutions[c.id].by}</b>
                    {resolutions[c.id].note && <span className="text-muted"> — {resolutions[c.id].note}</span>}
                  </div>
                ) : (c.status === "needs_judgment" || c.status === "self_attested") ? (
                  <div className="flex items-center gap-2 mt-2.5 flex-wrap">
                    <Button size="sm" variant="secondary" disabled={!canAct} onClick={() => resolveClaim(c.id, "verified")}><span className="inline-flex items-center gap-1.5"><IcoCheck size={13} />I checked — verified</span></Button>
                    <Button size="sm" variant="destructive" disabled={!canAct} onClick={() => resolveClaim(c.id, "concern")}><span className="inline-flex items-center gap-1.5"><IcoFlag size={13} />Raise concern</span></Button>
                    {pr && change && canFix && <Button size="sm" variant="secondary" disabled={!canAct || fixingClaim === c.id} onClick={() => fixClaim(c)}>{fixingClaim === c.id ? "fixing…" : <span className="inline-flex items-center gap-1.5"><IcoSparkle size={13} />Fix with AI</span>}</Button>}
                  </div>
                ) : c.status === "contradicted" ? (
                  <div className="flex items-center gap-2 mt-2.5 flex-wrap">
                    {pr && change && canFix && <Button size="sm" variant="secondary" disabled={!canAct || fixingClaim === c.id} onClick={() => fixClaim(c)}>{fixingClaim === c.id ? "fixing…" : <span className="inline-flex items-center gap-1.5"><IcoSparkle size={13} />Fix with AI</span>}</Button>}
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
                      renderItem={(op) => <div key={op} className="text-[12.5px] text-fault-text flex gap-2 items-baseline"><span className="text-[10px] font-semibold uppercase tracking-[0.05em] px-1.5 py-[1px] rounded bg-fault-wash flex-none">phantom</span><span className="leading-snug break-all">{op}</span></div>} />
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
                        {f.severity !== "info" && pr && f.path && canFix && (<><span className="text-faint">·</span><Button size="sm" variant="secondary" disabled={!canAct || fixing === idx} onClick={() => fixWithAI(idx, f)}>{fixing === idx ? "fixing…" : <span className="inline-flex items-center gap-1.5"><IcoSparkle size={13} />Fix with AI</span>}</Button></>)}
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
                      <span className={`grid place-items-center w-[24px] h-[24px] rounded-full flex-none text-[12px] ${vcolor(e.r.verdict)}`}>{e.r.verdict === "approve" ? <IcoCheck size={13} /> : e.r.verdict === "reject" || e.r.verdict === "request_changes" ? "!" : "·"}</span>
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
    </div>
  );
}



