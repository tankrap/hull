// Files tab: a branch-aware file browser with fuzzy/full-text search — extracted verbatim from App.tsx.
// Owns the lazy @pierre/diffs File viewer, the @pierre/trees sidebar, and the Hl code fragment,
// which are used only by this tab.
import { lazy, Suspense, useState, useEffect } from "react";
import { SearchInput } from "../ui/Field";
import { Picker } from "./menus";
import { Card, SectionHeader, Boundary } from "./primitives";
import { hlToHtml } from "../highlight";

const PierreFile = lazy(() => import("@pierre/diffs/react").then((m) => ({ default: m.File })));
const RepoTree = lazy(() => import("../RepoTree"));

// Syntax-highlighted code fragment (hljs HTML). Used across the diff viewer.
const Hl = ({ text, path }: { text: string; path: string }) => <span dangerouslySetInnerHTML={{ __html: hlToHtml(text, path) }} />;

// ── Files tab: a branch-aware file browser with fuzzy/full-text search ──────────────────────────
type SearchHit = { path: string; line: number; text: string; kind: "path" | "content" };
export function RepoFiles({ tenant, repo, authHeaders, theme }: { tenant: string; repo: string; authHeaders: () => Record<string, string>; theme: string }) {
  const base = `/api/repos/${encodeURIComponent(tenant)}/${repo}`;
  const [branches, setBranches] = useState<string[]>([]);
  const [branch, setBranch] = useState("main");
  const [file, setFile] = useState<{ path: string; text: string; binary: boolean; size: number } | null>(null);
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<SearchHit[] | null>(null);
  const [allPaths, setAllPaths] = useState<string[] | null>(null);
  // Below md the tree is a toggled drawer above the viewer; above md it's the fixed left column.
  const [treeOpen, setTreeOpen] = useState(false);

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

  const openFile = (p: string, ref: string = branch) => {
    setResults(null); setQuery("");
    fetch(`${base}/blob?ref=${encodeURIComponent(ref)}&path=${encodeURIComponent(p)}`, { headers: authHeaders() })
      .then((r) => r.json()).then((d) => setFile({ path: p, text: d.text ?? "", binary: !!d.binary, size: d.size ?? 0 })).catch(() => {});
  };
  // Clear any open file / search when the branch changes (the tree reloads via the paths effect).
  useEffect(() => { setResults(null); setQuery(""); setFile(null); /* eslint-disable-next-line */ }, [branch]);
  // Deep-link: /…/files?path=<p>&branch=<b> (e.g. from a dependency-graph node) opens that file once
  // branches settle. The optional branch param pins the file to the ref it was linked from (the graph
  // renders a specific branch); without it we honor whatever branch the picker settled on.
  const [pendingPath, setPendingPath] = useState<string | null>(() => new URLSearchParams(location.search).get("path"));
  const [pendingBranch] = useState<string | null>(() => new URLSearchParams(location.search).get("branch"));
  useEffect(() => {
    if (pendingPath && branches.length) {
      const ref = pendingBranch && branches.includes(pendingBranch) ? pendingBranch : branch;
      if (ref !== branch) setBranch(ref);
      openFile(pendingPath, ref);
      setPendingPath(null);
    }
    /* eslint-disable-next-line */
  }, [pendingPath, branches]);
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
        <div className="grid grid-cols-1 md:grid-cols-[minmax(190px,270px)_1fr] gap-4 items-start">
          {/* Below md the tree is a toggled drawer; the button collapses (md:hidden) so the desktop split is unchanged. */}
          <button className={`md:hidden inline-flex items-center gap-1.5 w-fit px-3 py-1.5 rounded-ctl border text-[13px] transition-colors ${treeOpen ? "text-ink border-ctl" : "text-muted border-rule hover:text-ink"}`} aria-expanded={treeOpen} onClick={() => setTreeOpen((v) => !v)}><FileIcon />Files</button>
          <Card className={`overflow-hidden max-h-[55vh] overflow-y-auto md:max-h-none ${treeOpen ? "" : "hidden md:block"}`}>
            <SectionHeader label="Files" right={<span className="text-[11.5px] text-faint truncate max-w-[110px]">{branch}</span>} />
            <div className="py-1.5 pl-1.5 pr-1">
              {allPaths === null ? (
                <div className="px-3 py-4 text-[12.5px] text-muted">Loading…</div>
              ) : allPaths.length === 0 ? (
                <div className="px-3 py-4 text-[12.5px] text-muted">No files.</div>
              ) : (
                <Suspense fallback={<div className="px-3 py-4 text-[12.5px] text-muted">Loading tree…</div>}>
                  <RepoTree paths={allPaths} selected={file?.path ?? null} onSelect={(p: string) => { openFile(p); setTreeOpen(false); }} />
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
