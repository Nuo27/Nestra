use super::*;

fn conv(f: impl Fn(&[u8]) -> Bytes, v: &serde_json::Value) -> serde_json::Value {
    serde_json::from_slice(&f(v.to_string().as_bytes())).unwrap_or(serde_json::Value::Null)
}

// ---- anthropic_to_responses (request) ----

#[test]
fn anthropic_request_system_messages_tools_and_choice() {
    let body = serde_json::json!({
        "model": "grok-4.5",
        "system": [{ "type": "text", "text": "You are helpful" }],
        "max_tokens": 32000,
        "temperature": 0.7,
        "stream": true,
        "stop_sequences": ["END"],
        "tools": [{
            "name": "bash",
            "description": "run a command",
            "input_schema": {
                "type": "object",
                "properties": { "cmd": { "type": "string", "format": "uri" } }
            }
        }],
        "tool_choice": { "type": "tool", "name": "bash" },
        "messages": [
            { "role": "user", "content": "hi" },
            {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "thinking" },
                    { "type": "thinking", "thinking": "plan" },
                    {
                        "type": "tool_use",
                        "id": "call_1",
                        "name": "bash",
                        "input": { "cmd": "ls" }
                    }
                ]
            },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "call_1",
                    "content": "file1"
                }]
            }
        ]
    });
    let out = conv(anthropic_to_responses, &body);
    assert_eq!(out["model"], "grok-4.5");
    assert_eq!(out["instructions"], "You are helpful");
    assert_eq!(out["max_output_tokens"], 32000);
    assert_eq!(out["temperature"], 0.7);
    assert_eq!(out["stream"], true);
    assert_eq!(out["include"], serde_json::json!(["usage"]));
    assert!(out.get("stop_sequences").is_none(), "stop_sequences dropped");
    assert!(out.get("stop").is_none());
    assert!(out.get("max_tokens").is_none());

    let tool = &out["tools"][0];
    assert_eq!(tool["type"], "function");
    assert_eq!(tool["name"], "bash");
    assert_eq!(tool["parameters"]["properties"]["cmd"].get("format"), None);
    assert_eq!(out["tool_choice"]["type"], "function");
    assert_eq!(out["tool_choice"]["name"], "bash");

    let input = out["input"].as_array().unwrap();
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"][0]["type"], "input_text");
    assert_eq!(input[1]["role"], "assistant");
    let asst_parts = input[1]["content"].as_array().unwrap();
    assert_eq!(asst_parts.len(), 1, "thinking history dropped");
    assert_eq!(asst_parts[0]["type"], "output_text");
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "call_1");
    assert_eq!(input[2]["name"], "bash");
    assert_eq!(input[2]["arguments"], r#"{"cmd":"ls"}"#);
    assert_eq!(input[3]["type"], "function_call_output");
    assert_eq!(input[3]["call_id"], "call_1");
    assert_eq!(input[3]["output"], "file1");
}

#[test]
fn anthropic_request_thinking_param_maps_to_reasoning_effort() {
    let body = serde_json::json!({
        "model": "grok-4.5",
        "thinking": { "type": "enabled", "budget_tokens": 8000 },
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let out = conv(anthropic_to_responses, &body);
    assert_eq!(out["reasoning"]["effort"], "medium");
    assert!(out.get("thinking").is_none());

    let body2 = serde_json::json!({
        "model": "grok-4.5",
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let out2 = conv(anthropic_to_responses, &body2);
    assert!(out2.get("reasoning").is_none());
}

#[test]
fn anthropic_request_image_block_becomes_input_image() {
    let body = serde_json::json!({
        "model": "grok-4.5",
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": "look" },
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "AAAA"
                    }
                }
            ]
        }]
    });
    let out = conv(anthropic_to_responses, &body);
    let parts = out["input"][0]["content"].as_array().unwrap();
    assert_eq!(parts[1]["type"], "input_image");
    assert_eq!(parts[1]["image_url"], "data:image/png;base64,AAAA");
}

// ---- chat_to_responses (request) ----

#[test]
fn chat_request_converts_messages_tools_and_usage_flag() {
    let body = serde_json::json!({
        "model": "grok-4.5",
        "messages": [
            { "role": "system", "content": "sys" },
            { "role": "user", "content": "hi" },
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "bash", "arguments": "{\"cmd\":\"ls\"}" }
                }]
            },
            { "role": "tool", "tool_call_id": "call_1", "content": "out" }
        ],
        "tools": [{
            "type": "function",
            "function": { "name": "bash", "parameters": { "type": "object" } }
        }],
        "tool_choice": { "type": "function", "function": { "name": "bash" } },
        "max_tokens": 64,
        "stop": ["END"],
        "stream": true,
        "stream_options": { "include_usage": true }
    });
    let out = conv(chat_to_responses, &body);
    assert_eq!(out["instructions"], "sys");
    assert_eq!(out["max_output_tokens"], 64);
    assert_eq!(out["include"], serde_json::json!(["usage"]));
    assert!(out.get("stop").is_none());
    assert!(out.get("stream_options").is_none());
    assert_eq!(out["tool_choice"]["type"], "function");
    assert_eq!(out["tool_choice"]["name"], "bash");

    let input = out["input"].as_array().unwrap();
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["call_id"], "call_1");
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["output"], "out");
}

// ---- responses_to_anthropic (response) ----

#[test]
fn responses_response_converts_items_status_and_usage() {
    let body = serde_json::json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "model": "grok-4.5",
        "output": [
            {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{ "type": "summary_text", "text": "let me think" }]
            },
            {
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": "ok", "annotations": [] }]
            },
            {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_9",
                "name": "bash",
                "arguments": "{\"cmd\":\"ls\"}",
                "status": "completed"
            }
        ],
        "usage": {
            "input_tokens": 228,
            "output_tokens": 10,
            "total_tokens": 238,
            "input_tokens_details": { "cached_tokens": 128 }
        }
    });
    let out = conv(responses_to_anthropic, &body);
    assert_eq!(out["type"], "message");
    assert_eq!(out["role"], "assistant");
    assert_eq!(out["id"], "resp_1");
    assert_eq!(out["stop_sequence"], serde_json::Value::Null);
    assert_eq!(out["stop_reason"], "tool_use");

    let content = out["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["thinking"], "let me think");
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[1]["text"], "ok");
    assert_eq!(content[2]["type"], "tool_use");
    assert_eq!(content[2]["id"], "call_9");
    assert_eq!(content[2]["name"], "bash");
    assert_eq!(content[2]["input"]["cmd"], "ls");

    assert_eq!(out["usage"]["input_tokens"], 100);
    assert_eq!(out["usage"]["output_tokens"], 10);
    assert_eq!(out["usage"]["cache_read_input_tokens"], 128);
}

#[test]
fn responses_response_status_maps_stop_reason() {
    let completed = serde_json::json!({
        "id": "r", "object": "response", "status": "completed",
        "output": [{ "type": "message", "content": [{ "type": "output_text", "text": "hi" }] }]
    });
    let out = conv(responses_to_anthropic, &completed);
    assert_eq!(out["stop_reason"], "end_turn");

    let incomplete = serde_json::json!({
        "id": "r", "object": "response", "status": "incomplete",
        "incomplete_details": { "reason": "max_output_tokens" },
        "output": [{ "type": "message", "content": [{ "type": "output_text", "text": "hi" }] }]
    });
    let out = conv(responses_to_anthropic, &incomplete);
    assert_eq!(out["stop_reason"], "max_tokens");
}

#[test]
fn responses_response_failed_becomes_error_envelope() {
    let failed = serde_json::json!({
        "id": "r", "object": "response", "status": "failed",
        "error": { "message": "boom", "type": "server_error" }
    });
    let out = conv(responses_to_anthropic, &failed);
    assert_eq!(out["type"], "error");
    assert_eq!(out["error"]["message"], "boom");
}

#[test]
fn responses_response_empty_summary_skips_thinking() {
    let body = serde_json::json!({
        "id": "r", "object": "response", "status": "completed",
        "output": [
            { "type": "reasoning", "id": "rs_1", "summary": [] },
            { "type": "message", "content": [{ "type": "output_text", "text": "hi" }] }
        ]
    });
    let out = conv(responses_to_anthropic, &body);
    let content = out["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "text");
}

// ---- responses_to_chat (response) ----

#[test]
fn responses_response_to_chat_completion() {
    let body = serde_json::json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "model": "grok-4.5",
        "output": [
            { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "think" }] },
            { "type": "message", "content": [{ "type": "output_text", "text": "ok" }] },
            { "type": "function_call", "call_id": "call_9", "name": "bash", "arguments": "{\"cmd\":\"ls\"}" }
        ],
        "usage": {
            "input_tokens": 228, "output_tokens": 10,
            "input_tokens_details": { "cached_tokens": 128 }
        }
    });
    let out = conv(responses_to_chat, &body);
    assert_eq!(out["object"], "chat.completion");
    assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(out["choices"][0]["message"]["content"], "ok");
    assert_eq!(out["choices"][0]["message"]["reasoning_content"], "think");
    let call = &out["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "bash");
    assert_eq!(call["function"]["arguments"], r#"{"cmd":"ls"}"#);
    assert_eq!(out["usage"]["prompt_tokens"], 228);
    assert_eq!(out["usage"]["completion_tokens"], 10);
    assert_eq!(out["usage"]["prompt_tokens_details"]["cached_tokens"], 128);
}

// ---- malformed no-op + sniffing ----

#[test]
fn malformed_inputs_pass_through_unchanged() {
    let junk = b"not json";
    for f in [
        anthropic_to_responses as fn(&[u8]) -> Bytes,
        chat_to_responses as fn(&[u8]) -> Bytes,
        responses_to_anthropic as fn(&[u8]) -> Bytes,
        responses_to_chat as fn(&[u8]) -> Bytes,
    ] {
        assert_eq!(&f(junk)[..], junk);
    }
}

#[test]
fn shape_sniffing_detects_wire_formats() {
    assert!(sniff_responses(br#"{"output":[]}"#));
    assert!(!sniff_responses(br#"{"choices":[]}"#));
    assert!(sniff_chat(br#"{"choices":[]}"#));
    assert!(!sniff_chat(br#"{"output":[]}"#));
    assert!(!sniff_responses(b"junk"));
}