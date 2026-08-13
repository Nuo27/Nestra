import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { useNavigate } from "@tanstack/react-router";
import { ExternalLink, FolderOpen, Trash2, Copy, Check } from "lucide-react";
import {
  sessionChildren,
  sessionDelete,
  sessionGet,
  sessionOpen,
  sessionRead,
  sessionReveal,
  type Session,
} from "../../ipc";
import { extractError } from "../../ipc/errors";
import { formatRelative } from "../../lib/format";
import { providerMeta } from "../../lib/sessionsMeta";
import { groupRenderItems } from "../../lib/sessionMessages";
import { invalidate, qk } from "../../lib/queries";
import { Card } from "../controls/Card";
import { Button } from "../controls/Button";
import { ButtonGroup } from "../controls/ButtonGroup";
import { confirmDialog } from "../controls/ConfirmDialog";
import { SectionLabel } from "../layout/PageHeader";
import { StatusDot } from "../feedback/StatusDot";
import { ErrorBanner } from "../feedback/ErrorBanner";
import { Skeleton } from "../ui/skeleton";
import { Tip } from "../ui/tooltip";
import { useUI } from "../../stores/ui";
import { useCopy } from "../../lib/useCopy";
import { SessionLineage } from "../orchestration/SessionLineage";
import { SubagentTree } from "./SubagentTree";
import { SessionMessageRows } from "./SessionMessageRows";

const PAGE = 100;

export function SessionDetail({ id, provider }: { id: string; provider: string }) {
  const { t } = useTranslation();
  const [shown, setShown] = useState(PAGE);
  const [actionErr, setActionErr] = useState<string | null>(null);
  const [copiedResume, copyResume] = useCopy();
  const [copiedPath, copyPath] = useCopy();
  const toast = useUI((s) => s.pushToast);
  const navigate = useNavigate();

  const meta = providerMeta(provider);
  const sessionQuery = useQuery({
    queryKey: qk.session(provider, id),
    queryFn: () => sessionGet(provider, id),
  });
  const messagesQuery = useQuery({
    queryKey: qk.sessionMessages(provider, id, shown),
    queryFn: () => sessionRead(provider, id, 0, shown),
  });

  const qc = useQueryClient();

  const handleOpen = async () => {
    setActionErr(null);
    try {
      await sessionOpen(provider, id);
    } catch (err) {
      setActionErr(extractError(err) ?? t("sessions.failedToOpen"));
    }
  };
  const handleReveal = async () => {
    setActionErr(null);
    try {
      await sessionReveal(provider, id);
    } catch (err) {
      setActionErr(extractError(err) ?? t("sessions.failedToReveal"));
    }
  };
  const handleDelete = async () => {
    setActionErr(null);
    const ok = await confirmDialog({
      title: t("sessions.deleteDetailTitle"),
      body: t("sessions.deleteDetailBody"),
      confirmLabel: t("common.delete"),
    });
    if (!ok) return;
    // Optimistic: pull the row out of every sessions list cache and clear the
    // detail caches immediately, then drop the detail pane — the row vanishes
    // the instant the user confirms, not after the backend finishes.
    qc.setQueriesData<Session[]>({ queryKey: ["sessions"] }, (old) =>
      Array.isArray(old)
        ? old.filter((s) => !(s.id === id && s.provider === provider))
        : old,
    );
    qc.removeQueries({ queryKey: ["session", provider, id] });
    navigate({ to: "/sessions", search: { id: undefined, provider: undefined } });
    // The actual deletion runs in the background; a failure rolls the row back
    // via the refetch that invalidate triggers.
    try {
      const result = await sessionDelete(provider, id);
      toast(t("sessions.deleted"), "success");
      // A non-empty removed_files list is the normal business result — log
      // at debug (was info, which noise-flooded every delete with files).
      if (result.removed_files.length > 0) {
        console.debug("Deleted session files:", result.removed_files);
      }
    } catch (err) {
      toast(extractError(err) ?? t("sessions.failedToDelete"), "error");
    }
    invalidate(qc, "session");
  };

  const session = sessionQuery.data ?? null;
  const window = messagesQuery.data;
  const messages = window?.messages ?? [];
  const total = window?.total ?? session?.message_count ?? 0;
  const children = useQuery({
    queryKey: qk.sessionChildren(provider, id),
    queryFn: () => sessionChildren(provider, id),
    enabled: (session?.child_count ?? 0) > 0,
  });

  return (
    <div className="w-full p-4">
      {/* header card */}
      <Card padding="md">
        <div className="flex items-center gap-2">
          <StatusDot color={meta.color} size={2.5} title={meta.label} />
          <SectionLabel inline>
            {meta.label}
            {session?.is_subagent ? t("sessions.subagentSuffix") : ""}
          </SectionLabel>
        </div>
        <h1 className="mt-1 break-words text-lg font-medium tracking-[-0.01em] text-fg">
          {session?.title ?? id}
        </h1>
        <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-subtle">
          {session?.project && <span>{session.project}</span>}
          {session?.cwd && <span className="min-w-0 max-w-[50%] truncate font-mono">· {session.cwd}</span>}
          <span className="tabular">· {t("sessions.messagesCount", { n: total })}</span>
          {session && <span>· {t("sessions.updatedAt", { rel: formatRelative(session.updated_at) })}</span>}
        </div>
        <ButtonGroup className="mt-3" justify="end" space="loose" wrap>
          {/* Icon-only compact cluster (DESIGN.md §4 rule 2): the label lives
              in the Tip, never as a second text element. */}
          <Tip
            content={
              session?.resume_command
                ? t("sessions.openTipCmd", { cwd: session.cwd ?? ".", cmd: session.resume_command })
                : t("sessions.openTip")
            }
          >
            <Button size="sm" variant="ghost" onClick={handleOpen} aria-label={t("sessions.openAria")}>
              <ExternalLink data-icon size={12} />
            </Button>
          </Tip>
          <Tip content={t("sessions.revealTip")}>
            <Button size="sm" variant="ghost" onClick={handleReveal} aria-label={t("sessions.revealSourceAria")}>
              <FolderOpen data-icon size={12} />
            </Button>
          </Tip>
          {session?.resume_command && (
            <Tip content={t("sessions.copyResumeTip", { cmd: session.resume_command })}>
              <Button
                size="sm"
                variant="ghost"
                onClick={() => copyResume(session.resume_command!)}
                aria-label={copiedResume ? t("sessions.copyResumeCopied") : t("sessions.copyResume")}
              >
                {copiedResume ? <Check data-icon size={12} /> : <Copy data-icon size={12} />}
              </Button>
            </Tip>
          )}
          {session && (
            <Tip content={t("sessions.copyPathTip", { path: session.source_path })}>
              <Button
                size="sm"
                variant="ghost"
                onClick={() => copyPath(session.source_path)}
                aria-label={copiedPath ? t("sessions.pathCopied") : t("sessions.copyPath")}
              >
                {copiedPath ? <Check data-icon size={12} /> : <Copy data-icon size={12} />}
              </Button>
            </Tip>
          )}
          <Tip content={t("sessions.deleteTip")}>
            <Button size="sm" variant="danger" onClick={handleDelete} aria-label={t("sessions.deleteAria")}>
              <Trash2 data-icon size={12} />
            </Button>
          </Tip>
          {actionErr && (
            <ErrorBanner
              variant="bare"
              className="basis-full"
              onDismiss={() => setActionErr(null)}
            >
              {actionErr}
            </ErrorBanner>
          )}
        </ButtonGroup>
      </Card>

      {/* gateway task lineage: tasks the orchestrator observed for this
          logical session, with their route history. Only renders when the
          gateway has actually routed traffic for this session. */}
      <SessionLineage sessionId={id} />

      {/* subagents */}
      {children.data && children.data.length > 0 && (
        <div className="mt-4">
          <SectionLabel className="mb-1.5">{t("sessions.subagentsTitle", { count: children.data.length })}</SectionLabel>
          <ul className="border border-border">
            {children.data.map((c) => (
              <SubagentTree key={`${c.provider}:${c.id}`} s={c} depth={0} />
            ))}
          </ul>
        </div>
      )}

      {/* messages */}
      <div className="mt-4 space-y-2">
        {messagesQuery.isLoading ? (
          <div className="space-y-2">
            {Array.from({ length: 4 }).map((_, i) => (
              <Skeleton key={i} className="h-16 w-full" />
            ))}
          </div>
        ) : messagesQuery.error ? (
          <div className="py-4">
            <ErrorBanner>
              {(messagesQuery.error as Error)?.message ?? t("sessions.messagesFailed")}
            </ErrorBanner>
          </div>
        ) : messages.length === 0 ? (
          <div className="py-4 text-center text-sm text-subtle">
            {t("sessions.noMessages")}
          </div>
        ) : (
          <>
            <SessionMessageRows items={groupRenderItems(messages)} />
            {messages.length < total && (
              <Button
                variant="ghost"
                className="w-full"
                onClick={() => setShown((n) => n + PAGE)}
              >
                {t("sessions.loadMore", { n: total - messages.length })}
              </Button>
            )}
          </>
        )}
      </div>
    </div>
  );
}
