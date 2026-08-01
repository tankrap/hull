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
type Review = { id: string; target: string; reviewer: string; verdict: string; summary: string; findings: Finding[]; ledger?: LedgerSnap };
type CodeRef = { repo: string; blob: string; path: string; line_start: number; line_end?: number };
type Issue = {
  number: number;
  title: string;
  body: string;
  author: string;
  assignees: string[];
  status: { state: string; reason?: string };
  code_refs: CodeRef[];
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

  // Issues for the selected repo under the selected tenant (M2). Click a repo card to switch.
  const [issueRepo, setIssueRepo] = useState<string>("hull");
  const [issues, setIssues] = useState<Issue[]>([]);
  const [prov, setProv] = useState<Record<string, { change: string; intent: string; author: string }[]>>({});

  // Notifications recorded by the core Notifier plugin capability (poll).
  const [notifs, setNotifs] = useState<{ kind: string; to: string[]; summary: string; ts: number; broadcast?: boolean }[]>([]);
  const [showNotifs, setShowNotifs] = useState(false);

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

  const transition = async (number: number, action: "close" | "reopen") => {
    if (!canAct) return alert("Sign in to act.");
    await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/issues/${number}`, {
      method: "PATCH",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({ action, actor: actingAs, ...(action === "close" ? { reason: "completed" } : {}) }),
    });
    loadIssues();
  };

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
    <div className="pr-thread">
      {comments.filter((c) => c.target === target).sort((a, b) => a.created_unix - b.created_unix).map((c) => (
        <div className="cmt" key={c.id}>
          <b className={actors.find((a) => a.id === c.author)?.kind ?? ""}>{handleOf(c.author)}</b>
          <span className="cmt-body">{c.body}</span>
        </div>
      ))}
      {comments.filter((c) => c.target === target).length === 0 && <div className="muted cmt-empty">no comments yet</div>}
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
    <div className="app">
      <header className="top">
        <button className="brand" onClick={() => setView("home")} title="home">
          <span className="logo">⬡</span> Hull
        </button>
        <div className="breadcrumb">
          {view === "repo" ? (
            <>
              <button className="link" onClick={() => setView("home")}>{tenant}</button>
              <span className="sep">/</span>
              <b>{issueRepo}</b>
            </>
          ) : (
            <span className="tag">situation room</span>
          )}
        </div>
        <div className="spacer" />
        <label className="tenant">
          tenant&nbsp;
          <input
            value={tenant}
            onChange={(e) => setTenant(e.target.value.trim())}
            spellCheck={false}
            aria-label="tenant"
          />
        </label>
        <div className="signin">
          {me ? (
            <span className="signed-in">
              signed in as{" "}
              <button className={"whoami " + me.kind} onClick={() => setShowProfile((s) => !s)} title="your identity & accountability">
                {me.handle} ▾
              </button>
              <button className="link" onClick={signOut}>sign out</button>
              {showProfile && profile && (
                <div className="profile-drop">
                  <div className="profile-head">
                    <b className={profile.kind}>{profile.handle}</b> <span className="muted">{profile.kind}</span>
                    {profile.accountable && <span className="badge ok">accountable</span>}
                  </div>
                  <div className="profile-row">
                    <span className="pk-label">actor id (public key)</span>
                    <code className="pk" title="your Ed25519 public key — this IS your identity; it can't be rotated without becoming a different actor">{profile.id}</code>
                  </div>
                  {profile.kind === "agent" && profile.delegation.length > 0 && (
                    <div className="profile-row">
                      <span className="pk-label">accountability chain</span>
                      <span className="chain">
                        {profile.delegation.map((h, i) => (
                          <span key={i} className="hop">
                            <b className={h.kind}>{h.handle}</b>
                            {i < profile.delegation.length - 1 && <span className="arrow"> → </span>}
                          </span>
                        ))}
                      </span>
                    </div>
                  )}
                  <div className="profile-row">
                    <span className="pk-label">memberships</span>
                    {profile.memberships.length > 0 ? (
                      <span className="memberships">
                        {profile.memberships.map((m, i) => (
                          <span key={i} className="mem">{m.account}<span className="role">{m.role}</span></span>
                        ))}
                      </span>
                    ) : (
                      <span className="muted">none</span>
                    )}
                  </div>
                  {profile.kind === "human" && (
                    <div className="profile-actions">
                      <button className="mint-agent" onClick={createAgent}>+ create an agent (chains to you)</button>
                    </div>
                  )}
                </div>
              )}
            </span>
          ) : (
            <>
              <input
                type="password"
                placeholder="secret key to sign in"
                value={secretInput}
                onChange={(e) => setSecretInput(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && signIn()}
              />
              <button onClick={signIn}>Sign in</button>
              <button className="link" onClick={registerAndSignIn}>new identity</button>
              <button className="link" onClick={() => signInWith(DEMO_OWNER_SECRET)} title="log in as the published demo owner (real signature login)">demo</button>
            </>
          )}
        </div>
        <div className="bell-wrap">
          <button className="bell" onClick={() => setShowNotifs((s) => !s)} title="notifications">
            🔔{notifs.length > 0 && <span className="bell-count">{notifs.length}</span>}
          </button>
          {showNotifs && (
            <div className="notif-drop">
              <div className="notif-head">
                inbox for <b>{handleOf(actingAs)}</b> <span className="muted">· via Notifier plugin</span>
              </div>
              {notifs.length === 0 && <div className="empty">nothing yet</div>}
              {notifs.slice(0, 15).map((n, i) => {
                const icon =
                  n.kind === "review_posted" ? "✍" :
                  n.kind === "review_requested" ? "👀" :
                  n.kind === "ci_passed" ? "✓" :
                  n.kind === "ci_failed" ? "✗" :
                  n.kind === "code_owner_referenced" ? "⬡" :
                  n.kind === "mirror_pushed" ? "⇄" : "•";
                return (
                  <div className="notif" key={i}>
                    <span className={"nk " + n.kind}>{icon} {n.kind.replace(/_/g, " ")}</span>
                    {n.broadcast && <span className="nbcast">team</span>}
                    <span className="ns">{n.summary}</span>
                  </div>
                );
              })}
            </div>
          )}
        </div>
      </header>

      {view === "home" && org && (
        <div className="org-card">
          <span className="org-name">{org.handle}</span>
          <span className="muted">{org.kind}</span>
          <span className="org-members">
            {org.members.map((m, i) => (
              <span className="mem" key={i}>
                {m.handle}<span className="role">{m.role}</span>
              </span>
            ))}
          </span>
        </div>
      )}
      {view === "home" && (
      <main className="grid">
        <section>
          <h2>Repositories <span className="muted">by live activity — click one to open it</span></h2>
          <div className="repos">
            {repos.length === 0 && (
              <div className="empty">
                no active repos for <b>{tenant}</b> — host one:{" "}
                <code>git push http://localhost:8930/{tenant}/&lt;repo&gt; main</code>
              </div>
            )}
            {repos.map((r) => (
              <article
                className={"repo" + (r.repo === issueRepo ? " selected" : "")}
                key={r.repo}
                onClick={() => selectRepo(r.repo)}
                title="open this repo's issues"
              >
                <div className="repo-head">
                  <span className="repo-name">{r.repo}</span>
                  <span className="score" title="live activity score">{r.score.toFixed(0)}</span>
                </div>
                {r.active_actors.length > 0 && (
                  <div className="actors">
                    {r.active_actors.map((a) => (
                      <span className={"chip " + (a.startsWith("agent") ? "agent" : "human")} key={a}>
                        {a}
                      </span>
                    ))}
                  </div>
                )}
                {r.hot_files.length > 0 && (
                  <ul className="files">
                    {r.hot_files.map((f) => (
                      <li key={f}><code>{f}</code></li>
                    ))}
                  </ul>
                )}
              </article>
            ))}
          </div>
        </section>

        <section>
          <h2>Live feed</h2>
          <ul className="feed">
            {events.length === 0 && <li className="empty">listening…</li>}
            {events.map((e, i) => (
              <li key={i} className={"ev ev-" + e.kind}>
                {renderEvent(e)}
              </li>
            ))}
          </ul>
        </section>
      </main>
      )}

      {view === "repo" && (
      <main className="repo-view">
        {secrets.length > 0 && (
          <div className="sec-banner">
            <b>⚠ {secrets.length} secret{secrets.length > 1 ? "s" : ""} detected on push</b>
            <ul>
              {secrets.slice(0, 5).map((s, i) => (
                <li key={i}>
                  {s.title} — <code>{s.path}:{s.line}</code> <span className="muted">{s.redacted}</span>
                </li>
              ))}
            </ul>
          </div>
        )}
        {mirror?.target && (
          <div className="mirror-panel">
            <span className="mirror-badge">⇄ mirrored</span>
            <span>
              linked to <code>{mirror.target}</code> · {mirror.outbound.length} change{mirror.outbound.length === 1 ? "" : "s"} pushed outbound
            </span>
            <span className="muted mirror-note">loop-safe: forge-originated changes are never pushed back; webhook redelivery is idempotent</span>
          </div>
        )}
        {ciConfig && (
          <div className="ci-panel">
            <span className="ci-badge">⚙ CI</span>
            <span>
              {ciConfig.url ? (
                <>dispatches to <code>{ciConfig.url}</code> <span className="muted">({ciConfig.source}{ciConfig.has_secret ? ", secret set" : ", no secret"})</span></>
              ) : (
                <span className="muted">{ciConfig.source} — checks run on the built-in local runner</span>
              )}
            </span>
            {isTenantOwner && (
              <form className="ci-form" onSubmit={(e) => { e.preventDefault(); saveCiConfig(false); }}>
                <input className="ci-url" placeholder="https://your-ci/hull" value={ciUrl} onChange={(e) => setCiUrl(e.target.value)} spellCheck={false} />
                <input className="ci-secret" type="text" placeholder="shared secret (optional)" value={ciSecret} onChange={(e) => setCiSecret(e.target.value)} spellCheck={false} />
                <button type="button" className="link" title="generate a random 32-byte secret" onClick={() => setCiSecret(bytesToHex(crypto.getRandomValues(new Uint8Array(32))))}>generate</button>
                <button type="submit">Set</button>
                {ciConfig.source === "repo" && <button type="button" className="link" onClick={() => saveCiConfig(true)}>clear</button>}
                <a className="ci-spec-link" href="https://github.com/tankrap/hull/blob/main/CI-SPEC.md" target="_blank" rel="noreferrer">spec ↗</a>
              </form>
            )}
          </div>
        )}
        <div className="tabs">
          <button className={"tab" + (tab === "issues" ? " active" : "")} onClick={() => setTab("issues")}>
            Issues <span className="muted">{issues.filter((i) => i.status.state === "open").length}</span>
          </button>
          <button className={"tab" + (tab === "prs" ? " active" : "")} onClick={() => setTab("prs")}>
            Pull requests <span className="muted">{prs.length}</span>
          </button>
          <input
            className="repo-search"
            placeholder={`filter ${tab === "issues" ? "issues" : "pull requests"}…`}
            value={q}
            onChange={(e) => setQ(e.target.value)}
            spellCheck={false}
          />
          {!canAct && (
            <span className="acting-note">
              read-only — <button className="link" onClick={() => signInWith(DEMO_OWNER_SECRET)}>sign in</button> to act
            </span>
          )}
        </div>

        {tab === "issues" && (
        <section className="issues">
        <div className="view-toggle">
          <button className={issueView === "list" ? "on" : ""} onClick={() => setIssueView("list")}>List</button>
          <button className={issueView === "board" ? "on" : ""} onClick={() => setIssueView("board")}>Board</button>
        </div>
        <form className="issue-form" onSubmit={createIssue}>
          <input
            placeholder="Open an issue…"
            value={form.title}
            onChange={(e) => setForm({ ...form, title: e.target.value })}
          />
          <input
            className="path"
            placeholder="path (optional, e.g. crates/hull-server/src/quic.rs)"
            value={form.path}
            onChange={(e) => setForm({ ...form, path: e.target.value })}
            spellCheck={false}
          />
          <input
            className="line"
            placeholder="line"
            value={form.line}
            onChange={(e) => setForm({ ...form, line: e.target.value })}
          />
          <select
            className="assignee-pick"
            value={form.assignee}
            onChange={(e) => setForm({ ...form, assignee: e.target.value })}
          >
            <option value="">assign…</option>
            {actors.map((a) => (
              <option key={a.id} value={a.id}>
                {a.handle}
              </option>
            ))}
          </select>
          <button type="submit">Open</button>
        </form>
        {issueView === "list" ? (
        <ul className="issue-list">
          {issues.length === 0 && <li className="empty">no issues yet — open one above</li>}
          {[...issues]
            .filter((it) => matchQ(`${it.title} ${it.body} #${it.number}`))
            .sort((a, b) => Number(a.status.state !== "open") - Number(b.status.state !== "open") || b.number - a.number)
            .map((it) => (
            <li key={it.number} className={"issue " + it.status.state}>
              <div className="issue-row">
                <span className={"state " + it.status.state} title={it.status.reason ?? ""}>
                  {it.status.state === "open" ? "open" : it.status.reason ?? "closed"}
                </span>
                <span className="num">#{it.number}</span>
                <button
                  className="it-title"
                  onClick={() => setOpenIssue(openIssue === it.number ? null : it.number)}
                  title="open issue"
                >
                  {openIssue === it.number ? "▾ " : "▸ "}
                  {it.title}
                </button>
                {it.code_refs.map((c, i) => (
                  <button
                    key={i}
                    className="coderef"
                    title={`content-addressed → keel blob ${c.blob} · click for provenance`}
                    onClick={() => showWhy(`${it.number}:${c.path}`, c.path)}
                  >
                    <code>
                      {c.path}:{c.line_start}
                      {c.line_end ? `-${c.line_end}` : ""}
                    </code>
                    <span className="blob">⬡ {c.blob.slice(0, 10)}</span>
                  </button>
                ))}
                {it.assignees.map((id) => (
                  <span key={id} className="assignee-chip" title="assignee">
                    ◎ {handleOf(id)}
                  </span>
                ))}
                {it.resolved_by && (
                  <span className="resolved-chip" title="closed by a merged PR — resolving keel change">
                    ⬡ resolved by {it.resolved_by.slice(0, 10)}
                  </span>
                )}
                {!it.resolved_by && (it.linked_prs?.length ?? 0) > 0 && (
                  <span className="linked-chip" title="a PR references this issue">
                    ⇄ {it.linked_prs!.length} linked PR{it.linked_prs!.length > 1 ? "s" : ""}
                  </span>
                )}
                <span className={"by " + (actors.find((a) => a.id === it.author)?.kind ?? "")}>
                  {handleOf(it.author)}
                </span>
                {it.status.state === "open" ? (
                  <button className="act close" onClick={() => transition(it.number, "close")}>
                    Close
                  </button>
                ) : (
                  <button className="act reopen" onClick={() => transition(it.number, "reopen")}>
                    Reopen
                  </button>
                )}
              </div>
              {openIssue === it.number && (
                <div className="issue-detail">
                  <div className="meta">
                    <span>opened by <b className={actors.find((a) => a.id === it.author)?.kind ?? ""}>{handleOf(it.author)}</b></span>
                    {it.assignees.length > 0 && (
                      <span>· assigned to {it.assignees.map((id) => handleOf(id)).join(", ")}</span>
                    )}
                    <span>· {it.status.state === "open" ? "open" : `closed (${it.status.reason ?? "closed"})`}</span>
                  </div>
                  {it.body && <p className="body">{it.body}</p>}
                  {it.code_refs.length === 0 && <p className="muted">no code references</p>}
                  {it.code_refs.length > 0 && (
                    <p className="muted">code references (click a ⬡ anchor above for keel provenance)</p>
                  )}
                  <div className="thread-wrap">
                    <h5>Discussion</h5>
                    <Thread target={`issue:${it.number}`} />
                  </div>
                </div>
              )}
              {it.code_refs.map((c) => {
                const key = `${it.number}:${c.path}`;
                return prov[key] ? (
                  <ul className="prov" key={key}>
                    <li className="prov-head">
                      keel provenance · <code>{c.path}</code>
                    </li>
                    {prov[key].length === 0 && <li className="empty">no recorded history</li>}
                    {prov[key].map((p, j) => (
                      <li key={j}>
                        <code className="ch">{p.change.slice(0, 10)}</code>
                        <span className="intent">{p.intent}</span>
                        <span className="by">{p.author}</span>
                      </li>
                    ))}
                  </ul>
                ) : null;
              })}
            </li>
          ))}
        </ul>
        ) : (
        <div className="board">
          {[
            { k: "open", label: "Open" },
            { k: "completed", label: "Completed" },
            { k: "not_planned", label: "Not planned" },
            { k: "cancelled", label: "Cancelled" },
            { k: "duplicate", label: "Duplicate" },
          ].map((col) => {
            const inCol = issues.filter((i) => (i.status.state === "open" ? "open" : i.status.reason) === col.k && matchQ(`${i.title} ${i.body} #${i.number}`));
            if (col.k !== "open" && inCol.length === 0) return null;
            return (
              <div className="col" key={col.k}>
                <div className="col-head">
                  {col.label} <span className="muted">{inCol.length}</span>
                </div>
                {inCol.map((it) => (
                  <div
                    className="card"
                    key={it.number}
                    onClick={() => {
                      setIssueView("list");
                      setOpenIssue(it.number);
                    }}
                  >
                    <div className="card-num">#{it.number}</div>
                    <div className="card-title">{it.title}</div>
                    {it.assignees.length > 0 && (
                      <div className="card-assignees">◎ {it.assignees.map((id) => handleOf(id)).join(", ")}</div>
                    )}
                  </div>
                ))}
              </div>
            );
          })}
        </div>
        )}
        </section>
        )}

        {tab === "prs" && (
        <section className="issues prs">
        <form className="issue-form" onSubmit={createPr}>
          <input
            placeholder="Open a PR from HEAD…"
            value={prTitle}
            onChange={(e) => setPrTitle(e.target.value)}
          />
          <button type="submit">Open PR</button>
        </form>
        <ul className="issue-list">
          {prs.length === 0 && <li className="empty">no pull requests yet</li>}
          {[...prs].filter((p) => matchQ(`${p.title} #${p.number}`)).sort((a, b) => b.number - a.number).map((p) => {
            const prReviews = reviews.filter((r) => r.target === `pr:${p.number}`);
            return (
            <li key={p.number} className="issue">
              <div className="issue-row">
                <span className={"verif " + (p.state === "merged" ? "merged" : p.verification)}>
                  {p.state === "merged" ? "merged" : p.verification}
                </span>
                <span className="num">!{p.number}</span>
                <button
                  className="it-title"
                  onClick={() => setOpenPr(openPr === p.number ? null : p.number)}
                  title="open reviews"
                >
                  {openPr === p.number ? "▾ " : "▸ "}
                  {p.title}
                </button>
                <span className="coderef" title={`proposes keel change ${p.changes[0]}`}>
                  <span className="blob">⬡ {(p.changes[0] ?? "").slice(0, 10)}</span>
                </span>
                {prReviews.length > 0 && (
                  <span className="review-count" title="reviews">{prReviews.length} review{prReviews.length > 1 ? "s" : ""}</span>
                )}
                {p.reviewers?.length > 0 && (
                  <span className="owners-chip" title="code owners auto-requested">◎ {p.reviewers.map((id) => handleOf(id)).join(", ")}</span>
                )}
                <span className={"by " + (actors.find((a) => a.id === p.author)?.kind ?? "")}>
                  {handleOf(p.author)}
                </span>
              </div>
              {openPr === p.number && (
                <div className="reviews">
                  <div className="merge-bar">
                    {p.state === "merged" ? (
                      <span className="merged-note">✓ merged</span>
                    ) : (
                      <>
                        <button className="merge-btn" onClick={() => mergePr(p.number)}>Merge</button>
                        <span className="muted">
                          gate: keel-verify green + an approving review from someone other than the author
                        </span>
                      </>
                    )}
                  </div>
                  {prReviews.length === 0 && <p className="muted">no reviews yet</p>}
                  {prReviews.map((r) => (
                    <button className="review clickable" key={r.id} onClick={() => setOpenReview(r)} title="open review">
                      <span className={"verdict " + r.verdict}>{r.verdict.replace("_", " ")}</span>
                      <b className={actors.find((a) => a.id === r.reviewer)?.kind ?? ""}>{handleOf(r.reviewer)}</b>
                      <span className="rv-summary">{r.summary || "open review →"}</span>
                      {r.findings?.length > 0 && (
                        <span className="find-count">{r.findings.length} finding{r.findings.length > 1 ? "s" : ""}</span>
                      )}
                    </button>
                  ))}
                  <div className="auto-review-bar">
                    <button className="auto-review-btn" disabled={autoReviewing === p.number} onClick={() => autoReview(p.number)}>
                      {autoReviewing === p.number ? "agent reviewing…" : "⬡ Agent auto-review"}
                    </button>
                    <span className="muted">runs checks + reconciles the change's claims, then posts an accountable agent review</span>
                  </div>
                  <div className="review-form">
                    <select
                      value={reviewForm.verdict}
                      onChange={(e) => setReviewForm({ ...reviewForm, verdict: e.target.value })}
                    >
                      <option value="approve">approve</option>
                      <option value="request_changes">request changes</option>
                      <option value="reject">reject</option>
                      <option value="comment">comment</option>
                    </select>
                    <input
                      placeholder={`review as ${handleOf(actingAs)}…`}
                      value={reviewForm.summary}
                      onChange={(e) => setReviewForm({ ...reviewForm, summary: e.target.value })}
                    />
                    <button onClick={() => submitReview(p.number)}>Submit review</button>
                    <div className="finding-row">
                      <span className="muted">finding (optional):</span>
                      <input
                        className="fp"
                        placeholder="path"
                        value={reviewForm.findPath}
                        onChange={(e) => setReviewForm({ ...reviewForm, findPath: e.target.value })}
                        spellCheck={false}
                      />
                      <select value={reviewForm.findSev} onChange={(e) => setReviewForm({ ...reviewForm, findSev: e.target.value })}>
                        <option value="info">info</option>
                        <option value="warn">warn</option>
                        <option value="blocker">blocker</option>
                      </select>
                      <input
                        className="fn"
                        placeholder="what's wrong"
                        value={reviewForm.findNote}
                        onChange={(e) => setReviewForm({ ...reviewForm, findNote: e.target.value })}
                      />
                    </div>
                  </div>
                  <div className="thread-wrap">
                    <h5>Discussion <span className="muted">humans and agents, one accountable thread</span></h5>
                    <Thread target={`pr:${p.number}`} />
                  </div>
                </div>
              )}
            </li>
            );
          })}
        </ul>
        </section>
        )}
      </main>
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
    <div className="app review-page">
      <header className="top">
        <button className="back" onClick={onBack}>
          ← situation room
        </button>
        <div className="tag">review package · synthesized understanding, not a diff</div>
      </header>
      <main className="review-main">
        <div className="rp-head">
          <span className={"verdict " + review.verdict}>{review.verdict.replace("_", " ")}</span>
          <h1>{pr ? `PR !${pr.number} · ${pr.title}` : review.target}</h1>
        </div>

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
        </section>

        {shownLedger && shownLedger.claims.length > 0 && (() => {
          const ledger = shownLedger;
          const n = (s: string) => ledger.claims.filter((c) => c.status === s).length;
          const contradicted = n("contradicted");
          return (
            <section className="rp-card reconcile">
              <h3>
                Reconciliation{" "}
                <span className="muted">
                  · {snapshot ? `evidence ${handleOf(review.reviewer)}'s verdict was based on` : "does the change do what its author said?"}
                </span>
              </h3>
              <div className="recon-summary">
                <span className="rc supported">{n("supported")} supported</span>
                <span className="rc contradicted">{contradicted} contradicted</span>
                <span className="rc unsupported">{n("unsupported")} unconfirmed</span>
              </div>
              {contradicted > 0 && (
                <p className="recon-warn">⚠ {contradicted} claim{contradicted > 1 ? "s" : ""} the change's own facts contradict — do not merge without resolving.</p>
              )}
              <ul className="recon-claims">
                {ledger.claims.map((c) => (
                  <li key={c.id} className={"claim " + c.status}>
                    <div className="claim-head">
                      <span className={"cstat " + c.status}>{c.status === "supported" ? "✓" : c.status === "contradicted" ? "✗" : "?"}</span>
                      <span className="claim-text">{c.text}</span>
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
                  </li>
                ))}
              </ul>
            </section>
          );
        })()}

        {review.findings?.length > 0 && (
          <section className="rp-card">
            <h3>Findings <span className="muted">({review.findings.length})</span></h3>
            <ul className="rp-findings">
              {review.findings.map((f, i) => (
                <li key={i}>
                  <span className={"sev " + f.severity}>{f.severity}</span>
                  <code>{f.path}{f.line ? `:${f.line}` : ""}</code>
                  <span className="fnote">{f.note}</span>
                </li>
              ))}
            </ul>
          </section>
        )}

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
      </main>
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
