//! OpenAI Responses API inbound handler (`/v1/responses`).
//!
//! No Nestra agent speaks Responses to the gateway today (Claude Code sends
//! Messages, OpenCode Desktop/Pi send Chat Completions) — this handler is
//! for future Responses-speaking clients and completes the three-wire
//! matrix. The inbound responses request is converted to the resolved
//! route's wire (Responses passthrough, Chat Completions, or Anthropic
//! Messages) and the upstream response is converted back.

use bytes::Bytes;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};

use crate::config_writer::ProviderKind;
use crate::error::AppError;
use crate::orchestration::identity::{ResolvedRoute, RoleSource, SubagentRole, TaskContext, TaskLifecycle};
use crate::orchestration::router::{self, RouterInputs};
use crate::orchestration::gateway::forward::{self, ForwardFuture, ForwardOutcome};
use crate::orchestration::gateway::stream::GatewayBody;

use super::protocol_anthropic::{error_is_timeout, upstream_client};
use super::protocol_openai::{parse_body_model, path_is_models};
use super::GatewayState;

/// Handle one Responses API request end-to-end.
pub async fn handle(
    req: Request<Incoming>,
    state: GatewayState,
    agent_id: &str,
) -> Result<Response<GatewayBody>, AppError> {
    if req.method() == Method::GET && path_is_models(req.uri().path()) {
        return Ok(super::protocol_openai::models_response());
    }
    if req.method() != Method::POST || !path_is_responses(req.uri().path()) {
        return Ok(error_response(
            StatusCode::NOT_FOUND,
            "nestra gateway: unsupported Responses path — expected POST /v1/responses",
        ));
    }
    let req_headers = req.headers().clone();
    let body_bytes = super::stream::read_request_body(req.into_body()).await?;
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
    let requested_model = parse_body_model(&body_bytes);
    let mut ctx = TaskContext::new_task(agent_id, None);
    ctx.protocol_hint = Some(ProviderKind::Responses);
    ctx.requested_model = requested_model;
    ctx.lifecycle = TaskLifecycle::Routed;
    let started_at = chrono::Utc::now().timestamp_millis();

    // Subagent identity: Responses requests carry the system prompt in
    // `instructions`. Reuse the same heuristic so per-role policies hit.
    if let Ok(body_json) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
        if let Some(system) = body_json.get("instructions") {
            let role = SubagentRole::from_system_prompt(system);
            if role != SubagentRole::Main {
                ctx.subagent_role = role;
                ctx.role_source = RoleSource::Heuristic;
            }
        }
    }

    // A request declaring tools may have executed a tool upstream — never
    // blind-retry it (the loop surfaces instead).
    let side_effect_risk = crate::orchestration::migration::body_has_side_effect_risk(&body_bytes);

    let agent = agent_id.to_string();
    let loop_state = state.clone();
    let forward_state = state.clone();
    let fwd_headers = req_headers.clone();
    let fwd_body = body_bytes.clone();

    forward::run_with_migration(
        &state,
        ctx,
        agent,
        started_at,
        side_effect_risk,
        move |ctx: &TaskContext| -> forward::ResolveFuture {
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
        move |route: &ResolvedRoute| -> ForwardFuture {
            let route = route.clone();
            let st = forward_state.clone();
            let headers = fwd_headers.clone();
            let body = fwd_body.clone();
            Box::pin(async move { forward_one(&st, route, headers, body).await })
        },
        error_response,
    )
    .await
}

/// Dial + relay ONE attempt against the resolved route. The responses
/// request is converted to the route's wire (chat or anthropic); a
/// responses-class route passes through untouched.
async fn forward_one(
    state: &GatewayState,
    route: ResolvedRoute,
    headers: hyper::HeaderMap,
    body: Bytes,
) -> ForwardOutcome {
    let rewritten = super::protocol_openai::rewrite_model(&body, &route.model);
    // (inbound=Responses, upstream=route.protocol) conversion pair.
    let (upstream_body, bridging) = match route.protocol {
        ProviderKind::Openai => (
            super::convert_responses::responses_req_to_chat(&rewritten),
            true,
        ),
        ProviderKind::Anthropic => (
            super::convert_responses::responses_req_to_anthropic(&rewritten),
            true,
        ),
        _ => (rewritten, false),
    };
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
    // An unparseable base_url is a config error, not a retry opportunity —
    // the request carries real credentials, so fail closed instead of falling
    // back to a loopback URL that would receive them.
    let upstream_url = match crate::protocol_url::parse_upstream_uri(&route.base_url, route.protocol)
    {
        Ok(u) => u,
        Err(e) => {
            return ForwardOutcome::Unreachable {
                timeout: false,
                message: e,
            }
        }
    };
    // Anthropic upstreams authenticate with x-api-key; everything else Bearer.
    // The auth scheme follows the UPSTREAM protocol, not `bridging` (which
    // describes body conversion — a converted-to-Anthropic body still needs
    // x-api-key, and an unconverted body to an OpenAI route needs Bearer).
    let upstream_req = match super::protocol_anthropic::build_upstream_request(
        &upstream_url,
        &headers,
        upstream_key,
        upstream_body,
        route.protocol != ProviderKind::Anthropic,
    ) {
        Ok(r) => r,
        Err(e) => {
            return ForwardOutcome::Unreachable {
                timeout: false,
                message: format!("build request: {e}"),
            }
        }
    };

    let client = upstream_client();
    let upstream_resp = match client.request(upstream_req).await {
        Ok(r) => r,
        Err(e) => {
            return ForwardOutcome::Unreachable {
                timeout: error_is_timeout(&e),
                message: e.to_string(),
            };
        }
    };

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();
    let relay = super::protocol_anthropic::relay_response(upstream_resp, status).await;
    // Convert the upstream body back to the Responses wire.
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
    }
}

/// `POST /v1/responses` (optionally agent-prefixed, e.g. `/<agent>/v1/responses`).
fn path_is_responses(p: &str) -> bool {
    p == "/v1/responses" || p.ends_with("/v1/responses")
}

/// OpenAI-style error envelope (matches `protocol_openai`).
fn error_response(status: StatusCode, message: &str) -> Response<GatewayBody> {
    let body = serde_json::json!({
        "error": { "message": message, "type": "nestra_gateway_error" }
    });
    let bytes = Bytes::from(body.to_string());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(GatewayBody::Full(http_body_util::Full::new(bytes)))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(GatewayBody::Full(http_body_util::Full::new(Bytes::new())))
                .unwrap()
        })
}
