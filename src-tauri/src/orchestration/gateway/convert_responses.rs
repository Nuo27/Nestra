//! OpenAI Responses API (`/v1/responses`) conversion for the gateway.
//!
//! Three wire formats now coexist: Anthropic Messages, OpenAI Chat
//! Completions, and the Responses API. Every direction is a pure
//! `Bytes -> Bytes` transform modelled on `convert.rs` — parse JSON, mutate,
//! re-serialize, and return the original bytes unchanged on any malformed
//! input. A broken conversion never fails a request, it relays the body
//! untouched.
//!
//! Fidelity is deliberately pragmatic (field mapping follows the
//! cc-switch proxy's battle-tested tables):
//!   - `system` → `instructions`; `max_tokens` → `max_output_tokens`;
//!     `stop_sequences`/`stop` are dropped (Responses has no stop list).
//!   - Anthropic `tool_use`/`tool_result` blocks become top-level
//!     `function_call`/`function_call_output` input items (and back).
//!   - Anthropic thinking (request parameter → `reasoning.effort`; history
//!     blocks dropped — Responses inputs don't store reasoning chains).
//!   - Responses `reasoning.summary` → Anthropic `thinking` blocks (empty
//!     summaries produce no block); `status` → `stop_reason`; usage keeps
//!     the invariant `input_tokens = input − cache_read` (saturating).

use bytes::Bytes;
use serde_json::{Map, Value};

use super::convert::canonical_json;
use super::convert::clean_schema;

// ---------------------------------------------------------------------------
// Request direction: Anthropic Messages → Responses
// ---------------------------------------------------------------------------

/// Anthropic Messages request → Responses API request. Malformed input
/// returns the original bytes.
pub fn anthropic_to_responses(body: &[u8]) -> Bytes {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Bytes::copy_from_slice(body),
    };
    let mut obj = match v.as_object() {
        Some(o) => o.clone(),
        None => return Bytes::copy_from_slice(body),
    };

    let mut out = Map::new();
    // model passthrough.
    if let Some(model) = obj.get("model").cloned() {
        out.insert("model".into(), model);
    }
    // system → instructions (string or array-of-text, joined).
    if let Some(sys) = obj.remove("system") {
        let text = super::convert::system_text(&sys);
        if !text.is_empty() {
            out.insert("instructions".into(), Value::String(text));
        }
    }
    // max_tokens → max_output_tokens; temperature/top_p/stream pass through.
    if let Some(mt) = obj.remove("max_tokens") {
        out.insert("max_output_tokens".into(), mt);
    }
    for key in ["temperature", "top_p", "stream"] {
        if let Some(v) = obj.remove(key) {
            out.insert(key.into(), v);
        }
    }
    // stop_sequences: not expressible in Responses — dropped.
    obj.remove("stop_sequences");
    // thinking parameter → reasoning.effort (budget-tiered, simplified).
    if let Some(th) = obj.remove("thinking") {
        if th.get("type").and_then(Value::as_str) == Some("enabled") {
            let budget = th.get("budget_tokens").and_then(Value::as_u64).unwrap_or(0);
            let effort = if budget < 4000 {
                "low"
            } else if budget < 16000 {
                "medium"
            } else {
                "high"
            };
            out.insert("reasoning".into(), serde_json::json!({ "effort": effort }));
        }
    }
    // tools: Anthropic input_schema → Responses function tools.
    if let Some(tools) = obj.remove("tools").and_then(|v| v.as_array().cloned()) {
        let mapped: Vec<Value> = tools
            .iter()
            .filter_map(anthropic_tool_to_responses)
            .collect();
        if !mapped.is_empty() {
            out.insert("tools".into(), Value::Array(mapped));
        }
    }
    // tool_choice: any → required; {type:tool,name} → {type:function,name}.
    if let Some(tc) = obj.remove("tool_choice") {
        out.insert("tool_choice".into(), map_anthropic_tool_choice(&tc));
    }
    // messages → input items.
    if let Some(msgs) = obj.remove("messages").and_then(|v| v.as_array().cloned()) {
        let items = anthropic_messages_to_input(&msgs);
        if !items.is_empty() {
            out.insert("input".into(), Value::Array(items));
        }
    }
    // Streaming must ask for usage explicitly.
    if out.get("stream").and_then(Value::as_bool) == Some(true) {
        out.insert("include".into(), serde_json::json!(["usage"]));
    }

    match serde_json::to_string(&Value::Object(out)) {
        Ok(s) => Bytes::from(s),
        Err(_) => Bytes::copy_from_slice(body),
    }
}

/// Anthropic tool schema → Responses `{type:"function",...}` tool.
fn anthropic_tool_to_responses(tool: &Value) -> Option<Value> {
    let mut t = Map::new();
    t.insert("type".into(), Value::String("function".into()));
    if let Some(name) = tool.get("name").and_then(Value::as_str) {
        t.insert("name".into(), Value::String(name.into()));
    } else {
        return None;
    }
    if let Some(desc) = tool.get("description").and_then(Value::as_str) {
        if !desc.is_empty() {
            t.insert("description".into(), Value::String(desc.into()));
        }
    }
    let mut schema = tool
        .get("input_schema")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({ "type": "object" }));
    clean_schema(&mut schema);
    if schema.get("type").is_none() {
        schema["type"] = Value::String("object".into());
    }
    t.insert("parameters".into(), schema);
    Some(Value::Object(t))
}

/// Anthropic tool_choice → Responses tool_choice:
///   `"any"` → `"required"`;
///   `{type:"tool",name}` → `{type:"function",name}`;
///   `{type:"auto"}` / `{type:"none"}` → the bare strings "auto"/"none"
///     (Responses accepts only strings or `{type:"function",…}` objects —
///     the old pass-through sent Anthropic's object form verbatim, which the
///     Responses API rejects);
///   everything else verbatim.
fn map_anthropic_tool_choice(tc: &Value) -> Value {
    match tc {
        Value::String(s) if s == "any" => Value::String("required".into()),
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("tool") => {
            let mut m = Map::new();
            m.insert("type".into(), Value::String("function".into()));
            if let Some(name) = o.get("name").cloned() {
                m.insert("name".into(), name);
            }
            Value::Object(m)
        }
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("auto") => {
            Value::String("auto".into())
        }
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("none") => {
            Value::String("none".into())
        }
        _ => tc.clone(),
    }
}

/// Anthropic messages → Responses input items. `tool_use` blocks lift to
/// top-level `function_call` items, `tool_result` to `function_call_output`;
/// thinking history blocks are dropped (Responses doesn't carry them).
fn anthropic_messages_to_input(messages: &[Value]) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();
    for m in messages {
        let Some(role) = m.get("role").and_then(Value::as_str) else { continue };
        match m.get("content") {
            Some(Value::String(s)) if !s.is_empty() => {
                items.push(message_item(role, vec![text_part(role, s)]));
            }
            Some(Value::Array(blocks)) => {
                let mut parts: Vec<Value> = Vec::new();
                let mut calls: Vec<Value> = Vec::new();
                let mut results: Vec<Value> = Vec::new();
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = block.get("text").and_then(Value::as_str) {
                                if !t.is_empty() {
                                    parts.push(text_part(role, t));
                                }
                            }
                        }
                        Some("image") => {
                            if let Some(p) = image_part(block) {
                                parts.push(p);
                            }
                        }
                        Some("tool_use") => {
                            if let Some(call) = tool_use_to_function_call(block) {
                                calls.push(call);
                            }
                        }
                        Some("tool_result") => {
                            if let Some(res) = tool_result_to_function_call_output(block) {
                                results.push(res);
                            }
                        }
                        // thinking / redacted_thinking history: dropped.
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    items.push(message_item(role, parts));
                }
                items.extend(calls);
                items.extend(results);
            }
            _ => {}
        }
    }
    items
}

fn message_item(role: &str, content: Vec<Value>) -> Value {
    serde_json::json!({ "role": role, "content": content })
}

fn text_part(role: &str, text: &str) -> Value {
    let kind = if role == "assistant" { "output_text" } else { "input_text" };
    serde_json::json!({ "type": kind, "text": text })
}

/// Anthropic image block → Responses `input_image` part.
fn image_part(block: &Value) -> Option<Value> {
    let src = block.get("source")?;
    match src.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media = src.get("media_type").and_then(Value::as_str).unwrap_or("image/png");
            let data = src.get("data").and_then(Value::as_str)?;
            Some(serde_json::json!({
                "type": "input_image",
                "image_url": format!("data:{media};base64,{data}"),
            }))
        }
        Some("url") => {
            let url = src.get("url").and_then(Value::as_str)?;
            Some(serde_json::json!({ "type": "input_image", "image_url": url }))
        }
        _ => None,
    }
}

/// Anthropic `tool_use` block → Responses `function_call` input item.
fn tool_use_to_function_call(block: &Value) -> Option<Value> {
    let mut item = Map::new();
    item.insert("type".into(), Value::String("function_call".into()));
    if let Some(id) = block.get("id").and_then(Value::as_str) {
        item.insert("call_id".into(), Value::String(id.into()));
    }
    if let Some(name) = block.get("name").and_then(Value::as_str) {
        item.insert("name".into(), Value::String(name.into()));
    } else {
        return None;
    }
    let input = block.get("input").cloned().unwrap_or(Value::Null);
    item.insert("arguments".into(), Value::String(canonical_json(&input)));
    Some(Value::Object(item))
}

/// Anthropic `tool_result` block → Responses `function_call_output` item.
fn tool_result_to_function_call_output(block: &Value) -> Option<Value> {
    let mut item = Map::new();
    item.insert("type".into(), Value::String("function_call_output".into()));
    if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
        item.insert("call_id".into(), Value::String(id.into()));
    } else {
        return None;
    }
    let output = match block.get("content") {
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
    item.insert("output".into(), output);
    Some(Value::Object(item))
}

// ---------------------------------------------------------------------------
// Request direction: Chat Completions → Responses
// ---------------------------------------------------------------------------

/// OpenAI Chat Completions request → Responses API request. Malformed input
/// returns the original bytes.
pub fn chat_to_responses(body: &[u8]) -> Bytes {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Bytes::copy_from_slice(body),
    };
    let mut obj = match v.as_object() {
        Some(o) => o.clone(),
        None => return Bytes::copy_from_slice(body),
    };

    let mut out = Map::new();
    if let Some(model) = obj.get("model").cloned() {
        out.insert("model".into(), model);
    }
    // Chat messages → Responses input items; system messages merge into
    // `instructions`.
    let mut instructions: Vec<String> = Vec::new();
    let mut items: Vec<Value> = Vec::new();
    if let Some(msgs) = obj.remove("messages").and_then(|v| v.as_array().cloned()) {
        for m in msgs {
            let Some(role) = m.get("role").and_then(Value::as_str) else { continue };
            match role {
                "system" | "developer" => {
                    if let Some(t) = message_text(&m) {
                        if !t.is_empty() {
                            instructions.push(t);
                        }
                    }
                }
                "tool" => {
                    let mut item = Map::new();
                    item.insert("type".into(), Value::String("function_call_output".into()));
                    if let Some(id) = m.get("tool_call_id").and_then(Value::as_str) {
                        item.insert("call_id".into(), Value::String(id.into()));
                    }
                    let output = message_text(&m).unwrap_or_default();
                    item.insert("output".into(), Value::String(output));
                    items.push(Value::Object(item));
                }
                "user" | "assistant" => {
                    let text = message_text(&m).unwrap_or_default();
                    // image_url content parts → input_image.
                    let mut parts = Vec::new();
                    if !text.is_empty() {
                        parts.push(text_part(role, &text));
                    }
                    if let Some(Value::Array(blocks)) = m.get("content") {
                        for b in blocks {
                            if b.get("type").and_then(Value::as_str) == Some("image_url") {
                                if let Some(url) = b
                                    .get("image_url")
                                    .and_then(|u| u.get("url"))
                                    .and_then(Value::as_str)
                                {
                                    parts.push(serde_json::json!({
                                        "type": "input_image",
                                        "image_url": url,
                                    }));
                                }
                            }
                        }
                    }
                    if !parts.is_empty() {
                        items.push(message_item(role, parts));
                    }
                    // tool_calls on an assistant message → function_call items.
                    // MUST come AFTER the message item: the Responses API
                    // requires `message` before `function_call` for the same
                    // turn, and a function_call without a preceding message
                    // breaks multi-turn tool use.
                    if let Some(calls) = m.get("tool_calls").and_then(|v| v.as_array().cloned()) {
                        for call in calls {
                            if let Some(item) = chat_tool_call_to_function_call(&call) {
                                items.push(item);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if !instructions.is_empty() {
        out.insert("instructions".into(), Value::String(instructions.join("\n")));
    }
    if !items.is_empty() {
        out.insert("input".into(), Value::Array(items));
    }
    // tools → Responses function tools (already OpenAI-shaped).
    if let Some(tools) = obj.remove("tools").and_then(|v| v.as_array().cloned()) {
        let mapped: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let mut nt = Map::new();
                nt.insert("type".into(), Value::String("function".into()));
                if let Some(name) = t.get("function").and_then(|f| f.get("name")).and_then(Value::as_str) {
                    nt.insert("name".into(), Value::String(name.into()));
                } else {
                    return None;
                }
                if let Some(desc) = t.get("function").and_then(|f| f.get("description")).and_then(Value::as_str) {
                    if !desc.is_empty() {
                        nt.insert("description".into(), Value::String(desc.into()));
                    }
                }
                if let Some(params) = t.get("function").and_then(|f| f.get("parameters")).cloned() {
                    nt.insert("parameters".into(), params);
                }
                Some(Value::Object(nt))
            })
            .collect();
        if !mapped.is_empty() {
            out.insert("tools".into(), Value::Array(mapped));
        }
    }
    // tool_choice: chat function-object → responses function-object.
    if let Some(tc) = obj.remove("tool_choice") {
        out.insert("tool_choice".into(), map_chat_tool_choice(&tc));
    }
    // max_tokens/max_completion_tokens → max_output_tokens; stop dropped;
    // stream passthrough. (Newer OpenAI clients send max_completion_tokens.)
    if let Some(mt) = obj
        .remove("max_tokens")
        .or_else(|| obj.remove("max_completion_tokens"))
    {
        out.insert("max_output_tokens".into(), mt);
    }
    obj.remove("stop");
    obj.remove("stream_options");
    for key in ["temperature", "top_p", "stream"] {
        if let Some(v) = obj.remove(key) {
            out.insert(key.into(), v);
        }
    }
    if out.get("stream").and_then(Value::as_bool) == Some(true) {
        out.insert("include".into(), serde_json::json!(["usage"]));
    }

    match serde_json::to_string(&Value::Object(out)) {
        Ok(s) => Bytes::from(s),
        Err(_) => Bytes::copy_from_slice(body),
    }
}

fn chat_tool_call_to_function_call(call: &Value) -> Option<Value> {
    let func = call.get("function")?;
    let mut item = Map::new();
    item.insert("type".into(), Value::String("function_call".into()));
    if let Some(id) = call.get("id").and_then(Value::as_str) {
        item.insert("call_id".into(), Value::String(id.into()));
    }
    if let Some(name) = func.get("name").and_then(Value::as_str) {
        item.insert("name".into(), Value::String(name.into()));
    } else {
        return None;
    }
    let args = func.get("arguments").and_then(Value::as_str).unwrap_or("{}");
    item.insert("arguments".into(), Value::String(args.to_string()));
    Some(Value::Object(item))
}

fn map_chat_tool_choice(tc: &Value) -> Value {
    match tc {
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("function") => {
            let mut m = Map::new();
            m.insert("type".into(), Value::String("function".into()));
            if let Some(name) = o.get("function").and_then(|f| f.get("name")).cloned() {
                m.insert("name".into(), name);
            }
            Value::Object(m)
        }
        _ => tc.clone(),
    }
}

/// Plain-text content of a chat message (string or text parts joined).
fn message_text(m: &Value) -> Option<String> {
    match m.get("content") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(parts)) => {
            let text: Vec<&str> = parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect();
            if text.is_empty() {
                None
            } else {
                Some(text.join("\n"))
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Response direction: Responses → Anthropic Messages
// ---------------------------------------------------------------------------

/// Responses API response → Anthropic Messages response. Malformed input
/// returns the original bytes.
pub fn responses_to_anthropic(body: &[u8]) -> Bytes {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Bytes::copy_from_slice(body),
    };
    let obj = match v.as_object() {
        Some(o) => o,
        None => return Bytes::copy_from_slice(body),
    };

    // Terminal failure envelopes are never presented as success.
    let status = obj.get("status").and_then(Value::as_str).unwrap_or("completed");
    if obj.get("error").map(|e| !e.is_null()).unwrap_or(false)
        || status == "failed"
        || status == "cancelled"
    {
        let msg = obj
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("upstream response failed");
        return Bytes::from(
            serde_json::json!({
                "type": "error",
                "error": { "type": "nestra_upstream_error", "message": msg }
            })
            .to_string(),
        );
    }

    let mut msg = Map::new();
    msg.insert("type".into(), Value::String("message".into()));
    msg.insert("role".into(), Value::String("assistant".into()));
    msg.insert(
        "id".into(),
        obj.get("id").cloned().unwrap_or_else(|| Value::String("msg_responses".into())),
    );
    if let Some(model) = obj.get("model").cloned() {
        msg.insert("model".into(), model);
    }
    msg.insert("stop_sequence".into(), Value::Null);

    // Output items → content blocks (reasoning first, then message/function
    // calls, in item order).
    let mut content: Vec<Value> = Vec::new();
    let mut saw_tool_use = false;
    if let Some(output) = obj.get("output").and_then(|v| v.as_array().cloned()) {
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("reasoning") => {
                    // summary_text → thinking block; empty summary → skip.
                    let text: Vec<String> = item
                        .get("summary")
                        .and_then(|v| v.as_array().cloned())
                        .map(|arr| {
                            arr.iter()
                                .filter(|s| s.get("type").and_then(Value::as_str) == Some("summary_text"))
                                .filter_map(|s| s.get("text").and_then(Value::as_str).map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    let joined = text.join("\n");
                    if !joined.is_empty() {
                        content.push(serde_json::json!({ "type": "thinking", "thinking": joined }));
                    }
                }
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(|v| v.as_array().cloned()) {
                        for part in parts {
                            match part.get("type").and_then(Value::as_str) {
                                Some("output_text") | Some("refusal") => {
                                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                                        if !t.is_empty() {
                                            content.push(serde_json::json!({ "type": "text", "text": t }));
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Some("function_call") => {
                    saw_tool_use = true;
                    let mut block = Map::new();
                    block.insert("type".into(), Value::String("tool_use".into()));
                    if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                        block.insert("id".into(), Value::String(id.into()));
                    } else if let Some(id) = item.get("id").and_then(Value::as_str) {
                        block.insert("id".into(), Value::String(id.into()));
                    }
                    if let Some(name) = item.get("name").and_then(Value::as_str) {
                        block.insert("name".into(), Value::String(name.into()));
                    }
                    let input = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|a| serde_json::from_str::<Value>(a).ok())
                        .unwrap_or(Value::Null);
                    block.insert("input".into(), input);
                    content.push(Value::Object(block));
                }
                _ => {}
            }
        }
    }
    msg.insert("content".into(), Value::Array(content));

    // status → stop_reason.
    let stop_reason = match status {
        "incomplete" => {
            let reason = obj
                .get("incomplete_details")
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("max_output_tokens");
            if reason == "content_filter" {
                "end_turn"
            } else {
                "max_tokens"
            }
        }
        _ => {
            if saw_tool_use {
                "tool_use"
            } else {
                "end_turn"
            }
        }
    };
    msg.insert("stop_reason".into(), Value::String(stop_reason.into()));

    // usage: Anthropic input excludes cached tokens.
    if let Some(usage) = obj.get("usage") {
        let input = usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        let cached = usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        let mut u = Map::new();
        u.insert("input_tokens".into(), Value::from(input.saturating_sub(cached)));
        u.insert("output_tokens".into(), Value::from(output));
        if cached > 0 {
            u.insert("cache_read_input_tokens".into(), Value::from(cached));
        }
        msg.insert("usage".into(), Value::Object(u));
    }

    match serde_json::to_string(&Value::Object(msg)) {
        Ok(s) => Bytes::from(s),
        Err(_) => Bytes::copy_from_slice(body),
    }
}

// ---------------------------------------------------------------------------
// Response direction: Responses → Chat Completions
// ---------------------------------------------------------------------------

/// Responses API response → Chat Completions response. Malformed input
/// returns the original bytes.
pub fn responses_to_chat(body: &[u8]) -> Bytes {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Bytes::copy_from_slice(body),
    };
    let obj = match v.as_object() {
        Some(o) => o,
        None => return Bytes::copy_from_slice(body),
    };

    let status = obj.get("status").and_then(Value::as_str).unwrap_or("completed");
    if obj.get("error").map(|e| !e.is_null()).unwrap_or(false)
        || status == "failed"
        || status == "cancelled"
    {
        let msg = obj
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("upstream response failed");
        return Bytes::from(
            serde_json::json!({ "error": { "message": msg, "type": "nestra_upstream_error" } })
                .to_string(),
        );
    }

    let mut message = Map::new();
    message.insert("role".into(), Value::String("assistant".into()));
    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut saw_tool = false;
    if let Some(output) = obj.get("output").and_then(|v| v.as_array().cloned()) {
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("reasoning") => {
                    if let Some(summary) = item.get("summary").and_then(|v| v.as_array().cloned()) {
                        for s in summary {
                            if s.get("type").and_then(Value::as_str) == Some("summary_text") {
                                if let Some(t) = s.get("text").and_then(Value::as_str) {
                                    if !t.is_empty() {
                                        reasoning_parts.push(t.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(|v| v.as_array().cloned()) {
                        for part in parts {
                            if part.get("type").and_then(Value::as_str) == Some("output_text") {
                                if let Some(t) = part.get("text").and_then(Value::as_str) {
                                    if !t.is_empty() {
                                        text_parts.push(t.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
                Some("function_call") => {
                    saw_tool = true;
                    let mut call = Map::new();
                    if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                        call.insert("id".into(), Value::String(id.into()));
                    }
                    call.insert("type".into(), Value::String("function".into()));
                    let mut func = Map::new();
                    if let Some(name) = item.get("name").and_then(Value::as_str) {
                        func.insert("name".into(), Value::String(name.into()));
                    }
                    let args = item.get("arguments").and_then(Value::as_str).unwrap_or("{}");
                    func.insert("arguments".into(), Value::String(args.to_string()));
                    call.insert("function".into(), Value::Object(func));
                    tool_calls.push(Value::Object(call));
                }
                _ => {}
            }
        }
    }
    let text = text_parts.join("\n");
    message.insert(
        "content".into(),
        if text.is_empty() { Value::Null } else { Value::String(text) },
    );
    if !reasoning_parts.is_empty() {
        message.insert(
            "reasoning_content".into(),
            Value::String(reasoning_parts.join("\n")),
        );
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }

    let finish_reason = match status {
        "incomplete" => "length",
        _ => {
            if saw_tool {
                "tool_calls"
            } else {
                "stop"
            }
        }
    };

    let mut choice = Map::new();
    choice.insert("index".into(), Value::from(0));
    choice.insert("message".into(), Value::Object(message));
    choice.insert("finish_reason".into(), Value::String(finish_reason.into()));
    choice.insert("logprobs".into(), Value::Null);

    let mut out = Map::new();
    out.insert(
        "id".into(),
        obj.get("id").cloned().unwrap_or_else(|| Value::String("chatcmpl-nestra".into())),
    );
    out.insert("object".into(), Value::String("chat.completion".into()));
    if let Some(model) = obj.get("model").cloned() {
        out.insert("model".into(), model);
    }
    out.insert("choices".into(), Value::Array(vec![Value::Object(choice)]));
    if let Some(usage) = obj.get("usage") {
        let input = usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        let output = usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        let cached = usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let mut u = Map::new();
        u.insert("prompt_tokens".into(), Value::from(input));
        u.insert("completion_tokens".into(), Value::from(output));
        // saturating: these come from untrusted upstream JSON — an overflow
        // must clamp, not panic (debug builds panic on u64 + overflow).
        u.insert("total_tokens".into(), Value::from(input.saturating_add(output)));
        if cached > 0 {
            let mut details = Map::new();
            details.insert("cached_tokens".into(), Value::from(cached));
            u.insert("prompt_tokens_details".into(), Value::Object(details));
        }
        out.insert("usage".into(), Value::Object(u));
    }

    match serde_json::to_string(&Value::Object(out)) {
        Ok(s) => Bytes::from(s),
        Err(_) => Bytes::copy_from_slice(body),
    }
}


// ---------------------------------------------------------------------------
// Shape sniffing (upstreams may ignore the requested wire format)
// ---------------------------------------------------------------------------

/// `true` when the body looks like a Responses API payload (has an
/// `output` array). Used to route buffered conversions when the upstream
/// ignores the configured wire format.
pub fn sniff_responses(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|v| v.get("output").cloned())
        .map(|o| o.is_array())
        .unwrap_or(false)
}

/// `true` when the body looks like a Chat Completions payload (`choices`).
pub fn sniff_chat(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|v| v.get("choices").cloned())
        .map(|o| o.is_array())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Codex inbound bridge: Responses → Chat (request) and Chat → Responses
// (response). Inverses of [`chat_to_responses`] / [`responses_to_chat`] —
// the gateway bridges a Responses-speaking agent (Codex) onto chat or
// anthropic upstreams by pivoting through the chat shape.
// ---------------------------------------------------------------------------

/// Responses API request → OpenAI Chat Completions request. Malformed input
/// returns the original bytes.
pub fn responses_to_chat_request(body: &[u8]) -> Bytes {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Bytes::copy_from_slice(body),
    };
    let mut obj = match v.as_object() {
        Some(o) => o.clone(),
        None => return Bytes::copy_from_slice(body),
    };

    let mut out = Map::new();
    if let Some(model) = obj.get("model").cloned() {
        out.insert("model".into(), model);
    }
    let mut messages: Vec<Value> = Vec::new();
    // instructions → a leading system message.
    if let Some(Value::String(s)) = obj.remove("instructions") {
        if !s.is_empty() {
            messages.push(serde_json::json!({ "role": "system", "content": s }));
        }
    }
    // input items → chat messages. A bare string input is a one-user-turn
    // shorthand.
    match obj.remove("input") {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                messages.push(serde_json::json!({ "role": "user", "content": s }));
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("message") => {
                        let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                        let mut content: Vec<Value> = Vec::new();
                        if let Some(parts) = item.get("content").and_then(Value::as_array) {
                            for part in parts {
                                match part.get("type").and_then(Value::as_str) {
                                    Some("input_text") | Some("output_text")
                                    | Some("summary_text") | Some("text") => {
                                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                                            content.push(serde_json::json!({ "type": "text", "text": t }));
                                        }
                                    }
                                    Some("input_image") => {
                                        let url = part
                                            .get("image_url")
                                            .cloned()
                                            .unwrap_or_else(|| Value::String(String::new()));
                                        content.push(serde_json::json!({
                                            "type": "image_url",
                                            "image_url": { "url": url },
                                        }));
                                    }
                                    _ => {}
                                }
                            }
                        }
                        if !content.is_empty() {
                            messages.push(serde_json::json!({
                                "role": role,
                                "content": content,
                            }));
                        }
                    }
                    Some("function_call") => {
                        // Assistant tool call. Chat wants tool_calls on an
                        // assistant message; a bare call becomes its own
                        // assistant message with only tool_calls.
                        let mut call = Map::new();
                        if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                            call.insert("id".into(), Value::String(id.into()));
                        }
                        call.insert("type".into(), Value::String("function".into()));
                        let mut func = Map::new();
                        if let Some(name) = item.get("name").and_then(Value::as_str) {
                            func.insert("name".into(), Value::String(name.into()));
                        }
                        func.insert(
                            "arguments".into(),
                            Value::String(
                                item.get("arguments").and_then(Value::as_str).unwrap_or("{}").into(),
                            ),
                        );
                        call.insert("function".into(), Value::Object(func));
                        messages.push(serde_json::json!({
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [Value::Object(call)],
                        }));
                    }
                    Some("function_call_output") => {
                        messages.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                            "content": item.get("output").cloned().unwrap_or(Value::String(String::new())),
                        }));
                    }
                    // reasoning items have no chat representation — dropped.
                    _ => {}
                }
            }
        }
        _ => {}
    }
    if !messages.is_empty() {
        out.insert("messages".into(), Value::Array(messages));
    }
    // Flat Responses function tools → nested chat shape.
    if let Some(tools) = obj.remove("tools").and_then(|v| v.as_array().cloned()) {
        let mapped: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                if t.get("type").and_then(Value::as_str) != Some("function") {
                    return None;
                }
                let name = t.get("name").and_then(Value::as_str)?;
                let mut f = Map::new();
                f.insert("name".into(), Value::String(name.into()));
                if let Some(desc) = t.get("description").and_then(Value::as_str) {
                    f.insert("description".into(), Value::String(desc.into()));
                }
                if let Some(params) = t.get("parameters").cloned() {
                    f.insert("parameters".into(), params);
                }
                Some(serde_json::json!({ "type": "function", "function": Value::Object(f) }))
            })
            .collect();
        if !mapped.is_empty() {
            out.insert("tools".into(), Value::Array(mapped));
        }
    }
    if let Some(tc) = obj.remove("tool_choice") {
        out.insert("tool_choice".into(), map_responses_tool_choice(&tc));
    }
    // max_output_tokens → max_tokens; reasoning.effort → reasoning_effort.
    if let Some(mt) = obj.remove("max_output_tokens") {
        out.insert("max_tokens".into(), mt);
    }
    if let Some(effort) = obj
        .remove("reasoning")
        .and_then(|r| r.get("effort").cloned())
    {
        out.insert("reasoning_effort".into(), effort);
    }
    // Responses-only knobs without a chat equivalent.
    obj.remove("include");
    obj.remove("parallel_tool_calls");
    obj.remove("previous_response_id");
    obj.remove("store");
    for key in ["temperature", "top_p", "stream"] {
        if let Some(v) = obj.remove(key) {
            out.insert(key.into(), v);
        }
    }

    match serde_json::to_string(&Value::Object(out)) {
        Ok(s) => Bytes::from(s),
        Err(_) => Bytes::copy_from_slice(body),
    }
}

fn map_responses_tool_choice(tc: &Value) -> Value {
    match tc {
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("function") => {
            let mut m = Map::new();
            m.insert("type".into(), Value::String("function".into()));
            if let Some(name) = o.get("name").cloned() {
                let mut f = Map::new();
                f.insert("name".into(), name);
                m.insert("function".into(), Value::Object(f));
            }
            Value::Object(m)
        }
        _ => tc.clone(),
    }
}

/// OpenAI Chat Completions response → Responses API response. Malformed
/// input returns the original bytes.
pub fn chat_to_responses_response(body: &[u8]) -> Bytes {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Bytes::copy_from_slice(body),
    };
    let obj = match v.as_object() {
        Some(o) => o,
        None => return Bytes::copy_from_slice(body),
    };
    // Chat error envelope is already the Responses error shape — pass it
    // through untouched.
    if obj.get("error").map(|e| !e.is_null()).unwrap_or(false) {
        return Bytes::copy_from_slice(body);
    }

    let choice = obj
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null);
    let message = choice.get("message").cloned().unwrap_or(Value::Null);
    let finish = choice.get("finish_reason").and_then(Value::as_str).unwrap_or("stop");

    let mut output: Vec<Value> = Vec::new();
    if let Some(rc) = message.get("reasoning_content").and_then(Value::as_str) {
        if !rc.is_empty() {
            output.push(serde_json::json!({
                "type": "reasoning",
                "id": "rs_nestra",
                "summary": [{ "type": "summary_text", "text": rc }],
            }));
        }
    }
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            output.push(serde_json::json!({
                "type": "message",
                "id": "msg_nestra",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": text, "annotations": [] }],
            }));
        }
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (i, call) in calls.iter().enumerate() {
            let func = call.get("function").cloned().unwrap_or(Value::Null);
            output.push(serde_json::json!({
                "type": "function_call",
                "id": format!("fc_nestra_{i}"),
                "call_id": call.get("id").cloned().unwrap_or(Value::Null),
                "name": func.get("name").cloned().unwrap_or(Value::Null),
                "arguments": func.get("arguments").cloned().unwrap_or(serde_json::json!("{}")),
                "status": "completed",
            }));
        }
    }

    let mut resp = Map::new();
    resp.insert(
        "id".into(),
        obj.get("id").cloned().unwrap_or_else(|| Value::String("resp_nestra".into())),
    );
    resp.insert("object".into(), Value::String("response".into()));
    if let Some(model) = obj.get("model").cloned() {
        resp.insert("model".into(), model);
    }
    resp.insert("created_at".into(), Value::from(0));
    if finish == "length" {
        resp.insert("status".into(), Value::String("incomplete".into()));
        resp.insert(
            "incomplete_details".into(),
            serde_json::json!({ "reason": "max_output_tokens" }),
        );
    } else {
        resp.insert("status".into(), Value::String("completed".into()));
    }
    resp.insert("output".into(), Value::Array(output));
    if let Some(usage) = obj.get("usage") {
        let input = usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
        let output_toks = usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
        let cached = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let mut u = Map::new();
        u.insert("input_tokens".into(), Value::from(input));
        u.insert("output_tokens".into(), Value::from(output_toks));
        // saturating: untrusted upstream JSON must clamp, not panic.
        u.insert("total_tokens".into(), Value::from(input.saturating_add(output_toks)));
        if cached > 0 {
            u.insert(
                "input_tokens_details".into(),
                serde_json::json!({ "cached_tokens": cached }),
            );
        }
        resp.insert("usage".into(), Value::Object(u));
    }

    match serde_json::to_string(&Value::Object(resp)) {
        Ok(s) => Bytes::from(s),
        Err(_) => Bytes::copy_from_slice(body),
    }
}

#[cfg(test)]
mod tests;
