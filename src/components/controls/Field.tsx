import { useId, type ReactNode } from "react"

/// Stacked label + control + hint. The single form-field wrapper — replaces
/// the ad-hoc `<label className="block"><span className="text-xs text-subtle">…`
/// pattern and the mcp inline label wrappers.
export function Field({
  label,
  hint,
  error,
  required,
  children,
}: {
  label: ReactNode
  hint?: ReactNode
  error?: ReactNode
  required?: boolean
  children: ReactNode
}) {
  // `<div>` not `<label>`: a label wrapper is invalid HTML around children
  // like Tabs or div-wrapped Input+Button clusters, and the implicit
  // label association only covers a single form control anyway. Error
  // and hint carry ids so callers can wire `aria-describedby` on the input.
  const hintId = useId()
  const errorId = useId()
  return (
    <div className="block">
      <div className="flex items-center gap-1 text-xs font-medium text-muted mb-1.5">
        <span id={hintId}>{label}</span>
        {required && (
          <span className="text-danger" aria-hidden="true">
            *
          </span>
        )}
      </div>
      {children}
      {error ? (
        <div
          id={errorId}
          role="alert"
          className="prose text-xs text-danger mt-1.5 leading-relaxed"
        >
          {error}
        </div>
      ) : hint ? (
        <div id={hintId} className="prose text-xs text-subtle mt-1.5 leading-relaxed">
          {hint}
        </div>
      ) : null}
    </div>
  )
}

/// Horizontal label-left / control-right row. Unifies the `flex items-center
/// justify-between` pattern duplicated across quota settings dialog, settings
/// toggle rows, and diagnostics KV rows. `label` is left-aligned; `children`
/// (the control) is right-aligned. Set `divider` to render a hairline beneath
/// — the standard for stacked settings lists so rows read as a list, not
/// floating fragments.
export function FieldRow({
  label,
  description,
  children,
  className,
  align = "center",
  divider = false,
}: {
  label: ReactNode
  description?: ReactNode
  children: ReactNode
  className?: string
  align?: "center" | "baseline"
  divider?: boolean
}) {
  const alignClass = align === "baseline" ? "items-baseline" : "items-center"
  return (
    <div
      className={`flex ${alignClass} justify-between gap-4 py-2 ${
        divider ? "border-b border-border last:border-b-0" : ""
      }${className ? ` ${className}` : ""}`}
    >
      <div className="min-w-0">
        <div className="text-sm text-fg">{label}</div>
        {description && (
          <div className="prose text-xs text-subtle mt-0.5 leading-relaxed">
            {description}
          </div>
        )}
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  )
}
