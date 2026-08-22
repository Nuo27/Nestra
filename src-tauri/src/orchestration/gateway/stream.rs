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
use http_body_util::{BodyStream, Full};
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

/// Parse usage tokens out of an SSE event-stream chunk. Anthropic streams the
/// `usage` object inside `message_start` (input + cache fields) and
/// `message_delta` (output). This scans one buffered chunk of decoded text for
/// those fields and accumulates into `usage`. Best-effort: a malformed chunk
/// is ignored (the next chunk may complete it).
pub fn observe_usage_chunk(text: &str, usage: &mut ObservedUsage) {
    // We only care about `message_start` and `message_delta` event types.
    // Walk data: lines, parse their JSON, and pull usage fields.
    for line in text.lines() {
        let payload = match line.strip_prefix("data: ") {
            Some(p) => p.trim(),
            None => continue,
        };
        if payload == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        let Some(obj) = v.as_object() else { continue };
        // `message_start` carries { message: { usage: { ... } } }
        if obj.get("type").and_then(|t| t.as_str()) == Some("message_start") {
            if let Some(u) = obj
                .get("message")
                .and_then(|m| m.get("usage"))
                .and_then(|u| u.as_object())
            {
                merge_usage_obj(u, usage);
            }
            continue;
        }
        // `message_delta` carries { usage: { output_tokens: N } }
        if obj.get("type").and_then(|t| t.as_str()) == Some("message_delta") {
            if let Some(u) = obj.get("usage").and_then(|u| u.as_object()) {
                merge_usage_obj(u, usage);
            }
        }
    }
}

/// Public alias so the protocol handler can merge a parsed `usage` object
/// (non-streaming path) without re-implementing the field walk.
pub fn merge_usage_obj_pub(u: &serde_json::Map<String, serde_json::Value>, usage: &mut ObservedUsage) {
    merge_usage_obj(u, usage)
}

fn merge_usage_obj(u: &serde_json::Map<String, serde_json::Value>, usage: &mut ObservedUsage) {
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
        let Some(obj) = v.as_object() else { continue };
        if obj.get("type").and_then(|t| t.as_str()) != Some("content_block_start") {
            continue;
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
    }
}

/// Observe one buffered text window of an OpenAI Chat Completions SSE stream.
/// Usage appears as a top-level `usage` on the final chunk (only when the
/// request set `stream_options.include_usage` — the gateway's Anthropic→OpenAI
/// bridge injects it, native OpenAI agents may not). Tool-call deltas arrive
/// on `choices[].delta.tool_calls`; `index` is constant per call and present
/// on every delta, so it is the dedup key (the `id` rides only the first
/// delta and would double-count).
pub fn observe_openai_chat_chunk(text: &str, obs: &mut StreamObservation) {
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
        let Some(obj) = v.as_object() else { continue };
        if let Some(u) = obj.get("usage").and_then(|u| u.as_object()) {
            merge_usage_obj(u, &mut obs.usage);
        }
        let Some(choices) = obj.get("choices").and_then(|c| c.as_array()) else {
            continue;
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
    }
}

/// Observe one buffered text window of an OpenAI Responses API SSE stream
/// (the upstream wire when the gateway bridges to a `responses` endpoint).
/// `response.completed` / `response.incomplete` carry `response.usage`
/// (Anthropic-style `input_tokens`/`output_tokens`, cache under
/// `input_tokens_details.cached_tokens`); tool calls surface as
/// `function_call` output items, announced by `response.output_item.added`.
pub fn observe_responses_chunk(text: &str, obs: &mut StreamObservation) {
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
        let Some(obj) = v.as_object() else { continue };
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
    }
}

#[cfg(test)]
mod tests;
