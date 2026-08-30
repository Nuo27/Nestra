// ──────────────────────────────────────────────────────────────────────────
// Provider Orchestration Layer — typed frontend boundary.
//
// These types mirror the orchestration data model in
// `src-tauri/src/orchestration/` (identity.rs + store.rs) and the canonical
// DDL in `src-tauri/src/schema.rs` (the routing_policy / logical_session /
// run / task / route_request / route_migration / model_catalog tables).
//
// The store persists the ordered route-target list as a serialized JSON
// string (an Option<String> column); the wrappers below parse it into typed
// objects at the boundary so consumers never touch raw JSON.
// ──────────────────────────────────────────────────────────────────────────

import { invoke } from "@tauri-apps/api/core";
import type { ModelAbilities } from "./index";

// ---- invoke wrappers (routing_policy — the only wired surface) -----------

/// One ordered (endpoint, model) pin. The router serves the first healthy
/// entry; failures walk the list. Mirrors `store::RouteTarget`.
export interface RouteTarget {
  endpoint: string;
  model: string;
}

/** Raw row shape straight from the Rust `RoutingPolicyRow` (JSON-string cols). */
interface RoutingPolicyRowRaw {
  agent_id: string;
  role: string;
  route_targets: string | null;
  migrate_on_quota: boolean;
  inject_cache_control: boolean;
  affinity_scope: "task" | "session" | "none";
  updated_at: number;
}

function parseTargets(s: string | null): RouteTarget[] {
  if (s == null) return [];
  try {
    const v = JSON.parse(s);
    if (!Array.isArray(v)) return [];
    return v
      .filter((x): x is { endpoint: string; model: string } =>
        typeof x?.endpoint === "string" && typeof x?.model === "string")
      .map((x) => ({ endpoint: x.endpoint, model: x.model }));
  } catch {
    return [];
  }
}

function fromRaw(r: RoutingPolicyRowRaw): RoutingPolicyRow {
  return {
    agent_id: r.agent_id,
    role: r.role,
    route_targets: parseTargets(r.route_targets),
    migrate_on_quota: r.migrate_on_quota,
    inject_cache_control: r.inject_cache_control,
    affinity_scope: r.affinity_scope,
    updated_at: r.updated_at,
  };
}

export async function routingPolicyList(agentId: string): Promise<RoutingPolicyRow[]> {
  const raw = await invoke<RoutingPolicyRowRaw[]>("routing_policy_list", { agentId });
  return raw.map(fromRaw);
}

export async function routingPolicyUpsert(input: RoutingPolicyInput): Promise<void> {
  await invoke("routing_policy_upsert", { policy: input });
}

export async function routingPolicyDelete(
  agentId: string,
  role: string,
): Promise<boolean> {
  return invoke<boolean>("routing_policy_delete", { agentId, role });
}

// ---- model catalog ------------------------------

/// One `(endpoint, model)` entry the router can consider, with its merged
/// `ModelAbilities`. Mirrors `commands::ModelCatalogEntry`.
export interface ModelCatalogEntry {
  endpoint_id: string;
  model_id: string;
  abilities: ModelAbilities;
}

/// Read the current `model_catalog` (cheap; stale only after endpoint/ability
/// edits, which the UI rebuilds for via [`modelCatalogRebuild`]).
export async function modelCatalog(): Promise<ModelCatalogEntry[]> {
  return invoke<ModelCatalogEntry[]>("orch_model_catalog");
}

/// Rebuild the `model_catalog` from live endpoints + the models.dev ability
/// cache, then return the fresh index. Call after editing endpoints/abilities.
export async function modelCatalogRebuild(): Promise<ModelCatalogEntry[]> {
  return invoke<ModelCatalogEntry[]>("orch_model_catalog_rebuild");
}

// ---- router dry-run -------------------------------------------------

/// Result of a router dry-run for one hypothetical Task. Lets the UI show
/// "this (agent, role, requested model) would resolve to (endpoint, model)
/// for this reason". Mirrors `commands::ResolvePreview`.
export interface ResolvePreview {
  endpoint_id: string; // empty when reason = "no_eligible"
  model: string;
  reason: "explicit" | "affinity" | "capability" | "fallback" | "no_eligible";
  cache_strategy: string; // "off" | "anthropicexplicit" | "deepseekauto" | "openrouterpassthrough"
  requested_model: string | null;
  requested_provider: string | null;
  /** The resolved model's context window (tokens) — what a routed alias
   *  advertises. `null` when the catalog carries no abilities for it. */
  context_window: number | null;
}

/// Dry-run the router. All capability fields are optional hints; pass only
/// the ones the Task requires. `role` is a policy-role key
/// ("main" | "*" | "claude:x" | "pi:x" | "opencode:x" | "tier:haiku/sonnet/opus").
export async function resolvePreview(input: {
  agentId: string;
  role?: string;
  requestedProvider?: string;
  requestedModel?: string;
  reasoning?: boolean;
  toolCall?: boolean;
  vision?: boolean;
  contextFloor?: number;
}): Promise<ResolvePreview> {
  return invoke<ResolvePreview>("orch_resolve_preview", {
    agentId: input.agentId,
    role: input.role ?? null,
    requestedProvider: input.requestedProvider ?? null,
    requestedModel: input.requestedModel ?? null,
    reasoning: input.reasoning ?? null,
    toolCall: input.toolCall ?? null,
    vision: input.vision ?? null,
    contextFloor: input.contextFloor ?? null,
  });
}

// ---- gateway: per-agent opt-in --------------------------------------------

/// Toggle gateway routing for an agent. When enabling, writes the stable
/// gateway alias as the agent's base_url; when disabling, restores direct
/// config (the real upstream). Supports all three agents.
export async function agentSetGatewayEnabled(
  agentId: string,
  enabled: boolean,
): Promise<void> {
  await invoke("agent_set_gateway_enabled", { agentId, enabled });
}

/// Read the per-agent gateway opt-in flag from `setting_kv`. Used so the
/// Agents-page toggle reflects persisted state without waiting on
/// `orchStatus` (which only lists enabled ids).
export async function agentGatewayEnabled(agentId: string): Promise<boolean> {
  const v = await invoke<unknown | null>("setting_get", {
    key: `orchestration.gateway.${agentId}`,
  });
  return v === true;
}

// ---- migration: route history + migration events -------------------------

/// Full route history for one task — every attempt's `RouteRecord`,
/// oldest-first. Mirrors `commands::orch_route_history`. This is the data
/// behind the "why this provider/model" trace: requested vs resolved model,
/// route reason, observed outcome, and the honest `generation_broken` flag.
export async function routeHistory(taskId: string): Promise<RouteRecord[]> {
  return invoke<RouteRecord[]>("orch_route_history", { taskId });
}

/// Migration events for one task. Mirrors `commands::orch_migrations`; each
/// row's `reason` is quota_exhausted | rate_limit | temp_5xx | timeout |
/// retries_exhausted.
export async function migrations(taskId: string): Promise<RouteMigrationRow[]> {
  return invoke<RouteMigrationRow[]>("orch_migrations", { taskId });
}

// ---- observability: task summaries ---------------------------------------

/// One task row for the Active-tasks view. Aggregates `route_request` by
/// task. Mirrors `store::TaskSummary`.
export interface TaskSummary {
  task_id: string;
  agent_id: string;
  logical_session: string | null;
  request_count: number;
  latest_status: number | null;
  generation_broken: boolean;
  first_seen: number;
  last_seen: number;
}

/// Task summaries for every observed task, most-recently-active first.
export async function tasks(limit?: number): Promise<TaskSummary[]> {
  return invoke<TaskSummary[]>("orch_tasks", { limit: limit ?? null });
}

/// Task summaries whose `logical_session` matches a session id.
export async function sessionTasks(
  logicalSession: string,
  limit?: number,
): Promise<TaskSummary[]> {
  return invoke<TaskSummary[]>("orch_session_tasks", {
    logicalSession,
    limit: limit ?? null,
  });
}

/// One subagent role observed on an agent's routed requests. `role` is the
/// policy key (`claude:researcher`, `opencode:research`, …), mapping 1:1 to a
/// `routing_policy.role` value. Mirrors `store::DetectedRoleSummary`.
export interface DetectedRoleSummary {
  role: string;
  request_count: number;
  last_seen: number;
}

/// Distinct subagent roles observed for one agent, most-recently-active
/// first. `main` is filtered. Feeds the routing-policy editor suggestions.
export async function detectedRoles(
  agentId: string,
  limit?: number,
): Promise<DetectedRoleSummary[]> {
  return invoke<DetectedRoleSummary[]>("orch_detected_roles", {
    agentId,
    limit: limit ?? null,
  });
}

/// One (day, agent, endpoint, model) usage bucket. Mirrors `store::UsageRow`.
/// `cost_usd` is computed at read time against the current price catalog;
/// `null` = no price known for this model (unknown spend, not free).
export interface UsageRow {
  day: string;
  agent_id: string;
  endpoint_id: string;
  model_id: string;
  requests: number;
  usage_input: number;
  usage_output: number;
  cache_creation: number;
  cache_read: number;
  cost_usd: number | null;
}

/// Usage dashboard rows: folded lifetime history (`usage_daily`) plus the
/// live retention window (`route_request`) — disjoint halves the backend
/// guarantees never overlap per row. `agentId` filters; `days` bounds the
/// window (omitted = lifetime).
export async function usageSummary(
  agentId?: string,
  days?: number,
): Promise<UsageRow[]> {
  return invoke<UsageRow[]>("orch_usage_summary", {
    agentId: agentId ?? null,
    days: days ?? null,
  });
}

/// Wipe ALL gateway observability data (tasks, requests, migrations, the
/// lifetime usage rollup, affinity) — the Settings → Data danger action.
/// Configuration is untouched.
export async function obsClear(): Promise<void> {
  await invoke("orch_obs_clear");
}

/// `routing_policy` row — keyed `(agent_id, role)`. `role = "*"` is the
/// catch-all default; otherwise it's a `SubagentRole::as_policy_key()`.
/// Mirrors `RoutingPolicyRow` (store.rs:31-48).
export interface RoutingPolicyRow {
  agent_id: string;
  role: string; // "*" | "tier:{haiku|sonnet|opus}" | "claude:{name}" | "pi:{role}" | "opencode:{name}"
  /// Ordered (endpoint, model) pins; the router serves the first healthy
  /// entry and failures walk the list. Empty = routing fails closed.
  route_targets: RouteTarget[];
  migrate_on_quota: boolean;
  inject_cache_control: boolean; // gates the AnthropicExplicit cache strategy
  affinity_scope: "task" | "session" | "none";
  updated_at: number;
}

/// Frontend-facing input for `routing_policy_upsert`. Same shape as
/// `RoutingPolicyRow` minus the server-set `updated_at` (the command stamps it).
/// store's JSON-string columns at the boundary.
export interface RoutingPolicyInput {
  agent_id: string;
  role: string;
  route_targets: RouteTarget[];
  migrate_on_quota: boolean;
  inject_cache_control: boolean;
  affinity_scope: "task" | "session" | "none";
}

/// `route_request` row — the single richest record: requested vs resolved
/// model/provider, route reason, observed outcome, prompt-cache metrics, and
/// the generation-broken honesty flag. Mirrors `RouteRecord`
/// (identity.rs:483-543). This is the natural center of any "route history"
/// / "why this provider" view.
export interface RouteRecord {
  request_id: string;
  task_id: string;
  agent_id: string;
  logical_session: string | null; // denormalized
  subagent_role: string | null;
  role_source: "native" | "heuristic" | null;
  requested_model: string | null;
  requested_provider: string | null;
  resolved_endpoint_id: string | null;
  resolved_model: string | null;
  protocol: string | null;
  route_reason:
    | "explicit"
    | "affinity"
    | "capability"
    | "fallback"
    | "no_eligible";
  http_status: number | null;
  usage_input: number | null;
  usage_output: number | null;
  cache_creation: number | null;
  cache_read: number | null;
  tool_calls: number | null; // distinct tool calls seen in the stream
  tool_names: string | null; // JSON {name: count} of observed invocations
  generation_broken: boolean;
  started_at: number;
  ended_at: number | null;
}

/// `route_migration` row — one migration event. Auth/4xx errors NEVER become
/// migrations. `from_endpoint_id`/`to_endpoint_id` are free text (no FK).
/// Mirrors `RouteMigrationRow` (store.rs:511-521).
export interface RouteMigrationRow {
  id: string;
  request_id: string;
  task_id: string;
  from_endpoint_id: string | null;
  to_endpoint_id: string | null;
  reason: string; // quota_exhausted|rate_limit|temp_5xx|timeout|policy|user_override
  detail: string | null;
  at_ms: number;
}
