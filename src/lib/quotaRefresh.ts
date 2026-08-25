import { useEffect, useMemo } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import {
  quotaRefreshGet,
  quotaRefreshSet,
  type EndpointInfo,
  type QuotaItem,
  type RefreshEndpointConfig,
  type RefreshSettings,
} from "../ipc";
import { useUI } from "../stores/ui";
import { qk } from "./queries";
import {
  builtinKindForUrl,
  composeEndpointConfig,
  isPlanActive,
  planFromSelectValue,
  quotaUrl,
  resolvePlan,
} from "./quota";

const DEFAULT_REFRESH: RefreshSettings = { endpoints: {} };

/// Per-endpoint fallback when the settings blob has no entry yet.
export const DEFAULT_CFG: RefreshEndpointConfig = {
  enabled: false,
  protocol: null,
  model: null,
  target_quota_name: null,
  last_status: null,
  check_rate_secs: 180,
  reset_grace_secs: 180,
  extractor: null,
  query_plan: null,
  provisioned: null,
  opencode_workspace_id: null,
};

/**
 * The quota-refresh config state machine for one endpoint: the persisted
 * `RefreshEndpointConfig`, the effective query plan + gate (plan AND
 * provisioned), the persist paths (`patch`/`toggle`/`setPlan`/`verifyQuery`),
 * and the preview-window selection. Shared by the quota card header/bars and
 * the settings dialog so both stay in lockstep.
 *
 * Plan changes re-lock the gate: clears `provisioned` (re-verify required),
 * turns keep-alive OFF, snaps auto-refresh OFF, and kicks a verify fetch so
 * the gate reopens on success.
 */
export function useQuotaRefresh(endpoint: EndpointInfo, targetItems: QuotaItem[]) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useUI((s) => s.pushToast);
  const auto = useUI((s) => s.quotaAuto);
  const setQuotaAuto = useUI((s) => s.setQuotaAuto);

  const refreshQ = useQuery({ queryKey: qk.quotaRefresh(), queryFn: quotaRefreshGet });
  const cfg: RefreshEndpointConfig =
    refreshQ.data?.endpoints[endpoint.id] ?? DEFAULT_CFG;

  // Effective plan + gating state. `resolvePlan` honours legacy blobs; the
  // keep-alive switch, auto-refresh, and the quota bars all gate on plan +
  // provisioned (the "query not set or not successful → everything off" rule).
  const plan = resolvePlan(cfg, endpoint);
  const planActive = isPlanActive(plan);
  const provisioned = cfg.provisioned ?? false;
  const canArm = planActive && provisioned;
  // The built-in kind this endpoint's host supports, if any. Flagged as
  // "(recommended)" in the plan picker.
  const availableBuiltin = useMemo(
    () => builtinKindForUrl(quotaUrl(endpoint)),
    [endpoint],
  );

  // Snap auto-refresh OFF the instant the gate closes (plan cleared or
  // un-verified). The switch is also disabled below, but this guarantees the
  // refetch interval stops and the toggle visually reflects "off" without a
  // manual flick when provisioned drops.
  useEffect(() => {
    if (!canArm && auto) setQuotaAuto(false);
  }, [canArm, auto, setQuotaAuto]);

  const toggle = (next: boolean) => {
    save(composeEndpointConfig({ ...cfg, enabled: next }));
  };

  // Persist a (possibly partial) config, keeping enabled/last_status intact
  // unless explicitly changed. Keeps the panel's Select writes and the
  // keep-alive switch on the same one-liner path. Merges from the LATEST
  // cache state — a render-closure snapshot would let rapid toggles
  // overwrite each other's writes.
  const save = (patch: RefreshEndpointConfig) => {
    const current =
      qc.getQueryData<RefreshSettings>(qk.quotaRefresh()) ?? refreshQ.data ?? DEFAULT_REFRESH;
    const nextCfg = composeEndpointConfig(patch);
    const updated: RefreshSettings = {
      ...current,
      endpoints: { ...current.endpoints, [endpoint.id]: nextCfg },
    };
    quotaRefreshSet(updated)
      .then(() => qc.invalidateQueries({ queryKey: qk.quotaRefresh() }))
      .catch(() => toast(t("quota.saveFailed"), "error"));
  };

  const patch = (
    p: Partial<
      Pick<
        RefreshEndpointConfig,
        | "enabled"
        | "protocol"
        | "model"
        | "check_rate_secs"
        | "reset_grace_secs"
        | "target_quota_name"
        | "extractor"
        | "query_plan"
        | "provisioned"
        | "preview_windows"
      >
    >,
  ) => save({ ...cfg, ...p });

  /// Change the query plan from the `<Select>` value ("none" | a BuiltinKind
  /// | "custom"). Any plan change re-locks the gate: clears `provisioned`
  /// (re-verify required), turns keep-alive OFF (it must be re-armed after a
  /// successful verify), and snaps auto-refresh OFF (per the "query not set or
  /// not successful → auto + keep-alive must be off" rule). Then kicks a verify
  /// fetch so the gate reopens on success.
  const setPlan = (value: string) => {
    const nextPlan = planFromSelectValue(value, plan, cfg.extractor);
    // Windows are plan-specific — reset the preview set so stale names don't
    // linger after switching plans.
    patch({ query_plan: nextPlan, provisioned: false, enabled: false, preview_windows: null });
    setQuotaAuto(false);
    qc.invalidateQueries({ queryKey: qk.endpointQuota(endpoint.id) });
  };

  /// Verify the current plan: re-fetch quota (the backend stamps
  /// `provisioned` on success) then refresh the settings blob so the gate
  /// state updates. Returns the fetch promise so the button can show loading.
  const verifyQuery = () =>
    qc
      .invalidateQueries({ queryKey: qk.endpointQuota(endpoint.id) })
      .then(() => qc.invalidateQueries({ queryKey: qk.quotaRefresh() }));

  // Default the target picker to the first item that matches the 5h-name
  // heuristic when the user hasn't picked one yet, so the dropdown always
  // has a sensible preselection on first visit.
  const defaultTarget = useMemo(() => {
    const hit = targetItems.find(
      (i) => i.name === "5h-token" || i.name.endsWith("/5h"),
    );
    return hit?.name ?? targetItems[0]?.name ?? null;
  }, [targetItems]);

  const targetQuotaName = cfg.target_quota_name ?? defaultTarget;

  // Effective preview set: explicit `preview_windows` (even empty = show
  // nothing) wins; unset falls back to the 5h-heuristic single window so
  // legacy endpoints keep their current card until the user touches a toggle.
  const previewWindows = useMemo(() => {
    if (cfg.preview_windows != null) return cfg.preview_windows;
    return defaultTarget ? [defaultTarget] : [];
  }, [cfg.preview_windows, defaultTarget]);

  /// Persist the enabled preview set. Turning the last window off writes an
  /// explicit empty list (card shows nothing) — the fallback only applies
  /// while the field is unset.
  const togglePreviewWindow = (name: string, on: boolean) => {
    const next = new Set(previewWindows);
    if (on) next.add(name);
    else next.delete(name);
    patch({ preview_windows: [...next] });
  };

  return {
    cfg,
    plan,
    planActive,
    provisioned,
    canArm,
    availableBuiltin,
    patch,
    toggle,
    setPlan,
    verifyQuery,
    targetQuotaName,
    previewWindows,
    togglePreviewWindow,
  };
}

export type QuotaRefresh = ReturnType<typeof useQuotaRefresh>;
