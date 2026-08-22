use super::*;
use http_body_util::Full;

fn collect_all(stream: AnthropicToChatStream) -> String {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let collected = http_body_util::BodyExt::collect(stream).await.unwrap();
        String::from_utf8_lossy(&collected.to_bytes()).into_owned()
    })
}

#[test]
fn text_then_message_stop_becomes_chat_chunks() {
    let upstream = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","model":"m3","role":"assistant","content":[]}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}

event: message_stop
data: {"type":"message_stop"}
"#;
    let body = Full::new(Bytes::from(upstream));
    let out = collect_all(AnthropicToChatStream::new(body));

    assert!(out.contains("\"delta\":{\"role\":\"assistant\"}"));
    assert!(out.contains("\"content\":\"Hi\""));
    assert!(out.contains("data: [DONE]"));
    assert!(out.contains("\"finish_reason\":\"stop\""));
}

#[test]
fn tool_use_stream_becomes_tool_call_deltas() {
    let upstream = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_2","model":"m3","content":[]}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"search"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":\"x\"}"}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":10}}

event: message_stop
data: {"type":"message_stop"}
"#;
    let body = Full::new(Bytes::from(upstream));
    let out = collect_all(AnthropicToChatStream::new(body));

    assert!(out.contains("\"name\":\"search\""));
    assert!(out.contains("\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\""));
    assert!(out.contains("\"finish_reason\":\"tool_calls\""));
    assert!(out.contains("\"prompt_tokens\":100"));
    assert!(out.contains("\"cached_tokens\":10"));
    assert!(out.contains("data: [DONE]"));
}

#[test]
fn upstream_error_emits_chat_error_then_done() {
    let upstream = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"boom\"}}\n\n";
    let body = Full::new(Bytes::from(upstream));
    let out = collect_all(AnthropicToChatStream::new(body));
    assert!(out.contains("\"error\":{"));
    assert!(out.contains("boom"));
    assert!(out.contains("data: [DONE]"));
}