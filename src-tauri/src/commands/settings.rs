use crate::error::{AppError, AppResult};
use crate::db;
use tauri::State;

// ---- Settings commands ----

#[tauri::command]
pub fn setting_get(
    state: State<'_, crate::AppState>,
    key: String,
) -> AppResult<Option<serde_json::Value>> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    db::get_setting(&conn, &key)
}

#[tauri::command]
pub fn setting_set(
    state: State<'_, crate::AppState>,
    key: String,
    value: serde_json::Value,
) -> AppResult<()> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    db::set_setting(&conn, &key, &value)
}

/// Delete one `setting_kv` row. Used by the Settings page's "clear models
/// cache" (the frontend already invoked this; the command was missing).
#[tauri::command]
pub fn setting_delete(
    state: State<'_, crate::AppState>,
    key: String,
) -> AppResult<()> {
    let conn = state.db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
    conn.execute("DELETE FROM setting_kv WHERE key = ?1", rusqlite::params![key])?;
    Ok(())
}
