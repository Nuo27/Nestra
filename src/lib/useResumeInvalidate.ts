import { useEffect, useRef } from "react";
import { useQueryClient } from "@tanstack/react-query";

/// On document visibility regain (window shown after being hidden), invalidate
/// the given query keys so TanStack refetches their active observers with
/// fresh backend state. Reliable WebView2 replacement for
/// `refetchOnWindowFocus`, which does not fire consistently when a Tauri
/// window is restored from tray / after minimize — only the `visibilitychange`
/// path here actually triggers under WebView2's controller-driven
/// visibility transitions.
///
/// One fetch per resume event — no per-render hammering. `visibilityState`
/// is checked so that ordinary window focus events (alt-tab, clicking the
/// title bar) don't trigger; only "was hidden, now visible" does.
///
/// Callers should leave per-query `refetchOnWindowFocus` off to keep a single
/// resume authority per surface and avoid duplicate fetches from
/// overlapping mechanisms.
export function useResumeInvalidate(
  keys: readonly (readonly unknown[])[],
): void {
  const qc = useQueryClient();
  const keysRef = useRef(keys);
  keysRef.current = keys;
  useEffect(() => {
    const onVisible = () => {
      if (document.visibilityState !== "visible") return;
      for (const key of keysRef.current) {
        void qc.invalidateQueries({ queryKey: key });
      }
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => document.removeEventListener("visibilitychange", onVisible);
  }, [qc]);
}