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


#[cfg(test)]
mod tests;
