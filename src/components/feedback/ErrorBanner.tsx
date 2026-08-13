import type { ReactNode } from "react"
import { useTranslation } from "react-i18next"
import { X } from "lucide-react"

type Severity = "error" | "warn" | "info"

const SEVERITY: Record<
  Severity,
  { border: string; bg: string; text: string; glyph: string }
> = {
  error: {
    border: "border-danger-border",
    bg: "bg-danger-soft",
    text: "text-danger",
    glyph: "!",
  },
  warn: {
    border: "border-warning-border",
    bg: "bg-warning-soft",
    text: "text-warning",
    glyph: "~",
  },
  info: {
    border: "border-accent-border",
    bg: "bg-accent-soft",
    text: "text-accent",
    glyph: "i",
  },
}

/// Inline error/warn/info banner — the single validation/error surface.
/// Terminal style: flat, single-glyph prefix. (`warn` is kept as an
/// alias of `warning` for the existing call sites.)
///
///   • `box`   — bordered box (default)
///   • `strip` — full-width bar (border-b only), for list-top error bars
///   • `bare`  — text-only line (no border/bg), for inline action errors
///
/// `onDismiss` renders a trailing ✕ so persistent errors (toasts already
/// reported the failure) can be closed instead of lingering forever.
export function ErrorBanner({
  severity = "error",
  children,
  onRetry,
  onDismiss,
  variant = "box",
  className,
}: {
  severity?: Severity
  children: ReactNode
  onRetry?: () => void
  onDismiss?: () => void
  variant?: "box" | "strip" | "bare"
  className?: string
}) {
  const { t } = useTranslation()
  const s = SEVERITY[severity]
  let shell: string
  if (variant === "strip") shell = `border-b border-border ${s.bg} px-3 py-1`
  else if (variant === "bare") shell = s.text
  else shell = `rounded-md border ${s.border} ${s.bg} px-3 py-2`
  return (
    <div
      role={severity === "error" ? "alert" : "status"}
      className={`flex items-start gap-2 text-xs ${shell}${
        className ? ` ${className}` : ""
      }`}
    >
      {variant !== "bare" && (
        <span className="mt-0.5 shrink-0 leading-relaxed">{s.glyph}</span>
      )}
      <div className="min-w-0 flex-1 leading-relaxed">{children}</div>
      {onRetry && (
        <button
          type="button"
          onClick={onRetry}
          className="shrink-0 font-medium leading-relaxed hover:brightness-125 focus-visible:shadow-focus"
        >
          {t("common.retry")}
        </button>
      )}
      {onDismiss && (
        <button
          type="button"
          onClick={onDismiss}
          aria-label={t("common.dismiss")}
          className="brackets-state shrink-0 leading-relaxed text-subtle hover:text-fg focus-visible:shadow-focus"
        >
          <X data-icon size={12} />
        </button>
      )}
    </div>
  )
}
