import type { ReactNode } from "react"
import { ChevronLeft } from "lucide-react"
import { useNavigate } from "@tanstack/react-router"
import { Button } from "../controls/Button"
import { InfoButton } from "../controls/InfoButton"

/// Page header. `sticky` renders a top-anchored bar with canvas backdrop +
/// blur — the standard for detail/edit pages (replaces the one-off sticky
/// header in provider-edit). Optional `back` slot accepts a BackLink.
///
/// `info` renders a small info icon after the title whose tooltip carries the
/// explanatory copy — long descriptions never clutter the header inline.
export function PageHeader({
  title,
  subtitle,
  info,
  action,
  back,
  sticky,
}: {
  title: ReactNode
  subtitle?: ReactNode
  /** Explanatory text shown as a tooltip (see `InfoButton`). */
  info?: ReactNode
  action?: ReactNode
  back?: ReactNode
  sticky?: boolean
}) {
  return (
    <div
      className={[
        sticky ? "sticky top-0 z-10 -mx-4 px-4 py-2.5 mb-3 border-b border-border bg-canvas" : "",
        // No subtitle: the header is one line tall, so center the action
        // cluster on the title's optical middle (items-start leaves sm/md
        // buttons 2-4px high of the text-xl cap). With a subtitle the left
        // block is two lines — top alignment keeps the action on the title row.
        `flex ${subtitle ? "items-start" : "items-center"} justify-between gap-3`,
      ]
        .filter(Boolean)
        .join(" ")}
    >
      <div className="min-w-0">
        {back && <div className="mb-2">{back}</div>}
        <div className="flex items-center gap-1.5">
          <h1 className="text-xl font-semibold tracking-[-0.01em] truncate">{title}</h1>
          {info && <InfoButton content={info} />}
        </div>
        {subtitle && (
          <div className="prose text-sm text-muted mt-1 leading-relaxed max-w-prose">
            {subtitle}
          </div>
        )}
      </div>
      {action && <div className="shrink-0 flex items-center gap-2">{action}</div>}
    </div>
  )
}

/// Inline back navigation. `to` navigates via the router when given;
/// otherwise `onClick`. Renders the unified `Button` so the bracket ring
/// behaves identically to every other affordance.
///
/// When BOTH are provided, `to` wins and `onClick` is ignored — callers that
/// need router params (e.g. `/agents/$id` with a concrete id) pass only
/// `onClick`.
export function BackLink({
  to,
  onClick,
  children,
}: {
  to?: string
  onClick?: () => void
  children: ReactNode
}) {
  const navigate = useNavigate()
  return (
    <Button
      variant="ghost"
      size="sm"
      onClick={() => (to ? navigate({ to }) : onClick?.())}
    >
      <ChevronLeft data-icon size={13} />
      {children}
    </Button>
  )
}

export function SectionLabel({
  children,
  className,
  inline = false,
}: {
  children: ReactNode
  className?: string
  /** Render as a `<span>` instead of a `<div>` for inline label usages. */
  inline?: boolean
}) {
  const cls = `text-2xs font-semibold uppercase tracking-[0.08em] text-subtle${className ? ` ${className}` : ""}`
  return inline ? <span className={cls}>{children}</span> : <div className={cls}>{children}</div>
}
