import React from "react";

// Minimal, XSS-safe markdown → React (no dangerouslySetInnerHTML on user content). Covers headings,
// bullet lists, blockquotes, fenced + inline code, bold/italic, and links — enough for comments,
// descriptions, and review summaries.
// When a repo base like "/acme/web" is given, `!11` links to that voyage and `#7` to that issue —
// so descriptions and comments cross-reference other work with a click.
let LINK_BASE: string | null = null;
function inline(text: string, kp: string): React.ReactNode[] {
  const nodes: React.ReactNode[] = [];
  const ref = LINK_BASE ? "|(?<![\\w])[!#]\\d+" : "";
  const re = new RegExp(`(\`[^\`]+\`|\\*\\*[^*]+\\*\\*|\\*[^*]+\\*|_[^_]+_|\\[[^\\]]+\\]\\([^)]+\\)|https?:\\/\\/[^\\s)]+|(?<![\\w])@[A-Za-z0-9][\\w:-]*${ref})`, "g");
  let last = 0, m: RegExpExecArray | null, i = 0;
  while ((m = re.exec(text))) {
    if (m.index > last) nodes.push(text.slice(last, m.index));
    const tok = m[0];
    const key = `${kp}-${i++}`;
    if (tok.startsWith("`")) nodes.push(<code key={key} className="px-1 py-0.5 rounded bg-paper border border-rule3 text-[0.92em]">{tok.slice(1, -1)}</code>);
    else if (tok.startsWith("**")) nodes.push(<b key={key}>{tok.slice(2, -2)}</b>);
    else if (tok.startsWith("*")) nodes.push(<i key={key}>{tok.slice(1, -1)}</i>);
    else if (tok.startsWith("_")) nodes.push(<i key={key}>{tok.slice(1, -1)}</i>);
    else if (tok.startsWith("[")) { const mm = /\[([^\]]+)\]\(([^)]+)\)/.exec(tok); if (mm && /^https?:\/\//.test(mm[2])) nodes.push(<a key={key} href={mm[2]} target="_blank" rel="noreferrer" className="text-steel-text">{mm[1]}</a>); else nodes.push(tok); }
    else if (tok[0] === "@") { nodes.push(<span key={key} className="font-semibold text-steel-text">{tok}</span>); }
    else if (LINK_BASE && (tok[0] === "!" || tok[0] === "#")) { const n = tok.slice(1); const href = `${LINK_BASE}/${tok[0] === "!" ? "voyages" : "issues"}/${n}`; nodes.push(<a key={key} href={href} className="text-steel-text font-medium">{tok}</a>); }
    else nodes.push(<a key={key} href={tok} target="_blank" rel="noreferrer" className="text-steel-text break-all">{tok}</a>);
    last = m.index + tok.length;
  }
  if (last < text.length) nodes.push(text.slice(last));
  return nodes;
}

export function Markdown({ text, className = "", linkBase = null }: { text: string; className?: string; linkBase?: string | null }) {
  LINK_BASE = linkBase;
  const lines = (text ?? "").split("\n");
  const blocks: React.ReactNode[] = [];
  let i = 0, k = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (line.trim().startsWith("```")) {
      const buf: string[] = []; i++;
      while (i < lines.length && !lines[i].trim().startsWith("```")) { buf.push(lines[i]); i++; }
      i++;
      blocks.push(<pre key={k++} className="my-1.5 p-2.5 rounded-ctl bg-paper border border-rule2 overflow-x-auto text-[12.5px] leading-[1.5]"><code>{buf.join("\n")}</code></pre>);
      continue;
    }
    const h = /^(#{1,3})\s+(.*)/.exec(line);
    if (h) { const lvl = h[1].length; blocks.push(<div key={k++} className={lvl === 1 ? "text-[16px] font-semibold mt-2 mb-0.5" : lvl === 2 ? "text-[15px] font-semibold mt-2 mb-0.5" : "text-[14px] font-semibold mt-1.5 mb-0.5"}>{inline(h[2], `h${k}`)}</div>); i++; continue; }
    if (/^\s*[-*]\s+/.test(line)) {
      const items: string[] = [];
      while (i < lines.length && /^\s*[-*]\s+/.test(lines[i])) { items.push(lines[i].replace(/^\s*[-*]\s+/, "")); i++; }
      blocks.push(<ul key={k++} className="list-disc pl-5 my-1 grid gap-0.5">{items.map((it, j) => <li key={j}>{inline(it, `li${k}-${j}`)}</li>)}</ul>);
      continue;
    }
    if (line.trim().startsWith(">")) {
      const items: string[] = [];
      while (i < lines.length && lines[i].trim().startsWith(">")) { items.push(lines[i].replace(/^\s*>\s?/, "")); i++; }
      blocks.push(<blockquote key={k++} className="border-l-2 border-rule pl-3 text-muted my-1">{inline(items.join(" "), `bq${k}`)}</blockquote>);
      continue;
    }
    if (line.trim() === "") { i++; continue; }
    const para: string[] = [];
    while (i < lines.length && lines[i].trim() !== "" && !/^(\s*[-*]\s+|#{1,3}\s+|>|```)/.test(lines[i])) { para.push(lines[i]); i++; }
    blocks.push(<p key={k++} className="my-0.5 leading-[1.55]">{para.map((pp, j) => <React.Fragment key={j}>{j > 0 && <br />}{inline(pp, `p${k}-${j}`)}</React.Fragment>)}</p>);
  }
  return <div className={className}>{blocks}</div>;
}
