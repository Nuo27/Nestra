import type { ReactNode } from "react";

/// Card header strip: accent icon + title + muted hint on a bottom border.
/// Shared by the agent detail page and the routing-policy sub-page.
export function SectionHeader({
  icon,
  title,
  hint,
}: {
  icon: ReactNode;
  title: string;
  hint: string;
}) {
  return (
    <div className="flex items-center gap-2 border-b border-border px-3 py-2">
      <span className="text-accent">{icon}</span>
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium text-fg">{title}</div>
        <div className="prose mt-0.5 text-2xs text-subtle">{hint}</div>
      </div>
    </div>
  );
}
