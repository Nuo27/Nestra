import type { ReactNode } from "react"

type Tone = "fg" | "default" | "accent" | "success" | "warning" | "danger"

const TONE: Record<Tone, string> = {
  fg: "text-fg",
  default: "text-muted",
  accent: "text-accent",
  success: "text-success",
  warning: "text-warning",
  danger: "text-danger",
}

/// Soft-shell (per-tone tinted border + bg) vs the flat inset shell.
const SOFT_SHELL: Partial<Record<Tone, string>> = {
  accent: "border-accent-border bg-accent-soft",
  success: "border-success-border bg-success-soft",
  warning: "border-warning-border bg-warning-soft",
  danger: "border-danger-border bg-danger-soft",
}

/// Mono terminal tag — the inset pill shell (`border border-border bg-inset
/// font-mono text-2xs`) for route reasons, milestone tags, version labels, and
/// live indicators. RouteBadge, the orchestration `landsIn`/`live` tags, and
/// the control-plane version tag rebuild on this. `variant="soft"` tints the
/// shell with the tone's `*-soft`/`*-border` pair.
export function Tag({
  children,
  tone = "default",
  variant = "inset",
  className,
  title,
}: {
  children: ReactNode
  tone?: Tone
  variant?: "inset" | "soft"
  className?: string
  title?: string
}) {
  const shell =
    variant === "soft" ? (SOFT_SHELL[tone] ?? "border-border bg-inset") : "border-border bg-inset"
  return (
    <span
      title={title}
      className={`inline-flex items-center gap-1 border px-1.5 py-0.5 font-mono text-2xs ${shell} ${TONE[tone]}${className ? ` ${className}` : ""}`}
    >
      {children}
    </span>
  )
}
