use crate::error::{AppError, AppResult};
use crate::skills::{self, SkillMeta};
use super::run_blocking;
use tauri::State;

// ---- Skills commands ----

#[tauri::command]
pub async fn skills_list(state: State<'_, crate::AppState>) -> AppResult<Vec<SkillMeta>> {
    let db = state.db.clone();
    run_blocking(move || {
        // DB read only under the lock, then drop it before the disk walk so a
        // slow skills dir never stalls other DB-backed commands.
        let managed = {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            skills::list_managed(&conn)?
        };
        skills::merge_unmanaged(managed)
    })
    .await
}

#[tauri::command]
pub fn skills_reveal(path: String) -> AppResult<()> {
    skills::reveal(&path)
}

#[tauri::command]
pub async fn skills_install(
    state: State<'_, crate::AppState>,
    source: String,
    agent_ids: Vec<String>,
) -> AppResult<SkillMeta> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        skills::install(&conn, &source, &agent_ids)
    })
    .await
}

#[tauri::command]
pub async fn skills_uninstall(state: State<'_, crate::AppState>, id: String) -> AppResult<()> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        skills::uninstall(&conn, &id)
    })
    .await
}

#[tauri::command]
pub async fn skills_toggle(
    state: State<'_, crate::AppState>,
    id: String,
    agent_id: String,
    enabled: bool,
) -> AppResult<SkillMeta> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        skills::toggle(&conn, &id, &agent_id, enabled)
    })
    .await
}

#[tauri::command]
pub async fn skills_import_scan(
    state: State<'_, crate::AppState>,
) -> AppResult<Vec<skills::UnmanagedSkill>> {
    let db = state.db.clone();
    run_blocking(move || {
        let managed = {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            skills::managed_ids(&conn)?
        };
        skills::import_scan_unmanaged(&managed)
    })
    .await
}

#[tauri::command]
pub async fn skills_import_one(
    state: State<'_, crate::AppState>,
    path: String,
    agent_id: String,
) -> AppResult<SkillMeta> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        skills::import_one(&conn, &path, &agent_id)
    })
    .await
}

/// Stop managing a skill: drop the DB row + SSOT but leave the agent-dir
/// copies (the Skills page's "restore" button). Contrast `skills_uninstall`,
/// which also removes the agent-dir copies.
#[tauri::command]
pub async fn skills_unmanage(
    state: State<'_, crate::AppState>,
    id: String,
) -> AppResult<()> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        skills::unmanage(&conn, &id)
    })
    .await
}
