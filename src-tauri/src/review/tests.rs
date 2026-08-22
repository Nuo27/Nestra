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