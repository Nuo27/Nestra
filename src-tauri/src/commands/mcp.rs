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
    // sync_all phases its own lock scopes (short snapshot lock, lock-free
    // file IO, short prune lock) — do NOT hold one lock across it all.
    run_blocking(move || mcp::sync_all(&db)).await
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

/// Per-server tool-usage summary (P1-1). `total_calls` counts gateway-
/// OBSERVED invocations attributed to this managed server (via the shared
/// `mcp__<server>__<tool>` namespace); zero means none were observed —
/// attribution currently covers that namespace only.
#[derive(Debug, serde::Serialize)]
pub struct McpUsageStat {
    pub server_id: String,
    pub server_name: String,
    pub total_calls: u64,
    pub last_used_at: Option<i64>,
    pub per_tool: std::collections::BTreeMap<String, u64>,
}

/// Aggregate gateway-observed tool invocations per managed MCP server.
/// Read-only (`db_read`), full scan over `tool_names`-bearing route_request
/// rows — correctness first; the vast majority of rows carry NULL and are
/// skipped cheaply.
// ponytail: full-scan per page open; if this ever measures slow on very large
// route_request tables, add an aggregate cache keyed off started_at.
/// Pure aggregation over a connection (unit-testable; the command wraps it
/// with the read connection).
fn aggregate_usage(conn: &rusqlite::Connection) -> AppResult<Vec<McpUsageStat>> {
    {
    // Managed servers (the SSOT the sync writes into agent configs).
    let mut stmt = conn
        .prepare("SELECT id, name FROM mcp_server")
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let managed: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| AppError::Internal(e.to_string()))?
        .collect::<rusqlite::Result<_>>()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    drop(stmt);

    let mut stats: std::collections::BTreeMap<String, McpUsageStat> = managed
        .into_iter()
        .map(|(server_id, server_name)| {
            (
                server_id.clone(),
                McpUsageStat {
                    server_id,
                    server_name,
                    total_calls: 0,
                    last_used_at: None,
                    per_tool: std::collections::BTreeMap::new(),
                },
            )
        })
        .collect();
    let name_to_id: std::collections::HashMap<String, String> = stats
        .iter()
        .map(|(id, s)| (s.server_name.clone(), id.clone()))
        .collect();

    let mut stmt = conn
        .prepare(
            "SELECT tool_names, started_at FROM route_request
             WHERE tool_names IS NOT NULL",
        )
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
            ))
        })
        .map_err(|e| AppError::Internal(e.to_string()))?;
    for row in rows {
        let (json, started_at) = row.map_err(|e| AppError::Internal(e.to_string()))?;
        let Ok(map) = serde_json::from_str::<std::collections::BTreeMap<String, u64>>(&json)
        else {
            continue;
        };
        for (tool, count) in map {
            // Attribution rule = the shared namespace parser. Tools that
            // don't parse, or whose server segment isn't a managed server,
            // stay unattributed — never guessed into a server.
            let Some(prov) = crate::session::parse_mcp_tool_name(&tool) else {
                continue;
            };
            let Some(server) = prov.server.and_then(|s| name_to_id.get(&s).cloned()) else {
                continue;
            };
            let stat = stats.get_mut(&server).expect("managed id exists");
            stat.total_calls += count;
            let tool_name = prov.tool_name.unwrap_or_else(|| tool.clone());
            *stat.per_tool.entry(tool_name).or_insert(0) += count;
            if let Some(t) = started_at {
                if stat.last_used_at.map_or(true, |l| t > l) {
                    stat.last_used_at = Some(t);
                }
            }
        }
    }
    Ok(stats.into_values().collect())
    }
}

#[tauri::command]
pub async fn mcp_usage_stats(
    state: State<'_, crate::AppState>,
) -> AppResult<Vec<McpUsageStat>> {
    let db = state.db_read.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        aggregate_usage(&conn)
    })
    .await
}

#[cfg(test)]
mod tests;
