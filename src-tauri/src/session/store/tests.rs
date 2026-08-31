use super::*;
use crate::testutil::temp_home;
use std::io::Write;
use std::path::PathBuf;

fn with_home<R>(home: &std::path::Path, f: impl FnOnce() -> R) -> R {
    let _guard = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let prev = std::env::var("NESTRA_HOME_DIR").ok();
    std::env::set_var("NESTRA_HOME_DIR", home);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    match prev {
        Some(p) => std::env::set_var("NESTRA_HOME_DIR", p),
        None => std::env::remove_var("NESTRA_HOME_DIR"),
    }
    match result {
        Ok(v) => v,
        Err(p) => std::panic::resume_unwind(p),
    }
}

/// Write one or more Claude project JSONL lines to `path`.
fn write_lines(path: &std::path::Path, lines: &[&str]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut f = std::fs::File::create(path).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

fn claude_user_line(id: &str, text: &str, ts: &str, cwd: &str) -> String {
    format!(
        r#"{{"type":"user","message":{{"role":"user","content":"{text}"}},"sessionId":"{id}","timestamp":"{ts}","cwd":"{cwd}"}}"#
    )
}

fn claude_assistant_line(id: &str, text: &str, ts: &str) -> String {
    format!(
        r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}},"sessionId":"{id}","timestamp":"{ts}"}}"#
    )
}

/// Reconcile provider into a fresh temp DB seeded from `home`. Returns the
/// (DB path, connection) with the schema migrated.
fn reconcile(home: &std::path::Path) -> (tempfile::TempDir, std::path::PathBuf, Connection) {
    let (db_path, guard) = temp_home();
    let conn = crate::db::open(&db_path).unwrap();
    crate::db::migrate(&conn).unwrap();
    with_home(home, || reconcile_provider(&conn, "claude-code-cli").unwrap());
    (guard, db_path, conn)
}

/// Rows for providers no longer in ALL_PROVIDERS (e.g. the pre-rename
/// `claude-code`/`pi` ids) are pruned; current providers survive.
#[test]
fn prune_removes_rows_for_renamed_providers() {
    let (db_path, _guard) = temp_home();
    let conn = crate::db::open(&db_path).unwrap();
    crate::db::migrate(&conn).unwrap();
    for (provider, id) in [("claude-code", "s1"), ("claude-code-cli", "s2"), ("pi", "s3")] {
        conn.execute(
            "INSERT INTO session (provider, id, title, summary, started_at, updated_at,
                 message_count, source_path, resume_command)
                 VALUES (?1, ?2, 't', '', 1, 1, 0, 'x', '')",
            [provider, id],
        )
        .unwrap();
    }
    prune_unknown_providers(&conn).unwrap();
    let mut stmt = conn
        .prepare("SELECT provider, id FROM session ORDER BY provider")
        .unwrap();
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(rows, vec![("claude-code-cli".to_string(), "s2".to_string())]);
}

// Claude canonical ids come from the `sessionId` field, not the filename.
const PARENT: &str = "parent-00000000-0000-0000-0000-000000000001";
const SUB: &str = "agent-abc123";
const OTHER: &str = "other-session";

/// A claude tree with a parent + one subagent, each with real files, plus
/// a second independent session. Returns (home, guard) — the guard keeps
/// the temp dir alive for the caller's scope.
fn seed_tree() -> (PathBuf, tempfile::TempDir) {
    let (home, guard) = temp_home();
    let proj = home.join(".claude").join("projects").join("projhash");
    let parent = proj.join(format!("{PARENT}.jsonl"));
    write_lines(
        &parent,
        &[
            &claude_user_line(PARENT, "what is the prime goal", "2026-08-06T10:00:00.000Z", "C:\\\\goal"),
            &claude_assistant_line(PARENT, "a one-line answer", "2026-08-06T10:00:01.000Z"),
            &claude_user_line(PARENT, "and the second question", "2026-08-06T10:00:02.000Z", "C:\\\\goal"),
        ],
    );
    let sub = proj.join(PARENT).join("subagents").join(format!("agent-{SUB}.jsonl"));
    write_lines(
        &sub,
        &[&format!(
            r#"{{"parentUuid":null,"isSidechain":true,"agentId":"{SUB}","type":"user","message":{{"role":"user","content":"do the thing"}},"sessionId":"{PARENT}","timestamp":"2026-08-06T10:00:03.000Z","cwd":"C:\\goal"}}"#
        )],
    );
    let other = proj.join(format!("{OTHER}.jsonl"));
    write_lines(
        &other,
        &[
            &claude_user_line(OTHER, "unrelated searchable alpha", "2026-08-06T09:00:00.000Z", "C:\\\\zeta"),
            &claude_assistant_line(OTHER, "zeta answer", "2026-08-06T09:00:05.000Z"),
        ],
    );
    (home, guard)
}

#[test]
fn list_sessions_filters_provider_and_excludes_subagents() {
    let (home, _home_g) = seed_tree();
    let (_reconcile_g, _db, conn) = reconcile(&home);
    let all = list_sessions(&conn, None, None, 100).unwrap();
    // parent + other are top-level; the subagent is filtered out.
    assert_eq!(all.len(), 2);
    let ids: Vec<&str> = all.iter().map(|s| s.id.as_str()).collect();
    assert!(ids.contains(&PARENT));
    assert!(ids.contains(&OTHER));
    assert!(!ids.contains(&SUB));
    // provider filter narrows correctly.
    let none = list_sessions(&conn, Some("nope"), None, 100).unwrap();
    assert!(none.is_empty());
}

#[test]
fn list_sessions_orders_by_updated_at_desc() {
    let (home, _home_g) = seed_tree();
    let (_reconcile_g, _db, conn) = reconcile(&home);
    let all = list_sessions(&conn, None, None, 100).unwrap();
    // OTHER updated first (09:00) so it sorts behind PARENT (10:00).
    let mut prev = i64::MAX;
    for s in &all {
        assert!(s.updated_at <= prev, "not sorted desc: {prev} >= {}", s.updated_at);
        prev = s.updated_at;
    }
    assert_eq!(all[0].id, PARENT);
}

#[test]
fn list_sessions_search_matches_title_and_respects_limit() {
    let (home, _home_g) = seed_tree();
    let (_reconcile_g, _db, conn) = reconcile(&home);
    let hit = list_sessions(&conn, None, Some("prime"), 100).unwrap();
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].id, PARENT);
    let limited = list_sessions(&conn, None, Some("alpha"), 0).unwrap();
    assert!(limited.is_empty(), "limit 0 should return nothing");
}

#[test]
fn list_children_returns_subagent_ordered_by_started_at() {
    let (home, _home_g) = seed_tree();
    let (_reconcile_g, _db, conn) = reconcile(&home);
    let kids = list_children(&conn, "claude-code-cli", PARENT).unwrap();
    assert_eq!(kids.len(), 1);
    assert_eq!(kids[0].id, SUB);
    assert!(kids[0].is_subagent);
    assert_eq!(kids[0].parent_session_id.as_deref(), Some(PARENT));
}

#[test]
fn get_session_found_and_not_found() {
    let (home, _home_g) = seed_tree();
    let (_reconcile_g, _db, conn) = reconcile(&home);
    let s = get_session(&conn, "claude-code-cli", PARENT).unwrap().expect("present");
    // Title = first user message text (importer rule).
    assert_eq!(s.title, "what is the prime goal");
    assert_eq!(s.project.as_deref(), Some("goal"));
    assert_eq!(s.message_count, 3);
    assert!(get_session(&conn, "claude-code-cli", "absent").unwrap().is_none());
    assert!(get_session(&conn, "wrong-provider", PARENT).unwrap().is_none());
}

#[test]
fn read_messages_offsets_and_limits_window() {
    let (home, _home_g) = seed_tree();
    let (_reconcile_g, _db, conn) = reconcile(&home);
    let full = read_messages(&conn, "claude-code-cli", PARENT, 0, 0).unwrap();
    assert_eq!(full.total, 3);
    assert_eq!(full.messages.len(), 3);
    // A 2-message window starting at message 1 (the assistant reply).
    let win = read_messages(&conn, "claude-code-cli", PARENT, 1, 2).unwrap();
    assert_eq!(win.total, 3);
    assert_eq!(win.messages.len(), 2);
    assert_eq!(win.messages[0].role, "assistant");
    // Beyond the end => empty window, correct total.
    let past = read_messages(&conn, "claude-code-cli", PARENT, 10, 5).unwrap();
    assert!(past.messages.is_empty());
    assert_eq!(past.total, 3);
    // Unknown session -> NotFound (no silent empty window).
    let err = read_messages(&conn, "claude-code-cli", "nope", 0, 0).unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

/// The index-only reconcile persists rollups (message_count, est_tokens,
/// top_consumer, last_model) computed from the parsed parts — the pressure
/// header reads them O(1). No `session_message`/`session_part` rows exist.
#[test]
fn reconcile_persists_index_rollups_only() {
    let (home, _home_g) = seed_tree();
    let (_reconcile_g, _db, conn) = reconcile(&home);
    let (est, top, model, count): (Option<i64>, Option<String>, Option<String>, i64) = conn
        .query_row(
            "SELECT est_tokens, top_consumer, last_model, message_count
             FROM session WHERE provider='claude-code-cli' AND id=?1",
            params![PARENT],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(count, 3);
    // Seed texts: "what is the prime goal" (22) + "a one-line answer" (17)
    // + "and the second question" (23) = 62 chars / 4.
    assert_eq!(est, Some(15), "char-based estimate (62 chars / 4)");
    assert_eq!(top.as_deref(), Some("message text"));
    assert_eq!(model, None, "seeded lines carry no model metadata");
    let mirror: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master
             WHERE type='table' AND name IN ('session_part','session_message')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(mirror, 0, "no mirror tables — dropped by the v3 migration");
}

#[test]
fn delete_session_removes_rows_and_source_files() {
    let (home, _home_g) = seed_tree();
    let (_reconcile_g, _db, conn) = reconcile(&home);
    let before = count_sessions(&conn).unwrap();
    assert_eq!(before, 3); // PARENT + OTHER + SUB

    // Track the parent's source file so we can prove disk deletion.
    let parent_src = get_session(&conn, "claude-code-cli", PARENT).unwrap().unwrap().source_path;
    assert!(std::path::Path::new(&parent_src).exists());

    let removed = delete_session(&conn, "claude-code-cli", PARENT).unwrap();
    assert!(!removed.is_empty());
    assert!(removed.contains(&parent_src), "removed list {removed:?} missing {parent_src}");
    assert!(!std::path::Path::new(&parent_src).exists(), "file should be gone from disk");

    assert!(get_session(&conn, "claude-code-cli", PARENT).unwrap().is_none());
    assert_eq!(count_sessions(&conn).unwrap(), 2);
    // The subagent row is independent and survives the parent delete.
    assert!(get_session(&conn, "claude-code-cli", SUB).unwrap().is_some());
}

#[test]
fn delete_session_errors_on_missing() {
    let (home, _home_g) = seed_tree();
    let (_reconcile_g, _db, conn) = reconcile(&home);
    let err = delete_session(&conn, "claude-code-cli", "not-here").unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[test]
fn delete_session_when_source_file_already_deleted() {
    let (home, _home_g) = seed_tree();
    let (_reconcile_g, _db, conn) = reconcile(&home);
    let s = get_session(&conn, "claude-code-cli", OTHER).unwrap().unwrap();
    std::fs::remove_file(&s.source_path).unwrap();
    // File already gone: delete still cleans DB rows without erroring.
    let removed = delete_session(&conn, "claude-code-cli", OTHER).unwrap();
    assert!(removed.contains(&s.source_path));
    assert!(get_session(&conn, "claude-code-cli", OTHER).unwrap().is_none());
}

// ---- incremental reconcile ---------------------------------------------------

/// The sentinel trick: stamp a rollup only a full-provider rewrite would
/// overwrite, then touch ONE session's source. If the incremental path works,
/// untouched sessions keep the sentinel; the touched session gets refreshed.
fn stamp_sentinel(conn: &Connection, provider: &str, id: &str) {
    conn.execute(
        "UPDATE session SET est_tokens = 987654 WHERE provider = ?1 AND id = ?2",
        params![provider, id],
    )
    .unwrap();
}

fn est_of(conn: &Connection, id: &str) -> Option<i64> {
    conn.query_row(
        "SELECT est_tokens FROM session WHERE provider='claude-code-cli' AND id = ?1",
        params![id],
        |r| r.get(0),
    )
    .unwrap()
}

/// One JSONL file changes → only that session's row is rewritten; every other
/// row (sentinel-stamped) is left byte-identical.
#[test]
fn incremental_reconcile_leaves_untouched_sessions_alone() {
    let (home, _home_g) = seed_tree();
    let (_reconcile_g, _db, conn) = reconcile(&home);
    stamp_sentinel(&conn, "claude-code-cli", PARENT);
    stamp_sentinel(&conn, "claude-code-cli", SUB);

    let other = get_session(&conn, "claude-code-cli", OTHER).unwrap().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20)); // distinct mtime
    write_lines(
        std::path::Path::new(&other.source_path),
        &[
            &claude_user_line(OTHER, "unrelated searchable alpha", "2026-08-06T09:00:00.000Z", "C:\\\\zeta"),
            &claude_assistant_line(OTHER, "zeta answer", "2026-08-06T09:00:05.000Z"),
            &claude_assistant_line(OTHER, "a later reply", "2026-08-06T09:00:09.000Z"),
        ],
    );
    with_home(&home, || reconcile_provider(&conn, "claude-code-cli").unwrap());

    // OTHER was refreshed: 3 messages, recomputed (non-sentinel) rollup.
    let refreshed = get_session(&conn, "claude-code-cli", OTHER).unwrap().unwrap();
    assert_eq!(refreshed.message_count, 3);
    assert_ne!(est_of(&conn, OTHER), Some(987654));
    // PARENT + SUB untouched.
    assert_eq!(est_of(&conn, PARENT), Some(987654), "untouched session must not be rewritten");
    assert_eq!(est_of(&conn, SUB), Some(987654), "untouched subagent must not be rewritten");
}

/// A removed source file drops exactly its session; the rest survive.
#[test]
fn incremental_reconcile_drops_session_whose_files_vanished() {
    let (home, _home_g) = seed_tree();
    let (_reconcile_g, _db, conn) = reconcile(&home);
    let other = get_session(&conn, "claude-code-cli", OTHER).unwrap().unwrap();
    std::fs::remove_file(&other.source_path).unwrap();
    with_home(&home, || reconcile_provider(&conn, "claude-code-cli").unwrap());
    assert!(get_session(&conn, "claude-code-cli", OTHER).unwrap().is_none());
    assert!(get_session(&conn, "claude-code-cli", PARENT).unwrap().is_some());
    assert!(get_session(&conn, "claude-code-cli", SUB).unwrap().is_some());
}

/// zcode/opencode: the whole provider lives in ONE db file, so its mtime
/// changes on every write. The incremental path must diff the source's own
/// `session` table instead — one new + one deleted session in the db touches
/// exactly those rows; the unchanged session keeps its sentinel.
#[test]
fn incremental_reconcile_zcode_diffs_by_session_not_db_mtime() {
    let (home, _home_g) = temp_home();
    let db = home.join(".zcode").join("cli").join("db").join("db.sqlite");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    {
        let fconn = Connection::open(&db).unwrap();
        fconn.execute_batch(
            "CREATE TABLE session (id TEXT, parent_id TEXT, directory TEXT, title TEXT,
                                  time_created INTEGER, time_updated INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER,
                                  time_updated INTEGER, data TEXT, sequence INTEGER);
             CREATE TABLE part (id TEXT, message_id TEXT, session_id TEXT, time_created INTEGER,
                               time_updated INTEGER, data TEXT, sequence INTEGER);",
        )
        .unwrap();
        for (sid, title) in [("sess-a", "A"), ("sess-b", "B")] {
            fconn.execute("INSERT INTO session VALUES (?1, NULL, 'C:/w', ?2, 100, 500)", params![sid, title]).unwrap();
            fconn.execute("INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                params![format!("m-{sid}"), sid, r#"{"role":"user"}"#]).unwrap();
            fconn.execute("INSERT INTO part (id, message_id, session_id, time_created, data) VALUES (?1, ?2, ?3, 110, ?4)",
                params![format!("p-{sid}"), format!("m-{sid}"), sid,
                        format!(r#"{{"type":"text","text":"hello {title}"}}"#)]).unwrap();
        }
    }

    let (db_dir, _db_g) = temp_home();
    let conn = crate::db::open(&db_dir).unwrap();
    crate::db::migrate(&conn).unwrap();
    with_home(&home, || reconcile_provider(&conn, "zcode-desktop").unwrap());
    assert_eq!(count_sessions(&conn).unwrap(), 2);
    stamp_sentinel(&conn, "zcode-desktop", "sess-a");
    let _ = db_dir; // keep the temp db alive

    // Mutate the source: add sess-c, delete sess-b. sess-a's time_updated
    // stays 500 — its row must not be rewritten.
    {
        let fconn = Connection::open(&db).unwrap();
        for table in ["part", "message"] {
            fconn.execute(&format!("DELETE FROM {table} WHERE session_id = 'sess-b'"), []).unwrap();
        }
        fconn.execute("DELETE FROM session WHERE id = 'sess-b'", []).unwrap();
        fconn.execute("INSERT INTO session VALUES ('sess-c', NULL, 'C:/w', 'C', 600, 700)", []).unwrap();
        fconn.execute("INSERT INTO message (id, session_id, data) VALUES ('m-c', 'sess-c', ?1)",
            params![r#"{"role":"assistant"}"#]).unwrap();
        fconn.execute("INSERT INTO part (id, message_id, session_id, time_created, data) VALUES ('p-c', 'm-c', 'sess-c', 610, ?1)",
            params![r#"{"type":"text","text":"hello C"}"#]).unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(20)); // db mtime moves
    with_home(&home, || reconcile_provider(&conn, "zcode-desktop").unwrap());

    assert!(get_session(&conn, "zcode-desktop", "sess-c").unwrap().is_some(), "new session indexed");
    assert!(get_session(&conn, "zcode-desktop", "sess-b").unwrap().is_none(), "deleted session dropped");
    let a = get_session(&conn, "zcode-desktop", "sess-a").unwrap().unwrap();
    assert_eq!(a.message_count, 1, "sess-a refreshed? no — it must keep its sentinel");
    let est: Option<i64> = conn.query_row(
        "SELECT est_tokens FROM session WHERE provider='zcode-desktop' AND id='sess-a'",
        [], |r| r.get(0),
    ).unwrap();
    assert_eq!(est, Some(987654), "db mtime changed, but sess-a's time_updated did not — no rewrite");
}
