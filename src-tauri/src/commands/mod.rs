//! Tauri command layer — one file per domain, mirroring the crate's module layout:
//! `common` (cross-domain validators) · `sessions` · `skills` · `mcp` · `settings` ·
//! `palette` · `diagnostics` · `endpoints` (provider CRUD) · `quota` · `presets` ·
//! `agents` (CLI switch surface) · `orchestration` (routing policy + control plane) ·
//! `gateway` (service control).
//!
//! Shared infrastructure (`run_blocking`, `AppParts`, `snapshot_state`,
//! `run_launch_reconcile`) lives here so every domain reaches it via `super::` —
//! the same child-sees-parent pattern `session/desktop.rs` uses.

pub mod agents;
pub mod autostart;
mod common;
pub mod diagnostics;
pub mod endpoints;
pub mod gateway;
pub mod handoff;
pub mod mcp;
pub mod orchestration;
pub mod palette;
pub mod presets;
pub mod quota;
pub mod review;
pub mod sessions;
pub mod settings;
pub mod skills;
pub mod updates;

use crate::error::{AppError, AppResult};
use std::sync::{Arc, Mutex};
use tauri::Emitter;

/// Run a blocking closure on the Tauri async runtime's thread pool so it never
/// stalls the webview/UI thread. Sync `#[tauri::command]` fns run on the main
/// thread; any heavy work (disk, HTTP, subprocess) must go through this.
async fn run_blocking<F, T>(f: F) -> AppResult<T>
where
    F: FnOnce() -> AppResult<T> + Send + 'static,
    T: Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(f).await {
        Ok(Ok(t)) => Ok(t),
        Ok(Err(e)) => Err(e),
        Err(je) => Err(AppError::Internal(format!("background task failed: {je}"))),
    }
}

/// Owned handles extracted from `&AppState` so agent-mutation commands can run
/// their file/db work on a blocking thread instead of stalling the UI thread.
#[derive(Clone)]
pub struct AppParts {
    pub db: Arc<Mutex<rusqlite::Connection>>,
}

/// Snapshot the pieces of `AppState` the agent commands need. Cheap: `db` is
/// an `Arc` clone. (The tray checkmark is updated via
/// `tray::set_autostart_checked`, not through `AppParts`.)
pub fn snapshot_state(state: &crate::AppState) -> AppParts {
    AppParts {
        db: state.db.clone(),
    }
}

/// Run the first-launch session reconcile in the background on the dedicated
/// `reconcile_db` connection (spawned from lib.rs setup). Emits
/// `sessions-reconciled` when done so the frontend can refresh its session
/// queries. The `flag` guards against double-work (e.g. a manual refresh
/// racing the launch reconcile); on failure it's reset so a later attempt can
/// retry. UI reads (`session_list`, etc.) are pure `db_read` reads and never
/// wait on this.
pub fn run_launch_reconcile(
    reconcile_db: Arc<Mutex<rusqlite::Connection>>,
    flag: Arc<Mutex<bool>>,
    app: tauri::AppHandle,
) {
    std::thread::Builder::new()
        .name("nestra-session-reconcile".into())
        .spawn(move || {
            let result = (|| -> AppResult<()> {
                let conn = reconcile_db
                    .lock()
                    .map_err(|e| AppError::Internal(e.to_string()))?;
                // Check-and-set under the conn lock so a racing manual
                // refresh can't double-reconcile.
                let already = {
                    let g = flag
                        .lock()
                        .map_err(|e| AppError::Internal(e.to_string()))?;
                    *g
                };
                if already {
                    return Ok(());
                }
                crate::session::store::reconcile_all(&conn)?;
                // Prune observability data (route_request / run / task /
                // route_migration / logical_session) older than the retention
                // window. These tables are insert-only otherwise (one row per
                // gateway request) and would grow without bound. Best-effort:
                // a prune failure is logged, not fatal.
                if let Err(e) = crate::db::prune_observability_data(&conn) {
                    tracing::warn!(error = %e, "observability prune failed");
                }
                if let Ok(mut g) = flag.lock() {
                    *g = true;
                }
                Ok(())
            })();
            if let Err(e) = result {
                tracing::warn!(error = %e, "launch session reconcile failed");
                if let Ok(mut g) = flag.lock() {
                    *g = false;
                }
            }
            let _ = app.emit("sessions-reconciled", ());
        })
        .expect("spawn session-reconcile thread");
}
