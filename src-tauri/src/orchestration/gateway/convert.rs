//! Anthropic Messages ↔ OpenAI Chat Completions conversion for the gateway.
//!
//! The gateway relays Anthropic-protocol agent requests (Claude Code) to
//! OpenAI-protocol upstreams (e.g. opencode-go's `/v1/chat/completions`)
//! when the resolved route speaks OpenAI. Both directions are pure
//! `Bytes -> Bytes` transforms modelled on `cache::inject_cache_control`:
//! parse JSON, mutate, re-serialize, and return the original bytes
//! unchanged on any malformed input — a broken conversion never fails a
//! request, it just relays the body untouched.
//!
//! Fidelity is deliberately pragmatic: fields OpenAI rejects are dropped,
//! fields only one side understands (thinking blocks, logprobs) are
//! simplified, and everything both sides share is passed through.

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use serde_json::{Map, Value};

use crate::config_writer::ProviderKind;

use super::stream::GatewayBody;
use super::stream_convert::OpenAiToAnthropicStream;

/// Convert a relayed upstream body back to the inbound agent's format.
/// Streaming SSE is rewritten frame-by-frame; a buffered JSON body is
/// converted wholesale. `success` gates the stream path — only 2xx SSE is
/// a live stream; errors pass through as-is.
///
/// The conversion pair is (inbound format, upstream wire):
///   anthropic inbound + openai upstream → openai_to_anthropic (existing)
///   anthropic inbound + responses upstream → responses_to_anthropic
///   openai inbound + responses upstream → responses_to_chat
/// Same-format pairs pass through untouched.
///
/// Buffered bodies are shape-sniffed as a fallback: upstreams sometimes
/// ignore the requested wire format and return a different one.
pub async fn convert_relay_body(
    body: GatewayBody,
    success: bool,
    inbound: ProviderKind,
    upstream: ProviderKind,
) -> GatewayBody {
    use super::convert_responses::{
        responses_to_anthropic, responses_to_chat, sniff_chat, sniff_responses,
    };
    let target: (ProviderKind, ProviderKind) = (inbound, upstream);
    match body {
        GatewayBody::Full(mut full) => {
            // Full<Bytes> is infallible; collect the (single) frame.
            let bytes = match full.frame().await {
                Some(Ok(frame)) => frame.into_data().map(|d| d.to_vec()).unwrap_or_default(),
                _ => Vec::new(),
            };
            // Error bodies pass through untouched — converting them would
            // lose the provider's error text the agent shows the user.
            if !success {
                return GatewayBody::Full(Full::new(Bytes::from(bytes)));
            }
            let converted: Bytes = match target {
                (ProviderKind::Anthropic, ProviderKind::Openai) => {
                    if sniff_responses(&bytes) {
                        responses_to_anthropic(&bytes)
                    } else {
                        openai_to_anthropic(&bytes)
                    }
                }
                (ProviderKind::Anthropic, ProviderKind::Responses) => {
                    if sniff_chat(&bytes) {
                        openai_to_anthropic(&bytes)
                    } else {
                        responses_to_anthropic(&bytes)
                    }
                }
                (ProviderKind::Openai, ProviderKind::Responses) => responses_to_chat(&bytes),
                _ => Bytes::from(bytes),
            };
            // A conversion that yields non-JSON (HTML/empty/malformed
            // upstream body) is relayed VERBATIM, not replaced with `{}` —
            // same promise as the module doc: a broken conversion passes
            // through untouched. Re-serializing here would also reorder keys.
            if serde_json::from_slice::<Value>(&converted).is_err() {
                return GatewayBody::Full(Full::new(converted));
            }
            GatewayBody::Full(Full::new(converted))
        }
        GatewayBody::Stream(stream) if success => {
            let wrapped = match target {
                (ProviderKind::Anthropic, ProviderKind::Openai) => {
                    GatewayBody::streaming(OpenAiToAnthropicStream::new(stream))
                }
                (ProviderKind::Anthropic, ProviderKind::Responses) => {
                    GatewayBody::streaming(
                        super::stream_responses::ResponsesToAnthropicStream::new(stream),
                    )
                }
                (ProviderKind::Openai, ProviderKind::Responses) => {
                    GatewayBody::streaming(super::stream_responses::ResponsesToChatStream::new(stream))
                }
                _ => GatewayBody::streaming(stream),
            };
            wrapped
        }
        other => other,
    }
}

/// Canonical JSON-serialize a value (used for tool-call arguments).
pub(super) fn canonical_json(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string())
}

/// Recursively strip schema constraints OpenAI rejects (`"format":"uri"`).
pub(super) fn clean_schema(v: &mut Value) {
    match v {
        Value::Object(map) => {
            if map.get("format").and_then(Value::as_str) == Some("uri") {
                map.remove("format");
            }
            for val in map.values_mut() {
                clean_schema(val);
            }
        }
        Value::Array(arr) => {
            for val in arr {
                clean_schema(val);
            }
        }
        _ => {}
    }
}

/// Extract text from an Anthropic `system` field (string or `{type:"text"}`
/// array parts). Empty parts are skipped.
pub(super) fn system_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Map one Anthropic content block list into OpenAI parts: text blocks
/// accumulate into `text_parts`, `tool_use` blocks become `tool_calls`
/// entries, `tool_result` blocks become `role:"tool"` messages.
fn convert_content_blocks(
    blocks: &[Value],
    text_parts: &mut Vec<String>,
    tool_calls: &mut Vec<Value>,
    tool_msgs: &mut Vec<Value>,
) {
    for block in blocks {
        let Some(kind) = block.get("type").and_then(Value::as_str) else { continue };
        match kind {
            "text" => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text_parts.push(text.to_string());
                    }
                }
            }
            "tool_use" => {
                let mut call = Map::new();
                if let Some(id) = block.get("id").and_then(Value::as_str) {
                    call.insert("id".into(), Value::String(id.into()));
                }
                call.insert("type".into(), Value::String("function".into()));
                let mut func = Map::new();
                if let Some(name) = block.get("name").and_then(Value::as_str) {
                    func.insert("name".into(), Value::String(name.into()));
                }
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                func.insert("arguments".into(), Value::String(canonical_json(&input)));
                call.insert("function".into(), Value::Object(func));
                tool_calls.push(Value::Object(call));
            }
            "tool_result" => {
                let mut msg = Map::new();
                msg.insert("role".into(), Value::String("tool".into()));
                if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                    msg.insert("tool_call_id".into(), Value::String(id.into()));
                }
                let content = match block.get("content") {
                    Some(Value::String(s)) => Value::String(s.clone()),
                    Some(Value::Array(parts)) => {
                        let text = parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n");
                        Value::String(text)
                    }
                    _ => Value::String(String::new()),
                };
                msg.insert("content".into(), content);
                tool_msgs.push(Value::Object(msg));
            }
            _ => {}
        }
    }
}

/// Anthropic Messages request → OpenAI Chat Completions request.
/// Malformed input returns the original bytes.
pub fn anthropic_to_openai(body: &[u8]) -> Bytes {
    let mut v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Bytes::copy_from_slice(body),
    };
    let obj = match v.as_object_mut() {
        Some(o) => o,
        None => return Bytes::copy_from_slice(body),
    };

    let mut messages: Vec<Value> = Vec::new();

    // `system` → leading system message.
    if let Some(sys) = obj.remove("system") {
        let text = system_text(&sys);
        if !text.is_empty() {
            let mut m = Map::new();
            m.insert("role".into(), Value::String("system".into()));
            m.insert("content".into(), Value::String(text));
            messages.push(Value::Object(m));
        }
    }

    // messages: role mapping + tool_use/tool_result splitting.
    if let Some(list) = obj.get("messages").and_then(Value::as_array).cloned() {
        for m in list {
            let Some(role) = m.get("role").and_then(Value::as_str) else { continue };
            let content = m.get("content");
            match (role, content) {
                ("user" | "assistant", Some(Value::String(s))) => {
                    let mut mm = Map::new();
                    mm.insert("role".into(), Value::String(role.into()));
                    mm.insert("content".into(), Value::String(s.clone()));
                    messages.push(Value::Object(mm));
                }
                ("user" | "assistant", Some(Value::Array(blocks))) => {
                    // Split text / tool_use / tool_result out of the block
                    // list: text → a content message, tool_use → assistant
                    // tool_calls, tool_result → a `tool` message.
                    let mut text_parts: Vec<String> = Vec::new();
                    let mut tool_calls: Vec<Value> = Vec::new();
                    let mut tool_msgs: Vec<Value> = Vec::new();
                    convert_content_blocks(blocks, &mut text_parts, &mut tool_calls, &mut tool_msgs);
                    messages.extend(tool_msgs);
                    let text = text_parts.join("\n");
                    // An assistant turn with tool_use carries BOTH its text
                    // and the tool_calls on one message; a pure-text turn is
                    // a plain content message; an empty turn is dropped.
                    if !tool_calls.is_empty() {
                        let mut mm = Map::new();
                        mm.insert("role".into(), Value::String("assistant".into()));
                        mm.insert("content".into(), Value::String(text));
                        mm.insert("tool_calls".into(), Value::Array(tool_calls));
                        messages.push(Value::Object(mm));
                    } else if !text.is_empty() {
                        let mut mm = Map::new();
                        mm.insert("role".into(), Value::String(role.into()));
                        mm.insert("content".into(), Value::String(text));
                        messages.push(Value::Object(mm));
                    }
                }
                _ => {}
            }
        }
    }
    obj.insert("messages".into(), Value::Array(messages));

    // tools: anthropic {input_schema} → openai {parameters}.
    if let Some(tools) = obj.get("tools").and_then(Value::as_array).cloned() {
        let mut out: Vec<Value> = Vec::new();
        for t in tools {
            let mut func = Map::new();
            if let Some(name) = t.get("name").and_then(Value::as_str) {
                func.insert("name".into(), Value::String(name.into()));
            }
            if let Some(desc) = t.get("description").and_then(Value::as_str) {
                func.insert("description".into(), Value::String(desc.into()));
            }
            let mut schema = t
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            clean_schema(&mut schema);
            if !schema.get("type").and_then(Value::as_str).is_some() {
                let mut o = Map::new();
                o.insert("type".into(), Value::String("object".into()));
                o.insert("properties".into(), Value::Object(Map::new()));
                schema = Value::Object(o);
            }
            func.insert("parameters".into(), schema);
            let mut tt = Map::new();
            tt.insert("type".into(), Value::String("function".into()));
            tt.insert("function".into(), Value::Object(func));
            out.push(Value::Object(tt));
        }
        obj.insert("tools".into(), Value::Array(out));
    }

    // tool_choice: anthropic → openai chat shape.
    //   any/auto/none strings → required/auto/none (OpenAI string form);
    //   {type:"tool", name} → {type:"function", function:{name}} — the outer
    //   object must NOT keep `type:"tool"` or a top-level `name` (OpenAI
    //   rejects both; the name lives inside `function`).
    if let Some(choice) = obj.get("tool_choice").cloned() {
        let mapped = match choice {
            Value::String(ref s) if s == "any" => Value::String("required".into()),
            Value::Object(mut m) if m.get("type").and_then(Value::as_str) == Some("tool") => {
                let name = m.remove("name").unwrap_or(Value::Null);
                let mut function = Map::new();
                function.insert("name".into(), name);
                let mut out = Map::new();
                out.insert("type".into(), Value::String("function".into()));
                out.insert("function".into(), Value::Object(function));
                Value::Object(out)
            }
            Value::Object(m) if m.get("type").and_then(Value::as_str) == Some("any") => {
                Value::String("required".into())
            }
            Value::Object(m) if m.get("type").and_then(Value::as_str) == Some("auto") => {
                Value::String("auto".into())
            }
            Value::Object(m) if m.get("type").and_then(Value::as_str) == Some("none") => {
                Value::String("none".into())
            }
            other => other,
        };
        obj.insert("tool_choice".into(), mapped);
    }

    // stop_sequences → stop.
    if let Some(stop) = obj.remove("stop_sequences") {
        obj.insert("stop".into(), stop);
    }
    // Fields OpenAI rejects outright — without stripping them, a request
    // carrying thinking/metadata/context_management 400s upstream. Remove
    // both the top-level keys and any nested `thinking` content blocks.
    for key in ["thinking", "metadata", "context_management"] {
        obj.remove(key);
    }
    if let Some(messages) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                content.retain(|b| b.get("type").and_then(Value::as_str) != Some("thinking"));
            }
        }
    }

    // stream_options.include_usage so the SSE stream carries usage.
    if obj.get("stream").and_then(Value::as_bool) == Some(true) {
        let opts = obj
            .entry(String::from("stream_options"))
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(m) = opts {
            m.insert(String::from("include_usage"), Value::Bool(true));
        }
    }

    Bytes::from(serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec()))
}

/// OpenAI Chat Completions response → Anthropic Messages response.
/// Malformed input returns the original bytes.
pub fn openai_to_anthropic(body: &[u8]) -> Bytes {
    let mut v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Bytes::copy_from_slice(body),
    };
    let obj = match v.as_object_mut() {
        Some(o) => o,
        None => return Bytes::copy_from_slice(body),
    };

    let mut msg = Map::new();
    msg.insert("type".into(), Value::String("message".into()));
    msg.insert("role".into(), Value::String("assistant".into()));
    if let Some(id) = obj.get("id").cloned() {
        msg.insert("id".into(), id);
    }
    if let Some(model) = obj.get("model").cloned() {
        msg.insert("model".into(), model);
    }
    msg.insert("stop_sequence".into(), Value::Null);

    let mut content: Vec<Value> = Vec::new();
    let mut finish_reason: Option<String> = None;

    if let Some(choices) = obj.get("choices").and_then(Value::as_array) {
        if let Some(first) = choices.first() {
            if let Some(reason) = first.get("finish_reason").and_then(Value::as_str) {
                finish_reason = Some(reason.to_string());
            }
            if let Some(message) = first.get("message") {
                // reasoning_content → leading thinking block.
                if let Some(r) = message.get("reasoning_content").and_then(Value::as_str) {
                    if !r.is_empty() {
                        let mut tb = Map::new();
                        tb.insert("type".into(), Value::String("thinking".into()));
                        tb.insert("thinking".into(), Value::String(r.into()));
                        content.push(Value::Object(tb));
                    }
                }
                // content (string or parts) → text blocks.
                match message.get("content") {
                    Some(Value::String(s)) if !s.is_empty() => {
                        let mut tb = Map::new();
                        tb.insert("type".into(), Value::String("text".into()));
                        tb.insert("text".into(), Value::String(s.clone()));
                        content.push(Value::Object(tb));
                    }
                    Some(Value::Array(parts)) => {
                        for p in parts {
                            if let Some(text) = p.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    let mut tb = Map::new();
                                    tb.insert("type".into(), Value::String("text".into()));
                                    tb.insert("text".into(), Value::String(text.into()));
                                    content.push(Value::Object(tb));
                                }
                            }
                        }
                    }
                    _ => {}
                }
                // tool_calls → tool_use blocks.
                if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let mut tb = Map::new();
                        tb.insert("type".into(), Value::String("tool_use".into()));
                        if let Some(id) = call.get("id").and_then(Value::as_str) {
                            tb.insert("id".into(), Value::String(id.into()));
                        }
                        if let Some(func) = call.get("function") {
                            if let Some(name) = func.get("name").and_then(Value::as_str) {
                                tb.insert("name".into(), Value::String(name.into()));
                            }
                            let input = func
                                .get("arguments")
                                .and_then(Value::as_str)
                                .and_then(|a| serde_json::from_str::<Value>(a).ok())
                                .unwrap_or(Value::Null);
                            tb.insert("input".into(), input);
                        }
                        content.push(Value::Object(tb));
                    }
                }
            }
        }
    }
    let has_tool_use = content
        .iter()
        .any(|c| c.get("type") == Some(&Value::String("tool_use".into())));
    msg.insert("content".into(), Value::Array(content));

    // finish_reason → stop_reason.
    let stop_reason = match finish_reason.as_deref() {
        Some("stop") => "end_turn",
        Some("length") => "max_tokens",
        Some("tool_calls") => "tool_use",
        Some("content_filter") => "end_turn",
        None if has_tool_use => "tool_use",
        _ => "end_turn",
    };
    msg.insert("stop_reason".into(), Value::String(stop_reason.into()));

    // usage: input = prompt − cache_read − cache_creation (saturating).
    if let Some(usage) = obj.get("usage") {
        let prompt = usage.get("prompt_tokens").and_then(Value::as_u64);
        let completion = usage.get("completion_tokens").and_then(Value::as_u64);
        let cached = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_write = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cache_write_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let mut u = Map::new();
        if let Some(p) = prompt {
            u.insert("input_tokens".into(), Value::from(p.saturating_sub(cached + cache_write)));
        }
        if let Some(c) = completion {
            u.insert("output_tokens".into(), Value::from(c));
        }
        if cached > 0 {
            u.insert("cache_read_input_tokens".into(), Value::from(cached));
        }
        if cache_write > 0 {
            u.insert("cache_creation_input_tokens".into(), Value::from(cache_write));
        }
        msg.insert("usage".into(), Value::Object(u));
    }

    Bytes::from(serde_json::to_vec(&msg).unwrap_or_else(|_| body.to_vec()))
}

#[cfg(test)]
mod tests {
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
}
