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

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::{BodyStream, Full};
use hyper::body::{Body, Frame};

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
            .and_then(|x| x.as_i64())
        {
            usage.cache_read = Some(n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_usage_chunk_extracts_message_start() {
        let chunk = r#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":42,"cache_creation_input_tokens":100,"cache_read_input_tokens":0}}}

event: message_delta
data: {"type":"message_delta","usage":{"output_tokens":128}}"#;
        let mut usage = ObservedUsage::default();
        observe_usage_chunk(chunk, &mut usage);
        assert_eq!(usage.input, Some(42));
        assert_eq!(usage.cache_creation, Some(100));
        assert_eq!(usage.cache_read, Some(0));
        assert_eq!(usage.output, Some(128));
    }

    #[test]
    fn observe_usage_chunk_ignores_garbage() {
        let mut usage = ObservedUsage::default();
        // Malformed JSON + unknown event types must not panic or pollute.
        observe_usage_chunk("data: not json\n\nevent: ping\ndata: {}", &mut usage);
        assert_eq!(usage, ObservedUsage::default());
    }

    #[test]
    fn merge_usage_obj_parses_openai_field_names() {
        // Chat Completions usage object — the field names differ from
        // Anthropic's; both vocabularies must map onto the same standard.
        let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
            r#"{"prompt_tokens":120,"completion_tokens":45,"total_tokens":165,
                "prompt_tokens_details":{"cached_tokens":88}}"#,
        )
        .unwrap();
        let mut usage = ObservedUsage::default();
        merge_usage_obj(&obj, &mut usage);
        assert_eq!(usage.input, Some(120), "prompt_tokens → input");
        assert_eq!(usage.output, Some(45), "completion_tokens → output");
        assert_eq!(usage.cache_read, Some(88), "cached_tokens → cache_read");
    }

    #[test]
    fn merge_usage_obj_anthropic_wins_over_openai_names() {
        // A body carrying BOTH vocabularies prefers the Anthropic names.
        let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
            r#"{"input_tokens":42,"output_tokens":7,"prompt_tokens":999}"#,
        )
        .unwrap();
        let mut usage = ObservedUsage::default();
        merge_usage_obj(&obj, &mut usage);
        assert_eq!(usage.input, Some(42));
        assert_eq!(usage.output, Some(7));
    }
}
