import { useEffect, useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { useNavigate } from "@tanstack/react-router";
import { HeartPulse } from "lucide-react";
import {
  endpointFetchQuota,
  providerHealthSnapshot,
  quotaKeepaliveStatus,
  quotaRefreshGet,
  type EndpointInfo,
  type EndpointQuota,
  type RefreshEndpointConfig,
} from "../../ipc";
import { useUI } from "../../stores/ui";
import { qk } from "../../lib/queries";
import { isPlanActive, resolvePlan } from "../../lib/quota";
import { DEFAULT_CFG } from "../../lib/quotaRefresh";
import { keepaliveMeta } from "../../lib/keepalive";
import { fmtMoney } from "../../lib/format";
import { useResumeInvalidate } from "../../lib/useResumeInvalidate";
import { Card } from "../controls/Card";
import { Button } from "../controls/Button";
import { StatusDot } from "../feedback/StatusDot";
import { QuotaItemRow } from "../display/QuotaItemRow";
import { Tip } from "../ui/tooltip";
import { Badge } from "../ui/badge";

type BadgeSpec = { tone: "success" | "warning" | "danger" | "neutral"; labelKey: string };

/// Per-endpoint 30-day usage totals folded on the Providers page (one query
/// for all cards). `unknown` = some rows carry tokens but no catalog price.
interface EndpointUsage {
  requests: number;
  input: number;
  output: number;
  cost: number;
  unknown: boolean;
}

/// Waterfall-style provider card. Header shows identity + status; a quiet
/// 30-day usage line sits under it; the quota section lets the user pick
/// which quota window is tracked (the keep-alive worker's
/// `target_quota_name`) and previews it quietly; the footer carries the
/// action buttons. Card height varies with the quota rows, which is what
/// makes the masonry layout work.
export function EndpointRow({
  endpoint,
  usage,
}: {
  endpoint: EndpointInfo;
  usage?: EndpointUsage;
}) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const configured = endpoint.has_api_key;
  const keyInvalid = endpoint.status === "invalid";
  const keyValid = endpoint.status === "valid";

  // Two nested ternaries (status + badge) collapsed into one lookup table —
  // the four (configured, keyInvalid, keyValid) combinations map 1:1.
  const combo = !configured
    ? ("no-key" as const)
    : keyInvalid
      ? ("invalid" as const)
      : keyValid
        ? ("valid" as const)
        : ("unvalidated" as const);
  const STATUS_BY_COMBO: Record<
    "no-key" | "invalid" | "valid" | "unvalidated",
    { status: "ok" | "outdated" | "missing" | "unknown"; badge: BadgeSpec }
  > = {
    "no-key": { status: "unknown", badge: { tone: "neutral", labelKey: "providers.badgeNoKey" } },
    invalid: { status: "missing", badge: { tone: "danger", labelKey: "providers.badgeInvalid" } },
    valid: { status: "ok", badge: { tone: "success", labelKey: "providers.badgeValid" } },
    unvalidated: { status: "outdated", badge: { tone: "warning", labelKey: "providers.badgeUnvalidated" } },
  };
  const { status, badge } = STATUS_BY_COMBO[combo];

  const protoSummary =
    endpoint.protocols.length > 0
      ? endpoint.protocols.map((p) => p.protocol).join(" · ")
      : null;

  const edit = () => navigate({ to: "/providers/$id", params: { id: endpoint.id } });
  const openQuota = () => navigate({ to: "/quota/$id", params: { id: endpoint.id } });

  return (
    <Card
      padding="sm"
      className={keyInvalid ? "opacity-80" : ""}
      footer={
        <div className="flex items-center justify-between">
          <KeepAliveChip endpointId={endpoint.id} />
          <div className="flex items-center gap-2 ml-auto shrink-0">
            <Button variant="ghost" size="sm" onClick={openQuota} title={t("providers.quotaBtnTitle")}>
              {t("providers.quotaBtn")}
            </Button>
            <Button variant="ghost" size="sm" onClick={edit} title={t("providers.editBtnTitle")}>
              {t("common.edit")}
            </Button>
          </div>
        </div>
      }
    >
      {/* Header — identity + status (not clickable; actions live in the
          footer). */}
      <div className="-m-1 flex items-center gap-3 p-1">
        <StatusDot status={status} title={t(badge.labelKey)} />
        <span className="min-w-0 flex-1 truncate text-md font-medium">
          {endpoint.display_name}
        </span>

        {/* Protocol summary — mono, subtle. Hidden on narrow widths to keep
            the row legible (the badge + name take priority); `min-w-0
            truncate` (not shrink-0) so a long dual-protocol label yields to
            the badges instead of pushing the row past the card edge. */}
        {protoSummary && (
          <span className="hidden min-w-0 truncate font-mono text-xs text-subtle sm:inline">
            {protoSummary}
          </span>
        )}

        <Badge tone={badge.tone} variant="soft">
          {t(badge.labelKey)}
        </Badge>

        <BreakerBadge endpointId={endpoint.id} />
      </div>

      {/* 30-day usage — one quiet mono line; endpoints with no traffic
          render nothing (the card stays clean). */}
      {usage && usage.requests > 0 && <UsageLine usage={usage} />}

      {/* Quota preview — renders its own full-width divider + bar, or nothing
          while the query is unverified. */}
      <QuotaSection endpoint={endpoint} />
    </Card>
  );
}

/// Quiet 30-day usage line: requests · in · out · spend. Spend rows without
/// a catalog price carry the `~+` marker (unknown ≠ free), same semantics as
/// the agent usage card.
function UsageLine({ usage }: { usage: EndpointUsage }) {
  const { t } = useTranslation();
  const fmt = (n: number) => n.toLocaleString();
  const fmtUsd = (n: number) =>
    n < 0.01 && n > 0 ? `$${n.toFixed(4)}` : `$${n.toFixed(2)}`;
  return (
    <div className="-mx-3 mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5 border-t border-border px-3 pt-1.5 font-mono text-2xs text-muted tabular">
      <span className="text-subtle">{t("providers.usage30d")}</span>
      <span>
        <span className="text-fg">{fmt(usage.requests)}</span>{" "}
        {t("agentDetail.usageRequests")}
      </span>
      <span>
        {t("agentDetail.usageIn")} <span className="text-fg">{fmt(usage.input)}</span>
      </span>
      <span>
        {t("agentDetail.usageOut")} <span className="text-fg">{fmt(usage.output)}</span>
      </span>
      <span className="ml-auto">
        {t("agentDetail.usageSpend")}{" "}
        <span className="text-fg">
          {usage.cost > 0 || !usage.unknown ? fmtUsd(usage.cost) : "—"}
        </span>
        {usage.unknown && (
          <span className="ml-1 text-warning" title={t("agentDetail.usageSpendUnknown")}>
            ~+
          </span>
        )}
      </span>
    </div>
  );
}

/// Circuit-breaker badge — the live routing health of this endpoint from the
/// gateway's breaker state machine. Renders NOTHING while closed (healthy is
/// the non-event); `open` shows a danger badge with the recovery countdown,
/// `half_open` a warning badge (probe requests in flight). Shares one query
/// key across all cards (TanStack dedupes; 10s refresh is a cheap in-memory
/// read).
function BreakerBadge({ endpointId }: { endpointId: string }) {
  const { t } = useTranslation();
  const q = useQuery({
    queryKey: qk.providerHealth(),
    queryFn: providerHealthSnapshot,
    refetchInterval: 10_000,
  });
  const snap = q.data?.find((s) => s.endpoint_id === endpointId);
  if (!snap || snap.state === "closed") return null;
  if (snap.state === "open") {
    const secs = Math.ceil((snap.recovery_in_ms ?? 0) / 1000);
    return (
      <Tip
        content={t("providers.breakerOpenTip", {
          secs,
          class: snap.last_failure ?? "—",
        })}
      >
        <Badge tone="danger" variant="soft">
          {t("providers.breakerOpen")}
          {/* Model ids (e.g. `moonshotai/kimi-k2`) are unbounded and the
              badge is whitespace-nowrap — cap the visible text and let the
              Tip carry the full id, or a long name overflows the card. */}
          {snap.model && (
            <span className="max-w-40 truncate">{` · ${snap.model}`}</span>
          )}
        </Badge>
      </Tip>
    );
  }
  return (
    <Tip content={t("providers.breakerHalfOpenTip")}>
      <Badge tone="warning" variant="soft">
        {t("providers.breakerHalfOpen")}
      </Badge>
    </Tip>
  );
}

/// Keep-alive status chip for the provider-card footer: `HeartPulse` glyph +
/// the phase label (icon + label, so the state reads at a glance). Rendered
/// ONLY when keep-alive is in use — the `disabled` / `not_configured` phases
/// mean it's off, so the chip is hidden and the card stays uncluttered. The
/// heartbeat is the keep-alive indicator, not part of the quota preview.
/// Phase → display semantics come from the shared `lib/keepalive` module so
/// this chip, the Quota-page trigger, and the popover never drift.
function KeepAliveChip({ endpointId }: { endpointId: string }) {
  const { t } = useTranslation();
  const q = useQuery({
    queryKey: qk.keepaliveStatus(endpointId),
    queryFn: () => quotaKeepaliveStatus(endpointId),
    // Poll only while the chip is VISIBLE: a disabled/unconfigured endpoint
    // renders nothing, and a hidden chip polling every 10s is pure waste.
    refetchInterval: (query) => {
      const phase = query.state.data?.phase ?? "disabled";
      return keepaliveMeta(phase).visible ? 10_000 : false;
    },
    // Don't refetch in the background; `useResumeInvalidate` (below) is the
    // sole resume authority here — `refetchOnWindowFocus` is unreliable in
    // WebView2 when the window is restored from tray.
    refetchIntervalInBackground: false,
  });
  useResumeInvalidate([qk.keepaliveStatus(endpointId)]);
  const phase = q.data?.phase ?? "disabled";
  const meta = keepaliveMeta(phase);
  if (!meta.visible) return null;
  const label = t(meta.labelKey);
  return (
    <Tip content={`${t("keepalive.title")} · ${label}`}>
      <span
        className={`inline-flex shrink-0 items-center gap-1 font-mono text-xs ${meta.color}${
          meta.pulse ? " animate-pulse" : ""
        }`}
      >
        <HeartPulse data-icon size={12} />
        {label}
      </span>
    </Tip>
  );
}

/// Quiet quota preview on the provider card: one row for the tracked window
/// (the keep-alive target, or the 5h-name heuristic) with a muted fine bar.
/// Renders NOTHING while the query is unverified — no bar, no divider, no
/// muted hint (the "no quota content until successful" rule). The keep-alive
/// chip lives separately in the footer, so it is not rendered here.
function QuotaSection({ endpoint }: { endpoint: EndpointInfo }) {
  const quotaCache = useUI((s) => s.quotaCache);
  const setQuotaCache = useUI((s) => s.setQuotaCache);
  const cached = quotaCache[endpoint.id] ?? null;

  const q = useQuery<EndpointQuota>({
    queryKey: qk.endpointQuota(endpoint.id),
    queryFn: () => endpointFetchQuota(endpoint.id),
    staleTime: 60_000,
    // Passive preview — the app-level QuotaAutoDriver (RootShell) owns the
    // auto interval and advances this shared cache while the card is visible.
    // No refetchOnWindowFocus: TanStack's focus manager doesn't fire reliably
    // in WebView2 when the window is restored from tray. The reliable resume
    // trigger lives in `useResumeInvalidate` below (visibilitychange).
  });
  useResumeInvalidate([qk.endpointQuota(endpoint.id)]);
  const data = q.data;
  useEffect(() => {
    if (data?.ok && data.items.length > 0) setQuotaCache(endpoint.id, data);
  }, [data, endpoint.id, setQuotaCache]);

  const shown = (data?.ok && data.items.length > 0 ? data : cached) ?? null;
  const items = shown?.items ?? [];

  // Tracked window: the persisted keep-alive target, else the 5h-name
  // heuristic (same default as the Quota page's picker). Hooks must run
  // before any early return (items can go 0 ↔ N across fetches).
  const refreshQ = useQuery({ queryKey: qk.quotaRefresh(), queryFn: quotaRefreshGet });
  const cfg: RefreshEndpointConfig =
    refreshQ.data?.endpoints[endpoint.id] ?? DEFAULT_CFG;
  // Same gate as the Quota page: the preview only renders when a plan is
  // selected AND a verifying fetch has returned data. Until then the whole
  // section is hidden (no placeholder, no divider) — the user reaches setup
  // via the footer's `quota` button.
  const plan = resolvePlan(cfg, endpoint);
  const canArm = isPlanActive(plan) && (cfg.provisioned ?? false);
  const defaultTarget = useMemo(() => {
    const hit = items.find((i) => i.name === "5h-token" || i.name.endsWith("/5h"));
    return hit?.name ?? items[0]?.name ?? null;
  }, [items]);

  // Preview windows come from the Quota page's settings dialog (multi-toggle).
  // Explicit `preview_windows` (even empty = show nothing) wins; unset falls
  // back to the 5h-heuristic single window for legacy endpoints.
  const visible = useMemo(() => {
    const names = cfg.preview_windows != null ? cfg.preview_windows : defaultTarget ? [defaultTarget] : [];
    return names
      .map((n) => items.find((it) => it.name === n))
      .filter((it): it is (typeof items)[number] => it != null);
  }, [cfg.preview_windows, defaultTarget, items]);

  if (!canArm || visible.length === 0) return null;

  // Full-width divider (negates the `p-3` card body horizontally) so it
  // matches the footer's edge-to-edge `border-t` instead of sitting inset.
  return (
    <div className="-mx-3 mt-1 border-t border-border px-3 pt-2 space-y-1.5">
      {visible.map((it) => (
        <QuotaItemRow
          key={it.name}
          name={it.name}
          pct={it.pct}
          detail={
            it.is_balance && it.remaining != null
              ? fmtMoney(it.remaining, it.unit)
              : undefined
          }
          resetsIn={it.resets_in}
          showReset
          size="sm"
          quiet
          isBalance={it.is_balance}
        />
      ))}
    </div>
  );
}
