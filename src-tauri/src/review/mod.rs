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
mod tests {
    use super::*;

    #[test]
    fn registry_single_flight_and_clear_guards_id() {
        let reg = ReviewRegistry::default();
        let sup = supervisor::PiSupervisor::spawn(
            "node",
            &["-e".to_string(), "process.stdin.resume();".to_string()],
            None,
        )
        .unwrap();
        assert!(reg.try_install(ActiveReview { review_id: "r1".into(), sup: sup.clone() }));
        assert!(reg.active().map(|(id, _)| id == "r1").unwrap_or(false));
        // Second install rejected while r1 runs.
        assert!(!reg.try_install(ActiveReview { review_id: "r2".into(), sup: sup.clone() }));
        // A stale clear (wrong id) must not evict the running review.
        reg.clear("r2");
        assert!(reg.active().is_some());
        reg.clear("r1");
        assert!(reg.active().is_none());
        sup.shutdown();
    }

    #[test]
    fn review_rows_round_trip() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::build_v1(&conn).unwrap();
        let info = ReviewInfo {
            id: "rv-1".into(),
            agent_id: "pi-cli".into(),
            reviewed_session_provider: "pi-cli".into(),
            reviewed_session_id: "s-1".into(),
            status: "pending".into(),
            review_role: Some("pi:reviewer".into()),
            verdict_summary: None,
            verdict_status: None,
            artifact_path: Some("/tmp/context.md".into()),
            context_pack: None,
            created_at: 1,
            finished_at: None,
            reviewer_endpoint_id: None,
            reviewer_model: None,
            task_id: None,
            live_events: None,
        };
        insert_review(&conn, &info, r#"{"title":"T"}"#).unwrap();
        mark_review_status(&conn, "rv-1", "reviewing").unwrap();
        finish_review(&conn, "rv-1", "verdict", Some("pass"), Some("all good")).unwrap();
        let got = get_review(&conn, "rv-1").unwrap().unwrap();
        assert_eq!(got.status, "verdict");
        assert_eq!(got.verdict_status.as_deref(), Some("pass"));
        assert_eq!(got.context_pack.and_then(|p| p.get("title").and_then(|t| t.as_str()).map(str::to_string)).as_deref(), Some("T"));
        assert!(got.finished_at.is_some());
        assert_eq!(list_reviews(&conn, 10).unwrap().len(), 1);
        assert!(get_review(&conn, "nope").unwrap().is_none());
    }

    /// Seed a finished review + one `pi:reviewer` route_request row.
    fn seed_backfill_env(
        conn: &Connection,
        review_id: &str,
        created: i64,
        finished: i64,
    ) {
        crate::schema::build_v1(conn).unwrap();
        conn.execute(
            "INSERT INTO review (id, agent_id, reviewed_session_provider, reviewed_session_id,
                                 status, created_at, finished_at)
             VALUES (?1,'pi-cli','pi-cli','s-1','verdict',?2,?3)",
            rusqlite::params![review_id, created, finished],
        )
        .unwrap();
    }

    fn insert_route(
        conn: &Connection,
        started: i64,
        logical_session: Option<&str>,
        endpoint: &str,
        model: &str,
        task: &str,
    ) {
        conn.execute(
            "INSERT OR IGNORE INTO provider_endpoint (id, kind, display_name, has_api_key, status, models_json)
             VALUES (?1,'custom','E',0,'unvalidated','{}')",
            rusqlite::params![endpoint],
        )
        .unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO task (id, lifecycle, started_at) VALUES (?1,'done',?2)",
            rusqlite::params![task, started],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO route_request (request_id, task_id, agent_id, logical_session,
                                        subagent_role, route_reason, resolved_endpoint_id,
                                        resolved_model, started_at)
             VALUES (?1,?2,'pi-cli',?3,'pi:reviewer','capability',?4,?5,?6)",
            rusqlite::params![
                format!("req-{started}"),
                task,
                logical_session,
                endpoint,
                model,
                started
            ],
        )
        .unwrap();
    }

    #[test]
    fn backfill_takes_newest_row_of_single_session_per_review() {
        let conn = Connection::open_in_memory().unwrap();
        seed_backfill_env(&conn, "rv-1", 0, 100);
        seed_backfill_env(&conn, "rv-2", 200, 300);
        // rv-1's window: two rows in the SAME session (multi-turn review)…
        insert_route(&conn, 10, Some("sessA"), "ep-old", "m-old", "t1");
        insert_route(&conn, 50, Some("sessA"), "ep-new", "m-new", "t1");
        // rv-2's window: its own session — must not leak into rv-1.
        insert_route(&conn, 250, Some("sessB"), "ep-b", "m-b", "t2");

        backfill_reviewer(&conn, "rv-1").unwrap();
        let r1 = get_review(&conn, "rv-1").unwrap().unwrap();
        // Same-session multi-row is allowed; the NEWEST row wins.
        assert_eq!(r1.reviewer_endpoint_id.as_deref(), Some("ep-new"));
        assert_eq!(r1.reviewer_model.as_deref(), Some("m-new"));
        assert_eq!(r1.task_id.as_deref(), Some("t1"));

        backfill_reviewer(&conn, "rv-2").unwrap();
        let r2 = get_review(&conn, "rv-2").unwrap().unwrap();
        assert_eq!(r2.reviewer_endpoint_id.as_deref(), Some("ep-b"));
        assert_eq!(r2.task_id.as_deref(), Some("t2"));
    }

    #[test]
    fn backfill_prefers_exact_session_join_over_time_window() {
        let conn = Connection::open_in_memory().unwrap();
        seed_backfill_env(&conn, "rv-x", 0, 100);
        set_review_session(&conn, "rv-x", "pi-cli", "sess-real").unwrap();
        // In-window row from ANOTHER session — the exact join must skip it…
        insert_route(&conn, 50, Some("sess-other"), "ep-other", "m-other", "t-other");
        // …and the real session's row is deliberately OUTSIDE the window: the
        // exact join finds it anyway (window is only the id-unknown fallback).
        insert_route(&conn, 500, Some("sess-real"), "ep-real", "m-real", "t-real");
        backfill_reviewer(&conn, "rv-x").unwrap();
        let r = get_review(&conn, "rv-x").unwrap().unwrap();
        assert_eq!(r.reviewer_endpoint_id.as_deref(), Some("ep-real"));
        assert_eq!(r.reviewer_model.as_deref(), Some("m-real"));
        assert_eq!(r.task_id.as_deref(), Some("t-real"));
    }

    #[test]
    fn backfill_is_null_on_ambiguous_sessions_or_no_match() {
        let conn = Connection::open_in_memory().unwrap();
        // Window with TWO distinct logical_sessions → ambiguity → NULL.
        seed_backfill_env(&conn, "rv-a", 0, 100);
        insert_route(&conn, 10, Some("sessA"), "ep-1", "m-1", "t1");
        insert_route(&conn, 20, Some("sessB"), "ep-2", "m-2", "t2");
        backfill_reviewer(&conn, "rv-a").unwrap();
        let ra = get_review(&conn, "rv-a").unwrap().unwrap();
        assert_eq!(ra.reviewer_endpoint_id, None);
        assert_eq!(ra.task_id, None);

        // A NULL-session row cannot anchor the link either → NULL.
        seed_backfill_env(&conn, "rv-c", 400, 500);
        insert_route(&conn, 410, None, "ep-3", "m-3", "t3");
        backfill_reviewer(&conn, "rv-c").unwrap();
        let rc = get_review(&conn, "rv-c").unwrap().unwrap();
        assert_eq!(rc.reviewer_endpoint_id, None);

        // No gateway traffic at all → NULL.
        seed_backfill_env(&conn, "rv-b", 0, 100);
        backfill_reviewer(&conn, "rv-b").unwrap();
        let rb = get_review(&conn, "rv-b").unwrap().unwrap();
        assert_eq!(rb.reviewer_endpoint_id, None);
    }
}
