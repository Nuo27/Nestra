import { useNavigate } from "@tanstack/react-router";
import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Page } from "../components/layout/Page";
import { PageHeader, BackLink } from "../components/layout/PageHeader";
import { Card } from "../components/controls/Card";
import { Skeleton } from "../components/ui/skeleton";
import { SectionHeader } from "../components/layout/SectionHeader";
import { RoutingPolicyEditor } from "../components/orchestration/RoutingPolicyEditor";
import { RoutedGate } from "../components/orchestration/RoutedGate";
import { SteadyRouteCard } from "../components/orchestration/SteadyRouteCard";
import { ModeSwitch } from "../components/orchestration/ModeSwitch";
import { Workflow } from "lucide-react";
import { agentList } from "../ipc";
import { qk } from "../lib/queries";
import { ErrorBanner } from "../components/feedback/ErrorBanner";

/// /agents/$id/routing — the (agent, role) → endpoint-chain policy editor on
/// its own page. Routed-mode surface: while the agent is Direct a hint +
/// ModeSwitch gate the page (see RoutedGate).
export function AgentRoutingPage({ id }: { id: string }) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const agentsQ = useQuery({ queryKey: qk.agents(), queryFn: agentList });
  const agent = (agentsQ.data ?? []).find((a) => a.id === id);

  if (agentsQ.isLoading) return <Skeleton className="h-10 w-64" />;
  if (agentsQ.isError) {
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
        title={`${agent.display_name} · ${t("agentRouting.titleSuffix")}`}
        info={t("agentRouting.help")}
        back={
          <BackLink
            onClick={() => navigate({ to: "/agents/$id", params: { id: agent.id } })}
          >
            {t("nav.agents")}
          </BackLink>
        }
        action={<ModeSwitch agentId={agent.id} supportsGateway={agent.capability.supports_gateway} />}
      />

      <RoutedGate
        agentId={agent.id}
        supportsGateway={agent.capability.supports_gateway}
        title={t("agentRouting.gateTitle")}
        hint={t("agentRouting.gateHint")}
      >
        <div className="space-y-3">
          <SteadyRouteCard agentId={agent.id} />
          <Card padding="none">
            <SectionHeader
              icon={<Workflow data-icon size={14} />}
              title={t("agentRouting.policyTitle")}
              hint={t("agentRouting.policyHint")}
            />
            <div className="p-3">
              <RoutingPolicyEditor agentId={agent.id} />
            </div>
          </Card>
        </div>
      </RoutedGate>
    </Page>
  );
}
