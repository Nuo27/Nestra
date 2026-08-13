import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { useNavigate } from "@tanstack/react-router";
import { sessionChildren, type Session } from "../../ipc";
import { qk } from "../../lib/queries";
import { formatRelative } from "../../lib/format";
import { providerMeta } from "../../lib/sessionsMeta";
import { StatusDot } from "../feedback/StatusDot";

// Recursive subagent tree: each node is a session row that lazily fetches
// its own grandchildren only when it has children.
export function SubagentTree({ s, depth }: { s: Session; depth: number }) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const meta = providerMeta(s.provider);
  // Only fetch grandchildren when this node has children of its own.
  const childrenQuery = useQuery({
    queryKey: qk.sessionChildren(s.provider, s.id),
    queryFn: () => sessionChildren(s.provider, s.id),
    enabled: s.child_count > 0,
  });
  const grandchildren = childrenQuery.data ?? [];
  const pad = depth === 0 ? "px-3" : depth === 1 ? "pl-7 pr-3" : "pl-11 pr-3";
  return (
    <li className="border-b border-border last:border-0">
      <div
        className={`flex cursor-pointer items-center gap-2 py-1.5 ${pad} transition-colors duration-fast hover:bg-surface`}
        onClick={() =>
          navigate({ to: "/sessions", search: { id: s.id, provider: s.provider } })
        }
      >
        <StatusDot color={meta.color} size={2} title={meta.label} />
        <span className="truncate text-sm text-muted">{s.title}</span>
        <span className="ml-auto shrink-0 text-xs tabular text-subtle">
          {t("sessions.msgCount", { n: s.message_count })}
        </span>
        <span className="shrink-0 text-xs text-subtle">{formatRelative(s.updated_at)}</span>
      </div>
      {grandchildren.length > 0 && (
        <ul>
          {grandchildren.map((c) => (
            <SubagentTree key={`${c.provider}:${c.id}`} s={c} depth={depth + 1} />
          ))}
        </ul>
      )}
    </li>
  );
}
