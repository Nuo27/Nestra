import type { ReactNode } from "react"

type Tone = "fg" | "muted" | "subtle"

const TONE: Record<Tone, string> = {
  fg: "text-fg",
  muted: "text-muted",
  subtle: "text-subtle",
}

/// Shared `<pre>` shell — the terminal code block. Absorbs the inline
/// `max-h-* overflow-auto scroll whitespace-pre-wrap break-words font-mono`
/// strings scattered across sessions, agents, and the config preview.
///   • `inset` — recessed block (bg-inset + hairline + padding) for standalone
///     code, config previews, and full message bodies.
///   • `bare` — transparent, borderless, caller-controlled padding for pres
///     that sit inside a MessageCard body.
export function CodeBlock({
  children,
  maxH = "max-h-80",
  variant = "inset",
  tone = "fg",
  size = "xs",
  italic = false,
  pad,
  className,
}: {
  children: ReactNode
  maxH?: string
  variant?: "inset" | "bare"
  tone?: Tone
  size?: "xs" | "sm"
  italic?: boolean
  pad?: string
  className?: string
}) {
  const padClass = pad ?? (variant === "inset" ? "p-3" : "")
  return (
    <pre
      // `pad` is the ONLY padding control — `className` is documented as
      // non-padding (callers passing p-* there fought the default).
      className={`overflow-auto ${maxH} scroll whitespace-pre-wrap break-words font-mono ${
        size === "sm" ? "text-sm" : "text-xs"
      } ${TONE[tone]} ${
        variant === "inset" ? "border border-border bg-inset" : ""
      } ${padClass}${italic ? " italic" : ""}${className ? ` ${className}` : ""}`}
    >
      {children}
    </pre>
  )
}
