// Shared presentational primitives extracted verbatim from App.tsx (pure structural move).
// Used across App.tsx and the extracted repo components.
import { Component } from "react";

export class Boundary extends Component<{ fallback: React.ReactNode; children: React.ReactNode }, { err: boolean }> {
  state = { err: false };
  static getDerivedStateFromError() { return { err: true }; }
  render() { return this.state.err ? this.props.fallback : this.props.children; }
}

export const Card = ({ children, className = "", id }: { children: React.ReactNode; className?: string; id?: string }) => (
  <div id={id} className={`bg-surface border border-rule rounded-card overflow-hidden ${className}`}>{children}</div>
);
export const SectionHeader = ({ label, right }: { label: string; right?: React.ReactNode }) => (
  <div className="flex items-center justify-between gap-3 px-5 py-3.5 border-b border-rule2">
    <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted">{label}</span>
    {right}
  </div>
);
