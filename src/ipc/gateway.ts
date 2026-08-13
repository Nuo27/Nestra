// ──────────────────────────────────────────────────────────────────────────
// Gateway Service Control Surface — typed frontend boundary.
//
// Mirrors the Rust `gateway_*` commands + structs in
// `src-tauri/src/commands.rs` and `orchestration/gateway/control.rs`. This is
// the Gateway *Service* (process/port/token/runtime) — routing policy lives in
// `./orchestration`, provider config in `./index`.
//
// Boundary rule: the loopback token is returned ONLY by `gatewayTokenGet`
// (explicit Reveal). It never rides in `GatewayServiceStatus` and thus never
// enters the React Query cache.
// ──────────────────────────────────────────────────────────────────────────

import { invoke } from "@tauri-apps/api/core";
import type { RouteRecord } from "./orchestration";

export type GatewayRuntimeState = "stopped" | "starting" | "running" | "error";

export interface GatewayActivityStats {
  /** Routed requests since this gateway run started (real route_request rows). */
  total_requests: number;
  last_request_at: number | null;
  /** Non-terminal tasks right now. */
  active_tasks: number;
}

/// Rich runtime status for the `/gateway` page. Credential-free: the token is
/// `has_token` only.
export interface GatewayServiceStatus {
  state: GatewayRuntimeState;
  enabled: boolean;
  configured_port: number;
  bound_base_url: string;
  started_at: number | null;
  uptime_secs: number | null;
  last_error: string | null;
  agents_enabled: string[];
  has_token: boolean;
  stats: GatewayActivityStats;
}

export interface GatewayToggleResult {
  ok: boolean;
  /// Agents whose config was rewritten (Direct on OFF, alias on ON).
  reverted: string[];
  /// Agents whose config rewrite FAILED — surfaced in the UI, not hidden.
  failed: string[];
  error: string | null;
}

export interface GatewayPortResult {
  ok: boolean;
  bound_port: number;
  failed: string[];
  error: string | null;
}

export interface GatewayTokenInfo {
  has_token: boolean;
  /// Plaintext loopback token — returned only here (explicit reveal).
  token: string;
}

export const gatewayGetStatus = () =>
  invoke<GatewayServiceStatus>("gateway_get_status");

export const gatewaySetEnabled = (enabled: boolean) =>
  invoke<GatewayToggleResult>("gateway_set_enabled", { enabled });

export const gatewayRestart = () =>
  invoke<GatewayToggleResult>("gateway_restart");

export const gatewaySetPort = (port: number) =>
  invoke<GatewayPortResult>("gateway_set_port", { port });

export const gatewayAutopickPort = () =>
  invoke<GatewayPortResult>("gateway_autopick_port");

export const gatewayTokenGet = () =>
  invoke<GatewayTokenInfo>("gateway_token_get");

export const gatewayTokenRegenerate = () =>
  invoke<GatewayToggleResult>("gateway_token_regenerate");

export const gatewayRecentActivity = (limit = 10) =>
  invoke<RouteRecord[]>("gateway_recent_activity", { limit });
