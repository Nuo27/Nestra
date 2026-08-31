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
//! - A request whose body declares tools/functions is never blind-retried
//!   once generation bytes were received: `side_effect_risk` forces `Surface`
//!   so a possibly-executed tool call is not double-executed. A pre-response
//!   failure replays per its class — nothing observable happened yet.
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
            // retried.
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
#[tracing::instrument(name = "gw_request", skip_all, fields(task = %ctx.task_id, agent = %agent_id, model = ?ctx.requested_model))]
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
    // Wall-clock cap on the WHOLE loop (tuning `request_deadline_secs`) —
    // the last-resort bound that makes an unbounded retry × migrate ladder
    // impossible even for a pathological policy × upstream combination.
    // tokio's Instant (not std's) so paused-clock tests control it.
    let deadline_secs = super::tuning::snapshot(&state.tuning).request_deadline_secs;
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(deadline_secs);

    loop {
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                deadline_secs,
                task = %ctx.task_id,
                "gateway: request deadline exceeded — abandoning retries/migrations"
            );
            mark_task_terminal(state, &ctx.task_id, TaskLifecycle::Failed).await;
            return Ok(error_response(
                StatusCode::GATEWAY_TIMEOUT,
                &format!(
                    "nestra gateway: request deadline of {deadline_secs}s exceeded (retries/migrations abandoned)"
                ),
            ));
        }
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
            tracing::warn!(
                eligible = false,
                after_failure = failed_endpoint.is_some(),
                task = %ctx.task_id,
                "gw.route: fail-closed (no healthy, quota-ok endpoint)"
            );
            // Terminal lifecycle: a resolve failure with no eligible route is
            // a task failure — leaving the task in 'born' would leak a
            // non-terminal row forever.
            mark_task_terminal(state, &ctx.task_id, TaskLifecycle::Failed).await;
            return Ok(error_response(StatusCode::SERVICE_UNAVAILABLE, msg));
        }
        tracing::info!(
            endpoint = %route.endpoint_id,
            model = %route.model,
            protocol = ?route.protocol,
            reason = %route.reason.as_str(),
            "gw.route"
        );

        // Persist the credential-free projection of this attempt. First
        // ensure the run + task rows exist (route_request has FKs to them;
        // the gateway never creates them elsewhere). Attempt-start stays
        // synchronous (the usage backfill targets this row by request_id),
        // but under a locked DB (launch reconcile) it must not eat the full
        // 5s busy timeout — cap the pre-dial stall at 2s and skip
        // bookkeeping beyond that (the outcome path's lock escape owns the
        // rest; a skipped row only costs one observability record).
        let request_id = ctx.request_id.to_string();
        {
            let conn = state.db.lock().await;
            let _ = conn.busy_timeout(std::time::Duration::from_millis(2000));
            let mut rec = RouteRecord::from_route(&ctx, &route, started_at);
            rec.generation_broken = generation_broken;
            record_attempt_start(&conn, &ctx, &rec);
            let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        }

        // Agent-disconnect honesty: if hyper drops this handler future
        // WHILE the attempt is in flight (the client vanished — its
        // connection error, not an upstream outcome), the attempt row would
        // dangle born/NULL forever. The guard finalizes it as 499 (the
        // client-closed convention) on drop; disarmed the moment `forward`
        // completes, because every completed outcome is recorded normally.
        let mut abort_guard =
            AttemptGuard::new(state.clone(), request_id.clone(), ctx.task_id);
        // Every nested event of this attempt (dial, relay, probe) inherits
        // the gw_request/gw_attempt correlation prefix — see gateway/trace.rs.
        let attempt_started = std::time::Instant::now();
        let mut outcome = tracing::Instrument::instrument(
            forward(&ctx, &route),
            tracing::info_span!(
                "gw_attempt",
                request = %request_id,
                endpoint = %route.endpoint_id,
                model = %route.model,
                reason = %route.reason.as_str(),
                attempt = attempts + 1,
            ),
        )
        .await;
        abort_guard.disarm();
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
                    &route.model,
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
                tracing::info!(
                    status = status.as_u16(),
                    class = "ok",
                    attempt = attempts,
                    duration_ms = attempt_started.elapsed().as_millis() as u64,
                    generation_broken,
                    "gw.attempt outcome"
                );
                tracing::info!(
                    status = status.as_u16(),
                    total_ms = chrono::Utc::now().timestamp_millis() - started_at,
                    generation_broken,
                    "gw.done"
                );
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
        let outcome_status = match &outcome {
            ForwardOutcome::Responded { status, .. } => status.as_u16(),
            ForwardOutcome::Unreachable { .. } => 502,
        };
        tracing::info!(
            status = outcome_status,
            class = class.as_str(),
            duration_ms = attempt_started.elapsed().as_millis() as u64,
            generation_started = gen_started,
            "gw.attempt outcome"
        );

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
            &route.model,
            &request_id,
            outcome.record_status(),
            Some(class),
            &usage_opt,
            None,
            None,
            generation_broken,
        )
        .await;

        // Fast-fail: when a RetrySame decision has nowhere else to go
        // (single-target policy, or every other target already
        // failed/degraded), the retry ladder only adds latency before the
        // same surfacing — collapse it and surface NOW so the agent gets a
        // prompt, honest error (an upstream zero-byte hang otherwise stalls
        // the agent for minutes).
        let decision = if matches!(decision, MigrationDecision::RetrySame { .. }) {
            let has_alternative = {
                let conn = state.db.lock().await;
                let inputs = crate::orchestration::router::RouterInputs {
                    conn: &conn,
                    health: &state.health,
                    quota: &state.quota,
                    affinity: &state.affinity,
                };
                let mut excluding = ctx.failed_endpoints.clone();
                if !excluding.iter().any(|e| e == &route.endpoint_id) {
                    excluding.push(route.endpoint_id.clone());
                }
                crate::orchestration::router::failover_targets(&inputs, &ctx, &excluding)
                    .map(|t| !t.is_empty())
                    .unwrap_or(false)
            };
            if has_alternative {
                decision
            } else {
                MigrationDecision::Surface
            }
        } else {
            decision
        };

        match &decision {
            MigrationDecision::RetrySame { backoff, .. } => tracing::info!(
                decision = %format!("retry-same in {}ms", backoff.as_millis()),
                "gw.decide"
            ),
            MigrationDecision::Migrate { reason, .. } => {
                tracing::info!(decision = %format!("migrate: {reason:?}"), "gw.decide")
            }
            MigrationDecision::Surface => tracing::info!(decision = "surface", "gw.decide"),
        }

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
                // Exclude the failed endpoint on the next resolve so the
                // router walks FORWARD in the policy's route-target list.
                if !ctx.failed_endpoints.iter().any(|e| e == &from_endpoint_id) {
                    ctx.failed_endpoints.push(from_endpoint_id.clone());
                }
                // Re-resolve for the fallback so the migration row records the
                // actual target (an excluded/degraded endpoint is skipped).
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

/// The resolve closure every protocol handler passes to [`run_with_migration`]:
/// lock the router's DB view and resolve the ctx. One shared builder instead
/// of three byte-identical closures.
pub(super) fn make_resolver(
    state: GatewayState,
) -> impl Fn(&TaskContext) -> ResolveFuture {
    move |ctx: &TaskContext| -> ResolveFuture {
        let st = state.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            let conn = st.db.lock().await;
            let inputs = crate::orchestration::router::RouterInputs {
                conn: &conn,
                health: &st.health,
                quota: &st.quota,
                affinity: &st.affinity,
            };
            crate::orchestration::router::resolve(&ctx, &inputs)
        })
    }
}

/// Shared dial/relay preamble for the protocol handlers' `forward_one`s:
/// reads the endpoint credential and parses the upstream URL (both fail
/// closed as `Unreachable` — an unparseable base_url must never fall back
/// to a loopback URL that would receive real credentials), hands the URL +
/// key to the protocol's `build` closure (auth style and header set differ
/// per wire), dials with the tuned headers timeout, logs non-success
/// statuses with the exact URL, then relays + observes via `probe_and_relay`.
/// Handlers keep only their wire-specific body prep and back-conversion.
pub(super) async fn dial_upstream(
    state: &GatewayState,
    route: &crate::orchestration::identity::ResolvedRoute,
    request_id: &str,
    build: impl FnOnce(hyper::Uri, String) -> AppResult<hyper::Request<GatewayBody>>,
) -> Result<
    (
        StatusCode,
        HeaderMap,
        super::protocol_anthropic::RelayOutcome,
    ),
    ForwardOutcome,
> {
    let upstream_key = match (state.credential_reader)(&route.endpoint_id) {
        Ok(Some(k)) => k,
        Ok(None) => {
            return Err(ForwardOutcome::Unreachable {
                timeout: false,
                message: format!("no API key for endpoint '{}'", route.endpoint_id),
            });
        }
        Err(e) => {
            return Err(ForwardOutcome::Unreachable {
                timeout: false,
                message: format!("credential read error: {e}"),
            });
        }
    };
    let upstream_url =
        match crate::protocol_url::parse_upstream_uri(&route.base_url, route.protocol) {
            Ok(u) => u,
            Err(e) => {
                return Err(ForwardOutcome::Unreachable {
                    timeout: false,
                    message: e,
                });
            }
        };
    let upstream_req = match build(upstream_url.clone(), upstream_key) {
        Ok(r) => r,
        Err(e) => {
            return Err(ForwardOutcome::Unreachable {
                timeout: false,
                message: format!("build request: {e}"),
            });
        }
    };
    let client = super::protocol_anthropic::shared_upstream_client().clone();
    let tuning = super::tuning::snapshot(&state.tuning);
    let upstream_resp = match tokio::time::timeout(
        std::time::Duration::from_secs(tuning.headers_timeout_secs),
        client.request(upstream_req),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return Err(ForwardOutcome::Unreachable {
                timeout: super::protocol_anthropic::error_is_timeout(&e),
                message: e.to_string(),
            });
        }
        Err(_) => {
            return Err(ForwardOutcome::Unreachable {
                timeout: true,
                message: format!(
                    "upstream did not send response headers within {}s",
                    tuning.headers_timeout_secs
                ),
            });
        }
    };
    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();
    if !status.is_success() {
        // The 404-style "page not found" failures are almost always a wrong
        // dialed URL (off-direction protocol row, bad base layout) — log the
        // EXACT upstream URL so the failure names itself.
        tracing::warn!(
            endpoint = %route.endpoint_id,
            model = %route.model,
            upstream = %upstream_url,
            status = status.as_u16(),
            "gateway: upstream non-success"
        );
    }
    let relay = super::protocol_anthropic::probe_and_relay(
        state.clone(),
        request_id,
        route.protocol,
        upstream_resp,
        &route.endpoint_id,
        &route.model,
    )
    .await?;
    Ok((status, resp_headers, relay))
}

/// Rotate `request_id` for the next attempt while preserving the task's
/// continuity handle (`task_id`) and the request's routing-relevant fields.
/// Carries parent/native identity through: `new_for_request` zeroes them,
/// and dropping them here would silently orphan this attempt from its
/// parent task and sub-agent chain.
fn rotate_ctx(ctx: &TaskContext, agent_id: &str) -> TaskContext {
    let mut next = TaskContext::new_for_request(agent_id, ctx.task_id, ctx.logical_session_id.clone());
    next.parent_task_id = ctx.parent_task_id;
    next.requested_model = ctx.requested_model.clone();
    next.requested_provider = ctx.requested_provider.clone();
    next.required_capabilities = ctx.required_capabilities.clone();
    next.subagent_role = ctx.subagent_role.clone();
    next.role_source = ctx.role_source;
    // Migration exclusions survive rotation — the next resolve must walk
    // past endpoints that already failed on this request.
    next.failed_endpoints = ctx.failed_endpoints.clone();
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
/// (only when missing) plus the `route_request` insert — one commit per
/// proxied request, not several auto-commits.
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
async fn read_migrate_policy(state: &GatewayState, ctx: &TaskContext) -> bool {
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
async fn record_attempt_outcome(
    state: &GatewayState,
    endpoint_id: &str,
    model: &str,
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
        state.health.record(endpoint_id, model, outcome, status);
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
    // route_request finalize + quota/circuit persistence: observability —
    // lock-escaped (see `observability_write`) so a locked DB never delays
    // the agent response. The quota snapshot is taken HERE (the in-memory
    // state above is authoritative for routing; the persisted copy is the
    // restart bridge).
    let quota_snap = state.quota.get(endpoint_id);
    let health = state.health.clone();
    let endpoint_id = endpoint_id.to_string();
    let request_id = request_id.to_string();
    let ended_at = chrono::Utc::now().timestamp_millis();
    let (inp, out, cc, cr) = match usage {
        Some(u) => (u.input, u.output, u.cache_creation, u.cache_read),
        None => (None, None, None, None),
    };
    let tool_names_owned = tool_names;
    let persist_quota = matches!(
        class,
        None | Some(FailureClass::QuotaExhausted)
    );
    observability_write(state, "finalize route_request", move |conn| {
        // Persist the quota-state change so it survives a restart —
        // `last_quota_state` must stay in sync with the in-memory store.
        if persist_quota {
            crate::orchestration::quota_state::persist(conn, &endpoint_id, &quota_snap)?;
        }
        store::update_route_request_outcome(
            conn,
            &request_id,
            Some(status as i64),
            inp,
            out,
            cc,
            cr,
            tool_calls,
            tool_names_owned.clone(),
            generation_broken,
            ended_at,
        )?;
        // Persist the open-circuit set when it transitioned. No-op while
        // the set is stable — see `ProviderHealth::persist_degraded`.
        health.persist_degraded(conn);
        Ok(())
    })
    .await;
}

/// Transition the task to its terminal lifecycle state (`done`,
/// `generationbroken`, or `failed`) when the loop exits. Best-effort and
/// lock-escaped: the lifecycle is observability, so it must neither stall
/// the response nor get lost to a momentary lock.
async fn mark_task_terminal(state: &GatewayState, task_id: &uuid::Uuid, terminal: TaskLifecycle) {
    let task_id = task_id.to_string();
    observability_write(state, "mark task terminal", move |conn| {
        store::set_task_lifecycle(
            conn,
            &task_id,
            terminal,
            chrono::Utc::now().timestamp_millis(),
        )
    })
    .await;
}

/// Run one best-effort OBSERVABILITY write (outcome finalize, task-terminal
/// lifecycle, migration row) with a bounded inline wait: normally it lands
/// immediately; when the gateway DB is momentarily locked (launch reconcile
/// holds per-provider write transactions for seconds at a time), the write
/// defers to a bounded background retry instead of stalling the
/// agent-facing response path — a 2×5s busy-timeout stall past the
/// client's patience is what produced the zcode reconnect loop while the
/// gateway rows said 200.
///
/// The inline attempt shortens the connection's busy timeout to 150ms so a
/// locked DB defers fast; retries run at the connection's normal timeout.
/// Attempt-START rows deliberately stay synchronous — the usage backfill
/// targets them by request_id, so their ordering must not float.
/// Finalize an attempt whose AGENT vanished mid-flight as `http_status`
/// 499 (client closed request — the nginx convention) instead of leaving a
/// born/NULL row. Guarded: only an STILL-OPEN attempt flips — a disconnect
/// after the response completed (hyper may drop the body without polling
/// its end frame, firing the abort guard post-delivery) keeps the
/// successful outcome and the done task. Observability-grade:
/// lock-escaped, best-effort.
pub(super) async fn finalize_client_aborted(
    state: &GatewayState,
    request_id: &str,
    task_id: &uuid::Uuid,
) {
    let request_id = request_id.to_string();
    let request_id_log = request_id.clone();
    let task_id = *task_id;
    let flipped = observability_write(state, "finalize client-aborted attempt", move |conn| {
        let flipped = store::mark_route_request_aborted_if_open(
            conn,
            &request_id,
            chrono::Utc::now().timestamp_millis(),
        )?;
        if !flipped {
            return Ok(false);
        }
        // The streaming body only knows its request_id — resolve the task
        // from the attempt row when the caller couldn't supply one.
        let task = if task_id.is_nil() {
            conn.query_row(
                "SELECT task_id FROM route_request WHERE request_id = ?1",
                rusqlite::params![request_id],
                |r| r.get::<_, String>(0),
            )
            .ok()
        } else {
            Some(task_id.to_string())
        };
        if let Some(t) = task {
            store::set_task_lifecycle(
                conn,
                &t,
                TaskLifecycle::Failed,
                chrono::Utc::now().timestamp_millis(),
            )?;
        }
        Ok(true)
    })
    .await;
    // Runs outside any span (the handler/body was dropped) — carry the id.
    // `None` = write deferred to the background retry; the guard makes the
    // outcome correct either way, so there is nothing decisive to log yet.
    match flipped {
        Some(true) => tracing::warn!(
            request = %request_id_log,
            "gw.abort: agent disconnected mid-stream — attempt finalized as 499"
        ),
        Some(false) => tracing::debug!(
            request = %request_id_log,
            "gw.abort: agent closed after completion — attempt already terminal"
        ),
        None => {}
    }
}

/// Drop guard around one in-flight attempt: armed before `forward`, disarmed
/// after. Dropped-while-armed means the handler future was cancelled (agent
/// disconnected) — see [`finalize_client_aborted`].
pub(super) struct AttemptGuard {
    state: Option<GatewayState>,
    request_id: String,
    task_id: uuid::Uuid,
}

impl AttemptGuard {
    fn new(state: GatewayState, request_id: String, task_id: uuid::Uuid) -> Self {
        Self {
            state: Some(state),
            request_id,
            task_id,
        }
    }

    fn disarm(&mut self) {
        self.state = None;
    }
}

impl Drop for AttemptGuard {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else { return };
        let request_id = std::mem::take(&mut self.request_id);
        let task_id = self.task_id;
        // Runs outside any span (the handler future was dropped) — carry the
        // correlation ids explicitly. Debug-level: the authoritative
        // WARN/debug pair is logged by `finalize_client_aborted`, which knows
        // whether the attempt actually flipped to 499 (a dropped handler is
        // usually a real abort, but a post-completion disconnect lands here
        // too).
        tracing::debug!(
            request = %request_id,
            task = %task_id,
            "gw.abort: agent disconnected mid-attempt"
        );
        tokio::spawn(async move {
            finalize_client_aborted(&state, &request_id, &task_id).await;
        });
    }
}

async fn observability_write<T: Send + Sync + 'static>(
    state: &GatewayState,
    what: &'static str,
    write: impl Fn(&rusqlite::Connection) -> crate::error::AppResult<T> + Send + Sync + 'static,
) -> Option<T> {
    let is_locked = |e: &crate::error::AppError| e.to_string().to_lowercase().contains("locked");
    {
        let conn = state.db.lock().await;
        let _ = conn.busy_timeout(std::time::Duration::from_millis(150));
        let result = write(&conn);
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        match result {
            Ok(v) => return Some(v),
            Err(e) if is_locked(&e) => {
                tracing::warn!("gateway: {what} deferred — database locked, retrying in background");
            }
            Err(e) => {
                tracing::warn!("gateway: {what} failed: {e}");
                return None;
            }
        }
    }
    let state = state.clone();
    tokio::spawn(async move {
        // ~60s of widening backoff — launch reconcile windows are tens of
        // seconds; beyond this the row is genuinely lost (logged, not hung).
        let backoffs_ms: [u64; 7] = [500, 1_000, 2_000, 4_000, 8_000, 15_000, 30_000];
        for ms in backoffs_ms {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            let conn = state.db.lock().await;
            match write(&conn) {
                Ok(_) => return,
                Err(e) if is_locked(&e) => continue,
                Err(e) => {
                    tracing::warn!("gateway: {what} retry failed: {e}");
                    return;
                }
            }
        }
        tracing::warn!("gateway: {what} gave up after retries");
    });
    // The write went to the background retry — no inline result to report.
    None
}

/// Record a migration event (the route_migration row). `from_endpoint_id` is
/// the endpoint that failed; `to_endpoint_id` is the re-resolved fallback
/// (`None` when nothing was eligible).
async fn record_migration(
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
    observability_write(state, "record route_migration", move |conn| {
        store::insert_route_migration(conn, &mig)
    })
    .await;
}

#[cfg(test)]
mod tests;
