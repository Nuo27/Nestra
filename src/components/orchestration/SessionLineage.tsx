import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { sessionTasks, routeHistory, type TaskSummary } from "../../ipc/orchestration";
import { SectionLabel } from "../layout/PageHeader";
import { Badge } from "../ui/badge";
import { Skeleton } from "../ui/skeleton";
import { RouteLineage } from "./RouteLineage";

/// Tasks the orchestration gateway observed for this logical session (matched
/// on the agent-native session id), each expandable to its full route
/// lineage. Renders nothing when the gateway hasn't routed this session yet —
/// that's the honest empty state (the card simply doesn't appear).
/// Note: OpenCode Desktop / Pi sessions (OpenAI protocol) don't carry a
/// stable session id the gateway can rely on, so session-level lineage is
/// only available for Claude Code sessions.
export function SessionLineage({ sessionId }: { sessionId: string }) {
  const { t } = useTranslation();
  const tasksQ = useQuery({
    queryKey: ["orchestration", "session-tasks", sessionId],
    queryFn: () => sessionTasks(sessionId, 10),
  });
  const tasks = tasksQ.data ?? [];
  if (tasks.length === 0) return null;
  return (
    <div className="mt-4">
      <SectionLabel className="mb-1.5">
        {t("sessions.gatewayTasks", { count: tasks.length })}
      </SectionLabel>
      <div className="space-y-1.5">
        {tasks.map((task) => (
          <SessionTaskRow key={task.task_id} taskId={task.task_id} summary={task} />
        ))}
      </div>
    </div>
  );
}

function SessionTaskRow({
  taskId,
  summary,
}: {
  taskId: string;
  summary: TaskSummary;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const historyQ = useQuery({
    queryKey: ["orchestration", "task-history", taskId],
    queryFn: () => routeHistory(taskId),
    enabled: open,
  });
  return (
    <div className="rounded border border-border bg-inset/60">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs"
      >
        <span
          aria-hidden
          className="text-subtle transition-transform"
          style={{ transform: open ? "rotate(90deg)" : undefined }}
        >
          ▸
        </span>
        <span className="min-w-0 flex-1 truncate font-mono text-fg">
          {taskId.slice(0, 8)}
        </span>
        <span className="font-mono text-2xs text-subtle tabular">
          {t("orchestration.reqCount", { n: summary.request_count })}
        </span>
        {summary.generation_broken && (
          <Badge tone="danger" variant="soft" className="font-mono text-2xs">
            {t("orchestration.genBroken")}
          </Badge>
        )}
      </button>
      {open && (
        <div className="border-t border-border px-3 py-2">
          {historyQ.isLoading ? (
            <Skeleton className="h-8 w-full" />
          ) : (
            <RouteLineage records={historyQ.data ?? []} />
          )}
        </div>
      )}
    </div>
  );
}
