import { Fragment, type ReactNode } from "react"
import { Tip } from "../ui/tooltip"

export interface SegmentItem<T extends string> {
  value: T
  label: ReactNode
  /** Optional hover/focus tooltip (the `Tip` compound — provider is global).
   *  Design rule: an item is ONE label only; anything more (explanations,
   *  defaults, units) goes in the tooltip, never a second text element. */
  tooltip?: string
  disabled?: boolean
}

/// Boxed single-select for "pick exactly one of N" choices — the radio-group
/// replacement that stops the "two/three bare switches standing in for a
/// single choice" anti-pattern (e.g. the old Settings detection-cadence +
/// log-retention rows). One shared bordered row; the active segment fills
/// with `accent-soft` and lifts to accent text. Still 0-radius, no shadow.
///
/// For boolean on/off use the `Switch` instead — this is exclusively for
/// enumerated single-select.
export function SegmentedControl<T extends string>({
  items,
  value,
  onChange,
  size = "md",
  fullWidth = false,
  className,
  ariaLabel,
}: {
  items: SegmentItem<T>[]
  value: T
  onChange: (next: T) => void
  size?: "sm" | "md"
  fullWidth?: boolean
  className?: string
  ariaLabel?: string
}) {
  // Every item shares one font size (text-xs) so a long label never renders
  // differently from its neighbours. flex-nowrap + whitespace-nowrap keep the
  // whole group on a single line — a too-wide group wraps as a unit in the
  // caller's flex-wrap container instead of squeezing items into two rows.
  // min-w-max lets an item widen to fit a long label (the group extends with
  // it) instead of the label being compressed; flex-1 still evens out items
  // when there is spare width.
  const pad = size === "sm" ? "px-2 py-1 text-xs" : "px-2 py-1.5 text-xs"
  return (
    <div
      role="radiogroup"
      aria-label={ariaLabel}
      className={`inline-flex flex-nowrap items-stretch border border-border bg-inset ${
        fullWidth ? "w-full" : ""
      }${className ? ` ${className}` : ""}`}
    >
      {items.map((it) => {
        const active = it.value === value
        const button = (
          <button
            type="button"
            role="radio"
            aria-checked={active}
            disabled={it.disabled}
            onClick={() => !it.disabled && onChange(it.value)}
            className={`relative flex-1 min-w-max whitespace-nowrap ${pad} font-medium transition-[background-color,color] duration-fast focus-visible:outline-none focus-visible:shadow-focus disabled:opacity-40 disabled:pointer-events-none ${
              active
                ? "bg-accent-soft text-accent"
                : "text-muted hover:text-fg hover:bg-raised"
            } ${items.length > 1 ? "border-r last:border-r-0 border-border" : ""}`}
          >
            {it.label}
          </button>
        )
        return it.tooltip ? (
          <Tip key={it.value} content={it.tooltip}>
            {button}
          </Tip>
        ) : (
          <Fragment key={it.value}>{button}</Fragment>
        )
      })}
    </div>
  )
}
