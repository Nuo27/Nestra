import type { ReactNode } from "react"

type Width = "default" | "wide" | "prose"

const WIDTH: Record<Width, string> = {
  default: "max-w-5xl",
  wide: "max-w-7xl",
  prose: "max-w-2xl",
}

/// Standard content frame for every page. Replaces the per-page
/// `div className="w-full p-5"` that had drifted into inconsistent padding
/// and no rhythm between sections. `sections` spacing gives the vertical
/// beat; `width` keeps long prose from stretching edge-to-edge on wide
/// windows while letting dashboards/grids go wide.
export function Page({
  children,
  width = "default",
  className,
}: {
  children: ReactNode
  width?: Width
  className?: string
}) {
  return (
    <div
      className={`w-full p-4 ${WIDTH[width]}${className ? ` ${className}` : ""}`}
    >
      <div className="space-y-4">{children}</div>
    </div>
  )
}
