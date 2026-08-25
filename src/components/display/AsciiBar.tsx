import { useEffect, useRef, useState } from "react";

/// ASCII block-character bar that fills the inner width of its parent
/// container, measured live via ResizeObserver so it tracks the real card
/// width at any viewport. The left/right space comes from the parent's
/// padding — this bar only fills the content box it's placed in.
///
/// `value` is clamped to 0–100, shown as a whole percent. Visual treatment
/// is terminal-flavoured: a thin gap between every cell gives a "lossy"
/// segmented look, the filled body carries a soft colour glow, and the
/// leading-edge cell pulses like a cursor. Colour is auto-assigned by
/// depletion severity so a near-full quota reads red without the caller
/// picking colour.
type GlyphSet = "block" | "fine";

const FILLED: Record<GlyphSet, string> = {
  block: "█",
  fine: "■",
};
const TRACK: Record<GlyphSet, string> = {
  block: "░",
  fine: "·",
};

/// Thin space inserted between cells for the lossy segmented look. Being
/// a real glyph it folds into the unit-width measurement, so the cell count
/// still fits the container.
const GAP = "\u2009";

/// Depletion thresholds for the auto tone. ≥80% used = danger (near reset),
/// ≥50% = warning, else success.
function autoTone(pct: number): "success" | "warning" | "danger" {
  if (pct >= 80) return "danger";
  if (pct >= 50) return "warning";
  return "success";
}

const TONE_CLASS: Record<"success" | "warning" | "danger", string> = {
  success: "text-success",
  warning: "text-warning",
  danger: "text-danger",
};

/// Floor on the computed cell count so tiny containers stay legible.
const MIN_CELLS = 8;

export function AsciiBar({
  value,
  size = "block",
  pulse = true,
}: {
  value: number;
  size?: GlyphSet;
  /** Pulse the leading-edge cursor cell (disable for static contexts). */
  pulse?: boolean;
}) {
  const fillGlyph = FILLED[size];
  const trackGlyph = TRACK[size];

  // Measure the rendered width of one *unit* (glyph + gap) so the cell count
  // fits the container exactly even with the inter-cell spacing. `clientWidth`
  // is the inner content box, so the bar already respects the parent's
  // left/right padding.
  const unit = fillGlyph + GAP;
  const measureRef = useRef<HTMLSpanElement | null>(null);
  const sizerRef = useRef<HTMLSpanElement | null>(null);
  const [measuredCells, setMeasuredCells] = useState(MIN_CELLS);

  useEffect(() => {
    const el = measureRef.current;
    const sizer = sizerRef.current;
    if (!el || !sizer) return;
    const recompute = () => {
      const unitW = sizer.getBoundingClientRect().width;
      if (unitW > 0) {
        const n = Math.max(MIN_CELLS, Math.floor(el.clientWidth / unitW));
        setMeasuredCells((cur) => (cur === n ? cur : n));
      }
    };
    recompute();
    const ro = new ResizeObserver(recompute);
    ro.observe(el);
    return () => ro.disconnect();
  }, [unit]);

  // NaN/Infinity must not render "NaN%" or make `String.repeat` throw on
  // negative/Infinity cell counts — clamp to finite, non-negative values.
  const safeValue = Number.isFinite(value) ? value : 0;
  const pct = Math.max(0, Math.min(100, safeValue));
  const pctInt = Math.round(pct);
  const safeCells = Number.isFinite(measuredCells)
    ? Math.max(0, Math.floor(measuredCells))
    : 0;
  const filled = Math.round((pct / 100) * safeCells);
  const fillClassFinal = TONE_CLASS[autoTone(pct)];

  // Body = all filled cells except the bright pulsing leading-edge cursor.
  const bodyLen = Math.max(0, filled - 1);
  const hasCursor = filled > 0;
  const bodyStr = `${fillGlyph}${GAP}`.repeat(bodyLen);
  const cursorStr = `${fillGlyph}${GAP}`;
  const trackStr = `${trackGlyph}${GAP}`.repeat(Math.max(0, safeCells - filled));

  return (
    <span
      ref={measureRef}
      className="relative block whitespace-pre tabular leading-none"
      title={`${pctInt}%`}
    >
      {/* hidden unit (glyph + gap) used to sample the rendered cell width */}
      <span ref={sizerRef} className="pointer-events-none absolute opacity-0">
        {unit}
      </span>
      {hasCursor && (
        <span className={`${fillClassFinal} asciibar-fill`}>{bodyStr}</span>
      )}
      {hasCursor && (
        <span
          className={`${fillClassFinal}${pulse ? " asciibar-cursor" : " asciibar-fill"}`}
        >
          {cursorStr}
        </span>
      )}
      <span className="text-subtle">{trackStr}</span>
    </span>
  );
}
