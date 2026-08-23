//! Anthropic Messages protocol handler.
//!
//! Inbound: a Claude Code request to `POST /v1/messages` (the agent was
//! configured with the gateway's `http://127.0.0.1:<port>` as its
//! `ANTHROPIC_BASE_URL`). The body is the standard Anthropic Messages JSON:
//!
//! ```json
//! { "model": "claude-sonnet-4-5", "messages": [...], "stream": true, ... }
//! ```
//!
//! What the handler does:
//!   1. Build a [`TaskContext`] from the request (agent = claude-code-cli, session
//!      id from the `session_id` header Claude Code sends, requested model from
//!      the body, role = Main with Heuristic source — adapter refines
//!      subagent-role extraction).
//!   2. Detect side-effect risk (non-empty `tools` array in the body).
//!   3. Hand the request to [`super::forward::run_with_migration`] — the
//!      retry/migrate loop — with this protocol's resolve + forward closures.
//!   4. Each attempt: [`router::resolve`] → rewrite `model` → resolve the
//!      credential → dial `<resolved base_url>/v1/messages` with `x-api-key` +
//!      `anthropic-version` → stream the response back verbatim.
//!   5. On failure, the loop classifies, retries (bounded, same provider) or
//!      migrates (re-resolve picks a fallback), recording honest
//!      `generation_broken` flags. Auth/BadRequest and side-effect-risk
//!      failures are surfaced to the agent as-is.
//!
//! Prompt-cache `cache_control` injection is applied when policy opts in.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use std::pin::Pin;

use crate::error::{AppError, AppResult};
use crate::config_writer::ProviderKind;
use crate::orchestration::identity::{
    ResolvedRoute, RoleSource, SubagentRole, TaskContext, TaskLifecycle,
};
use crate::orchestration::migration;
use crate::orchestration::router::{self, RouterInputs};

use super::forward::{ForwardFuture, ForwardOutcome};
use super::stream::{
    GatewayBody, ObservedUsage, StreamObservation, observe_anthropic_chunk,
    observe_openai_chat_chunk, observe_responses_chunk, read_request_body,
};
use super::GatewayState;

/// The hyper client used to dial upstreams. Cheap to clone (it pools
/// connections).
///
/// NOTE: this is rebuilt on every call — the doc below previously claimed
/// "built once per gateway process", but there is no caching (no `OnceLock`/
/// static) and each `forward_one` constructs a fresh `HttpsConnector` +
/// `Client`, re-reading system TLS roots per request. That is a known
/// performance gap (no connection reuse across requests), tracked
/// separately; this doc describes reality, not the intended design.
/// Public to the sibling OpenAI handler so both protocols share one client.
pub(super) fn upstream_client() -> Client<HttpsConnector<HttpConnector>, GatewayBody> {
    // HTTPS-aware connector: real upstreams (z-ai, MiniMax, …) are HTTPS and
    // a bare `HttpConnector` fails every such dial ("unsupported scheme").
    // System root store (Windows CA) via rustls-native-certs; HTTP stays
    // supported so mock/test upstreams keep working. Keep-alive on by default.
    let https = HttpsConnectorBuilder::new()
        .with_native_roots()
        .expect("system TLS roots must load")
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
    Client::builder(hyper_util::rt::TokioExecutor::new()).build(https)
}

/// Handle one Anthropic Messages request end-to-end. `agent_id` is supplied
/// by the dispatcher (extracted from the path prefix, or defaulted to
/// "claude-code-cli" for prefix-less requests).
pub async fn handle(
    req: Request<Incoming>,
    state: GatewayState,
    agent_id: &str,
) -> Result<Response<GatewayBody>, AppError> {
    // Only POST /v1/messages is supported.
    if req.method() != Method::POST || !path_is_messages(req.uri().path()) {
        return Ok(error_response(
            StatusCode::NOT_FOUND,
            "nestra gateway: only POST /v1/messages is supported",
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
    // Build the TaskContext. Parse the body ONCE and reuse the Value for
    // model extraction + subagent detection below (the old code parsed the
    // whole body twice on the hot path).
    let body_json: Option<serde_json::Value> = serde_json::from_slice(&body_bytes).ok();
    let requested_model = body_json
        .as_ref()
        .and_then(|v| v.get("model"))
        .and_then(|m| m.as_str())
        .map(str::to_string);
    let session_id = req_headers
        .get("session_id")
        .or_else(|| req_headers.get("x-session-id"))
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut ctx = TaskContext::new_task(agent_id, session_id);
    ctx.protocol_hint = Some(ProviderKind::Anthropic);
    ctx.requested_model = requested_model;
    // Tier intent from the model slot the agent used (Claude Code's per-tier
    // env vars each carry a distinct tier id) — feeds `tier:*` policy rows.
    ctx.budget_tier = ctx
        .requested_model
        .as_deref()
        .and_then(crate::orchestration::identity::BudgetTier::from_model_id);
    ctx.lifecycle = TaskLifecycle::Routed;
    let started_at = chrono::Utc::now().timestamp_millis();

    // Subagent identity: conservatively detect a Claude Code subagent from
    // the request's system prompt (built-in "You are Claude Code's X
    // subagent" or custom "You are X, operating as"). The main thread's
    // system prompt does not match, so it stays Main. This makes per-role
    // routing policies (e.g. `claude:researcher`) actually hit.
    if let Some(body_json) = &body_json {
        if let Some(system) = body_json.get("system") {
            let role = SubagentRole::from_system_prompt(system);
            if role != SubagentRole::Main {
                ctx.subagent_role = role;
                ctx.role_source = RoleSource::Heuristic;
            }
        }
    }

    // Capability requirements derived from the body activate the router's
    // capability stage (tool/vision/reasoning; Smart Gateway fix 2).
    // Conservative: absent signals stay false → no filtering.
    ctx.required_capabilities =
        crate::orchestration::capability_registry::derive_capability_req(
            &body_bytes,
            ProviderKind::Anthropic,
        );

    // State 3: a request declaring tools/functions may have executed a tool
    // upstream — never blind-retry it (the loop surfaces instead).
    let side_effect_risk = migration::body_has_side_effect_risk(&body_bytes);

    // Wire the protocol-specific parts into the shared retry/migrate loop.
    let agent = agent_id.to_string();
    let loop_state = state.clone();
    let forward_state = state.clone();
    let fwd_headers = req_headers.clone();
    let fwd_body = body_bytes.clone();
    // Subagent key for the diagnostics header (main | claude:name). Captured
    // once — the role is constant across retries/migrations of one request.
    let fwd_subagent_key = ctx.subagent_role.as_policy_key();
    let fwd_agent_id = agent_id.to_string();

    super::forward::run_with_migration(
        &state,
        ctx,
        agent,
        started_at,
        side_effect_risk,
        // Resolve closure: re-resolves per attempt; the router skips
        // degraded + quota-exhausted endpoints, so a post-failure
        // re-resolve picks a different endpoint automatically. It is a
        // FUTURE (not a blocking call) because the gateway runs on the Tokio
        // runtime — blocking_lock() would panic on a worker thread.
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
        // Forward closure: one protocol-specific dial + relay. Receives the
        // per-attempt ctx so the relay's usage backfill targets the CURRENT
        // attempt's `route_request` row (the loop rotates request_id on every
        // retry/migration).
        move |ctx: &TaskContext, route: &ResolvedRoute| -> ForwardFuture {
            let route = route.clone();
            let st = forward_state.clone();
            let headers = fwd_headers.clone();
            let body = fwd_body.clone();
            let agent_id = fwd_agent_id.clone();
            let subagent_key = fwd_subagent_key.clone();
            let request_id = ctx.request_id.to_string();
            Box::pin(async move {
                forward_one(&st, route, headers, body, &agent_id, &subagent_key, &request_id).await
            })
        },
        error_response,
    )
    .await
}

/// Dial + relay ONE attempt against the resolved route. Returns the observed
/// outcome (status/body/usage + whether any bytes streamed before a failure).
async fn forward_one(
    state: &GatewayState,
    route: ResolvedRoute,
    headers: hyper::HeaderMap,
    body: Bytes,
    agent_id: &str,
    subagent_key: &str,
    request_id: &str,
) -> ForwardOutcome {
    // Rewrite the model field to the resolved model.
    let rewritten = rewrite_model(&body, &route.model);
    // Cross-protocol bridge: when the resolved route speaks OpenAI or the
    // Responses API (e.g. opencode-go routed from an Anthropic inbound),
    // convert the request to that wire, dial its path with Bearer auth, and
    // convert the response (SSE or not) back to Anthropic on the way out.
    // Policy-gated prompt-cache injection is Anthropic-only and meaningless
    // on a converted upstream.
    let bridging = route.protocol != ProviderKind::Anthropic;
    let upstream_body = match route.protocol {
        ProviderKind::Openai => super::convert::anthropic_to_openai(&rewritten),
        ProviderKind::Responses => super::convert_responses::anthropic_to_responses(&rewritten),
        _ => match route.cache_strategy {
            crate::orchestration::identity::CacheStrategy::AnthropicExplicit => {
                crate::orchestration::cache::inject_cache_control(
                    &rewritten,
                    crate::orchestration::cache::DEFAULT_MAX_BREAKPOINTS,
                )
            }
            _ => rewritten,
        },
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
    // The URL join follows the resolved wire (`route.protocol`). An
    // unparseable base_url is a config error, not a retry opportunity — the
    // request carries real credentials, so we fail closed instead of falling
    // back to a loopback URL that would receive them.
    let upstream_url =
        match crate::protocol_url::parse_upstream_uri(&route.base_url, route.protocol) {
            Ok(u) => u,
            Err(e) => {
                return ForwardOutcome::Unreachable {
                    timeout: false,
                    message: e,
                }
            }
        };
    // Diagnostics header: lets the upstream (and the mock) echo back exactly
    // how this request was routed — which endpoint/model, why, and which
    // subagent role. Seen by the user in the reply text (mock echoes it).
    let mut diag_headers = headers.clone();
    let diag = format!(
        "agent={};endpoint={};model={};reason={};subagent={}",
        agent_id,
        route.endpoint_id,
        route.model,
        route.reason.as_str(),
        subagent_key,
    );
    diag_headers.insert(
        "x-nestra-route",
        hyper::header::HeaderValue::from_str(&diag)
            .unwrap_or_else(|_| hyper::header::HeaderValue::from_static("error")),
    );
    let upstream_req = match build_upstream_request(&upstream_url, &diag_headers, upstream_key, upstream_body, bridging)
    {
        Ok(r) => r,
        Err(e) => {
            return ForwardOutcome::Unreachable {
                timeout: false,
                message: format!("build request: {e}"),
            }
        }
    };
    let client = upstream_client();
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
                timeout: error_is_timeout(&e),
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
    let relay = match probe_and_relay(
        state.clone(),
        request_id,
        route.protocol,
        upstream_resp,
        &route.endpoint_id,
        &route.model,
    )
    .await
    {
        Ok(relay) => relay,
        Err(in_band_failure) => return in_band_failure,
    };
    // Convert the upstream body back to Anthropic when bridging. The pair
    // (inbound=Anthropic, upstream=route.protocol) picks the converter.
    let body = if bridging {
        super::convert::convert_relay_body(
            relay.body,
            status.is_success(),
            ProviderKind::Anthropic,
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

/// Detect a connect/read timeout in the legacy client's error chain.
/// `hyper_util::client::legacy::Error` does not expose `is_timeout` directly;
/// the underlying `hyper::Error` does. Shared with the OpenAI handler.
pub(super) fn error_is_timeout(e: &hyper_util::client::legacy::Error) -> bool {
    let mut cur: Option<&dyn std::error::Error> = Some(e);
    for _ in 0..6 {
        let Some(err) = cur else { break };
        if let Some(he) = err.downcast_ref::<hyper::Error>() {
            if he.is_timeout() {
                return true;
            }
        }
        cur = err.source();
    }
    false
}

/// `true` for `/v1/messages` OR `/<agent>/v1/messages` (the dispatcher routes
/// by agent prefix; both forms are accepted so prefix-less and agent-prefixed
/// configs both match). Trailing slash / query tolerated.
fn path_is_messages(p: &str) -> bool {
    let path = p.split('?').next().unwrap_or(p);
    let trimmed = path.trim_end_matches('/');
    if trimmed == "/v1/messages" {
        return true;
    }
    // `/<agent-id>/v1/messages` — strip the leading /<segment>/ and re-check.
    if let Some(rest) = trimmed.strip_prefix('/') {
        if let Some(after_agent) = rest.split_once('/') {
            // `after_agent.1` is `v1/messages` (NO leading slash) — normalize
            // to `/v1/messages` before comparing.
            let candidate = format!("/{}", after_agent.1);
            return candidate == "/v1/messages";
        }
    }
    false
}

/// Rewrite the `model` field in an Anthropic Messages body to `resolved_model`.
/// Returns the original bytes unchanged when the body isn't JSON or has no
/// `model` field (the upstream will reject it; we don't invent a model).
fn rewrite_model(body: &[u8], resolved_model: &str) -> Bytes {
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

/// Build the upstream URL from the resolved base_url + path. The base_url may
/// or may not end in `/v1`; we normalize so the final URL hits
/// `<base>/v1/messages`.


/// Build the upstream request, copying through the agent's headers minus the
/// hop-by-hop set, and setting `x-api-key` + `anthropic-version` if the agent
/// didn't send them (some third-party CLIs omit the version header).
pub(super) fn build_upstream_request(
    url: &hyper::Uri,
    original_headers: &hyper::HeaderMap,
    api_key: String,
    body: Bytes,
    bridging: bool,
) -> AppResult<Request<GatewayBody>> {
    let mut builder = Request::builder().method(Method::POST).uri(url);
    // Copy headers, skipping hop-by-hop + the auth header (we set it fresh).
    // `accept-encoding` is skipped too: gzip responses would need
    // decompression before usage parsing / Responses conversion, and the
    // upstream's own compression adds nothing on loopback-adjacent paths.
    // `cookie`/`x-api-key`/`api-key` are skipped so an agent's stale
    // credential or session cookie for a DIFFERENT provider can never reach
    // the resolved upstream (the gateway's own credential is authoritative).
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
        "accept-encoding",
        "x-api-key",
        "api-key",
        "cookie",
        "authorization",
    ];
    for (name, value) in original_headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if SKIP.contains(&lower.as_str()) {
            continue;
        }
        // An OpenAI upstream must not receive the agent's Anthropic
        // protocol version header (some strict gateways reject unknown
        // versioned headers).
        if bridging && lower == "anthropic-version" {
            continue;
        }
        builder = builder.header(name.clone(), value);
    }
    if bridging {
        // OpenAI upstream: Bearer auth, no anthropic-version.
        builder = builder.header(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|e| {
                AppError::Validation(format!("api key contains invalid header chars: {e}"))
            })?,
        );
    } else {
        // Anthropic upstream: x-api-key + version header. The agent's
        // original key is replaced — the gateway's resolved credential is
        // authoritative.
        builder = builder.header("x-api-key", HeaderValue::from_str(&api_key).map_err(|e| {
            AppError::Validation(format!("api key contains invalid header chars: {e}"))
        })?);
        if !original_headers.contains_key("anthropic-version") {
            builder = builder.header("anthropic-version", "2023-06-01");
        }
    }
    let req = builder
        .body(GatewayBody::Full(
            http_body_util::Full::new(body),
        ))
        .map_err(|e| AppError::Internal(format!("build upstream request: {e}")))?;
    Ok(req)
}

/// The result of relaying one upstream response: the agent-facing body, any
/// observed usage, and the generation/side-effect signals loop needs.
pub(super) struct RelayOutcome {
    pub body: GatewayBody,
    pub usage: Option<ObservedUsage>,
    /// True when ANY response bytes were received (correction #3 state 2 —
    /// a fresh upstream generation may have started before the relay
    /// completed or was interrupted).
    pub generation_started: bool,
    /// `Some(msg)` when a buffered body read failed mid-stream (partial
    /// bytes lost — the response is NOT usable and the generation is broken).
    pub body_error: Option<String>,
    /// Buffered-path tool observation (SSE returns None — its accumulator
    /// backfills after the stream ends).
    pub tool_calls: Option<i64>,
    pub tool_names: Option<std::collections::BTreeMap<String, u64>>,
}

/// Relay the upstream response back to the agent. For 2xx SSE responses we
/// stream verbatim and return immediately — the stream is committed, so a
/// later mid-stream drop is the agent's observation, not the loop's. For
/// everything else we buffer the body so we can observe usage, detect a
/// mid-stream interrupt, and hand the loop an honest
/// `generation_started`/`body_error` signal.
///
/// Streaming usage: the SSE body is wrapped in [`ObservingBody`], which
/// accumulates usage + tool-call ids while the agent consumes the stream and
/// backfills the `route_request` row when the stream ends (Smart Gateway
/// fix 1). `RelayOutcome.usage` is still `None` on this path — at return time
/// the stream has not been read yet. The accumulator is a `std::sync::Mutex`
/// held only for brief, non-`.await` sections inside `poll_frame` — the
/// original streaming-observation code panicked because it used
/// `tokio::sync::Mutex::blocking_lock` on the worker thread; that pattern
/// stays out.
///
/// `upstream_wire` is the UPSTREAM's protocol (`route.protocol`) — the body
/// wrapper sees the raw upstream bytes, which for a bridged route (e.g.
/// Anthropic inbound → Responses upstream) are in the upstream's SSE dialect,
/// not the inbound one.
///
/// Known limitation (documented, not forced): a native OpenAI inbound stream
/// reports usage only when the agent itself set `stream_options.include_usage`
/// (the gateway's Anthropic→OpenAI bridge injects it — see
/// `convert::anthropic_to_openai`).
pub(super) async fn relay_response<B>(
    state: GatewayState,
    request_id: String,
    upstream_wire: ProviderKind,
    upstream_resp: Response<B>,
    status: StatusCode,
) -> RelayOutcome
where
    B: hyper::body::Body<Data = Bytes> + Send + Sync + 'static,
    B::Error: std::fmt::Display,
{
    let (parts, body) = upstream_resp.into_parts();
    let is_sse = parts
        .headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("text/event-stream"))
        .unwrap_or(false);

    let tuning = super::tuning::snapshot(&state.tuning);

    if is_sse && status.is_success() {
        // Committed stream: bytes flow to the agent as they arrive. The loop
        // treats a 2xx SSE response as success; generation is already started.
        let observing = ObservingBody::new(
            body,
            state,
            request_id,
            upstream_wire,
            std::time::Duration::from_secs(tuning.stream_silence_timeout_secs),
        );
        return RelayOutcome {
            body: GatewayBody::streaming(observing),
            usage: None,
            generation_started: true,
            body_error: None,
            tool_calls: None,
            tool_names: None,
        };
    }

    // Buffered path: read all frames so we can observe usage from the final
    // JSON AND detect a mid-stream interrupt (partial bytes). An interrupted
    // read is NOT handed to the agent as an empty 2xx — the loop treats it as
    // a transient failure with `generation_started = true`.
    let mut buf = Vec::new();
    let mut got_any = false;
    let mut body_error: Option<String> = None;
    let mut stream = std::pin::pin!(body);
    // The whole buffered read is capped (tuning `buffered_body_timeout_secs`)
    // — an upstream that sends headers and then stalls must fail in bounded
    // time instead of hanging the agent forever.
    let buffered_timeout = std::time::Duration::from_secs(tuning.buffered_body_timeout_secs);
    let collect = async {
        loop {
            match stream.as_mut().frame().await {
                Some(Ok(frame)) => {
                    if let Some(bytes) = frame.data_ref() {
                        got_any = true;
                        buf.extend_from_slice(bytes);
                    }
                }
                Some(Err(e)) => {
                    body_error = Some(e.to_string());
                    break;
                }
                None => break,
            }
        }
    };
    if tokio::time::timeout(buffered_timeout, collect)
        .await
        .is_err()
    {
        tracing::warn!(
            timeout_secs = buffered_timeout.as_secs(),
            "gateway: buffered upstream body incomplete — treating as interrupted"
        );
        body_error = Some(format!(
            "buffered body incomplete after {}s",
            buffered_timeout.as_secs()
        ));
    }
    // Non-success upstream responses are buffered here — log the status and
    // the upstream's own error text (capped) so a 4xx/5xx failure is
    // diagnosable from the log without a proxy. The quota-marker classifier
    // reads the same body, so this is the raw text it sees.
    if !status.is_success() {
        let snippet: String = String::from_utf8_lossy(&buf[..buf.len().min(512)]).into_owned();
        tracing::info!(
            status = status.as_u16(),
            upstream_error = %snippet,
            "gateway: upstream error response"
        );
    }
    let usage = if status.is_success() && body_error.is_none() {
        let mut u = ObservedUsage::default();
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf) {
            if let Some(usage_obj) = v.get("usage").and_then(|u| u.as_object()) {
                use crate::orchestration::gateway::stream::merge_usage_obj_pub as merge;
                merge(usage_obj, &mut u);
            }
        }
        Some(u)
    } else {
        None
    };
    let (tool_calls, tool_names) = if status.is_success() && body_error.is_none() {
        super::stream::tools_in_buffered_body(&buf, upstream_wire)
    } else {
        (None, None)
    };
    RelayOutcome {
        body: GatewayBody::Full(Full::new(Bytes::from(buf))),
        usage,
        // Only SUCCESS bytes can mean generation started. An error body
        // (4xx/5xx with a JSON envelope) downloaded fine — but nothing was
        // generated, so the migration loop must treat the failure as
        // pre-response and replay it per class. Counting error bodies as
        // "generation started" surfaced every buffered 503 to tool-carrying
        // agents immediately, bypassing retry/failover entirely (observed:
        // zcode + opencode-go's "Endpoint is unavailable" 503s).
        generation_started: got_any && status.is_success(),
        body_error,
        tool_calls,
        tool_names,
    }
}

/// Probe a 2xx SSE upstream's first complete event for an in-band terminal
/// error, then relay. An in-band error (zero generated content + an
/// error-valued terminal `finish_reason`, `response.failed`, or an error
/// envelope — observed on opencode-go's free models) returns a 503-shaped
/// [`ForwardOutcome`] the retry/migration loop can act on, instead of
/// relaying a "successful" empty stream to the agent. Non-SSE / non-2xx
/// responses skip the probe.
pub(super) async fn probe_and_relay(
    state: GatewayState,
    request_id: &str,
    upstream_wire: ProviderKind,
    upstream_resp: Response<Incoming>,
    log_endpoint: &str,
    log_model: &str,
) -> Result<RelayOutcome, ForwardOutcome> {
    let status = upstream_resp.status();
    let is_sse = upstream_resp
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("text/event-stream"))
        .unwrap_or(false);
    if !(status.is_success() && is_sse) {
        return Ok(
            relay_response(state, request_id.to_string(), upstream_wire, upstream_resp, status)
                .await,
        );
    }
    let (parts, body) = upstream_resp.into_parts();
    let first_event_timeout = std::time::Duration::from_secs(
        super::tuning::snapshot(&state.tuning).first_event_timeout_secs,
    );
    match super::stream::probe_first_sse_event(body, first_event_timeout).await {
        super::stream::FirstEventProbe::InBandError { reason } => {
            tracing::warn!(
                endpoint = log_endpoint,
                model = log_model,
                reason = %reason,
                "gateway: upstream in-band stream error — failing attempt (retry/migrate)"
            );
            Err(ForwardOutcome::Responded {
                status: StatusCode::SERVICE_UNAVAILABLE,
                headers: HeaderMap::new(),
                body: GatewayBody::Full(Full::new(Bytes::new())),
                usage: None,
                tool_calls: None,
                tool_names: None,
                generation_started: false,
                body_error: None,
            })
        }
        super::stream::FirstEventProbe::Ok { held, rest } => {
            let resp =
                Response::from_parts(parts, super::stream::PrependBody::new(held, rest));
            Ok(
                relay_response(state, request_id.to_string(), upstream_wire, resp, status).await,
            )
        }
    }
}

/// A streaming body wrapper that observes usage + tool calls while relaying
/// SSE bytes verbatim to the agent (Smart Gateway fix 1).
///
/// ## Panic safety (the constraint this design exists to honor)
///
/// `poll_frame` runs on the Tokio worker thread, so it must never block.
/// The accumulator is a `std::sync::Mutex` locked only for a brief,
/// non-`.await` section per frame. The original streaming-usage code used
/// `tokio::sync::Mutex::blocking_lock` inside `poll_frame` and panicked —
/// that pattern must not come back.
///
/// ## Backfill
///
/// When the stream ends (cleanly or on error), a detached task locks the
/// gateway DB and fills `route_request.usage_*` / `tool_calls` for the
/// attempt's `request_id` — `record_attempt_outcome` has already finalized
/// the row with NULL usage when the 2xx stream was handed over. If the agent
/// disconnects and the body is dropped without reaching a terminal poll, no
/// backfill happens (accepted: best-effort observability).
struct ObservingBody {
    inner: Pin<Box<dyn hyper::body::Body<Data = Bytes, Error = std::io::Error> + Send + Sync>>,
    /// The upstream's wire dialect — picks the SSE observer.
    wire: ProviderKind,
    /// Accumulated usage + tool-call ids. `Arc` so the finish task can read a
    /// snapshot after the body is gone.
    obs: std::sync::Arc<std::sync::Mutex<StreamObservation>>,
    /// Carry buffer for an SSE line split across frames. Only grows to one
    /// line's length; a single unterminated line above `CARRY_CAP` is dropped
    /// whole (usage/tool events are nowhere near that size).
    carry: String,
    state: GatewayState,
    request_id: String,
    /// Backfill fired once (terminal poll already seen).
    done: bool,
    /// A terminal SSE error event is queued (delivered next poll, then the
    /// stream ends `None`) — converts an upstream mid-stream reset into a
    /// structured error the agent can parse instead of a connection teardown.
    pending_terminal: bool,
    /// Mid-stream silence watchdog (tuning `stream_silence_timeout_secs`):
    /// reset on every received frame; if the gap between frames exceeds the
    /// deadline the stream terminates with a structured error instead of
    /// hanging the agent forever. `None` when disabled (0).
    silence: Option<Pin<Box<tokio::time::Sleep>>>,
    silence_dur: std::time::Duration,
}

/// Cap on the unterminated-line carry buffer (bytes). Generous on purpose:
/// only a malformed/hostile stream approaches it.
const CARRY_CAP: usize = 128 * 1024;

impl ObservingBody {
    fn new(
        inner: impl hyper::body::Body<Data = Bytes> + Send + Sync + 'static,
        state: GatewayState,
        request_id: String,
        wire: ProviderKind,
        silence_timeout: std::time::Duration,
    ) -> Self {
        // Normalize the upstream error type (hyper::Error for `Incoming`,
        // io::Error for probe-wrapped bodies) — the Err arm below converts
        // either into a terminal SSE event, never a body error.
        let inner = inner.map_err(|_| std::io::Error::other("upstream stream error"));
        let silence = (!silence_timeout.is_zero())
            .then(|| Box::pin(tokio::time::sleep(silence_timeout)));
        Self {
            inner: Box::pin(inner),
            wire,
            obs: std::sync::Arc::new(std::sync::Mutex::new(StreamObservation::default())),
            carry: String::new(),
            state,
            request_id,
            done: false,
            pending_terminal: false,
            silence,
            silence_dur: silence_timeout,
        }
    }

    /// Feed one frame's bytes to the observer. Brief, non-`.await` lock.
    fn observe_frame(&mut self, bytes: &Bytes) {
        if let Ok(mut obs) = self.obs.lock() {
            observe_text_window(self.wire, &mut self.carry, bytes, &mut obs);
        }
    }

    /// Terminal state (clean end or upstream error): feed any trailing
    /// unterminated line, then fire the one-shot backfill task.
    fn finish(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        let rest = std::mem::take(&mut self.carry);
        if !rest.is_empty() {
            if let Ok(mut obs) = self.obs.lock() {
                observe_wire_chunk(self.wire, &rest, &mut obs);
            }
        }
        let state = self.state.clone();
        let request_id = std::mem::take(&mut self.request_id);
        let obs = self.obs.clone();
        tokio::spawn(async move {
            // Snapshot under a brief lock, then do the DB write without it.
            let (usage, tool_calls, tool_names) = match obs.lock() {
                Ok(o) => (
                    o.usage.clone(),
                    o.tool_call_ids.len() as i64,
                    serde_json::to_string(&o.tool_names).ok(),
                ),
                Err(_) => return, // poisoned — nothing sane to write
            };
            let conn = state.db.lock().await;
            if let Err(e) = crate::orchestration::store::backfill_route_request_usage(
                &conn,
                &request_id,
                &usage,
                Some(tool_calls),
                tool_names,
            ) {
                tracing::warn!("gateway: usage backfill failed for {request_id}: {e}");
            }
        });
    }

    /// Mid-stream failure (upstream error or silence timeout): finish the
    /// observation and produce the terminal SSE error event the agent parses
    /// instead of seeing a connection reset. The event is queued in
    /// `pending_terminal`; the next poll hands it out and the one after ends
    /// the stream.
    fn terminate(&mut self, msg: String) -> Bytes {
        self.finish();
        let terminal = terminal_sse_error(self.wire, &msg);
        self.pending_terminal = true;
        Bytes::from(terminal)
    }
}

/// Dispatch one complete-lines text window to the upstream-dialect observer.
fn observe_wire_chunk(
    wire: ProviderKind,
    text: &str,
    obs: &mut StreamObservation,
) {
    match wire {
        ProviderKind::Anthropic => observe_anthropic_chunk(text, obs),
        ProviderKind::Openai => observe_openai_chat_chunk(text, obs),
        ProviderKind::Responses => observe_responses_chunk(text, obs),
        // A custom upstream never reaches the SSE relay with a recognizable
        // dialect — no observation rather than a wrong-dialect parse.
        ProviderKind::Custom => {}
    }
}

/// Append one frame's bytes to the carry buffer, feed every COMPLETE line to
/// the observer, and keep the trailing partial line in `carry` (an SSE event
/// may split across frames). Pure so the split-frame behavior is unit-testable
/// without a socket. A single unterminated line above `CARRY_CAP` is dropped
/// whole (usage/tool events are nowhere near that size).
fn observe_text_window(
    wire: ProviderKind,
    carry: &mut String,
    bytes: &[u8],
    obs: &mut StreamObservation,
) {
    carry.push_str(&String::from_utf8_lossy(bytes));
    if carry.len() > CARRY_CAP && !carry.contains('\n') {
        carry.clear();
        return;
    }
    let Some(last_nl) = carry.rfind('\n') else {
        return; // no complete line yet
    };
    // Split off the trailing partial line; observe the complete prefix.
    let tail = carry.split_off(last_nl + 1);
    let complete = std::mem::replace(carry, tail);
    observe_wire_chunk(wire, &complete, obs);
}

/// Terminal SSE event emitted when the upstream stream dies mid-flight, so
/// the agent gets a structured error instead of a bare connection reset.
/// The body is JSON-encoded (serde) so the upstream's message (which can
/// carry quotes/newlines) stays inside a parseable `data:` line.
fn terminal_sse_error(wire: ProviderKind, message: &str) -> String {
    let json = serde_json::to_string(&serde_json::json!({
        "type": "error",
        "error": { "type": "overloaded_error", "message": message }
    }))
    .unwrap_or_else(|_| "{}".into());
    match wire {
        ProviderKind::Anthropic => format!("event: error\ndata: {json}\n\n"),
        _ => format!("data: {json}\n\ndata: [DONE]\n\n"),
    }
}

impl hyper::body::Body for ObservingBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        // SAFETY: `ObservingBody` wraps `Incoming` (Unpin) plus plain fields
        // mutated only here under the body's single-consumer contract — the
        // manual projection below is the documented hyper pattern for passing
        // a pinned body field through to the inner `Body::poll_frame`.
        // (All fields are Unpin, so `Pin::get_mut` would also be sound; this
        // mirrors the sibling stream wrappers.)
        let this = unsafe { self.get_unchecked_mut() };
        // The terminal error event was handed out on the previous poll — end
        // the stream now (hyper never sees a body error, so no connection
        // reset).
        if this.pending_terminal {
            this.pending_terminal = false;
            return std::task::Poll::Ready(None);
        }
        let poll = this.inner.as_mut().poll_frame(cx);
        match poll {
            std::task::Poll::Pending => {
                // Race the inner body against the silence deadline: polling
                // the Sleep here registers its waker alongside the inner
                // body's, so whichever completes first wakes this task. A
                // mid-stream stall (bytes already flowed, so no retry —
                // honest termination) must not hang the agent forever.
                if let Some(sleep) = this.silence.as_mut() {
                    if std::future::Future::poll(sleep.as_mut(), cx).is_ready() {
                        let msg = format!(
                            "upstream stream stalled: no data for {}s",
                            this.silence_dur.as_secs()
                        );
                        tracing::warn!("gateway: {msg}");
                        let terminal = this.terminate(msg);
                        return std::task::Poll::Ready(Some(Ok(
                            hyper::body::Frame::data(terminal),
                        )));
                    }
                }
                std::task::Poll::Pending
            }
            std::task::Poll::Ready(None) => {
                this.finish();
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Ready(Some(Err(e))) => {
                let msg = format!("upstream body: {e}");
                tracing::warn!("gateway: relay stream error — terminating cleanly: {msg}");
                let terminal = this.terminate(msg);
                std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(terminal))))
            }
            std::task::Poll::Ready(Some(Ok(frame))) => {
                if let Some(bytes) = frame.data_ref() {
                    this.observe_frame(bytes);
                }
                // Data arrived — push the silence deadline out again.
                if let Some(sleep) = this.silence.as_mut() {
                    sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + this.silence_dur);
                }
                std::task::Poll::Ready(Some(Ok(frame)))
            }
        }
    }
}

/// Build the agent-facing JSON error response.
pub(super) fn error_response(status: StatusCode, message: &str) -> Response<GatewayBody> {
    let mut resp = Response::new(error_body(status, message));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

fn error_body(_status: StatusCode, message: &str) -> GatewayBody {
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": "nestra_gateway_error", "message": message }
    });
    GatewayBody::json_full(body)
}

/// Build the agent-facing response from the upstream's status + headers +
/// (streaming or buffered) body. Drops hop-by-hop headers.
pub(super) fn build_agent_response(
    status: StatusCode,
    mut headers: hyper::HeaderMap,
    body: GatewayBody,
) -> Response<GatewayBody> {
    for h in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "transfer-encoding",
        "upgrade",
        "content-length",
    ] {
        headers.remove(h);
    }
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    resp
}

#[cfg(test)]
mod tests;
