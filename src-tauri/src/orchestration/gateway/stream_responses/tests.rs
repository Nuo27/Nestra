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
fn responses_chat_stream_handles_opencode_event_shapes() {
    // EXACT event shapes captured from opencode-go's /v1/responses SSE
    // (ox-alpha-free): item events carry `item` (not `output_item`) and an
    // `output_index`; argument deltas are keyed by `output_index` (no
    // `item_id`) and the first fragment omits the opening `{`; `completed`
    // carries usage only (no output array to infer tool_use from).
    let upstream = format!(
        "{}{}{}{}{}{}",
        r_ev("response.output_item.added", r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"call_1","type":"function_call","name":"bash","call_id":"call_1","arguments":""}}"#),
        r_ev("response.function_call_arguments.delta", r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"command\":\""}"#),
        r_ev("response.function_call_arguments.delta", r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"echo"}"#),
        r_ev("response.function_call_arguments.delta", r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":" hi\""}"#),
        r_ev("response.function_call_arguments.delta", r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"}"}"#),
        r_ev("response.completed", r#"{"type":"response.completed","response":{"id":"r1","model":"ox-alpha-free","usage":{"input_tokens":236,"output_tokens":27}}}"#),
    );
    let out = collect_all(ResponsesToChatStream::new(Full::new(frame(&upstream))));

    // Tool start chunk carries id + name + type (key order is serde's).
    assert!(out.contains(r#""tool_calls":[{"#), "start chunk: {out}");
    assert!(out.contains(r#""type":"function""#), "start chunk type: {out}");
    assert!(out.contains(r#""id":"call_1""#));
    assert!(out.contains(r#""name":"bash""#), "start chunk name: {out}");
    // Argument fragments reconstruct a parseable JSON object (leading `{"`
    // restored on the first fragment).
    let args: String = out
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            v["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .map(String::from)
        })
        .collect();
    let parsed: serde_json::Value = serde_json::from_str(&args).expect("args must parse");
    assert_eq!(parsed["command"], "echo hi");
    // finish_reason reflects the tool call even without an output array.
    assert!(out.contains(r#""finish_reason":"tool_calls""#), "finish: {out}");
    assert!(out.contains(r#""prompt_tokens":236"#));
    assert!(out.contains("data: [DONE]"));
}

#[test]
fn responses_anthropic_stream_handles_opencode_event_shapes() {
    // Same opencode quirks through the Anthropic converter (zcode path).
    let upstream = format!(
        "{}{}{}",
        r_ev("response.output_item.added", r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"call_1","type":"function_call","name":"bash","call_id":"call_1","arguments":""}}"#),
        r_ev("response.function_call_arguments.delta", r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"command\":\"echo hi\"}"}"#),
        r_ev("response.completed", r#"{"type":"response.completed","response":{"id":"r1","status":"completed","usage":{"input_tokens":236,"output_tokens":27}}}"#),
    );
    let out = collect_all(ResponsesToAnthropicStream::new(Full::new(frame(&upstream))));

    assert!(out.contains(r#""type":"tool_use""#), "tool_use block: {out}");
    assert!(out.contains(r#""name":"bash""#));
    let args: String = out
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v["delta"]["partial_json"].as_str().map(String::from))
        .collect();
    let parsed: serde_json::Value = serde_json::from_str(&args).expect("args must parse");
    assert_eq!(parsed["command"], "echo hi");
    assert!(out.contains(r#""stop_reason":"tool_use""#), "stop_reason: {out}");
}

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

// ---------------------------------------------------------------------------
// ChatToResponsesStream (Codex inbound bridge)
// ---------------------------------------------------------------------------

fn chat_chunk(delta: serde_json::Value, finish: Option<&str>) -> String {
    let mut choice = serde_json::json!({
        "index": 0,
        "delta": delta,
        "finish_reason": null,
    });
    if let Some(f) = finish {
        choice["finish_reason"] = serde_json::json!(f);
    }
    serde_json::json!({
        "id": "chatcmpl-x",
        "object": "chat.completion.chunk",
        "model": "glm-5.3",
        "choices": [choice],
    })
    .to_string()
}

/// Extract `event:` names in order from an SSE stream dump.
fn event_names(sse_text: &str) -> Vec<String> {
    sse_text
        .lines()
        .filter_map(|l| l.strip_prefix("event: "))
        .map(str::to_string)
        .collect()
}

fn data_payload(sse_text: &str, event: &str) -> serde_json::Value {
    let mut current = "";
    for line in sse_text.lines() {
        if let Some(e) = line.strip_prefix("event: ") {
            current = e;
        } else if let Some(d) = line.strip_prefix("data: ") {
            if current == event {
                return serde_json::from_str(d).unwrap();
            }
        }
    }
    panic!("event {event} not found");
}

#[test]
fn chat_to_responses_stream_emits_codex_event_sequence() {
    let sse_text = collect_all(ChatToResponsesStream::new(Full::new(frame(&format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chat_chunk(serde_json::json!({"role":"assistant"}), None),
        chat_chunk(serde_json::json!({"content":"hel"}), None),
        chat_chunk(serde_json::json!({"content":"lo"}), None),
    )))));
    let names = event_names(&sse_text);
    assert_eq!(
        names,
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    // [DONE] without a finish_reason chunk finalizes as completed with the
    // full text snapshot.
    let done = data_payload(&sse_text, "response.output_text.done");
    assert_eq!(done["text"], "hello");
    let completed = data_payload(&sse_text, "response.completed");
    assert_eq!(completed["type"], "response.completed");
    assert_eq!(completed["response"]["status"], "completed");
    assert_eq!(completed["response"]["output"][0]["content"][0]["text"], "hello");
}

#[test]
fn chat_to_responses_stream_tools_and_usage() {
    let final_chunk = serde_json::json!({
        "id": "chatcmpl-x",
        "object": "chat.completion.chunk",
        "model": "glm-5.3",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3},
    })
    .to_string();
    let sse_text = collect_all(ChatToResponsesStream::new(Full::new(frame(&format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: {}\n\n",
        chat_chunk(
            serde_json::json!({"tool_calls":[{"index":0,"id":"c1","function":{"name":"shell"}}]}),
            None,
        ),
        chat_chunk(
            serde_json::json!({"tool_calls":[{"index":0,"function":{"arguments":"{\"cm"}}]}),
            None,
        ),
        chat_chunk(
            serde_json::json!({"tool_calls":[{"index":0,"function":{"arguments":"d\":\"ls\"}"}}]}),
            None,
        ),
        final_chunk,
    )))));
    let added = data_payload(&sse_text, "response.output_item.added");
    assert_eq!(added["item"]["type"], "function_call");
    assert_eq!(added["item"]["call_id"], "c1");
    assert_eq!(added["output_index"], 1);
    let completed = data_payload(&sse_text, "response.completed");
    let fc = completed["response"]["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .unwrap();
    assert_eq!(fc["arguments"], "{\"cmd\":\"ls\"}");
    assert_eq!(completed["response"]["usage"]["total_tokens"], 10);
}

#[test]
fn chat_to_responses_stream_error_becomes_failed() {
    let sse_text = collect_all(ChatToResponsesStream::new(Full::new(frame(
        "data: {\"error\":{\"message\":\"boom\"}}\n\n",
    ))));
    let failed = data_payload(&sse_text, "response.failed");
    assert_eq!(failed["response"]["status"], "failed");
    assert_eq!(failed["response"]["error"]["message"], "boom");
}


/// Strict client-state-machine replay (the codex-rs consumption model):
/// every event parses, dispatches on `type`, references an ANNOUNCED item,
/// and the sequence closes with a terminal event carrying usage — the
/// zcode-class failure (a malformed first/terminal event killing the
/// client's stream consumer) must be impossible on this wire.
#[test]
fn chat_to_responses_stream_survives_strict_client_replay() {
    let final_chunk = serde_json::json!({
        "id": "chatcmpl-x", "object": "chat.completion.chunk", "model": "glm-5.3",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3},
    })
    .to_string();
    let upstream = [
        chat_chunk(serde_json::json!({"content": "he"}), None),
        chat_chunk(
            serde_json::json!({"content": "llo", "tool_calls": [
                {"index": 0, "id": "c1", "function": {"name": "bash", "arguments": "{\"cmd\":\"ls\"}"}}
            ]}),
            None,
        ),
        final_chunk,
    ].join("

data: ");
    let sse_text = collect_all(ChatToResponsesStream::new(Full::new(frame(&format!(
        "data: {upstream}

data: [DONE]

",
    )))));
    let mut announced: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut saw_created = false;
    let mut terminal: Option<&str> = None;
    let mut events = 0usize;
    for block in sse_text.split("

") {
        let (name, data) = match block.strip_prefix("event: ") {
            Some(rest) => match rest.split_once("
data: ") {
                Some((n, d)) => (n.trim(), d.trim()),
                None => panic!("malformed frame: {block:?}"),
            },
            None => continue,
        };
        events += 1;
        assert!(terminal.is_none(), "no event may follow the terminal {terminal:?}");
        let v: serde_json::Value = serde_json::from_str(data)
            .unwrap_or_else(|e| panic!("event {name} payload must parse: {e}"));
        match name {
            "response.created" => {
                assert_eq!(v["response"]["status"], "in_progress");
                assert_eq!(v["response"]["object"], "response");
                saw_created = true;
            }
            "response.output_item.added" => {
                assert!(saw_created, "item added before response.created");
                let id = v["item"]["id"].as_str().expect("item.id").to_string();
                assert!(v["item"]["status"] == "in_progress");
                announced.insert(id);
            }
            "response.output_text.delta" | "response.function_call_arguments.delta" => {
                let id = v["item_id"].as_str().expect("delta.item_id").to_string();
                assert!(announced.contains(&id), "delta for unannounced item {id}");
                assert!(v["delta"].is_string(), "delta payload must be a string");
            }
            "response.output_text.done" => {
                assert!(v["text"].is_string(), "text done carries full text");
            }
            "response.output_item.done" => {
                let id = v["item"]["id"].as_str().expect("done.item.id").to_string();
                assert!(announced.contains(&id), "done for unannounced item {id}");
                assert_eq!(v["item"]["status"], "completed");
            }
            "response.completed" => {
                assert!(v["response"]["usage"]["input_tokens"].is_u64(), "terminal carries usage: {v}");
                assert!(v["response"]["usage"]["total_tokens"].is_u64());
                assert!(v["response"]["output"].as_array().unwrap().len() == 2);
                terminal = Some(name);
            }
            "response.failed" | "response.incomplete" => terminal = Some(name),
            other => panic!("unknown event type {other}"),
        }
    }
    assert!(saw_created, "stream must open with response.created");
    assert_eq!(terminal, Some("response.completed"), "stream must close with a terminal event");
    assert!(events >= 8, "expected the full event chain, got {events}");
}
