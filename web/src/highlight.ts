// Syntax highlighting (highlight.js, curated languages) + word-level intra-line diff, for the
// code-review diff viewer. Highlighting returns HTML (hljs-* classes styled in globals.css); the
// word diff returns per-side segments so a modified line can strike the removed tokens and highlight
// the added ones (the walk_all → slice look).
import hljs from "highlight.js/lib/core";
import rust from "highlight.js/lib/languages/rust";
import typescript from "highlight.js/lib/languages/typescript";
import javascript from "highlight.js/lib/languages/javascript";
import python from "highlight.js/lib/languages/python";
import go from "highlight.js/lib/languages/go";
import json from "highlight.js/lib/languages/json";
import bash from "highlight.js/lib/languages/bash";
import yaml from "highlight.js/lib/languages/yaml";
import cssLang from "highlight.js/lib/languages/css";
import xml from "highlight.js/lib/languages/xml";
import markdown from "highlight.js/lib/languages/markdown";
import sql from "highlight.js/lib/languages/sql";
import ini from "highlight.js/lib/languages/ini";
import c from "highlight.js/lib/languages/c";
import cpp from "highlight.js/lib/languages/cpp";

for (const [n, l] of Object.entries({ rust, typescript, javascript, python, go, json, bash, yaml, css: cssLang, xml, markdown, sql, ini, c, cpp })) {
  hljs.registerLanguage(n, l as never);
}

const EXT: Record<string, string> = {
  rs: "rust", ts: "typescript", tsx: "typescript", js: "javascript", jsx: "javascript", mjs: "javascript", cjs: "javascript",
  py: "python", go: "go", json: "json", sh: "bash", bash: "bash", zsh: "bash", yml: "yaml", yaml: "yaml",
  css: "css", scss: "css", html: "xml", xml: "xml", svg: "xml", md: "markdown", markdown: "markdown",
  sql: "sql", toml: "ini", ini: "ini", c: "c", h: "c", cpp: "cpp", cc: "cpp", hpp: "cpp", cxx: "cpp",
};

export function langFromPath(path: string): string | null {
  const ext = (path.split(".").pop() || "").toLowerCase();
  return EXT[ext] ?? null;
}

function escapeHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/** Syntax-highlight one line to HTML (hljs-* classes). Falls back to escaped text. */
export function hlToHtml(code: string, path: string): string {
  const lang = langFromPath(path);
  if (!lang || !hljs.getLanguage(lang)) return escapeHtml(code);
  try {
    return hljs.highlight(code, { language: lang, ignoreIllegals: true }).value;
  } catch {
    return escapeHtml(code);
  }
}

export type Seg = { text: string; changed: boolean };

/** Token-level diff between two lines (LCS over word/space/punct tokens). */
export function wordDiff(oldLine: string, newLine: string): { old: Seg[]; next: Seg[] } {
  const tok = (s: string) => s.match(/(\s+|[A-Za-z0-9_]+|[^A-Za-z0-9_\s])/g) ?? [];
  const a = tok(oldLine), b = tok(newLine);
  const n = a.length, m = b.length;
  const dp: number[][] = Array.from({ length: n + 1 }, () => new Array(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
  const oldSeg: Seg[] = [], nextSeg: Seg[] = [];
  const push = (arr: Seg[], t: string, c: boolean) => { const last = arr[arr.length - 1]; if (last && last.changed === c) last.text += t; else arr.push({ text: t, changed: c }); };
  let i = 0, j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) { push(oldSeg, a[i], false); push(nextSeg, b[j], false); i++; j++; }
    else if (dp[i + 1][j] >= dp[i][j + 1]) { push(oldSeg, a[i], true); i++; }
    else { push(nextSeg, b[j], true); j++; }
  }
  while (i < n) { push(oldSeg, a[i], true); i++; }
  while (j < m) { push(nextSeg, b[j], true); j++; }
  return { old: oldSeg, next: nextSeg };
}
