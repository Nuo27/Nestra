use super::*;

/// The canonical schema must build cleanly on an empty DB and be idempotent.
#[test]
fn build_v1_is_clean_and_idempotent() {
    let conn = Connection::open_in_memory().unwrap();
    build_v1(&conn).unwrap();
    // Re-running is a no-op (every statement is IF NOT EXISTS).
    build_v1(&conn).unwrap();
    let v: i32 = conn
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
}

/// A fresh DB (no schema_version table) reports `None`, and migrate()
/// builds the canonical schema for it.
#[test]
fn migrate_empty_db_builds_v1() {
    let conn = Connection::open_in_memory().unwrap();
    assert!(on_disk_version(&conn).unwrap().is_none());
    migrate(&conn).unwrap();
    let v: i32 = conn
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
    // Orchestration tables exist.
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='route_request'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

/// Drift repair: a pre-existing v1 DB whose `mcp_server` predates the
/// `disabled_agents` column must get it backfilled by `build_v1`, with the
/// existing rows surviving and defaulting to `'[]'`.
#[test]
fn build_v1_backfills_mcp_disabled_agents_on_drifted_db() {
    let conn = Connection::open_in_memory().unwrap();
    // Simulate the drifted install: an `mcp_server` table without the
    // tri-state `disabled_agents` column, plus one managed row.
    conn.execute_batch(
        "CREATE TABLE mcp_server (
                id             TEXT PRIMARY KEY,
                name           TEXT NOT NULL,
                transport_json TEXT NOT NULL,
                enabled_agents TEXT NOT NULL DEFAULT '[]',
                created_at     INTEGER NOT NULL
             );
             INSERT INTO mcp_server (id, name, transport_json, created_at)
             VALUES ('s1', 'codegraph', '{}', 0);",
    )
    .unwrap();

    build_v1(&conn).unwrap();

    // The column now exists...
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('mcp_server') WHERE name='disabled_agents'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "disabled_agents must be backfilled onto a drifted table");
    // ...the existing row survives with the default value...
    let v: String = conn
        .query_row(
            "SELECT disabled_agents FROM mcp_server WHERE id='s1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v, "[]");
    // ...and re-running is a no-op (no duplicate column error).
    build_v1(&conn).unwrap();
}

#[test]
fn build_v1_backfills_route_request_tool_calls_on_drifted_db() {
    let conn = Connection::open_in_memory().unwrap();
    // Simulate the drifted install: a v1 `route_request` without the
    // streaming `tool_calls` column (every other column present, so the
    // canonical DDL's index statements still apply — only `ensure_column`
    // can add the missing one).
    conn.execute_batch(
        "CREATE TABLE route_request (
                request_id        TEXT PRIMARY KEY,
                task_id           TEXT NOT NULL,
                agent_id          TEXT NOT NULL,
                logical_session   TEXT,
                subagent_role     TEXT,
                role_source       TEXT,
                requested_model   TEXT,
                requested_provider TEXT,
                resolved_endpoint_id TEXT,
                resolved_model    TEXT,
                protocol          TEXT,
                route_reason      TEXT NOT NULL,
                http_status       INTEGER,
                usage_input       INTEGER,
                usage_output      INTEGER,
                cache_creation    INTEGER,
                cache_read        INTEGER,
                generation_broken INTEGER NOT NULL DEFAULT 0,
                started_at        INTEGER NOT NULL,
                ended_at          INTEGER
             );",
    )
    .unwrap();

    build_v1(&conn).unwrap();

    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('route_request') WHERE name='tool_calls'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "tool_calls must be backfilled onto a drifted table");
    // Re-running is a no-op (no duplicate column error).
    build_v1(&conn).unwrap();
}

/// P1-1 dual-path verification: `tool_names` round-trips on a FRESH db
/// (full canonical schema) and on an EXISTING db upgraded through
/// `ensure_column` — insert + read back in both cases.
#[test]
fn tool_names_round_trips_on_fresh_and_upgraded_dbs() {
    for upgraded in [false, true] {
        let conn = Connection::open_in_memory().unwrap();
        if upgraded {
            // Existing install: the v1 table WITHOUT the P1-1 columns,
            // then build_v1's ensure_column upgrades it in place.
            conn.execute_batch(
                "CREATE TABLE route_request (
                        request_id        TEXT PRIMARY KEY,
                        task_id           TEXT NOT NULL,
                        agent_id          TEXT NOT NULL,
                        logical_session   TEXT,
                        subagent_role     TEXT,
                        role_source       TEXT,
                        requested_model   TEXT,
                        requested_provider TEXT,
                        resolved_endpoint_id TEXT,
                        resolved_model    TEXT,
                        protocol          TEXT,
                        route_reason      TEXT NOT NULL,
                        http_status       INTEGER,
                        usage_input       INTEGER,
                        usage_output      INTEGER,
                        cache_creation    INTEGER,
                        cache_read        INTEGER,
                        tool_calls        INTEGER,
                        generation_broken INTEGER NOT NULL DEFAULT 0,
                        started_at        INTEGER NOT NULL,
                        ended_at          INTEGER
                     );",
            )
            .unwrap();
        }
        build_v1(&conn).unwrap();

        let rec = crate::orchestration::identity::RouteRecord {
            request_id: uuid::Uuid::new_v4(),
            task_id: uuid::Uuid::new_v4(),
            agent_id: "pi-cli".into(),
            logical_session: None,
            subagent_role: None,
            role_source: None,
            requested_model: None,
            requested_provider: None,
            resolved_endpoint_id: None,
            resolved_model: None,
            protocol: None,
            route_reason: "explicit".into(),
            http_status: None,
            usage_input: None,
            usage_output: None,
            cache_creation: None,
            cache_read: None,
            tool_calls: Some(2),
            tool_names: Some(r#"{"mcp__fs__read": 2}"#.into()),
            generation_broken: false,
            started_at: 1,
            ended_at: None,
        };
        // route_request.task_id has an FK to task(id).
        conn.execute(
            "INSERT INTO task (id, lifecycle, started_at) VALUES (?1,'born',0)",
            rusqlite::params![rec.task_id.to_string()],
        )
        .unwrap();
        crate::orchestration::store::insert_route_request(&conn, &rec).unwrap();
        let got = crate::orchestration::store::route_history_for_task(
            &conn,
            &rec.task_id.to_string(),
        )
        .unwrap();
        assert_eq!(got.len(), 1, "upgraded={upgraded}");
        assert_eq!(got[0].tool_calls, Some(2));
        assert_eq!(got[0].tool_names.as_deref(), Some(r#"{"mcp__fs__read": 2}"#));
    }
}

#[test]
fn build_v1_recreates_handoff_table_on_pre_handoff_db() {
    let conn = Connection::open_in_memory().unwrap();
    build_v1(&conn).unwrap();
    // Simulate a pre-handoff install (0.1.1) — no `handoff` table.
    conn.execute_batch("DROP TABLE handoff;").unwrap();
    build_v1(&conn).unwrap();
    // The table is back and accepts a row (patch-release additive backfill;
    // new tables ride the `CREATE TABLE IF NOT EXISTS` in the batch).
    conn.execute(
        "INSERT INTO handoff (id, source_provider, source_session_id, artifact_path,
                                  sections_json, created_at)
             VALUES ('h1','pi-cli','s1','/tmp/h1.md','{}',0)",
        [],
    )
    .unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM handoff", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn build_v1_recreates_review_table_on_pre_review_db() {
    let conn = Connection::open_in_memory().unwrap();
    build_v1(&conn).unwrap();
    conn.execute_batch("DROP TABLE review;").unwrap();
    build_v1(&conn).unwrap();
    conn.execute(
        "INSERT INTO review (id, agent_id, reviewed_session_provider, reviewed_session_id,
                                 status, created_at)
             VALUES ('r1','pi-cli','pi-cli','s1','pending',0)",
        [],
    )
    .unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM review", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

/// Re-running migrate on an already-v1 DB is a no-op (data preserved).
#[test]
fn migrate_on_v1_is_noop() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    conn.execute(
        "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status)
             VALUES ('ep-x','custom','X',0,'unvalidated')",
        [],
    )
    .unwrap();
    // Second migrate must not drop the row.
    migrate(&conn).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM provider_endpoint WHERE id='ep-x'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

/// Credential-boundary guard: no orchestration table may carry a column
/// whose name suggests a secret. This is a static check over the canonical
/// DDL so a future contributor cannot accidentally add one without
/// updating this test.
#[test]
fn no_secret_columns_in_orchestration_tables() {
    let forbidden_substrings = ["key", "secret", "credential", "token", "password", "passwd"];
    let orchestration_tables = [
        "routing_policy",
        "task",
        "route_request",
        "route_migration",
        "model_catalog",
    ];
    // Parse column names out of SCHEMA_V1 for the orchestration tables.
    for table in orchestration_tables {
        let block = extract_table_block(SCHEMA_V1, table);
        assert!(!block.is_empty(), "table {table} missing from SCHEMA_V1");
        for line in block.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
                continue; // not a column line
            }
            let col_name = trimmed
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
            if col_name.is_empty() {
                continue;
            }
            let lower = col_name.to_ascii_lowercase();
            for bad in forbidden_substrings {
                assert!(
                    !lower.contains(bad),
                    "orchestration table {table} column {col_name} contains forbidden substring {bad:?} (credential boundary)"
                );
            }
        }
    }
}

/// Crude extraction of one `CREATE TABLE` block by table name. Returns the
/// inner column-definition lines (between the parens), not including the
/// `PRIMARY KEY` / `FOREIGN KEY` / `CREATE INDEX` lines.
fn extract_table_block(ddl: &str, table: &str) -> String {
    let needle = format!("CREATE TABLE IF NOT EXISTS {table} ");
    let start = match ddl.find(&needle) {
        Some(i) => i,
        None => return String::new(),
    };
    let after_open = match ddl[start..].find('(') {
        Some(i) => start + i + 1,
        None => return String::new(),
    };
    // find the matching close paren at depth 0
    let mut depth = 1isize;
    let mut end = after_open;
    for (i, c) in ddl[after_open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = after_open + i;
                    break;
                }
            }
            _ => {}
        }
    }
    ddl[after_open..end].to_string()
}

/// Fresh-data policy: a pre-release database (any version other than 1) is
/// refused. migrate() must return an error, must NOT modify the database,
/// and the original tables/rows must be intact afterwards.
#[test]
fn pre_release_database_is_refused_and_left_intact() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
             CREATE TABLE provider_endpoint (id TEXT PRIMARY KEY, display_name TEXT);
             INSERT INTO schema_version (version, applied_at) VALUES (23, 0);
             INSERT INTO provider_endpoint (id, display_name) VALUES ('ep-1','Main');",
    )
    .unwrap();

    // migrate() must refuse (pre-release version 23).
    let err = migrate(&conn);
    assert!(err.is_err(), "pre-release DB must be refused");

    // The database must be left intact (not modified, not rebuilt).
    let v: i64 = conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 23, "original schema_version must be untouched");
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM provider_endpoint WHERE id='ep-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "original data must be untouched");
    // The canonical v1 tables must NOT have been created.
    let has_route: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='route_request'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_route, 0, "no canonical tables created on refusal");
}

/// Fresh-data policy: a future/unknown version is also refused.
#[test]
fn unknown_version_is_refused() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
             INSERT INTO schema_version (version, applied_at) VALUES (99, 0);",
    )
    .unwrap();
    assert!(migrate(&conn).is_err());
}

/// The Sessions-list query must use `idx_session_top_level` — i.e. an
/// index range scan instead of a full scan + filesort. Pins the index
/// optimization so a future schema edit can't silently regress it.
#[test]
fn session_list_query_uses_top_level_index() {
    let conn = Connection::open_in_memory().unwrap();
    build_v1(&conn).unwrap();
    for i in 0..8 {
        conn.execute(
            "INSERT INTO session
                   (provider, id, title, summary, message_count, source_path,
                    resume_command, started_at, updated_at)
                 VALUES ('p', ?1, 't', '', 1, 'x', 'y', 0, ?2)",
            rusqlite::params![format!("s{i}"), i * 1000],
        )
        .unwrap();
    }
    let plan: String = conn
        .query_row(
            "EXPLAIN QUERY PLAN
                 SELECT provider FROM session WHERE is_subagent = 0
                 ORDER BY updated_at DESC LIMIT 300",
            [],
            |r| r.get(3), // column 3 = detail (col 2 is the "notused" int in modern SQLite)
        )
        .unwrap();
    assert!(
        plan.contains("idx_session_top_level"),
        "expected idx_session_top_level in query plan, got: {plan}"
    );
}

/// A pre-optimization database may still carry the old redundant /
/// superseded indexes. build_v1 must drop them and create the new
/// composites, idempotently.
#[test]
fn build_v1_drops_legacy_indexes_and_creates_new() {
    let conn = Connection::open_in_memory().unwrap();
    build_v1(&conn).unwrap();
    // Simulate a pre-optimization install: re-add the legacy indexes.
    conn.execute_batch(
        "CREATE INDEX idx_session_message ON session_message(provider, session_id, seq);
             CREATE INDEX idx_agent_provider_binding_agent ON agent_provider_binding(agent_id);
             CREATE INDEX idx_route_request_task ON route_request(task_id);",
    )
    .unwrap();
    build_v1(&conn).unwrap();
    let names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='index'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for legacy in ["idx_session_message", "idx_agent_provider_binding_agent", "idx_route_request_task"] {
        assert!(!names.iter().any(|n| n == legacy), "{legacy} should be dropped");
    }
    for keep in [
        "idx_session_top_level",
        "idx_route_request_task_started",
        "idx_route_request_logical_session",
    ] {
        assert!(names.iter().any(|n| n == keep), "{keep} should exist");
    }
}