import { type ReactNode } from "react";
import { useNavigate } from "@tanstack/react-router";
import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { agentList, endpointList, type AgentInfo } from "../../ipc";
import { routingPolicyList } from "../../ipc/orchestration";
import { qk } from "../../lib/queries";
import { Page } from "../layout/Page";
import { PageHeader, BackLink } from "../layout/PageHeader";
import { Skeleton } from "../ui/skeleton";
import { ErrorBanner } from "../feedback/ErrorBanner";
import { AgentKindBadge } from "./AgentKindBadge";
import { ModeSwitch, useAgentModeToggle } from "../orchestration/ModeSwitch";

/// Shared frame for the agent sub-pages (detail + routing): the triple
/// guard (loading / error+retry / not-found), and one sticky header with
/// the page's SINGLE ModeSwitch plus a live mode summary subtitle. Both
/// pages render inside it so their chrome can never drift apart. Children
/// receive the resolved agent (and nothing else — mode state comes from
/// `useAgentModeToggle` where needed).
export function AgentPageFrame({
  agentId,
  backTo,
  titleSuffix,
  children,
}: {
  agentId: string;
  backTo: "agents" | "detail";
  titleSuffix?: string;
  children: (agent: AgentInfo) => ReactNode;
}) {
  const { t } = useTranslation();
  const agentsQ = useQuery({ queryKey: qk.agents(), queryFn: agentList });
  const agent = (agentsQ.data ?? []).find((a) => a.id === agentId);

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
      <AgentPageHeader agent={agent} backTo={backTo} titleSuffix={titleSuffix} />
      {children(agent)}
    </Page>
  );
}

/// The sticky header both sub-pages share: identity + kind badge + optional
/// suffix, the live mode summary as subtitle, and the single ModeSwitch.
function AgentPageHeader({
  agent,
  backTo,
  titleSuffix,
}: {
  agent: AgentInfo;
  backTo: "agents" | "detail";
  titleSuffix?: string;
}) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const supported = agent.capability.supports_gateway;
  const { routed } = useAgentModeToggle(agent.id, supported);

  return (
    <PageHeader
      sticky
      title={
        <span className="flex items-center gap-2">
          {agent.display_name}
          {titleSuffix && (
            <span className="text-sm font-normal text-muted">· {titleSuffix}</span>
          )}
          <AgentKindBadge id={agent.id} />
        </span>
      }
      subtitle={<ModeSubtitle agent={agent} routed={routed} />}
      info={supported ? t("agentDetail.helpGateway") : t("agentDetail.helpPlain")}
      back={
        backTo === "agents" ? (
          <BackLink to="/agents">{t("nav.agents")}</BackLink>
        ) : (
          <BackLink
            onClick={() => navigate({ to: "/agents/$id", params: { id: agent.id } })}
          >
            {agent.display_name}
          </BackLink>
        )
      }
      action={<ModeSwitch agentId={agent.id} supportsGateway={supported} />}
    />
  );
}

/// One mono summary line answering "where does this agent's traffic go
/// right now": the active Direct binding, or the `*` policy's first target
/// in Routed mode. Deliberately cheap — the full picture is the detail
/// page's route overview card.
function ModeSubtitle({ agent, routed }: { agent: AgentInfo; routed: boolean }) {
  const { t } = useTranslation();
  const endpointsQ = useQuery({ queryKey: qk.endpoints(), queryFn: endpointList });
  const policiesQ = useQuery({
    queryKey: qk.routingPolicies(agent.id),
    queryFn: () => routingPolicyList(agent.id),
  });
  const endpoints = endpointsQ.data ?? [];
  const nameFor = (id: string) =>
    endpoints.find((e) => e.id === id)?.display_name ?? id;

  if (!agent.capability.supports_gateway || !routed) {
    const active = agent.providers.find(
      (p) => p.provider_id === agent.active_provider_id,
    );
    return (
      <span className="font-mono text-2xs text-subtle">
        {active
          ? t("agentDetail.subDirect", { provider: nameFor(active.provider_id) })
          : t("agentDetail.subUnbound")}
      </span>
    );
  }
  const star = (policiesQ.data ?? []).find((p) => p.role === "*");
  const first = star?.route_targets?.[0];
  return (
    <span className="font-mono text-2xs text-subtle">
      {first
        ? t("agentDetail.subRouted", {
            provider: nameFor(first.endpoint),
            model: first.model,
          })
        : t("agentDetail.subRoutedEmpty")}
    </span>
  );
}
