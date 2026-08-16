import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { useMemo, useState } from "react";
import { Cable, Plus } from "lucide-react";
import {
  agentList,
  mcpDelete,
  mcpImportOne,
  mcpList,
  mcpProbe,
  mcpSetState,
  mcpSyncAll,
  mcpUnmanage,
  mcpUsageStats,
  type McpServer,
  type ProbeResult,
} from "../ipc";
import { useMCPCapableAgents } from "../lib/agents";
import { Card } from "../components/controls/Card";
import { Button } from "../components/controls/Button";
import { Page } from "../components/layout/Page";
import type { AgentState } from "../components/controls/AgentToggleGroup";
import { Tabs } from "../components/controls/Tabs";
import { PageHeader } from "../components/layout/PageHeader";
import { SyncIndicator } from "../components/feedback/SyncIndicator";
import { qk } from "../lib/queries";
import { ErrorBanner } from "../components/feedback/ErrorBanner";
import { EmptyState } from "../components/feedback/EmptyState";
import { Skeleton } from "../components/ui/skeleton";
import { extractError } from "../ipc/errors";
import { useUI } from "../stores/ui";
import { McpImportSection } from "../components/mcp/McpImportSection";
import { McpServerDialog } from "../components/mcp/McpServerDialog";
import { McpServerList, type McpAgentOption } from "../components/mcp/McpServerList";

/// Lookup of display name by agent id, built from the agent list the backend
/// already returns (with a `supports_mcp` flag). This replaces the old
/// hardcoded `MCP_AGENT_IDS` / `AGENT_LABELS` that had to be kept in sync
/// with `mcp/providers.rs` by hand.
function useAgentLabels() {
  const agentQ = useQuery({ queryKey: qk.agents(), queryFn: agentList });
  const labels = new Map<string, string>();
  for (const c of agentQ.data ?? []) labels.set(c.id, c.display_name);
  return (id: string) => labels.get(id) ?? id;
}

export function McpPage() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const q = useQuery({ queryKey: qk.mcp(), queryFn: mcpList });
  const capableAgentsQ = useMCPCapableAgents();
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState<McpServer | null>(null);
  const [restoringId, setRestoringId] = useState<string | null>(null);
  // Page-level tab: managed (DB rows) vs importable (candidates found in
  // agent configs). Mirrors the Skills page's tab structure so the two
  // surfaces read as siblings.
  const [tab, setTab] = useState<"managed" | "importable">("managed");
  // Click-to-test results keyed by server id. Cleared on toggle/delete so a
  // stale "ok" doesn't survive a config change.
  const [probes, setProbes] = useState<Record<string, ProbeResult>>({});
  // Set of in-flight probe ids — a single id broke concurrent probes
  // ("test all"): the first finisher cleared the loading state for everyone.
  const [probing, setProbing] = useState<Set<string>>(new Set());
  const servers = q.data ?? [];
  const usageQ = useQuery({ queryKey: qk.mcpUsage(), queryFn: mcpUsageStats });
  const usageByServer = Object.fromEntries(
    (usageQ.data ?? []).map((u) => [u.server_id, u]),
  );
  const labelForAgent = useAgentLabels();
  const toast = useUI((s) => s.pushToast);

  // All MCP-capable agents (including disconnected ones) so the user can see
  // and untoggle stale enablements on an agent that's no longer detected.
  // `tri` marks agents whose config format carries a per-server `enabled`
  // field — only they get the "written but disabled" state.
  const allMcpAgents: McpAgentOption[] = (capableAgentsQ.data ?? []).map((c) => ({
    id: c.id,
    connected: c.status === "ok",
    tri: c.capability.supports_mcp_enabled,
  }));

  // Edit dialog: connected agents PLUS every agent the server is currently
  // written on (enabled/disabled) even when it's offline right now — saving
  // with only the connected subset would silently drop an offline agent's
  // enablement and the next persist would delete its config entry.
  const editAgents = useMemo(() => {
    const connected = allMcpAgents.filter((c) => c.connected);
    if (!editing) return connected;
    const bound = new Set([...editing.enabled_agents, ...editing.disabled_agents]);
    const known = new Set(connected.map((c) => c.id));
    const offline = [...bound].filter((id) => !known.has(id));
    return [
      ...connected,
      ...offline.map((id) => ({ id, connected: false, tri: false })),
    ];
  }, [allMcpAgents, editing]);

  const invalidate = () => {
    qc.invalidateQueries({ queryKey: qk.mcp() });
    qc.invalidateQueries({ queryKey: qk.agents() });
  };

  const setStateMut = useMutation({
    mutationFn: (v: { id: string; agent: string; state: AgentState }) =>
      mcpSetState(v.id, v.agent, v.state),
    onSuccess: (_d, vars) => {
      invalidate();
      const agent = labelForAgent(vars.agent);
      toast(
        vars.state === "enabled"
          ? t("mcp.stateEnabledToast", { agent })
          : vars.state === "disabled"
            ? t("mcp.stateDisabledToast", { agent })
            : t("mcp.stateRemovedToast", { agent }),
        "success",
      );
    },
    onError: (e) => toast(t("mcp.stateFailed", { err: extractError(e) }), "error"),
  });
  const deleteMut = useMutation({
    mutationFn: (id: string) => mcpDelete(id),
    onSuccess: () => {
      invalidate();
      toast(t("mcp.deleted"), "success");
    },
    onError: (e) => toast(t("mcp.deleteFailed", { err: extractError(e) }), "error"),
  });
  const restoreMut = useMutation({
    // "Restore" = return the server to an unmanaged state: Nestra drops its DB
    // row but leaves the entries already in agent config files in place, so
    // the MCP keeps working and re-surfaces under Importable. It is the
    // inverse of import — distinct from delete (which also strips the config
    // entries). The confirm dialog (on the button) tells the user this.
    mutationFn: (id: string) => mcpUnmanage(id),
    onSuccess: (_d, id) => {
      invalidate();
      // Refresh the import scan so the newly-unmanaged server shows under
      // Importable immediately.
      qc.invalidateQueries({ queryKey: qk.mcpImport() });
      const name = servers.find((s) => s.id === id)?.name ?? id;
      toast(t("mcp.restoredToast", { name }), "success");
    },
    onError: (e) => toast(t("mcp.restoreFailed", { err: extractError(e) }), "error"),
  });
  const onRestore = (id: string) => {
    setRestoringId(id);
    restoreMut.mutate(id, { onSettled: () => setRestoringId(null) });
  };
  const importMut = useMutation({
    // Sequential: each mcpImportOne re-reads the DB row, so parallel calls
    // race the read-modify-write and the last writer drops the others'
    // enables. Folding the imports one at a time keeps the merge correct.
    mutationFn: async (v: { agents: string[]; name: string }) => {
      let last: McpServer | null = null;
      for (const agent of v.agents) {
        last = await mcpImportOne(agent, v.name);
      }
      return last;
    },
    onSuccess: () => {
      invalidate();
      toast(t("mcp.imported"), "success");
    },
    onError: (e) => toast(t("mcp.importFailed", { err: extractError(e) }), "error"),
  });
  const syncAllMut = useMutation({
    mutationFn: () => mcpSyncAll(),
    onSuccess: () => {
      invalidate();
      toast(t("mcp.synced"), "success");
    },
    onError: (e) => toast(t("mcp.syncFailed", { err: extractError(e) }), "error"),
  });
  const probeMut = useMutation({
    mutationFn: (id: string) => mcpProbe(id),
    onMutate: (id) =>
      setProbing((cur) => new Set(cur).add(id)),
    onSettled: (_d, _e, id) =>
      setProbing((cur) => {
        const next = new Set(cur);
        next.delete(id);
        return next;
      }),
    onSuccess: (r, id) => {
      setProbes((cur) => ({ ...cur, [id]: r }));
      toast(
        r.ok
          ? r.latency_ms != null
            ? t("mcp.probeOk", { ms: r.latency_ms })
            : t("mcp.probeOkPlain")
          : t("mcp.probeFailed", { reason: r.reason ?? t("mcp.probeUnknown") }),
        r.ok ? "success" : "error",
      );
    },
    onError: (e, id) => {
      // AppError path (e.g. unknown id). Synthesize a failure result so the
      // dot stays consistent with the toast.
      setProbes((cur) => ({
        ...cur,
        [id]: { ok: false, latency_ms: null, reason: extractError(e) },
      }));
      toast(t("mcp.probeFailed", { reason: extractError(e) }), "error");
    },
  });

  return (
    <Page>
      <PageHeader
        title={t("mcp.title")}
        info={t("mcp.help")}
        action={
          <div className="flex items-center gap-3">
            <SyncIndicator query={q} />
            <Button
              variant="ghost"
              size="sm"
              disabled={syncAllMut.isPending}
              onClick={() => syncAllMut.mutate()}
              title={t("mcp.syncAllTitle")}
            >
              {syncAllMut.isPending ? t("mcp.syncing") : t("mcp.syncAll")}
            </Button>
            <Button
              variant="ghost"
              size="sm"
              disabled={probeMut.isPending || servers.length === 0}
              loading={probeMut.isPending}
              onClick={() => servers.forEach((s) => probeMut.mutate(s.id))}
              title={t("mcp.testAllTitle")}
            >
              {t("mcp.testAll")}
            </Button>
            <Button variant="primary" size="sm" onClick={() => setAdding(true)}>
              <Plus data-icon size={14} />
              {t("mcp.addServer")}
            </Button>
          </div>
        }
      />

      <Tabs
        size="sm"
        value={tab}
        onChange={(v) => setTab(v as "managed" | "importable")}
        items={[
          { id: "managed", label: t("mcp.tabManaged") },
          { id: "importable", label: t("mcp.tabImportable") },
        ]}
      />

      {tab === "managed" && (
        <>
          {q.isLoading && (
            <Card padding="md">
              <div className="space-y-3">
                <Skeleton className="h-4 w-1/4" />
                <Skeleton className="h-8 w-full" />
                <Skeleton className="h-8 w-full" />
              </div>
            </Card>
          )}

          {q.isError && (
            <ErrorBanner onRetry={() => q.refetch()}>
              {t("mcp.loadFailed")}
            </ErrorBanner>
          )}

          {q.data && servers.length === 0 && (
            <EmptyState
              title={t("mcp.noServers")}
              hint={t("mcp.noServersHint")}
              icon={<Cable data-icon size={20} />}
            />
          )}

          {servers.length > 0 && (
            <McpServerList
              usage={usageByServer}
              servers={servers}
              agents={allMcpAgents}
              labelForAgent={labelForAgent}
              probes={probes}
              probing={probing}
              setStateMut={setStateMut}
              deleteMut={deleteMut}
              onRestore={onRestore}
              restoringId={restoringId}
              probeMut={probeMut}
              onEdit={setEditing}
            />
          )}
        </>
      )}

      {tab === "importable" && (
        <McpImportSection
          labelForAgent={labelForAgent}
          onImport={(agents, name) => importMut.mutate({ agents, name })}
        />
      )}

      {adding && (
        <McpServerDialog
          mode="add"
          agents={allMcpAgents.filter((c) => c.connected)}
          labelForAgent={labelForAgent}
          onCancel={() => setAdding(false)}
          onDone={() => {
            invalidate();
            setAdding(false);
          }}
        />
      )}
      {editing && (
        <McpServerDialog
          mode="edit"
          initial={editing}
          agents={editAgents}
          labelForAgent={labelForAgent}
          onCancel={() => setEditing(null)}
          onDone={() => {
            invalidate();
            setEditing(null);
          }}
        />
      )}
    </Page>
  );
}
