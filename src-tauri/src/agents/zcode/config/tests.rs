use super::*;
use crate::config_writer::{backup_path_for, SwitchContext};
use crate::model_abilities::{ModelAbilities, ModelLimit};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

const FIXTURE: &str = include_str!("../../../fixtures/zcode/config.json");

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
        provider_kind: ProviderKind::Anthropic,
        display_name: "Z.ai".into(),
        base_url: "https://api.z.ai/api/anthropic".into(),
        api_key: "sk-test".into(),
        models: ModelsConfig::Openai {
            default: "GLM-5.3".into(),
            available: vec!["GLM-5.3".into(), "GLM-5.2".into()],
        },
        advanced_env: Default::default(),
        model_abilities: Default::default(),
    }
}

fn apply_to_fixture(c: &SwitchContext) -> serde_json::Value {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.json");
    fs::write(&cfg, FIXTURE).unwrap();
    ZCode.apply(&cfg, c).unwrap();
    serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap()
}

#[test]
fn apply_writes_provider_entry_and_preserves_rest() {
    let written = apply_to_fixture(&ctx());
    let entry = &written["provider"]["nestra-zai"];
    assert_eq!(entry["kind"], "anthropic");
    assert_eq!(entry["options"]["baseURL"], "https://api.z.ai/api/anthropic");
    assert_eq!(entry["options"]["apiKey"], "sk-test");
    assert_eq!(entry["enabled"], true);
    assert_eq!(entry["source"], "custom");
    // fallback limits when abilities are unknown
    assert_eq!(entry["models"]["GLM-5.3"]["limit"]["context"], 200_000);
    // both available models listed
    assert!(entry["models"].as_object().unwrap().contains_key("GLM-5.2"));
    // user-configured provider + unrelated keys preserved
    assert!(written["provider"]["builtin:zai-coding-plan"].is_object());
    assert_eq!(written["unrelated"]["keep"], true);
}

#[test]
fn apply_uses_abilities_limits_when_known() {
    let mut c = ctx();
    let mut m = HashMap::new();
    m.insert(
        "GLM-5.3".into(),
        ModelAbilities {
            reasoning: None,
            tool_call: None,
            attachment: None,
            temperature: None,
            api: None,
            cost: None,
            limit: Some(ModelLimit { context: 1_000_000, output: 128_000, input: None }),
            modalities: None,
        },
    );
    c.model_abilities = m;
    let written = apply_to_fixture(&c);
    assert_eq!(written["provider"]["nestra-zai"]["models"]["GLM-5.3"]["limit"]["context"], 1_000_000);
}

#[test]
fn apply_normalizes_full_messages_path() {
    let mut c = ctx();
    c.base_url = "https://api.z.ai/api/anthropic/v1/messages".into();
    let written = apply_to_fixture(&c);
    assert_eq!(written["provider"]["nestra-zai"]["options"]["baseURL"], "https://api.z.ai/api/anthropic");
}

#[test]
fn apply_openai_compatible_endpoint_writes_matching_kind() {
    // An OpenAI-protocol binding (DeepSeek, OpenRouter, z.ai paas/v4, …)
    // writes kind "openai-compatible"; the base KEEPS its `/v1` (ZCode
    // appends /chat/completions itself) and a full path is stripped.
    let mut c = ctx();
    c.provider_kind = ProviderKind::Openai;
    c.base_url = "https://api.deepseek.com/v1/chat/completions".into();
    c.models = ModelsConfig::Openai {
        default: "deepseek-chat".into(),
        available: vec!["deepseek-chat".into()],
    };
    let written = apply_to_fixture(&c);
    let entry = &written["provider"]["nestra-zai"];
    assert_eq!(entry["kind"], "openai-compatible");
    assert_eq!(entry["options"]["baseURL"], "https://api.deepseek.com/v1");
}

#[test]
fn switch_replaces_previous_managed_key() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.json");
    fs::write(&cfg, FIXTURE).unwrap();
    ZCode.apply(&cfg, &ctx()).unwrap();
    let mut c = ctx();
    c.provider_id = "other".into();
    ZCode.apply(&cfg, &c).unwrap();

    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    let provider = written["provider"].as_object().unwrap();
    assert!(provider.contains_key("nestra-other"));
    assert!(!provider.contains_key("nestra-zai"), "stale managed key must be replaced");
}

#[test]
fn apply_set_rejects_multiple_entries() {
    let set = ProviderSet {
        entries: vec![ctx(), ctx()],
        default_provider_id: "zai".into(),
        default_model: "GLM-5.3".into(),
    };
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.json");
    fs::write(&cfg, FIXTURE).unwrap();
    assert!(ZCode.apply_set(&cfg, &set).is_err());
}

#[test]
fn gateway_write_points_at_alias() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.json");
    fs::write(&cfg, FIXTURE).unwrap();
    ZCode
        .apply_gateway_set(
            &cfg,
            &GatewayAlias::simple(
                "http://127.0.0.1:18777/zcode-desktop",
                "nestra",
                "gw-token",
            ),
        )
        .unwrap();

    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    let entry = &written["provider"]["nestra-gateway"];
    assert_eq!(entry["options"]["baseURL"], "http://127.0.0.1:18777/zcode-desktop");
    assert_eq!(entry["options"]["apiKey"], "gw-token");
    // single alias model
    let models = entry["models"].as_object().unwrap();
    assert_eq!(models.len(), 1);
    assert!(models.contains_key("nestra"));
    // No abilities on the alias → conservative fallback limits.
    assert_eq!(models["nestra"]["limit"]["context"], 200_000);
    assert_eq!(models["nestra"]["limit"]["output"], 128_000);
}

#[test]
fn gateway_write_carries_alias_abilities_as_limits() {
    // The alias slot carries the steady-state model's abilities → ZCode
    // plans against the REAL window instead of the 200k fallback.
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.json");
    fs::write(&cfg, FIXTURE).unwrap();
    let alias = GatewayAlias {
        gateway_base_url: "http://127.0.0.1:18777/zcode-desktop".into(),
        model_alias: crate::config_writer::AliasModel {
            id: "nestra".into(),
            abilities: Some(crate::model_abilities::ModelAbilities {
                reasoning: None,
                tool_call: None,
                attachment: None,
                temperature: None,
                limit: Some(crate::model_abilities::ModelLimit {
                    context: 1_000_000,
                    output: 64_000,
                    input: None,
                }),
                modalities: None,
                api: None,
                cost: None,
            }),
        },
        tier_aliases: None,
        sentinel_key: "gw-token".into(),
    };
    ZCode.apply_gateway_set(&cfg, &alias).unwrap();

    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    let limit = &written["provider"]["nestra-gateway"]["models"]["nestra"]["limit"];
    assert_eq!(limit["context"], 1_000_000);
    assert_eq!(limit["output"], 64_000);
}

#[test]
fn restore_is_byte_exact() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.json");
    fs::write(&cfg, FIXTURE).unwrap();
    let original = fs::read(&cfg).unwrap();

    ZCode.apply(&cfg, &ctx()).unwrap();
    ZCode.restore(&cfg).unwrap();

    assert_eq!(fs::read(&cfg).unwrap(), original);
    assert!(!backup_path_for(&cfg).exists());
}

#[test]
fn inspect_lists_unmanaged_and_remove_deletes() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("config.json");
    fs::write(&cfg, FIXTURE).unwrap();
    ZCode.apply(&cfg, &ctx()).unwrap();

    let detected = ZCode.inspect(&cfg).unwrap();
    let keys: Vec<&str> = detected.iter().map(|d| d.key.as_str()).collect();
    assert_eq!(keys, ["builtin:zai-coding-plan"]);

    ZCode.remove(&cfg, "builtin:zai-coding-plan").unwrap();
    let after = ZCode.inspect(&cfg).unwrap();
    assert!(after.is_empty());
    // managed entry untouched by remove
    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    assert!(written["provider"]["nestra-zai"].is_object());
}