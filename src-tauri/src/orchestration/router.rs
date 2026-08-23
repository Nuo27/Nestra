//! The orchestration router — resolves a [`TaskContext`] to a [`ResolvedRoute`].
//!
//! The resolution algorithm runs against the policy table + an in-memory
//! health + affinity store, so it is fully testable and the orchestration
//! surface can show a dry-run resolution preview. The gateway is what actually
//! *uses* the route at request time.
//!
//! ## Resolution order
//!
//! 1. **Explicit** — the Task's `requested_provider` + `requested_model`, if
//!    both are set and the endpoint is healthy + quota-ok. Honors a user/agent
//!    pin without further ranking.
//! 2. **Affinity** — for a task-grain affinity scope, reuse the last route
//!    used by this `task_id` if it is still healthy + quota-ok. This is the
//!    cache-friendly path (keeping a task on one provider is what makes
//!    prompt-cache creation amortize). Session-grain affinity is a weaker
//!    hint, used only when task id is unknown.
//! 3. **Policy targets** — the role's ordered `route_targets` list (explicit
//!    `(endpoint, model)` pins); the first entry that exists and is healthy +
//!    quota-ok wins. Failures walk the list: the migration loop marks failed
//!    endpoints on the context and re-resolves. No capability filtering —
//!    an explicit target is the user's intent.
//! 4. **Fail closed** — `RouteReason::NoEligible` with no endpoint.
//!
//! Health and quota signals gate every step: a degraded or quota-exhausted
//! endpoint is skipped.

use std::collections::HashMap;
use std::sync::Mutex;

use rusqlite::Connection;
use uuid::Uuid;

use crate::config_writer::ProviderKind;
use crate::db;
use crate::error::{AppError, AppResult};

use super::health::ProviderHealth;
use super::identity::{
    CacheStrategy, CredentialHandle, ResolvedRoute, RouteReason, TaskContext,
};
use super::quota_state::QuotaState;
use super::store;

/// In-memory route affinity: the last route a task (or session) used, so the
/// router can reuse it and protect prompt cache. Process-global
/// it into `AppState`; tests construct their own.
pub struct RouteAffinity {
    inner: Mutex<HashMap<AffinityKey, AffinityValue>>,
    /// Last session-snapshot persist (Unix millis) — the debounce that keeps
    /// `record` off the DB on the per-request hot path.
    last_persist_ms: Mutex<Option<i64>>,
}

/// `setting_kv` key for the persisted session-grain affinity snapshot.
const AFFINITY_PERSIST_KEY: &str = "route_affinity";
/// Minimum spacing between affinity snapshot writes.
const AFFINITY_PERSIST_DEBOUNCE_MS: i64 = 5_000;

/// The grain an affinity entry is keyed at. Matches `routing_policy.affinity_scope`.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum AffinityKey {
    Task(Uuid),
    Session { agent_id: String, logical_session_id: String },
}

#[derive(Debug, Clone)]
struct AffinityValue {
    endpoint_id: String,
    model: String,
}

/// Persisted session-grain affinity row (`setting_kv["route_affinity"]`).
/// Credential-free: endpoint id + model id only.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SessionAffinityRow {
    agent_id: String,
    logical_session_id: String,
    endpoint_id: String,
    model: String,
}

impl Default for RouteAffinity {
    fn default() -> Self {
        Self::new()
    }
}

impl RouteAffinity {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            last_persist_ms: Mutex::new(None),
        }
    }

    /// Cap on live affinity entries. The map was unbounded — every distinct
    /// task/session left a row forever. When the cap is exceeded the OLDEST
    /// task entries are dropped (HashMap iteration order is not insertion
    /// order, so this is approximate; the bound is what matters, not which
    /// entry goes).
    const MAX_ENTRIES: usize = 4096;

    fn evict_if_needed(map: &mut HashMap<AffinityKey, AffinityValue>) {
        if map.len() <= Self::MAX_ENTRIES {
            return;
        }
        let excess = map.len() - Self::MAX_ENTRIES;
        // Drop up to `excess` task-grain entries (never session entries —
        // those are few and high-value).
        let mut dropped = 0usize;
        let task_keys: Vec<AffinityKey> = map
            .keys()
            .filter(|k| matches!(k, AffinityKey::Task(_)))
            .take(excess)
            .cloned()
            .collect();
        for k in task_keys {
            map.remove(&k);
            dropped += 1;
            if dropped >= excess {
                break;
            }
        }
    }

    /// Record the route chosen for a task so the next request in the same
    /// task reuses it (affinity). `conn` is the caller's already-held gateway
    /// connection — used for the debounced session-grain snapshot (Smart
    /// Gateway fix 3: a restart otherwise loses the session's provider pin
    /// and the prompt-cache prefix with it).
    pub fn record(
        &self,
        conn: &rusqlite::Connection,
        ctx: &TaskContext,
        endpoint_id: &str,
        model: &str,
    ) {
        let mut map = self.inner.lock().expect("affinity lock poisoned");
        Self::evict_if_needed(&mut map);
        map.insert(
            AffinityKey::Task(ctx.task_id),
            AffinityValue {
                endpoint_id: endpoint_id.to_string(),
                model: model.to_string(),
            },
        );
        // Session-grain affinity is recorded only when the session is known.
        // It is a weaker hint than the task entry, but the session route MUST
        // still refresh on every request — `or_insert` kept the FIRST route
        // forever, so a session whose endpoint degraded kept pinning to it.
        if let Some(sid) = &ctx.logical_session_id {
            map.insert(
                AffinityKey::Session {
                    agent_id: ctx.agent_id.clone(),
                    logical_session_id: sid.clone(),
                },
                AffinityValue {
                    endpoint_id: endpoint_id.to_string(),
                    model: model.to_string(),
                },
            );
        }
        drop(map);
        // Debounced snapshot: at most one setting_kv write per interval no
        // matter how hot the proxy path is.
        let now = chrono::Utc::now().timestamp_millis();
        let due = self
            .last_persist_ms
            .lock()
            .expect("affinity persist lock poisoned")
            .map_or(true, |t| now - t >= AFFINITY_PERSIST_DEBOUNCE_MS);
        if due {
            self.persist_sessions(conn);
        }
    }

    /// Snapshot the SESSION-grain entries to `setting_kv`. Task entries are
    /// deliberately not persisted: a `task_id` lives one HTTP lifecycle
    /// (retry/migration reuse), so the cross-restart-valuable part is the
    /// session pin. Credential-free (endpoint id + model id only).
    fn persist_sessions(&self, conn: &rusqlite::Connection) {
        let map = self.inner.lock().expect("affinity lock poisoned");
        let mut rows: Vec<SessionAffinityRow> = map
            .iter()
            .filter_map(|(k, v)| match k {
                AffinityKey::Session {
                    agent_id,
                    logical_session_id,
                } => Some(SessionAffinityRow {
                    agent_id: agent_id.clone(),
                    logical_session_id: logical_session_id.clone(),
                    endpoint_id: v.endpoint_id.clone(),
                    model: v.model.clone(),
                }),
                AffinityKey::Task(_) => None,
            })
            .collect();
        drop(map);
        rows.sort_by(|a, b| a.logical_session_id.cmp(&b.logical_session_id));
        let Ok(value) = serde_json::to_value(&rows) else {
            return;
        };
        if crate::db::set_setting(conn, AFFINITY_PERSIST_KEY, &value).is_ok() {
            *self
                .last_persist_ms
                .lock()
                .expect("affinity persist lock poisoned") = Some(chrono::Utc::now().timestamp_millis());
        }
    }

    /// Restore session-grain affinity at startup. Task entries stay
    /// in-memory-only (ephemeral by nature).
    pub fn load_sessions(&self, conn: &rusqlite::Connection) {
        let Ok(Some(v)) = crate::db::get_setting(conn, AFFINITY_PERSIST_KEY) else {
            return;
        };
        let Ok(rows) = serde_json::from_value::<Vec<SessionAffinityRow>>(v) else {
            return;
        };
        let mut map = self.inner.lock().expect("affinity lock poisoned");
        for r in rows {
            map.insert(
                AffinityKey::Session {
                    agent_id: r.agent_id,
                    logical_session_id: r.logical_session_id,
                },
                AffinityValue {
                    endpoint_id: r.endpoint_id,
                    model: r.model,
                },
            );
        }
    }

    /// Write the session snapshot NOW (no debounce) — the exit path's final
    /// flush so the ≤5s debounce tail doesn't die with the process.
    pub fn flush_sessions(&self, conn: &rusqlite::Connection) {
        self.persist_sessions(conn);
    }

    /// Look up the last route for `ctx`'s affinity grain.
    pub fn lookup(&self, ctx: &TaskContext, scope: AffinityScope) -> Option<(String, String)> {
        let map = self.inner.lock().expect("affinity lock poisoned");
        match scope {
            AffinityScope::Task => map
                .get(&AffinityKey::Task(ctx.task_id))
                .map(|v| (v.endpoint_id.clone(), v.model.clone())),
            AffinityScope::Session => ctx
                .logical_session_id
                .as_ref()
                .and_then(|sid| {
                    map.get(&AffinityKey::Session {
                        agent_id: ctx.agent_id.clone(),
                        logical_session_id: sid.clone(),
                    })
                    .map(|v| (v.endpoint_id.clone(), v.model.clone()))
                }),
            AffinityScope::None => None,
        }
    }

    pub fn clear(&self) {
        self.inner.lock().expect("affinity lock poisoned").clear();
    }
}

/// The affinity grain a policy requests. Mirrors the `affinity_scope` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AffinityScope {
    Task,
    Session,
    None,
}

impl AffinityScope {
    pub fn from_policy_str(s: &str) -> Self {
        match s {
            "session" => AffinityScope::Session,
            "none" => AffinityScope::None,
            _ => AffinityScope::Task, // default
        }
    }
}

/// Inputs the router reads. Grouped so tests can inject fakes and the gateway
/// can wire the real process-global stores.
pub struct RouterInputs<'a> {
    pub conn: &'a Connection,
    pub health: &'a ProviderHealth,
    pub quota: &'a QuotaState,
    pub affinity: &'a RouteAffinity,
}

/// Resolve a [`TaskContext`] to a [`ResolvedRoute`] against the live policy +
/// registry + health + quota + affinity.
///
/// Returns a route even on failure (`reason = NoEligible`, `endpoint_id`
/// empty) so the caller can record the attempt uniformly; the only `Err`
/// path is a DB/lookup failure that means we couldn't even attempt resolution.
pub fn resolve(ctx: &TaskContext, inputs: &RouterInputs<'_>) -> AppResult<ResolvedRoute> {
    let policy = store::routing_policy_for(
        inputs.conn,
        &ctx.agent_id,
        &ctx.policy_role_key(),
        ctx.budget_tier.as_ref(),
    )?;
    let scope = AffinityScope::from_policy_str(&policy.affinity_scope);

    // 1. Explicit pin (highest priority). Health + quota still gate it; there
    // is no capability or model-list filtering — an explicit pin is the
    // caller's stated intent.
    if let (Some(ep), Some(model)) = (&ctx.requested_provider, &ctx.requested_model) {
        if endpoint_exists(inputs.conn, ep)?
            && !inputs.health.is_degraded(ep, model)
            && !inputs.quota.is_exhausted(ep)
        {
            let route = build_route(ctx, ep, model, RouteReason::Explicit, inputs, policy.inject_cache_control)?;
            inputs.affinity.record(inputs.conn, ctx, ep, model);
            return Ok(route);
        }
    }

    // 2. Affinity (cache-friendly reuse).
    if let Some((ep, model)) = inputs.affinity.lookup(ctx, scope) {
        if endpoint_exists(inputs.conn, &ep)?
            && !inputs.health.is_degraded(&ep, &model)
            && !inputs.quota.is_exhausted(&ep)
        {
            let route = build_route(ctx, &ep, &model, RouteReason::Affinity, inputs, policy.inject_cache_control)?;
            // Affinity hit re-records (refreshes at_ms).
            inputs.affinity.record(inputs.conn, ctx, &ep, &model);
            return Ok(route);
        }
    }

    // 3. Policy targets: the role's ordered (endpoint, model) list; the
    // first entry that exists and is healthy + quota-ok wins. Endpoints
    // that already failed on THIS request (migration loop) are skipped so
    // a re-resolve walks the list forward.
    if let Some((ep, model, reason)) = pick_by_targets(ctx, &policy, inputs)? {
        let route = build_route(ctx, &ep, &model, reason, inputs, policy.inject_cache_control)?;
        inputs.affinity.record(inputs.conn, ctx, &ep, &model);
        return Ok(route);
    }

    // 4. Fail closed.
    Ok(ResolvedRoute {
        endpoint_id: String::new(),
        provider_kind: ProviderKind::Custom,
        model: String::new(),
        base_url: String::new(),
        protocol: ProviderKind::Custom,
        credential: CredentialHandle::new(String::new(), String::new()),
        cache_strategy: CacheStrategy::Off,
        reason: RouteReason::NoEligible,
        route_lineage: Vec::new(),
    })
}

/// The steady-state route for one agent: what `resolve` picks for a fresh
/// main-role task under the current policy — i.e. the model a routed agent's
/// requests actually land on absent affinity/failover. Used to advertise the
/// REAL model abilities (context window above all) in agent configs
/// (`GatewayAlias`) and in the gateway's `GET /v1/models`.
#[derive(Debug, Clone)]
pub struct SteadyStateRoute {
    pub endpoint_id: String,
    pub model_id: String,
    /// Merged abilities from the model catalog (`None` when the catalog has no
    /// row for the resolved model — callers degrade to placeholders).
    pub abilities: Option<crate::model_abilities::ModelAbilities>,
}

/// Best-effort steady-state resolution for `tier` (`None` = unclassified /
/// main). `None` is returned on ANY failure (no eligible endpoint, DB error)
/// so callers can fall back to conservative placeholder values without
/// failing the config write. Callers should ensure the catalog is fresh
/// (`capability_registry::rebuild`) first.
pub fn steady_state(
    inputs: &RouterInputs<'_>,
    agent_id: &str,
    tier: Option<&super::identity::BudgetTier>,
) -> Option<SteadyStateRoute> {
    let mut ctx = TaskContext::new_task(agent_id, None);
    ctx.budget_tier = tier.copied();
    let route = resolve(&ctx, inputs).ok()?;
    if route.endpoint_id.is_empty() || route.model.is_empty() {
        return None; // fail-closed
    }
    let abilities = store::list_model_catalog(inputs.conn, &route.endpoint_id)
        .ok()?
        .into_iter()
        .find(|r| r.model_id == route.model)
        .and_then(|r| serde_json::from_str(&r.abilities_json).ok());
    Some(SteadyStateRoute {
        endpoint_id: route.endpoint_id,
        model_id: route.model,
        abilities,
    })
}

/// Walk the policy's ordered route-target list and return the first
/// `(endpoint, model)` whose endpoint exists, is healthy, quota-ok, and has
/// not already failed on this request (migration exclusion). No capability
/// filtering — an explicit target is the user's stated intent. `None` when
/// the list is empty or nothing qualifies.
fn pick_by_targets(
    ctx: &TaskContext,
    policy: &store::RoutingPolicyRow,
    inputs: &RouterInputs<'_>,
) -> AppResult<Option<(String, String, RouteReason)>> {
    for target in policy.targets() {
        if ctx.failed_endpoints.iter().any(|e| e == &target.endpoint) {
            continue;
        }
        if !endpoint_exists(inputs.conn, &target.endpoint)? {
            tracing::warn!(
                agent = %ctx.agent_id,
                endpoint = %target.endpoint,
                "routing: policy target references a missing endpoint — skipping"
            );
            continue;
        }
        if inputs.health.is_degraded(&target.endpoint, &target.model)
                || inputs.quota.is_exhausted(&target.endpoint)
            {
            continue;
        }
        return Ok(Some((
            target.endpoint.clone(),
            target.model.clone(),
            RouteReason::Policy,
        )));
    }
    Ok(None)
}

/// Pure-read answer to "if the current endpoint fails, is there anywhere
/// else to go?" — the policy's route targets minus `excluding`, filtered by
/// the same health/quota/existence gates as [`pick_by_targets`]. The
/// migration loop uses this to FAST-FAIL: with no remaining target (a
/// single-target policy, or everything else already failed), retrying the
/// same endpoint just burns latency — surface the error promptly instead.
/// No affinity side effects.
pub fn failover_targets(
    inputs: &RouterInputs<'_>,
    ctx: &TaskContext,
    excluding: &[String],
) -> AppResult<Vec<store::RouteTarget>> {
    let policy = store::routing_policy_for(
        inputs.conn,
        &ctx.agent_id,
        &ctx.policy_role_key(),
        ctx.budget_tier.as_ref(),
    )?;
    let mut remaining = Vec::new();
    for target in policy.targets() {
        if excluding.iter().any(|e| e == &target.endpoint) {
            continue;
        }
        if !endpoint_exists(inputs.conn, &target.endpoint)? {
            continue;
        }
        if inputs.health.is_degraded(&target.endpoint, &target.model)
            || inputs.quota.is_exhausted(&target.endpoint)
        {
            continue;
        }
        remaining.push(target);
    }
    Ok(remaining)
}

fn endpoint_exists(conn: &Connection, endpoint_id: &str) -> AppResult<bool> {
    Ok(db::get_endpoint(conn, endpoint_id)?.is_some())
}

/// Build a [`ResolvedRoute`] for a chosen `(endpoint, model)`, resolving the
/// protocol/base_url and a placeholder credential. gateway will refresh
/// the credential from `secrets::get` at request time; dry-run path
/// leaves the key empty (the [`CredentialHandle`] is a structural placeholder
/// so the type contract is honored end-to-end).
fn build_route(
    ctx: &TaskContext,
    endpoint_id: &str,
    model: &str,
    reason: RouteReason,
    inputs: &RouterInputs<'_>,
    inject_cache: bool,
) -> AppResult<ResolvedRoute> {
    let ep = db::get_endpoint(inputs.conn, endpoint_id)?
        .ok_or_else(|| AppError::NotFound(format!("endpoint {endpoint_id}")))?;
    // Pick the protocol row matching the inbound direction: an OpenAI-shape
    // request needs the endpoint's OpenAI-compatible base (e.g. z-ai's
    // `/api/paas/v4`), an Anthropic request the Anthropic row. Fall back to
    // the first row when the direction has no match — mock endpoints often
    // declare a single row that serves both shapes. `None` hint keeps the
    // historical first-row behavior. Default to Custom when no rows exist
    // (the gateway can still dial a bare base_url, but routing is degraded
    // until a protocol is set).
    //
    // Same-gateway bridge: when an endpoint declares BOTH an anthropic and
    // an openai row on the SAME base_url (opencode-go serves both protocols
    // on one gateway), an Anthropic inbound picks the openai row so the
    // gateway's conversion layer speaks the official per-model protocol
    // (some models reject the Anthropic wire outright — grok/kimi/mimo).
    // Distinct base_urls (DeepSeek/Moonshot real dual endpoints) keep the
    // direction-matched row.
    let (protocol, base_url) = {
        let row = match ctx.protocol_hint {
            Some(ProviderKind::Anthropic) => {
                let anthropic_row = ep
                    .protocols
                    .iter()
                    .find(|p| parse_kind(&p.protocol) == ProviderKind::Anthropic);
                let openai_row = ep
                    .protocols
                    .iter()
                    .find(|p| parse_kind(&p.protocol) == ProviderKind::Openai);
                match (anthropic_row, openai_row) {
                    (Some(a), Some(o))
                        if a.base_url.trim_end_matches('/') == o.base_url.trim_end_matches('/') =>
                    {
                        Some(o)
                    }
                    (Some(_), _) => anthropic_row,
                    _ => openai_row.or_else(|| ep.protocols.first()),
                }
            }
            Some(kind) => ep
                .protocols
                .iter()
                .find(|p| parse_kind(&p.protocol) == kind)
                .or_else(|| ep.protocols.first()),
            None => ep.protocols.first(),
        };
        row.map(|p| (parse_kind(&p.protocol), p.base_url.clone()))
            .unwrap_or((ProviderKind::Custom, String::new()))
    };
    // Off-direction fallback is a 404 factory: an OpenAI inbound dialing an
    // anthropic base (or vice versa) joins the wrong path and the upstream
    // answers "page not found". Name it so the config fix is obvious.
    if let Some(hint) = ctx.protocol_hint {
        let row_kind = protocol;
        let has_match = ep
            .protocols
            .iter()
            .any(|p| parse_kind(&p.protocol) == hint);
        if !has_match && row_kind != hint {
            tracing::warn!(
                agent = %ctx.agent_id,
                endpoint = %endpoint_id,
                inbound = %hint.as_str(),
                base = %base_url,
                "routing: no protocol row for the inbound direction — dialing an off-direction base; add a matching protocol row on the Providers page if this 404s upstream"
            );
        }
    }

    // Wire selection: the per-model `api` dialect (from the catalog's merged
    // abilities) decides the upstream wire when known — an anthropic inbound
    // for a responses-class model (grok) must be converted to the Responses
    // API, an openai-class model (kimi) to Chat Completions, an
    // anthropic-class model (deepseek) dials Messages directly (native
    // caching). `None` api follows the row protocol — the historical
    // behavior. Chat inbounds keep Chat for anthropic-class models (they
    // empirically accept the chat wire too) and only switch to Responses
    // for responses-class models.
    let wire = wire_for_model(ctx, inputs.conn, endpoint_id, model, protocol)?;

    // Cache strategy is wire-aware: Anthropic-wire requests can use explicit
    // caching (gated by policy.inject_cache_control — the gateway honors
    // it); OpenRouter/DeepSeek are auto-cache; everything else off. This is
    // a hint, not a constraint (plan §7).
    let cache_strategy = if inject_cache && wire == ProviderKind::Anthropic {
        CacheStrategy::AnthropicExplicit
    } else if is_openrouter(&base_url) {
        CacheStrategy::OpenRouterPassthrough
    } else if is_deepseek(&base_url) {
        CacheStrategy::DeepSeekAuto
    } else {
        CacheStrategy::Off
    };

    // Lineage: prior request ids for this task. The gateway records each
    // request_id as it issues.
    Ok(ResolvedRoute {
        endpoint_id: endpoint_id.to_string(),
        provider_kind: protocol,
        model: model.to_string(),
        base_url,
        protocol: wire,
        credential: CredentialHandle::new(endpoint_id.to_string(), String::new()),
        cache_strategy,
        reason,
        route_lineage: Vec::new(),
    })
}

fn parse_kind(s: &str) -> ProviderKind {
    match s {
        "anthropic" => ProviderKind::Anthropic,
        "openai-comp" => ProviderKind::Openai,
        // Legacy alias: a stored "openrouter" row is OpenAI Chat Completions
        // wire (the `ProviderKind::Openrouter` variant was removed — OpenRouter
        // now binds via anthropic/openai rows). `build_v1` normalizes such rows
        // to "openai-comp", so this only covers any straggler.
        "openrouter" => ProviderKind::Openai,
        "response-api" => ProviderKind::Responses,
        _ => ProviderKind::Custom,
    }
}

/// Derive the upstream wire from the model's `api` dialect + inbound hint.
///
/// The model's `api` (from the catalog's merged abilities — models.dev +
/// corrections + user overrides) states which wire it is officially served
/// on. `None` means "unknown, follow the endpoint" (historical behavior).
/// The match table:
///   - `responses`-class models (grok-4.5, gpt-5.6-luna) ALWAYS go to the
///     Responses API — the chat wire is broken for them upstream (503).
///   - `openai`-class models (kimi/hy3/mimo) → Chat Completions (they reject
///     the Anthropic wire outright).
///   - `anthropic`-class models on an Anthropic inbound → Messages directly
///     (native caching, no conversion); on a Chat inbound they stay on Chat
///     (they empirically accept both); on a Responses inbound they convert
///     to Messages.
///   - `None` → the row protocol (historical behavior).
fn wire_for_model(
    ctx: &TaskContext,
    conn: &rusqlite::Connection,
    endpoint_id: &str,
    model: &str,
    row: ProviderKind,
) -> AppResult<ProviderKind> {
    let api = catalog_api(conn, endpoint_id, model)?;
    Ok(match (ctx.protocol_hint, api.as_deref()) {
        (_, Some("response-api")) => ProviderKind::Responses,
        (_, Some("openai-comp")) => ProviderKind::Openai,
        (Some(ProviderKind::Anthropic), Some("anthropic")) => ProviderKind::Anthropic,
        (Some(ProviderKind::Anthropic), _) => row,
        // A chat inbound follows the endpoint's row: an openai row stays
        // native Chat, an endpoint whose ONLY row is Anthropic (e.g.
        // MiniMax-M3 on `…/anthropic`) gets the Messages wire and is bridged
        // by the OpenAI handler.
        (Some(ProviderKind::Openai), _) => row,
        (Some(ProviderKind::Responses), Some("anthropic")) => ProviderKind::Anthropic,
        // Custom inbounds aren't served by the gateway (only
        // anthropic/openai/responses paths dispatch); fall through to the row.
        (Some(ProviderKind::Custom), _) => row,
        (Some(ProviderKind::Responses), _) => row,
        (None, _) => row,
    })
}

/// The model's `api` dialect from the catalog's abilities_json. `None` when
/// the model is not cataloged (e.g. the "nestra" alias) or has no `api`.
fn catalog_api(
    conn: &rusqlite::Connection,
    endpoint_id: &str,
    model: &str,
) -> AppResult<Option<String>> {
    let Some(row) = store::list_model_catalog(conn, endpoint_id)?
        .into_iter()
        .find(|r| r.model_id == model)
    else {
        return Ok(None);
    };
    let abilities: crate::model_abilities::ModelAbilities =
        serde_json::from_str(&row.abilities_json).unwrap_or_default();
    Ok(abilities.api)
}

fn is_openrouter(url: &str) -> bool {
    url.contains("openrouter.ai")
}

fn is_deepseek(url: &str) -> bool {
    url.contains("deepseek.com")
}

#[cfg(test)]
mod tests;
