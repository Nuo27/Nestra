import type { ReactNode } from "react";

/// Specialized empty state for orchestration surfaces. The gateway is live and
/// writes real rows as traffic flows; this empty state shows when no matching
/// rows exist yet (e.g. no requests through the gateway, no subagents detected,
/// no migrations triggered). Honest — no fake data.
export function EmptyOrchestration({
  title,
  hint,
  children,
}: {
  title: string;
  hint?: ReactNode;
  children?: ReactNode;
}) {
  return (
    <div className="surface-panel flex flex-col items-center justify-center gap-2 px-6 py-10 text-center">
      <div className="flex items-center gap-2">
        <span className="font-mono text-2xs text-subtle">$</span>
        <span className="text-sm font-medium text-muted">{title}</span>
      </div>
      {hint && (
        <div className="prose mx-auto max-w-sm text-xs text-subtle leading-relaxed">
          {hint}
        </div>
      )}
      {children && <div className="mt-2">{children}</div>}
    </div>
  );
}
