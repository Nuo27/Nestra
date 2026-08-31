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
    // must not become /v1/v1/models.
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