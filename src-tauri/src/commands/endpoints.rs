use crate::error::{AppError, AppResult};
use crate::db::EndpointRow;
use crate::{db, secrets};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use super::common::{validate_base_url, validate_id, validate_protocol, validate_protocol_kind};
use super::run_blocking;
use tauri::State;

// =============================================================
// Provider/CLI split surface.
// Provider = LLM endpoint + key. CLI = detected binary. Switching writes
// the CLI's global config via config_writer.
// =============================================================



#[derive(Serialize)]
pub struct ValidationResult {
    pub ok: bool,
    pub error_code: Option<String>,
    pub message: Option<String>,
}

#[derive(Serialize)]
pub struct EndpointInfo {
    pub id: String,
    pub display_name: String,
    pub has_api_key: bool,
    pub status: String,
    pub models: Option<serde_json::Value>,
    pub advanced_env: Option<serde_json::Value>,
    /// User-saved per-model ability overrides, keyed by model id. Empty
    /// object when the user has never edited anything. The UI treats a
    /// missing entry here as "use whatever models.dev says" — see
    /// `model_abilities_defaults` for the upstream-source-of-truth.
    #[serde(default)]
    pub model_abilities: HashMap<String, crate::model_abilities::ModelAbilities>,
    /// models.dev-derived defaults for this endpoint's selected models.
    /// Surfaced to the UI so the Capabilities disclosure can pre-populate
    /// from a known-good baseline. Empty when the cache is cold (the
    /// Capabilities disclosure falls back to "no default").
    #[serde(default)]
    pub model_abilities_defaults: HashMap<String, crate::model_abilities::ModelAbilities>,
    pub last_validated_at: Option<i64>,
    pub models_fetched_at: Option<i64>,
    pub protocols: Vec<ProtocolInfo>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ProtocolInfo {
    pub protocol: String,
    pub base_url: String,
}

fn endpoint_to_info(row: EndpointRow) -> EndpointInfo {
    let models = row
        .models_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    let advanced_env = row
        .advanced_env_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    let model_abilities = crate::model_abilities::parse_overrides(
        row.model_abilities_json.as_deref(),
    );
    // Defaults live in a separate helper so callers that already hold the DB
    // lock can pass them in without re-opening the connection.
    EndpointInfo {
        id: row.id,
        display_name: row.display_name,
        has_api_key: row.has_api_key,
        status: row.status,
        models,
        advanced_env,
        model_abilities,
        model_abilities_defaults: HashMap::new(),
        last_validated_at: row.last_validated_at,
        models_fetched_at: row.models_fetched_at,
        protocols: row
            .protocols
            .into_iter()
            .map(|p| ProtocolInfo {
                protocol: p.protocol,
                base_url: p.base_url,
            })
            .collect(),
    }
}

/// Compute the models.dev-derived defaults for an endpoint's selected models.
/// Uses the existing models.dev cache — no network. Empty when the cache is
/// cold or the endpoint has no models configured. Vendor-authoritative
/// corrections (`load_corrections`) layer on top so the UI shows the same
/// value that will actually be written.
fn endpoint_default_abilities(
    conn: &rusqlite::Connection,
    row: &EndpointRow,
) -> HashMap<String, crate::model_abilities::ModelAbilities> {
    let models = row
        .models_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    let ids = collect_model_ids(models.as_ref());
    if ids.is_empty() {
        return HashMap::new();
    }
    let defaults = crate::model_abilities::subset_for(
        &crate::model_abilities::load_index(conn).unwrap_or_default(),
        &ids,
    );
    // Subset the corrections to the same id set, then layer corrections on
    // top of defaults — corrections win over models.dev, user overrides (in
    // the disclosure) layer on top of the result via `resolveField`.
    let corrections = crate::model_abilities::load_corrections();
    let correction_subset = crate::model_abilities::subset_for(&corrections, &ids);
    crate::model_abilities::merge_into(defaults, correction_subset)
}

/// Flatten the endpoint's stored model list to the distinct ids the UI
/// exposes (default + tier + available). Same shape as `ModelsConfig::ids()`
/// but parses from the JSON value the `endpoint_to_info` caller already has
/// rather than re-walking the typed enum.
fn collect_model_ids(models: Option<&serde_json::Value>) -> Vec<String> {
    let Some(m) = models else {
        return Vec::new();
    };
    let mut ids: Vec<String> = Vec::new();
    let push = |ids: &mut Vec<String>, id: &str| {
        if !id.is_empty() && !ids.iter().any(|e| e == id) {
            ids.push(id.to_string());
        }
    };
    for k in ["default", "haiku", "sonnet", "opus"] {
        if let Some(s) = m.get(k).and_then(|v| v.as_str()) {
            push(&mut ids, s);
        }
    }
    if let Some(arr) = m.get("available").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str() {
                push(&mut ids, s);
            }
        }
    }
    ids
}

// ---- Provider (endpoint) commands ----

#[tauri::command]
pub fn endpoint_list(state: State<'_, crate::AppState>) -> AppResult<Vec<EndpointInfo>> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    let rows = db::list_endpoints(&conn)?;
    let defaults = crate::model_abilities::load_index(&conn).unwrap_or_default();
    let corrections = crate::model_abilities::load_corrections();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut info = endpoint_to_info(row);
        let ids = collect_model_ids(info.models.as_ref());
        let base = crate::model_abilities::subset_for(&defaults, &ids);
        let correction_subset = crate::model_abilities::subset_for(&corrections, &ids);
        info.model_abilities_defaults =
            crate::model_abilities::merge_into(base, correction_subset);
        out.push(info);
    }
    Ok(out)
}

#[tauri::command]
pub fn endpoint_get(state: State<'_, crate::AppState>, id: String) -> AppResult<EndpointInfo> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    let row = db::get_endpoint(&conn, &id)?
        .ok_or_else(|| AppError::NotFound(format!("endpoint '{id}' not found")))?;
    let mut info = endpoint_to_info(row.clone());
    info.model_abilities_defaults = endpoint_default_abilities(&conn, &row);
    Ok(info)
}

#[tauri::command]
pub fn endpoint_create(
    state: State<'_, crate::AppState>,
    id: String,
    display_name: String,
) -> AppResult<EndpointInfo> {
    validate_id(&id)?;
    let name = display_name.trim();
    if name.is_empty() {
        return Err(AppError::Validation("display_name cannot be empty".into()));
    }
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    match db::create_endpoint(&conn, &id, "custom", name) {
        Ok(()) => {}
        Err(AppError::Db(rusqlite::Error::SqliteFailure(err, _)))
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            return Err(AppError::Conflict(format!("endpoint '{id}' already exists")));
        }
        Err(e) => return Err(e),
    }
    let row = db::get_endpoint(&conn, &id)?
        .ok_or_else(|| AppError::Internal("endpoint disappeared after insert".into()))?;
    Ok(endpoint_to_info(row))
}

#[tauri::command]
pub fn endpoint_delete(
    state: State<'_, crate::AppState>,
    id: String,
) -> AppResult<()> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    let _existed = db::delete_endpoint(&conn, &id)?;
    drop(conn);
    let _ = secrets::delete(&id);
    Ok(())
}

#[tauri::command]
pub fn endpoint_add_protocol(
    state: State<'_, crate::AppState>,
    id: String,
    protocol: String,
    base_url: String,
) -> AppResult<EndpointInfo> {
    validate_id(&id)?;
    validate_protocol_kind(&protocol)?;
    let cleaned = validate_base_url(&base_url)?;
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    if db::get_endpoint(&conn, &id)?.is_none() {
        return Err(AppError::NotFound(format!("endpoint '{id}' not found")));
    }
    db::upsert_endpoint_protocol(&conn, &id, &protocol, &cleaned)?;
    let row = db::get_endpoint(&conn, &id)?
        .ok_or_else(|| AppError::Internal("endpoint disappeared".into()))?;
    Ok(endpoint_to_info(row))
}

#[tauri::command]
pub fn endpoint_remove_protocol(
    state: State<'_, crate::AppState>,
    id: String,
    protocol: String,
) -> AppResult<EndpointInfo> {
    validate_id(&id)?;
    validate_protocol(&protocol)?;
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    if db::get_endpoint(&conn, &id)?.is_none() {
        return Err(AppError::NotFound(format!("endpoint '{id}' not found")));
    }
    db::delete_endpoint_protocol(&conn, &id, &protocol)?;
    let row = db::get_endpoint(&conn, &id)?
        .ok_or_else(|| AppError::Internal("endpoint disappeared".into()))?;
    Ok(endpoint_to_info(row))
}

#[tauri::command]
pub fn endpoint_set_name(
    state: State<'_, crate::AppState>,
    id: String,
    display_name: String,
) -> AppResult<()> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    db::set_endpoint_name(&conn, &id, &display_name)
}

#[tauri::command]
pub fn endpoint_set_models(
    state: State<'_, crate::AppState>,
    id: String,
    models: serde_json::Value,
) -> AppResult<()> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    db::set_endpoint_models(&conn, &id, &serde_json::to_string(&models)?)?;
    // Keep the routing catalog fresh: a default-model change must be visible
    // to the next gateway request. Best-effort — a rebuild failure never
    // fails the save.
    let _ = crate::orchestration::capability_registry::rebuild_endpoint(&conn, &id);
    Ok(())
}

#[tauri::command]
pub fn endpoint_set_advanced_env(
    state: State<'_, crate::AppState>,
    id: String,
    env: serde_json::Value,
) -> AppResult<()> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    db::set_endpoint_advanced_env(&conn, &id, &serde_json::to_string(&env)?)
}

/// Replace the per-endpoint model-ability overrides. `abilities` is the full
/// authoritative map the user wants stored (after applying every Reset
/// from the UI) — partial diffs are computed on the frontend. Pass `{}` to
/// clear every override. The map is persisted as a JSON object keyed by
/// model id; the OpenCode writer reads it via `build_switch_context`.
#[tauri::command]
pub fn endpoint_set_model_abilities(
    state: State<'_, crate::AppState>,
    id: String,
    abilities: HashMap<String, crate::model_abilities::ModelAbilities>,
) -> AppResult<()> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    if db::get_endpoint(&conn, &id)?.is_none() {
        return Err(AppError::NotFound(format!("endpoint '{id}' not found")));
    }
    let json = serde_json::to_string(&abilities)?;
    db::set_endpoint_model_abilities(&conn, &id, Some(&json))
}

#[tauri::command]
pub async fn endpoint_set_api_key(
    state: State<'_, crate::AppState>,
    id: String,
    key: String,
) -> AppResult<ValidationResult> {
    if key.trim().is_empty() {
        return Err(AppError::Validation("API key cannot be empty".into()));
    }
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        let endpoint = db::get_endpoint(&conn, &id)?
            .ok_or_else(|| AppError::NotFound(format!("endpoint '{id}' not found")))?;
        drop(conn);

        if endpoint.protocols.is_empty() {
            return Err(AppError::Validation(
                "no protocol endpoint configured — add one on the detail page".into(),
            ));
        }

        // Validate against every protocol row (shared with create-with-preset).
        let (primary_protocol, all_models) =
            validate_key_against_protocols(&id, &endpoint.protocols, &key)?;
        let cached_str = build_models_json(all_models)?;

        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        secrets::set(&id, &key)?;
        db::set_endpoint_models(&conn, &id, &cached_str)?;
        db::mark_endpoint_key(&conn, &id, true, "valid")?;
        // Opportunistic models.dev ability-cache refresh (best-effort).
        let _ = crate::model_abilities::refresh_if_stale(&conn);
        tracing::info!(
            endpoint = %id, protocol = %primary_protocol,
            "api key set + validated"
        );
        Ok(ValidationResult { ok: true, error_code: None, message: None })
    })
    .await
}

#[tauri::command]
pub fn endpoint_clear_api_key(
    state: State<'_, crate::AppState>,
    id: String,
) -> AppResult<()> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    if db::get_endpoint(&conn, &id)?.is_none() {
        return Err(AppError::NotFound(format!("endpoint '{id}' not found")));
    }
    db::mark_endpoint_key(&conn, &id, false, "unvalidated")?;
    drop(conn);
    secrets::delete(&id)?;
    Ok(())
}

/// Result of a single-step create-with-preset: the new endpoint id + the
/// key-validation outcome. `validation.ok = false` means the endpoint was
/// created (with protocols) but the key was rejected — the frontend routes
/// the user to the edit page to fix it. The endpoint always exists on
/// return; we never roll back the create, only the key.
#[derive(Serialize)]
pub struct CreateWithPresetResult {
    pub id: String,
    pub validation: ValidationResult,
}

/// Create an endpoint from a preset, add every preset protocol, and validate
/// the supplied API key in one atomic flow. The common case ("pick a preset,
/// paste a key") no longer requires a visit to the edit page. The protocols
/// are always persisted; on key-validation failure the endpoint is left in
/// `unvalidated` state so the edit page can pick up where this left off.
///
/// Each protocol pair is validated + URL-cleaned exactly as
/// `endpoint_add_protocol` would; a malformed protocol in the preset is
/// skipped (best-effort) rather than failing the whole create.
#[tauri::command]
pub async fn endpoint_create_with_preset(
    state: State<'_, crate::AppState>,
    id: String,
    display_name: String,
    protocols: Vec<ProtocolInfo>,
    api_key: String,
    quota_query: Option<crate::endpoint_quota::BuiltinKind>,
) -> AppResult<CreateWithPresetResult> {
    validate_id(&id)?;
    let name = display_name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::Validation("display_name cannot be empty".into()));
    }
    if api_key.trim().is_empty() {
        return Err(AppError::Validation("API key cannot be empty".into()));
    }
    let db = state.db.clone();
    run_blocking(move || {
        // 1. Create the endpoint row.
        {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            match db::create_endpoint(&conn, &id, "custom", &name) {
                Ok(()) => {}
                Err(AppError::Db(rusqlite::Error::SqliteFailure(err, _)))
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    return Err(AppError::Conflict(format!(
                        "endpoint '{id}' already exists"
                    )));
                }
                Err(e) => return Err(e),
            }
        }

        // 2. Add every preset protocol (best-effort: skip malformed ones).
        {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            for p in &protocols {
                if validate_protocol_kind(&p.protocol).is_err() {
                    tracing::warn!(
                        endpoint = %id, protocol = %p.protocol,
                        "preset protocol rejected, skipping"
                    );
                    continue;
                }
                let cleaned = match validate_base_url(&p.base_url) {
                    Ok(u) => u,
                    Err(e) => {
                        tracing::warn!(
                            endpoint = %id, protocol = %p.protocol, error = %e,
                            "preset base_url rejected, skipping"
                        );
                        continue;
                    }
                };
                db::upsert_endpoint_protocol(&conn, &id, &p.protocol, &cleaned)?;
            }
            let row = db::get_endpoint(&conn, &id)?
                .ok_or_else(|| AppError::Internal("endpoint disappeared after insert".into()))?;
            if row.protocols.is_empty() {
                // No protocols survived validation — leave the endpoint in
                // its unvalidated state and let the user finish on the edit
                // page. Surface a clear error rather than silently persisting
                // a dead endpoint + key.
                return Ok(CreateWithPresetResult {
                    id,
                    validation: ValidationResult {
                        ok: false,
                        error_code: Some("no_protocols".into()),
                        message: Some(
                            "preset had no usable protocols — finish setup on the edit page".into(),
                        ),
                    },
                });
            }
        }

        // 2b. Stamp the inherited query plan (preset-borne built-in) so the
        // Quota page + keep-alive are queryable without extra configuration.
        // Best-effort: a settings-blob hiccup is logged, not fatal — the
        // endpoint is still created and `resolve_plan` will backfill via
        // host detection if the stamp is missing.
        if let Some(kind) = quota_query {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            let ep_id = id.clone();
            if let Err(e) = crate::quota_refresh::update_settings(&conn, |settings| {
                let entry = settings.endpoints.entry(ep_id).or_default();
                entry.query_plan =
                    Some(crate::endpoint_quota::QuotaQueryPlan::Preset { kind });
            }) {
                tracing::warn!(endpoint = %id, error = %e, "failed to stamp inherited query plan");
            }
        }

        // 3. Validate the key against the freshly-added protocols.
        let endpoint = {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            db::get_endpoint(&conn, &id)?
                .ok_or_else(|| AppError::Internal("endpoint disappeared before validation".into()))?
        };
        match validate_key_against_protocols(&id, &endpoint.protocols, &api_key.trim()) {
            Ok((primary_protocol, models)) => {
                let cached_str = build_models_json(models)?;
                let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
                secrets::set(&id, &api_key)?;
                db::set_endpoint_models(&conn, &id, &cached_str)?;
                db::mark_endpoint_key(&conn, &id, true, "valid")?;
                let _ = crate::model_abilities::refresh_if_stale(&conn);
                tracing::info!(
                    endpoint = %id, protocol = %primary_protocol,
                    "preset endpoint created + key validated"
                );
                Ok(CreateWithPresetResult {
                    id,
                    validation: ValidationResult { ok: true, error_code: None, message: None },
                })
            }
            Err(e) => {
                // Endpoint + protocols are persisted; only the key failed.
                // Surface the validation error so the frontend can route to
                // the edit page to fix it.
                tracing::info!(endpoint = %id, error = %e, "preset create: key validation failed");
                Ok(CreateWithPresetResult {
                    id,
                    validation: ValidationResult {
                        ok: false,
                        error_code: Some("validation_failed".into()),
                        message: Some(e.to_string()),
                    },
                })
            }
        }
    })
    .await
}

/// Try each `(protocol, base_url)` pair against the upstream `/models`
/// endpoint until one validates the key. First success wins. Returns the
/// validated model-id list (deduped + sorted) and the protocol that
/// succeeded. On total failure, surfaces the last protocol's error.
///
/// Shared by `endpoint_set_api_key` (edit-page save) and
/// `endpoint_create_with_preset` (single-step create+validate). Pure — does
/// not touch the DB or keychain; the caller persists on success.
fn validate_key_against_protocols(
    endpoint_id: &str,
    protocols: &[crate::db::ProtocolEntry],
    key: &str,
) -> AppResult<(String, Vec<String>)> {
    let mut all_models: Vec<String> = Vec::new();
    let mut primary_protocol = String::new();
    for proto in protocols {
        match fetch_models_http(&proto.protocol, &proto.base_url, key) {
            Ok(mut models) => {
                primary_protocol = proto.protocol.clone();
                all_models.append(&mut models);
                break;
            }
            Err(e) => {
                tracing::info!(
                    endpoint = %endpoint_id, protocol = %proto.protocol, error = %e,
                    "protocol validation failed, trying next"
                );
            }
        }
    }
    if primary_protocol.is_empty() {
        // All protocols failed — re-try the openai-style one last for a
        // final HTTP error to surface to the user.
        let last = protocols
            .iter()
            .find(|p| p.protocol == "openai-comp" || p.protocol == "custom")
            .or_else(|| protocols.first())
            .ok_or_else(|| AppError::Validation("no protocol endpoint configured — add one on the detail page".into()))?;
        return Err(AppError::Validation(format!(
            "all protocol endpoints failed — last tried '{}': {}",
            last.protocol,
            fetch_models_http(&last.protocol, &last.base_url, key)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        )));
    }
    // The model list may be anonymous (opencode-go's /v1/models serves
    // everyone), so a successful fetch proves nothing about the key. When a
    // garbage key also fetches the list, run a real auth probe (one minimal
    // chat completion) so "key validated" is honest instead of a no-op.
    let probe_proto = protocols
        .iter()
        .find(|p| p.protocol == primary_protocol)
        .ok_or_else(|| AppError::Validation("validated protocol row missing".into()))?;
    let garbage_key = "sk-nestra-invalid-probe";
    if fetch_models_http(&probe_proto.protocol, &probe_proto.base_url, garbage_key).is_ok() {
        let probe_model = all_models.first().cloned().unwrap_or_default();
        probe_auth_http(&probe_proto.protocol, &probe_proto.base_url, key, &probe_model)?;
    }
    all_models.sort();
    all_models.dedup();
    Ok((primary_protocol, all_models))
}

/// POST a minimal chat completion to prove the key is accepted. Only used
/// when the provider's model list is anonymous (the list alone can't verify
/// a key). A 2xx passes; a 401/403 fails with the upstream's own message;
/// any other status is a warning, not a failure — the key may be fine while
/// the probe model/shape is rejected.
fn probe_auth_http(protocol: &str, base_url: &str, key: &str, model: &str) -> AppResult<()> {
    let kind = if protocol == "anthropic" {
        crate::config_writer::ProviderKind::Anthropic
    } else {
        crate::config_writer::ProviderKind::Openai
    };
    let url = crate::protocol_url::join_protocol_path(base_url, kind);
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
        "stream": false,
    });
    let req = ureq::post(&url);
    let req = match protocol {
        "anthropic" => req
            .set("x-api-key", key)
            .set("anthropic-version", "2023-06-01"),
        _ => req.set("Authorization", &format!("Bearer {key}")),
    };
    match req.send_json(body) {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(401 | 403, resp)) => {
            let msg = resp.into_string().unwrap_or_default();
            Err(AppError::Validation(format!(
                "API key rejected by upstream ({url}): {msg}"
            )))
        }
        Err(ureq::Error::Status(code, _)) => {
            tracing::warn!("auth probe status {code} for {url} — key left unverified");
            Ok(())
        }
        Err(e) => {
            tracing::warn!("auth probe failed: {e}");
            Ok(())
        }
    }
}

/// Build the canonical `models_json` payload from a validated model list.
/// The first id becomes the default; haiku/sonnet/opus tiers are left empty
/// for the edit page to fill in.
fn build_models_json(all_models: Vec<String>) -> AppResult<String> {
    let default_model = all_models.first().cloned().unwrap_or_default();
    let cached = serde_json::json!({
        "default": default_model,
        "haiku": "",
        "sonnet": "",
        "opus": "",
        "available": all_models,
    });
    Ok(serde_json::to_string(&cached)?)
}

/// GET the provider's model list. Iterates protocol rows, returns the union
/// of all model ids.
fn fetch_models_http(protocol: &str, base_url: &str, key: &str) -> AppResult<Vec<String>> {
    let kind = if protocol == "anthropic" {
        crate::config_writer::ProviderKind::Anthropic
    } else {
        crate::config_writer::ProviderKind::Openai
    };
    let url = crate::protocol_url::join_models_path(base_url, kind);
    let req = ureq::get(&url);
    let req = match protocol {
        "anthropic" => req.set("x-api-key", key).set("anthropic-version", "2023-06-01"),
        _ => req.set("Authorization", &format!("Bearer {key}")),
    };
    let resp = req
        .call()
        .map_err(|e| AppError::Http(format!("models fetch failed: {e}")))?;
    let value: serde_json::Value = resp
        .into_json()
        .map_err(|e| AppError::Http(format!("models parse failed: {e}")))?;
    let ids = value
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    m.get("id")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(ids)
}

#[tauri::command]
pub async fn endpoint_fetch_models(
    state: State<'_, crate::AppState>,
    id: String,
) -> AppResult<Vec<String>> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        let endpoint = db::get_endpoint(&conn, &id)?
            .ok_or_else(|| AppError::NotFound(format!("endpoint '{id}' not found")))?;
        drop(conn);
        let key = secrets::get(&id)?.unwrap_or_default();
        let mut all: Vec<String> = Vec::new();
        for proto in &endpoint.protocols {
            if let Ok(mut ids) = fetch_models_http(&proto.protocol, &proto.base_url, &key) {
                all.append(&mut ids);
            }
        }
        all.sort();
        all.dedup();
        // Opportunistic: refresh the models.dev ability cache while we're
        // already hitting the network. Best-effort — ignore errors so a
        // models.dev outage never blocks the model list.
        if let Ok(conn) = db.lock() {
            let _ = crate::model_abilities::refresh_if_stale(&conn);
        }
        Ok(all)
    })
    .await
}

#[tauri::command]
pub async fn endpoint_fetch_quota(
    state: State<'_, crate::AppState>,
    id: String,
) -> AppResult<crate::endpoint_quota::EndpointQuota> {
    let db = state.db.clone();
    run_blocking(move || {
        // Load endpoint + stored config under one lock, then release before
        // the network fetch.
        let (endpoint, cfg) = {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            let endpoint = db::get_endpoint(&conn, &id)?
                .ok_or_else(|| AppError::NotFound(format!("endpoint '{id}' not found")))?;
            let settings = crate::quota_refresh::load_settings(&conn)?;
            let cfg = settings.endpoints.get(&id).cloned().unwrap_or_default();
            (endpoint, cfg)
        };
        let key = secrets::get(&id)?
            .ok_or_else(|| AppError::Validation("API key not set — open detail page to enter one".into()))?;
        let plan = crate::quota_refresh::resolve_plan(&cfg, &endpoint);
        // OpenCode Go authenticates its dashboard scrape with a session cookie
        // + workspace ID (not the API key). Load them only for that plan; the
        // fetcher surfaces a clear "creds not set" snapshot when absent.
        let opencode = if matches!(
            plan,
            crate::endpoint_quota::QuotaQueryPlan::Preset {
                kind: crate::endpoint_quota::BuiltinKind::OpencodeGo
            }
        ) {
            crate::quota_refresh::load_opencode_creds(&id, &cfg)
        } else {
            None
        };
        // A Preset/Mock plan needs a resolvable quota URL; Custom + OpenCode
        // Go supply their own. Surface a clear validation error before the
        // network. (OpenCode Go endpoints always carry /v1 protocols anyway.)
        if !matches!(
            plan,
            crate::endpoint_quota::QuotaQueryPlan::Custom(_)
                | crate::endpoint_quota::QuotaQueryPlan::Preset {
                    kind: crate::endpoint_quota::BuiltinKind::OpencodeGo
                }
        ) {
            db::pick_quota_url(&endpoint.protocols).ok_or_else(|| AppError::Validation(
                "no protocol endpoint configured — add one on the detail page".into(),
            ))?;
        }
        let quota = crate::endpoint_quota::fetch_with_plan(
            &endpoint,
            &key,
            &plan,
            opencode.as_ref().map(|(c, w)| (c.as_str(), w.as_str())),
        );
        // Provisioning side-effect: any successful fetch (manual refresh,
        // worker tick, UI Verify) stamps provisioned=true so the keep-alive
        // worker is allowed to arm and the quota bars unlock. This is the
        // single chokepoint for "data has been correctly retrieved".
        if quota.ok && !quota.items.is_empty() {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            if let Err(e) = crate::quota_refresh::mark_provisioned_public(&conn, &id) {
                tracing::warn!(endpoint = %id, error = %e, "failed to stamp provisioned");
            }
        }
        Ok(quota)
    })
    .await
}

