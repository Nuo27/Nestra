use super::*;

#[test]
fn classifies_all_six_classes() {
    assert_eq!(
        FailureClass::classify(429, "quota exceeded", false),
        FailureClass::QuotaExhausted
    );
    assert_eq!(
        FailureClass::classify(429, "too many requests", false),
        FailureClass::RateLimit
    );
    assert_eq!(
        FailureClass::classify(503, "upstream down", false),
        FailureClass::Temp5xx
    );
    assert_eq!(
        FailureClass::classify(0, "", true),
        FailureClass::Timeout,
        "timeout short-circuits regardless of status"
    );
    assert_eq!(
        FailureClass::classify(401, "unauthorized", false),
        FailureClass::Auth
    );
    assert_eq!(
        FailureClass::classify(400, "malformed request", false),
        FailureClass::BadRequest
    );
}

#[test]
fn quota_markers_in_4xx_route_to_quota() {
    // A 400 carrying quota language is the rare provider that reports
    // exhaustion as 400 — treat as quota so we migrate, not surface.
    assert_eq!(
        FailureClass::classify(400, "your quota limit is reached", false),
        FailureClass::QuotaExhausted
    );
    assert_eq!(
        FailureClass::classify(403, "余额不足", false),
        FailureClass::QuotaExhausted,
        "Chinese 余额 marker must match"
    );
}

#[test]
fn rate_limit_language_is_not_quota_exhaustion() {
    // Ordinary rate limiting must retry same provider, not migrate — the
    // markers deliberately exclude "rate"/"limit" so a routine 429 never
    // trips a migration.
    assert_eq!(
        FailureClass::classify(429, "rate limit reached", false),
        FailureClass::RateLimit
    );
    assert_eq!(
        FailureClass::classify(429, "too many requests, slow down", false),
        FailureClass::RateLimit
    );
    // "exceed" IS a quota marker (exceeded-token-quota shape) — the
    // limit-vs-exceeded distinction is exactly what the marker set pins.
    assert_eq!(
        FailureClass::classify(400, "exceeds the limit for this model", false),
        FailureClass::QuotaExhausted,
        "'exceeds … limit' carries exhaustion language"
    );
    // A generic 400 with only "limit" wording (no quota/exceed/throttle)
    // stays a validation error.
    assert_eq!(
        FailureClass::classify(400, "the model does not allow this limit", false),
        FailureClass::BadRequest,
        "bare 'limit' wording is not exhaustion"
    );
}

#[test]
fn auth_and_bad_request_never_migrate() {
    // correction #3: migrating an unauthorized/malformed request fails
    // identically elsewhere, so the router must surface, not migrate.
    assert!(!FailureClass::Auth.can_migrate());
    assert!(!FailureClass::BadRequest.can_migrate());
    assert!(FailureClass::QuotaExhausted.can_migrate());
    assert!(FailureClass::Temp5xx.can_migrate());
    assert!(FailureClass::RateLimit.can_migrate());
    assert!(FailureClass::Timeout.can_migrate());
}

#[test]
fn only_transient_classes_retry_same_provider() {
    assert!(FailureClass::Temp5xx.retry_same_provider());
    assert!(FailureClass::RateLimit.retry_same_provider());
    assert!(FailureClass::Timeout.retry_same_provider());
    assert!(!FailureClass::QuotaExhausted.retry_same_provider());
    assert!(!FailureClass::Auth.retry_same_provider());
    assert!(!FailureClass::BadRequest.retry_same_provider());
}

#[test]
fn health_window_opens_circuit_after_threshold_migratable() {
    let h = ProviderHealth::new();
    let ep = "ep-1";
    // Two quota failures: not yet degraded.
    h.record(ep, HealthOutcome::Fail(FailureClass::QuotaExhausted), 429);
    h.record(ep, HealthOutcome::Fail(FailureClass::QuotaExhausted), 429);
    assert!(!h.is_degraded(ep));
    assert_eq!(h.get(ep).consecutive_migratable, 2);
    // Third migratable failure opens the circuit.
    h.record(ep, HealthOutcome::Fail(FailureClass::Temp5xx), 503);
    assert!(h.is_degraded(ep), "3 consecutive migratable failures must degrade");
    assert!(h.get(ep).degraded);
    // One Ok clears it.
    h.record(ep, HealthOutcome::Ok, 200);
    assert!(!h.is_degraded(ep));
    assert_eq!(h.get(ep).consecutive_migratable, 0);
}

#[test]
fn auth_failures_do_not_open_circuit() {
    // Auth is non-migratable, so consecutive_auth never trips the
    // migratable threshold. The endpoint stays eligible (the router will
    // surface the 401 to the agent; surfacing is the operator signal).
    let h = ProviderHealth::new();
    let ep = "ep-1";
    for _ in 0..5 {
        h.record(ep, HealthOutcome::Fail(FailureClass::Auth), 401);
    }
    assert!(!h.is_degraded(ep), "auth failures must not degrade (non-migratable)");
    assert_eq!(h.get(ep).consecutive_failures, 5);
    assert_eq!(h.get(ep).consecutive_migratable, 0);
}

#[test]
fn window_caps_at_max() {
    let h = ProviderHealth::new();
    let ep = "ep-1";
    for _ in 0..(WINDOW + 10) {
        h.record(ep, HealthOutcome::Ok, 200);
    }
    assert_eq!(h.get(ep).recent.len(), WINDOW);
}

#[test]
fn eligible_filters_out_degraded() {
    let h = ProviderHealth::new();
    for _ in 0..DEGRADED_THRESHOLD {
        h.record("ep-bad", HealthOutcome::Fail(FailureClass::QuotaExhausted), 429);
    }
    h.record("ep-good", HealthOutcome::Ok, 200);
    let eligible = h.eligible(&["ep-bad".into(), "ep-good".into(), "ep-unseen".into()]);
    assert!(eligible.contains(&"ep-good".into()));
    assert!(eligible.contains(&"ep-unseen".into()), "unseen endpoints are eligible");
    assert!(!eligible.contains(&"ep-bad".into()), "degraded endpoint excluded");
}

#[test]
fn outcome_from_response_classifies() {
    assert!(matches!(
        HealthOutcome::from_response(200, "", false),
        HealthOutcome::Ok
    ));
    assert!(matches!(
        HealthOutcome::from_response(429, "too many requests", false),
        HealthOutcome::Fail(FailureClass::RateLimit)
    ));
}

fn mem_conn() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::schema::build_v1(&conn).unwrap();
    conn
}

#[test]
fn degraded_circuit_persists_and_restores() {
    let conn = mem_conn();
    let h = ProviderHealth::new();
    for _ in 0..DEGRADED_THRESHOLD {
        h.record("ep-1", HealthOutcome::Fail(FailureClass::Temp5xx), 503);
    }
    assert!(h.is_degraded("ep-1"));
    h.persist_degraded(&conn);

    // Simulated restart: a fresh instance restores the open circuit.
    let h2 = ProviderHealth::new();
    h2.load(&conn);
    assert!(h2.is_degraded("ep-1"), "degraded circuit survives restart");
    assert!(h2.eligible(&["ep-1".into()]).is_empty());

    // An Ok clears it; the healthy transition persists as an empty set.
    h2.record("ep-1", HealthOutcome::Ok, 200);
    h2.persist_degraded(&conn);
    let h3 = ProviderHealth::new();
    h3.load(&conn);
    assert!(!h3.is_degraded("ep-1"), "cleared circuit stays cleared across restart");
}

#[test]
fn expired_persisted_circuit_is_dropped_at_load() {
    let conn = mem_conn();
    // A degraded_at beyond the TTL — the stale-circuit trap fix 3 exists
    // for: without the TTL check this endpoint would never route again.
    let stale = serde_json::json!([{
        "endpoint_id": "ep-old",
        "degraded_at_ms": chrono::Utc::now().timestamp_millis() - DEGRADED_TTL_MS - 1,
        "class": "temp_5xx"
    }]);
    crate::db::set_setting(&conn, PERSIST_KEY, &stale).unwrap();
    let h = ProviderHealth::new();
    h.load(&conn);
    assert!(!h.is_degraded("ep-old"));
    assert_eq!(h.eligible(&["ep-old".into()]), vec!["ep-old".to_string()]);
}

#[test]
fn persist_degraded_writes_only_on_transition() {
    let conn = mem_conn();
    let h = ProviderHealth::new();
    for _ in 0..DEGRADED_THRESHOLD {
        h.record("ep-1", HealthOutcome::Fail(FailureClass::Temp5xx), 503);
    }
    h.persist_degraded(&conn);
    // Delete the row to detect any rewrite; a further failure does NOT
    // transition (still degraded, same open time) so the snapshot is
    // unchanged and the persist must be a no-op.
    conn.execute("DELETE FROM setting_kv WHERE key = 'provider_health'", [])
        .unwrap();
    h.record("ep-1", HealthOutcome::Fail(FailureClass::Temp5xx), 503);
    h.persist_degraded(&conn);
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM setting_kv WHERE key = 'provider_health'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "stable degraded set must not rewrite the setting");
}