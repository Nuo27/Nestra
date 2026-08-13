use crate::error::{AppError, AppResult};

// ---- Validators ----

pub(crate) fn validate_id(id: &str) -> AppResult<()> {
    if id.is_empty() {
        return Err(AppError::Validation("id cannot be empty".into()));
    }
    if id.len() > 64 {
        return Err(AppError::Validation("id must be ≤ 64 chars".into()));
    }
    if !id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return Err(AppError::Validation(
            "id must be lowercase a-z, 0-9, and dashes".into(),
        ));
    }
    if id.starts_with('-') || id.ends_with('-') {
        return Err(AppError::Validation("id cannot start or end with '-'".into()));
    }
    Ok(())
}

pub(crate) fn validate_protocol(p: &str) -> AppResult<()> {
    validate_id(p)
}

/// Strict check used on add/upsert: protocol must be a real ProviderKind.
/// (Removal keeps the lenient `validate_protocol` so mistyped keys
/// like "open" can still be deleted.)
pub(crate) fn validate_protocol_kind(p: &str) -> AppResult<()> {
    let valid = [
        crate::config_writer::ProviderKind::Anthropic,
        crate::config_writer::ProviderKind::Openai,
        crate::config_writer::ProviderKind::Responses,
        crate::config_writer::ProviderKind::Custom,
    ];
    if !valid.iter().any(|k| k.as_str() == p) {
        return Err(AppError::Validation(format!(
            "protocol must be one of: {}",
            valid.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(())
}

pub(crate) fn validate_base_url(s: &str) -> AppResult<String> {
    let trimmed = s.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(AppError::Validation("base_url cannot be empty".into()));
    }
    let parsed = url::Url::parse(trimmed)
        .map_err(|e| AppError::Validation(format!("invalid base_url: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(AppError::Validation("base_url must be http(s)".into()));
    }
    if parsed.host_str().map_or(true, str::is_empty) {
        return Err(AppError::Validation("base_url must have a host".into()));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_id_accepts_normal() {
        assert!(validate_id("anthropic").is_ok());
        assert!(validate_id("my-anthropic-1").is_ok());
        assert!(validate_id("a").is_ok());
        assert!(validate_id(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn validate_id_rejects_bad() {
        assert!(validate_id("").is_err());
        assert!(validate_id(&"a".repeat(65)).is_err());
        assert!(validate_id("My-Anthropic").is_err());
        assert!(validate_id("-leading").is_err());
        assert!(validate_id("trailing-").is_err());
        assert!(validate_id("has space").is_err());
        assert!(validate_id("has_underscore").is_err());
        assert!(validate_id("dot.dot").is_err());
    }

    #[test]
    fn validate_base_url_accepts_http_https() {
        assert!(validate_base_url("https://api.anthropic.com").is_ok());
        assert!(validate_base_url("http://localhost:8080/v1").is_ok());
        assert!(validate_base_url("  https://api.openai.com/v1/  ").is_ok());
    }

    #[test]
    fn validate_base_url_rejects_bad() {
        assert!(validate_base_url("").is_err());
        assert!(validate_base_url("   ").is_err());
        assert!(validate_base_url("not a url").is_err());
        assert!(validate_base_url("ftp://example.com").is_err());
        assert!(validate_base_url("https://").is_err());
        assert!(validate_base_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn validate_protocol_kind_accepts_canonical_and_rejects_typos() {
        for ok in ["anthropic", "openai-comp", "response-api", "custom"] {
            assert!(validate_protocol_kind(ok).is_ok(), "{ok} should be accepted");
        }
        // `openrouter` is no longer a stored protocol kind — OpenRouter binds
        // through `anthropic`/`openai` rows like any OpenAI-compatible provider.
        assert!(validate_protocol_kind("openrouter").is_err());
        // the exact typo that caused minimax to show only claude code:
        assert!(validate_protocol_kind("open").is_err());
        assert!(validate_protocol_kind("").is_err());
        assert!(validate_protocol_kind("OpenAI").is_err());
        assert!(validate_protocol_kind("claude").is_err());
    }
}
