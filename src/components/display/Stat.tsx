import type { ReactNode } from "react"

/// Small bordered `bg-inset` tile: 2xs uppercase tracked label + tabular
/// numeric or text value. The default metric tile across the app —
/// replaces two local `Stat` / inline `Field` grids (agent-detail cockpit,
/// gateway dashboard). Optional `hint` carries secondary meta below the
/// value; `tone` adds a semantic accent for the value.
export function Stat({
  label,
  value,
  hint,
  tone,
  className,
}: {
  label: ReactNode
  value: ReactNode
  hint?: ReactNode
  tone?: "default" | "success" | "warning" | "danger" | "accent"
  className?: string
}) {
  const TONE: Record<NonNullable<typeof tone>, string> = {
    default: "text-fg",
    success: "text-success",
    warning: "text-warning",
    danger: "text-danger",
    accent: "text-accent",
  }
  return (
    <div
      className={`flex flex-col gap-0.5 border border-border bg-inset px-2.5 py-1.5 ${
        className ?? ""
      }`}
    >
      <span className="text-2xs font-semibold uppercase tracking-[0.08em] text-subtle">
        {label}
      </span>
      <span className={`font-mono text-sm tabular ${TONE[tone ?? "default"]}`}>
        {value}
      </span>
      {hint && <span className="text-2xs text-muted">{hint}</span>}
    </div>
  )
}