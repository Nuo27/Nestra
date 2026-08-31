import { useEffect, useRef } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  endpointList,
  quotaRefreshGet,
  type EndpointInfo,
  type RefreshSettings,
} from "../../ipc";
import { qk } from "../../lib/queries";
import { useUI } from "../../stores/ui";
import { isPlanActive, resolvePlan, shouldCatchUpRefresh } from "../../lib/quota";

/// App-level quota auto-refresh driver — the ONE refresh authority. Mounted
/// once in RootShell so armed endpoints keep refreshing on every page, not
/// just while the Quota page is open. Invisible leaf component: renders null,
/// re-renders nothing else.
///
/// Semantics:
/// - deadline = the shared `qk.endpointQuota(id)` cache's `dataUpdatedAt` +
///   `quotaIntervalSec` — an ABSOLUTE deadline, so a hidden/throttled timer
///   degrades to a catch-up fetch on the next visible tick, never a drift.
/// - candidates = endpoints whose query plan is active AND provisioned (the
///   same canArm gate as the bars/keep-alive) — the gate closing drops the
///   endpoint from the loop by itself.
/// - `invalidateQueries` only refetches queries with a mounted observer, so
///   invisible endpoints cost nothing; their stale cache refetches on the
///   next mount. The decision logic is `shouldCatchUpRefresh` (throttles
///   failed fetches to one retry per interval — unit-tested in lib/quota).
/// - the first-ever fetch of an endpoint stays with mount refetch (the
///   deadline is only computed from an existing `dataUpdatedAt`).
export function QuotaAutoDriver() {
  const qc = useQueryClient();
  const auto = useUI((s) => s.quotaAuto);
  const intervalSec = useUI((s) => s.quotaIntervalSec);
  // Keep the two lookup caches warm regardless of which page is open.
  useQuery({ queryKey: qk.endpoints(), queryFn: endpointList });
  useQuery({ queryKey: qk.quotaRefresh(), queryFn: quotaRefreshGet });
  const lastAttempt = useRef(new Map<string, number>());

  // Re-arming auto-refresh must catch up immediately (a prior throttled
  // attempt must not block a fresh arm). Reset on every auto flip.
  useEffect(() => {
    if (auto) lastAttempt.current.clear();
  }, [auto]);

  useEffect(() => {
    if (!auto) return;
    const check = () => {
      const now = Date.now();
      const endpoints = qc.getQueryData<EndpointInfo[]>(qk.endpoints()) ?? [];
      const settings = qc.getQueryData<RefreshSettings>(qk.quotaRefresh());
      for (const e of endpoints) {
        const cfg = settings?.endpoints[e.id];
        if (!cfg || !isPlanActive(resolvePlan(cfg, e)) || !(cfg.provisioned ?? false)) {
          continue;
        }
        const key = qk.endpointQuota(e.id);
        const state = qc.getQueryState(key);
        if (!state || state.dataUpdatedAt <= 0) continue;
        if (
          shouldCatchUpRefresh({
            auto: true,
            isFetching: state.fetchStatus === "fetching",
            nextRefreshAt: state.dataUpdatedAt + intervalSec * 1000,
            now,
            lastAttemptAt: lastAttempt.current.get(e.id) ?? 0,
            intervalSec,
          })
        ) {
          lastAttempt.current.set(e.id, now);
          void qc.invalidateQueries({ queryKey: key });
        }
      }
    };
    const id = window.setInterval(check, 1000);
    // Hidden-window timers throttle; re-check the moment the window returns
    // so an elapsed deadline catches up immediately (useNow semantics).
    document.addEventListener("visibilitychange", check);
    window.addEventListener("focus", check);
    check();
    return () => {
      window.clearInterval(id);
      document.removeEventListener("visibilitychange", check);
      window.removeEventListener("focus", check);
    };
  }, [auto, intervalSec, qc]);

  return null;
}
