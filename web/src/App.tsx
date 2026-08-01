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
              <article className="repo" key={r.repo}>
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
