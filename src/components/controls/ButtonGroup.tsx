import type { ReactNode } from "react"

/// Compact reusable button cluster — the shared grammar for "two or more
/// buttons that act as one row". Absorbs the hand-rolled
/// `gap-0.5` + `!px-0` icon-cluster idiom (e.g. the sessions selection
/// toolbar) and the looser labeled-action rows.
///
/// - `space="tight"` → `gap-0.5`: the icon-only cluster grammar (OrderedChain
///   move/remove, selection toolbars). Pair with `size="xs"` buttons.
/// - `space="loose"` → `gap-2`: labeled pairs and dialog footers.
/// - `justify="between"` renders a full-width row (cancel left, confirm right)
///   for dialog footers.
///
/// This is a container only — members keep their own `variant`/`size`/`title`.
/// For a single-select control use `SegmentedControl`; this is for action
/// clusters, not selection.
export function ButtonGroup({
  children,
  className,
  justify = "start",
  space = "tight",
  wrap = false,
}: {
  children: ReactNode
  className?: string
  /** `between` = full-width justify-between (dialog footers), `end` = right. */
  justify?: "start" | "between" | "end"
  /** `tight` = gap-0.5 icon-cluster grammar; `loose` = gap-2 labeled pairs. */
  space?: "tight" | "loose"
  wrap?: boolean
}) {
  // Lookup table instead of the nested ternary (no-nested-ternary rule).
  const JUSTIFY_CLASS: Record<"start" | "between" | "end", string> = {
    start: "",
    between: "w-full justify-between",
    end: "w-full justify-end",
  }
  return (
    <div
      className={`inline-flex items-center ${JUSTIFY_CLASS[justify]} ${
        space === "tight" ? "gap-0.5" : "gap-2"
      }${wrap ? " flex-wrap" : ""}${className ? ` ${className}` : ""}`}
    >
      {children}
    </div>
  )
}
