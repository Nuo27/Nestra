import type { CSSProperties, ReactNode } from "react"
import { InfoButton } from "./InfoButton"

type Tone = "default" | "danger" | "inset"
type Padding = "none" | "sm" | "md" | "lg"

/// Per-tone surface overrides applied via inline style so the base
/// `.surface-panel` (border + radius) stays shared. Only the background and
/// border color move with the tone — keeping the terminal grammar intact.
const TONE_STYLE: Record<Tone, CSSProperties> = {
  default: {},
  danger: {
    background: "var(--danger-soft)",
    borderColor: "var(--danger-border)",
  },
  inset: { background: "var(--bg-inset)" },
}

const PADDING_CLASS: Record<Padding, string> = {
  none: "",
  sm: "p-3",
  md: "p-4",
  lg: "p-5",
}

const HEADER_PAD: Record<Padding, string> = {
  none: "px-4 py-3",
  sm: "px-3 py-2.5",
  md: "px-4 py-3",
  lg: "px-5 py-3.5",
}

/// Single card / panel surface for the whole app. Absorbs the ad-hoc bordered
/// divs that were scattered around (QuotaCard, QuotaSkeleton, CliRow,
/// EndpointCard, session message cards). `interactive` adds a hover border
/// lift for clickable cards; `tone="danger"` for danger zones.
export function Card({
  children,
  className,
  title,
  description,
  hint,
  info,
  action,
  footer,
  tone = "default",
  padding = "md",
  interactive = false,
  minH,
}: {
  children: ReactNode
  className?: string
  title?: ReactNode
  description?: ReactNode
  hint?: ReactNode
  /** Explanatory text shown as a tooltip next to the card title. */
  info?: ReactNode
  action?: ReactNode
  footer?: ReactNode
  tone?: Tone
  padding?: Padding
  interactive?: boolean
  /** CSS min-height applied to the body region (e.g. "180px"). Lets grid
   * cards align regardless of content while still letting the footer pin. */
  minH?: string
}) {
  return (
    <div
      className={`surface-panel flex flex-col text-fg transition-[border-color] duration-fast ${
        interactive ? "hover:border-border-strong cursor-pointer" : ""
      }${className ? ` ${className}` : ""}`}
      style={TONE_STYLE[tone]}
    >
      {(title || action) && (
        <div className={`border-b border-border ${HEADER_PAD[padding]}`}>
          <div className="flex items-center justify-between gap-3">
            <div className="flex min-w-0 items-center gap-1.5">
              <div className="truncate text-md font-medium">{title}</div>
              {info && <InfoButton content={info} />}
            </div>
            {action}
          </div>
          {description && (
            <div className="prose text-xs text-muted mt-1 leading-relaxed">
              {description}
            </div>
          )}
          {hint && (
            <div className="text-xs text-subtle mt-1 leading-relaxed">{hint}</div>
          )}
        </div>
      )}
      <div
        className={`flex-1 ${PADDING_CLASS[padding]}${minH ? ` min-h-[${minH}]` : ""}`}
        style={minH ? { minHeight: minH } : undefined}
      >
        {children}
      </div>
      {footer && (
        <div className="border-t border-border px-4 py-3">{footer}</div>
      )}
    </div>
  )
}
