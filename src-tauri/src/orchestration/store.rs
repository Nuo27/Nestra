//! CRUD over the orchestration tables (the control plane).
//!
//! Every persisted struct here is **credential-free by construction**
//! (correction #5): no `String`/`Vec<u8>` field named `*key*`/`*secret*`/
//! `*credential*`/`*token*`/`*password*` appears on any row type. The
//! [`Self::no_persisted_secret_fields`] test serializes every persisted struct
//! and walks the JSON keys to enforce this at test time — a contributor cannot
//! accidentally add a credential column without breaking the build.
//!
//! The live API key is resolved at request time only, via
//! [`crate::secrets::get`] → [`super::CredentialHandle`], and never passes
//! through any type in this module.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

use super::identity::{RouteReason, TaskLifecycle};

// ===========================================================================
// routing_policy
// ===========================================================================

/// One routing-policy row, keyed by `(agent_id, role)`. The router reads
/// preferred/fallback endpoint lists, allowed-model globs, migration and
/// cache-injection toggles, and the affinity scope from here.
///
/// `role = "*"` is the catch-all default policy used when no per-role row
/// matches a Task's [`super::SubagentRole`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingPolicyRow {
    pub agent_id: String,
    pub role: String,
    /// JSON array of endpoint ids in priority order. `None` = no preference.
    pub preferred_endpoints: Option<String>,
    /// JSON array of endpoint ids to try on migration. `None` = derive from
    /// all enabled endpoints.
    pub fallback_endpoints: Option<String>,
    /// JSON array of model-id globs the policy allows, or `None` for "any".
    pub allowed_models: Option<String>,
    pub migrate_on_quota: bool,
    pub inject_cache_control: bool,
    /// `"task"` | `"session"` | `"none"`. Task-grain affinity is the default
    /// because it protects prompt cache.
    pub affinity_scope: String,
    pub updated_at: i64,
}

impl RoutingPolicyRow {
    pub fn default_for(agent_id: &str, role: &str, now: i64) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            role: role.to_string(),
            preferred_endpoints: None,
            fallback_endpoints: None,
            allowed_models: None,
            migrate_on_quota: true,
            inject_cache_control: false,
            affinity_scope: "task".to_string(),
            updated_at: now,
        }
    }
}

pub fn upsert_routing_policy(conn: &Connection, row: &RoutingPolicyRow) -> AppResult<()> {
    conn.execute(
        "INSERT INTO routing_policy
           (agent_id, role, preferred_endpoints, fallback_endpoints, allowed_models,
            migrate_on_quota, inject_cache_control, affinity_scope, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(agent_id, role) DO UPDATE SET
           preferred_endpoints   = excluded.preferred_endpoints,
           fallback_endpoints    = excluded.fallback_endpoints,
           allowed_models        = excluded.allowed_models,
           migrate_on_quota      = excluded.migrate_on_quota,
           inject_cache_control  = excluded.inject_cache_control,
           affinity_scope        = excluded.affinity_scope,
           updated_at            = excluded.updated_at",
        rusqlite::params![
            row.agent_id,
            row.role,
            row.preferred_endpoints,
            row.fallback_endpoints,
            row.allowed_models,
            row.migrate_on_quota as i64,
            row.inject_cache_control as i64,
            row.affinity_scope,
            row.updated_at,
        ],
    )?;
    Ok(())
}

/// Look up the most specific policy for `(agent_id, role)`, falling back to the
/// agent's `role = "*"` catch-all, then to a synthesized default. Never errors
/// on a missing row — the router always gets *some* policy.
pub fn routing_policy_for(
    conn: &Connection,
    agent_id: &str,
    role: &str,
) -> AppResult<RoutingPolicyRow> {
    if let Some(row) = routing_policy_exact(conn, agent_id, role)? {
        return Ok(row);
    }
    if let Some(row) = routing_policy_exact(conn, agent_id, "*")? {
        return Ok(row);
    }
    Ok(RoutingPolicyRow::default_for(
        agent_id,
        role,
        chrono::Utc::now().timestamp_millis(),
    ))
}

fn routing_policy_exact(
    conn: &Connection,
    agent_id: &str,
    role: &str,
) -> AppResult<Option<RoutingPolicyRow>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, role, preferred_endpoints, fallback_endpoints, allowed_models,
                migrate_on_quota, inject_cache_control, affinity_scope, updated_at
         FROM routing_policy WHERE agent_id = ?1 AND role = ?2",
    )?;
    let mut rows = stmt.query(rusqlite::params![agent_id, role])?;
    match rows.next()? {
        Some(r) => Ok(Some(RoutingPolicyRow {
            agent_id: r.get(0)?,
            role: r.get(1)?,
            preferred_endpoints: r.get(2)?,
            fallback_endpoints: r.get(3)?,
            allowed_models: r.get(4)?,
            migrate_on_quota: r.get::<_, i64>(5)? != 0,
            inject_cache_control: r.get::<_, i64>(6)? != 0,
            affinity_scope: r.get(7)?,
            updated_at: r.get(8)?,
        })),
        None => Ok(None),
    }
}

pub fn list_routing_policies(conn: &Connection, agent_id: &str) -> AppResult<Vec<RoutingPolicyRow>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, role, preferred_endpoints, fallback_endpoints, allowed_models,
                migrate_on_quota, inject_cache_control, affinity_scope, updated_at
         FROM routing_policy WHERE agent_id = ?1 ORDER BY role",
    )?;
    let rows = stmt.query_map(rusqlite::params![agent_id], |r| {
        Ok(RoutingPolicyRow {
            agent_id: r.get(0)?,
            role: r.get(1)?,
            preferred_endpoints: r.get(2)?,
            fallback_endpoints: r.get(3)?,
            allowed_models: r.get(4)?,
            migrate_on_quota: r.get::<_, i64>(5)? != 0,
            inject_cache_control: r.get::<_, i64>(6)? != 0,
            affinity_scope: r.get(7)?,
            updated_at: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn delete_routing_policy(conn: &Connection, agent_id: &str, role: &str) -> AppResult<bool> {
    let n = conn.execute(
        "DELETE FROM routing_policy WHERE agent_id = ?1 AND role = ?2",
        rusqlite::params![agent_id, role],
    )?;
    Ok(n > 0)
}

// ===========================================================================
// task
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRow {
    pub id: String,
    pub parent_task_id: Option<String>,
    /// One of [`TaskLifecycle::as_str`].
    pub lifecycle: String,
    /// JSON `{agent, kind, ref_id}` for an agent-native task handle, or `None`.
    pub native_task_ref: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

pub fn insert_task(conn: &Connection, row: &TaskRow) -> AppResult<()> {
    conn.execute(
        "INSERT INTO task (id, parent_task_id, lifecycle, native_task_ref, started_at, ended_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            row.id,
            row.parent_task_id,
            row.lifecycle,
            row.native_task_ref,
            row.started_at,
            row.ended_at,
        ],
    )?;
    Ok(())
}

/// Transition a Task to a new lifecycle state. Validates the target is a known
/// [`TaskLifecycle`] variant (rejects typos early). `ended_at` is set only for
/// terminal transitions.
pub fn set_task_lifecycle(
    conn: &Connection,
    id: &str,
    next: TaskLifecycle,
    now: i64,
) -> AppResult<()> {
    let next_str = next.as_str();
    let n = conn.execute(
        "UPDATE task SET lifecycle = ?1, ended_at = CASE WHEN ?2 THEN ?3 ELSE ended_at END
         WHERE id = ?4",
        rusqlite::params![next_str, next.is_terminal() as i64, now, id],
    )?;
    if n == 0 {
        return Err(AppError::NotFound(format!("task {id}")));
    }
    Ok(())
}

pub fn get_task(conn: &Connection, id: &str) -> AppResult<Option<TaskRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_task_id, lifecycle, native_task_ref, started_at, ended_at
         FROM task WHERE id = ?1",
    )?;
    let mut rows = stmt.query(rusqlite::params![id])?;
    match rows.next()? {
        Some(r) => Ok(Some(TaskRow {
            id: r.get(0)?,
            parent_task_id: r.get(1)?,
            lifecycle: r.get(2)?,
            native_task_ref: r.get(3)?,
            started_at: r.get(4)?,
            ended_at: r.get(5)?,
        })),
        None => Ok(None),
    }
}

// ===========================================================================
// route_request — credential-free persisted projection
// ===========================================================================

use super::identity::RouteRecord;

/// Persist the credential-free projection of one routed request. The full
/// [`RouteRecord`] carries everything the UI needs (requested vs. resolved
/// model, reason, observed status/usage/cache, generation-broken flag) and
/// nothing it must not (no key/secret/credential/token).
pub fn insert_route_request(conn: &Connection, rec: &RouteRecord) -> AppResult<()> {
    // prepare_cached: this runs on EVERY proxied request — skip re-parsing the
    // 22-column SQL each time (statement cached per connection).
    conn.prepare_cached(
        "INSERT INTO route_request
           (request_id, task_id, agent_id, logical_session, subagent_role, role_source,
            requested_model, requested_provider, resolved_endpoint_id, resolved_model,
            protocol, route_reason, http_status, usage_input, usage_output,
            cache_creation, cache_read, tool_calls, tool_names, generation_broken,
            started_at, ended_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
    )?
    .execute(rusqlite::params![
        rec.request_id.to_string(),
        rec.task_id.to_string(),
        rec.agent_id,
        rec.logical_session,
        rec.subagent_role,
        rec.role_source,
        rec.requested_model,
        rec.requested_provider,
        rec.resolved_endpoint_id,
        rec.resolved_model,
        rec.protocol,
        rec.route_reason,
        rec.http_status,
        rec.usage_input,
        rec.usage_output,
        rec.cache_creation,
        rec.cache_read,
        rec.tool_calls,
        rec.tool_names,
        rec.generation_broken as i64,
        rec.started_at,
        rec.ended_at,
    ])?;
    Ok(())
}

/// Backfill the observed outcome fields of a request after the upstream
/// response streams. `generation_broken` is set when the response was produced
/// by a fresh upstream generation after a mid-stream migration (correction #2).
#[allow(clippy::too_many_arguments)]
pub fn update_route_request_outcome(
    conn: &Connection,
    request_id: &str,
    http_status: Option<i64>,
    usage_input: Option<i64>,
    usage_output: Option<i64>,
    cache_creation: Option<i64>,
    cache_read: Option<i64>,
    tool_calls: Option<i64>,
    tool_names: Option<String>,
    generation_broken: bool,
    ended_at: i64,
) -> AppResult<bool> {
    // prepare_cached: per-request hot path (same rationale as insert_route_request).
    // Tool columns overwrite (SSE passes None here; the post-stream backfill
    // COALESCEs its observation in afterwards).
    let n = conn
        .prepare_cached(
            "UPDATE route_request SET
               http_status       = ?1,
               usage_input       = ?2,
               usage_output      = ?3,
               cache_creation    = ?4,
               cache_read        = ?5,
               tool_calls        = ?6,
               tool_names        = ?7,
               generation_broken = ?8,
               ended_at          = ?9
             WHERE request_id = ?10",
        )?
        .execute(rusqlite::params![
            http_status,
            usage_input,
            usage_output,
            cache_creation,
            cache_read,
            tool_calls,
            tool_names,
            generation_broken as i64,
            ended_at,
            request_id,
        ])?;
    Ok(n > 0)
}

/// Backfill the usage + tool-call count observed while an SSE stream was
/// relayed. The outcome UPDATE that runs when the 2xx stream is handed to the
/// agent writes NULL usage (the stream hasn't been read yet); this runs once
/// the stream ends and fills only what was actually observed — COALESCE keeps
/// any earlier non-NULL value. Best-effort observability, never a continuation
/// claim.
pub fn backfill_route_request_usage(
    conn: &Connection,
    request_id: &str,
    usage: &super::gateway::stream::ObservedUsage,
    tool_calls: Option<i64>,
    tool_names: Option<String>,
) -> AppResult<bool> {
    let n = conn
        .prepare_cached(
            "UPDATE route_request SET
               usage_input    = COALESCE(?1, usage_input),
               usage_output   = COALESCE(?2, usage_output),
               cache_creation = COALESCE(?3, cache_creation),
               cache_read     = COALESCE(?4, cache_read),
               tool_calls     = COALESCE(?5, tool_calls),
               tool_names     = COALESCE(?6, tool_names)
             WHERE request_id = ?7",
        )?
        .execute(rusqlite::params![
            usage.input,
            usage.output,
            usage.cache_creation,
            usage.cache_read,
            tool_calls,
            tool_names,
            request_id,
        ])?;
    Ok(n > 0)
}

/// Full route history for one Task, oldest-first — the lineage the UI shows
/// under "why this provider/model" and what a migration decision consults.
pub fn route_history_for_task(conn: &Connection, task_id: &str) -> AppResult<Vec<RouteRecord>> {
    let mut stmt = conn.prepare(
        "SELECT request_id, task_id, agent_id, logical_session, subagent_role, role_source,
                requested_model, requested_provider, resolved_endpoint_id, resolved_model,
                protocol, route_reason, http_status, usage_input, usage_output,
                cache_creation, cache_read, tool_calls, tool_names, generation_broken, started_at, ended_at
         FROM route_request WHERE task_id = ?1 ORDER BY started_at",
    )?;
    let rows = stmt.query_map(rusqlite::params![task_id], |r| {
        let request_id: String = r.get(0)?;
        let task_id_str: String = r.get(1)?;
        Ok(RouteRecord {
            request_id: parse_uuid(&request_id, "request_id"),
            task_id: parse_uuid(&task_id_str, "task_id"),
            agent_id: r.get(2)?,
            logical_session: r.get(3)?,
            subagent_role: r.get(4)?,
            role_source: r.get(5)?,
            requested_model: r.get(6)?,
            requested_provider: r.get(7)?,
            resolved_endpoint_id: r.get(8)?,
            resolved_model: r.get(9)?,
            protocol: r.get(10)?,
            route_reason: r.get(11)?,
            http_status: r.get(12)?,
            usage_input: r.get(13)?,
            usage_output: r.get(14)?,
            cache_creation: r.get(15)?,
            cache_read: r.get(16)?,
            tool_calls: r.get(17)?,
            tool_names: r.get(18)?,
            generation_broken: r.get::<_, i64>(19)? != 0,
            started_at: r.get(20)?,
            ended_at: r.get(21)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The N most-recent `route_request` rows across all tasks (credential-free
/// `RouteRecord` projections), newest-first. Backs the Gateway page's "recent
/// activity" list — real data only; an empty result is shown as-is (no mock).
pub fn recent_route_requests(conn: &Connection, limit: i64) -> AppResult<Vec<RouteRecord>> {
    let mut stmt = conn.prepare(
        "SELECT request_id, task_id, agent_id, logical_session, subagent_role, role_source,
                requested_model, requested_provider, resolved_endpoint_id, resolved_model,
                protocol, route_reason, http_status, usage_input, usage_output,
                cache_creation, cache_read, tool_calls, tool_names, generation_broken, started_at, ended_at
         FROM route_request ORDER BY started_at DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![limit], |r| {
        let request_id: String = r.get(0)?;
        let task_id_str: String = r.get(1)?;
        Ok(RouteRecord {
            request_id: parse_uuid(&request_id, "request_id"),
            task_id: parse_uuid(&task_id_str, "task_id"),
            agent_id: r.get(2)?,
            logical_session: r.get(3)?,
            subagent_role: r.get(4)?,
            role_source: r.get(5)?,
            requested_model: r.get(6)?,
            requested_provider: r.get(7)?,
            resolved_endpoint_id: r.get(8)?,
            resolved_model: r.get(9)?,
            protocol: r.get(10)?,
            route_reason: r.get(11)?,
            http_status: r.get(12)?,
            usage_input: r.get(13)?,
            usage_output: r.get(14)?,
            cache_creation: r.get(15)?,
            cache_read: r.get(16)?,
            tool_calls: r.get(17)?,
            tool_names: r.get(18)?,
            generation_broken: r.get::<_, i64>(19)? != 0,
            started_at: r.get(20)?,
            ended_at: r.get(21)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// `(count, last_started_at)` for requests at or after `since_ms` — the Gateway
/// page's session-scoped counters ("this run"). `since_ms` is the gateway's
/// `started_at`; when the gateway is down (`None`) the result is `(0, None)`.
/// Real data from `route_request`; never synthesized.
pub fn gateway_session_stats(
    conn: &Connection,
    since_ms: Option<i64>,
) -> AppResult<(i64, Option<i64>)> {
    match since_ms {
        Some(s) => {
            let (count, last): (i64, Option<i64>) = conn.query_row(
                "SELECT COUNT(*), MAX(started_at) FROM route_request WHERE started_at >= ?1",
                rusqlite::params![s],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok((count, last))
        }
        None => Ok((0, None)),
    }
}

/// Count of in-flight (non-terminal) tasks — `lifecycle NOT IN` the terminal
/// set. `forward.rs` inserts tasks as `'born'` and only some mid-flight
/// transitions are set, so the negative filter is the reliable "active" test.
pub fn active_task_count(conn: &Connection) -> AppResult<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM task
         WHERE lifecycle NOT IN ('done', 'failed', 'generationbroken')",
        [],
        |r| r.get(0),
    )?)
}

/// One row of the task-summary view observability). Aggregates the
/// `route_request` table by task so the UI can list tasks without pulling
/// every request row.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskSummary {
    pub task_id: String,
    pub agent_id: String,
    pub logical_session: Option<String>,
    /// Number of request attempts recorded for this task (retries +
    /// migrations produce multiple rows).
    pub request_count: i64,
    /// HTTP status of the most recent request, when observed.
    pub latest_status: Option<i64>,
    /// `true` when ANY request in the task was flagged generation-broken.
    pub generation_broken: bool,
    pub first_seen: i64,
    pub last_seen: i64,
}

/// One subagent role observed on an agent's routed requests. `role` is the
/// policy key (`claude:researcher`, `opencode:research`, …) — it maps 1:1 to
/// a `routing_policy.role` value, so the UI can offer these as suggestions.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DetectedRoleSummary {
    pub role: String,
    /// How many requests carried this role.
    pub request_count: i64,
    /// Most recent request with this role (ms epoch).
    pub last_seen: i64,
}

/// Distinct subagent roles observed per agent, most-recently-active first.
/// `main` is filtered (the main thread is not a subagent; the catch-all `*`
/// policy already covers it). `limit` caps the list (a suggestion strip shows
/// a recent window).
pub fn detected_roles(conn: &Connection, agent_id: &str, limit: i64) -> AppResult<Vec<DetectedRoleSummary>> {
    let mut stmt = conn.prepare(
        "SELECT subagent_role, COUNT(*) AS request_count, MAX(started_at) AS last_seen
         FROM route_request
         WHERE agent_id = ?1 AND subagent_role IS NOT NULL AND subagent_role != 'main'
         GROUP BY subagent_role
         ORDER BY last_seen DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![agent_id, limit], |r| {
        Ok(DetectedRoleSummary {
            role: r.get(0)?,
            request_count: r.get(1)?,
            last_seen: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Summaries for every task the gateway has observed, most-recently-active
/// first. `limit` caps the list (the UI shows a recent window).
pub fn task_summaries(conn: &Connection, limit: i64) -> AppResult<Vec<TaskSummary>> {
    let mut stmt = conn.prepare(
        "SELECT task_id,
                MAX(agent_id),
                MAX(logical_session),
                COUNT(*) AS request_count,
                (SELECT http_status FROM route_request r2
                  WHERE r2.task_id = r.task_id
                  ORDER BY started_at DESC LIMIT 1) AS latest_status,
                MAX(generation_broken) AS generation_broken,
                MIN(started_at) AS first_seen,
                MAX(started_at) AS last_seen
         FROM route_request r
         GROUP BY task_id
         ORDER BY last_seen DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![limit], |r| {
        Ok(TaskSummary {
            task_id: r.get(0)?,
            agent_id: r.get(1)?,
            logical_session: r.get(2)?,
            request_count: r.get(3)?,
            latest_status: r.get(4)?,
            generation_broken: r.get::<_, i64>(5)? != 0,
            first_seen: r.get(6)?,
            last_seen: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Summaries for tasks whose `logical_session` matches a session id (the
/// `/sessions/:id` lineage view — the gateway records the agent-native
/// session id on every route_request when the agent sends one).
pub fn tasks_for_session(
    conn: &Connection,
    logical_session: &str,
    limit: i64,
) -> AppResult<Vec<TaskSummary>> {
    let mut stmt = conn.prepare(
        "SELECT task_id,
                MAX(agent_id),
                MAX(logical_session),
                COUNT(*) AS request_count,
                (SELECT http_status FROM route_request r2
                  WHERE r2.task_id = r.task_id
                  ORDER BY started_at DESC LIMIT 1) AS latest_status,
                MAX(generation_broken) AS generation_broken,
                MIN(started_at) AS first_seen,
                MAX(started_at) AS last_seen
         FROM route_request r
         WHERE logical_session = ?1
         GROUP BY task_id
         ORDER BY last_seen DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![logical_session, limit], |r| {
        Ok(TaskSummary {
            task_id: r.get(0)?,
            agent_id: r.get(1)?,
            logical_session: r.get(2)?,
            request_count: r.get(3)?,
            latest_status: r.get(4)?,
            generation_broken: r.get::<_, i64>(5)? != 0,
            first_seen: r.get(6)?,
            last_seen: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn parse_uuid(s: &str, field: &str) -> uuid::Uuid {
    uuid::Uuid::parse_str(s).unwrap_or_else(|_| {
        // A malformed uuid in storage is a corruption we want to notice loudly
        // rather than silently turning into a nil id; but panicking in a query
        // path is too much. Log and return nil so the caller sees a gap.
        tracing::error!("corrupt uuid in route_request {field}: {s}");
        uuid::Uuid::nil()
    })
}

// ===========================================================================
// route_migration
// ===========================================================================

/// One migration event. Auth/4xx errors never become migrations — only
/// quota/rate-limit/temporary-5xx/timeout/policy/user-override do. Reason
/// vocabulary is owned by the failure taxonomy; the store just persists
/// whatever the engine records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteMigrationRow {
    pub id: String,
    pub request_id: String,
    pub task_id: String,
    pub from_endpoint_id: Option<String>,
    pub to_endpoint_id: Option<String>,
    pub reason: String,
    pub detail: Option<String>,
    pub at_ms: i64,
}

pub fn insert_route_migration(conn: &Connection, row: &RouteMigrationRow) -> AppResult<()> {
    // prepare_cached: migration hops happen on the request hot path.
    conn.prepare_cached(
        "INSERT INTO route_migration
           (id, request_id, task_id, from_endpoint_id, to_endpoint_id, reason, detail, at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?
    .execute(rusqlite::params![
        row.id,
        row.request_id,
        row.task_id,
        row.from_endpoint_id,
        row.to_endpoint_id,
        row.reason,
        row.detail,
        row.at_ms,
    ])?;
    Ok(())
}

pub fn migrations_for_task(conn: &Connection, task_id: &str) -> AppResult<Vec<RouteMigrationRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, request_id, task_id, from_endpoint_id, to_endpoint_id, reason, detail, at_ms
         FROM route_migration WHERE task_id = ?1 ORDER BY at_ms",
    )?;
    let rows = stmt.query_map(rusqlite::params![task_id], |r| {
        Ok(RouteMigrationRow {
            id: r.get(0)?,
            request_id: r.get(1)?,
            task_id: r.get(2)?,
            from_endpoint_id: r.get(3)?,
            to_endpoint_id: r.get(4)?,
            reason: r.get(5)?,
            detail: r.get(6)?,
            at_ms: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ===========================================================================
// model_catalog
// ===========================================================================

/// One (endpoint, model) entry the router can consider, with its merged
/// `ModelAbilities` payload. Built by the capability registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalogRow {
    pub endpoint_id: String,
    pub model_id: String,
    /// Serialized [`crate::model_abilities::ModelAbilities`].
    pub abilities_json: String,
}

pub fn upsert_model_catalog(conn: &Connection, row: &ModelCatalogRow) -> AppResult<()> {
    conn.execute(
        "INSERT INTO model_catalog (endpoint_id, model_id, abilities_json)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(endpoint_id, model_id) DO UPDATE SET abilities_json = excluded.abilities_json",
        rusqlite::params![row.endpoint_id, row.model_id, row.abilities_json],
    )?;
    Ok(())
}

pub fn delete_model_catalog_for_endpoint(conn: &Connection, endpoint_id: &str) -> AppResult<()> {
    conn.execute(
        "DELETE FROM model_catalog WHERE endpoint_id = ?1",
        rusqlite::params![endpoint_id],
    )?;
    Ok(())
}

pub fn list_model_catalog(conn: &Connection, endpoint_id: &str) -> AppResult<Vec<ModelCatalogRow>> {
    let mut stmt = conn.prepare(
        "SELECT endpoint_id, model_id, abilities_json FROM model_catalog
         WHERE endpoint_id = ?1 ORDER BY model_id",
    )?;
    let rows = stmt.query_map(rusqlite::params![endpoint_id], |r| {
        Ok(ModelCatalogRow {
            endpoint_id: r.get(0)?,
            model_id: r.get(1)?,
            abilities_json: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// `true` when a stored `route_reason` value is one the code emits. Used by
/// callers (and tests) to validate before persisting — keeps the reason
/// vocabulary honest even though the column itself is free-text.
pub fn is_known_route_reason(s: &str) -> bool {
    [
        RouteReason::Explicit.as_str(),
        RouteReason::Affinity.as_str(),
        RouteReason::Capability.as_str(),
        RouteReason::Fallback.as_str(),
        RouteReason::NoEligible.as_str(),
    ]
    .contains(&s)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;

    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::build_v1(&conn).unwrap();
        conn
    }

    #[test]
    fn routing_policy_upsert_get_fallback() {
        let conn = fresh_db();
        let now = 1_700_000_000;
        // No row → falls back to a synthesized default (task affinity, no injection).
        let p = routing_policy_for(&conn, "claude-code-cli", "claude:researcher").unwrap();
        assert_eq!(p.affinity_scope, "task");
        assert!(!p.inject_cache_control);
        assert!(p.migrate_on_quota);

        // Insert a per-role row + a catch-all row.
        upsert_routing_policy(
            &conn,
            &RoutingPolicyRow {
                agent_id: "claude-code-cli".into(),
                role: "claude:researcher".into(),
                preferred_endpoints: Some(r#"["ep-1"]"#.into()),
                fallback_endpoints: Some(r#"["ep-2"]"#.into()),
                allowed_models: None,
                migrate_on_quota: false,
                inject_cache_control: true,
                affinity_scope: "session".into(),
                updated_at: now,
            },
        )
        .unwrap();
        upsert_routing_policy(
            &conn,
            &RoutingPolicyRow {
                agent_id: "claude-code-cli".into(),
                role: "*".into(),
                preferred_endpoints: Some(r#"["ep-x"]"#.into()),
                fallback_endpoints: None,
                allowed_models: None,
                migrate_on_quota: true,
                inject_cache_control: false,
                affinity_scope: "task".into(),
                updated_at: now,
            },
        )
        .unwrap();

        // Specific role wins.
        let p = routing_policy_for(&conn, "claude-code-cli", "claude:researcher").unwrap();
        assert_eq!(p.role, "claude:researcher");
        assert!(p.inject_cache_control);
        assert!(!p.migrate_on_quota);
        assert_eq!(p.affinity_scope, "session");

        // Unknown role falls back to the catch-all.
        let p = routing_policy_for(&conn, "claude-code-cli", "claude:other").unwrap();
        assert_eq!(p.role, "*");
        assert!(!p.inject_cache_control);
        assert_eq!(p.preferred_endpoints.as_deref(), Some(r#"["ep-x"]"#));
    }

    #[test]
    fn task_lifecycle_sets_ended_at_only_on_terminal() {
        let conn = fresh_db();
        insert_task(
            &conn,
            &TaskRow {
                id: "task-1".into(),
                parent_task_id: None,
                lifecycle: "born".into(),
                native_task_ref: None,
                started_at: 100,
                ended_at: None,
            },
        )
        .unwrap();

        // Non-terminal: ended_at stays NULL.
        set_task_lifecycle(&conn, "task-1", TaskLifecycle::InFlight, 200).unwrap();
        let t = get_task(&conn, "task-1").unwrap().unwrap();
        assert_eq!(t.lifecycle, "inflight");
        assert!(t.ended_at.is_none());

        // Terminal: ended_at set.
        set_task_lifecycle(&conn, "task-1", TaskLifecycle::Done, 300).unwrap();
        let t = get_task(&conn, "task-1").unwrap().unwrap();
        assert_eq!(t.lifecycle, "done");
        assert_eq!(t.ended_at, Some(300));
    }

    /// Seed the task row a route_request row needs to satisfy its FKs
    /// (optionally resolved_endpoint_id → provider_endpoint).
    fn seed_task_chain(conn: &Connection, task_id: &str, with_endpoint: bool) {
        if with_endpoint {
            conn.execute(
                "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status)
                 VALUES ('ep-1','custom','Main',0,'unvalidated')",
                [],
            )
            .unwrap();
        }
        insert_task(
            conn,
            &TaskRow {
                id: task_id.into(),
                parent_task_id: None,
                lifecycle: "inflight".into(),
                native_task_ref: None,
                started_at: 0,
                ended_at: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn route_request_round_trip_and_history() {
        let conn = fresh_db();
        let task_id = uuid::Uuid::new_v4();
        // Seed the parent run/task + endpoint the FKs point at.
        seed_task_chain(&conn, &task_id.to_string(), true);
        let rec = RouteRecord {
            request_id: uuid::Uuid::new_v4(),
            task_id,
            agent_id: "claude-code-cli".into(),
            logical_session: Some("sess-1".into()),
            subagent_role: Some("main".into()),
            role_source: Some("native".into()),
            requested_model: Some("claude-3-opus".into()),
            requested_provider: None,
            resolved_endpoint_id: Some("ep-1".into()),
            resolved_model: Some("claude-3-opus".into()),
            protocol: Some("anthropic".into()),
            route_reason: "capability".into(),
            http_status: None,
            usage_input: None,
            usage_output: None,
            cache_creation: None,
            cache_read: None,
            tool_calls: None,
            tool_names: None,
            generation_broken: false,
            started_at: 100,
            ended_at: None,
        };
        insert_route_request(&conn, &rec).unwrap();

        // Backfill outcome (buffered-path semantics: tools ride the finalize).
        update_route_request_outcome(
            &conn,
            &rec.request_id.to_string(),
            Some(200),
            Some(123),
            Some(456),
            Some(0),
            Some(100),
            Some(3),
            Some(r#"{"Bash": 3}"#.to_string()),
            false,
            200,
        )
        .unwrap();

        let hist = route_history_for_task(&conn, &rec.task_id.to_string()).unwrap();
        assert_eq!(hist.len(), 1);
        let h = &hist[0];
        assert_eq!(h.http_status, Some(200));
        assert_eq!(h.usage_input, Some(123));
        assert_eq!(h.tool_calls, Some(3));
        assert_eq!(h.tool_names.as_deref(), Some(r#"{"Bash": 3}"#));
        assert_eq!(h.cache_read, Some(100));
        assert!(!h.generation_broken);
        assert_eq!(h.ended_at, Some(200));
        assert_eq!(h.resolved_endpoint_id.as_deref(), Some("ep-1"));
    }

    #[test]
    fn task_summaries_aggregate_requests_per_task() {
        let conn = fresh_db();
        // Seed two task rows, then record three requests:
        // t-1 gets two attempts (one generation-broken), t-2 one.
        // Task ids are Nestra UUIDs (the store serializes RouteRecord.task_id
        // to string); use real UUIDs so the FK + round-trip stay valid.
        let t1 = uuid::Uuid::new_v4();
        let t2 = uuid::Uuid::new_v4();
        for task_id in [t1, t2] {
            insert_task(
                &conn,
                &TaskRow {
                    id: task_id.to_string(),
                    parent_task_id: None,
                    lifecycle: "inflight".into(),
                    native_task_ref: None,
                    started_at: 0,
                    ended_at: None,
                },
            )
            .unwrap();
        }
        for (task_id, gen_broken) in [(t1, true), (t1, false), (t2, false)] {
            let rec = RouteRecord {
                request_id: uuid::Uuid::new_v4(),
                task_id,
                agent_id: "claude-code-cli".into(),
                logical_session: Some("sess-1".into()),
                subagent_role: Some("main".into()),
                role_source: Some("native".into()),
                requested_model: Some("m".into()),
                requested_provider: None,
                resolved_endpoint_id: None,
                resolved_model: None,
                protocol: None,
                route_reason: "capability".into(),
                http_status: Some(if gen_broken { 503 } else { 200 }),
                usage_input: None,
                usage_output: None,
                cache_creation: None,
                cache_read: None,
                tool_calls: None,
            tool_names: None,
                generation_broken: gen_broken,
                started_at: 100,
                ended_at: Some(200),
            };
            insert_route_request(&conn, &rec).unwrap();
        }

        let summaries = task_summaries(&conn, 50).unwrap();
        assert_eq!(summaries.len(), 2, "aggregated by task_id");
        let s1 = summaries.iter().find(|s| s.task_id == t1.to_string()).unwrap();
        assert_eq!(s1.request_count, 2);
        assert!(s1.generation_broken, "t1 has one broken attempt");
        let s2 = summaries.iter().find(|s| s.task_id == t2.to_string()).unwrap();
        assert_eq!(s2.request_count, 1);
        assert!(!s2.generation_broken);

        // Per-session filter returns both (same logical session).
        let sess = tasks_for_session(&conn, "sess-1", 50).unwrap();
        assert_eq!(sess.len(), 2);
        // Unknown session → empty.
        assert!(tasks_for_session(&conn, "nope", 50).unwrap().is_empty());
    }

    #[test]
    fn detected_roles_aggregates_by_role_and_filters_main() {
        let conn = fresh_db();
        let t1 = uuid::Uuid::new_v4();
        let t2 = uuid::Uuid::new_v4();
        for task_id in [t1, t2] {
            insert_task(
                &conn,
                &TaskRow {
                    id: task_id.to_string(),
                    parent_task_id: None,
                    lifecycle: "inflight".into(),
                    native_task_ref: None,
                    started_at: 0,
                    ended_at: None,
                },
            )
            .unwrap();
        }
        // claude:researcher ×2 (older), opencode:research ×1 (newest),
        // main ×1 (must be filtered).
        for (task_id, role, at) in [
            (t1, "claude:researcher", 100),
            (t1, "claude:researcher", 200),
            (t2, "opencode:research", 300),
            (t2, "main", 400),
        ] {
            insert_route_request(
                &conn,
                &RouteRecord {
                    request_id: uuid::Uuid::new_v4(),
                    task_id,
                    agent_id: "opencode-desktop".into(),
                    logical_session: Some("sess-1".into()),
                    subagent_role: Some(role.into()),
                    role_source: Some("heuristic".into()),
                    requested_model: Some("nestra".into()),
                    requested_provider: None,
                    resolved_endpoint_id: None,
                    resolved_model: None,
                    protocol: None,
                    route_reason: "capability".into(),
                    http_status: Some(200),
                    usage_input: None,
                    usage_output: None,
                    cache_creation: None,
                    cache_read: None,
                    tool_calls: None,
            tool_names: None,
                    generation_broken: false,
                    started_at: at,
                    ended_at: Some(at + 10),
                },
            )
            .unwrap();
        }

        let roles = detected_roles(&conn, "opencode-desktop", 20).unwrap();
        assert_eq!(roles.len(), 2, "main must be filtered out");
        assert_eq!(roles[0].role, "opencode:research", "newest last_seen first");
        assert_eq!(roles[0].request_count, 1);
        assert_eq!(roles[1].role, "claude:researcher");
        assert_eq!(roles[1].request_count, 2);
        // Another agent sees nothing.
        assert!(detected_roles(&conn, "claude-code-cli", 20).unwrap().is_empty());
        // Limit applies.
        assert_eq!(detected_roles(&conn, "opencode-desktop", 1).unwrap().len(), 1);
    }

    #[test]
    fn route_migration_round_trip() {
        let conn = fresh_db();
        // Seed the parent task + a route_request row the migration FKs point at.
        // Migrations can reference any endpoint (including ones not in the
        // catalog), and route_migration itself has no FK to provider_endpoint,
        // so we don't need to seed endpoints here.
        let task_id = uuid::Uuid::new_v4();
        let request_id = uuid::Uuid::new_v4();
        seed_task_chain(&conn, &task_id.to_string(), false);
        insert_route_request(
            &conn,
            &RouteRecord {
                request_id,
                task_id,
                agent_id: "claude-code-cli".into(),
                logical_session: Some("sess-1".into()),
                subagent_role: Some("main".into()),
                role_source: Some("native".into()),
                requested_model: None,
                requested_provider: None,
                resolved_endpoint_id: None,
                resolved_model: None,
                protocol: None,
                route_reason: "capability".into(),
                http_status: None,
                usage_input: None,
                usage_output: None,
                cache_creation: None,
                cache_read: None,
                tool_calls: None,
            tool_names: None,
                generation_broken: false,
                started_at: 0,
                ended_at: None,
            },
        )
        .unwrap();

        let row = RouteMigrationRow {
            id: "m-1".into(),
            request_id: request_id.to_string(),
            task_id: task_id.to_string(),
            from_endpoint_id: Some("ep-1".into()),
            to_endpoint_id: Some("ep-2".into()),
            reason: "quota_exhausted".into(),
            detail: Some("5h window elapsed".into()),
            at_ms: 100,
        };
        insert_route_migration(&conn, &row).unwrap();
        let got = migrations_for_task(&conn, &task_id.to_string()).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].reason, "quota_exhausted");
        assert_eq!(got[0].to_endpoint_id.as_deref(), Some("ep-2"));
    }

    /// Credential-boundary guard (correction #5). Serializes every persisted
    /// struct in this module and asserts no JSON key contains a secret-naming
    /// substring. A contributor adding a credential column to any row type
    /// breaks this test before merge.
    #[test]
    fn no_persisted_secret_fields() {
        let forbidden = ["key", "secret", "credential", "token", "password", "passwd", "apikey"];

        let samples: Vec<(String, serde_json::Value)> = vec![
            (
                "DetectedRoleSummary".into(),
                serde_json::to_value(DetectedRoleSummary {
                    role: "claude:researcher".into(),
                    request_count: 2,
                    last_seen: 100,
                })
                .unwrap(),
            ),
            (
                "RoutingPolicyRow".into(),
                serde_json::to_value(RoutingPolicyRow::default_for("a", "main", 0)).unwrap(),
            ),
            (
                "TaskRow".into(),
                serde_json::to_value(TaskRow {
                    id: "t".into(),
                    parent_task_id: None,
                    lifecycle: "born".into(),
                    native_task_ref: None,
                    started_at: 0,
                    ended_at: None,
                })
                .unwrap(),
            ),
            (
                "RouteRecord".into(),
                serde_json::to_value(RouteRecord {
                    request_id: uuid::Uuid::nil(),
                    task_id: uuid::Uuid::nil(),
                    agent_id: "a".into(),
                    logical_session: None,
                    subagent_role: None,
                    role_source: None,
                    requested_model: None,
                    requested_provider: None,
                    resolved_endpoint_id: None,
                    resolved_model: None,
                    protocol: None,
                    route_reason: "capability".into(),
                    http_status: None,
                    usage_input: None,
                    usage_output: None,
                    cache_creation: None,
                    cache_read: None,
                    tool_calls: None,
            tool_names: None,
                    generation_broken: false,
                    started_at: 0,
                    ended_at: None,
                })
                .unwrap(),
            ),
            (
                "RouteMigrationRow".into(),
                serde_json::to_value(RouteMigrationRow {
                    id: "m".into(),
                    request_id: "r".into(),
                    task_id: "t".into(),
                    from_endpoint_id: None,
                    to_endpoint_id: None,
                    reason: "policy".into(),
                    detail: None,
                    at_ms: 0,
                })
                .unwrap(),
            ),
            (
                "ModelCatalogRow".into(),
                serde_json::to_value(ModelCatalogRow {
                    endpoint_id: "e".into(),
                    model_id: "m".into(),
                    abilities_json: "{}".into(),
                })
                .unwrap(),
            ),
        ];

        fn walk_keys(value: &serde_json::Value, keys: &mut Vec<String>) {
            match value {
                serde_json::Value::Object(map) => {
                    for (k, v) in map {
                        keys.push(k.clone());
                        walk_keys(v, keys);
                    }
                }
                serde_json::Value::Array(arr) => {
                    for v in arr {
                        walk_keys(v, keys);
                    }
                }
                _ => {}
            }
        }

        for (name, value) in samples {
            let mut keys = Vec::new();
            walk_keys(&value, &mut keys);
            for k in &keys {
                let lower = k.to_ascii_lowercase();
                for bad in forbidden {
                    assert!(
                        !lower.contains(bad),
                        "persisted struct {name} has a secret-named field {k:?} (contains {bad:?}) — credential boundary violation"
                    );
                }
            }
        }
    }
}
