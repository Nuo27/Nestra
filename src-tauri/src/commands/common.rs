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
mod tests;
