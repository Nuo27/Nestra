//! Pi config writer — three files under `~/.pi/agent/`:
//! - `models-store.json`: provider catalog + root `models` array — each
//!   `nestra-<id>` provider carries its
//!   `baseUrl`, `api`, and a nested `models` array (no `apiKey` here).
//! - `auth.json`: credentials `{ "nestra-<id>": { "type": "api_key", "key": "..." } }`,
//!   the same file Pi's `/login` writes — Nestra keeps it in sync so the two
//!   agree.
//! - `settings.json`: `defaultProvider` + `defaultModel` (the file Pi reads at
//!   startup — see https://pi.dev/docs/latest/settings). `defaultProvider` is
//!   the `nestra-<id>` catalog key; `defaultModel` is the bare model id.
//!
//! Format reference: https://pi.dev/docs/latest/models

use super::internal;
use crate::config_writer::{
    ensure_backup, atomic_write, restore_from_backup, ConfigAdapter, DetectedProvider,
    ModelSelection, ModelsConfig, ProviderKind, ProviderSet,
};
use crate::error::{AppError, AppResult};
use std::path::{Path, PathBuf};

pub struct Pi;

fn auth_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("auth.json")
}

fn settings_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("settings.json")
}

impl ConfigAdapter for Pi {
    fn accepts(&self) -> &'static [ProviderKind] {
        &[
            ProviderKind::Anthropic,
            ProviderKind::Openai,
            ProviderKind::Custom,
        ]
    }

    fn model_selection(&self) -> ModelSelection {
        ModelSelection::FreeForm
    }

    fn apply_set(&self, config_path: &Path, set: &ProviderSet) -> AppResult<bool> {
        let auth = auth_path(config_path);
        let settings = settings_path(config_path);
        let backup_created = ensure_backup(config_path)?;
        ensure_backup(&auth)?;
        ensure_backup(&settings)?;

        write_models(config_path, set)?;
        write_auth(&auth, set)?;
        write_settings(&settings, set)?;
        Ok(backup_created)
    }

    /// Gateway mode: write one `nestra-gw` provider across the three Pi files
    /// (models.json + auth.json + settings.json), all pointing at the gateway's
    /// loopback URL. Pi's `api: "openai-completions"` makes it post Chat
    /// Completions to `<gateway>/<agent>/v1/chat/completions`, which the
    /// dispatcher routes to the OpenAI handler. The router resolves the real
    /// provider/model per-task.
    fn apply_gateway_set(
        &self,
        config_path: &Path,
        alias: &crate::config_writer::GatewayAlias,
    ) -> AppResult<bool> {
        let auth = auth_path(config_path);
        let settings = settings_path(config_path);
        let backup_created = ensure_backup(config_path)?;
        ensure_backup(&auth)?;
        ensure_backup(&settings)?;
        write_gateway_models(config_path, alias)?;
        write_gateway_auth(&auth, alias)?;
        write_gateway_settings(&settings, alias)?;
        Ok(backup_created)
    }

    fn restore(&self, config_path: &Path) -> AppResult<()> {
        let auth = auth_path(config_path);
        let settings = settings_path(config_path);
        // All three files restore or NONE is claimed restored — the old
        // `let _ =` left the trio inconsistent (e.g. main restored but auth
        // still carrying the Nestra key) while returning Ok.
        restore_from_backup(config_path)?;
        restore_from_backup(&auth)?;
        restore_from_backup(&settings)?;
        Ok(())
    }

    fn inspect(&self, config_path: &Path) -> AppResult<Vec<DetectedProvider>> {
        let root = read_json_object(config_path)?;
        let providers = match root.get("providers").and_then(|v| v.as_object()) {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };
        Ok(providers
            .keys()
            .map(|k| DetectedProvider {
                key: k.clone(),
                display_name: k.clone(),
                managed: k.starts_with("nestra-"),
            })
            .collect())
    }

    fn remove(&self, config_path: &Path, key: &str) -> AppResult<()> {
        remove_from_models(config_path, key)?;
        remove_from_auth(&auth_path(config_path), key)?;
        // The removed provider may have been the default — clear the stale
        // pointers so Pi doesn't boot pointing at a deleted provider.
        let settings = settings_path(config_path);
        if settings.exists() {
            let mut root = read_json_object(&settings)?;
            let removed_default = root
                .get("defaultProvider")
                .and_then(|v| v.as_str())
                .map(|p| p == key || p == format!("nestra-{key}"))
                .unwrap_or(false);
            if removed_default {
                root.remove("defaultProvider");
                root.remove("defaultModel");
                let bytes = serde_json::to_vec_pretty(&root)?;
                atomic_write(&settings, &bytes)?;
            }
        }
        Ok(())
    }

    fn extra_config_paths(&self, config_path: &Path) -> Vec<PathBuf> {
        vec![auth_path(config_path), settings_path(config_path)]
    }
}

fn api_for(kind: ProviderKind) -> String {
    match kind {
        ProviderKind::Anthropic => "anthropic-messages".into(),
        _ => "openai-completions".into(),
    }
}

/// Pi's baseUrl must be the API ROOT — Pi appends the protocol path itself
/// (`/v1/messages` for anthropic, `/chat/completions` for openai). Shared
/// normalization strips any full path the user entered in the endpoint row.
fn pi_base_url(ctx: &crate::config_writer::SwitchContext) -> String {
    crate::protocol_url::normalize_protocol_base(&ctx.base_url, ctx.provider_kind)
}

/// Write provider blocks into models.json — Pi's config entry point (the
/// file is reloaded each time /model opens). Pi's native layout nests the
/// model list INSIDE each provider block (`providers.<id>.models`), with
/// plain model ids; `models-store.json` is Pi's own runtime cache and must
/// NOT be written (Pi overwrites it). Nestra-owned entries are replaced,
/// user entries survive. No `apiKey` — auth.json supplies credentials,
/// matching Pi's `/login` convention. Default provider/model selection is
/// written to settings.json, not here.
fn write_models(config_path: &Path, set: &ProviderSet) -> AppResult<()> {
    let mut root = read_json_object(config_path)?;

    let providers_root = root
        .entry("providers".to_string())
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let providers = providers_root
        .as_object_mut()
        .ok_or_else(|| AppError::Internal("models.json `providers` is not an object".into()))?;

    // Replace Nestra-owned entries; user keys survive untouched.
    providers.retain(|k, _| !k.starts_with("nestra-"));

    for ctx in &set.entries {
        let pid = format!("nestra-{}", ctx.provider_id);
        let mut provider = serde_json::Map::new();
        provider.insert("baseUrl".into(), serde_json::Value::String(pi_base_url(ctx)));
        provider.insert(
            "api".into(),
            serde_json::Value::String(api_for(ctx.provider_kind)),
        );
        // Models nest inside the provider block (Pi's native layout).
        provider.insert("models".into(), models_array(&ctx.models));
        providers.insert(pid, serde_json::Value::Object(provider));
    }

    let bytes = serde_json::to_vec_pretty(&root)?;
    atomic_write(config_path, &bytes)
}
fn write_settings(settings_path: &Path, set: &ProviderSet) -> AppResult<()> {
    let mut root = read_json_object(settings_path)?;
    root.insert(
        "defaultProvider".into(),
        serde_json::Value::String(format!("nestra-{}", set.default_provider_id)),
    );
    root.insert(
        "defaultModel".into(),
        serde_json::Value::String(set.default_model.clone()),
    );
    let bytes = serde_json::to_vec_pretty(&root)?;
    if let Some(parent) = settings_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    atomic_write(settings_path, &bytes)
}

/// Write all credentials into auth.json under `nestra-<id>`.
fn write_auth(auth_path: &Path, set: &ProviderSet) -> AppResult<()> {
    let mut root = read_auth(auth_path)?;
    root.retain(|k, _| !k.starts_with("nestra-"));
    for ctx in &set.entries {
        let pid = format!("nestra-{}", ctx.provider_id);
        let mut cred = serde_json::Map::new();
        cred.insert("type".into(), serde_json::Value::String("api_key".into()));
        cred.insert("key".into(), serde_json::Value::String(ctx.api_key.clone()));
        root.insert(pid, serde_json::Value::Object(cred));
    }

    let bytes = serde_json::to_vec_pretty(&root)?;
    if let Some(parent) = auth_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    atomic_write(auth_path, &bytes)
}

fn remove_from_models(config_path: &Path, key: &str) -> AppResult<()> {
    let mut root = read_json_object(config_path)?;
    if let Some(providers) = root.get_mut("providers").and_then(|v| v.as_object_mut()) {
        providers.remove(key);
    }
    let bytes = serde_json::to_vec_pretty(&root)?;
    atomic_write(config_path, &bytes)
}


fn remove_from_auth(auth_path: &Path, key: &str) -> AppResult<()> {
    if !auth_path.exists() {
        return Ok(());
    }
    let mut root = read_auth(auth_path)?;
    root.remove(key);
    let bytes = serde_json::to_vec_pretty(&root)?;
    atomic_write(auth_path, &bytes)
}

/// Build the `models` array nested inside one provider entry (Pi's native
/// layout). The id is the bare model identifier passed to the upstream API.
fn models_array(m: &ModelsConfig) -> serde_json::Value {
    let ids: Vec<String> = match m {
        ModelsConfig::Anthropic { default, haiku, sonnet, opus, .. } => {
            // Default first, then non-empty tiers that differ from it —
            // tiers that fall back to default would otherwise duplicate rows.
            let mut v = vec![default.clone()];
            for tier in [haiku, sonnet, opus] {
                if !tier.is_empty() && tier != default {
                    v.push(tier.clone());
                }
            }
            v
        }
        ModelsConfig::Openai { default, available } => {
            if available.is_empty() {
                vec![default.clone()]
            } else {
                available.clone()
            }
        }
    };
    let arr: Vec<serde_json::Value> = ids
        .into_iter()
        .map(|id| serde_json::json!({ "id": id, "name": id }))
        .collect();
    serde_json::Value::Array(arr)
}
// ---- gateway-mode writers (one nestra-gw provider across the 3 files) ----

/// Write a single `nestra-gw` provider into models-store.json pointing at
/// the gateway, using `api: "openai-completions"` so Pi posts Chat
/// Completions. The gateway alias is also added to the ROOT `models` array
/// so Pi's picker lists it.
fn write_gateway_models(config_path: &Path, alias: &crate::config_writer::GatewayAlias) -> AppResult<()> {
    let mut root = read_json_object(config_path)?;
    let providers_root = root
        .entry("providers".to_string())
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let providers = providers_root
        .as_object_mut()
        .ok_or_else(|| AppError::Internal("models.json `providers` is not an object".into()))?;
    providers.retain(|k, _| !k.starts_with("nestra-"));
    let mut provider = serde_json::Map::new();
    provider.insert(
        "baseUrl".into(),
        serde_json::Value::String(alias.gateway_base_url.clone()),
    );
    // openai-completions so Pi speaks Chat Completions -> OpenAI handler.
    provider.insert("api".into(), serde_json::Value::String("openai-completions".into()));
    // One placeholder model nested under the alias; the router resolves the
    // real model per-task.
    provider.insert(
        "models".into(),
        serde_json::Value::Array(vec![serde_json::json!({
            "id": alias.model_alias,
            "name": alias.model_alias,
        })]),
    );
    providers.insert("nestra-gw".into(), serde_json::Value::Object(provider));
    let bytes = serde_json::to_vec_pretty(&root)?;
    atomic_write(config_path, &bytes)
}


fn write_gateway_auth(auth_path: &Path, alias: &crate::config_writer::GatewayAlias) -> AppResult<()> {
    let mut root = read_json_object(auth_path)?;
    let auth = root
        .entry("nestra-gw".to_string())
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let auth_obj = auth
        .as_object_mut()
        .ok_or_else(|| AppError::Internal("auth.json `nestra-gw` is not an object".into()))?;
    auth_obj.insert("type".into(), serde_json::Value::String("api_key".into()));
    auth_obj.insert("key".into(), serde_json::Value::String(alias.sentinel_key.clone()));
    let bytes = serde_json::to_vec_pretty(&root)?;
    if let Some(parent) = auth_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    atomic_write(auth_path, &bytes)
}

/// Write `defaultProvider=nestra-gw` + `defaultModel=<alias>` into settings.json.
fn write_gateway_settings(
    settings_path: &Path,
    alias: &crate::config_writer::GatewayAlias,
) -> AppResult<()> {
    let mut root = read_json_object(settings_path)?;
    root.insert(
        "defaultProvider".into(),
        serde_json::Value::String("nestra-gw".into()),
    );
    root.insert(
        "defaultModel".into(),
        serde_json::Value::String(alias.model_alias.clone()),
    );
    let bytes = serde_json::to_vec_pretty(&root)?;
    if let Some(parent) = settings_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    atomic_write(settings_path, &bytes)
}

fn read_json_object(path: &Path) -> AppResult<serde_json::Map<String, serde_json::Value>> {
    if !path.exists() {
        return Ok(Default::default());
    }
    let text = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(internal)?;
    match value {
        serde_json::Value::Object(m) => Ok(m),
        _ => Err(AppError::Internal("config root is not an object".into())),
    }
}

fn read_auth(path: &Path) -> AppResult<serde_json::Map<String, serde_json::Value>> {
    if !path.exists() {
        return Ok(Default::default());
    }
    let text = std::fs::read_to_string(path).map_err(AppError::Io)?;
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Internal(format!("auth.json is not valid JSON: {e}")))?;
    match v {
        serde_json::Value::Object(m) => Ok(m),
        _ => Err(AppError::Internal("auth.json root is not an object".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use crate::config_writer::{backup_path_for, SwitchContext};
    use std::fs;

    const FIXTURE: &str = include_str!("../fixtures/pi/models-store.json");

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
}
