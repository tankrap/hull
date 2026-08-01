import { useEffect, useRef, useState } from "react";

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
type PR = { number: number; title: string; author: string; changes: string[]; verification: string };
type Review = { id: string; target: string; reviewer: string; verdict: string; summary: string };
type CodeRef = { repo: string; blob: string; path: string; line_start: number; line_end?: number };
type Issue = {
  number: number;
  title: string;
  body: string;
  author: string;
  assignees: string[];
  status: { state: string; reason?: string };
  code_refs: CodeRef[];
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
  const [notifs, setNotifs] = useState<{ kind: string; to: string[]; summary: string; ts: number }[]>([]);
  const [showNotifs, setShowNotifs] = useState(false);
  useEffect(() => {
    const load = () =>
      fetch("/api/notifications").then((r) => r.json()).then((d) => setNotifs(d.notifications ?? [])).catch(() => {});
    load();
    const t = setInterval(load, 4000);
    return () => clearInterval(t);
  }, []);

  // Registered actors + who we're acting as (every authoring action must be an accountable actor).
  const [actors, setActors] = useState<Actor[]>([]);
  const [actingAs, setActingAs] = useState<string>("");
  useEffect(() => {
    fetch("/api/actors")
      .then((r) => r.json())
      .then((d) => {
        const list: Actor[] = d.actors ?? [];
        setActors(list);
        setActingAs((cur) => cur || list.find((a) => a.kind === "human")?.id || list[0]?.id || "");
      })
      .catch(() => {});
  }, []);
  const handleOf = (id: string) => actors.find((a) => a.id === id)?.handle ?? id.slice(0, 8);

  // Navigation feel: clicking a repo scrolls to its issues; clicking an issue expands its detail.
  const issuesRef = useRef<HTMLElement>(null);
  const [openIssue, setOpenIssue] = useState<number | null>(null);
  const selectRepo = (repo: string) => {
    setIssueRepo(repo);
    setOpenIssue(null);
    setTimeout(() => issuesRef.current?.scrollIntoView({ behavior: "smooth", block: "start" }), 50);
  };

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
    await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/issues/${number}`, {
      method: "PATCH",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ action, actor: actingAs, ...(action === "close" ? { reason: "completed" } : {}) }),
    });
    loadIssues();
  };

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
  // Reviews (first-class), loaded per repo and filtered to a PR target.
  const [reviews, setReviews] = useState<Review[]>([]);
  const [openPr, setOpenPr] = useState<number | null>(null);
  const [openReview, setOpenReview] = useState<Review | null>(null);
  const [reviewForm, setReviewForm] = useState({ verdict: "approve", summary: "" });
  const loadReviews = () =>
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/reviews`)
      .then((r) => r.json())
      .then((d) => setReviews(d.reviews ?? []))
      .catch(() => {});
  useEffect(() => {
    loadReviews();
  }, [tenant, issueRepo]);
  const submitReview = async (prNumber: number) => {
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/reviews`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        target: `pr:${prNumber}`,
        reviewer: actingAs,
        verdict: reviewForm.verdict,
        summary: reviewForm.summary.trim(),
      }),
    });
    if (res.ok) {
      setReviewForm({ verdict: "approve", summary: "" });
      loadReviews();
    } else {
      alert(await res.text());
    }
  };

  const createPr = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!prTitle.trim()) return;
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/prs`, {
      method: "POST",
      headers: { "content-type": "application/json" },
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
    if (!form.title.trim()) return;
    const code_ref = form.path.trim()
      ? { path: form.path.trim(), line_start: Number(form.line) || 1 }
      : null;
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/issues`, {
      method: "POST",
      headers: { "content-type": "application/json" },
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
        onBack={() => setOpenReview(null)}
      />
    );
  }

  return (
    <div className="app">
      <header className="top">
        <div className="brand">
          <span className="logo">⬡</span> Hull
        </div>
        <div className="tag">situation room · what the fleet is doing right now</div>
        <label className="tenant">
          tenant&nbsp;
          <input
            value={tenant}
            onChange={(e) => setTenant(e.target.value.trim())}
            spellCheck={false}
            aria-label="tenant"
          />
        </label>
        <div className="bell-wrap">
          <button className="bell" onClick={() => setShowNotifs((s) => !s)} title="notifications">
            🔔{notifs.length > 0 && <span className="bell-count">{notifs.length}</span>}
          </button>
          {showNotifs && (
            <div className="notif-drop">
              <div className="notif-head">notifications <span className="muted">via Notifier plugin</span></div>
              {notifs.length === 0 && <div className="empty">nothing yet</div>}
              {notifs.slice(0, 12).map((n, i) => (
                <div className="notif" key={i}>
                  <span className={"nk " + n.kind}>{n.kind.replace("_", " ")}</span>
                  <span className="ns">{n.summary}</span>
                </div>
              ))}
            </div>
          )}
        </div>
      </header>

      <main className="grid">
        <section>
          <h2>Repositories <span className="muted">by live activity</span></h2>
          <div className="repos">
            {repos.length === 0 && <div className="empty">waiting for activity…</div>}
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

      <section className="issues" ref={issuesRef}>
        <h2>
          Issues <span className="muted">{tenant}/{issueRepo}</span>
          <span className="counts">
            {issues.filter((i) => i.status.state === "open").length} open ·{" "}
            {issues.filter((i) => i.status.state !== "open").length} closed
          </span>
          <label className="acting">
            acting as&nbsp;
            <select value={actingAs} onChange={(e) => setActingAs(e.target.value)}>
              {actors.map((a) => (
                <option key={a.id} value={a.id}>
                  {a.handle} ({a.kind}){a.accountable ? "" : " ⚠ unaccountable"}
                </option>
              ))}
            </select>
          </label>
        </h2>
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
        <ul className="issue-list">
          {issues.length === 0 && <li className="empty">no issues yet — open one above</li>}
          {[...issues]
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
      </section>

      <section className="issues prs">
        <h2>
          Pull requests <span className="muted">{tenant}/{issueRepo}</span>
        </h2>
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
          {[...prs].sort((a, b) => b.number - a.number).map((p) => {
            const prReviews = reviews.filter((r) => r.target === `pr:${p.number}`);
            return (
            <li key={p.number} className="issue">
              <div className="issue-row">
                <span className={"verif " + p.verification}>{p.verification}</span>
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
                <span className={"by " + (actors.find((a) => a.id === p.author)?.kind ?? "")}>
                  {handleOf(p.author)}
                </span>
              </div>
              {openPr === p.number && (
                <div className="reviews">
                  {prReviews.length === 0 && <p className="muted">no reviews yet</p>}
                  {prReviews.map((r) => (
                    <button className="review clickable" key={r.id} onClick={() => setOpenReview(r)} title="open review">
                      <span className={"verdict " + r.verdict}>{r.verdict.replace("_", " ")}</span>
                      <b className={actors.find((a) => a.id === r.reviewer)?.kind ?? ""}>{handleOf(r.reviewer)}</b>
                      <span className="rv-summary">{r.summary || "open review →"}</span>
                    </button>
                  ))}
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
                  </div>
                </div>
              )}
            </li>
            );
          })}
        </ul>
      </section>
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
  onBack,
}: {
  review: Review;
  pr: PR | null;
  actors: Actor[];
  tenant: string;
  repo: string;
  onBack: () => void;
}) {
  type ChangeInfo = { id: string; intent: string; author: string; files: { path: string; status: string }[] };
  const [change, setChange] = useState<ChangeInfo | null>(null);
  const handleOf = (id: string) => actors.find((a) => a.id === id)?.handle ?? id.slice(0, 8);
  const reviewerActor = actors.find((a) => a.id === review.reviewer);
  const changeId = pr?.changes[0];
  useEffect(() => {
    if (!changeId) return;
    fetch(`/api/repos/${encodeURIComponent(tenant)}/${repo}/change/${changeId}`)
      .then((r) => r.json())
      .then((d) => setChange(d.change))
      .catch(() => {});
  }, [changeId, tenant, repo]);

  const independent = pr ? pr.author !== review.reviewer : true;
  const risk =
    pr?.verification === "green"
      ? "low — keel verify is green"
      : change && change.files.length > 8
        ? "elevated — unverified and a broad change"
        : "moderate — unverified";

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
          <h3>Risk read</h3>
          <p>
            verification: <b>{pr?.verification ?? "—"}</b> · risk: <b>{risk}</b>
          </p>
        </section>

        <section className="rp-card muted-card">
          <h3>
            Session context <span className="muted">(from the keel session that produced this change)</span>
          </h3>
          <p className="muted">
            task · reasoning · semantic operations · tests &amp; CI — these come from the keel session behind the
            change. Not yet populated for a change pushed over plain git; wiring the session-linked review package
            is the next slice.
          </p>
        </section>
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
