import type { ReactNode } from "react"

type Tone = "neutral" | "accent" | "success" | "warning" | "danger"
type Variant = "soft" | "outline" | "solid"

type Classes = { soft: string; outline: string; solid: string }

const TONE: Record<Tone, Classes> = {
  neutral: {
    soft: "bg-raised text-muted",
    outline: "border border-border text-muted",
    solid: "bg-fg-subtle text-canvas",
  },
  accent: {
    soft: "bg-accent-soft text-accent",
    outline: "border border-accent-border text-accent",
    solid: "bg-accent text-[color:var(--fg-on-accent)]",
  },
  success: {
    soft: "bg-success-soft text-success",
    outline: "border border-border text-success",
    solid: "bg-success text-canvas",
  },
  warning: {
    soft: "bg-warning-soft text-warning",
    outline: "border border-border text-warning",
    solid: "bg-warning text-canvas",
  },
  danger: {
    soft: "bg-danger-soft text-danger",
    outline: "border border-danger-border text-danger",
    solid: "bg-danger text-canvas",
  },
}

/// Compact status tag. Replaces the scattered inline
/// badges: sessions MetadataBadges, tool error badge, provider/cli status
/// labels, the mcp "mixed transport" tag.
export function Badge({
  children,
  tone = "neutral",
  variant = "soft",
  className,
  title,
}: {
  children: ReactNode
  tone?: Tone
  variant?: Variant
  className?: string
  title?: string
}) {
  return (
    <span
      title={title}
      className={`inline-flex items-center gap-1 px-0.5 text-[11px] font-medium leading-[1.4] whitespace-nowrap ${TONE[tone][variant]}${className ? ` ${className}` : ""}`}
    >
      {children}
    </span>
  )
}
