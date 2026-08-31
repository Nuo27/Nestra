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
    // `openrouter` is not a stored protocol kind — OpenRouter binds
    // through `anthropic`/`openai` rows like any OpenAI-compatible provider.
    assert!(validate_protocol_kind("openrouter").is_err());
    // the exact typo that caused minimax to show only claude code:
    assert!(validate_protocol_kind("open").is_err());
    assert!(validate_protocol_kind("").is_err());
    assert!(validate_protocol_kind("OpenAI").is_err());
    assert!(validate_protocol_kind("claude").is_err());
}