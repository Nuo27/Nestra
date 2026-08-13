import type { ReactNode } from "react"

/// Recessed inset block — the shared `bg-inset border` well for draft forms,
/// protocol rows, and preview panes. 0-radius, no shadow (flat/ascii).
export function InsetBlock({
  children,
  pad = "p-3",
  className,
}: {
  children: ReactNode
  pad?: string
  className?: string
}) {
  return (
    <div
      className={`border border-border bg-inset ${pad}${className ? ` ${className}` : ""}`}
    >
      {children}
    </div>
  )
}
