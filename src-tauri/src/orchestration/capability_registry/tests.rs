use super::*;
use crate::config_writer::ProviderKind;
use crate::model_abilities::{ModelAbilities, ModelLimit, Modality, Modalities};
use crate::schema;

fn abilities_with(context: u64) -> ModelAbilities {
    ModelAbilities {
        reasoning: Some(true),
        tool_call: Some(true),
        attachment: None,
        temperature: None,
        limit: Some(ModelLimit {
            context,
            output: 8192,
            input: None,
        }),
        modalities: None,
        api: None,
        cost: None,
    }
}

#[test]
fn satisfies_filters_on_explicit_false_only() {
    let a = abilities_with(200_000);
    // Required reasoning + tool_call, model has both → ok.
    assert!(satisfies(
        &CapabilityReq {
            reasoning: true,
            tool_call: true,
            vision: false,
            context_floor: Some(100_000),
        },
        &a
    ));
    // Context floor too high → reject.
    assert!(!satisfies(
        &CapabilityReq {
            reasoning: true,
            tool_call: true,
            vision: false,
            context_floor: Some(500_000),
        },
        &a
    ));
    // Reasoning required, model reports false → reject.
    let no_reason = ModelAbilities {
        reasoning: Some(false),
        ..a.clone()
    };
    assert!(!satisfies(
        &CapabilityReq {
            reasoning: true,
            tool_call: false,
            vision: false,
            context_floor: None,
        },
        &no_reason
    ));
}

#[test]
fn unknown_capability_does_not_filter() {
    // A model with NO ability data (all None) must be eligible for any
    // request — we over-include rather than reject on missing data.
    let unknown = ModelAbilities {
        reasoning: None,
        tool_call: None,
        attachment: None,
        temperature: None,
        limit: None,
        modalities: None,
        api: None,
        cost: None,
    };
    assert!(satisfies(
        &CapabilityReq {
            reasoning: true,
            tool_call: true,
            vision: true,
            context_floor: Some(1_000_000),
        },
        &unknown
    ));
}

#[test]
fn vision_checks_image_modality() {
    let with_image = ModelAbilities {
        modalities: Some(Modalities {
            input: vec![Modality::Text, Modality::Image],
            output: vec![Modality::Text],
        }),
        ..abilities_with(200_000)
    };
    let text_only = ModelAbilities {
        modalities: Some(Modalities {
            input: vec![Modality::Text],
            output: vec![Modality::Text],
        }),
        ..abilities_with(200_000)
    };
    let vision_req = CapabilityReq {
        reasoning: false,
        tool_call: false,
        vision: true,
        context_floor: None,
    };
    assert!(satisfies(&vision_req, &with_image));
    assert!(!satisfies(&vision_req, &text_only));
}

#[test]
fn model_ids_from_anthropic_dedupes_tiers() {
    let json = r#"{"default":"sonnet","haiku":"haiku","sonnet":"sonnet","opus":"opus","available":[]}"#;
    let ids = model_ids_from(ProviderKind::Anthropic, Some(json));
    // ModelsConfig::Anthropic ids() dedupes; order is default first.
    assert!(ids.contains(&"sonnet".to_string()));
    assert!(ids.contains(&"haiku".to_string()));
    assert!(ids.contains(&"opus".to_string()));
    // No duplicates.
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), ids.len());
}

#[test]
fn model_ids_from_anthropic_unions_available() {
    // opencode-go's anthropic row: tiers empty (fall back to default),
    // full list lives in `available` — the catalog must index all of
    // them, not just default+tiers (regression: catalog held 1 of 25).
    let json = r#"{"default":"deepseek-v4-flash","haiku":"","opus":"","sonnet":"",
            "available":["deepseek-v4-flash","grok-4.5","kimi-k3","qwen3.8-max"]}"#;
    let ids = model_ids_from(ProviderKind::Anthropic, Some(json));
    assert_eq!(ids.len(), 4);
    for m in ["deepseek-v4-flash", "grok-4.5", "kimi-k3", "qwen3.8-max"] {
        assert!(ids.contains(&m.to_string()), "missing {m}");
    }
    // Tier models already covered by available are not duplicated.
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), ids.len());
}

#[test]
fn model_ids_from_openai_uses_available_plus_default() {
    let json = r#"{"default":"gpt-4o","available":["gpt-4o","gpt-4o-mini"]}"#;
    let ids = model_ids_from(ProviderKind::Openai, Some(json));
    assert_eq!(ids, vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()]);
}

#[test]
fn rebuild_populates_catalog_from_endpoints() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::build_v1(&conn).unwrap();
    // Seed an endpoint with a model + protocol.
    conn.execute(
        "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status, models_json)
             VALUES ('ep-1','custom','Main',0,'unvalidated',
                     '{\"default\":\"m-1\",\"available\":[\"m-1\",\"m-2\"]}')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url)
             VALUES ('ep-1','openai-comp','https://x')",
        [],
    )
    .unwrap();

    let n = rebuild(&conn).unwrap();
    assert_eq!(n, 2, "two distinct models should be cataloged");

    let rows = store::list_model_catalog(&conn, "ep-1").unwrap();
    let ids: Vec<_> = rows.iter().map(|r| r.model_id.as_str()).collect();
    assert!(ids.contains(&"m-1"));
    assert!(ids.contains(&"m-2"));
    // abilities_json is valid ModelAbilities JSON (defaulted, all-None
    // since no models.dev match for fake ids).
    for r in &rows {
        let parsed: ModelAbilities = serde_json::from_str(&r.abilities_json).unwrap();
        assert_eq!(parsed, ModelAbilities::default());
    }
}

#[test]
fn rebuild_respects_user_overrides() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::build_v1(&conn).unwrap();
    conn.execute(
        "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status,
                                            models_json, model_abilities_json)
             VALUES ('ep-1','custom','Main',0,'unvalidated',
                     '{\"default\":\"m-1\"}',
                     '{\"m-1\":{\"reasoning\":true}}')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO endpoint_protocol (endpoint_id, protocol, base_url)
             VALUES ('ep-1','openai-comp','https://x')",
        [],
    )
    .unwrap();

    rebuild(&conn).unwrap();
    let rows = store::list_model_catalog(&conn, "ep-1").unwrap();
    let a: ModelAbilities = serde_json::from_str(&rows[0].abilities_json).unwrap();
    assert_eq!(a.reasoning, Some(true), "user override must land in catalog");
}

#[test]
fn derive_capability_req_anthropic_full_signal() {
    let body = br#"{"model":"m","thinking":{"budget_tokens":1024},"tools":[{"name":"bash"}],
             "messages":[{"role":"user","content":[{"type":"text","text":"see"},
                                                   {"type":"image","source":{"data":"x"}}]}]}"#;
    let req = derive_capability_req(body, ProviderKind::Anthropic);
    assert!(req.tool_call && req.vision && req.reasoning);
}

#[test]
fn derive_capability_req_conservative_on_plain_text() {
    // No tools, string content (not blocks), no thinking → no constraints
    // on either dialect — a text-only request must never filter.
    let body = br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
    assert_eq!(
        derive_capability_req(body, ProviderKind::Anthropic),
        CapabilityReq::default()
    );
    assert_eq!(
        derive_capability_req(body, ProviderKind::Openai),
        CapabilityReq::default()
    );
}

#[test]
fn derive_capability_req_openai_functions_and_image_url() {
    let body = br#"{"model":"m","functions":[{"name":"f"}],
             "messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:"}}]}]}"#;
    let req = derive_capability_req(body, ProviderKind::Openai);
    assert!(req.tool_call && req.vision && !req.reasoning);
}

#[test]
fn derive_capability_req_tolerates_garbage_and_null_thinking() {
    assert_eq!(
        derive_capability_req(b"not json", ProviderKind::Anthropic),
        CapabilityReq::default()
    );
    let req =
        derive_capability_req(br#"{"thinking":null,"messages":[]}"#, ProviderKind::Anthropic);
    assert!(!req.reasoning, "explicit null thinking counts as absent");
}