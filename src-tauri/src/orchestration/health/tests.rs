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

/// The default tuning's failure threshold (3) — the tests below assume it.
const THRESHOLD: u32 = 3;

#[test]
fn breaker_opens_after_threshold_migratable_failures() {
    let h = ProviderHealth::new();
    let ep = "ep-1";
    // Two quota failures: not yet open.
    h.record(ep, "m", HealthOutcome::Fail(FailureClass::QuotaExhausted), 429);
    h.record(ep, "m", HealthOutcome::Fail(FailureClass::QuotaExhausted), 429);
    assert!(!h.is_degraded(ep, "m"));
    assert_eq!(h.get(ep, "m").consecutive_migratable, 2);
    // Third migratable failure opens the circuit.
    h.record(ep, "m", HealthOutcome::Fail(FailureClass::Temp5xx), 503);
    assert!(h.is_degraded(ep, "m"), "3 consecutive migratable failures must open");
    assert!(matches!(
        h.get(ep, "m").breaker,
        BreakerState::Open { .. }
    ));
}

#[test]
fn auth_failures_do_not_open_circuit() {
    // Auth is non-migratable, so consecutive_auth never trips the
    // migratable threshold. The endpoint stays eligible (the router will
    // surface the 401 to the agent; surfacing is the operator signal).
    let h = ProviderHealth::new();
    let ep = "ep-1";
    for _ in 0..5 {
        h.record(ep, "m", HealthOutcome::Fail(FailureClass::Auth), 401);
    }
    assert!(!h.is_degraded(ep, "m"), "auth failures must not open (non-migratable)");
    assert_eq!(h.get(ep, "m").consecutive_failures, 5);
    assert_eq!(h.get(ep, "m").consecutive_migratable, 0);
}

#[test]
fn window_caps_at_max() {
    let h = ProviderHealth::new();
    let ep = "ep-1";
    for _ in 0..(WINDOW + 10) {
        h.record(ep, "m", HealthOutcome::Ok, 200);
    }
    assert_eq!(h.get(ep, "m").recent.len(), WINDOW);
}

#[test]
fn model_isolation_keeps_healthy_models_routable() {
    // The model-grain point: m-bad tripping must NOT exile m-good on the
    // same endpoint, and an unseen target is eligible.
    let h = ProviderHealth::new();
    for _ in 0..THRESHOLD {
        h.record("ep-1", "m-bad", HealthOutcome::Fail(FailureClass::QuotaExhausted), 429);
    }
    assert!(h.is_degraded("ep-1", "m-bad"), "the failing model's circuit opens");
    assert!(!h.is_degraded("ep-1", "m-good"), "healthy models on the same endpoint stay routable");
    assert!(!h.is_degraded("ep-unseen", "m-any"), "unseen targets are eligible");
}

/// Seed a `ProviderHealth` with one endpoint in an explicit breaker state
/// (the map is private, but this child module may touch it — avoids sleeping
/// on real recovery waits).
fn seeded(state: BreakerState) -> ProviderHealth {
    let h = ProviderHealth::new();
    h.inner.lock().unwrap().insert(
        "ep-1/m".to_string(),
        EndpointHealth {
            breaker: state,
            consecutive_migratable: THRESHOLD,
            consecutive_failures: THRESHOLD,
            last_failure: Some(FailureClass::Temp5xx),
            ..Default::default()
        },
    );
    h
}

/// Open → (recovery wait elapses) → eligible as HalfOpen → probe Oks close
/// the circuit only after the success threshold; a probe failure re-opens
/// with a FRESH stamp (the wait restarts).
#[test]
fn half_open_probes_close_after_success_threshold() {
    let wait_ms: i64 = 60_000;
    let opened_at = chrono::Utc::now().timestamp_millis() - wait_ms - 1;
    let h = seeded(BreakerState::Open { opened_at_ms: opened_at });

    // Recovery wait elapsed → lazily HalfOpen → eligible (probe allowed).
    assert!(!h.is_degraded("ep-1", "m"), "expired Open must probe (HalfOpen)");

    // First probe Ok: successes 1 < 2 → still HalfOpen.
    h.record("ep-1", "m", HealthOutcome::Ok, 200);
    assert!(!h.is_degraded("ep-1", "m"));
    assert_eq!(
        h.get("ep-1", "m").breaker,
        BreakerState::HalfOpen { successes: 1 },
        "one probe success keeps the circuit half-open"
    );

    // Second probe Ok: threshold reached → Closed.
    h.record("ep-1", "m", HealthOutcome::Ok, 200);
    assert_eq!(h.get("ep-1", "m").breaker, BreakerState::Closed);
}

#[test]
fn half_open_probe_failure_reopens_with_fresh_stamp() {
    let wait_ms: i64 = 60_000;
    let opened_at = chrono::Utc::now().timestamp_millis() - wait_ms - 1_000;
    let h = seeded(BreakerState::Open { opened_at_ms: opened_at });
    assert!(!h.is_degraded("ep-1", "m"), "expired Open probes");

    // The probe fails → re-open, with a FRESH stamp (>= the original).
    let before = chrono::Utc::now().timestamp_millis();
    h.record("ep-1", "m", HealthOutcome::Fail(FailureClass::Temp5xx), 503);
    match h.get("ep-1", "m").breaker {
        BreakerState::Open { opened_at_ms } => {
            assert!(
                opened_at_ms >= opened_at,
                "re-open must stamp fresh (not reuse the expired stamp)"
            );
            assert!(opened_at_ms <= before + 1_000);
        }
        other => panic!("expected re-open, got {other:?}"),
    }
    assert!(h.is_degraded("ep-1", "m"), "fresh Open excludes again");
}

/// Still inside the recovery wait: Open stays Open (excluded), and further
/// failures must NOT reset the stamp (a still-failing endpoint cannot
/// extend its own wait window forever by failing).
#[test]
fn fresh_open_holds_stamp_under_continued_failure() {
    let opened_at = chrono::Utc::now().timestamp_millis() - 1_000; // 1s ago
    let h = seeded(BreakerState::Open { opened_at_ms: opened_at });
    assert!(h.is_degraded("ep-1", "m"));
    h.record("ep-1", "m", HealthOutcome::Fail(FailureClass::Temp5xx), 503);
    assert_eq!(
        h.get("ep-1", "m").breaker,
        BreakerState::Open { opened_at_ms: opened_at },
        "continued failure must keep the original stamp"
    );
}

/// The error-rate backstop: a flapping endpoint (fail fail ok repeat — the
/// consecutive count never reaches the threshold) opens the circuit once
/// the windowed rate crosses `breaker_error_rate_pct` with enough samples.
#[test]
fn error_rate_backstop_opens_flapping_endpoint() {
    let h = ProviderHealth::new();
    let ep = "ep-1";
    // fail,fail,ok × 7 = 21 outcomes → window keeps the last 20 (rate ≥ 60%,
    // consecutive never exceeds 2 < 3).
    for _ in 0..7 {
        h.record(ep, "m", HealthOutcome::Fail(FailureClass::Temp5xx), 503);
        h.record(ep, "m", HealthOutcome::Fail(FailureClass::Temp5xx), 503);
        h.record(ep, "m", HealthOutcome::Ok, 200);
    }
    assert!(
        matches!(h.get(ep, "m").breaker, BreakerState::Open { .. }),
        "flapping endpoint must trip the error-rate backstop (state: {:?})",
        h.get(ep, "m").breaker
    );
    assert!(h.is_degraded(ep, "m"));
}

#[test]
fn error_rate_backstop_disabled_at_zero_pct() {
    let tuning = crate::orchestration::gateway::tuning::shared_default();
    *tuning.write().unwrap() = crate::orchestration::gateway::tuning::GatewayTuning {
        breaker_error_rate_pct: 0,
        ..Default::default()
    };
    let h = ProviderHealth::with_tuning(tuning);
    for _ in 0..7 {
        h.record("ep-1", "m", HealthOutcome::Fail(FailureClass::Temp5xx), 503);
        h.record("ep-1", "m", HealthOutcome::Fail(FailureClass::Temp5xx), 503);
        h.record("ep-1", "m", HealthOutcome::Ok, 200);
    }
    assert_eq!(h.get("ep-1", "m").breaker, BreakerState::Closed);
}

/// A healthy endpoint's Ok must never re-open via the error-rate backstop
/// (the stale-window livelock guard: rate check fires on failures only).
#[test]
fn error_rate_backstop_not_evaluated_after_ok() {
    let h = ProviderHealth::new();
    // Fill the window with a ≥60% failure rate without crossing the
    // consecutive threshold, then flip fully healthy.
    for _ in 0..7 {
        h.record("ep-1", "m", HealthOutcome::Fail(FailureClass::Temp5xx), 503);
        h.record("ep-1", "m", HealthOutcome::Fail(FailureClass::Temp5xx), 503);
        h.record("ep-1", "m", HealthOutcome::Ok, 200);
    }
    assert!(matches!(h.get("ep-1", "m").breaker, BreakerState::Open { .. }));
    // Expire the wait (fresh seeded Open past the wait) and record an Ok —
    // the stale window must NOT re-open the just-closed circuit.
    let wait_ms: i64 = 60_000;
    let opened_at = chrono::Utc::now().timestamp_millis() - wait_ms - 1;
    *h.inner.lock().unwrap().get_mut("ep-1/m").unwrap() =
        EndpointHealth {
            breaker: BreakerState::Open { opened_at_ms: opened_at },
            recent: std::iter::repeat(OutcomeSnap {
                at_ms: opened_at,
                ok: false,
                class: Some(FailureClass::Temp5xx),
                status: 503,
            })
            .take(20)
            .collect(),
            consecutive_migratable: 0,
            consecutive_failures: 0,
            last_failure: Some(FailureClass::Temp5xx),
        };
    h.record("ep-1", "m", HealthOutcome::Ok, 200);
    assert!(
        !matches!(h.get("ep-1", "m").breaker, BreakerState::Open { .. }),
        "an Ok must not re-open via the stale window"
    );
}

fn mem_conn() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::schema::build_v1(&conn).unwrap();
    conn
}

#[test]
fn open_circuit_persists_and_restores() {
    let conn = mem_conn();
    let h = ProviderHealth::new();
    for _ in 0..THRESHOLD {
        h.record("ep-1", "m", HealthOutcome::Fail(FailureClass::Temp5xx), 503);
    }
    assert!(h.is_degraded("ep-1", "m"));
    h.persist_degraded(&conn);

    // Simulated restart: a fresh instance restores the open circuit (well
    // inside the recovery wait, so it restores as Open).
    let h2 = ProviderHealth::new();
    h2.load(&conn);
    assert!(h2.is_degraded("ep-1", "m"), "open circuit survives restart");

    // Strict breaker: closing requires the recovery wait to elapse (lazy
    // half-open) and then `breaker_success_threshold` probe Oks. Age the
    // restored stamp past the wait instead of sleeping.
    {
        let mut map = h2.inner.lock().unwrap();
        let BreakerState::Open { .. } = map.get("ep-1/m").unwrap().breaker else {
            panic!("restored circuit must be Open");
        };
        let aged = chrono::Utc::now().timestamp_millis() - 61_000;
        map.get_mut("ep-1/m").unwrap().breaker = BreakerState::Open { opened_at_ms: aged };
    }
    h2.record("ep-1", "m", HealthOutcome::Ok, 200);
    h2.record("ep-1", "m", HealthOutcome::Ok, 200);
    assert_eq!(h2.get("ep-1", "m").breaker, BreakerState::Closed);
    h2.persist_degraded(&conn);
    let h3 = ProviderHealth::new();
    h3.load(&conn);
    assert!(!h3.is_degraded("ep-1", "m"), "closed circuit stays closed across restart");
}

#[test]
fn open_circuit_persists_in_old_json_format() {
    // Rows written by the pre-three-state build use `degraded_at_ms` — they
    // must restore as Open (recent enough).
    let conn = mem_conn();
    let now = chrono::Utc::now().timestamp_millis();
    let legacy = serde_json::json!([{
        "endpoint_id": "ep-legacy",
        "degraded_at_ms": now - 1_000,
        "class": "temp5xx"
    }]);
    crate::db::set_setting(&conn, PERSIST_KEY, &legacy).unwrap();
    let h = ProviderHealth::new();
    h.load(&conn);
    assert!(h.is_degraded("ep-legacy", "any-model"), "legacy degraded_at_ms rows restore as any-model Open");
}

#[test]
fn expired_persisted_circuit_is_dropped_at_load() {
    let conn = mem_conn();
    // An opened_at beyond the persistence TTL — the stale-circuit trap the
    // TTL exists for: without the check this endpoint would never route again.
    let stale = serde_json::json!([{
        "endpoint_id": "ep-old",
        "opened_at_ms": chrono::Utc::now().timestamp_millis() - PERSIST_TTL_MS - 1,
        "class": "temp5xx"
    }]);
    crate::db::set_setting(&conn, PERSIST_KEY, &stale).unwrap();
    let h = ProviderHealth::new();
    h.load(&conn);
    assert!(!h.is_degraded("ep-old", "m"));
}

#[test]
fn persist_writes_only_on_transition() {
    let conn = mem_conn();
    let h = ProviderHealth::new();
    for _ in 0..THRESHOLD {
        h.record("ep-1", "m", HealthOutcome::Fail(FailureClass::Temp5xx), 503);
    }
    h.persist_degraded(&conn);
    // Delete the row to detect any rewrite; a further failure does NOT
    // transition (still open, same stamp) so the snapshot is unchanged and
    // the persist must be a no-op.
    conn.execute("DELETE FROM setting_kv WHERE key = 'provider_health'", [])
        .unwrap();
    h.record("ep-1", "m", HealthOutcome::Fail(FailureClass::Temp5xx), 503);
    h.persist_degraded(&conn);
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM setting_kv WHERE key = 'provider_health'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "stable open set must not rewrite the setting");
}

#[test]
fn snapshot_all_reports_effective_states() {
    let h = ProviderHealth::new();
    for _ in 0..THRESHOLD {
        h.record("ep-open", "m", HealthOutcome::Fail(FailureClass::Temp5xx), 503);
    }
    h.record("ep-ok", "m", HealthOutcome::Ok, 200);
    let snap = h.snapshot_all();
    let open = snap.iter().find(|s| s.endpoint_id == "ep-open").unwrap();
    assert_eq!(open.state, "open");
    assert!(open.recovery_in_ms.is_some());
    let ok = snap.iter().find(|s| s.endpoint_id == "ep-ok").unwrap();
    assert_eq!(ok.state, "closed");
    assert_eq!(ok.recovery_in_ms, None);
}
