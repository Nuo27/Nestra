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
use super::stream_convert::{AnthropicToChatStream, OpenAiToAnthropicStream};

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
        chat_to_responses_response, responses_to_anthropic, responses_to_chat, sniff_chat,
        sniff_responses,
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
                (ProviderKind::Openai, ProviderKind::Anthropic) => anthropic_to_chat(&bytes),
                // Codex inbound: pivot through the chat shape (an anthropic
                // upstream converts to chat first, then on to Responses).
                (ProviderKind::Responses, ProviderKind::Openai) => {
                    chat_to_responses_response(&bytes)
                }
                (ProviderKind::Responses, ProviderKind::Anthropic) => {
                    chat_to_responses_response(&anthropic_to_chat(&bytes))
                }
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
                (ProviderKind::Openai, ProviderKind::Anthropic) => {
                    GatewayBody::streaming(AnthropicToChatStream::new(stream))
                }
                (ProviderKind::Responses, ProviderKind::Openai) => {
                    GatewayBody::streaming(super::stream_responses::ChatToResponsesStream::new(stream))
                }
                (ProviderKind::Responses, ProviderKind::Anthropic) => {
                    // anthropic SSE → chat SSE → responses SSE (chat pivot).
                    GatewayBody::streaming(super::stream_responses::ChatToResponsesStream::new(
                        AnthropicToChatStream::new(stream),
                    ))
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

/// Flatten a `tool_result` / chat tool-message `content` field (string, or
/// an array of text parts) into plain text joined by newlines; anything
/// else is the empty string.
pub(super) fn tool_content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// OpenAI Chat usage object → Anthropic usage map: input = prompt −
/// cache_read − cache_creation (saturating), cache fields only when
/// nonzero. Shared by the buffered `openai_to_anthropic` and the SSE
/// converter's final-frame usage fold.
pub(super) fn usage_anthropic_from_chat(usage: &Value) -> Map<String, Value> {
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
    let mut m = Map::new();
    if let Some(p) = prompt {
        m.insert("input_tokens".into(), Value::from(p.saturating_sub(cached + cache_write)));
    }
    if let Some(c) = completion {
        m.insert("output_tokens".into(), Value::from(c));
    }
    if cached > 0 {
        m.insert("cache_read_input_tokens".into(), Value::from(cached));
    }
    if cache_write > 0 {
        m.insert("cache_creation_input_tokens".into(), Value::from(cache_write));
    }
    m
}

/// Anthropic usage object → OpenAI Chat usage (`prompt_tokens` /
/// `completion_tokens` / `prompt_tokens_details.cached_tokens`). `None`
/// when the source carries no token counts — callers then omit `usage`.
pub(super) fn chat_usage_from_anthropic(u: &Value) -> Option<Value> {
    let mut usage = Map::new();
    if let Some(i) = u.get("input_tokens").and_then(Value::as_u64) {
        usage.insert("prompt_tokens".into(), Value::from(i));
    }
    if let Some(o) = u.get("output_tokens").and_then(Value::as_u64) {
        usage.insert("completion_tokens".into(), Value::from(o));
    }
    let cached = u.get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let cache_write = u.get("cache_creation_input_tokens").and_then(Value::as_u64).unwrap_or(0);
    if cached > 0 || cache_write > 0 {
        let mut details = Map::new();
        if cached > 0 {
            details.insert("cached_tokens".into(), Value::from(cached));
        }
        usage.insert("prompt_tokens_details".into(), Value::Object(details));
    }
    if usage.is_empty() {
        None
    } else {
        Some(Value::Object(usage))
    }
}

/// Responses API usage object → Anthropic usage (input excludes cached
/// tokens, saturating). Always returns a full object; callers that may omit
/// `usage` gate on the source field themselves.
pub(super) fn usage_anthropic_from_responses(usage: &Value) -> Value {
    let input = usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let output = usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
    let cached = usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut u = Map::new();
    u.insert("input_tokens".into(), Value::from(input.saturating_sub(cached)));
    u.insert("output_tokens".into(), Value::from(output));
    if cached > 0 {
        u.insert("cache_read_input_tokens".into(), Value::from(cached));
    }
    Value::Object(u)
}

/// OpenAI `finish_reason` → Anthropic `stop_reason`.
pub(super) fn finish_reason_to_stop_reason(r: &str) -> &'static str {
    match r {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        "content_filter" => "end_turn",
        _ => "end_turn",
    }
}

/// Anthropic `stop_reason` → OpenAI `finish_reason`.
pub(super) fn stop_reason_to_finish_reason(r: &str) -> &'static str {
    match r {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        _ => "stop",
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
                msg.insert(
                    "content".into(),
                    Value::String(tool_content_text(block.get("content"))),
                );
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
        None if has_tool_use => "tool_use",
        Some(r) => finish_reason_to_stop_reason(r),
        _ => "end_turn",
    };
    msg.insert("stop_reason".into(), Value::String(stop_reason.into()));

    // usage: input = prompt − cache_read − cache_creation (saturating).
    if let Some(usage) = obj.get("usage") {
        msg.insert("usage".into(), Value::Object(usage_anthropic_from_chat(usage)));
    }

    Bytes::from(serde_json::to_vec(&msg).unwrap_or_else(|_| body.to_vec()))
}

/// OpenAI Chat Completions request → Anthropic Messages request — the reverse
/// of `anthropic_to_openai`, bridging a chat-wire agent (opencode/pi) to an
/// endpoint whose only protocol row is Anthropic (e.g. MiniMax-M3 on
/// `…/anthropic/v1/messages`). Malformed input returns the original bytes.
pub fn chat_to_anthropic(body: &[u8]) -> Bytes {
    let mut v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Bytes::copy_from_slice(body),
    };
    let obj = match v.as_object_mut() {
        Some(o) => o,
        None => return Bytes::copy_from_slice(body),
    };

    let mut system: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();

    if let Some(list) = obj.get("messages").and_then(Value::as_array).cloned() {
        for m in list {
            let Some(role) = m.get("role").and_then(Value::as_str) else { continue };
            match role {
                // OpenAI system is a role message; Anthropic wants top-level.
                "system" => {
                    if let Some(s) = m.get("content").and_then(Value::as_str) {
                        system.push(s.to_string());
                    }
                }
                "user" | "assistant" => {
                    let mut blocks: Vec<Value> = Vec::new();
                    match m.get("content") {
                        Some(Value::String(s)) if !s.is_empty() => {
                            let mut tb = Map::new();
                            tb.insert("type".into(), Value::String("text".into()));
                            tb.insert("text".into(), Value::String(s.clone()));
                            blocks.push(Value::Object(tb));
                        }
                        Some(Value::Array(parts)) => {
                            for p in parts {
                                match p.get("type").and_then(Value::as_str) {
                                    Some("text") => {
                                        if let Some(t) = p.get("text").and_then(Value::as_str) {
                                            if !t.is_empty() {
                                                let mut tb = Map::new();
                                                tb.insert("type".into(), Value::String("text".into()));
                                                tb.insert("text".into(), Value::String(t.into()));
                                                blocks.push(Value::Object(tb));
                                            }
                                        }
                                    }
                                    Some("image_url") => {
                                        // Only data-URI images convert; an
                                        // http(s) URL can't be re-encoded here.
                                        let url = p
                                            .get("image_url")
                                            .and_then(|i| i.get("url"))
                                            .and_then(Value::as_str);
                                        if let Some((media_type, data)) = url.and_then(split_data_uri)
                                        {
                                            let mut img = Map::new();
                                            img.insert("type".into(), Value::String("image".into()));
                                            let mut source = Map::new();
                                            source.insert("type".into(), Value::String("base64".into()));
                                            source.insert("media_type".into(), Value::String(media_type.into()));
                                            source.insert("data".into(), Value::String(data.into()));
                                            img.insert("source".into(), Value::Object(source));
                                            blocks.push(Value::Object(img));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                    // assistant tool_calls ride the same message → tool_use.
                    if role == "assistant" {
                        if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
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
                                blocks.push(Value::Object(tb));
                            }
                        }
                    }
                    if !blocks.is_empty() {
                        let mut mm = Map::new();
                        mm.insert("role".into(), Value::String(role.into()));
                        mm.insert("content".into(), Value::Array(blocks));
                        messages.push(Value::Object(mm));
                    }
                }
                // OpenAI `tool` role = tool result.
                "tool" => {
                    let mut tb = Map::new();
                    tb.insert("type".into(), Value::String("tool_result".into()));
                    tb.insert("tool_use_id".into(), m.get("tool_call_id").cloned().unwrap_or(Value::Null));
                    tb.insert(
                        "content".into(),
                        Value::String(tool_content_text(m.get("content"))),
                    );
                    let mut mm = Map::new();
                    mm.insert("role".into(), Value::String("user".into()));
                    mm.insert("content".into(), Value::Array(vec![Value::Object(tb)]));
                    messages.push(Value::Object(mm));
                }
                _ => {}
            }
        }
    }
    if !system.is_empty() {
        obj.insert("system".into(), Value::String(system.join("\n")));
    }
    obj.insert("messages".into(), Value::Array(messages));

    // tools: {type:function, function:{name,description,parameters}} →
    // anthropic {name, description, input_schema}.
    if let Some(tools) = obj.get("tools").and_then(Value::as_array).cloned() {
        let mut out: Vec<Value> = Vec::new();
        for t in tools {
            let Some(func) = t.get("function") else { continue };
            let mut tf = Map::new();
            if let Some(name) = func.get("name").and_then(Value::as_str) {
                tf.insert("name".into(), Value::String(name.into()));
            }
            if let Some(desc) = func.get("description").and_then(Value::as_str) {
                tf.insert("description".into(), Value::String(desc.into()));
            }
            let mut schema = func
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            clean_schema(&mut schema);
            if !schema.get("type").and_then(Value::as_str).is_some() {
                let mut o = Map::new();
                o.insert("type".into(), Value::String("object".into()));
                o.insert("properties".into(), Value::Object(Map::new()));
                schema = Value::Object(o);
            }
            tf.insert("input_schema".into(), schema);
            out.push(Value::Object(tf));
        }
        obj.insert("tools".into(), Value::Array(out));
    }

    // tool_choice reverse map (openai → anthropic).
    if let Some(choice) = obj.get("tool_choice").cloned() {
        let mapped = match choice {
            Value::String(ref s) if s == "required" => Value::String("any".into()),
            Value::Object(mut m)
                if m.get("type").and_then(Value::as_str) == Some("function") =>
            {
                let name = m
                    .get_mut("function")
                    .and_then(|f| f.get("name"))
                    .cloned()
                    .or_else(|| m.remove("name"));
                let mut out = Map::new();
                out.insert("type".into(), Value::String("tool".into()));
                if let Some(n) = name {
                    out.insert("name".into(), n);
                }
                Value::Object(out)
            }
            other => other,
        };
        obj.insert("tool_choice".into(), mapped);
    }

    // stop → stop_sequences; strip openai-only keys Anthropic rejects.
    if let Some(stop) = obj.remove("stop") {
        obj.insert("stop_sequences".into(), stop);
    }
    for key in [
        "stream_options",
        "logprobs",
        "top_logprobs",
        "n",
        "presence_penalty",
        "frequency_penalty",
        "seed",
        "user",
        "response_format",
        "logit_bias",
    ] {
        obj.remove(key);
    }

    Bytes::from(serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec()))
}

/// Anthropic Messages response → OpenAI Chat Completions response — the
/// reverse of `openai_to_anthropic`. Malformed input returns the original
/// bytes.
pub fn anthropic_to_chat(body: &[u8]) -> Bytes {
    let mut v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Bytes::copy_from_slice(body),
    };
    let obj = match v.as_object_mut() {
        Some(o) => o,
        None => return Bytes::copy_from_slice(body),
    };

    let mut text: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut stop_reason: Option<String> = None;
    if let Some(reason) = obj.get("stop_reason").and_then(Value::as_str) {
        stop_reason = Some(stop_reason_to_finish_reason(reason).to_string());
    }

    if let Some(blocks) = obj.get("content").and_then(Value::as_array) {
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        if !t.is_empty() {
                            text.push(t.to_string());
                        }
                    }
                }
                Some("tool_use") => {
                    let mut call = Map::new();
                    call.insert("index".into(), Value::from(tool_calls.len()));
                    if let Some(id) = b.get("id").and_then(Value::as_str) {
                        call.insert("id".into(), Value::String(id.into()));
                    }
                    let mut function = Map::new();
                    if let Some(name) = b.get("name").and_then(Value::as_str) {
                        function.insert("name".into(), Value::String(name.into()));
                    }
                    let args = b
                        .get("input")
                        .and_then(|i| serde_json::to_string(i).ok())
                        .unwrap_or_else(|| "{}".into());
                    function.insert("arguments".into(), Value::String(args));
                    call.insert("function".into(), Value::Object(function));
                    tool_calls.push(Value::Object(call));
                }
                _ => {}
            }
        }
    }

    let mut message = Map::new();
    message.insert("role".into(), Value::String("assistant".into()));
    message.insert("content".into(), Value::String(text.join("\n")));
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    let mut choice = Map::new();
    choice.insert("index".into(), Value::from(0));
    choice.insert("message".into(), Value::Object(message));
    choice.insert(
        "finish_reason".into(),
        Value::String(stop_reason.unwrap_or_else(|| "stop".into())),
    );

    let usage = obj.get("usage").and_then(chat_usage_from_anthropic);

    let mut out = Map::new();
    if let Some(id) = obj.get("id").cloned() {
        out.insert("id".into(), id);
    }
    if let Some(model) = obj.get("model").cloned() {
        out.insert("model".into(), model);
    }
    out.insert("object".into(), Value::String("chat.completion".into()));
    out.insert("choices".into(), Value::Array(vec![Value::Object(choice)]));
    if let Some(usage) = usage {
        out.insert("usage".into(), usage);
    }

    Bytes::from(serde_json::to_vec(&out).unwrap_or_else(|_| body.to_vec()))
}

/// Split a `data:<media_type>;base64,<data>` URI. Anything else (http(s)
/// URLs, plain text) returns `None` — the bridge drops the part rather than
/// fabricating bytes.
fn split_data_uri(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media = meta.strip_suffix(";base64")?;
    Some((media, data))
}

#[cfg(test)]
mod tests;
