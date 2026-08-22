//! OpenCode config writer — `~/.config/opencode/opencode.json` (JSONC).
//! Owns `nestra-*` keys under `provider`, plus the top-level `"model"`
//! pointer OpenCode uses to pick the default at startup
//! (format: `provider_id/model_id`, see https://opencode.ai/docs/models/).
//!
//! Comments are read via `jsonc-parser` and the file is rewritten as pretty
//! JSON (comments are not preserved on write). The pre-Nestra backup
//! guarantees a one-click revert. Full AST-preserving JSONC editing is a
//! future enhancement.

use crate::agents::internal;
use crate::config_writer::{
    ensure_backup, atomic_write, restore_from_backup, ConfigAdapter, DetectedProvider,
    ModelSelection, ModelsConfig, ProviderKind, ProviderSet, SwitchContext,
};
use crate::error::{AppError, AppResult};
use std::path::Path;

pub struct OpenCode;

/// Registry constructor for the opencode writer — see [super::SPEC].
pub fn new() -> Box<dyn crate::config_writer::ConfigAdapter> {
    Box::new(OpenCode)
}

impl ConfigAdapter for OpenCode {
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
        let backup_created = ensure_backup(config_path)?;
        let mut root = read_jsonc_object(config_path)?;

        let provider_root = root
            .entry("provider".to_string())
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        let providers = provider_root
            .as_object_mut()
            .ok_or_else(|| AppError::Internal("opencode.json `provider` is not an object".into()))?;

        // Replace Nestra-owned entries wholesale; user keys survive untouched.
        providers.retain(|k, _| !k.starts_with("nestra-"));

        for ctx in &set.entries {
            let key = format!("nestra-{}", ctx.provider_id);
            let mut block = serde_json::Map::new();
            block.insert("npm".into(), serde_json::Value::String(npm_for(ctx.provider_kind)));
            block.insert(
                "name".into(),
                serde_json::Value::String(format!("{} (via Nestra)", ctx.display_name)),
            );
            let mut options = serde_json::Map::new();
            options.insert("baseURL".into(), serde_json::Value::String(sdk_base_url(&ctx)));
            options.insert("apiKey".into(), serde_json::Value::String(ctx.api_key.clone()));
            block.insert("options".into(), serde_json::Value::Object(options));
            block.insert("models".into(), models_map(&ctx.models, &ctx.model_abilities));

            providers.insert(key, serde_json::Value::Object(block));
        }

        // Top-level `model` follows the docs: `provider_id/model_id`, where
        // `provider_id` is the key Nestra owns in the `provider` block.
        let model_value = format!("nestra-{}/{}", set.default_provider_id, set.default_model);
        root.insert("model".into(), serde_json::Value::String(model_value));

        let bytes = serde_json::to_vec_pretty(&root)?;
        atomic_write(config_path, &bytes)?;
        Ok(backup_created)
    }

    /// Gateway mode: write one `nestra-gw` provider block pointing at the
    /// gateway's loopback URL + a `model: nestra-gw/<alias>` pointer. The
    /// `@ai-sdk/openai-compatible` npm package is used so OpenCode posts
    /// OpenAI-shape Chat Completions to `<gateway>/<agent>/v1/chat/completions`,
    /// which the dispatcher routes to the OpenAI handler. The router then
    /// resolves the real provider/model per-task.
    fn apply_gateway_set(
        &self,
        config_path: &Path,
        alias: &crate::config_writer::GatewayAlias,
    ) -> AppResult<bool> {
        let backup_created = ensure_backup(config_path)?;
        let mut root = read_jsonc_object(config_path)?;
        let provider_root = root
            .entry("provider".to_string())
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        let providers = provider_root
            .as_object_mut()
            .ok_or_else(|| AppError::Internal("opencode.json `provider` is not an object".into()))?;
        // Replace any prior nestra-owned gateway block; user keys survive.
        providers.retain(|k, _| !k.starts_with("nestra-"));
        let mut block = serde_json::Map::new();
        // openai-compatible so OpenCode speaks Chat Completions (the gateway's
        // OpenAI handler), regardless of the real upstream's native protocol.
        block.insert(
            "npm".into(),
            serde_json::Value::String("@ai-sdk/openai-compatible".into()),
        );
        block.insert(
            "name".into(),
            serde_json::Value::String("Nestra Gateway".into()),
        );
        let mut options = serde_json::Map::new();
        options.insert(
            "baseURL".into(),
            // OpenAI SDK convention: the base URL ends with `/v1` and the SDK
            // appends `/chat/completions` (+ probes `GET /models`). The
            // gateway accepts both forms, but writing `/v1` makes the config
            // match the documented `<gateway>/<agent>/v1/…` contract.
            serde_json::Value::String(format!(
                "{}/v1",
                alias.gateway_base_url.trim_end_matches('/')
            )),
        );
        options.insert(
            "apiKey".into(),
            serde_json::Value::String(alias.sentinel_key.clone()),
        );
        block.insert("options".into(), serde_json::Value::Object(options));
        // One model entry under the alias name; the router resolves the real
        // model per-task, so the catalog OpenCode sees is a single entry.
        // Custom providers get NO models.dev data — OpenCode renders a model
        // from this config entry alone, so limits/reasoning MUST be declared
        // here or the UI shows context 0 / "reasoning: no allow". The entry
        // carries the steady-state model's REAL abilities when the alias
        // resolved one; otherwise neutral placeholders (200k window, tools +
        // reasoning on).
        let mut entry = serde_json::Map::new();
        entry.insert(
            "name".into(),
            serde_json::Value::String(alias.model_alias.id.clone()),
        );
        match &alias.model_alias.abilities {
            Some(a) => {
                for (k, v) in crate::model_abilities::to_model_entry_fields(a) {
                    entry.insert(k, v);
                }
            }
            None => {
                entry.insert(
                    "limit".into(),
                    serde_json::json!({ "context": 200000, "output": 8192 }),
                );
                entry.insert("reasoning".into(), serde_json::Value::Bool(true));
            }
        }
        let mut models_map = serde_json::Map::new();
        models_map.insert(
            alias.model_alias.id.clone(),
            serde_json::Value::Object(entry),
        );
        block.insert("models".into(), serde_json::Value::Object(models_map));
        providers.insert("nestra-gw".into(), serde_json::Value::Object(block));
        // Top-level model pointer: `nestra-gw/<alias>`.
        root.insert(
            "model".into(),
            serde_json::Value::String(format!("nestra-gw/{}", alias.model_alias.id)),
        );
        let bytes = serde_json::to_vec_pretty(&root)?;
        crate::config_writer::atomic_write(config_path, &bytes)?;
        Ok(backup_created)
    }

    fn restore(&self, config_path: &Path) -> AppResult<()> {
        restore_from_backup(config_path)
    }

    fn inspect(&self, config_path: &Path) -> AppResult<Vec<DetectedProvider>> {
        let root = read_jsonc_object(config_path)?;
        let providers = match root.get("provider").and_then(|v| v.as_object()) {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };
        Ok(providers
            .iter()
            .map(|(k, v)| {
                let name = v
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or(k)
                    .to_string();
                DetectedProvider {
                    key: k.clone(),
                    display_name: name,
                    managed: k.starts_with("nestra-"),
                }
            })
            .collect())
    }

    fn remove(&self, config_path: &Path, key: &str) -> AppResult<()> {
        let mut root = read_jsonc_object(config_path)?;
        if let Some(providers) = root.get_mut("provider").and_then(|v| v.as_object_mut()) {
            providers.remove(key);
        }
        let bytes = serde_json::to_vec_pretty(&root)?;
        atomic_write(config_path, &bytes)?;
        Ok(())
    }
}

fn npm_for(kind: ProviderKind) -> String {
    match kind {
        ProviderKind::Anthropic => "@ai-sdk/anthropic".into(),
        _ => "@ai-sdk/openai-compatible".into(),
    }
}

/// opencode's `@ai-sdk/anthropic` posts to `${baseURL}/messages` (its own
/// default base is `https://api.anthropic.com/v1`), so an Anthropic-protocol
/// base must end at the version root. Shared normalization strips any full
/// path the user entered (`…/v1/messages`), then `/v1` is appended.
/// OpenAI-compatible providers get the normalized base (their SDK appends
/// `/chat/completions`).
fn sdk_base_url(ctx: &SwitchContext) -> String {
    let norm = crate::protocol_url::normalize_protocol_base(&ctx.base_url, ctx.provider_kind);
    if ctx.provider_kind != ProviderKind::Anthropic {
        return norm;
    }
    if norm.ends_with("/v1") {
        norm
    } else {
        format!("{norm}/v1")
    }
}

fn models_map(
    m: &ModelsConfig,
    abilities: &std::collections::HashMap<String, crate::model_abilities::ModelAbilities>,
) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    let build_entry = |id: &str| -> serde_json::Value {
        let mut entry = serde_json::Map::new();
        entry.insert("name".into(), serde_json::Value::String(id.into()));
        // Append any matched models.dev ability fields (reasoning,
        // tool_call, attachment, temperature, limit) in schema order.
        // Unmatched ids stay name-only — OpenCode still works.
        if let Some(a) = abilities.get(id) {
            for (k, v) in crate::model_abilities::to_model_entry_fields(a) {
                entry.insert(k, v);
            }
        }
        serde_json::Value::Object(entry)
    };
    match m {
        ModelsConfig::Anthropic { default, haiku, sonnet, opus, .. } => {
            // The top-level `model` pointer is `nestra-<id>/<default>` —
            // the DEFAULT must exist in the map or OpenCode points at a
            // nonexistent model (tiers may repeat; dedupe by key).
            for id in [default.as_str(), haiku.as_str(), sonnet.as_str(), opus.as_str()] {
                if !map.contains_key(id) {
                    map.insert(id.into(), build_entry(id));
                }
            }
        }
        ModelsConfig::Openai { default, available } => {
            let ids: Vec<&str> = if available.is_empty() {
                vec![default.as_str()]
            } else {
                available.iter().map(String::as_str).collect()
            };
            for id in ids {
                map.insert(id.into(), build_entry(id));
            }
        }
    }
    serde_json::Value::Object(map)
}

fn read_jsonc_object(path: &Path) -> AppResult<serde_json::Map<String, serde_json::Value>> {
    if !path.exists() {
        return Ok(Default::default());
    }
    let raw = std::fs::read_to_string(path)?;
    let parsed = jsonc_parser::parse_to_serde_value(&raw, &Default::default())
        .map_err(internal)?
        .ok_or_else(|| AppError::Internal("opencode.json root is empty".into()))?;
    match parsed {
        serde_json::Value::Object(m) => Ok(m),
        _ => Err(AppError::Internal("opencode.json root is not an object".into())),
    }
}

#[cfg(test)]
mod tests;