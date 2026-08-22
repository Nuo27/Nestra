use crate::error::{AppError, AppResult};
use crate::agents;
use crate::config_writer::{
    backup_path_for, ModelsConfig, ProviderKind, ProviderSet, SwitchContext,
};
use crate::db::AgentRow;
use crate::db;
use crate::secrets;
use serde::{Deserialize, Serialize};
use super::{run_blocking, snapshot_state, AppParts};
use tauri::State;

#[derive(Serialize)]
pub struct AgentInfo {
    pub id: String,
    pub kind: String,
    pub display_name: String,
    pub agent_path: Option<String>,
    pub installed_version: Option<String>,
    pub status: String,
    pub source: AgentSource,
    pub active_provider_id: Option<String>,
    pub has_backup: bool,
    pub agent_path_override: Option<String>,
    pub config_path_override: Option<String>,
    pub config_path: Option<String>,
    /// When `false`, Nestra stops writing the CLI's config file. The row
    /// remains visible and detection still runs; provider switching is inert
    /// until re-enabled. Drives the enable/disable switch on each agent card.
    pub enabled: bool,
    /// UI-agnostic capability booleans (see `agents::Capability`). The
    /// frontend renders the per-agent configuration UI from these — never
    /// hardcodes per-agent specifics.
    pub capability: crate::agents::Capability,
    /// Wire protocols the agent's ConfigAdapter can inject, computed from
    /// `adapter_for(spec.config.writer).accepts()`. The frontend filters the
    /// provider preset list by this set. Empty for read-only agents.
    pub supported_protocols: Vec<String>,
    /// How the agent's config format surfaces model selection (drives the
    /// models-editor shape).
    pub model_selection: crate::config_writer::ModelSelection,
    /// Provider entries owned by this agent. For single-slot agents the array
    /// carries at most one entry; multi-slot agents expose the full list.
    pub providers: Vec<AgentProvider>,
    /// `true` when the agent has a ConfigAdapter and Nestra can actively
    /// manage its config.
    pub manageable: bool,
    /// `true` when a Factory Configuration snapshot has been captured for
    /// this agent. Drives the "Restore factory" action.
    pub has_factory: bool,
    /// Free-form detection hint surfaced to the UI. Always `None` for the
    /// currently-supported agents.
    pub status_detail: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct AgentProvider {
    /// The agent this binding belongs to.
    pub agent_id: String,
    /// The endpoint id this binding points at (the global Provider record).
    pub provider_id: String,
    pub display_name: String,
    pub protocol: String,
    pub base_url: String,
    pub has_api_key: bool,
    pub status: String,
    pub last_validated_at: Option<i64>,
    /// `true` when this binding is the active one for the agent (exactly one
    /// per agent at any time). The frontend renders an "active" badge and
    /// routes the Activate action through `agent_switch_provider`.
    pub active: bool,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum AgentSource {
    /// Detected via PATH / install-dir / config-dir probes.
    Auto,
    /// User pinned a path or install dir.
    Manual,
}

#[derive(Serialize)]
pub struct SwitchResultOut {
    pub ok: bool,
    pub agent_id: String,
    pub provider_id: String,
    pub backup_created: bool,
    pub error: Option<String>,
}

fn agent_to_info(row: AgentRow) -> AgentInfo {
    let source = if row.path_override.is_some() || row.config_path_override.is_some() {
        AgentSource::Manual
    } else {
        AgentSource::Auto
    };
    // Capability + manageability + adapter-derived fields come from the AGENTS
    // registry (single source of truth). Unknown ids (rows no longer in
    // AGENTS) get neutral defaults — purely defensive: `list_agent_infos`
    // filters non-registry rows out before this runs, and `detect_all_agents`
    // never re-probes ids without a detector.
    let spec = crate::agents::agent_spec(&row.id);
    let manageable = spec.map(|s| s.manageable()).unwrap_or(false);
    let capability = spec.map(|s| s.capability).unwrap_or(crate::agents::Capability {
        manageable: false,
        supports_provider_configuration: false,
        supports_multiple_providers: false,
        supports_provider_injection: false,
        supports_factory_restore: false,
        supports_sessions: false,
        supports_mcp: false,
        supports_mcp_enabled: false,
        supports_skills: false,
        supports_gateway: false,
    });
    let (supported_protocols, model_selection) = match spec.and_then(|s| agents::adapter_for(s.config.writer)) {
        Some(adapter) => (
            adapter.accepts().iter().map(|k| k.as_str().to_string()).collect(),
            adapter.model_selection(),
        ),
        None => (Vec::new(), crate::config_writer::ModelSelection::FreeForm),
    };
    AgentInfo {
        id: row.id,
        kind: row.kind,
        display_name: row.display_name,
        agent_path: row.path,
        installed_version: row.installed_version,
        status: row.status,
        source,
        active_provider_id: row.active_provider_id,
        has_backup: row.backup_path.is_some(),
        agent_path_override: row.path_override,
        config_path_override: row.config_path_override,
        config_path: row.config_path,
        enabled: row.enabled,
        capability,
        supported_protocols,
        model_selection,
        providers: Vec::new(), // populated separately by list_agent_infos
        manageable,
        has_factory: row.factory_backup_path.is_some(),
        status_detail: row.status_detail,
    }
}

fn binding_to_info(b: db::BindingRow) -> AgentProvider {
    AgentProvider {
        agent_id: b.agent_id,
        provider_id: b.endpoint_id,
        display_name: b.display_name,
        protocol: b.resolved_protocol.unwrap_or_default(),
        base_url: b.resolved_base_url.unwrap_or_default(),
        has_api_key: b.has_api_key,
        status: b.status,
        last_validated_at: b.last_validated_at,
        active: b.active,
    }
}

/// Models.dev defaults flow into `SwitchContext.model_abilities` only when
/// the user has no override for that field. Pure unit test (no DB) so it
/// stays fast.
#[cfg(test)]
mod abilities_merge_tests;

/// Empty tier fields fall back to `default`.
fn parse_models(kind: ProviderKind, json: Option<&str>) -> Option<ModelsConfig> {
    let v: serde_json::Value = serde_json::from_str(json?).ok()?;
    let default_str = v
        .get("default")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let available: Vec<String> = v
        .get("available")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let pick_tier = |tier: &str| -> String {
        v.get(tier)
            .and_then(|s| s.as_str())
            .map(String::from)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default_str.clone())
    };
    match kind {
        ProviderKind::Anthropic => Some(ModelsConfig::Anthropic {
            default: default_str.clone(),
            haiku: pick_tier("haiku"),
            sonnet: pick_tier("sonnet"),
            opus: pick_tier("opus"),
        }),
        _ => Some(ModelsConfig::Openai {
            default: default_str,
            available,
        }),
    }
}

pub(crate) fn detect_all_agents(conn: &rusqlite::Connection) -> AppResult<Vec<AgentRow>> {
    let agents_rows = db::list_agents(conn)?;
    for row in agents_rows.iter() {
        let Some(detector) = agents::agent_spec(&row.id) else { continue };
        let override_path = row.path_override.as_deref().map(std::path::Path::new);
        let config_override = row.config_path_override.as_deref().map(std::path::Path::new);
        let probe = agents::detect::probe(detector, override_path, config_override)?;
        let status = match probe.status {
            agents::detect::ProbeStatus::Ok => "ok",
            agents::detect::ProbeStatus::Missing => "missing",
            agents::detect::ProbeStatus::ManualMissing => "manual_missing",
        };
        let agent_path_str = probe
            .cli_path
            .as_ref()
            .and_then(|p| p.to_str())
            .map(String::from);
        let config_path_str = probe
            .config_path
            .as_ref()
            .and_then(|p| p.to_str())
            .map(String::from);
        db::upsert_agent(
            conn,
            &row.id,
            &row.kind,
            &row.display_name,
            agent_path_str.as_deref(),
            probe.installed_version.as_deref(),
            status,
            config_path_str.as_deref(),
        )?;
    }
    db::list_agents(conn)
}

// ---- CLI commands ----

/// Reads the persisted detection cadence ("on-launch" default, "manual").
pub(crate) fn detection_cadence(conn: &rusqlite::Connection) -> String {
    db::get_setting(conn, "app")
        .ok()
        .flatten()
        .and_then(|v| {
            v.get("detection_cadence")
                .and_then(|c| c.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| "on-launch".into())
}

/// Cache read: returns the last detected state without re-scanning.
/// (Detection happens at launch and on explicit `agent_detect`.)
fn list_agent_infos(conn: &rusqlite::Connection) -> AppResult<Vec<AgentInfo>> {
    let mut infos: Vec<AgentInfo> = db::list_agents(conn)?
        .into_iter()
        // Only the closed AGENTS registry surfaces in the UI. Historical rows
        // carried over by the 0.1 import (copilot-cli, qwen-code, opencode, …)
        // stay in the DB — detection never re-probes them — but are never
        // listed, so the Agents page shows just the three live agents.
        .filter(|row| crate::agents::agent_spec(&row.id).is_some())
        .map(agent_to_info)
        .collect();
    // Attach per-agent providers in one pass. Two queries instead of N×2.
    let all_providers = load_all_agent_providers(conn)?;
    for info in &mut infos {
        if let Some(provs) = all_providers.get(&info.id) {
            info.providers = provs.clone();
        }
    }
    Ok(infos)
}

fn load_all_agent_providers(
    conn: &rusqlite::Connection,
) -> AppResult<std::collections::HashMap<String, Vec<AgentProvider>>> {
    use std::collections::HashMap;
    let rows = db::list_all_bindings(conn)?;
    let mut out: HashMap<String, Vec<AgentProvider>> = HashMap::new();
    for row in rows {
        out.entry(row.agent_id.clone())
            .or_default()
            .push(binding_to_info(row));
    }
    Ok(out)
}

#[tauri::command]
pub fn agent_list(state: State<'_, crate::AppState>) -> AppResult<Vec<AgentInfo>> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    list_agent_infos(&conn)
}

#[tauri::command]
pub async fn agent_detect(state: State<'_, crate::AppState>) -> AppResult<Vec<AgentInfo>> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        Ok(detect_all_agents(&conn)?.into_iter().map(agent_to_info).collect())
    })
    .await
}

/// Build a `SwitchContext` for a single endpoint. Used by the
/// `agent_apply_provider_selection` flow (single- and multi-entry) and by
/// `do_switch_provider` (tray / direct-config restore).
///
/// `protocol` is the per-binding wire override from the protocol picker:
/// - `Some(name)` honoring an accepted protocol row the endpoint carries →
///   that row is used (how a dual-protocol endpoint like OpenRouter is steered
///   to anthropic vs openai per agent).
/// - `None`, or a name that is no longer accepted / no longer present → fall
///   back to the first accepted protocol row (the historical default, so a
///   NULL override reproduces the pre-picker result).
fn build_switch_context(
    parts: &AppParts,
    agent_id: &str,
    provider_id: &str,
    protocol: Option<&str>,
) -> AppResult<(crate::agents::AgentSpec, Box<dyn crate::config_writer::ConfigAdapter>, SwitchContext)> {
    let spec = crate::agents::agent_spec(agent_id)
        .ok_or_else(|| AppError::NotFound(format!("unknown agent '{agent_id}'")))?
        .clone();
    let adapter = agents::adapter_for(spec.config.writer)
        .ok_or_else(|| AppError::NotFound(format!("no config adapter for '{agent_id}'")))?;

    let endpoint = {
        let conn = parts.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        db::get_endpoint(&conn, provider_id)?
            .ok_or_else(|| AppError::NotFound(format!("endpoint '{provider_id}' not found")))?
    };

    let accepted = adapter.accepts();
    // A stored override wins only when it still names an accepted protocol the
    // endpoint carries; otherwise the first accepted row (historical default).
    let chosen_row = protocol
        .filter(|name| accepted.iter().any(|k| k.as_str() == *name))
        .and_then(|name| endpoint.protocols.iter().find(|p| p.protocol == name));
    let (kind, base_url) = if let Some(row) = chosen_row {
        let kind = accepted
            .iter()
            .find(|k| k.as_str() == row.protocol)
            .copied()
            .expect("chosen row's protocol is in accepts()");
        (kind, row.base_url.clone())
    } else {
        accepted
            .iter()
            .find_map(|k| {
                endpoint.protocols.iter()
                    .find(|p| p.protocol == k.as_str())
                    .map(|p| (*k, p.base_url.clone()))
            })
            .ok_or_else(|| {
                AppError::Validation(format!(
                    "endpoint '{provider_id}' has no protocol accepted by '{agent_id}' (accepts: {})",
                    accepted
                        .iter()
                        .map(crate::config_writer::ProviderKind::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?
    };

    let key = secrets::get(provider_id)?
        .ok_or_else(|| AppError::Validation(format!(
            "no API key set for endpoint '{provider_id}' — add one on the Providers page"
        )))?;
    let models = parse_models(kind, endpoint.models_json.as_deref())
        .ok_or_else(|| AppError::Validation(
            "no models selected on the endpoint — open the Providers page and pick a model".into(),
        ))?;
    let advanced_env = endpoint
        .advanced_env_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| match v {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default();

    // Subset the global models.dev ability index down to this provider's
    // model ids. We also do a best-effort refresh here — on a stale cache,
    // a switch self-heals so the next write carries the freshest reasoning/
    // tool_call/attachment/limit fields. Network failure leaves the prior
    // cache (or empty) intact, so this never blocks a switch.
    //
    // Layering (low → high): models.dev cache < bundled corrections (for
    // vendor-stale entries like MiniMax-M3 context) < user per-endpoint
    // overrides. The override merge only wins on fields the user explicitly
    // set, so the UI's "Reset" button is a pure delete of the override row.
    let model_abilities = {
        let conn = parts.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        let _ = crate::model_abilities::refresh(&conn, false);
        let defaults = crate::model_abilities::load_index(&conn)
            .map(|idx| crate::model_abilities::subset_for(&idx, &models.ids()))
            .unwrap_or_default();
        let corrections = crate::model_abilities::subset_for(
            &crate::model_abilities::load_corrections(),
            &models.ids(),
        );
        let with_corrections = crate::model_abilities::merge_into(defaults, corrections);
        let overrides = crate::model_abilities::parse_overrides(
            endpoint.model_abilities_json.as_deref(),
        );
        crate::model_abilities::merge_into(with_corrections, overrides)
    };

    // Anthropic-protocol binds (Claude Code) only write models that speak
    // Anthropic on this endpoint; OpenAI binds exclude responses-class
    // models (grok-4.5/gpt-5.6-luna are broken on the chat wire — route
    // them through the gateway instead).
    let models = match kind {
        crate::config_writer::ProviderKind::Anthropic => {
            filter_models_for_anthropic(models, &model_abilities)?
        }
        crate::config_writer::ProviderKind::Openai => {
            filter_models_for_openai(models, &model_abilities)?
        }
        _ => models,
    };

    let ctx = SwitchContext {
        provider_id: provider_id.to_string(),
        provider_kind: kind,
        display_name: endpoint.display_name,
        base_url,
        api_key: key,
        models,
        advanced_env,
        model_abilities,
    };
    Ok((spec, adapter, ctx))
}

/// Direct-mode protocol filter: when binding an Anthropic-protocol agent
/// (Claude Code), only write models that actually speak Anthropic on this
/// endpoint (per-model `api` from the corrections map — e.g. opencode-go's
/// grok/kimi/mimo are OpenAI-only and would fail on the Anthropic wire).
/// A tier whose model is OpenAI-only falls back to the default; a default
/// that is OpenAI-only is a hard error so the user picks an
/// Anthropic-capable model instead of shipping a config that fails on
/// first use. OpenAI-protocol binds keep the full list.
fn filter_models_for_anthropic(
    models: crate::config_writer::ModelsConfig,
    abilities: &std::collections::HashMap<String, crate::model_abilities::ModelAbilities>,
) -> AppResult<crate::config_writer::ModelsConfig> {
    let anthropic_only = |id: &str| {
        matches!(
            abilities.get(id).and_then(|a| a.api.as_deref()),
            Some("openai-comp") | Some("response-api")
        )
    };
    match models {
        crate::config_writer::ModelsConfig::Anthropic { default, haiku, sonnet, opus } => {
            if anthropic_only(&default) {
                return Err(AppError::Validation(format!(
                    "model '{default}' does not support the Anthropic protocol on this endpoint — pick a model that does on the Providers page"
                )));
            }
            let fallback = |m: String| if anthropic_only(&m) { default.clone() } else { m };
            let (haiku, sonnet, opus) = (fallback(haiku), fallback(sonnet), fallback(opus));
            Ok(crate::config_writer::ModelsConfig::Anthropic {
                default,
                haiku,
                sonnet,
                opus,
            })
        }
        other => Ok(other),
    }
}

/// Symmetric filter for OpenAI (chat-completions) Direct binds: models whose
/// official dialect is the Responses API (grok-4.5, gpt-5.6-luna) are broken
/// on the chat wire (grok returns 503 upstream), so they must NOT be written
/// into a chat config — the user routes them through the gateway instead
/// (Routed mode converts). A responses-class default is a hard error; the
/// rest of the list is filtered down.
fn filter_models_for_openai(
    models: crate::config_writer::ModelsConfig,
    abilities: &std::collections::HashMap<String, crate::model_abilities::ModelAbilities>,
) -> AppResult<crate::config_writer::ModelsConfig> {
    let responses_only = |id: &str| {
        matches!(
            abilities.get(id).and_then(|a| a.api.as_deref()),
            Some("response-api")
        )
    };
    match models {
        crate::config_writer::ModelsConfig::Openai { default, available } => {
            if responses_only(&default) {
                return Err(AppError::Validation(format!(
                    "model '{default}' is Responses-API only on this endpoint — pick a model that speaks Chat Completions (or use Routed mode)"
                )));
            }
            let available: Vec<String> = available
                .into_iter()
                .filter(|m| !responses_only(m))
                .collect();
            Ok(crate::config_writer::ModelsConfig::Openai { default, available })
        }
        other => Ok(other),
    }
}

/// Persist factory snapshots for an adapter's primary + extra config paths,
/// and store the primary factory path on the agent row the first time only.
/// No-op for agents that don't have a relative config path.
fn capture_factory_snapshots(parts: &AppParts, agent_id: &str, spec: &crate::agents::AgentSpec, adapter: &dyn crate::config_writer::ConfigAdapter) -> AppResult<()> {
    if spec.config.relative_path.is_empty() {
        return Ok(());
    }
    let path = crate::db::home_dir()?.join(&spec.config.relative_path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let all_paths = std::iter::once(path.clone())
        .chain(adapter.extra_config_paths(&path));
    for p in all_paths {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = crate::config_writer::capture_factory(&p, false);
    }
    // Persist factory path on the agent row once.
    {
        let conn = parts.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        let already = db::list_agents(&conn)?
            .into_iter()
            .find(|c| c.id == agent_id)
            .and_then(|c| c.factory_backup_path)
            .is_some();
        if !already {
            let fp = crate::config_writer::factory_path_for(&path);
            if let Some(s) = fp.to_str() {
                let _ = db::set_agent_factory_path(&conn, agent_id, Some(s));
            }
        }
    }
    Ok(())
}

fn ensure_enabled(parts: &AppParts, agent_id: &str) -> AppResult<()> {
    let conn = parts.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    let row = db::list_agents(&conn)?
        .into_iter()
        .find(|c| c.id == agent_id)
        .ok_or_else(|| AppError::NotFound(format!("agent '{agent_id}' not found")))?;
    if !row.enabled {
        return Err(AppError::Validation(format!(
            "'{agent_id}' is disabled — re-enable it in the Agents page to switch providers"
        )));
    }
    Ok(())
}

fn ensure_compatible(spec: &crate::agents::AgentSpec, adapter: &dyn crate::config_writer::ConfigAdapter, endpoint_id: &str, endpoint_protocols: &[db::EndpointProtocolRow], protocol: Option<&str>) -> AppResult<()> {
    let accepted = adapter.accepts();
    // When the picker selected a specific protocol, validate exactly that one
    // (must be accepted AND present as a row). Rejecting here surfaces a bad
    // selection instead of silently falling back in `build_switch_context`.
    if let Some(name) = protocol {
        if !accepted.iter().any(|k| k.as_str() == name) {
            return Err(AppError::Validation(format!(
                "agent '{}' does not accept protocol '{name}' (accepts: {})",
                spec.id,
                accepted.iter().map(crate::config_writer::ProviderKind::as_str).collect::<Vec<_>>().join(", ")
            )));
        }
        if !endpoint_protocols.iter().any(|p| p.protocol == name) {
            return Err(AppError::Validation(format!(
                "endpoint '{endpoint_id}' has no '{name}' protocol row"
            )));
        }
        return Ok(());
    }
    let compatible = endpoint_protocols
        .iter()
        .any(|p| accepted.iter().any(|k| k.as_str() == p.protocol));
    if !compatible {
        return Err(AppError::Validation(format!(
            "endpoint '{endpoint_id}' has no protocol accepted by '{}' (accepts: {})",
            spec.id,
            accepted.iter().map(crate::config_writer::ProviderKind::as_str).collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(())
}

/// Best-effort post-switch hook: Claude Code reads `hasCompletedOnboarding`
/// from `~/.claude.json` at startup and prompts the user through first-run
/// setup when it's false — flipping it true after a Nestra-managed switch
/// keeps the agent quiet. Other agents don't need this. Failure is non-fatal.
fn mark_onboarding_if_needed(agent_id: &str) {
    if agent_id != "claude-code-cli" {
        return;
    }
    if let Ok(home) = db::home_dir() {
        if let Err(e) = agents::claude_code::mark_onboarding_complete(&home) {
            tracing::warn!("failed to mark claude-code onboarding complete: {e}");
        }
    }
}

/// Shared core of `agent_switch_provider`. Holds the file write logic so the
/// tray quick-switch menu can invoke the same flow as the frontend command.
/// Runs on a blocking thread; the caller refreshes the tray menu afterwards.
pub(crate) fn do_switch_provider(
    parts: &AppParts,
    agent_id: String,
    provider_id: String,
) -> AppResult<SwitchResultOut> {
    ensure_enabled(parts, &agent_id)?;
    // Honor a stored per-binding protocol override (the picker's choice) when
    // re-switching (tray quick-switch / direct-config restore).
    let stored_protocol = {
        let conn = parts.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        db::binding_protocol(&conn, &agent_id, &provider_id)?
    };
    let (spec, adapter, ctx) =
        build_switch_context(parts, &agent_id, &provider_id, stored_protocol.as_deref())?;
    capture_factory_snapshots(parts, &agent_id, &spec, adapter.as_ref())?;

    {
        let conn = parts.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        if spec.capability.supports_multiple_providers {
            db::upsert_binding(&conn, &agent_id, &provider_id)?;
            db::set_active_binding(&conn, &agent_id, &provider_id)?;
        } else {
            // Single-slot: the binding set is exactly the active provider.
            // replace wipes any stale extras older builds accumulated
            // (switch used to upsert without clearing), keeping the DB honest.
            // Preserve the stored protocol override across the delete+re-insert.
            db::replace_bindings(
                &conn,
                &agent_id,
                std::slice::from_ref(&(provider_id.clone(), stored_protocol.clone())),
                &provider_id,
            )?;
        }
    }

    let path = crate::db::home_dir()?.join(&spec.config.relative_path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let outcome = adapter.apply(&path, &ctx)?;

    mark_onboarding_if_needed(&agent_id);

    let backup_str = backup_path_for(&path).to_str().map(String::from);
    {
        let conn = parts.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        db::set_agent_binding(&conn, &agent_id, backup_str.as_deref())?;
    }

    Ok(SwitchResultOut {
        ok: true,
        agent_id,
        provider_id,
        backup_created: outcome,
        error: None,
    })
}

/// Apply the user's full provider selection to an agent. Multi-provider
/// agents (Pi, OpenCode) write every selected provider into the config and
/// set the default to `default_provider_id`. Single-slot agents (Claude Code)
/// refuse with an error if more than one entry is passed. Always rewrites the
/// config — no-op short-circuits are wrong here (the user just changed their
/// mind about what should be active).
/// One entry of a provider selection: which endpoint, and (optionally) which
/// of its protocol rows to bind through — the per-binding Direct-wire override
/// from the protocol picker. `protocol: None` means "resolve the default
/// (first accepted)".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSelection {
    pub provider_id: String,
    #[serde(default)]
    pub protocol: Option<String>,
}

fn do_apply_provider_selection(
    parts: &AppParts,
    agent_id: String,
    selected: Vec<ProviderSelection>,
    default_provider_id: String,
) -> AppResult<()> {
    ensure_enabled(parts, &agent_id)?;
    let spec = crate::agents::agent_spec(&agent_id)
        .ok_or_else(|| AppError::NotFound(format!("unknown agent '{agent_id}'")))?
        .clone();
    let adapter = agents::adapter_for(spec.config.writer)
        .ok_or_else(|| AppError::NotFound(format!("no config adapter for '{agent_id}'")))?;

    if selected.is_empty() {
        // Clearing the entire selection — write nothing Nestra-owned.
        let path = crate::db::home_dir()?.join(&spec.config.relative_path);
        adapter.restore(&path)?;
        let conn = parts.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        db::clear_all_bindings(&conn, &agent_id)?;
        return Ok(());
    }
    if !selected.iter().any(|e| e.provider_id == default_provider_id) {
        return Err(AppError::Validation(format!(
            "default provider '{default_provider_id}' is not in the selection"
        )));
    }

    capture_factory_snapshots(parts, &agent_id, &spec, adapter.as_ref())?;

    // Validate protocol compatibility, then build a SwitchContext per entry.
    let mut entries: Vec<SwitchContext> = Vec::with_capacity(selected.len());
    for sel in &selected {
        let protocols = {
            let conn = parts.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            db::endpoint_protocols(&conn, &sel.provider_id)?
        };
        ensure_compatible(
            &spec,
            adapter.as_ref(),
            &sel.provider_id,
            &protocols,
            sel.protocol.as_deref(),
        )?;
        let (_spec, _adapter, ctx) =
            build_switch_context(parts, &agent_id, &sel.provider_id, sel.protocol.as_deref())?;
        entries.push(ctx);
    }

    let default_model = entries
        .iter()
        .find(|c| c.provider_id == default_provider_id)
        .map(|c| c.models.default_model().to_string())
        .ok_or_else(|| AppError::NotFound(format!(
            "default provider '{default_provider_id}' not in selection"
        )))?;
    let set = ProviderSet {
        entries,
        default_provider_id: default_provider_id.clone(),
        default_model,
    };

    let path = crate::db::home_dir()?.join(&spec.config.relative_path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _outcome = adapter.apply_set(&path, &set)?;

    mark_onboarding_if_needed(&agent_id);

    {
        let conn = parts.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        let rows: Vec<(String, Option<String>)> = selected
            .iter()
            .map(|s| (s.provider_id.clone(), s.protocol.clone()))
            .collect();
        db::replace_bindings(&conn, &agent_id, &rows, &default_provider_id)?;
    }
    Ok(())
}

#[tauri::command]
pub async fn agent_apply_provider_selection(
    state: State<'_, crate::AppState>,
    agent_id: String,
    selected: Vec<ProviderSelection>,
    default_provider_id: String,
) -> AppResult<()> {
    let parts = snapshot_state(&state);
    let refresh_id = agent_id.clone();
    run_blocking(move || do_apply_provider_selection(&parts, agent_id, selected, default_provider_id)).await?;
    // A binding change moves the router's candidate list (and with it the
    // steady-state model a routed alias advertises) — refresh it. In Direct
    // mode this is a no-op (the flag is off).
    super::gateway::refresh_alias_if_routed(&state, &refresh_id).await;
    Ok(())
}

/// Clear the agent's active provider block (single-shot restore) — keeps
/// other bindings in place for multi-provider agents, but resets whatever was
/// active. For multi-provider agents prefer `agent_apply_provider_selection`
/// with an empty list; this command is kept for the single-slot "Factory
/// config" radio row.
#[tauri::command]
pub async fn agent_clear_provider(state: State<'_, crate::AppState>, agent_id: String) -> AppResult<()> {
    let parts = snapshot_state(&state);
    run_blocking(move || {
        let spec = crate::agents::agent_spec(&agent_id)
            .ok_or_else(|| AppError::NotFound(format!("unknown agent '{agent_id}'")))?;
        let adapter = agents::adapter_for(spec.config.writer)
            .ok_or_else(|| AppError::NotFound(format!("no config adapter for '{agent_id}'")))?;
        let path = crate::db::home_dir()?.join(spec.config.relative_path);
        adapter.restore(&path)?;
        let conn = parts.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        let _ = db::clear_active_binding(&conn, &agent_id)?;
        Ok(())
    })
    .await
}

/// Read the agent's current config file (the live file on disk, not any
/// Nestra snapshot) and return its raw contents so the user can preview what
/// the agent has configured today. `path` is the resolved config path;
/// `content` is `None` when no file exists yet (common for just-installed
/// agents). `detected` lists provider entries the user configured directly in
/// the agent's config file (invisible to the binding table).
#[derive(Serialize)]
pub struct AgentConfigContent {
    pub path: Option<String>,
    pub content: Option<String>,
    pub detected: Vec<crate::config_writer::DetectedProvider>,
}

#[tauri::command]
pub async fn agent_read_config(agent_id: String) -> AppResult<AgentConfigContent> {
    run_blocking(move || {
        let spec = crate::agents::agent_spec(&agent_id)
            .ok_or_else(|| AppError::NotFound(format!("unknown agent '{agent_id}'")))?;
        let path = crate::db::home_dir()?.join(spec.config.relative_path);
        let content = if path.is_file() {
            Some(std::fs::read_to_string(&path)?)
        } else {
            None
        };
        let detected = agents::adapter_for(spec.config.writer)
            .map(|a| a.inspect(&path))
            .transpose()?
            .unwrap_or_default();
        Ok(AgentConfigContent {
            path: path.to_str().map(String::from),
            content,
            detected,
        })
    })
    .await
}

/// Remove a single provider entry from the agent's config file (one the user
/// configured directly, not a Nestra-managed `nestra-*` key). Returns the
/// refreshed detected list so the UI can re-render without a full re-read.
#[tauri::command]
pub async fn agent_remove_detected(
    agent_id: String,
    key: String,
) -> AppResult<Vec<crate::config_writer::DetectedProvider>> {
    run_blocking(move || {
        // Trust-boundary guard: never let a caller delete a Nestra-managed
        // `nestra-*` entry through this path — that would desync the file
        // from the binding table. Managed entries are removed via unbind.
        if key.starts_with("nestra-") {
            return Err(AppError::Validation(format!(
                "refusing to remove managed entry '{key}' — use unbind instead"
            )));
        }
        let spec = crate::agents::agent_spec(&agent_id)
            .ok_or_else(|| AppError::NotFound(format!("unknown agent '{agent_id}'")))?;
        let adapter = agents::adapter_for(spec.config.writer)
            .ok_or_else(|| AppError::NotFound(format!("no config adapter for '{agent_id}'")))?;
        let path = crate::db::home_dir()?.join(spec.config.relative_path);
        adapter.remove(&path, &key)?;
        adapter.inspect(&path)
    })
    .await
}

/// Re-probe one row using its current overrides and return the updated
/// `AgentInfo`. Shared by the override commands.
fn reprobe_one(conn: &rusqlite::Connection, agent_id: &str) -> AppResult<AgentInfo> {
    let updated = detect_all_agents(conn)?;
    updated
        .into_iter()
        .find(|r| r.id == agent_id)
        .map(agent_to_info)
        .ok_or_else(|| AppError::NotFound(format!("agent '{agent_id}' not found")))
}

#[tauri::command]
pub async fn agent_set_override(
    state: State<'_, crate::AppState>,
    agent_id: String,
    agent_path: Option<String>,
    config_path: Option<String>,
) -> AppResult<AgentInfo> {
    // Validate the binary path before we persist anything.
    if let Some(p) = &agent_path {
        let path = std::path::Path::new(p);
        if !path.exists() {
            return Err(AppError::Validation(format!(
                "binary path '{p}' does not exist"
            )));
        }
    }
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        db::set_agent_overrides(
            &conn,
            &agent_id,
            agent_path.as_deref(),
            config_path.as_deref(),
        )?;
        reprobe_one(&conn, &agent_id)
    })
    .await
}

#[tauri::command]
pub async fn agent_clear_override(
    state: State<'_, crate::AppState>,
    agent_id: String,
) -> AppResult<AgentInfo> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        db::set_agent_overrides(&conn, &agent_id, None, None)?;
        reprobe_one(&conn, &agent_id)
    })
    .await
}

// ---- Agent enable/disable ----

/// Toggle whether Nestra actively manages this agent's config. Returns the
/// refreshed AgentInfo (with capabilities + providers).
///
/// **Enable**: snapshot the live config as the Factory Configuration (once),
/// then Nestra takes over — every subsequent switch writes the config from
/// the active binding set. The pre-Nestra snapshot is the canonical revert.
///
/// **Disable**: clear every binding Nestra owns (active + multi), restore the
/// Factory Configuration to disk, and wipe the snapshot + agent bookkeeping.
/// After this the user can flip the switch back on to re-take control; the
/// `agent_restore_factory` UI is gone — ON/OFF is the only toggle.
#[tauri::command]
pub async fn agent_set_enabled(
    state: State<'_, crate::AppState>,
    agent_id: String,
    enabled: bool,
) -> AppResult<AgentInfo> {
    let parts = snapshot_state(&state);
    run_blocking(move || do_set_enabled(&parts, &agent_id, enabled)).await
}

fn do_set_enabled(parts: &AppParts, agent_id: &str, enabled: bool) -> AppResult<AgentInfo> {
    let conn = parts.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    let spec = crate::agents::agent_spec(agent_id)
        .ok_or_else(|| AppError::NotFound(format!("unknown agent '{agent_id}'")))?;

    db::set_agent_enabled(&conn, agent_id, enabled)?;

    if enabled {
        // Capture the pre-Nestra snapshot exactly once (first enable wins).
        if !spec.config.relative_path.is_empty() {
            let already = db::list_agents(&conn)?
                .into_iter()
                .find(|c| c.id == agent_id)
                .and_then(|c| c.factory_backup_path)
                .is_some();
            if !already {
                if let Ok(path) = crate::db::home_dir() {
                    let p = path.join(&spec.config.relative_path);
                    let _ = crate::config_writer::capture_factory(&p, false);
                    let fp = crate::config_writer::factory_path_for(&p);
                    if let Some(s) = fp.to_str() {
                        let _ = db::set_agent_factory_path(&conn, agent_id, Some(s));
                    }
                }
            }
        }
    } else {
        // Hand-off: every binding Nestra owns is dropped; the adapter reverts
        // the live config from the factory snapshot; the snapshot itself is
        // removed so a future enable starts fresh.
        if !spec.config.relative_path.is_empty() {
            if let Some(adapter) = agents::adapter_for(spec.config.writer) {
                let path = crate::db::home_dir()?.join(&spec.config.relative_path);
                let _ = adapter.restore(&path);
                for extra in adapter.extra_config_paths(&path) {
                    let _ = adapter.restore(&extra);
                }
            }
        }
        db::clear_all_bindings(&conn, agent_id)?;
        db::set_agent_factory_path(&conn, agent_id, None)?;
        let factory_path = crate::config_writer::factory_path_for(
            &crate::db::home_dir()?.join(&spec.config.relative_path),
        );
        if factory_path.exists() {
            let _ = std::fs::remove_file(factory_path);
        }
    }

    let mut info = reprobe_one(&conn, agent_id)?;
    if let Ok(provs) = db::list_bindings(&conn, agent_id) {
        info.providers = provs.into_iter().map(binding_to_info).collect();
    }
    Ok(info)
}

#[cfg(test)]
mod tests;
