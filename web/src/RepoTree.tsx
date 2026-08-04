import { FileTree, useFileTree } from "@pierre/trees/react";

// The directory tree for the Files page — @pierre/trees fed a flat list of every path in the branch.
// Clicking a file (a leaf that exists in `paths`) opens it via `onSelect`; directory rows just expand.
export default function RepoTree({ paths, selected, onSelect }:
  { paths: string[]; selected: string | null; onSelect: (path: string) => void }) {
  const fileSet = new Set(paths);
  const { model } = useFileTree({
    paths,
    density: "compact",
    initialSelectedPaths: selected ? [selected] : undefined,
    onSelectionChange: (sel) => {
      const hit = sel.find((p) => fileSet.has(p));
      if (hit) onSelect(hit);
    },
  });
  // @pierre/trees is virtualized — it needs a concrete height on its host element to compute rows.
  return <FileTree model={model} className="text-[13px] leading-[1.6]" style={{ height: 580 }} />;
}
