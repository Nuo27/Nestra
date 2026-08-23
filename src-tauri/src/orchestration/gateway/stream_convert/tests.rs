use super::*;
use http_body_util::Full;

fn frame(data: &str) -> Bytes {
    Bytes::from(data.to_string())
}

fn collect_all(stream: OpenAiToAnthropicStream) -> String {
    let body = http_body_util::Empty::<Bytes>::new();
    let _ = body;
    // Drive via poll in a loop using BodyExt::collect on the wrapper.
    // Simpler: wrap in Full-like helper.
    let mut out = String::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Build a BodyExt-compatible wrapper: OpenAiToAnthropicStream IS a Body.
        let collected = http_body_util::BodyExt::collect(stream).await.unwrap();
        out = String::from_utf8_lossy(&collected.to_bytes()).into_owned();
    });
    out
}

#[test]
fn text_and_tool_stream_converts_to_anthropic_events() {
    // Simulated upstream SSE: text delta → tool call (id, name, args) →
    // usage → finish → [DONE].
    let upstream = r#"data: {"id":"chatcmpl-1","model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"search","arguments":"{\"q\":"}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]},"finish_reason":"tool_calls"}]}

data: {"id":"chatcmpl-1","usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":10}}}

data: [DONE]
"#;
    // Feed as a single body chunk.
    let body = Full::new(frame(upstream));
    let stream = OpenAiToAnthropicStream::new(body);
    let out = collect_all(stream);

    assert!(out.contains("event: message_start"));
    assert!(out.contains("\"type\":\"message_start\""));
    assert!(out.contains("\"role\":\"assistant\""));
    // text block
    assert!(out.contains("\"type\":\"content_block_start\""));
    assert!(out.contains("\"type\":\"text_delta\""));
    assert!(out.contains("Hello"));
    // tool block
    assert!(out.contains("\"type\":\"tool_use\""));
    assert!(out.contains("\"name\":\"search\""));
    assert!(out.contains("\"type\":\"input_json_delta\""));
    assert!(out.contains("\"partial_json\":\"{\\\"q\\\":"));
    // terminal events
    assert!(out.contains("\"type\":\"message_delta\""));
    assert!(out.contains("\"stop_reason\":\"tool_use\""));
    assert!(out.contains("\"usage\""));
    assert!(out.contains("\"input_tokens\":90")); // 100 - 10 cached
    assert!(out.contains("\"output_tokens\":20"));
    assert!(out.contains("event: message_stop"));
}

#[test]
fn plain_text_stream_ends_with_end_turn() {
    let upstream = r#"data: {"id":"x","choices":[{"index":0,"delta":{"role":"assistant"}}]}

data: {"id":"x","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}

data: [DONE]
"#;
    let body = Full::new(frame(upstream));
    let out = collect_all(OpenAiToAnthropicStream::new(body));
    assert!(out.contains("\"stop_reason\":\"end_turn\""));
    assert!(out.contains("\"type\":\"text_delta\""));
    assert!(out.contains("hi"));
    assert!(out.contains("event: message_stop"));
}

#[test]
fn upstream_error_emits_anthropic_error_event() {
    let body = Full::new(frame("data: {\"error\":{\"message\":\"boom\"}}\n\n"));
    let out = collect_all(OpenAiToAnthropicStream::new(body));
    assert!(out.contains("event: error"));
    assert!(out.contains("nestra_upstream_error"));
}

/// Claude-Code-family strictness: `message_start` MUST carry `usage`
/// (clients read `message.usage.input_tokens` off the first event and
/// abort the stream consumer when it's missing — the zcode reconnect
/// loop), and `message_stop` MUST carry `{"type":"message_stop"}`.
#[test]
fn anthropic_stream_start_carries_usage_and_typed_stop() {
    let upstream = "data: {\"id\":\"c1\",\"model\":\"m\",\"usage\":{\"prompt_tokens\":42},\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}

        data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}

        data: [DONE]

";
    let out = collect_all(OpenAiToAnthropicStream::new(Full::new(frame(upstream))));
    // message_start carries usage (first-chunk prompt_tokens when present).
    let start = out.split("event: content_block_start").next().unwrap();
    assert!(
        start.contains("\"usage\":{\"input_tokens\":42,\"output_tokens\":1}"),
        "message_start must open with a usage object: {start}"
    );
    // message_stop carries the typed payload.
    assert!(
        out.contains("event: message_stop
data: {\"type\":\"message_stop\"}"),
        "message_stop must carry {{\"type\":\"message_stop\"}}: {}",
        out.split("event: message_delta").nth(1).unwrap_or("")
    );
}
