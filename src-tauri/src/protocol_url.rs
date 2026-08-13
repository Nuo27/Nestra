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
mod tests {
    use super::*;

    #[test]
    fn normalize_anthropic_strips_full_path() {
        assert_eq!(
            normalize_protocol_base("https://api.minimaxi.com/anthropic/v1/messages", ProviderKind::Anthropic),
            "https://api.minimaxi.com/anthropic"
        );
        // API root and version root pass through.
        assert_eq!(
            normalize_protocol_base("https://api.minimaxi.com/anthropic", ProviderKind::Anthropic),
            "https://api.minimaxi.com/anthropic"
        );
        assert_eq!(
            normalize_protocol_base("https://api.anthropic.com/v1", ProviderKind::Anthropic),
            "https://api.anthropic.com"
        );
        assert_eq!(
            normalize_protocol_base("https://openrouter.ai/api/v1", ProviderKind::Anthropic),
            "https://openrouter.ai/api"
        );
    }

    #[test]
    fn normalize_openai_strips_full_path() {
        assert_eq!(
            normalize_protocol_base("https://x.com/v1/chat/completions", ProviderKind::Openai),
            "https://x.com"
        );
        assert_eq!(
            normalize_protocol_base("https://x.com/chat/completions", ProviderKind::Openai),
            "https://x.com"
        );
        assert_eq!(
            normalize_protocol_base("https://api.z.ai/api/paas/v4", ProviderKind::Openai),
            "https://api.z.ai/api/paas/v4"
        );
    }

    #[test]
    fn join_anthropic_never_doubles() {
        assert_eq!(
            join_protocol_path("https://api.minimaxi.com/anthropic/v1/messages", ProviderKind::Anthropic),
            "https://api.minimaxi.com/anthropic/v1/messages"
        );
        assert_eq!(
            join_protocol_path("https://api.minimaxi.com/anthropic", ProviderKind::Anthropic),
            "https://api.minimaxi.com/anthropic/v1/messages"
        );
        assert_eq!(
            join_protocol_path("https://api.anthropic.com/v1", ProviderKind::Anthropic),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            join_protocol_path("http://127.0.0.1:8787", ProviderKind::Anthropic),
            "http://127.0.0.1:8787/v1/messages"
        );
    }

    #[test]
    fn join_openai_never_doubles() {
        assert_eq!(
            join_protocol_path("https://x.com/v1/chat/completions", ProviderKind::Openai),
            "https://x.com/v1/chat/completions"
        );
        assert_eq!(
            join_protocol_path("https://api.openai.com/v1", ProviderKind::Openai),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            join_protocol_path("https://api.z.ai/api/paas/v4", ProviderKind::Openai),
            "https://api.z.ai/api/paas/v4/chat/completions"
        );
        assert_eq!(
            join_protocol_path("http://127.0.0.1:8787", ProviderKind::Openai),
            "http://127.0.0.1:8787/v1/chat/completions"
        );
    }

    #[test]
    fn join_models_paths() {
        assert_eq!(
            join_models_path("https://api.minimaxi.com/anthropic/v1/messages", ProviderKind::Anthropic),
            "https://api.minimaxi.com/anthropic/v1/models"
        );
        assert_eq!(
            join_models_path("https://api.openai.com/v1", ProviderKind::Openai),
            "https://api.openai.com/v1/models"
        );
    }

    #[test]
    fn join_models_path_v1_base_does_not_double() {
        // opencode-go's base already ends in /v1 — the anthropic model list
        // must not become /v1/v1/models (this used to 404 during validation).
        assert_eq!(
            join_models_path("https://opencode.ai/zen/go/v1", ProviderKind::Anthropic),
            "https://opencode.ai/zen/go/v1/models"
        );
        assert_eq!(
            join_models_path("https://api.anthropic.com/v1", ProviderKind::Anthropic),
            "https://api.anthropic.com/v1/models"
        );
    }

    #[test]
    fn join_responses_never_doubles() {
        assert_eq!(
            join_protocol_path("https://opencode.ai/zen/go/v1", ProviderKind::Responses),
            "https://opencode.ai/zen/go/v1/responses"
        );
        assert_eq!(
            join_protocol_path("https://opencode.ai/zen/go/v1/responses", ProviderKind::Responses),
            "https://opencode.ai/zen/go/v1/responses"
        );
        assert_eq!(
            join_protocol_path("https://api.x.ai/v1", ProviderKind::Responses),
            "https://api.x.ai/v1/responses"
        );
        assert_eq!(
            join_protocol_path("http://127.0.0.1:8787", ProviderKind::Responses),
            "http://127.0.0.1:8787/v1/responses"
        );
        assert_eq!(
            join_models_path("https://opencode.ai/zen/go/v1", ProviderKind::Responses),
            "https://opencode.ai/zen/go/v1/models"
        );
    }
}
