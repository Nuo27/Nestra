use super::*;
use std::path::PathBuf;
use crate::config_writer::{backup_path_for, ProviderKind, SwitchContext};
use crate::model_abilities::{ModelAbilities, ModelLimit};
use serde_json::json;
use std::fs;
use std::collections::HashMap;

const FIXTURE: &str = include_str!("../../fixtures/claude_code/settings.json");

fn tmp() -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("")
        .tempdir()
        .expect("tempdir");
    (dir.path().to_path_buf(), dir)
}

fn ctx() -> SwitchContext {
    SwitchContext {
        provider_id: "anthropic".into(),
        provider_kind: ProviderKind::Anthropic,
        display_name: "Anthropic".into(),
        base_url: "https://api.anthropic.com".into(),
        api_key: "sk-test".into(),
        models: ModelsConfig::Anthropic {
            default: "claude-default".into(),
            haiku: "claude-haiku".into(),
            sonnet: "claude-sonnet".into(),
            opus: "claude-opus".into(),
        },
        advanced_env: {
            let mut m = serde_json::Map::new();
            m.insert("API_TIMEOUT_MS".into(), json!(3000000));
            m.insert("ANTHROPIC_AUTH_TOKEN".into(), json!("should-not-overwrite"));
            m
        },
        model_abilities: Default::default(),
    }
}

#[test]
fn apply_writes_env_block_and_preserves_rest() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("settings.json");
    fs::write(&cfg, FIXTURE).unwrap();

    let out = ClaudeCode.apply(&cfg, &ctx()).unwrap();
    assert!(out);

    let written: serde_json::Value = serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    let env = written.get("env").unwrap().as_object().unwrap();
    assert_eq!(env["ANTHROPIC_BASE_URL"], "https://api.anthropic.com");
    assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "sk-test");
    assert_eq!(env["ANTHROPIC_MODEL"], "claude-default");
    assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "claude-haiku");
    assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "claude-sonnet");
    assert_eq!(env["ANTHROPIC_DEFAULT_OPUS_MODEL"], "claude-opus");
    // user var preserved
    assert_eq!(env["SOME_USER_VAR"], "keep-me");
    // advanced env merged as string, reserved key not overwritten
    assert_eq!(env["API_TIMEOUT_MS"], "3000000");
    assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "sk-test");
    // top-level permissions preserved
    assert!(written.get("permissions").is_some());
}

fn abilities_with_context(model_id: &str, ctx: u64) -> HashMap<String, ModelAbilities> {
    let mut m = HashMap::new();
    m.insert(
        model_id.into(),
        ModelAbilities {
            reasoning: None,
            tool_call: None,
            attachment: None,
            temperature: None,
            api: None,
            limit: Some(ModelLimit { context: ctx, output: 8_000, input: None }),
            modalities: None,
        },
    );
    m
}

#[test]
fn apply_appends_1m_suffix_to_models_at_or_above_threshold() {
    // default + sonnet + opus get the marker; haiku stays bare (200k).
    let mut c = ctx();
    c.model_abilities = abilities_with_context("claude-default", 1_000_000)
        .into_iter()
        .chain(abilities_with_context("claude-sonnet", 1_000_000))
        .chain(abilities_with_context("claude-opus", 2_000_000))
        .chain(abilities_with_context("claude-haiku", 200_000))
        .collect();

    let (dir, _dir_g) = tmp();
    let cfg = dir.join("settings.json");
    fs::write(&cfg, FIXTURE).unwrap();
    ClaudeCode.apply(&cfg, &c).unwrap();

    let written: serde_json::Value = serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    let env = written.get("env").unwrap().as_object().unwrap();
    assert_eq!(env["ANTHROPIC_MODEL"], "claude-default[1m]");
    assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "claude-haiku");
    assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "claude-sonnet[1m]");
    assert_eq!(env["ANTHROPIC_DEFAULT_OPUS_MODEL"], "claude-opus[1m]");
}

#[test]
fn apply_is_noop_when_abilities_map_is_empty() {
    // No model_abilities populated → baseline writes (no suffix), proving
    // the helper is reached but returns bare ids when abilities are
    // missing entirely (e.g. cold models.dev cache).
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("settings.json");
    fs::write(&cfg, FIXTURE).unwrap();
    ClaudeCode.apply(&cfg, &ctx()).unwrap();

    let written: serde_json::Value = serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    let env = written.get("env").unwrap().as_object().unwrap();
    assert_eq!(env["ANTHROPIC_MODEL"], "claude-default");
    assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "claude-haiku");
}

#[test]
fn apply_does_not_double_append_when_id_already_carries_marker() {
    // User typed the model name with the marker themselves. Idempotent.
    let mut c = ctx();
    c.models = ModelsConfig::Anthropic {
        default: "claude-default[1m]".into(),
        haiku: "claude-haiku".into(),
        sonnet: "claude-sonnet[1M]".into(),
        opus: "claude-opus".into(),
    };
    c.model_abilities = abilities_with_context("claude-default[1m]", 1_000_000)
        .into_iter()
        .chain(abilities_with_context("claude-haiku", 200_000))
        .chain(abilities_with_context("claude-sonnet[1M]", 1_000_000))
        .chain(abilities_with_context("claude-opus", 1_000_000))
        .collect();

    let (dir, _dir_g) = tmp();
    let cfg = dir.join("settings.json");
    fs::write(&cfg, FIXTURE).unwrap();
    ClaudeCode.apply(&cfg, &c).unwrap();

    let written: serde_json::Value = serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    let env = written.get("env").unwrap().as_object().unwrap();
    assert_eq!(env["ANTHROPIC_MODEL"], "claude-default[1m]");
    assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "claude-haiku");
    assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "claude-sonnet[1M]");
    assert_eq!(env["ANTHROPIC_DEFAULT_OPUS_MODEL"], "claude-opus[1m]");
}

#[test]
fn apply_anthropic_compatible_aggregator_blanks_api_key() {
    // OpenRouter (and any Anthropic-compatible aggregator) now binds through
    // an `anthropic` protocol row, so provider_kind is Anthropic — but the
    // base_url is NOT the official endpoint. A stale real-Anthropic
    // ANTHROPIC_API_KEY would win over the auth token and misroute, so it
    // must still be blanked (base_url-driven, not kind-driven).
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("settings.json");
    fs::write(
        &cfg,
        r#"{ "env": { "ANTHROPIC_API_KEY": "sk-stale-anthropic", "SOME_USER_VAR": "keep-me" } }"#,
    )
    .unwrap();

    let mut c = ctx();
    c.provider_kind = ProviderKind::Anthropic;
    c.base_url = "https://openrouter.ai/api/v1".into();
    c.models = ModelsConfig::Openai {
        default: "openrouter/auto".into(),
        available: vec!["openrouter/auto".into()],
    };
    ClaudeCode.apply(&cfg, &c).unwrap();

    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    let env = written["env"].as_object().unwrap();
    assert_eq!(env["ANTHROPIC_BASE_URL"], "https://openrouter.ai/api");
    assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "sk-test");
    assert_eq!(env["ANTHROPIC_API_KEY"], "", "stale key must be blanked for non-official endpoints");
    assert_eq!(env["SOME_USER_VAR"], "keep-me", "user vars preserved");
}

#[test]
fn restore_is_byte_exact() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("settings.json");
    fs::write(&cfg, FIXTURE).unwrap();
    let original = fs::read(&cfg).unwrap();

    ClaudeCode.apply(&cfg, &ctx()).unwrap();
    ClaudeCode.restore(&cfg).unwrap();

    assert_eq!(fs::read(&cfg).unwrap(), original);
    assert!(!backup_path_for(&cfg).exists());
}

#[test]
fn second_switch_does_not_re_backup() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("settings.json");
    fs::write(&cfg, FIXTURE).unwrap();

    ClaudeCode.apply(&cfg, &ctx()).unwrap();
    let backup_bytes = fs::read(backup_path_for(&cfg)).unwrap();
    // simulate a user edit between switches
    fs::write(&cfg, "{ \"permissions\": {} }").unwrap();

    let out = ClaudeCode.apply(&cfg, &ctx()).unwrap();
    assert!(!out); // reused
    assert_eq!(fs::read(backup_path_for(&cfg)).unwrap(), backup_bytes);
}

#[test]
fn mark_onboarding_merges_into_existing_file() {
    let (dir, _dir_g) = tmp();
    let path = dir.join(".claude.json");
    fs::write(&path, r#"{ "mcpServers": { "x": {} }, "numStartups": 5 }"#).unwrap();

    mark_onboarding_complete(&dir).unwrap();

    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(v["hasCompletedOnboarding"], serde_json::Value::Bool(true));
    // existing content preserved
    assert_eq!(v["numStartups"], 5);
    assert!(v["mcpServers"]["x"].is_object());
}

#[test]
fn mark_onboarding_creates_file_when_absent() {
    let (dir, _dir_g) = tmp();
    let path = dir.join(".claude.json");
    assert!(!path.exists());

    mark_onboarding_complete(&dir).unwrap();

    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(v["hasCompletedOnboarding"], serde_json::Value::Bool(true));
}

#[test]
fn apply_set_rejects_multiple_entries() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("settings.json");
    fs::write(&cfg, FIXTURE).unwrap();
    let set = ProviderSet {
        entries: vec![ctx(), ctx()],
        default_provider_id: "anthropic".into(),
        default_model: "claude-default".into(),
    };
    assert!(ClaudeCode.apply_set(&cfg, &set).is_err());
}

fn gw_alias(tiers: Option<crate::config_writer::TierAliases>) -> crate::config_writer::GatewayAlias {
    crate::config_writer::GatewayAlias {
        gateway_base_url: "http://127.0.0.1:18777/claude-code-cli".into(),
        model_alias: crate::config_writer::AliasModel {
            id: "claude-sonnet-4-5".into(),
            abilities: None,
        },
        tier_aliases: tiers,
        sentinel_key: "gw-token".into(),
    }
}

fn tier_slot(id: &str, ctx: u64) -> crate::config_writer::AliasModel {
    crate::config_writer::AliasModel {
        id: id.into(),
        abilities: Some(ModelAbilities {
            reasoning: None,
            tool_call: None,
            attachment: None,
            temperature: None,
            limit: Some(ModelLimit { context: ctx, output: 8_000, input: None }),
            modalities: None,
            api: None,
        }),
    }
}

#[test]
fn apply_gateway_set_writes_per_tier_aliases_with_1m_markers() {
    // Tier slots get distinct real CC ids, each `[1m]`-marked iff ITS
    // steady-state model advertises >=1M context (haiku tier here is a
    // 200k model → bare; sonnet/opus are 1M+ → marked).
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("settings.json");
    fs::write(&cfg, FIXTURE).unwrap();
    let alias = gw_alias(Some(crate::config_writer::TierAliases {
        haiku: tier_slot("claude-haiku-4-5", 200_000),
        sonnet: tier_slot("claude-sonnet-4-5", 1_000_000),
        opus: tier_slot("claude-opus-4-5", 2_000_000),
    }));
    ClaudeCode.apply_gateway_set(&cfg, &alias).unwrap();

    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    let env = written["env"].as_object().unwrap();
    assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:18777/claude-code-cli");
    assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "gw-token");
    // Primary = the sonnet-class slot, marked.
    assert_eq!(env["ANTHROPIC_MODEL"], "claude-sonnet-4-5[1m]");
    assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "claude-haiku-4-5");
    assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "claude-sonnet-4-5[1m]");
    assert_eq!(env["ANTHROPIC_DEFAULT_OPUS_MODEL"], "claude-opus-4-5[1m]");
    assert_eq!(env["ANTHROPIC_API_KEY"], "");
}

#[test]
fn apply_gateway_set_without_tiers_repeats_primary_alias_bare() {
    // No tier slots (non-resolving steady state) → every env var repeats
    // the primary alias, no `[1m]` (no abilities to justify it).
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("settings.json");
    fs::write(&cfg, FIXTURE).unwrap();
    let alias = gw_alias(None);
    ClaudeCode.apply_gateway_set(&cfg, &alias).unwrap();

    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    let env = written["env"].as_object().unwrap();
    for key in [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
    ] {
        assert_eq!(env[key], "claude-sonnet-4-5", "{key} repeats the bare primary");
    }
}