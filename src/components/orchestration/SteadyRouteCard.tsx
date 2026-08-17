import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Route as RouteIcon } from "lucide-react";
import { Card } from "../controls/Card";
import { SectionHeader } from "../layout/SectionHeader";
import { Skeleton } from "../ui/skeleton";
import { RoleKey } from "./RoleKey";
import { RouteBadge } from "./RouteBadge";
import { endpointList, type EndpointInfo } from "../../ipc";
import { routingPolicyList, resolvePreview } from "../../ipc/orchestration";
import { qk } from "../../lib/queries";
import { fmtNum } from "../../lib/format";

/// "What does my configuration actually do right now": a router dry-run
/// (resolve preview) for the main role plus one row per CONFIGURED `tier:*`
/// policy, each showing provider → model → context window → route reason.
/// Answers the "I changed the preferred providers but nothing tells me what
/// took effect" gap directly above the policy editor. Read-only — the
/// dry-run never sends traffic.
export function SteadyRouteCard({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const endpointsQ = useQuery({ queryKey: qk.endpoints(), queryFn: endpointList });
  const policiesQ = useQuery({
    queryKey: qk.routingPolicies(agentId),
    queryFn: () => routingPolicyList(agentId),
  });
  const tierRoles = (policiesQ.data ?? [])
    .map((p) => p.role)
    .filter((r) => r.startsWith("tier:"))
    .sort();
  // Policy edits bump updated_at; keying the dry-run rows on the max stamp
  // makes a save refetch them immediately (no cross-component invalidation).
  const stamp = Math.max(0, ...(policiesQ.data ?? []).map((p) => p.updated_at));

  return (
    <Card padding="none">
      <SectionHeader
        icon={<RouteIcon data-icon size={14} />}
        title={t("agentRouting.steadyTitle")}
        hint={t("agentRouting.steadyHint")}
      />
      <div className="flex flex-col divide-y divide-border p-3">
        <SteadyRouteRow
          agentId={agentId}
          role="main"
          stamp={stamp}
          endpoints={endpointsQ.data ?? []}
        />
        {tierRoles.map((role) => (
          <SteadyRouteRow
            key={role}
            agentId={agentId}
            role={role}
            stamp={stamp}
            endpoints={endpointsQ.data ?? []}
          />
        ))}
      </div>
    </Card>
  );
}

/// One dry-run row. `role` is a policy-role key ("main" | "tier:haiku" | …).
function SteadyRouteRow({
  agentId,
  role,
  stamp,
  endpoints,
}: {
  agentId: string;
  role: string;
  stamp: number;
  endpoints: EndpointInfo[];
}) {
  const { t } = useTranslation();
  const q = useQuery({
    queryKey: ["orchestration", "resolve-preview", agentId, role, stamp],
    queryFn: () => resolvePreview({ agentId, role }),
  });

  if (q.isLoading) {
    return (
      <div className="py-2">
        <Skeleton className="h-5 w-full" />
      </div>
    );
  }
  if (q.isError || !q.data) {
    return (
      <div className="flex items-center gap-2 py-2 font-mono text-2xs text-warning">
        <RoleKey roleKey={role} />
        <span>{t("common.loadFailed")}</span>
      </div>
    );
  }
  const p = q.data;
  if (p.reason === "no_eligible" || !p.endpoint_id) {
    return (
      <div className="flex flex-wrap items-center gap-2 py-2">
        <RoleKey roleKey={role} />
        <span className="text-xs text-warning">{t("agentRouting.steadyNoRoute")}</span>
      </div>
    );
  }
  const provider =
    endpoints.find((e) => e.id === p.endpoint_id)?.display_name ?? p.endpoint_id;

  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1 py-2">
      <RoleKey roleKey={role} />
      <span aria-hidden className="text-subtle">
        →
      </span>
      <span className="min-w-0 truncate text-xs text-fg">{provider}</span>
      <span className="min-w-0 truncate font-mono text-2xs text-fg">{p.model}</span>
      <span className="font-mono text-2xs text-subtle">
        {p.context_window != null
          ? t("agentRouting.steadyContext", { n: fmtNum(p.context_window) })
          : t("agentRouting.steadyNoWindow")}
      </span>
      <RouteBadge reason={p.reason} />
    </div>
  );
}
