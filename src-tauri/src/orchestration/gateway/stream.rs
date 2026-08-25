//! Body + streaming utilities for the gateway.
//!
//! Two concerns:
//!   1. Reading the inbound request body fully enough to rewrite the `model`
//!      field and record usage — Anthropic Messages are JSON, so we buffer the
//!      whole request body (they are not large relative to context windows).
//!   2. Streaming the upstream response back to the agent. Anthropic Messages
//!      uses SSE (`text/event-stream`) for streaming responses; we pass the
//!      bytes through while observing the final `message_start` / `message_delta`
//!      usage block to capture input/output/cache tokens.
//!
//! Scope: read-whole-request, forward, stream-back-verbatim with usage
//! observation. No body mutation beyond the model-field rewrite (the Anthropic
//! path also applies policy-gated `cache_control` injection).

use std::collections::{BTreeMap, HashSet};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::{BodyExt, BodyStream, Full};
use hyper::body::{Body, Frame};

use crate::config_writer::ProviderKind;
use crate::error::AppResult;

/// The gateway's response body type. Either a full buffered body (for JSON
/// error responses) or a streaming body (for proxied SSE).
pub enum GatewayBody {
    /// A complete, in-memory body (used for our own JSON error responses).
    Full(Full<Bytes>),
    /// A streaming body forwarding upstream bytes (used for proxied responses,
    /// incl. SSE). Wraps a `hyper::body::Body` from the upstream connection.
    Stream(BodyStream<Pin<Box<dyn Body<Data = Bytes, Error = std::io::Error> + Send + Sync>>>),
}

impl Body for GatewayBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        // Safety: we project to the enum variant. Both inner bodies are `Unpin`
        // in practice (`Full` is; `BodyStream` wraps a pinned box). This
        // manual projection avoids pulling in `pin-project-lite` for one enum.
        let this = unsafe { self.get_unchecked_mut() };
        match this {
            GatewayBody::Full(f) => {
                unsafe { Pin::new_unchecked(f) }
                    .poll_frame(cx)
                    .map_err(|e| {
                        // `Full<Bytes>` is infallible, but the trait requires it.
                        std::io::Error::other(format!("full body: {e:?}"))
                    })
            }
            GatewayBody::Stream(s) => unsafe { Pin::new_unchecked(s) }.poll_frame(cx),
        }
    }

    /// Forward the inner body's size hint. Without this the trait default
    /// ("unknown length") makes hyper encode EVERY buffered upstream request
    /// as `transfer-encoding: chunked` — opencode-go's edge holds chunked
    /// request bodies for ~60-90s and then 503s "Endpoint is unavailable"
    /// (the ox-alpha-free routed-503 root cause; direct clients send
    /// content-length and work). A `Full` body reports its exact length so
    /// hyper emits `content-length` instead.
    fn size_hint(&self) -> hyper::body::SizeHint {
        match self {
            GatewayBody::Full(f) => f.size_hint(),
            GatewayBody::Stream(_) => hyper::body::SizeHint::new(),
        }
    }
}

impl GatewayBody {
    /// Build a JSON error response body.
    pub fn json_full(json: serde_json::Value) -> Self {
        let bytes = serde_json::to_vec(&json).unwrap_or_else(|_| b"{}".to_vec());
        GatewayBody::Full(Full::new(Bytes::from(bytes)))
    }

    /// Build a streaming body from an upstream response body.
    pub fn streaming<B>(body: B) -> Self
    where
        B: Body<Data = Bytes, Error = std::io::Error> + Send + Sync + 'static,
    {
        GatewayBody::Stream(BodyStream::new(Box::pin(body)))
    }
}

/// Read an inbound request body fully into bytes. Anthropic Messages bodies
/// are JSON (not chunked uploads), so buffering the whole thing is fine and
/// lets us rewrite the `model` field before forwarding.
pub async fn read_request_body<B>(body: B) -> AppResult<Bytes>
where
    B: Body,
    B::Error: std::fmt::Display,
{
    use http_body_util::BodyExt;
    let bytes = body
        .collect()
        .await
        .map_err(|e| crate::error::AppError::Upstream(format!("read request body: {e}")))?
        .to_bytes();
    // Debug-only wire evidence (see gateway/trace.rs): the raw inbound body
    // BEFORE any model rewrite or cross-wire conversion.
    tracing::debug!(bytes = bytes.len(), body = %super::trace::capture(&bytes), "gw.request body");
    Ok(bytes)
}

/// Observed usage from an Anthropic Messages response. Extracted from the
/// `usage` block in `message_start` (input tokens, cache creation/read) and
/// `message_delta` (output tokens). `None` fields when absent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ObservedUsage {
    pub input: Option<i64>,
    pub output: Option<i64>,
    pub cache_creation: Option<i64>,
    pub cache_read: Option<i64>,
}

/// Walk an SSE buffer's `data:` payloads, invoking `f` for each parsed JSON
/// object. `[DONE]` sentinels and unparseable/non-object lines are skipped —
/// the shared preamble of the per-wire observers below.
fn for_each_data_payload(
    text: &str,
    mut f: impl FnMut(&serde_json::Map<String, serde_json::Value>),
) {
    for line in text.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        if let Some(obj) = v.as_object() {
            f(obj);
        }
    }
}

/// Parse usage tokens out of an SSE event-stream chunk. Anthropic streams the
/// `usage` object inside `message_start` (input + cache fields) and
/// `message_delta` (output). This scans one buffered chunk of decoded text for
/// those fields and accumulates into `usage`. Best-effort: a malformed chunk
/// is ignored (the next chunk may complete it).
fn observe_usage_chunk(text: &str, usage: &mut ObservedUsage) {
    // We only care about `message_start` and `message_delta` event types.
    // Walk data: lines, parse their JSON, and pull usage fields.
    for_each_data_payload(text, |obj| {
        // `message_start` carries { message: { usage: { ... } } }
        if obj.get("type").and_then(|t| t.as_str()) == Some("message_start") {
            if let Some(u) = obj
                .get("message")
                .and_then(|m| m.get("usage"))
                .and_then(|u| u.as_object())
            {
                merge_usage_obj(u, usage);
            }
            return;
        }
        // `message_delta` carries { usage: { output_tokens: N } }
        if obj.get("type").and_then(|t| t.as_str()) == Some("message_delta") {
            if let Some(u) = obj.get("usage").and_then(|u| u.as_object()) {
                merge_usage_obj(u, usage);
            }
        }
    });
}

/// Merge a parsed `usage` object into the accumulator. `pub(crate)` so the
/// protocol handlers' non-streaming paths share the field walk.
pub(crate) fn merge_usage_obj(u: &serde_json::Map<String, serde_json::Value>, usage: &mut ObservedUsage) {
    // Anthropic field names.
    if let Some(n) = u.get("input_tokens").and_then(|x| x.as_i64()) {
        usage.input = Some(n);
    }
    if let Some(n) = u.get("output_tokens").and_then(|x| x.as_i64()) {
        // `message_delta` carries the cumulative output; take the last value.
        usage.output = Some(n);
    }
    if let Some(n) = u.get("cache_creation_input_tokens").and_then(|x| x.as_i64()) {
        usage.cache_creation = Some(n);
    }
    if let Some(n) = u.get("cache_read_input_tokens").and_then(|x| x.as_i64()) {
        usage.cache_read = Some(n);
    }
    // OpenAI field names (chat completions): prompt_tokens/completion_tokens
    // and prompt_tokens_details.cached_tokens. Without this the OpenAI
    // buffered path observed NO usage at all — quota/health silently read
    // zeros for every OpenAI endpoint.
    if usage.input.is_none() {
        if let Some(n) = u.get("prompt_tokens").and_then(|x| x.as_i64()) {
            usage.input = Some(n);
        }
    }
    if usage.output.is_none() {
        if let Some(n) = u.get("completion_tokens").and_then(|x| x.as_i64()) {
            usage.output = Some(n);
        }
    }
    if usage.cache_read.is_none() {
        if let Some(n) = u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            // Responses API nests the same number under input_tokens_details.
            .or_else(|| {
                u.get("input_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
            })
            .and_then(|x| x.as_i64())
        {
            usage.cache_read = Some(n);
        }
    }
}

/// Tool calls in a buffered (non-SSE) response body, per the upstream wire.
/// Mirrors the SSE observers' semantics: distinct call ids for the count,
/// name counts per observed invocation. `(None, None)` when the body carries
/// no tool calls (or isn't JSON).
pub fn tools_in_buffered_body(
    body: &[u8],
    wire: ProviderKind,
) -> (Option<i64>, Option<BTreeMap<String, u64>>) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (None, None);
    };
    let mut ids: HashSet<String> = HashSet::new();
    let mut names = BTreeMap::new();
    let mut visit = |id: Option<&str>, name: Option<&str>| {
        if let Some(id) = id.filter(|s| !s.is_empty()) {
            ids.insert(id.to_string());
        }
        if let Some(name) = name {
            bump(&mut names, name);
        }
    };
    match wire {
        ProviderKind::Anthropic => {
            if let Some(blocks) = v.get("content").and_then(|c| c.as_array()) {
                for b in blocks {
                    if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        visit(
                            b.get("id").and_then(|x| x.as_str()),
                            b.get("name").and_then(|x| x.as_str()),
                        );
                    }
                }
            }
        }
        ProviderKind::Openai => {
            if let Some(choices) = v.get("choices").and_then(|c| c.as_array()) {
                for ch in choices {
                    let Some(tcs) = ch
                        .get("message")
                        .and_then(|m| m.get("tool_calls"))
                        .and_then(|t| t.as_array())
                    else {
                        continue;
                    };
                    for tc in tcs {
                        visit(
                            tc.get("id").and_then(|x| x.as_str()),
                            tc.get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|x| x.as_str()),
                        );
                    }
                }
            }
        }
        ProviderKind::Responses => {
            if let Some(items) = v.get("output").and_then(|o| o.as_array()) {
                for it in items {
                    if it.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                        visit(
                            it.get("call_id").or_else(|| it.get("id")).and_then(|x| x.as_str()),
                            it.get("name").and_then(|x| x.as_str()),
                        );
                    }
                }
            }
        }
        ProviderKind::Custom => {}
    }
    if ids.is_empty() && names.is_empty() {
        return (None, None);
    }
    (Some(ids.len() as i64), Some(names))
}

/// What one relayed SSE stream yielded: usage tokens plus the distinct tool
/// calls seen in it. Accumulated across frames by the observing relay body
/// (`protocol_anthropic::ObservingBody`); `tool_call_ids` is a set because the
/// count, not the order or the names, is what v1 persists (`route_request.
/// tool_calls`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamObservation {
    pub usage: ObservedUsage,
    pub tool_call_ids: HashSet<String>,
    /// Observed tool-call invocations per tool NAME (counts, not successes —
    /// the gateway counts what it saw in the stream; a later execution
    /// failure or connection drop does not un-count it).
    pub tool_names: BTreeMap<String, u64>,
}

fn bump(map: &mut BTreeMap<String, u64>, name: &str) {
    if !name.is_empty() {
        *map.entry(name.to_string()).or_insert(0) += 1;
    }
}

/// Observe one buffered text window of an Anthropic Messages SSE stream.
/// Usage rides `message_start`/`message_delta` (see [`observe_usage_chunk`]);
/// tool calls are announced by `content_block_start` blocks of type
/// `tool_use`, deduplicated by block id.
pub fn observe_anthropic_chunk(text: &str, obs: &mut StreamObservation) {
    observe_usage_chunk(text, &mut obs.usage);
    for_each_data_payload(text, |obj| {
        if obj.get("type").and_then(|t| t.as_str()) != Some("content_block_start") {
            return;
        }
        let block = obj.get("content_block");
        let is_tool = block
            .and_then(|b| b.get("type"))
            .and_then(|t| t.as_str())
            == Some("tool_use");
        if is_tool {
            if let Some(id) = block.and_then(|b| b.get("id")).and_then(|i| i.as_str()) {
                obs.tool_call_ids.insert(id.to_string());
            }
            if let Some(name) = block.and_then(|b| b.get("name")).and_then(|n| n.as_str()) {
                bump(&mut obs.tool_names, name);
            }
        }
    });
}

/// Observe one buffered text window of an OpenAI Chat Completions SSE stream.
/// Usage appears as a top-level `usage` on the final chunk (only when the
/// request set `stream_options.include_usage` — the gateway's Anthropic→OpenAI
/// bridge injects it, native OpenAI agents may not). Tool-call deltas arrive
/// on `choices[].delta.tool_calls`; `index` is constant per call and present
/// on every delta, so it is the dedup key (the `id` rides only the first
/// delta and would double-count).
pub fn observe_openai_chat_chunk(text: &str, obs: &mut StreamObservation) {
    for_each_data_payload(text, |obj| {
        if let Some(u) = obj.get("usage").and_then(|u| u.as_object()) {
            merge_usage_obj(u, &mut obs.usage);
        }
        let Some(choices) = obj.get("choices").and_then(|c| c.as_array()) else {
            return;
        };
        for choice in choices {
            let Some(tcs) = choice
                .get("delta")
                .and_then(|d| d.get("tool_calls"))
                .and_then(|t| t.as_array())
            else {
                continue;
            };
            for tc in tcs {
                let key = tc
                    .get("index")
                    .and_then(|i| i.as_u64())
                    .map(|i| format!("idx:{i}"))
                    .or_else(|| {
                        tc.get("id")
                            .and_then(|i| i.as_str())
                            .map(str::to_string)
                    });
                if let Some(k) = key {
                    obs.tool_call_ids.insert(k);
                }
                // The name rides only the FIRST delta of a call (continuation
                // deltas carry just the index) — count once per named delta.
                if let Some(name) = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                {
                    bump(&mut obs.tool_names, name);
                }
            }
        }
    });
}

/// Observe one buffered text window of an OpenAI Responses API SSE stream
/// (the upstream wire when the gateway bridges to a `responses` endpoint).
/// `response.completed` / `response.incomplete` carry `response.usage`
/// (Anthropic-style `input_tokens`/`output_tokens`, cache under
/// `input_tokens_details.cached_tokens`); tool calls surface as
/// `function_call` output items, announced by `response.output_item.added`.
pub fn observe_responses_chunk(text: &str, obs: &mut StreamObservation) {
    for_each_data_payload(text, |obj| {
        match obj.get("type").and_then(|t| t.as_str()) {
            Some("response.completed") | Some("response.incomplete") => {
                if let Some(u) = obj
                    .get("response")
                    .and_then(|r| r.get("usage"))
                    .and_then(|u| u.as_object())
                {
                    merge_usage_obj(u, &mut obs.usage);
                }
            }
            Some("response.output_item.added") => {
                let is_call = obj
                    .get("item")
                    .and_then(|i| i.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("function_call");
                if is_call {
                    if let Some(id) = obj
                        .get("item")
                        .and_then(|i| i.get("call_id").or_else(|| i.get("id")))
                        .and_then(|i| i.as_str())
                    {
                        obs.tool_call_ids.insert(id.to_string());
                    }
                    if let Some(name) =
                        obj.get("item").and_then(|i| i.get("name")).and_then(|n| n.as_str())
                    {
                        bump(&mut obs.tool_names, name);
                    }
                }
            }
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests;

// ===========================================================================
// First-event in-band error probe
//
// Some upstreams (observed: opencode-go's free models on the chat wire)
// answer a 200 + SSE stream that terminates IN-BAND with an error-valued
// terminal `finish_reason` (e.g. "network_error") and zero generated
// content. Relaying that verbatim hands the agent a "successful" empty
// response and the retry/migration machinery never fires. The probe reads
// the FIRST complete SSE event before the gateway commits to relaying: an
// in-band terminal error fails the attempt (the loop then retries / walks
// the policy's route-target list); anything healthy is prepended back.
// ===========================================================================

/// The boxed body type shared by the probe and `PrependBody`.
pub(crate) type SharedBody = Pin<Box<dyn Body<Data = Bytes, Error = std::io::Error> + Send + Sync>>;

/// Result of probing a 2xx SSE upstream's first complete event.
pub(crate) enum FirstEventProbe {
    /// The first event is healthy or indeterminate. `held` carries every
    /// byte read during the probe and must be relayed before `rest`.
    Ok { held: Bytes, rest: SharedBody },
    /// The stream terminated in-band with no generated content (an
    /// error-valued terminal `finish_reason`, `response.failed`, or an error
    /// JSON envelope) — the attempt must fail so the retry/migration loop
    /// can act on it.
    InBandError { reason: String },
}

/// Hard cap on probe buffering: the first event of a healthy stream is a
/// single small chunk; anything larger is passed through unprobed.
const PROBE_CAP: usize = 256 * 1024;

/// Probe the FIRST complete SSE event of a 2xx upstream response. The probe
/// window (first-event timeout from the live tuning) bounds an upstream that
/// opens the stream and then zero-byte hangs (observed on opencode-go) — the
/// attempt fails in bounded time so the agent gets a prompt, honest error
/// instead of a multi-minute hang.
pub(crate) async fn probe_first_sse_event<B>(body: B, timeout: std::time::Duration) -> FirstEventProbe
where
    B: Body<Data = Bytes> + Send + Sync + 'static,
    B::Error: std::fmt::Display,
{
    // Box (and normalize the error type — hyper::Error for `Incoming`,
    // io::Error for wrapped bodies) up front: `Pin<&mut dyn Body>` is Unpin
    // so `frame()` polls fine, and the remainder moves out unchanged.
    let mut body: SharedBody =
        Box::pin(body.map_err(|_| std::io::Error::other("upstream stream error")));
    // The whole probe (accumulate until the first complete event) is capped —
    // an upstream that opens the stream and then zero-byte hangs fails here
    // in bounded time and the attempt is treated as an in-band failure.
    let probe = tokio::time::timeout(timeout, async {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if let Some(boundary) = find_event_boundary(&buf) {
            if let Some(reason) = first_event_is_inband_error(&buf[..boundary]) {
                return FirstEventProbe::InBandError { reason };
            }
            return FirstEventProbe::Ok {
                held: Bytes::from(std::mem::take(&mut buf)),
                rest: body,
            };
        }
        if buf.len() > PROBE_CAP {
            return FirstEventProbe::Ok {
                held: Bytes::from(std::mem::take(&mut buf)),
                rest: body,
            };
        }
        match body.as_mut().frame().await {
            Some(Ok(frame)) => {
                if let Some(bytes) = frame.data_ref() {
                    buf.extend_from_slice(bytes);
                }
            }
            // A stream that dies before its first complete event is an
            // immediate-failure shape — surface it as an in-band error so the
            // loop retries/migrates instead of relaying a broken body.
            Some(Err(e)) => {
                return FirstEventProbe::InBandError {
                    reason: format!("upstream stream error before first event: {e}"),
                };
            }
            None => {
                // Stream ended without a complete event boundary — hand the
                // bytes to the relay verbatim (its truncation handling owns
                // this shape).
                return FirstEventProbe::Ok {
                    held: Bytes::from(std::mem::take(&mut buf)),
                    rest: body,
                };
            }
        }
    }
    });
    match probe.await {
        Ok(outcome) => outcome,
        Err(_) => FirstEventProbe::InBandError {
            reason: format!(
                "first SSE event not received within {}s",
                timeout.as_secs()
            ),
        },
    }
}

/// Byte offset of the first complete SSE event boundary (`\n\n`), excluding
/// the boundary itself. Handles `\r\n\r\n` by returning the offset of the
/// `\r` before the `\n\n`-style split.
fn find_event_boundary(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).position(|w| w == b"\n\n")?;
    let mut end = lf;
    if end > 0 && buf[end - 1] == b'\r' {
        end -= 1;
    }
    Some(end)
}

/// `true` (with a reason) when the FIRST SSE event is an in-band terminal
/// error with no generated content.
fn first_event_is_inband_error(event_bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(event_bytes).ok()?;
    let mut data = String::new();
    let mut event_name = String::new();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(v.trim());
        } else if let Some(v) = line.strip_prefix("event:") {
            event_name = v.trim().to_string();
        }
    }
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
        return None;
    };
    // Responses wire: an immediate failed response.
    if event_name == "response.failed" || v.get("type").and_then(|t| t.as_str()) == Some("response.failed") {
        let msg = v["response"]["error"]["message"]
            .as_str()
            .or_else(|| v["error"]["message"].as_str())
            .unwrap_or("response.failed");
        return Some(msg.to_string());
    }
    // An error envelope on any wire.
    if let Some(msg) = v["error"]["message"].as_str() {
        if !msg.is_empty() {
            return Some(msg.to_string());
        }
    }
    // Chat wire: a terminal error-valued finish_reason on an empty delta —
    // the observed opencode failure shape (the ONLY chunk of the stream).
    if let Some(choice) = v.get("choices").and_then(|c| c.get(0)) {
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            let benign = matches!(fr, "stop" | "length" | "tool_calls" | "function_call");
            if !benign {
                let empty = |field: &str| {
                    choice["delta"][field]
                        .as_str()
                        .map_or(true, |s| s.is_empty())
                };
                let no_tools = choice["delta"]
                    .get("tool_calls")
                    .and_then(|t| t.as_array())
                    .map_or(true, |a| a.is_empty());
                if empty("content") && empty("reasoning_content") && no_tools {
                    return Some(format!("finish_reason={fr}"));
                }
            }
        }
    }
    None
}

/// Relays `held` bytes first, then the remaining upstream body — what the
/// probe hands back so the first event is not swallowed.
pub(crate) struct PrependBody {
    held: Option<Bytes>,
    inner: SharedBody,
}

impl PrependBody {
    pub(crate) fn new(held: Bytes, inner: SharedBody) -> Self {
        Self {
            held: Some(held),
            inner,
        }
    }
}

impl Body for PrependBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if let Some(held) = this.held.take() {
            if !held.is_empty() {
                return Poll::Ready(Some(Ok(Frame::data(held))));
            }
        }
        this.inner.as_mut().poll_frame(cx)
    }
}
