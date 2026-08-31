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
//! Auth: two shapes, picked per switch from the login state in the sibling
//! `auth.json` (the Desktop app's login gate passes when that file carries
//! ChatGPT `tokens` OR an `OPENAI_API_KEY`):
//! - ChatGPT login present: `requires_openai_auth = true` +
//!   `experimental_bearer_token` — the "keep ChatGPT login, route model
//!   traffic via API key" shape; `auth.json` is never touched.
//! - no ChatGPT login (pure-API mode): the provider
//!   table drops `requires_openai_auth` (its official meaning is "this
//!   provider uses OpenAI authentication"; `true` is exactly what makes
//!   the app demand a ChatGPT login at startup) and the key is written
//!   into `auth.json` as `OPENAI_API_KEY` so the gate passes. Nestra owns
//!   that key slot while managing Codex — every switch refreshes it, and
//!   restore returns the user's original file.
//!
//! After a switch, [`super::sync`] rewrites the provider recorded in session
//! metadata so existing conversations stay visible in the Desktop app.

use crate::agents::internal;
use crate::config_writer::{
    atomic_write, backup_path_for, ensure_backup, restore_from_backup, ConfigAdapter,
    DetectedProvider, GatewayAlias, ModelSelection, ProviderKind, ProviderSet, NO_ORIGINAL_SENTINEL,
};
use crate::error::{AppError, AppResult};
use std::path::{Path, PathBuf};
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
        let auth = auth_path(config_path);
        // No ChatGPT login in auth.json → pure-API shape, else the app
        // demands a login at startup (see module header).
        let pure_api = !has_chatgpt_login(&auth);
        let backup_created = ensure_backup(config_path)?;
        if pure_api {
            ensure_backup(&auth)?;
        }
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
            pure_api,
        ));
        set_root_keys(&mut doc, &provider_key, &default_model, context_window);
        write(config_path, &doc)?;
        if pure_api {
            write_auth_key(&auth, &ctx.api_key)?;
        }

        super::sync::sync_provider_visibility(config_path, &provider_key);
        Ok(backup_created)
    }

    fn restore(&self, config_path: &Path) -> AppResult<()> {
        restore_from_backup(config_path)?;
        restore_auth(&auth_path(config_path))?;
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
    /// `model_context_window` — Codex otherwise guesses 200k). Auth shape
    /// follows the same auth.json login-state rule as Direct mode; the
    /// sentinel lands in auth.json when there is no login.
    fn apply_gateway_set(&self, config_path: &Path, alias: &GatewayAlias) -> AppResult<bool> {
        let auth = auth_path(config_path);
        let pure_api = !has_chatgpt_login(&auth);
        let backup_created = ensure_backup(config_path)?;
        if pure_api {
            ensure_backup(&auth)?;
        }
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
            pure_api,
        ));
        set_root_keys(&mut doc, &provider_key, &alias.model_alias.id, context_window);
        write(config_path, &doc)?;
        if pure_api {
            write_auth_key(&auth, &alias.sentinel_key)?;
        }

        super::sync::sync_provider_visibility(config_path, &provider_key);
        Ok(backup_created)
    }

    fn extra_config_paths(&self, config_path: &Path) -> Vec<PathBuf> {
        vec![auth_path(config_path)]
    }
}

/// The `[model_providers.<key>]` value Nestra writes. Two auth shapes,
/// picked by the caller from the auth.json login state (see module header):
/// - signed in (`pure_api = false`): `requires_openai_auth` +
///   `experimental_bearer_token` is the proven "official login + API key"
///   combination — the bearer token authenticates this provider's requests
///   while the ChatGPT login state in auth.json stays untouched.
/// - pure API (`pure_api = true`): no `requires_openai_auth` (its official
///   meaning is "uses OpenAI authentication" — `true` makes the Desktop
///   app demand a login); the bearer token still authenticates the wire
///   and [`write_auth_key`] satisfies the app's login gate.
fn provider_entry(display_name: &str, base_url: &str, api_key: &str, pure_api: bool) -> Table {
    let mut t = Table::new();
    t["name"] = value(format!("{display_name} (via Nestra)"));
    t["wire_api"] = value("responses");
    t["base_url"] = value(base_url);
    if !pure_api {
        t["requires_openai_auth"] = value(true);
    }
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

/// `auth.json` — the sibling of config.toml holding the app's login state.
fn auth_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name("auth.json")
}

/// Whether auth.json holds a ChatGPT login (`tokens`). A missing file
/// reads as false. A file that fails to parse also reads as false here;
/// the subsequent [`write_auth_key`] then fails loudly instead of
/// clobbering it. A bare `OPENAI_API_KEY` does NOT count: while Nestra
/// manages Codex it owns that slot (every switch refreshes it), so a key
/// written by an earlier switch — or the user's own — must not flip the
/// adapter into the keep-login shape.
fn has_chatgpt_login(auth: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(auth) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .is_some_and(|v| {
            v.get("tokens")
                .and_then(|t| t.as_object())
                .is_some_and(|t| !t.is_empty())
        })
}

/// Merge the provider key into auth.json as `OPENAI_API_KEY` (+ explicit
/// `apikey` auth mode) so the Desktop app's login gate passes without a
/// ChatGPT account. Unrelated fields are preserved; `atomic_write` keeps
/// the app from ever observing a half-written file.
fn write_auth_key(auth: &Path, key: &str) -> AppResult<()> {
    let mut root: serde_json::Map<String, serde_json::Value> = match std::fs::read_to_string(auth)
    {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).map_err(|e| {
            AppError::Validation(format!(
                "{} is not valid JSON ({e}) — fix or remove the file, then switch again",
                auth.display()
            ))
        })?,
        _ => Default::default(),
    };
    root.insert("OPENAI_API_KEY".into(), serde_json::json!(key));
    root.insert("auth_mode".into(), serde_json::json!("apikey"));
    let bytes = serde_json::to_vec_pretty(&serde_json::Value::Object(root))?;
    atomic_write(auth, &bytes)
}

/// Restore auth.json alongside config.toml. No-op when Nestra never wrote
/// it (no backup taken). The generic restore's hand-edit guard only
/// recognizes empty/`{}` shells, so a backup holding the no-original
/// sentinel plus a live file that carries only Nestra's own keys
/// (`OPENAI_API_KEY`/`auth_mode`) is deleted here — otherwise the app
/// would keep showing a signed-in-with-API-key state whose key points at
/// a provider that no longer exists.
fn restore_auth(auth: &Path) -> AppResult<()> {
    let backup = backup_path_for(auth);
    if !backup.exists() {
        return Ok(());
    }
    let nestra_created = std::fs::read(&backup).is_ok_and(|b| b == NO_ORIGINAL_SENTINEL.as_bytes())
        && std::fs::read_to_string(auth)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .is_some_and(|v| {
                v.as_object()
                    .is_some_and(|o| o.keys().all(|k| k == "OPENAI_API_KEY" || k == "auth_mode"))
            });
    if nestra_created {
        match std::fs::remove_file(auth) {
            Ok(()) => std::fs::remove_file(&backup).map_err(AppError::Io),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::remove_file(&backup).map_err(AppError::Io)
            }
            Err(e) => Err(AppError::Io(e)),
        }
    } else {
        restore_from_backup(auth)
    }
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
