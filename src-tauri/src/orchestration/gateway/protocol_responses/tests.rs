use super::*;

#[test]
fn path_is_responses_matches() {
    for p in [
        "/v1/responses",
        "/responses",
        "/codex-desktop/v1/responses",
        "/codex-desktop/responses",
        "/v1/responses/",
        "/v1/responses?x=1",
    ] {
        assert!(path_is_responses(p), "{p} should match");
    }
    for p in [
        "/v1/messages",
        "/v1/chat/completions",
        "/codex-desktop/v1/models",
        "/respon ses",
    ] {
        assert!(!path_is_responses(p), "{p} should not match");
    }
}

#[test]
fn rewrite_model_replaces_field() {
    let body = br#"{"model":"gpt-5.3-codex","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],"stream":true}"#;
    let out = rewrite_model(body, "glm-5.3");
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["model"], "glm-5.3");
    // Rest of the body preserved.
    assert_eq!(v["stream"], true);
    assert!(v["input"].as_array().unwrap().len() == 1);
}

#[test]
fn responses_to_chat_request_maps_codex_shape() {
    let body = br#"{
        "model": "gpt-5.3-codex",
        "instructions": "you are codex",
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"run ls"}]},
            {"type":"function_call","call_id":"c1","name":"shell","arguments":"{\"cmd\":\"ls\"}"},
            {"type":"function_call_output","call_id":"c1","output":"files"}
        ],
        "tools": [{"type":"function","name":"shell","description":"run","parameters":{"type":"object"}}],
        "max_output_tokens": 1024,
        "reasoning": {"effort": "medium"},
        "stream": true
    }"#;
    let out = super::super::convert_responses::responses_to_chat_request(body);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["model"], "gpt-5.3-codex");
    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "you are codex");
    assert_eq!(msgs[1]["role"], "user");
    assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "shell");
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["tool_call_id"], "c1");
    assert_eq!(v["tools"][0]["function"]["name"], "shell");
    assert_eq!(v["max_tokens"], 1024);
    assert_eq!(v["reasoning_effort"], "medium");
    assert_eq!(v["stream"], true);
    // Responses-only knobs dropped.
    assert!(v.get("instructions").is_none());
    assert!(v.get("input").is_none());
    assert!(v.get("reasoning").is_none());
}

#[test]
fn responses_to_chat_request_malformed_passthrough() {
    let body = b"not json";
    let out = super::super::convert_responses::responses_to_chat_request(body);
    assert_eq!(&out[..], body);
}

#[test]
fn chat_to_responses_response_maps_back() {
    let body = br#"{
        "id": "chatcmpl-1",
        "model": "glm-5.3",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "done",
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
    }"#;
    let out = super::super::convert_responses::chat_to_responses_response(body);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["object"], "response");
    assert_eq!(v["status"], "completed");
    let output = v["output"].as_array().unwrap();
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[0]["content"][0]["text"], "done");
    assert_eq!(output[1]["type"], "function_call");
    assert_eq!(output[1]["call_id"], "c1");
    assert_eq!(output[1]["arguments"], "{\"cmd\":\"ls\"}");
    assert_eq!(v["usage"]["input_tokens"], 10);
    assert_eq!(v["usage"]["output_tokens"], 5);
    assert_eq!(v["usage"]["total_tokens"], 15);
}

#[test]
fn chat_to_responses_response_length_is_incomplete() {
    let body = br#"{"choices":[{"message":{"role":"assistant","content":"cut"},"finish_reason":"length"}]}"#;
    let out = super::super::convert_responses::chat_to_responses_response(body);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["status"], "incomplete");
    assert_eq!(v["incomplete_details"]["reason"], "max_output_tokens");
}
