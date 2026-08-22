//! Endpoint base-URL normalization and protocol-aware path joining.
//!
//! A user-entered `base_url` may be the API root (`https://api.minimaxi.com/
//! anthropic`), a version root (`https://api.openai.com/v1`), or the FULL API
//! path (`https://api.minimaxi.com/anthropic/v1/messages`,
//! `https://x.com/v1/chat/completions`). Every join site — gateway forward,
//! agent config writes, model fetch, quota engine — goes through this module
//! so the canonical protocol path is never doubled.

use crate::config_writer::ProviderKind;

/// Strip any already-present canonical path so appending never doubles it.
///   anthropic: `.../anthropic/v1/messages` -> `.../anthropic`; `.../v1` -> `...`
///   openai:    `.../v1/chat/completions` -> `.../v1`; `.../chat/completions` -> `...`
///   responses: `.../v1/responses` -> `.../v1`
pub fn normalize_protocol_base(base: &str, kind: ProviderKind) -> String {
    let t = base.trim_end_matches('/');
    match kind {
        ProviderKind::Anthropic => {
            // Claude Code appends `/v1/messages` to ANTHROPIC_BASE_URL
            // itself, so a base that already ends in `/v1` (official
            // `https://api.anthropic.com/v1`, or OpenRouter's documented
            // `https://openrouter.ai/api/v1`) must lose it — otherwise the
            // final URL doubles the segment (`.../v1/v1/messages`).
            let stripped = t.strip_suffix("/v1/messages").unwrap_or(t);
            stripped
                .trim_end_matches('/')
                .strip_suffix("/v1")
                .unwrap_or(stripped)
                .to_string()
        }
        ProviderKind::Responses => t
            .strip_suffix("/v1/responses")
            .unwrap_or(t)
            .to_string(),
        _ => t
            .strip_suffix("/v1/chat/completions")
            .or_else(|| t.strip_suffix("/chat/completions"))
            .unwrap_or(t)
            .to_string(),
    }
}

/// Join the protocol's canonical API path, sensing the base's shape:
///   anthropic: `.../v1` -> `.../v1/messages`; API root -> `.../v1/messages`
///   openai:    `.../v1` | `.../v4` -> `.../v1/chat/completions`; bare -> `/v1/chat/completions`
///   responses: `.../v1` -> `.../v1/responses`; bare -> `/v1/responses`
pub fn join_protocol_path(base: &str, kind: ProviderKind) -> String {
    let t = normalize_protocol_base(base, kind);
    match kind {
        ProviderKind::Anthropic => {
            if t.ends_with("/v1") {
                format!("{t}/messages")
            } else {
                format!("{t}/v1/messages")
            }
        }
        ProviderKind::Responses => {
            let last = t.rsplit('/').next().unwrap_or("");
            let versioned = last.len() > 1
                && last.starts_with('v')
                && last[1..].chars().all(|c| c.is_ascii_digit());
            if versioned {
                format!("{t}/responses")
            } else {
                format!("{t}/v1/responses")
            }
        }
        _ => {
            let last = t.rsplit('/').next().unwrap_or("");
            let versioned = last.len() > 1
                && last.starts_with('v')
                && last[1..].chars().all(|c| c.is_ascii_digit());
            if versioned {
                format!("{t}/chat/completions")
            } else {
                format!("{t}/v1/chat/completions")
            }
        }
    }
}

/// Join the model-list path (`/v1/models` for Anthropic, `/models` else).
/// Same `/v1` sense as `join_protocol_path`: a base that already ends in
/// `/v1` (e.g. `https://opencode.ai/zen/go/v1`) must not double it.
pub fn join_models_path(base: &str, kind: ProviderKind) -> String {
    let t = normalize_protocol_base(base, kind);
    match kind {
        ProviderKind::Anthropic => {
            if t.ends_with("/v1") {
                format!("{t}/models")
            } else {
                format!("{t}/v1/models")
            }
        }
        _ => format!("{t}/models"),
    }
}

/// Parse the protocol-joined upstream URL as a `hyper::Uri`. Returns a
/// human-readable error instead of a URL when the user-configured `base_url`
/// is unparseable — the gateway must treat that as an unreachable upstream
/// (never fall back to a hardcoded loopback URL: the request carries real
/// credentials, and a silent `127.0.0.1` retry would leak them to an
/// unintended local service while masking the config error).
pub fn parse_upstream_uri(base: &str, kind: ProviderKind) -> Result<hyper::Uri, String> {
    let joined = join_protocol_path(base, kind);
    joined
        .parse()
        .map_err(|e| format!("invalid upstream URL '{joined}': {e}"))
}

#[cfg(test)]
mod tests;
