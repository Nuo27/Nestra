use crate::error::{AppError, AppResult};
use crate::db;
use super::run_blocking;
use tauri::State;

// ===========================================================================
// Orchestration — routing policy.
//
// User-editable routing policies: per agent + subagent role → ordered
// (endpoint, model) route-target list. The store layer (`orchestration::store`)
// holds the full data model (run/task/route_request/route_migration/
// model_catalog), which the gateway runtime populates as it proxies requests.
// ===========================================================================

/// Frontend-facing routing-policy shape. `route_targets` is a typed ordered
/// list (vs. the store's serialized JSON column) so the IPC boundary stays
/// ergonomic. An empty list is preserved as `None` ("no targets" — routing
/// fails closed for the role until one is added).
#[derive(serde::Deserialize)]
pub struct RoutingPolicyInput {
    pub agent_id: String,
    pub role: String,
    pub route_targets: Vec<RouteTargetInput>,
    pub migrate_on_quota: bool,
    pub inject_cache_control: bool,
    pub affinity_scope: String,
}

/// One ordered (endpoint, model) pin.
#[derive(serde::Deserialize)]
pub struct RouteTargetInput {
    pub endpoint: String,
    pub model: String,
}

impl RoutingPolicyInput {
    fn into_row(self) -> AppResult<crate::orchestration::store::RoutingPolicyRow> {
        let targets: Vec<crate::orchestration::store::RouteTarget> = self
            .route_targets
            .into_iter()
            .map(|t| crate::orchestration::store::RouteTarget {
                endpoint: t.endpoint,
                model: t.model,
            })
            .collect();
        Ok(crate::orchestration::store::RoutingPolicyRow {
            agent_id: self.agent_id,
            role: self.role,
            route_targets: Some(serde_json::to_string(&targets)?),
            migrate_on_quota: self.migrate_on_quota,
            inject_cache_control: self.inject_cache_control,
            affinity_scope: self.affinity_scope,
            updated_at: chrono::Utc::now().timestamp_millis(),
        })
    }
}

#[tauri::command]
pub fn routing_policy_list(
    state: State<'_, crate::AppState>,
    agent_id: String,
) -> AppResult<Vec<crate::orchestration::store::RoutingPolicyRow>> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    crate::orchestration::store::list_routing_policies(&conn, &agent_id)
}

#[tauri::command]
pub async fn routing_policy_upsert(
    state: State<'_, crate::AppState>,
    policy: RoutingPolicyInput,
) -> AppResult<()> {
    let agent_id = policy.agent_id.clone();
    let row = policy.into_row()?;
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        crate::orchestration::store::upsert_routing_policy(&conn, &row)
    })
    .await?;
    // A policy edit can change the steady-state route (and with it the
    // context window the agent's alias advertises) — refresh it.
    super::gateway::refresh_alias_if_routed(&state, &agent_id).await;
    Ok(())
}

#[tauri::command]
pub async fn routing_policy_delete(
    state: State<'_, crate::AppState>,
    agent_id: String,
    role: String,
) -> AppResult<bool> {
    // The `*` catch-all is the mandatory default policy — a role that matches
    // no specific row must always have somewhere to land (or fail closed
    // honestly), and un-deleting it from the UI is the only supported flow.
    if role == "*" {
        return Err(AppError::Validation(
            "the '*' catch-all policy cannot be deleted — clear its targets instead".into(),
        ));
    }
    let db = state.db.clone();
    let role_clone = role.clone();
    let agent_for_db = agent_id.clone();
    let existed = run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        crate::orchestration::store::delete_routing_policy(&conn, &agent_for_db, &role_clone)
    })
    .await?;
    super::gateway::refresh_alias_if_routed(&state, &agent_id).await;
    Ok(existed)
}

// ---- orchestration: model catalog / quota state / resolve preview --------
//
// These three commands surface the control plane (capability registry,
// reactive quota state, router dry-run) to the `/orchestration` page. They
// are read-only or dry-run, so the
// health store is empty and quota is loaded from the persisted
// `last_quota_state` column. The router's dry-run runs against this snapshot.

/// Rebuild the `model_catalog` from the live endpoints + models.dev ability
/// cache, then return the full `(endpoint, model, abilities)` index. The
/// `/orchestration` Model-catalog card reads this. Call after editing
/// endpoints/abilities so the catalog reflects current config.
#[tauri::command]
pub fn orch_model_catalog_rebuild(state: State<'_, crate::AppState>) -> AppResult<Vec<ModelCatalogEntry>> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    let _ = crate::orchestration::capability_registry::rebuild(&conn)?;
    read_catalog(&conn)
}

/// Read the current `model_catalog` without rebuilding (cheap; the catalog is
/// only stale after endpoint/ability edits, which the UI rebuilds for).
#[tauri::command]
pub fn orch_model_catalog(state: State<'_, crate::AppState>) -> AppResult<Vec<ModelCatalogEntry>> {
    let conn = state.db_read.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    read_catalog(&conn)
}

/// One `(endpoint_id, model_id, abilities)` row for the catalog view. Mirrors
/// `orchestration::store::ModelCatalogRow` but with abilities parsed to a
/// typed object for the frontend.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelCatalogEntry {
    pub endpoint_id: String,
    pub model_id: String,
    pub abilities: crate::model_abilities::ModelAbilities,
}

fn read_catalog(conn: &rusqlite::Connection) -> AppResult<Vec<ModelCatalogEntry>> {
    let endpoints = db::list_endpoints(conn)?;
    let mut out = Vec::new();
    for ep in &endpoints {
        for row in crate::orchestration::store::list_model_catalog(conn, &ep.id)? {
            let abilities: crate::model_abilities::ModelAbilities =
                serde_json::from_str(&row.abilities_json).unwrap_or_default();
            out.push(ModelCatalogEntry {
                endpoint_id: ep.id.clone(),
                model_id: row.model_id,
                abilities,
            });
        }
    }
    Ok(out)
}

/// Dry-run the router against the live policy + catalog + quota + health
/// Lets the user see exactly
/// which `(endpoint, model, reason)` a Task with these parameters would
/// resolve to, without sending any traffic. `requested_provider` /
/// `requested_model` are optional agent-stated hints.
#[tauri::command]
pub fn orch_resolve_preview(
    state: State<'_, crate::AppState>,
    agent_id: String,
    role: Option<String>,
    requested_provider: Option<String>,
    requested_model: Option<String>,
    reasoning: Option<bool>,
    tool_call: Option<bool>,
    vision: Option<bool>,
    context_floor: Option<u64>,
) -> AppResult<ResolvePreview> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    // Ensure the catalog reflects current endpoint config before resolving.
    let _ = crate::orchestration::capability_registry::rebuild(&conn)?;

    let mut ctx = crate::orchestration::TaskContext::new_task(&agent_id, None);
    if let Some(role) = role {
        // The user supplies a policy-role key directly (e.g. "main", "*",
        // "claude:researcher", "tier:haiku"); parse it into the SubagentRole
        // (+ budget tier for tier keys) for display.
        let (parsed, tier) = parse_role_key(&role);
        ctx.subagent_role = parsed;
        ctx.budget_tier = tier;
    }
    ctx.requested_provider = requested_provider;
    ctx.requested_model = requested_model;
    ctx.required_capabilities = crate::orchestration::CapabilityReq {
        reasoning: reasoning.unwrap_or(false),
        tool_call: tool_call.unwrap_or(false),
        vision: vision.unwrap_or(false),
        context_floor,
    };

    let health = crate::orchestration::health::ProviderHealth::new();
    let quota = crate::orchestration::quota_state::load_all_from_db(&conn)?;
    let affinity = crate::orchestration::router::RouteAffinity::new();
    let inputs = crate::orchestration::router::RouterInputs {
        conn: &conn,
        health: &health,
        quota: &quota,
        affinity: &affinity,
    };
    let route = crate::orchestration::router::resolve(&ctx, &inputs)?;
    // The resolved model's context window from the catalog — the number the
    // routed alias advertises, so the UI can show "what will my config
    // actually give the agent" next to the policy editor.
    let context_window = crate::orchestration::store::list_model_catalog(&conn, &route.endpoint_id)
        .unwrap_or_default()
        .into_iter()
        .find(|r| r.model_id == route.model)
        .and_then(|r| {
            serde_json::from_str::<crate::model_abilities::ModelAbilities>(&r.abilities_json).ok()
        })
        .and_then(|a| a.limit.map(|l| l.context));
    Ok(ResolvePreview {
        endpoint_id: route.endpoint_id,
        model: route.model,
        reason: route.reason.as_str().to_string(),
        cache_strategy: format!("{:?}", route.cache_strategy).to_lowercase(),
        requested_model: ctx.requested_model.clone(),
        requested_provider: ctx.requested_provider.clone(),
        context_window,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ResolvePreview {
    pub endpoint_id: String,
    pub model: String,
    pub reason: String,
    pub cache_strategy: String,
    pub requested_model: Option<String>,
    pub requested_provider: Option<String>,
    /// The resolved model's context window (tokens) from the model catalog —
    /// the number a routed alias advertises. `None` when the catalog carries
    /// no abilities for the resolved model.
    pub context_window: Option<u64>,
}

/// Parse a policy-role key ("main" | "*" | "claude:x" | "pi:x" |
/// "opencode:x" | "tier:haiku/sonnet/opus") back into a
/// [`SubagentRole`] (+ [`BudgetTier`] for tier keys) for the resolve-preview
/// display. Tier keys ride the Main role — the tier feeds the policy lookup
/// chain, not the subagent role. Unknown prefixes default to Main.
fn parse_role_key(key: &str) -> (crate::orchestration::SubagentRole, Option<crate::orchestration::BudgetTier>) {
    use crate::orchestration::SubagentRole;
    if key == "main" || key == "*" {
        return (SubagentRole::Main, None);
    }
    if let Some(t) = key.strip_prefix("tier:") {
        let tier = match t {
            "haiku" => crate::orchestration::BudgetTier::Haiku,
            "sonnet" => crate::orchestration::BudgetTier::Sonnet,
            "opus" => crate::orchestration::BudgetTier::Opus,
            _ => return (SubagentRole::Main, None),
        };
        return (SubagentRole::Main, Some(tier));
    }
    if let Some(name) = key.strip_prefix("claude:") {
        return (SubagentRole::ClaudeAgent { name: name.into() }, None);
    }
    if let Some(role) = key.strip_prefix("pi:") {
        return (SubagentRole::PiSubagent { role: role.into() }, None);
    }
    if let Some(name) = key.strip_prefix("opencode:") {
        return (SubagentRole::OpenCodeAgent { name: name.into() }, None);
    }
    (SubagentRole::Main, None)
}

/// Full route history for one task: every attempt's credential-free
/// `RouteRecord` (requested vs resolved model, reason, observed outcome,
/// generation-broken flag), oldest-first. This is the data behind the UI's
/// "why this provider/model" trace.
#[tauri::command]
pub fn orch_route_history(
    state: State<'_, crate::AppState>,
    task_id: String,
) -> AppResult<Vec<crate::orchestration::identity::RouteRecord>> {
    let conn = state.db_read.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    crate::orchestration::store::route_history_for_task(&conn, &task_id)
}

/// Migration events for one task (the route_migration rows). Each carries the
/// from/to endpoints, the reason (quota_exhausted | rate_limit | temp_5xx |
/// timeout | retries_exhausted), and a detail note.
#[tauri::command]
pub fn orch_migrations(
    state: State<'_, crate::AppState>,
    task_id: String,
) -> AppResult<Vec<crate::orchestration::store::RouteMigrationRow>> {
    let conn = state.db_read.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    crate::orchestration::store::migrations_for_task(&conn, &task_id)
}

/// Task summaries for every task the gateway has observed, most-recently-
/// active first. Powers the `/orchestration` Active-tasks card. `limit`
/// caps the window (default 50).
#[tauri::command]
pub fn orch_tasks(
    state: State<'_, crate::AppState>,
    limit: Option<i64>,
) -> AppResult<Vec<crate::orchestration::store::TaskSummary>> {
    let conn = state.db_read.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    crate::orchestration::store::task_summaries(&conn, limit.unwrap_or(50).max(1).min(500))
}

/// Distinct subagent roles observed for one agent (from `route_request`),
/// most-recently-active first. Feeds the routing-policy editor's suggestions
/// and the agent detail page's "detected roles" card.
#[tauri::command]
pub fn orch_detected_roles(
    state: State<'_, crate::AppState>,
    agent_id: String,
    limit: Option<i64>,
) -> AppResult<Vec<crate::orchestration::store::DetectedRoleSummary>> {
    let conn = state.db_read.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    crate::orchestration::store::detected_roles(&conn, &agent_id, limit.unwrap_or(20).max(1).min(100))
}

/// Task summaries whose `logical_session` matches a session id. Powers the
/// per-session lineage on `/sessions/:id`.
#[tauri::command]
pub fn orch_session_tasks(
    state: State<'_, crate::AppState>,
    logical_session: String,
    limit: Option<i64>,
) -> AppResult<Vec<crate::orchestration::store::TaskSummary>> {
    let conn = state.db_read.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    crate::orchestration::store::tasks_for_session(
        &conn,
        &logical_session,
        limit.unwrap_or(20).max(1).min(100),
    )
}

/// Usage dashboard rows: per-(day, agent, endpoint, model) token + cache
/// counts with read-time USD cost. Prices resolve endpoint-level ability
/// overrides first, then the models.dev catalog; rows with no known price
/// carry `cost_usd: null` (unknown spend, not free).
#[tauri::command]
pub fn orch_usage_summary(
    state: State<'_, crate::AppState>,
    agent_id: Option<String>,
    days: Option<u64>,
) -> AppResult<Vec<crate::orchestration::store::UsageRow>> {
    use std::collections::HashMap;
    let conn = state.db_read.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    let global = crate::model_abilities::load_index(&conn)?;
    let mut overrides: HashMap<String, HashMap<String, crate::model_abilities::ModelAbilities>> =
        HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, model_abilities_json FROM provider_endpoint
             WHERE model_abilities_json IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, json) = row?;
            overrides.insert(id, crate::model_abilities::parse_overrides(Some(&json)));
        }
    }
    let prices = |endpoint: &str, model: &str| -> Option<crate::model_abilities::CostPerMtok> {
        let norm = crate::model_abilities::normalize(model);
        overrides
            .get(endpoint)
            .and_then(|m| m.get(model).or_else(|| m.get(&norm)))
            .and_then(|a| a.cost.clone())
            .or_else(|| global.get(model).and_then(|a| a.cost.clone()))
    };
    crate::orchestration::store::usage_summary_rows(&conn, agent_id.as_deref(), days, &prices)
}

/// Clear ALL gateway observability data (tasks + cascaded requests/migrations,
/// the lifetime usage rollup, the affinity snapshot) — the Settings → Data
/// danger action. Configuration (endpoints, policies, bindings, secrets,
/// imported sessions) is untouched. The in-memory affinity table is cleared
/// FIRST: the router re-persists its snapshot on the next request / at exit,
/// which would silently undo the wipe within this process. Write path → the
/// UI `db` connection.
#[tauri::command]
pub fn orch_obs_clear(state: State<'_, crate::AppState>) -> AppResult<()> {
    state.orch_affinity.clear();
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    crate::orchestration::store::clear_observability(&conn)
}
