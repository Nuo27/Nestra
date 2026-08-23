//! Responses API SSE stream conversion for the gateway.
//!
//! Two converters cover the outbound matrix (responses upstream, chat or
//! anthropic inbound):
//!   [`ResponsesToAnthropicStream`]  — responses SSE → Anthropic events.
//!   [`ResponsesToChatStream`]       — responses SSE → chat completion chunks.
//!
//! Engineering details shared with the chat converter (`stream_convert.rs`):
//! one SSE event per poll, index-keyed tool buffering with late start,
//! deferred message_delta with usage, and a failed upstream stream is never
//! presented as success (error event instead). Responses-specific details:
//!   - the `event:` line is authoritative; gateways that omit it fall back
//!     to the JSON `type` field;
//!   - there is no `[DONE]` — `response.completed` / `response.incomplete`
//!     are the terminal events (clean EOF without one finalizes as end_turn);
//!   - multi-byte UTF-8 split across frames is buffered (3-byte remainder)
//!     instead of lossy-replaced;
//!   - `response.completed` carries the full response object, so usage and
//!     stop_reason come from the embedded snapshot.

use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::BodyExt as _;
use hyper::body::{Body, Frame};
use serde_json::{Map, Value};

type InnerStream = BoxBody<Bytes, std::io::Error>;

/// Flat Responses-wire event (output_item.added / deltas / dones): the data
/// carries `type` + the event fields at the same level — official clients
/// deserialize by the `type` discriminator, so it must be present.
fn sse_typed(event: &str, mut data: Value) -> Bytes {
    if let Some(obj) = data.as_object_mut() {
        obj.entry("type".to_string())
            .or_insert_with(|| Value::String(event.to_string()));
    }
    sse(event, data)
}

/// Response-level event (created / completed / incomplete / failed): the
/// response object is WRAPPED — `{"type": <event>, "response": {...}}`. A
/// bare response object as the payload is an unknown event to codex / the
/// openai SDKs (the discriminator never fires).
fn sse_response(event: &str, resp: Map<String, Value>) -> Bytes {
    sse(
        event,
        serde_json::json!({"type": event, "response": Value::Object(resp)}),
    )
}

/// SSE framing helper: `event: <name>\ndata: <json>\n\n`. An empty event
/// name emits a bare `data:` frame (chat chunks convention).
fn sse(event: &str, data: Value) -> Bytes {
    let head = if event.is_empty() {
        String::new()
    } else {
        format!("event: {event}\n")
    };
    Bytes::from(format!(
        "{head}data: {}\n\n",
        serde_json::to_string(&data).unwrap_or_else(|_| "{}".into())
    ))
}

/// Take ONE complete SSE frame (without the blank-line separator) off the
/// buffer. Handles both `\r\n\r\n` and `\n\n` boundaries; `None` when no
/// complete frame is buffered. One-frame-at-a-time so a converter that
/// yields after emitting can resume where it left off.
fn take_one_frame(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let d = buf.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2));
    let crlf = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4));
    let cut = match (d, crlf) {
        (Some((di, _dl)), Some((ci, cl))) if ci < di => Some((ci, cl)),
        (Some(x), Some(_)) => Some(x),
        (Some(x), None) => Some(x),
        (None, Some(x)) => Some(x),
        (None, None) => None,
    };
    let (idx, len) = cut?;
    let frame = buf.drain(..idx).collect();
    buf.drain(..len);
    Some(frame)
}

/// Hard cap on undelimited SSE bytes buffered before a frame separator. An
/// upstream that never sends `\n\n` would otherwise grow the buffer without
/// bound — a memory-exhaustion DoS via the proxy. Over the cap the stream is
/// aborted with an error event instead.
const MAX_FRAME_BUFFER: usize = 8 * 1024 * 1024; // 8 MiB

/// The output item of a `response.output_item.*` event. OpenAI names it
/// `output_item`; opencode-go names it `item` — accept both.
fn output_item<'a>(j: &'a Value) -> Option<&'a Value> {
    j.get("output_item").or_else(|| j.get("item"))
}

/// Resolve the tool key for an argument-delta event: OpenAI tags them with
/// `item_id`; opencode-go tags them with `output_index` only. Returns the
/// key the converter registered the tool under (`item_id` or `#<index>`).
fn delta_tool_key(j: &Value) -> Option<String> {
    if let Some(id) = j.get("item_id").and_then(Value::as_str) {
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    j.get("output_index")
        .and_then(Value::as_u64)
        .map(|oi| format!("#{oi}"))
}

/// opencode-go's argument delta stream truncates the start of the arguments
/// object — the fragments concatenate to `command":"echo hi"}` (missing the
/// `{"` prefix), so they would never parse as JSON. Restore the missing
/// prefix on the first fragment of each tool: `{"` before a bare key, `{`
/// before a quoted key.
fn brace_first_fragment(first: bool, delta: &str) -> String {
    if !first || delta.starts_with('{') {
        delta.to_string()
    } else if delta.starts_with('"') {
        format!("{{{}", delta)
    } else {
        format!("{{\"{}", delta)
    }
}

/// Event name + data for one SSE frame: `event:` line when present, else the
/// JSON `type` field (some compatible gateways omit the event line).
fn event_name(frame: &str) -> (Option<String>, Option<Value>) {
    let mut event = None;
    let mut data_parts: Vec<&str> = Vec::new();
    for line in frame.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("data:") {
            data_parts.push(v);
        }
    }
    let body = data_parts.join("\n");
    let body = body.trim();
    let json = if body.is_empty() || body == "[DONE]" {
        None
    } else {
        serde_json::from_str(body).ok()
    };
    let name = event.or_else(|| {
        json.as_ref()
            .and_then(|j: &Value| j.get("type"))
            .and_then(Value::as_str)
            .map(String::from)
    });
    (name, json)
}

/// Multi-byte UTF-8 accumulation: complete characters flow out, a partial
/// trailing character stays buffered until its continuation arrives. Bytes
/// that can never form valid UTF-8 are replaced with U+FFFD rather than
/// stalling the stream.
struct Utf8Accum {
    pending: Vec<u8>,
}

impl Utf8Accum {
    fn new() -> Self {
        Self { pending: Vec::new() }
    }

    fn push(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        self.pending.extend_from_slice(bytes);
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    out.extend_from_slice(s.as_bytes());
                    self.pending.clear();
                    return;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        out.extend_from_slice(&self.pending[..valid]);
                        self.pending.drain(..valid);
                        continue;
                    }
                    // Nothing valid at the front. Distinguish a TRUNCATED
                    // multi-byte char (error_len() == None — wait for more
                    // bytes) from DEFINITELY INVALID bytes (error_len() ==
                    // Some(n) — replace exactly that run with U+FFFD).
                    // The old lead-byte heuristic stalled forever on a lead
                    // byte followed by an invalid continuation (e.g. 0xE4 0x28):
                    // a malformed upstream chunk then hung the whole stream.
                    match e.error_len() {
                        None => return, // truncated — wait for continuation
                        Some(n) => {
                            out.extend_from_slice(b"\xEF\xBF\xBD");
                            self.pending.drain(..n.max(1));
                        }
                    }
                }
            }
        }
    }
}

/// Responses usage object → Anthropic usage (input excludes cached tokens).
fn anthropic_usage_from_responses(usage: &Value) -> Option<Value> {
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
    Some(Value::Object(u))
}

/// Responses usage object → chat usage (prompt/completion tokens).
fn chat_usage_from_responses(usage: &Value) -> Option<Value> {
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
    u.insert("total_tokens".into(), Value::from(input + output));
    if cached > 0 {
        let mut details = Map::new();
        details.insert("cached_tokens".into(), Value::from(cached));
        u.insert("prompt_tokens_details".into(), Value::Object(details));
    }
    Some(Value::Object(u))
}

// ===========================================================================
// Responses SSE → Anthropic events
// ===========================================================================

struct ToolState {
    block_index: usize,
    id: Option<String>,
    name: Option<String>,
    started: bool,
    pending_args: Vec<u8>,
    /// Whether any argument fragment arrived — guards the leading-`{`
    /// restoration for opencode's brace-less first fragment.
    args_seen: bool,
}

/// State machine converting `response.*` SSE events into Anthropic
/// `message_start / content_block_* / message_delta / message_stop` events.
pub struct ResponsesToAnthropicStream {
    inner: InnerStream,
    utf8: Utf8Accum,
    buf: Vec<u8>,
    out: Vec<u8>,
    done: bool,
    message_id: Option<String>,
    model: Option<String>,
    sent_message_start: bool,
    next_content_index: usize,
    open_reasoning: Option<usize>,
    open_text: Option<usize>,
    tool_states: HashMap<String, ToolState>,
    /// `#<output_index>` → item_id: opencode tags delta events by output
    /// index instead of item id.
    index_alias: HashMap<String, String>,
    /// A function_call output item was seen (opencode's `response.completed`
    /// carries no output array to infer this from).
    saw_tool: bool,
    open_tool_order: Vec<String>,
    stop_reason: Option<String>,
    latest_usage: Option<Value>,
    finished: bool,
}

impl ResponsesToAnthropicStream {
    pub fn new<B>(inner: B) -> Self
    where
        B: Body<Data = Bytes> + Send + Sync + 'static,
    {
        let inner = inner.map_err(|_| std::io::Error::other("upstream stream error"));
        Self {
            inner: inner.boxed(),
            utf8: Utf8Accum::new(),
            buf: Vec::new(),
            out: Vec::new(),
            done: false,
            message_id: None,
            model: None,
            sent_message_start: false,
            next_content_index: 0,
            open_reasoning: None,
            open_text: None,
            tool_states: HashMap::new(),
            index_alias: HashMap::new(),
            saw_tool: false,
            open_tool_order: Vec::new(),
            stop_reason: None,
            latest_usage: None,
            finished: false,
        }
    }

    fn ensure_message_start(&mut self) {
        if self.sent_message_start {
            return;
        }
        self.sent_message_start = true;
        let id = self.message_id.clone().unwrap_or_else(|| "msg_responses".into());
        let model = self.model.clone().unwrap_or_default();
        self.out.extend_from_slice(&sse(
            "message_start",
            serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": { "input_tokens": 0, "output_tokens": 0 },
                }
            }),
        ));
    }

    fn open_text_block(&mut self) {
        if self.open_text.is_some() {
            return;
        }
        self.ensure_message_start();
        self.close_text_and_reasoning();
        let idx = self.next_content_index;
        self.next_content_index += 1;
        self.open_text = Some(idx);
        self.out.extend_from_slice(&sse(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": { "type": "text", "text": "" }
            }),
        ));
    }

    fn open_reasoning_block(&mut self) {
        if self.open_reasoning.is_some() {
            return;
        }
        // Thinking blocks must precede text; drop reasoning arriving after
        // text (fidelity tradeoff — see convert_responses.rs).
        if self.open_text.is_some() {
            return;
        }
        self.ensure_message_start();
        let idx = self.next_content_index;
        self.next_content_index += 1;
        self.open_reasoning = Some(idx);
        self.out.extend_from_slice(&sse(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": { "type": "thinking", "thinking": "" }
            }),
        ));
    }

    /// Close the text/reasoning blocks only (a tool block may start after).
    fn close_text_and_reasoning(&mut self) {
        if let Some(idx) = self.open_text.take() {
            self.out.extend_from_slice(&sse(
                "content_block_stop",
                serde_json::json!({ "type": "content_block_stop", "index": idx }),
            ));
        }
        if let Some(idx) = self.open_reasoning.take() {
            self.out.extend_from_slice(&sse(
                "content_block_stop",
                serde_json::json!({ "type": "content_block_stop", "index": idx }),
            ));
        }
    }

    /// Close every open block (finish path): text/reasoning plus any started
    /// tool blocks. Unstarted tool states are dropped — they never emitted
    /// a start, so nothing needs closing.
    fn close_open_blocks(&mut self) {
        self.close_text_and_reasoning();
        for item_id in std::mem::take(&mut self.open_tool_order) {
            if let Some(state) = self.tool_states.get(&item_id) {
                if state.started {
                    self.out.extend_from_slice(&sse(
                        "content_block_stop",
                        serde_json::json!({
                            "type": "content_block_stop",
                            "index": state.block_index
                        }),
                    ));
                }
                self.tool_states.remove(&item_id);
            }
        }
    }

    /// Emit the tool_use block start (with any buffered argument fragment)
    /// and return the assigned content-block index.
    fn start_tool_block(&mut self, id: &str, name: &str, pending_args: &[u8]) -> usize {
        self.ensure_message_start();
        self.close_text_and_reasoning();
        let idx = self.next_content_index;
        self.next_content_index += 1;
        self.out.extend_from_slice(&sse(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": {}
                }
            }),
        ));
        if !pending_args.is_empty() {
            let args = String::from_utf8_lossy(pending_args).into_owned();
            self.out.extend_from_slice(&sse(
                "content_block_delta",
                serde_json::json!({
                    "type": "content_block_delta",
                    "index": idx,
                    "delta": { "type": "input_json_delta", "partial_json": args }
                }),
            ));
        }
        idx
    }

    fn close_tool_block(&mut self, item_id: &str) {
        if let Some(state) = self.tool_states.get(&item_id.to_string()) {
            if state.started {
                self.out.extend_from_slice(&sse(
                    "content_block_stop",
                    serde_json::json!({
                        "type": "content_block_stop",
                        "index": state.block_index
                    }),
                ));
            }
        }
        self.tool_states.remove(&item_id.to_string());
        self.open_tool_order.retain(|i| i != item_id);
    }

    fn process_event(&mut self, name: Option<&str>, json: Option<&Value>) {
        let Some(name) = name else { return };

        match name {
            "response.created" => {
                if let Some(j) = json {
                    if let Some(r) = j.get("response") {
                        self.message_id = r.get("id").and_then(Value::as_str).map(String::from);
                        self.model = r.get("model").and_then(Value::as_str).map(String::from);
                    }
                }
            }
            "response.in_progress" => {}
            "response.output_item.added" => {
                let Some(j) = json else { return };
                let Some(item) = output_item(j) else { return };
                let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
                match item.get("type").and_then(Value::as_str) {
                    Some("reasoning") => self.open_reasoning_block(),
                    Some("message") => {}
                    Some("function_call") => {
                        self.saw_tool = true;
                        // opencode keys the delta events by output index —
                        // remember the mapping back to the item id.
                        if let Some(oi) = j.get("output_index").and_then(Value::as_u64) {
                            self.index_alias.insert(format!("#{oi}"), item_id.to_string());
                        }
                        let st = ToolState {
                            block_index: 0,
                            id: item
                                .get("call_id")
                                .or_else(|| item.get("id"))
                                .and_then(Value::as_str)
                                .map(String::from),
                            name: item.get("name").and_then(Value::as_str).map(String::from),
                            started: false,
                            pending_args: Vec::new(),
                            args_seen: false,
                        };
                        // Name is present in well-formed gateways — start
                        // immediately; otherwise wait for the first delta.
                        let can_start = st.name.is_some();
                        self.tool_states.insert(item_id.to_string(), st);
                        if !self.open_tool_order.contains(&item_id.to_string()) {
                            self.open_tool_order.push(item_id.to_string());
                        }
                        if can_start {
                            // Phase 1: read the state (releases the borrow).
                            let (id, name, args) = {
                                let state = self.tool_states.get(&item_id.to_string());
                                match state {
                                    Some(s) => (
                                        s.id.clone().unwrap_or_default(),
                                        s.name.clone().unwrap_or_default(),
                                        s.pending_args.clone(),
                                    ),
                                    None => return,
                                }
                            };
                            // Phase 2: emit through &mut self.
                            let idx = self.start_tool_block(&id, &name, &args);
                            // Phase 3: write back.
                            if let Some(state) = self.tool_states.get_mut(&item_id.to_string()) {
                                state.block_index = idx;
                                state.started = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            "response.content_part.added" => self.open_text_block(),
            "response.output_text.delta" | "response.refusal.delta" => {
                let text = json
                    .and_then(|j| j.get("delta"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if text.is_empty() {
                    return;
                }
                if self.open_text.is_none() {
                    self.open_text_block();
                }
                let idx = self.open_text.unwrap_or(0);
                self.out.extend_from_slice(&sse(
                    "content_block_delta",
                    serde_json::json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": { "type": "text_delta", "text": text }
                    }),
                ));
            }
            "response.output_text.done" | "response.refusal.done" => {
                if let Some(idx) = self.open_text.take() {
                    self.out.extend_from_slice(&sse(
                        "content_block_stop",
                        serde_json::json!({ "type": "content_block_stop", "index": idx }),
                    ));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta"
            | "response.reasoning.delta" => {
                let text = json
                    .and_then(|j| j.get("delta"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if text.is_empty() {
                    return;
                }
                if self.open_reasoning.is_none() {
                    self.open_reasoning_block();
                }
                let Some(idx) = self.open_reasoning else { return };
                self.out.extend_from_slice(&sse(
                    "content_block_delta",
                    serde_json::json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": { "type": "thinking_delta", "thinking": text }
                    }),
                ));
            }
            "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                if let Some(idx) = self.open_reasoning.take() {
                    self.out.extend_from_slice(&sse(
                        "content_block_stop",
                        serde_json::json!({ "type": "content_block_stop", "index": idx }),
                    ));
                }
            }
            "response.function_call_arguments.delta" => {
                let Some(j) = json else { return };
                let item_id = delta_tool_key(j)
                    .map(|k| self.index_alias.get(&k).cloned().unwrap_or(k))
                    .unwrap_or_default();
                let delta = j.get("delta").and_then(Value::as_str).unwrap_or("");

                if delta.is_empty() {
                    return;
                }
                let entry = self.tool_states.get_mut(&item_id);
                let Some(state) = entry else { return };

                let fragment = brace_first_fragment(!state.args_seen, delta);
                state.args_seen = true;
                if state.started {
                    let idx = state.block_index;
                    self.out.extend_from_slice(&sse(
                        "content_block_delta",
                        serde_json::json!({
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": { "type": "input_json_delta", "partial_json": fragment }
                        }),
                    ));
                } else {
                    // Arguments arrived before name: buffer, flushed by the
                    // late start.
                    state.pending_args.extend_from_slice(fragment.as_bytes());
                }
            }
            "response.function_call_arguments.done" => {
                let Some(j) = json else { return };
                let item_id = delta_tool_key(j)
                    .map(|k| self.index_alias.get(&k).cloned().unwrap_or(k))
                    .unwrap_or_default();
                // Late start (name arrived in a delta-tagged event).
                let needs_start = {
                    let state = self.tool_states.get(&item_id.to_string());
                    match state {
                        Some(s) if !s.started && s.name.is_none() => {
                            // No name ever arrived — drop the block.
                            self.tool_states.remove(&item_id.to_string());
                            self.open_tool_order.retain(|i| i != item_id.as_str());
                            return;
                        }
                        Some(s) if !s.started => true,
                        _ => false,
                    }
                };
                if needs_start {
                    let (id, name, args) = {
                        let state = self.tool_states.get(&item_id.to_string());
                        match state {
                            Some(s) => (
                                s.id.clone().unwrap_or_default(),
                                s.name.clone().unwrap_or_default(),
                                s.pending_args.clone(),
                            ),
                            None => return,
                        }
                    };
                    let idx = self.start_tool_block(&id, &name, &args);
                    if let Some(state) = self.tool_states.get_mut(&item_id.to_string()) {
                        state.block_index = idx;
                        state.started = true;
                    }
                }
                self.close_tool_block(&item_id);
            }
            "response.output_item.done" => {
                let item = json.and_then(output_item);
                let item_id = item
                    .and_then(|i| i.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let kind = item
                    .and_then(|i| i.get("type"))
                    .and_then(Value::as_str);
                match kind {
                    Some("function_call") => self.close_tool_block(&item_id),
                    Some("reasoning") => {
                        if let Some(idx) = self.open_reasoning.take() {
                            self.out.extend_from_slice(&sse(
                                "content_block_stop",
                                serde_json::json!({ "type": "content_block_stop", "index": idx }),
                            ));
                        }
                    }
                    Some("message") => {
                        if let Some(idx) = self.open_text.take() {
                            self.out.extend_from_slice(&sse(
                                "content_block_stop",
                                serde_json::json!({ "type": "content_block_stop", "index": idx }),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                let resp = json
                    .and_then(|j| j.get("response"))
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                self.latest_usage = resp
                    .get("usage")
                    .cloned()
                    .or_else(|| json.and_then(|j| j.get("usage")).cloned());
                let status = resp.get("status").and_then(Value::as_str).unwrap_or("completed");
                let has_tool = resp
                    .get("output")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter().any(|i| {
                            i.get("type").and_then(Value::as_str) == Some("function_call")
                        })
                    })
                    .unwrap_or(false);
                // opencode's completed event carries usage only — no output
                // array — so also honor the function_call items seen live.
                let has_tool = has_tool || self.saw_tool;
                self.stop_reason = Some(match status {
                    "incomplete" => {
                        let reason = resp
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
                        if has_tool {
                            "tool_use"
                        } else {
                            "end_turn"
                        }
                    }
                }
                .to_string());
                self.finish();
            }
            "response.failed" | "error" => {
                let msg = json
                    .and_then(|j| j.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("upstream stream failed");
                self.emit_error(msg);
            }
            _ => {}
        }
    }

    fn emit_error(&mut self, message: &str) {
        self.finished = true;
        self.done = true;
        self.out.extend_from_slice(&sse(
            "error",
            serde_json::json!({
                "type": "error",
                "error": { "type": "nestra_upstream_error", "message": message }
            }),
        ));
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.close_open_blocks();
        let stop_reason = self.stop_reason.clone().unwrap_or_else(|| "end_turn".into());
        let mut delta = Map::new();
        delta.insert("stop_reason".into(), Value::String(stop_reason));
        delta.insert("stop_sequence".into(), Value::Null);
        let mut ev = Map::new();
        ev.insert("type".into(), Value::String("message_delta".into()));
        ev.insert("delta".into(), Value::Object(delta));
        if let Some(usage) = self.latest_usage.clone() {
            if let Some(u) = anthropic_usage_from_responses(&usage) {
                // Anthropic's message_delta.usage accepts ONLY output_tokens
                // — input/cache fields belong in message_start (and the
                // message_start above carries zeros; full usage lands in the
                // delta we're allowed to send).
                let mut delta_usage = Map::new();
                if let Some(ot) = u.get("output_tokens").cloned() {
                    delta_usage.insert("output_tokens".into(), ot);
                }
                ev.insert("usage".into(), Value::Object(delta_usage));
            }
        }
        self.out.extend_from_slice(&sse("message_delta", Value::Object(ev)));
        self.out.extend_from_slice(&sse("message_stop", serde_json::json!({ "type": "message_stop" })));
        self.done = true;
    }

    /// Shared poll loop: drain out → poll inner → feed frames to
    /// `process_event`. One SSE event is emitted per poll.
    /// Process every complete SSE frame buffered so far. Returns true when
    /// the caller should yield (out non-empty or done).
    fn drain_buffered_events(&mut self) -> bool {
        while let Some(f) = take_one_frame(&mut self.buf) {
            let (name, json) = event_name(&String::from_utf8_lossy(&f));
            self.process_event(name.as_deref(), json.as_ref());
            if !self.out.is_empty() || self.done {
                return true;
            }
        }
        false
    }

    fn poll_loop(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        if !self.out.is_empty() {
            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(std::mem::take(&mut self.out))))));
        }
        if self.done {
            return Poll::Ready(None);
        }
        // Frames buffered by a previous poll must drain before reading more
        // — the inner body may already be exhausted.
        if self.drain_buffered_events() {
            if !self.out.is_empty() {
                return Poll::Ready(Some(Ok(Frame::data(Bytes::from(
                    std::mem::take(&mut self.out),
                )))));
            }
            if self.done {
                return Poll::Ready(None);
            }
        }
        loop {
            match Pin::new(&mut self.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Some(data) = frame.data_ref() {
                        let mut tmp = Vec::new();
                        self.utf8.push(data, &mut tmp);
                        self.buf.extend_from_slice(&tmp);
                        // Cap undelimited buffering: an upstream that never
                        // sends a frame separator must abort, not OOM us.
                        if self.buf.len() > MAX_FRAME_BUFFER {
                            self.emit_error("upstream stream exceeded the frame buffer limit");
                        }
                        if self.drain_buffered_events() {
                            if !self.out.is_empty() {
                                return Poll::Ready(Some(Ok(Frame::data(Bytes::from(
                                    std::mem::take(&mut self.out),
                                )))));
                            }
                            if self.done {
                                return Poll::Ready(None);
                            }
                        }
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    self.emit_error(&e.to_string());
                    return if self.out.is_empty() {
                        Poll::Ready(None)
                    } else {
                        Poll::Ready(Some(Ok(Frame::data(Bytes::from(std::mem::take(&mut self.out))))))
                    };
                }
                Poll::Ready(None) => {
                    // Drain buffered frames, then process a trailing frame
                    // without the blank-line separator (EOF sentinel).
                    if self.drain_buffered_events() {
                        if !self.out.is_empty() {
                            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(
                                std::mem::take(&mut self.out),
                            )))));
                        }
                        if self.done {
                            return Poll::Ready(None);
                        }
                    }
                    if !self.buf.is_empty() {
                        let leftover = std::mem::take(&mut self.buf);
                        let (name, json) = event_name(&String::from_utf8_lossy(&leftover));
                        self.process_event(name.as_deref(), json.as_ref());
                        if !self.out.is_empty() {
                            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(
                                std::mem::take(&mut self.out),
                            )))));
                        }
                    }
                    self.finish();
                    return if self.out.is_empty() {
                        Poll::Ready(None)
                    } else {
                        Poll::Ready(Some(Ok(Frame::data(Bytes::from(std::mem::take(&mut self.out))))))
                    };
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl Body for ResponsesToAnthropicStream {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        // Safe projection: every field is Unpin (Vec/String/Option/Box/
        // HashMap), so `Pin::get_mut` applies — no unsafe needed here.
        let this = self.as_mut().get_mut();
        this.poll_loop(cx)
    }
}

// ===========================================================================
// Responses SSE → Chat Completions chunks
// ===========================================================================

/// State machine converting `response.*` SSE events into
/// `chat.completion.chunk` frames ending with `[DONE]`.
pub struct ResponsesToChatStream {
    inner: InnerStream,
    utf8: Utf8Accum,
    buf: Vec<u8>,
    out: Vec<u8>,
    done: bool,
    tool_indices: HashMap<String, usize>,
    /// Tools whose first argument fragment arrived — guards the leading-`{`
    /// restoration for opencode's brace-less first fragment.
    tool_args_seen: std::collections::HashSet<usize>,
    next_tool_index: usize,
    stop_reason: Option<String>,
    latest_usage: Option<Value>,
    finished: bool,
}

impl ResponsesToChatStream {
    pub fn new<B>(inner: B) -> Self
    where
        B: Body<Data = Bytes> + Send + Sync + 'static,
    {
        let inner = inner.map_err(|_| std::io::Error::other("upstream stream error"));
        Self {
            inner: inner.boxed(),
            utf8: Utf8Accum::new(),
            buf: Vec::new(),
            out: Vec::new(),
            done: false,
            tool_indices: HashMap::new(),
            tool_args_seen: std::collections::HashSet::new(),
            next_tool_index: 0,
            stop_reason: None,
            latest_usage: None,
            finished: false,
        }
    }

    fn chat_finish_reason(&self) -> Value {
        match self.stop_reason.as_deref() {
            Some("tool_use") => Value::String("tool_calls".into()),
            Some("max_tokens") => Value::String("length".into()),
            _ => Value::String("stop".into()),
        }
    }

    fn chunk(&mut self, delta: Value, finish: Option<Value>) {
        let mut choice = Map::new();
        choice.insert("index".into(), Value::from(0));
        choice.insert("delta".into(), delta);
        choice.insert("finish_reason".into(), finish.unwrap_or(Value::Null));
        self.out.extend_from_slice(&sse(
            "",
            serde_json::json!({
                "id": "chatcmpl-nestra",
                "object": "chat.completion.chunk",
                "choices": [Value::Object(choice)]
            }),
        ));
    }

    fn process_event(&mut self, name: Option<&str>, json: Option<&Value>) {
        let Some(name) = name else { return };
        match name {
            "response.output_item.added" => {
                let Some(j) = json else { return };
                let Some(item) = output_item(j) else { return };
                if item.get("type").and_then(Value::as_str) != Some("function_call") {
                    return;
                }
                let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
                let idx = self.next_tool_index;
                self.next_tool_index += 1;
                self.tool_indices.insert(item_id.to_string(), idx);
                // opencode tags the argument-delta events by output index
                // instead of item id — register that key too.
                if let Some(oi) = j.get("output_index").and_then(Value::as_u64) {
                    self.tool_indices.insert(format!("#{oi}"), idx);
                }
                let mut tool = Map::new();
                tool.insert("index".into(), Value::from(idx));
                tool.insert("type".into(), Value::String("function".into()));
                if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                    tool.insert("id".into(), Value::String(id.into()));
                }
                let mut func = Map::new();
                if let Some(name) = item.get("name").and_then(Value::as_str) {
                    func.insert("name".into(), Value::String(name.into()));
                }
                tool.insert("function".into(), Value::Object(func));
                self.chunk(serde_json::json!({ "tool_calls": [Value::Object(tool)] }), None);
            }
            "response.function_call_arguments.delta" => {
                let Some(j) = json else { return };
                let Some(key) = delta_tool_key(j) else { return };
                let delta = j.get("delta").and_then(Value::as_str).unwrap_or("");
                if delta.is_empty() {
                    return;
                }
                let Some(&idx) = self.tool_indices.get(&key) else {
                    return;
                };
                let fragment = brace_first_fragment(self.tool_args_seen.insert(idx), delta);
                let mut tool = Map::new();
                tool.insert("index".into(), Value::from(idx));
                tool.insert("function".into(), serde_json::json!({ "arguments": fragment }));
                self.chunk(serde_json::json!({ "tool_calls": [Value::Object(tool)] }), None);
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let delta = json
                    .and_then(|j| j.get("delta"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if delta.is_empty() {
                    return;
                }
                self.chunk(serde_json::json!({ "content": delta }), None);
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let delta = json
                    .and_then(|j| j.get("delta"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if delta.is_empty() {
                    return;
                }
                self.chunk(serde_json::json!({ "reasoning_content": delta }), None);
            }
            "response.completed" | "response.incomplete" => {
                let resp = json
                    .and_then(|j| j.get("response"))
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                self.latest_usage = resp
                    .get("usage")
                    .cloned()
                    .or_else(|| json.and_then(|j| j.get("usage")).cloned());
                let status = resp.get("status").and_then(Value::as_str).unwrap_or("completed");
                let has_tool = resp
                    .get("output")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter().any(|i| {
                            i.get("type").and_then(Value::as_str) == Some("function_call")
                        })
                    })
                    .unwrap_or(false);
                // opencode's completed event carries usage only — no output
                // array — so also honor the function_call items seen live.
                let has_tool = has_tool || self.next_tool_index > 0;
                self.stop_reason = Some(match status {
                    "incomplete" => "max_tokens",
                    _ => {
                        if has_tool {
                            "tool_use"
                        } else {
                            "end_turn"
                        }
                    }
                }
                .to_string());
                self.finish();
            }
            "response.failed" | "error" => {
                let msg = json
                    .and_then(|j| j.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("upstream stream failed");
                self.emit_error(msg);
            }
            _ => {}
        }
    }

    fn emit_error(&mut self, message: &str) {
        self.finished = true;
        self.done = true;
        self.out.extend_from_slice(&sse(
            "",
            serde_json::json!({ "error": { "message": message, "type": "nestra_upstream_error" } }),
        ));
        self.out.extend_from_slice(b"data: [DONE]\n\n");
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let mut choice = Map::new();
        choice.insert("index".into(), Value::from(0));
        choice.insert("delta".into(), serde_json::json!({}));
        choice.insert("finish_reason".into(), self.chat_finish_reason());
        let mut chunk = Map::new();
        chunk.insert("id".into(), Value::String("chatcmpl-nestra".into()));
        chunk.insert("object".into(), Value::String("chat.completion.chunk".into()));
        chunk.insert("choices".into(), Value::Array(vec![Value::Object(choice)]));
        if let Some(usage) = self.latest_usage.clone() {
            if let Some(u) = chat_usage_from_responses(&usage) {
                chunk.insert("usage".into(), u);
            }
        }
        self.out.extend_from_slice(&sse("", Value::Object(chunk)));
        self.out.extend_from_slice(b"data: [DONE]\n\n");
        self.done = true;
    }

    /// Process every complete SSE frame buffered so far. Returns true when
    /// the caller should yield (out non-empty or done).
    fn drain_buffered_events(&mut self) -> bool {
        while let Some(f) = take_one_frame(&mut self.buf) {
            let (name, json) = event_name(&String::from_utf8_lossy(&f));
            self.process_event(name.as_deref(), json.as_ref());
            if !self.out.is_empty() || self.done {
                return true;
            }
        }
        false
    }

    fn poll_loop(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        if !self.out.is_empty() {
            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(std::mem::take(&mut self.out))))));
        }
        if self.done {
            return Poll::Ready(None);
        }
        // Frames buffered by a previous poll must drain before reading more
        // — the inner body may already be exhausted.
        if self.drain_buffered_events() {
            if !self.out.is_empty() {
                return Poll::Ready(Some(Ok(Frame::data(Bytes::from(
                    std::mem::take(&mut self.out),
                )))));
            }
            if self.done {
                return Poll::Ready(None);
            }
        }
        loop {
            match Pin::new(&mut self.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Some(data) = frame.data_ref() {
                        let mut tmp = Vec::new();
                        self.utf8.push(data, &mut tmp);
                        self.buf.extend_from_slice(&tmp);
                        // Cap undelimited buffering: an upstream that never
                        // sends a frame separator must abort, not OOM us.
                        if self.buf.len() > MAX_FRAME_BUFFER {
                            self.emit_error("upstream stream exceeded the frame buffer limit");
                        }
                        if self.drain_buffered_events() {
                            if !self.out.is_empty() {
                                return Poll::Ready(Some(Ok(Frame::data(Bytes::from(
                                    std::mem::take(&mut self.out),
                                )))));
                            }
                            if self.done {
                                return Poll::Ready(None);
                            }
                        }
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    self.emit_error(&e.to_string());
                    return if self.out.is_empty() {
                        Poll::Ready(None)
                    } else {
                        Poll::Ready(Some(Ok(Frame::data(Bytes::from(std::mem::take(&mut self.out))))))
                    };
                }
                Poll::Ready(None) => {
                    // Drain buffered frames, then process a trailing frame
                    // without the blank-line separator (EOF sentinel).
                    if self.drain_buffered_events() {
                        if !self.out.is_empty() {
                            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(
                                std::mem::take(&mut self.out),
                            )))));
                        }
                        if self.done {
                            return Poll::Ready(None);
                        }
                    }
                    if !self.buf.is_empty() {
                        let leftover = std::mem::take(&mut self.buf);
                        let (name, json) = event_name(&String::from_utf8_lossy(&leftover));
                        self.process_event(name.as_deref(), json.as_ref());
                        if !self.out.is_empty() {
                            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(
                                std::mem::take(&mut self.out),
                            )))));
                        }
                    }
                    self.finish();
                    return if self.out.is_empty() {
                        Poll::Ready(None)
                    } else {
                        Poll::Ready(Some(Ok(Frame::data(Bytes::from(std::mem::take(&mut self.out))))))
                    };
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl Body for ResponsesToChatStream {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        // Safe projection: every field is Unpin (Vec/String/Option/Box/
        // HashMap), so `Pin::get_mut` applies — no unsafe needed here.
        let this = self.as_mut().get_mut();
        this.poll_loop(cx)
    }
}

/// Chat Completions SSE chunks → Responses API SSE events. The inbound half
/// of the Codex bridge (chat-shaped upstream, Responses-speaking agent);
/// chained after [`super::stream_convert::AnthropicToChatStream`] it also
/// covers anthropic-shaped upstreams. Mirrors [`ResponsesToChatStream`]'s
/// engineering (one frame per poll, index-keyed tool buffering, deferred
/// `response.completed` with usage, errors surfaced as `response.failed`).
///
/// Event protocol (the set the codex client consumes):
///   - first assistant delta → `response.created` + `response.output_item.added`
///     for the assistant message item;
///   - content deltas → `response.output_text.delta` (+ `.done` at finish);
///   - tool_calls deltas → `response.output_item.added` (function_call) once
///     per index, then `response.function_call_arguments.delta`;
///   - the finish_reason chunk → `response.output_item.done` for every open
///     item, then `response.completed` carrying the full output snapshot +
///     usage (chat `[DONE]` without a finish chunk finalizes as completed);
///   - `data: {"error": …}` → `response.failed`.
pub struct ChatToResponsesStream {
    inner: InnerStream,
    utf8: Utf8Accum,
    buf: Vec<u8>,
    out: Vec<u8>,
    done: bool,
    /// output_index → item state. 0 is the assistant message; tools take
    /// 1.. in first-seen order.
    message_open: bool,
    message_text: String,
    tool_indices: HashMap<usize, ToolItem>,
    next_tool_index: usize,
    latest_usage: Option<Value>,
    finish_reason: Option<String>,
    finished: bool,
    model: Option<String>,
    response_id: String,
}

struct ToolItem {
    /// Responses item id (`fc_…`).
    item_id: String,
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
    /// Delta key already announced ("function_call_arguments") — set when
    /// `response.output_item.added` went out for this tool.
    open: bool,
    output_index: usize,
}

impl ChatToResponsesStream {
    pub fn new<B>(inner: B) -> Self
    where
        B: Body<Data = Bytes> + Send + Sync + 'static,
    {
        let inner = inner.map_err(|_| std::io::Error::other("upstream stream error"));
        Self {
            inner: inner.boxed(),
            utf8: Utf8Accum::new(),
            buf: Vec::new(),
            out: Vec::new(),
            done: false,
            message_open: false,
            message_text: String::new(),
            tool_indices: HashMap::new(),
            next_tool_index: 0,
            latest_usage: None,
            finish_reason: None,
            finished: false,
            model: None,
            response_id: format!("resp-nestra-{}", uuid::Uuid::new_v4()),
        }
    }

    fn response_skeleton(&self, status: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("id".into(), Value::String(self.response_id.clone()));
        m.insert("object".into(), Value::String("response".into()));
        if let Some(model) = &self.model {
            m.insert("model".into(), Value::String(model.clone()));
        }
        m.insert("status".into(), Value::String(status.into()));
        m
    }

    fn ensure_message_item(&mut self) {
        if self.message_open {
            return;
        }
        self.message_open = true;
        let mut resp = self.response_skeleton("in_progress");
        resp.insert("output".into(), Value::Array(vec![]));
        self.out.extend_from_slice(&sse_response("response.created", resp));
        self.out.extend_from_slice(&sse_typed(
            "response.output_item.added",
            serde_json::json!({
                "output_index": 0,
                "item": {
                    "type": "message",
                    "id": "msg-nestra",
                    "role": "assistant",
                    "status": "in_progress",
                    "content": [],
                },
            }),
        ));
    }

    fn process_chunk(&mut self, chunk: &Value) {
        if let Some(model) = chunk.get("model").and_then(Value::as_str) {
            if self.model.is_none() {
                self.model = Some(model.to_string());
            }
        }
        if let Some(usage) = chunk.get("usage") {
            if usage.is_object() && !usage.as_object().unwrap().is_empty() {
                self.latest_usage = Some(usage.clone());
            }
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        else {
            return;
        };
        let delta = choice.get("delta").cloned().unwrap_or(Value::Null);

        // Text content.
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                self.ensure_message_item();
                self.message_text.push_str(text);
                self.out.extend_from_slice(&sse_typed(
                    "response.output_text.delta",
                    serde_json::json!({
                        "item_id": "msg-nestra",
                        "output_index": 0,
                        "content_index": 0,
                        "delta": text,
                    }),
                ));
            }
        }
        // Tool call deltas: first sighting announces the item, then argument
        // fragments stream as function_call_arguments deltas.
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let idx = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                // Pre-compute new-slot identity: a closure in or_insert_with
                // would capture all of `self` alongside the field borrow.
                let fresh = !self.tool_indices.contains_key(&idx);
                let (item_id, output_index) = if fresh {
                    let i = self.next_tool_index;
                    self.next_tool_index += 1;
                    (format!("fc-nestra-{i}"), i + 1)
                } else {
                    let t = &self.tool_indices[&idx];
                    (t.item_id.clone(), t.output_index)
                };
                let entry = self.tool_indices.entry(idx).or_insert(ToolItem {
                    item_id: item_id.clone(),
                    call_id: None,
                    name: None,
                    arguments: String::new(),
                    open: false,
                    output_index,
                });
                let func = call.get("function").cloned().unwrap_or(Value::Null);
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    entry.call_id = Some(id.to_string());
                }
                if let Some(name) = func.get("name").and_then(Value::as_str) {
                    entry.name = Some(name.to_string());
                }
                let frag = func.get("arguments").and_then(Value::as_str).unwrap_or("");
                if !frag.is_empty() {
                    entry.arguments.push_str(frag);
                }
                let call_id = entry.call_id.clone();
                let name = entry.name.clone();
                let announce = !entry.open;
                entry.open = true;
                if announce {
                    self.out.extend_from_slice(&sse_typed(
                        "response.output_item.added",
                        serde_json::json!({
                            "output_index": output_index,
                            "item": {
                                "type": "function_call",
                                "id": item_id,
                                "call_id": call_id,
                                "name": name,
                                "arguments": "",
                                "status": "in_progress",
                            },
                        }),
                    ));
                }
                if !frag.is_empty() {
                    self.out.extend_from_slice(&sse_typed(
                        "response.function_call_arguments.delta",
                        serde_json::json!({
                            "item_id": item_id,
                            "output_index": output_index,
                            "delta": frag,
                        }),
                    ));
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
            self.finish();
        }
    }

    /// Terminal events: item done for every open item + `response.completed`
    /// carrying the full output snapshot and usage.
    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let mut output: Vec<Value> = Vec::new();
        if self.message_open {
            self.out.extend_from_slice(&sse_typed(
                "response.output_text.done",
                serde_json::json!({
                    "item_id": "msg-nestra",
                    "output_index": 0,
                    "content_index": 0,
                    "text": self.message_text,
                }),
            ));
            output.push(serde_json::json!({
                "type": "message",
                "id": "msg-nestra",
                "role": "assistant",
                "status": "completed",
                "content": [{
                    "type": "output_text",
                    "text": self.message_text,
                    "annotations": [],
                }],
            }));
            self.out.extend_from_slice(&sse_typed(
                "response.output_item.done",
                serde_json::json!({
                    "output_index": 0,
                    "item": output[0],
                }),
            ));
        }
        let mut ordered: Vec<&mut ToolItem> = self.tool_indices.values_mut().collect();
        ordered.sort_by_key(|t| t.output_index);
        for tool in ordered {
            let item = serde_json::json!({
                "type": "function_call",
                "id": tool.item_id,
                "call_id": tool.call_id,
                "name": tool.name,
                "arguments": tool.arguments,
                "status": "completed",
            });
            output.push(item.clone());
            self.out.extend_from_slice(&sse_typed(
                "response.output_item.done",
                serde_json::json!({
                    "output_index": tool.output_index,
                    "item": item,
                }),
            ));
        }
        let incomplete = self.finish_reason.as_deref() == Some("length");
        let mut resp = self.response_skeleton(if incomplete { "incomplete" } else { "completed" });
        if incomplete {
            resp.insert(
                "incomplete_details".into(),
                serde_json::json!({ "reason": "max_output_tokens" }),
            );
        }
        resp.insert("output".into(), Value::Array(output));
        if let Some(usage) = self.latest_usage.clone() {
            let input = usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
            let out_toks = usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
            resp.insert(
                "usage".into(),
                serde_json::json!({
                    "input_tokens": input,
                    "output_tokens": out_toks,
                    "total_tokens": input.saturating_add(out_toks),
                }),
            );
        }
        let event = if incomplete { "response.incomplete" } else { "response.completed" };
        self.out.extend_from_slice(&sse_response(event, resp));
        self.done = true;
    }

    fn emit_error(&mut self, message: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.done = true;
        let mut resp = self.response_skeleton("failed");
        resp.insert(
            "error".into(),
            serde_json::json!({ "code": "nestra_upstream_error", "message": message }),
        );
        self.out.extend_from_slice(&sse_response("response.failed", resp));
    }

    /// Process buffered chat chunks; `Ok(true)` = caller should yield.
    fn drain_buffered_chunks(&mut self) -> bool {
        while let Some(f) = take_one_frame(&mut self.buf) {
            let frame = String::from_utf8_lossy(&f);
            for line in frame.lines() {
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data == "[DONE]" {
                    // Terminal sentinel — a clean EOF without a finish_reason
                    // chunk finalizes as completed.
                    self.finish();
                    continue;
                }
                match serde_json::from_str::<Value>(data) {
                    Ok(v) => {
                        if v.get("error").map(|e| !e.is_null()).unwrap_or(false) {
                            let msg = v
                                .get("error")
                                .and_then(|e| e.get("message"))
                                .and_then(Value::as_str)
                                .unwrap_or("upstream stream failed");
                            self.emit_error(msg);
                        } else {
                            self.process_chunk(&v);
                        }
                    }
                    Err(_) => continue,
                }
            }
            if !self.out.is_empty() || self.done {
                return true;
            }
        }
        false
    }

    fn poll_loop(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        if !self.out.is_empty() {
            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(std::mem::take(&mut self.out))))));
        }
        if self.done {
            return Poll::Ready(None);
        }
        if self.drain_buffered_chunks() {
            if !self.out.is_empty() {
                return Poll::Ready(Some(Ok(Frame::data(Bytes::from(
                    std::mem::take(&mut self.out),
                )))));
            }
            if self.done {
                return Poll::Ready(None);
            }
        }
        loop {
            match Pin::new(&mut self.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Some(data) = frame.data_ref() {
                        let mut tmp = Vec::new();
                        self.utf8.push(data, &mut tmp);
                        self.buf.extend_from_slice(&tmp);
                        if self.buf.len() > MAX_FRAME_BUFFER {
                            self.emit_error("upstream stream exceeded the frame buffer limit");
                        }
                        if self.drain_buffered_chunks() {
                            if !self.out.is_empty() {
                                return Poll::Ready(Some(Ok(Frame::data(Bytes::from(
                                    std::mem::take(&mut self.out),
                                )))));
                            }
                            if self.done {
                                return Poll::Ready(None);
                            }
                        }
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    self.emit_error(&e.to_string());
                    return if self.out.is_empty() {
                        Poll::Ready(None)
                    } else {
                        Poll::Ready(Some(Ok(Frame::data(Bytes::from(std::mem::take(&mut self.out))))))
                    };
                }
                Poll::Ready(None) => {
                    if self.drain_buffered_chunks() {
                        if !self.out.is_empty() {
                            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(
                                std::mem::take(&mut self.out),
                            )))));
                        }
                        if self.done {
                            return Poll::Ready(None);
                        }
                    }
                    if !self.buf.is_empty() {
                        let leftover = std::mem::take(&mut self.buf);
                        for line in String::from_utf8_lossy(&leftover).lines() {
                            if let Some(data) = line.strip_prefix("data:").map(str::trim) {
                                if data != "[DONE]" {
                                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                                        self.process_chunk(&v);
                                    }
                                }
                            }
                        }
                    }
                    self.finish();
                    return if self.out.is_empty() {
                        Poll::Ready(None)
                    } else {
                        Poll::Ready(Some(Ok(Frame::data(Bytes::from(std::mem::take(&mut self.out))))))
                    };
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl Body for ChatToResponsesStream {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.as_mut().get_mut();
        this.poll_loop(cx)
    }
}

#[cfg(test)]
mod tests;
