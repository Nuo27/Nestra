//! ZCode config writer — `~/.zcode/v2/config.json`, owns the `nestra-*`
//! entries in the top-level `provider` map.
//!
//! ZCode (z.ai's desktop agent harness) keeps custom model providers in the
//! desktop app's config under `provider` keyed like `builtin:zai-coding-plan`
//! (see the real-world shape pinned by `fixtures/zcode/config.json`). Each
//! entry is `{ name, kind, options { baseURL, apiKey }, enabled, source,
//! models }`; `kind` is `"anthropic"` (base + `/v1/messages`) or
//! `"openai-compatible"` (base + `/chat/completions`, base keeps its `/v1`) —
//! both documented at zcode.z.ai/en/docs/configuration. Selecting the provider
//! happens in ZCode's Model Providers UI — Nestra writes the entry, it does
//! not touch ZCode's selection keys in `setting.json`.

use super::internal;
use crate::config_writer::{
    atomic_write, ensure_backup, restore_from_backup, ConfigAdapter, DetectedProvider,
    GatewayAlias, ModelSelection, ModelsConfig, ProviderKind, ProviderSet,
};
use crate::error::{AppError, AppResult};
use std::path::Path;

/// Prefix of the provider-map keys Nestra owns. Replaced wholesale on switch.
const MANAGED_PREFIX: &str = "nestra-";

pub struct ZCode;

impl ConfigAdapter for ZCode {
    fn accepts(&self) -> &'static [ProviderKind] {
        // Both wire families ZCode supports (docs → "Connect Models"):
        // anthropic-compatible and OpenAI-compatible endpoints. Custom is
        // user-defined OpenAI-compatible, so it maps to the same wire.
        &[ProviderKind::Anthropic, ProviderKind::Openai, ProviderKind::Custom]
    }

    fn model_selection(&self) -> ModelSelection {
        // ZCode surfaces a flat models map per provider, not Claude-style
        // tiers — same editor shape as OpenCode.
        ModelSelection::FreeForm
    }

    fn apply_set(&self, config_path: &Path, set: &ProviderSet) -> AppResult<bool> {
        // Single-slot, like Claude Code: refuse multi-entry writes rather than
        // silently dropping entries.
        if set.entries.len() != 1 {
            return Err(AppError::Validation(format!(
                "ZCode accepts exactly one provider (got {})",
                set.entries.len()
            )));
        }
        let ctx = &set.entries[0];
        let backup_created = ensure_backup(config_path)?;
        let mut root = read_json_object(config_path)?;
        let provider = provider_map_mut(&mut root)?;

        remove_managed(provider);
        let key = format!("{MANAGED_PREFIX}{}", ctx.provider_id);
        // `kind` follows the bound protocol row: anthropic base +
        // `/v1/messages`; openai-compatible base + `/chat/completions`.
        // ZCode (AI-SDK style) appends ONLY `/chat/completions`, so the
        // openai base must carry its version root (`/v1`, `/paas/v4`) —
        // derive it through the canonical join and strip the tail, which
        // senses the version root like every other join site.
        let (zcode_kind, base) = match ctx.provider_kind {
            ProviderKind::Anthropic => (
                "anthropic",
                crate::protocol_url::normalize_protocol_base(&ctx.base_url, ProviderKind::Anthropic),
            ),
            _ => {
                let full = crate::protocol_url::join_protocol_path(&ctx.base_url, ProviderKind::Openai);
                (
                    "openai-compatible",
                    full.strip_suffix("/chat/completions")
                        .unwrap_or(&full)
                        .to_string(),
                )
            }
        };
        provider.insert(key, provider_entry(
            &ctx.display_name,
            zcode_kind,
            &base,
            &ctx.api_key,
            &ctx.models,
            &ctx.model_abilities,
        ));

        write(config_path, &root)?;
        Ok(backup_created)
    }

    fn restore(&self, config_path: &Path) -> AppResult<()> {
        restore_from_backup(config_path)
    }

    fn inspect(&self, config_path: &Path) -> AppResult<Vec<DetectedProvider>> {
        let root = read_json_object(config_path)?;
        let Some(provider) = root.get("provider").and_then(|v| v.as_object()) else {
            return Ok(Vec::new());
        };
        Ok(provider
            .iter()
            .filter(|(k, v)| !k.starts_with(MANAGED_PREFIX) && v.is_object())
            .map(|(k, v)| DetectedProvider {
                key: k.clone(),
                display_name: v
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or(k)
                    .to_string(),
                managed: false,
            })
            .collect())
    }

    fn remove(&self, config_path: &Path, key: &str) -> AppResult<()> {
        let mut root = read_json_object(config_path)?;
        let provider = provider_map_mut(&mut root)?;
        if provider.remove(key).is_none() {
            return Err(AppError::Validation(format!("provider '{key}' not found")));
        }
        write(config_path, &root)
    }

    /// Gateway mode: same provider entry, but pointed at the stable gateway
    /// alias with the sentinel key and a single alias model (carrying the
    /// steady-state model's real limits — a bare alias makes ZCode fall back
    /// to its 200k/128k guess). The router resolves the real upstream
    /// per-task.
    fn apply_gateway_set(
        &self,
        config_path: &Path,
        alias: &GatewayAlias,
    ) -> AppResult<bool> {
        let backup_created = ensure_backup(config_path)?;
        let mut root = read_json_object(config_path)?;
        let provider = provider_map_mut(&mut root)?;

        remove_managed(provider);
        // Single alias model — the gateway ignores the agent-stated model and
        // resolves the real one per-task.
        let models = ModelsConfig::Openai {
            default: alias.model_alias.id.clone(),
            available: vec![alias.model_alias.id.clone()],
        };
        let mut abilities = std::collections::HashMap::new();
        if let Some(a) = &alias.model_alias.abilities {
            abilities.insert(alias.model_alias.id.clone(), a.clone());
        }
        provider.insert(
            format!("{MANAGED_PREFIX}gateway"),
            provider_entry(
                "Nestra Gateway",
                // The gateway's inbound wire for zcode is always Anthropic
                // Messages (dispatch default arm); the router converts to
                // whatever the resolved upstream speaks.
                "anthropic",
                &alias.gateway_base_url,
                &alias.sentinel_key,
                &models,
                &abilities,
            ),
        );

        write(config_path, &root)?;
        Ok(backup_created)
    }
}

/// Drop every `nestra-*` key from the provider map (a switch replaces the
/// previous binding — the endpoint id changes, so stale keys must not linger).
fn remove_managed(provider: &mut serde_json::Map<String, serde_json::Value>) {
    provider.retain(|k, _| !k.starts_with(MANAGED_PREFIX));
}

/// Build one ZCode provider entry. `models` become the entry's models map with
/// context/output limits from models.dev when known (conservative 200k/128k
/// fallback — a wrong low limit only clamps ZCode's context planning, an
/// inflated one would break requests).
fn provider_entry(
    display_name: &str,
    kind: &str,
    base_url: &str,
    api_key: &str,
    models: &ModelsConfig,
    abilities: &std::collections::HashMap<String, crate::model_abilities::ModelAbilities>,
) -> serde_json::Value {
    let mut models_map = serde_json::Map::new();
    for id in models.ids() {
        let (context, output) = match abilities.get(&id).and_then(|a| a.limit.as_ref()) {
            Some(l) => (l.context, l.output),
            None => (200_000, 128_000),
        };
        models_map.insert(
            id,
            serde_json::json!({
                "limit": { "context": context, "output": output },
                "modalities": { "input": ["text"], "output": ["text"] },
            }),
        );
    }
    serde_json::json!({
        "name": format!("{display_name} (via Nestra)"),
        "kind": kind,
        "options": { "baseURL": base_url, "apiKey": api_key },
        "enabled": true,
        "source": "custom",
        "models": models_map,
    })
}

fn provider_map_mut(
    root: &mut serde_json::Map<String, serde_json::Value>,
) -> AppResult<&mut serde_json::Map<String, serde_json::Value>> {
    let obj = root
        .entry("provider".to_string())
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    obj.as_object_mut()
        .ok_or_else(|| AppError::Internal("config.json `provider` is not an object".into()))
}

fn read_json_object(path: &Path) -> AppResult<serde_json::Map<String, serde_json::Value>> {
    if !path.exists() {
        return Ok(Default::default());
    }
    let text = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(internal)?;
    value
        .into_object()
        .ok_or_else(|| AppError::Internal("config.json root is not an object".into()))
}

fn write(path: &Path, root: &serde_json::Map<String, serde_json::Value>) -> AppResult<()> {
    let bytes = serde_json::to_vec_pretty(root)?;
    atomic_write(path, &bytes)
}

trait IntoObject {
    fn into_object(self) -> Option<serde_json::Map<String, serde_json::Value>>;
}
impl IntoObject for serde_json::Value {
    fn into_object(self) -> Option<serde_json::Map<String, serde_json::Value>> {
        match self {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
