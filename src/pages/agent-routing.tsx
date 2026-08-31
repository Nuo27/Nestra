import { useTranslation } from "react-i18next";
import { Workflow } from "lucide-react";
import { AgentPageFrame } from "../components/agents/AgentPageFrame";
import { Card } from "../components/controls/Card";
import { SectionHeader } from "../components/layout/SectionHeader";
import { Note } from "../components/feedback/Note";
import { useAgentModeToggle } from "../components/orchestration/ModeSwitch";
import { RoutingPolicyEditor } from "../components/orchestration/RoutingPolicyEditor";

/// /agents/$id/routing — the focused (agent, role) → endpoint-chain policy
/// editor. Policy data is mode-independent, so the page stays fully
/// editable while the agent is Direct — a Note explains when it takes
/// effect. The route overview lives on the detail page's cockpit.
export function AgentRoutingPage({ id }: { id: string }) {
  const { t } = useTranslation();
  return (
    <AgentPageFrame
      agentId={id}
      backTo="detail"
      titleSuffix={t("agentRouting.titleSuffix")}
    >
      {(agent) => <RoutingBody agentId={agent.id} supportsGateway={agent.capability.supports_gateway} />}
    </AgentPageFrame>
  );
}

function RoutingBody({ agentId, supportsGateway }: { agentId: string; supportsGateway: boolean }) {
  const { t } = useTranslation();
  const { routed } = useAgentModeToggle(agentId, supportsGateway);
  return (
    <div className="space-y-3">
      {!routed && <Note>{t("agentRouting.directNote")}</Note>}
      <Card padding="none">
        <SectionHeader
          icon={<Workflow data-icon size={14} />}
          title={t("agentRouting.policyTitle")}
          hint={t("agentRouting.policyHint")}
        />
        <div className="p-3">
          <RoutingPolicyEditor agentId={agentId} />
        </div>
      </Card>
    </div>
  );
}
