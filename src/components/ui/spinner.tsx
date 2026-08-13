import { LoaderCircle } from "lucide-react"

/** Single loading glyph. Standardizes the ad-hoc `<Icon className="animate-spin" />`
 * pattern that was reinvented in provider-edit, clis, diagnostics, sessions. */
export function Spinner({
  size = 14,
  className,
}: {
  size?: number
  className?: string
}) {
  return (
    <LoaderCircle
      data-icon
      size={size}
      strokeWidth={2}
      className={`animate-spin ${className ?? ""}`}
    />
  )
}
