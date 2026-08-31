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

use super::identity::TaskLifecycle;

// ===========================================================================
// routing_policy
// ===========================================================================

/// One routing-policy row, keyed by `(agent_id, role)`. The router reads
/// preferred/fallback endpoint lists, allowed-model globs, migration and
/// cache-injection toggles, and the affinity scope from here.
///
/// `role = "*"` is the catch-all default policy used when no per-role row
/// One entry of a policy's ordered route-target list: an explicit
/// (endpoint, model) pin. The router serves the first healthy, quota-ok
/// entry; failures walk the list in order.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RouteTarget {
    pub endpoint: String,
    pub model: String,
}

/// A routing-policy row. The policy layer's persisted shape (`routing_policy`
/// table). `routing_policy_for` resolves the agent/role/tier lookup chain and
/// synthesizes a default row on miss.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingPolicyRow {
    pub agent_id: String,
    pub role: String,
    /// JSON array of [`RouteTarget`] in priority order, or `None` for an
    /// empty list (routing fails closed for a role with no targets).
    pub route_targets: Option<String>,
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
            route_targets: None,
            migrate_on_quota: true,
            inject_cache_control: false,
            affinity_scope: "task".to_string(),
            updated_at: now,
        }
    }

    /// Parse `route_targets` into typed targets; empty on `None`/malformed.
    pub fn targets(&self) -> Vec<RouteTarget> {
        self.route_targets
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<RouteTarget>>(s).ok())
            .unwrap_or_default()
    }
}

pub fn upsert_routing_policy(conn: &Connection, row: &RoutingPolicyRow) -> AppResult<()> {
    conn.execute(
        "INSERT INTO routing_policy
           (agent_id, role, route_targets,
            migrate_on_quota, inject_cache_control, affinity_scope, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(agent_id, role) DO UPDATE SET
           route_targets        = excluded.route_targets,
           migrate_on_quota     = excluded.migrate_on_quota,
           inject_cache_control = excluded.inject_cache_control,
           affinity_scope       = excluded.affinity_scope,
           updated_at           = excluded.updated_at",
        rusqlite::params![
            row.agent_id,
            row.role,
            row.route_targets,
            row.migrate_on_quota as i64,
            row.inject_cache_control as i64,
            row.affinity_scope,
            row.updated_at,
        ],
    )?;
    Ok(())
}

/// Look up the most specific policy for `(agent_id, role)`, falling back to
/// the agent's tier row (`tier:haiku`/`tier:sonnet`/`tier:opus` — only when the
/// request carried a classifiable budget tier), then the `role = "*"`
/// catch-all, then a synthesized default. Never errors on a missing row — the
/// router always gets *some* policy.
///
/// Specificity order: exact role > tier > wildcard. A subagent's own policy
/// (e.g. `claude:researcher`) therefore still governs all of its requests,
/// including its background-tier ones.
pub fn routing_policy_for(
    conn: &Connection,
    agent_id: &str,
    role: &str,
    tier: Option<&super::identity::BudgetTier>,
) -> AppResult<RoutingPolicyRow> {
    if let Some(row) = routing_policy_exact(conn, agent_id, role)? {
        return Ok(row);
    }
    if let Some(t) = tier {
        if let Some(row) = routing_policy_exact(conn, agent_id, &t.as_policy_key())? {
            return Ok(row);
        }
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
        "SELECT agent_id, role, route_targets,
                migrate_on_quota, inject_cache_control, affinity_scope, updated_at
         FROM routing_policy WHERE agent_id = ?1 AND role = ?2",
    )?;
    let mut rows = stmt.query(rusqlite::params![agent_id, role])?;
    match rows.next()? {
        Some(r) => Ok(Some(RoutingPolicyRow {
            agent_id: r.get(0)?,
            role: r.get(1)?,
            route_targets: r.get(2)?,
            migrate_on_quota: r.get::<_, i64>(3)? != 0,
            inject_cache_control: r.get::<_, i64>(4)? != 0,
            affinity_scope: r.get(5)?,
            updated_at: r.get(6)?,
        })),
        None => Ok(None),
    }
}

pub fn list_routing_policies(conn: &Connection, agent_id: &str) -> AppResult<Vec<RoutingPolicyRow>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, role, route_targets,
                migrate_on_quota, inject_cache_control, affinity_scope, updated_at
         FROM routing_policy WHERE agent_id = ?1 ORDER BY role",
    )?;
    let rows = stmt.query_map(rusqlite::params![agent_id], |r| {
        Ok(RoutingPolicyRow {
            agent_id: r.get(0)?,
            role: r.get(1)?,
            route_targets: r.get(2)?,
            migrate_on_quota: r.get::<_, i64>(3)? != 0,
            inject_cache_control: r.get::<_, i64>(4)? != 0,
            affinity_scope: r.get(5)?,
            updated_at: r.get(6)?,
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
/// Flip an attempt to 499 ONLY while it is still open (`http_status` NULL).
/// A post-completion agent disconnect (hyper can drop the response body
/// without polling its end frame, firing the abort guard AFTER the full
/// response was delivered and the outcome finalized) must not overwrite a
/// terminal outcome nor fail the done task. Returns whether the row flipped.
pub fn mark_route_request_aborted_if_open(
    conn: &Connection,
    request_id: &str,
    ended_at: i64,
) -> AppResult<bool> {
    let n = conn
        .prepare_cached(
            "UPDATE route_request SET http_status = 499, ended_at = ?2
             WHERE request_id = ?1 AND http_status IS NULL",
        )?
        .execute(rusqlite::params![request_id, ended_at])?;
    Ok(n > 0)
}

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

// ===========================================================================
// Usage summary (dashboard)
// ===========================================================================

/// One (day, agent, endpoint, model) usage bucket. `cost_usd` is computed at
/// read time against the current price catalog (models.dev + per-endpoint
/// overrides) — prices change, so spend is never persisted. `None` when no
/// price is known for any component (unknown ≠ free).
#[derive(Debug, Clone, Serialize)]
pub struct UsageRow {
    pub day: String,
    pub agent_id: String,
    pub endpoint_id: String,
    pub model_id: String,
    pub requests: i64,
    pub usage_input: i64,
    pub usage_output: i64,
    pub cache_creation: i64,
    pub cache_read: i64,
    pub cost_usd: Option<f64>,
}

/// The two non-overlapping halves of the usage dashboard, returned as
/// day-grain rows the caller sums: `usage_daily` holds everything pruned out
/// of `route_request` (lifetime history), the live table holds the current
/// retention window. A calendar day can appear in both — its rows are
/// disjoint sets, so summing is exact. `agent` filters; `days` bounds the
/// window (`None` = lifetime). `prices` resolves (endpoint, model) →
/// catalog pricing; rows without a price carry `cost_usd: None`.
pub fn usage_summary_rows(
    conn: &Connection,
    agent: Option<&str>,
    days: Option<u64>,
    prices: &dyn Fn(&str, &str) -> Option<crate::model_abilities::CostPerMtok>,
) -> AppResult<Vec<UsageRow>> {
    let mut rows: Vec<UsageRow> = Vec::new();
    let mut push = |q: &str, params: &[&dyn rusqlite::ToSql]| -> AppResult<()> {
        let mut stmt = conn.prepare(q)?;
        let mapped = stmt.query_map(params, |r| {
            Ok(UsageRow {
                day: r.get(0)?,
                agent_id: r.get(1)?,
                endpoint_id: r.get(2)?,
                model_id: r.get(3)?,
                requests: r.get(4)?,
                usage_input: r.get(5)?,
                usage_output: r.get(6)?,
                cache_creation: r.get(7)?,
                cache_read: r.get(8)?,
                cost_usd: None,
            })
        })?;
        rows.extend(mapped.collect::<rusqlite::Result<Vec<_>>>()?);
        Ok(())
    };

    // Folded history (exact day strings, newest last).
    if let Some(d) = days {
        push(
            "SELECT day, agent_id, endpoint_id, model_id, requests, usage_input,
                    usage_output, cache_creation, cache_read
             FROM usage_daily
             WHERE (?1 IS NULL OR agent_id = ?1) AND day >= date('now', ?2)
             ORDER BY day",
            &[
                &agent as &dyn rusqlite::ToSql,
                &format!("-{} days", d.max(1)),
            ],
        )?;
    } else {
        push(
            "SELECT day, agent_id, endpoint_id, model_id, requests, usage_input,
                    usage_output, cache_creation, cache_read
             FROM usage_daily WHERE ?1 IS NULL OR agent_id = ?1 ORDER BY day",
            &[&agent as &dyn rusqlite::ToSql],
        )?;
    }

    // Live window (rows not yet folded — everything still in route_request).
    let live_since = days
        .map(|d| chrono::Utc::now().timestamp_millis() - (d.max(1) as i64 * 86_400_000))
        .unwrap_or(0);
    push(
        "SELECT strftime('%Y-%m-%d', started_at/1000, 'unixepoch'), agent_id,
                COALESCE(resolved_endpoint_id, ''), COALESCE(resolved_model, ''),
                count(*), COALESCE(sum(usage_input), 0), COALESCE(sum(usage_output), 0),
                COALESCE(sum(cache_creation), 0), COALESCE(sum(cache_read), 0)
         FROM route_request
         WHERE (?1 IS NULL OR agent_id = ?1) AND started_at >= ?2
         GROUP BY 1, 2, 3, 4
         ORDER BY 1",
        &[&agent as &dyn rusqlite::ToSql, &live_since],
    )?;

    // Read-time pricing: components with a known price contribute; a row
    // whose priced components are all unknown carries `None`, never 0.
    for row in &mut rows {
        if let Some(p) = prices(&row.endpoint_id, &row.model_id) {
            let mut cost = 0.0;
            let mut known = false;
            let mut per_m = |tokens: i64, price: Option<f64>| {
                if let Some(pp) = price {
                    cost += tokens as f64 / 1_000_000.0 * pp;
                    known = true;
                }
            };
            per_m(row.usage_input, p.input);
            per_m(row.usage_output, p.output);
            per_m(row.cache_read, p.cache_read);
            per_m(row.cache_creation, p.cache_write);
            row.cost_usd = known.then_some(cost);
        }
    }
    Ok(rows)
}

/// Wipe ALL gateway observability data: tasks (cascading to route_request /
/// route_migration), the lifetime usage rollup, and the persisted affinity
/// snapshot. Configuration — endpoints, policies, bindings, secrets, imported
/// sessions — is untouched. The deletes run in ONE transaction: a partial
/// wipe (tasks gone, usage/affinity left) must never be observable. Callers
/// must ALSO clear the in-memory affinity table or the runtime re-persists
/// its snapshot (see `orch_obs_clear`).
pub fn clear_observability(conn: &Connection) -> AppResult<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM task", [])?;
    tx.execute("DELETE FROM usage_daily", [])?;
    // Affinity rides in setting_kv (session-grain snapshot), not a table.
    tx.execute("DELETE FROM setting_kv WHERE key = 'route_affinity'", [])?;
    tx.commit()?;
    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests;
