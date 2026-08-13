import { invoke } from "@tauri-apps/api/core";

export interface QuotaItem {
  name: string;
  pct: number;
  used: number | null;
  total: number | null;
  remaining: number | null;
  resets_in: string | null;
  resets_at_ms: number | null;
  /** Currency unit for balance-based items (e.g. "CNY", "USD"). Null for
   *  window-based quota. */
  unit: string | null;
  /** True for monetary-balance items (OpenRouter credits, Moonshot balance):
   *  no reset-window semantics, and keep-alive never pings them. */
  is_balance: boolean;
}

export interface QuotaExtractorFields {
  /** JSON path (dot-separated, numeric segments index arrays). */
  name?: string;
  used?: string;
  remaining?: string;
  total?: string;
  unit?: string;
}

/// User-configured balance extractor: GET a URL and pull fields from the
/// JSON response by dot-path. `{{baseUrl}}` / `{{apiKey}}` in url/headers
/// are substituted from the endpoint. Custom extracts are always
/// balance-shaped (is_balance): no reset window, keep-alive never pings.
export interface QuotaExtractorConfig {
  enabled: boolean;
  url: string;
  headers?: Record<string, string>;
  /** Static currency unit used when `fields.unit` is not configured. */
  unit?: string | null;
  fields: QuotaExtractorFields;
}

/// A built-in provider quota fetcher — the dispatch key for the `preset`
/// query plan. Mirrors `endpoint_quota::BuiltinKind` (snake_case on the wire).
export type BuiltinKind = "zai" | "minimax" | "openrouter" | "opencode_go" | "mock";

/// How an endpoint's quota is queried — the unified "query plan". Mirrors
/// `endpoint_quota::QuotaQueryPlan` (internally tagged on `source`). `none`
/// means no query configured (quota display + keep-alive stay gated until
/// the user picks one). Plans are declared by provider presets and stamped
/// at create time; legacy endpoints resolve a plan from their base_url host.
export type QuotaQueryPlan =
  | { source: "none" }
  | { source: "preset"; kind: BuiltinKind }
  | ({ source: "custom" } & QuotaExtractorConfig);

export interface RefreshEndpointConfig {
  enabled: boolean;
  protocol: string | null;
  model: string | null;
  target_quota_name: string | null;
  last_status: string | null;
  check_rate_secs: number;
  reset_grace_secs: number;
  /** Custom quota extractor; when enabled it overrides the built-in fetch.
   *  Legacy — kept for compat with older blobs; new writes use query_plan. */
  extractor: QuotaExtractorConfig | null;
  /** Explicit query plan (the canonical "how is quota queried" choice).
   *  `null` means "use legacy resolution" (extractor.enabled or host detect). */
  query_plan?: QuotaQueryPlan | null;
  /** True once any fetch has returned data for the current plan. Gates both
   *  the keep-alive switch and the quota bars. Cleared when the plan changes. */
  provisioned?: boolean | null;
  /** Quota windows shown on the provider-card preview (multi-select).
   *  `null` (legacy) falls back to the 5h-name heuristic; `[]` = show none.
   *  Display-only — independent of `target_quota_name` (the keep-alive ping
   *  target). Set from the Quota page's settings dialog. */
  preview_windows?: string[] | null;
  /** OpenCode Go dashboard workspace ID (non-secret). Paired with the `auth`
   *  cookie stored encrypted under `opencode-go-cookie-{id}`; edited in the
   *  Quota settings dialog's creds section. Must be carried through every
   *  full-blob settings write (`quota_refresh_set_settings` replaces the
   *  whole blob) or the server's serde default silently erases it. */
  opencode_workspace_id?: string | null;
}

export interface RefreshSettings {
  endpoints: Record<string, RefreshEndpointConfig>;
}
export interface EndpointQuota {
  ok: boolean;
  plan: string | null;
  error: string | null;
  items: QuotaItem[];
}
export const endpointFetchQuota = (id: string) =>
  invoke<EndpointQuota>("endpoint_fetch_quota", { id });

// ---- 5h quota auto-refresh ----
export const quotaRefreshGet = () =>
  invoke<RefreshSettings>("quota_refresh_get_settings");
export const quotaRefreshSet = (value: RefreshSettings) =>
  invoke<void>("quota_refresh_set_settings", { value });

/// OpenCode Go dashboard credentials status. The cookie value is never
/// returned — only whether one is set (credential boundary, like API keys).
export interface OpencodeCredsStatus {
  workspace_id: string | null;
  has_cookie: boolean;
}
export const opencodeGetCreds = (endpointId: string) =>
  invoke<OpencodeCredsStatus>("opencode_get_creds", { endpointId });
export const opencodeSetCreds = (
  endpointId: string,
  cookie: string,
  workspaceId: string,
) =>
  invoke<void>("opencode_set_creds", {
    endpointId,
    cookie,
    workspaceId,
  });

export interface PingPreview {
  method: string;
  url: string;
  headers: [string, string][];
  body: string;
  protocol: string;
  model: string;
}
export const quotaKeepalivePreview = (endpointId: string) =>
  invoke<PingPreview>("quota_keepalive_preview", { endpointId });
export interface PingNowResult {
  ok: boolean;
  status: string;
}
export const quotaPingNow = (endpointId: string) =>
  invoke<PingNowResult>("quota_ping_now", { endpointId });

export type KeepAlivePhase =
  | "disabled"
  | "not_configured"
  | "unverified"
  | "idle"
  | "resetting"
  | "pinging"
  | "retrying"
  | "error";
export interface KeepAliveState {
  phase: KeepAlivePhase;
  last_success_at: number | null;
  next_fire_at: number | null;
  last_error: string | null;
  attempts: number;
  /// Epoch ms of the most recent worker heartbeat (one per tick), shared
  /// across all endpoints since the worker is a single thread. `null` before
  /// the first tick completes. A stale value means the worker is dead or the
  /// process is suspended.
  last_heartbeat_at: number | null;
  /// Epoch ms of the last worker panic recovered by the supervisor, or
  /// `null` if none (or it has since ticked successfully).
  last_panic_at: number | null;
}
export const quotaKeepaliveStatus = (endpointId: string) =>
  invoke<KeepAliveState>("quota_keepalive_status", { endpointId });
