import { useTranslation } from "react-i18next";
import type { RouteRecord } from "../../ipc/orchestration";
import { RouteBadge, type RouteReason } from "./RouteBadge";
import { CacheMetricsRow } from "./CacheMetricsRow";
import { formatRelative } from "../../lib/format";

/// Read-only per-task route-history timeline. Each row is one `RouteRecord`
/// (one HTTP request; retries/migrations within a Task produce new rows).
/// Renders the requested → resolved mapping, the route reason, observed
/// outcome (status, tokens), and prompt-cache metrics. This is the
/// "why this provider/model for this task" view that a migration decision
/// consults and that `/sessions/:id` will surface.
///
/// Empty state is intentionally NOT handled here — callers decide what an
/// empty lineage means (often "no gateway traffic yet").
export function RouteLineage({ records }: { records: RouteRecord[] }) {
  const { t } = useTranslation();
  if (records.length === 0) {
    return null;
  }
  return (
    <ol className="flex flex-col">
      {records.map((r, i) => (
        <li
          key={r.request_id}
          className="flex flex-col gap-1.5 border-l border-border pl-3 pr-1 py-2 relative last:border-l-2 last:border-l-accent"
        >
          <span className="absolute -left-[3px] top-3 h-1.5 w-1.5 bg-border last:bg-accent" />
          <div className="flex items-center gap-2">
            <span className="font-mono text-2xs text-subtle tabular">
              {String(i + 1).padStart(2, "0")}
            </span>
            <RouteBadge reason={(r.route_reason as RouteReason) ?? "explicit"} />
            {r.generation_broken && (
              <span
                className="font-mono text-2xs text-danger"
                title={t("route.genBrokenTip")}
              >
                {t("route.genBroken")}
              </span>
            )}
            <span className="ml-auto font-mono text-2xs text-subtle tabular">
              {formatRelative(r.started_at)}
            </span>
          </div>
          <div className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5 font-mono text-xs">
            <span className="text-subtle">{t("route.req")}:</span>
            <span className="text-muted">
              {r.requested_provider ?? "—"} / {r.requested_model ?? "—"}
            </span>
            <span className="text-accent">→</span>
            <span className="text-fg">
              {r.resolved_endpoint_id ?? "—"} / {r.resolved_model ?? "—"}
            </span>
          </div>
          {(r.http_status != null ||
            r.usage_input != null ||
            r.usage_output != null ||
            r.cache_creation != null ||
            r.cache_read != null) && (
            <div className="flex flex-wrap items-center gap-x-3 gap-y-0.5 font-mono text-2xs text-subtle tabular">
              {r.http_status != null && (
                <span>
                  {t("route.http")}{" "}
                  <span className={r.http_status >= 400 ? "text-danger" : "text-fg"}>{r.http_status}</span>
                </span>
              )}
              {(r.usage_input != null || r.usage_output != null) && (
                <span>
                  {t("route.tok")} {r.usage_input ?? 0}/{r.usage_output ?? 0}
                </span>
              )}
              {(r.cache_creation != null || r.cache_read != null) && (
                <CacheMetricsRow
                  creation={r.cache_creation}
                  read={r.cache_read}
                />
              )}
            </div>
          )}
        </li>
      ))}
    </ol>
  );
}
