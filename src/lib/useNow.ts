import { useEffect, useState } from "react";

/// UI-only wall clock. Returns the current epoch-ms time, refreshed every
/// `stepMs` and immediately on window focus / visibility regain. The tick
/// exists ONLY to make the UI re-render a countdown — business correctness
/// (when a refetch or keep-alive fire is due) always derives from absolute
/// deadlines or backend state, never from this timer surviving or not.
///
/// `syncKey` re-syncs the clock the instant its value changes (e.g. a query's
/// `dataUpdatedAt`), so a countdown restarts cleanly at the full interval
/// instead of waiting up to `stepMs` for the next tick.
export function useNow(stepMs = 1000, syncKey?: unknown): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    setNow(Date.now());
  }, [syncKey]);
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), stepMs);
    const sync = () => setNow(Date.now());
    document.addEventListener("visibilitychange", sync);
    window.addEventListener("focus", sync);
    return () => {
      clearInterval(id);
      document.removeEventListener("visibilitychange", sync);
      window.removeEventListener("focus", sync);
    };
  }, [stepMs]);
  return now;
}
