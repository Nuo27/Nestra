import type { ReactNode } from "react"

/// Composed empty state. Title + hint + optional action, centered and
/// breathable. The single replacement for the ad-hoc "nothing here" blocks
/// (incl. the local EmptyState that lived in sessions.tsx).
export function EmptyState({
  title,
  hint,
  action,
  icon,
  className,
}: {
  title: ReactNode
  hint?: ReactNode
  action?: ReactNode
  icon?: ReactNode
  className?: string
}) {
  return (
    <div
      className={`border border-dashed border-border px-6 py-12 text-center${
        className ? ` ${className}` : ""
      }`}
    >
      {icon && (
        <div className="mx-auto mb-4 flex h-11 w-11 items-center justify-center text-subtle [&_svg]:size-5">
          {icon}
        </div>
      )}
      <div className="inline-block text-md font-medium text-fg">{title}</div>
      {hint && (
        <div className="mx-auto mt-1.5 max-w-sm text-sm text-muted leading-relaxed">
          {hint}
        </div>
      )}
      {action && <div className="mt-5 flex justify-center">{action}</div>}
    </div>
  )
}
