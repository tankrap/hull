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
type CodeRef = { repo: string; blob: string; path: string; line_start: number; line_end?: number };
type Issue = {
  number: number;
  title: string;
  body: string;
  author: string;
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
  const [form, setForm] = useState({ title: "", path: "", line: "" });
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

  const createIssue = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!form.title.trim()) return;
    const code_ref = form.path.trim()
      ? { path: form.path.trim(), line_start: Number(form.line) || 1 }
      : null;
    const res = await fetch(`/api/repos/${encodeURIComponent(tenant)}/${issueRepo}/issues`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ title: form.title.trim(), author: actingAs, code_ref }),
    });
    if (res.ok) {
      setForm({ title: "", path: "", line: "" });
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
                onClick={() => setIssueRepo(r.repo)}
                title="show this repo's issues"
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

      <section className="issues">
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
                <span className="it-title">{it.title}</span>
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
