import {
  Fragment,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react"
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
/// single choice" anti-pattern. One shared bordered row; an absolutely-
/// positioned thumb (`bg-accent-soft`) tracks the active item with a
/// 150ms `ease-spring` slide — `--ease-spring` exists for this single use,
/// giving the switch a mechanical settle that reads as deliberate.
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
  const pad = size === "sm" ? "px-2 py-1 text-xs" : "px-2 py-1.5 text-xs"
  const containerRef = useRef<HTMLDivElement>(null)
  const itemRefs = useRef(new Map<string, HTMLButtonElement>())
  const [thumb, setThumb] = useState<{ left: number; width: number }>({ left: 0, width: 0 })

  // Measure the active item — runs after layout, on value/item-count change,
  // and on resize. Initial render with no measurement is hidden via the
  // `opacity-0` until first measure lands, so the thumb never flashes in
  // its old position during a remount.
  useLayoutEffect(() => {
    const container = containerRef.current
    if (!container) return
    const measure = () => {
      const btn = itemRefs.current.get(value)
      if (!btn) return
      const cRect = container.getBoundingClientRect()
      const bRect = btn.getBoundingClientRect()
      setThumb({ left: bRect.left - cRect.left, width: bRect.width })
    }
    measure()
    const ro = new ResizeObserver(measure)
    ro.observe(container)
    window.addEventListener("resize", measure)
    return () => {
      ro.disconnect()
      window.removeEventListener("resize", measure)
    }
  }, [value, items])

  return (
    <div
      role="radiogroup"
      aria-label={ariaLabel}
      ref={containerRef}
      className={`relative inline-flex flex-nowrap items-stretch border border-border bg-inset ${
        fullWidth ? "w-full" : ""
      }${className ? ` ${className}` : ""}`}
    >
      {/* Sliding thumb — sits under the labels, behind them, animates via
          transform+width transitions. `ease-spring` is reserved for this
          single use per DESIGN.md §11. */}
      <span
        aria-hidden="true"
        className="pointer-events-none absolute top-0 bottom-0 bg-accent-soft transition-[transform,width,opacity] ease-spring duration-150 will-change-transform"
        style={{
          transform: `translateX(${thumb.left}px)`,
          width: thumb.width,
          opacity: thumb.width > 0 ? 1 : 0,
        }}
      />
      {items.map((it) => {
        const active = it.value === value
        const button = (
          <button
            ref={(el) => {
              if (el) itemRefs.current.set(it.value, el)
              else itemRefs.current.delete(it.value)
            }}
            type="button"
            role="radio"
            aria-checked={active}
            disabled={it.disabled}
            onClick={() => !it.disabled && onChange(it.value)}
            className={`relative z-10 flex-1 min-w-max whitespace-nowrap ${pad} font-medium transition-colors duration-fast focus-visible:outline-none focus-visible:shadow-focus disabled:opacity-40 disabled:pointer-events-none ${
              active ? "text-accent" : "text-muted hover:text-fg"
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