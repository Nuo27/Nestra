use crate::error::{AppError, AppResult};
use crate::{db, secrets};
use serde::Serialize;
use super::run_blocking;
use tauri::State;

// ---- 5h-quota auto refresh settings ----

#[tauri::command]
pub fn quota_refresh_get_settings(
    state: State<'_, crate::AppState>,
) -> AppResult<crate::quota_refresh::RefreshSettings> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    crate::quota_refresh::load_settings(&conn)
}

#[tauri::command]
pub fn quota_refresh_set_settings(
    state: State<'_, crate::AppState>,
    value: crate::quota_refresh::RefreshSettings,
) -> AppResult<()> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    crate::quota_refresh::save_settings(&conn, &value)
}

/// OpenCode Go dashboard credentials — the workspace ID (non-secret) and the
/// `auth` session cookie (secret). Returned to the UI WITHOUT the cookie
/// value: only whether one is set, mirroring how API keys are never surfaced.
#[derive(Debug, Clone, Serialize)]
pub struct OpencodeCredsStatus {
    pub workspace_id: Option<String>,
    pub has_cookie: bool,
}

#[tauri::command]
pub fn opencode_get_creds(
    state: State<'_, crate::AppState>,
    endpoint_id: String,
) -> AppResult<OpencodeCredsStatus> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    let settings = crate::quota_refresh::load_settings(&conn)?;
    let workspace_id = settings
        .endpoints
        .get(&endpoint_id)
        .and_then(|c| c.opencode_workspace_id.clone());
    let has_cookie = secrets::get(&format!("opencode-go-cookie-{endpoint_id}"))?
        .is_some_and(|c| !c.is_empty());
    Ok(OpencodeCredsStatus { workspace_id, has_cookie })
}

/// Store the OpenCode Go dashboard credentials. The cookie is encrypted at
/// rest (`secrets.rs`, AES-256-GCM, key `opencode-go-cookie-{id}`); the
/// workspace ID is non-secret and lives in the settings blob. Changing the
/// credentials clears the verified state so a fetch must re-confirm data
/// before the bars/keep-alive unlock.
#[tauri::command]
pub fn opencode_set_creds(
    state: State<'_, crate::AppState>,
    endpoint_id: String,
    cookie: String,
    workspace_id: String,
) -> AppResult<()> {
    let cookie = cookie.trim();
    if !cookie.is_empty() {
        secrets::set(&format!("opencode-go-cookie-{endpoint_id}"), cookie)?;
    }
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    crate::quota_refresh::update_settings(&conn, |settings| {
        crate::quota_refresh::set_opencode_workspace_id(settings, &endpoint_id, &workspace_id);
    })
}

#[tauri::command]
pub fn quota_keepalive_preview(
    state: State<'_, crate::AppState>,
    endpoint_id: String,
) -> AppResult<crate::quota_refresh::PingPreview> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    let ep = db::get_endpoint(&conn, &endpoint_id)?
        .ok_or_else(|| AppError::Validation(format!("unknown endpoint '{endpoint_id}'")))?;
    let settings = crate::quota_refresh::load_settings(&conn)?;
    let cfg = settings.endpoints.get(&endpoint_id).cloned().unwrap_or_default();
    crate::quota_refresh::build_ping_preview(&ep, &cfg)
}

/// Result of an on-demand keep-alive ping fired by the popover `[test]`
/// link. On failure, `status` carries the full body snippet (no
/// truncation) so the user can see exactly what the server said.
#[derive(Debug, Clone, Serialize)]
pub struct PingNowResult {
    pub ok: bool,
    pub status: String,
}

#[tauri::command]
pub async fn quota_ping_now(
    state: State<'_, crate::AppState>,
    endpoint_id: String,
) -> AppResult<PingNowResult> {
    let db = state.db.clone();
    run_blocking(move || {
        let (ep, cfg) = {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            let ep = db::get_endpoint(&conn, &endpoint_id)?
                .ok_or_else(|| AppError::Validation(format!("unknown endpoint '{endpoint_id}'")))?;
            let settings = crate::quota_refresh::load_settings(&conn)?;
            let cfg = settings
                .endpoints
                .get(&endpoint_id)
                .cloned()
                .unwrap_or_default();
            (ep, cfg)
        };
        let key = match secrets::get(&endpoint_id) {
            Ok(Some(k)) if !k.is_empty() => k,
            _ => {
                return Ok(PingNowResult {
                    ok: false,
                    status: "no API key configured".into(),
                })
            }
        };
        crate::quota_refresh::update_keepalive(&endpoint_id, |s| {
            s.phase = crate::quota_refresh::KeepAlivePhase::Pinging
        });
        let res = crate::quota_refresh::try_ping(&ep, &cfg, &key);
        crate::quota_refresh::record_ping_outcome(&endpoint_id, &res);
        let status = match &res {
            Ok(()) => "ok".to_string(),
            Err(f) if f.transient => format!("retrying: {}", f.message),
            Err(f) => format!("error: {}", f.message),
        };
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        let _ = crate::quota_refresh::set_status_public(&conn, &endpoint_id, &status);
        Ok(PingNowResult {
            ok: res.is_ok(),
            status,
        })
    })
    .await
}

/// Runtime keep-alive state for one endpoint (phase, last success, next
/// fire, error, attempts). Read-only and lock-free — in-memory only, so it
/// needs no DB connection.
#[tauri::command]
pub fn quota_keepalive_status(
    endpoint_id: String,
) -> crate::quota_refresh::KeepAliveState {
    crate::quota_refresh::keepalive_state(&endpoint_id)
}
