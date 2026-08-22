use super::*;
use http_body_util::Full;

fn frame(text: &str) -> Bytes {
    Bytes::from(text.to_string())
}

/// Two sequential chunks — drives the UTF-8 cross-frame buffering.
struct TwoChunks(Option<Bytes>, Option<Bytes>);

impl Body for TwoChunks {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        if let Some(b) = self.0.take() {
            Poll::Ready(Some(Ok(Frame::data(b))))
        } else if let Some(b) = self.1.take() {
            Poll::Ready(Some(Ok(Frame::data(b))))
        } else {
            Poll::Ready(None)
        }
    }
}

fn collect_all<B: Body<Data = Bytes, Error = std::io::Error>>(stream: B) -> String {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let collected = http_body_util::BodyExt::collect(stream).await.unwrap();
        String::from_utf8_lossy(&collected.to_bytes()).into_owned()
    })
}

/// Responses SSE frame: event + data with blank-line separator.
fn r_ev(event: &str, data: &str) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

#[test]
fn responses_stream_text_tool_reasoning_and_usage() {
    let upstream = format!(
        "{}{}{}{}{}{}{}{}",
        r_ev("response.created", r#"{"type":"response.created","response":{"id":"resp_1","model":"grok-4.5"}}"#),
        r_ev("response.output_item.added", r#"{"type":"response.output_item.added","output_item":{"id":"rs_1","type":"reasoning","summary":[]}}"#),
        r_ev("response.reasoning_summary_text.delta", r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"let me think"}"#),
        r_ev("response.reasoning_summary_text.done", r#"{"type":"response.reasoning_summary_text.done","item_id":"rs_1"}"#),
        r_ev("response.output_item.added", r#"{"type":"response.output_item.added","output_item":{"id":"msg_1","type":"message","role":"assistant","content":[]}}"#),
        r_ev("response.content_part.added", r#"{"type":"response.content_part.added","item_id":"msg_1","part":{"type":"output_text","text":""}}"#),
        r_ev("response.output_text.delta", r#"{"type":"response.output_text.delta","item_id":"msg_1","delta":"Hello"}"#),
        r_ev("response.completed", r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"message"}],"usage":{"input_tokens":228,"output_tokens":10,"input_tokens_details":{"cached_tokens":128}}}}"#),
    );
    let body = Full::new(frame(&upstream));
    let out = collect_all(ResponsesToAnthropicStream::new(body));

    assert!(out.contains("event: message_start"));
    assert!(out.contains("\"type\":\"message_start\""));
    assert!(out.contains("\"id\":\"resp_1\""));
    // thinking block from reasoning summary
    assert!(out.contains("\"type\":\"thinking\""));
    assert!(out.contains("\"type\":\"thinking_delta\""));
    assert!(out.contains("let me think"));
    // text block
    assert!(out.contains("\"type\":\"text_delta\""));
    assert!(out.contains("Hello"));
    // terminal: end_turn + usage. Anthropic's message_delta.usage accepts
    // ONLY output_tokens (input/cache fields would be rejected) — the
    // input side (228 - 128 = 100) must NOT appear in the delta.
    assert!(out.contains("event: message_delta"));
    assert!(out.contains("\"stop_reason\":\"end_turn\""));
    assert!(out.contains("\"output_tokens\":10"));
    assert!(
        !out.contains("\"input_tokens\":100"),
        "message_delta must not carry input_tokens"
    );
    assert!(out.contains("event: message_stop"));
}

#[test]
fn responses_stream_tool_call_late_arguments() {
    let upstream = format!(
        "{}{}{}{}",
        r_ev("response.output_item.added", r#"{"type":"response.output_item.added","output_item":{"id":"fc_1","type":"function_call","call_id":"call_9","name":"bash","arguments":"","status":"in_progress"}}"#),
        r_ev("response.function_call_arguments.delta", r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"cmd\":"}"#),
        r_ev("response.function_call_arguments.delta", r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"\"ls\"}"}"#),
        r_ev("response.completed", r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"function_call"}],"usage":{"input_tokens":10,"output_tokens":5}}}"#),
    );
    let body = Full::new(frame(&upstream));
    let out = collect_all(ResponsesToAnthropicStream::new(body));


    assert!(out.contains("\"type\":\"tool_use\""));
    assert!(out.contains("\"name\":\"bash\""));
    assert!(out.contains("\"id\":\"call_9\""));
    assert!(out.contains("\"type\":\"input_json_delta\""));
    assert!(out.contains("\"partial_json\":\"{\\\"cmd\\\":"));
    assert!(out.contains("\"stop_reason\":\"tool_use\""));
    assert!(out.contains("event: message_stop"));
}

#[test]
fn responses_stream_incomplete_maps_to_max_tokens() {
    let upstream = format!(
        "{}",
        r_ev("response.incomplete", r#"{"type":"response.incomplete","response":{"id":"r","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":5,"output_tokens":5}}}"#),
    );
    let body = Full::new(frame(&upstream));
    let out = collect_all(ResponsesToAnthropicStream::new(body));
    assert!(out.contains("\"stop_reason\":\"max_tokens\""));
    assert!(out.contains("event: message_stop"));
}

#[test]
fn responses_stream_failed_emits_error_event() {
    let upstream = r_ev(
        "response.failed",
        r#"{"type":"response.failed","error":{"message":"boom","type":"server_error"}}"#,
    );
    let body = Full::new(frame(&upstream));
    let out = collect_all(ResponsesToAnthropicStream::new(body));
    assert!(out.contains("event: error"));
    assert!(out.contains("nestra_upstream_error"));
    assert!(out.contains("boom"));
    // a failed stream must not present success events
    assert!(!out.contains("message_stop"));
}

#[test]
fn responses_stream_utf8_split_across_frames() {
    // "你" is 3 bytes; split the frame mid-character.
    let part1 = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"delta\":\"";
    let part2 = "\u{4f60}";
    let part3 = "\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
    let b1 = part1.as_bytes().to_vec();
    let mut b2 = part2.as_bytes().to_vec();
    let b3 = part3.as_bytes().to_vec();
    // split the 3-byte char: first 2 bytes in chunk A, last byte in B
    let split_at = 1;
    let b2a = b2.drain(..split_at).collect::<Vec<_>>();
    let b2b = b2;
    let mut chunk_a = b1.clone();
    chunk_a.extend_from_slice(&b2a);
    let mut chunk_b = b2b;
    chunk_b.extend_from_slice(&b3);

    let body = TwoChunks(Some(Bytes::from(chunk_a)), Some(Bytes::from(chunk_b)));
    let out = collect_all(ResponsesToAnthropicStream::new(body));
    // The delta text must arrive as the full "你" — no U+FFFD.
    assert!(!out.contains("\u{fffd}"));
    assert!(out.contains("\"text_delta\""));
    assert!(out.contains("\u{4f60}"));
}

// ---- ResponsesToChatStream ----

#[test]
fn responses_stream_to_chat_chunks() {
    let upstream = format!(
        "{}{}{}",
        r_ev("response.output_item.added", r#"{"type":"response.output_item.added","output_item":{"id":"msg_1","type":"message","role":"assistant","content":[]}}"#),
        r_ev("response.output_text.delta", r#"{"type":"response.output_text.delta","item_id":"msg_1","delta":"ok"}"#),
        r_ev("response.completed", r#"{"type":"response.completed","response":{"id":"r","status":"completed","output":[{"type":"message"}],"usage":{"input_tokens":228,"output_tokens":10,"input_tokens_details":{"cached_tokens":128}}}}"#),
    );
    let body = Full::new(frame(&upstream));
    let out = collect_all(ResponsesToChatStream::new(body));

    assert!(out.contains("chat.completion.chunk"));
    assert!(out.contains("\"content\":\"ok\""));
    assert!(out.contains("\"finish_reason\":\"stop\""));
    assert!(out.contains("\"prompt_tokens\":228"));
    assert!(out.contains("\"cached_tokens\":128"));
    assert!(out.contains("data: [DONE]"));
}

#[test]
fn responses_stream_to_chat_tool_call_chunks() {
    let upstream = format!(
        "{}{}{}",
        r_ev("response.output_item.added", r#"{"type":"response.output_item.added","output_item":{"id":"fc_1","type":"function_call","call_id":"call_9","name":"bash","arguments":"","status":"in_progress"}}"#),
        r_ev("response.function_call_arguments.delta", r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{}"}"#),
        r_ev("response.completed", r#"{"type":"response.completed","response":{"id":"r","status":"completed","output":[{"type":"function_call"}],"usage":{"input_tokens":1,"output_tokens":1}}}"#),
    );
    let body = Full::new(frame(&upstream));
    let out = collect_all(ResponsesToChatStream::new(body));


    assert!(out.contains("\"tool_calls\""));
    assert!(out.contains("\"name\":\"bash\""));
    assert!(out.contains("\"arguments\":\"{}\""));
    assert!(out.contains("\"finish_reason\":\"tool_calls\""));
    assert!(out.contains("data: [DONE]"));
}
