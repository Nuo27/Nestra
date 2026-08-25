//! End-to-end gateway tests for the Responses API wire: each inbound
//! (Anthropic Messages / Chat Completions / Responses) routed to a local
//! mock upstream, exercising request conversion, URL join, and response
//! conversion back to the inbound format.

use bytes::Bytes;
use hyper::header::{HeaderValue, CONTENT_TYPE};
use hyper::HeaderMap;
use std::sync::Arc;

use super::stream::GatewayBody;
use super::GatewayState;

/// Local mock upstream: reads one HTTP request, returns `(path, body_json)`,
/// and replies with `payload` (raw HTTP response text).
async fn mock_upstream(
    payload: String,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<(String, serde_json::Value)>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
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
        let body_json: serde_json::Value = text
            .split_once("\r\n\r\n")
            .and_then(|(_, rest)| {
                let start = rest.find('{')?;
                serde_json::from_str(&rest[start..].trim_end_matches("\r\n0\r\n\r\n")).ok()
            })
            .unwrap_or_default();
        socket.write_all(payload.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        (path, body_json)
    });
    (addr, handle)
}

/// Seed the in-memory DB with an opencode-go-style endpoint (dual rows on
/// one base_url) plus a `*` policy targeting its default model.
fn seed_conn(
    addr: std::net::SocketAddr,
    agent: &str,
    models_json: &str,
) -> rusqlite::Connection {
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
         VALUES ('opencode-go','custom','OpenCode Go',1,'valid',?1)",
        rusqlite::params![models_json],
    )
    .unwrap();
    for protocol in ["anthropic", "openai-comp"] {
        conn.execute(
            "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url) VALUES ('opencode-go',?1,?2)",
            rusqlite::params![protocol, format!("http://{addr}")],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO agent_provider_binding (agent_id, endpoint_id, active, created_at)
         VALUES (?1,'opencode-go',1,0)",
        rusqlite::params![agent],
    )
    .unwrap();
    seed_star_policy(&conn, agent, "opencode-go", models_json);
    crate::orchestration::capability_registry::rebuild(&conn).unwrap();
    conn
}

/// Seed a `*` policy row targeting `(endpoint, models_json.default)`.
fn seed_star_policy(conn: &rusqlite::Connection, agent: &str, endpoint: &str, models_json: &str) {
    let default = serde_json::from_str::<serde_json::Value>(models_json)
        .ok()
        .and_then(|v| {
            v.get("default")
                .and_then(|d| d.as_str())
                .map(String::from)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "m-1".into());
    let targets = serde_json::to_string(&vec![serde_json::json!({
        "endpoint": endpoint,
        "model": default,
    })])
    .unwrap();
    conn.execute(
        "INSERT INTO routing_policy (agent_id, role, route_targets, migrate_on_quota,
                                    inject_cache_control, affinity_scope, updated_at)
         VALUES (?1,'*',?2,1,0,'task',1)",
        rusqlite::params![agent, targets],
    )
    .unwrap();
}

pub(super) fn state_for(conn: rusqlite::Connection) -> GatewayState {
    GatewayState {
        db: Arc::new(tokio::sync::Mutex::new(conn)),
        health: Arc::new(crate::orchestration::health::ProviderHealth::new()),
        quota: Arc::new(crate::orchestration::quota_state::QuotaState::new()),
        affinity: Arc::new(crate::orchestration::router::RouteAffinity::new()),
        credential_reader: Arc::new(|_| Ok(Some("test-key".into()))),
        // tests_e2e exercises the protocol handlers directly (not `dispatch`),
        // so the token is never read here; supplying one keeps the struct valid.
        loopback_token: Arc::new(tokio::sync::RwLock::new("test-token".into())),
        tuning: super::tuning::shared_default(),
    }
}

fn headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h
}

/// Collect a buffered GatewayBody into its JSON value.
async fn body_json(body: GatewayBody) -> serde_json::Value {
    use http_body_util::BodyExt as _;
    let bytes = body.collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// Tier-aware routing: a `tier:haiku` policy row steers haiku-tier requests
/// (model id `claude-haiku-4-5`) to a cheaper endpoint's default model, while
/// sonnet-tier requests keep resolving to the agent's bound endpoint. Both
/// upstreams speak the Anthropic wire (same-wire relay, so a plain message
/// JSON payload suffices).
#[tokio::test]
async fn tier_policy_routes_haiku_requests_to_preferred_endpoint() {
    let msg_payload = |model: &str| {
        let payload = format!(
            r#"{{"id":"msg_1","type":"message","role":"assistant","model":"{model}","content":[{{"type":"text","text":"hi"}}],"stop_reason":"end_turn","usage":{{"input_tokens":10,"output_tokens":2}}}}"#
        );
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            payload.len(),
            payload
        )
    };
    let (main_addr, main_upstream) = mock_upstream(msg_payload("glm-5.2")).await;
    let (cheap_addr, cheap_upstream) = mock_upstream(msg_payload("cheap-2")).await;

    // Seed: bound endpoint (main, glm-5.2) + a cheap endpoint whose default
    // model is cheap-2, plus a tier:haiku policy preferring the cheap one.
    let conn = {
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
        for (id, addr, default) in [
            ("ep-main", main_addr, "glm-5.2"),
            ("ep-cheap", cheap_addr, "cheap-2"),
        ] {
            conn.execute(
                "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status, models_json)
                 VALUES (?1,'custom','test',1,'valid',?2)",
                rusqlite::params![
                    id,
                    format!(r#"{{"available":["{default}"],"default":"{default}"}}"#)
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url) VALUES (?1,'anthropic',?2)",
                rusqlite::params![id, format!("http://{addr}")],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO agent_provider_binding (agent_id, endpoint_id, active, created_at)
             VALUES ('claude-code-cli','ep-main',1,0)",
            [],
        )
        .unwrap();
        // tier:haiku → the cheap endpoint; `*` → the main endpoint.
        let haiku_targets = serde_json::to_string(&vec![serde_json::json!({
            "endpoint": "ep-cheap", "model": "cheap-2"
        })])
        .unwrap();
        conn.execute(
            "INSERT INTO routing_policy (agent_id, role, route_targets, migrate_on_quota,
                                        inject_cache_control, affinity_scope, updated_at)
             VALUES ('claude-code-cli','tier:haiku',?1,1,0,'task',1)",
            rusqlite::params![haiku_targets],
        )
        .unwrap();
        seed_star_policy(&conn, "claude-code-cli", "ep-main", r#"{"available":["glm-5.2"],"default":"glm-5.2"}"#);
        crate::orchestration::capability_registry::rebuild(&conn).unwrap();
        conn
    };
    let state = state_for(conn);

    // Haiku-tier request → the tier policy's preferred endpoint + its model.
    let body = br#"{"model":"claude-haiku-4-5","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;
    let resp = super::protocol_anthropic::handle_bytes(
        headers(),
        Bytes::from_static(body),
        state.clone(),
        "claude-code-cli",
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), hyper::StatusCode::OK);
    let (path, req_body) = cheap_upstream.await.unwrap();
    assert_eq!(path, "/v1/messages");
    assert_eq!(req_body["model"], "cheap-2", "haiku tier follows the tier:haiku policy");
    drop(body_json(resp.into_body()).await);

    // Sonnet-tier request → the bound endpoint's default (no tier row for it).
    let body = br#"{"model":"claude-sonnet-4-5","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;
    let resp = super::protocol_anthropic::handle_bytes(
        headers(),
        Bytes::from_static(body),
        state,
        "claude-code-cli",
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), hyper::StatusCode::OK);
    let (_path, req_body) = main_upstream.await.unwrap();
    assert_eq!(req_body["model"], "glm-5.2", "sonnet tier keeps the bound default");
}

/// Anthropic inbound (Claude Code) → responses-class model (grok-4.5):
/// the request is converted to the Responses API, dialed on `/v1/responses`,
/// and the responses-shaped response is converted back to an Anthropic
/// message.
#[tokio::test]
async fn anthropic_inbound_to_responses_upstream() {
    let payload = r#"{"id":"resp_1","object":"response","status":"completed","model":"grok-4.5","output":[{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hi from grok"}]}],"usage":{"input_tokens":100,"output_tokens":5,"total_tokens":105}}"#;
    let (addr, upstream) = mock_upstream(format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
        payload.len(),
        payload
    ))
    .await;

    let conn = seed_conn(
        addr,
        "claude-code-cli",
        r#"{"available":["grok-4.5"],"default":"grok-4.5"}"#,
    );
    let state = state_for(conn);

    let body = br#"{"model":"claude-haiku-4-5","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;
    let resp = super::protocol_anthropic::handle_bytes(
        headers(),
        Bytes::from_static(body),
        state,
        "claude-code-cli",
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), hyper::StatusCode::OK);

    // Upstream saw the Responses path + a responses-shaped converted body.
    let (path, req_body) = upstream.await.unwrap();
    assert_eq!(path, "/v1/responses");
    assert_eq!(req_body["model"], "grok-4.5", "model rewritten to the resolved model");
    assert_eq!(req_body["input"][0]["role"], "user");
    assert!(
        req_body.get("instructions").is_none() || req_body["instructions"].is_null(),
        "anthropic->responses conversion must not fabricate instructions"
    );

    // Response converted back to an Anthropic message.
    let out = body_json(resp.into_body()).await;
    assert_eq!(out["type"], "message");
    assert_eq!(out["role"], "assistant");
    assert_eq!(out["content"][0]["type"], "text");
    assert_eq!(out["content"][0]["text"], "hi from grok");
    assert_eq!(out["stop_reason"], "end_turn");
}

/// Chat inbound (OpenCode Desktop) → responses-class model: the chat
/// request is converted to Responses and the responses response back to a
/// chat completion.
#[tokio::test]
async fn chat_inbound_to_responses_upstream() {
    let payload = r#"{"id":"resp_1","object":"response","status":"completed","model":"grok-4.5","output":[{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":50,"output_tokens":3,"total_tokens":53}}"#;
    let (addr, upstream) = mock_upstream(format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
        payload.len(),
        payload
    ))
    .await;

    let conn = seed_conn(
        addr,
        "opencode-desktop",
        r#"{"available":["grok-4.5"],"default":"grok-4.5"}"#,
    );
    let state = state_for(conn);

    let body = br#"{"model":"nestra","messages":[{"role":"user","content":"hi"}],"max_tokens":64}"#;
    let resp = super::protocol_openai::handle_bytes(
        headers(),
        Bytes::from_static(body),
        state,
        "opencode-desktop",
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), hyper::StatusCode::OK);

    let (path, req_body) = upstream.await.unwrap();
    assert_eq!(path, "/v1/responses");
    assert_eq!(req_body["model"], "grok-4.5");
    assert_eq!(req_body["input"][0]["role"], "user");

    // Response converted back to a chat completion.
    let out = body_json(resp.into_body()).await;
    assert_eq!(out["object"], "chat.completion");
    assert_eq!(out["choices"][0]["message"]["content"], "ok");
    assert_eq!(out["choices"][0]["finish_reason"], "stop");
}

/// Chat inbound (OpenCode Desktop) → endpoint whose ONLY row is Anthropic
/// (single-row custom endpoint, e.g. MiniMax-M3 on `…/anthropic`): the chat
/// request is converted to Messages, dialed on `/v1/messages` with
/// `x-api-key` auth, and the anthropic response converted back to a chat
/// completion. This is the case that used to 404 "page not found".
#[tokio::test]
async fn chat_inbound_to_anthropic_upstream() {
    let payload = r#"{"id":"msg_1","type":"message","role":"assistant","model":"MiniMax-M3","content":[{"type":"text","text":"hi from m3"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":2}}"#;
    let (addr, upstream) = mock_upstream(format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
        payload.len(),
        payload
    ))
    .await;

    let conn = {
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
        // anthropic-only row — the single-row custom endpoint.
        conn.execute(
            "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status, models_json)
             VALUES ('m3','custom','MiniMax',1,'valid',?1)",
            rusqlite::params![r#"{"available":["MiniMax-M3"],"default":"MiniMax-M3"}"#],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url) VALUES ('m3','anthropic',?1)",
            rusqlite::params![format!("http://{addr}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agent_provider_binding (agent_id, endpoint_id, active, created_at)
             VALUES ('opencode-desktop','m3',1,0)",
            [],
        )
        .unwrap();
        seed_star_policy(&conn, "opencode-desktop", "m3", r#"{"available":["MiniMax-M3"],"default":"MiniMax-M3"}"#);
        crate::orchestration::capability_registry::rebuild(&conn).unwrap();
        conn
    };
    let state = state_for(conn);

    let body = br#"{"model":"nestra","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;
    let resp = super::protocol_openai::handle_bytes(
        headers(),
        Bytes::from_static(body),
        state,
        "opencode-desktop",
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), hyper::StatusCode::OK);

    // Upstream saw the Messages path + an anthropic-shaped __converted__ body.
    let (path, req_body) = upstream.await.unwrap();
    assert_eq!(path, "/v1/messages");
    assert_eq!(req_body["model"], "MiniMax-M3", "model rewritten to the resolved model");
    assert_eq!(
        req_body["messages"][0]["role"], "user",
        "chat body converted to anthropic messages"
    );

    // Response converted back to a chat completion.
    let out = body_json(resp.into_body()).await;
    assert_eq!(out["object"], "chat.completion");
    assert_eq!(out["choices"][0]["message"]["content"], "hi from m3");
    assert_eq!(out["choices"][0]["finish_reason"], "stop");
    assert_eq!(out["usage"]["prompt_tokens"], 10);
}

/// Anthropic inbound, streaming (`stream: true`): the 2xx SSE response is
/// relayed verbatim to the agent, and the stream's observed usage + tool-call
/// count are BACKFILLED into `route_request` after the stream ends (Smart
/// Gateway fix 1 — the loop itself records NULL usage when it hands over the
/// committed stream). The seeded endpoint resolves to its openai-comp row, so
/// this also exercises the accumulator keying off the UPSTREAM wire (raw
/// OpenAI chunks observed below the anthropic-converting wrapper).
#[tokio::test]
async fn streaming_sse_backfills_usage_and_tool_calls() {
    // The router resolves the seeded endpoint's openai-comp row, so the mock
    // must speak chat-completions SSE; the observation asserts on OpenAI
    // field names (prompt_tokens/…) and index-keyed tool-call dedup.
    let openai_sse = concat!(
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"bash\",\"arguments\":\"{}\"}}]}}]}\n",
        "\n",
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"more\"}}]}}]}\n",
        "\n",
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":23,\"prompt_tokens_details\":{\"cached_tokens\":7}}}\n",
        "\n",
        "data: [DONE]\n",
    );
    let (addr, upstream) = mock_upstream(format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
        openai_sse.len(),
        openai_sse
    ))
    .await;

    let conn = seed_conn(
        addr,
        "claude-code-cli",
        r#"{"available":["test-model"],"default":"test-model"}"#,
    );
    let state = state_for(conn);
    let db = state.db.clone();

    let body =
        br#"{"model":"claude-haiku-4-5","stream":true,"max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;
    let resp = super::protocol_anthropic::handle_bytes(
        headers(),
        Bytes::from_static(body),
        state,
        "claude-code-cli",
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), hyper::StatusCode::OK);
    assert!(
        resp.headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/event-stream"))
    );

    // Consume the stream to drive the observing body to its terminal poll —
    // that is where the backfill task spawns. Under the tokio test runtime,
    // so a panic inside poll_frame (the old blocking_lock failure mode)
    // would fail the test here.
    use http_body_util::BodyExt as _;
    let relayed = resp.into_body().collect().await.unwrap().to_bytes();
    let relayed = String::from_utf8_lossy(&relayed);
    assert!(relayed.contains("message_stop"), "SSE bytes relay verbatim");

    let (path, _req) = upstream.await.unwrap();
    assert_eq!(path, "/v1/chat/completions");

    // The backfill runs in a detached task — poll the row briefly.
    let mut row = None;
    for _ in 0..40 {
        let observed = {
            let conn = db.lock().await;
            conn.query_row(
                "SELECT usage_input, usage_output, cache_read, tool_calls
                 FROM route_request ORDER BY started_at DESC LIMIT 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, Option<i64>>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .ok()
        };
        if let Some(v) = observed.filter(|(i, ..)| i.is_some()) {
            row = Some(v);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        row,
        Some((Some(11), Some(23), Some(7), Some(1))),
        "stream usage + tool_calls backfilled onto route_request"
    );
}

/// Multi-shot mock upstream: serves `count` connections, each replied to
/// with `payload`; returns every request's (path, body).
async fn mock_upstream_n(
    payload: String,
    count: usize,
) -> (
    std::net::SocketAddr,
    tokio::task::JoinHandle<Vec<(String, serde_json::Value)>>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let mut seen = Vec::new();
        for _ in 0..count {
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
                let complete = text
                    .split_once("\r\n\r\n")
                    .map(|(head, body)| {
                        let clen = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if clen > 0 {
                            body.len() >= clen
                        } else {
                            body.contains("0\r\n\r\n")
                        }
                    })
                    .unwrap_or(false);
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
            let body_json = text
                .split_once("\r\n\r\n")
                .and_then(|(_, rest)| {
                    let start = rest.find('{')?;
                    serde_json::from_str(
                        &rest[start..].trim_end_matches("\r\n0\r\n\r\n"),
                    )
                    .ok()
                })
                .unwrap_or_default();
            seen.push((path, body_json));
            socket.write_all(payload.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
            let _ = socket.shutdown().await;
        }
        seen
    });
    (addr, handle)
}

/// The opencode-go free-model failure shape as an SSE stream: a 200 whose
/// ONLY event is a terminal error-valued finish_reason with no content.
/// With the ordered route-target policy, the gateway must fail the attempt
/// (first-event probe), retry the same target per the taxonomy, then WALK
/// the list to the healthy second target — the agent ends up with the good
/// stream and a migration row records the walk.
#[tokio::test]
async fn in_band_stream_error_walks_route_targets() {
    let bad_payload = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
        data: {\"choices\":[{\"index\":0,\"finish_reason\":\"network_error\",\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n\
        data: [DONE]\n\n";
    let good_payload = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
        data: {\"choices\":[{\"index\":0,\"finish_reason\":null,\"delta\":{\"role\":\"assistant\",\"content\":\"hi from good\"}}]}\n\n\
        data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\n\
        data: [DONE]\n\n";
    // 3 attempts land on the bad target (initial + 2 same-endpoint retries),
    // then the migration walks to the good target.
    let (bad_addr, bad_upstream) = mock_upstream_n(bad_payload.to_string(), 3).await;
    let (good_addr, good_upstream) = mock_upstream_n(good_payload.to_string(), 1).await;

    let conn = {
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
        for (id, addr, model) in [
            ("ep-bad", bad_addr, "m-bad"),
            ("ep-good", good_addr, "m-good"),
        ] {
            conn.execute(
                "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status, models_json)
                 VALUES (?1,'custom','test',1,'valid',?2)",
                rusqlite::params![id, format!(r#"{{"available":["{model}"],"default":"{model}"}}"#)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url) VALUES (?1,'openai-comp',?2)",
                rusqlite::params![id, format!("http://{addr}")],
            )
            .unwrap();
        }
        seed_star_policy_rows(
            &conn,
            "opencode-desktop",
            &[("ep-bad", "m-bad"), ("ep-good", "m-good")],
        );
        crate::orchestration::capability_registry::rebuild(&conn).unwrap();
        conn
    };
    let state = state_for(conn);

    let body = br#"{"model":"nestra","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;
    let resp = super::protocol_openai::handle_bytes(
        headers(),
        Bytes::from_static(body),
        state.clone(),
        "opencode-desktop",
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), hyper::StatusCode::OK);

    use http_body_util::BodyExt as _;
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("hi from good"), "agent must get the healthy target's stream: {text}");

    // The bad target absorbed the initial attempt + 2 same-endpoint retries.
    let bad_seen = bad_upstream.await.unwrap();
    assert_eq!(bad_seen.len(), 3);
    assert!(bad_seen.iter().all(|(_, b)| b["model"] == "m-bad"));
    // The good target served the migrated request.
    let good_seen = good_upstream.await.unwrap();
    assert_eq!(good_seen.len(), 1);
    assert_eq!(good_seen[0].1["model"], "m-good");

    // Observability: 4 route_request rows (3 bad + 1 good), 1 migration row.
    let db = state.db.lock().await;
    let rows: i64 = db
        .query_row("SELECT count(*) FROM route_request", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 4);
    let (from_ep, to_ep, reason): (String, String, String) = db
        .query_row(
            "SELECT from_endpoint_id, to_endpoint_id, reason FROM route_migration",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((from_ep.as_str(), to_ep.as_str()), ("ep-bad", "ep-good"));
    assert_eq!(reason, "retries_exhausted");
}

/// The surfaced form of the opencode-go free-model in-band error on a
/// SINGLE-target policy: the probe's synthetic 503 must reach the agent
/// with a JSON body carrying the reason — not a bare empty 503 the agent
/// can't distinguish from a gateway bug.
#[tokio::test]
async fn surfaced_in_band_error_503_carries_reason_body() {
    let bad_payload = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
        data: {\"choices\":[{\"index\":0,\"finish_reason\":\"network_error\",\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n\
        data: [DONE]\n\n";
    let (bad_addr, bad_upstream) = mock_upstream_n(bad_payload.to_string(), 1).await;

    let conn = {
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
             VALUES ('ep-bad','custom','test',1,'valid',?1)",
            rusqlite::params![r#"{"available":["m-bad"],"default":"m-bad"}"#],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url) VALUES ('ep-bad','openai-comp',?1)",
            rusqlite::params![format!("http://{bad_addr}")],
        )
        .unwrap();
        seed_star_policy_rows(&conn, "opencode-desktop", &[("ep-bad", "m-bad")]);
        crate::orchestration::capability_registry::rebuild(&conn).unwrap();
        conn
    };
    let state = state_for(conn);

    let body = br#"{"model":"nestra","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;
    let resp = super::protocol_openai::handle_bytes(
        headers(),
        Bytes::from_static(body),
        state.clone(),
        "opencode-desktop",
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), hyper::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        resp.headers().get(hyper::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        Some("application/json"),
        "the synthetic 503 must declare its JSON body"
    );
    let json = body_json(resp.into_body()).await;
    assert_eq!(json["error"]["type"], "nestra_gateway_error");
    let msg = json["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("finish_reason=network_error"),
        "the probe reason must surface to the agent: {msg}"
    );

    // Single-target fast-fail: exactly one attempt, no retry ladder.
    assert_eq!(bad_upstream.await.unwrap().len(), 1);
}

/// Seed a `*` policy with explicit ordered targets (test-local form of
/// `seed_star_policy` that takes pairs directly).
pub(super) fn seed_star_policy_rows(conn: &rusqlite::Connection, agent: &str, targets: &[(&str, &str)]) {
    let targets = serde_json::to_string(
        &targets
            .iter()
            .map(|(ep, m)| serde_json::json!({"endpoint": ep, "model": m}))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO routing_policy (agent_id, role, route_targets, migrate_on_quota,
                                    inject_cache_control, affinity_scope, updated_at)
         VALUES (?1,'*',?2,1,0,'task',1)",
        rusqlite::params![agent, targets],
    )
    .unwrap();
}


/// A REAL upstream 503 (with an error JSON body) on a TOOL-CARRYING
/// request: the buffered relay must not count the error body as
/// "generation started" (the zcode regression — opencode-go's "Endpoint is
/// unavailable" 503s surfaced straight to the agent, bypassing
/// retry/failover). Expected: 3 same-endpoint retries, then migration to
/// the healthy second target.
#[tokio::test]
async fn buffered_503_with_body_retries_then_migrates_for_tool_requests() {
    let err_payload = "HTTP/1.1 503 Service Unavailable
content-type: application/json
connection: close

{\"error\":{\"type\":\"server_error\",\"message\":\"Endpoint is unavailable.\"}}";
    let good_payload = "HTTP/1.1 200 OK
content-type: application/json
connection: close

{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"m-good\",\"content\":[{\"type\":\"text\",\"text\":\"hi from good\"}],\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}";
    let (bad_addr, bad_upstream) = mock_upstream_n(err_payload.to_string(), 3).await;
    let (good_addr, good_upstream) = mock_upstream_n(good_payload.to_string(), 1).await;

    let conn = {
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
        for (id, addr, model) in [
            ("ep-bad", bad_addr, "m-bad"),
            ("ep-good", good_addr, "m-good"),
        ] {
            conn.execute(
                "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status, models_json)
                 VALUES (?1,'custom','test',1,'valid',?2)",
                rusqlite::params![id, format!(r#"{{"available":["{model}"],"default":"{model}"}}"#)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url) VALUES (?1,'anthropic',?2)",
                rusqlite::params![id, format!("http://{addr}")],
            )
            .unwrap();
        }
        seed_star_policy_rows(
            &conn,
            "claude-code-cli",
            &[("ep-bad", "m-bad"), ("ep-good", "m-good")],
        );
        crate::orchestration::capability_registry::rebuild(&conn).unwrap();
        conn
    };
    let state = state_for(conn);

    // Tools-carrying Anthropic request (side-effect risk).
    let body = br#"{"model":"claude-haiku-4-5","max_tokens":64,"messages":[{"role":"user","content":"hi"}],"tools":[{"name":"bash","description":"run","input_schema":{"type":"object","properties":{"command":{"type":"string"}}}}]}"#;
    let resp = super::protocol_anthropic::handle_bytes(
        headers(),
        Bytes::from_static(body),
        state.clone(),
        "claude-code-cli",
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), hyper::StatusCode::OK, "failover must land on the healthy target");

    use http_body_util::BodyExt as _;
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("hi from good"), "agent must get the healthy target's reply: {text}");

    // The bad target absorbed the initial attempt + 2 same-endpoint retries;
    // the migration row records the walk to the good target.
    assert_eq!(bad_upstream.await.unwrap().len(), 3);
    assert_eq!(good_upstream.await.unwrap().len(), 1);
    let db = state.db.lock().await;
    let (from_ep, to_ep): (String, String) = db
        .query_row(
            "SELECT from_endpoint_id, to_endpoint_id FROM route_migration",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((from_ep.as_str(), to_ep.as_str()), ("ep-bad", "ep-good"));
}


/// Fast-fail: a SINGLE-target policy surfaces a tool-carrying request's
/// upstream 503 IMMEDIATELY — no retry ladder (there is nowhere to migrate,
/// so retries only add latency before the same error). One attempt row.
#[tokio::test]
async fn single_target_policy_fails_fast_without_retry_ladder() {
    let err_payload = "HTTP/1.1 503 Service Unavailable
content-type: application/json
connection: close

{\"error\":{\"message\":\"Endpoint is unavailable.\"}}";
    let (bad_addr, bad_upstream) = mock_upstream_n(err_payload.to_string(), 1).await;

    let conn = {
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
             VALUES ('ep-bad','custom','test',1,'valid',?1)",
            rusqlite::params![r#"{"available":["m-bad"],"default":"m-bad"}"#],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url) VALUES ('ep-bad','anthropic',?1)",
            rusqlite::params![format!("http://{bad_addr}")],
        )
        .unwrap();
        seed_star_policy_rows(&conn, "claude-code-cli", &[("ep-bad", "m-bad")]);
        crate::orchestration::capability_registry::rebuild(&conn).unwrap();
        conn
    };
    let state = state_for(conn);

    let body = br#"{"model":"claude-haiku-4-5","max_tokens":64,"messages":[{"role":"user","content":"hi"}],"tools":[{"name":"bash","description":"run","input_schema":{"type":"object","properties":{"command":{"type":"string"}}}}]}"#;
    let resp = super::protocol_anthropic::handle_bytes(
        headers(),
        Bytes::from_static(body),
        state.clone(),
        "claude-code-cli",
    )
    .await
    .unwrap();
    // The upstream 503 surfaces AS-IS, immediately.
    assert_eq!(resp.status(), hyper::StatusCode::SERVICE_UNAVAILABLE);
    drop(body_json(resp.into_body()).await);

    // Exactly ONE attempt — the ladder was skipped (no alternative target).
    assert_eq!(bad_upstream.await.unwrap().len(), 1);
    let db = state.db.lock().await;
    let rows: i64 = db
        .query_row("SELECT count(*) FROM route_request", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1, "fast-fail must not retry a single-target policy");
}

/// A mock upstream that accepts the connection and NEVER responds: the
/// 30s headers-phase timeout fires (auto-advanced by paused tokio time),
/// the attempt fails as Timeout, and with a single-target policy the agent
/// gets the gateway 502 immediately instead of hanging forever.
#[tokio::test(start_paused = true)]
async fn silent_upstream_hits_phase_timeout_and_surfaces() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let silent = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        // Drain the request, never answer.
        let mut buf = [0u8; 4096];
        loop {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let _ = socket.shutdown().await;
    });

    let conn = {
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
             VALUES ('ep-silent','custom','test',1,'valid',?1)",
            rusqlite::params![r#"{"available":["m-1"],"default":"m-1"}"#],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url) VALUES ('ep-silent','anthropic',?1)",
            rusqlite::params![format!("http://{addr}")],
        )
        .unwrap();
        seed_star_policy_rows(&conn, "claude-code-cli", &[("ep-silent", "m-1")]);
        crate::orchestration::capability_registry::rebuild(&conn).unwrap();
        conn
    };
    let state = state_for(conn);

    let body = br#"{"model":"claude-haiku-4-5","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;
    let resp = super::protocol_anthropic::handle_bytes(
        headers(),
        Bytes::from_static(body),
        state.clone(),
        "claude-code-cli",
    )
    .await
    .unwrap();
    assert_eq!(
        resp.status(),
        hyper::StatusCode::BAD_GATEWAY,
        "silent upstream surfaces as the gateway 502 (Unreachable/timeout)"
    );
    let db = state.db.lock().await;
    let rows: i64 = db
        .query_row("SELECT count(*) FROM route_request", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1, "single-target timeout surfaces without retries");
    silent.abort();
}


/// Common fixture for the timeout e2e tests: schema + agents + ONE custom
/// endpoint at `addr` (protocol `protocol`) + a single-target `*` policy.
fn seed_single_endpoint_conn(
    agent: &str,
    ep: &str,
    addr: std::net::SocketAddr,
    model: &str,
    protocol: &str,
) -> rusqlite::Connection {
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
         VALUES (?1,'custom','test',1,'valid',?2)",
        rusqlite::params![
            ep,
            format!(r#"{{"available":["{model}"],"default":"{model}"}}"#)
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url) VALUES (?1,?2,?3)",
        rusqlite::params![ep, protocol, format!("http://{addr}")],
    )
    .unwrap();
    seed_star_policy_rows(&conn, agent, &[(ep, model)]);
    crate::orchestration::capability_registry::rebuild(&conn).unwrap();
    conn
}

/// A mock upstream that accepts one connection, drains the request, writes
/// `prefix` (partial headers+body), flushes, then NEVER sends more and NEVER
/// closes — the mid-stream stall shape.
async fn mock_stalling_upstream(prefix: String) -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 8192];
        // Drain until the request looks complete (same heuristic as
        // mock_upstream_n, single read loop).
        let mut acc = Vec::new();
        loop {
            let n = socket.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            acc.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&acc).to_string();
            let complete = text
                .split_once("\r\n\r\n")
                .map(|(head, body)| {
                    let clen = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if clen > 0 {
                        body.len() >= clen
                    } else {
                        body.contains("0\r\n\r\n")
                    }
                })
                .unwrap_or(false);
            if complete {
                break;
            }
        }
        socket.write_all(prefix.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        // Hold the connection open forever — the stall.
        let mut hold = [0u8; 64];
        loop {
            match socket.read(&mut hold).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });
    addr
}

/// Mid-stream silence timeout: an SSE stream that delivers its first healthy
/// event and then stalls must TERMINATE with a structured error event after
/// the silence window — not hang the agent forever.
///
/// Real time (not `start_paused`): paused-clock auto-advance races real-TCP
/// IO delivery during the dial phase, so the test shrinks the silence window
/// to 1s via the live tuning slot instead.
#[tokio::test]
async fn stream_stall_midway_hits_silence_timeout() {
    // 200 + SSE headers + ONE healthy chat event, then silence.
    let prefix = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
        data: {\"choices\":[{\"index\":0,\"finish_reason\":null,\"delta\":{\"role\":\"assistant\",\"content\":\"partial\"}}]}\n\n".to_string();
    let addr = mock_stalling_upstream(prefix).await;
    let state = state_for(seed_single_endpoint_conn(
        "opencode-desktop",
        "ep-stall",
        addr,
        "m-stall",
        "openai-comp",
    ));
    *state.tuning.write().unwrap() = super::tuning::GatewayTuning {
        stream_silence_timeout_secs: 1,
        ..Default::default()
    };

    let body = br#"{"model":"nestra","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
    let resp = super::protocol_openai::handle_bytes(
        headers(),
        Bytes::from_static(body),
        state.clone(),
        "opencode-desktop",
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), hyper::StatusCode::OK);

    use http_body_util::BodyExt as _;
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("stalled"),
        "the stall must terminate with a structured error event: {text}"
    );
    assert!(
        text.contains("partial"),
        "the first healthy event must have been relayed before the stall: {text}"
    );
}

/// Buffered total timeout: a NON-streaming upstream that sends headers + a
/// partial JSON body (content-length lies) and then stalls must surface as
/// the gateway 502 "interrupted" after the buffered-body window — not hang
/// forever. Real time with a 1s window (same paused-clock reasoning as the
/// silence test above).
#[tokio::test]
async fn buffered_body_stall_hits_total_timeout() {
    let prefix = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 4096\r\n\r\n\
        {\"id\":\"msg_1\",\"content\":[{\"type\":\"text\",\"text\":\"par".to_string();
    let addr = mock_stalling_upstream(prefix).await;
    let state = state_for(seed_single_endpoint_conn(
        "claude-code-cli",
        "ep-stall",
        addr,
        "m-stall",
        "anthropic",
    ));
    *state.tuning.write().unwrap() = super::tuning::GatewayTuning {
        buffered_body_timeout_secs: 1,
        ..Default::default()
    };

    let body = br#"{"model":"claude-haiku-4-5","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;
    let resp = super::protocol_anthropic::handle_bytes(
        headers(),
        Bytes::from_static(body),
        state.clone(),
        "claude-code-cli",
    )
    .await
    .unwrap();
    assert_eq!(
        resp.status(),
        hyper::StatusCode::BAD_GATEWAY,
        "a stalled buffered body must surface as the gateway 502 (interrupted)"
    );
    let db = state.db.lock().await;
    let rows: i64 = db
        .query_row("SELECT count(*) FROM route_request", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1, "single-target stall surfaces without retries");
}

/// The Responses inbound path (Codex desktop) now shares the headers-phase
/// timeout: a never-responding upstream surfaces as the gateway 502 instead
/// of hanging the Codex client forever.
#[tokio::test(start_paused = true)]
async fn responses_inbound_dial_timeout_surfaces() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let silent = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        loop {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let _ = socket.shutdown().await;
    });

    let state = state_for(seed_single_endpoint_conn(
        "codex-desktop",
        "ep-silent",
        addr,
        "m-silent",
        "openai-responses",
    ));

    let body = br#"{"model":"nestra","input":[{"role":"user","content":"hi"}],"stream":true}"#;
    let resp = super::protocol_responses::handle_bytes(
        headers(),
        Bytes::from_static(body),
        state.clone(),
        "codex-desktop",
    )
    .await
    .unwrap();
    assert_eq!(
        resp.status(),
        hyper::StatusCode::BAD_GATEWAY,
        "silent Responses upstream surfaces as the gateway 502 (dial timeout)"
    );
    silent.abort();
}


/// Regression: the upstream request MUST carry content-length, never
/// `transfer-encoding: chunked`. This was the routed-ox-alpha-free 503 root
/// cause: `GatewayBody` didn't forward `size_hint`, so hyper chunked EVERY
/// buffered upstream request; opencode-go's edge intermittently holds
/// chunked request bodies for ~60-90s and then 503s "Endpoint is
/// unavailable" — while direct clients (undici/curl, always content-length)
/// kept working, which is why it looked like a provider-side "bad window".
#[tokio::test]
async fn upstream_requests_carry_content_length_not_chunked() {
    // Echo-ish mock: serve one connection, reply 200 JSON, and RETAIN the
    // raw request bytes for the assertion.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = tokio::spawn(async move {
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
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let clen = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if clen > 0 && body.len() >= clen {
                    break;
                }
            }
        }
        let reply = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"id\":\"x\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}";
        socket.write_all(reply.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        let _ = socket.shutdown().await;
        buf
    });

    let state = state_for(seed_single_endpoint_conn(
        "claude-code-cli",
        "ep-cl",
        addr,
        "m-1",
        "anthropic",
    ));
    let body = br#"{"model":"claude-haiku-4-5","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;
    let resp = super::protocol_anthropic::handle_bytes(
        headers(),
        Bytes::from_static(body),
        state.clone(),
        "claude-code-cli",
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), hyper::StatusCode::OK);

    let raw = String::from_utf8_lossy(&seen.await.unwrap()).to_string();
    let (head, rest) = raw
        .split_once("\r\n\r\n")
        .expect("captured request must parse");
    let lower = head.to_ascii_lowercase();
    assert!(
        lower.starts_with("post /v1/messages"),
        "unexpected request line: {head}"
    );
    assert!(
        lower.contains("content-length:"),
        "upstream request must carry content-length (chunked framing is the 503 root cause): {head}"
    );
    assert!(
        !lower.contains("transfer-encoding:"),
        "upstream request must never be chunked: {head}"
    );
    let clen: usize = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse().ok())
        })
        .expect("content-length parses");
    assert_eq!(rest.len(), clen, "content-length must equal the body size");
}

// ---- Logging correlation contract (see gateway/trace.rs) ----

/// One captured event: the message, its structured fields, and the span
/// chain (innermost first) it fired inside.
struct CapturedEvent {
    message: String,
    fields: Vec<(String, String)>,
    spans: Vec<String>,
}

#[derive(Default)]
struct CaptureEvents {
    events: std::sync::Mutex<Vec<CapturedEvent>>,
}

/// Test-only layer that records every event dispatched on its thread.
struct CaptureLayer {
    capture: std::sync::Arc<CaptureEvents>,
}

impl<C> tracing_subscriber::Layer<C> for CaptureLayer
where
    C: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        ctx: tracing_subscriber::layer::Context<'_, C>,
    ) {
        let mut collector = FieldCollector(Vec::new());
        event.record(&mut collector);
        let spans = ctx
            .event_scope(event)
            .map(|scope| scope.map(|s| s.name().to_string()).collect())
            .unwrap_or_default();
        let message = collector
            .0
            .iter()
            .find(|(k, _)| k == "message")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        self.capture.events.lock().unwrap().push(CapturedEvent {
            message,
            fields: collector.0,
            spans,
        });
    }
}

struct FieldCollector(Vec<(String, String)>);

impl tracing::field::Visit for FieldCollector {
    fn record_debug(
        &mut self,
        field: &tracing::field::Field,
        value: &dyn std::fmt::Debug,
    ) {
        self.0.push((field.name().to_string(), format!("{value:?}")));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
}

/// The logging correlation contract: one successful request must emit the
/// lifecycle vocabulary (`gw.request inbound` → `gw.route` →
/// `gw.attempt outcome` → `gw.done`), and the loop-phase events must fire
/// inside the `gw_attempt` → `gw_request` span chain — the whole point of
/// the system is that any log line is attributable to a request.
#[tokio::test]
async fn lifecycle_events_carry_request_correlation() {
    let ok_payload = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n\
        {\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"hi\"}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}";
    let (addr, upstream) = mock_upstream_n(ok_payload.to_string(), 1).await;

    let conn = {
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
             VALUES ('ep-ok','custom','test',1,'valid',?1)",
            rusqlite::params![r#"{"available":["m-ok"],"default":"m-ok"}"#],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url) VALUES ('ep-ok','openai-comp',?1)",
            rusqlite::params![format!("http://{addr}")],
        )
        .unwrap();
        seed_star_policy_rows(&conn, "opencode-desktop", &[("ep-ok", "m-ok")]);
        crate::orchestration::capability_registry::rebuild(&conn).unwrap();
        conn
    };
    let state = state_for(conn);

    let capture = std::sync::Arc::new(CaptureEvents::default());
    let subscriber = {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry().with(CaptureLayer {
            capture: capture.clone(),
        })
    };
    // Current-thread runtime + thread-local default: the WHOLE request path
    // (and any spawned observability task) dispatches into the capture.
    let _guard = tracing::subscriber::set_default(subscriber);

    let body = br#"{"model":"nestra","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;
    let resp = super::protocol_openai::handle_bytes(
        headers(),
        Bytes::from_static(body),
        state.clone(),
        "opencode-desktop",
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), hyper::StatusCode::OK);
    drop(_guard);
    assert_eq!(upstream.await.unwrap().len(), 1);

    let events = capture.events.lock().unwrap();
    let find = |name: &str| {
        events
            .iter()
            .find(|e| e.message == name)
            .unwrap_or_else(|| panic!("missing milestone {name}; got {:?}", events.iter().map(|e| e.message.clone()).collect::<Vec<_>>()))
    };
    let field = |e: &CapturedEvent, k: &str| {
        e.fields
            .iter()
            .find(|(k2, _)| k2 == k)
            .map(|(_, v)| v.clone())
    };

    let inbound = find("gw.request inbound");
    assert!(
        field(inbound, "task").is_some_and(|t| !t.is_empty()),
        "inbound carries the task id"
    );
    assert_eq!(field(inbound, "wire").as_deref(), Some("openai"));

    let route = find("gw.route");
    assert_eq!(field(route, "endpoint").as_deref(), Some("ep-ok"));
    assert_eq!(field(route, "model").as_deref(), Some("m-ok"));

    let outcome = find("gw.attempt outcome");
    assert_eq!(field(outcome, "status").as_deref(), Some("200"));
    assert_eq!(
        outcome.spans,
        vec!["gw_request".to_string()],
        "the success outcome fires after the attempt future completed — it sits at the request level (its `attempt` field carries the ordinal)"
    );
    assert!(field(outcome, "duration_ms").is_some(), "success attempts log latency");

    let done = find("gw.done");
    assert_eq!(field(done, "status").as_deref(), Some("200"));
    assert_eq!(
        done.spans,
        vec!["gw_request".to_string()],
        "loop events nest in the request span"
    );

    // The attempt-level nesting is proven by an event fired INSIDE the
    // forward future: the debug wire-evidence event must carry the
    // gw_attempt → gw_request chain (the whole point — any line during an
    // attempt is attributable to it).
    let wire = events
        .iter()
        .find(|e| e.message == "gw.upstream request")
        .expect("debug wire evidence event");
    assert_eq!(
        wire.spans,
        vec!["gw_attempt".to_string(), "gw_request".to_string()],
        "attempt-phase events nest in the attempt→request span chain"
    );
}
