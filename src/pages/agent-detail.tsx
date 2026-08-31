import { useTranslation } from "react-i18next";
import { useMemo, useState } from "react";
import { Link } from "@tanstack/react-router";
import { useQuery } from "@tanstack/react-query";
import { Workflow, ListTree, Plug, ShieldCheck, Coins, ScrollText } from "lucide-react";
import { AgentPageFrame } from "../components/agents/AgentPageFrame";
import { Button } from "../components/controls/Button";
import { Card } from "../components/controls/Card";
import { SectionHeader } from "../components/layout/SectionHeader";
import { Disclosure } from "../components/controls/Disclosure";
import { Stat } from "../components/display/Stat";
import { Skeleton } from "../components/ui/skeleton";
import { Badge } from "../components/ui/badge";
import { EmptyOrchestration } from "../components/orchestration/EmptyOrchestration";
import { useAgentModeToggle } from "../components/orchestration/ModeSwitch";
import { ROLE_CHIP_ACTIVE, ROLE_CHIP_BASE } from "../components/orchestration/RoleChip";
import { RouteLineage } from "../components/orchestration/RouteLineage";
import { RoleKey } from "../components/orchestration/RoleKey";
import { SteadyRouteCard } from "../components/orchestration/SteadyRouteCard";
import { ProviderConfigPanel } from "../components/agents/ProviderConfigPanel";
import type { AgentInfo, EndpointInfo } from "../ipc";
import { endpointList } from "../ipc";
import {
  routeHistory,
  migrations,
  tasks,
  detectedRoles,
  routingPolicyList,
  usageSummary,
  type TaskSummary,
  type RouteMigrationRow,
} from "../ipc/orchestration";
import { qk } from "../lib/queries";

/// HTTP-status → badge tone. Extracted from a nested ternary
/// (no-nested-ternary rule).
function statusToneOf(status: number | null): "neutral" | "success" | "danger" {
  if (status === null || status === undefined) return "neutral";
  return status < 400 ? "success" : "danger";
}

/// Agent detail — the dual-mode cockpit. The ACTIVE mode renders as the
/// primary column (Direct binding editor / route overview + policy entry);
/// the inactive mode stays visible as a compact summary card with a
/// one-click switch, so the page never goes half-empty and switching is
/// never blind. Below (Routed only): the live task list and the 30-day
/// usage breakdown. Header/guards live in the shared AgentPageFrame.
export function AgentDetailPage({ id }: { id: string }) {
  return (
    <AgentPageFrame agentId={id} backTo="agents">
      {(agent) => <AgentCockpit agent={agent} />}
    </AgentPageFrame>
  );
}

function AgentCockpit({ agent }: { agent: AgentInfo }) {
  const supported = agent.capability.supports_gateway;
  const { routed } = useAgentModeToggle(agent.id, supported);
  const endpointsQ = useQuery({ queryKey: qk.endpoints(), queryFn: endpointList });
  const endpoints = endpointsQ.data ?? [];

  if (!supported) {
    // No gateway writer: Direct is the only mode.
    return <DirectCard agent={agent} endpoints={endpoints} />;
  }

  return (
    <div className="space-y-4">
      {/* Keyed wrapper re-mounts on mode flip so the keyed subtree fades
          back in via `animate-in` (DESIGN.md §11). The prior subtree
          unmounts instantly — no exit animation, to avoid ghosting during
          the fade-in of the new one. */}
      <div key={routed ? "routed" : "direct"} className="space-y-4 animate-in fade-in slide-in-from-bottom-1 duration-fast">
        {routed ? (
          <>
            <SteadyRouteCard agentId={agent.id} />
            <RoutingEntryCard agent={agent} />
            <UsageCard agentId={agent.id} />
            <TasksCard agentId={agent.id} />
          </>
        ) : (
          <DirectCard agent={agent} endpoints={endpoints} />
        )}
      </div>
    </div>
  );
}

/// The Direct-mode primary card: the provider binding editor in a proper
/// section.
function DirectCard({ agent, endpoints }: { agent: AgentInfo; endpoints: EndpointInfo[] }) {
  const { t } = useTranslation();
  return (
    <Card padding="none">
      <SectionHeader
        icon={<Plug data-icon size={14} />}
        title={t("agentDetail.directTitle")}
        hint={t("agentDetail.directHint")}
      />
      <div className="p-3">
        <ProviderConfigPanel agent={agent} endpoints={endpoints} />
      </div>
    </Card>
  );
}

/// Routed-mode policy hub: role-policy count + detected subagent roles + the
/// entries into the policy editor (and, for pi-cli, the review runtime).
function RoutingEntryCard({ agent }: { agent: AgentInfo }) {
  const { t } = useTranslation();
  const policiesQ = useQuery({
    queryKey: qk.routingPolicies(agent.id),
    queryFn: () => routingPolicyList(agent.id),
  });
  const rolesQ = useQuery({
    queryKey: qk.detectedRoles(agent.id),
    queryFn: () => detectedRoles(agent.id),
  });
  const roles = rolesQ.data ?? [];
  const star = (policiesQ.data ?? []).find((p) => p.role === "*");
  const starCount = star?.route_targets.length ?? 0;

  return (
    <Card padding="none">
      <SectionHeader
        icon={<Workflow data-icon size={14} />}
        title={t("agentDetail.routingCard")}
        hint={t("agentDetail.routingCardHint")}
      />
      <div className="flex flex-col divide-y divide-border">
        <div className="flex flex-wrap items-center gap-2 px-3 py-2">
          <span className="font-mono text-2xs text-muted tabular">
            {starCount > 0
              ? t("agentDetail.policySummary", {
                  roles: (policiesQ.data ?? []).length,
                  targets: starCount,
                })
              : t("agentDetail.policyEmpty")}
          </span>
          <span className="ms-auto" />
          {agent.id === "pi-cli" && (
            <Link
              to="/agents/$id/review"
              params={{ id: agent.id }}
              search={{ session: undefined }}
            >
              <Button size="sm" variant="ghost">
                <ShieldCheck data-icon size={14} />
              </Button>
            </Link>
          )}
          <Link to="/agents/$id/routing" params={{ id: agent.id }}>
            <Button size="sm" variant="secondary">
              {t("agentDetail.editPolicy")}
            </Button>
          </Link>
        </div>
        {roles.length > 0 && (
          <div className="flex flex-wrap items-center gap-1.5 px-3 py-2">
            <span className="font-mono text-2xs text-subtle">
              {t("agentDetail.detectedRoles")}
            </span>
            {roles.map((r) => (
              <Link
                key={r.role}
                to="/agents/$id/routing"
                params={{ id: agent.id }}
                className={ROLE_CHIP_BASE + ROLE_CHIP_ACTIVE}
                title={t("agentDetail.roleChipTip", { count: r.request_count, role: r.role })}
              >
                <RoleKey roleKey={r.role} />
                <span className="text-subtle tabular">×{r.request_count}</span>
              </Link>
            ))}
          </div>
        )}
      </div>
    </Card>
  );
}

/// This agent's live gateway tasks. A persistent card (not a self-hiding
/// collapsible): no traffic shows the honest empty state instead of a
/// vanishing section. The list scrolls internally past ~7 rows so a long
/// task history doesn't stretch the page.
function TasksCard({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  return (
    <Card padding="none">
      <SectionHeader
        icon={<ListTree data-icon size={14} />}
        title={t("agentDetail.tasks")}
        hint={t("agentDetail.tasksHint")}
      />
      <div className="max-h-72 overflow-y-auto scroll p-3">
        <AgentTasks agentId={agentId} />
      </div>
    </Card>
  );
}

function AgentTasks({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const q = useQuery({
    queryKey: ["orchestration", "tasks"],
    queryFn: () => tasks(50),
    refetchInterval: 5000,
  });
  if (q.isLoading) return <Skeleton className="h-8 w-full" />;
  // A failed poll must not render as "no gateway traffic" — that reads as a
  // routing verdict when it's an IPC failure.
  if (q.isError) {
    return (
      <EmptyOrchestration
        title={t("common.loadFailed")}
        hint={t("agentDetail.tasksLoadFailedHint")}
      />
    );
  }
  const rows = (q.data ?? []).filter((t) => t.agent_id === agentId);
  if (rows.length === 0) {
    return (
      <EmptyOrchestration
        title={t("agentDetail.noGatewayTraffic")}
        hint={t("agentDetail.noGatewayTrafficHint")}
      />
    );
  }
  return (
    <div className="space-y-1.5">
      {rows.map((t) => (
        <TaskRow key={t.task_id} summary={t} />
      ))}
    </div>
  );
}

function TaskRow({ summary }: { summary: TaskSummary }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const historyQ = useQuery({
    queryKey: ["orchestration", "task-history", summary.task_id],
    queryFn: () => routeHistory(summary.task_id),
    enabled: open,
  });
  const migrationsQ = useQuery({
    queryKey: ["orchestration", "task-migrations", summary.task_id],
    queryFn: () => migrations(summary.task_id),
    enabled: open,
  });

  const status = summary.latest_status;
  const statusTone = statusToneOf(status);

  // Disclosure owns the toggle; the side link jumps to the gateway log
  // filtered on this task (outside the toggle button — no nested controls).
  return (
    <div className="flex items-center border border-border bg-inset">
      <Disclosure
        className="min-w-0 flex-1"
        onOpenChange={setOpen}
        header={
          <span className="flex items-center gap-2 font-mono text-xs">
            <span className="min-w-0 flex-1 truncate text-fg">
              {summary.task_id.slice(0, 8)}
              {summary.logical_session ? (
                <span className="ml-1.5 font-mono text-2xs text-subtle">
                  {summary.logical_session.slice(0, 12)}
                </span>
              ) : null}
            </span>
            <span className="font-mono text-2xs text-subtle tabular">
              {t("orchestration.reqCount", { n: summary.request_count })}
            </span>
            {summary.generation_broken && (
              <Badge tone="danger" variant="soft" className="font-mono text-2xs">
                {t("orchestration.genBroken")}
              </Badge>
            )}
            {status != null && (
              <Badge tone={statusTone} variant="soft" className="font-mono text-2xs">
                http {status}
              </Badge>
            )}
          </span>
        }
      >
        <div className="border-t border-border px-3 py-2">
          {historyQ.isLoading ? (
            <Skeleton className="h-8 w-full" />
          ) : (
            <RouteLineage records={historyQ.data ?? []} />
          )}
          {(migrationsQ.data ?? []).length > 0 && (
            <div className="mt-2 border-t border-border pt-2">
              <div className="mb-1 font-mono text-2xs text-subtle">
                {t("orchestration.migrations", { count: migrationsQ.data?.length })}
              </div>
              <ul className="space-y-1">
                {(migrationsQ.data ?? []).map((m) => (
                  <MigrationRow key={m.id} m={m} />
                ))}
              </ul>
            </div>
          )}
        </div>
      </Disclosure>
      <Link
        to="/gateway/logs"
        search={{ task: summary.task_id }}
        aria-label={t("orchestration.viewLogs")}
        title={t("orchestration.viewLogs")}
        className="shrink-0 px-2 text-subtle transition-[color] duration-fast hover:text-accent"
      >
        <ScrollText data-icon size={13} />
      </Link>
    </div>
  );
}

function MigrationRow({ m }: { m: RouteMigrationRow }) {
  return (
    <li className="flex items-center gap-2 font-mono text-2xs text-muted">
      <span className="text-accent">{m.from_endpoint_id ?? "—"}</span>
      <span aria-hidden>→</span>
      <span className="text-fg">{m.to_endpoint_id ?? "—"}</span>
      <span className="text-subtle">{m.reason}</span>
      <span className="ml-auto text-subtle tabular">
        {new Date(m.at_ms).toLocaleTimeString()}
      </span>
    </li>
  );
}

/// Gateway-observed usage for this agent, last 30 days. Totals as a stat
/// tile row, then the per-model breakdown. The backend unions folded
/// lifetime history with the live window; spend is priced at read time
/// (`cost_usd: null` rows carry tokens but no dollars — flagged, never
/// silently free). Persistent card; no traffic shows the empty state.
function UsageCard({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const q = useQuery({
    // The 30-day window is part of the key: a different window elsewhere
    // would otherwise share this cache entry while holding other data.
    queryKey: ["orchestration", "usage", agentId, 30],
    queryFn: () => usageSummary(agentId, 30),
    refetchInterval: 15000,
  });
  const rows = q.data ?? [];

  const { totals, byModel } = useMemo(() => {
    const zero = {
      requests: 0,
      input: 0,
      output: 0,
      cacheRead: 0,
      cacheWrite: 0,
      cost: 0,
      unknown: false,
    };
    const totals = rows.reduce(
      (a, r) => ({
        requests: a.requests + r.requests,
        input: a.input + r.usage_input,
        output: a.output + r.usage_output,
        cacheRead: a.cacheRead + r.cache_read,
        cacheWrite: a.cacheWrite + r.cache_creation,
        cost: a.cost + (r.cost_usd ?? 0),
        unknown: a.unknown || r.cost_usd == null,
      }),
      zero,
    );
    const byModel = new Map<
      string,
      typeof totals & { model: string; endpoint: string }
    >();
    for (const r of rows) {
      const key = `${r.endpoint_id || "—"} / ${r.model_id || "—"}`;
      const cur =
        byModel.get(key) ??
        {
          ...zero,
          model: r.model_id || "—",
          endpoint: r.endpoint_id || "—",
        };
      cur.requests += r.requests;
      cur.input += r.usage_input;
      cur.output += r.usage_output;
      cur.cacheRead += r.cache_read;
      cur.cacheWrite += r.cache_creation;
      cur.cost += r.cost_usd ?? 0;
      cur.unknown = cur.unknown || r.cost_usd == null;
      byModel.set(key, cur);
    }
    return { totals, byModel: [...byModel.values()] };
  }, [rows]);

  const fmt = (n: number) => n.toLocaleString();
  const fmtUsd = (n: number) =>
    n < 0.01 && n > 0 ? `$${n.toFixed(4)}` : `$${n.toFixed(2)}`;

  return (
    <Card padding="none">
      <SectionHeader
        icon={<Coins data-icon size={14} />}
        title={t("agentDetail.usage")}
        hint={t("agentDetail.usageHint")}
      />
      <div className="p-3">
        {q.isLoading ? (
          <Skeleton className="h-8 w-full" />
        ) : q.isError ? (
          // A failed poll must not render as the "no traffic" empty state.
          <EmptyOrchestration
            title={t("common.loadFailed")}
            hint={t("agentDetail.usageLoadFailedHint")}
          />
        ) : rows.length === 0 ? (
          <EmptyOrchestration
            title={t("agentDetail.noGatewayTraffic")}
            hint={t("agentDetail.usageEmptyHint")}
          />
        ) : (
          <div className="space-y-3">
            <div className="grid grid-cols-2 gap-2 sm:grid-cols-5">
              <Stat label={t("agentDetail.usageRequests")} value={fmt(totals.requests)} />
              <Stat label={t("agentDetail.usageIn")} value={fmt(totals.input)} />
              <Stat label={t("agentDetail.usageOut")} value={fmt(totals.output)} />
              <Stat label={t("cache.read")} value={fmt(totals.cacheRead)} />
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
            <ul className="space-y-1 border-t border-border pt-2">
              {byModel.map((m) => (
                <li
                  key={`${m.endpoint}/${m.model}`}
                  className="flex flex-wrap items-center gap-x-3 gap-y-0.5 font-mono text-2xs text-muted tabular"
                >
                  <span className="min-w-0 flex-1 truncate">
                    <span className="text-fg">{m.model}</span>
                    <span className="text-subtle"> @ {m.endpoint}</span>
                  </span>
                  <span>
                    {fmt(m.requests)} {t("agentDetail.usageRequests")}
                  </span>
                  <span>
                    {t("agentDetail.usageIn")} {fmt(m.input)}
                  </span>
                  <span>
                    {t("agentDetail.usageOut")} {fmt(m.output)}
                  </span>
                  <span>
                    {t("agentDetail.usageSpend")}{" "}
                    {m.cost > 0 || !m.unknown ? fmtUsd(m.cost) : "—"}
                  </span>
                </li>
              ))}
            </ul>
          </div>
        )}
      </div>
    </Card>
  );
}

