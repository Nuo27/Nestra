use super::*;
use crate::schema;

fn fresh_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::build_v1(&conn).unwrap();
    conn
}

#[test]
fn routing_policy_upsert_get_fallback() {
    let conn = fresh_db();
    let now = 1_700_000_000;
    // No row → falls back to a synthesized default (task affinity, no injection).
    let p = routing_policy_for(&conn, "claude-code-cli", "claude:researcher", None).unwrap();
    assert_eq!(p.affinity_scope, "task");
    assert!(!p.inject_cache_control);
    assert!(p.migrate_on_quota);

    // Insert a per-role row + a catch-all row.
    upsert_routing_policy(
        &conn,
        &RoutingPolicyRow {
            agent_id: "claude-code-cli".into(),
            role: "claude:researcher".into(),
            route_targets: Some(r#"[{\"endpoint\":\"ep-1\",\"model\":\"m\"},{\"endpoint\":\"ep-2\",\"model\":\"m\"}]"#.into()),
            migrate_on_quota: false,
            inject_cache_control: true,
            affinity_scope: "session".into(),
            updated_at: now,
        },
    )
    .unwrap();
    upsert_routing_policy(
        &conn,
        &RoutingPolicyRow {
            agent_id: "claude-code-cli".into(),
            role: "*".into(),
            route_targets: Some(r#"[{\"endpoint\":\"ep-x\",\"model\":\"m\"}]"#.into()),
            migrate_on_quota: true,
            inject_cache_control: false,
            affinity_scope: "task".into(),
            updated_at: now,
        },
    )
    .unwrap();

    // Specific role wins.
    let p = routing_policy_for(&conn, "claude-code-cli", "claude:researcher", None).unwrap();
    assert_eq!(p.role, "claude:researcher");
    assert!(p.inject_cache_control);
    assert!(!p.migrate_on_quota);
    assert_eq!(p.affinity_scope, "session");

    // Unknown role falls back to the catch-all.
    let p = routing_policy_for(&conn, "claude-code-cli", "claude:other", None).unwrap();
    assert_eq!(p.role, "*");
    assert!(!p.inject_cache_control);
    assert_eq!(
        p.route_targets.as_deref(),
        Some(r#"[{\"endpoint\":\"ep-x\",\"model\":\"m\"}]"#)
    );
}

/// Lookup specificity with a budget tier in play: exact role > tier row >
/// `*` catch-all > synthesized default.
#[test]
fn routing_policy_tier_sits_between_role_and_catch_all() {
    use super::super::identity::BudgetTier;
    let conn = fresh_db();
    let now = 1_700_000_000;
    for role in ["tier:haiku", "*"] {
        upsert_routing_policy(
            &conn,
            &RoutingPolicyRow {
                agent_id: "claude-code-cli".into(),
                role: role.into(),
                route_targets: Some(format!(r#"[{{\"endpoint\":\"{role}-ep\",\"model\":\"m\"}}]"#).into()),
                migrate_on_quota: true,
                inject_cache_control: false,
                affinity_scope: "task".into(),
                updated_at: now,
            },
        )
        .unwrap();
    }

    // No tier → straight to the catch-all.
    let p = routing_policy_for(&conn, "claude-code-cli", "main", None).unwrap();
    assert_eq!(p.role, "*");
    // Haiku tier → the tier row (between exact role and catch-all).
    let p =
        routing_policy_for(&conn, "claude-code-cli", "main", Some(&BudgetTier::Haiku)).unwrap();
    assert_eq!(p.role, "tier:haiku");
    // An exact role row still outranks the tier row.
    upsert_routing_policy(
        &conn,
        &RoutingPolicyRow {
            agent_id: "claude-code-cli".into(),
            role: "claude:researcher".into(),
            route_targets: Some(r#"[{\"endpoint\":\"role-ep\",\"model\":\"m\"}]"#.into()),
            migrate_on_quota: true,
            inject_cache_control: false,
            affinity_scope: "task".into(),
            updated_at: now,
        },
    )
    .unwrap();
    let p = routing_policy_for(
        &conn,
        "claude-code-cli",
        "claude:researcher",
        Some(&BudgetTier::Haiku),
    )
    .unwrap();
    assert_eq!(p.role, "claude:researcher");
}

#[test]
fn task_lifecycle_sets_ended_at_only_on_terminal() {
    let conn = fresh_db();
    insert_task(
        &conn,
        &TaskRow {
            id: "task-1".into(),
            parent_task_id: None,
            lifecycle: "born".into(),
            native_task_ref: None,
            started_at: 100,
            ended_at: None,
        },
    )
    .unwrap();

    // Non-terminal: ended_at stays NULL.
    set_task_lifecycle(&conn, "task-1", TaskLifecycle::InFlight, 200).unwrap();
    let t = get_task(&conn, "task-1").unwrap().unwrap();
    assert_eq!(t.lifecycle, "inflight");
    assert!(t.ended_at.is_none());

    // Terminal: ended_at set.
    set_task_lifecycle(&conn, "task-1", TaskLifecycle::Done, 300).unwrap();
    let t = get_task(&conn, "task-1").unwrap().unwrap();
    assert_eq!(t.lifecycle, "done");
    assert_eq!(t.ended_at, Some(300));
}

/// Seed the task row a route_request row needs to satisfy its FKs
/// (optionally resolved_endpoint_id → provider_endpoint).
fn seed_task_chain(conn: &Connection, task_id: &str, with_endpoint: bool) {
    if with_endpoint {
        conn.execute(
            "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status)
                 VALUES ('ep-1','custom','Main',0,'unvalidated')",
            [],
        )
        .unwrap();
    }
    insert_task(
        conn,
        &TaskRow {
            id: task_id.into(),
            parent_task_id: None,
            lifecycle: "inflight".into(),
            native_task_ref: None,
            started_at: 0,
            ended_at: None,
        },
    )
    .unwrap();
}

#[test]
fn mark_aborted_flips_only_open_attempts() {
    // A post-completion agent disconnect (hyper can drop the response body
    // without polling its end frame) must not overwrite a terminal outcome:
    // only a still-open (NULL status) attempt flips to 499.
    let conn = fresh_db();
    let task_id = uuid::Uuid::new_v4();
    seed_task_chain(&conn, &task_id.to_string(), false);
    let rec = |status: Option<i64>| RouteRecord {
        request_id: uuid::Uuid::new_v4(),
        task_id,
        agent_id: "codex-desktop".into(),
        logical_session: None,
        subagent_role: Some("main".into()),
        role_source: Some("heuristic".into()),
        requested_model: Some("nestra".into()),
        requested_provider: None,
        resolved_endpoint_id: None,
        resolved_model: None,
        protocol: None,
        route_reason: "policy".into(),
        http_status: status,
        usage_input: None,
        usage_output: None,
        cache_creation: None,
        cache_read: None,
        tool_calls: None,
        tool_names: None,
        generation_broken: false,
        started_at: 100,
        ended_at: None,
    };
    let open = rec(None);
    let done = rec(Some(200));
    insert_route_request(&conn, &open).unwrap();
    insert_route_request(&conn, &done).unwrap();

    assert!(
        mark_route_request_aborted_if_open(&conn, &open.request_id.to_string(), 300).unwrap(),
        "an open attempt flips to 499"
    );
    assert!(
        !mark_route_request_aborted_if_open(&conn, &done.request_id.to_string(), 300).unwrap(),
        "a terminal attempt is left alone"
    );

    let status = |rid: &str| {
        conn.query_row(
            "SELECT http_status FROM route_request WHERE request_id = ?1",
            rusqlite::params![rid],
            |r| r.get::<_, Option<i64>>(0),
        )
        .unwrap()
    };
    assert_eq!(status(&open.request_id.to_string()), Some(499));
    assert_eq!(status(&done.request_id.to_string()), Some(200));
}

#[test]
fn route_request_round_trip_and_history() {
    let conn = fresh_db();
    let task_id = uuid::Uuid::new_v4();
    // Seed the parent run/task + endpoint the FKs point at.
    seed_task_chain(&conn, &task_id.to_string(), true);
    let rec = RouteRecord {
        request_id: uuid::Uuid::new_v4(),
        task_id,
        agent_id: "claude-code-cli".into(),
        logical_session: Some("sess-1".into()),
        subagent_role: Some("main".into()),
        role_source: Some("native".into()),
        requested_model: Some("claude-3-opus".into()),
        requested_provider: None,
        resolved_endpoint_id: Some("ep-1".into()),
        resolved_model: Some("claude-3-opus".into()),
        protocol: Some("anthropic".into()),
        route_reason: "capability".into(),
        http_status: None,
        usage_input: None,
        usage_output: None,
        cache_creation: None,
        cache_read: None,
        tool_calls: None,
        tool_names: None,
        generation_broken: false,
        started_at: 100,
        ended_at: None,
    };
    insert_route_request(&conn, &rec).unwrap();

    // Backfill outcome (buffered-path semantics: tools ride the finalize).
    update_route_request_outcome(
        &conn,
        &rec.request_id.to_string(),
        Some(200),
        Some(123),
        Some(456),
        Some(0),
        Some(100),
        Some(3),
        Some(r#"{"Bash": 3}"#.to_string()),
        false,
        200,
    )
    .unwrap();

    let hist = route_history_for_task(&conn, &rec.task_id.to_string()).unwrap();
    assert_eq!(hist.len(), 1);
    let h = &hist[0];
    assert_eq!(h.http_status, Some(200));
    assert_eq!(h.usage_input, Some(123));
    assert_eq!(h.tool_calls, Some(3));
    assert_eq!(h.tool_names.as_deref(), Some(r#"{"Bash": 3}"#));
    assert_eq!(h.cache_read, Some(100));
    assert!(!h.generation_broken);
    assert_eq!(h.ended_at, Some(200));
    assert_eq!(h.resolved_endpoint_id.as_deref(), Some("ep-1"));
}

#[test]
fn task_summaries_aggregate_requests_per_task() {
    let conn = fresh_db();
    // Seed two task rows, then record three requests:
    // t-1 gets two attempts (one generation-broken), t-2 one.
    // Task ids are Nestra UUIDs (the store serializes RouteRecord.task_id
    // to string); use real UUIDs so the FK + round-trip stay valid.
    let t1 = uuid::Uuid::new_v4();
    let t2 = uuid::Uuid::new_v4();
    for task_id in [t1, t2] {
        insert_task(
            &conn,
            &TaskRow {
                id: task_id.to_string(),
                parent_task_id: None,
                lifecycle: "inflight".into(),
                native_task_ref: None,
                started_at: 0,
                ended_at: None,
            },
        )
        .unwrap();
    }
    for (task_id, gen_broken) in [(t1, true), (t1, false), (t2, false)] {
        let rec = RouteRecord {
            request_id: uuid::Uuid::new_v4(),
            task_id,
            agent_id: "claude-code-cli".into(),
            logical_session: Some("sess-1".into()),
            subagent_role: Some("main".into()),
            role_source: Some("native".into()),
            requested_model: Some("m".into()),
            requested_provider: None,
            resolved_endpoint_id: None,
            resolved_model: None,
            protocol: None,
            route_reason: "capability".into(),
            http_status: Some(if gen_broken { 503 } else { 200 }),
            usage_input: None,
            usage_output: None,
            cache_creation: None,
            cache_read: None,
            tool_calls: None,
        tool_names: None,
            generation_broken: gen_broken,
            started_at: 100,
            ended_at: Some(200),
        };
        insert_route_request(&conn, &rec).unwrap();
    }

    let summaries = task_summaries(&conn, 50).unwrap();
    assert_eq!(summaries.len(), 2, "aggregated by task_id");
    let s1 = summaries.iter().find(|s| s.task_id == t1.to_string()).unwrap();
    assert_eq!(s1.request_count, 2);
    assert!(s1.generation_broken, "t1 has one broken attempt");
    let s2 = summaries.iter().find(|s| s.task_id == t2.to_string()).unwrap();
    assert_eq!(s2.request_count, 1);
    assert!(!s2.generation_broken);

    // Per-session filter returns both (same logical session).
    let sess = tasks_for_session(&conn, "sess-1", 50).unwrap();
    assert_eq!(sess.len(), 2);
    // Unknown session → empty.
    assert!(tasks_for_session(&conn, "nope", 50).unwrap().is_empty());
}

#[test]
fn detected_roles_aggregates_by_role_and_filters_main() {
    let conn = fresh_db();
    let t1 = uuid::Uuid::new_v4();
    let t2 = uuid::Uuid::new_v4();
    for task_id in [t1, t2] {
        insert_task(
            &conn,
            &TaskRow {
                id: task_id.to_string(),
                parent_task_id: None,
                lifecycle: "inflight".into(),
                native_task_ref: None,
                started_at: 0,
                ended_at: None,
            },
        )
        .unwrap();
    }
    // claude:researcher ×2 (older), opencode:research ×1 (newest),
    // main ×1 (must be filtered).
    for (task_id, role, at) in [
        (t1, "claude:researcher", 100),
        (t1, "claude:researcher", 200),
        (t2, "opencode:research", 300),
        (t2, "main", 400),
    ] {
        insert_route_request(
            &conn,
            &RouteRecord {
                request_id: uuid::Uuid::new_v4(),
                task_id,
                agent_id: "opencode-desktop".into(),
                logical_session: Some("sess-1".into()),
                subagent_role: Some(role.into()),
                role_source: Some("heuristic".into()),
                requested_model: Some("nestra".into()),
                requested_provider: None,
                resolved_endpoint_id: None,
                resolved_model: None,
                protocol: None,
                route_reason: "capability".into(),
                http_status: Some(200),
                usage_input: None,
                usage_output: None,
                cache_creation: None,
                cache_read: None,
                tool_calls: None,
        tool_names: None,
                generation_broken: false,
                started_at: at,
                ended_at: Some(at + 10),
            },
        )
        .unwrap();
    }

    let roles = detected_roles(&conn, "opencode-desktop", 20).unwrap();
    assert_eq!(roles.len(), 2, "main must be filtered out");
    assert_eq!(roles[0].role, "opencode:research", "newest last_seen first");
    assert_eq!(roles[0].request_count, 1);
    assert_eq!(roles[1].role, "claude:researcher");
    assert_eq!(roles[1].request_count, 2);
    // Another agent sees nothing.
    assert!(detected_roles(&conn, "claude-code-cli", 20).unwrap().is_empty());
    // Limit applies.
    assert_eq!(detected_roles(&conn, "opencode-desktop", 1).unwrap().len(), 1);
}

#[test]
fn route_migration_round_trip() {
    let conn = fresh_db();
    // Seed the parent task + a route_request row the migration FKs point at.
    // Migrations can reference any endpoint (including ones not in the
    // catalog), and route_migration itself has no FK to provider_endpoint,
    // so we don't need to seed endpoints here.
    let task_id = uuid::Uuid::new_v4();
    let request_id = uuid::Uuid::new_v4();
    seed_task_chain(&conn, &task_id.to_string(), false);
    insert_route_request(
        &conn,
        &RouteRecord {
            request_id,
            task_id,
            agent_id: "claude-code-cli".into(),
            logical_session: Some("sess-1".into()),
            subagent_role: Some("main".into()),
            role_source: Some("native".into()),
            requested_model: None,
            requested_provider: None,
            resolved_endpoint_id: None,
            resolved_model: None,
            protocol: None,
            route_reason: "capability".into(),
            http_status: None,
            usage_input: None,
            usage_output: None,
            cache_creation: None,
            cache_read: None,
            tool_calls: None,
        tool_names: None,
            generation_broken: false,
            started_at: 0,
            ended_at: None,
        },
    )
    .unwrap();

    let row = RouteMigrationRow {
        id: "m-1".into(),
        request_id: request_id.to_string(),
        task_id: task_id.to_string(),
        from_endpoint_id: Some("ep-1".into()),
        to_endpoint_id: Some("ep-2".into()),
        reason: "quota_exhausted".into(),
        detail: Some("5h window elapsed".into()),
        at_ms: 100,
    };
    insert_route_migration(&conn, &row).unwrap();
    let got = migrations_for_task(&conn, &task_id.to_string()).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].reason, "quota_exhausted");
    assert_eq!(got[0].to_endpoint_id.as_deref(), Some("ep-2"));
}

/// Credential-boundary guard (correction #5). Serializes every persisted
/// struct in this module and asserts no JSON key contains a secret-naming
/// substring. A contributor adding a credential column to any row type
/// breaks this test before merge.
#[test]
fn no_persisted_secret_fields() {
    let forbidden = ["key", "secret", "credential", "token", "password", "passwd", "apikey"];

    let samples: Vec<(String, serde_json::Value)> = vec![
        (
            "DetectedRoleSummary".into(),
            serde_json::to_value(DetectedRoleSummary {
                role: "claude:researcher".into(),
                request_count: 2,
                last_seen: 100,
            })
            .unwrap(),
        ),
        (
            "RoutingPolicyRow".into(),
            serde_json::to_value(RoutingPolicyRow::default_for("a", "main", 0)).unwrap(),
        ),
        (
            "TaskRow".into(),
            serde_json::to_value(TaskRow {
                id: "t".into(),
                parent_task_id: None,
                lifecycle: "born".into(),
                native_task_ref: None,
                started_at: 0,
                ended_at: None,
            })
            .unwrap(),
        ),
        (
            "RouteRecord".into(),
            serde_json::to_value(RouteRecord {
                request_id: uuid::Uuid::nil(),
                task_id: uuid::Uuid::nil(),
                agent_id: "a".into(),
                logical_session: None,
                subagent_role: None,
                role_source: None,
                requested_model: None,
                requested_provider: None,
                resolved_endpoint_id: None,
                resolved_model: None,
                protocol: None,
                route_reason: "capability".into(),
                http_status: None,
                usage_input: None,
                usage_output: None,
                cache_creation: None,
                cache_read: None,
                tool_calls: None,
        tool_names: None,
                generation_broken: false,
                started_at: 0,
                ended_at: None,
            })
            .unwrap(),
        ),
        (
            "RouteMigrationRow".into(),
            serde_json::to_value(RouteMigrationRow {
                id: "m".into(),
                request_id: "r".into(),
                task_id: "t".into(),
                from_endpoint_id: None,
                to_endpoint_id: None,
                reason: "policy".into(),
                detail: None,
                at_ms: 0,
            })
            .unwrap(),
        ),
        (
            "ModelCatalogRow".into(),
            serde_json::to_value(ModelCatalogRow {
                endpoint_id: "e".into(),
                model_id: "m".into(),
                abilities_json: "{}".into(),
            })
            .unwrap(),
        ),
    ];

    fn walk_keys(value: &serde_json::Value, keys: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    keys.push(k.clone());
                    walk_keys(v, keys);
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    walk_keys(v, keys);
                }
            }
            _ => {}
        }
    }

    for (name, value) in samples {
        let mut keys = Vec::new();
        walk_keys(&value, &mut keys);
        for k in &keys {
            let lower = k.to_ascii_lowercase();
            for bad in forbidden {
                assert!(
                    !lower.contains(bad),
                    "persisted struct {name} has a secret-named field {k:?} (contains {bad:?}) — credential boundary violation"
                );
            }
        }
    }
}
// ---- usage summary -----------------------------------------------------------

fn seed_usage_task(
    conn: &Connection,
    task_id: &str,
    started_at: i64,
    agent: &str,
    endpoint: Option<&str>,
    model: &str,
    inp: i64,
    out: i64,
) {
    if let Some(ep) = endpoint {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM provider_endpoint WHERE id = ?1",
                rusqlite::params![ep],
                |r| r.get(0),
            )
            .unwrap();
        if n == 0 {
            conn.execute(
                "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status)
                 VALUES (?1,'custom','E',0,'unvalidated')",
                rusqlite::params![ep],
            )
            .unwrap();
        }
    }
    conn.execute(
        "INSERT INTO task (id, lifecycle, started_at) VALUES (?1, 'inflight', ?2)",
        rusqlite::params![task_id, started_at],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO route_request
           (request_id, task_id, agent_id, resolved_endpoint_id, resolved_model,
            route_reason, started_at, usage_input, usage_output)
         VALUES (?1, ?2, ?3, ?4, ?5, 'policy', ?6, ?7, ?8)",
        rusqlite::params![
            format!("req-{task_id}-{started_at}"),
            task_id,
            agent,
            endpoint,
            model,
            started_at,
            inp,
            out
        ],
    )
    .unwrap();
}

fn no_prices(_: &str, _: &str) -> Option<crate::model_abilities::CostPerMtok> {
    None
}

/// Pruned tasks land in `usage_daily`, live ones stay in `route_request`, and
/// the summary returns BOTH halves — summing them reconstructs the true
/// lifetime total with no double count.
#[test]
fn usage_summary_unions_folded_and_live_without_double_count() {
    let conn = fresh_db();
    let now = chrono::Utc::now().timestamp_millis();
    let day = 86_400_000i64;
    // Old task (past retention) — prune folds it into usage_daily.
    seed_usage_task(&conn, "t-old", now - 40 * day, "pi-cli", Some("ep-1"), "m-a", 1000, 2000);
    // Same calendar day as "now-ish" is impossible to guarantee; use distinct
    // agents/models so the buckets are comparable.
    crate::db::prune_observability_data(&conn).unwrap();
    // Live task.
    seed_usage_task(&conn, "t-new", now, "pi-cli", Some("ep-1"), "m-a", 3000, 4000);

    let rows = usage_summary_rows(&conn, None, None, &no_prices).unwrap();
    let total_in: i64 = rows.iter().map(|r| r.usage_input).sum();
    let total_out: i64 = rows.iter().map(|r| r.usage_output).sum();
    assert_eq!(total_in, 4000, "folded 1000 + live 3000");
    assert_eq!(total_out, 6000, "folded 2000 + live 4000");
    assert!(
        rows.iter().any(|r| r.day.len() == 10),
        "day strings are YYYY-MM-DD"
    );
}

#[test]
fn usage_summary_filters_agent_and_computes_cost_at_read_time() {
    let conn = fresh_db();
    let now = chrono::Utc::now().timestamp_millis();
    seed_usage_task(&conn, "t-a", now, "pi-cli", Some("ep-1"), "m-a", 1_000_000, 1_000_000);
    seed_usage_task(&conn, "t-b", now, "zcode-desktop", Some("ep-1"), "m-b", 500, 500);

    let prices = |_ep: &str, m: &str| -> Option<crate::model_abilities::CostPerMtok> {
        (m == "m-a").then(|| crate::model_abilities::CostPerMtok {
            input: Some(3.0),
            output: Some(15.0),
            ..Default::default()
        })
    };
    let rows = usage_summary_rows(&conn, Some("pi-cli"), None, &prices).unwrap();
    assert_eq!(rows.len(), 1, "agent filter holds");
    assert_eq!(rows[0].model_id, "m-a");
    // 1M in @ $3 + 1M out @ $15 = $18; cache components unpriced → ignored.
    assert!((rows[0].cost_usd.unwrap() - 18.0).abs() < 1e-9);

    // Unpriced model → unknown spend, not free.
    let rows = usage_summary_rows(&conn, Some("zcode-desktop"), None, &prices).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].cost_usd.is_none());
}

/// `clear_observability` wipes the gateway's observability set (tasks +
/// cascade, usage rollup, affinity snapshot) while configuration tables —
/// routing policy, endpoints — survive untouched.
#[test]
fn clear_observability_keeps_configuration() {
    let conn = fresh_db();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    let now = 1_700_000_000;

    seed_usage_task(&conn, "t-a", now, "pi-cli", Some("ep-1"), "m-a", 100, 100);
    upsert_routing_policy(
        &conn,
        &RoutingPolicyRow {
            agent_id: "pi-cli".into(),
            role: "*".into(),
            route_targets: Some(r#"[{"endpoint":"ep-1","model":"m-a"}]"#.into()),
            migrate_on_quota: true,
            inject_cache_control: false,
            affinity_scope: "task".into(),
            updated_at: now,
        },
    )
    .unwrap();
    crate::db::set_setting(
        &conn,
        "route_affinity",
        &serde_json::json!({ "k": "v" }),
    )
    .unwrap();

    clear_observability(&conn).unwrap();

    let count = |sql: &str| -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    };
    assert_eq!(count("SELECT count(*) FROM task"), 0);
    assert_eq!(count("SELECT count(*) FROM route_request"), 0);
    assert_eq!(count("SELECT count(*) FROM usage_daily"), 0);
    assert_eq!(count("SELECT count(*) FROM setting_kv WHERE key = 'route_affinity'"), 0);
    // Configuration survives.
    assert_eq!(count("SELECT count(*) FROM routing_policy"), 1);
    assert_eq!(count("SELECT count(*) FROM provider_endpoint"), 1);
}
