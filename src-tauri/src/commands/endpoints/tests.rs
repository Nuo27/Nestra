use super::*;
use crate::model_abilities::ModelLimit;

#[test]
fn parse_models_entry_reads_openrouter_shape() {
    let v = serde_json::json!({
        "id": "openai/gpt-4o",
        "context_length": 128_000,
        "architecture": { "input_modalities": ["text", "image"] },
        "top_provider": { "max_completion_tokens": 16_384 },
        "supported_parameters": ["tools", "reasoning", "structured_outputs"],
    });
    let h = parse_models_entry(&v).expect("hint");
    assert_eq!(
        h.limit,
        Some(ModelLimit { context: 128_000, output: 16_384, input: None })
    );
    assert_eq!(h.tool_call, Some(true));
    assert_eq!(h.reasoning, Some(true));
    assert_eq!(h.attachment, Some(true));
    assert_eq!(h.temperature, None);
}

#[test]
fn parse_models_entry_defaults_missing_output_to_placeholder() {
    let v = serde_json::json!({ "id": "x", "context_length": 200_000 });
    let h = parse_models_entry(&v).expect("hint");
    assert_eq!(
        h.limit,
        Some(ModelLimit { context: 200_000, output: 8_192, input: None })
    );
}

#[test]
fn parse_models_entry_id_only_returns_none() {
    // Plain OpenAI/Anthropic `/models` entries carry no ability fields.
    let v = serde_json::json!({ "id": "claude-sonnet-4-5", "display_name": "Sonnet" });
    assert!(parse_models_entry(&v).is_none());
}

#[test]
fn parse_models_entry_missing_context_drops_limit() {
    let v = serde_json::json!({
        "id": "x",
        "supported_parameters": ["tools"],
    });
    let h = parse_models_entry(&v).expect("hint");
    assert_eq!(h.limit, None);
    assert_eq!(h.tool_call, Some(true));
}

/// The hint filter in `endpoint_fetch_models`: hints survive only for
/// models the local chain (models.dev + corrections) cannot resolve.
#[test]
fn hints_filter_drops_locally_resolvable_models() {
    let base = crate::model_abilities::build_index(&serde_json::json!({
        "anthropic/claude-sonnet-4-5": {
            "id": "claude-sonnet-4-5",
            "limit": { "context": 200_000, "output": 16_384 },
        }
    }));
    let raw_hints: HashMap<String, _> = [(
        "claude-sonnet-4-5".to_string(),
        crate::model_abilities::ModelAbilities {
            limit: Some(ModelLimit { context: 1, output: 1, input: None }),
            ..Default::default()
        },
    )]
    .into_iter()
    .collect();
    let hints: HashMap<_, _> = raw_hints
        .into_iter()
        .filter(|(mid, _)| crate::model_abilities::abilities_for(&base, mid).is_none())
        .collect();
    assert!(hints.is_empty(), "resolvable model must not become an override");

    let raw_hints: HashMap<String, _> = [(
        "brand-new-model".to_string(),
        crate::model_abilities::ModelAbilities {
            limit: Some(ModelLimit { context: 256_000, output: 8_192, input: None }),
            ..Default::default()
        },
    )]
    .into_iter()
    .collect();
    let hints: HashMap<_, _> = raw_hints
        .into_iter()
        .filter(|(mid, _)| crate::model_abilities::abilities_for(&base, mid).is_none())
        .collect();
    assert!(hints.contains_key("brand-new-model"));
}