//! Codex config writer — `~/.codex/config.toml`, TOML edited surgically with
//! `toml_edit` (comments, key order, and every unrelated section — desktop
//! state, marketplaces, plugins, `mcp_servers`, `projects`, `notify` — are
//! preserved byte-for-byte).
//!
//! Codex selects a provider via the root `model_provider` key + a
//! `[model_providers.<id>]` table and speaks ONLY the OpenAI Responses wire
//! (`wire_api = "responses"`), so Direct bindings need a Responses-protocol
//! endpoint; any upstream works in Routed mode (the gateway converts).
//!
//! Auth: the provider table carries `experimental_bearer_token` +
//! `requires_openai_auth = true` — the "keep ChatGPT login, route model
//! traffic via API key" shape battle-tested by CodexPlusPlus. `auth.json`
//! (ChatGPT login state / keyring pointer) is NEVER touched.
//!
//! After a switch, [`super::sync`] rewrites the provider recorded in session
//! metadata so existing conversations stay visible in the Desktop app.

use crate::agents::internal;
use crate::config_writer::{
    atomic_write, ensure_backup, restore_from_backup, ConfigAdapter, DetectedProvider,
    GatewayAlias, ModelSelection, ProviderKind, ProviderSet,
};
use crate::error::{AppError, AppResult};
use std::path::Path;
use toml_edit::{value, DocumentMut, Item, Table};

/// Prefix of the `[model_providers]` keys Nestra owns.
const MANAGED_PREFIX: &str = "nestra-";

pub struct Codex;

/// Registry constructor for the codex writer — see [super::SPEC].
pub fn new() -> Box<dyn ConfigAdapter> {
    Box::new(Codex)
}

impl ConfigAdapter for Codex {
    fn accepts(&self) -> &'static [ProviderKind] {
        // Codex only speaks the Responses wire; a Direct binding must be a
        // Responses endpoint (the `openai-responses` preset / custom). Chat
        // and Anthropic endpoints are usable through the gateway instead.
        &[ProviderKind::Responses]
    }

    fn model_selection(&self) -> ModelSelection {
        // Flat model list (root `model` key), no Claude-style tiers.
        ModelSelection::FreeForm
    }

    fn apply_set(&self, config_path: &Path, set: &ProviderSet) -> AppResult<bool> {
        // Single-slot, like Claude Code / ZCode: refuse multi-entry writes.
        if set.entries.len() != 1 {
            return Err(AppError::Validation(format!(
                "Codex accepts exactly one provider (got {})",
                set.entries.len()
            )));
        }
        let ctx = &set.entries[0];
        let backup_created = ensure_backup(config_path)?;
        let mut doc = read_doc(config_path)?;

        let provider_key = format!("{MANAGED_PREFIX}{}", ctx.provider_id);
        // Codex appends `/responses` to base_url itself, so write the
        // version root: derive the canonical full path, then strip the
        // `/responses` tail (senses `/v1` roots like every other join site).
        let full =
            crate::protocol_url::join_protocol_path(&ctx.base_url, ProviderKind::Responses);
        let base = full.strip_suffix("/responses").unwrap_or(&full).to_string();
        let default_model = ctx.models.default_model().to_string();
        let context_window = ctx
            .model_abilities
            .get(&default_model)
            .and_then(|a| a.limit.as_ref())
            .map(|l| l.context);

        write_provider_table(&mut doc, &provider_key, provider_entry(
            &ctx.display_name,
            &base,
            &ctx.api_key,
        ));
        set_root_keys(&mut doc, &provider_key, &default_model, context_window);
        write(config_path, &doc)?;

        super::sync::sync_provider_visibility(config_path, &provider_key);
        Ok(backup_created)
    }

    fn restore(&self, config_path: &Path) -> AppResult<()> {
        restore_from_backup(config_path)?;
        // Re-point session metadata at whatever the restored file selects so
        // old conversations stay visible under the pre-Nestra provider.
        if let Ok(doc) = read_doc(config_path) {
            if let Some(key) = doc
                .get("model_provider")
                .and_then(|v| v.as_str())
                .map(str::to_string)
            {
                super::sync::sync_provider_visibility(config_path, &key);
            }
        }
        Ok(())
    }

    fn inspect(&self, config_path: &Path) -> AppResult<Vec<DetectedProvider>> {
        let doc = read_doc(config_path)?;
        let Some(providers) = doc.get("model_providers").and_then(Item::as_table) else {
            return Ok(Vec::new());
        };
        Ok(providers
            .iter()
            .filter(|(k, _)| !k.starts_with(MANAGED_PREFIX))
            .map(|(k, v)| DetectedProvider {
                key: k.to_string(),
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
        let mut doc = read_doc(config_path)?;
        let Some(providers) = doc
            .get_mut("model_providers")
            .and_then(Item::as_table_mut)
        else {
            return Err(AppError::Validation(format!(
                "provider '{key}' not found"
            )));
        };
        if providers.remove(key).is_none() {
            return Err(AppError::Validation(format!(
                "provider '{key}' not found"
            )));
        }
        // Drop the selection keys when they pointed at the removed table —
        // Codex falls back to its default provider.
        if doc
            .get("model_provider")
            .and_then(|v| v.as_str())
            .is_some_and(|v| v == key)
        {
            doc.as_table_mut().remove("model_provider");
        }
        write(config_path, &doc)
    }

    /// Gateway mode: same provider table, pointed at the stable gateway alias
    /// with the sentinel key as the bearer token and the alias model
    /// (carrying the steady-state model's real context window via
    /// `model_context_window` — Codex otherwise guesses 200k).
    fn apply_gateway_set(&self, config_path: &Path, alias: &GatewayAlias) -> AppResult<bool> {
        let backup_created = ensure_backup(config_path)?;
        let mut doc = read_doc(config_path)?;

        let provider_key = format!("{MANAGED_PREFIX}gateway");
        let base = format!("{}/v1", alias.gateway_base_url.trim_end_matches('/'));
        let context_window = alias
            .model_alias
            .abilities
            .as_ref()
            .and_then(|a| a.limit.as_ref())
            .map(|l| l.context);

        write_provider_table(&mut doc, &provider_key, provider_entry(
            "Nestra Gateway",
            &base,
            &alias.sentinel_key,
        ));
        set_root_keys(&mut doc, &provider_key, &alias.model_alias.id, context_window);
        write(config_path, &doc)?;

        super::sync::sync_provider_visibility(config_path, &provider_key);
        Ok(backup_created)
    }
}

/// The `[model_providers.<key>]` value Nestra writes. `requires_openai_auth`
/// + `experimental_bearer_token` is the proven "official login + API key"
/// combination: the bearer token authenticates this provider's requests
/// while the ChatGPT login state in auth.json stays untouched.
fn provider_entry(display_name: &str, base_url: &str, api_key: &str) -> Table {
    let mut t = Table::new();
    t["name"] = value(format!("{display_name} (via Nestra)"));
    t["wire_api"] = value("responses");
    t["base_url"] = value(base_url);
    t["requires_openai_auth"] = value(true);
    t["experimental_bearer_token"] = value(api_key);
    t
}

/// Replace every `nestra-*` table under `[model_providers]` with `table` and
/// register `key` as the active provider. Creates an implicit
/// `[model_providers]` container when absent (renders as
/// `[model_providers.<key>]`, never a bare header).
fn write_provider_table(doc: &mut DocumentMut, key: &str, entry: Table) {
    let root = doc.as_table_mut();
    let providers = root
        .entry("model_providers")
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_mut()
        .expect("model_providers is a table by construction");
    let managed: Vec<String> = providers
        .iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| k.starts_with(MANAGED_PREFIX))
        .collect();
    for k in managed {
        providers.remove(&k);
    }
    providers.insert(key, Item::Table(entry));
}

/// Root selection keys: active provider, default model, and the real context
/// window when the abilities chain knows it.
fn set_root_keys(doc: &mut DocumentMut, provider: &str, model: &str, context_window: Option<u64>) {
    let root = doc.as_table_mut();
    root.insert("model_provider", value(provider));
    root.insert("model", value(model));
    match context_window {
        Some(n) => {
            root.insert("model_context_window", value(n as i64));
        }
        None => {
            root.remove("model_context_window");
        }
    }
}

fn read_doc(path: &Path) -> AppResult<DocumentMut> {
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    let text = std::fs::read_to_string(path)?;
    text.parse().map_err(internal)
}

fn write(path: &Path, doc: &DocumentMut) -> AppResult<()> {
    let mut out = doc.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    atomic_write(path, out.as_bytes())
}

#[cfg(test)]
mod tests;
