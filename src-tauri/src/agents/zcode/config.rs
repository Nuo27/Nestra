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
//!
//! `config.json` is NOT the file the agent authenticates with: ZCode imports
//! that map into its runtime store (`provider_config.json`) and only while
//! that store is MISSING — afterwards it polls the store every second and
//! sends the credential stored there. Nestra therefore mirrors its provider
//! rule into the store on every write (see [`sync_runtime_store`]); a key
//! that only lives in `config.json` never reaches a running ZCode and the
//! gateway answers 401.

use crate::agents::internal;
use crate::config_writer::{
    atomic_write, ensure_backup, restore_from_backup, ConfigAdapter, DetectedProvider,
    GatewayAlias, ModelSelection, ModelsConfig, ProviderKind, ProviderSet,
};
use crate::error::{AppError, AppResult};
use std::path::Path;

/// Prefix of the provider-map keys Nestra owns. Replaced wholesale on switch.
const MANAGED_PREFIX: &str = "nestra-";

/// ZCode's runtime provider store — `~/.zcode/v2/provider_config.json`.
/// Holds the credential the agent actually sends (see module docs).
const STORE_FILENAME: &str = "provider_config.json";

/// Conservative limits advertised when the real model's abilities are unknown:
/// a wrong low limit only clamps ZCode's context planning.
const DEFAULT_CONTEXT: u64 = 200_000;
const DEFAULT_OUTPUT: u64 = 128_000;

pub struct ZCode;

/// Registry constructor for the zcode writer — see [super::SPEC].
pub fn new() -> Box<dyn crate::config_writer::ConfigAdapter> {
    Box::new(ZCode)
}

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
        let slots = model_slots(&ctx.models, &ctx.model_abilities);
        provider.insert(key.clone(), provider_entry(
            &ctx.display_name,
            zcode_kind,
            &base,
            &ctx.api_key,
            &slots,
        ));

        write(config_path, &root)?;
        sync_runtime_store(
            config_path,
            &StoreRule {
                provider_id: &key,
                provider_name: &entry_name(&ctx.display_name),
                api_type: store_api_type(zcode_kind),
                base_url: &base,
                api_key: &ctx.api_key,
                models: &slots,
            },
        )?;
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
        let slots = model_slots(&models, &abilities);
        let key = format!("{MANAGED_PREFIX}gateway");
        provider.insert(
            key.clone(),
            provider_entry(
                "Nestra Gateway",
                // The gateway's inbound wire for zcode is always Anthropic
                // Messages (dispatch default arm); the router converts to
                // whatever the resolved upstream speaks.
                "anthropic",
                &alias.gateway_base_url,
                &alias.sentinel_key,
                &slots,
            ),
        );

        write(config_path, &root)?;
        sync_runtime_store(
            config_path,
            &StoreRule {
                provider_id: &key,
                provider_name: &entry_name("Nestra Gateway"),
                api_type: "anthropic-messages",
                base_url: &alias.gateway_base_url,
                api_key: &alias.sentinel_key,
                models: &slots,
            },
        )?;
        Ok(backup_created)
    }
}

/// One model slot: the id ZCode knows plus the limits advertised to it.
struct ModelSlot {
    id: String,
    context: u64,
    output: u64,
}

/// Model slots + their advertised limits, shared by the `config.json` entry
/// and the runtime-store rule so both advertise the same window.
fn model_slots(
    models: &ModelsConfig,
    abilities: &std::collections::HashMap<String, crate::model_abilities::ModelAbilities>,
) -> Vec<ModelSlot> {
    models
        .ids()
        .into_iter()
        .map(|id| {
            let limit = abilities.get(&id).and_then(|a| a.limit.as_ref());
            ModelSlot {
                id,
                context: limit.map(|l| l.context).unwrap_or(DEFAULT_CONTEXT),
                output: limit.map(|l| l.output).unwrap_or(DEFAULT_OUTPUT),
            }
        })
        .collect()
}

/// The provider name ZCode shows — identical in `config.json` and the store.
fn entry_name(display_name: &str) -> String {
    format!("{display_name} (via Nestra)")
}

/// ZCode's runtime API type for a `config.json` provider kind.
fn store_api_type(zcode_kind: &str) -> &'static str {
    if zcode_kind == "anthropic" {
        "anthropic-messages"
    } else {
        "openai-chat-completions"
    }
}

/// One Nestra-managed provider rule for ZCode's runtime store.
struct StoreRule<'a> {
    /// Rule id — the same key the provider gets in `config.json`.
    provider_id: &'a str,
    /// Display name shown in ZCode's provider list.
    provider_name: &'a str,
    /// `anthropic-messages` | `openai-chat-completions`.
    api_type: &'a str,
    /// Base URL (ZCode appends its own version tail).
    base_url: &'a str,
    /// The credential the agent sends: the gateway's loopback token in Routed
    /// mode, the real provider key in Direct mode.
    api_key: &'a str,
    /// Model slots advertised to the agent.
    models: &'a [ModelSlot],
}

/// Mirror one Nestra-managed provider into ZCode's runtime store
/// (`provider_config.json`).
///
/// ZCode imports the `provider` map of `config.json` ONLY while the store
/// file is missing; afterwards the store is the credential source and is
/// polled every second. So the rule is CREATED when missing (otherwise the
/// provider never shows up in a ZCode that already ran) and its key + endpoint
/// are refreshed when present (otherwise a rotated loopback token keeps
/// failing with a 401). Every other provider in the store is left untouched.
fn sync_runtime_store(config_path: &Path, rule: &StoreRule) -> AppResult<()> {
    let store_path = config_path
        .parent()
        .ok_or_else(|| AppError::Internal("zcode config path has no parent".into()))?
        .join(STORE_FILENAME);
    if !store_path.exists() {
        // ZCode creates the store on first start and imports `config.json`
        // while it is missing — creating it here would suppress that import.
        return Ok(());
    }
    let mut root = match read_json_object(&store_path) {
        Ok(root) => root,
        Err(e) => {
            // ZCode owns this file; a broken store is its problem to recover
            // (it rewrites the file from the builtin providers). Warn and
            // keep the switch working.
            tracing::warn!(path = %store_path.display(), error = %e, "zcode provider store unreadable — credential not mirrored");
            return Ok(());
        }
    };

    let ids: Vec<String> = rule.models.iter().map(|m| m.id.clone()).collect();
    {
        let config = member_obj(&mut root, "config");
        let rules = member_obj(config, "providerConfigRules");
        let provider_rules = member_arr(rules, "providerRules");
        let entry = match provider_rules
            .iter()
            .position(|r| r.get("providerId").and_then(|v| v.as_str()) == Some(rule.provider_id))
        {
            Some(i) => &mut provider_rules[i],
            None => {
                provider_rules.push(serde_json::json!({}));
                provider_rules.last_mut().expect("just pushed")
            }
        };
        let entry = as_obj_mut(entry);
        entry.insert("providerId".into(), serde_json::json!(rule.provider_id));
        entry.insert("providerName".into(), serde_json::json!(rule.provider_name));
        entry.insert("enabled".into(), serde_json::json!(true));
        let cfg = entry
            .entry("config".to_string())
            .or_insert_with(|| serde_json::json!({}));
        let cfg = as_obj_mut(cfg);
        cfg.insert("group".into(), serde_json::json!("standard-personal"));
        let access = cfg
            .entry("access".to_string())
            .or_insert_with(|| serde_json::json!({}));
        let access = as_obj_mut(access);
        access.insert("type".into(), serde_json::json!("api-key"));
        access.insert("apiKey".into(), serde_json::json!(rule.api_key));
        let api = cfg
            .entry("api".to_string())
            .or_insert_with(|| serde_json::json!({}));
        let api = as_obj_mut(api);
        api.insert("type".into(), serde_json::json!(rule.api_type));
        api.insert("baseUrl".into(), serde_json::json!(rule.base_url));
        cfg.insert("personalModelIds".into(), serde_json::json!(ids));
        cfg.insert("modelOrder".into(), serde_json::json!(ids));
    }
    {
        // Per-model context window — ZCode plans its context against this.
        let config = member_obj(&mut root, "config");
        let rules = member_obj(config, "providerConfigRules");
        let model_rules = member_obj(rules, "modelConfigRules");
        let entries = member_arr(model_rules, "providerModelRules");
        for slot in rule.models {
            let idx = entries.iter().position(|r| {
                r.get("providerId").and_then(|v| v.as_str()) == Some(rule.provider_id)
                    && r.get("modelId").and_then(|v| v.as_str()) == Some(slot.id.as_str())
            });
            let entry = match idx {
                Some(i) => &mut entries[i],
                None => {
                    entries.push(serde_json::json!({
                        "modelId": slot.id,
                        "providerId": rule.provider_id,
                    }));
                    entries.last_mut().expect("just pushed")
                }
            };
            let entry = as_obj_mut(entry);
            let props = entry
                .entry("config".to_string())
                .or_insert_with(|| serde_json::json!({}));
            let props = as_obj_mut(props);
            let properties = props
                .entry("properties".to_string())
                .or_insert_with(|| serde_json::json!({}));
            let properties = as_obj_mut(properties);
            properties.insert("contextWindow".into(), serde_json::json!(slot.context));
        }
    }

    let bytes = serde_json::to_vec_pretty(&root)?;
    atomic_write(&store_path, &bytes)
}

/// Object member of `root` at `key`, created when missing.
fn member_obj<'a>(
    root: &'a mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> &'a mut serde_json::Map<String, serde_json::Value> {
    let slot = root
        .entry(key.to_string())
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    as_obj_mut(slot)
}

/// Array member of `root` at `key`, created when missing.
fn member_arr<'a>(
    root: &'a mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> &'a mut Vec<serde_json::Value> {
    let slot = root
        .entry(key.to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    if !slot.is_array() {
        *slot = serde_json::Value::Array(Vec::new());
    }
    slot.as_array_mut().expect("array")
}

/// View `value` as an object, replacing a non-object payload.
fn as_obj_mut(
    value: &mut serde_json::Value,
) -> &mut serde_json::Map<String, serde_json::Value> {
    if !value.is_object() {
        *value = serde_json::Value::Object(Default::default());
    }
    value.as_object_mut().expect("object")
}

/// Drop every `nestra-*` key from the provider map (a switch replaces the
/// previous binding — the endpoint id changes, so stale keys must not linger).
fn remove_managed(provider: &mut serde_json::Map<String, serde_json::Value>) {
    provider.retain(|k, _| !k.starts_with(MANAGED_PREFIX));
}

/// Build one ZCode provider entry. `slots` become the entry's models map with
/// the context/output limits advertised to the agent (see [`model_slots`]).
fn provider_entry(
    display_name: &str,
    kind: &str,
    base_url: &str,
    api_key: &str,
    slots: &[ModelSlot],
) -> serde_json::Value {
    let mut models_map = serde_json::Map::new();
    for slot in slots {
        models_map.insert(
            slot.id.clone(),
            serde_json::json!({
                "limit": { "context": slot.context, "output": slot.output },
                "modalities": { "input": ["text"], "output": ["text"] },
            }),
        );
    }
    serde_json::json!({
        "name": entry_name(display_name),
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
