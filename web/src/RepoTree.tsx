import { FileTree, useFileTree } from "@pierre/trees/react";

// The @pierre/trees `--trees-*` custom properties cascade through the component's shadow DOM, so we
// map them onto hull's own design tokens here — the tree then reads as native chrome (Mona Sans,
// steel selection, paper hover, rule indent guides) and follows the light/dark toggle for free.
const TREE_THEME = {
  height: 580,
  "--trees-font-family": "inherit",
  "--trees-font-size": "12.5px",
  "--trees-row-height": "26px",
  "--trees-item-height": "26px",
  "--trees-item-padding-x": "8px",
  "--trees-border-radius": "6px",
  "--trees-bg": "transparent",
  "--trees-fg": "var(--body)",
  "--trees-fg-muted": "var(--muted)",
  "--trees-accent": "var(--steel)",
  "--trees-selected-bg": "var(--steel-wash)",
  "--trees-selected-fg": "var(--steel-text)",
  "--trees-theme-list-hover-bg": "var(--rule3)",
  "--trees-theme-list-active-selection-bg": "var(--steel-wash)",
  "--trees-theme-list-active-selection-fg": "var(--steel-text)",
  "--trees-indent-guide-bg": "var(--rule2)",
  "--trees-focus-ring-color": "var(--steel)",
  "--trees-scrollbar-thumb": "var(--rule2)",
} as React.CSSProperties;

// The directory tree for the Files page — @pierre/trees fed a flat list of every path in the branch.
// Clicking a file (a leaf that exists in `paths`) opens it via `onSelect`; directory rows just expand.
export default function RepoTree({ paths, selected, onSelect }:
  { paths: string[]; selected: string | null; onSelect: (path: string) => void }) {
  const fileSet = new Set(paths);
  const { model } = useFileTree({
    paths,
    density: "default",
    initialSelectedPaths: selected ? [selected] : undefined,
    onSelectionChange: (sel) => {
      const hit = sel.find((p) => fileSet.has(p));
      if (hit) onSelect(hit);
    },
  });
  // @pierre/trees is virtualized — it needs a concrete height on its host element to compute rows.
  return <FileTree model={model} style={TREE_THEME} />;
}
