import { useTranslation } from "react-i18next";
import { formatRelative } from "../../lib/format";
import type { McpUsageStat } from "../../ipc";
import { Activity, Trash2 } from "lucide-react";
import type { UseMutationResult } from "@tanstack/react-query";
import type { McpServer, ProbeResult } from "../../ipc";
import { transportLabel } from "../../lib/mcp";
import type { AgentState } from "../controls/AgentToggleGroup";
import { AgentStateGroup } from "../controls/AgentToggleGroup";
import { Card } from "../controls/Card";
import { Button } from "../controls/Button";
import { confirmDialog } from "../controls/ConfirmDialog";
import { ProbeDot } from "./ProbeDot";

/// A server row's agent-toggle target (id + tri-state capability).
export type McpAgentOption = { id: string; connected: boolean; tri: boolean };

/**
 * The managed-servers list: one row per server with the per-agent state
 * group, restore/delete/test actions, and the click-to-test dot. The probe
 * state + row mutations are owned by the page (the header's "Test all"
 * shares the same `probeMut`/`probing`), so this stays presentational.
 */
export function McpServerList({
  usage,
  servers,
  agents,
  labelForAgent,
  probes,
  probing,
  setStateMut,
  deleteMut,
  onRestore,
  restoringId,
  probeMut,
  onEdit,
}: {
  servers: McpServer[];
  /** Per-server gateway-observed usage (P1-1); servers absent from the map
   *  simply render no badge. */
  usage: Record<string, McpUsageStat>;
  agents: McpAgentOption[];
  labelForAgent: (id: string) => string;
  probes: Record<string, ProbeResult>;
  probing: Set<string>;
  setStateMut: UseMutationResult<
    unknown,
    unknown,
    { id: string; agent: string; state: AgentState }
  >;
  deleteMut: UseMutationResult<unknown, unknown, string>;
  /** Confirm + restore-to-unmanaged for one server (the page owns the
   *  `restoringId` busy state around the mutation). */
  onRestore: (id: string) => void;
  restoringId: string | null;
  probeMut: UseMutationResult<ProbeResult, unknown, string>;
  onEdit: (s: McpServer) => void;
}) {
  const { t } = useTranslation();

  return (
    <Card
      title={t("mcp.serversTitle", { count: servers.length })}
      description={t("mcp.serversDesc")}
    >
      <ul className="divide-y divide-border">
        {servers.map((s) => (
          <li key={s.id} className="flex flex-wrap items-center gap-3 py-2">
            <div className="min-w-0 flex-1 basis-40">
              <div className="flex items-center gap-2">
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => onEdit(s)}
                  title={`${s.name} — ${transportLabel(s.transport.kind, s.transport.command ?? s.transport.url)}`}
                  className="min-w-0 truncate text-sm font-medium text-fg"
                >
                  {s.name}
                </Button>
                <ProbeDot result={probes[s.id]} />
              </div>
              {usage[s.id] && (
                <div className="text-2xs text-subtle tabular">
                  {usage[s.id].total_calls > 0
                    ? t("mcp.usageObserved", {
                        n: usage[s.id].total_calls,
                        rel: formatRelative(usage[s.id].last_used_at ?? 0),
                      })
                    : t("mcp.usageNone")}
                </div>
              )}
            </div>
            <div className="flex shrink-0 flex-wrap items-center gap-2">
              {agents.length > 0 && (
                <AgentStateGroup
                  items={agents.map((c) => ({
                    id: c.id,
                    label: labelForAgent(c.id),
                    state: s.enabled_agents.includes(c.id)
                      ? "enabled"
                      : s.disabled_agents.includes(c.id)
                        ? "disabled"
                        : "absent",
                    tri: c.tri,
                    pending:
                      setStateMut.isPending &&
                      setStateMut.variables?.id === s.id &&
                      setStateMut.variables?.agent === c.id,
                    disabled: !c.connected,
                  }))}
                  onSetState={(agent, state) =>
                    setStateMut.mutate({ id: s.id, agent, state })
                  }
                />
              )}
              <Button
                variant="ghost"
                size="sm"
                disabled={restoringId === s.id}
                loading={restoringId === s.id}
                onClick={async () => {
                  // Restore now means "stop managing" (keeps the agent
                  // config entries). Confirm first so the user knows — it
                  // removes the server from Nestra's management.
                  const ok = await confirmDialog({
                    title: t("mcp.restoreConfirmTitle", { name: s.name }),
                    body: t("mcp.restoreConfirmBody"),
                    confirmLabel: t("mcp.restoreBtn"),
                    tone: "primary",
                  });
                  if (!ok) return;
                  onRestore(s.id);
                }}
                title={t("mcp.restoreTitle")}
              >
                {t("mcp.restoreBtn")}
              </Button>
              <Button
                variant="danger"
                size="sm"
                disabled={deleteMut.isPending}
                onClick={async () => {
                  const ok = await confirmDialog({
                    title: t("mcp.deleteConfirmTitle", { name: s.name }),
                    body: t("mcp.deleteConfirmBody"),
                    confirmLabel: t("common.delete"),
                  });
                  if (ok) deleteMut.mutate(s.id);
                }}
                title={t("mcp.deleteTitle")}
                aria-label={t("mcp.deleteTitle")}
              >
                <Trash2 data-icon size={13} />
              </Button>
              <Button
                variant="ghost"
                size="sm"
                disabled={probing.has(s.id)}
                loading={probing.has(s.id)}
                onClick={() => probeMut.mutate(s.id)}
                title={t("mcp.testTitle")}
                aria-label={t("mcp.testAria")}
              >
                <Activity data-icon size={13} />
              </Button>
            </div>
          </li>
        ))}
      </ul>
    </Card>
  );
}
