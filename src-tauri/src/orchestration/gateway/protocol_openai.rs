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
    // the first chat. Answer it with the gateway's placeholder alias — the
    // router rewrites the real model per-task, so this list only feeds the
    // client's picker.
    if req.method() == Method::GET && path_is_models(req.uri().path()) {
        return Ok(models_response());
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

/// Dial + relay ONE attempt against the resolved route (OpenAI wire shape).
async fn forward_one(
    state: &GatewayState,
    route: ResolvedRoute,
    headers: hyper::HeaderMap,
    body: Bytes,
) -> ForwardOutcome {
    let rewritten = rewrite_model(&body, &route.model);
    // Responses-class models (grok-4.5, gpt-5.6-luna) cannot speak the chat
    // wire — convert the request to the Responses API and dial its path.
    // Everything else stays on Chat Completions.
    let bridging = route.protocol == ProviderKind::Responses;
    let upstream_body = if bridging {
        super::convert_responses::chat_to_responses(&rewritten)
    } else {
        rewritten
    };
    let upstream_kind = if bridging {
        ProviderKind::Responses
    } else {
        ProviderKind::Openai
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
    let upstream_req =
        match build_upstream_request(&upstream_url, &headers, upstream_key, upstream_body) {
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
            };
        }
    };

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();
    // Relay + observe. Reuse the Anthropic relay for the mechanics (it streams
    // verbatim + observes usage from SSE or buffered JSON); the OpenAI usage
    // fields differ (prompt_tokens/completion_tokens) so we post-process the
    // observed usage to map them onto our standard input/output fields.
    let relay = super::protocol_anthropic::relay_response(upstream_resp, status).await;
    let usage = relay.usage.map(|u| map_openai_usage(u, &resp_headers));
    // Convert the responses-wire body back to chat chunks when bridging.
    let body = if bridging {
        super::convert::convert_relay_body(
            relay.body,
            status.is_success(),
            ProviderKind::Openai,
            ProviderKind::Responses,
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
    }
}

/// OpenAI usage uses `prompt_tokens` / `completion_tokens` (not
/// `input_tokens` / `output_tokens`). The shared usage observer in
/// `stream::merge_usage_obj` now recognizes BOTH vocabularies (Anthropic +
/// OpenAI field names), so the non-streaming buffered path captures OpenAI
/// usage automatically and this mapping is an identity passthrough.
///
/// Streaming responses (body not buffered at this point) stay best-effort:
/// usage may be None — the status-based outcome is still recorded.
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

/// `GET /models` answer: the gateway's single placeholder model. The alias
/// for every non-Claude agent is `nestra` (commands.rs `apply_gateway_alias`);
/// Claude Code never reaches the OpenAI path. The router rewrites the real
/// model per-task, so this list only feeds the client's picker — the
/// `limit`/`tool_call` fields are neutral placeholders so OpenAI-compatible
/// clients (AI SDK) can render a usable model entry with tool support.
fn models_payload() -> serde_json::Value {
    serde_json::json!({
        "object": "list",
        "data": [{
            "id": "nestra",
            "object": "model",
            "created": 0,
            "owned_by": "nestra-gw",
            "limit": { "context": 200000, "output": 8192 },
            "tool_call": true,
            "reasoning": true,
        }],
    })
}

pub(super) fn models_response() -> Response<GatewayBody> {
    let body = models_payload();
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

/// Build the upstream request. OpenAI uses `Authorization: Bearer <key>` (not
/// `x-api-key`). We strip any inbound `authorization` and set it fresh from
/// the resolved credential.
fn build_upstream_request(
    url: &hyper::Uri,
    original_headers: &hyper::HeaderMap,
    api_key: String,
    body: Bytes,
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

fn error_response(status: StatusCode, message: &str) -> Response<GatewayBody> {
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
mod tests {
    use super::*;

    #[test]
    fn path_is_chat_completions_matches() {
        assert!(path_is_chat_completions("/v1/chat/completions"));
        assert!(path_is_chat_completions("/v1/chat/completions/"));
        assert!(path_is_chat_completions("/v1/chat/completions?foo=bar"));
        assert!(path_is_chat_completions("/pi/v1/chat/completions"));
        assert!(path_is_chat_completions("/opencode-desktop/v1/chat/completions"));
        // No-`/v1` forms: the OpenAI-compatible SDK appends only
        // `/chat/completions` to the configured base URL, and configs written
        // before the `/v1` fix omit the segment.
        assert!(path_is_chat_completions("/chat/completions"));
        assert!(path_is_chat_completions("/opencode-desktop/chat/completions"));
        assert!(path_is_chat_completions("/opencode-desktop/chat/completions/"));
        assert!(!path_is_chat_completions("/v1/messages"));
        assert!(!path_is_chat_completions("/v1/chat/completions/extra"));
        assert!(!path_is_chat_completions("/claude-code-cli/v1/messages"));
        assert!(!path_is_chat_completions("/opencode-desktop/v1/messages"));
    }

    #[test]
    fn path_is_models_matches() {
        assert!(path_is_models("/v1/models"));
        assert!(path_is_models("/models"));
        assert!(path_is_models("/opencode-desktop/v1/models"));
        assert!(path_is_models("/opencode-desktop/models"));
        assert!(path_is_models("/opencode-desktop/v1/models?foo=bar"));
        assert!(!path_is_models("/v1/chat/completions"));
        assert!(!path_is_models("/opencode-desktop/v1/models/extra"));
    }

    #[test]
    fn rewrite_model_replaces_field() {
        let body = br#"{"model":"gpt-4o","messages":[]}"#;
        let out = rewrite_model(body, "resolved-model");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "resolved-model");
    }

    #[test]
    fn models_payload_carries_placeholder_capabilities() {
        let v = models_payload();
        let m = &v["data"][0];
        assert_eq!(m["id"], "nestra");
        assert_eq!(m["tool_call"], true);
        assert_eq!(m["reasoning"], true);
        assert!(m["limit"]["context"].is_number());
        assert!(m["limit"]["output"].is_number());
    }

    /// End-to-end "does it actually work" check: one OpenAI chat request for
    /// the `nestra` alias flows through the real forward path to a local
    /// upstream — the router picks the z-ai-style endpoint's DEFAULT model
    /// (glm-5.2, not alphabetical glm-4.7), dials the OPENAI protocol row,
    /// rewrites the body model, and relays the 200 response.
    #[tokio::test]
    async fn handle_bytes_routes_default_model_to_matching_protocol_row() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // 1. Local upstream that records the request path + body model and
        // replies with a 200 OpenAI completion.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                let n = socket.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                let header_end = text.find("\r\n\r\n");
                let complete = match header_end {
                    Some(i) => {
                        let head = &text[..i];
                        let clen = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        let body = &text[i + 4..];
                        if clen > 0 {
                            body.len() >= clen
                        } else {
                            // Chunked: terminator is a final `0\r\n\r\n`.
                            body.contains("0\r\n\r\n")
                        }
                    }
                    None => false,
                };
                if complete {
                    break;
                }
            }
            let text = String::from_utf8_lossy(&buf).to_string();
            let path = text
                .lines()
                .next()
                .unwrap_or("")
                .split(' ')
                .nth(1)
                .unwrap_or("")
                .to_string();
            // hyper sends the body chunked: `<hex>\r\n{json}\r\n0\r\n\r\n`.
            // Strip the chunk framing before parsing the JSON.
            let body_json: serde_json::Value = text
                .split_once("\r\n\r\n")
                .and_then(|(_, rest)| {
                    let start = rest.find('{')?;
                    serde_json::from_str(
                        &rest[start..].trim_end_matches("\r\n0\r\n\r\n"),
                    )
                    .ok()
                })
                .unwrap_or_default();
            let model = body_json["model"].as_str().unwrap_or("").to_string();
            let payload = r#"{"id":"chatcmpl-1","object":"chat.completion","model":"glm-5.2","choices":[{"index":0,"message":{"role":"assistant","content":"hi from upstream"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                payload.len(),
                payload
            );
            socket.write_all(resp.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
            (path, model, text)
        });

        // 2. In-memory DB shaped like the user's z-ai endpoint: anthropic row
        // FIRST, openai row pointing at the local upstream, default glm-5.2.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::build_v1(&conn).unwrap();
        for a in crate::agents::agents() {
            conn.execute(
                "INSERT OR IGNORE INTO agent (id, kind, display_name, status, last_detected_at, enabled)
                 VALUES (?1, ?2, ?3, 'ok', 0, 1)",
                rusqlite::params![a.id, a.kind, a.display_name],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status, models_json)
             VALUES ('z-ai','anthropic','z.ai',0,'valid',?1)",
            rusqlite::params![r#"{"available":["glm-4.7","glm-5.2"],"default":"glm-5.2"}"#],
        )
        .unwrap();
        for (protocol, base) in [
            ("anthropic".to_string(), "https://api.z.ai/api/anthropic".to_string()),
            ("openai-comp".to_string(), format!("http://{addr}")),
        ] {
            conn.execute(
                "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url) VALUES ('z-ai',?1,?2)",
                rusqlite::params![protocol, base],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO agent_provider_binding (agent_id, endpoint_id, active, created_at)
             VALUES ('opencode-desktop','z-ai',1,0)",
            [],
        )
        .unwrap();
        crate::orchestration::capability_registry::rebuild(&conn).unwrap();

        // 3. GatewayState with a stub credential reader (no keychain).
        let state = GatewayState {
            db: std::sync::Arc::new(tokio::sync::Mutex::new(conn)),
            health: std::sync::Arc::new(crate::orchestration::health::ProviderHealth::new()),
            quota: std::sync::Arc::new(crate::orchestration::quota_state::QuotaState::new()),
            affinity: std::sync::Arc::new(crate::orchestration::router::RouteAffinity::new()),
            credential_reader: std::sync::Arc::new(|_| Ok(Some("test-key".into()))),
            loopback_token: std::sync::Arc::new(tokio::sync::RwLock::new("test-token".into())),
        };

        // 4. One OpenAI chat request for the alias model.
        let body =
            br#"{"model":"nestra","messages":[{"role":"user","content":"hello"}],"stream":false}"#;
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            "content-type",
            hyper::header::HeaderValue::from_static("application/json"),
        );
        let resp = handle_bytes(headers, Bytes::from_static(body), state, "opencode-desktop")
            .await
            .unwrap();
        assert_eq!(resp.status(), hyper::StatusCode::OK);

        // 5. The upstream saw the resolved DEFAULT model on the openai row's
        // base (bare local addr → `/v1/chat/completions` join).
        let (path, model, raw) = upstream.await.unwrap();
        assert_eq!(model, "glm-5.2", "alias must resolve to the endpoint default; upstream saw: {raw:?}");
        assert_eq!(path, "/v1/chat/completions");
    }
}
