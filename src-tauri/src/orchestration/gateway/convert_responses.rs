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
    // max_tokens → max_output_tokens; stop dropped; stream passthrough.
    if let Some(mt) = obj.remove("max_tokens") {
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
// Request direction: Responses inbound → Anthropic Messages
// ---------------------------------------------------------------------------

/// Responses API request (client → gateway) → Anthropic Messages request.
/// Malformed input returns the original bytes.
pub fn responses_req_to_anthropic(body: &[u8]) -> Bytes {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Bytes::copy_from_slice(body),
    };
    let obj = match v.as_object() {
        Some(o) => o,
        None => return Bytes::copy_from_slice(body),
    };

    let mut out = Map::new();
    if let Some(model) = obj.get("model").cloned() {
        out.insert("model".into(), model);
    }
    if let Some(inst) = obj.get("instructions").and_then(Value::as_str) {
        if !inst.is_empty() {
            out.insert("system".into(), Value::String(inst.into()));
        }
    }
    if let Some(mt) = obj.get("max_output_tokens").cloned() {
        out.insert("max_tokens".into(), mt);
    }
    for key in ["temperature", "top_p", "stream"] {
        if let Some(v) = obj.get(key).cloned() {
            out.insert(key.into(), v);
        }
    }
    // reasoning.effort → thinking budget (simplified tier map).
    if let Some(effort) = obj
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(Value::as_str)
    {
        let budget = match effort {
            "low" => 4000,
            "medium" => 8000,
            "high" => 16000,
            "xhigh" => 32000,
            _ => 8000,
        };
        out.insert(
            "thinking".into(),
            serde_json::json!({ "type": "enabled", "budget_tokens": budget }),
        );
    }
    // tools: Responses function tools → Anthropic input_schema shape.
    if let Some(tools) = obj.get("tools").and_then(|v| v.as_array().cloned()) {
        let mapped: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let mut nt = Map::new();
                if let Some(name) = t.get("name").and_then(Value::as_str) {
                    nt.insert("name".into(), Value::String(name.into()));
                } else {
                    return None;
                }
                if let Some(desc) = t.get("description").and_then(Value::as_str) {
                    if !desc.is_empty() {
                        nt.insert("description".into(), Value::String(desc.into()));
                    }
                }
                let schema = t
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({ "type": "object" }));
                nt.insert("input_schema".into(), schema);
                Some(Value::Object(nt))
            })
            .collect();
        if !mapped.is_empty() {
            out.insert("tools".into(), Value::Array(mapped));
        }
    }
    // tool_choice: {type:function,name} → {type:tool,name}; required → any.
    if let Some(tc) = obj.get("tool_choice").cloned() {
        out.insert("tool_choice".into(), map_responses_tool_choice_to_anthropic(&tc));
    }
    // input items → messages.
    if let Some(items) = obj.get("input").and_then(|v| v.as_array().cloned()) {
        let messages = responses_input_to_anthropic_messages(&items);
        if !messages.is_empty() {
            out.insert("messages".into(), Value::Array(messages));
        }
    }

    match serde_json::to_string(&Value::Object(out)) {
        Ok(s) => Bytes::from(s),
        Err(_) => Bytes::copy_from_slice(body),
    }
}

/// Responses tool_choice → Anthropic tool_choice:
///   `"required"` → `"any"`;
///   `"auto"` / `"none"` strings → `{type:"auto"}` / `{type:"none"}` objects
///     (Anthropic accepts only strings `"any"` or `{type:…}` objects — the
///     old pass-through sent the bare strings, which Anthropic rejects);
///   `{type:"function",name}` → `{type:"tool",name}`;
///   everything else verbatim.
fn map_responses_tool_choice_to_anthropic(tc: &Value) -> Value {
    match tc {
        Value::String(s) if s == "required" => Value::String("any".into()),
        Value::String(s) if s == "auto" => {
            serde_json::json!({ "type": "auto" })
        }
        Value::String(s) if s == "none" => {
            serde_json::json!({ "type": "none" })
        }
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("function") => {
            let mut m = Map::new();
            m.insert("type".into(), Value::String("tool".into()));
            if let Some(name) = o.get("name").cloned() {
                m.insert("name".into(), name);
            }
            Value::Object(m)
        }
        _ => tc.clone(),
    }
}

fn responses_input_to_anthropic_messages(items: &[Value]) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();
    for item in items {
        // Role-bearing items without an explicit type are messages
        // (some clients omit the type on message items).
        let is_message = item.get("type").and_then(Value::as_str) == Some("message")
            || (item.get("type").is_none() && item.get("role").is_some());
        match item.get("type").and_then(Value::as_str) {
            _ if is_message => {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                let mut parts: Vec<Value> = Vec::new();
                if let Some(content) = item.get("content").and_then(|v| v.as_array().cloned()) {
                    for part in content {
                        match part.get("type").and_then(Value::as_str) {
                            Some("input_text") | Some("output_text") => {
                                if let Some(t) = part.get("text").and_then(Value::as_str) {
                                    if !t.is_empty() {
                                        parts.push(serde_json::json!({ "type": "text", "text": t }));
                                    }
                                }
                            }
                            Some("input_image") => {
                                if let Some(url) = part.get("image_url").and_then(Value::as_str) {
                                    if let Some(rest) = url.strip_prefix("data:") {
                                        let mut it = rest.splitn(2, ';');
                                        if let (Some(media), Some(after)) = (it.next(), it.next()) {
                                            if let Some(data) = after.strip_prefix("base64,") {
                                                parts.push(serde_json::json!({
                                                    "type": "image",
                                                    "source": {
                                                        "type": "base64",
                                                        "media_type": media,
                                                        "data": data,
                                                    },
                                                }));
                                            }
                                        }
                                    } else {
                                        parts.push(serde_json::json!({
                                            "type": "image",
                                            "source": { "type": "url", "url": url },
                                        }));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if !parts.is_empty() {
                    messages.push(serde_json::json!({ "role": role, "content": parts }));
                }
            }
            Some("function_call") => {
                let mut block = Map::new();
                block.insert("type".into(), Value::String("tool_use".into()));
                if let Some(id) = item.get("call_id").and_then(Value::as_str) {
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
                messages.push(serde_json::json!({
                    "role": "assistant",
                    "content": [Value::Object(block)],
                }));
            }
            Some("function_call_output") => {
                let mut block = Map::new();
                block.insert("type".into(), Value::String("tool_result".into()));
                if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                    block.insert("tool_use_id".into(), Value::String(id.into()));
                }
                block.insert(
                    "content".into(),
                    item.get("output").cloned().unwrap_or(Value::String(String::new())),
                );
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": [Value::Object(block)],
                }));
            }
            // reasoning / other items: dropped.
            _ => {}
        }
    }
    messages
}

// ---------------------------------------------------------------------------
// Request direction: Responses inbound → Chat Completions
// ---------------------------------------------------------------------------

/// Responses API request (client → gateway) → Chat Completions request.
/// Malformed input returns the original bytes.
pub fn responses_req_to_chat(body: &[u8]) -> Bytes {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Bytes::copy_from_slice(body),
    };
    let obj = match v.as_object() {
        Some(o) => o,
        None => return Bytes::copy_from_slice(body),
    };

    let mut out = Map::new();
    if let Some(model) = obj.get("model").cloned() {
        out.insert("model".into(), model);
    }
    let mut messages: Vec<Value> = Vec::new();
    if let Some(inst) = obj.get("instructions").and_then(Value::as_str) {
        if !inst.is_empty() {
            messages.push(serde_json::json!({ "role": "system", "content": inst }));
        }
    }
    if let Some(items) = obj.get("input").and_then(|v| v.as_array().cloned()) {
        for item in items {
            // Role-bearing items without an explicit type are messages
            // (some clients omit the type on message items).
            let is_message = item.get("type").and_then(Value::as_str) == Some("message")
                || (item.get("type").is_none() && item.get("role").is_some());
            match item.get("type").and_then(Value::as_str) {
                _ if is_message => {
                    let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                    let text: Vec<String> = item
                        .get("content")
                        .and_then(|v| v.as_array().cloned())
                        .map(|parts| {
                            parts
                                .iter()
                                .filter(|p| {
                                    matches!(
                                        p.get("type").and_then(Value::as_str),
                                        Some("input_text") | Some("output_text")
                                    )
                                })
                                .filter_map(|p| p.get("text").and_then(Value::as_str).map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    if !text.is_empty() {
                        messages.push(serde_json::json!({
                            "role": role,
                            "content": text.join("\n"),
                        }));
                    }
                }
                Some("function_call") => {
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
                    messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": Value::Null,
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
                _ => {}
            }
        }
    }
    if !messages.is_empty() {
        out.insert("messages".into(), Value::Array(messages));
    }
    // tools / tool_choice → chat shapes.
    if let Some(tools) = obj.get("tools").and_then(|v| v.as_array().cloned()) {
        let mapped: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let mut func = Map::new();
                if let Some(name) = t.get("name").and_then(Value::as_str) {
                    func.insert("name".into(), Value::String(name.into()));
                } else {
                    return None;
                }
                if let Some(desc) = t.get("description").and_then(Value::as_str) {
                    if !desc.is_empty() {
                        func.insert("description".into(), Value::String(desc.into()));
                    }
                }
                if let Some(params) = t.get("parameters").cloned() {
                    func.insert("parameters".into(), params);
                }
                let mut nt = Map::new();
                nt.insert("type".into(), Value::String("function".into()));
                nt.insert("function".into(), Value::Object(func));
                Some(Value::Object(nt))
            })
            .collect();
        if !mapped.is_empty() {
            out.insert("tools".into(), Value::Array(mapped));
        }
    }
    if let Some(tc) = obj.get("tool_choice").cloned() {
        let chat_tc = match &tc {
            Value::String(s) if s == "required" => Value::String("required".into()),
            Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("function") => {
                let mut m = Map::new();
                m.insert("type".into(), Value::String("function".into()));
                let mut f = Map::new();
                if let Some(name) = o.get("name").cloned() {
                    f.insert("name".into(), name);
                }
                m.insert("function".into(), Value::Object(f));
                Value::Object(m)
            }
            _ => tc.clone(),
        };
        out.insert("tool_choice".into(), chat_tc);
    }
    if let Some(mt) = obj.get("max_output_tokens").cloned() {
        out.insert("max_tokens".into(), mt);
    }
    for key in ["temperature", "top_p", "stream"] {
        if let Some(v) = obj.get(key).cloned() {
            out.insert(key.into(), v);
        }
    }
    if out.get("stream").and_then(Value::as_bool) == Some(true) {
        out.insert("stream_options".into(), serde_json::json!({ "include_usage": true }));
    }

    match serde_json::to_string(&Value::Object(out)) {
        Ok(s) => Bytes::from(s),
        Err(_) => Bytes::copy_from_slice(body),
    }
}

// ---------------------------------------------------------------------------
// Response direction: Anthropic upstream → Responses inbound
// ---------------------------------------------------------------------------

/// Anthropic Messages response (upstream) → Responses API response.
/// Malformed input returns the original bytes.
pub fn anthropic_resp_to_responses(body: &[u8]) -> Bytes {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Bytes::copy_from_slice(body),
    };
    let obj = match v.as_object() {
        Some(o) => o,
        None => return Bytes::copy_from_slice(body),
    };

    // Error envelope → responses error.
    if obj.get("type").and_then(Value::as_str) == Some("error") {
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

    let mut out = Map::new();
    out.insert(
        "id".into(),
        obj.get("id").cloned().unwrap_or_else(|| Value::String("resp_nestra".into())),
    );
    out.insert("object".into(), Value::String("response".into()));
    if let Some(model) = obj.get("model").cloned() {
        out.insert("model".into(), model);
    }

    let mut output: Vec<Value> = Vec::new();
    let mut text_parts: Vec<Value> = Vec::new();
    let mut msg_item = Map::new();
    let mut msg_open = false;
    if let Some(content) = obj.get("content").and_then(|v| v.as_array().cloned()) {
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        if !t.is_empty() {
                            if !msg_open {
                                msg_item.insert("type".into(), Value::String("message".into()));
                                msg_item.insert("role".into(), Value::String("assistant".into()));
                                msg_open = true;
                            }
                            text_parts.push(serde_json::json!({ "type": "output_text", "text": t }));
                        }
                    }
                }
                Some("thinking") => {
                    if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                        if !t.is_empty() {
                            output.push(serde_json::json!({
                                "type": "reasoning",
                                "summary": [{ "type": "summary_text", "text": t }],
                            }));
                        }
                    }
                }
                Some("tool_use") => {
                    let mut item = Map::new();
                    item.insert("type".into(), Value::String("function_call".into()));
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        item.insert("call_id".into(), Value::String(id.into()));
                    }
                    if let Some(name) = block.get("name").and_then(Value::as_str) {
                        item.insert("name".into(), Value::String(name.into()));
                    }
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    item.insert("arguments".into(), Value::String(canonical_json(&input)));
                    output.push(Value::Object(item));
                }
                _ => {}
            }
        }
    }
    if msg_open {
        msg_item.insert("content".into(), Value::Array(text_parts));
        msg_item.insert("status".into(), Value::String("completed".into()));
        // The message item goes AFTER any reasoning items (thinking blocks
        // precede text in Anthropic).
        let reasoning_count = output
            .iter()
            .filter(|o| o.get("type").and_then(Value::as_str) == Some("reasoning"))
            .count();
        output.insert(reasoning_count, Value::Object(msg_item));
    }

    // stop_reason → status.
    let stop_reason = obj.get("stop_reason").and_then(Value::as_str).unwrap_or("end_turn");
    match stop_reason {
        "max_tokens" => {
            out.insert("status".into(), Value::String("incomplete".into()));
            out.insert(
                "incomplete_details".into(),
                serde_json::json!({ "reason": "max_output_tokens" }),
            );
        }
        _ => {
            out.insert("status".into(), Value::String("completed".into()));
        }
    }
    out.insert("output".into(), Value::Array(output));

    // usage passthrough with cache split.
    if let Some(usage) = obj.get("usage") {
        let input = usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        let output_t = usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        let cached = usage.get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0);
        let mut u = Map::new();
        // saturating: untrusted upstream JSON — clamp instead of panicking.
        u.insert("input_tokens".into(), Value::from(input.saturating_add(cached)));
        u.insert("output_tokens".into(), Value::from(output_t));
        if cached > 0 {
            let mut details = Map::new();
            details.insert("cached_tokens".into(), Value::from(cached));
            u.insert("input_tokens_details".into(), Value::Object(details));
        }
        out.insert("usage".into(), Value::Object(u));
    }

    match serde_json::to_string(&Value::Object(out)) {
        Ok(s) => Bytes::from(s),
        Err(_) => Bytes::copy_from_slice(body),
    }
}

// ---------------------------------------------------------------------------
// Response direction: Chat upstream → Responses inbound
// ---------------------------------------------------------------------------

/// Chat Completions response (upstream) → Responses API response.
/// Malformed input returns the original bytes.
pub fn chat_resp_to_responses(body: &[u8]) -> Bytes {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Bytes::copy_from_slice(body),
    };
    let obj = match v.as_object() {
        Some(o) => o,
        None => return Bytes::copy_from_slice(body),
    };

    // Error envelope → responses error.
    if let Some(err) = obj.get("error") {
        let msg = err.get("message").and_then(Value::as_str).unwrap_or("upstream response failed");
        return Bytes::from(
            serde_json::json!({ "error": { "message": msg, "type": "nestra_upstream_error" } })
                .to_string(),
        );
    }

    let mut out = Map::new();
    out.insert(
        "id".into(),
        obj.get("id").cloned().unwrap_or_else(|| Value::String("resp_nestra".into())),
    );
    out.insert("object".into(), Value::String("response".into()));
    if let Some(model) = obj.get("model").cloned() {
        out.insert("model".into(), model);
    }

    let mut output: Vec<Value> = Vec::new();
    let mut finish_reason = "stop";
    if let Some(choice) = obj.get("choices").and_then(|c| c.get(0)) {
        finish_reason = choice.get("finish_reason").and_then(Value::as_str).unwrap_or("stop");
        if let Some(message) = choice.get("message") {
            let mut text_parts: Vec<Value> = Vec::new();
            let mut msg_item = Map::new();
            let mut msg_open = false;
            // reasoning_content → reasoning item.
            if let Some(r) = message.get("reasoning_content").and_then(Value::as_str) {
                if !r.is_empty() {
                    output.push(serde_json::json!({
                        "type": "reasoning",
                        "summary": [{ "type": "summary_text", "text": r }],
                    }));
                }
            }
            match message.get("content") {
                Some(Value::String(s)) if !s.is_empty() => {
                    msg_open = true;
                    msg_item.insert("type".into(), Value::String("message".into()));
                    msg_item.insert("role".into(), Value::String("assistant".into()));
                    text_parts.push(serde_json::json!({ "type": "output_text", "text": s }));
                }
                Some(Value::Array(parts)) => {
                    let text: Vec<&str> = parts
                        .iter()
                        .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect();
                    if !text.is_empty() {
                        msg_open = true;
                        msg_item.insert("type".into(), Value::String("message".into()));
                        msg_item.insert("role".into(), Value::String("assistant".into()));
                        text_parts.push(serde_json::json!({
                            "type": "output_text",
                            "text": text.join("\n"),
                        }));
                    }
                }
                _ => {}
            }
            if let Some(calls) = message.get("tool_calls").and_then(|v| v.as_array().cloned()) {
                for call in calls {
                    let mut item = Map::new();
                    item.insert("type".into(), Value::String("function_call".into()));
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        item.insert("call_id".into(), Value::String(id.into()));
                    }
                    if let Some(name) = call.get("function").and_then(|f| f.get("name")).and_then(Value::as_str) {
                        item.insert("name".into(), Value::String(name.into()));
                    }
                    let args = call
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                        .unwrap_or("{}");
                    item.insert("arguments".into(), Value::String(args.to_string()));
                    output.push(Value::Object(item));
                }
            }
            if msg_open {
                msg_item.insert("content".into(), Value::Array(text_parts));
                msg_item.insert("status".into(), Value::String("completed".into()));
                // The message item goes BEFORE tool items (reasoning was
                // pushed first; insert the message after reasoning items).
                let reasoning_count = output
                    .iter()
                    .filter(|o| o.get("type").and_then(Value::as_str) == Some("reasoning"))
                    .count();
                output.insert(reasoning_count, Value::Object(msg_item));
            }
        }
    }

    match finish_reason {
        "length" | "max_tokens" => {
            out.insert("status".into(), Value::String("incomplete".into()));
            out.insert(
                "incomplete_details".into(),
                serde_json::json!({ "reason": "max_output_tokens" }),
            );
        }
        _ => {
            out.insert("status".into(), Value::String("completed".into()));
        }
    }
    out.insert("output".into(), Value::Array(output));

    if let Some(usage) = obj.get("usage") {
        let input = usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
        let output_t = usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
        let cached = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let mut u = Map::new();
        u.insert("input_tokens".into(), Value::from(input));
        u.insert("output_tokens".into(), Value::from(output_t));
        if cached > 0 {
            let mut details = Map::new();
            details.insert("cached_tokens".into(), Value::from(cached));
            u.insert("input_tokens_details".into(), Value::Object(details));
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


#[cfg(test)]
mod tests {
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

    // ---- inbound request conversions ----

    #[test]
    fn responses_req_to_anthropic_round_trips() {
        let body = serde_json::json!({
            "model": "grok-4.5",
            "instructions": "be nice",
            "max_output_tokens": 128,
            "reasoning": { "effort": "high" },
            "tools": [{ "type": "function", "name": "bash", "parameters": { "type": "object" } }],
            "tool_choice": { "type": "function", "name": "bash" },
            "input": [
                { "role": "user", "content": [{ "type": "input_text", "text": "hi" }] },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "bash",
                    "arguments": "{\"cmd\":\"ls\"}"
                },
                { "type": "function_call_output", "call_id": "call_1", "output": "out" }
            ]
        });
        let out = conv(responses_req_to_anthropic, &body);
        assert_eq!(out["system"], "be nice");
        assert_eq!(out["max_tokens"], 128);
        assert_eq!(out["thinking"]["budget_tokens"], 16000);
        assert_eq!(out["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(out["tool_choice"]["type"], "tool");
        assert_eq!(out["tool_choice"]["name"], "bash");
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["id"], "call_1");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["content"], "out");
    }

    #[test]
    fn responses_req_to_chat_converts_items() {
        let body = serde_json::json!({
            "model": "grok-4.5",
            "instructions": "be nice",
            "max_output_tokens": 64,
            "input": [
                { "role": "user", "content": [{ "type": "input_text", "text": "hi" }] },
                { "type": "function_call", "call_id": "c1", "name": "bash", "arguments": "{}" },
                { "type": "function_call_output", "call_id": "c1", "output": "out" }
            ],
            "stream": true
        });
        let out = conv(responses_req_to_chat, &body);
        assert_eq!(out["max_tokens"], 64);
        assert_eq!(out["stream_options"]["include_usage"], true);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["content"], "out");
    }

    // ---- upstream responses to responses inbound ----

    #[test]
    fn anthropic_resp_to_responses_converts_blocks() {
        let body = serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "grok-4.5",
            "stop_reason": "tool_use",
            "content": [
                { "type": "thinking", "thinking": "plan" },
                { "type": "text", "text": "doing" },
                { "type": "tool_use", "id": "call_1", "name": "bash", "input": { "cmd": "ls" } }
            ],
            "usage": { "input_tokens": 100, "output_tokens": 5, "cache_read_input_tokens": 50 }
        });
        let out = conv(anthropic_resp_to_responses, &body);
        assert_eq!(out["object"], "response");
        assert_eq!(out["status"], "completed");
        let output = out["output"].as_array().unwrap();
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["summary"][0]["text"], "plan");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["text"], "doing");
        assert_eq!(output[2]["type"], "function_call");
        assert_eq!(output[2]["arguments"], r#"{"cmd":"ls"}"#);
        assert_eq!(out["usage"]["input_tokens"], 150);
        assert_eq!(out["usage"]["input_tokens_details"]["cached_tokens"], 50);
    }

    #[test]
    fn chat_resp_to_responses_converts_choices() {
        let body = serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "model": "grok-4.5",
            "choices": [{
                "index": 0,
                "finish_reason": "length",
                "message": {
                    "role": "assistant",
                    "content": "partial",
                    "reasoning_content": "think",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "bash", "arguments": "{}" }
                    }]
                }
            }],
            "usage": { "prompt_tokens": 90, "completion_tokens": 7 }
        });
        let out = conv(chat_resp_to_responses, &body);
        assert_eq!(out["status"], "incomplete");
        assert_eq!(out["incomplete_details"]["reason"], "max_output_tokens");
        let output = out["output"].as_array().unwrap();
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["text"], "partial");
        assert_eq!(output[2]["type"], "function_call");
        assert_eq!(out["usage"]["input_tokens"], 90);
        assert_eq!(out["usage"]["output_tokens"], 7);
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
            responses_req_to_anthropic as fn(&[u8]) -> Bytes,
            responses_req_to_chat as fn(&[u8]) -> Bytes,
            anthropic_resp_to_responses as fn(&[u8]) -> Bytes,
            chat_resp_to_responses as fn(&[u8]) -> Bytes,
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
}
