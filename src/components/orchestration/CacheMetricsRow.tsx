import { useTranslation } from "react-i18next";

/// Compact prompt-cache metric row. `cache_creation` / `cache_read` are the
/// observed token counts on `route_request`; `strategy` is the `CacheStrategy`
/// that was in effect (identity.rs:334-353). Each strategy gets a glyph so the
/// row reads as a console annotation:
///
///   off                  → `·`  (no cache strategy)
///   anthropic_explicit   → `▲`  (cache_control injection, gated by policy)
///   deepseek_auto        → `~`  (URL-detected, no body mutation)
///   openrouter_passthrough → `⇉` (URL-detected, no body mutation)
export type CacheStrategy =
  | "off"
  | "anthropic_explicit"
  | "deepseek_auto"
  | "openrouter_passthrough";

const STRATEGY_GLYPH: Record<CacheStrategy, string> = {
  off: "·",
  anthropic_explicit: "▲",
  deepseek_auto: "~",
  openrouter_passthrough: "⇉",
};

export function CacheMetricsRow({
  creation,
  read,
  strategy = "off",
}: {
  creation?: number | null;
  read?: number | null;
  strategy?: CacheStrategy;
}) {
  const { t } = useTranslation();
  const hasMetrics = creation !== null && creation !== undefined
    || read !== null && read !== undefined;
  if (!hasMetrics && strategy === "off") {
    return (
      <span className="font-mono text-2xs text-subtle">
        <span aria-hidden>{STRATEGY_GLYPH.off}</span> {t("cache.noCache")}
      </span>
    );
  }
  const strategyLabel = t(`cache.strategy.${strategy}`);
  return (
    <span
      className="inline-flex items-center gap-2 font-mono text-2xs text-subtle tabular"
      title={t("cache.strategyTitle", { strategy: strategyLabel })}
    >
      <span aria-hidden className="text-accent">
        {STRATEGY_GLYPH[strategy] ?? STRATEGY_GLYPH.off}
      </span>
      {hasMetrics && (
        <span>
          <span className="text-muted">{creation ?? 0}</span>
          <span className="text-subtle"> {t("cache.create")}</span>
          <span className="mx-1 text-subtle">/</span>
          <span className="text-muted">{read ?? 0}</span>
          <span className="text-subtle"> {t("cache.read")}</span>
        </span>
      )}
      {!hasMetrics && <span className="text-subtle">{strategyLabel}</span>}
    </span>
  );
}
