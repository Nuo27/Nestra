import type { ReactNode } from "react"

/** Shimmering placeholder for loading content. Standardizes the local
 * QuotaSkeleton (quota.tsx) and ListSkeleton (sessions.tsx). */
export function Skeleton({
  className,
  children,
}: {
  className?: string
  children?: ReactNode
}) {
  return (
    <div
      aria-hidden="true"
      className={`relative overflow-hidden bg-raised pointer-events-none select-none ${className ?? ""}`}
    >
      <div className="absolute inset-0 -translate-x-full animate-[shimmer_1.6s_var(--ease-standard)_infinite] bg-gradient-to-r from-transparent via-white/[0.04] to-transparent" />
      {children}
    </div>
  )
}
