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
fn models_payload_falls_back_to_placeholder_capabilities() {
    let v = models_payload(None);
    let m = &v["data"][0];
    assert_eq!(m["id"], "nestra");
    assert_eq!(m["tool_call"], true);
    assert_eq!(m["reasoning"], true);
    assert_eq!(m["limit"]["context"], 200_000);
    assert_eq!(m["limit"]["output"], 8_192);
}

#[test]
fn models_payload_carries_real_abilities_when_resolved() {
    let a = crate::model_abilities::ModelAbilities {
        reasoning: Some(false),
        tool_call: Some(true),
        attachment: None,
        temperature: None,
        limit: Some(crate::model_abilities::ModelLimit {
            context: 1_000_000,
            output: 64_000,
            input: None,
        }),
        modalities: None,
        api: None,
        cost: None,
    };
    let v = models_payload(Some(&a));
    let m = &v["data"][0];
    assert_eq!(m["limit"]["context"], 1_000_000);
    assert_eq!(m["limit"]["output"], 64_000);
    assert_eq!(m["reasoning"], false, "honest flags, not blanket true");
    assert_eq!(m["tool_call"], true);
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
    // The `*` policy target pins the endpoint's default model explicitly.
    conn.execute(
        "INSERT INTO routing_policy (agent_id, role, route_targets, migrate_on_quota,
                                    inject_cache_control, affinity_scope, updated_at)
             VALUES ('opencode-desktop','*',?1,1,0,'task',1)",
        rusqlite::params![serde_json::to_string(&vec![serde_json::json!({
            "endpoint": "z-ai", "model": "glm-5.2"
        })])
        .unwrap()],
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
        tuning: super::super::tuning::shared_default(),
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