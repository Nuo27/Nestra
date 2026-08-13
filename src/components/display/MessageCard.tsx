import { useState, type ReactNode } from "react"
import { ChevronRight } from "lucide-react"

/// Collapsible terminal message card — the shared anatomy of the sessions
/// conversation rows (thinking / tool / tool-pair / message). A bordered flat
/// box with an optional toggle header (chevron + label + trailing timestamp)
/// and a body region that reveals when open. `plain` renders a padded,
/// always-open message box for user/assistant text.
///
/// The body carries its own padding (callers use `CodeBlock` or custom rows);
/// `bodyBorder` draws the hairline above it, matching the sessions pre anatomy.
export function MessageCard({
  header,
  trailing,
  body,
  defaultOpen = true,
  chevron = "subtle",
  borderTone = "default",
  plain = false,
  bodyBorder = true,
  className,
}: {
  header?: ReactNode
  trailing?: ReactNode
  body?: ReactNode
  defaultOpen?: boolean
  chevron?: "subtle" | "warning"
  borderTone?: "default" | "danger"
  plain?: boolean
  bodyBorder?: boolean
  className?: string
}) {
  const [open, setOpen] = useState(defaultOpen)
  const chevronClass = chevron === "warning" ? "text-warning" : "text-subtle"
  // Hover border only on collapsible cards (they're interactive); a plain
  // card must not hint at interactivity, and a danger border must not be
  // overridden by the hover color.
  const hoverClass = plain ? "" : " hover:border-border-strong"
  const container =
    `border bg-surface text-sm transition-colors duration-fast ${hoverClass} ${
      borderTone === "danger" ? "border-danger-border" : "border-border"
    }${className ? ` ${className}` : ""}`

  if (plain) {
    return (
      <div className={container}>
        <div className="p-3">
          {(header !== undefined || trailing !== undefined) && (
            <div className="mb-1 flex items-baseline gap-2">
              {header}
              {trailing !== undefined && (
                <span className="ml-auto shrink-0 text-xs tabular text-subtle">
                  {trailing}
                </span>
              )}
            </div>
          )}
          {body}
        </div>
      </div>
    )
  }

  return (
    <div className={container}>
      {header !== undefined && (
        <button
          type="button"
          aria-expanded={open}
          onClick={() => setOpen((o) => !o)}
          className="flex w-full items-center gap-2 px-3 py-1.5 text-left"
        >
          <ChevronRight
            data-icon
            size={12}
            className={`shrink-0 ${chevronClass} transition-transform duration-fast ${
              open ? "rotate-90" : ""
            }`}
          />
          <span className="min-w-0 flex-1">{header}</span>
          {trailing !== undefined && (
            <span className="ml-auto shrink-0 text-xs tabular text-subtle">
              {trailing}
            </span>
          )}
        </button>
      )}
      {open && body !== undefined && (
        <div className={bodyBorder ? "border-t border-border" : ""}>{body}</div>
      )}
    </div>
  )
}
