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
    // No auth.json in the tempdir → no ChatGPT login → pure-API shape.
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.toml");
    fs::write(&cfg, FIXTURE).unwrap();
    Codex.apply(&cfg, &ctx()).unwrap();
    let doc: DocumentMut = fs::read_to_string(&cfg).unwrap().parse().unwrap();
    let entry = &doc["model_providers"]["nestra-zai"];
    assert_eq!(entry["wire_api"].as_str(), Some("responses"));
    // /v1/responses tail stripped — Codex appends /responses itself.
    assert_eq!(entry["base_url"].as_str(), Some("https://api.z.ai/api/coding/v1"));
    // requires_openai_auth is ABSENT — `true` is what makes the app demand
    // a ChatGPT login when auth.json carries no credentials.
    assert!(entry.get("requires_openai_auth").is_none());
    assert_eq!(entry["experimental_bearer_token"].as_str(), Some("sk-test"));
    assert_eq!(doc["model_provider"].as_str(), Some("nestra-zai"));
    assert_eq!(doc["model"].as_str(), Some("glm-5.3"));
    // The key also lands in auth.json so the app's login gate passes.
    let auth: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("auth.json")).unwrap()).unwrap();
    assert_eq!(auth["OPENAI_API_KEY"].as_str(), Some("sk-test"));
    assert_eq!(auth["auth_mode"].as_str(), Some("apikey"));
    assert!(backup_path_for(&dir.join("auth.json")).exists());
}

#[test]
fn apply_keeps_official_login_shape_when_tokens_present() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.toml");
    fs::write(&cfg, FIXTURE).unwrap();
    let original = r#"{"auth_mode":"chatgpt","tokens":{"id_token":"t","access_token":"a","refresh_token":"r"}}"#;
    fs::write(dir.join("auth.json"), original).unwrap();
    Codex.apply(&cfg, &ctx()).unwrap();
    let doc: DocumentMut = fs::read_to_string(&cfg).unwrap().parse().unwrap();
    assert_eq!(
        doc["model_providers"]["nestra-zai"]["requires_openai_auth"].as_bool(),
        Some(true)
    );
    // auth.json stays byte-identical — the login state is never touched.
    assert_eq!(fs::read_to_string(dir.join("auth.json")).unwrap(), original);
}

#[test]
fn pure_api_refreshes_stale_key_on_reswitch() {
    // Nestra owns the OPENAI_API_KEY slot in pure mode: a key left by an
    // earlier switch (to another provider) must NOT flip the adapter into
    // the keep-login shape — it is refreshed instead.
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.toml");
    fs::write(&cfg, FIXTURE).unwrap();
    fs::write(dir.join("auth.json"), r#"{"OPENAI_API_KEY":"stale","auth_mode":"apikey"}"#).unwrap();
    Codex.apply(&cfg, &ctx()).unwrap();
    let doc: DocumentMut = fs::read_to_string(&cfg).unwrap().parse().unwrap();
    assert!(doc["model_providers"]["nestra-zai"]
        .get("requires_openai_auth")
        .is_none());
    let auth: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("auth.json")).unwrap()).unwrap();
    assert_eq!(auth["OPENAI_API_KEY"].as_str(), Some("sk-test"));
}

#[test]
fn pure_api_write_preserves_unrelated_auth_fields() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.toml");
    fs::write(&cfg, FIXTURE).unwrap();
    fs::write(
        dir.join("auth.json"),
        r#"{"auth_mode":"apikey","last_refresh":"2026-01-01"}"#,
    )
    .unwrap();
    Codex.apply(&cfg, &ctx()).unwrap();
    let auth: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("auth.json")).unwrap()).unwrap();
    assert_eq!(auth["last_refresh"].as_str(), Some("2026-01-01"));
    assert_eq!(auth["OPENAI_API_KEY"].as_str(), Some("sk-test"));
}

#[test]
fn pure_api_write_fails_loudly_on_unparseable_auth_json() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.toml");
    fs::write(&cfg, FIXTURE).unwrap();
    fs::write(dir.join("auth.json"), "not json{").unwrap();
    let err = Codex.apply(&cfg, &ctx()).unwrap_err();
    assert!(
        matches!(err, crate::error::AppError::Validation(_)),
        "got: {err:?}"
    );
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
    // No login → pure-API shape here too.
    assert!(entry.get("requires_openai_auth").is_none());
    assert_eq!(entry["experimental_bearer_token"].as_str(), Some("loopback-token"));
    assert_eq!(doc["model"].as_str(), Some("nestra"));
    assert_eq!(doc["model_provider"].as_str(), Some("nestra-gateway"));
    // The sentinel lands in auth.json — gateway inbound auth only checks
    // the Bearer VALUE, wherever the client reads it from.
    let auth: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("auth.json")).unwrap()).unwrap();
    assert_eq!(auth["OPENAI_API_KEY"].as_str(), Some("loopback-token"));
}

#[test]
fn gateway_set_keeps_official_login_shape_when_tokens_present() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.toml");
    fs::write(&cfg, FIXTURE).unwrap();
    let original = r#"{"tokens":{"access_token":"a"},"auth_mode":"chatgpt"}"#;
    fs::write(dir.join("auth.json"), original).unwrap();
    let alias = crate::config_writer::GatewayAlias::simple(
        "http://127.0.0.1:18777/codex-desktop",
        "nestra",
        "loopback-token",
    );
    Codex.apply_gateway_set(&cfg, &alias).unwrap();
    let doc: DocumentMut = fs::read_to_string(&cfg).unwrap().parse().unwrap();
    assert_eq!(
        doc["model_providers"]["nestra-gateway"]["requires_openai_auth"].as_bool(),
        Some(true)
    );
    assert_eq!(fs::read_to_string(dir.join("auth.json")).unwrap(), original);
}

#[test]
fn restore_also_restores_auth_json() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.toml");
    fs::write(&cfg, FIXTURE).unwrap();
    let original_auth = r#"{"auth_mode":"apikey"}"#;
    fs::write(dir.join("auth.json"), original_auth).unwrap();
    Codex.apply(&cfg, &ctx()).unwrap();
    assert_ne!(
        fs::read_to_string(dir.join("auth.json")).unwrap(),
        original_auth
    );
    Codex.restore(&cfg).unwrap();
    assert_eq!(fs::read_to_string(&cfg).unwrap(), FIXTURE);
    assert_eq!(fs::read_to_string(dir.join("auth.json")).unwrap(), original_auth);
    assert!(!backup_path_for(&dir.join("auth.json")).exists());
}

#[test]
fn restore_removes_nestra_created_auth_json() {
    // Pre-Nestra there was no auth.json at all — restore must delete the
    // one Nestra created, not leave a dead key behind (the app would show
    // a signed-in-with-API-key state that 401s everywhere).
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.toml");
    fs::write(&cfg, FIXTURE).unwrap();
    Codex.apply(&cfg, &ctx()).unwrap();
    assert!(dir.join("auth.json").exists());
    Codex.restore(&cfg).unwrap();
    assert!(!dir.join("auth.json").exists());
    assert!(!backup_path_for(&dir.join("auth.json")).exists());
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
