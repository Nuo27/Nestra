//! OpenAI Responses protocol handler — Codex's inbound wire.
//!
//! Inbound: a Codex request to `POST /v1/responses` (the agent's
//! `[model_providers.nestra-*].base_url` points at the gateway). Codex speaks
//! ONLY the Responses API (`wire_api = "responses"`).
//!
//! Structurally identical to the Chat path ([`super::protocol_openai`]):
//! the same retry/migrate loop, and bridging pivots through the chat shape —
//! the request is converted Responses→Chat once (then Chat→Anthropic when the
//! resolved row is anthropic), and the response is converted back
//! Chat→Responses (buffered via [`convert_responses::chat_to_responses_response`],
//! streaming via [`stream_responses::ChatToResponsesStream`], composed after
//! [`stream_convert::AnthropicToChatStream`] for anthropic upstreams). A
//! Responses upstream passes through natively.

use bytes::Bytes;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};

use crate::config_writer::ProviderKind;
use crate::error::AppError;
use crate::orchestration::identity::{
    ResolvedRoute, RoleSource, SubagentRole, TaskContext, TaskLifecycle,
};
use crate::orchestration::migration;
use crate::orchestration::router::{self, RouterInputs};

use super::forward::ForwardOutcome;
use super::protocol_openai::{build_upstream_request, error_response, parse_body_model, rewrite_model};
use super::stream::{read_request_body, GatewayBody};
use super::GatewayState;

/// Handle one Responses request end-to-end. `agent_id` is the Nestra agent id
/// (passed in by the dispatcher, which extracted it from the request's path
/// prefix).
pub async fn handle(
    req: Request<Incoming>,
    state: GatewayState,
    agent_id: &str,
) -> Result<Response<GatewayBody>, AppError> {
    if req.method() != Method::POST || !path_is_responses(req.uri().path()) {
        return Ok(error_response(
            StatusCode::NOT_FOUND,
            "nestra gateway: unsupported Responses path — expected POST /responses (or /v1/responses)",
        ));
    }
    let req_headers = req.headers().clone();
    let body_bytes = read_request_body(req.into_body()).await?;
    handle_bytes(req_headers, body_bytes, state, agent_id).await
}

/// The request body is fully buffered here, so the retry/migrate loop is
/// directly testable without dialing the gateway's HTTP socket.
pub async fn handle_bytes(
    req_headers: hyper::HeaderMap,
    body_bytes: Bytes,
    state: GatewayState,
    agent_id: &str,
) -> Result<Response<GatewayBody>, AppError> {
    let mut ctx = TaskContext::new_task(agent_id, None);
    ctx.protocol_hint = Some(ProviderKind::Responses);
    ctx.requested_model = parse_body_model(&body_bytes);
    ctx.budget_tier = ctx
        .requested_model
        .as_deref()
        .and_then(crate::orchestration::identity::BudgetTier::from_model_id);
    ctx.lifecycle = TaskLifecycle::Routed;
    let started_at = chrono::Utc::now().timestamp_millis();

    // Subagent identity: Responses carries the system prompt as
    // `instructions` — same heuristic as the chat path's system message.
    if let Some(instructions) = serde_json::from_slice::<serde_json::Value>(&body_bytes)
        .ok()
        .and_then(|v| v.get("instructions").and_then(|i| i.as_str()).map(str::to_string))
    {
        let role = SubagentRole::from_system_prompt(&serde_json::Value::String(instructions));
        if role != SubagentRole::Main {
            let key = role.as_policy_key();
            ctx.subagent_role = role;
            ctx.role_source = RoleSource::Heuristic;
            tracing::info!("gateway: responses subagent detected: {key}");
        }
    }

    ctx.required_capabilities =
        crate::orchestration::capability_registry::derive_capability_req(
            &body_bytes,
            ProviderKind::Responses,
        );
    let side_effect_risk = migration::body_has_side_effect_risk(&body_bytes);

    let agent = agent_id.to_string();
    let loop_state = state.clone();
    let forward_state = state.clone();
    let fwd_headers = req_headers.clone();
    let fwd_body = body_bytes.clone();

    super::forward::run_with_migration(
        &state,
        ctx,
        agent,
        started_at,
        side_effect_risk,
        move |ctx: &TaskContext| -> super::forward::ResolveFuture {
            let st = loop_state.clone();
            let ctx = ctx.clone();
            Box::pin(async move {
                let conn = st.db.lock().await;
                let inputs = RouterInputs {
                    conn: &conn,
                    health: &st.health,
                    quota: &st.quota,
                    affinity: &st.affinity,
                };
                router::resolve(&ctx, &inputs)
            })
        },
        move |ctx: &TaskContext, route: &ResolvedRoute| -> super::forward::ForwardFuture {
            let route = route.clone();
            let st = forward_state.clone();
            let headers = fwd_headers.clone();
            let body = fwd_body.clone();
            let request_id = ctx.request_id.to_string();
            Box::pin(async move { forward_one(&st, route, headers, body, &request_id).await })
        },
        error_response,
    )
    .await
}

/// Dial + relay ONE attempt against the resolved route (Responses wire shape
/// inbound; the upstream wire decides the bridge).
async fn forward_one(
    state: &GatewayState,
    route: ResolvedRoute,
    headers: hyper::HeaderMap,
    body: Bytes,
    request_id: &str,
) -> ForwardOutcome {
    let rewritten = rewrite_model(&body, &route.model);
    let bridging = route.protocol != ProviderKind::Responses;
    let upstream_body = match route.protocol {
        ProviderKind::Responses => rewritten,
        // Pivot through chat: Responses→Chat, and on to Anthropic when the
        // row speaks Messages.
        ProviderKind::Anthropic => super::convert::chat_to_anthropic(&super::convert_responses::responses_to_chat_request(&rewritten)),
        _ => super::convert_responses::responses_to_chat_request(&rewritten),
    };
    let upstream_kind = route.protocol;
    let upstream_key = match (state.credential_reader)(&route.endpoint_id) {
        Ok(Some(k)) => k,
        Ok(None) => {
            return ForwardOutcome::Unreachable {
                timeout: false,
                message: format!("no API key for endpoint '{}'", route.endpoint_id),
            }
        }
        Err(e) => {
            return ForwardOutcome::Unreachable {
                timeout: false,
                message: format!("credential read error: {e}"),
            }
        }
    };
    let upstream_url = match crate::protocol_url::parse_upstream_uri(&route.base_url, upstream_kind)
    {
        Ok(u) => u,
        Err(e) => {
            return ForwardOutcome::Unreachable {
                timeout: false,
                message: e,
            }
        }
    };
    let upstream_req = match build_upstream_request(
        &upstream_url,
        &headers,
        upstream_key,
        upstream_body,
        upstream_kind,
    ) {
        Ok(r) => r,
        Err(e) => {
            return ForwardOutcome::Unreachable {
                timeout: false,
                message: format!("build request: {e}"),
            }
        }
    };

    let client = super::protocol_anthropic::upstream_client();
    let upstream_resp = match client.request(upstream_req).await {
        Ok(r) => r,
        Err(e) => {
            return ForwardOutcome::Unreachable {
                timeout: super::protocol_anthropic::error_is_timeout(&e),
                message: e.to_string(),
            }
        }
    };

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();
    if !status.is_success() {
        tracing::warn!(
            endpoint = %route.endpoint_id,
            model = %route.model,
            upstream = %upstream_url,
            status = status.as_u16(),
            "gateway: upstream non-success"
        );
    }
    let relay = super::protocol_anthropic::relay_response(
        state.clone(),
        request_id.to_string(),
        upstream_kind,
        upstream_resp,
        status,
    )
    .await;
    // Convert a bridged upstream wire back to the Responses shape the agent
    // speaks; a native responses upstream passes through verbatim.
    let body = if bridging {
        super::convert::convert_relay_body(
            relay.body,
            status.is_success(),
            ProviderKind::Responses,
            route.protocol,
        )
        .await
    } else {
        relay.body
    };
    ForwardOutcome::Responded {
        status,
        headers: resp_headers,
        body,
        usage: relay.usage,
        generation_started: relay.generation_started,
        body_error: relay.body_error,
        tool_calls: relay.tool_calls,
        tool_names: relay.tool_names,
    }
}

/// `true` for `/v1/responses` OR `/<agent>/v1/responses`, and the same forms
/// WITHOUT the `/v1` segment (mirrors [`super::protocol_openai`]'s chat path
/// matcher). Trailing slash / query tolerated.
pub(super) fn path_is_responses(p: &str) -> bool {
    let path = p.split('?').next().unwrap_or(p);
    let trimmed = path.trim_end_matches('/');
    if matches!(trimmed, "/v1/responses" | "/responses") {
        return true;
    }
    if let Some(rest) = trimmed.strip_prefix('/') {
        if let Some((_agent, r)) = rest.split_once('/') {
            let candidate = format!("/{r}");
            return matches!(candidate.as_str(), "/v1/responses" | "/responses");
        }
    }
    false
}

#[cfg(test)]
mod tests;
