//! Review Runtime — one click spawns an isolated review Pi session on a
//! stronger model and returns a verdict, without touching the main context.
//!
//! The runtime plane exists exactly here: Nestra supervises ONLY sessions it
//! spawns itself (contract: `pi --mode rpc` children owned by
//! [`supervisor::PiSupervisor`]). The control plane is the existing gateway —
//! the review session's HTTP goes through the `nestra-gw` alias, the
//! `pi:reviewer` role marker routes it via `routing_policy`, and
//! `route_request` rows record the observability. No new agent (contract #5):
//! a review is a `pi-cli` ROLE, not a registry entry.
//!
//! R1 scope: single concurrent review, verdict from the reviewer's final
//! message (`VERDICT:` line convention). The structured verdict artifact
//! file, follow-ups, and test-result gathering are R2.

pub mod context;
pub mod supervisor;

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use serde_json::Value;

use crate::error::AppResult;

/// The single active review (R1: one at a time). Stored in `AppState` behind
/// a Clone handle, mirroring the `GatewayControl` precedent.
#[derive(Clone, Default)]
pub struct ReviewRegistry {
    inner: Arc<Mutex<Option<ActiveReview>>>,
}

pub struct ActiveReview {
    pub review_id: String,
    pub sup: Arc<supervisor::PiSupervisor>,
}

impl ReviewRegistry {
    /// The active review, if any (cloned handles — cheap).
    pub fn active(&self) -> Option<(String, Arc<supervisor::PiSupervisor>)> {
        self.inner
            .lock()
            .ok()?
            .as_ref()
            .map(|a| (a.review_id.clone(), a.sup.clone()))
    }

    /// Install; `false` (and no install) when another review is running.
    pub fn try_install(&self, a: ActiveReview) -> bool {
        match self.inner.lock() {
            Ok(mut slot) => {
                if slot.is_some() {
                    false
                } else {
                    *slot = Some(a);
                    true
                }
            }
            Err(_) => false,
        }
    }

    /// Clear only when `id` still owns the slot (an abort racing the natural
    /// finish must not clear a NEWER review).
    pub fn clear(&self, id: &str) {
        if let Ok(mut slot) = self.inner.lock() {
            if slot.as_ref().map(|a| a.review_id.as_str()) == Some(id) {
                *slot = None;
            }
        }
    }
}

/// One `review` row as the UI sees it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReviewInfo {
    pub id: String,
    pub agent_id: String,
    pub reviewed_session_provider: String,
    pub reviewed_session_id: String,
    pub status: String,
    pub review_role: Option<String>,
    pub verdict_summary: Option<String>,
    pub verdict_status: Option<String>,
    pub artifact_path: Option<String>,
    pub context_pack: Option<Value>,
    pub created_at: i64,
    pub finished_at: Option<i64>,
    /// Backfilled from the review's own gateway rows (NULL when the review
    /// never routed or the window is ambiguous).
    pub reviewer_endpoint_id: Option<String>,
    pub reviewer_model: Option<String>,
    pub task_id: Option<String>,
    /// Live event log — only while the review runs (`review_get`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_events: Option<Vec<Value>>,
}

const REVIEW_COLS: &str =
    "id, agent_id, reviewed_session_provider, reviewed_session_id, status, review_role, \
     verdict_summary, verdict_status, artifact_path, context_pack_json, created_at, finished_at, \
     reviewer_endpoint_id, reviewer_model, task_id";

fn row_to_info(r: &rusqlite::Row<'_>) -> rusqlite::Result<ReviewInfo> {
    let pack_json: Option<String> = r.get(9)?;
    Ok(ReviewInfo {
        id: r.get(0)?,
        agent_id: r.get(1)?,
        reviewed_session_provider: r.get(2)?,
        reviewed_session_id: r.get(3)?,
        status: r.get(4)?,
        review_role: r.get(5)?,
        verdict_summary: r.get(6)?,
        verdict_status: r.get(7)?,
        artifact_path: r.get(8)?,
        context_pack: pack_json
            .and_then(|j| serde_json::from_str::<Value>(&j).ok()),
        created_at: r.get(10)?,
        finished_at: r.get(11)?,
        reviewer_endpoint_id: r.get(12)?,
        reviewer_model: r.get(13)?,
        task_id: r.get(14)?,
        live_events: None,
    })
}

pub fn insert_review(conn: &Connection, info: &ReviewInfo, pack_json: &str) -> AppResult<()> {
    conn.execute(
        "INSERT INTO review (id, agent_id, reviewed_session_provider, reviewed_session_id,
                             status, review_role, artifact_path, context_pack_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            info.id,
            info.agent_id,
            info.reviewed_session_provider,
            info.reviewed_session_id,
            info.status,
            info.review_role,
            info.artifact_path,
            pack_json,
            info.created_at,
        ],
    )?;
    Ok(())
}

/// Finalize one review (status + verdict + finished_at).
pub fn finish_review(
    conn: &Connection,
    id: &str,
    status: &str,
    verdict_status: Option<&str>,
    verdict_summary: Option<&str>,
) -> AppResult<()> {
    conn.execute(
        "UPDATE review SET status = ?2, verdict_status = ?3, verdict_summary = ?4,
                           finished_at = ?5
         WHERE id = ?1",
        rusqlite::params![
            id,
            status,
            verdict_status,
            verdict_summary,
            chrono::Utc::now().timestamp_millis(),
        ],
    )?;
    Ok(())
}

pub fn mark_review_status(conn: &Connection, id: &str, status: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE review SET status = ?2 WHERE id = ?1",
        rusqlite::params![id, status],
    )?;
    Ok(())
}

pub fn get_review(conn: &Connection, id: &str) -> AppResult<Option<ReviewInfo>> {
    let r = conn.query_row(
        &format!("SELECT {REVIEW_COLS} FROM review WHERE id = ?1"),
        rusqlite::params![id],
        row_to_info,
    );
    match r {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Most recent reviews, newest-first.
pub fn list_reviews(conn: &Connection, limit: u32) -> AppResult<Vec<ReviewInfo>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {REVIEW_COLS} FROM review ORDER BY created_at DESC LIMIT ?1"
    ))?;
    let rows = stmt.query_map(rusqlite::params![limit], row_to_info)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Record the spawned session's native id (revealed by the RPC stream).
pub fn set_review_session(
    conn: &Connection,
    id: &str,
    provider: &str,
    session_id: &str,
) -> AppResult<()> {
    conn.execute(
        "UPDATE review SET review_session_id = ?2, review_session_provider = ?3 WHERE id = ?1",
        rusqlite::params![id, session_id, provider],
    )?;
    Ok(())
}

/// Backfill `reviewer_endpoint_id` / `reviewer_model` / `task_id` from the
/// gateway rows the review itself generated.
///
/// Primary path: EXACT join on `route_request.logical_session =
/// review_session_id` once the RPC stream revealed the native id — immune to
/// concurrent same-role traffic. Newest row of that session wins.
///
/// Fallback (id unknown): the review's CLOSED `[created_at, finished_at]`
/// window, where every matched `pi:reviewer` route_request must share ONE
/// non-null `logical_session` — several rows in the SAME session are normal
/// (multi-turn review; the newest wins), a NULL session row or a SECOND
/// distinct session is ambiguity → keep NULL (correctness over fill-rate).
pub fn backfill_reviewer(conn: &Connection, id: &str) -> AppResult<()> {
    let row = conn.query_row(
        "SELECT created_at, finished_at, review_session_id FROM review WHERE id = ?1",
        rusqlite::params![id],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        },
    );
    let (created, finished, review_session) = match row {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    // Still running (no finished_at) — nothing to backfill from yet.
    let Some(finished) = finished else {
        return Ok(());
    };

    let apply = |endpoint: &Option<String>, model: &Option<String>, task: &Option<String>| {
        conn.execute(
            "UPDATE review SET reviewer_endpoint_id = ?2, reviewer_model = ?3, task_id = ?4
             WHERE id = ?1",
            rusqlite::params![id, endpoint, model, task],
        )
    };

    // Primary: exact session join.
    if let Some(sess) = review_session.filter(|s| !s.is_empty()) {
        let exact = conn.query_row(
            "SELECT resolved_endpoint_id, resolved_model, task_id FROM route_request
             WHERE agent_id = 'pi-cli' AND subagent_role = 'pi:reviewer'
               AND logical_session = ?1
             ORDER BY started_at DESC LIMIT 1",
            rusqlite::params![sess],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        );
        if let Ok((endpoint, model, task)) = exact {
            apply(&endpoint, &model, &task)?;
        }
        return Ok(());
    }

    // Fallback: closed window + single-logical_session uniqueness.
    let mut stmt = conn.prepare(
        "SELECT logical_session, resolved_endpoint_id, resolved_model, task_id
         FROM route_request
         WHERE agent_id = 'pi-cli' AND subagent_role = 'pi:reviewer'
           AND started_at >= ?1 AND started_at <= ?2
         ORDER BY started_at DESC",
    )?;
    let rows: Vec<(Option<String>, Option<String>, Option<String>, Option<String>)> = stmt
        .query_map(rusqlite::params![created, finished], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    if rows.is_empty() {
        return Ok(());
    }
    let unambiguous =
        rows.iter().all(|(s, ..)| s.is_some() && s.as_deref() == rows[0].0.as_deref());
    if !unambiguous {
        return Ok(());
    }
    let (_, endpoint, model, task) = &rows[0];
    apply(endpoint, model, task)?;
    Ok(())
}

#[cfg(test)]
mod tests;
