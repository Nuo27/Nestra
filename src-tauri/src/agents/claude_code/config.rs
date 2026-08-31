//! Claude Code config writer — `~/.claude/settings.json`, owns the `env` block.
//! Anthropic protocol only.

use crate::agents::internal;
use crate::config_writer::{
    ensure_backup, atomic_write, restore_from_backup, ConfigAdapter, ModelSelection,
    ModelsConfig, ProviderKind, ProviderSet,
};
use crate::error::{AppError, AppResult};
use std::collections::HashSet;
use std::path::Path;

pub struct ClaudeCode;

/// Registry constructor for the claude-code-cli writer — see [super::SPEC].
pub fn new() -> Box<dyn crate::config_writer::ConfigAdapter> {
    Box::new(ClaudeCode)
}

/// Keys Nestra owns inside `env`. Never overwritten by advanced-env merges.
const RESERVED: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
];

impl ConfigAdapter for ClaudeCode {
    fn accepts(&self) -> &'static [ProviderKind] {
        // Anthropic-wire only. OpenRouter (and any other Anthropic-compatible
        // aggregator) binds through an `anthropic` protocol row — see the
        // per-binding protocol picker in `build_switch_context`.
        &[ProviderKind::Anthropic]
    }

    fn model_selection(&self) -> ModelSelection {
        ModelSelection::AnthropicTiers
    }

    fn apply_set(&self, config_path: &Path, set: &ProviderSet) -> AppResult<bool> {
        // Claude Code is single-slot — refuse multi-entry writes rather than
        // silently dropping. Callers should fall back to a single switch.
        if set.entries.len() != 1 {
            return Err(AppError::Validation(format!(
                "Claude Code accepts exactly one provider (got {})",
                set.entries.len()
            )));
        }
        let ctx = &set.entries[0];
        let backup_created = ensure_backup(config_path)?;
        let mut root = read_json_object(config_path)?;
        let env_obj = root
            .entry("env".to_string())
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        let env = env_obj
            .as_object_mut()
            .ok_or_else(|| AppError::Internal("settings.json `env` is not an object".into()))?;

        env.insert(
            "ANTHROPIC_BASE_URL".into(),
            // Claude appends `/v1/messages` itself — normalize any full path
            // the user entered (`…/anthropic/v1/messages`) so it isn't doubled.
            serde_json::Value::String(crate::protocol_url::normalize_protocol_base(
                &ctx.base_url,
                ProviderKind::Anthropic,
            )),
        );
        env.insert(
            "ANTHROPIC_AUTH_TOKEN".into(),
            serde_json::Value::String(ctx.api_key.clone()),
        );
        // Stale `ANTHROPIC_API_KEY` (e.g. from a prior real-Anthropic login)
        // wins over the auth token and breaks routing to ANY non-official
        // endpoint — Nestra always writes `ANTHROPIC_AUTH_TOKEN`, so blank the
        // API-key slot whenever we're NOT pointing at the official Anthropic
        // endpoint. Covers OpenRouter / MiniMax / any Anthropic-compatible
        // aggregator bound through an `anthropic` row (kind is Anthropic, but
        // the base_url is not api.anthropic.com).
        let non_official = crate::protocol_url::normalize_protocol_base(
            &ctx.base_url,
            ProviderKind::Anthropic,
        ) != "https://api.anthropic.com";
        if non_official {
            env.insert("ANTHROPIC_API_KEY".into(), serde_json::Value::String("".into()));
        }
        // Claude Code treats unknown model ids as 200k-context by default. For
        // third-party Anthropic-protocol endpoints that advertise ≥1M context
        // (MiniMax-M3, glm-5.2, …) it needs the `[1m]` suffix to recognise
        // the wider window. `claude_code_model_id` is idempotent + no-op when
        // abilities don't report ≥1M, so official Claude models (200k) and
        // models without abilities data are written bare.
        let with_1m = |id: &str| {
            crate::model_abilities::claude_code_model_id(
                id,
                ctx.model_abilities.get(id),
            )
        };
        let (haiku, sonnet, opus) = anthropic_tiers(&ctx.models);
        let default_model = anthropic_default(&ctx.models);
        env.insert(
            "ANTHROPIC_MODEL".into(),
            serde_json::Value::String(with_1m(&default_model)),
        );
        env.insert(
            "ANTHROPIC_DEFAULT_HAIKU_MODEL".into(),
            serde_json::Value::String(with_1m(&haiku)),
        );
        env.insert(
            "ANTHROPIC_DEFAULT_SONNET_MODEL".into(),
            serde_json::Value::String(with_1m(&sonnet)),
        );
        env.insert(
            "ANTHROPIC_DEFAULT_OPUS_MODEL".into(),
            serde_json::Value::String(with_1m(&opus)),
        );

        let reserved: HashSet<&str> = RESERVED.iter().copied().collect();
        for (k, v) in &ctx.advanced_env {
            if reserved.contains(k.as_str()) {
                continue;
            }
            env.insert(k.clone(), serde_json::Value::String(stringify_value(v)));
        }

        let bytes = serde_json::to_vec_pretty(&root)?;
        atomic_write(config_path, &bytes)?;
        Ok(backup_created)
    }

    /// Gateway mode: write the stable gateway alias as `ANTHROPIC_BASE_URL`
    /// + the loopback token + per-tier model aliases. The agent then talks
    /// to the Nestra gateway, which resolves the real upstream per-task.
    /// Switching the resolved route does not rewrite this file (only
    /// policy/endpoint edits refresh it — see `refresh_alias_if_routed`).
    fn apply_gateway_set(
        &self,
        config_path: &Path,
        alias: &crate::config_writer::GatewayAlias,
    ) -> AppResult<bool> {
        let backup_created = ensure_backup(config_path)?;
        let mut root = read_json_object(config_path)?;
        let env_obj = root
            .entry("env".to_string())
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        let env = env_obj
            .as_object_mut()
            .ok_or_else(|| AppError::Internal("settings.json `env` is not an object".into()))?;

        env.insert(
            "ANTHROPIC_BASE_URL".into(),
            serde_json::Value::String(alias.gateway_base_url.clone()),
        );
        env.insert(
            "ANTHROPIC_AUTH_TOKEN".into(),
            serde_json::Value::String(alias.sentinel_key.clone()),
        );
        // Blank the API key slot so it can't win over the auth token
        // (same reason as the OpenRouter branch in `apply_set`).
        env.insert("ANTHROPIC_API_KEY".into(), serde_json::Value::String("".into()));
        // Per-slot aliases, each carrying its tier's steady-state abilities:
        // the `[1m]` marker makes Claude Code perceive the REAL context window
        // instead of defaulting the id to 200k (same rule as `apply_set`), and
        // the distinct tier ids let the gateway classify haiku/sonnet/opus
        // intent for `tier:*` routing policies. Without tier slots every env
        // var repeats the primary alias.
        let tiered = |m: &crate::config_writer::AliasModel| {
            crate::model_abilities::claude_code_model_id(&m.id, m.abilities.as_ref())
        };
        let (primary, haiku, sonnet, opus) = match &alias.tier_aliases {
            Some(t) => (
                tiered(&t.sonnet),
                tiered(&t.haiku),
                tiered(&t.sonnet),
                tiered(&t.opus),
            ),
            None => {
                let p = tiered(&alias.model_alias);
                (p.clone(), p.clone(), p.clone(), p.clone())
            }
        };
        env.insert("ANTHROPIC_MODEL".into(), serde_json::Value::String(primary));
        env.insert(
            "ANTHROPIC_DEFAULT_HAIKU_MODEL".into(),
            serde_json::Value::String(haiku),
        );
        env.insert(
            "ANTHROPIC_DEFAULT_SONNET_MODEL".into(),
            serde_json::Value::String(sonnet),
        );
        env.insert(
            "ANTHROPIC_DEFAULT_OPUS_MODEL".into(),
            serde_json::Value::String(opus),
        );

        let bytes = serde_json::to_vec_pretty(&root)?;
        atomic_write(config_path, &bytes)?;
        Ok(backup_created)
    }

    fn restore(&self, config_path: &Path) -> AppResult<()> {
        restore_from_backup(config_path)
    }
}

/// Mark Claude Code's onboarding complete in `<home>/.claude.json` — the file
/// Claude Code reads at startup. Merges the flag into any existing content
/// (preserving mcpServers / history / etc.), creating the file if absent, and
/// writes atomically. Idempotent; never un-sets the flag. Callers should treat
/// a failure here as best-effort (don't block the switch).
pub fn mark_onboarding_complete(home: &Path) -> AppResult<()> {
    let path = home.join(".claude.json");
    let mut root = read_json_object(&path)?;
    root.insert("hasCompletedOnboarding".into(), serde_json::Value::Bool(true));
    let bytes = serde_json::to_vec_pretty(&root)?;
    atomic_write(&path, &bytes)
}

/// Claude Code always wants three tiers. An anthropic-kind Provider supplies
/// them directly; an openai-shape Provider (e.g. OpenRouter) repeats its
/// default across all three — user can refine via the provider detail page.
fn anthropic_tiers(m: &ModelsConfig) -> (String, String, String) {
    match m {
        ModelsConfig::Anthropic { haiku, sonnet, opus, .. } => {
            (haiku.clone(), sonnet.clone(), opus.clone())
        }
        ModelsConfig::Openai { default, .. } => {
            (default.clone(), default.clone(), default.clone())
        }
    }
}

/// Primary model written to ANTHROPIC_MODEL — the provider's default (not a tier).
fn anthropic_default(m: &ModelsConfig) -> String {
    match m {
        ModelsConfig::Anthropic { default, .. } => default.clone(),
        ModelsConfig::Openai { default, .. } => default.clone(),
    }
}

fn stringify_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn read_json_object(path: &Path) -> AppResult<serde_json::Map<String, serde_json::Value>> {
    if !path.exists() {
        return Ok(Default::default());
    }
    let text = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(internal)?;
    value
        .into_object()
        .ok_or_else(|| AppError::Internal("settings.json root is not an object".into()))
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
