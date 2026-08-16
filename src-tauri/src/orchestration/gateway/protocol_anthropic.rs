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
use hyper::header::HeaderValue;
use hyper::{Method, Request, Response, StatusCode};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;

use crate::error::{AppError, AppResult};
use crate::config_writer::ProviderKind;
use crate::orchestration::identity::{
    ResolvedRoute, RoleSource, SubagentRole, TaskContext, TaskLifecycle,
};
use crate::orchestration::migration;
use crate::orchestration::router::{self, RouterInputs};

use super::forward::{ForwardFuture, ForwardOutcome};
use super::stream::{read_request_body, GatewayBody, ObservedUsage};
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
        // Forward closure: one protocol-specific dial + relay.
        move |route: &ResolvedRoute| -> ForwardFuture {
            let route = route.clone();
            let st = forward_state.clone();
            let headers = fwd_headers.clone();
            let body = fwd_body.clone();
            let agent_id = fwd_agent_id.clone();
            let subagent_key = fwd_subagent_key.clone();
            Box::pin(async move { forward_one(&st, route, headers, body, &agent_id, &subagent_key).await })
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
    let relay = relay_response(upstream_resp, status).await;
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
}

/// Relay the upstream response back to the agent. For 2xx SSE responses we
/// stream verbatim and return immediately — the stream is committed, so a
/// later mid-stream drop is the agent's observation, not the loop's. For
/// everything else we buffer the body so we can observe usage, detect a
/// mid-stream interrupt, and hand the loop an honest
/// `generation_started`/`body_error` signal.
///
/// Known limitation (deliberate): SSE streams report `usage: None`. Usage
/// observation on the streaming path was removed — it scanned chunks via a
/// `Mutex` whose `blocking_lock` panicked on the Tokio worker thread — so
/// streaming requests record no token usage. Buffered (non-SSE) responses
/// observe usage from the final JSON body. Reintroducing streaming usage
/// needs an async-safe accumulator and is tracked as a separate effort.
pub(super) async fn relay_response(
    upstream_resp: Response<Incoming>,
    status: StatusCode,
) -> RelayOutcome {
    let (parts, body) = upstream_resp.into_parts();
    let is_sse = parts
        .headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("text/event-stream"))
        .unwrap_or(false);

    if is_sse && status.is_success() {
        // Committed stream: bytes flow to the agent as they arrive. The loop
        // treats a 2xx SSE response as success; generation is already started.
        let observing = ObservingBody::new(body);
        return RelayOutcome {
            body: GatewayBody::streaming(observing),
            usage: None,
            generation_started: true,
            body_error: None,
        };
    }

    // Buffered path: read all frames so we can observe usage from the final
    // JSON AND detect a mid-stream interrupt (partial bytes). An interrupted
    // read is NOT handed to the agent as an empty 2xx — the loop treats it as
    // a transient failure with `generation_started = true`.
    let mut buf = Vec::new();
    let mut got_any = false;
    let mut body_error: Option<String> = None;
    let mut stream = body;
    loop {
        match stream.frame().await {
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
    RelayOutcome {
        body: GatewayBody::Full(Full::new(Bytes::from(buf))),
        usage,
        generation_started: got_any,
        body_error,
    }
}

/// A pass-through streaming body wrapper. (ObservingBody used to scan
/// SSE chunks for usage via a Mutex — but that `blocking_lock` panics on the
/// Tokio worker thread, and the accumulated data was never consumed anyway;
/// the usage observation for non-streaming bodies happens on the buffered
/// path below, and SSE cache metrics are captured from `message_start` by
/// `stream::observe_usage_chunk` only when the loop has a buffered body.)
struct ObservingBody {
    inner: Incoming,
}

impl ObservingBody {
    fn new(inner: Incoming) -> Self {
        Self { inner }
    }
}

impl hyper::body::Body for ObservingBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        // SAFETY: `ObservingBody` is a single-field wrapper around `Incoming`,
        // which is `Unpin` — the manual projection below is the documented
        // hyper pattern for passing a pinned body field through to the inner
        // `Body::poll_frame`. (All fields are Unpin, so `Pin::get_mut` would
        // also be sound; this mirrors the sibling stream wrappers.)
        let this = unsafe { self.get_unchecked_mut() };
        let poll = std::pin::Pin::new(&mut this.inner).poll_frame(cx);
        match poll {
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Ready(Some(Err(e))) => std::task::Poll::Ready(Some(Err(
                std::io::Error::other(format!("upstream body: {e}")),
            ))),
            std::task::Poll::Ready(Some(Ok(frame))) => std::task::Poll::Ready(Some(Ok(frame))),
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
mod tests {
    use super::*;

    #[test]
    fn path_is_messages_accepts_both_forms() {
        // prefix-less path + agent-prefixed path both route here
        // (dispatch already splits agents, so ANY `/<agent>/v1/messages` is
        // this handler's business — including hypothetical non-claude agents).
        assert!(path_is_messages("/v1/messages"));
        assert!(path_is_messages("/v1/messages/"));
        assert!(path_is_messages("/v1/messages?foo=bar"));
        assert!(path_is_messages("/claude-code-cli/v1/messages"));
        assert!(path_is_messages("/pi/v1/messages"), "any agent prefix is accepted here");
        assert!(!path_is_messages("/v1/chat/completions"));
        assert!(!path_is_messages("/claude-code-cli/v1/messages/extra"));
    }
}
