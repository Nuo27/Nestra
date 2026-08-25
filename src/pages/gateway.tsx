import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Link } from "@tanstack/react-router";
import { ScrollText } from "lucide-react";
import {
  gatewayAutopickPort,
  gatewayGetStatus,
  gatewayRecentActivity,
  gatewayRestart,
  gatewaySetEnabled,
  gatewaySetPort,
  gatewayTokenGet,
  gatewayTokenRegenerate,
  type GatewayRuntimeState,
} from "../ipc/gateway";
import { agentList } from "../ipc";
import { extractError } from "../ipc/errors";
import { invalidateGateway, qk } from "../lib/queries";
import { useCopy } from "../lib/useCopy";
import { useUI } from "../stores/ui";
import { Page } from "../components/layout/Page";
import { PageHeader } from "../components/layout/PageHeader";
import { Card } from "../components/controls/Card";
import { Button } from "../components/controls/Button";
import { confirmDialog } from "../components/controls/ConfirmDialog";
import { Badge } from "../components/ui/badge";
import { Input } from "../components/ui/input";
import { Switch } from "../components/ui/switch";
import { StatusDot } from "../components/feedback/StatusDot";
import { Skeleton } from "../components/ui/skeleton";

/// Gateway Service control surface. Owns ONLY the gateway process: global
/// enable, runtime state, port (+ Hybrid fallback), loopback token, and basic
/// activity. Routing policy is on `/agents/$id/routing`; provider config on
/// `/providers/$id`. No mock data — every number is real (`route_request` /
/// `task`) or shown as an honest empty/unavailable state.
export function GatewayPage() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useUI((s) => s.pushToast);
  const [copied, copy] = useCopy();

  const statusQ = useQuery({
    queryKey: qk.gatewayStatus(),
    queryFn: gatewayGetStatus,
    // Poll while it settles; idle once stably running.
    refetchInterval: (q) =>
      q.state.data?.state === "running" ? 5000 : q.state.data?.state === "starting" ? 1000 : 0,
  });

  const agentsQ = useQuery({ queryKey: qk.agents(), queryFn: agentList });

  const onMutated = (msgOk: string) => () => {
    invalidateGateway(qc);
    toast(msgOk, "success");
  };
  const onErr = (key: string) => (e: unknown) =>
    toast(t(key, { err: extractError(e) ?? String(e) }), "error");

  const setEnabledMut = useMutation({
    mutationFn: (enabled: boolean) => gatewaySetEnabled(enabled),
    onSuccess: (d, enabled) => {
      invalidateGateway(qc);
      if (d.failed.length > 0) {
        toast(t("gateway.partialFailToast", { agents: d.failed.join(", ") }), "error");
      } else {
        toast(enabled ? t("gateway.enabledToast") : t("gateway.disabledToast"), "success");
      }
    },
    onError: onErr("gateway.toggleFailed"),
  });

  const restartMut = useMutation({
    mutationFn: gatewayRestart,
    onSuccess: onMutated(t("gateway.restartedToast")),
    onError: onErr("gateway.restartFailed"),
  });

  const autopickMut = useMutation({
    mutationFn: gatewayAutopickPort,
    onSuccess: (d) => {
      invalidateGateway(qc);
      if (d.failed.length > 0)
        toast(t("gateway.partialFailToast", { agents: d.failed.join(", ") }), "error");
      else toast(t("gateway.autopickToast", { port: d.bound_port }), "success");
    },
    onError: onErr("gateway.autopickFailed"),
  });

  const regenMut = useMutation({
    mutationFn: gatewayTokenRegenerate,
    onSuccess: (d) => {
      invalidateGateway(qc);
      setRevealed(null); // force a fresh fetch next reveal
      if (d.failed.length > 0)
        toast(t("gateway.partialFailToast", { agents: d.failed.join(", ") }), "error");
      else toast(t("gateway.tokenRegenToast"), "success");
    },
    onError: onErr("gateway.tokenRegenFailed"),
  });

  // Token reveal: fetched into LOCAL state only (never React Query cache).
  const [revealed, setRevealed] = useState<string | null>(null);
  const reveal = () =>
    gatewayTokenGet()
      .then((info) => setRevealed(info.token))
      .catch((e) => toast(t("gateway.tokenReadFailed", { err: extractError(e) ?? String(e) }), "error"));

  const [portDraft, setPortDraft] = useState("");
  const setPortMut = useMutation({
    mutationFn: (port: number) => gatewaySetPort(port),
    onSuccess: (d) => {
      invalidateGateway(qc);
      setPortDraft("");
      if (d.failed.length > 0)
        toast(t("gateway.partialFailToast", { agents: d.failed.join(", ") }), "error");
      else toast(t("gateway.portSetToast", { port: d.bound_port }), "success");
    },
    onError: onErr("gateway.portSetFailed"),
  });

  const status = statusQ.data;
  const enabled = status?.enabled ?? false;
  const state: GatewayRuntimeState = status?.state ?? "stopped";

  return (
    <Page width="wide">
      <PageHeader
        title={t("gateway.title")}
        info={t("gateway.help")}
        action={
          <div className="flex items-center gap-2">
            <span className="font-mono text-2xs text-subtle">{enabled ? t("gateway.on") : t("gateway.off")}</span>
            <Switch
              checked={enabled}
              disabled={setEnabledMut.isPending}
              onCheckedChange={async (v) => {
                if (
                  v === false &&
                  (status?.agents_enabled.length ?? 0) > 0 &&
                  !(await confirmDialog({
                    title: t("gateway.offConfirmTitle"),
                    body: t("gateway.offConfirmBody", { agents: status?.agents_enabled.join(", ") }),
                    confirmLabel: t("gateway.offConfirmLabel"),
                    tone: "danger",
                  }))
                )
                  return;
                setEnabledMut.mutate(v);
              }}
            />
          </div>
        }
      />

      {statusQ.isLoading ? (
        <Skeleton className="h-24 w-full" />
      ) : (
        <RuntimeCard
          state={state}
          baseUrl={status?.bound_base_url ?? ""}
          startedAt={status?.started_at ?? null}
          uptimeSecs={status?.uptime_secs ?? null}
          lastError={status?.last_error ?? null}
          onCopy={copy}
          copied={copied}
          onRestart={() => restartMut.mutate()}
          restarting={restartMut.isPending}
          canRestart={enabled}
        />
      )}

      <Card title={t("gateway.portCard")} description={t("gateway.portCardDesc")}>
        <div className="flex flex-wrap items-center justify-between gap-4">
          <div className="min-w-0">
            <div className="text-sm text-fg">
              {t("gateway.listeningOn")}{" "}
              <span className="font-mono text-2xs text-subtle">
                127.0.0.1:{status?.configured_port ?? 18777}
              </span>
            </div>
            <div className="prose text-xs text-subtle mt-0.5">{t("gateway.portHint")}</div>
          </div>
          <div className="flex items-center gap-2 ml-auto shrink-0">
            <Input
              size="sm"
              className="w-24 font-mono"
              placeholder="18777"
              value={portDraft}
              onChange={(e) => setPortDraft(e.target.value.replace(/[^0-9]/g, ""))}
            />
            <Button
              size="sm"
              variant="secondary"
              loading={setPortMut.isPending}
              disabled={!portDraft || Number(portDraft) < 1 || Number(portDraft) > 65535}
              onClick={() => setPortMut.mutate(Number(portDraft))}
            >
              {t("gateway.applyPort")}
            </Button>
          </div>
        </div>
        <div className="mt-3 flex flex-wrap items-center justify-end gap-2 border-t border-border pt-3">
          {state === "error" && (
            <Button size="sm" variant="primary" loading={autopickMut.isPending} onClick={() => autopickMut.mutate()}>
              {t("gateway.autoPick")}
            </Button>
          )}
          <Button
            size="sm"
            variant="ghost"
            loading={autopickMut.isPending}
            onClick={() => autopickMut.mutate()}
          >
            {t("gateway.autoPickNextFree")}
          </Button>
          <Button size="sm" variant="ghost" onClick={() => setPortMut.mutate(18777)}>
            {t("gateway.resetDefault")}
          </Button>
        </div>
      </Card>

      <Card title={t("gateway.tokenCard")} description={t("gateway.tokenCardDesc")}>
        <div className="flex flex-wrap items-center justify-between gap-4">
          <div className="min-w-0">
            <div className="font-mono text-sm text-fg">
              {revealed ?? (status?.has_token ? "••••••••••••••••••••••••" : t("gateway.noToken"))}
            </div>
            <div className="prose text-xs text-subtle mt-0.5">{t("gateway.tokenHint")}</div>
          </div>
          <div className="flex items-center gap-2 ml-auto shrink-0">
            <Button
              size="sm"
              variant="ghost"
              onClick={() => {
                // Toggle: an already-revealed token hides in place — the old
                // button always re-fetched, so a shown token could never be
                // re-masked.
                if (revealed) setRevealed(null);
                else reveal();
              }}
            >
              {revealed ? t("gateway.hide") : t("gateway.reveal")}
            </Button>
            <Button
              size="sm"
              variant="ghost"
              disabled={!revealed}
              onClick={() => revealed && copy(revealed)}
            >
              {copied ? t("gateway.copied") : t("gateway.copy")}
            </Button>
            <Button
              size="sm"
              variant="danger"
              loading={regenMut.isPending}
              onClick={async () => {
                if (
                  await confirmDialog({
                    title: t("gateway.regenConfirmTitle"),
                    body: t("gateway.regenConfirmBody"),
                    confirmLabel: t("gateway.regenConfirmLabel"),
                    tone: "danger",
                  })
                )
                  regenMut.mutate();
              }}
            >
              {t("gateway.regenerate")}
            </Button>
          </div>
        </div>
      </Card>

      <Card title={t("gateway.agentsCard")} description={t("gateway.agentsCardDesc")}>
        <ConnectedAgents
          enabledIds={status?.agents_enabled ?? []}
          agents={agentsQ.data ?? []}
          gatewayRunning={state === "running"}
        />
      </Card>

      <ActivityCard />
    </Page>
  );
}

// ---- sections ---------------------------------------------------------------

function RuntimeCard({
  state,
  baseUrl,
  startedAt,
  uptimeSecs,
  lastError,
  onCopy,
  copied,
  onRestart,
  restarting,
  canRestart,
}: {
  state: GatewayRuntimeState;
  baseUrl: string;
  startedAt: number | null;
  uptimeSecs: number | null;
  lastError: string | null;
  onCopy: (s: string) => Promise<void>;
  copied: boolean;
  onRestart: () => void;
  restarting: boolean;
  canRestart: boolean;
}) {
  const { t } = useTranslation();
  const tone =
    state === "running" ? "success" : state === "error" ? "danger" : state === "starting" ? "warning" : "neutral";
  const label =
    state === "running"
      ? t("gateway.running")
      : state === "starting"
        ? t("gateway.starting")
        : state === "error"
          ? t("gateway.error")
          : t("gateway.stopped");
  return (
    <Card
      title={t("gateway.runtimeCard")}
      description={t("gateway.runtimeCardDesc")}
      action={
        <Button size="sm" variant="ghost" loading={restarting} disabled={!canRestart} onClick={onRestart}>
          {t("gateway.restart")}
        </Button>
      }
    >
      <div className="flex flex-wrap items-center gap-3">
        <Badge
          tone={tone}
          variant="soft"
          className={`font-mono text-2xs ${state === "starting" ? "animate-pulse" : ""}`}
        >
          {state === "running" ? "● " : state === "error" ? "! " : "○ "}
          {label}
        </Badge>
        {baseUrl && (
          <button
            className="truncate font-mono text-2xs text-subtle hover:text-fg"
            onClick={() => onCopy(baseUrl)}
            title={t("gateway.copy")}
          >
            {baseUrl} {copied ? "✓" : ""}
          </button>
        )}
      </div>
      <div className="mt-3 grid grid-cols-2 gap-2 border-t border-border pt-3 text-2xs text-subtle sm:grid-cols-3">
        <Field label={t("gateway.uptime")} value={uptimeSecs == null ? "—" : fmtUptime(uptimeSecs)} />
        <Field label={t("gateway.startedAt")} value={startedAt == null ? "—" : fmtTime(startedAt)} />
        <Field label={t("gateway.lastError")} value={lastError ?? "—"} mono />
      </div>
    </Card>
  );
}

function ConnectedAgents({
  enabledIds,
  agents,
  gatewayRunning,
}: {
  enabledIds: string[];
  agents: { id: string; display_name: string }[];
  gatewayRunning: boolean;
}) {
  const { t } = useTranslation();
  if (enabledIds.length === 0)
    return <div className="prose text-xs text-subtle">{t("gateway.noRoutedAgents")}</div>;
  const name = (id: string) => agents.find((a) => a.id === id)?.display_name ?? id;
  return (
    <ul className="space-y-2">
      {enabledIds.map((id) => (
        <li key={id} className="flex items-center justify-between gap-3">
          <div className="flex items-center gap-2">
            <StatusDot status={gatewayRunning ? "ok" : "missing"} />
            <span className="text-sm text-fg">{name(id)}</span>
            {!gatewayRunning && (
              <Badge tone="warning" variant="soft" className="text-2xs">
                {t("gateway.revertedToDirect")}
              </Badge>
            )}
          </div>
          <Link to="/agents/$id/routing" params={{ id }} className="font-mono text-2xs text-subtle hover:text-fg">
            {t("gateway.configure")} →
          </Link>
        </li>
      ))}
    </ul>
  );
}

/// Recent routed requests — the compact projection of the log surface. The
/// card header's icon button opens the full log viewer (`/gateway/logs`),
/// where the same traffic is visible with per-request correlation.
function ActivityCard() {
  const { t } = useTranslation();
  const statusQ = useQuery({ queryKey: qk.gatewayStatus(), queryFn: gatewayGetStatus });
  const activityQ = useQuery({ queryKey: qk.gatewayActivity(), queryFn: () => gatewayRecentActivity(8) });
  const stats = statusQ.data?.stats;
  return (
    <Card
      title={t("gateway.activityCard")}
      description={t("gateway.activityCardDesc")}
      action={
        <Link to="/gateway/logs">
          <Button size="sm" variant="ghost" aria-label={t("gateway.viewLogs")} title={t("gateway.viewLogs")}>
            <ScrollText data-icon size={14} />
          </Button>
        </Link>
      }
    >
      <div className="grid grid-cols-3 gap-2 text-2xs text-subtle">
        <Field label={t("gateway.totalRequests")} value={String(stats?.total_requests ?? 0)} />
        <Field label={t("gateway.lastRequest")} value={stats?.last_request_at ? fmtTime(stats.last_request_at) : "—"} />
        <Field label={t("gateway.activeTasks")} value={String(stats?.active_tasks ?? 0)} />
      </div>
      <div className="mt-3 border-t border-border pt-3">
        {activityQ.isLoading ? (
          <Skeleton className="h-8 w-full" />
        ) : (activityQ.data?.length ?? 0) === 0 ? (
          <div className="prose text-xs text-subtle">{t("gateway.noActivity")}</div>
        ) : (
        <div className="animate-in fade-in duration-fast">
          <ul className="space-y-1 font-mono text-2xs text-subtle">
            {activityQ.data!.map((r) => (
              <li key={r.request_id} className="flex items-center gap-2">
                <span>{fmtTime(r.started_at)}</span>
                <span className="text-fg">{r.resolved_model ?? r.requested_model ?? "?"}</span>
                <span>·</span>
                <span>{r.http_status ?? "…"}</span>
                <span>·</span>
                <span>{r.route_reason}</span>
              </li>
            ))}
          </ul>
        </div>
        )}
      </div>
    </Card>
  );
}

function Field({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="min-w-0">
      <div className="text-subtle">{label}</div>
      <div className={`truncate text-fg ${mono ? "font-mono" : ""}`}>{value}</div>
    </div>
  );
}

// ---- formatters (local; no time-locale dependency) --------------------------

function fmtUptime(secs: number): string {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

function fmtTime(ms: number): string {
  const d = new Date(ms);
  // HH:MM:SS — compact, locale-stable.
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}
