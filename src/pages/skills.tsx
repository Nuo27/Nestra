import { useTranslation } from "react-i18next";
import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { FolderOpen, RefreshCw, Trash2, Unlink } from "lucide-react";
import {
  skillsImportOne,
  skillsList,
  skillsUnmanage,
  skillsReveal,
  skillsToggle,
  skillsUninstall,
  type SkillMeta,
} from "../ipc";
import { useActiveAgents, useAgentLabels } from "../lib/agents";
import { Card } from "../components/controls/Card";
import { Button } from "../components/controls/Button";
import { Page } from "../components/layout/Page";
import { AgentToggleGroup } from "../components/controls/AgentToggleGroup";
import { Tabs } from "../components/controls/Tabs";
import { PageHeader } from "../components/layout/PageHeader";
import { SyncIndicator } from "../components/feedback/SyncIndicator";
import { qk } from "../lib/queries";
import { ErrorBanner } from "../components/feedback/ErrorBanner";
import { EmptyState } from "../components/feedback/EmptyState";
import { ListSkeletonCard } from "../components/feedback/ListSkeletonCard";
import { confirmDialog } from "../components/controls/ConfirmDialog";
import { ButtonGroup } from "../components/controls/ButtonGroup";
import { extractError } from "../ipc/errors";
import { useUI } from "../stores/ui";

type Filter = "managed" | "importable" | "builtin";

export function SkillsPage() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const q = useQuery({ queryKey: qk.skills(), queryFn: skillsList });
  const activeAgentsQ = useActiveAgents("supports_skills");
  const skills = q.data ?? [];
  const labelForAgent = useAgentLabels();
  const toast = useUI((s) => s.pushToast);
  const [filter, setFilter] = useState<Filter>("managed");

  // Agents that are connected, enabled, and support skills. Unified filter with
  // the same criteria as the MCP page (via useActiveAgents).
  const skillAgents = (activeAgentsQ.data ?? []).map((c) => c.id);

  const invalidate = () => qc.invalidateQueries({ queryKey: qk.skills() });

  const toggleMut = useMutation({
    mutationFn: (v: { id: string; agent: string; enabled: boolean }) =>
      skillsToggle(v.id, v.agent, v.enabled),
    onSuccess: (_d, vars) => {
      // `variables` only keeps the LAST call — with two rows in flight the
      // earlier one's pending state vanished and its row looked idle while
      // the mutation ran. A per-row Set fixes that.
      setToggling((cur) => {
        const next = new Set(cur);
        next.delete(`${vars.id}:${vars.agent}`);
        return next;
      });
      invalidate();
      toast(
        vars.enabled
          ? t("skills.enabledToast", { agent: labelForAgent(vars.agent) })
          : t("skills.disabledToast", { agent: labelForAgent(vars.agent) }),
        "success",
      );
    },
    onError: (e, vars) => {
      setToggling((cur) => {
        const next = new Set(cur);
        next.delete(`${vars.id}:${vars.agent}`);
        return next;
      });
      toast(t("skills.toggleFailed", { err: extractError(e) }), "error");
    },
  });

  const [toggling, setToggling] = useState<Set<string>>(new Set());

  const uninstallMut = useMutation({
    mutationFn: (id: string) => skillsUninstall(id),
    onSuccess: () => {
      invalidate();
      toast(t("skills.uninstalled"), "success");
    },
    onError: (e) => toast(t("skills.uninstallFailed", { err: extractError(e) }), "error"),
  });

  const importMut = useMutation({
    mutationFn: (v: { path: string; agent: string }) =>
      skillsImportOne(v.path, v.agent),
    onSuccess: () => {
      invalidate();
      toast(t("skills.imported"), "success");
    },
    onError: (e) => toast(t("skills.importFailed", { err: extractError(e) }), "error"),
  });

  const unmanageMut = useMutation({
    // "Restore" here = stop managing: drop the DB row + SSOT, leave the
    // agent-dir copies (keeps working, becomes Importable). The inverse of
    // import — distinct from uninstall, which also removes the copies. The
    // confirm dialog (on the button) tells the user this.
    mutationFn: (id: string) => skillsUnmanage(id),
    onSuccess: (_d, id) => {
      invalidate();
      const name = skills.find((s) => s.id === id)?.name ?? id;
      toast(t("skills.unmanaged", { name }), "success");
    },
    onError: (e) => toast(t("skills.unmanageFailed", { err: extractError(e) }), "error"),
  });

  let visible = skills.filter((s) => !s.managed && !s.builtin);
  if (filter === "managed") visible = skills.filter((s) => s.managed);
  else if (filter === "builtin") visible = skills.filter((s) => !s.managed && s.builtin);

  const emptyTitle = () => {
    if (filter === "importable") return t("skills.nothingToImport");
    const label = t(filter === "managed" ? "skills.tabManaged" : "skills.tabBuiltin");
    return t("skills.noFilterSkills", { filter: label });
  };
  const emptyHint = () => {
    if (filter === "importable") return t("skills.importableHint");
    if (filter === "builtin") return t("skills.builtinHint");
    return t("skills.managedHint");
  };

  const filters: { id: Filter; label: string }[] = [
    { id: "managed", label: t("skills.tabManaged") },
    { id: "importable", label: t("skills.tabImportable") },
    { id: "builtin", label: t("skills.tabBuiltin") },
  ];

  return (
    <Page>
      <PageHeader
        title={t("skills.title")}
        info={t("skills.help")}
        action={
          <>
            <SyncIndicator query={q} />
            {/* skills_list re-walks the agent skill dirs on every call, so a
                refetch IS a rescan — same anatomy as the gateway-logs refresh. */}
            <Button
              size="sm"
              variant="secondary"
              loading={q.isFetching}
              onClick={() => void q.refetch()}
            >
              {!q.isFetching && <RefreshCw data-icon size={13} />}
              {t("skills.rescan")}
            </Button>
          </>
        }
      />

      {q.isError && (
        <ErrorBanner onRetry={() => invalidate()}>
          {extractError(q.error)}
        </ErrorBanner>
      )}

      <Tabs
        size="sm"
        value={filter}
        onChange={(v) => setFilter(v as Filter)}
        items={filters.map((f) => ({ id: f.id, label: f.label }))}
      />

      {q.isLoading && <ListSkeletonCard />}

      {!q.isLoading && visible.length === 0 && (
        <EmptyState
          title={emptyTitle()}
          hint={emptyHint()}
        />
      )}

      {visible.length > 0 && (
        <Card
          title={t("skills.cardTitle", { count: visible.length })}
          description={t("skills.cardDesc")}
        >
          <ul className="divide-y divide-border">
            {visible.map((s) => (
              <SkillRow
                key={s.id}
                skill={s}
                skillAgents={skillAgents}
                labelForAgent={labelForAgent}
                pendingAgent={
                  // Per-row pending from the Set, not the last-mutation
                  // variables (which drops earlier in-flight rows).
                  (() => {
                    const hit = [...toggling].find((k) => k.startsWith(`${s.id}:`));
                    return hit ? hit.slice(s.id.length + 1) : null;
                  })()
                }
                onToggle={(agent, enabled) => {
                  setToggling((cur) => new Set(cur).add(`${s.id}:${agent}`));
                  toggleMut.mutate({ id: s.id, agent, enabled });
                }}
                onUninstall={() => {
                  confirmDialog({
                    title: t("skills.uninstallConfirmTitle", { name: s.name }),
                    body: t("skills.uninstallConfirmBody"),
                    confirmLabel: t("skills.uninstallConfirmLabel"),
                  }).then((ok) => ok && uninstallMut.mutate(s.id));
                }}
                onReveal={() => {
                  // A rejected reveal must not be an unhandled rejection
                  // with no user feedback.
                  skillsReveal(s.path).catch((e) =>
                    toast(t("skills.revealFailed", { err: extractError(e) ?? String(e) }), "error"),
                  );
                }}
                onImport={() => importMut.mutate({ path: s.path, agent: s.source })}
                onUnmanage={() => {
                  confirmDialog({
                    title: t("skills.unmanageConfirmTitle", { name: s.name }),
                    body: t("skills.unmanageConfirmBody"),
                    confirmLabel: t("skills.unmanageConfirmLabel"),
                    tone: "primary",
                  }).then((ok) => ok && unmanageMut.mutate(s.id));
                }}
              />
            ))}
          </ul>
        </Card>
      )}
    </Page>
  );
}

function SkillRow({
  skill,
  skillAgents,
  labelForAgent,
  onToggle,
  onUninstall,
  onReveal,
  onImport,
  onUnmanage,
  pendingAgent,
}: {
  skill: SkillMeta;
  skillAgents: string[];
  labelForAgent: (id: string) => string;
  onToggle: (agent: string, enabled: boolean) => void;
  onUninstall: () => void;
  onReveal: () => void;
  onImport: () => void;
  onUnmanage: () => void;
  pendingAgent: string | null;
}) {
  const { t } = useTranslation();
  const managed = skill.managed;
  const builtin = skill.builtin;
  return (
    <li className="flex flex-wrap items-center gap-3 py-2">
      <div
        className="min-w-0 flex-1 basis-40"
        title={skill.description ?? undefined}
      >
        <div className="truncate text-sm font-medium text-fg">{skill.name}</div>
      </div>
      <div className="flex shrink-0 flex-wrap items-center gap-2">
        {managed ? (
          skillAgents.length === 0 ? (
            <span className="prose text-xs italic text-subtle">
              {t("skills.noAgents")}
            </span>
          ) : (
            <AgentToggleGroup
              items={skillAgents.map((agent) => ({
                id: agent,
                label: labelForAgent(agent),
                checked: skill.enabled_agents.includes(agent),
                pending: pendingAgent === agent,
              }))}
              onToggle={(agent, enabled) => onToggle(agent, enabled)}
            />
          )
        ) : (
          <span className="text-xs text-subtle">
            {t("skills.sourceLabel", { agent: labelForAgent(skill.source) })}
            {builtin ? t("skills.bundledSuffix") : ""}
          </span>
        )}
        <ButtonGroup space="loose">
          <Button
            variant="ghost"
            size="sm"
            onClick={onReveal}
            title={t("skills.revealTitle")}
            aria-label={t("skills.revealTitle")}
          >
            <FolderOpen data-icon size={13} />
          </Button>
          {!managed && !builtin && (
            <Button
              variant="primary"
              size="sm"
              onClick={onImport}
              title={t("skills.importTitle")}
            >
              {t("skills.importBtn")}
            </Button>
          )}
          {managed && (
            <>
              <Button
                variant="ghost"
                size="sm"
                onClick={onUnmanage}
                title={t("skills.unmanageTitle")}
                aria-label={t("skills.unmanageTitle")}
              >
                <Unlink data-icon size={13} />
              </Button>
              <Button
                variant="danger"
                size="sm"
                onClick={onUninstall}
                title={t("skills.uninstallTitle")}
                aria-label={t("skills.uninstallTitle")}
              >
                <Trash2 data-icon size={13} />
              </Button>
            </>
          )}
        </ButtonGroup>
      </div>
    </li>
  );
}
