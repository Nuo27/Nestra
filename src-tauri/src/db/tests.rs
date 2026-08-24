use super::*;

fn agent_columns(conn: &Connection) -> Vec<String> {
    conn.prepare("PRAGMA table_info(agent)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .filter_map(Result::ok)
        .collect()
}

/// Assert the active-provider invariant for an agent: the canonical source
/// of truth is `agent_provider_binding.active=1` — at most one binding may
/// be active, and `list_agents` derives an `active_provider_id` that
/// matches it (or both are empty).
fn assert_active_invariant(conn: &Connection, agent_id: &str) {
    let active_bindings: Vec<String> = conn
        .prepare("SELECT endpoint_id FROM agent_provider_binding WHERE agent_id = ?1 AND active = 1")
        .unwrap()
        .query_map(rusqlite::params![agent_id], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        active_bindings.len() <= 1,
        "agent {agent_id}: at most one binding may be active, found {}: {active_bindings:?}",
        active_bindings.len()
    );
    let binding_active = active_bindings.into_iter().next();
    // list_agents derives active_provider_id from the binding table; it
    // must match the binding's active row exactly.
    let derived = list_agents(conn)
        .unwrap()
        .into_iter()
        .find(|a| a.id == agent_id)
        .map(|a| a.active_provider_id)
        .flatten();
    assert_eq!(
        derived, binding_active,
        "agent {agent_id}: list_agents active_provider_id ({derived:?}) must equal the active binding ({binding_active:?})"
    );
}

fn seed_two_endpoints(conn: &Connection) {
    create_endpoint(conn, "ep-1", "anthropic", "One").unwrap();
    upsert_endpoint_protocol(conn, "ep-1", "anthropic", "https://one").unwrap();
    create_endpoint(conn, "ep-2", "anthropic", "Two").unwrap();
    upsert_endpoint_protocol(conn, "ep-2", "anthropic", "https://two").unwrap();
}

/// The canonical active-provider invariant must hold across every write
/// path: exactly one active binding, none, switching, replacing, and
/// deleting the active binding. The active provider is derived solely from
/// `agent_provider_binding.active=1`; `list_agents` must surface it
/// consistently. This is the gate the single-source-of-truth model relies
/// on.
#[test]
fn active_provider_invariant_holds_across_all_write_paths() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    seed_two_endpoints(&conn);
    // The registry seeds the agent row for claude-code at migrate time.
    const A: &str = "claude-code-cli";

    // (a) no active binding: both empty.
    assert_active_invariant(&conn, A);

    // (b) exactly one active binding: switch to ep-1.
    upsert_binding(&conn, A, "ep-1").unwrap();
    set_active_binding(&conn, A, "ep-1").unwrap();
    assert_active_invariant(&conn, A);

    // (c) switching active providers: ep-1 → ep-2.
    upsert_binding(&conn, A, "ep-2").unwrap();
    set_active_binding(&conn, A, "ep-2").unwrap();
    assert_active_invariant(&conn, A);
    // ep-1 must no longer be active.
    let ep1_active: i64 = conn
        .query_row(
            "SELECT active FROM agent_provider_binding WHERE agent_id=?1 AND endpoint_id='ep-1'",
            rusqlite::params![A],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ep1_active, 0);

    // (d) replacing the binding set: select both, default ep-1.
    replace_bindings(
        &conn,
        A,
        &[("ep-1".into(), None), ("ep-2".into(), None)],
        "ep-1",
    )
    .unwrap();
    assert_active_invariant(&conn, A);

    // (d') replacing with an empty selection clears the active pointer.
    replace_bindings(&conn, A, &[], "ep-1").unwrap();
    assert_active_invariant(&conn, A);
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM agent_provider_binding WHERE agent_id=?1",
            rusqlite::params![A],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "empty replace must drop all bindings");

    // (e) deleting the active binding: re-bind ep-1, then clear_active_binding.
    upsert_binding(&conn, A, "ep-1").unwrap();
    set_active_binding(&conn, A, "ep-1").unwrap();
    assert_active_invariant(&conn, A);
    let removed = clear_active_binding(&conn, A).unwrap();
    assert_eq!(removed.as_deref(), Some("ep-1"));
    assert_active_invariant(&conn, A);

    // (f) clear_all_bindings also clears the active pointer.
    upsert_binding(&conn, A, "ep-2").unwrap();
    set_active_binding(&conn, A, "ep-2").unwrap();
    assert_active_invariant(&conn, A);
    clear_all_bindings(&conn, A).unwrap();
    assert_active_invariant(&conn, A);
}

#[test]
fn migration_adds_factory_and_status_detail_columns_and_seeds() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    let cols = agent_columns(&conn);
    assert!(cols.contains(&"factory_backup_path".into()));
    assert!(cols.contains(&"status_detail".into()));
    assert!(cols.contains(&"path".into()));
    assert!(cols.contains(&"path_override".into()));
    let ids: Vec<String> = list_agents(&conn).unwrap().into_iter().map(|c| c.id).collect();
    // The agent table is seeded from the registry: exactly the declared
    // agents are present, every one as a `missing`-status placeholder
    // until detection runs.
    let mut registry_ids: Vec<String> = crate::agents::agents()
        .iter()
        .map(|a| a.id.to_string())
        .collect();
    registry_ids.sort();
    assert_eq!(ids, registry_ids, "agent table must mirror the registry (set-wise)");
    for row in list_agents(&conn).unwrap() {
        assert_eq!(row.status, "missing");
    }
}

/// Regression: `list_bindings` / `list_all_bindings` must surface the
/// joined endpoint protocol under the `resolved_protocol` column that
/// `row_to_binding` reads. Alias drift here was a silent runtime failure
/// on every tray rebuild / providers refresh.
#[test]
fn binding_list_resolves_endpoint_protocol_columns() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    create_endpoint(&conn, "ep-1", "anthropic", "Anthropic main").unwrap();
    upsert_endpoint_protocol(&conn, "ep-1", "anthropic", "https://api.example.com").unwrap();
    upsert_binding(&conn, "claude-code-cli", "ep-1").unwrap();

    let rows = list_bindings(&conn, "claude-code-cli").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].resolved_protocol.as_deref(), Some("anthropic"));
    assert_eq!(
        rows[0].resolved_base_url.as_deref(),
        Some("https://api.example.com")
    );

    let all = list_all_bindings(&conn).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].resolved_protocol.as_deref(), Some("anthropic"));
}

/// v22 adds `model_abilities_json` to `provider_endpoint`. Round-trips
/// through the setter and exposes it on the row getter — the OpenCode
/// writer's whole reason for existing.
#[test]
fn model_abilities_json_round_trips() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    create_endpoint(&conn, "ep", "custom", "ep").unwrap();
    // Default: no override saved → None.
    assert!(get_endpoint(&conn, "ep").unwrap().unwrap().model_abilities_json.is_none());
    let payload = r#"{"MiniMax-M3":{"reasoning":true,"tool_call":true}}"#;
    set_endpoint_model_abilities(&conn, "ep", Some(payload)).unwrap();
    assert_eq!(
        get_endpoint(&conn, "ep").unwrap().unwrap().model_abilities_json.as_deref(),
        Some(payload)
    );
    // Clearing restores None.
    set_endpoint_model_abilities(&conn, "ep", None).unwrap();
    assert!(get_endpoint(&conn, "ep").unwrap().unwrap().model_abilities_json.is_none());
}

/// `prune_observability_data` must delete runs — cascading to task /
/// route_request / route_migration — plus logical_sessions older than the
/// retention window, while keeping recent rows. This is the backing
/// implementation of the `log_retention_days` UI setting.
#[test]
fn prune_observability_data_respects_retention_window() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    // Cascade deletes require foreign_keys=ON (set by db::open in prod).
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();

    let now = chrono::Utc::now().timestamp_millis();
    let old = now - 40 * 86_400_000; // 40 days ago
    let recent = now - 86_400_000; // 1 day ago

    // Old task + cascaded children.
    conn.execute(
        "INSERT INTO task (id, started_at, ended_at)
             VALUES ('task-old', ?1, ?1)",
        rusqlite::params![old],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO route_request (request_id, task_id, agent_id, route_reason, started_at, ended_at)
             VALUES ('req-old', 'task-old', 'claude-code-cli', 'test', ?1, ?1)",
        rusqlite::params![old],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO route_migration (id, request_id, task_id, reason, at_ms)
             VALUES ('mig-old', 'req-old', 'task-old', 'quota_exhausted', ?1)",
        rusqlite::params![old],
    )
    .unwrap();

    // Recent task + route_request (no migration — not every request migrates).
    conn.execute(
        "INSERT INTO task (id, started_at)
             VALUES ('task-recent', ?1)",
        rusqlite::params![recent],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO route_request (request_id, task_id, agent_id, route_reason, started_at)
             VALUES ('req-recent', 'task-recent', 'claude-code-cli', 'test', ?1)",
        rusqlite::params![recent],
    )
    .unwrap();

    // Set retention to 7 days.
    set_setting(&conn, "log_retention_days", &serde_json::json!(7)).unwrap();

    let pruned = prune_observability_data(&conn).unwrap();
    // 1 old task = 1 direct delete (cascades are extra).
    assert_eq!(pruned, 1);

    // Old rows gone — cascade cleaned route_request / route_migration.
    assert_eq!(count(&conn, "task", "id = 'task-old'"), 0);
    assert_eq!(count(&conn, "route_request", "request_id = 'req-old'"), 0);
    assert_eq!(count(&conn, "route_migration", "id = 'mig-old'"), 0);

    // Recent rows survive.
    assert_eq!(count(&conn, "task", "id = 'task-recent'"), 1);
    assert_eq!(count(&conn, "route_request", "request_id = 'req-recent'"), 1);
}

/// When `log_retention_days` is unset, the default (30 days) applies.
#[test]
fn prune_defaults_to_30_days_when_setting_absent() {
    let conn = Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();

    let now = chrono::Utc::now().timestamp_millis();
    // 31 days old — just outside the 30-day default window.
    let old = now - 31 * 86_400_000;
    conn.execute(
        "INSERT INTO task (id, started_at)
             VALUES ('t', ?1)",
        rusqlite::params![old],
    )
    .unwrap();
    // No log_retention_days set → default 30.
    let pruned = prune_observability_data(&conn).unwrap();
    assert_eq!(
        pruned, 1,
        "31-day-old task should be pruned by the default 30-day window"
    );
}

fn count(conn: &Connection, table: &str, where_clause: &str) -> i64 {
    let sql = format!("SELECT count(*) FROM {table} WHERE {where_clause}");
    conn.query_row(&sql, [], |r| r.get(0)).unwrap()
}

// ---- pre-migration snapshot -------------------------------------------------

/// A version-mismatched on-disk database gets snapshotted BEFORE the
/// migrator refuses it — the file copy is the user's recovery path when a
/// future v2 migration goes wrong.
#[test]
fn migrate_snapshots_version_mismatched_db_before_refusing() {
    let tmp = tempfile::tempdir().unwrap();
    let conn = Connection::open(tmp.path().join("nestra.db")).unwrap();
    crate::schema::build_v1(&conn).unwrap();
    // Fake a pre-release version: build_v1 wrote version 1; bump it so the
    // migrator sees a mismatch.
    conn.execute_batch("UPDATE schema_version SET version = 99;")
        .unwrap();

    let res = migrate(&conn);
    assert!(res.is_err(), "non-v1 database must still be refused");

    let backups = tmp.path().join("db_backups");
    let mut snaps: Vec<_> = std::fs::read_dir(&backups)
        .expect("db_backups created")
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(snaps.len(), 1, "exactly one snapshot taken");
    let snap = snaps.remove(0);
    assert!(
        snap.to_string_lossy().contains("v99-to-v"),
        "snapshot name records the version transition: {}",
        snap.display()
    );
    assert!(snap.join("nestra.db").exists(), "snapshot holds the db file");
}

/// Same-version and fresh databases take no snapshot — the safety net must
/// not litter the data dir on every launch.
#[test]
fn migrate_takes_no_snapshot_when_version_matches() {
    let tmp = tempfile::tempdir().unwrap();
    let conn = Connection::open(tmp.path().join("nestra.db")).unwrap();
    migrate(&conn).unwrap(); // fresh → build_v1
    migrate(&conn).unwrap(); // v1 → idempotent rebuild
    assert!(
        !tmp.path().join("db_backups").exists(),
        "no snapshot without a version change"
    );
}

#[test]
fn backup_rotation_keeps_newest_three() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("db_backups");
    std::fs::create_dir_all(root.join("v1-to-v2-100")).unwrap();
    std::fs::create_dir_all(root.join("v1-to-v2-200")).unwrap();
    std::fs::create_dir_all(root.join("v1-to-v2-300")).unwrap();
    std::fs::create_dir_all(root.join("v1-to-v2-400")).unwrap();
    std::fs::create_dir_all(root.join("v1-to-v2-500")).unwrap();

    rotate_backup_snapshots(&root);

    let mut left: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, vec!["v1-to-v2-300", "v1-to-v2-400", "v1-to-v2-500"]);
}
/// Prune folds departing usage into `usage_daily` exactly once: sums land,
/// rows leave, and a re-run changes nothing.
#[test]
fn prune_folds_usage_exactly_once() {
    let tmp = tempfile::tempdir().unwrap();
    let conn = Connection::open(tmp.path().join("nestra.db")).unwrap();
    migrate(&conn).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let old = now - 40 * 86_400_000;

    conn.execute(
        "INSERT INTO task (id, lifecycle, started_at) VALUES ('t-old','done',?1)",
        rusqlite::params![old],
    )
    .unwrap();
    for (i, (model, inp, out)) in [("m-a", 100, 200), ("m-b", 10, 20)].iter().enumerate() {
        conn.execute(
            "INSERT INTO route_request
               (request_id, task_id, agent_id, resolved_model, route_reason,
                started_at, usage_input, usage_output)
             VALUES (?1, 't-old', 'pi-cli', ?2, 'policy', ?3, ?4, ?5)",
            rusqlite::params![format!("r-{i}"), model, old, inp, out],
        )
        .unwrap();
    }

    let pruned = prune_observability_data(&conn).unwrap();
    assert_eq!(pruned, 1, "one old task pruned");

    let (reqs, total_in): (i64, i64) = conn
        .query_row(
            "SELECT sum(requests), sum(usage_input) FROM usage_daily",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((reqs, total_in), (2, 110), "both requests folded with their tokens");

    let left: i64 = conn
        .query_row("SELECT count(*) FROM route_request", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0, "source rows deleted after folding");

    // Re-run: nothing left to fold, sums unchanged (exactly-once).
    prune_observability_data(&conn).unwrap();
    let total_in2: i64 = conn
        .query_row("SELECT sum(usage_input) FROM usage_daily", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total_in2, 110, "no double count on re-run");
}
