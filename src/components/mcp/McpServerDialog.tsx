import { useEffect, useState } from "react";
import { useMutation } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { mcpProbeDraft, mcpSave, type McpKind, type McpServer, type ProbeResult } from "../../ipc";
import { extractError } from "../../ipc/errors";
import { splitArgs, quoteArg } from "../../lib/mcp";
import type { AgentState } from "../controls/AgentToggleGroup";
import { AgentStateGroup } from "../controls/AgentToggleGroup";
import { EnvEditor } from "../controls/EnvEditor";
import { Tabs } from "../controls/Tabs";
import { Field } from "../controls/Field";
import { Button } from "../controls/Button";
import { Input } from "../ui/input";
import { ErrorBanner } from "../feedback/ErrorBanner";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "../ui/dialog";

export function McpServerDialog({
  mode,
  initial,
  agents,
  labelForAgent,
  onCancel,
  onDone,
}: {
  mode: "add" | "edit";
  initial?: McpServer;
  /// Connected MCP-capable agents to offer, with the tri-state capability
  /// (`tri`: the agent's format carries a per-server `enabled` field).
  agents: { id: string; tri: boolean }[];
  labelForAgent: (id: string) => string;
  onCancel: () => void;
  onDone: () => void;
}) {
  const { t } = useTranslation();
  const [name, setName] = useState(initial?.name ?? "");
  const [kind, setKind] = useState<McpKind>(initial?.transport.kind ?? "stdio");
  const [command, setCommand] = useState(initial?.transport.command ?? "");
  // Args pre-fill must quote whitespace-containing args — submit re-splits
  // with splitArgs, so a bare join(" ") would corrupt `["--config","a b"]`
  // into three args on a no-op open→save round trip.
  const [args, setArgs] = useState((initial?.transport.args ?? []).map(quoteArg).join(" "));
  const [url, setUrl] = useState(initial?.transport.url ?? "");
  const [env, setEnv] = useState<Record<string, string>>(initial?.transport.env ?? {});
  // Per-agent overrides keyed by agent id. Initialized from the server (edit)
  // or empty (add). Only meaningful for agents the server is written on.
  const [overrides, setOverrides] = useState<Record<string, Record<string, string>>>(
    initial?.env_overrides ?? {},
  );
  // Per-agent tri-state: absent / disabled (written, flag off) / enabled.
  const [agentStates, setAgentStates] = useState<Record<string, AgentState>>(() => {
    const out: Record<string, AgentState> = {};
    for (const a of agents) {
      out[a.id] = initial?.enabled_agents.includes(a.id)
        ? "enabled"
        : initial?.disabled_agents.includes(a.id)
          ? "disabled"
          : "absent";
    }
    return out;
  });
  const [error, setError] = useState<string | null>(null);

  const saveMut = useMutation({
    mutationFn: (s: McpServer) => mcpSave(s),
    onSuccess: onDone,
    onError: (e) => setError(extractError(e)),
  });

  // Save-time transport draft, shared by submit + the pre-save probe. Empty
  // env keys are transient editor rows — never persist them.
  const dropEmptyKeys = (env: Record<string, string>) =>
    Object.fromEntries(Object.entries(env).filter(([k]) => k.trim() !== ""));
  const draftTransport = () => ({
    kind,
    command: kind === "stdio" ? command.trim() || null : null,
    args: kind === "stdio" ? splitArgs(args) : [],
    env: dropEmptyKeys(env),
    url: kind === "stdio" ? null : url.trim() || null,
  });

  // Pre-save connectivity probe on the DRAFT transport: verify before
  // committing. A failed probe is a result (shown inline), never a dialog
  // error — saving anyway stays allowed.
  const [probe, setProbe] = useState<ProbeResult | null>(null);
  const probeMut = useMutation({
    mutationFn: () => mcpProbeDraft(draftTransport()),
    onSuccess: (r) => setProbe(r),
    onError: (e) =>
      setProbe({ ok: false, latency_ms: null, reason: extractError(e) ?? String(e) }),
  });
  // Editing any transport field invalidates the last probe result — a stale
  // "reachable" verdict must not vouch for a config the user just changed.
  useEffect(() => {
    setProbe(null);
  }, [kind, command, args, url, env]);

  const submit = () => {
    if (!name.trim()) {
      setError(t("mcp.needsName"));
      return;
    }
    const server: McpServer = {
      // The backend canonicalizes the id from the name (slugify); pass the
      // existing id in edit mode (so a suffixed collision row edits in place)
      // and empty in add mode.
      id: initial?.id ?? "",
      name: name.trim(),
      transport: draftTransport(),
      enabled_agents: agents
        .filter((a) => agentStates[a.id] === "enabled")
        .map((a) => a.id),
      disabled_agents: agents
        .filter((a) => agentStates[a.id] === "disabled")
        .map((a) => a.id),
      managed: true,
      env_overrides: Object.fromEntries(
        Object.entries(overrides).map(([id, env]) => [id, dropEmptyKeys(env)]),
      ),
    };
    saveMut.mutate(server);
  };

  return (
    <Dialog
      open
      // Block Esc / overlay / X while a save is in flight: unmounting the
      // dialog drops the mutation's onSuccess (invalidate + onDone), leaving
      // the list stale after the backend actually saved.
      onOpenChange={(o) => {
        if (!o && saveMut.isPending) return;
        if (!o) onCancel();
      }}
    >
      <DialogContent size="md">
        <DialogHeader>
          <DialogTitle>
            {mode === "add" ? t("mcp.dialogAddTitle") : t("mcp.dialogEditTitle", { name: initial?.name })}
          </DialogTitle>
          <DialogDescription>
            {t("mcp.dialogDesc")}
          </DialogDescription>
        </DialogHeader>
        <DialogBody className="space-y-4">
          <Field label={t("mcp.name")}>
            <Input
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={t("mcp.namePlaceholder")}
              autoFocus
            />
          </Field>
          <Field label={t("mcp.transport")}>
            <Tabs
              size="sm"
              value={kind}
              onChange={(v) => setKind(v as McpKind)}
              items={[
                { id: "stdio", label: "stdio" },
                { id: "http", label: "http" },
                { id: "sse", label: "sse" },
              ]}
            />
          </Field>
          {kind === "stdio" ? (
            <>
              <Field label={t("mcp.command")}>
                <Input
                  value={command}
                  onChange={(e) => setCommand(e.target.value)}
                  placeholder={t("mcp.commandPlaceholder")}
                />
              </Field>
              <Field label={t("mcp.args")} hint={t("mcp.argsHint")}>
                <Input
                  value={args}
                  onChange={(e) => setArgs(e.target.value)}
                  placeholder={t("mcp.argsPlaceholder")}
                />
              </Field>
              <EnvEditor title={t("mcp.envGlobal")} pairs={env} onChange={setEnv} />
            </>
          ) : (
            <Field label={t("mcp.url")}>
              <Input
                value={url}
                onChange={(e) => setUrl(e.target.value)}
                placeholder={t("mcp.urlPlaceholder")}
              />
            </Field>
          )}
          {agents.length > 0 && (
            <Field label={t("mcp.writeTo")}>
              <div className="space-y-2">
                <AgentStateGroup
                  items={agents.map((a) => ({
                    id: a.id,
                    label: labelForAgent(a.id),
                    state: agentStates[a.id] ?? "absent",
                    tri: a.tri,
                  }))}
                  onSetState={(id, state) =>
                    setAgentStates((cur) => ({ ...cur, [id]: state }))
                  }
                />
                {kind === "stdio" &&
                  agents
                    .filter((a) => agentStates[a.id] === "enabled")
                    .map((a) => (
                      <EnvEditor
                        key={a.id}
                        title={t("mcp.envOverride", { agent: labelForAgent(a.id) })}
                        pairs={overrides[a.id] ?? {}}
                        onChange={(next) => setOverrides((cur) => ({ ...cur, [a.id]: next }))}
                      />
                    ))}
              </div>
            </Field>
          )}
          {probe && (
            <div
              className={`font-mono text-2xs ${probe.ok ? "text-success" : "text-danger"}`}
            >
              {probe.ok
                ? t("mcp.probeOk", { ms: probe.latency_ms ?? "—" })
                : t("mcp.probeFail", { reason: probe.reason ?? "—" })}
            </div>
          )}
          {error && <ErrorBanner severity="warn">{error}</ErrorBanner>}
        </DialogBody>
        <DialogFooter>
          <Button
            variant="ghost"
            loading={probeMut.isPending}
            disabled={saveMut.isPending}
            onClick={() => probeMut.mutate()}
            title={t("mcp.testDraftTip")}
          >
            {t("mcp.testDraft")}
          </Button>
          <Button variant="ghost" onClick={onCancel} disabled={saveMut.isPending}>{t("common.cancel")}</Button>
          <Button
            variant="primary"
            loading={saveMut.isPending}
            onClick={submit}
          >
            {mode === "add" ? t("mcp.addServer") : t("common.save")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
