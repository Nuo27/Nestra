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
/// one base_url) bound to an agent, with the default model in `models_json`.
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
    crate::orchestration::capability_registry::rebuild(&conn).unwrap();
    conn
}

fn state_for(conn: rusqlite::Connection) -> GatewayState {
    GatewayState {
        db: Arc::new(tokio::sync::Mutex::new(conn)),
        health: Arc::new(crate::orchestration::health::ProviderHealth::new()),
        quota: Arc::new(crate::orchestration::quota_state::QuotaState::new()),
        affinity: Arc::new(crate::orchestration::router::RouteAffinity::new()),
        credential_reader: Arc::new(|_| Ok(Some("test-key".into()))),
        // tests_e2e exercises the protocol handlers directly (not `dispatch`),
        // so the token is never read here; supplying one keeps the struct valid.
        loopback_token: Arc::new(tokio::sync::RwLock::new("test-token".into())),
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
