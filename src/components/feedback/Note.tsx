import type { ReactNode } from "react"

/// Small muted prose caption — the shared explanatory-note voice across
/// agents, sessions, and quota (`prose text-xs text-subtle leading-relaxed`).
export function Note({
  children,
  className,
}: {
  children: ReactNode
  className?: string
}) {
  return (
    <div className={`prose text-xs text-subtle leading-relaxed${className ? ` ${className}` : ""}`}>
      {children}
    </div>
  )
}
