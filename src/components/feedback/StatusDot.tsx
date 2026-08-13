export type StatusLevel = "ok" | "outdated" | "missing" | "unknown"

/// Single-glyph indicator: ok `●`, outdated `!`, missing `○`, unknown `?`.
/// A custom `color` (per-provider brand dots in sessions) tints the glyph.
const LEVEL_GLYPH: Record<StatusLevel, string> = {
  ok: "●",
  outdated: "!",
  missing: "○",
  unknown: "?",
}

const LEVEL_COLOR: Record<StatusLevel, string> = {
  ok: "text-success",
  outdated: "text-warning",
  missing: "text-danger",
  unknown: "text-subtle",
}

const SIZE_TEXT: Record<1.5 | 2 | 2.5, string> = {
  1.5: "text-2xs",
  2: "text-xs",
  2.5: "text-sm",
}

/// One status indicator for the whole app. Pass either `status`
/// (semantic) or `color` (arbitrary CSS color — the per-provider brand dots).
/// `color` wins. `title` is REQUIRED for accessibility: a brand-dot without
/// one would otherwise fall back to a hardcoded English label.
export function StatusDot({
  status,
  color,
  size = 2,
  className,
  title,
}: {
  status?: StatusLevel
  color?: string
  size?: 1.5 | 2 | 2.5
  className?: string
  title?: string
}) {
  const level = status ?? "unknown"
  return (
    <span
      className={`inline-block shrink-0 leading-none ${SIZE_TEXT[size]} ${
        color ? "" : LEVEL_COLOR[level]
      }${className ? ` ${className}` : ""}`}
      style={color ? { color } : undefined}
      role="img"
      aria-label={title ?? "status"}
      title={title}
    >
      {/* `color` is the brand-dot path (per-agent markers in sessions) —
          always a filled circle, tinted by the passed color. The status
          path keeps its level-specific glyph (●/!/○/?). */}
      {color ? "●" : LEVEL_GLYPH[level]}
    </span>
  )
}
