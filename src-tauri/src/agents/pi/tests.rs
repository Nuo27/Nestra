use super::*;
use std::path::PathBuf;
use crate::config_writer::{backup_path_for, SwitchContext};
use std::fs;

const FIXTURE: &str = include_str!("../../fixtures/pi/models-store.json");

fn tmp() -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("nestra-pi-test-")
        .tempdir()
        .expect("tempdir");
    (dir.path().to_path_buf(), dir)
}

fn ctx_openai() -> SwitchContext {
    SwitchContext {
        provider_id: "openai".into(),
        provider_kind: ProviderKind::Openai,
        display_name: "OpenAI".into(),
        base_url: "https://api.openai.com/v1".into(),
        api_key: "sk-openai".into(),
        models: ModelsConfig::Openai {
            default: "gpt-4o".into(),
            available: vec!["gpt-4o".into()],
        },
        advanced_env: Default::default(),
        model_abilities: Default::default(),
    }
}

fn ctx_anthropic() -> SwitchContext {
    SwitchContext {
        provider_id: "minimax-cn".into(),
        provider_kind: ProviderKind::Anthropic,
        display_name: "MiniMax CN".into(),
        base_url: "https://api.minimax.com/v1".into(),
        api_key: "sk-mm".into(),
        models: ModelsConfig::Anthropic {
            default: "MiniMax-M3".into(),
            haiku: "MiniMax-M3-haiku".into(),
            sonnet: "MiniMax-M3".into(),
            opus: "MiniMax-M3".into(),
        },
        advanced_env: Default::default(),
        model_abilities: Default::default(),
    }
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn read_auth_json(dir: &Path) -> serde_json::Map<String, serde_json::Value> {
    read_json(&dir.join("auth.json")).as_object().unwrap().clone()
}

fn read_settings_json(dir: &Path) -> serde_json::Map<String, serde_json::Value> {
    read_json(&dir.join("settings.json")).as_object().unwrap().clone()
}

// ---- catalog + auth (unchanged behavior) ----

#[test]
fn pi_base_url_strips_anthropic_path() {
    use crate::config_writer::SwitchContext;
    let ctx = SwitchContext {
        provider_id: "minimax".into(),
        provider_kind: crate::config_writer::ProviderKind::Anthropic,
        display_name: "minimax".into(),
        base_url: "https://api.minimaxi.com/anthropic/v1/messages".into(),
        api_key: "k".into(),
        models: ModelsConfig::Openai { default: "m".into(), available: vec![] },
        advanced_env: Default::default(),
        model_abilities: Default::default(),
    };
    assert_eq!(
        pi_base_url(&ctx),
        "https://api.minimaxi.com/anthropic",
        "Pi appends /v1/messages itself"
    );
    // OpenAI-style base: Pi appends /v1/chat/completions itself, and the
    // shared normalizer now strips a trailing /v1 for Anthropic-append
    // clients — the same rule applies here.
    let ctx2 = SwitchContext { base_url: "https://api.minimaxi.com/v1".into(), ..ctx };
    assert_eq!(pi_base_url(&ctx2), "https://api.minimaxi.com");
}

#[test]
fn models_array_dedupes_anthropic_tiers() {
    // default=M3, haiku=M2.7, sonnet/opus fall back to M3 → only two rows.
    let cfg = ModelsConfig::Anthropic {
        default: "MiniMax-M3".into(),
        haiku: "MiniMax-M2.7".into(),
        sonnet: "MiniMax-M3".into(),
        opus: "MiniMax-M3".into(),
    };
    let arr = models_array(&cfg);
    let ids: Vec<&str> = arr
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["MiniMax-M3", "MiniMax-M2.7"]);
}

#[test]
fn apply_writes_provider_and_root_catalog() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("models.json");
    fs::write(&cfg, FIXTURE).unwrap();

    Pi.apply(&cfg, &ctx_openai()).unwrap();

    let store = read_json(&cfg);
    let p = &store["providers"]["nestra-openai"];
    assert_eq!(p["baseUrl"], "https://api.openai.com/v1");
    assert_eq!(p["api"], "openai-completions");
    assert!(p.get("apiKey").is_none(), "apiKey must not be in models-store.json");
    // Pi's native layout: providers hold only {api, baseUrl}; the model
    // catalog lives in the ROOT `models` array with compound ids.
    // Models nest inside the provider block (Pi's native layout).
    let models = p["models"].as_array().unwrap();
    assert!(models.iter().any(|m| m["id"] == "gpt-4o"));
    assert_eq!(store["providers"]["existing"]["baseUrl"], "https://api.existing.com/v1");
    assert!(
        store["providers"]["existing"]["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "other-model"),
        "user provider models survive"
    );

    let auth = read_auth_json(&dir);
    assert_eq!(auth["nestra-openai"]["type"], "api_key");
    assert_eq!(auth["nestra-openai"]["key"], "sk-openai");
}

#[test]
fn re_switch_replaces_only_own_models() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("models.json");
    fs::write(&cfg, FIXTURE).unwrap();
    Pi.apply(&cfg, &ctx_openai()).unwrap();

    let mut ctx = ctx_openai();
    ctx.models = ModelsConfig::Openai {
        default: "gpt-4o-mini".into(),
        available: vec!["gpt-4o-mini".into()],
    };
    Pi.apply(&cfg, &ctx).unwrap();

    let store = read_json(&cfg);
    let models = store["providers"]["nestra-openai"]["models"].as_array().unwrap();
    assert!(!models.iter().any(|m| m["id"] == "gpt-4o"));
    assert!(models.iter().any(|m| m["id"] == "gpt-4o-mini"));
    // User provider + its models untouched.
    assert!(store["providers"]["existing"].is_object());
    assert!(
        store["providers"]["existing"]["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "other-model")
    );
}

#[test]
fn remove_cleans_both_files() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("models.json");
    fs::write(&cfg, FIXTURE).unwrap();
    Pi.apply(&cfg, &ctx_openai()).unwrap();

    let detected = Pi.inspect(&cfg).unwrap();
    assert!(!detected.iter().find(|d| d.key == "existing").unwrap().managed);
    assert!(detected.iter().find(|d| d.key == "nestra-openai").unwrap().managed);

    Pi.remove(&cfg, "existing").unwrap();
    let after = read_json(&cfg);
    assert!(after["providers"].get("existing").is_none());
    assert!(after["providers"]["nestra-openai"].is_object());
}

// ---- defaults live in settings.json, NOT models.json ----

#[test]
fn apply_writes_defaults_to_settings_not_models_file() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("models.json");
    fs::write(&cfg, FIXTURE).unwrap();
    Pi.apply(&cfg, &ctx_anthropic()).unwrap();

    let settings = read_settings_json(&dir);
    // defaultProvider is the nestra-<id> catalog key so Pi can resolve it.
    assert_eq!(settings["defaultProvider"], "nestra-minimax-cn");
    assert_eq!(settings["defaultModel"], "MiniMax-M3");

    let store = read_json(&cfg);
    assert!(
        store.get("defaultProvider").is_none(),
        "defaultProvider must not be in models.json"
    );
    assert!(
        store.get("defaultModel").is_none(),
        "defaultModel must not be in models.json"
    );
}

#[test]
fn apply_preserves_existing_settings_fields() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("models.json");
    let settings = dir.join("settings.json");
    fs::write(&cfg, FIXTURE).unwrap();
    fs::write(
        &settings,
        r#"{"theme":"dark","defaultThinkingLevel":"medium","packages":["npm:pi-lmstudio"]}"#,
    )
    .unwrap();

    Pi.apply(&cfg, &ctx_anthropic()).unwrap();

    let after = read_settings_json(&dir);
    assert_eq!(after["theme"], "dark");
    assert_eq!(after["defaultThinkingLevel"], "medium");
    assert_eq!(after["packages"][0], "npm:pi-lmstudio");
    assert_eq!(after["defaultProvider"], "nestra-minimax-cn");
    assert_eq!(after["defaultModel"], "MiniMax-M3");
}

#[test]
fn apply_creates_settings_when_absent() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("models.json");
    fs::write(&cfg, FIXTURE).unwrap();
    assert!(!dir.join("settings.json").exists());

    Pi.apply(&cfg, &ctx_anthropic()).unwrap();

    let after = read_settings_json(&dir);
    assert_eq!(after["defaultProvider"], "nestra-minimax-cn");
    assert_eq!(after["defaultModel"], "MiniMax-M3");
}

// ---- multi-provider ----

#[test]
fn apply_set_writes_all_providers_and_default() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("models.json");
    fs::write(&cfg, FIXTURE).unwrap();

    let set = ProviderSet {
        entries: vec![ctx_anthropic(), ctx_openai()],
        default_provider_id: "minimax-cn".into(),
        default_model: "MiniMax-M3".into(),
    };
    Pi.apply_set(&cfg, &set).unwrap();

    let store = read_json(&cfg);
    assert!(store["providers"]["nestra-minimax-cn"].is_object());
    assert!(store["providers"]["nestra-openai"].is_object());

    let settings = read_settings_json(&dir);
    assert_eq!(settings["defaultProvider"], "nestra-minimax-cn");
    assert_eq!(settings["defaultModel"], "MiniMax-M3");

    let auth = read_auth_json(&dir);
    assert!(auth["nestra-minimax-cn"].is_object());
    assert!(auth["nestra-openai"].is_object());
}

#[test]
fn apply_set_replaces_previous_owned_providers() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("models.json");
    fs::write(&cfg, FIXTURE).unwrap();

    Pi.apply_set(
        &cfg,
        &ProviderSet {
            entries: vec![ctx_anthropic()],
            default_provider_id: "minimax-cn".into(),
            default_model: "MiniMax-M3".into(),
        },
    )
    .unwrap();

    Pi.apply_set(
        &cfg,
        &ProviderSet {
            entries: vec![ctx_openai()],
            default_provider_id: "openai".into(),
            default_model: "gpt-4o".into(),
        },
    )
    .unwrap();

    let store = read_json(&cfg);
    assert!(store["providers"].get("nestra-minimax-cn").is_none());
    assert!(store["providers"]["nestra-openai"].is_object());
    assert!(store["providers"]["existing"].is_object(), "user entry preserved");

    let settings = read_settings_json(&dir);
    assert_eq!(settings["defaultProvider"], "nestra-openai");
    assert_eq!(settings["defaultModel"], "gpt-4o");

    let auth = read_auth_json(&dir);
    assert!(auth.get("nestra-minimax-cn").is_none());
    assert!(auth["nestra-openai"].is_object());
}

// ---- restore ----

#[test]
fn restore_reverts_all_three_files() {
    let (dir, _dir_g) = tmp();
    let cfg = dir.join("models.json");
    let auth = dir.join("auth.json");
    let settings = dir.join("settings.json");
    fs::write(&cfg, FIXTURE).unwrap();
    fs::write(&auth, r#"{"existing":{"type":"api_key","key":"old"}}"#).unwrap();
    fs::write(&settings, r#"{"theme":"dark","defaultProvider":"existing","defaultModel":"old"}"#).unwrap();

    let orig_cfg = fs::read(&cfg).unwrap();
    let orig_auth = fs::read(&auth).unwrap();
    let orig_settings = fs::read(&settings).unwrap();

    Pi.apply(&cfg, &ctx_openai()).unwrap();
    Pi.restore(&cfg).unwrap();

    assert_eq!(fs::read(&cfg).unwrap(), orig_cfg);
    assert_eq!(fs::read(&auth).unwrap(), orig_auth);
    assert_eq!(fs::read(&settings).unwrap(), orig_settings);
    assert!(!backup_path_for(&cfg).exists());
}

// ---- extra_config_paths ----

#[test]
fn extra_config_paths_returns_auth_and_settings() {
    let cfg = Path::new("/home/u/.pi/agent/models.json");
    let extra = Pi.extra_config_paths(cfg);
    assert_eq!(
        extra,
        vec![
            PathBuf::from("/home/u/.pi/agent/auth.json"),
            PathBuf::from("/home/u/.pi/agent/settings.json"),
        ]
    );
}