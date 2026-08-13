import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { memo } from "react";
import { sessionChildren, type Session } from "../../ipc";
import { qk } from "../../lib/queries";
import { formatRelative } from "../../lib/format";
import { providerMeta } from "../../lib/sessionsMeta";
import { Checkbox } from "../ui/checkbox";
import { StatusDot } from "../feedback/StatusDot";
import { Skeleton } from "../ui/skeleton";
import { Button } from "./Button";

function SessionRowImpl({
  s,
  checked,
  selected,
  expanded,
  onSelect,
  onToggleExpand,
  onToggleCheck,
  onSelectChild,
}: {
  s: Session;
  checked: boolean;
  selected: boolean;
  expanded: boolean;
  onSelect: () => void;
  onToggleExpand: () => void;
  onToggleCheck: (checked: boolean) => void;
  onSelectChild: (s: Session) => void;
}) {
  const { t } = useTranslation();
  const meta = providerMeta(s.provider);
  const childrenQuery = useQuery({
    queryKey: qk.sessionChildren(s.provider, s.id),
    queryFn: () => sessionChildren(s.provider, s.id),
    enabled: expanded && s.child_count > 0,
  });

  return (
    <li className="border-b border-border">
      <div
        className={
          "group relative flex cursor-pointer " +
          (selected
            ? "bg-raised before:absolute before:left-0 before:top-1.5 before:bottom-1.5 before:w-0.5 before:bg-accent"
            : "hover:bg-surface")
        }
        onClick={onSelect}
        title={s.project ? `${s.title} — ${s.project}` : s.title}
      >
        <div className="flex min-w-0 items-center gap-1.5 py-1.5 pl-2 pr-2">
          {/* `-m-1 p-1` grows the hit target beyond the 16px box without
              shifting layout; without it, clicks landing just outside the box
              fall through to the row's navigation onClick and the selection
              "won't take". */}
          <span
            className="-m-1 shrink-0 cursor-pointer p-1"
            onClick={(e) => e.stopPropagation()}
            title={t("sessions.selectForBatch")}
          >
            <Checkbox checked={checked} onCheckedChange={onToggleCheck} />
          </span>
          <StatusDot color={meta.color} size={2} title={meta.label} />
          <span className="min-w-0 flex-1 truncate text-sm text-fg">{s.title}</span>
          <span className="shrink-0 text-xs tabular text-subtle">
            {t("sessions.msgCount", { n: s.message_count })} · {formatRelative(s.updated_at)}
          </span>
          {s.child_count > 0 && (
            <Button
              size="xs"
              variant="ghost"
              className="shrink-0"
              onClick={(e) => {
                e.stopPropagation();
                onToggleExpand();
              }}
              title={t("sessions.subagentTitle", { count: s.child_count })}
            >
              {expanded ? "−" : `+${s.child_count}`}
            </Button>
          )}
        </div>
      </div>

      {expanded && s.child_count > 0 && (
        <ul className="border-t border-border bg-canvas">
          {childrenQuery.isLoading ? (
            <li className="px-3 py-1.5">
              <Skeleton className="h-3 w-1/2" />
            </li>
          ) : (
            (childrenQuery.data ?? []).map((c) => {
              const cm = providerMeta(c.provider);
              return (
                <li
                  key={c.id}
                  className="flex cursor-pointer items-center gap-2 px-3 py-1.5 pl-6 transition-colors duration-fast hover:bg-surface"
                  onClick={() => onSelectChild(c)}
                >
                  <StatusDot color={cm.color} size={1.5} title={cm.label} />
                  <span className="truncate text-xs text-muted">{c.title}</span>
                  <span className="ml-auto shrink-0 text-xs tabular text-subtle">
                    {t("sessions.msgCount", { n: c.message_count })}
                  </span>
                </li>
              );
            })
          )}
        </ul>
      )}
    </li>
  );
}

/// Memoized SessionRow — skips re-render when the Session object reference +
/// UI state (checked/selected/expanded) are unchanged. With `keepPreviousData`,
/// a filter refetch produces new Session objects only for the new filter; the
/// previous filter's rows keep their references and skip re-render entirely.
export const SessionRow = memo(SessionRowImpl, (prev, next) =>
  prev.s === next.s &&
  prev.checked === next.checked &&
  prev.selected === next.selected &&
  prev.expanded === next.expanded,
);

export function ListSkeleton() {
  return (
    <ul>
      {Array.from({ length: 8 }).map((_, i) => (
        <li key={i} className="border-b border-border px-3 py-1.5">
          <Skeleton className="h-3 w-2/3" />
        </li>
      ))}
    </ul>
  );
}
