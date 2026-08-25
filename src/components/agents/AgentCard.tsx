import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { AgentKindBadge } from "./AgentKindBadge";
import { ArrowRight } from "lucide-react";
import { useNavigate } from "@tanstack/react-router";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import {
  agentClearOverride,
  agentSetEnabled,
  agentSetOverride,
  type AgentInfo,
  type EndpointInfo,
} from "../../ipc";
import { extractError } from "../../ipc/errors";
import { agentGatewayEnabled, tasks, type TaskSummary } from "../../ipc/orchestration";
import { qk } from "../../lib/queries";
import { statusToneClass } from "../../lib/agents";
import { useUI } from "../../stores/ui";
import { Button } from "../controls/Button";
import { ButtonGroup } from "../controls/ButtonGroup";
import { Card } from "../controls/Card";
import { StatusDot } from "../feedback/StatusDot";
import { Note } from "../feedback/Note";
import { Badge } from "../ui/badge";
import { Switch } from "../ui/switch";
import { ModeSwitch } from "../orchestration/ModeSwitch";

export function AgentCard({ agent, endpoints }: { agent: AgentInfo; endpoints: EndpointInfo[] }) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useUI((s) => s.pushToast);
  const connected = agent.status === "ok" || agent.status === "manual_ok";

  const setEnabledMut = useMutation({
    mutationFn: (enabled: boolean) => agentSetEnabled(agent.id, enabled),
    onSuccess: (updated) => {
      qc.setQueryData<AgentInfo[]>(["agents"], (curr) =>
        (curr ?? []).map((c) => (c.id === updated.id ? updated : c)),
      );
      qc.invalidateQueries({ queryKey: qk.agentConfig(agent.id) });
      toast(
        updated.enabled
          ? t("agents.toggleEnabled", { name: agent.display_name })
          : t("agents.toggleDisabled", { name: agent.display_name }),
        "success",
      );
    },
    onError: (e: unknown) =>
      toast(t("agents.updateFailed", { name: agent.display_name, err: extractError(e) ?? String(e) }), "error"),
  });

  const setOverrideMut = useMutation({
    mutationFn: (vars: { agentPath: string | null; configPath: string | null }) =>
      agentSetOverride(agent.id, vars.agentPath, vars.configPath),
    onSuccess: (updated) => {
      qc.setQueryData<AgentInfo[]>(["agents"], (curr) =>
        (curr ?? []).map((c) => (c.id === updated.id ? updated : c)),
      );
      qc.invalidateQueries({ queryKey: qk.agents() });
      toast(t("agents.configured", { name: agent.display_name }), "success");
    },
    onError: (e: unknown) =>
      toast(t("agents.configureFailed", { name: agent.display_name, err: extractError(e) ?? String(e) }), "error"),
  });

  const clearOverrideMut = useMutation({
    mutationFn: () => agentClearOverride(agent.id),
    onSuccess: (updated) => {
      qc.setQueryData<AgentInfo[]>(["agents"], (curr) =>
        (curr ?? []).map((c) => (c.id === updated.id ? updated : c)),
      );
      qc.invalidateQueries({ queryKey: qk.agents() });
      toast(t("agents.autoDetectRestored", { name: agent.display_name }), "success");
    },
    onError: (e: unknown) =>
      toast(t("agents.clearOverrideFailed", { name: agent.display_name, err: extractError(e) ?? String(e) }), "error"),
  });

  const pickBinary = async () => {
    try {
      const picked = await openDialog({
        multiple: false,
        directory: false,
        title: t("agents.locateTitle", { name: agent.display_name }),
      });
      if (typeof picked === "string") {
        setOverrideMut.mutate({ agentPath: picked, configPath: null });
      }
    } catch (e) {
      // A rejected dialog promise must not surface as an unhandled rejection.
      toast(t("agents.pickFailed", { name: agent.display_name, err: extractError(e) ?? String(e) }), "error");
    }
  };
  const pickFolder = async () => {
    try {
      const picked = await openDialog({
        multiple: false,
        directory: true,
        title: t("agents.pickFolderTitle", { name: agent.display_name }),
      });
      if (typeof picked === "string") {
        setOverrideMut.mutate({ agentPath: null, configPath: picked });
      }
    } catch (e) {
      toast(t("agents.pickFailed", { name: agent.display_name, err: extractError(e) ?? String(e) }), "error");
    }
  };

  const hasOverride = agent.agent_path_override !== null || agent.config_path_override !== null;
  const statusLabel =
    agent.status === "ok"
      ? agent.installed_version ?? t("agents.installed")
      : agent.status === "manual_ok"
        ? t("agents.statusManual")
        : agent.status === "manual_missing"
          ? t("agents.statusManualMissing")
          : t("agents.statusMissing");

  return (
    <Card
      padding="none"
      className="overflow-hidden transition-colors duration-fast hover:border-border-strong"
    >
      <AgentStatusBanner
        enabled={agent.enabled}
        connected={connected}
        agentId={agent.id}
        displayName={agent.display_name}
        statusLabel={statusLabel}
        source={agent.source}
        loading={setEnabledMut.isPending}
        onToggle={(v) => setEnabledMut.mutate(v)}
      />

      <div className="space-y-2 p-3">
        <div className="flex flex-wrap items-center gap-2 text-xs text-subtle">
          <CapabilityBadge agent={agent} />
        </div>

        {!connected ? (
          <ButtonGroup space="loose" justify="end" wrap>
            <Button size="sm" variant="ghost" onClick={pickBinary} disabled={setOverrideMut.isPending}>{t("agents.configureBinary")}</Button>
            <Button size="sm" variant="ghost" onClick={pickFolder} disabled={setOverrideMut.isPending}>{t("agents.configureFolder")}</Button>
            {hasOverride && (
              <Button size="sm" variant="ghost" onClick={() => clearOverrideMut.mutate()} disabled={clearOverrideMut.isPending}>{t("agents.clearOverride")}</Button>
            )}
            <span className="text-xs text-subtle">
              {t("agents.installHint")}
            </span>
          </ButtonGroup>
        ) : !agent.enabled ? (
          <DisabledNote />
        ) : (
          <AgentCardBody agent={agent} endpoints={endpoints} />
        )}
      </div>
    </Card>
  );
}

/// The connected+enabled body: mode switch (Direct | Routed), a compact
/// mode-specific status summary, and the entry point to the full detail page.
function AgentCardBody({
  agent,
  endpoints,
}: {
  agent: AgentInfo;
  endpoints: EndpointInfo[];
}) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const routedQ = useQuery({
    queryKey: ["orchestration", "gateway-flag", agent.id],
    queryFn: () => agentGatewayEnabled(agent.id),
    enabled: agent.capability.supports_gateway,
  });
  const routed = routedQ.data ?? false;

  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between gap-3">
        <ModeSwitch agentId={agent.id} supportsGateway={agent.capability.supports_gateway} />
        <div className="flex items-center gap-2">
          {routed ? <RoutedSummary agent={agent} /> : <DirectSummary agent={agent} endpoints={endpoints} />}
          <Button
            size="sm"
            variant="ghost"
            onClick={() => navigate({ to: "/agents/$id", params: { id: agent.id } })}
          >
            {t("agents.open")} <ArrowRight data-icon size={13} />
          </Button>
        </div>
      </div>
      {routed && <RoutedTaskLine agent={agent} />}
    </div>
  );
}

/// Direct-mode summary: the active provider + default model (from bindings).
function DirectSummary({ agent, endpoints }: { agent: AgentInfo; endpoints: EndpointInfo[] }) {
  const { t } = useTranslation();
  const active = agent.providers.find((p) => p.active) ?? agent.providers[0];
  const ep = endpoints.find((e) => e.id === agent.active_provider_id);
  const model = ep?.models?.default;
  return (
    <span className="font-mono text-2xs text-subtle tabular">
      {active ? `${active.display_name}${model ? ` / ${String(model)}` : ""}` : t("agents.noProviderBound")}
    </span>
  );
}

/// Routed-mode summary: one-line task health for this agent (latest status,
/// request count, any broken generation).
function RoutedSummary({ agent }: { agent: AgentInfo }) {
  const { t } = useTranslation();
  const tasksQ = useQuery({
    queryKey: ["orchestration", "tasks"],
    queryFn: () => tasks(50),
  });
  const mine = (tasksQ.data ?? []).filter((t) => t.agent_id === agent.id);
  const latest = mine[0]; // tasks() sorts by last_seen desc
  if (!latest) {
    return <span className="font-mono text-2xs text-subtle">{t("agents.noTrafficYet")}</span>;
  }
  const status = latest.latest_status;
  const tone = statusToneClass(status);
  return (
    <span className="font-mono text-2xs tabular">
      <span className="text-subtle">{t("orchestration.modeRouted")}</span>
      {" · "}
      <span className={tone}>{status ?? "—"}</span>
      {" · "}
      <span className="text-subtle">{t("orchestration.reqCount", { n: latest.request_count })}</span>
      {latest.generation_broken && <span className="text-danger"> · {t("orchestration.genBroken")}</span>}
    </span>
  );
}

/// Routed-mode second line: the agent's most recent tasks, compact.
function RoutedTaskLine({ agent }: { agent: AgentInfo }) {
  // SAME key + fn as RoutedSummary: the two share the cache, and a shared
  // key with a different queryFn would mean the second fn never runs (the
  // first registered wins) — stale 20-row data forever.
  const tasksQ = useQuery({
    queryKey: ["orchestration", "tasks"],
    queryFn: () => tasks(50),
    refetchInterval: 5000,
  });
  const mine = (tasksQ.data ?? []).filter((t) => t.agent_id === agent.id).slice(0, 3);
  if (mine.length === 0) return null;
  return (
    <div className="flex flex-wrap items-center gap-1.5 border-t border-border pt-2">
      {mine.map((t) => (
        <TaskChip key={t.task_id} t={t} />
      ))}
    </div>
  );
}

function TaskChip({ t }: { t: TaskSummary }) {
  const { t: tr } = useTranslation();
  const status = t.latest_status;
  const tone = statusToneClass(status);
  return (
    <span className="inline-flex items-center gap-1.5 border border-border bg-inset px-2 py-0.5 font-mono text-2xs text-muted">
      <span className="text-subtle">{t.task_id.slice(0, 6)}</span>
      <span className={tone}>{status ?? "—"}</span>
      <span className="text-subtle">{tr("orchestration.reqCountCompact", { n: t.request_count })}</span>
      {t.generation_broken && <span className="text-danger">!</span>}
    </span>
  );
}

// ============ AgentStatusBanner ============
/// Status banner: the enable switch plus the agent's connection state. The
/// "Managed/Off" label + switch carry the configuring state, so no extra
/// status line is needed.
function AgentStatusBanner({
  enabled,
  connected,
  agentId,
  displayName,
  statusLabel,
  source,
  loading,
  onToggle,
}: {
  enabled: boolean;
  connected: boolean;
  agentId: string;
  displayName: string;
  statusLabel: string;
  source: "auto" | "manual";
  loading: boolean;
  onToggle: (v: boolean) => void;
}) {
  const { t } = useTranslation();
  const managing = enabled && connected;
  return (
    <div
      className={`flex items-center gap-3 border-b px-3 py-1.5 ${
        managing
          ? "border-success-border bg-success-soft"
          : "border-warning-border bg-warning-soft"
      }`}
    >
      <StatusDot
        status={connected ? "ok" : "missing"}
        size={2}
        title={connected ? (source === "manual" ? t("agents.installedManual") : t("agents.installed")) : t("agents.statusMissing")}
      />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="text-md font-medium">{displayName}</span>
          <AgentKindBadge id={agentId} />
          <span className="text-xs text-muted tabular">{statusLabel}</span>
        </div>
      </div>
      <div className="flex items-center gap-2">
        <span className={`text-2xs uppercase tracking-[0.08em] ${managing ? "text-success" : "text-muted"}`}>
          {managing ? t("agents.managed") : t("agents.off")}
        </span>
        <Switch
          checked={enabled}
          onCheckedChange={(v) => onToggle(v)}
          disabled={loading || !connected}
          title={
            managing
              ? t("agents.managingDesc")
              : t("agents.leavingDesc")
          }
        />
      </div>
    </div>
  );
}

/// Factory Configuration row. The snapshot now lives in the same ON/OFF
/// toggle: enabling captures it, disabling wipes the binding set + restores
/// it. No standalone restore UI.

export function CapabilityBadge({ agent }: { agent: AgentInfo }) {
  const { t } = useTranslation();
  if (agent.supported_protocols.length === 0) return null;
  const protocols = agent.supported_protocols.join(", ");
  const slot = agent.capability.supports_multiple_providers
    ? t("agents.multiProvider")
    : t("agents.singleProvider");
  const model = agent.model_selection === "anthropic_tiers" ? t("agents.anthropicTiers") : t("agents.freeForm");
  return (
    <Badge tone="neutral" variant="soft" className="px-2 uppercase tracking-[0.08em]" title={t("agents.supportsTitle", { protocols })}>
      {slot} · {model}
    </Badge>
  );
}

function DisabledNote() {
  const { t } = useTranslation();
  return (
    <Note className="mt-3">
      {t("agents.disabledNote")}
    </Note>
  );
}
