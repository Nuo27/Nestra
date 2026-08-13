//! Responses API SSE stream conversion for the gateway.
//!
//! Four converters cover the wire matrix:
//!   [`ResponsesToAnthropicStream`]  — responses SSE → Anthropic events
//!     (anthropic inbound, responses upstream).
//!   [`ResponsesToChatStream`]       — responses SSE → chat completion chunks
//!     (chat inbound, responses upstream).
//!   [`ChatToResponsesStream`]       — chat chunks → responses SSE
//!     (responses inbound, chat upstream).
//!   [`AnthropicToResponsesStream`]  — Anthropic events → responses SSE
//!     (responses inbound, anthropic upstream).
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
                let Some(item) = j.get("output_item") else { return };
                let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
                match item.get("type").and_then(Value::as_str) {
                    Some("reasoning") => self.open_reasoning_block(),
                    Some("message") => {}
                    Some("function_call") => {
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
                let item_id = json
                    .and_then(|j| j.get("item_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let delta = json
                    .and_then(|j| j.get("delta"))
                    .and_then(Value::as_str)
                    .unwrap_or("");

                if delta.is_empty() {
                    return;
                }
                let entry = self.tool_states.get_mut(&item_id.to_string());
                let Some(state) = entry else { return };

                if state.started {
                    let idx = state.block_index;
                    let text = delta.to_string();
                    self.out.extend_from_slice(&sse(
                        "content_block_delta",
                        serde_json::json!({
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": { "type": "input_json_delta", "partial_json": text }
                        }),
                    ));
                } else {
                    // Arguments arrived before name: buffer, flushed by the
                    // late start.
                    state.pending_args.extend_from_slice(delta.as_bytes());
                }
            }
            "response.function_call_arguments.done" => {
                let item_id = json
                    .and_then(|j| j.get("item_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                // Late start (name arrived in a delta-tagged event).
                let needs_start = {
                    let state = self.tool_states.get(&item_id.to_string());
                    match state {
                        Some(s) if !s.started && s.name.is_none() => {
                            // No name ever arrived — drop the block.
                            self.tool_states.remove(&item_id.to_string());
                            self.open_tool_order.retain(|i| i != item_id);
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
                let item_id = json
                    .and_then(|j| j.get("output_item"))
                    .and_then(|i| i.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let kind = json
                    .and_then(|j| j.get("output_item"))
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
                let Some(item) = j.get("output_item") else { return };
                if item.get("type").and_then(Value::as_str) != Some("function_call") {
                    return;
                }
                let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
                let idx = self.next_tool_index;
                self.next_tool_index += 1;
                self.tool_indices.insert(item_id.to_string(), idx);
                let mut tool = Map::new();
                tool.insert("index".into(), Value::from(idx));
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
                let item_id = json
                    .and_then(|j| j.get("item_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let delta = json
                    .and_then(|j| j.get("delta"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if delta.is_empty() {
                    return;
                }
                let Some(&idx) = self.tool_indices.get(&item_id.to_string()) else {
                    return;
                };
                let mut tool = Map::new();
                tool.insert("index".into(), Value::from(idx));
                tool.insert("function".into(), serde_json::json!({ "arguments": delta }));
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

// ===========================================================================
// Chat Completions chunks → Responses SSE (responses inbound, chat upstream)
// ===========================================================================

struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    args: Vec<u8>,
    /// `output_item.added` already emitted for this tool.
    added: bool,
    /// `function_call_arguments.done` + `output_item.done` already emitted.
    done: bool,
}

/// State machine converting `chat.completion.chunk` frames into
/// `response.*` SSE events. Tool-call deltas are buffered by chat `index`;
/// `output_item.added` fires once id+name are known, argument deltas stream
/// as they arrive, and the done events are deferred until the stream ends —
/// flushing "done" early would truncate every tool's JSON arguments.
pub struct ChatToResponsesStream {
    inner: InnerStream,
    utf8: Utf8Accum,
    buf: Vec<u8>,
    out: Vec<u8>,
    done: bool,
    response_id: String,
    model: String,
    sent_created: bool,
    text_item_added: bool,
    reasoning_item_added: bool,
    /// Monotonic per-response output index — a constant 0 breaks clients that
    /// expect unique indices across items (text AND reasoning can coexist).
    next_output_index: u32,
    /// Assigned output indices for the message / reasoning items (set at
    /// first `added` emission; deltas and done events reuse them).
    text_output_index: u32,
    reasoning_output_index: u32,
    tool_calls: HashMap<usize, PendingToolCall>,
    tool_flush_order: Vec<usize>,
    stop_reason: Option<String>,
    latest_usage: Option<Value>,
    finished: bool,
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
            response_id: "resp_nestra".into(),
            model: String::new(),
            sent_created: false,
            text_item_added: false,
            reasoning_item_added: false,
            next_output_index: 0,
            text_output_index: 0,
            reasoning_output_index: 0,
            tool_calls: HashMap::new(),
            tool_flush_order: Vec::new(),
            stop_reason: None,
            latest_usage: None,
            finished: false,
        }
    }

    fn emit(&mut self, event: &str, data: Value) {
        self.out.extend_from_slice(&sse(event, data));
    }

    fn ensure_created(&mut self) {
        if self.sent_created {
            return;
        }
        self.sent_created = true;
        self.emit(
            "response.created",
            serde_json::json!({
                "type": "response.created",
                "response": {
                    "id": self.response_id,
                    "object": "response",
                    "status": "in_progress",
                    "model": self.model,
                    "output": [],
                }
            }),
        );
        self.emit("response.in_progress", serde_json::json!({ "type": "response.in_progress" }));
    }

    /// Emit `output_item.added` for a tool whose id+name are both known.
    /// Once per tool — re-emitting on every delta chunk breaks the client
    /// state machine (duplicate ids, missing done events).
    fn ensure_tool_added(&mut self, index: usize) {
        if self.tool_calls.get(&index).map(|t| t.added).unwrap_or(false) {
            return;
        }
        let Some(tool) = self.tool_calls.get_mut(&index) else {
            return;
        };
        let Some(name) = tool.name.clone() else {
            return; // name not yet known — wait
        };
        let id = tool.id.clone().unwrap_or_else(|| format!("call_{index}"));
        tool.added = true;
        self.ensure_created();
        self.emit(
            "response.output_item.added",
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": index,
                "output_item": {
                    "id": format!("fc_{index}"),
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": "",
                    "status": "in_progress",
                }
            }),
        );
    }

    /// Emit the freshly-arrived argument bytes for tool `index` as a delta.
    fn emit_tool_args_delta(&mut self, index: usize) {
        let Some(tool) = self.tool_calls.get(&index) else {
            return;
        };
        if !tool.added || tool.done {
            return;
        }
        let args = String::from_utf8_lossy(&tool.args).into_owned();
        if args.is_empty() {
            return;
        }
        self.emit(
            "response.function_call_arguments.delta",
            serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "item_id": format!("fc_{index}"),
                "output_index": index,
                "delta": args,
            }),
        );
    }

    /// Close a tool: `function_call_arguments.done` + `output_item.done`.
    /// Called from `finish()` (EOF) — never while deltas may still arrive.
    fn complete_tool(&mut self, index: usize) {
        if self.tool_calls.get(&index).map(|t| t.done).unwrap_or(false) {
            return;
        }
        let Some(tool) = self.tool_calls.get_mut(&index) else {
            return;
        };
        let Some(name) = tool.name.clone() else {
            return;
        };
        if !tool.added {
            return; // never added → nothing to close
        }
        tool.done = true;
        let id = tool.id.clone().unwrap_or_else(|| format!("call_{index}"));
        let done_args = String::from_utf8_lossy(&tool.args).into_owned();
        self.emit(
            "response.function_call_arguments.done",
            serde_json::json!({
                "type": "response.function_call_arguments.done",
                "item_id": format!("fc_{index}"),
                "output_index": index,
            }),
        );
        self.emit(
            "response.output_item.done",
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": index,
                "output_item": {
                    "id": format!("fc_{index}"),
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": done_args,
                    "status": "completed",
                }
            }),
        );
    }

    fn process_chunk(&mut self, chunk: &Value) {
        // usage-only final chunk (stream_options.include_usage).
        if let Some(usage) = chunk.get("usage") {
            self.latest_usage = Some(usage.clone());
        }
        let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else {
            return;
        };
        let delta = choice.get("delta").unwrap_or(&serde_json::Value::Null);
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                self.ensure_created();
                // added events fire ONCE per item — re-emitting on every
                // delta chunk (same id) breaks the client state machine.
                if !self.text_item_added {
                    self.text_item_added = true;
                    let output_index = self.next_output_index;
                    self.next_output_index += 1;
                    self.text_output_index = output_index;
                    self.emit(
                        "response.output_item.added",
                        serde_json::json!({
                            "type": "response.output_item.added",
                            "output_index": output_index,
                            "output_item": {
                                "id": "msg_0",
                                "type": "message",
                                "role": "assistant",
                                "status": "in_progress",
                                "content": [],
                            }
                        }),
                    );
                    self.emit(
                        "response.content_part.added",
                        serde_json::json!({
                            "type": "response.content_part.added",
                            "item_id": "msg_0",
                            "output_index": output_index,
                            "content_index": 0,
                            "part": { "type": "output_text", "text": "", "annotations": [] }
                        }),
                    );
                }
                let output_index = self.text_output_index;
                self.emit(
                    "response.output_text.delta",
                    serde_json::json!({
                        "type": "response.output_text.delta",
                        "item_id": "msg_0",
                        "output_index": output_index,
                        "content_index": 0,
                        "delta": text,
                    }),
                );
            }
        }
        if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
            if !reasoning.is_empty() {
                self.ensure_created();
                if !self.reasoning_item_added {
                    self.reasoning_item_added = true;
                    let output_index = self.next_output_index;
                    self.next_output_index += 1;
                    self.reasoning_output_index = output_index;
                    self.emit(
                        "response.output_item.added",
                        serde_json::json!({
                            "type": "response.output_item.added",
                            "output_index": output_index,
                            "output_item": {
                                "id": "rs_0",
                                "type": "reasoning",
                                "summary": [],
                                "status": "in_progress",
                            }
                        }),
                    );
                }
                let output_index = self.reasoning_output_index;
                self.emit(
                    "response.reasoning_summary_text.delta",
                    serde_json::json!({
                        "type": "response.reasoning_summary_text.delta",
                        "item_id": "rs_0",
                        "output_index": output_index,
                        "delta": reasoning,
                    }),
                );
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let idx = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let entry = self.tool_calls.entry(idx).or_insert_with(|| PendingToolCall {
                    id: None,
                    name: None,
                    args: Vec::new(),
                    added: false,
                    done: false,
                });
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    entry.id = Some(id.to_string());
                }
                if let Some(name) = call.get("function").and_then(|f| f.get("name")).and_then(Value::as_str) {
                    entry.name = Some(name.to_string());
                }
                if let Some(args) = call.get("function").and_then(|f| f.get("arguments")).and_then(Value::as_str) {
                    entry.args.extend_from_slice(args.as_bytes());
                }
                if !self.tool_flush_order.contains(&idx) {
                    self.tool_flush_order.push(idx);
                }
                // Added event once id+name are known; argument deltas stream
                // immediately (they may span several chunks). The done events
                // are deferred to `finish()` so no argument bytes are lost.
                if entry.id.is_some() && entry.name.is_some() {
                    self.ensure_tool_added(idx);
                    self.emit_tool_args_delta(idx);
                }
            }
        }
        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            if !finish.is_empty() {
                self.stop_reason = Some(finish.to_string());
            }
        }
    }

    fn emit_error(&mut self, message: &str) {
        self.finished = true;
        self.done = true;
        self.emit(
            "response.failed",
            serde_json::json!({
                "type": "response.failed",
                "error": { "message": message, "type": "nestra_upstream_error" }
            }),
        );
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.ensure_created();
        // Close the text item that was added but never completed (the
        // Responses wire requires a done event per added item).
        if self.text_item_added {
            let output_index = self.text_output_index;
            self.emit(
                "response.output_text.done",
                serde_json::json!({
                    "type": "response.output_text.done",
                    "item_id": "msg_0",
                    "output_index": output_index,
                    "content_index": 0,
                    "text": "",
                }),
            );
            self.emit(
                "response.content_part.done",
                serde_json::json!({
                    "type": "response.content_part.done",
                    "item_id": "msg_0",
                    "output_index": output_index,
                    "content_index": 0,
                    "part": { "type": "output_text", "text": "", "annotations": [] },
                }),
            );
            self.emit(
                "response.output_item.done",
                serde_json::json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "output_item": {
                        "id": "msg_0",
                        "type": "message",
                        "role": "assistant",
                        "status": "completed",
                        "content": [ { "type": "output_text", "text": "", "annotations": [] } ],
                    }
                }),
            );
        }
        if self.reasoning_item_added {
            let output_index = self.reasoning_output_index;
            self.emit(
                "response.reasoning_summary_text.done",
                serde_json::json!({
                    "type": "response.reasoning_summary_text.done",
                    "item_id": "rs_0",
                    "output_index": output_index,
                    "summary": [],
                }),
            );
            self.emit(
                "response.output_item.done",
                serde_json::json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "output_item": {
                        "id": "rs_0",
                        "type": "reasoning",
                        "summary": [],
                        "status": "completed",
                    }
                }),
            );
        }
        // Complete every tool whose added event went out — arguments were
        // streamed as deltas, so the done envelope carries the final bytes.
        let pending: Vec<usize> = self
            .tool_flush_order
            .iter()
            .copied()
            .filter(|i| {
                self.tool_calls
                    .get(i)
                    .map(|t| t.added && !t.done)
                    .unwrap_or(false)
            })
            .collect();
        for idx in pending {
            self.complete_tool(idx);
        }
        // Close text item if we emitted deltas.
        // (Simplified: the completed envelope carries the final output.)
        let status = match self.stop_reason.as_deref() {
            Some("length") | Some("max_tokens") => "incomplete",
            _ => "completed",
        };
        let mut resp = Map::new();
        resp.insert("id".into(), Value::String(self.response_id.clone()));
        resp.insert("object".into(), Value::String("response".into()));
        resp.insert("status".into(), Value::String(status.into()));
        resp.insert("model".into(), Value::String(self.model.clone()));
        // The completed envelope's `output` mirrors every item whose added
        // event went out — a constant `[]` breaks clients that render from it.
        let mut output: Vec<Value> = Vec::new();
        if self.text_item_added {
            output.push(serde_json::json!({
                "id": "msg_0",
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [ { "type": "output_text", "text": "", "annotations": [] } ],
            }));
        }
        if self.reasoning_item_added {
            output.push(serde_json::json!({
                "id": "rs_0",
                "type": "reasoning",
                "summary": [],
                "status": "completed",
            }));
        }
        for idx in self.tool_flush_order.iter().copied() {
            if let Some(t) = self.tool_calls.get(&idx) {
                if t.added {
                    let id = t.id.clone().unwrap_or_else(|| format!("call_{idx}"));
                    let name = t.name.clone().unwrap_or_default();
                    let args = String::from_utf8_lossy(&t.args).into_owned();
                    output.push(serde_json::json!({
                        "id": format!("fc_{idx}"),
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": args,
                        "status": "completed",
                    }));
                }
            }
        }
        resp.insert("output".into(), Value::Array(output));
        if status == "incomplete" {
            resp.insert(
                "incomplete_details".into(),
                serde_json::json!({ "reason": "max_output_tokens" }),
            );
        }
        // Usage must be the RESPONSES shape (input_tokens/output_tokens) —
        // copying the chat usage object verbatim would send
        // prompt_tokens/completion_tokens, which Responses clients reject.
        if let Some(usage) = &self.latest_usage {
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
            resp.insert("usage".into(), Value::Object(u));
        }
        self.emit("response.completed", serde_json::json!({
            "type": "response.completed",
            "response": Value::Object(resp),
        }));
        self.done = true;
    }

    /// Process every complete SSE frame buffered so far. Returns true when
    /// the caller should yield (out non-empty or done).
    fn drain_buffered_chunks(&mut self) -> bool {
        while let Some(f) = take_one_frame(&mut self.buf) {
            let (_, json) = event_name(&String::from_utf8_lossy(&f));
            if let Some(chunk) = json {
                self.process_chunk(&chunk);
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
                        // Cap undelimited buffering: an upstream that never
                        // sends a frame separator must abort, not OOM us.
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
                        let (_, json) = event_name(&String::from_utf8_lossy(&leftover));
                        if let Some(chunk) = json {
                            self.process_chunk(&chunk);
                        }
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

impl Body for ChatToResponsesStream {
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
// Anthropic events → Responses SSE (responses inbound, anthropic upstream)
// ===========================================================================

enum OpenBlock {
    Text { item_id: String, output_index: u32 },
    Thinking { item_id: String, emitted: bool, output_index: u32 },
    Tool { item_id: String, call_id: String, name: String, emitted_args: bool, output_index: u32 },
}

/// State machine converting Anthropic SSE events into `response.*` events.
pub struct AnthropicToResponsesStream {
    inner: InnerStream,
    utf8: Utf8Accum,
    buf: Vec<u8>,
    out: Vec<u8>,
    done: bool,
    response_id: String,
    model: String,
    sent_created: bool,
    open: Option<OpenBlock>,
    /// Monotonic per-response output index — the Responses wire requires a
    /// unique output_index per output item; a constant 0 breaks clients.
    next_output_index: u32,
    stop_reason: Option<String>,
    latest_usage: Option<Value>,
    finished: bool,
}

impl AnthropicToResponsesStream {
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
            response_id: "resp_nestra".into(),
            model: String::new(),
            sent_created: false,
            open: None,
            next_output_index: 0,
            stop_reason: None,
            latest_usage: None,
            finished: false,
        }
    }

    fn emit(&mut self, event: &str, data: Value) {
        self.out.extend_from_slice(&sse(event, data));
    }

    fn ensure_created(&mut self) {
        if self.sent_created {
            return;
        }
        self.sent_created = true;
        self.emit(
            "response.created",
            serde_json::json!({
                "type": "response.created",
                "response": {
                    "id": self.response_id,
                    "object": "response",
                    "status": "in_progress",
                    "model": self.model,
                    "output": [],
                }
            }),
        );
        self.emit("response.in_progress", serde_json::json!({ "type": "response.in_progress" }));
    }

    fn close_open(&mut self) {
        let Some(open) = self.open.take() else { return };
        match open {
            OpenBlock::Text { item_id, output_index } => {
                self.emit(
                    "response.output_text.done",
                    serde_json::json!({
                        "type": "response.output_text.done",
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                    }),
                );
                self.emit(
                    "response.content_part.done",
                    serde_json::json!({
                        "type": "response.content_part.done",
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                    }),
                );
                self.emit(
                    "response.output_item.done",
                    serde_json::json!({
                        "type": "response.output_item.done",
                        "output_index": output_index,
                        "output_item": {
                            "id": item_id,
                            "type": "message",
                            "role": "assistant",
                            "status": "completed",
                            "content": [],
                        }
                    }),
                );
            }
            OpenBlock::Thinking { item_id, output_index, .. } => {
                self.emit(
                    "response.reasoning_summary_text.done",
                    serde_json::json!({
                        "type": "response.reasoning_summary_text.done",
                        "item_id": item_id,
                        "output_index": output_index,
                    }),
                );
                self.emit(
                    "response.output_item.done",
                    serde_json::json!({
                        "type": "response.output_item.done",
                        "output_index": output_index,
                        "output_item": {
                            "id": item_id,
                            "type": "reasoning",
                            "summary": [],
                            "status": "completed",
                        }
                    }),
                );
            }
            OpenBlock::Tool { item_id, call_id, name, emitted_args, output_index } => {
                if !emitted_args {
                    self.emit(
                        "response.function_call_arguments.delta",
                        serde_json::json!({
                            "type": "response.function_call_arguments.delta",
                            "item_id": item_id,
                            "output_index": output_index,
                            "delta": "{}",
                        }),
                    );
                }
                self.emit(
                    "response.function_call_arguments.done",
                    serde_json::json!({
                        "type": "response.function_call_arguments.done",
                        "item_id": item_id,
                        "output_index": output_index,
                    }),
                );
                self.emit(
                    "response.output_item.done",
                    serde_json::json!({
                        "type": "response.output_item.done",
                        "output_index": output_index,
                        "output_item": {
                            "id": item_id,
                            "type": "function_call",
                            "call_id": call_id,
                            "name": name,
                            "arguments": "{}",
                            "status": "completed",
                        }
                    }),
                );
            }
        }
    }

    fn process_event(&mut self, name: Option<&str>, json: Option<&Value>) {
        let Some(name) = name else { return };
        match name {
            "message_start" => {
                if let Some(msg) = json.and_then(|j| j.get("message")) {
                    self.response_id = msg
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("resp_nestra")
                        .to_string();
                    self.model = msg.get("model").and_then(Value::as_str).unwrap_or("").to_string();
                }
                self.ensure_created();
            }
            "content_block_start" => {
                let Some(block) = json.and_then(|j| j.get("content_block")) else { return };
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        self.ensure_created();
                        let item_id = format!("msg_{}", self.out.len());
                        let output_index = self.next_output_index;
                        self.next_output_index += 1;
                        self.open = Some(OpenBlock::Text { item_id: item_id.clone(), output_index });
                        self.emit(
                            "response.output_item.added",
                            serde_json::json!({
                                "type": "response.output_item.added",
                                "output_index": output_index,
                                "output_item": {
                                    "id": item_id,
                                    "type": "message",
                                    "role": "assistant",
                                    "status": "in_progress",
                                    "content": [],
                                }
                            }),
                        );
                        self.emit(
                            "response.content_part.added",
                            serde_json::json!({
                                "type": "response.content_part.added",
                                "item_id": item_id,
                                "output_index": output_index,
                                "content_index": 0,
                                "part": { "type": "output_text", "text": "", "annotations": [] }
                            }),
                        );
                    }
                    Some("thinking") => {
                        self.ensure_created();
                        let item_id = format!("rs_{}", self.out.len());
                        let output_index = self.next_output_index;
                        self.next_output_index += 1;
                        self.open = Some(OpenBlock::Thinking { item_id: item_id.clone(), emitted: false, output_index });
                        self.emit(
                            "response.output_item.added",
                            serde_json::json!({
                                "type": "response.output_item.added",
                                "output_index": output_index,
                                "output_item": {
                                    "id": item_id,
                                    "type": "reasoning",
                                    "summary": [],
                                    "status": "in_progress",
                                }
                            }),
                        );
                        self.emit(
                            "response.reasoning_summary_part.added",
                            serde_json::json!({
                                "type": "response.reasoning_summary_part.added",
                                "item_id": item_id,
                                "output_index": output_index,
                                "summary_index": 0,
                            }),
                        );
                    }
                    Some("tool_use") => {
                        self.ensure_created();
                        let item_id = format!("fc_{}", self.out.len());
                        let call_id = block.get("id").and_then(Value::as_str).unwrap_or(&item_id).to_string();
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                        let output_index = self.next_output_index;
                        self.next_output_index += 1;
                        self.open = Some(OpenBlock::Tool {
                            item_id: item_id.clone(),
                            call_id: call_id.clone(),
                            name: name.clone(),
                            emitted_args: false,
                            output_index,
                        });
                        self.emit(
                            "response.output_item.added",
                            serde_json::json!({
                                "type": "response.output_item.added",
                                "output_index": output_index,
                                "output_item": {
                                    "id": item_id,
                                    "type": "function_call",
                                    "call_id": call_id,
                                    "name": name,
                                    "arguments": "",
                                    "status": "in_progress",
                                }
                            }),
                        );
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let Some(delta) = json.and_then(|j| j.get("delta")) else { return };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        if text.is_empty() {
                            return;
                        }
                        let (item_id, output_index) = match &self.open {
                            Some(OpenBlock::Text { item_id, output_index }) => (item_id.clone(), *output_index),
                            _ => return,
                        };
                        self.emit(
                            "response.output_text.delta",
                            serde_json::json!({
                                "type": "response.output_text.delta",
                                "item_id": item_id,
                                "output_index": output_index,
                                "content_index": 0,
                                "delta": text,
                            }),
                        );
                    }
                    Some("thinking_delta") => {
                        let text = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                        if text.is_empty() {
                            return;
                        }
                        let (item_id, output_index) = match &mut self.open {
                            Some(OpenBlock::Thinking { item_id, emitted, output_index }) => {
                                *emitted = true;
                                (item_id.clone(), *output_index)
                            }
                            _ => return,
                        };
                        self.emit(
                            "response.reasoning_summary_text.delta",
                            serde_json::json!({
                                "type": "response.reasoning_summary_text.delta",
                                "item_id": item_id,
                                "output_index": output_index,
                                "delta": text,
                            }),
                        );
                    }
                    Some("input_json_delta") => {
                        let text = delta.get("partial_json").and_then(Value::as_str).unwrap_or("");
                        if text.is_empty() {
                            return;
                        }
                        let (item_id, output_index) = match &mut self.open {
                            Some(OpenBlock::Tool { item_id, emitted_args, output_index, .. }) => {
                                *emitted_args = true;
                                (item_id.clone(), *output_index)
                            }
                            _ => return,
                        };
                        self.emit(
                            "response.function_call_arguments.delta",
                            serde_json::json!({
                                "type": "response.function_call_arguments.delta",
                                "item_id": item_id,
                                "output_index": output_index,
                                "delta": text,
                            }),
                        );
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                self.close_open();
            }
            "message_delta" => {
                if let Some(delta) = json.and_then(|j| j.get("delta")) {
                    if let Some(r) = delta.get("stop_reason").and_then(Value::as_str) {
                        self.stop_reason = Some(r.to_string());
                    }
                }
                if let Some(usage) = json.and_then(|j| j.get("usage")) {
                    self.latest_usage = Some(usage.clone());
                }
            }
            "message_stop" => {
                self.finish();
            }
            "error" => {
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
        self.emit(
            "response.failed",
            serde_json::json!({
                "type": "response.failed",
                "error": { "message": message, "type": "nestra_upstream_error" }
            }),
        );
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.ensure_created();
        self.close_open();
        let status = match self.stop_reason.as_deref() {
            Some("max_tokens") => "incomplete",
            _ => "completed",
        };
        let mut resp = Map::new();
        resp.insert("id".into(), Value::String(self.response_id.clone()));
        resp.insert("object".into(), Value::String("response".into()));
        resp.insert("status".into(), Value::String(status.into()));
        resp.insert("model".into(), Value::String(self.model.clone()));
        resp.insert("output".into(), Value::Array(Vec::new()));
        if status == "incomplete" {
            resp.insert(
                "incomplete_details".into(),
                serde_json::json!({ "reason": "max_output_tokens" }),
            );
        }
        if let Some(usage) = self.latest_usage.clone() {
            resp.insert("usage".into(), usage);
        }
        self.emit(
            "response.completed",
            serde_json::json!({ "type": "response.completed", "response": Value::Object(resp) }),
        );
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

impl Body for AnthropicToResponsesStream {
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

#[cfg(test)]
mod tests {
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

    // ---- ChatToResponsesStream (responses inbound, chat upstream) ----

    #[test]
    fn chat_stream_to_responses_events() {
        let upstream = r#"data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"},"finish_reason":null}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}

data: {"id":"chatcmpl-1","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}

data: [DONE]
"#;
        let body = Full::new(frame(upstream));
        let out = collect_all(ChatToResponsesStream::new(body));

        assert!(out.contains("response.created"));
        assert!(out.contains("response.in_progress"));
        assert!(out.contains("response.output_text.delta"));
        assert!(out.contains("\"delta\":\"hi\""));
        assert!(out.contains("response.function_call_arguments.delta"));
        assert!(out.contains("\"name\":\"bash\""));
        assert!(out.contains("response.completed"));
        assert!(out.contains("\"status\":\"completed\""));
        // Usage mapped to the Responses shape (input/output tokens), not the
        // raw chat usage object (prompt/completion tokens).
        assert!(out.contains("\"input_tokens\":10"));
        assert!(out.contains("\"output_tokens\":5"));
    }

    #[test]
    fn chat_stream_to_responses_length_finish_is_incomplete() {
        let upstream = r#"data: {"id":"x","choices":[{"index":0,"delta":{"content":"part"},"finish_reason":"length"}]}

data: [DONE]
"#;
        let body = Full::new(frame(upstream));
        let out = collect_all(ChatToResponsesStream::new(body));
        assert!(out.contains("\"status\":\"incomplete\""));
        assert!(out.contains("\"reason\":\"max_output_tokens\""));
    }

    // ---- AnthropicToResponsesStream (responses inbound, anthropic upstream) ----

    #[test]
    fn anthropic_stream_to_responses_events() {
        let upstream = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","model":"grok-4.5","role":"assistant","content":[]}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"plan"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"done"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":100,"output_tokens":5}}

event: message_stop
data: {"type":"message_stop"}
"#;
        let body = Full::new(frame(upstream));
        let out = collect_all(AnthropicToResponsesStream::new(body));


        assert!(out.contains("response.created"));
        assert!(out.contains("response.in_progress"));
        assert!(out.contains("\"type\":\"reasoning\""));
        assert!(out.contains("response.reasoning_summary_text.delta"));
        assert!(out.contains("\"delta\":\"plan\""));
        assert!(out.contains("response.output_text.delta"));
        assert!(out.contains("\"delta\":\"done\""));
        assert!(out.contains("response.completed"));
        assert!(out.contains("\"status\":\"completed\""));
        assert!(out.contains("\"input_tokens\":100"));
    }

    #[test]
    fn anthropic_stream_error_becomes_response_failed() {
        let upstream = r#"event: error
data: {"type":"error","error":{"type":"nestra_upstream_error","message":"boom"}}
"#;
        let body = Full::new(frame(upstream));
        let out = collect_all(AnthropicToResponsesStream::new(body));
        assert!(out.contains("response.failed"));
        assert!(out.contains("boom"));
    }
}
