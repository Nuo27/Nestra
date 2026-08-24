use super::*;
use crate::model_abilities::{merge_into, merge_field_overrides, ModelAbilities, ModelLimit};
use std::collections::HashMap;

fn ab(reasoning: bool, tool_call: bool, attachment: bool, limit: Option<ModelLimit>) -> ModelAbilities {
    ModelAbilities {
        reasoning: Some(reasoning),
        tool_call: Some(tool_call),
        attachment: Some(attachment),
        temperature: None,
        limit,
        modalities: None,
        api: None,
        cost: None,
    }
}

#[test]
fn default_inherits_into_missing_override_field() {
    let def = ab(
        true,
        true,
        true,
        Some(ModelLimit { context: 200_000, output: 8_000, input: None }),
    );
    // Override with every field None → nothing set, everything inherits.
    let ov = ModelAbilities::default();
    let merged = merge_field_overrides(def.clone(), ov);
    assert_eq!(merged.reasoning, def.reasoning);
    assert_eq!(merged.tool_call, def.tool_call);
    assert_eq!(merged.attachment, def.attachment);
    assert_eq!(merged.temperature, def.temperature);
    assert_eq!(merged.limit.as_ref().unwrap().context, 200_000);
}

#[test]
fn override_replaces_default_at_field_level() {
    let def = ab(false, true, true, None);
    // Override flips reasoning on; tool_call/attachment stay None → inherit.
    let ov = ModelAbilities {
        reasoning: Some(true),
        ..ModelAbilities::default()
    };
    let merged = merge_field_overrides(def, ov);
    assert_eq!(merged.reasoning, Some(true));
    assert_eq!(merged.tool_call, Some(true));
    assert_eq!(merged.attachment, Some(true));
}

#[test]
fn merge_into_unions_ids_and_handles_reset() {
    let mut defaults = HashMap::new();
    defaults.insert(
        "MiniMax-M3".into(),
        ab(true, true, true, Some(ModelLimit { context: 512_000, output: 128_000, input: None })),
    );
    // After a Reset, the override map has the id but every field None —
    // the merge must drop back to the models.dev default.
    let mut overrides = HashMap::new();
    overrides.insert("MiniMax-M3".into(), ModelAbilities::default());
    let merged = merge_into(defaults, overrides);
    let m = merged.get("MiniMax-M3").unwrap();
    assert_eq!(m.reasoning, Some(true), "reset drops to default");
    assert_eq!(m.tool_call, Some(true));
    assert_eq!(m.limit.as_ref().unwrap().context, 512_000);
}

fn ab_with_api(api: &str) -> ModelAbilities {
    ModelAbilities {
        reasoning: Some(true),
        api: Some(api.into()),
        cost: None,
        ..ModelAbilities::default()
    }
}

#[test]
fn filter_models_for_anthropic_keeps_matching_and_filters_openai_only() {
    let mut abilities = HashMap::new();
    abilities.insert("claude-sonnet".into(), ab_with_api("anthropic"));
    abilities.insert("grok-4.5".into(), ab_with_api("openai-comp"));
    abilities.insert("mystery-model".into(), ab_with_api("anthropic"));
    // Unknown api (None) stays — follows the endpoint protocol.
    abilities.insert("legacy".into(), ModelAbilities::default());

    let models = crate::config_writer::ModelsConfig::Anthropic {
        default: "mystery-model".into(),
        haiku: "grok-4.5".into(),     // openai-only → falls back to default
        sonnet: "legacy".into(),      // unknown api → kept
        opus: "claude-sonnet".into(), // anthropic → kept
    };
    let filtered = filter_models_for_anthropic(models, &abilities).unwrap();
    match filtered {
        crate::config_writer::ModelsConfig::Anthropic { default, haiku, sonnet, opus } => {
            assert_eq!(default, "mystery-model");
            assert_eq!(haiku, "mystery-model", "openai-only tier falls back to default");
            assert_eq!(sonnet, "legacy");
            assert_eq!(opus, "claude-sonnet");
        }
        other => panic!("expected Anthropic config, got {other:?}"),
    }
}

#[test]
fn filter_models_for_anthropic_rejects_openai_only_default() {
    let mut abilities = HashMap::new();
    abilities.insert("grok-4.5".into(), ab_with_api("openai-comp"));
    let models = crate::config_writer::ModelsConfig::Anthropic {
        default: "grok-4.5".into(),
        haiku: "grok-4.5".into(),
        sonnet: "grok-4.5".into(),
        opus: "grok-4.5".into(),
    };
    assert!(filter_models_for_anthropic(models, &abilities).is_err());
}

#[test]
fn filter_models_for_openai_drops_responses_class_models() {
    let mut abilities = HashMap::new();
    abilities.insert("grok-4.5".into(), ab_with_api("response-api"));
    abilities.insert("gpt-5.6-luna".into(), ab_with_api("response-api"));
    abilities.insert("deepseek-v4-flash".into(), ab_with_api("anthropic"));
    abilities.insert("kimi-k3".into(), ab_with_api("openai-comp"));
    // Unknown api (None) stays — follows the endpoint protocol.
    abilities.insert("legacy".into(), ModelAbilities::default());

    let models = crate::config_writer::ModelsConfig::Openai {
        default: "deepseek-v4-flash".into(),
        available: vec![
            "deepseek-v4-flash".into(),
            "kimi-k3".into(),
            "grok-4.5".into(),
            "gpt-5.6-luna".into(),
            "legacy".into(),
        ],
    };
    let filtered = filter_models_for_openai(models, &abilities).unwrap();
    match filtered {
        crate::config_writer::ModelsConfig::Openai { default, available } => {
            assert_eq!(default, "deepseek-v4-flash");
            assert_eq!(
                available,
                vec!["deepseek-v4-flash".to_string(), "kimi-k3".to_string(), "legacy".to_string()],
                "responses-class models are excluded from chat binds"
            );
        }
        other => panic!("expected Openai config, got {other:?}"),
    }
}

#[test]
fn filter_models_for_openai_rejects_responses_only_default() {
    let mut abilities = HashMap::new();
    abilities.insert("grok-4.5".into(), ab_with_api("response-api"));
    let models = crate::config_writer::ModelsConfig::Openai {
        default: "grok-4.5".into(),
        available: vec!["grok-4.5".into()],
    };
    assert!(filter_models_for_openai(models, &abilities).is_err());
}

#[test]
fn filter_models_for_openai_passes_anthropic_config_through() {
    let mut abilities = HashMap::new();
    abilities.insert("grok-4.5".into(), ab_with_api("response-api"));
    let models = crate::config_writer::ModelsConfig::Anthropic {
        default: "claude-sonnet".into(),
        haiku: "claude-sonnet".into(),
        sonnet: "claude-sonnet".into(),
        opus: "claude-sonnet".into(),
    };
    // Anthropic-shaped configs are not the openai filter's concern.
    assert!(filter_models_for_openai(models, &abilities).is_ok());
}

#[test]
fn filter_models_for_anthropic_passes_openai_config_through() {
    let mut abilities = HashMap::new();
    abilities.insert("grok-4.5".into(), ab_with_api("openai-comp"));
    let models = crate::config_writer::ModelsConfig::Openai {
        default: "grok-4.5".into(),
        available: vec!["grok-4.5".into(), "deepseek-v4-flash".into()],
    };
    // OpenAI-protocol binds keep the full list — no filtering.
    match filter_models_for_anthropic(models, &abilities).unwrap() {
        crate::config_writer::ModelsConfig::Openai { available, .. } => {
            assert_eq!(available.len(), 2);
        }
        other => panic!("expected Openai config, got {other:?}"),
    }
}