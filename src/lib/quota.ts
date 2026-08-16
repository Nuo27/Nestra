import type {
  BuiltinKind,
  EndpointInfo,
  QuotaExtractorConfig,
  QuotaQueryPlan,
  RefreshEndpointConfig,
} from "../ipc";

/// The first openai/custom protocol's base_url, else the first protocol —
/// mirrors `db::pick_quota_url` (the SSOT for which URL a quota query hits).
export function quotaUrl(endpoint: EndpointInfo): string {
  return (
    endpoint.protocols.find((p) => p.protocol === "openai-comp" || p.protocol === "custom")
      ?.base_url ??
    endpoint.protocols[0]?.base_url ??
    ""
  );
}

/// Resolve a built-in query kind from a base_url host. Mirrors
/// `endpoint_quota::provider_kind_for` — used to backfill the effective plan
/// for legacy endpoints and to flag the recommended option in the picker.
export function builtinKindForUrl(url: string): BuiltinKind | null {
  const host = url.toLowerCase();
  if (host.includes("z.ai")) return "zai";
  if (host.includes("minimax")) return "minimax";
  if (host.includes("openrouter.ai")) return "openrouter";
  if (host.includes("opencode.ai")) return "opencode_go";
  if (host.includes("127.0.0.1") || host.includes("localhost")) return "mock";
  return null;
}

/// Resolve the effective query plan for an endpoint, honouring legacy blobs.
/// Mirrors `quota_refresh::resolve_plan`: explicit `query_plan` wins; else a
/// legacy enabled extractor becomes Custom; else host detection backfills a
/// Preset; else None. The single source of truth for "which plan is in use".
export function resolvePlan(
  cfg: RefreshEndpointConfig,
  endpoint: EndpointInfo,
): QuotaQueryPlan {
  if (cfg.query_plan) return cfg.query_plan;
  if (cfg.extractor?.enabled) {
    return {
      source: "custom",
      enabled: true,
      url: cfg.extractor.url,
      headers: cfg.extractor.headers,
      unit: cfg.extractor.unit,
      fields: cfg.extractor.fields,
    };
  }
  const kind = builtinKindForUrl(quotaUrl(endpoint));
  return kind ? { source: "preset", kind } : { source: "none" };
}

/// True when the plan can fetch quota (i.e. not `None`). The "verified" half
/// of the gate lives in `provisioned`; combine as `isPlanActive(plan) && provisioned`.
export function isPlanActive(plan: QuotaQueryPlan): boolean {
  return plan.source !== "none";
}

export const BUILTIN_LABEL: Record<BuiltinKind, string> = {
  zai: "Z.ai",
  minimax: "MiniMax",
  openrouter: "OpenRouter",
  opencode_go: "OpenCode Go",
  mock: "Local mock",
};

/// All built-in query kinds, in picker order. Scalable — adding a new
/// built-in fetcher means appending here + `BUILTIN_LABEL`.
export const BUILTIN_OPTIONS: { value: BuiltinKind; label: string }[] = [
  { value: "zai", label: "Z.ai" },
  { value: "minimax", label: "MiniMax" },
  { value: "openrouter", label: "OpenRouter" },
  { value: "opencode_go", label: "OpenCode Go" },
  { value: "mock", label: "Local mock" },
];

/// One-line label for the plan-status indicator on the card. Returns a
/// translation KEY ("quota.planLabel*") — callers render with `t(...)` and
/// interpolate the built-in kind themselves (module maps store keys).
export function planLabel(plan: QuotaQueryPlan): "quota.planLabelBuiltin" | "quota.planLabelCustom" | "quota.planLabelNone" {
  switch (plan.source) {
    case "preset":
      return "quota.planLabelBuiltin";
    case "custom":
      return "quota.planLabelCustom";
    case "none":
      return "quota.planLabelNone";
  }
}

/// Flatten a plan to the string value used by the plan `<Select>`: "none",
/// "custom", or the built-in kind. Inverse of [`planFromSelectValue`].
export function planToSelectValue(plan: QuotaQueryPlan): string {
  return plan.source === "preset" ? plan.kind : plan.source;
}

/// Build a plan from a `<Select>` value, preserving an existing custom
/// extractor config when (re-)selecting Custom.
export function planFromSelectValue(
  value: string,
  current: QuotaQueryPlan,
  legacyExtractor: QuotaExtractorConfig | null,
): QuotaQueryPlan {
  if (value === "none") return { source: "none" };
  if (value === "custom") {
    // Preserve an existing extractor config when (re-)selecting Custom;
    // otherwise start a blank one. (if/else, not nested ternary.)
    let existing: QuotaExtractorConfig | null = null;
    if (current.source === "custom") {
      existing = {
        enabled: true,
        url: current.url,
        headers: current.headers,
        unit: current.unit,
        fields: current.fields,
      };
    } else if (legacyExtractor) {
      existing = {
        enabled: true,
        url: legacyExtractor.url,
        headers: legacyExtractor.headers,
        unit: legacyExtractor.unit,
        fields: legacyExtractor.fields,
      };
    }
    if (existing) return { source: "custom", ...existing };
    return { source: "custom", enabled: true, url: "", headers: {}, unit: null, fields: {} };
  }
  // A built-in kind.
  return { source: "preset", kind: value as BuiltinKind };
}

/// Build the persisted per-endpoint config for a full-blob settings write.
/// `quota_refresh_set_settings` replaces the whole blob server-side, so every
/// persisted field must be carried from the incoming config — a field omitted
/// here is silently erased (serde `#[serde(default)]` fills `null`). This is
/// exactly how `opencode_workspace_id` used to get wiped by any unrelated
/// quota-settings write; keep this list in lock-step with the Rust
/// `StoredEndpointConfig` struct.
export function composeEndpointConfig(
  patch: RefreshEndpointConfig,
): RefreshEndpointConfig {
  return {
    enabled: patch.enabled,
    protocol: patch.protocol,
    model: patch.model,
    target_quota_name: patch.target_quota_name,
    last_status: patch.last_status,
    check_rate_secs: patch.check_rate_secs || 180,
    reset_grace_secs: patch.reset_grace_secs || 180,
    extractor: patch.extractor,
    query_plan: patch.query_plan,
    provisioned: patch.provisioned,
    preview_windows: patch.preview_windows,
    opencode_workspace_id: patch.opencode_workspace_id,
  };
}

/// Inputs to the single-refresh-authority decision. Pure so the deadline logic
/// is unit-testable without a React/jsdom harness.
export interface CatchUpRefreshArgs {
  auto: boolean;
  isFetching: boolean;
  /** Absolute deadline for the next auto-refresh (epoch ms); 0 = not armed. */
  nextRefreshAt: number;
  /** UI-only wall clock (epoch ms). */
  now: number;
  /** Epoch ms of the last catch-up attempt for this deadline. */
  lastAttemptAt: number;
  intervalSec: number;
}

/// Decide whether a catch-up refetch should fire right now. The ONLY refresh
/// authority for auto-refresh: fires when the absolute deadline has passed,
/// no fetch is in flight, and the last attempt is older than one interval.
/// The `intervalSec` throttle turns a failed fetch (deadline unchanged) into a
/// retry on the same cadence as the countdown instead of a per-render hammer
/// loop; a success advances `nextRefreshAt` past `now`, which silences the
/// decision until the next deadline.
export function shouldCatchUpRefresh({
  auto,
  isFetching,
  nextRefreshAt,
  now,
  lastAttemptAt,
  intervalSec,
}: CatchUpRefreshArgs): boolean {
  if (!auto || isFetching) return false;
  if (nextRefreshAt <= 0 || now < nextRefreshAt) return false;
  return now - lastAttemptAt >= intervalSec * 1000;
}
