//! OpenAI Chat Completions protocol handler.
//!
//! Inbound: an OpenCode or Pi request to `POST /v1/chat/completions` (the
//! agent was configured with the gateway's URL as its `base_url`). The body is
//! the standard OpenAI Chat Completions JSON:
//!
//! ```json
//! { "model": "gpt-4o", "messages": [...], "stream": true, ... }
//! ```
//!
//! Structurally identical to the Anthropic path
//! ([`super::protocol_anthropic`]), differing only in the wire details:
//!   - path: `/v1/chat/completions` (not `/v1/messages`)
//!   - auth: `Authorization: Bearer <key>` (not `x-api-key`)
//!   - usage: `{ "usage": { "prompt_tokens", "completion_tokens", ... } }`
//!     for non-streaming; for SSE, the final `data: {...}` chunk carries usage
//!     when `stream_options.include_usage` was set (we observe what's there).
//!
//! The retry/migrate loop is shared: this module supplies the resolve +
//! forward closures and the OpenAI error envelope to
//! [`super::forward::run_with_migration`]; the loop records outcomes,
//! migrations, and generation-broken flags identically for both protocols.

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::HeaderValue;
use hyper::{Method, Request, Response, StatusCode};

use crate::error::{AppError, AppResult};
use crate::config_writer::ProviderKind;
use crate::orchestration::identity::{ResolvedRoute, RoleSource, SubagentRole, TaskContext, TaskLifecycle};
use crate::orchestration::migration;
use crate::orchestration::router::{self, RouterInputs};

use super::forward::{ForwardFuture, ForwardOutcome};
use super::stream::{read_request_body, GatewayBody, ObservedUsage};
use super::GatewayState;

/// Handle one OpenAI Chat Completions request end-to-end. `agent_id` is the
/// Nestra agent id (passed in by the dispatcher, which extracted it from the
/// request's path prefix).
pub async fn handle(
    req: Request<Incoming>,
    state: GatewayState,
    agent_id: &str,
) -> Result<Response<GatewayBody>, AppError> {
    // OpenCode/AI-SDK probes the model catalog via `GET {base}/models` before
    // the first chat. Answer with the alias entry carrying the REAL
    // steady-state abilities for this agent — a placeholder 200k limit lies
    // about the actual window the router will serve.
    if req.method() == Method::GET && path_is_models(req.uri().path()) {
        return Ok(models_response(&state, agent_id).await);
    }
    if req.method() != Method::POST || !path_is_chat_completions(req.uri().path()) {
        return Ok(error_response(
            StatusCode::NOT_FOUND,
            "nestra gateway: unsupported OpenAI path — expected POST /chat/completions (or /v1/chat/completions)",
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
    // Build the TaskContext. OpenCode/Pi don't send a stable session header we
    // can rely on, so logical_session_id is None here and refined later
    // when an adapter proves a header exists. Requested model from the body.
    let requested_model = parse_body_model(&body_bytes);
    let mut ctx = TaskContext::new_task(agent_id, None);
    ctx.protocol_hint = Some(ProviderKind::Openai);
    ctx.requested_model = requested_model;
    // Tier intent from the model id (no-op for the generic "nestra" alias,
    // but a real-tier id classifies the same as on the Anthropic path).
    ctx.budget_tier = ctx
        .requested_model
        .as_deref()
        .and_then(crate::orchestration::identity::BudgetTier::from_model_id);
    ctx.lifecycle = TaskLifecycle::Routed;
    let started_at = chrono::Utc::now().timestamp_millis();

    // Subagent identity: OpenAI requests carry the system prompt inside the
    // `messages` array (role: "system"). Conservatively detect an OpenCode
    // agent (or any "you are X, operating/working/acting as" pattern) so
    // per-role routing policies (`opencode:<name>`) actually hit; the main
    // thread stays Main.
    if let Ok(body_json) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
        let system = body_json
            .get("messages")
            .and_then(|m| m.as_array())
            .and_then(|arr| {
                arr.iter().find(|m| {
                    m.get("role").and_then(|r| r.as_str()) == Some("system")
                })
            })
            .and_then(|m| m.get("content"));
        if let Some(system) = system {
            let sys_text = crate::orchestration::identity::system_text(system);
            // Byte-slicing a user-controlled UTF-8 string at a fixed length
            // can land mid-multibyte-char — floor_char_boundary keeps the
            // window on a char edge (remotely triggerable panic otherwise).
            let cap = sys_text.len().min(300);
            let cap = sys_text.floor_char_boundary(cap);
            // Debug-level and length-only: the prompt text can carry
            // project-specific instructions/paths/credentials — never log it
            // at info (the Anthropic path doesn't either).
            tracing::debug!(
                "gateway: openai system prompt ({} chars): {:?}",
                sys_text.len(),
                &sys_text[..cap]
            );
            let role = SubagentRole::from_system_prompt(system);
            if role != SubagentRole::Main {
                let key = role.as_policy_key();
                ctx.subagent_role = role;
                ctx.role_source = RoleSource::Heuristic;
                tracing::info!("gateway: openai subagent detected: {key}");
            }
        }
    }

    // Capability requirements derived from the body activate the router's
    // capability stage (tool/vision; Smart Gateway fix 2). Conservative:
    // absent signals stay false → no filtering.
    ctx.required_capabilities =
        crate::orchestration::capability_registry::derive_capability_req(
            &body_bytes,
            ProviderKind::Openai,
        );

    // State 3: a request declaring tools/functions may have executed a tool
    // upstream — never blind-retry it (the loop surfaces instead).
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
        move |ctx: &TaskContext, route: &ResolvedRoute| -> ForwardFuture {
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

/// Dial + relay ONE attempt against the resolved route (OpenAI wire shape).
async fn forward_one(
    state: &GatewayState,
    route: ResolvedRoute,
    headers: hyper::HeaderMap,
    body: Bytes,
    request_id: &str,
) -> ForwardOutcome {
    let rewritten = rewrite_model(&body, &route.model);
    // Bridge the inbound chat wire to whatever wire the resolved row speaks:
    // Responses-class models (grok-4.5, gpt-5.6-luna) get the Responses API,
    // an endpoint whose only row is Anthropic (e.g. MiniMax-M3 on
    // `…/anthropic`) gets Messages — otherwise native Chat Completions.
    let bridging = route.protocol != ProviderKind::Openai;
    let upstream_body = match route.protocol {
        ProviderKind::Responses => super::convert_responses::chat_to_responses(&rewritten),
        ProviderKind::Anthropic => super::convert::chat_to_anthropic(&rewritten),
        _ => rewritten,
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
    // An unparseable base_url is a config error, not a retry opportunity —
    // the request carries real credentials, so fail closed instead of falling
    // back to a loopback URL that would receive them.
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
    let tuning = super::tuning::snapshot(&state.tuning);
    let upstream_resp = match tokio::time::timeout(
        std::time::Duration::from_secs(tuning.headers_timeout_secs),
        client.request(upstream_req),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return ForwardOutcome::Unreachable {
                timeout: super::protocol_anthropic::error_is_timeout(&e),
                message: e.to_string(),
            };
        }
        Err(_) => {
            return ForwardOutcome::Unreachable {
                timeout: true,
                message: format!(
                    "upstream did not send response headers within {}s",
                    tuning.headers_timeout_secs
                ),
            };
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
    // Relay + observe. Reuse the Anthropic relay for the mechanics (it streams
    // verbatim + observes usage from SSE or buffered JSON); the OpenAI usage
    // fields differ (prompt_tokens/completion_tokens) so we post-process the
    // observed usage to map them onto our standard input/output fields. The
    // relay's own SSE accumulator keys off `upstream_kind` (the raw upstream
    // bytes are chat-wire unless bridged to Responses).
    let relay = match super::protocol_anthropic::probe_and_relay(
        state.clone(),
        &request_id.to_string(),
        upstream_kind,
        upstream_resp,
        &route.endpoint_id,
        &route.model,
    )
    .await
    {
        Ok(relay) => relay,
        Err(in_band_failure) => return in_band_failure,
    };
    let usage = relay.usage.map(|u| map_openai_usage(u, &resp_headers));
    // Convert a bridged upstream wire back to chat chunks (Responses or
    // Anthropic); native chat passes through verbatim.
    let body = if bridging {
        super::convert::convert_relay_body(
            relay.body,
            status.is_success(),
            ProviderKind::Openai,
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
        usage,
        generation_started: relay.generation_started,
        body_error: relay.body_error,
        tool_calls: relay.tool_calls,
        tool_names: relay.tool_names,
    }
}

/// OpenAI usage uses `prompt_tokens` / `completion_tokens` (not
/// `input_tokens` / `output_tokens`). The shared usage observer in
/// `stream::merge_usage_obj` now recognizes BOTH vocabularies (Anthropic +
/// OpenAI field names), so the non-streaming buffered path captures OpenAI
/// usage automatically and this mapping is an identity passthrough.
///
/// Streaming usage is captured by the relay's SSE accumulator (backfilled
/// after the stream ends); it is present only when the stream carried a
/// `usage` chunk — i.e. `stream_options.include_usage` was set by the agent
/// (the gateway's bridges inject it; a native OpenAI agent's body may not).
fn map_openai_usage(observed: ObservedUsage, _headers: &hyper::HeaderMap) -> ObservedUsage {
    observed
}

/// `true` for `/v1/chat/completions` OR `/<agent>/v1/chat/completions` (the
/// dispatcher routes by agent prefix), and the same forms WITHOUT the `/v1`
/// segment. The OpenAI-compatible SDK appends `/chat/completions` to the
/// configured base URL; the base may or may not carry `/v1` (current writers
/// emit it, older configs lack it). Trailing slash / query tolerated.
fn path_is_chat_completions(p: &str) -> bool {
    let path = p.split('?').next().unwrap_or(p);
    let trimmed = path.trim_end_matches('/');
    if matches!(trimmed, "/v1/chat/completions" | "/chat/completions") {
        return true;
    }
    if let Some(rest) = trimmed.strip_prefix('/') {
        if let Some((_agent, r)) = rest.split_once('/') {
            // `/<agent>/…` — normalize the remainder (NO leading slash).
            let candidate = format!("/{r}");
            return matches!(candidate.as_str(), "/v1/chat/completions" | "/chat/completions");
        }
    }
    false
}

/// `true` for `/v1/models` / `/<agent>/v1/models` and their no-`/v1` forms —
/// the OpenAI-compatible SDK lists models via `GET {base}/models` at startup.
pub(super) fn path_is_models(p: &str) -> bool {
    let path = p.split('?').next().unwrap_or(p);
    let trimmed = path.trim_end_matches('/');
    if trimmed == "/v1/models" || trimmed == "/models" {
        return true;
    }
    if let Some(rest) = trimmed.strip_prefix('/') {
        if let Some((_agent, r)) = rest.split_once('/') {
            let candidate = format!("/{r}");
            return candidate == "/v1/models" || candidate == "/models";
        }
    }
    false
}

/// `GET /models` answer: the gateway's single alias entry (`nestra` — the id
/// every non-Claude agent's config sends; Claude Code never reaches the
/// OpenAI path). The `limit`/flag fields carry the abilities of the model the
/// router resolves in the steady state for this agent, so OpenAI-compatible
/// clients (AI SDK) render the real context window. Neutral placeholders when
/// nothing resolves (no endpoints / cold catalog).
fn models_payload(abilities: Option<&crate::model_abilities::ModelAbilities>) -> serde_json::Value {
    let (context, output) = abilities
        .and_then(|a| a.limit.as_ref())
        .map(|l| (l.context, l.output))
        .unwrap_or((200_000, 8_192));
    let reasoning = abilities.and_then(|a| a.reasoning).unwrap_or(true);
    let tool_call = abilities.and_then(|a| a.tool_call).unwrap_or(true);
    serde_json::json!({
        "object": "list",
        "data": [{
            "id": "nestra",
            "object": "model",
            "created": 0,
            "owned_by": "nestra-gw",
            "limit": { "context": context, "output": output },
            "tool_call": tool_call,
            "reasoning": reasoning,
        }],
    })
}

/// Resolve the agent's steady-state abilities on the gateway's own connection
/// (live health/quota; a scratch affinity so the probe never pollutes the
/// process-global affinity map) and answer `GET /models` with them.
async fn models_response(state: &GatewayState, agent_id: &str) -> Response<GatewayBody> {
    let abilities = {
        let conn = state.db.lock().await;
        // Fresh catalog so the advertised limits match current endpoint
        // config (same rebuild the alias write performs). Best-effort.
        let _ = crate::orchestration::capability_registry::rebuild(&conn);
        let scratch_affinity = router::RouteAffinity::new();
        let inputs = RouterInputs {
            conn: &conn,
            health: &state.health,
            quota: &state.quota,
            affinity: &scratch_affinity,
        };
        router::steady_state(&inputs, agent_id, None).and_then(|s| s.abilities)
    };
    let body = models_payload(abilities.as_ref());
    let mut resp = Response::new(GatewayBody::json_full(body));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

pub(super) fn parse_body_model(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("model")?.as_str().map(str::to_string)
}

pub(super) fn rewrite_model(body: &[u8], resolved_model: &str) -> Bytes {
    let mut v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Bytes::copy_from_slice(body),
    };
    let obj = match v.as_object_mut() {
        Some(o) => o,
        None => return Bytes::copy_from_slice(body),
    };
    if obj.contains_key("model") {
        obj.insert(
            "model".to_string(),
            serde_json::Value::String(resolved_model.to_string()),
        );
        Bytes::from(serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec()))
    } else {
        Bytes::copy_from_slice(body)
    }
}



/// App identity sent to aggregators that use it for attribution / ranking
/// (OpenRouter's `HTTP-Referer` + `X-Title`). Only injected in Routed mode —
/// Direct mode has the agent place the call itself, so Nestra can't add them.
const APP_TITLE: &str = "Nestra";
const APP_REFERER: &str = "https://github.com/Nuo/Nestra";

/// Build the upstream request. Chat/Responses wires use
/// `Authorization: Bearer <key>`; the Anthropic wire uses `x-api-key`
/// (Anthropic's native auth — some anthropic-compatible gateways 401 on
/// Bearer-only). We strip any inbound `authorization` and set it fresh from
/// the resolved credential.
pub(super) fn build_upstream_request(
    url: &hyper::Uri,
    original_headers: &hyper::HeaderMap,
    api_key: String,
    body: Bytes,
    upstream_kind: ProviderKind,
) -> AppResult<Request<GatewayBody>> {
    let mut builder = Request::builder().method(Method::POST).uri(url);
    const SKIP: &[&str] = &[
        "host",
        "content-length",
        "connection",
        "keep-alive",
        "proxy-connection",
        "transfer-encoding",
        "upgrade",
        "te",
        "trailers",
        "authorization",
    ];
    for (name, value) in original_headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if SKIP.contains(&lower.as_str()) {
            continue;
        }
        builder = builder.header(name.clone(), value);
    }
    if upstream_kind == ProviderKind::Anthropic {
        builder = builder.header(
            "x-api-key",
            HeaderValue::from_str(&api_key).map_err(|e| {
                AppError::Validation(format!("api key contains invalid header chars: {e}"))
            })?,
        );
    }
    let bearer = format!("Bearer {api_key}");
    builder = builder.header(
        hyper::header::AUTHORIZATION,
        HeaderValue::from_str(&bearer).map_err(|e| {
            AppError::Validation(format!("api key contains invalid header chars: {e}"))
        })?,
    );
    // OpenRouter attribution headers — the one genuine wire-level difference
    // between OpenRouter and a plain OpenAI-compatible upstream in Routed mode.
    if url.host().map(|h| h.contains("openrouter.ai")).unwrap_or(false) {
        builder = builder.header("HTTP-Referer", APP_REFERER);
        builder = builder.header("X-Title", APP_TITLE);
    }
    let req = builder
        .body(GatewayBody::Full(Full::new(body)))
        .map_err(|e| AppError::Internal(format!("build upstream request: {e}")))?;
    Ok(req)
}

pub(super) fn error_response(status: StatusCode, message: &str) -> Response<GatewayBody> {
    let body = serde_json::json!({
        "error": { "message": message, "type": "nestra_gateway_error" }
    });
    let mut resp = Response::new(GatewayBody::json_full(body));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

#[cfg(test)]
mod tests;
