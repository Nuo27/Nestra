use super::*;

#[test]
fn observe_usage_chunk_extracts_message_start() {
    let chunk = r#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":42,"cache_creation_input_tokens":100,"cache_read_input_tokens":0}}}

event: message_delta
data: {"type":"message_delta","usage":{"output_tokens":128}}"#;
    let mut usage = ObservedUsage::default();
    observe_usage_chunk(chunk, &mut usage);
    assert_eq!(usage.input, Some(42));
    assert_eq!(usage.cache_creation, Some(100));
    assert_eq!(usage.cache_read, Some(0));
    assert_eq!(usage.output, Some(128));
}

#[test]
fn observe_usage_chunk_ignores_garbage() {
    let mut usage = ObservedUsage::default();
    // Malformed JSON + unknown event types must not panic or pollute.
    observe_usage_chunk("data: not json\n\nevent: ping\ndata: {}", &mut usage);
    assert_eq!(usage, ObservedUsage::default());
}

#[test]
fn merge_usage_obj_parses_openai_field_names() {
    // Chat Completions usage object — the field names differ from
    // Anthropic's; both vocabularies must map onto the same standard.
    let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
        r#"{"prompt_tokens":120,"completion_tokens":45,"total_tokens":165,
                "prompt_tokens_details":{"cached_tokens":88}}"#,
    )
    .unwrap();
    let mut usage = ObservedUsage::default();
    merge_usage_obj(&obj, &mut usage);
    assert_eq!(usage.input, Some(120), "prompt_tokens → input");
    assert_eq!(usage.output, Some(45), "completion_tokens → output");
    assert_eq!(usage.cache_read, Some(88), "cached_tokens → cache_read");
}

#[test]
fn merge_usage_obj_anthropic_wins_over_openai_names() {
    // A body carrying BOTH vocabularies prefers the Anthropic names.
    let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
        r#"{"input_tokens":42,"output_tokens":7,"prompt_tokens":999}"#,
    )
    .unwrap();
    let mut usage = ObservedUsage::default();
    merge_usage_obj(&obj, &mut usage);
    assert_eq!(usage.input, Some(42));
    assert_eq!(usage.output, Some(7));
}

#[test]
fn observe_anthropic_chunk_counts_tool_use_blocks() {
    let chunk = r#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":42}}}

event: content_block_start
data: {"type":"content_block_start","content_block":{"type":"tool_use","id":"toolu_1","name":"bash"}}

event: content_block_start
data: {"type":"content_block_start","content_block":{"type":"tool_use","id":"toolu_2","name":"read"}}

event: content_block_start
data: {"type":"content_block_start","content_block":{"type":"text"}}

event: message_delta
data: {"type":"message_delta","usage":{"output_tokens":9}}"#;
    let mut obs = StreamObservation::default();
    observe_anthropic_chunk(chunk, &mut obs);
    assert_eq!(obs.usage, ObservedUsage { input: Some(42), output: Some(9), cache_creation: None, cache_read: None });
    assert_eq!(obs.tool_call_ids.len(), 2, "two distinct tool_use ids, text blocks ignored");
    assert_eq!(obs.tool_names.get("bash"), Some(&1));
    assert_eq!(obs.tool_names.get("read"), Some(&1));
    // Same id twice (e.g. split frames re-observed) must not double-count.
    observe_anthropic_chunk(
        r#"data: {"type":"content_block_start","content_block":{"type":"tool_use","id":"toolu_1"}}"#,
        &mut obs,
    );
    assert_eq!(obs.tool_call_ids.len(), 2);
}

#[test]
fn observe_openai_chat_chunk_usage_and_tool_calls() {
    let chunk = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"bash"}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ls"}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_b","function":{"name":"read"}}]}}]}

data: {"choices":[{"delta":{}}],"usage":{"prompt_tokens":120,"completion_tokens":45,"prompt_tokens_details":{"cached_tokens":88}}}"#;
    let mut obs = StreamObservation::default();
    observe_openai_chat_chunk(chunk, &mut obs);
    assert_eq!(obs.usage.input, Some(120));
    assert_eq!(obs.usage.output, Some(45));
    assert_eq!(obs.usage.cache_read, Some(88));
    assert_eq!(obs.tool_call_ids.len(), 2, "index-keyed dedup: continuation deltas of call_a collapse");
    assert!(obs.tool_call_ids.contains("idx:0") && obs.tool_call_ids.contains("idx:1"));
    // Names ride only the first delta of each call → one count per call.
    assert_eq!(obs.tool_names.get("bash"), Some(&1));
    assert_eq!(obs.tool_names.get("read"), Some(&1));
}

#[test]
fn observe_responses_chunk_usage_and_function_calls() {
    let chunk = r#"event: response.output_item.added
data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_p1","name":"bash"}}

event: response.output_item.added
data: {"type":"response.output_item.added","item":{"type":"message","id":"msg_1"}}

event: response.completed
data: {"type":"response.completed","response":{"usage":{"input_tokens":77,"output_tokens":31,"input_tokens_details":{"cached_tokens":12}}}}"#;
    let mut obs = StreamObservation::default();
    observe_responses_chunk(chunk, &mut obs);
    assert_eq!(obs.usage.input, Some(77));
    assert_eq!(obs.usage.output, Some(31));
    assert_eq!(obs.usage.cache_read, Some(12));
    assert_eq!(obs.tool_call_ids.len(), 1, "only function_call items count");
    assert!(obs.tool_call_ids.contains("call_p1"));
    assert_eq!(obs.tool_names.get("bash"), Some(&1));
    // Garbage tolerance: malformed JSON and unknown events are ignored.
    observe_responses_chunk("data: not json\n\nevent: ping\ndata: {}", &mut obs);
    assert_eq!(obs.tool_call_ids.len(), 1);
}
// ---- first-event in-band error probe ----

use super::*;
use http_body_util::Full;

fn probe_over(bytes: &'static [u8]) -> FirstEventProbe {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(probe_first_sse_event(
        Full::new(Bytes::from_static(bytes)),
        std::time::Duration::from_secs(30),
    ))
}

#[test]
fn probe_flags_chat_network_error_terminal_chunk() {
    // The observed opencode-go failure shape: 200 + SSE whose ONLY event is
    // a terminal error-valued finish_reason on an empty delta.
    let out = probe_over(
        b"data: {\"choices\":[{\"index\":0,\"finish_reason\":\"network_error\",\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
    );
    match out {
        FirstEventProbe::InBandError { reason } => assert!(reason.contains("network_error")),
        _ => panic!("expected InBandError"),
    }
}

#[test]
fn probe_passes_healthy_first_chunk_through() {
    let out = probe_over(
        b"data: {\"choices\":[{\"index\":0,\"finish_reason\":null,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n",
    );
    match out {
        FirstEventProbe::Ok { held, .. } => {
            assert!(held.starts_with(b"data:"));
            assert!(held.ends_with(b"\n\n") || held.ends_with(b"\n"));
        }
        _ => panic!("expected Ok"),
    }
}

#[test]
fn probe_passes_benign_terminal_first_chunk() {
    // A first chunk that is ALREADY terminal but benign ("stop") with no
    // content is a legitimate empty completion — not a failure.
    let out = probe_over(
        b"data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
    );
    assert!(matches!(out, FirstEventProbe::Ok { .. }));
}

#[test]
fn probe_flags_content_bearing_error_chunk_as_ok() {
    // Content already streamed in the first event + error finish_reason:
    // generation started — the agent gets it (honesty contract).
    let out = probe_over(
        b"data: {\"choices\":[{\"index\":0,\"finish_reason\":\"network_error\",\"delta\":{\"content\":\"partial\"}}]}\n\n",
    );
    assert!(matches!(out, FirstEventProbe::Ok { .. }));
}

#[test]
fn probe_flags_error_envelope_and_responses_failed() {
    let out = probe_over(b"data: {\"error\":{\"message\":\"overloaded\"}}\n\n");
    match out {
        FirstEventProbe::InBandError { reason } => assert_eq!(reason, "overloaded"),
        _ => panic!("expected InBandError"),
    }
    let out = probe_over(
        b"event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"boom\"}}}\n\n",
    );
    match out {
        FirstEventProbe::InBandError { reason } => assert_eq!(reason, "boom"),
        _ => panic!("expected InBandError"),
    }
}

#[test]
fn probe_passes_ping_and_done_events() {
    assert!(matches!(
        probe_over(b": ping\n\ndata: [DONE]\n\n"),
        FirstEventProbe::Ok { .. }
    ));
    // Responses wire healthy first event.
    assert!(matches!(
        probe_over(
            b"event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\"}}\n\n"
        ),
        FirstEventProbe::Ok { .. }
    ));
}
