use crate::error::{AppError, AppResult};
use crate::session::provider::{build_resume_command, default_provider_registry};
use crate::session::{store, MessageWindow, Session};
use serde::Serialize;
use super::run_blocking;
use tauri::State;

// ---- Session commands ----
//
// The UI reads the universal, persisted session model from the SQLite store
// (see src/session/{model,store}.rs). Reconciliation runs in the background
// on a dedicated connection at launch (lib.rs) — session reads are PURE
// reads through `db_read` (WAL reader), so they never wait on the reconcile
// or on the gateway's writes. `session_refresh` forces a re-scan on the
// dedicated `reconcile_db` (still synchronous — the user clicked it — but it
// doesn't block UI reads).

#[tauri::command]
pub async fn session_list(
    state: State<'_, crate::AppState>,
    provider_id: Option<String>,
    search: Option<String>,
    limit: Option<u32>,
) -> AppResult<Vec<Session>> {
    let db = state.db_read.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        store::list_sessions(&conn, provider_id.as_deref(), search.as_deref(), limit.unwrap_or(300))
    })
    .await
}

#[tauri::command]
pub async fn session_search(
    state: State<'_, crate::AppState>,
    query: String,
    limit: Option<u32>,
) -> AppResult<Vec<Session>> {
    let db = state.db_read.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        store::search_sessions(&conn, &query, limit.unwrap_or(50))
    })
    .await
}

#[tauri::command]
pub async fn session_children(
    state: State<'_, crate::AppState>,
    provider_id: String,
    parent_id: String,
) -> AppResult<Vec<Session>> {
    let db = state.db_read.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        store::list_children(&conn, &provider_id, &parent_id)
    })
    .await
}

#[tauri::command]
pub async fn session_get(
    state: State<'_, crate::AppState>,
    provider_id: String,
    id: String,
) -> AppResult<Option<Session>> {
    let db = state.db_read.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        store::get_session(&conn, &provider_id, &id)
    })
    .await
}

#[tauri::command]
pub async fn session_read(
    state: State<'_, crate::AppState>,
    provider_id: String,
    id: String,
    offset: Option<u32>,
    limit: Option<u32>,
) -> AppResult<MessageWindow> {
    let db = state.db_read.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        store::read_messages(&conn, &provider_id, &id, offset.unwrap_or(0), limit.unwrap_or(0))
    })
    .await
}

/// Force a re-scan from disk. Runs synchronously on the dedicated
/// `reconcile_db` connection — the user clicked it and expects completion —
/// but because it's not on `db`/`db_read`, UI reads stay responsive while it
/// runs (WAL: the reconcile write doesn't block readers).
#[tauri::command]
pub async fn session_refresh(state: State<'_, crate::AppState>) -> AppResult<()> {
    let db = state.reconcile_db.clone();
    let flag = state.session_reconciled.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        crate::session::store::reconcile_all(&conn)?;
        if let Ok(mut g) = flag.lock() {
            *g = true;
        }
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn session_export(
    state: State<'_, crate::AppState>,
    provider_id: String,
    id: String,
) -> AppResult<String> {
    let db = state.db_read.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        store::export_session(&conn, &provider_id, &id)
    })
    .await
}

/// The `cmd /K` argument that runs `command` in a window that stays open
/// afterwards. Pure so the exact string is unit-testable.
fn terminal_command(command: &str) -> String {
    format!("/K {command}")
}

/// Launch `command` in its own console window, detached from the Nestra process.
/// Spawns `cmd /K <command>` with `CREATE_NEW_CONSOLE`, which gives the child
/// its own fresh console group (so it survives Nestra exiting) and a window
/// that stays open after the resume runs. `cwd`, when given and non-empty, is
/// applied so the resume starts in the session's project dir.
///
/// NOTE: `CREATE_NEW_CONSOLE` (0x10) must NOT be combined with
/// `DETACHED_PROCESS` (0x08) — the two are mutually exclusive and
/// `CreateProcess` fails with ERROR_INVALID_PARAMETER (os error 87).
#[cfg(windows)]
fn spawn_detached_terminal(command: &str, cwd: Option<&str>) -> AppResult<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_CONSOLE: u32 = 0x00000010;

    // Guard: empty cwd makes current_dir("") fail on Windows.
    let cwd_path = cwd.filter(|s| !s.is_empty());

    let inner = terminal_command(command);

    let mut c = std::process::Command::new("cmd");
    c.arg(&inner).creation_flags(CREATE_NEW_CONSOLE);
    if let Some(dir) = cwd_path.as_deref() {
        c.current_dir(dir);
    }
    let child = c.spawn().map_err(|e| {
        AppError::Internal(format!(
            "failed to spawn terminal `cmd /C {inner:?}` (cwd={cwd_path:?}): {e}"
        ))
    })?;
    tracing::info!(pid = child.id(), inner = %inner, "spawned detached terminal");
    Ok(())
}

#[cfg(not(windows))]
fn spawn_detached_terminal(_command: &str, _cwd: Option<&str>) -> AppResult<()> {
    Err(AppError::Internal(
        "opening a terminal is Windows-only".into(),
    ))
}

/// One-click "open in terminal": resolve the session's native agent from the
/// registry, build the resume command, and spawn a detached CMD at the
/// session's `cwd`.
///
/// Fails with `Validation` when the session's provider has no resumable agent.
#[tauri::command]
pub async fn session_open(
    state: State<'_, crate::AppState>,
    provider: String,
    id: String,
) -> AppResult<()> {
    let db = state.db_read.clone();
    let session = run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        store::get_session(&conn, &provider, &id)?.ok_or_else(|| {
            AppError::NotFound(format!("session {provider}/{id} not found"))
        })
    })
    .await?;
    let registry = default_provider_registry();
    let target = registry
        .iter()
        .find(|p| p.id == session.provider)
        .ok_or_else(|| {
            AppError::Validation(format!(
                "'{}' is not a registered provider",
                session.provider
            ))
        })?;
    let agent_id = target.agent_id.ok_or_else(|| {
        AppError::Validation(
            target
                .unsupported_reason
                .unwrap_or("provider has no resumable agent")
                .to_string(),
        )
    })?;
    let resume = build_resume_command(&registry, &session.provider, agent_id, &session.id)?;
    tracing::info!(
        provider = %session.provider,
        agent_id,
        cwd = ?session.cwd,
        resume = %resume,
        "session_open: launching"
    );
    spawn_detached_terminal(&resume, session.cwd.as_deref())
}

/// Reveal a session's source file in the OS file manager (parent folder,
/// opened in the platform default — File Explorer on Windows).
#[tauri::command]
pub async fn session_reveal(
    state: State<'_, crate::AppState>,
    provider: String,
    id: String,
) -> AppResult<()> {
    let db = state.db_read.clone();
    let session = run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        store::get_session(&conn, &provider, &id)?.ok_or_else(|| {
            AppError::NotFound(format!("session {provider}/{id} not found"))
        })
    })
    .await?;
    if session.source_path.is_empty() {
        return Err(AppError::NotFound(
            "session has no source path on disk".into(),
        ));
    }
    let target = crate::agents::reveal_target(&session.source_path);
    crate::agents::reveal_in_explorer(&target).map_err(|e| AppError::Internal(format!("reveal failed: {e}")))?;
    Ok(())
}

/// Result of deleting one session. The frontend surfaces this in the confirm
/// dialog so the user sees exactly what was removed from disk.
#[derive(Serialize)]
pub struct SessionDeleteResult {
    pub provider: String,
    pub id: String,
    pub removed_files: Vec<String>,
}

/// Delete a session from Nestra's DB AND remove its source files from disk.
/// This is destructive — the underlying CLI log is gone, so the session can
/// no longer be resumed from the original CLI. Use only from a confirmed UI
/// action.
#[tauri::command]
pub async fn session_delete(
    state: State<'_, crate::AppState>,
    provider: String,
    id: String,
) -> AppResult<SessionDeleteResult> {
    let db = state.db.clone();
    let flag = state.session_reconciled.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        let removed = store::delete_session(&conn, &provider, &id)?;
        // Force re-reconcile on next access so we don't serve stale rows that
        // a re-scan might resurrect (they won't, files are gone — but the flag
        // also protects against half-deleted state on error).
        let mut guard = flag.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        *guard = false;
        Ok(SessionDeleteResult {
            provider,
            id,
            removed_files: removed,
        })
    })
    .await
}

#[cfg(test)]
mod tests {

    #[test]
    fn build_resume_command_substitutes_native_id() {
        use crate::session::provider::{
            build_resume_command, default_provider_registry,
        };
        let reg = default_provider_registry();
        // Each provider's native CLI accepts its own id.
        assert_eq!(
            build_resume_command(&reg, "claude-code", "claude-code", "abc").unwrap(),
            "claude --resume abc"
        );
        // Pi uses the corrected flag.
        assert_eq!(
            build_resume_command(&reg, "pi", "pi", "uuid-pi").unwrap(),
            "pi --session uuid-pi"
        );
        // opencode-desktop is intentionally non-resumable (no resume_command),
        // so it's absent from the resumable registry; OpenCode sessions surface
        // as browse/delete-only.
    }

    #[test]
    fn build_resume_command_refuses_cross_provider() {
        use crate::session::provider::{
            build_resume_command, default_provider_registry,
        };
        let reg = default_provider_registry();
        // Claude session cannot be opened in Pi's CLI (cross-provider refused).
        let err = build_resume_command(&reg, "claude-code", "pi", "x").unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("source 'claude-code'"), "{msg}");
    }

    #[test]
    fn build_resume_command_refuses_unsupported_cli() {
        use crate::session::provider::{
            build_resume_command, default_provider_registry,
        };
        let reg = default_provider_registry();
        // An agent id not in the registry at all — resume must be refused.
        let err = build_resume_command(&reg, "nonexistent-cli", "custom", "x").unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("is not a registered agent"),
            "{msg}"
        );
    }

    #[test]
    fn terminal_command_wraps_resume_in_kept_open_cmd_window() {
        use super::terminal_command;
        assert_eq!(
            terminal_command("claude --resume abc"),
            "/K claude --resume abc"
        );
        // Even when the resume command itself contains spaces it passes through
        // verbatim; `cmd /K` runs it then keeps the window open.
        assert_eq!(
            terminal_command("pi --session x y z"),
            "/K pi --session x y z"
        );
    }

    #[test]
    fn reveal_target_resolves_file_to_parent_dir() {
        use crate::agents::reveal_target;
        // A session's source_path is its backing file → reveal the folder.
        assert_eq!(
            reveal_target("C:\\Users\\me\\.claude\\projects\\p\\abc.jsonl"),
            std::path::PathBuf::from("C:\\Users\\me\\.claude\\projects\\p")
        );
    }

    #[test]
    fn reveal_target_resolves_directory_to_itself() {
        use crate::agents::reveal_target;
        // A session whose source_path IS a directory reveals that directory.
        let dir = std::env::temp_dir().join(format!("nestra-reveal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.to_string_lossy().to_string();
        assert_eq!(reveal_target(&p), std::path::PathBuf::from(&p));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
