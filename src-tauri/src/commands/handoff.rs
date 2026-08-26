//! Handoff commands (Context Lifecycle R1/R2) — thin IPC over
//! [`crate::session::handoff`]. Reads go through `db_read`; the DB writers
//! (`handoff_save`, `handoff_delete`, `handoff_spawn`'s row updates) go
//! through `db`, matching the session-command split (see commands/sessions.rs).

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use super::run_blocking;
use crate::error::{AppError, AppResult};
use crate::review::supervisor::PiSupervisor;
use crate::session::handoff::{self, ContextPressure, HandoffInfo};

#[tauri::command]
pub async fn session_context_pressure(
    state: State<'_, crate::AppState>,
    provider_id: String,
    session_id: String,
) -> AppResult<ContextPressure> {
    let db = state.db_read.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        let parts = handoff::parts_for_session(&conn, &provider_id, &session_id)?;
        // Real catalog window when the session's model is identified;
        // otherwise the default 200k window (flagged estimated).
        let window = handoff::last_model(&parts)
            .and_then(|m| handoff::window_for_model(&conn, &m));
        Ok(handoff::context_pressure(&parts, window))
    })
    .await
}

/// The editable artifact BEFORE committing: structural extraction rendered as
/// markdown. The UI shows this in an editor; `handoff_save` persists the
/// (possibly edited) text.
#[derive(Serialize)]
pub struct HandoffPreview {
    pub markdown: String,
}

#[tauri::command]
pub async fn handoff_preview(
    state: State<'_, crate::AppState>,
    provider_id: String,
    session_id: String,
) -> AppResult<HandoffPreview> {
    let db = state.db_read.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        let title: String = conn
            .query_row(
                "SELECT title FROM session WHERE provider = ?1 AND id = ?2",
                rusqlite::params![provider_id, session_id],
                |r| r.get(0),
            )
            .map_err(|_| AppError::Validation(format!("session {provider_id}/{session_id} not found")))?;
        let parts = handoff::parts_for_session(&conn, &provider_id, &session_id)?;
        let sections = handoff::build_sections(&parts);
        Ok(HandoffPreview {
            markdown: handoff::render_markdown(&title, &sections),
        })
    })
    .await
}

#[tauri::command]
pub async fn handoff_save(
    state: State<'_, crate::AppState>,
    provider_id: String,
    session_id: String,
    markdown: String,
) -> AppResult<HandoffInfo> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        handoff::save_handoff(&conn, &provider_id, &session_id, &markdown)
    })
    .await
}

#[tauri::command]
pub async fn handoff_list(
    state: State<'_, crate::AppState>,
    provider_id: String,
    session_id: String,
) -> AppResult<Vec<HandoffInfo>> {
    let db = state.db_read.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        handoff::list_handoffs(&conn, &provider_id, &session_id)
    })
    .await
}

#[tauri::command]
pub async fn handoff_delete(state: State<'_, crate::AppState>, id: String) -> AppResult<()> {
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        handoff::delete_handoff(&conn, &id)
    })
    .await
}

/// Copy the artifact into the session repo's `.pi/` + add the removable
/// reference line (unsupervised injection). Returns the injected file path.
#[tauri::command]
pub async fn handoff_inject(state: State<'_, crate::AppState>, id: String) -> AppResult<String> {
    let db = state.db_read.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        handoff::inject_handoff(&conn, &id)
    })
    .await
}

/// Remove the injected `.pi/` copy + its reference line (anti-pollution).
#[tauri::command]
pub async fn handoff_inject_remove(state: State<'_, crate::AppState>, id: String) -> AppResult<()> {
    let db = state.db_read.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        handoff::remove_injected_handoff(&conn, &id)
    })
    .await
}

/// Promote a handoff to a durable knowledge file (frontmatter markdown under
/// `~/.nestra/knowledge/`). Returns the written path.
#[tauri::command]
pub async fn handoff_to_knowledge(
    state: State<'_, crate::AppState>,
    id: String,
    kind: Option<String>,
) -> AppResult<String> {
    let db = state.db_read.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        handoff::handoff_to_knowledge(&conn, &id, &kind.unwrap_or_else(|| "decision".into()))
    })
    .await
}

/// Supervised RPC injection (the P0 handoff path): spawn a fresh Pi session
/// (no reviewer marker — a plain work session on the gateway alias), send the
/// handoff artifact as the initial prompt, and watch the landing. When the
/// stream reveals the native session id it is recorded on the handoff row —
/// the user can then `pi --session <id>` to take over. Emits
/// `handoff:<id>:event` per RPC event and `handoff:<id>:done` at the end.
#[tauri::command]
pub async fn handoff_spawn(
    state: State<'_, crate::AppState>,
    app: AppHandle,
    id: String,
) -> AppResult<HandoffInfo> {
    // Artifact content + the source session's cwd.
    let (artifact_path, cwd) = {
        let db = state.db_read.clone();
        let id = id.clone();
        run_blocking(move || {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            conn.query_row(
                "SELECT h.artifact_path, s.cwd
                 FROM handoff h JOIN session s
                   ON s.provider = h.source_provider AND s.id = h.source_session_id
                 WHERE h.id = ?1",
                rusqlite::params![id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .map_err(|_| AppError::Validation(format!("handoff {id} not found")))
        })
        .await?
    };
    let artifact = std::path::PathBuf::from(&artifact_path);
    if !crate::session::handoff::is_handoff_artifact_path(&artifact) {
        return Err(AppError::Validation(format!(
            "handoff artifact path outside .nestra/handoffs: {artifact_path}"
        )));
    }
    let markdown = std::fs::read_to_string(&artifact)
        .map_err(|e| AppError::Internal(format!("handoff artifact unreadable: {e}")))?;

    let exe = super::review::resolve_pi_exe(&state).await?;
    // The single supervised-child slot is shared with reviews (one pi RPC
    // child at a time, whatever its purpose).
    if state.reviews.active().is_some() {
        return Err(AppError::Validation(
            "a supervised session is already running".into(),
        ));
    }
    let short: String = id.chars().take(8).collect();
    let args: Vec<String> = [
        "--mode",
        "rpc",
        "--provider",
        "nestra-gw",
        "--name",
        &format!("nestra-handoff-{short}"),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let sup = PiSupervisor::spawn(&exe, &args, cwd.as_deref())?;
    let slot = format!("handoff:{id}");
    if !state
        .reviews
        .try_install(crate::review::ActiveReview { review_id: slot.clone(), sup: sup.clone() })
    {
        sup.shutdown();
        return Err(AppError::Validation(
            "a supervised session is already running".into(),
        ));
    }

    let prompt = format!(
        "You are continuing from a previous session's handoff. Read it fully, restate the goal in one line, then begin continuing the work.\n\n---\n\n{markdown}\n\n---\n"
    );
    let registry = state.reviews.clone();
    let hid = id.clone();
    tauri::async_runtime::spawn(async move {
        run_handoff_landing(&app, &registry, &hid, &slot, &sup, &prompt).await;
    });

    let db = state.db_read.clone();
    let id2 = id.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        handoff::get_handoff_row(&conn, &id2)
    })
    .await
}

/// Mini-runner: land the handoff, record the native session id once the
/// stream reveals it, then release the child (the user resumes it
/// interactively via `pi --session <id>` — headless RPC is for the landing,
/// not for running the whole task).
async fn run_handoff_landing(
    app: &AppHandle,
    registry: &crate::review::ReviewRegistry,
    handoff_id: &str,
    slot: &str,
    sup: &std::sync::Arc<PiSupervisor>,
    prompt: &str,
) {
    use std::time::{Duration, Instant};
    let event_name = format!("handoff:{handoff_id}:event");
    let done_name = format!("handoff:{handoff_id}:done");
    let timeout = Duration::from_secs(600);
    if sup
        .send(&serde_json::json!({ "type": "prompt", "text": prompt }))
        .is_err()
    {
        registry.clear(slot);
        sup.shutdown();
        let _ = app.emit(&done_name, ());
        return;
    }
    let deadline = Instant::now() + timeout;
    loop {
        let still_ours = registry.active().map(|(rid, _)| rid == slot).unwrap_or(false);
        if !still_ours {
            let _ = app.emit(&done_name, ());
            return;
        }
        match sup.next_event(Duration::from_millis(500)) {
            Some(v) => {
                let _ = app.emit(&event_name, &v);
                if crate::review::supervisor::has_settled(&[v]) {
                    break;
                }
            }
            None => {
                if sup.is_finished() || Instant::now() > deadline {
                    break;
                }
            }
        }
    }
    // Record the native session id on the handoff row (fresh connection).
    if let Some(native) = crate::review::supervisor::session_id_of(&sup.events_snapshot()) {
        if let Some(conn) = crate::db::data_dir().ok().and_then(|d| crate::db::open(&d).ok()) {
            let _ = handoff::set_target_session(&conn, handoff_id, &native);
        }
    }
    registry.clear(slot);
    sup.shutdown();
    let _ = app.emit(&done_name, ());
}
