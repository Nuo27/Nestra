import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Settings2 } from "lucide-react";
import {
  endpointFetchQuota,
  endpointGet,
  type EndpointInfo,
  type EndpointQuota,
} from "../ipc";
import { Card } from "../components/controls/Card";
import { Button } from "../components/controls/Button";
import { ButtonGroup } from "../components/controls/ButtonGroup";
import { KeepAlivePopover } from "../components/controls/KeepAlivePopover";
import { QuotaSettingsDialog } from "../components/controls/QuotaSettingsDialog";
import { QuotaRow, QuotaSkeletonRows } from "../components/display/QuotaRow";
import { Page } from "../components/layout/Page";
import { PageHeader, BackLink } from "../components/layout/PageHeader";
import { Skeleton } from "../components/ui/skeleton";
import { useUI } from "../stores/ui";
import { qk } from "../lib/queries";
import { useQuotaRefresh } from "../lib/quotaRefresh";
import { BUILTIN_LABEL, planLabel } from "../lib/quota";

export function QuotaPage({ id }: { id: string }) {
  const { t } = useTranslation();
  const endpointQ = useQuery({ queryKey: qk.endpoint(id), queryFn: () => endpointGet(id) });
  const endpoint = endpointQ.data;

  return (
    <Page>
      <PageHeader
        back={<BackLink to="/">{t("nav.providers")}</BackLink>}
        title={endpoint?.display_name || id}
      />
      {endpoint ? (
        <QuotaCard endpoint={endpoint} />
      ) : endpointQ.isLoading ? (
        <Card padding="lg">
          <Skeleton className="h-4 w-20" />
          <div className="mt-4 space-y-3">
            <QuotaSkeletonRows />
          </div>
        </Card>
      ) : (
        <div className="text-sm text-danger">{t("quota.providerNotFound")}</div>
      )}
    </Page>
  );
}

function QuotaCard({ endpoint }: { endpoint: EndpointInfo }) {
  const { t } = useTranslation();
  const auto = useUI((s) => s.quotaAuto);
  const intervalSec = useUI((s) => s.quotaIntervalSec);
  const quotaCache = useUI((s) => s.quotaCache);
  const setQuotaCache = useUI((s) => s.setQuotaCache);
  const cached = quotaCache[endpoint.id] ?? null;

  // Per-provider card title: the endpoint URL carries the kind signal
  // (z.ai / MiniMax have their own tier/plan copy).
  const endpointUrl =
    endpoint.protocols.find((p) => p.protocol === "openai-comp" || p.protocol === "custom")
      ?.base_url ??
    endpoint.protocols[0]?.base_url ??
    "";
  // Host-based card flavor: z.ai → lite, MiniMax → token plan, else default.
  let cardTitle = t("quota.cardTitle");
  if (endpointUrl.includes("z.ai")) cardTitle = t("quota.cardLite");
  else if (endpointUrl.includes("minimax")) cardTitle = t("quota.cardTokenPlan");

  const q = useQuery<EndpointQuota>({
    queryKey: qk.endpointQuota(endpoint.id),
    queryFn: () => endpointFetchQuota(endpoint.id),
    staleTime: 60_000,
    gcTime: 30 * 60_000,
    refetchInterval: auto ? intervalSec * 1000 : false,
    // Keep polling while the window is hidden (minimized / occluded /
    // closed-to-tray): quota freshness is the whole point of auto-refresh.
    refetchIntervalInBackground: true,
    refetchOnMount: false,
    // Catch up the moment the user returns — covers OS suspend/wake and any
    // timer throttling during long hidden periods. Skips when data is fresh
    // (staleTime).
    refetchOnWindowFocus: true,
  });
  const data = q.data;
  const qc = useQueryClient();
  useEffect(() => {
    if (data?.ok && data.items.length > 0) {
      setQuotaCache(endpoint.id, data);
      // A successful fetch may have flipped `provisioned` server-side (the
      // provisioning side-effect in `endpoint_fetch_quota`). Refresh the
      // settings blob so the gate state (keep-alive switch + bars) updates
      // without forcing a manual "verify query" click on the first hit.
      qc.invalidateQueries({ queryKey: qk.quotaRefresh() });
    }
  }, [data, endpoint.id, setQuotaCache, qc]);

  const fresh = data?.ok && data.items.length > 0 ? data : null;
  const shown = fresh ?? cached;
  // First-ever visit (no TanStack cache, no app cache): show skeleton bars.
  // Background refetch while we already have data: keep showing the data, just
  // spin the refresh icon — never blank the bars mid-read.
  const isFirstLoad = q.isFetching && !shown;

  // Names of every quota item the provider exposed in the latest fetch
  // (cached or fresh). These populate the "target quota" picker so the user
  // chooses exactly which window the worker tracks.
  const targetItems = shown?.items ?? [];
  const rf = useQuotaRefresh(endpoint, targetItems);
  const { plan, planActive, provisioned, canArm, verifyQuery } = rf;

  // Countdown until next automatic refetch. We track the last successful
  // fetch timestamp from TanStack and tick a local interval so the UI can
  // surface "next refresh in 18s" without polling the query.
  const nextRefreshMs = q.dataUpdatedAt > 0 && auto ? q.dataUpdatedAt + intervalSec * 1000 : 0;
  const [now, setNow] = useState(() => Date.now());
  // One interval for the lifetime of `auto`  — NOT keyed on nextRefreshMs, so a
  // refetch no longer tears it down + recreates it (the old recreation left
  // `now` stale for up to 1s, which combined with rounding skipped seconds).
  useEffect(() => {
    if (!auto) return;
    setNow(Date.now()); // re-sync when auto flips on
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [auto]);
  // Re-sync the instant a fetch resolves so the countdown restarts cleanly at
  // the full interval (floor — shows exactly N, not N-1 / N+1).
  useEffect(() => {
    setNow(Date.now());
  }, [q.dataUpdatedAt]);
  const secsLeft = nextRefreshMs > 0 ? Math.max(0, Math.floor((nextRefreshMs - now) / 1000)) : 0;
  // "sending request" only while a fetch is genuinely in flight — a due-but-
  // not-yet-fired tick or a paused interval must not stick the label on.
  const sending = auto && shown && q.isFetching;

  const [settingsOpen, setSettingsOpen] = useState(false);

  return (
    <Card padding="lg">
      <div className="flex items-center justify-between gap-4">
        <div className="flex items-center gap-3">
          <div className="text-md font-medium tracking-[-0.01em]">{cardTitle}</div>
          {/* Query-plan status — the active handle for quota. "not configured"
              is muted so the eye is drawn to the bars area's configure CTA. */}
          <span
            className={`font-mono text-xs ${planActive ? "text-subtle" : "text-muted"}`}
            title={t("quota.planTitleHint")}
          >
            {t("quota.planStatus")} ·{" "}
            {t(planLabel(plan), {
              kind: plan.source === "preset" ? BUILTIN_LABEL[plan.kind] : "",
            })}
          </span>
          {/* Live status: countdown to next auto-fetch, or "sending request"
              while a fetch is in flight / just due. Never blanks the bars. */}
          {shown && auto &&
            (sending ? (
              <span className="font-mono text-xs text-accent">{t("quota.sending")}</span>
            ) : (
              <span className="font-mono text-xs text-subtle tabular">{t("quota.nextIn", { n: secsLeft })}</span>
            ))}
        </div>
        <ButtonGroup space="loose">
          <KeepAlivePopover endpointId={endpoint.id} />
          {/* Verify: prominent when a plan is set but not yet provisioned.
              Refresh (below) does the same fetch; this CTA makes the gate
              explicit so the user knows to confirm data before arming. */}
          {planActive && !provisioned && (
            <Button
              variant="primary"
              size="sm"
              disabled={q.isFetching}
              loading={q.isFetching}
              onClick={verifyQuery}
            >
              {q.isFetching ? t("quota.verifying") : t("quota.verifyQuery")}
            </Button>
          )}
          <Button
            variant="ghost"
            size="sm"
            disabled={q.isFetching}
            loading={q.isFetching && provisioned}
            onClick={() =>
              qc.invalidateQueries({ queryKey: qk.endpointQuota(endpoint.id) })
            }
          >
            {q.isFetching ? t("quota.refreshing") : t("common.refresh")}
          </Button>
          <Button
            variant="ghost"
            size="sm"
            onClick={() => setSettingsOpen(true)}
            aria-label={t("quota.settingsTitle")}
            title={t("quota.settingsTitle")}
          >
            <Settings2 data-icon size={14} />
          </Button>
        </ButtonGroup>
      </div>

      <QuotaSettingsDialog
        endpoint={endpoint}
        open={settingsOpen}
        onOpenChange={setSettingsOpen}
        rf={rf}
        targetItems={targetItems}
        currentPlan={shown?.plan ?? null}
      />

      {isFirstLoad && <div className="mt-4 space-y-3"><QuotaSkeletonRows /></div>}

      {/* Bars are gated: with no query plan, or before the first verifying
          fetch succeeds, collapse to a configure/verify CTA instead of
          showing skeleton or stale error text. The verify-failed error is
          still surfaced so the user sees why the query didn't yield data. */}
      {!isFirstLoad && !canArm && (
        <div className="mt-4 space-y-2">
          <div className="text-sm text-muted">
            {planActive
              ? t("quota.verifyToSee")
              : t("quota.configureToSee")}
          </div>
          {planActive && data?.error && (
            <pre className="whitespace-pre-wrap break-all font-mono text-xs text-danger">
              {data.error}
            </pre>
          )}
          {planActive && !provisioned && (
            <div className="flex justify-end">
              <Button variant="primary" size="sm" disabled={q.isFetching} loading={q.isFetching} onClick={verifyQuery}>
                {q.isFetching ? t("quota.verifying") : t("quota.verifyQuery")}
              </Button>
            </div>
          )}
        </div>
      )}

      {!isFirstLoad && canArm && !shown && (
        <div className="mt-4 text-sm text-muted">{data?.error ?? t("quota.noData")}</div>
      )}

      {shown && canArm && (
        <>
          <div className="mt-4 space-y-3">
            {shown.items.map((it) => (
              <QuotaRow key={it.name} item={it} />
            ))}
          </div>
        </>
      )}
    </Card>
  );
}
