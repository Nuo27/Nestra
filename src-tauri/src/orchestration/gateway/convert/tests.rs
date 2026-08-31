use super::*;

#[test]
fn request_converts_system_messages_and_tools() {
    let body = serde_json::json!({
        "model": "deepseek-v4-flash",
        "system": "You are helpful",
        "max_tokens": 64,
        "temperature": 0.7,
        "stop_sequences": ["END"],
        "stream": true,
        "messages": [
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": [
                { "type": "text", "text": "thinking…" },
                { "type": "tool_use", "id": "toolu_1", "name": "search",
                  "input": { "q": "x" } }
            ]},
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "toolu_1", "content": "results" }
            ]}
        ],
        "tools": [
            { "name": "search", "description": "Search",
              "input_schema": { "type": "object", "properties": { "q": { "type": "string", "format": "uri" } } } }
        ],
        "tool_choice": { "type": "tool", "name": "search" }
    });
    let out: Value = serde_json::from_slice(&anthropic_to_openai(body.to_string().as_bytes())).unwrap();

    assert_eq!(out["model"], "deepseek-v4-flash");
    assert_eq!(out["max_tokens"], 64);
    assert_eq!(out["temperature"], 0.7);
    assert_eq!(out["stop"][0], "END");
    assert_eq!(out["stream_options"]["include_usage"], true);

    let msgs = out["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "You are helpful");
    assert_eq!(msgs[1]["role"], "user");
    // assistant: text content + tool_calls
    let asst = &msgs[2];
    assert_eq!(asst["role"], "assistant");
    assert_eq!(asst["content"], "thinking…");
    assert_eq!(asst["tool_calls"][0]["function"]["name"], "search");
    assert_eq!(asst["tool_calls"][0]["function"]["arguments"], r#"{"q":"x"}"#);
    // tool_result → tool message
    let tool_msg = &msgs[3];
    assert_eq!(tool_msg["role"], "tool");
    assert_eq!(tool_msg["tool_call_id"], "toolu_1");
    assert_eq!(tool_msg["content"], "results");

    // tools converted + uri format stripped
    let tools = out["tools"].as_array().unwrap();
    assert_eq!(tools[0]["function"]["name"], "search");
    assert!(tools[0]["function"]["parameters"]["properties"]["q"].get("format").is_none());
    // tool_choice any→required style mapping
    assert_eq!(
        out["tool_choice"]["function"]["name"],
        "search",
        "tool_choice: {}",
        out["tool_choice"]
    );
}

#[test]
fn request_malformed_returns_original() {
    let garbage = b"not json";
    assert_eq!(anthropic_to_openai(garbage), Bytes::copy_from_slice(garbage));
}

#[test]
fn response_converts_content_tools_and_usage() {
    let body = serde_json::json!({
        "id": "chatcmpl-1",
        "model": "deepseek-v4-flash",
        "choices": [{
            "finish_reason": "tool_calls",
            "message": {
                "role": "assistant",
                "reasoning_content": "let me think",
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "function": { "name": "search", "arguments": "{\"q\":\"x\"}" }
                }]
            }
        }],
        "usage": { "prompt_tokens": 120, "completion_tokens": 30,
            "prompt_tokens_details": { "cached_tokens": 40, "cache_write_tokens": 5 } }
    });
    let out: Value = serde_json::from_slice(&openai_to_anthropic(body.to_string().as_bytes())).unwrap();

    assert_eq!(out["type"], "message");
    assert_eq!(out["role"], "assistant");
    assert_eq!(out["stop_reason"], "tool_use");
    let content = out["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["thinking"], "let me think");
    assert_eq!(content[1]["type"], "tool_use");
    assert_eq!(content[1]["name"], "search");
    assert_eq!(content[1]["input"]["q"], "x");
    // usage invariant: input = prompt − cached − cache_write
    assert_eq!(out["usage"]["input_tokens"], 120 - 40 - 5);
    assert_eq!(out["usage"]["output_tokens"], 30);
    assert_eq!(out["usage"]["cache_read_input_tokens"], 40);
    assert_eq!(out["usage"]["cache_creation_input_tokens"], 5);
}

#[test]
fn response_defaults_stop_reason() {
    let body = serde_json::json!({
        "choices": [{ "finish_reason": "stop", "message": { "content": "done" } }]
    });
    let out: Value = serde_json::from_slice(&openai_to_anthropic(body.to_string().as_bytes())).unwrap();
    assert_eq!(out["stop_reason"], "end_turn");
    assert_eq!(out["content"][0]["type"], "text");
    assert_eq!(out["content"][0]["text"], "done");
}

#[test]
fn chat_request_to_anthropic_messages() {
    // ChatGPT-shape request → Anthropic Messages (tools, system, a
    // tool_result, an image), the missing bridge direction.
    let body = serde_json::json!({
        "model": "MiniMax-M3",
        "messages": [
            { "role": "system", "content": "You are helpful" },
            { "role": "user", "content": "look at this" },
            { "role": "user", "content": [
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,AA==" } }
            ]},
            { "role": "assistant", "content": "ok", "tool_calls": [
                { "id": "call_1", "type": "function", "function": { "name": "search", "arguments": "{\"q\":\"x\"}" } }
            ]},
            { "role": "tool", "tool_call_id": "call_1", "content": "results" }
        ],
        "tools": [{ "type": "function", "function": { "name": "search", "description": "Search it", "parameters": { "type": "object", "properties": { "q": { "type": "string" } } } } }],
        "tool_choice": { "type": "function", "function": { "name": "search" } },
        "stop": ["END"],
        "stream_options": { "include_usage": true }
    });
    let out: Value = serde_json::from_slice(&chat_to_anthropic(body.to_string().as_bytes())).unwrap();

    assert_eq!(out["system"], "You are helpful");
    let messages = out["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["content"][0]["type"], "image");
    assert_eq!(messages[1]["content"][0]["source"]["data"], "AA==");
    assert_eq!(messages[2]["content"][1]["type"], "tool_use");
    assert_eq!(messages[2]["content"][1]["id"], "call_1");
    assert_eq!(messages[2]["content"][1]["input"]["q"], "x");
    assert_eq!(messages[3]["content"][0]["type"], "tool_result");
    assert_eq!(messages[3]["content"][0]["tool_use_id"], "call_1");
    // tools → {name, input_schema}
    assert_eq!(out["tools"][0]["name"], "search");
    assert_eq!(out["tools"][0]["input_schema"]["properties"]["q"]["type"], "string");
    // tool_choice reverse-mapped; stop → stop_sequences; openai-only
    // stream_options stripped.
    assert_eq!(out["tool_choice"]["type"], "tool");
    assert_eq!(out["tool_choice"]["name"], "search");
    assert_eq!(out["stop_sequences"][0], "END");
    assert!(out.get("stream_options").is_none());
}

/// OpenAI's STRING tool_choice forms must become the Anthropic OBJECT form —
/// a bare string is a 422 "Input should be a valid dictionary" on
/// Anthropic-compatible upstreams (z.ai returns exactly that for
/// `"tool_choice":"auto"`). `required` maps to Anthropic's `any`.
#[test]
fn chat_request_tool_choice_strings_map_to_anthropic_objects() {
    for (input, expect_type) in [("auto", "auto"), ("none", "none"), ("required", "any")] {
        let body = serde_json::json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
            "tool_choice": input,
        });
        let out: Value =
            serde_json::from_slice(&chat_to_anthropic(body.to_string().as_bytes())).unwrap();
        assert_eq!(
            out["tool_choice"]["type"], expect_type,
            "string {input:?} must map to the anthropic object form"
        );
    }
}

#[test]
fn anthropic_response_to_chat_completion() {
    // Anthropic message response → chat completion (tool_use + usage).
    let body = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "MiniMax-M3",
        "content": [
            { "type": "text", "text": "sure" },
            { "type": "tool_use", "id": "toolu_1", "name": "search", "input": { "q": "x" } }
        ],
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 100, "output_tokens": 20, "cache_read_input_tokens": 10, "cache_creation_input_tokens": 5 }
    });
    let out: Value = serde_json::from_slice(&anthropic_to_chat(body.to_string().as_bytes())).unwrap();

    assert_eq!(out["object"], "chat.completion");
    let choice = &out["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    let message = &choice["message"];
    assert_eq!(message["role"], "assistant");
    assert_eq!(message["content"], "sure");
    assert_eq!(message["tool_calls"][0]["id"], "toolu_1");
    assert_eq!(message["tool_calls"][0]["function"]["name"], "search");
    assert_eq!(message["tool_calls"][0]["function"]["arguments"], r#"{"q":"x"}"#);
    assert_eq!(out["usage"]["prompt_tokens"], 100);
    assert_eq!(out["usage"]["completion_tokens"], 20);
    assert_eq!(out["usage"]["prompt_tokens_details"]["cached_tokens"], 10);
}