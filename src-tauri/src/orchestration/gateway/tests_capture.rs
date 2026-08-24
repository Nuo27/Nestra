//! Manual differential-capture harness (not part of the normal suite).
//!
//! `cargo test --lib capture_upstream_request -- --ignored --nocapture`
//! stands the gateway up against a local TCP echo that DUMPS the exact
//! upstream request bytes (request line + headers + body) to
//! `target/upstream-capture.txt`, using a realistic zcode-shaped inbound
//! request (tool-carrying, streaming, Claude-Code-style SDK headers). The
//! capture is then replayed verbatim against the real upstream by
//! `scripts/replay-capture.cjs` to bisect which byte difference (header or
//! body shape) is behind an upstream 503 that direct requests don't hit.

use super::tests_e2e::{seed_star_policy_rows, state_for};
use bytes::Bytes;
use http::HeaderMap;

#[tokio::test]
#[ignore = "manual differential-capture harness — dumps target/upstream-capture.txt"]
async fn capture_upstream_request() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dump = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        // Read until the request looks complete (headers + content-length
        // body), then one extra beat for pipelined bytes.
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
                if body.len() >= clen {
                    break;
                }
            }
        }
        let reply = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n\
            {\"id\":\"x\",\"content\":[{\"type\":\"text\",\"text\":\"captured\"}]}";
        socket.write_all(reply.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        let _ = socket.shutdown().await;
        buf
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
             VALUES ('ep-cap','custom','test',1,'valid',?1)",
            rusqlite::params![r#"{"available":["ox-alpha-free"],"default":"ox-alpha-free"}"#],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url) VALUES ('ep-cap','openai-comp',?1)",
            rusqlite::params![format!("http://{addr}")],
        )
        .unwrap();
        seed_star_policy_rows(&conn, "zcode-desktop", &[("ep-cap", "ox-alpha-free")]);
        crate::orchestration::capability_registry::rebuild(&conn).unwrap();
        conn
    };
    let state = state_for(conn);

    // Claude-Code-style SDK headers — what a zcode desktop request carries.
    let mut h = HeaderMap::new();
    let put = |h: &mut HeaderMap, k: &str, v: &'static str| {
        h.insert(
            http::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            http::HeaderValue::from_static(v),
        );
    };
    put(&mut h, "content-type", "application/json");
    put(&mut h, "user-agent", "zcode/1.0.0 (external, cli)");
    put(&mut h, "x-app", "cli");
    put(&mut h, "anthropic-version", "2023-06-01");
    put(
        &mut h,
        "anthropic-beta",
        "claude-code-20250219,ozone-account-20250410",
    );
    put(&mut h, "x-stainless-lang", "js");
    put(&mut h, "x-stainless-package-version", "0.39.0");
    put(&mut h, "x-stainless-os", "Windows");
    put(&mut h, "x-stainless-arch", "x64");
    put(&mut h, "x-stainless-runtime", "node");
    put(&mut h, "x-stainless-runtime-version", "v22.0.0");
    put(&mut h, "accept", "application/json");
    put(&mut h, "accept-encoding", "gzip");
    put(&mut h, "x-api-key", "gw-token");

    // Tool-carrying streaming anthropic request (the zcode shape that 503s).
    let body = br#"{"model":"nestra","max_tokens":4096,"stream":true,"system":"You are a coding agent.","messages":[{"role":"user","content":"Use bash to run: echo hi"}],"tools":[{"name":"bash","description":"Run a shell command","input_schema":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}]}"#;

    let resp = super::protocol_anthropic::handle_bytes(h, Bytes::from_static(body), state, "zcode-desktop")
        .await
        .unwrap();
    let captured = dump.await.unwrap();
    let out = concat!(env!("CARGO_MANIFEST_DIR"), "/target/upstream-capture.txt");
    std::fs::write(out, &captured).expect("write capture");
    eprintln!(
        "captured {} bytes → target/upstream-capture.txt (gateway status {})",
        captured.len(),
        resp.status()
    );
}
