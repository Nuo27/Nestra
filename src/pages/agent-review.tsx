import { useEffect, useMemo, useState } from "react";
import { useSearch } from "@tanstack/react-router";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { ShieldCheck, Play, Square, RefreshCw } from "lucide-react";

import { SectionLabel } from "../components/layout/PageHeader";
import { Card } from "../components/controls/Card";
import { Button } from "../components/controls/Button";
import { ButtonGroup } from "../components/controls/ButtonGroup";
import { AgentPageFrame } from "../components/agents/AgentPageFrame";
import { RoutedGate } from "../components/orchestration/RoutedGate";
import { ErrorBanner } from "../components/feedback/ErrorBanner";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "../components/ui/select";
import { Skeleton } from "../components/ui/skeleton";
import { Tip } from "../components/ui/tooltip";
import {
  reviewAbort,
  reviewCreate,
  reviewGet,
  reviewList,
  reviewStart,
  sessionList,
  type ReviewInfo,
} from "../ipc";
import { extractError } from "../ipc/errors";
import { invalidate, qk } from "../lib/queries";
import { formatRelative } from "../lib/format";
import { useUI } from "../stores/ui";

/// /agents/$id/review — Review Runtime (Pi-only): spawn an isolated review
/// session on the reviewed work, watch its live RPC event stream, read the
/// verdict. Gated by RoutedGate (the review routes through the gateway alias).
export function AgentReviewPage({ id }: { id: string }) {
  const { t } = useTranslation();
  const search = useSearch({ from: "/agents/$id/review" }) as { session?: string };

  return (
    <AgentPageFrame
      agentId={id}
      backTo="detail"
      titleSuffix={t("agentReview.titleSuffix")}
    >
      {(agent) => (
        <RoutedGate
          agentId={agent.id}
          supportsGateway={agent.capability.supports_gateway}
          title={t("agentRouting.gateTitle")}
          hint={t("agentRouting.gateHint")}
        >
          <ReviewBody preselectSession={search.session} />
        </RoutedGate>
      )}
    </AgentPageFrame>
  );
}

/// Status → translation key (translated at render per the i18n pattern).
const STATUS_KEYS: Record<string, string> = {
  pending: "agentReview.statusPending",
  reviewing: "agentReview.statusReviewing",
  verdict: "agentReview.statusVerdict",
  failed: "agentReview.statusFailed",
  aborted: "agentReview.statusAborted",
};

function ReviewBody({ preselectSession }: { preselectSession?: string }) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useUI((s) => s.pushToast);
  const [selected, setSelected] = useState<string | null>(null);

  const sessionsQ = useQuery({
    queryKey: qk.sessions("pi-cli", ""),
    queryFn: () => sessionList("pi-cli", undefined, 100),
  });
  const reviewsQ = useQuery({
    queryKey: qk.reviews(),
    queryFn: reviewList,
    // Refresh while a review is in flight (live events also arrive via the
    // Tauri event stream — this covers the gap).
    refetchInterval: (q) =>
      (q.state.data ?? []).some((r) => r.status === "reviewing") ? 2000 : false,
  });
  const reviewQ = useQuery({
    queryKey: qk.review(selected ?? ""),
    queryFn: () => reviewGet(selected!),
    enabled: selected != null,
    refetchInterval: (q) => (q.state.data?.status === "reviewing" ? 1500 : false),
  });
  const review = reviewQ.data ?? null;

  // Live events for the selected running review.
  const [events, setEvents] = useState<unknown[]>([]);
  useEffect(() => {
    setEvents([]);
    if (!selected) return;
    // Unsubscribing is async: an event that arrives between cleanup and the
    // unlisten resolution must not touch the (already switched) state.
    let disposed = false;
    const regs = [
      listen<unknown>(`review:${selected}:event`, (e) => {
        if (!disposed) setEvents((prev) => [...prev.slice(-200), e.payload]);
      }),
      listen(`review:${selected}:done`, () => {
        if (!disposed) invalidate(qc, "review");
      }),
    ];
    return () => {
      disposed = true;
      void Promise.all(regs).then((uns) => uns.forEach((u) => u()));
    };
  }, [selected, qc]);

  const startMutation = useMutation({
    mutationFn: async (sessionId: string) => {
      const created = await reviewCreate("pi-cli", sessionId);
      return reviewStart(created.id);
    },
    onSuccess: (info) => {
      toast(t("agentReview.started"), "success");
      setSelected(info.id);
      invalidate(qc, "review");
    },
    onError: (e) => toast(extractError(e) ?? t("agentReview.startFailed"), "error"),
  });

  const abortMutation = useMutation({
    mutationFn: (rid: string) => reviewAbort(rid),
    onSuccess: () => {
      toast(t("agentReview.aborted"), "success");
      invalidate(qc, "review");
    },
    onError: (e) => toast(extractError(e) ?? t("agentReview.abortFailed"), "error"),
  });

  const sessions = useMemo(
    () =>
      (sessionsQ.data ?? [])
        .filter((s) => !s.is_subagent)
        .sort((a, b) => b.updated_at - a.updated_at),
    [sessionsQ.data],
  );
  const [picked, setPicked] = useState<string>(preselectSession ?? "");
  const busy = startMutation.isPending;

  return (
    <div className="space-y-3">
      <div className="grid grid-cols-1 gap-3 lg:grid-cols-2">
        {/* New review */}
        <Card padding="md">
          <SectionLabel className="mb-2">{t("agentReview.newTitle")}</SectionLabel>
          <Select
            value={picked}
            onValueChange={setPicked}
            disabled={busy}
          >
            <SelectTrigger size="md" className="w-full">
              <SelectValue placeholder={t("agentReview.pickSession")} />
            </SelectTrigger>
            <SelectContent>
              {sessions.map((s) => (
                <SelectItem key={s.id} value={s.id}>
                  {s.title} · {formatRelative(s.updated_at)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <ButtonGroup className="mt-3" justify="start">
            <Button
              variant="primary"
              size="sm"
              loading={busy}
              onClick={() => {
                if (picked) startMutation.mutate(picked);
              }}
            >
              <Play data-icon size={12} />
              {t("agentReview.startButton")}
            </Button>
          </ButtonGroup>
          {startMutation.isError && (
            <ErrorBanner className="mt-2" onDismiss={() => startMutation.reset()}>
              {extractError(startMutation.error) ?? t("agentReview.startFailed")}
            </ErrorBanner>
          )}
          <p className="mt-2 text-2xs text-subtle">{t("agentReview.newHint")}</p>
        </Card>

        {/* Runner: live events + verdict for the selected review */}
        {review && (
          <Card padding="md">
            <div className="flex items-center justify-between gap-2">
              <SectionLabel>
                {t("agentReview.runnerTitle")} ·{" "}
                {t(STATUS_KEYS[review.status] ?? "agentReview.statusPending")}
              </SectionLabel>
              {review.status === "reviewing" && (
                <Tip content={t("agentReview.abortTip")}>
                  <Button
                    size="sm"
                    variant="danger"
                    loading={abortMutation.isPending}
                    onClick={() => abortMutation.mutate(review.id)}
                    aria-label={t("agentReview.abortTip")}
                  >
                    <Square data-icon size={12} />
                  </Button>
                </Tip>
              )}
            </div>
            {review.verdict_summary && (
              <div className="mt-2 border border-border bg-inset px-2 py-1.5 text-sm text-fg">
                <span className="mr-1.5 font-medium">
                  {review.verdict_status ? review.verdict_status.toUpperCase() : "—"}
                </span>
                {review.verdict_summary}
              </div>
            )}
            {(events.length > 0 || (review.live_events?.length ?? 0) > 0) && (
              <pre className="mt-2 max-h-64 overflow-auto border border-border bg-inset p-2 font-mono text-2xs leading-relaxed text-subtle">
                {(events.length > 0 ? events : (review.live_events ?? []))
                  .map((e) => JSON.stringify(e))
                  .join("\n")}
              </pre>
            )}
            {review.status === "reviewing" && events.length === 0 && (
              <p className="mt-2 text-2xs text-subtle">{t("agentReview.waiting")}</p>
            )}
          </Card>
        )}
      </div>

      {/* History */}
      <Card
        padding="md"
        title={t("agentReview.historyTitle")}
        action={
          <Tip content={t("common.refresh")}>
            <Button
              size="sm"
              variant="ghost"
              onClick={() => void reviewsQ.refetch()}
              aria-label={t("common.refresh")}
            >
              <RefreshCw data-icon size={12} />
            </Button>
          </Tip>
        }
      >
        {reviewsQ.isLoading ? (
          <Skeleton className="h-16 w-full" />
        ) : (reviewsQ.data ?? []).length === 0 ? (
          <p className="py-3 text-center text-sm text-subtle">{t("agentReview.empty")}</p>
        ) : (
          <ul className="divide-y divide-border">
            {(reviewsQ.data ?? []).map((r) => (
              <ReviewRow key={r.id} r={r} active={r.id === selected} onSelect={() => setSelected(r.id)} />
            ))}
          </ul>
        )}
      </Card>
    </div>
  );
}

function ReviewRow({ r, active, onSelect }: { r: ReviewInfo; active: boolean; onSelect: () => void }) {
  const { t } = useTranslation();
  return (
    <li>
      <button
        type="button"
        onClick={onSelect}
        className={`flex w-full items-center gap-2 px-1 py-2 text-left transition-colors duration-fast hover:bg-raised ${
          active ? "bg-inset" : ""
        }`}
      >
        <ShieldCheck data-icon size={14} className="shrink-0 text-accent" />
        <span className="tabular text-xs text-subtle">{formatRelative(r.created_at)}</span>
        <span className="min-w-0 flex-1 truncate text-sm text-fg">
          {r.verdict_summary ?? r.reviewed_session_id}
        </span>
        <span className="shrink-0 text-2xs text-subtle">
          {t(STATUS_KEYS[r.status] ?? "agentReview.statusPending")}
          {r.verdict_status ? ` · ${r.verdict_status}` : ""}
        </span>
      </button>
    </li>
  );
}
