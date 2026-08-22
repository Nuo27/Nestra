//! Provider-aware prompt-cache planning.
//!
//! This module decides WHERE to place `cache_control` breakpoints in an
//! Anthropic Messages request — and only when the routing policy explicitly
//! opts in (`inject_cache_control = true`, default off). It follows the
//! **official Anthropic API semantics** (verified against the platform docs):
//!
//! - `cache_control: {"type": "ephemeral"}` may be placed on:
//!   system content blocks, tool definitions in `tools`, and message content
//!   blocks (text/image/document/tool_use/tool_result) in user AND assistant
//!   turns.
//! - **At most 4 cache breakpoints per request**; a 5th is a 400 upstream.
//! - Thinking blocks, sub-content blocks (citations), and empty text blocks
//!   cannot be cached directly.
//! - Cache prefix order is `tools`, `system`, then `messages`; the breakpoint
//!   should sit on the LAST block of a stable prefix.
//!
//! [`CachePlan::for_request`] derives breakpoints from the request's actual
//! shape, obeying the official rules, and the routing policy decides *how
//! many* breakpoints to allow (1 by default — the single most valuable one:
//! end of the static prefix). This keeps the injection provider-aware (a
//! DeepSeek/OpenRouter endpoint gets none) and conservatively within the
//! API's constraints.
//!
//! This module is pure — it inspects a JSON body and returns a new body (or
//! the original bytes unchanged when nothing applies). The gateway calls it
//! in the Anthropic forward path, gated by `CacheStrategy::AnthropicExplicit`.

use bytes::Bytes;
use serde_json::{Map, Value};

/// How many breakpoints Nestra may add. `1` is the conservative default: the
/// single highest-value breakpoint (end of the static prefix), which is what
/// makes the NEXT request in the same task a cache-read. The policy could
/// raise this later; the 4-breakpoint cap is enforced regardless.
pub const DEFAULT_MAX_BREAKPOINTS: usize = 1;
/// Hard cap from the Anthropic API (a 5th explicit breakpoint is a 400).
pub const HARD_MAX_BREAKPOINTS: usize = 4;

/// Inject `cache_control: {"type":"ephemeral"}` breakpoints into an Anthropic
/// Messages body. Returns the modified bytes, or the original bytes unchanged
/// when:
///   - the body isn't JSON / isn't an object,
///   - the body has no cacheable blocks,
///   - `max_breakpoints == 0` (injection disabled).
///
/// Breakpoint selection (official semantics, no hardcoded trio):
///   1. **tools** — the LAST tool definition gets the breakpoint (caches the
///      whole tool prefix; highest value when the agent ships a tool schema).
///   2. **system** — the LAST system content block (only when no tools, or as
///      the second breakpoint).
///   3. **messages** — the LAST text content block of the LAST user message
///      (Anthropic only honors the breakpoint on the final message; marking
///      an earlier user turn wastes a breakpoint and can 400).
///
/// Each additional breakpoint walks the next source in that order, up to
/// `max_breakpoints` (and the hard cap). Breakpoints the request ALREADY
/// carries (agent- or prior-pass-authored) count against the same 4-cap.
/// Only non-empty text blocks are marked (empty text blocks cannot be cached
/// per the docs).
pub fn inject_cache_control(body: &[u8], max_breakpoints: usize) -> Bytes {
    if max_breakpoints == 0 {
        return Bytes::copy_from_slice(body);
    }
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return Bytes::copy_from_slice(body);
    };
    let Some(obj) = v.as_object_mut() else {
        return Bytes::copy_from_slice(body);
    };

    // Existing breakpoints eat into the budget: the API caps the TOTAL at 4,
    // so a request that already carries 3-4 must not receive more (a 5th is a
    // 400 upstream).
    let existing = count_breakpoints_in_map(obj);
    let budget = max_breakpoints.min(HARD_MAX_BREAKPOINTS.saturating_sub(existing));
    let mut remaining = budget;
    if remaining == 0 {
        return Bytes::copy_from_slice(body);
    }

    // 1. Tools: mark the last tool definition.
    if remaining > 0 {
        if let Some(tools) = obj.get_mut("tools").and_then(|t| t.as_array_mut()) {
            if let Some(last) = tools.last_mut() {
                mark_object(last, &mut remaining);
            }
        }
    }
    // 2. System: mark the last system content block.
    if remaining > 0 {
        if let Some(system) = obj.get_mut("system") {
            mark_last_text_block(system, &mut remaining);
        }
    }
    // 3. Messages: the LAST user message's last text block.
    if remaining > 0 {
        if let Some(messages) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) {
            for msg in messages.iter_mut().rev() {
                let is_user = msg
                    .get("role")
                    .and_then(|r| r.as_str())
                    .map(|r| r == "user")
                    .unwrap_or(false);
                if !is_user {
                    continue;
                }
                if let Some(content) = msg.get_mut("content") {
                    mark_last_text_block(content, &mut remaining);
                }
                break; // only the LAST user message
            }
        }
    }

    if remaining == budget {
        // Nothing was marked (no cacheable blocks) — return unchanged.
        return Bytes::copy_from_slice(body);
    }
    Bytes::from(serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec()))
}

/// Count `cache_control` occurrences anywhere in the request (tools, system,
/// message content blocks). Every one occupies a slot in the API's 4-cap.
fn count_breakpoints_in_map(m: &serde_json::Map<String, Value>) -> usize {
    let here = usize::from(m.contains_key("cache_control"));
    here + m
        .values()
        .map(|v| match v {
            Value::Array(arr) => arr.iter().map(count_breakpoints_in_value).sum(),
            Value::Object(m2) => count_breakpoints_in_map(m2),
            _ => 0,
        })
        .sum::<usize>()
}

fn count_breakpoints_in_value(v: &Value) -> usize {
    match v {
        Value::Array(arr) => arr.iter().map(count_breakpoints_in_value).sum(),
        Value::Object(m) => count_breakpoints_in_map(m),
        _ => 0,
    }
}

/// Mark the LAST text block in a content array (or a single string content —
/// string content is a text block and can be marked only if it's an object
/// form; a plain string has no place for cache_control, so we leave it).
fn mark_last_text_block(value: &mut Value, remaining: &mut usize) {
    match value {
        Value::Array(arr) => {
            for block in arr.iter_mut().rev() {
                let is_text = block
                    .get("type")
                    .and_then(|t| t.as_str())
                    .map(|t| t == "text")
                    .unwrap_or(false);
                let nonempty = block
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                if is_text && nonempty {
                    mark_object(block, remaining);
                    return;
                }
            }
        }
        Value::String(s) if !s.is_empty() => {
            // String content can't carry `cache_control` — normalize it to a
            // text block (Anthropic accepts both shapes; the object form is
            // what can hold a breakpoint). Previously string-form content
            // was silently skipped, so common simple requests (string system
            // or message content) never got any cache benefit.
            let mut block = Map::new();
            block.insert("type".into(), Value::String("text".into()));
            block.insert("text".into(), Value::String(std::mem::take(s)));
            let mut normalized = vec![Value::Object(block)];
            if let Some(b) = normalized.first_mut() {
                mark_object(b, remaining);
            }
            *value = Value::Array(normalized);
        }
        _ => {}
    }
}

/// Insert `cache_control` into an object if it doesn't already have one, and
/// decrement the remaining-breakpoints budget.
fn mark_object(obj: &mut Value, remaining: &mut usize) {
    if *remaining == 0 {
        return;
    }
    let Some(o) = obj.as_object_mut() else {
        return;
    };
    if o.contains_key("cache_control") {
        return; // already marked (by the agent or a prior pass)
    }
    o.insert(
        "cache_control".to_string(),
        serde_json::json!({ "type": "ephemeral" }),
    );
    *remaining -= 1;
}

/// Parse the policy's `inject_cache_control` flag into a breakpoint budget.
/// `true` → [`DEFAULT_MAX_BREAKPOINTS`]; `false` → 0 (no injection).
pub fn breakpoints_from_policy(inject: bool) -> usize {
    if inject {
        DEFAULT_MAX_BREAKPOINTS
    } else {
        0
    }
}

#[cfg(test)]
mod tests;
