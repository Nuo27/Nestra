import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { useNavigate, useSearch } from "@tanstack/react-router";
import { useEffect, useMemo, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { agentList, sessionList, sessionRefresh } from "../ipc";
import { extractError } from "../ipc/errors";
import { useUI } from "../stores/ui";
import { EmptyState } from "../components/feedback/EmptyState";
import { ErrorBanner } from "../components/feedback/ErrorBanner";
import { SectionLabel } from "../components/layout/PageHeader";
import { ResizeHandle } from "../components/layout/ResizeHandle";
import { ListSkeleton, SessionRow } from "../components/controls/SessionRow";
import { SessionsListToolbar } from "../components/controls/SessionsListToolbar";
import { SessionDetail } from "../components/display/SessionDetail";
import { useSessionSelection } from "../lib/sessionSelection";
import { qk } from "../lib/queries";
import { bucketByDate } from "../lib/sessionsBucket";
import { AGENT_TO_PROVIDER } from "../lib/sessionsMeta";

// Narrowest the session list may be. Wide enough to hold the toolbar row in
// full (agents filter + selection cluster: count, all/none, reveal, delete,
// clear) so the action icons never overflow the panel. Applied both as the
// ResizeHandle drag clamp and as a render-time floor (persisted widths from
// an older session can otherwise arrive narrower than this).
// The agents filter shrinks to content (w-fit) now, so the binding term is the
// selection cluster (256) + padding; 400 still leaves room for a long agent
// name in the filter + row content.
const SESSIONS_LIST_MIN_WIDTH = 400;

export function SessionsPage() {
  const { t } = useTranslation();
  const search = useSearch({ from: "/sessions" }) as {
    id?: string;
    provider?: string;
  };
  const navigate = useNavigate();
  const selectedId = search.id;
  const selectedProvider = search.provider;

  const [provider, setProvider] = useState<string>("");
  const [query, setQuery] = useState("");
  const [expanded, setExpanded] = useState<Set<string>>(new Set());

  const qc = useQueryClient();
  const toast = useUI((s) => s.pushToast);

  // Listen for the background first-launch reconcile — it ran on a separate
  // connection, so the cached session queries are stale until this fires.
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    // `listen` is async — assigning the cleanup AFTER it resolves races an
    // early unmount (the listener would leak). The cancelled flag makes the
    // resolution a no-op on unmount; errors are logged, not thrown.
    listen("sessions-reconciled", () => {
      qc.invalidateQueries({ queryKey: ["sessions"] });
    })
      .then((fn) => {
        if (cancelled) {
          fn(); // resolved after unmount — release immediately
        } else {
          unlisten = fn;
        }
      })
      .catch((e) => console.error("sessions-reconciled listen failed", e));
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [qc]);
  const sessionsListWidth = useUI((s) => s.sessionsListWidth);
  const setSessionsListWidth = useUI((s) => s.setSessionsListWidth);

  // Dropdown is driven by connected agents (status ok), mapped to the
  // session-provider id they log under. Only providers with a resumable agent
  // appear — no all-providers list.
  const agentQ = useQuery({ queryKey: qk.agents(), queryFn: agentList });
  const connectedProviders = useMemo(() => {
    const set: Map<string, string> = new Map(); // providerId -> label
    for (const c of agentQ.data ?? []) {
      if (c.status === "ok") {
        const providerId = AGENT_TO_PROVIDER[c.id];
        if (providerId) set.set(providerId, c.display_name);
      }
    }
    return set;
  }, [agentQ.data]);

  const listQuery = useQuery({
    queryKey: qk.sessions(provider, query),
    queryFn: () => sessionList(provider || undefined, query || undefined, 300),
    placeholderData: keepPreviousData,
  });

  const sessions = listQuery.data ?? [];
  const buckets = useMemo(() => bucketByDate(sessions), [sessions]);
  const selection = useSessionSelection(sessions);

  const refreshMut = useMutation({
    mutationFn: sessionRefresh,
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.sessions() });
      selection.clearSelection();
      toast(t("sessions.rescanToast"), "success");
    },
    onError: (e) =>
      toast(t("sessions.rescanFailed", { err: extractError(e) ?? String(e) }), "error"),
  });

  const select = (s: (typeof sessions)[number]) =>
    navigate({ to: "/sessions", search: { id: s.id, provider: s.provider } });

  const toggleExpand = (s: (typeof sessions)[number]) => {
    // Composite key: two providers can share a session id (e.g. "default")
    // and a bare id would link their expand states together.
    const k = `${s.provider}:${s.id}`;
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(k)) next.delete(k);
      else next.add(k);
      return next;
    });
  };

  return (
    <div className="flex h-full min-h-0">
      {/* ---- list (master) ---- */}
      <aside
        className="relative flex shrink-0 flex-col border-r border-border"
        style={{ width: Math.max(sessionsListWidth, SESSIONS_LIST_MIN_WIDTH) }}
      >
        <ResizeHandle
          width={sessionsListWidth}
          onResize={setSessionsListWidth}
          min={SESSIONS_LIST_MIN_WIDTH}
        />
        <SessionsListToolbar
          listQuery={listQuery}
          refreshMut={refreshMut}
          query={query}
          onQueryChange={setQuery}
          provider={provider}
          onProviderChange={setProvider}
          connectedProviders={connectedProviders}
          selection={selection}
          total={sessions.length}
        />

        {selection.bulkErr && (
          <ErrorBanner
            severity="error"
            variant="strip"
            onDismiss={selection.clearBulkErr}
          >
            {selection.bulkErr}
          </ErrorBanner>
        )}

        <div className="scroll min-h-0 flex-1 overflow-y-auto">
          {listQuery.isLoading ? (
            <ListSkeleton />
          ) : listQuery.error ? (
            <div className="px-3 py-4">
              <ErrorBanner>
                {(listQuery.error as Error)?.message ?? t("sessions.loadFailed")}
              </ErrorBanner>
            </div>
          ) : sessions.length === 0 ? (
            <div className="p-3">
              <EmptyState
                title={query ? t("sessions.noMatches") : t("sessions.noneFound")}
                hint={t("sessions.noneFoundHint")}
              />
            </div>
          ) : (
            buckets.map((b) => (
              <div key={b.key}>
                <SectionLabel className="sticky top-0 z-10 bg-canvas px-3 py-1">
                  {t(b.labelKey)}
                </SectionLabel>
                <ul>
                  {b.sessions.map((s) => (
                    <SessionRow
                      key={`${s.provider}:${s.id}`}
                      s={s}
                      checked={selection.liveSelected.has(selection.keyOf(s))}
                      selected={s.id === selectedId && s.provider === selectedProvider}
                      expanded={expanded.has(selection.keyOf(s))}
                      onSelect={() => select(s)}
                      onToggleExpand={() => toggleExpand(s)}
                      onToggleCheck={(checked) => selection.toggleOne(s, checked)}
                      onSelectChild={select}
                    />
                  ))}
                </ul>
              </div>
            ))
          )}
        </div>
      </aside>

      {/* ---- detail ---- */}
      <section className="min-h-0 min-w-0 flex-1 overflow-y-auto">
        {selectedId && selectedProvider ? (
          <SessionDetail id={selectedId} provider={selectedProvider} />
        ) : (
          <div className="prose flex h-full items-center justify-center p-6 text-sm text-subtle">
            {t("sessions.emptyDetail")}
          </div>
        )}
      </section>
    </div>
  );
}
