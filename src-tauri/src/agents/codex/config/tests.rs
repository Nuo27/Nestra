use super::*;
use crate::config_writer::{backup_path_for, ModelsConfig, SwitchContext};
use crate::model_abilities::{ModelAbilities, ModelLimit};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

const FIXTURE: &str = include_str!("../../../fixtures/codex/config.toml");

fn tmp() -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("")
        .tempdir()
        .expect("tempdir");
    (dir.path().to_path_buf(), dir)
}

fn ctx() -> SwitchContext {
    SwitchContext {
        provider_id: "zai".into(),
        provider_kind: ProviderKind::Responses,
        display_name: "Z.ai Responses".into(),
        base_url: "https://api.z.ai/api/coding/v1/responses".into(),
        api_key: "sk-test".into(),
        models: ModelsConfig::Openai {
            default: "glm-5.3".into(),
            available: vec!["glm-5.3".into()],
        },
        advanced_env: Default::default(),
        model_abilities: Default::default(),
    }
}

fn apply_to_fixture(c: &SwitchContext) -> toml_edit::DocumentMut {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.toml");
    fs::write(&cfg, FIXTURE).unwrap();
    Codex.apply(&cfg, c).unwrap();
    fs::read_to_string(&cfg).unwrap().parse().unwrap()
}

#[test]
fn apply_writes_provider_and_selection_keys() {
    let doc = apply_to_fixture(&ctx());
    let entry = &doc["model_providers"]["nestra-zai"];
    assert_eq!(entry["wire_api"].as_str(), Some("responses"));
    // /v1/responses tail stripped — Codex appends /responses itself.
    assert_eq!(entry["base_url"].as_str(), Some("https://api.z.ai/api/coding/v1"));
    assert_eq!(entry["requires_openai_auth"].as_bool(), Some(true));
    assert_eq!(entry["experimental_bearer_token"].as_str(), Some("sk-test"));
    assert_eq!(doc["model_provider"].as_str(), Some("nestra-zai"));
    assert_eq!(doc["model"].as_str(), Some("glm-5.3"));
}

#[test]
fn apply_preserves_every_unrelated_section() {
    let doc = apply_to_fixture(&ctx());
    assert_eq!(doc["model_reasoning_effort"].as_str(), Some("medium"));
    assert_eq!(doc["features"]["goals"].as_bool(), Some(true));
    assert_eq!(
        doc["desktop"]["conversationDetailMode"].as_str(),
        Some("STEPS_COMMANDS")
    );
    assert!(doc["marketplaces"]["openai-bundled"].is_table());
    assert!(doc["plugins"]["visualize@openai-bundled"].is_table());
    assert!(doc["mcp_servers"]["codegraph"].is_table());
    assert!(doc["mcp_servers"]["unity"].is_table());
    assert!(doc["projects"]["c:\\users\\me\\demo"].is_table());
    // The user's own provider table survives alongside the Nestra one.
    assert!(doc["model_providers"]["custom"].is_table());
}

#[test]
fn switch_replaces_previous_nestra_provider() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.toml");
    fs::write(&cfg, FIXTURE).unwrap();
    Codex.apply(&cfg, &ctx()).unwrap();
    let mut second = ctx();
    second.provider_id = "other".into();
    Codex.apply(&cfg, &second).unwrap();
    let doc: DocumentMut = fs::read_to_string(&cfg).unwrap().parse().unwrap();
    assert!(doc["model_providers"].get("nestra-zai").is_none());
    assert!(doc["model_providers"]["nestra-other"].is_table());
    assert_eq!(doc["model_provider"].as_str(), Some("nestra-other"));
}

#[test]
fn context_window_written_from_abilities() {
    let mut c = ctx();
    let mut abilities = HashMap::new();
    abilities.insert(
        "glm-5.3".to_string(),
        ModelAbilities {
            limit: Some(ModelLimit { context: 400_000, output: 128_000, input: None }),
            ..Default::default()
        },
    );
    c.model_abilities = abilities;
    let doc = apply_to_fixture(&c);
    assert_eq!(doc["model_context_window"].as_integer(), Some(400_000));
}

#[test]
fn gateway_set_points_at_alias_with_sentinel() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.toml");
    fs::write(&cfg, FIXTURE).unwrap();
    let alias = crate::config_writer::GatewayAlias::simple(
        "http://127.0.0.1:18777/codex-desktop",
        "nestra",
        "loopback-token",
    );
    Codex.apply_gateway_set(&cfg, &alias).unwrap();
    let doc: DocumentMut = fs::read_to_string(&cfg).unwrap().parse().unwrap();
    let entry = &doc["model_providers"]["nestra-gateway"];
    assert_eq!(entry["base_url"].as_str(), Some("http://127.0.0.1:18777/codex-desktop/v1"));
    assert_eq!(entry["experimental_bearer_token"].as_str(), Some("loopback-token"));
    assert_eq!(doc["model"].as_str(), Some("nestra"));
    assert_eq!(doc["model_provider"].as_str(), Some("nestra-gateway"));
}

#[test]
fn inspect_lists_user_providers_only() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.toml");
    fs::write(&cfg, FIXTURE).unwrap();
    let detected = Codex.inspect(&cfg).unwrap();
    assert_eq!(detected.len(), 1);
    assert_eq!(detected[0].key, "custom");
    assert!(!detected[0].managed);
}

#[test]
fn remove_drops_table_and_selection_key() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.toml");
    fs::write(&cfg, FIXTURE).unwrap();
    Codex.apply(&cfg, &ctx()).unwrap();
    Codex.remove(&cfg, "nestra-zai").unwrap();
    let doc: DocumentMut = fs::read_to_string(&cfg).unwrap().parse().unwrap();
    assert!(doc["model_providers"].get("nestra-zai").is_none());
    assert!(doc.get("model_provider").is_none());
    // Removing an unrelated provider keeps the selection keys.
    Codex.apply(&cfg, &ctx()).unwrap();
    Codex.remove(&cfg, "custom").unwrap();
    let doc: DocumentMut = fs::read_to_string(&cfg).unwrap().parse().unwrap();
    assert_eq!(doc["model_provider"].as_str(), Some("nestra-zai"));
}

#[test]
fn apply_creates_backup_once() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.toml");
    fs::write(&cfg, FIXTURE).unwrap();
    assert!(Codex.apply(&cfg, &ctx()).unwrap(), "first apply creates backup");
    assert!(backup_path_for(&cfg).exists());
    assert!(!Codex.apply(&cfg, &ctx()).unwrap(), "second apply must not re-backup");
}
