//! The retry/migrate loop shared by both protocol handlers.
//!
//! [`run_with_migration`] is the gateway's migration engine loop: it resolves
//! a route, forwards one attempt, classifies the failure, asks
//! [`migration::decide`] what to do next, and acts on the decision —
//! retrying the same provider (bounded, with backoff), migrating to a
//! fallback (re-resolve skips degraded/quota-exhausted endpoints), or
//! surfacing the upstream error to the agent.
//!
//! The protocol-specific parts (upstream URL shape, auth header, usage
//! mapping, error envelope) stay in the handlers; this module owns the
//! shared bookkeeping: route_request rows, health/quota outcomes, the
//! route_migration row, and the task_id-preserving request_id rotation.
//!
//! ## Generation/side-effect honesty (correction #3)
//!
//! - A failure with NO response bytes relayed is safe to retry/migrate with
//!   `generation_broken = false` on the next attempt.
//! - A failure after bytes were received (buffered body read interrupted)
//!   is a broken generation: the next attempt's `route_request` row is
//!   inserted with `generation_broken = true`.
//! - A request whose body declares tools/functions is never blind-retried:
//!   `side_effect_risk` forces `Surface` so a possibly-executed tool call is
//!   not double-executed.
//!
//! Note: for SSE responses the gateway commits to relaying the stream the
//! moment it returns from the forward closure, so a mid-stream SSE drop is
//! not observable inside this loop — the observable mid-stream failure is
//! the buffered (non-SSE) path, where `body_error` reports the interrupt and
//! `generation_started` reports that bytes had already flowed.

use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{HeaderMap, Response, StatusCode};

use crate::error::{AppError, AppResult};
use crate::orchestration::gateway::stream::{GatewayBody, ObservedUsage};
use crate::orchestration::health::{FailureClass, HealthOutcome};
use crate::orchestration::identity::{
    ResolvedRoute, RouteReason, RouteRecord, TaskContext, TaskLifecycle,
};
use crate::orchestration::migration::{self, MigrationDecision};
use crate::orchestration::store;

use super::protocol_anthropic::build_agent_response;
use super::GatewayState;

/// A boxed, `Send` forward attempt future (one HTTP dial + relay).
pub type ForwardFuture = Pin<Box<dyn Future<Output = ForwardOutcome> + Send>>;

/// A boxed, `Send` route-resolution future. Resolve touches the DB (async
/// lock) — it must be a future so the loop can `await` it from the Tokio
/// runtime, never a blocking call on a worker thread.
pub type ResolveFuture = Pin<Box<dyn Future<Output = AppResult<ResolvedRoute>> + Send>>;

/// What one `forward` attempt produced. `Responded` means an HTTP response
/// was received (any status); `Unreachable` means no response at all
/// (connect failure, credential/build error).
pub enum ForwardOutcome {
    Responded {
        status: StatusCode,
        headers: HeaderMap,
        body: GatewayBody,
        usage: Option<ObservedUsage>,
        /// True when ANY response bytes were received before the failure
        /// (generation may have started — correction #3 state 2).
        generation_started: bool,
        /// `Some(msg)` when a buffered body read was interrupted mid-stream
        /// (partial bytes lost). The response is NOT usable.
        body_error: Option<String>,
        /// Buffered-path tool observation (gateway-observed invocations; SSE
        /// backfills after the stream ends).
        tool_calls: Option<i64>,
        tool_names: Option<std::collections::BTreeMap<String, u64>>,
    },
    Unreachable {
        timeout: bool,
        message: String,
    },
}

impl ForwardOutcome {
    /// The status to record on the failed `route_request` row (502 for
    /// unreachable, matching convention).
    fn record_status(&self) -> u16 {
        match self {
            // A 2xx whose buffered body read failed mid-stream is NOT a
            // success — the recorded status must reflect the failure so
            // task_summaries doesn't show a "successful" attempt that was
            // retried (the old code stored the literal 200).
            ForwardOutcome::Responded {
                status,
                body_error: Some(_),
                ..
            } => {
                if status.is_success() {
                    502
                } else {
                    status.as_u16()
                }
            }
            ForwardOutcome::Responded { status, .. } => status.as_u16(),
            ForwardOutcome::Unreachable { .. } => 502,
        }
    }
}

/// Run the full retry/migrate loop for one inbound request.
///
/// `ctx` carries the initial task/request identity. On every retry and
/// migration the loop rotates `request_id` (via [`TaskContext::new_for_request`])
/// while preserving `task_id` — the task is the continuity handle, the
/// request id is per-attempt.
///
/// `resolve` re-resolves the route (the router skips degraded +
/// quota-exhausted endpoints, so a post-failure re-resolve naturally picks a
/// different endpoint). `forward` performs one protocol-specific attempt.
/// `error_response` builds the protocol's agent-facing JSON error envelope.
pub async fn run_with_migration(
    state: &GatewayState,
    mut ctx: TaskContext,
    agent_id: String,
    started_at: i64,
    side_effect_risk: bool,
    resolve: impl Fn(&TaskContext) -> ResolveFuture,
    forward: impl Fn(&TaskContext, &ResolvedRoute) -> ForwardFuture,
    error_response: impl Fn(StatusCode, &str) -> Response<GatewayBody>,
) -> Result<Response<GatewayBody>, AppError> {
    let policy_allows_migrate = read_migrate_policy(state, &ctx).await;
    // Honesty flag carried from one attempt to the next: set by the decision
    // when the failed attempt had already started a generation.
    let mut generation_broken = false;
    // Attempts on the CURRENT endpoint (reset on migration).
    let mut attempts: u32 = 0;
    // Endpoint that failed (set once a Migrate decision fires) — distinguishes
    // "no eligible route" (first resolve) from "no eligible fallback".
    let mut failed_endpoint: Option<String> = None;
    // RetrySame means "retry the SAME endpoint" — the route must be reused,
    // not re-resolved (a concurrent degrade/quota update could otherwise
    // silently switch endpoints while sharing the retry budget, and the
    // migrate_on_quota policy would be bypassed).
    let mut retry_route: Option<ResolvedRoute> = None;

    loop {
        let route = match retry_route.take() {
            Some(r) => r,
            None => resolve(&ctx).await?,
        };
        if route.reason == RouteReason::NoEligible || route.endpoint_id.is_empty() {
            let msg = if failed_endpoint.is_some() {
                "nestra gateway: no eligible fallback (all remaining endpoints degraded or quota-exhausted)"
            } else {
                "nestra gateway: no eligible route (no healthy, quota-ok endpoint for this task)"
            };
            // Terminal lifecycle: a resolve failure with no eligible route is
            // a task failure — leaving the task in 'born' would leak a
            // non-terminal row forever.
            mark_task_terminal(state, &ctx.task_id, TaskLifecycle::Failed).await;
            return Ok(error_response(StatusCode::SERVICE_UNAVAILABLE, msg));
        }

        // Persist the credential-free projection of this attempt. First
        // ensure the run + task rows exist (route_request has FKs to them;
        // the gateway never creates them elsewhere).
        let request_id = ctx.request_id.to_string();
        {
            let conn = state.db.lock().await;
            let mut rec = RouteRecord::from_route(&ctx, &route, started_at);
            rec.generation_broken = generation_broken;
            record_attempt_start(&conn, &ctx, &rec);
        }

        let mut outcome = forward(&ctx, &route).await;
        attempts += 1;

        // Extract what the failure path needs, by reference. On SUCCESS we
        // take the body by value (GatewayBody is not Clone) and return; on
        // failure we fall through with the borrowed fields intact.
        let success = match &mut outcome {
            ForwardOutcome::Responded {
                status,
                headers,
                body,
                usage,
                tool_calls,
                tool_names,
                body_error: None,
                ..
            } if status.is_success() => {
                record_attempt_outcome(
                    state,
                    &route.endpoint_id,
                    &request_id,
                    status.as_u16(),
                    None, // Ok — clears quota + resets health
                    usage,
                    *tool_calls,
                    tool_names_json(tool_names),
                    generation_broken,
                )
                .await;
                // Terminal lifecycle: a successful attempt (possibly after a
                // mid-stream migration) is `done`; if that migration broke
                // generation continuity we label the honest `generationbroken`
                // terminal state so the UI doesn't claim a lossless reply.
                let terminal = if generation_broken {
                    TaskLifecycle::GenerationBroken
                } else {
                    TaskLifecycle::Done
                };
                mark_task_terminal(state, &ctx.task_id, terminal).await;
                // Replace the borrowed body with a placeholder so we can move
                // the real one out (the outcome is about to be dropped anyway).
                let taken = std::mem::replace(
                    body,
                    GatewayBody::Full(Full::new(Bytes::new())),
                );
                return Ok(build_agent_response(*status, headers.clone(), taken));
            }
            _ => false,
        };
        // If we got here, the attempt failed. `success` is false; the borrow
        // of `outcome` has ended, so we can proceed with the failure path.
        let _ = success;

        // Failure: classify → decide → act.
        let (class, gen_started, usage_opt) = match &outcome {
            ForwardOutcome::Responded {
                status,
                body,
                usage,
                generation_started,
                body_error,
                ..
            } => {
                // A 2xx/4xx/5xx whose buffered body read failed mid-stream has
                // lost its quota-marker body text — classify by status alone.
                // (A bare 2xx-with-body-error falls through classify's
                // fallback to Temp5xx, i.e. retryable transient.)
                let body_text = if body_error.is_some() {
                    String::new()
                } else {
                    gateway_body_text(body)
                };
                (
                    FailureClass::classify(status.as_u16(), &body_text, false),
                    *generation_started,
                    usage.clone(),
                )
            }
            ForwardOutcome::Unreachable { timeout, .. } => {
                (FailureClass::classify(502, "", *timeout), false, None)
            }
        };

        let decision = migration::decide(
            class,
            attempts,
            gen_started,
            side_effect_risk,
            policy_allows_migrate,
            route.endpoint_id.clone(),
        );

        // Record the failed attempt's outcome (health + quota + row finalize)
        // BEFORE acting — the quota marking is what makes re-resolve skip this
        // endpoint after a QuotaExhausted decision.
        record_attempt_outcome(
            state,
            &route.endpoint_id,
            &request_id,
            outcome.record_status(),
            Some(class),
            &usage_opt,
            None,
            None,
            generation_broken,
        )
        .await;

        match decision {
            MigrationDecision::RetrySame {
                backoff, generation_broken: next_broken, ..
            } => {
                generation_broken = next_broken;
                tokio::time::sleep(backoff).await;
                // Same endpoint, fresh request id (same task id). Reuse the
                // resolved route on the next iteration — do NOT re-resolve
                // (see `retry_route` above).
                retry_route = Some(route.clone());
                ctx = rotate_ctx(&ctx, &agent_id);
            }
            MigrationDecision::Migrate {
                reason,
                from_endpoint_id,
                generation_broken: next_broken,
            } => {
                generation_broken = next_broken;
                failed_endpoint = Some(from_endpoint_id.clone());
                // Re-resolve for the fallback so the migration row records the
                // actual target. The failed endpoint is now quota-exhausted /
                // degraded, so the router picks a different one (or none).
                let to = match resolve(&ctx).await {
                    Ok(r) if r.reason != RouteReason::NoEligible && !r.endpoint_id.is_empty() => {
                        Some(r.endpoint_id.clone())
                    }
                    _ => None,
                };
                record_migration(
                    state,
                    &ctx,
                    &from_endpoint_id,
                    to.as_deref(),
                    reason.as_str(),
                    Some(format!("class={}", class.as_str())),
                )
                .await;
                ctx = rotate_ctx(&ctx, &agent_id);
                attempts = 0;
            }
            MigrationDecision::Surface => {
                // Terminal lifecycle: the task failed (no eligible route, or
                // the upstream error was surfaced to the agent as-is).
                mark_task_terminal(state, &ctx.task_id, TaskLifecycle::Failed).await;
                // Consume the outcome by value (final use): move `body` into the
                // agent-facing response when usable, else surface a gateway error.
                return Ok(match outcome {
                    // An interrupted buffered body can't be handed to the agent
                    // as-is (it is empty) — surface a gateway error instead.
                    ForwardOutcome::Responded {
                        body_error: Some(msg),
                        ..
                    } => error_response(
                        StatusCode::BAD_GATEWAY,
                        &format!("nestra gateway: upstream response interrupted: {msg}"),
                    ),
                    // Surface the upstream error response as-is.
                    ForwardOutcome::Responded {
                        status,
                        headers,
                        body,
                        ..
                    } => build_agent_response(status, headers, body),
                    ForwardOutcome::Unreachable { message, .. } => {
                        error_response(StatusCode::BAD_GATEWAY, &message)
                    }
                });
            }
        }
    }
}

/// Rotate `request_id` for the next attempt while preserving the task's
/// continuity handle (`task_id`) and the request's routing-relevant fields.
/// Carries parent/native identity through: `new_for_request` zeroes them,
/// and dropping them here would silently orphan this attempt from its
/// parent task and sub-agent chain.
fn rotate_ctx(ctx: &TaskContext, agent_id: &str) -> TaskContext {
    let mut next = TaskContext::new_for_request(agent_id, ctx.task_id, ctx.logical_session_id.clone());
    next.parent_task_id = ctx.parent_task_id;
    next.native_task_ref = ctx.native_task_ref.clone();
    next.requested_model = ctx.requested_model.clone();
    next.requested_provider = ctx.requested_provider.clone();
    next.required_capabilities = ctx.required_capabilities.clone();
    next.subagent_role = ctx.subagent_role.clone();
    next.role_source = ctx.role_source;
    next.lifecycle = TaskLifecycle::Routed;
    // The inbound protocol direction must survive retries: without it a
    // same-gateway bridge (anthropic inbound → openai row) silently flips to
    // the anthropic row on the second attempt and the model rejects the
    // Anthropic wire (grok: "not supported for format anthropic").
    next.protocol_hint = ctx.protocol_hint;
    next
}

/// Ensure the `task` row a route_request FK-references exists. Idempotent:
/// if the task already exists, nothing happens. Synchronous (all rusqlite);
/// the caller holds the DB lock.
/// Record the "attempt started" bookkeeping in ONE transaction: the task seed
/// (only when missing) plus the `route_request` insert. Previously 3-4
/// separate auto-commit transactions per proxied request; now one commit.
/// Best-effort (observability data — failures are logged, never fatal), so a
/// transaction failure skips the bookkeeping rather than erroring the request.
fn record_attempt_start(
    conn: &rusqlite::Connection,
    ctx: &TaskContext,
    rec: &RouteRecord,
) {
    if let Ok(tx) = conn.unchecked_transaction() {
        if let Err(e) = ensure_task_chain(&tx, ctx) {
            tracing::warn!("gateway: failed to seed task chain: {e}");
        }
        if let Err(e) = store::insert_route_request(&tx, rec) {
            tracing::warn!("gateway: failed to record route_request: {e}");
        }
        let _ = tx.commit();
    } else {
        tracing::warn!("gateway: failed to open attempt transaction — skipping bookkeeping");
    }
}

fn ensure_task_chain(conn: &rusqlite::Connection, ctx: &TaskContext) -> AppResult<()> {
    let task_id = ctx.task_id.to_string();
    if store::get_task(conn, &task_id)?.is_some() {
        return Ok(());
    }
    store::insert_task(
        conn,
        &store::TaskRow {
            id: task_id,
            parent_task_id: ctx.parent_task_id.map(|p| p.to_string()),
            lifecycle: "born".to_string(),
            native_task_ref: None,
            started_at: chrono::Utc::now().timestamp_millis(),
            ended_at: None,
        },
    )?;
    Ok(())
}

/// Best-effort text of a buffered body (for quota-marker classification).
/// Caps at 4 KiB — only a marker phrase is needed.
fn gateway_body_text(body: &GatewayBody) -> String {
    let GatewayBody::Full(full) = body else {
        return String::new();
    };
    // `Full<Bytes>` exposes its inner data only via `into_inner` (which
    // consumes); `Bytes` is Clone so we clone the full body and take it.
    let Some(bytes) = full.clone().into_inner() else {
        return String::new();
    };
    let len = bytes.len().min(4096);
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

/// Read the migration policy for this task's (agent, role). Returns `true`
/// when migration is allowed (the default; `routing_policy.migrate_on_quota`
/// can disable it). Best-effort: on error, defaults to allowing migration.
pub async fn read_migrate_policy(state: &GatewayState, ctx: &TaskContext) -> bool {
    let conn = state.db.lock().await;
    store::routing_policy_for(&conn, &ctx.agent_id, &ctx.policy_role_key(), ctx.budget_tier.as_ref())
        .map(|p| p.migrate_on_quota)
        .unwrap_or(true)
}

/// Serialize the buffered-path tool-name map for the finalize UPDATE.
fn tool_names_json(names: &Option<std::collections::BTreeMap<String, u64>>) -> Option<String> {
    names.as_ref().and_then(|m| serde_json::to_string(m).ok())
}

/// Record the outcome of one attempt: health + quota update + route_request
/// row finalize (status, usage, tools, generation_broken, ended_at).
#[allow(clippy::too_many_arguments)]
pub async fn record_attempt_outcome(
    state: &GatewayState,
    endpoint_id: &str,
    request_id: &str,
    status: u16,
    class: Option<FailureClass>,
    usage: &Option<ObservedUsage>,
    tool_calls: Option<i64>,
    tool_names: Option<String>,
    generation_broken: bool,
) {
    // Health + quota.
    if !endpoint_id.is_empty() {
        let outcome = match class {
            None => HealthOutcome::Ok,
            Some(c) => HealthOutcome::Fail(c),
        };
        state.health.record(endpoint_id, outcome, status);
        match outcome {
            HealthOutcome::Ok => state.quota.clear_exhausted(endpoint_id),
            HealthOutcome::Fail(FailureClass::QuotaExhausted) => {
                state
                    .quota
                    .mark_exhausted(endpoint_id, Some(format!("HTTP {status}")));
            }
            _ => {}
        }
    }
    // route_request finalize.
    let conn = state.db.lock().await;
    // Persist the quota-state change so it survives a restart — the gateway
    // previously only mutated the in-memory store, leaving the documented
    // `last_quota_state` persistence bridge disconnected (the column stayed
    // stale forever).
    match class {
        None => {
            let _ = crate::orchestration::quota_state::persist(
                &conn,
                endpoint_id,
                &state.quota.get(endpoint_id),
            );
        }
        Some(FailureClass::QuotaExhausted) => {
            let _ = crate::orchestration::quota_state::persist(
                &conn,
                endpoint_id,
                &state.quota.get(endpoint_id),
            );
        }
        _ => {}
    }
    let ended_at = chrono::Utc::now().timestamp_millis();
    let (inp, out, cc, cr) = match usage {
        Some(u) => (u.input, u.output, u.cache_creation, u.cache_read),
        None => (None, None, None, None),
    };
    if let Err(e) = store::update_route_request_outcome(
        &conn,
        request_id,
        Some(status as i64),
        inp,
        out,
        cc,
        cr,
        tool_calls,
        tool_names,
        generation_broken,
        ended_at,
    ) {
        tracing::warn!("gateway: failed to finalize route_request: {e}");
    }
    // Persist the degraded-endpoint circuit when it transitioned (Smart
    // Gateway fix 3). No-op while the set is stable — see
    // `ProviderHealth::persist_degraded`.
    state.health.persist_degraded(&conn);
}

/// Transition the task to its terminal lifecycle state (`done`,
/// `generationbroken`, or `failed`) when the loop exits. Best-effort: the
/// lifecycle is observability, so a failed write only warns.
async fn mark_task_terminal(state: &GatewayState, task_id: &uuid::Uuid, terminal: TaskLifecycle) {
    let conn = state.db.lock().await;
    if let Err(e) = store::set_task_lifecycle(
        &conn,
        &task_id.to_string(),
        terminal,
        chrono::Utc::now().timestamp_millis(),
    ) {
        tracing::warn!("gateway: failed to mark task {task_id} {terminal:?}: {e}");
    }
}

/// Record a migration event (the route_migration row). `from_endpoint_id` is
/// the endpoint that failed; `to_endpoint_id` is the re-resolved fallback
/// (`None` when nothing was eligible).
pub async fn record_migration(
    state: &GatewayState,
    ctx: &TaskContext,
    from_endpoint_id: &str,
    to_endpoint_id: Option<&str>,
    reason: &str,
    detail: Option<String>,
) {
    let mig = store::RouteMigrationRow {
        id: uuid::Uuid::new_v4().to_string(),
        request_id: ctx.request_id.to_string(),
        task_id: ctx.task_id.to_string(),
        from_endpoint_id: Some(from_endpoint_id.to_string()),
        to_endpoint_id: to_endpoint_id.map(str::to_string),
        reason: reason.to_string(),
        detail,
        at_ms: chrono::Utc::now().timestamp_millis(),
    };
    let conn = state.db.lock().await;
    if let Err(e) = store::insert_route_migration(&conn, &mig) {
        tracing::warn!("gateway: failed to record route_migration: {e}");
    }
}

#[cfg(test)]
mod tests;
