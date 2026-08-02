import { useEffect, useRef, useState } from "react";
import * as ed from "@noble/ed25519";

const hexToBytes = (h: string) => Uint8Array.from((h.match(/../g) ?? []).map((x) => parseInt(x, 16)));
const bytesToHex = (b: Uint8Array) => [...b].map((x) => x.toString(16).padStart(2, "0")).join("");

// The published demo-owner secret (mirrors hull-server's DEMO_OWNER_SECRET). "Sign in as demo" signs
// the login challenge with this key — real signature auth, just a publicly-known demo credential.
const DEMO_OWNER_SECRET = "68756c6c2d64656d6f2d6f776e65722d6b65792d64656d6f2d6f6e6c79212121";

// Mirrors hull-server's activity model.
type RepoActivity = {
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

type Actor = { id: string; handle: string; kind: "human" | "agent"; accountable: boolean; human_root: string | null };
type PR = { number: number; title: string; author: string; changes: string[]; verification: string; state: string; reviewers: string[] };
type Finding = { path: string; line?: number; severity: string; note: string };
type ClaimEv = { kind: string; detail: string; supports: boolean };
type LedgerSnap = { change: string; claims: { id: string; text: string; source: string; status: string; evidence: ClaimEv[] }[] };
type Review = { id: string; target: string; reviewer: string; verdict: string; summary: string; findings: Finding[]; ledger?: LedgerSnap; artifact_id?: string };
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

/**
 * The home page IS a live projection of the fleet's coordination stream: repos rank by activity
 * (an agent starting work floats a repo up), and the event ticker shows what's happening now.
 */
export function App() {
  const [repos, setRepos] = useState<RepoActivity[]>([]);
  const [events, setEvents] = useState<ActivityEvent[]>([]);
  const [tenant, setTenant] = useState<string>(
    () => new URLSearchParams(location.search).get("tenant") || "tankrap",
  );
  const feedRef = useRef<EventSource | null>(null);

  // Theme: light-first (the design's default), dark via [data-theme] on <html>. Persisted.
  const [theme, setTheme] = useState<string>(
    () => localStorage.getItem("hull_theme") || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"),
  );
  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
    localStorage.setItem("hull_theme", theme);
  }, [theme]);

  // Issues for the selected repo under the selected tenant (M2). Click a repo card to switch.
  const [issueRepo, setIssueRepo] = useState<string>("hull");
  const [issues, setIssues] = useState<Issue[]>([]);
  const [prov, setProv] = useState<Record<string, { change: string; intent: string; author: string }[]>>({});

  // Notifications recorded by the core Notifier plugin capability (poll).
  const [notifs, setNotifs] = useState<{ kind: string; to: string[]; summary: string; ts: number; broadcast?: boolean }[]>([]);
  const [showNotifs, setShowNotifs] = useState(false);
  // Unread tracking: the badge counts only notifications newer than what you've last opened.
  const [seenTs, setSeenTs] = useState<number>(() => Number(localStorage.getItem("hull_notif_seen") ?? 0));
  const toggleNotifs = () =>
    setShowNotifs((s) => {
      const opening = !s;
      if (opening && notifs.length) {
        const maxTs = Math.max(...notifs.map((n) => n.ts));
        setSeenTs(maxTs);
        localStorage.setItem("hull_notif_seen", String(maxTs));
      }
      return opening;
    });

  // Auth: sign in by proving possession of an actor's Ed25519 key → session token.
  const [token, setToken] = useState<string>(() => localStorage.getItem("hull_token") ?? "");
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
  const [showProfile, setShowProfile] = useState(false);
  useEffect(() => {
    if (!token) { setProfile(null); return; }
    fetch("/api/me", { headers: { authorization: `Bearer ${token}` } })
      .then((r) => (r.ok ? r.json() : null))
      .then(setProfile)
      .catch(() => setProfile(null));
  }, [token, me]);
  // Register a fresh human identity and sign in with it — one click to a usable session.
  const registerAndSignIn = async () => {
    const handle = prompt("handle for your new identity", "you") ?? "";
    if (!handle.trim()) return;
    const res = await fetch("/api/actors", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ handle: handle.trim(), kind: "human" }),
    });
    if (!res.ok) {
      alert(await res.text());
      return;
    }
    const { secret_key } = await res.json();
    await signInWith(secret_key);
    alert("Your secret key (save it to sign in again):\n\n" + secret_key);
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
        alert(await res.text());
        return;
      }
      const { token: t } = await res.json();
      localStorage.setItem("hull_token", t);
      setToken(t);
      setSecretInput("");
    } catch (e) {
      alert("bad secret key");
    }
  };
  const signOut = () => {
    localStorage.removeItem("hull_token");
    setToken("");
    setMe(null);
  };
  // Mint an agent that cryptographically chains to you (the authenticated caller is the parent).
  const createAgent = async () => {
    const handle = prompt("handle for your agent (it will chain to you)", "agent:mine") ?? "";
    if (!handle.trim()) return;
    const res = await fetch("/api/actors", {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
      body: JSON.stringify({ handle: handle.trim(), kind: "agent" }),
    });
    if (!res.ok) return alert(await res.text());
    const { secret_key } = await res.json();
    fetch("/api/actors").then((r) => r.json()).then((d) => setActors(d.actors ?? []));
    alert(`Agent created, delegated by you.\n\nIts secret key (save it — the agent signs in with this):\n\n${secret_key}`);
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
  const kindOf = (id: string): string => actors.find((a) => a.id === id)?.kind ?? "";
  const initials = (s: string) => (s.replace(/[^a-zA-Z0-9]/g, " ").trim().split(/\s+/).map((w) => w[0]).join("").slice(0, 2) || s.slice(0, 2)).toUpperCase();
  // active_actors arrive as handles OR raw ids; resolve ids to handles so a 64-hex key never shows.
  const actorName = (a: string) => (/^[0-9a-f]{16,}$/i.test(a) ? handleOf(a) : a);
  const openIcon = (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <line x1="7" y1="17" x2="17" y2="7" /><polyline points="8 7 17 7 17 16" />
    </svg>
  );
  // You act only as your signed-in self. No token ⇒ no identity ⇒ writes are blocked (server 401s).
  const actingAs = me?.id ?? "";
  const canAct = !!me;
  // Quick filter across the current repo's issues / PRs (title, body, #number).
  const [q, setQ] = useState("");
  const matchQ = (s: string) => q.trim() === "" || s.toLowerCase().includes(q.trim().toLowerCase());
  // Compact relative time ("3h ago") from a unix seconds timestamp.
  const timeAgo = (unix: number) => {
    if (!unix) return "";
    const s = Math.max(1, Math.floor(Date.now() / 1000 - unix));
    const steps: [number, string][] = [[60, "s"], [60, "m"], [24, "h"], [30, "d"], [12, "mo"], [Infinity, "y"]];
    let v = s, u = "s";
    for (const [div, unit] of steps) { if (v < div) { u = unit; break; } v = Math.floor(v / div); u = unit; }
    return `${v}${u} ago`;
  };

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
  const [view, setView] = useState<"home" | "repo">("home");
  const [tab, setTab] = useState<"issues" | "prs">("issues");
  const [issueView, setIssueView] = useState<"list" | "board">("list");
  const [openIssue, setOpenIssue] = useState<number | null>(null);
  const selectRepo = (repo: string) => {
    setIssueRepo(repo);
    setOpenIssue(null);
    setTab("issues");
    setView("repo");
  };
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
  const [form, setForm] = useState({ title: "", path: "", line: "", assignee: "" });
  const loadIssues = () =>
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/issues`)
      .then((r) => r.json())
      .then((d) => setIssues(d.issues ?? []))
      .catch(() => {});
  useEffect(() => {
    loadIssues();
  }, [tenant, issueRepo]);

  const issueAction = async (number: number, action: string, extra: Record<string, unknown> = {}) => {
    if (!canAct) return alert("Sign in to act.");
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/issues/${number}`, {
      method: "PATCH",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ action, ...(action === "close" ? { reason: "completed" } : {}), ...extra }),
    });
    if (res.ok) loadIssues();
    else alert(await res.text());
  };
  const transition = (number: number, action: "close" | "reopen") => issueAction(number, action);
  const [labelDraft, setLabelDraft] = useState<Record<number, string>>({});

  // Accounts / orgs (membership + roles).
  type Account = { id: string; handle: string; kind: string; repos: string[]; members: { handle: string; role: string }[] };
  const [accounts, setAccounts] = useState<Account[]>([]);
  useEffect(() => {
    fetch("/api/accounts").then((r) => r.json()).then((d) => setAccounts(d.accounts ?? [])).catch(() => {});
  }, []);
  const org = accounts.find((a) => a.handle === tenant);

  // Server-side secret-scan findings for the selected repo.
  const [secrets, setSecrets] = useState<{ path: string; line: number; title: string; redacted: string }[]>([]);
  useEffect(() => {
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/security`)
      .then((r) => r.json())
      .then((d) => setSecrets(d.secrets ?? []))
      .catch(() => {});
  }, [tenant, issueRepo, view]);

  // Pull requests for the selected repo.
  const [prs, setPrs] = useState<PR[]>([]);
  const [prTitle, setPrTitle] = useState("");
  const loadPrs = () =>
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/prs`)
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
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/mirror`)
      .then((r) => r.json())
      .then((d) => setMirror(d))
      .catch(() => {});
  }, [tenant, issueRepo, view, prs]);

  // CI endpoint config for the selected repo (owners can point it at a CI system per CI-SPEC.md).
  type CiConfig = { url: string | null; has_secret: boolean; source: string };
  const [ciConfig, setCiConfig] = useState<CiConfig | null>(null);
  const [ciUrl, setCiUrl] = useState("");
  const [ciSecret, setCiSecret] = useState("");
  const loadCiConfig = () =>
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/ci-config`)
      .then((r) => r.json())
      .then((d) => { setCiConfig(d); setCiUrl(d.url ?? ""); })
      .catch(() => {});
  useEffect(() => { loadCiConfig(); }, [tenant, issueRepo, view]);
  const isTenantOwner = !!profile?.memberships.some((m) => m.account === tenant && (m.role === "owner" || m.role === "admin"));
  const saveCiConfig = async (clear: boolean) => {
    if (!canAct) return alert("Sign in to act.");
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/ci-config`, {
      method: "PUT",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ url: clear ? "" : ciUrl.trim(), secret: clear ? "" : ciSecret }),
    });
    if (res.ok) { setCiSecret(""); loadCiConfig(); }
    else alert(await res.text());
  };

  // Autonomy policy for the selected repo (tier T0–T3, resolved repo → account → instance).
  type Autonomy = { tier: string; source: string; protected_paths: string[] };
  const [autonomy, setAutonomy] = useState<Autonomy | null>(null);
  const loadAutonomy = () =>
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/autonomy`)
      .then((r) => r.json())
      .then((d) => setAutonomy(d))
      .catch(() => {});
  useEffect(() => { loadAutonomy(); }, [tenant, issueRepo, view]);
  const setTier = async (tier: string) => {
    if (!canAct) return alert("Sign in to act.");
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/autonomy`, {
      method: "PUT",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ tier }),
    });
    if (res.ok) loadAutonomy();
    else alert(await res.text());
  };
  const TIERS: Record<string, string> = {
    t0: "Observe — no autonomous action",
    t1: "Review-required — agents auto-review; a human approves",
    t2: "Auto-approve low-risk — agent approve merges green, uncontradicted, non-protected changes",
    t3: "Autonomous — agent approve counts broadly (never protected paths)",
  };

  // Reviews (first-class), loaded per repo and filtered to a PR target.
  const [reviews, setReviews] = useState<Review[]>([]);
  const [openPr, setOpenPr] = useState<number | null>(null);
  const [openReview, setOpenReview] = useState<Review | null>(null);
  const [reviewForm, setReviewForm] = useState({ verdict: "approve", summary: "", findPath: "", findNote: "", findSev: "warn" });
  const loadReviews = () =>
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/reviews`)
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
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/comments`)
      .then((r) => r.json())
      .then((d) => setComments(d.comments ?? []))
      .catch(() => {});
  useEffect(() => { loadComments(); }, [tenant, issueRepo]);
  // Post to any target — `pr:N` or `issue:N` — keyed by the target string so drafts don't collide.
  const postComment = async (target: string) => {
    if (!canAct) return alert("Sign in to act.");
    const body = (commentDraft[target] ?? "").trim();
    if (!body) return;
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/comments`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ target, body }),
    });
    if (res.ok) { setCommentDraft((d) => ({ ...d, [target]: "" })); loadComments(); }
    else alert(await res.text());
  };
  // A reusable thread block for a target (pr:N / issue:N).
  const Thread = ({ target }: { target: string }) => (
    <div className="thread">
      {comments.filter((c) => c.target === target).sort((a, b) => a.created_unix - b.created_unix).map((c) => (
        <div className="cmt" key={c.id}>
          <b className={kindOf(c.author)}>{handleOf(c.author)}</b>
          <span className="cbody">{c.body}</span>
          <span className="cts" title={new Date(c.created_unix * 1000).toLocaleString()}>{timeAgo(c.created_unix)}</span>
        </div>
      ))}
      {comments.filter((c) => c.target === target).length === 0 && <div className="empty">no comments yet</div>}
      <div className="cmt-form">
        <input
          placeholder={canAct ? "comment…" : "sign in to comment"}
          disabled={!canAct}
          value={commentDraft[target] ?? ""}
          onChange={(e) => setCommentDraft((d) => ({ ...d, [target]: e.target.value }))}
          onKeyDown={(e) => e.key === "Enter" && postComment(target)}
        />
        <button disabled={!canAct} onClick={() => postComment(target)}>Comment</button>
      </div>
    </div>
  );
  const submitReview = async (prNumber: number) => {
    if (!canAct) return alert("Sign in to act.");
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/reviews`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({
        target: `pr:${prNumber}`,
        reviewer: actingAs,
        verdict: reviewForm.verdict,
        summary: reviewForm.summary.trim(),
        findings:
          reviewForm.findPath.trim() && reviewForm.findNote.trim()
            ? [{ path: reviewForm.findPath.trim(), severity: reviewForm.findSev, note: reviewForm.findNote.trim() }]
            : [],
      }),
    });
    if (res.ok) {
      setReviewForm({ verdict: "approve", summary: "", findPath: "", findNote: "", findSev: "warn" });
      loadReviews();
    } else {
      alert(await res.text());
    }
  };

  const [autoReviewing, setAutoReviewing] = useState<number | null>(null);
  const requestReviewer = async (prNumber: number, reviewer: string) => {
    if (!canAct || !reviewer) return;
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/prs/${prNumber}/reviewers`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ reviewer }),
    });
    if (res.ok) loadPrs();
    else alert(await res.text());
  };
  const autoReview = async (prNumber: number) => {
    if (!canAct) return alert("Sign in to act.");
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
        alert(await res.text());
      }
    } finally {
      setAutoReviewing(null);
    }
  };

  const closePr = async (number: number, reopen: boolean) => {
    if (!canAct) return alert("Sign in to act.");
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/prs/${number}/close`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ reopen }),
    });
    if (res.ok) loadPrs();
    else alert(await res.text());
  };
  const mergePr = async (number: number) => {
    if (!canAct) return alert("Sign in to act.");
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/prs/${number}/merge`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ actor: actingAs }),
    });
    if (res.ok) loadPrs();
    else alert(await res.text());
  };

  const createPr = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!canAct) return alert("Sign in to act.");
    if (!prTitle.trim()) return;
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/prs`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ title: prTitle.trim(), author: actingAs }),
    });
    if (res.ok) {
      setPrTitle("");
      loadPrs();
    } else {
      alert(await res.text());
    }
  };

  const createIssue = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!canAct) return alert("Sign in to act.");
    if (!form.title.trim()) return;
    const code_ref = form.path.trim()
      ? { path: form.path.trim(), line_start: Number(form.line) || 1 }
      : null;
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/issues`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({
        title: form.title.trim(),
        author: actingAs,
        code_ref,
        assignees: form.assignee ? [form.assignee] : [],
      }),
    });
    if (res.ok) {
      setForm({ title: "", path: "", line: "", assignee: "" });
      loadIssues();
    } else {
      alert(await res.text());
    }
  };

  // Poll the activity-ranked home for the selected tenant (each org sees only its own fleet).
  useEffect(() => {
    setRepos([]);
    const load = () =>
      fetch(`/api/home?tenant=${encodeURIComponent(tenant)}`)
        .then((r) => r.json())
        .then((d) => setRepos(d.repos ?? []))
        .catch(() => {});
    load();
    const t = setInterval(load, 2000);
    return () => clearInterval(t);
  }, [tenant]);

  // Live event stream over SSE, scoped to the selected tenant.
  useEffect(() => {
    setEvents([]);
    const es = new EventSource(`/api/feed?tenant=${encodeURIComponent(tenant)}`);
    feedRef.current = es;
    es.onmessage = (m) => {
      try {
        const ev = JSON.parse(m.data) as ActivityEvent;
        setEvents((prev) => [ev, ...prev].slice(0, 40));
        if (ev.kind === "issue") loadIssues(); // reflect new issues live
      } catch {
        /* ignore keep-alives */
      }
    };
    return () => es.close();
  }, [tenant]);

  if (openReview) {
    return (
      <ReviewPage
        review={openReview}
        pr={prs.find((p) => `pr:${p.number}` === openReview.target) ?? null}
        actors={actors}
        tenant={tenant}
        repo={issueRepo}
        token={token}
        me={me}
        onBack={() => setOpenReview(null)}
      />
    );
  }

  return (
    <div className="shell">
      <header className="topbar">
        <button className="brand" onClick={() => setView("home")} title="home">
          <span className="mark" aria-hidden /> <span className="wordmark">hull</span>
        </button>
        <div className="crumbs">
          {view === "repo" ? (
            <>
              <button className="link" onClick={() => setView("home")}>{tenant}</button>
              <span className="sep">/</span>
              <span className="cur">{issueRepo}</span>
            </>
          ) : (
            <span>situation room</span>
          )}
        </div>
        <div className="grow" />
        <div className="searchbox" title="tenant / organization">
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round"><circle cx="11" cy="11" r="8" /><line x1="21" y1="21" x2="16.65" y2="16.65" /></svg>
          <input value={tenant} onChange={(e) => setTenant(e.target.value.trim())} spellCheck={false} aria-label="tenant" placeholder="tenant" />
        </div>
        <button className="icon-btn" onClick={() => setTheme((t) => (t === "dark" ? "light" : "dark"))} title={theme === "dark" ? "switch to light" : "switch to dark"} aria-label="toggle theme">
          {theme === "dark" ? "☀" : "☾"}
        </button>
        <div className="bell-wrap">
          <button className="icon-btn" onClick={toggleNotifs} title="notifications">
            🔔{notifs.filter((n) => n.ts > seenTs).length > 0 && <span className="bell-count">{notifs.filter((n) => n.ts > seenTs).length}</span>}
          </button>
          {showNotifs && (
            <div className="pop">
              <div className="pop-head">inbox for <b>{handleOf(actingAs)}</b> · via Notifier plugin</div>
              {notifs.length === 0 && <div className="empty">nothing yet</div>}
              {notifs.slice(0, 15).map((n, i) => (
                <div className={"notif" + (n.ts > seenTs ? " unread" : "")} key={i}>
                  <span className="nk">{n.kind.replace(/_/g, " ")}</span>
                  {n.broadcast && <span className="pill tag">team</span>}
                  <span className="ns">{n.summary}</span>
                </div>
              ))}
            </div>
          )}
        </div>
        {me ? (
          <div className="bell-wrap">
            <button className="avatar" onClick={() => setShowProfile((s) => !s)} title={`${me.handle} · your identity`}>{initials(me.handle)}</button>
            {showProfile && profile && (
              <div className="pop">
                <div className="pop-head"><b className={profile.kind}>{profile.handle}</b> · {profile.kind}{profile.accountable && <> · <span className="pill pass">accountable</span></>}</div>
                <div className="pop-row"><span className="pk-label">actor id</span><span className="pk" title="your Ed25519 public key — this IS your identity">{profile.id}</span></div>
                {profile.kind === "agent" && profile.delegation.length > 0 && (
                  <div className="pop-row"><span className="pk-label">accountability</span><span>{profile.delegation.map((h, i) => (<span key={i}><b className={h.kind}>{h.handle}</b>{i < profile.delegation.length - 1 && " → "}</span>))}</span></div>
                )}
                <div className="pop-row"><span className="pk-label">memberships</span>{profile.memberships.length > 0 ? <span style={{ display: "flex", flexWrap: "wrap", gap: 6 }}>{profile.memberships.map((m, i) => (<span key={i} className="pill">{m.account} · {m.role}</span>))}</span> : <span className="muted">none</span>}</div>
                <div className="pop-row"><span className="pk-label" /><span style={{ display: "flex", gap: 14 }}>{profile.kind === "human" && <button className="link" onClick={createAgent}>+ create agent</button>}<button className="link" onClick={signOut}>sign out</button></span></div>
              </div>
            )}
          </div>
        ) : (
          <div className="signin">
            <input type="password" placeholder="secret key" value={secretInput} onChange={(e) => setSecretInput(e.target.value)} onKeyDown={(e) => e.key === "Enter" && signIn()} />
            <button className="secondary" onClick={signIn}>Sign in</button>
            <button className="link" onClick={registerAndSignIn}>new</button>
            <button className="link" onClick={() => signInWith(DEMO_OWNER_SECRET)} title="log in as the published demo owner">demo</button>
          </div>
        )}
      </header>

      {view === "home" && (
        <div className="page-body">
          <div className="body-head">
            <span className="eyebrow">Situation room</span>
            <div className="grow" />
            {org && <span className="pill">{org.handle} · {org.kind}</span>}
          </div>
          <div className="split">
            <div className="panel">
              <div className="panel-head">
                <span className="strong">Repositories</span>
                <span className="muted">by live activity</span>
                <div className="grow" />
                <span className="muted">click to open</span>
              </div>
              {repos.length === 0 && (
                <div className="empty">no active repos for {tenant} — push one to <code>http://localhost:8930/{tenant}/&lt;repo&gt;</code></div>
              )}
              <div className="rows">
                {repos.map((r) => (
                  <button className="row" key={r.repo} onClick={() => selectRepo(r.repo)} title="open this repo">
                    <div className="main">
                      <div className="line1">
                        <span className="rtitle">{r.repo}</span>
                        {r.active_actors.slice(0, 4).map((a) => (
                          <span className={"pill " + (actorName(a).startsWith("agent") ? "agent" : "human")} key={a}>{actorName(a)}</span>
                        ))}
                      </div>
                      <div className="meta">
                        <span>activity {r.score.toFixed(0)}</span>
                        {r.hot_files.slice(0, 3).map((f) => (<span key={f}>{f}</span>))}
                      </div>
                    </div>
                    <span className="trailing swap">
                      <span className="diffstat muted">{r.active_actors.length} active</span>
                      <span className="openlink">{openIcon}</span>
                    </span>
                  </button>
                ))}
              </div>
            </div>
            <div className="side">
              <div className="card" style={{ padding: 0, overflow: "hidden" }}>
                <div className="card-title" style={{ padding: "12px 14px", borderBottom: "1px solid var(--rule2)", marginBottom: 0 }}>Live feed</div>
                <div>
                  {events.length === 0 && <div className="empty">listening…</div>}
                  {events.slice(0, 24).map((e, i) => (
                    <div key={i} style={{ padding: "9px 14px", borderTop: i ? "1px solid var(--rule2)" : "none", fontSize: 12.5, color: "var(--body)" }}>{renderEvent(e)}</div>
                  ))}
                </div>
              </div>
              {org && (
                <div className="card">
                  <div className="card-title">{org.handle} <span className="muted" style={{ fontWeight: 400 }}>· {org.members.length} members</span></div>
                  <div style={{ display: "flex", flexWrap: "wrap", gap: 6 }}>
                    {org.members.map((m, i) => (<span className="pill" key={i}>{m.handle} · {m.role}</span>))}
                  </div>
                </div>
              )}
            </div>
          </div>
        </div>
      )}

      {view === "repo" && (
        <>
          <div className="repo-header">
            <span className="repo-mark" aria-hidden />
            <span className="repo-name">{tenant} / {issueRepo}</span>
            {autonomy && (
              <span className={"pill " + (autonomy.tier === "t3" || autonomy.tier === "t2" ? "warn" : "")} title={TIERS[autonomy.tier]}>autonomy {autonomy.tier.toUpperCase()}</span>
            )}
            {mirror?.target && <span className="pill" title={`mirrored to ${mirror.target}`}>⇄ mirrored</span>}
            <div className="grow" />
            {!canAct && <span className="muted" style={{ fontSize: 12.5 }}>read-only · <button className="link" onClick={() => signInWith(DEMO_OWNER_SECRET)}>sign in</button></span>}
          </div>
          <div className="nav">
            <button className={"nav-item" + (tab === "issues" ? " on" : "")} onClick={() => setTab("issues")}>
              <span className="lbl">Issues <span className="nav-count">{issues.filter((i) => i.status.state === "open").length}</span></span>
              <span className="bar" />
            </button>
            <button className={"nav-item" + (tab === "prs" ? " on" : "")} onClick={() => setTab("prs")}>
              <span className="lbl">Pull requests <span className="nav-count">{prs.length}</span></span>
              <span className="bar" />
            </button>
          </div>
          <div className="page-body">
            <div className="split">
              <div>
                {tab === "issues" && (
                  <div className="stack">
                    <div className="body-head" style={{ marginBottom: 0 }}>
                      <div className="seg">
                        <button className={issueView === "list" ? "on" : ""} onClick={() => setIssueView("list")}>List</button>
                        <button className={issueView === "board" ? "on" : ""} onClick={() => setIssueView("board")}>Board</button>
                      </div>
                      <div className="grow" />
                      <input style={{ maxWidth: 200 }} placeholder="filter issues…" value={q} onChange={(e) => setQ(e.target.value)} spellCheck={false} />
                    </div>
                    <form className="form-row" onSubmit={createIssue}>
                      <input placeholder="Open an issue…" value={form.title} onChange={(e) => setForm({ ...form, title: e.target.value })} />
                      <input style={{ flex: 2 }} placeholder="path (optional)" value={form.path} onChange={(e) => setForm({ ...form, path: e.target.value })} spellCheck={false} />
                      <input className="field-narrow" placeholder="line" value={form.line} onChange={(e) => setForm({ ...form, line: e.target.value })} />
                      <select value={form.assignee} onChange={(e) => setForm({ ...form, assignee: e.target.value })}>
                        <option value="">assign…</option>
                        {actors.map((a) => (<option key={a.id} value={a.id}>{a.handle}</option>))}
                      </select>
                      <button type="submit">Open</button>
                    </form>
                    {issueView === "list" ? (
                      <div className="panel">
                        <div className="panel-head"><span className="strong">{issues.filter((i) => i.status.state === "open").length} open</span><span className="muted">{issues.length} total</span></div>
                        {issues.length === 0 && <div className="empty">no issues yet — open one above</div>}
                        <div className="rows">
                          {[...issues]
                            .filter((it) => matchQ(`${it.title} ${it.body} #${it.number} ${it.labels.join(" ")}`))
                            .sort((a, b) => Number(a.status.state !== "open") - Number(b.status.state !== "open") || b.number - a.number)
                            .map((it) => (
                              <div key={it.number}>
                                <div className="row" onClick={() => setOpenIssue(openIssue === it.number ? null : it.number)}>
                                  <div className="main">
                                    <div className="line1">
                                      <span className="rtitle">{it.title}</span>
                                      <span className={"pill " + (it.status.state === "open" ? "open" : "closed")} title={it.status.reason ?? ""}>{it.status.state === "open" ? "open" : it.status.reason ?? "closed"}</span>
                                      {it.labels.map((l) => (<span key={l} className="pill" style={{ cursor: "pointer" }} onClick={(e) => { e.stopPropagation(); setQ(l); }}>{l}</span>))}
                                      {it.assignees.map((id) => (<span key={id} className="pill">◎ {handleOf(id)}</span>))}
                                      {it.resolved_by && <span className="pill pass" title="closed by a merged PR">⬡ resolved</span>}
                                      {!it.resolved_by && (it.linked_prs?.length ?? 0) > 0 && <span className="pill">⇄ {it.linked_prs!.length} PR{it.linked_prs!.length > 1 ? "s" : ""}</span>}
                                    </div>
                                    <div className="meta">
                                      <span>#{it.number}</span>
                                      <span className={kindOf(it.author)}>{handleOf(it.author)}</span>
                                      {it.code_refs.map((c, i) => (
                                        <span key={i} className="agent-text" style={{ cursor: "pointer" }} title={`keel blob ${c.blob} · provenance`} onClick={(e) => { e.stopPropagation(); showWhy(`${it.number}:${c.path}`, c.path); }}>⬡ {c.path}:{c.line_start}{c.line_end ? `-${c.line_end}` : ""}</span>
                                      ))}
                                    </div>
                                  </div>
                                  <span className="trailing">
                                    {it.status.state === "open"
                                      ? <button className="btn-sec" style={{ height: 26 }} onClick={(e) => { e.stopPropagation(); transition(it.number, "close"); }}>Close</button>
                                      : <button className="btn-sec" style={{ height: 26 }} onClick={(e) => { e.stopPropagation(); transition(it.number, "reopen"); }}>Reopen</button>}
                                  </span>
                                </div>
                                {openIssue === it.number && (
                                  <div style={{ padding: "0 14px 16px 14px", display: "grid", gap: 12 }}>
                                    {it.body && <p style={{ color: "var(--body)", margin: 0 }}>{it.body}</p>}
                                    <div className="mrow">
                                      <span className="pk-label">assignees</span>
                                      {it.assignees.map((id) => (<span key={id} className="pill">{handleOf(id)}{canAct && <button className="pill-x" title="unassign" onClick={() => issueAction(it.number, "unassign", { assignee: id })}>×</button>}</span>))}
                                      {canAct && me && !it.assignees.includes(me.id) && (<button className="link" onClick={() => issueAction(it.number, "assign", { assignee: me.id })}>assign me</button>)}
                                      {it.assignees.length === 0 && !canAct && <span className="muted">none</span>}
                                    </div>
                                    <div className="mrow">
                                      <span className="pk-label">labels</span>
                                      {it.labels.map((l) => (<span key={l} className="pill">{l}{canAct && <button className="pill-x" title="remove" onClick={() => issueAction(it.number, "unlabel", { label: l })}>×</button>}</span>))}
                                      {canAct && (
                                        <input style={{ height: 26, maxWidth: 130 }} placeholder="add label…" value={labelDraft[it.number] ?? ""} onChange={(e) => setLabelDraft((d) => ({ ...d, [it.number]: e.target.value }))}
                                          onKeyDown={(e) => { if (e.key === "Enter" && (labelDraft[it.number] ?? "").trim()) { issueAction(it.number, "label", { label: labelDraft[it.number].trim() }); setLabelDraft((d) => ({ ...d, [it.number]: "" })); } }} />
                                      )}
                                    </div>
                                    {it.code_refs.map((c) => {
                                      const key = `${it.number}:${c.path}`;
                                      return prov[key] ? (
                                        <div className="card" key={key} style={{ padding: 12 }}>
                                          <div className="pk-label" style={{ marginBottom: 6 }}>keel provenance · {c.path}</div>
                                          {prov[key].length === 0 && <div className="muted">no recorded history</div>}
                                          {prov[key].map((pp, j) => (<div key={j} className="stat-row"><span>⬡ {pp.change.slice(0, 10)} · {pp.intent}</span><span className="muted">{pp.author}</span></div>))}
                                        </div>
                                      ) : null;
                                    })}
                                    <div><div className="pk-label" style={{ marginBottom: 8 }}>Discussion</div><Thread target={`issue:${it.number}`} /></div>
                                  </div>
                                )}
                              </div>
                            ))}
                        </div>
                      </div>
                    ) : (
                      <div className="board">
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
                            <div className="board-col" key={col.k}>
                              <div className="board-col-head">{col.label} <span className="muted">{inCol.length}</span></div>
                              {inCol.map((it) => (
                                <div className="board-card" key={it.number} onClick={() => { setIssueView("list"); setOpenIssue(it.number); }}>
                                  <div className="muted" style={{ fontSize: 11 }}>#{it.number}</div>
                                  <div style={{ fontSize: 13, fontWeight: 500, marginTop: 4 }}>{it.title}</div>
                                  {it.assignees.length > 0 && <div className="muted" style={{ fontSize: 12, marginTop: 8 }}>◎ {it.assignees.map((id) => handleOf(id)).join(", ")}</div>}
                                </div>
                              ))}
                            </div>
                          );
                        })}
                      </div>
                    )}
                  </div>
                )}

                {tab === "prs" && (
                  <div className="stack">
                    <form className="form-row" onSubmit={createPr}>
                      <input placeholder="Open a PR from HEAD…" value={prTitle} onChange={(e) => setPrTitle(e.target.value)} />
                      <input style={{ maxWidth: 180 }} placeholder="filter…" value={q} onChange={(e) => setQ(e.target.value)} spellCheck={false} />
                      <button type="submit">Open PR</button>
                    </form>
                    <div className="panel">
                      <div className="panel-head"><span className="strong">{prs.filter((p) => p.state === "open").length} open</span><span className="muted">{prs.length} total</span></div>
                      {prs.length === 0 && <div className="empty">no pull requests yet</div>}
                      <div className="rows">
                        {[...prs].filter((p) => matchQ(`${p.title} #${p.number}`)).sort((a, b) => b.number - a.number).map((p) => {
                          const prReviews = reviews.filter((r) => r.target === `pr:${p.number}`);
                          const st = p.state === "merged" ? "merged" : p.state === "closed" ? "closed" : p.verification;
                          return (
                            <div key={p.number}>
                              <div className="row" onClick={() => setOpenPr(openPr === p.number ? null : p.number)}>
                                <div className="main">
                                  <div className="line1">
                                    <span className="rtitle">{p.title}</span>
                                    <span className={"pill " + st}>{st}</span>
                                    {prReviews.length > 0 && <span className="pill">{prReviews.length} review{prReviews.length > 1 ? "s" : ""}</span>}
                                  </div>
                                  <div className="meta">
                                    <span>!{p.number}</span>
                                    <span className="agent-text">⬡ {(p.changes[0] ?? "").slice(0, 10)}</span>
                                    <span className={kindOf(p.author)}>{handleOf(p.author)}</span>
                                    {p.reviewers?.length > 0 && <span>◎ {p.reviewers.map((id) => handleOf(id)).join(", ")}</span>}
                                  </div>
                                </div>
                                <span className="trailing swap">
                                  <span className="diffstat muted">{prReviews.length ? `${prReviews.length} rev` : "review"}</span>
                                  <span className="openlink">{openIcon}</span>
                                </span>
                              </div>
                              {openPr === p.number && (
                                <div style={{ padding: "0 14px 16px 14px", display: "grid", gap: 12 }}>
                                  <div className="mrow">
                                    {p.state === "merged" ? <span className="pill pass">✓ merged</span>
                                      : p.state === "closed" ? <><span className="pill closed">closed</span>{canAct && <button className="link" onClick={() => closePr(p.number, true)}>reopen</button>}</>
                                        : <>
                                          <button style={{ height: 28 }} onClick={() => mergePr(p.number)}>Merge</button>
                                          {canAct && <button className="link" onClick={() => closePr(p.number, false)}>close</button>}
                                          <span className="muted" style={{ fontSize: 12 }}>gate: keel-verify green + an approving review from someone other than the author</span>
                                        </>}
                                  </div>
                                  {prReviews.length === 0 && <div className="muted">no reviews yet</div>}
                                  {prReviews.map((r) => (
                                    <button className="review-row" key={r.id} onClick={() => setOpenReview(r)} title="open review">
                                      <span className={"pill " + r.verdict}>{r.verdict.replace("_", " ")}</span>
                                      <b className={kindOf(r.reviewer)}>{handleOf(r.reviewer)}</b>
                                      <span className="rv-summary">{r.summary || "open review →"}</span>
                                      {r.findings?.length > 0 && <span className="pill">{r.findings.length} finding{r.findings.length > 1 ? "s" : ""}</span>}
                                      <span className="openlink" style={{ position: "static", opacity: 1, transform: "none", marginLeft: "auto" }}>{openIcon}</span>
                                    </button>
                                  ))}
                                  <div className="mrow">
                                    <button className="btn-sec" disabled={autoReviewing === p.number} onClick={() => autoReview(p.number)}>
                                      {autoReviewing === p.number ? "agent reviewing…" : "⬡ Agent auto-review"}
                                    </button>
                                    {canAct && (
                                      <select value="" onChange={(e) => { requestReviewer(p.number, e.target.value); e.target.value = ""; }}>
                                        <option value="">request a reviewer…</option>
                                        {actors.filter((a) => a.id !== p.author && !p.reviewers?.includes(a.id)).map((a) => (<option key={a.id} value={a.id}>{a.handle} ({a.kind})</option>))}
                                      </select>
                                    )}
                                  </div>
                                  <div className="form-row">
                                    <select value={reviewForm.verdict} onChange={(e) => setReviewForm({ ...reviewForm, verdict: e.target.value })}>
                                      <option value="approve">approve</option>
                                      <option value="request_changes">request changes</option>
                                      <option value="reject">reject</option>
                                      <option value="comment">comment</option>
                                    </select>
                                    <input placeholder={`review as ${handleOf(actingAs)}…`} value={reviewForm.summary} onChange={(e) => setReviewForm({ ...reviewForm, summary: e.target.value })} />
                                    <button onClick={() => submitReview(p.number)}>Submit</button>
                                  </div>
                                  <div className="form-row">
                                    <span className="pk-label">finding</span>
                                    <input style={{ flex: 1 }} placeholder="path" value={reviewForm.findPath} onChange={(e) => setReviewForm({ ...reviewForm, findPath: e.target.value })} spellCheck={false} />
                                    <select value={reviewForm.findSev} onChange={(e) => setReviewForm({ ...reviewForm, findSev: e.target.value })}>
                                      <option value="info">info</option><option value="warn">warn</option><option value="blocker">blocker</option>
                                    </select>
                                    <input style={{ flex: 2 }} placeholder="what's wrong" value={reviewForm.findNote} onChange={(e) => setReviewForm({ ...reviewForm, findNote: e.target.value })} />
                                  </div>
                                  <div><div className="pk-label" style={{ marginBottom: 8 }}>Discussion · humans and agents, one accountable thread</div><Thread target={`pr:${p.number}`} /></div>
                                </div>
                              )}
                            </div>
                          );
                        })}
                      </div>
                    </div>
                  </div>
                )}
              </div>

              <div className="side">
                {secrets.length > 0 && (
                  <div className="card" style={{ borderColor: "color-mix(in oklab, var(--fault) 30%, transparent)" }}>
                    <div className="card-title" style={{ color: "var(--fault-text)" }}>⚠ {secrets.length} secret{secrets.length > 1 ? "s" : ""} on push</div>
                    {secrets.slice(0, 5).map((s, i) => (<div key={i} className="stat-row" style={{ display: "block", marginTop: 6 }}>{s.title} — <span className="muted">{s.path}:{s.line}</span></div>))}
                  </div>
                )}
                <div className="card">
                  <div className="card-title">Autonomy</div>
                  {autonomy ? (
                    <>
                      <div className="stat-row"><span>tier</span><b>{autonomy.tier.toUpperCase()}</b></div>
                      <div className="stat-row"><span>source</span><span className="muted">{autonomy.source}</span></div>
                      <p className="muted" style={{ fontSize: 12, margin: "8px 0 0" }}>{TIERS[autonomy.tier]}</p>
                      {isTenantOwner && (
                        <select style={{ width: "100%", marginTop: 10 }} value={autonomy.tier} onChange={(e) => setTier(e.target.value)}>
                          {["t0", "t1", "t2", "t3"].map((t) => (<option key={t} value={t}>{t.toUpperCase()} — {TIERS[t].split("—")[0].trim()}</option>))}
                        </select>
                      )}
                    </>
                  ) : <div className="muted">—</div>}
                </div>
                <div className="card">
                  <div className="card-title">Drydock (CI)</div>
                  {ciConfig?.url ? (
                    <><div className="stat-row"><span>endpoint</span><span className="muted" style={{ maxWidth: 140, overflow: "hidden", textOverflow: "ellipsis" }}>{ciConfig.url}</span></div><div className="stat-row"><span>source</span><span className="muted">{ciConfig.source}{ciConfig.has_secret ? " · secret" : ""}</span></div></>
                  ) : <p className="muted" style={{ fontSize: 12, margin: 0 }}>{ciConfig?.source ?? "built-in"} — checks run on the built-in local runner.</p>}
                  {isTenantOwner && (
                    <form style={{ display: "grid", gap: 6, marginTop: 10 }} onSubmit={(e) => { e.preventDefault(); saveCiConfig(false); }}>
                      <input placeholder="https://your-ci/hull" value={ciUrl} onChange={(e) => setCiUrl(e.target.value)} spellCheck={false} />
                      <input type="text" placeholder="shared secret (optional)" value={ciSecret} onChange={(e) => setCiSecret(e.target.value)} spellCheck={false} />
                      <div className="mrow">
                        <button type="submit" style={{ height: 28 }}>Set</button>
                        <button type="button" className="link" onClick={() => setCiSecret(bytesToHex(crypto.getRandomValues(new Uint8Array(32))))}>generate</button>
                        {ciConfig?.source === "repo" && <button type="button" className="link" onClick={() => saveCiConfig(true)}>clear</button>}
                        <a className="link" href="https://github.com/tankrap/hull/blob/main/CI-SPEC.md" target="_blank" rel="noreferrer">spec ↗</a>
                      </div>
                    </form>
                  )}
                </div>
                {mirror?.target && (
                  <div className="card">
                    <div className="card-title">Mirror</div>
                    <div className="stat-row"><span>target</span><span className="muted">{mirror.target}</span></div>
                    <div className="stat-row"><span>pushed</span><b>{mirror.outbound.length}</b></div>
                    <p className="muted" style={{ fontSize: 11.5, margin: "8px 0 0" }}>loop-safe: forge-originated changes are never pushed back.</p>
                  </div>
                )}
              </div>
            </div>
          </div>
        </>
      )}
    </div>
  );
}

/** The review "package" — a dedicated page synthesizing what a reviewer needs, not a one-liner. */
function ReviewPage({
  review,
  pr,
  actors,
  tenant,
  repo,
  token,
  me,
  onBack,
}: {
  review: Review;
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
  type FileDiff = { path: string; status: string; ops: string[]; hunks: { old_start: number; new_start: number; lines: DiffLine[] }[] };
  type Evidence = { kind: string; detail: string; supports: boolean };
  type Claim = { id: string; text: string; source: string; status: string; evidence: Evidence[] };
  type Ledger = { change: string; claims: Claim[] };
  const [change, setChange] = useState<ChangeInfo | null>(null);
  const [diff, setDiff] = useState<FileDiff[]>([]);
  const [ledger, setLedger] = useState<Ledger | null>(null);
  const [openFile, setOpenFile] = useState<string | null>(null);
  const handleOf = (id: string) => actors.find((a) => a.id === id)?.handle ?? id.slice(0, 8);
  const reviewerActor = actors.find((a) => a.id === review.reviewer);
  const changeId = pr?.changes[0];
  const loadChange = () => {
    if (!changeId) return;
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}`)
      .then((r) => r.json())
      .then((d) => setChange(d.change))
      .catch(() => {});
  };
  useEffect(loadChange, [changeId, tenant, repo]);
  useEffect(() => {
    if (!changeId) return;
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}/diff`)
      .then((r) => r.json())
      .then((d) => setDiff(d.files ?? []))
      .catch(() => {});
  }, [changeId, tenant, repo]);
  // If this review carries an immutable ledger snapshot (an agent reconciliation review), show that
  // — it's the evidence the verdict was actually based on. Otherwise reconcile live.
  const snapshot = review.ledger ?? null;
  const loadLedger = () => {
    if (snapshot || !changeId) return;
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}/ledger`)
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
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}/ledger`)
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
    if (!canAct) return alert("Sign in to act.");
    const note = prompt(judgment === "verified" ? "What did you check? (optional note)" : "What's the concern?") ?? "";
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}/claims/${claimId}/resolve`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ judgment, note }),
    });
    if (res.ok) loadResolutions();
    else alert(await res.text());
  };

  // "Fix with AI": ask the fixer to propose a patch for a finding; it posts to the PR thread.
  const [fixing, setFixing] = useState<number | null>(null);
  const fixWithAI = async (idx: number, f: Finding) => {
    if (!canAct || !pr) return alert("Sign in to act.");
    setFixing(idx);
    try {
      const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/prs/${pr.number}/fix`, {
        method: "POST",
        headers: { "content-type": "application/json", ...authHeaders() },
        body: JSON.stringify({ path: f.path, note: f.note, severity: f.severity }),
      });
      if (res.ok) { const d = await res.json(); alert("AI fix applied as a new change (re-verified):\n\n" + (d.fix?.explanation ?? "")); loadThread(); loadChange(); }
      else alert(await res.text());
    } finally {
      setFixing(null);
    }
  };

  const verify = async (green: boolean) => {
    if (!changeId) return;
    if (!canAct) return alert("Sign in to act.");
    await fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}/verify`, {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ green }),
    });
    loadChange();
  };

  const [checking, setChecking] = useState(false);
  const [checkResult, setCheckResult] = useState<{ status: string; summary: string; memoized: boolean } | null>(null);
  const runChecks = async (force: boolean) => {
    if (!changeId) return;
    if (!canAct) return alert("Sign in to act.");
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

  const independent = pr ? pr.author !== review.reviewer : true;
  const verification = change?.verification ?? "unverified";
  const risk =
    verification === "green"
      ? "low — keel verify is green"
      : verification === "red"
        ? "high — keel verify is red"
        : change && change.files.length > 8
          ? "elevated — unverified and a broad change"
          : "moderate — unverified";

  // Discussion thread — the same PR thread as the compact view, followed into the deep review page.
  type Cmt = { id: string; target: string; author: string; body: string; created_unix: number };
  const [thread, setThread] = useState<Cmt[]>([]);
  const [draft, setDraft] = useState("");
  const loadThread = () =>
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/comments`)
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
    else alert(await res.text());
  };

  return (
    <div className="shell review-page">
      <header className="topbar">
        <button className="brand" onClick={onBack} title="back to situation room"><span className="mark" aria-hidden /> <span className="wordmark">hull</span></button>
        <div className="crumbs"><button className="link" onClick={onBack}>← situation room</button></div>
        <div className="grow" />
        <span className="muted" style={{ fontSize: 12.5 }}>review package</span>
      </header>
      <div className="page-body">
        <div className="rp-head">
          <span className={"pill " + review.verdict}>{review.verdict.replace("_", " ")}</span>
          <span className="rp-title">{pr ? `PR !${pr.number} · ${pr.title}` : review.target}</span>
        </div>
        <div className="stack">{/* review cards */}
        {(() => {
          // F5: degraded-state badges — surface where the review is thinner than ideal.
          const needs = shownLedger?.claims.filter((c) => c.status === "needs_judgment").length ?? 0;
          const selfAtt = shownLedger?.claims.filter((c) => c.status === "self_attested").length ?? 0;
          const badges: { cls: string; label: string; title: string }[] = [];
          if (change && !change.session) badges.push({ cls: "no-plan", label: "no plan captured", title: "pushed as plain git — no session/plan; provenance is reconstructed, not native (commit with keel --session)" });
          if (change && change.verification !== "green") badges.push({ cls: "unverified", label: `checks ${change.verification}`, title: "checks are not green — the mechanical evidence is incomplete" });
          if (needs > 0) badges.push({ cls: "partial", label: `${needs} unresolved claim${needs > 1 ? "s" : ""}`, title: "claims the engine couldn't verify — a human must judge them (partial review)" });
          if (selfAtt > 0) badges.push({ cls: "self", label: "self-attested tests", title: "green, but the change tests itself — not independently verified" });
          return badges.length > 0 ? (
            <div className="degraded-badges">
              {badges.map((b, i) => (
                <span key={i} className={"degraded " + b.cls} title={b.title}>⚠ {b.label}</span>
              ))}
            </div>
          ) : null;
        })()}

        <section className="rp-card">
          <h3>Reviewer</h3>
          <p>
            <b className={reviewerActor?.kind}>{handleOf(review.reviewer)}</b> ({reviewerActor?.kind ?? "actor"})
            {reviewerActor?.human_root && (
              <>
                {" "}· accountable to human <code>{reviewerActor.human_root.slice(0, 10)}</code>
              </>
            )}
            {independent ? (
              <span className="badge ok"> independent of the author</span>
            ) : (
              <span className="badge warn"> same as the author</span>
            )}
          </p>
          {review.summary && <p className="summary">{review.summary}</p>}
          {review.artifact_id && (
            <p className="audit-artifact">
              <span className="muted">audit artifact</span>{" "}
              <a href={`/api/repos/${encodeURIComponent(tenant)}/${repo}/artifacts/${review.artifact_id}`} target="_blank" rel="noreferrer" title="content-addressed record of why this verdict was reached — immutable">
                ⬡ {review.artifact_id.slice(0, 12)}
              </a>{" "}
              <span className="muted">· content-addressed, immutable</span>
            </p>
          )}
        </section>

        {shownLedger && shownLedger.claims.length > 0 && (() => {
          const ledger = shownLedger;
          const POSITIVE = ["verified_mechanically", "verified_read_only", "self_attested"];
          const n = (s: string) => ledger.claims.filter((c) => c.status === s).length;
          const supported = ledger.claims.filter((c) => POSITIVE.includes(c.status)).length;
          const contradicted = n("contradicted");
          const selfAtt = n("self_attested");
          const needs = n("needs_judgment");
          // status → glyph, label, css class
          const meta: Record<string, [string, string]> = {
            verified_mechanically: ["✓", "verified"],
            verified_read_only: ["◎", "read-only"],
            self_attested: ["⚠", "self-attested"],
            contradicted: ["✗", "contradicted"],
            needs_judgment: ["?", "needs judgment"],
          };
          // Order: contradicted first (surface at top), then needs-judgment, then positives.
          const order: Record<string, number> = { contradicted: 0, needs_judgment: 1, self_attested: 2, verified_read_only: 3, verified_mechanically: 4 };
          const claims = [...ledger.claims].sort((a, b) => (order[a.status] ?? 5) - (order[b.status] ?? 5));
          return (
            <section className="rp-card reconcile">
              <h3>
                Reconciliation{" "}
                <span className="muted">
                  · {snapshot ? `evidence ${handleOf(review.reviewer)}'s verdict was based on` : "does the change do what its author said?"}
                </span>
              </h3>
              <div className="recon-summary">
                <span className="rc supported">{supported} verified</span>
                {selfAtt > 0 && <span className="rc self">{selfAtt} self-attested</span>}
                <span className="rc contradicted">{contradicted} contradicted</span>
                <span className="rc unsupported">{needs} needs judgment</span>
              </div>
              {contradicted > 0 && (
                <p className="recon-warn">⚠ {contradicted} claim{contradicted > 1 ? "s" : ""} the change's own facts contradict — do not merge without resolving.</p>
              )}
              {(() => {
                // F1: unverified/contradicted are primary; verified rows collapse by default.
                const isVerified = (s: string) => s === "verified_mechanically" || s === "verified_read_only";
                const primary = claims.filter((c) => !isVerified(c.status));
                const verified = claims.filter((c) => isVerified(c.status));
                // One traceable row: status → claim → evidence → (resolution / action).
                const row = (c: (typeof claims)[number]) => (
                  <li key={c.id} className={"claim " + c.status}>
                    <div className="claim-head">
                      <span className={"cstat " + c.status} title={(meta[c.status] ?? ["?", c.status])[1]}>{(meta[c.status] ?? ["?"])[0]}</span>
                      <span className="claim-text">{c.text}</span>
                      <span className="claim-status-label">{(meta[c.status] ?? ["", c.status])[1]}</span>
                      <span className="claim-src">{c.source}</span>
                    </div>
                    {c.evidence.length > 0 && (
                      <ul className="claim-ev">
                        {c.evidence.map((e, i) => (
                          <li key={i} className={e.supports ? "ok" : "bad"}>
                            <span className="ev-kind">{e.kind}</span> {e.detail}
                          </li>
                        ))}
                      </ul>
                    )}
                    {resolutions[c.id] ? (
                      <div className={"claim-resolution " + resolutions[c.id].judgment}>
                        {resolutions[c.id].judgment === "verified" ? "✓ verified by a human" : "⚑ concern raised"} · <b>{resolutions[c.id].by}</b>
                        {resolutions[c.id].note && <span className="res-note"> — {resolutions[c.id].note}</span>}
                      </div>
                    ) : (c.status === "needs_judgment" || c.status === "self_attested") ? (
                      <div className="claim-actions">
                        <span className="ca-prompt">a human must judge this:</span>
                        <button className="ca ok" disabled={!canAct} onClick={() => resolveClaim(c.id, "verified")}>✓ I checked — verified</button>
                        <button className="ca bad" disabled={!canAct} onClick={() => resolveClaim(c.id, "concern")}>⚑ raise concern</button>
                      </div>
                    ) : null}
                  </li>
                );
                return (
                  <>
                    <ul className="recon-claims">
                      {primary.length === 0 && <li className="claim-none muted">nothing needs attention — all claims verified</li>}
                      {primary.map(row)}
                    </ul>
                    {verified.length > 0 && (
                      <details className="verified-fold">
                        <summary>✓ {verified.length} verified claim{verified.length > 1 ? "s" : ""} <span className="muted">— show</span></summary>
                        <ul className="recon-claims">{verified.map(row)}</ul>
                      </details>
                    )}
                  </>
                );
              })()}
            </section>
          );
        })()}

        {review.findings?.length > 0 && (() => {
          const rank: Record<string, number> = { blocker: 0, warn: 1, info: 2 };
          const ranked = [...review.findings].sort((a, b) => (rank[a.severity] ?? 3) - (rank[b.severity] ?? 3));
          const counts = ranked.reduce((m: Record<string, number>, f) => ({ ...m, [f.severity]: (m[f.severity] ?? 0) + 1 }), {});
          return (
            <section className="rp-card">
              <h3>
                Findings <span className="muted">· risk-ranked</span>
                <span className="find-tally">
                  {counts.blocker ? <span className="sev blocker">{counts.blocker} blocker</span> : null}
                  {counts.warn ? <span className="sev warn">{counts.warn} warn</span> : null}
                  {counts.info ? <span className="sev info">{counts.info} info</span> : null}
                </span>
              </h3>
              <ul className="rp-findings">
                {ranked.map((f, i) => (
                  <li key={i} className={"sev-row " + f.severity}>
                    <span className={"sev " + f.severity}>{f.severity}</span>
                    {f.path && <code>{f.path}{f.line ? `:${f.line}` : ""}</code>}
                    <span className="fnote">{f.note}</span>
                    {f.severity !== "info" && pr && f.path && (
                      <button className="fix-ai" disabled={!canAct || fixing === i} onClick={() => fixWithAI(i, f)}>
                        {fixing === i ? "fixing…" : "✨ Fix with AI"}
                      </button>
                    )}
                  </li>
                ))}
              </ul>
            </section>
          );
        })()}

        <section className="rp-card">
          <h3>Proposed change</h3>
          {change ? (
            <>
              <p>
                <span className="blob">⬡ {change.id.slice(0, 12)}</span> · {change.intent} ·{" "}
                <span className="muted">{change.author.split(" ")[0]}</span>
              </p>
              <h4>
                What it touches <span className="muted">({change.files.length} files — from keel)</span>
              </h4>
              <ul className="rp-files">
                {change.files.map((f) => (
                  <li key={f.path}>
                    <span className={"fst " + f.status}>{f.status[0].toUpperCase()}</span> <code>{f.path}</code>
                  </li>
                ))}
              </ul>
            </>
          ) : (
            <p className="muted">resolving the change from keel…</p>
          )}
        </section>

        <section className="rp-card">
          <h3>
            Changes <span className="muted">({diff.length} files) · what changed, as operations</span>
          </h3>
          {diff.length === 0 && <p className="muted">no textual changes (or binary)</p>}
          {diff.length > 0 &&
            (() => {
              const allOps = [...new Set(diff.flatMap((f) => f.ops))];
              const count = (t: string) => diff.reduce((s, f) => s + f.hunks.reduce((x, h) => x + h.lines.filter((l) => l.tag === t).length, 0), 0);
              return (
                <div className="ops-summary">
                  <div className="ops-title">
                    Semantic operations <span className="shape">· {diff.length} files · +{count("add")} / -{count("del")}</span>
                  </div>
                  {allOps.length > 0 ? (
                    <div className="ops-list">
                      {allOps.map((o, i) => (
                        <span className={"op " + (o.startsWith("removed") ? "del" : "add")} key={i}>{o}</span>
                      ))}
                    </div>
                  ) : (
                    <div className="muted">no signature-level operations — see the line diff below</div>
                  )}
                </div>
              );
            })()}
          {diff.map((f) => (
            <div className="fdiff" key={f.path}>
              <button className="fdiff-head" onClick={() => setOpenFile(openFile === f.path ? null : f.path)}>
                <span className={"fst " + f.status}>{f.status[0].toUpperCase()}</span>
                <code>{f.path}</code>
                {f.ops.length > 0 && (
                  <span className="ops">
                    {f.ops.slice(0, 5).map((o, i) => (
                      <span className={"op " + (o.startsWith("removed") ? "del" : "add")} key={i}>{o}</span>
                    ))}
                  </span>
                )}
                <span className="expand">{openFile === f.path ? "hide diff ▾" : "diff ▸"}</span>
              </button>
              {openFile === f.path && (
                <div className="hunks">
                  {f.hunks.map((h, i) => (
                    <div className="hunk" key={i}>
                      <div className="hunk-h">@@ -{h.old_start} +{h.new_start} @@</div>
                      {h.lines.map((l, j) => (
                        <div className={"dl " + l.tag} key={j}>
                          <span className="mark">{l.tag === "add" ? "+" : l.tag === "del" ? "-" : " "}</span>
                          {l.text}
                        </div>
                      ))}
                    </div>
                  ))}
                </div>
              )}
            </div>
          ))}
        </section>

        <section className="rp-card">
          <h3>Tests &amp; CI · risk</h3>
          <p>
            keel verification: <span className={"verif " + verification}>{verification}</span> · risk: <b>{risk}</b>
          </p>
          <div className="verify-actions">
            <button className="act run-checks" disabled={checking} onClick={() => runChecks(false)}>
              {checking ? "running checks…" : "Run checks"}
            </button>
            {checkResult && (
              <span className={"check-result " + checkResult.status}>
                {checkResult.status}
                {checkResult.memoized && <span className="memo-tag">memoized</span>}
                <span className="check-summary">{checkResult.summary}</span>
              </span>
            )}
          </div>
          <p className="muted checks-note">
            Checks run the change's own tree in a fresh checkout and memoize by content — an unchanged
            tree is an instant cache hit. The result writes back to keel verification above.
            {checkResult && (
              <>
                {" "}
                <button className="linklike" disabled={checking} onClick={() => runChecks(true)}>re-run (bypass memo)</button>
              </>
            )}
          </p>
          <div className="verify-actions">
            <button className="act close" onClick={() => verify(true)}>Override: green</button>
            <button className="act reopen" onClick={() => verify(false)}>Override: red</button>
          </div>
        </section>

        {change?.session ? (
          <section className="rp-card">
            <h3>
              Session <span className="muted">the agent session behind this change</span>
            </h3>
            <p><b>task:</b> {change.session.task}</p>
            <p>
              <b>model:</b> {change.session.model || "—"}
              {change.session.lesson && <> · <b>lesson:</b> <i>{change.session.lesson}</i></>}
            </p>
            {(change.session.tool_calls > 0 || change.session.tokens_out > 0) && (
              <p className="muted">
                agent-session totals: {change.session.tool_calls} tool calls · {change.session.tokens_in}/
                {change.session.tokens_out} tokens (spans the whole run, not just this change)
              </p>
            )}
          </section>
        ) : (
          <section className="rp-card muted-card">
            <h3>Session <span className="muted">reasoning · operations · tokens</span></h3>
            <p className="muted">
              No keel session is linked to this change — it was pushed as a plain git commit. Commit with{" "}
              <code>keel commit --session</code> or <code>keel capture</code> and the task, reasoning, tool calls,
              and lesson show up here automatically.
            </p>
          </section>
        )}

        {pr && (
          <section className="rp-card">
            <h3>Discussion <span className="muted">humans and agents, one accountable thread</span></h3>
            <div className="pr-thread">
              {thread.sort((a, b) => a.created_unix - b.created_unix).map((c) => (
                <div className="cmt" key={c.id}>
                  <b className={actors.find((a) => a.id === c.author)?.kind ?? ""}>{handleOf(c.author)}</b>
                  <span className="cmt-body">{c.body}</span>
                </div>
              ))}
              {thread.length === 0 && <div className="muted cmt-empty">no comments yet</div>}
              <div className="cmt-form">
                <input
                  placeholder={canAct ? "comment…" : "sign in to comment"}
                  disabled={!canAct}
                  value={draft}
                  onChange={(e) => setDraft(e.target.value)}
                  onKeyDown={(e) => e.key === "Enter" && postThreadComment()}
                />
                <button disabled={!canAct} onClick={postThreadComment}>Comment</button>
              </div>
            </div>
          </section>
        )}
        </div>
      </div>
    </div>
  );
}

function renderEvent(e: ActivityEvent) {
  switch (e.kind) {
    case "agent_brief":
      return (
        <>
          <b>{e.actor}</b> is working in <b>{e.repo}</b> · <code>{e.file}</code>
          <span className="task"> — {e.task}</span>
        </>
      );
    case "lesson":
      return (
        <>
          lesson in <b>{e.repo}</b> · <code>{e.file}</code>: <i>{e.lesson}</i>
        </>
      );
    case "push":
      return (
        <>
          <b>{e.actor}</b> pushed to <b>{e.repo}</b> · <code>{e.change}</code>
        </>
      );
    case "issue":
      return (
        <>
          <b>{e.actor}</b> {e.action} issue #{e.number} in <b>{e.repo}</b>
        </>
      );
  }
}
