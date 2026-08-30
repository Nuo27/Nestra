import { useMemo } from "react";
import { useMutation, useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Link, useNavigate } from "@tanstack/react-router";
import { agentList, endpointList, quotaKeepaliveStatus, type AgentInfo } from "../ipc";
import { extractError } from "../ipc/errors";
import {
  gatewayGetStatus,
  gatewayRestart,
  providerHealthReset,
  providerHealthSnapshot,
  type GatewayRuntimeState,
} from "../ipc/gateway";
import { agentGatewayEnabled, tasks, usageSummary } from "../ipc/orchestration";
import { invalidateGateway, qk } from "../lib/queries";
import { useUI } from "../stores/ui";
import { Page } from "../components/layout/Page";
import { PageHeader } from "../components/layout/PageHeader";
import { Card } from "../components/controls/Card";
import { Button } from "../components/controls/Button";
import { Badge } from "../components/ui/badge";
import { Skeleton } from "../components/ui/skeleton";
import { StatusDot } from "../components/feedback/StatusDot";
import { EmptyOrchestration } from "../components/orchestration/EmptyOrchestration";
import { AgentKindBadge } from "../components/agents/AgentKindBadge";
import { Stat } from "../components/display/Stat";

/// Gateway status poll cadence, shared by every observer of qk.gatewayStatus()
/// (AlertsCard + GatewaySummaryCard here, the Gateway page) so they never
/// disagree and never double-fetch at different rhythms.
function gatewayStatusInterval(q: { state: { data?: { state?: GatewayRuntimeState } } }) {
  if (q.state.data?.state === "running") return 5000;
  if (q.state.data?.state === "starting") return 1000;
  return 0;
}

/// Badge tone + leading glyph per gateway runtime state (no nested ternaries).
const GATEWAY_STATE_TONE: Partial<Record<GatewayRuntimeState, "success" | "danger" | "warning">> = {
  running: "success",
  error: "danger",
  starting: "warning",
};
const GATEWAY_STATE_GLYPH: Record<GatewayRuntimeState, string> = {
  running: "● ",
  error: "! ",
  starting: "○ ",
  stopped: "○ ",
};

/// Overview — the landing dashboard. Four signals at a glance, in priority
/// order: what needs attention (aggregated anomalies), gateway health, 30-day
/// usage, and the enabled agents' routing modes. Everything is a read-only
/// projection of existing surfaces; every query shares its key with the
/// owning page, and actions link into those pages.
export function OverviewPage() {
  const { t } = useTranslation();
  return (
    <Page width="wide">
      <PageHeader title={t("overview.title")} info={t("overview.help")} />
      <div className="space-y-4">
        <AlertsCard />
        <div className="grid gap-4 lg:grid-cols-2">
          <GatewaySummaryCard />
          <UsageSummaryCard />
        </div>
        <AgentsOverviewCard />
      </div>
    </Page>
  );
}

// ---- alerts -------------------------------------------------------------------

/// One anomaly row: a tone dot + mono text, so a stack reads as a feed.
function AlertRow({
  tone,
  children,
}: {
  tone: "danger" | "warning";
  children: string;
}) {
  return (
    <li className="flex items-center gap-2 font-mono text-xs text-fg">
      <StatusDot status={tone === "danger" ? "missing" : "outdated"} />
      <span className="min-w-0 truncate">{children}</span>
    </li>
  );
}

/// Aggregated anomaly feed: gateway-down-while-routed, open/probing breakers,
/// keep-alive error phases, enabled-but-missing agents. Nothing wrong → one
/// quiet green line.
function AlertsCard() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useUI((s) => s.pushToast);

  const statusQ = useQuery({
    queryKey: qk.gatewayStatus(),
    queryFn: gatewayGetStatus,
    refetchInterval: gatewayStatusInterval,
  });
  const healthQ = useQuery({
    queryKey: qk.providerHealth(),
    queryFn: providerHealthSnapshot,
    refetchInterval: 10_000,
  });
  const agentsQ = useQuery({ queryKey: qk.agents(), queryFn: agentList });
  const endpointsQ = useQuery({ queryKey: qk.endpoints(), queryFn: endpointList });
  const endpoints = endpointsQ.data ?? [];

  // Keep-alive phases: one query per endpoint via useQueries (the count varies
  // with the endpoint list — a .map of useQuery would break the hooks rule).
  // Same keys as the Providers card chips; fetch-once here (no interval) —
  // the Providers page owns live polling.
  const kaQueries = useQueries({
    queries: endpoints.map((e) => ({
      queryKey: qk.keepaliveStatus(e.id),
      queryFn: () => quotaKeepaliveStatus(e.id),
    })),
  });

  const resetMut = useMutation({
    mutationFn: providerHealthReset,
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: qk.providerHealth() });
      toast(t("providers.healthResetToast"), "success");
    },
    onError: (e: unknown) => toast(extractError(e) ?? String(e), "error"),
  });

  const breakerRows = useMemo(
    () => (healthQ.data ?? []).filter((s) => s.state !== "closed"),
    [healthQ.data],
  );

  const alerts = useMemo(() => {
    const rows: { key: string; tone: "danger" | "warning"; text: string }[] = [];
    const st = statusQ.data;
    if (st && st.agents_enabled.length > 0 && st.state !== "running") {
      rows.push({
        key: "gw-down",
        tone: "danger",
        text: t("overview.alertGatewayDown", { state: t(`gateway.${st.state}`) }),
      });
    }
    const name = (id: string) => endpoints.find((e) => e.id === id)?.display_name ?? id;
    for (const s of breakerRows) {
      rows.push({
        key: `breaker-${s.endpoint_id}-${s.model}`,
        tone: s.state === "open" ? "danger" : "warning",
        text:
          s.state === "open"
            ? t("overview.alertBreakerOpen", {
                endpoint: name(s.endpoint_id),
                model: s.model || "—",
                secs: Math.ceil((s.recovery_in_ms ?? 0) / 1000),
              })
            : t("overview.alertBreakerHalfOpen", {
                endpoint: name(s.endpoint_id),
                model: s.model || "—",
              }),
      });
    }
    endpoints.forEach((e, i) => {
      const phase = kaQueries[i].data?.phase;
      if (phase === "error" || phase === "resetting" || phase === "retrying") {
        rows.push({
          key: `ka-${e.id}`,
          tone: phase === "error" ? "danger" : "warning",
          text: t("overview.alertKeepalive", {
            endpoint: e.display_name,
            phase: t(`keepalive.${phase}`),
          }),
        });
      }
    });
    for (const a of agentsQ.data ?? []) {
      if (a.enabled && (a.status === "missing" || a.status === "manual_missing")) {
        rows.push({
          key: `agent-missing-${a.id}`,
          tone: "warning",
          text: t("overview.alertAgentMissing", { name: a.display_name }),
        });
      }
    }
    return rows;
  }, [statusQ.data, breakerRows, agentsQ.data, endpoints, kaQueries, t]);

  const loading =
    statusQ.isLoading || healthQ.isLoading || agentsQ.isLoading || endpointsQ.isLoading;
  // A failed query must NOT read as "all systems nominal" — surface the same
  // load-failed state the Gateway/Usage cards use.
  const failed =
    statusQ.isError || healthQ.isError || agentsQ.isError || endpointsQ.isError;

  return (
    <Card
      title={t("overview.alertsCard")}
      description={t("overview.alertsHint")}
      padding="sm"
      action={
        breakerRows.length > 0 ? (
          <Button size="sm" variant="ghost" loading={resetMut.isPending} onClick={() => resetMut.mutate()}>
            {t("providers.resetHealth")}
          </Button>
        ) : undefined
      }
    >
      {loading ? (
        <Skeleton className="h-8 w-full" />
      ) : failed ? (
        <EmptyOrchestration title={t("common.loadFailed")} hint={t("overview.loadFailedHint")} />
      ) : alerts.length === 0 ? (
        <div className="flex items-center gap-2 font-mono text-xs text-subtle">
          <StatusDot status="ok" />
          {t("overview.alertsClear")}
        </div>
      ) : (
        <ul className="space-y-1.5">
          {alerts.map((a) => (
            <AlertRow key={a.key} tone={a.tone}>
              {a.text}
            </AlertRow>
          ))}
        </ul>
      )}
    </Card>
  );
}

// ---- gateway summary ------------------------------------------------------------

function GatewaySummaryCard() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useUI((s) => s.pushToast);
  const statusQ = useQuery({
    queryKey: qk.gatewayStatus(),
    queryFn: gatewayGetStatus,
    refetchInterval: gatewayStatusInterval,
  });
  const restartMut = useMutation({
    mutationFn: gatewayRestart,
    onSuccess: () => {
      invalidateGateway(qc);
      toast(t("gateway.restartedToast"), "success");
    },
    onError: (e: unknown) =>
      toast(t("gateway.restartFailed", { err: extractError(e) ?? String(e) }), "error"),
  });

  const st = statusQ.data;
  const state = st?.state ?? "stopped";
  const tone = GATEWAY_STATE_TONE[state] ?? "neutral";
  const glyph = GATEWAY_STATE_GLYPH[state];

  return (
    <Card
      title={t("overview.gatewayCard")}
      description={t("overview.gatewayHint")}
      padding="sm"
      action={
        <Button
          size="sm"
          variant="ghost"
          loading={restartMut.isPending}
          disabled={!st?.enabled}
          onClick={() => restartMut.mutate()}
        >
          {t("gateway.restart")}
        </Button>
      }
    >
      {statusQ.isLoading ? (
        <Skeleton className="h-16 w-full" />
      ) : statusQ.isError ? (
        <EmptyOrchestration title={t("common.loadFailed")} hint={t("overview.loadFailedHint")} />
      ) : (
        <div className="space-y-3">
          <div className="flex flex-wrap items-center gap-2">
            <Badge
              tone={tone}
              variant="soft"
              className={`font-mono text-2xs ${state === "starting" ? "animate-pulse" : ""}`}
            >
              {glyph}
              {t(`gateway.${state}`)}
            </Badge>
            {st?.bound_base_url && (
              <span className="truncate font-mono text-2xs text-subtle">{st.bound_base_url}</span>
            )}
          </div>
          <div className="grid grid-cols-3 gap-2">
            <Stat
              label={t("overview.routingAgents")}
              value={String(st?.agents_enabled.length ?? 0)}
              tone={
                st && st.agents_enabled.length > 0 && state !== "running" ? "danger" : "default"
              }
            />
            <Stat label={t("gateway.totalRequests")} value={String(st?.stats.total_requests ?? 0)} />
            <Stat label={t("gateway.activeTasks")} value={String(st?.stats.active_tasks ?? 0)} />
          </div>
        </div>
      )}
    </Card>
  );
}

// ---- usage summary ---------------------------------------------------------------

/// Global 30-day usage totals (every agent) + top endpoints by requests.
/// Same `orch_usage_summary` call as the per-agent card, with `agentId`
/// omitted — the backend already supports the global fold.
function UsageSummaryCard() {
  const { t } = useTranslation();
  const q = useQuery({
    // SAME key + fn as the Providers page's per-endpoint usage fold: the two
    // surfaces share one cache entry (a bare "usage-overview" key would fetch
    // and store the same data twice).
    queryKey: ["orchestration", "usage-by-endpoint", 30],
    queryFn: () => usageSummary(undefined, 30),
    refetchInterval: 15000,
  });
  const rows = q.data ?? [];
  const { totals, byEndpoint } = useMemo(() => {
    const zero = { requests: 0, input: 0, output: 0, cost: 0, unknown: false };
    const totals = rows.reduce(
      (a, r) => ({
        requests: a.requests + r.requests,
        input: a.input + r.usage_input,
        output: a.output + r.usage_output,
        cost: a.cost + (r.cost_usd ?? 0),
        unknown: a.unknown || r.cost_usd == null,
      }),
      zero,
    );
    const byEndpoint = new Map<string, typeof zero & { endpoint: string }>();
    for (const r of rows) {
      const cur = byEndpoint.get(r.endpoint_id) ?? { ...zero, endpoint: r.endpoint_id };
      cur.requests += r.requests;
      cur.input += r.usage_input;
      cur.output += r.usage_output;
      cur.cost += r.cost_usd ?? 0;
      cur.unknown = cur.unknown || r.cost_usd == null;
      byEndpoint.set(r.endpoint_id, cur);
    }
    const list = [...byEndpoint.values()].sort((a, b) => b.requests - a.requests).slice(0, 3);
    return { totals, byEndpoint: list };
  }, [rows]);

  const fmt = (n: number) => n.toLocaleString();
  const fmtUsd = (n: number) => (n < 0.01 && n > 0 ? `$${n.toFixed(4)}` : `$${n.toFixed(2)}`);

  return (
    <Card title={t("overview.usageCard")} description={t("overview.usageHint")} padding="sm">
      {q.isLoading ? (
        <Skeleton className="h-16 w-full" />
      ) : q.isError ? (
        <EmptyOrchestration title={t("common.loadFailed")} hint={t("overview.loadFailedHint")} />
      ) : rows.length === 0 ? (
        <EmptyOrchestration
          title={t("agentDetail.noGatewayTraffic")}
          hint={t("overview.usageEmptyHint")}
        />
      ) : (
        <div className="space-y-3">
          <div className="grid grid-cols-4 gap-2">
            <Stat label={t("agentDetail.usageRequests")} value={fmt(totals.requests)} />
            <Stat label={t("agentDetail.usageIn")} value={fmt(totals.input)} />
            <Stat label={t("agentDetail.usageOut")} value={fmt(totals.output)} />
            <Stat
              label={t("agentDetail.usageSpend")}
              value={
                <>
                  {fmtUsd(totals.cost)}
                  {totals.unknown && (
                    <span className="ml-1 text-warning" title={t("agentDetail.usageSpendUnknown")}>
                      ~+
                    </span>
                  )}
                </>
              }
            />
          </div>
          {byEndpoint.length > 0 && (
            <ul className="space-y-1 border-t border-border pt-2">
              {byEndpoint.map((m) => (
                <li
                  key={m.endpoint}
                  className="flex flex-wrap items-center gap-x-3 font-mono text-2xs text-muted tabular"
                >
                  <span className="min-w-0 flex-1 truncate text-fg">{m.endpoint}</span>
                  <span>
                    {fmt(m.requests)} {t("agentDetail.usageRequests")}
                  </span>
                  <span>
                    {t("agentDetail.usageSpend")} {m.cost > 0 || !m.unknown ? fmtUsd(m.cost) : "—"}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </Card>
  );
}

// ---- agents overview ---------------------------------------------------------------

/// Enabled agents, one row each: mode badge + the mode-specific summary
/// (Direct: bound provider/model · Routed: traffic health). The whole row
/// links into the agent's detail page.
function AgentsOverviewCard() {
  const { t } = useTranslation();
  const agentsQ = useQuery({ queryKey: qk.agents(), queryFn: agentList });
  const rows = (agentsQ.data ?? []).filter(
    (a) => a.enabled && (a.status === "ok" || a.status === "manual_ok"),
  );
  return (
    <Card
      title={t("overview.agentsCard")}
      description={t("overview.agentsHint")}
      padding="sm"
      action={
        <Link to="/agents">
          <Button size="sm" variant="ghost">
            {t("nav.agents")} →
          </Button>
        </Link>
      }
    >
      {agentsQ.isLoading ? (
        <Skeleton className="h-16 w-full" />
      ) : rows.length === 0 ? (
        <EmptyOrchestration title={t("agents.none.title")} hint={t("agents.none.hint")} />
      ) : (
        <ul className="divide-y divide-border">
          {rows.map((a) => (
            <AgentModeRow key={a.id} agent={a} />
          ))}
        </ul>
      )}
    </Card>
  );
}

function AgentModeRow({ agent }: { agent: AgentInfo }) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const routedQ = useQuery({
    queryKey: ["orchestration", "gateway-flag", agent.id],
    queryFn: () => agentGatewayEnabled(agent.id),
    enabled: agent.capability.supports_gateway,
  });
  const routed = routedQ.data ?? false;

  return (
    <li>
      <button
        type="button"
        onClick={() => navigate({ to: "/agents/$id", params: { id: agent.id } })}
        className="flex w-full items-center gap-3 py-2 text-left transition-colors duration-fast hover:bg-raised focus-visible:shadow-focus"
      >
        <StatusDot status="ok" />
        <span className="text-sm text-fg">{agent.display_name}</span>
        <AgentKindBadge id={agent.id} />
        <Badge tone={routed ? "accent" : "neutral"} variant="soft" className="font-mono text-2xs">
          {routed ? t("orchestration.modeRouted") : t("orchestration.modeDirect")}
        </Badge>
        <span className="ml-auto min-w-0 max-w-[50%] truncate font-mono text-2xs text-subtle tabular">
          {routed ? <RoutedLine agentId={agent.id} /> : <DirectLine agent={agent} />}
        </span>
      </button>
    </li>
  );
}

function DirectLine({ agent }: { agent: AgentInfo }) {
  const { t } = useTranslation();
  const endpointsQ = useQuery({ queryKey: qk.endpoints(), queryFn: endpointList });
  const active = agent.providers.find((p) => p.active) ?? agent.providers[0];
  const ep = (endpointsQ.data ?? []).find((e) => e.id === agent.active_provider_id);
  const model = ep?.models?.default;
  return (
    <span className="truncate">
      {active ? `${active.display_name}${model ? ` / ${String(model)}` : ""}` : t("agents.noProviderBound")}
    </span>
  );
}

function RoutedLine({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  // SAME key + fn as the Agents page's RoutedTaskLine: shares the cache.
  const tasksQ = useQuery({
    queryKey: ["orchestration", "tasks"],
    queryFn: () => tasks(50),
    refetchInterval: 5000,
  });
  const mine = (tasksQ.data ?? []).filter((x) => x.agent_id === agentId);
  if (mine.length === 0) return <span>{t("agents.noTrafficYet")}</span>;
  const total = mine.reduce((a, x) => a + x.request_count, 0);
  const broken = mine.some((x) => x.generation_broken);
  return (
    <span className="truncate">
      {t("orchestration.reqCount", { n: total })}
      {broken && <span className="text-danger"> · {t("orchestration.genBroken")}</span>}
    </span>
  );
}
