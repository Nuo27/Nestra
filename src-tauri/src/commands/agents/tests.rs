use super::*;

#[test]
fn parse_models_tier_fallback_to_default() {
    let json = r#"{"default":"gpt-4o","available":["gpt-4o","gpt-3.5"],"haiku":"","sonnet":"","opus":""}"#;
    let m = parse_models(crate::config_writer::ProviderKind::Anthropic, Some(json)).unwrap();
    match m {
        ModelsConfig::Anthropic { default, haiku, sonnet, opus } => {
            assert_eq!(default, "gpt-4o");
            assert_eq!(haiku, "gpt-4o");
            assert_eq!(sonnet, "gpt-4o");
            assert_eq!(opus, "gpt-4o");
        }
        _ => panic!("expected Anthropic variant"),
    }
}

#[test]
fn parse_models_preserves_explicit_tier_picks() {
    let json = r#"{"default":"gpt-4o","haiku":"haiku-x","sonnet":"sonnet-x","opus":"opus-x"}"#;
    let m = parse_models(crate::config_writer::ProviderKind::Anthropic, Some(json)).unwrap();
    match m {
        ModelsConfig::Anthropic { haiku, sonnet, opus, .. } => {
            assert_eq!(haiku, "haiku-x");
            assert_eq!(sonnet, "sonnet-x");
            assert_eq!(opus, "opus-x");
        }
        _ => panic!(),
    }
}

#[test]
fn parse_models_openai_shape_uses_default_and_available() {
    let json = r#"{"default":"gpt-4o","available":["gpt-4o","gpt-3.5"]}"#;
    let m = parse_models(crate::config_writer::ProviderKind::Openai, Some(json)).unwrap();
    match m {
        ModelsConfig::Openai { default, available } => {
            assert_eq!(default, "gpt-4o");
            assert_eq!(available, vec!["gpt-4o", "gpt-3.5"]);
        }
        _ => panic!("expected Openai variant"),
    }
}

fn open_db() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();
    conn
}

#[test]
fn build_switch_context_merges_cache_with_persisted_override() {
    use crate::db;
    use crate::model_abilities::{ModelAbilities, ModelLimit};
    use std::collections::HashMap;

    let conn = open_db();

    // Seed models.dev cache as if `refresh_if_stale` had populated it.
    let payload = serde_json::json!({
        "minimax/MiniMax-M3": {
            "id": "minimax/MiniMax-M3",
            "reasoning": true,
            "tool_call": true,
            "attachment": true,
            "temperature": false,
            "limit": { "context": 512000, "output": 128000 }
        }
    });
    db::set_setting(
        &conn,
        "models_dev_cache",
        &serde_json::json!({
            "fetched_at": chrono::Utc::now().timestamp_millis(),
            "json": payload
        }),
    )
    .unwrap();

    // Create an endpoint with a saved override: temperature flipped on,
    // no other fields set. Plus a model id the cache doesn't know.
    db::create_endpoint(&conn, "minimax-cn", "anthropic", "minimax").unwrap();
    db::upsert_endpoint_protocol(
        &conn,
        "minimax-cn",
        "anthropic",
        "https://api.minimaxi.com/anthropic",
    )
    .unwrap();
    db::set_endpoint_models(
        &conn,
        "minimax-cn",
        r#"{"default":"MiniMax-M3","haiku":"","sonnet":"","opus":"","available":["MiniMax-M3","MiniMax-M2.7"]}"#,
    )
    .unwrap();
    let mut saved: HashMap<String, ModelAbilities> = HashMap::new();
    saved.insert(
        "MiniMax-M3".into(),
        ModelAbilities {
            reasoning: None,
            tool_call: None,
            attachment: None,
            temperature: Some(true), // override: flip on
            limit: None,
            modalities: None,
            api: None,
            cost: None,
        },
    );
    saved.insert(
        "MiniMax-M2.7".into(),
        ModelAbilities {
            reasoning: Some(true),
            tool_call: Some(true),
            attachment: Some(true),
            temperature: Some(false),
            limit: Some(ModelLimit { context: 1, output: 1, input: None }),
            modalities: None,
            api: None,
            cost: None,
        },
    );
    db::set_endpoint_model_abilities(
        &conn,
        "minimax-cn",
        Some(&serde_json::to_string(&saved).unwrap()),
    )
    .unwrap();

    // Reproduce the merge step build_switch_context runs.
    let index = crate::model_abilities::load_index(&conn).unwrap();
    let ids = vec!["MiniMax-M3".to_string(), "MiniMax-M2.7".to_string()];
    let defaults = crate::model_abilities::subset_for(&index, &ids);
    let overrides = crate::model_abilities::parse_overrides(
        db::get_endpoint(&conn, "minimax-cn")
            .unwrap()
            .unwrap()
            .model_abilities_json
            .as_deref(),
    );
    let merged = crate::model_abilities::merge_into(defaults, overrides);

    let m3 = merged.get("MiniMax-M3").unwrap();
    assert_eq!(m3.reasoning, Some(true), "default reasoning preserved");
    assert_eq!(m3.tool_call, Some(true));
    assert_eq!(m3.attachment, Some(true));
    assert_eq!(m3.temperature, Some(true), "override flipped temperature");
    assert_eq!(m3.limit.as_ref().unwrap().context, 512000);

    let m27 = merged.get("MiniMax-M2.7").unwrap();
    assert_eq!(m27.reasoning, Some(true), "override-only entry populates fully");
    assert_eq!(m27.tool_call, Some(true));
    assert_eq!(m27.attachment, Some(true));
}

#[test]
fn detection_cadence_defaults_to_on_launch() {
    let conn = open_db();
    assert_eq!(detection_cadence(&conn), "on-launch");
}

#[test]
fn detection_cadence_reads_manual() {
    let conn = open_db();
    crate::db::set_setting(
        &conn,
        "app",
        &serde_json::json!({ "detection_cadence": "manual" }),
    )
    .unwrap();
    assert_eq!(detection_cadence(&conn), "manual");
}

#[test]
fn agent_list_reads_cache_without_redetecting() {
    let conn = open_db();
    // A seeded 'missing' row with a known timestamp. agent_list (cache read)
    // must return it untouched — detection would have flipped it to "ok" or
    // bumped last_detected_at, so this proves no re-scan happened. Seeded on
    // a registry id (claude-code) because list_agent_infos only surfaces
    // agents from the closed AGENTS registry.
    crate::db::upsert_agent(&conn, "claude-code-cli", "claude-code-cli", "Claude Code", None, None, "missing", None)
        .unwrap();
    let infos = list_agent_infos(&conn).unwrap();
    let row = infos
        .iter()
        .find(|i| i.id == "claude-code-cli")
        .expect("seeded claude-code row present");
    assert_eq!(row.status, "missing");
    assert_eq!(row.agent_path, None);
}

#[test]
fn agent_list_filters_rows_not_in_registry() {
    let conn = open_db();
    // Rows whose kind has no detector in the registry.
    // They must never surface in the Agents page.
    for (id, kind, display) in [
        ("copilot-cli", "copilot-cli", "GitHub Copilot CLI"),
        ("opencode", "opencode", "OpenCode"),
        ("qwen-code", "qwen-code", "Qwen Code"),
    ] {
        crate::db::upsert_agent(&conn, id, kind, display, None, None, "missing", None)
            .unwrap();
    }
    crate::db::upsert_agent(&conn, "pi-cli", "pi-cli", "Pi", None, None, "ok", None).unwrap();
    let infos = list_agent_infos(&conn).unwrap();
    let ids: Vec<&str> = infos.iter().map(|i| i.id.as_str()).collect();
    for ghost in ["copilot-cli", "opencode", "qwen-code"] {
        assert!(!ids.contains(&ghost), "{ghost} must be filtered out");
    }
    assert!(ids.contains(&"pi-cli"), "registry agent stays visible");
}