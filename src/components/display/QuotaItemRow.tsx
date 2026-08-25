import { useTranslation } from "react-i18next"
import { AsciiBar } from "./AsciiBar"

/// One quota item as an ASCII bar row — shared between the full Quota page
/// (`size="md"`, with used/total detail) and the compact provider-card preview
/// (`size="sm"`, mono, with a `↻ resets in` line). Absorbs the two near-identical
/// QuotaRow / quota-preview-row implementations.
///
/// `isBalance` renders a monetary balance (OpenRouter credits, Moonshot
/// balance) as a single line — name + remaining amount. No percentage, no
/// bar, no reset line: a balance has no window to fill, so a fill ratio
/// would be meaningless (and OpenRouter's `limit` is null for unlimited
/// keys anyway).
///
/// `quiet` (card previews) mutes the bar: `fine` glyphs (■/·) and no pulsing
/// cursor — the bar stays readable but steps out of the way of the card's
/// other content.
export function QuotaItemRow({
  name,
  pct,
  detail,
  resetsIn,
  showReset = true,
  size = "md",
  quiet = false,
  isBalance = false,
}: {
  name: string
  pct: number
  detail?: string | null
  resetsIn?: string | null
  showReset?: boolean
  size?: "sm" | "md"
  /** Mute the bar for compact previews (fine glyphs, no pulse). */
  quiet?: boolean
  /** Balance-based item: render remaining amount only, no bar/percent. */
  isBalance?: boolean
}) {
  const { t } = useTranslation()
  // NaN/Infinity/undefined from upstream JSON must not render "NaN%" or
  // corrupt the bar — clamp only finite values, else 0.
  const p = Number.isFinite(pct) ? Math.max(0, Math.min(100, pct)) : 0
  const head =
    size === "sm"
      ? "flex items-baseline justify-between gap-2 font-mono text-xs"
      : "mb-1.5 flex items-baseline justify-between text-sm"
  const reset =
    size === "sm" ? "text-subtle" : "mt-1 text-xs text-subtle"

  // Balance: one quiet line — name left, remaining amount right. The
  // amount is the whole point; a percentage/bar would be noise.
  if (isBalance) {
    return (
      <div className={head}>
        <span className="min-w-0 truncate text-muted">{name}</span>
        <span className="shrink-0 tabular text-fg">{detail ?? "—"}</span>
      </div>
    )
  }

  return (
    <div>
      <div className={head}>
        <span className="min-w-0 truncate text-muted">{name}</span>
        <span className="shrink-0 tabular">
          <span className="text-fg">{Math.round(p)}%</span>
          {detail && (
            <span className="ml-2 text-subtle">{detail}</span>
          )}
        </span>
      </div>
      <div className={size === "md" ? "mt-1 font-mono text-xs" : ""}>
        {quiet ? (
          <AsciiBar value={p} size="fine" pulse={false} />
        ) : (
          <AsciiBar value={p} />
        )}
      </div>
      {showReset && resetsIn && (
        <div className={reset}>
          {size === "sm" ? "↻ " : ""}{t("quota.resetsIn", { n: resetsIn })}
        </div>
      )}
    </div>
  )
}
