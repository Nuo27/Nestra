import { useTranslation } from "react-i18next";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { agentDetect, agentList, endpointList } from "../ipc";
import { Button } from "../components/controls/Button";
import { Page } from "../components/layout/Page";
import { PageHeader, SectionLabel } from "../components/layout/PageHeader";
import { SyncIndicator } from "../components/feedback/SyncIndicator";
import { EmptyState } from "../components/feedback/EmptyState";
import { ErrorBanner } from "../components/feedback/ErrorBanner";
import { GatewayStatusBar } from "../components/orchestration/GatewayStatusBar";
import { AgentCard } from "../components/agents/AgentCard";
import { qk } from "../lib/queries";
import { useUI } from "../stores/ui";
import { Skeleton } from "../components/ui/skeleton";

export function AgentsPage() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const q = useQuery({ queryKey: qk.agents(), queryFn: agentList });
  const toast = useUI((s) => s.pushToast);
  const detect = useMutation({
    mutationFn: agentDetect,
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.agents() });
      toast(t("agents.detectionRefreshed"), "success");
    },
  });

  const all = q.data ?? [];
  // Hide unsupported rows (historical entries marked by migrations).
  const visible = all.filter((c) => c.status !== "unsupported");
  const connected = visible.filter((c) => c.status === "ok" || c.status === "manual_ok");
  const missing = visible.filter((c) => c.status === "missing" || c.status === "manual_missing");
  const endpointsQ = useQuery({ queryKey: qk.endpoints(), queryFn: endpointList });
  const endpoints = endpointsQ.data ?? [];

  return (
    <Page>
      <PageHeader
        title={t("agents.title")}
        info={t("agents.help")}
        action={
          <div className="flex items-center gap-3">
            <SyncIndicator query={q} />
            <Button size="sm" onClick={() => detect.mutate()} disabled={detect.isPending} loading={detect.isPending}>
              {detect.isPending ? t("agents.detecting") : t("agents.redetect")}
            </Button>
          </div>
        }
      />

      <div className="mb-4">
        <GatewayStatusBar />
      </div>

      {q.isLoading && (
        <div className="space-y-2">
          {Array.from({ length: 3 }).map((_, i) => (
            <Skeleton key={i} className="h-20 w-full" />
          ))}
        </div>
      )}
      {q.isError && (
        <ErrorBanner onRetry={() => q.refetch()}>{t("agents.loadFailed")}</ErrorBanner>
      )}

      {!q.isLoading && !q.isError && visible.length === 0 && (
        <EmptyState
          title={t("agents.none.title")}
          hint={t("agents.none.hint")}
        />
      )}

      <div className="space-y-2 animate-in fade-in duration-fast">
        {connected.map((c) => (
          <AgentCard key={c.id} agent={c} endpoints={endpoints} />
        ))}
      </div>

      {missing.length > 0 && (
        <>
          <SectionLabel className="mb-2 block">{t("agents.notConnected")}</SectionLabel>
          <div className="space-y-2 opacity-70">
            {missing.map((c) => (
              <AgentCard key={c.id} agent={c} endpoints={endpoints} />
            ))}
          </div>
        </>
      )}
    </Page>
  );
}
