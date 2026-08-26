import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { QueryObserverResult } from "@tanstack/react-query";

/// Phantom indicator for a TanStack query. Never shows during a plain
/// cache-first render or a background refresh that changed nothing (decision
/// #4: no noise when nothing changed). Flashes "updated" only after a
/// refetch whose payload actually differs from what was last rendered.
export function SyncIndicator({
  query,
  className,
}: {
  /// Pass the whole `useQuery` result; `T` is ignored, we only need its
  /// `data` to diff against the previous snapshot.
  query: QueryObserverResult<unknown>;
  className?: string;
}) {
  const { t } = useTranslation();
  const { data, isPending, isFetching, isError } = query;
  const [brief, setBrief] = useState(false);
  const lastSig = useRef<string | null>(null);
  const firstData = useRef(true);

  // Diff payload signatures across data changes. `data` identity changes on
  // every refetch; `signature` collapses equivalent payloads so "same data"
  // refreshes stay invisible.
  useEffect(() => {
    if (isPending || data == null) return;
    const sig = signature(data);
    if (firstData.current) {
      lastSig.current = sig;
      firstData.current = false;
      return;
    }
    if (sig !== lastSig.current) {
      lastSig.current = sig;
      setBrief(true);
    }
  }, [data, isPending]);

  // The 1.8s reset timer is keyed on `brief` itself: while the flash is up a
  // live timer ALWAYS exists, so an equivalent data refresh re-running the
  // diff effect above can never clear the timer and strand "updated" on
  // screen until the next real change.
  useEffect(() => {
    if (!brief) return;
    const id = window.setTimeout(() => setBrief(false), 1800);
    return () => window.clearTimeout(id);
  }, [brief]);

  if (isError) {
    return (
      <span className={`inline-flex items-center gap-1 text-2xs text-danger select-none ${className}`}>
        {/* `!` glyph — the design's danger marker; bare text alone read as a
            leftover fragment rather than a status. */}
        <span aria-hidden="true">!</span>
        {t("sync.syncError")}
      </span>
    );
  }
  // `brief` is the visible 1.8s flash after content actually changed. No
  // flash on cache-first render, on a no-op background refresh, or while a
  // fetch is still in flight — the page content itself is the confirmation.
  if (!brief || isFetching) return null;
  return (
    <span
      className={`inline-flex items-center gap-1 text-2xs text-subtle select-none animate-in fade-in-0 duration-slow ${className}`}
      aria-live="polite"
    >
      {t("sync.updated")}
    </span>
  );
}

function signature(data: unknown): string {
  if (data == null) return "∅";
  if (Array.isArray(data)) {
    // Fingerprint a list from (length, head, mid, tail) — the old (length,
    // first, slice(1,4)) sample missed a change at index ≥ 4 entirely.
    const n = data.length;
    const head = JSON.stringify(data[0]);
    const mid = JSON.stringify(data[Math.floor(n / 2)]);
    const tail = JSON.stringify(data[n - 1]);
    return `${n}|${head}|${mid}|${tail}`;
  }
  return JSON.stringify(data);
}