import { useTranslation } from "react-i18next";
import { useState, type ReactNode } from "react";
import { Link } from "@tanstack/react-router";
import { useQuery } from "@tanstack/react-query";
import { Workflow, ListTree, ArrowRight, Users } from "lucide-react";
import { Page } from "../components/layout/Page";
import { PageHeader, BackLink } from "../components/layout/PageHeader";
import { Card } from "../components/controls/Card";
import { Disclosure } from "../components/controls/Disclosure";
import { Skeleton } from "../components/ui/skeleton";
import { Badge } from "../components/ui/badge";
import { ErrorBanner } from "../components/feedback/ErrorBanner";
import { EmptyOrchestration } from "../components/orchestration/EmptyOrchestration";
import { ModeSwitch } from "../components/orchestration/ModeSwitch";
import { RouteLineage } from "../components/orchestration/RouteLineage";
import { RoleKey } from "../components/orchestration/RoleKey";
import {
  endpointList,
  agentList,
  type AgentInfo,
  type EndpointInfo,
} from "../ipc";
import { ProviderConfigPanel } from "../components/agents/ProviderConfigPanel";
import {
  routeHistory,
  migrations,
  tasks,
  agentGatewayEnabled,
  detectedRoles,
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

/// Agent detail — the merged Direct/Routed surface.
/// Direct: the classic provider-binding editor. Routed: entry card to the
/// routing policy sub-page, plus this agent's live tasks. The mode
/// switch is shared with the /agents card (same `setting_kv` flag), so
/// toggling here is reflected there instantly.
export function AgentDetailPage({ id }: { id: string }) {
  const { t } = useTranslation();
  const agentsQ = useQuery({ queryKey: qk.agents(), queryFn: agentList });
  const endpointsQ = useQuery({ queryKey: qk.endpoints(), queryFn: endpointList });
  const agent = (agentsQ.data ?? []).find((a) => a.id === id);
  const endpoints = endpointsQ.data ?? [];

  if (agentsQ.isLoading) return <Skeleton className="h-10 w-64" />;
  if (agentsQ.isError) {
    // A query failure must not masquerade as "agent not found" — show the
    // error with a retry instead of a misleading empty state.
    return (
      <Page>
        <PageHeader title={t("agents.title")} back={<BackLink to="/agents">{t("nav.agents")}</BackLink>} />
        <ErrorBanner onRetry={() => agentsQ.refetch()}>{t("agents.loadFailed")}</ErrorBanner>
      </Page>
    );
  }
  if (!agent) {
    return (
      <Page>
        <PageHeader title={t("agents.notFound")} back={<BackLink to="/agents">{t("nav.agents")}</BackLink>} />
      </Page>
    );
  }

  return (
    <Page width="wide">
      <PageHeader
        title={agent.display_name}
        info={agent.capability.supports_gateway ? t("agentDetail.helpGateway") : t("agentDetail.helpPlain")}
        back={<BackLink to="/agents">{t("nav.agents")}</BackLink>}
        action={<ModeSwitch agentId={agent.id} supportsGateway={agent.capability.supports_gateway} />}
      />

      <AgentDetailBody agent={agent} endpoints={endpoints} />
    </Page>
  );
}

function AgentDetailBody({
  agent,
  endpoints,
}: {
  agent: AgentInfo;
  endpoints: EndpointInfo[];
}) {
  const { t } = useTranslation();
  const routedQ = useQuery({
    queryKey: ["orchestration", "gateway-flag", agent.id],
    queryFn: () => agentGatewayEnabled(agent.id),
    enabled: agent.capability.supports_gateway,
  });
  const routed = routedQ.data ?? false;

  return routed ? (
    <div className="space-y-3">
      <div className="grid grid-cols-1 gap-3 lg:grid-cols-2">
        <EntryCard
          to="/agents/$id/routing"
          agentId={agent.id}
          icon={<Workflow data-icon size={14} />}
          title={t("agentDetail.policyTitle")}
          hint={t("agentDetail.policyHint")}
        />
      </div>

      <DetectedRolesCard agentId={agent.id} />

      <CollapsibleSection
        icon={<ListTree data-icon size={14} />}
        title={t("agentDetail.tasks")}
        hint={t("agentDetail.tasksHint")}
      >
        <AgentTasks agentId={agent.id} />
      </CollapsibleSection>
    </div>
  ) : (
    <ProviderConfigPanel agent={agent} endpoints={endpoints} />
  );
}

/// Collapsible card section: SectionHeader-style header that toggles its
/// body. Defaults to closed so the detail page stays compact.
function CollapsibleSection({
  icon,
  title,
  hint,
  children,
}: {
  icon: ReactNode;
  title: string;
  hint: string;
  children: ReactNode;
}) {
  return (
    <Card padding="none">
      <Disclosure
        defaultOpen={false}
        buttonClassName="px-3 py-2"
        header={
          <span className="flex min-w-0 flex-1 items-center gap-2">
            <span className="shrink-0 text-accent">{icon}</span>
            <span className="min-w-0 flex-1">
              <span className="block text-sm font-medium text-fg">{title}</span>
              <span className="prose mt-0.5 block text-2xs text-subtle">{hint}</span>
            </span>
          </span>
        }
      >
        <div className="border-t border-border p-3">{children}</div>
      </Disclosure>
    </Card>
  );
}

/// Subagent roles this agent has actually used (from `route_request`),
/// newest first. Chips link to the routing page for per-role policy editing.
/// Collapsed by default; hidden entirely when nothing was detected.
function DetectedRolesCard({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const q = useQuery({
    queryKey: qk.detectedRoles(agentId),
    queryFn: () => detectedRoles(agentId),
  });
  const roles = q.data ?? [];
  if (roles.length === 0) return null;
  return (
    <CollapsibleSection
      icon={<Users data-icon size={14} />}
      title={t("agentDetail.detectedRoles")}
      hint={t("agentDetail.detectedRolesHint")}
    >
      <div className="flex flex-wrap items-center gap-1.5">
        {roles.map((r) => (
          <Link
            key={r.role}
            to="/agents/$id/routing"
            params={{ id: agentId }}
            className="inline-flex items-center gap-1.5 rounded border border-border bg-inset px-1.5 py-0.5 font-mono text-2xs text-fg transition-colors duration-fast hover:border-accent/50 hover:bg-raised"
            title={t("agentDetail.roleChipTip", { count: r.request_count, role: r.role })}
          >
            <RoleKey roleKey={r.role} />
            <span className="text-subtle tabular">×{r.request_count}</span>
          </Link>
        ))}
      </div>
    </CollapsibleSection>
  );
}

/// Full-card link to a Routed-mode sub-page.
function EntryCard({
  icon,
  title,
  hint,
  to,
  agentId,
}: {
  icon: React.ReactNode;
  title: string;
  hint: string;
  to: "/agents/$id/routing";
  agentId: string;
}) {
  return (
    <Link
      to={to}
      params={{ id: agentId }}
      className="group flex items-center gap-3 rounded border border-border bg-inset/40 px-3 py-2 transition-[border-color,background-color] duration-fast hover:border-accent/50 hover:bg-inset"
    >
      <span className="text-accent">{icon}</span>
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm font-medium text-fg">{title}</span>
        <span className="mt-0.5 block truncate text-2xs text-subtle">{hint}</span>
      </span>
      <ArrowRight
        data-icon
        size={14}
        className="shrink-0 text-subtle transition-transform duration-fast group-hover:translate-x-0.5"
      />
    </Link>
  );
}

/// This agent's live tasks (aggregated from route_request), each expandable
/// to its full route lineage + migration events.
function AgentTasks({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const q = useQuery({
    queryKey: ["orchestration", "tasks"],
    queryFn: () => tasks(50),
    refetchInterval: 5000,
  });
  if (q.isLoading) return <Skeleton className="h-8 w-full" />;
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

  return (
    <div className="rounded border border-border bg-inset/60">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs"
      >
        <span
          aria-hidden
          className="text-subtle transition-transform"
          style={{ transform: open ? "rotate(90deg)" : undefined }}
        >
          ▸
        </span>
        <span className="min-w-0 flex-1 truncate font-mono text-fg">
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
      </button>
      {open && (
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
      )}
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
