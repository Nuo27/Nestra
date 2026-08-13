import { useEffect, useMemo, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { useNavigate, useSearch } from "@tanstack/react-router";
import { sessionDelete, sessionReveal, type Session } from "../ipc";
import { confirmDialog } from "../components/controls/ConfirmDialog";
import { useUI } from "../stores/ui";
import { invalidate } from "./queries";

/**
 * Bulk-selection state for the sessions list toolbar + batch actions.
 *
 * Owns the `selected` key set (`${provider}:${id}`), the live (pruned)
 * selection, the batch error banner, and the reveal/delete bulk actions.
 * The batch actions are self-contained (confirm, optimistic cache update,
 * background deletion, error summary) so the page only renders their state.
 */
export function useSessionSelection(sessions: Session[]) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useUI((s) => s.pushToast);
  const navigate = useNavigate();
  const search = useSearch({ from: "/sessions" }) as {
    id?: string;
    provider?: string;
  };
  const selectedId = search.id;
  const selectedProvider = search.provider;

  // Bulk-select by composite key `${provider}:${id}`. Resetting the selection
  // on filter change is intentional — selected rows that fall out of the
  // filter would otherwise be invisible to the user.
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [bulkErr, setBulkErr] = useState<string | null>(null);
  // The failure is also reported via toast; auto-clear the banner so it
  // doesn't linger at the top of the list indefinitely (✕ dismisses too).
  useEffect(() => {
    if (!bulkErr) return;
    const timer = setTimeout(() => setBulkErr(null), 8000);
    return () => clearTimeout(timer);
  }, [bulkErr]);

  const keyOf = (s: Session) => `${s.provider}:${s.id}`;
  // Prune stale selections whenever the visible list changes (filter, refresh).
  const visibleKeys = useMemo(
    () => new Set(sessions.map(keyOf)),
    [sessions],
  );
  const liveSelected = useMemo(() => {
    const next = new Set<string>();
    for (const k of selected) if (visibleKeys.has(k)) next.add(k);
    return next;
  }, [selected, visibleKeys]);

  // Set by the checkbox's reported value rather than blind-flipping: if the
  // checked prop ever diverges from `selected` (e.g. a mid-click refetch
  // pruned the row), the toggle direction stays correct.
  const toggleOne = (s: Session, checked: boolean) => {
    const k = keyOf(s);
    setSelected((prev) => {
      const next = new Set(prev);
      if (checked) next.add(k);
      else next.delete(k);
      return next;
    });
  };
  const toggleAll = () => {
    if (liveSelected.size === sessions.length) setSelected(new Set());
    else setSelected(new Set(sessions.map(keyOf)));
  };
  const clearSelection = () => setSelected(new Set());

  // Bulk: reveal — best-effort, partial failure is fine (each session opens
  // its own file manager window).
  const bulkReveal = async () => {
    setBulkErr(null);
    const picks = sessions.filter((s) => liveSelected.has(keyOf(s)));
    await Promise.allSettled(
      picks.map((s) => sessionReveal(s.provider, s.id)),
    ).then((results) => {
      const failed = results.filter((r) => r.status === "rejected").length;
      if (failed > 0) {
        setBulkErr(t("sessions.bulkRevealFailed", { count: picks.length, failed }));
      }
    });
  };

  // Bulk: delete — single confirm for the whole batch, then sequential
  // deletes in the background. The rows vanish from the list the moment the
  // user confirms; the disk deletion is fire-and-forget with a failure
  // summary (a failed batch rolls the rows back via the refetch below).
  const bulkDelete = async () => {
    setBulkErr(null);
    const picks = sessions.filter((s) => liveSelected.has(keyOf(s)));
    if (picks.length === 0) return;
    const ok = await confirmDialog({
      title: t("sessions.deleteConfirmTitle", { count: picks.length }),
      body: t("sessions.deleteConfirmBody", { count: picks.length }),
      confirmLabel: t("common.delete"),
    });
    if (!ok) return;
    // Optimistic: pull every picked row out of all sessions list caches and
    // clear their detail caches immediately, before any backend call.
    const keySet = new Set(picks.map(keyOf));
    qc.setQueriesData<Session[]>({ queryKey: ["sessions"] }, (old) =>
      Array.isArray(old) ? old.filter((s) => !keySet.has(keyOf(s))) : old,
    );
    for (const s of picks) {
      qc.removeQueries({ queryKey: ["session", s.provider, s.id] });
    }
    setSelected(new Set());
    // If the open detail pane was one of the deleted sessions, drop it so
    // the pane unmounts instead of showing stale cache.
    if (selectedId && selectedProvider &&
        picks.some((s) => s.id === selectedId && s.provider === selectedProvider)) {
      navigate({ to: "/sessions", search: { id: undefined, provider: undefined } });
    }
    // Background deletion.
    let failed = 0;
    for (const s of picks) {
      try {
        await sessionDelete(s.provider, s.id);
      } catch {
        failed++;
      }
    }
    if (failed > 0) {
      setBulkErr(t("sessions.bulkDeleteFailed", { count: picks.length, failed }));
      toast(t("sessions.bulkDeleteFailed", { count: picks.length, failed }), "error");
    } else {
      toast(t("sessions.deletedToast", { count: picks.length }), "success");
    }
    // Final sync with disk (also rolls back any failed rows).
    invalidate(qc, "session");
  };

  return {
    liveSelected,
    bulkErr,
    clearBulkErr: () => setBulkErr(null),
    keyOf,
    toggleOne,
    toggleAll,
    clearSelection,
    bulkReveal,
    bulkDelete,
  };
}

export type SessionSelection = ReturnType<typeof useSessionSelection>;
