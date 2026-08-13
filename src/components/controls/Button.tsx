import * as React from "react"
import { Spinner } from "../ui/spinner"

type Variant = "primary" | "secondary" | "ghost" | "subtle" | "danger"
type Size = "xs" | "sm" | "md"

/// Single button component for the whole app. Variant = surface grammar,
/// size = density. Two visual tiers:
///   • Solid tier  — `primary` (filled accent CTA) + `secondary` (outline box
///     with a real hit-target). These carry `.surface-action` / `.surface-outline`
///     so they read as "press me" affordances, not floating text.
///   • Text tier   — `ghost` / `subtle` / `danger` stay text-only and reveal
///     the `[ ]` ring via `.brackets-state` on hover / focus / active. Use for
///     tertiary actions, inline links, and destructive confirmations.
/// Replaces the prior Button + IconButton + Link + CopyButton split.
const VARIANT_CLASS: Record<Variant, string> = {
  // Solid tier — real surfaces, no bracket reveal. Primary is a quiet filled
  // accent with NO border (the fill is the affordance; a border would add
  // visual weight it doesn't need).
  primary: "surface-action hover:brightness-110 font-medium",
  secondary:
    "surface-outline hover:bg-raised hover:border-accent-border font-medium",
  // Text tier — bracket-as-state.
  ghost: "brackets-state text-muted hover:text-fg",
  subtle: "brackets-state text-subtle hover:text-muted",
  danger: "brackets-state text-danger hover:brightness-110",
}

/// Variants that render as a solid box (real hit-target, surface fill).
/// Used to pick height/padding vs. the text-tier buttons.
const SOLID = new Set<Variant>(["primary", "secondary"])

const SIZE_CLASS: Record<Size, string> = {
  xs: "h-5 px-1.5 text-xs gap-1 [&_[data-icon]]:size-3",
  sm: "h-6 px-2 text-xs gap-1.5 [&_[data-icon]]:size-3.5",
  md: "h-7 px-2.5 text-sm gap-2 [&_[data-icon]]:size-4",
}

/// Text-tier buttons don't impose a fixed height (they're inline text), so
/// they use vertical padding instead — keeps them from dwarfing inline copy.
const TEXT_SIZE_CLASS: Record<Size, string> = {
  xs: "py-0.5 px-1 text-xs gap-1 [&_[data-icon]]:size-3",
  sm: "py-1 px-1.5 text-xs gap-1.5 [&_[data-icon]]:size-3.5",
  md: "py-1.5 px-2 text-sm gap-2 [&_[data-icon]]:size-4",
}

const BASE =
  "inline-flex items-center justify-center whitespace-nowrap cursor-pointer select-none transition-[color,background-color,border-color,box-shadow,filter] duration-fast ease-out focus-visible:outline-none focus-visible:shadow-focus active:brightness-110 disabled:pointer-events-none disabled:opacity-40 disabled:cursor-not-allowed [&_[data-icon]]:shrink-0 [&_[data-icon]]:pointer-events-none"

export function Button({
  variant = "ghost",
  size = "md",
  loading = false,
  type = "button",
  className,
  disabled,
  children,
  ...rest
}: Omit<React.ButtonHTMLAttributes<HTMLButtonElement>, "className"> & {
  variant?: Variant
  size?: Size
  loading?: boolean
  className?: string
}) {
  const solid = SOLID.has(variant)
  const sizeClass = solid ? SIZE_CLASS[size] : TEXT_SIZE_CLASS[size]
  return (
    <button
      type={type}
      className={`${BASE} ${VARIANT_CLASS[variant]} ${sizeClass}${className ? ` ${className}` : ""}`}
      disabled={disabled || loading}
      aria-busy={loading || undefined}
      {...rest}
    >
      {loading && <Spinner size={size === "xs" ? 12 : 14} />}
      {children}
    </button>
  )
}
