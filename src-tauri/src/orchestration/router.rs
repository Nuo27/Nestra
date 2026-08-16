//! The orchestration router — resolves a [`TaskContext`] to a [`ResolvedRoute`].
//!
//! The resolution algorithm runs against the policy table + capability
//! registry + an in-memory health + affinity store, so it is fully testable
//! and the orchestration surface can show a dry-run resolution preview. The
//! gateway is what actually *uses* the route at request time.
//!
//! ## Resolution order
//!
//! 1. **Explicit** — the Task's `requested_provider` + `requested_model`, if
//!    both are set and the endpoint is healthy + eligible. Honors a user/agent
//!    pin without further ranking.
//! 2. **Affinity** — for a task-grain affinity scope, reuse the last route
//!    used by this `task_id` if it is still healthy + quota-ok + eligible.
//!    This is the cache-friendly path (keeping a task on one provider is what
//!    makes prompt-cache creation amortize). Session-grain affinity is a
//!    weaker hint, used only when task id is unknown.
//! 3. **Capability** — rank capability-eligible endpoints by (cost, latency,
//!    cache locality) and pick the best. Cost/latency data is not yet modeled,
//!    so the ranking is deterministic-but-stable: policy
//!    `preferred_endpoints` order first, then configured-endpoint order.
//! 4. **Fail closed** — `RouteReason::NoEligible` with no endpoint.
//!
//! Health and quota signals gate every step: a degraded or quota-exhausted
//! endpoint is skipped unless it is the *only* option AND policy allows
//! last-resort use.

use std::collections::HashMap;
use std::sync::Mutex;

use rusqlite::Connection;
use uuid::Uuid;

use crate::config_writer::ProviderKind;
use crate::db;
use crate::error::{AppError, AppResult};

use super::capability_registry;
use super::health::ProviderHealth;
use super::identity::{
    CapabilityReq, CacheStrategy, CredentialHandle, ResolvedRoute, RouteReason, TaskContext,
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
    let policy = store::routing_policy_for(inputs.conn, &ctx.agent_id, &ctx.policy_role_key())?;
    let scope = AffinityScope::from_policy_str(&policy.affinity_scope);

    // 1. Explicit pin (highest priority). Policy still applies: the pinned
    // endpoint must be capable of the request AND its model must pass the
    // policy's allowed-models whitelist — an explicit pin bypassing both
    // would let any pinned (endpoint, model) dodge routing policy.
    if let (Some(ep), Some(model)) = (&ctx.requested_provider, &ctx.requested_model) {
        if endpoint_exists(inputs.conn, ep)?
            && !inputs.health.is_degraded(ep)
            && !inputs.quota.is_exhausted(ep)
            && model_eligible_for_req(inputs.conn, ep, model, &ctx.required_capabilities)?
            && model_allowed(&policy.allowed_models, model)
        {
            let route = build_route(ctx, ep, model, RouteReason::Explicit, inputs, policy.inject_cache_control)?;
            inputs.affinity.record(inputs.conn, ctx, ep, model);
            return Ok(route);
        }
    }

    // 2. Affinity (cache-friendly reuse).
    if let Some((ep, model)) = inputs.affinity.lookup(ctx, scope) {
        if endpoint_exists(inputs.conn, &ep)?
            && !inputs.health.is_degraded(&ep)
            && !inputs.quota.is_exhausted(&ep)
            && model_eligible_for_req(inputs.conn, &ep, &model, &ctx.required_capabilities)?
        {
            let route = build_route(ctx, &ep, &model, RouteReason::Affinity, inputs, policy.inject_cache_control)?;
            // Affinity hit re-records (refreshes at_ms).
            inputs.affinity.record(inputs.conn, ctx, &ep, &model);
            return Ok(route);
        }
    }

    // 3. Capability-ranked selection over the policy's candidate lists.
    if let Some((ep, model, reason)) = pick_by_capability(ctx, &policy, inputs)? {
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

/// Append `list` to `candidates`, skipping ids already present. Order-
/// preserving dedup so the policy's preferred chain wins ties.
fn extend_dedup(list: &[String], candidates: &mut Vec<String>) {
    for id in list {
        if !candidates.iter().any(|c| c == id) {
            candidates.push(id.clone());
        }
    }
}

/// Parse a `["a","b"]`-style JSON-string column into a `Vec<String>`. `None`
/// / malformed / non-array yields `None` (treated as "no list" by callers).
fn parse_json_array(s: Option<&str>) -> Option<Vec<String>> {
    let s = s?;
    let arr: Vec<String> = serde_json::from_str(s).ok()?;
    Some(arr)
}

/// Walk the policy's preferred → fallback endpoint chains (in order), then
/// the agent's bound endpoints, and return the first `(endpoint, model)` that
/// is healthy, quota-ok, and capability-eligible. Returns the matching reason
/// (`Capability` for a fresh pick). Returns `None` when nothing qualifies.
fn pick_by_capability(
    ctx: &TaskContext,
    policy: &store::RoutingPolicyRow,
    inputs: &RouterInputs<'_>,
) -> AppResult<Option<(String, String, RouteReason)>> {
    // Build the ordered candidate endpoint list: policy preferred → fallback →
    // all agent-bound endpoints (deduped). This is the pool we rank. The policy
    // stores these as JSON-string columns (`preferred_endpoints` etc.); parse
    // them into real arrays at the boundary.
    let mut candidates: Vec<String> = Vec::new();
    if let Some(pref) = parse_json_array(policy.preferred_endpoints.as_deref()) {
        extend_dedup(&pref, &mut candidates);
    }
    if let Some(fall) = parse_json_array(policy.fallback_endpoints.as_deref()) {
        extend_dedup(&fall, &mut candidates);
    }
    // Always include the agent's bound endpoints as a last-resort pool so a
    // policy with no explicit lists still routes.
    let bindings = db::list_bindings(inputs.conn, &ctx.agent_id)?;
    let bound: Vec<String> = bindings.iter().map(|b| b.endpoint_id.clone()).collect();
    extend_dedup(&bound, &mut candidates);

    // Capability-eligible (endpoint, model) pairs across ALL endpoints, then
    // intersect with the candidate order so preferred endpoints win ties.
    let eligible = capability_registry::eligible_models(inputs.conn, &ctx.required_capabilities)?;
    for ep_id in &candidates {
        if inputs.health.is_degraded(ep_id) || inputs.quota.is_exhausted(ep_id) {
            continue;
        }
        // Routing semantics: the endpoint's DEFAULT model is preferred — read
        // live from `models_json.default` (no catalog staleness) — but if the
        // default is missing, capability-ineligible, or excluded by the
        // policy's allowed-models glob, ANOTHER of the endpoint's eligible
        // models must still qualify. (The old code checked only the default
        // and then skipped the whole endpoint, even when a valid model sat
        // right behind it.)
        let default_m = endpoint_default_model(inputs.conn, ep_id)?;
        let mut models: Vec<&String> = eligible
            .iter()
            .filter(|(e, _, _)| e == ep_id)
            .map(|(_, m, _)| m)
            .collect();
        models.sort_by_key(|m| usize::from(default_m.as_deref() != Some(m.as_str())));
        for m in models {
            // Enforce the policy's allowed-models glob list, when set.
            if !model_allowed(&policy.allowed_models, m) {
                continue;
            }
            return Ok(Some((ep_id.clone(), m.clone(), RouteReason::Capability)));
        }
    }
    Ok(None)
}

/// Read the endpoint's `default` model id from its `models_json` (live DB
/// read — no catalog staleness). `None` when unset/malformed.
fn endpoint_default_model(
    conn: &rusqlite::Connection,
    endpoint_id: &str,
) -> AppResult<Option<String>> {
    let Some(ep) = db::get_endpoint(conn, endpoint_id)? else {
        return Ok(None);
    };
    let Some(json) = ep.models_json.as_deref() else {
        return Ok(None);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Ok(None);
    };
    Ok(v.get("default")
        .and_then(|s| s.as_str())
        .map(String::from)
        .filter(|s| !s.is_empty()))
}

/// `true` when `model` passes the policy's `allowed_models` globs. `None` (no
/// list) means "any model allowed". A glob supports a trailing `*` wildcard
/// (e.g. `claude-*`); other patterns match literally.
fn model_allowed(allowed: &Option<String>, model: &str) -> bool {
    let Some(json) = allowed else {
        return true; // no list = any
    };
    let Ok(arr): std::result::Result<Vec<String>, _> = serde_json::from_str(json) else {
        return true; // malformed list = permissive (don't block routing on bad data)
    };
    if arr.is_empty() {
        return true;
    }
    arr.iter().any(|pat| {
        if let Some(prefix) = pat.strip_suffix('*') {
            model.starts_with(prefix)
        } else {
            model == pat
        }
    })
}

fn endpoint_exists(conn: &Connection, endpoint_id: &str) -> AppResult<bool> {
    Ok(db::get_endpoint(conn, endpoint_id)?.is_some())
}

/// Re-validate that a model still satisfies the capability request (affinity
/// reuse must not hand back an ineligible model after policy/capability edits).
fn model_eligible_for_req(
    conn: &Connection,
    endpoint_id: &str,
    model: &str,
    req: &CapabilityReq,
) -> AppResult<bool> {
    for row in store::list_model_catalog(conn, endpoint_id)? {
        if row.model_id == model {
            let abilities: crate::model_abilities::ModelAbilities =
                serde_json::from_str(&row.abilities_json).unwrap_or_default();
            return Ok(capability_registry::satisfies(req, &abilities));
        }
    }
    // Model not in catalog → treat as eligible (don't break affinity on a
    // cold catalog; the gateway will surface a real upstream error if any).
    Ok(true)
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
        (Some(ProviderKind::Openai), _) => ProviderKind::Openai,
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
mod tests {
    use super::*;

    fn seed_endpoint(
        conn: &Connection,
        id: &str,
        protocol: &str,
        base_url: &str,
        default_model: &str,
    ) {
        conn.execute(
            "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status, models_json)
             VALUES (?1,'custom','Main',0,'unvalidated',?2)",
            rusqlite::params![id, format!("{{\"default\":\"{default_model}\"}}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url)
             VALUES (?1,?2,?3)",
            rusqlite::params![id, protocol, base_url],
        )
        .unwrap();
    }

    fn seed_binding(conn: &Connection, agent_id: &str, endpoint_id: &str) {
        conn.execute(
            "INSERT INTO agent_provider_binding (agent_id, endpoint_id, active, created_at)
             VALUES (?1,?2,1,0)",
            rusqlite::params![agent_id, endpoint_id],
        )
        .unwrap();
    }

    /// Test harness owning the stores so borrows in `RouterInputs` outlive the
    /// `resolve()` call. Construct per test; call `.inputs()` to borrow.
    struct TestEnv {
        conn: rusqlite::Connection,
        health: ProviderHealth,
        quota: QuotaState,
        affinity: RouteAffinity,
    }
    impl TestEnv {
        fn new() -> Self {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            crate::schema::build_v1(&conn).unwrap();
            // Seed the agent registry rows the bindings FK references. The
            // router only routes for agents that exist; tests use claude-code.
            for a in crate::agents::agents() {
                conn.execute(
                    "INSERT OR IGNORE INTO agent (id, kind, display_name, status, last_detected_at, enabled)
                     VALUES (?1, ?2, ?3, 'ok', 0, 1)",
                    rusqlite::params![a.id, a.kind, a.display_name],
                )
                .unwrap();
            }
            Self {
                conn,
                health: ProviderHealth::new(),
                quota: QuotaState::new(),
                affinity: RouteAffinity::new(),
            }
        }
        fn inputs(&self) -> RouterInputs<'_> {
            RouterInputs {
                conn: &self.conn,
                health: &self.health,
                quota: &self.quota,
                affinity: &self.affinity,
            }
        }
    }

    #[test]
    fn explicit_pin_wins_when_endpoint_healthy() {
        let env = TestEnv::new();
        seed_endpoint(&env.conn, "ep-1", "anthropic", "https://api.anthropic.com", "claude-sonnet");
        capability_registry::rebuild(&env.conn).unwrap();

        let mut ctx = TaskContext::new_task("claude-code-cli", None);
        ctx.requested_provider = Some("ep-1".into());
        ctx.requested_model = Some("claude-sonnet".into());

        let r = resolve(&ctx, &env.inputs()).unwrap();
        assert_eq!(r.reason, RouteReason::Explicit);
        assert_eq!(r.endpoint_id, "ep-1");
        assert_eq!(r.model, "claude-sonnet");
    }

    #[test]
    fn explicit_pin_falls_through_when_degraded() {
        let env = TestEnv::new();
        seed_endpoint(&env.conn, "ep-1", "anthropic", "https://x", "m-1");
        seed_binding(&env.conn, "claude-code-cli", "ep-1");
        capability_registry::rebuild(&env.conn).unwrap();

        // Degrade ep-1 with 3 migratable failures.
        for _ in 0..3 {
            env.health.record(
                "ep-1",
                crate::orchestration::health::HealthOutcome::Fail(
                    crate::orchestration::health::FailureClass::QuotaExhausted,
                ),
                429,
            );
        }

        let mut ctx = TaskContext::new_task("claude-code-cli", None);
        ctx.requested_provider = Some("ep-1".into());
        ctx.requested_model = Some("m-1".into());
        // ep-1 is degraded AND it's the only candidate → fail closed.
        let r = resolve(&ctx, &env.inputs()).unwrap();
        assert_eq!(r.reason, RouteReason::NoEligible);
        assert!(r.endpoint_id.is_empty());
    }

    #[test]
    fn affinity_reuses_previous_route_for_same_task() {
        let env = TestEnv::new();
        seed_endpoint(&env.conn, "ep-1", "anthropic", "https://x", "m-1");
        seed_binding(&env.conn, "claude-code-cli", "ep-1");
        capability_registry::rebuild(&env.conn).unwrap();

        // First request: capability pick records affinity.
        let ctx1 = TaskContext::new_task("claude-code-cli", None);
        let r1 = resolve(&ctx1, &env.inputs()).unwrap();
        assert_eq!(r1.reason, RouteReason::Capability);
        assert_eq!(r1.endpoint_id, "ep-1");

        // Second request for the SAME task_id → affinity hit.
        let mut ctx2 = TaskContext::new_for_request("claude-code-cli", ctx1.task_id, None);
        ctx2.lifecycle = crate::orchestration::identity::TaskLifecycle::InFlight;
        let r2 = resolve(&ctx2, &env.inputs()).unwrap();
        assert_eq!(r2.reason, RouteReason::Affinity, "same task_id must hit affinity");
        assert_eq!(r2.endpoint_id, "ep-1");
    }

    #[test]
    fn allowed_models_glob_filters_capability_pick() {
        let env = TestEnv::new();
        seed_endpoint(&env.conn, "ep-1", "openai-comp", "https://x", "gpt-4o");
        seed_binding(&env.conn, "claude-code-cli", "ep-1");
        capability_registry::rebuild(&env.conn).unwrap();

        // Policy: only allow `claude-*`. ep-1 only has `gpt-4o` → no match.
        let now = chrono::Utc::now().timestamp_millis();
        store::upsert_routing_policy(
            &env.conn,
            &store::RoutingPolicyRow {
                agent_id: "claude-code-cli".into(),
                role: "*".into(),
                preferred_endpoints: None,
                fallback_endpoints: None,
                allowed_models: Some(r#"["claude-*"]"#.into()),
                migrate_on_quota: true,
                inject_cache_control: false,
                affinity_scope: "task".into(),
                updated_at: now,
            },
        )
        .unwrap();

        let ctx = TaskContext::new_task("claude-code-cli", None);
        let r = resolve(&ctx, &env.inputs()).unwrap();
        assert_eq!(r.reason, RouteReason::NoEligible, "gpt-4o blocked by claude-* glob");
    }

    #[test]
    fn quota_exhausted_endpoint_is_skipped() {
        let env = TestEnv::new();
        seed_endpoint(&env.conn, "ep-1", "openai-comp", "https://x", "m-1");
        seed_endpoint(&env.conn, "ep-2", "openai-comp", "https://y", "m-2");
        seed_binding(&env.conn, "claude-code-cli", "ep-1");
        seed_binding(&env.conn, "claude-code-cli", "ep-2");
        capability_registry::rebuild(&env.conn).unwrap();

        env.quota.mark_exhausted("ep-1", Some("5h window elapsed".into()));

        let ctx = TaskContext::new_task("claude-code-cli", None);
        let r = resolve(&ctx, &env.inputs()).unwrap();
        // ep-1 exhausted → router skips to ep-2.
        assert_eq!(r.endpoint_id, "ep-2");
        assert_eq!(r.reason, RouteReason::Capability);
    }

    #[test]
    fn model_allowed_helper() {
        assert!(model_allowed(&None, "anything"));
        assert!(model_allowed(&Some(r#"[]"#.into()), "anything")); // empty = permissive
        assert!(model_allowed(&Some(r#"["claude-*"]"#.into()), "claude-sonnet"));
        assert!(!model_allowed(&Some(r#"["claude-*"]"#.into()), "gpt-4o"));
        assert!(model_allowed(&Some(r#"["gpt-4o"]"#.into()), "gpt-4o"));
        assert!(!model_allowed(&Some(r#"["gpt-4o"]"#.into()), "gpt-4o-mini"));
        // Malformed JSON = permissive (never block on bad data).
        assert!(model_allowed(&Some("not json".into()), "x"));
    }

    /// Overwrite an endpoint's `models_json` and rebuild the catalog.
    fn seed_endpoint_models(conn: &Connection, id: &str, models_json: &str) {
        conn.execute(
            "UPDATE provider_endpoint SET models_json = ?2 WHERE id = ?1",
            rusqlite::params![id, models_json],
        )
        .unwrap();
        capability_registry::rebuild(conn).unwrap();
    }

    /// Overwrite `models_json` WITHOUT rebuilding the catalog — proves the
    /// router reads the default live instead of the cached catalog.
    fn set_models_no_rebuild(conn: &Connection, id: &str, models_json: &str) {
        conn.execute(
            "UPDATE provider_endpoint SET models_json = ?2 WHERE id = ?1",
            rusqlite::params![id, models_json],
        )
        .unwrap();
    }

    fn add_protocol_row(conn: &Connection, id: &str, protocol: &str, base_url: &str) {
        conn.execute(
            "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url) VALUES (?1,?2,?3)",
            rusqlite::params![id, protocol, base_url],
        )
        .unwrap();
    }

    /// Routing semantics: the first eligible provider serves its DEFAULT
    /// model. glm-4.7 sorts before glm-5.2 alphabetically — the default must
    /// still win (regression: the old code picked the alphabetical-first
    /// catalog model and ignored `models_json.default`).
    #[test]
    fn capability_pick_prefers_endpoint_default_model() {
        let env = TestEnv::new();
        seed_endpoint(&env.conn, "z-ai", "openai-comp", "https://api.z.ai/api/paas/v4", "glm-4.7");
        seed_binding(&env.conn, "opencode-desktop", "z-ai");
        seed_endpoint_models(
            &env.conn,
            "z-ai",
            r#"{"available":["glm-4.7","glm-5.2"],"default":"glm-5.2"}"#,
        );

        let mut ctx = TaskContext::new_task("opencode-desktop", None);
        ctx.requested_model = Some("nestra".into());
        ctx.protocol_hint = Some(ProviderKind::Openai);
        let route = resolve(&ctx, &env.inputs()).unwrap();

        assert_eq!(route.endpoint_id, "z-ai");
        assert_eq!(route.model, "glm-5.2", "default must win over alphabetical-first");
        assert_eq!(route.reason, RouteReason::Capability);
    }

    /// A default-model edit on the Provider page takes effect on the NEXT
    /// request — the router reads `models_json.default` live (no catalog
    /// rebuild, no gateway restart).
    #[test]
    fn default_model_edit_takes_effect_without_catalog_rebuild() {
        let env = TestEnv::new();
        seed_endpoint(&env.conn, "ep-1", "openai-comp", "http://127.0.0.1:8787", "glm-5.2");
        seed_binding(&env.conn, "opencode-desktop", "ep-1");
        seed_endpoint_models(
            &env.conn,
            "ep-1",
            r#"{"available":["glm-4.7","glm-5.2"],"default":"glm-5.2"}"#,
        );

        let mut ctx = TaskContext::new_task("opencode-desktop", None);
        ctx.protocol_hint = Some(ProviderKind::Openai);
        assert_eq!(resolve(&ctx, &env.inputs()).unwrap().model, "glm-5.2");

        // Flip the default to a model already in the catalog — NO rebuild.
        set_models_no_rebuild(
            &env.conn,
            "ep-1",
            r#"{"available":["glm-4.7","glm-5.2"],"default":"glm-4.7"}"#,
        );
        // A NEW task (fresh task_id) — the old task's affinity would reuse the
        // prior route by design.
        let mut ctx2 = TaskContext::new_task("opencode-desktop", None);
        ctx2.protocol_hint = Some(ProviderKind::Openai);
        assert_eq!(resolve(&ctx2, &env.inputs()).unwrap().model, "glm-4.7");
    }

    /// The upstream base_url follows the inbound direction: an OpenAI-shape
    /// request picks the endpoint's `openai` protocol row, an Anthropic one
    /// the `anthropic` row — not blindly the first row.
    #[test]
    fn protocol_hint_picks_matching_endpoint_row() {
        let env = TestEnv::new();
        seed_endpoint(&env.conn, "z-ai", "anthropic", "https://api.z.ai/api/anthropic", "glm-5.2");
        add_protocol_row(&env.conn, "z-ai", "openai-comp", "https://api.z.ai/api/paas/v4");
        seed_binding(&env.conn, "opencode-desktop", "z-ai");
        seed_endpoint_models(&env.conn, "z-ai", r#"{"available":["glm-5.2"],"default":"glm-5.2"}"#);

        let mut openai_ctx = TaskContext::new_task("opencode-desktop", None);
        openai_ctx.protocol_hint = Some(ProviderKind::Openai);
        assert_eq!(
            resolve(&openai_ctx, &env.inputs()).unwrap().base_url,
            "https://api.z.ai/api/paas/v4"
        );

        let mut anthropic_ctx = TaskContext::new_task("opencode-desktop", None);
        anthropic_ctx.protocol_hint = Some(ProviderKind::Anthropic);
        assert_eq!(
            resolve(&anthropic_ctx, &env.inputs()).unwrap().base_url,
            "https://api.z.ai/api/anthropic"
        );

        // No hint → historical first-row behavior.
        let bare_ctx = TaskContext::new_task("opencode-desktop", None);
        assert_eq!(
            resolve(&bare_ctx, &env.inputs()).unwrap().base_url,
            "https://api.z.ai/api/anthropic"
        );
    }

    /// A same-gateway endpoint (opencode-go: anthropic + openai rows on ONE
    /// base_url) routes an Anthropic inbound to the openai row so the
    /// conversion layer can speak the official per-model protocol; distinct
    /// base_urls keep the direction-matched row. The WIRE then follows the
    /// model's api dialect: deepseek-v4-flash is anthropic-class (corrections
    /// map), so it dials Messages directly — no conversion needed.
    #[test]
    fn same_gateway_dual_rows_prefer_openai_for_anthropic_inbound() {
        let env = TestEnv::new();
        seed_endpoint(&env.conn, "opencode-go", "anthropic", "https://opencode.ai/zen/go/v1", "deepseek-v4-flash");
        add_protocol_row(&env.conn, "opencode-go", "openai-comp", "https://opencode.ai/zen/go/v1");
        seed_binding(&env.conn, "claude-code-cli", "opencode-go");
        seed_binding(&env.conn, "opencode-desktop", "opencode-go");
        seed_endpoint_models(
            &env.conn,
            "opencode-go",
            r#"{"available":["deepseek-v4-flash"],"default":"deepseek-v4-flash"}"#,
        );

        let mut anthropic_ctx = TaskContext::new_task("claude-code-cli", None);
        anthropic_ctx.protocol_hint = Some(ProviderKind::Anthropic);
        let route = resolve(&anthropic_ctx, &env.inputs()).unwrap();
        assert_eq!(
            route.protocol,
            ProviderKind::Anthropic,
            "deepseek-v4-flash is anthropic-class (corrections) → dials Messages directly"
        );
        assert_eq!(route.base_url, "https://opencode.ai/zen/go/v1");

        // OpenAI inbound keeps the openai wire (anthropic-class models accept
        // the chat wire too).
        let mut openai_ctx = TaskContext::new_task("opencode-desktop", None);
        openai_ctx.protocol_hint = Some(ProviderKind::Openai);
        let route = resolve(&openai_ctx, &env.inputs()).unwrap();
        assert_eq!(route.protocol, ProviderKind::Openai);
    }

    /// Distinct base_urls (DeepSeek/Moonshot real dual endpoints) keep the
    /// direction-matched row — Anthropic inbound hits the anthropic endpoint.
    #[test]
    fn distinct_base_urls_keep_direction_matched_row() {
        let env = TestEnv::new();
        seed_endpoint(&env.conn, "deepseek", "anthropic", "https://api.deepseek.com/anthropic", "deepseek-chat");
        add_protocol_row(&env.conn, "deepseek", "openai-comp", "https://api.deepseek.com/v1");
        seed_binding(&env.conn, "claude-code-cli", "deepseek");
        seed_endpoint_models(
            &env.conn,
            "deepseek",
            r#"{"available":["deepseek-chat"],"default":"deepseek-chat"}"#,
        );

        let mut anthropic_ctx = TaskContext::new_task("claude-code-cli", None);
        anthropic_ctx.protocol_hint = Some(ProviderKind::Anthropic);
        let route = resolve(&anthropic_ctx, &env.inputs()).unwrap();
        assert_eq!(route.protocol, ProviderKind::Anthropic);
        assert_eq!(route.base_url, "https://api.deepseek.com/anthropic");
    }

    /// Mock-style endpoints declare one row that serves both shapes; a
    /// direction with no matching row falls back to the first one.
    #[test]
    fn protocol_hint_falls_back_to_first_row_when_no_match() {
        let env = TestEnv::new();
        seed_endpoint(&env.conn, "mock-a", "anthropic", "http://127.0.0.1:8787", "claude-haiku-4-5");
        seed_binding(&env.conn, "pi-cli", "mock-a");
        seed_endpoint_models(
            &env.conn,
            "mock-a",
            r#"{"available":["claude-haiku-4-5"],"default":"claude-haiku-4-5"}"#,
        );

        let mut ctx = TaskContext::new_task("pi-cli", None);
        ctx.protocol_hint = Some(ProviderKind::Openai);
        let route = resolve(&ctx, &env.inputs()).unwrap();
        assert_eq!(route.base_url, "http://127.0.0.1:8787");
    }

    /// Overwrite a catalog row's abilities `api` dialect (simulating the
    /// corrections/override layer) so wire selection can be tested directly.
    fn set_catalog_api(conn: &Connection, endpoint: &str, model: &str, api: &str) {
        let abilities = format!(r#"{{"api":"{api}"}}"#);
        conn.execute(
            "UPDATE model_catalog SET abilities_json = ?3 WHERE endpoint_id = ?1 AND model_id = ?2",
            rusqlite::params![endpoint, model, abilities],
        )
        .unwrap();
    }

    /// A responses-class model (grok-4.5) on an Anthropic inbound resolves to
    /// the Responses wire — the chat wire is broken upstream for it (503).
    #[test]
    fn wire_responses_class_routes_to_responses_api() {
        let env = TestEnv::new();
        seed_endpoint(&env.conn, "opencode-go", "anthropic", "https://opencode.ai/zen/go/v1", "grok-4.5");
        add_protocol_row(&env.conn, "opencode-go", "openai-comp", "https://opencode.ai/zen/go/v1");
        seed_binding(&env.conn, "claude-code-cli", "opencode-go");
        seed_endpoint_models(
            &env.conn,
            "opencode-go",
            r#"{"available":["grok-4.5"],"default":"grok-4.5"}"#,
        );
        set_catalog_api(&env.conn, "opencode-go", "grok-4.5", "response-api");

        let mut ctx = TaskContext::new_task("claude-code-cli", None);
        ctx.protocol_hint = Some(ProviderKind::Anthropic);
        let route = resolve(&ctx, &env.inputs()).unwrap();
        assert_eq!(route.protocol, ProviderKind::Responses);
        assert_eq!(route.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(route.model, "grok-4.5");
    }

    /// An openai-class model (kimi-k3) on an Anthropic inbound resolves to
    /// the Chat wire (conversion) — it rejects the Anthropic wire.
    #[test]
    fn wire_openai_class_routes_to_chat() {
        let env = TestEnv::new();
        seed_endpoint(&env.conn, "opencode-go", "anthropic", "https://opencode.ai/zen/go/v1", "kimi-k3");
        add_protocol_row(&env.conn, "opencode-go", "openai-comp", "https://opencode.ai/zen/go/v1");
        seed_binding(&env.conn, "claude-code-cli", "opencode-go");
        seed_endpoint_models(
            &env.conn,
            "opencode-go",
            r#"{"available":["kimi-k3"],"default":"kimi-k3"}"#,
        );
        set_catalog_api(&env.conn, "opencode-go", "kimi-k3", "openai-comp");

        let mut ctx = TaskContext::new_task("claude-code-cli", None);
        ctx.protocol_hint = Some(ProviderKind::Anthropic);
        let route = resolve(&ctx, &env.inputs()).unwrap();
        assert_eq!(route.protocol, ProviderKind::Openai);
    }

    /// A chat inbound only switches wire for responses-class models;
    /// anthropic-class and unknown models stay on Chat (they accept it).
    #[test]
    fn wire_chat_inbound_switches_only_for_responses_class() {
        let env = TestEnv::new();
        seed_endpoint(&env.conn, "opencode-go", "anthropic", "https://opencode.ai/zen/go/v1", "deepseek-v4-flash");
        add_protocol_row(&env.conn, "opencode-go", "openai-comp", "https://opencode.ai/zen/go/v1");
        seed_binding(&env.conn, "opencode-desktop", "opencode-go");
        seed_endpoint_models(
            &env.conn,
            "opencode-go",
            r#"{"available":["deepseek-v4-flash","grok-4.5"],"default":"deepseek-v4-flash"}"#,
        );

        // deepseek-v4-flash: anthropic-class (corrections) → stays Chat.
        let mut ds_ctx = TaskContext::new_task("opencode-desktop", None);
        ds_ctx.protocol_hint = Some(ProviderKind::Openai);
        ds_ctx.requested_model = Some("deepseek-v4-flash".into());
        ds_ctx.requested_provider = Some("opencode-go".into());
        let route = resolve(&ds_ctx, &env.inputs()).unwrap();
        assert_eq!(route.protocol, ProviderKind::Openai);

        // grok-4.5: responses-class → Chat inbound also switches to Responses.
        set_catalog_api(&env.conn, "opencode-go", "grok-4.5", "response-api");
        let mut gr_ctx = TaskContext::new_task("opencode-desktop", None);
        gr_ctx.protocol_hint = Some(ProviderKind::Openai);
        gr_ctx.requested_model = Some("grok-4.5".into());
        gr_ctx.requested_provider = Some("opencode-go".into());
        let route = resolve(&gr_ctx, &env.inputs()).unwrap();
        assert_eq!(route.protocol, ProviderKind::Responses);
    }

    /// Unknown api (no corrections/overrides) follows the row protocol —
    /// historical behavior preserved.
    #[test]
    fn wire_unknown_api_follows_row() {
        let env = TestEnv::new();
        seed_endpoint(&env.conn, "ep-1", "anthropic", "https://api.example.com", "m-1");
        add_protocol_row(&env.conn, "ep-1", "openai-comp", "https://api.example.com/v1");
        seed_binding(&env.conn, "claude-code-cli", "ep-1");
        seed_endpoint_models(&env.conn, "ep-1", r#"{"available":["m-1"],"default":"m-1"}"#);

        // Distinct base_urls → direction-matched anthropic row → Anthropic wire.
        let mut a_ctx = TaskContext::new_task("claude-code-cli", None);
        a_ctx.protocol_hint = Some(ProviderKind::Anthropic);
        let r = resolve(&a_ctx, &env.inputs()).unwrap();
        assert_eq!(r.protocol, ProviderKind::Anthropic);

        let mut o_ctx = TaskContext::new_task("opencode-desktop", None);
        o_ctx.protocol_hint = Some(ProviderKind::Openai);
        seed_binding(&env.conn, "opencode-desktop", "ep-1");
        let r = resolve(&o_ctx, &env.inputs()).unwrap();
        assert_eq!(r.protocol, ProviderKind::Openai);
    }

    #[test]
    fn session_affinity_survives_simulated_restart() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::build_v1(&conn).unwrap();
        let a = RouteAffinity::new();
        let ctx = TaskContext::new_task("claude-code-cli", Some("sess-1".to_string()));
        a.record(&conn, &ctx, "ep-a", "model-a");

        // Fresh instance = a Nestra restart: only session-grain affinity
        // comes back (task entries are ephemeral by design).
        let b = RouteAffinity::new();
        b.load_sessions(&conn);
        assert_eq!(
            b.lookup(&ctx, AffinityScope::Session),
            Some(("ep-a".to_string(), "model-a".to_string()))
        );
        assert_eq!(b.lookup(&ctx, AffinityScope::Task), None);
    }

    #[test]
    fn affinity_persist_is_debounced() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::build_v1(&conn).unwrap();
        let a = RouteAffinity::new();
        let ctx = TaskContext::new_task("claude-code-cli", Some("sess-1".to_string()));
        a.record(&conn, &ctx, "ep-a", "model-a");

        // Second record inside the debounce window must not rewrite the
        // setting (detect via row deletion: a rewrite would re-insert it).
        conn.execute("DELETE FROM setting_kv WHERE key = 'route_affinity'", [])
            .unwrap();
        a.record(&conn, &ctx, "ep-b", "model-b");
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM setting_kv WHERE key = 'route_affinity'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "debounced record must skip the setting write");
    }

    #[test]
    fn capability_req_routes_vision_away_from_text_only_model() {
        let env = TestEnv::new();
        seed_endpoint(&env.conn, "ep-text", "anthropic", "https://t", "m-text");
        seed_endpoint(&env.conn, "ep-vis", "anthropic", "https://v", "m-vis");
        seed_binding(&env.conn, "claude-code-cli", "ep-text");
        seed_binding(&env.conn, "claude-code-cli", "ep-vis");
        // ep-text's model reports text-only input; ep-vis's reports text+image.
        for (id, abilities) in [
            ("ep-text", r#"{"m-text":{"modalities":{"input":["text"]}}}"#),
            ("ep-vis", r#"{"m-vis":{"modalities":{"input":["text","image"]}}}"#),
        ] {
            env.conn
                .execute(
                    "UPDATE provider_endpoint SET model_abilities_json = ?2 WHERE id = ?1",
                    rusqlite::params![id, abilities],
                )
                .unwrap();
        }
        capability_registry::rebuild(&env.conn).unwrap();

        // An image-bearing request derives vision=true and must not resolve
        // onto the text-only model (Smart Gateway fix 2 activating the
        // previously inert capability stage).
        let body = br#"{"model":"m-text","messages":[{"role":"user","content":[
             {"type":"text","text":"see"},{"type":"image","source":{"data":"x"}}]}]}"#;
        let mut ctx = TaskContext::new_task("claude-code-cli", None);
        ctx.required_capabilities =
            capability_registry::derive_capability_req(body, ProviderKind::Anthropic);
        assert!(ctx.required_capabilities.vision);
        let r = resolve(&ctx, &env.inputs()).unwrap();
        assert_eq!(r.endpoint_id, "ep-vis", "vision request excludes the text-only model");

        // A text-only request stays eligible for the text-only model.
        let mut ctx2 = TaskContext::new_task("claude-code-cli", None);
        ctx2.requested_model = Some("m-text".into());
        let r2 = resolve(&ctx2, &env.inputs()).unwrap();
        assert_eq!(r2.model, "m-text");
    }
}
