use super::*;
use crate::orchestration::identity::{CacheStrategy, CredentialHandle, RouteReason};
use std::sync::Arc;

/// Build a `GatewayState` over an in-memory DB with the canonical schema,
/// plus stub health/quota/affinity stores and a no-op credential reader.
fn gateway_state(conn: rusqlite::Connection) -> GatewayState {
    crate::schema::build_v1(&conn).unwrap();
    GatewayState {
        db: Arc::new(tokio::sync::Mutex::new(conn)),
        health: Arc::new(crate::orchestration::health::ProviderHealth::new()),
        quota: Arc::new(crate::orchestration::quota_state::QuotaState::new()),
        affinity: Arc::new(crate::orchestration::router::RouteAffinity::new()),
        credential_reader: Arc::new(|_| Ok(None)),
        loopback_token: Arc::new(tokio::sync::RwLock::new("test-token".into())),
        tuning: super::super::tuning::shared_default(),
    }
}

fn ok_route(endpoint_id: &str, _ctx: &TaskContext) -> ResolvedRoute {
    ResolvedRoute {
        endpoint_id: endpoint_id.to_string(),
        provider_kind: crate::config_writer::ProviderKind::Anthropic,
        model: "claude-haiku-4-5".to_string(),
        base_url: "https://api.example.com".to_string(),
        protocol: crate::config_writer::ProviderKind::Anthropic,
        credential: CredentialHandle::new(endpoint_id, "sk-test".into()),
        cache_strategy: CacheStrategy::Off,
        reason: RouteReason::Explicit,
        route_lineage: Vec::new(),
    }
}

/// The gateway's live write→read link: a successful pass through
/// `run_with_migration` must (1) persist a `route_request` row, (2) be
/// visible to the `orch_tasks` read path (`store::task_summaries`), and
/// (3) transition the task lifecycle to `done`. This is the acceptance
/// criterion for the orchestration surface (Tasks / RouteLineage).
#[tokio::test]
async fn gateway_success_records_request_visible_to_task_reads() {
    let state = gateway_state(rusqlite::Connection::open_in_memory().unwrap());
    // The route_request row FKs to provider_endpoint; seed the endpoint
    // the stub resolve will return so the FK validates.
    {
        let conn = state.db.lock().await;
        crate::db::create_endpoint(&conn, "ep-1", "anthropic", "Test").unwrap();
    }
    let ctx = TaskContext::new_task("claude-code-cli", Some("sess-1".to_string()));

    let resolve = |_ctx: &TaskContext| -> ResolveFuture {
        let route = ok_route("ep-1", _ctx);
        Box::pin(async move { Ok(route) })
    };
    let forward = |_ctx: &TaskContext, _route: &ResolvedRoute| -> ForwardFuture {
        Box::pin(async move {
            ForwardOutcome::Responded {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: GatewayBody::Full(Full::new(Bytes::from(
                    r#"{"id":"msg-1","content":[]}"#,
                ))),
                tool_calls: None,
                tool_names: None,
                usage: Some(ObservedUsage {
                    input: Some(100),
                    output: Some(50),
                    cache_creation: None,
                    cache_read: None,
                }),
                generation_started: false,
                body_error: None,
            }
        })
    };
    let error_response = |status: StatusCode, msg: &str| {
        build_agent_response(status, HeaderMap::new(), GatewayBody::Full(Full::new(Bytes::from(msg.to_string()))))
    };

    let resp = run_with_migration(
        &state,
        ctx,
        "claude-code-cli".to_string(),
        chrono::Utc::now().timestamp_millis(),
        false, // no side-effect risk
        resolve,
        forward,
        error_response,
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The write→read link: route_request persisted, visible to the task
    // summaries read used by `orch_tasks`.
    let conn = state.db.lock().await;
    let summaries = store::task_summaries(&conn, 10).unwrap();
    assert_eq!(summaries.len(), 1, "task_summaries must surface the routed task");
    let s = &summaries[0];
    assert_eq!(s.agent_id, "claude-code-cli");
    assert_eq!(s.latest_status, Some(200));
    // Lifecycle transitioned to a terminal state by the success path.
    let lc: String = conn
        .query_row("SELECT lifecycle FROM task", [], |r| r.get(0))
        .unwrap();
    assert_eq!(lc, "done", "successful request must mark the task done");
    // The route_request row carries the observed usage.
    let (inp, out): (i64, i64) = conn
        .query_row(
            "SELECT usage_input, usage_output FROM route_request",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((inp, out), (100, 50));
}

/// A surfaced failure (side-effect-risk body that already streamed response
/// bytes — never blindly retried) must mark the task `failed` — the terminal
/// lifecycle for the honest failure path.
#[tokio::test]
async fn gateway_surface_failure_marks_task_failed() {
    let state = gateway_state(rusqlite::Connection::open_in_memory().unwrap());
    {
        let conn = state.db.lock().await;
        crate::db::create_endpoint(&conn, "ep-1", "anthropic", "Test").unwrap();
    }
    let ctx = TaskContext::new_task("claude-code-cli", Some("sess-1".to_string()));

    let resolve = |_ctx: &TaskContext| -> ResolveFuture {
        let route = ok_route("ep-1", _ctx);
        Box::pin(async move { Ok(route) })
    };
    // A 500 after bytes flowed on a side-effect-risk body must Surface (never
    // blind-retry a possibly-executed tool call), so the loop exits via the
    // MigrationDecision::Surface arm and marks the task failed.
    let forward = |_ctx: &TaskContext, _route: &ResolvedRoute| -> ForwardFuture {
        Box::pin(async move {
            ForwardOutcome::Responded {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                headers: HeaderMap::new(),
                body: GatewayBody::Full(Full::new(Bytes::from(
                    r#"{"type":"error","error":{"message":"boom"}}"#,
                ))),
                usage: None,
                tool_calls: None,
                tool_names: None,
                generation_started: true,
                body_error: None,
            }
        })
    };
    let error_response = |status: StatusCode, msg: &str| {
        build_agent_response(status, HeaderMap::new(), GatewayBody::Full(Full::new(Bytes::from(msg.to_string()))))
    };

    let resp = run_with_migration(
        &state,
        ctx,
        "claude-code-cli".to_string(),
        chrono::Utc::now().timestamp_millis(),
        true, // side-effect risk — surfaces because bytes already flowed
        resolve,
        forward,
        error_response,
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let conn = state.db.lock().await;
    let lc: String = conn
        .query_row("SELECT lifecycle FROM task", [], |r| r.get(0))
        .unwrap();
    assert_eq!(lc, "failed", "surfaced failure must mark the task failed");
}

/// The pre-response replay path (the production ox-alpha-free scenario): a
/// tool-carrying request that fails with an empty-body 503 BEFORE any
/// response bytes must retry the same endpoint, then migrate — and when no
/// fallback is eligible, terminate with a 503 and the `failed` lifecycle
/// instead of surfacing the first failure (or looping forever).
#[tokio::test]
async fn gateway_pre_response_failure_retries_then_falls_back() {
    let state = gateway_state(rusqlite::Connection::open_in_memory().unwrap());
    {
        let conn = state.db.lock().await;
        crate::db::create_endpoint(&conn, "ep-1", "anthropic", "Test").unwrap();
        crate::db::create_endpoint(&conn, "ep-2", "anthropic", "Test").unwrap();
        // A second policy target must exist for the retry ladder to run at
        // all: the fast-fail guard collapses RetrySame to Surface when
        // `failover_targets` sees no alternative (single-target policies
        // fail fast by design — see the e2e
        // `single_target_policy_fails_fast_without_retry_ladder`).
        let targets = serde_json::to_string(&[
            serde_json::json!({"endpoint": "ep-1", "model": "claude-haiku-4-5"}),
            serde_json::json!({"endpoint": "ep-2", "model": "claude-haiku-4-5"}),
        ])
        .unwrap();
        conn.execute(
            "INSERT INTO routing_policy (agent_id, role, route_targets, migrate_on_quota,
                                        inject_cache_control, affinity_scope, updated_at)
             VALUES ('claude-code-cli','*',?1,1,0,'task',1)",
            [&targets],
        )
        .unwrap();
    }
    let ctx = TaskContext::new_task("claude-code-cli", Some("sess-1".to_string()));

    // First resolve returns the route; the post-Migrate re-resolve reports no
    // eligible fallback (the real router skips the now-degraded endpoint).
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let resolve = move |_ctx: &TaskContext| -> ResolveFuture {
        let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let route = ok_route("ep-1", _ctx);
        Box::pin(async move {
            if n == 0 {
                Ok(route)
            } else {
                Ok(ResolvedRoute {
                    endpoint_id: String::new(),
                    reason: RouteReason::NoEligible,
                    ..route
                })
            }
        })
    };
    // Always an empty-body 503, generation never started.
    let forward = |_ctx: &TaskContext, _route: &ResolvedRoute| -> ForwardFuture {
        Box::pin(async move {
            ForwardOutcome::Responded {
                status: StatusCode::SERVICE_UNAVAILABLE,
                headers: HeaderMap::new(),
                body: GatewayBody::Full(Full::new(Bytes::new())),
                usage: None,
                tool_calls: None,
                tool_names: None,
                generation_started: false,
                body_error: None,
            }
        })
    };
    let error_response = |status: StatusCode, msg: &str| {
        build_agent_response(status, HeaderMap::new(), GatewayBody::Full(Full::new(Bytes::from(msg.to_string()))))
    };

    let resp = run_with_migration(
        &state,
        ctx,
        "claude-code-cli".to_string(),
        chrono::Utc::now().timestamp_millis(),
        true, // side-effect risk — replays because no bytes flowed
        resolve,
        forward,
        error_response,
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    let conn = state.db.lock().await;
    let lc: String = conn
        .query_row("SELECT lifecycle FROM task", [], |r| r.get(0))
        .unwrap();
    assert_eq!(lc, "failed", "exhausted fallback must mark the task failed");
    // Initial attempt + 2 same-endpoint retries (attempts 1 and 2 → RetrySame,
    // attempt 3 → Migrate): 3 route_request rows, 1 migration row.
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM route_request", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 3, "expected initial attempt + 2 retries");
    let (reason,): (String,) = conn
        .query_row("SELECT reason FROM route_migration", [], |r| {
            Ok((r.get(0)?,))
        })
        .unwrap();
    assert_eq!(reason, "retries_exhausted");
}

/// The whole-loop deadline: even with alternatives available (the ladder
/// WOULD keep going), `request_deadline_secs` caps the total wall clock and
/// the loop stops with an honest 504. Pure stubs (no real IO) so paused
/// tokio time auto-advances deterministically: each attempt "takes" 20s of
/// paused time, the deadline is the 30s clamp minimum.
#[tokio::test(start_paused = true)]
async fn request_deadline_caps_the_ladder() {
    let state = gateway_state(rusqlite::Connection::open_in_memory().unwrap());
    {
        let conn = state.db.lock().await;
        crate::db::create_endpoint(&conn, "ep-1", "anthropic", "Test").unwrap();
        crate::db::create_endpoint(&conn, "ep-2", "anthropic", "Test").unwrap();
        let targets = serde_json::to_string(&[
            serde_json::json!({"endpoint": "ep-1", "model": "claude-haiku-4-5"}),
            serde_json::json!({"endpoint": "ep-2", "model": "claude-haiku-4-5"}),
        ])
        .unwrap();
        conn.execute(
            "INSERT INTO routing_policy (agent_id, role, route_targets, migrate_on_quota,
                                        inject_cache_control, affinity_scope, updated_at)
             VALUES ('claude-code-cli','*',?1,1,0,'task',1)",
            [&targets],
        )
        .unwrap();
    }
    // 30s is the clamp minimum for the deadline.
    *state.tuning.write().unwrap() = super::super::tuning::GatewayTuning {
        request_deadline_secs: 30,
        ..Default::default()
    };
    let ctx = TaskContext::new_task("claude-code-cli", Some("sess-1".to_string()));

    // Always the same route (the ladder would loop forever without the
    // deadline) and always a 20s-then-503 attempt.
    let resolve = |_ctx: &TaskContext| -> ResolveFuture {
        let route = ok_route("ep-1", _ctx);
        Box::pin(async move { Ok(route) })
    };
    let forward = |_ctx: &TaskContext, _route: &ResolvedRoute| -> ForwardFuture {
        Box::pin(async move {
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
            ForwardOutcome::Responded {
                status: StatusCode::SERVICE_UNAVAILABLE,
                headers: HeaderMap::new(),
                body: GatewayBody::Full(Full::new(Bytes::new())),
                usage: None,
                tool_calls: None,
                tool_names: None,
                generation_started: false,
                body_error: None,
            }
        })
    };
    let error_response = |status: StatusCode, msg: &str| {
        build_agent_response(status, HeaderMap::new(), GatewayBody::Full(Full::new(Bytes::from(msg.to_string()))))
    };

    let resp = run_with_migration(
        &state,
        ctx,
        "claude-code-cli".to_string(),
        chrono::Utc::now().timestamp_millis(),
        false,
        resolve,
        forward,
        error_response,
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);

    let conn = state.db.lock().await;
    let lc: String = conn
        .query_row("SELECT lifecycle FROM task", [], |r| r.get(0))
        .unwrap();
    assert_eq!(lc, "failed", "deadline expiry must mark the task failed");
    // Attempt 1 ends at t=20, attempt 2 at t=40; the loop-top check at t≥30
    // stops the ladder after two attempts.
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM route_request", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 2, "deadline fires between attempt 2 and 3");
}


/// Lock-escape: while ANOTHER connection holds the write lock (launch
/// reconcile's shape — multi-second per-provider transactions), a terminal
/// observability write must (a) return FAST (the inline attempt defers at
/// ~150ms busy, never the 5s connection default — that stall is what made
/// zcode reconnect-loop while the gateway rows said 200), and (b) still
/// land once the lock releases (bounded background retry). Real file-backed
/// SQLite — in-memory DBs are per-connection and can't reproduce the
/// cross-connection lock.
#[tokio::test]
async fn locked_db_defers_observability_write_instead_of_stalling() {
    let dir = tempfile::tempdir().unwrap();
    let gw_conn = crate::db::open(dir.path()).unwrap();
    let state = gateway_state(gw_conn);
    let task_id = uuid::Uuid::new_v4();
    {
        let conn = state.db.lock().await;
        conn.execute(
            "INSERT INTO task (id, lifecycle, started_at) VALUES (?1, 'born', 0)",
            rusqlite::params![task_id.to_string()],
        )
        .unwrap();
    }

    // Antagonist: a separate connection holding the write lock for 1.5s.
    let ant = crate::db::open(dir.path()).unwrap();
    ant.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let holder = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        ant.execute_batch("ROLLBACK;").unwrap();
    });

    let t0 = std::time::Instant::now();
    mark_task_terminal(&state, &task_id, TaskLifecycle::Done).await;
    let elapsed = t0.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "terminal write must defer fast under lock (took {elapsed:?})"
    );

    holder.join().unwrap();
    // The deferred write lands via the retry ladder shortly after release.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    loop {
        let lc = {
            let conn = state.db.lock().await;
            conn.query_row(
                "SELECT lifecycle FROM task WHERE id = ?1",
                rusqlite::params![task_id.to_string()],
                |r| r.get::<_, String>(0),
            )
            .ok()
        };
        if lc.as_deref() == Some("done") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "deferred terminal write must land after lock release"
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}
