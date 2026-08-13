use crate::error::{AppError, AppResult};
use crate::mcp;
use super::run_blocking;
use tauri::State;

// ---- MCP commands ----

#[tauri::command]
pub async fn mcp_list(state: State<'_, crate::AppState>) -> AppResult<Vec<mcp::McpServer>> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        mcp::list(&conn)
    })
    .await
}

#[tauri::command]
pub async fn mcp_save(
    state: State<'_, crate::AppState>,
    server: mcp::McpServer,
) -> AppResult<mcp::McpServer> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        mcp::save(&conn, &server)
    })
    .await
}

#[tauri::command]
pub async fn mcp_set_state(
    state: State<'_, crate::AppState>,
    id: String,
    agent_id: String,
    mcp_state: mcp::AgentMcpState,
) -> AppResult<mcp::McpServer> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        mcp::set_state(&conn, &id, &agent_id, mcp_state)
    })
    .await
}

#[tauri::command]
pub async fn mcp_delete(state: State<'_, crate::AppState>, id: String) -> AppResult<()> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        mcp::delete(&conn, &id)
    })
    .await
}

/// Stop managing an MCP server: drop the DB row but leave the entries already
/// written into agent config files (the MCP page's "restore" button). Contrast
/// `mcp_delete`, which also strips the entries from every agent config.
#[tauri::command]
pub async fn mcp_unmanage(state: State<'_, crate::AppState>, id: String) -> AppResult<()> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        mcp::unmanage(&conn, &id)
    })
    .await
}

#[tauri::command]
pub async fn mcp_sync_all(state: State<'_, crate::AppState>) -> AppResult<()> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        mcp::sync_all(&conn)
    })
    .await
}

#[tauri::command]
pub async fn mcp_import_scan(
    state: State<'_, crate::AppState>,
) -> AppResult<Vec<mcp::ImportCandidate>> {
    let db = state.db.clone();
    run_blocking(move || {
        let managed: std::collections::HashMap<String, std::collections::HashSet<String>> = {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            mcp::list(&conn)?
                .into_iter()
                .map(|s| {
                    let agents = s
                        .enabled_agents
                        .iter()
                        .chain(s.disabled_agents.iter())
                        .cloned()
                        .collect();
                    (s.id, agents)
                })
                .collect()
        };
        mcp::import_scan_unmanaged(&managed)
    })
    .await
}

#[tauri::command]
pub async fn mcp_import_all(state: State<'_, crate::AppState>) -> AppResult<Vec<mcp::McpServer>> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        mcp::import_all(&conn)
    })
    .await
}

#[tauri::command]
pub async fn mcp_import_one(
    state: State<'_, crate::AppState>,
    agent_id: String,
    name: String,
) -> AppResult<mcp::McpServer> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        mcp::import_one(&conn, &agent_id, &name)
    })
    .await
}

#[tauri::command]
pub async fn mcp_sync_agent(
    state: State<'_, crate::AppState>,
    agent_id: String,
) -> AppResult<()> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        mcp::sync_agent(&conn, &agent_id)
    })
    .await
}

#[tauri::command]
pub async fn mcp_probe(
    state: State<'_, crate::AppState>,
    id: String,
) -> AppResult<mcp::ProbeResult> {
    let db = state.db.clone();
    run_blocking(move || {
        let transport = {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            mcp::probe::fetch_transport(&conn, &id)?
        };
        // Drop the DB lock before the (up to 8s) probe so a hung server can't
        // stall every other DB-backed command.
        mcp::probe_transport(&transport)
    })
    .await
}
