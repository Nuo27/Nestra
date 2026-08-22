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

/// A surfaced failure (side-effect-risk body that must never be blindly
/// retried) must mark the task `failed` — the terminal lifecycle for the
/// honest failure path.
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
    // A 500 response on a side-effect-risk body must Surface (never blind
    // retry a possibly-executed tool call), so the loop exits via the
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
        true, // side-effect risk — never blind-retry
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