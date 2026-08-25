import type { KeyboardEvent, ReactNode } from "react"

interface TabItem {
  id: string
  label: ReactNode
}

type Variant = "segmented" | "underline"

/// Tab strip. Two real variants (no longer a flat row for both):
///   • `segmented` — a shared bordered row; the active tab fills with
///     `accent-soft` and lifts to accent text. Reads as a control.
///   • `underline` — flat text row with a 2px accent underline on the active
///     tab. Reads as navigation.
/// Bracket `[ ]` reveal stays as a hover layer on inactive tabs.
///
/// WAI-ARIA keyboard pattern: ArrowLeft/Right moves focus roving-tabindex
/// style, and Enter/Space (native button behavior) activates. The tablist
/// has a single tab stop — only the active tab is focusable.
export function Tabs({
  items,
  value,
  onChange,
  className,
  size = "md",
  variant = "segmented",
  fullWidth = false,
  ariaLabel,
}: {
  items: TabItem[]
  value: string
  onChange: (id: string) => void
  className?: string
  size?: "sm" | "md"
  variant?: Variant
  fullWidth?: boolean
  /** Accessible name for the tablist — required when more than one Tabs
   * instance appears on a page (nested tabs). */
  ariaLabel?: string
}) {
  const t = size === "sm" ? "text-xs" : "text-sm"
  const pad = size === "sm" ? "px-2 py-1" : "px-2.5 py-1.5"

  // Roving-tabindex arrow handling shared by both variants.
  const onKeyDown = (e: KeyboardEvent<HTMLButtonElement>) => {
    if (e.key !== "ArrowRight" && e.key !== "ArrowLeft") return;
    e.preventDefault();
    const idx = items.findIndex((it) => it.id === value);
    if (idx === -1) return;
    const dir = e.key === "ArrowRight" ? 1 : -1;
    const next = items[(idx + dir + items.length) % items.length];
    onChange(next.id);
    // Move focus to the newly-selected tab (it is the only tab stop).
    const target = document.getElementById(`tab-${next.id}`);
    target?.focus();
  };

  const tabButton = (it: TabItem, active: boolean, cls: string) => (
    <button
      key={it.id}
      id={`tab-${it.id}`}
      type="button"
      role="tab"
      aria-selected={active}
      tabIndex={active ? 0 : -1}
      aria-controls={ariaLabel ? `${ariaLabel}-panel` : undefined}
      onClick={() => onChange(it.id)}
      onKeyDown={onKeyDown}
      className={cls}
    >
      {it.label}
    </button>
  )

  if (variant === "underline") {
    return (
      <div
        className={`inline-flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-border ${
          fullWidth ? "w-full" : ""
        }${className ? ` ${className}` : ""}`}
        role="tablist"
        aria-label={ariaLabel}
      >
        {items.map((it) => {
          const active = it.id === value
          return tabButton(
            it,
            active,
            `relative -mb-px ${pad} ${t} font-medium transition-[color,box-shadow] duration-fast focus-visible:outline-none focus-visible:shadow-focus ${
              active
                ? "text-accent border-b-2 border-accent"
                : "text-muted hover:text-fg border-b-2 border-transparent"
            }`,
          )
        })}
      </div>
    )
  }

  // segmented
  return (
    <div
      className={`inline-flex flex-wrap items-stretch border border-border bg-inset ${
        fullWidth ? "w-full" : ""
      }${className ? ` ${className}` : ""}`}
      role="tablist"
      aria-label={ariaLabel}
    >
      {items.map((it) => {
        const active = it.id === value
        return tabButton(
          it,
          active,
          `brackets-state ${pad} ${t} font-medium transition-[background-color,color] duration-fast focus-visible:outline-none focus-visible:shadow-focus ${
            active
              ? "bg-accent-soft text-accent"
              : "text-muted hover:text-fg hover:bg-raised"
          } border-r last:border-r-0 border-border`,
        )
      })}
    </div>
  )
}
