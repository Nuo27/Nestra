//! Review commands (Review Runtime R1) — create/start/abort/list/get over
//! [`crate::review`]. The runner loop (spawned by `review_start`) drives the
//! supervised RPC session to `agent_settled`, emits `review:<id>:event` per
//! RPC event + `review:<id>:done` at the end, and finalizes the row with the
//! verdict parsed from the reviewer's final message.

use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, State};

use super::run_blocking;
use crate::error::{AppError, AppResult};
use crate::review::supervisor;
use crate::review::{self, ReviewInfo, ReviewRegistry};

/// Hard wall for one review run. A settled reviewer never hits it; a wedged
/// one must not hold the single-flight slot forever.
const REVIEW_TIMEOUT: Duration = Duration::from_secs(600);

#[tauri::command]
pub async fn review_create(
    state: State<'_, crate::AppState>,
    provider_id: String,
    session_id: String,
) -> AppResult<ReviewInfo> {
    // Gather on the read connection…
    let (pack, cwd) = {
        let db = state.db_read.clone();
        let provider_id = provider_id.clone();
        let session_id = session_id.clone();
        run_blocking(move || {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            review::context::gather(&conn, &provider_id, &session_id)
        })
        .await?
    };
    // …write the context artifact (keyed by the fresh id), then insert the row.
    let id = uuid::Uuid::new_v4().to_string();
    let artifact = review::context::write_context_md(cwd.as_deref(), &id, &pack)?;
    let pack_json = serde_json::to_string(&pack)
        .map_err(|e| AppError::Internal(format!("serialize pack: {e}")))?;
    let info = ReviewInfo {
        id,
        agent_id: "pi-cli".into(),
        reviewed_session_provider: provider_id,
        reviewed_session_id: session_id,
        status: "pending".into(),
        review_role: Some("pi:reviewer".into()),
        verdict_summary: None,
        verdict_status: None,
        artifact_path: Some(artifact),
        context_pack: None,
        created_at: chrono::Utc::now().timestamp_millis(),
        finished_at: None,
        reviewer_endpoint_id: None,
        reviewer_model: None,
        task_id: None,
        live_events: None,
    };
    let db = state.db.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        review::insert_review(&conn, &info, &pack_json)?;
        Ok(info)
    })
    .await
}

#[tauri::command]
pub async fn review_start(
    state: State<'_, crate::AppState>,
    app: AppHandle,
    id: String,
) -> AppResult<ReviewInfo> {
    // Row + prompt inputs on the read connection.
    let (prompt, cwd) = {
        let db = state.db_read.clone();
        let id = id.clone();
        run_blocking(move || {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            let info = review::get_review(&conn, &id)?
                .ok_or_else(|| AppError::Validation(format!("review {id} not found")))?;
            if !matches!(info.status.as_str(), "pending" | "failed" | "aborted") {
                return Err(AppError::Validation(format!(
                    "review is already {} — create a new one",
                    info.status
                )));
            }
            let pack = info
                .context_pack
                .as_ref()
                .and_then(|v| serde_json::to_string(v).ok())
                .and_then(|j| review::context::pack_from_json(&j))
                .ok_or_else(|| AppError::Internal("review context pack unreadable".into()))?;
            let cwd: Option<String> = conn
                .query_row(
                    "SELECT cwd FROM session WHERE provider = ?1 AND id = ?2",
                    rusqlite::params![info.reviewed_session_provider, info.reviewed_session_id],
                    |r| r.get(0),
                )
                .unwrap_or(None);
            Ok((review::context::render_prompt(&pack, &id), cwd))
        })
        .await?
    };

    // Resolve the pi executable: detected path / override, else PATH. Shared
    // with the handoff RPC-injection spawn (both supervise a plain pi child).
    let exe = resolve_pi_exe(&state).await?;

    // Single-flight install + spawn. The reviewer role marker makes the
    // gateway classify this session as `pi:reviewer` (zero gateway change);
    // `nestra-gw` is the alias Routed mode writes into pi's config.
    if state.reviews.active().is_some() {
        return Err(AppError::Validation("a supervised session is already running".into()));
    }
    let short: String = id.chars().take(8).collect();
    let args: Vec<String> = [
        "--mode",
        "rpc",
        "--append-system-prompt",
        supervisor::REVIEWER_MARKER,
        "--name",
        &format!("nestra-review-{short}"),
        "--provider",
        "nestra-gw",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let sup = supervisor::PiSupervisor::spawn(&exe, &args, cwd.as_deref())?;
    let installed = state.reviews.try_install(review::ActiveReview {
        review_id: id.clone(),
        sup: sup.clone(),
    });
    if !installed {
        sup.shutdown();
        return Err(AppError::Validation("a supervised session is already running".into()));
    }

    {
        let db = state.db.clone();
        let id = id.clone();
        run_blocking(move || {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            review::mark_review_status(&conn, &id, "reviewing")
        })
        .await?;
    }

    // Runner loop on the async runtime. Owns its own DB connection (the UI
    // locks must never be held across the minutes-long supervision).
    let registry: ReviewRegistry = state.reviews.clone();
    let rid = id.clone();
    let cwd_clone = cwd.clone();
    tauri::async_runtime::spawn(async move {
        run_to_completion(&app, &registry, &rid, &sup, &prompt, cwd_clone.as_deref()).await;
    });

    let mut info = get_fresh(&state, &id).await?;
    info.live_events = Some(Vec::new());
    Ok(info)
}

/// Fetch the current row (helper for the return value above).
async fn get_fresh(state: &State<'_, crate::AppState>, id: &str) -> AppResult<ReviewInfo> {
    let db = state.db_read.clone();
    let id = id.to_string();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        review::get_review(&conn, &id)?
            .ok_or_else(|| AppError::Validation(format!("review {id} not found")))
    })
    .await
}

/// Resolve the pi executable: detected path / override (agent row), else
/// PATH lookup. Shared by the review spawn and the handoff RPC injection.
pub(crate) async fn resolve_pi_exe(
    state: &State<'_, crate::AppState>,
) -> AppResult<String> {
    let db = state.db_read.clone();
    let stored = run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        Ok(conn
            .query_row(
                "SELECT COALESCE(path_override, path) FROM agent WHERE id = 'pi-cli'",
                [],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten())
    })
    .await?
    .filter(|p| !p.is_empty());
    match stored {
        Some(p) => Ok(p),
        None => which::which("pi")
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|_| AppError::Validation("pi executable not found — detect Pi first".into())),
    }
}

/// The supervision loop: prompt → events → settled → verdict. Emits
/// `review:<id>:event` per RPC event and `review:<id>:done` at the end.
async fn run_to_completion(
    app: &AppHandle,
    registry: &ReviewRegistry,
    id: &str,
    sup: &std::sync::Arc<supervisor::PiSupervisor>,
    prompt: &str,
    cwd: Option<&str>,
) {
    let event_name = format!("review:{id}:event");
    let done_name = format!("review:{id}:done");
    let outcome: Result<String, String>; // Ok(final reply) | Err(failure reason)

    if let Err(e) = sup.send(&serde_json::json!({ "type": "prompt", "text": prompt })) {
        outcome = Err(e.to_string());
    } else {
        let deadline = Instant::now() + REVIEW_TIMEOUT;
        outcome = loop {
            // Aborted elsewhere (the slot was cleared) → exit silently; the
            // abort command already finalized the row.
            let still_ours = registry
                .active()
                .map(|(rid, _)| rid == id)
                .unwrap_or(false);
            if !still_ours {
                let _ = app.emit(&done_name, ());
                return;
            }
            match sup.next_event(Duration::from_millis(500)) {
                Some(v) => {
                    let _ = app.emit(&event_name, &v);
                    if supervisor::has_settled(&[v]) {
                        break Ok(final_reply(sup).await);
                    }
                }
                None => {
                    if sup.is_finished() {
                        break Err("review session exited before settling".into());
                    }
                    if Instant::now() > deadline {
                        break Err("review timed out".into());
                    }
                }
            }
        };
    }

    // Finalize on a fresh connection (never the UI locks).
    let finalize_conn = crate::db::data_dir()
        .ok()
        .and_then(|dir| crate::db::open(&dir).ok());
    if let Some(conn) = finalize_conn {
        // Session identity first: the native id the RPC stream revealed (if
        // any) upgrades the backfill to the exact logical_session join.
        if let Some(native) = supervisor::session_id_of(&sup.events_snapshot()) {
            let _ = review::set_review_session(&conn, id, "pi-cli", &native);
        }
        match outcome {
            Ok(text) => {
                // Structured verdict file wins over the reply's VERDICT: line.
                let file = std::fs::read_to_string(review::context::verdict_file_path(cwd, id))
                    .ok();
                let (vstatus, summary) =
                    review::context::merge_verdict(file.as_deref(), &text);
                let _ = review::finish_review(&conn, id, "verdict", vstatus.as_deref(), Some(&summary));
            }
            Err(reason) => {
                let _ = review::finish_review(&conn, id, "failed", None, Some(&reason));
            }
        }
        // Backfill reviewer endpoint/model/task from the review's own gateway
        // rows (ambiguous window → stays NULL; see backfill_reviewer).
        let _ = review::backfill_reviewer(&conn, id);
    }
    registry.clear(id);
    sup.shutdown();
    let _ = app.emit(&done_name, ());
}

/// After `agent_settled`: the final assistant text, falling back to a
/// `get_messages` round-trip when no message event carried it.
async fn final_reply(sup: &std::sync::Arc<supervisor::PiSupervisor>) -> String {
    if let Some(t) = supervisor::final_assistant_text(&sup.events_snapshot()) {
        return t;
    }
    let _ = sup.send(&serde_json::json!({ "type": "get_messages" }));
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let _ = sup.next_event(Duration::from_millis(200));
        if let Some(t) = supervisor::final_assistant_text(&sup.events_snapshot()) {
            return t;
        }
    }
    "reviewer settled without a visible reply".into()
}

#[tauri::command]
pub async fn review_abort(state: State<'_, crate::AppState>, id: String) -> AppResult<()> {
    // Finalize FIRST (the runner exits silently once the slot is cleared).
    {
        let db = state.db.clone();
        let id = id.clone();
        run_blocking(move || {
            let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
            review::finish_review(&conn, &id, "aborted", None, Some("aborted by user"))
        })
        .await?;
    }
    if let Some((active_id, sup)) = state.reviews.active() {
        if active_id == id {
            state.reviews.clear(&id);
            sup.shutdown();
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn review_list(state: State<'_, crate::AppState>) -> AppResult<Vec<ReviewInfo>> {
    let db = state.db_read.clone();
    run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        review::list_reviews(&conn, 50)
    })
    .await
}

#[tauri::command]
pub async fn review_get(
    state: State<'_, crate::AppState>,
    id: String,
) -> AppResult<Option<ReviewInfo>> {
    let db = state.db_read.clone();
    let mut info = run_blocking(move || {
        let conn = db.lock().map_err(|e| AppError::Internal(e.to_string()))?;
        review::get_review(&conn, &id)
    })
    .await?;
    if let Some(info) = info.as_mut() {
        if let Some((active_id, sup)) = state.reviews.active() {
            if active_id == info.id {
                info.live_events = Some(sup.events_snapshot());
            }
        }
    }
    Ok(info)
}
