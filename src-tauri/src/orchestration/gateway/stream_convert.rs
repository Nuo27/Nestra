//! OpenAI Chat Completions SSE stream → Anthropic Messages SSE events.
//!
//! When the gateway relays an Anthropic-protocol agent request to an
//! OpenAI-protocol upstream and the upstream answers with a streaming
//! `text/event-stream` of `chat.completion.chunk` frames, this body wrapper
//! rewrites the wire on the fly into the Anthropic event sequence Claude
//! Code expects:
//!
//! ```text
//! message_start → content_block_start/delta/stop* → message_delta → message_stop
//! ```
//!
//! The state machine is deliberately narrow: text deltas and tool-call
//! deltas (keyed by `tool_call.index`) are the only streaming shapes OpenAI
//! emits, and the final usage frame (which the upstream sends because the
//! converted request injects `stream_options.include_usage`) is folded into
//! the deferred `message_delta`. Argument fragments are passed through
//! verbatim — partial JSON is legal mid-stream, never accumulated or
//! validated. Malformed upstream data is emitted as an Anthropic `error`
//! event instead of a corrupted terminal sequence.

use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::{BodyExt, BodyStream};
use hyper::body::{Body, Frame};
use serde_json::{Map, Value};

/// One in-flight tool-call block, keyed by the OpenAI `tool_call.index`.
struct ToolState {
    /// Assigned at `content_block_start` EMISSION time (not at first sight) —
    /// claiming the index earlier lets two tools whose id/name arrive before
    /// either start collide on one index.
    block_index: Option<usize>,
    id: Option<String>,
    name: Option<String>,
    started: bool,
    /// Argument fragments that arrived before id/name did — flushed as one
    /// `input_json_delta` right after `content_block_start`.
    pending_args: Vec<u8>,
}

struct StreamState {
    message_id: Option<String>,
    model: Option<String>,
    sent_message_start: bool,
    next_content_index: usize,
    open_text_index: Option<usize>,
    tool_states: HashMap<u32, ToolState>,
    open_tool_indices: Vec<u32>,
    stop_reason: Option<String>,
    latest_usage: Option<Map<String, Value>>,
    finished: bool,
}

/// SSE-framed `Bytes -> Bytes` Anthropic-event emitter, fed by an upstream
/// OpenAI streaming body.
pub struct OpenAiToAnthropicStream {
    inner: BodyStream<Pin<Box<dyn Body<Data = Bytes, Error = std::io::Error> + Send + Sync>>>,
    state: StreamState,
    /// Bytes waiting to be emitted as frames (each poll drains one SSE event).
    out: Vec<u8>,
    /// Raw upstream bytes not yet split into SSE frames.
    buf: Vec<u8>,
    done: bool,
}

fn sse(event: &str, data: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("event: {event}\n").as_bytes());
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(serde_json::to_string(data).unwrap_or_else(|_| "{}".into()).as_bytes());
    out.extend_from_slice(b"\n\n");
    out
}

fn usage_map(usage: &Value) -> Map<String, Value> {
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

fn map_stop_reason(r: &str) -> &'static str {
    match r {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        "content_filter" => "end_turn",
        _ => "end_turn",
    }
}

impl OpenAiToAnthropicStream {
    pub fn new<B>(inner: B) -> Self
    where
        B: Body<Data = Bytes> + Send + Sync + 'static,
        B::Error: std::fmt::Debug,
    {
        // Normalize any upstream error type to io::Error so the wrapper's
        // Body impl stays uniform (the gateway's relay bodies already carry
        // io::Error; test bodies are infallible). Keep the original error
        // text — a fixed string loses the diagnostic detail.
        let inner = inner.map_err(|e| {
            std::io::Error::other(format!("upstream stream error: {e:?}"))
        });
        OpenAiToAnthropicStream {
            inner: BodyStream::new(Box::pin(inner)),
            state: StreamState {
                message_id: None,
                model: None,
                sent_message_start: false,
                next_content_index: 0,
                open_text_index: None,
                tool_states: HashMap::new(),
                open_tool_indices: Vec::new(),
                stop_reason: None,
                latest_usage: None,
                finished: false,
            },
            out: Vec::new(),
            buf: Vec::new(),
            done: false,
        }
    }

    fn emit(&mut self, event: &str, data: &Value) {
        self.out.extend(sse(event, data));
    }

    /// Split the raw buffer into complete SSE frames. Both `\n\n` and
    /// `\r\n\r\n` separators are accepted; partial frames stay buffered.
    fn take_frames(&mut self) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        loop {
            let double = self.buf.windows(4).position(|w| w == b"\r\n\r\n");
            let single = self.buf.windows(2).position(|w| w == b"\n\n");
            let end = match (double, single) {
                (Some(d), Some(s)) => Some(d.min(s)),
                (a, b) => a.or(b),
            };
            let Some(end) = end else { break };
            let frame: Vec<u8> = self.buf.drain(..end).collect();
            // consume the separator
            if self.buf.starts_with(b"\r\n\r\n") {
                self.buf.drain(..4);
            } else {
                self.buf.drain(..2);
            }
            frames.push(frame);
        }
        frames
    }

    /// Process one `chat.completion.chunk` JSON (or the `[DONE]` marker).
    fn process_frame(&mut self, frame: &[u8]) {
        let text = String::from_utf8_lossy(frame);
        let data = text
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .collect::<Vec<_>>()
            .join("\n");
        let data = data.trim();
        if data == "[DONE]" {
            self.finish_stream();
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            // Not JSON — ignore stray keep-alive lines.
            return;
        };

        // Upstream error envelope (non-2xx passthrough, or a mid-stream
        // failure): emit an Anthropic error event and stop.
        if let Some(err) = chunk.get("error") {
            let mut e_obj = Map::new();
            e_obj.insert("type".into(), Value::String("nestra_upstream_error".into()));
            if let Some(msg) = err.get("message").and_then(Value::as_str) {
                e_obj.insert("message".into(), Value::String(msg.into()));
            }
            let mut ev = Map::new();
            ev.insert("type".into(), Value::String("error".into()));
            ev.insert("error".into(), Value::Object(e_obj));
            self.emit("error", &Value::Object(ev));
            self.state.finished = true;
            return;
        }

        if !self.state.sent_message_start {
            let id = chunk.get("id").and_then(Value::as_str).map(String::from);
            let model = chunk.get("model").and_then(Value::as_str).map(String::from);
            self.state.message_id = id;
            self.state.model = model;
            let mut message = Map::new();
            message.insert("id".into(), Value::String(self.state.message_id.clone().unwrap_or_else(|| "msg_".into())));
            message.insert("type".into(), Value::String("message".into()));
            message.insert("role".into(), Value::String("assistant".into()));
            message.insert("model".into(), Value::String(self.state.model.clone().unwrap_or_default()));
            message.insert("content".into(), Value::Array(Vec::new()));
            message.insert("stop_reason".into(), Value::Null);
            message.insert("stop_sequence".into(), Value::Null);
            let mut start = Map::new();
            start.insert("type".into(), Value::String("message_start".into()));
            start.insert("message".into(), Value::Object(message));
            self.emit("message_start", &Value::Object(start));
            self.state.sent_message_start = true;
        }

        // usage (the final chunk carries it when include_usage was set).
        if let Some(usage) = chunk.get("usage") {
            if !usage.is_null() {
                self.state.latest_usage = Some(usage_map(usage));
            }
        }

        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else { return };
        let Some(first) = choices.first() else { return };
        let Some(delta) = first.get("delta") else { return };

        // Text delta.
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                match self.state.open_text_index {
                    None => {
                        let idx = self.state.next_content_index;
                        let mut cb = Map::new();
                        cb.insert("type".into(), Value::String("text".into()));
                        cb.insert("text".into(), Value::String(String::new()));
                        let mut start = Map::new();
                        start.insert("type".into(), Value::String("content_block_start".into()));
                        start.insert("index".into(), Value::from(idx));
                        start.insert("content_block".into(), Value::Object(cb));
                        self.emit("content_block_start", &Value::Object(start));
                        self.state.open_text_index = Some(idx);
                        self.state.next_content_index = idx + 1;
                    }
                    Some(_) => {}
                }
                let mut d = Map::new();
                d.insert("type".into(), Value::String("text_delta".into()));
                d.insert("text".into(), Value::String(text.into()));
                let mut delta_event = Map::new();
                delta_event.insert("type".into(), Value::String("content_block_delta".into()));
                delta_event.insert("index".into(), Value::from(self.state.open_text_index.unwrap_or(0)));
                delta_event.insert("delta".into(), Value::Object(d));
                self.emit("content_block_delta", &Value::Object(delta_event));
            }
        }

        // Tool-call deltas, keyed by index. State updates and event
        // emission are kept in two passes so the `tool_states` borrow never
        // aliases `self.emit` (which borrows the whole stream mutably).
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            // Pass 1: update per-tool state; collect (idx, start, args) to emit.
            let mut starts: Vec<(u32, usize, String, String)> = Vec::new();
            let mut args_deltas: Vec<(u32, usize, String)> = Vec::new();
            for call in calls {
                let idx = call.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                let entry = self.state.tool_states.entry(idx).or_insert_with(|| ToolState {
                    block_index: None,
                    id: None,
                    name: None,
                    started: false,
                    pending_args: Vec::new(),
                });
                if let Some(func) = call.get("function") {
                    if let Some(name) = func.get("name").and_then(Value::as_str) {
                        entry.name = Some(name.to_string());
                    }
                    if let Some(args) = func.get("arguments").and_then(Value::as_str) {
                        if entry.started {
                            if let Some(bi) = entry.block_index {
                                args_deltas.push((idx, bi, args.to_string()));
                            }
                        } else {
                            entry.pending_args.extend_from_slice(args.as_bytes());
                        }
                    }
                }
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    entry.id = Some(id.to_string());
                }
                // Start only with a NON-EMPTY name — an empty-string name is
                // an illegal tool_use and must not emit a start event.
                let name_ready = entry
                    .name
                    .as_deref()
                    .map(|n| !n.is_empty())
                    .unwrap_or(false);
                if !entry.started && name_ready {
                    let bi = self.state.next_content_index;
                    entry.block_index = Some(bi);
                    let id = entry.id.clone().unwrap_or_default();
                    let name = entry.name.clone().unwrap_or_default();
                    starts.push((idx, bi, id, name));
                    entry.started = true;
                    self.state.next_content_index = bi + 1;
                    self.state.open_tool_indices.push(idx);
                }
            }
            // Pass 2: emit the collected events (borrows are free now).
            for (idx, bi, id, name) in starts {
                let mut cb = Map::new();
                cb.insert("type".into(), Value::String("tool_use".into()));
                cb.insert("id".into(), Value::String(id));
                cb.insert("name".into(), Value::String(name));
                cb.insert("input".into(), Value::Object(Map::new()));
                let mut start = Map::new();
                start.insert("type".into(), Value::String("content_block_start".into()));
                start.insert("index".into(), Value::from(bi));
                start.insert("content_block".into(), Value::Object(cb));
                self.emit("content_block_start", &Value::Object(start));
                // Flush any argument fragments that arrived before start.
                if let Some(entry) = self.state.tool_states.get_mut(&idx) {
                    if !entry.pending_args.is_empty() {
                        let args = std::mem::take(&mut entry.pending_args);
                        self.emit_tool_args_delta(idx, bi, &args);
                    }
                }
            }
            for (idx, bi, text) in args_deltas {
                self.emit_tool_args_delta(idx, bi, text.as_bytes());
            }
        }

        // finish_reason — hold it for message_delta at [DONE].
        if let Some(reason) = first.get("finish_reason").and_then(Value::as_str) {
            self.state.stop_reason = Some(map_stop_reason(reason).to_string());
        }
    }

    fn emit_tool_args_delta(&mut self, _idx: u32, block_index: usize, args: &[u8]) {
        let text = String::from_utf8_lossy(args).into_owned();
        let mut d = Map::new();
        d.insert("type".into(), Value::String("input_json_delta".into()));
        d.insert("partial_json".into(), Value::String(text));
        let mut delta_event = Map::new();
        delta_event.insert("type".into(), Value::String("content_block_delta".into()));
        delta_event.insert("index".into(), Value::from(block_index));
        delta_event.insert("delta".into(), Value::Object(d));
        self.emit("content_block_delta", &Value::Object(delta_event));
    }

    /// Close open blocks, emit the deferred message_delta + message_stop.
    fn finish_stream(&mut self) {
        if self.state.finished {
            return;
        }
        self.state.finished = true;
        // An empty/early-EOF upstream never produced a first chunk, so no
        // message_start was emitted — Claude Code requires the stream to open
        // with one, or it rejects the whole response. Emit a minimal start
        // before the delta/stop sequence.
        if !self.state.sent_message_start {
            let mut message = Map::new();
            message.insert("id".into(), Value::String("msg_".into()));
            message.insert("type".into(), Value::String("message".into()));
            message.insert("role".into(), Value::String("assistant".into()));
            message.insert("model".into(), Value::String(String::new()));
            message.insert("content".into(), Value::Array(Vec::new()));
            message.insert("stop_reason".into(), Value::Null);
            message.insert("stop_sequence".into(), Value::Null);
            let mut start = Map::new();
            start.insert("type".into(), Value::String("message_start".into()));
            start.insert("message".into(), Value::Object(message));
            self.emit("message_start", &Value::Object(start));
            self.state.sent_message_start = true;
        }
        if let Some(text_idx) = self.state.open_text_index.take() {
            let mut stop = Map::new();
            stop.insert("type".into(), Value::String("content_block_stop".into()));
            stop.insert("index".into(), Value::from(text_idx));
            self.emit("content_block_stop", &Value::Object(stop));
        }
        let open_tools: Vec<usize> = self
            .state
            .open_tool_indices
            .drain(..)
            .filter_map(|idx| self.state.tool_states.get(&idx).and_then(|t| t.block_index))
            .collect();
        for bi in open_tools {
            let mut stop = Map::new();
            stop.insert("type".into(), Value::String("content_block_stop".into()));
            stop.insert("index".into(), Value::from(bi));
            self.emit("content_block_stop", &Value::Object(stop));
        }
        let mut delta = Map::new();
        delta.insert("type".into(), Value::String("message_delta".into()));
        let mut d = Map::new();
        d.insert("stop_reason".into(), Value::String(self.state.stop_reason.clone().unwrap_or_else(|| "end_turn".into())));
        d.insert("stop_sequence".into(), Value::Null);
        delta.insert("delta".into(), Value::Object(d));
        if let Some(usage) = self.state.latest_usage.clone() {
            delta.insert("usage".into(), Value::Object(usage));
        }
        self.emit("message_delta", &Value::Object(delta));
        self.emit("message_stop", &Value::Object(Map::new()));
    }
}

impl Body for OpenAiToAnthropicStream {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        // All fields of `Self` are `Unpin` (Vec/String/Option/Box), so the
        // safe `Pin::get_mut` projection applies — no unsafe needed here.
        let this = self.get_mut();

        loop {
            // Drain pending output first.
            if !this.out.is_empty() {
                let bytes = std::mem::take(&mut this.out);
                return Poll::Ready(Some(Ok(Frame::data(Bytes::from(bytes)))));
            }
            if this.done {
                return Poll::Ready(None);
            }

            // Poll the inner body first and drop the borrow, then mutate
            // `this` (buffer/frames/state) — avoids aliasing the pinned
            // projection with the &mut self below.
            let polled = Pin::new(&mut this.inner).poll_frame(cx);
            match polled {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        this.buf.extend_from_slice(&data);
                        let frames = this.take_frames();
                        for f in frames {
                            this.process_frame(&f);
                        }
                        if this.state.finished {
                            this.done = true;
                            continue; // emit the remaining output next loop
                        }
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    // Surface upstream failure as an Anthropic error event.
                    let mut err = Map::new();
                    err.insert("type".into(), Value::String("error".into()));
                    let mut e_obj = Map::new();
                    e_obj.insert("type".into(), Value::String("nestra_upstream_error".into()));
                    e_obj.insert("message".into(), Value::String(e.to_string()));
                    err.insert("error".into(), Value::Object(e_obj));
                    this.emit("error", &Value::Object(err));
                    this.state.finished = true;
                    this.done = true;
                    continue;
                }
                Poll::Ready(None) => {
                    // Upstream ended without [DONE] — close cleanly anyway.
                    this.finish_stream();
                    this.done = true;
                    continue;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Anthropic Messages SSE → OpenAI Chat Completions SSE — the mirror of
/// `OpenAiToAnthropicStream`, used when a chat-wire agent (opencode/pi) is
/// bridged to an Anthropic-protocol upstream. The relay already observed
/// usage on the anthropic frames, so this wrapper is pure synchronous
/// wire-rewriting: text deltas → `delta.content`, tool blocks →
/// `delta.tool_calls`, `message_delta` → finish_reason + usage chunk,
/// `message_stop` → `[DONE]`. Malformed/unknown events are skipped; a
/// mid-stream upstream failure emits a chat `{"error":…}` then `[DONE]`
/// instead of a corrupted stream.
pub struct AnthropicToChatStream {
    inner: BodyStream<Pin<Box<dyn Body<Data = Bytes, Error = std::io::Error> + Send + Sync>>>,
    message_id: Option<String>,
    model: Option<String>,
    sent_first_chunk: bool,
    /// content-block index → chat tool_call index (allocated at
    /// content_block_start; `input_json_delta`s address blocks by index).
    tool_by_block: HashMap<usize, u32>,
    next_tool_index: u32,
    finished: bool,
    /// Bytes waiting to be emitted (one SSE event per poll drain).
    out: Vec<u8>,
    /// Raw upstream bytes not yet split into SSE frames.
    buf: Vec<u8>,
    done: bool,
}

impl AnthropicToChatStream {
    pub fn new<B>(inner: B) -> Self
    where
        B: Body<Data = Bytes> + Send + Sync + 'static,
        B::Error: std::fmt::Debug,
    {
        let inner = inner.map_err(|e| std::io::Error::other(format!("upstream stream error: {e:?}")));
        AnthropicToChatStream {
            inner: BodyStream::new(Box::pin(inner)),
            message_id: None,
            model: None,
            sent_first_chunk: false,
            tool_by_block: HashMap::new(),
            next_tool_index: 0,
            finished: false,
            out: Vec::new(),
            buf: Vec::new(),
            done: false,
        }
    }

    fn emit_data(&mut self, payload: serde_json::Value) {
        self.out
            .extend_from_slice(b"data: ");
        self.out.extend_from_slice(
            serde_json::to_string(&payload)
                .unwrap_or_else(|_| "{}".into())
                .as_bytes(),
        );
        self.out.extend_from_slice(b"\n\n");
    }

    /// A chat chunk with the given `choices` array (and optional usage).
    fn emit_chunk(&mut self, choices: Value, usage: Option<Value>) {
        let mut chunk = Map::new();
        chunk.insert(
            "id".into(),
            Value::String(self.message_id.clone().unwrap_or_else(|| "chatcmpl-".into())),
        );
        chunk.insert("object".into(), Value::String("chat.completion.chunk".into()));
        if let Some(m) = &self.model {
            chunk.insert("model".into(), Value::String(m.clone()));
        }
        chunk.insert("choices".into(), choices);
        if let Some(u) = usage {
            chunk.insert("usage".into(), u);
        }
        self.emit_data(Value::Object(chunk));
    }

    /// The stream must open with a role-announcing empty chunk; the first
    /// processable frame emits it lazily.
    fn ensure_first(&mut self) {
        if self.sent_first_chunk {
            return;
        }
        self.sent_first_chunk = true;
        let mut delta = Map::new();
        delta.insert("role".into(), Value::String("assistant".into()));
        let mut choice = Map::new();
        choice.insert("index".into(), Value::from(0));
        choice.insert("delta".into(), Value::Object(delta));
        choice.insert("finish_reason".into(), Value::Null);
        self.emit_chunk(Value::Array(vec![Value::Object(choice)]), None);
    }

    /// Split the raw buffer into complete SSE frames (both separator styles).
    fn take_frames(&mut self) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        loop {
            let double = self.buf.windows(4).position(|w| w == b"\r\n\r\n");
            let single = self.buf.windows(2).position(|w| w == b"\n\n");
            let end = match (double, single) {
                (Some(d), Some(s)) => Some(d.min(s)),
                (a, b) => a.or(b),
            };
            let Some(end) = end else { break };
            let frame: Vec<u8> = self.buf.drain(..end).collect();
            if self.buf.starts_with(b"\r\n\r\n") {
                self.buf.drain(..4);
            } else {
                self.buf.drain(..2);
            }
            frames.push(frame);
        }
        frames
    }

    fn map_stop_reason(r: &str) -> &'static str {
        match r {
            "tool_use" => "tool_calls",
            "max_tokens" => "length",
            _ => "stop",
        }
    }

    fn emit_done(&mut self) {
        self.out.extend_from_slice(b"data: [DONE]\n\n");
    }

    fn emit_finish_and_done(&mut self, stop_reason: &str, usage: Option<Value>) {
        let delta = Map::new();
        let mut choice = Map::new();
        choice.insert("index".into(), Value::from(0));
        choice.insert("delta".into(), Value::Object(delta));
        choice.insert("finish_reason".into(), Value::String(Self::map_stop_reason(stop_reason).into()));
        self.emit_chunk(Value::Array(vec![Value::Object(choice)]), usage);
        self.emit_done();
        self.finished = true;
    }

    /// Process one anthropic SSE frame (event + data lines).
    fn process_frame(&mut self, frame: &[u8]) {
        let text = String::from_utf8_lossy(frame);
        let mut event = "";
        let mut data_lines: Vec<&str> = Vec::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                event = rest.trim();
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest);
            }
        }
        let data = data_lines.join("\n");
        let data = data.trim();
        if data.is_empty() {
            return;
        }
        // Anthropic `error` events → a chat error chunk, then [DONE].
        if event == "error" {
            let mut e_obj = Map::new();
            e_obj.insert("type".into(), Value::String("nestra_upstream_error".into()));
            if let Some(v) = serde_json::from_str::<Value>(data).ok() {
                let message = v
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str);
                if let Some(m) = message {
                    e_obj.insert("message".into(), Value::String(m.into()));
                }
            }
            let mut err = Map::new();
            err.insert("error".into(), Value::Object(e_obj));
            self.emit_data(Value::Object(err));
            self.emit_done();
            self.finished = true;
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { return };
        match event {
            "message_start" => {
                if let Some(message) = v.get("message") {
                    if let Some(id) = message.get("id").and_then(Value::as_str) {
                        self.message_id = Some(id.to_string());
                    }
                    if let Some(m) = message.get("model").and_then(Value::as_str) {
                        self.model = Some(m.to_string());
                    }
                }
                self.ensure_first();
            }
            "content_block_start" => {
                self.ensure_first();
                let Some(cb) = v.get("content_block") else { return };
                if cb.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let block_idx = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let tool_idx = self.next_tool_index;
                    self.next_tool_index += 1;
                    self.tool_by_block.insert(block_idx, tool_idx);
                    let mut tc = Map::new();
                    tc.insert("index".into(), Value::from(tool_idx));
                    if let Some(id) = cb.get("id").and_then(Value::as_str) {
                        tc.insert("id".into(), Value::String(id.into()));
                    }
                    let mut function = Map::new();
                    if let Some(name) = cb.get("name").and_then(Value::as_str) {
                        function.insert("name".into(), Value::String(name.into()));
                    }
                    function.insert("arguments".into(), Value::String(String::new()));
                    tc.insert("function".into(), Value::Object(function));
                    let mut delta = Map::new();
                    delta.insert("tool_calls".into(), Value::Array(vec![Value::Object(tc)]));
                    let mut choice = Map::new();
                    choice.insert("index".into(), Value::from(0));
                    choice.insert("delta".into(), Value::Object(delta));
                    choice.insert("finish_reason".into(), Value::Null);
                    self.emit_chunk(Value::Array(vec![Value::Object(choice)]), None);
                }
            }
            "content_block_delta" => {
                self.ensure_first();
                let Some(delta) = v.get("delta") else { return };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = delta.get("text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                let mut d = Map::new();
                                d.insert("content".into(), Value::String(t.into()));
                                let mut choice = Map::new();
                                choice.insert("index".into(), Value::from(0));
                                choice.insert("delta".into(), Value::Object(d));
                                choice.insert("finish_reason".into(), Value::Null);
                                self.emit_chunk(Value::Array(vec![Value::Object(choice)]), None);
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        let block_idx = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let Some(&tool_idx) = self.tool_by_block.get(&block_idx) else { return };
                        let partial = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let mut tc = Map::new();
                        tc.insert("index".into(), Value::from(tool_idx));
                        let mut function = Map::new();
                        function.insert("arguments".into(), Value::String(partial.into()));
                        tc.insert("function".into(), Value::Object(function));
                        let mut d = Map::new();
                        d.insert("tool_calls".into(), Value::Array(vec![Value::Object(tc)]));
                        let mut choice = Map::new();
                        choice.insert("index".into(), Value::from(0));
                        choice.insert("delta".into(), Value::Object(d));
                        choice.insert("finish_reason".into(), Value::Null);
                        self.emit_chunk(Value::Array(vec![Value::Object(choice)]), None);
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                self.ensure_first();
                let reason = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("end_turn");
                let usage = v
                    .get("usage")
                    .filter(|u| !u.is_null())
                    .map(chat_usage_from_anthropic);
                self.emit_finish_and_done(reason, usage.flatten());
            }
            "message_stop" => {
                if !self.finished {
                    let delta = Map::new();
                    let mut choice = Map::new();
                    choice.insert("index".into(), Value::from(0));
                    choice.insert("delta".into(), Value::Object(delta));
                    choice.insert("finish_reason".into(), Value::String("stop".into()));
                    self.emit_chunk(Value::Array(vec![Value::Object(choice)]), None);
                    self.emit_done();
                    self.finished = true;
                }
            }
            // ping / keepalive / anything else: ignore.
            _ => {}
        }
    }
}

impl Body for AnthropicToChatStream {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        loop {
            if !this.out.is_empty() {
                let bytes = std::mem::take(&mut this.out);
                return Poll::Ready(Some(Ok(Frame::data(Bytes::from(bytes)))));
            }
            if this.done {
                return Poll::Ready(None);
            }
            let polled = Pin::new(&mut this.inner).poll_frame(cx);
            match polled {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        this.buf.extend_from_slice(&data);
                        let frames = this.take_frames();
                        for f in frames {
                            this.process_frame(&f);
                        }
                        if this.finished {
                            this.done = true;
                            continue;
                        }
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    // Upstream died mid-stream: emit a chat error + [DONE].
                    let mut err = Map::new();
                    let mut e_obj = Map::new();
                    e_obj.insert("type".into(), Value::String("nestra_upstream_error".into()));
                    e_obj.insert("message".into(), Value::String(e.to_string()));
                    err.insert("error".into(), Value::Object(e_obj));
                    this.emit_data(Value::Object(err));
                    this.done = true;
                    continue;
                }
                Poll::Ready(None) => {
                    // Upstream ended without message_stop — close cleanly.
                    if !this.finished {
                        let delta = Map::new();
                        let mut choice = Map::new();
                        choice.insert("index".into(), Value::from(0));
                        choice.insert("delta".into(), Value::Object(delta));
                        choice.insert("finish_reason".into(), Value::String("stop".into()));
                        this.emit_chunk(Value::Array(vec![Value::Object(choice)]), None);
                        this.emit_done();
                        this.finished = true;
                    }
                    this.done = true;
                    continue;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Anthropic usage map → OpenAI chat usage (`prompt_tokens` /
/// `completion_tokens` / `prompt_tokens_details.cached_tokens`), mirroring
/// `convert::anthropic_to_chat`'s buffered mapping for the stream path.
fn chat_usage_from_anthropic(u: &Value) -> Option<Value> {
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

#[cfg(test)]
mod anthropic_to_chat_tests;

#[cfg(test)]
mod tests;
