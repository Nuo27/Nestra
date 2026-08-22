use super::*;

fn ep() -> String {
    "ep-1".to_string()
}

// ---- Auth / BadRequest always surface ----

#[test]
fn auth_never_migrates_or_retries() {
    for attempts in 1..=5 {
        for gen in [false, true] {
            for side in [false, true] {
                for policy in [true, false] {
                    let d = decide(
                        FailureClass::Auth,
                        attempts,
                        gen,
                        side,
                        policy,
                        ep(),
                    );
                    assert_eq!(
                        d,
                        MigrationDecision::Surface,
                        "Auth must surface (attempts={attempts}, gen={gen}, side={side}, policy={policy})"
                    );
                }
            }
        }
    }
}

#[test]
fn bad_request_never_migrates_or_retries() {
    let d = decide(FailureClass::BadRequest, 1, false, false, true, ep());
    assert_eq!(d, MigrationDecision::Surface);
}

// ---- side-effect risk always surfaces ----

#[test]
fn side_effect_risk_surfaces_even_for_migratable_class() {
    // A quota-exhausted failure on a tool-calling request → surface, not migrate.
    let d = decide(
        FailureClass::QuotaExhausted,
        1,
        false,
        true,
        true,
        ep(),
    );
    assert_eq!(d, MigrationDecision::Surface);
    // Same for temp-5xx with retries left.
    let d = decide(FailureClass::Temp5xx, 1, false, true, true, ep());
    assert_eq!(d, MigrationDecision::Surface);
}

// ---- QuotaExhausted migrates immediately ----

#[test]
fn quota_exhausted_migrates_immediately_when_policy_allows() {
    let d = decide(FailureClass::QuotaExhausted, 1, false, false, true, ep());
    match d {
        MigrationDecision::Migrate {
            reason,
            from_endpoint_id,
            generation_broken,
        } => {
            assert_eq!(reason, MigrationReason::QuotaExhausted);
            assert_eq!(from_endpoint_id, "ep-1");
            assert!(!generation_broken);
        }
        other => panic!("expected Migrate, got {other:?}"),
    }
}

#[test]
fn quota_exhausted_surfaces_when_policy_disallows() {
    let d = decide(FailureClass::QuotaExhausted, 1, false, false, false, ep());
    assert_eq!(d, MigrationDecision::Surface);
}

#[test]
fn quota_exhausted_with_gen_started_flags_broken() {
    let d = decide(FailureClass::QuotaExhausted, 1, true, false, true, ep());
    match d {
        MigrationDecision::Migrate { generation_broken, .. } => assert!(generation_broken),
        other => panic!("expected Migrate, got {other:?}"),
    }
}

// ---- RateLimit / Temp5xx / Timeout retry then migrate ----

#[test]
fn temp_5xx_retries_same_up_to_max() {
    // attempts 1, 2, 3 → RetrySame (MAX_RETRIES=3).
    for attempts in 1..MAX_RETRIES {
        let d = decide(FailureClass::Temp5xx, attempts, false, false, true, ep());
        match d {
            MigrationDecision::RetrySame {
                attempts_so_far,
                max,
                generation_broken,
                ..
            } => {
                assert_eq!(attempts_so_far, attempts);
                assert_eq!(max, MAX_RETRIES);
                assert!(!generation_broken);
            }
            other => panic!("attempts={attempts}: expected RetrySame, got {other:?}"),
        }
    }
    // attempts == MAX_RETRIES → escalate.
    let d = decide(FailureClass::Temp5xx, MAX_RETRIES, false, false, true, ep());
    match d {
        MigrationDecision::Migrate { reason, .. } => {
            assert_eq!(reason, MigrationReason::RetriesExhausted)
        }
        other => panic!("expected Migrate (retries exhausted), got {other:?}"),
    }
}

#[test]
fn retries_exhausted_surfaces_when_policy_disallows() {
    let d = decide(FailureClass::Temp5xx, MAX_RETRIES, false, false, false, ep());
    assert_eq!(d, MigrationDecision::Surface);
}

#[test]
fn rate_limit_and_timeout_behave_like_temp_5xx() {
    for class in [FailureClass::RateLimit, FailureClass::Timeout] {
        let d = decide(class, 1, false, false, true, ep());
        assert!(matches!(d, MigrationDecision::RetrySame { .. }));
        let d = decide(class, MAX_RETRIES, false, false, true, ep());
        assert!(matches!(d, MigrationDecision::Migrate { .. }));
    }
}

#[test]
fn gen_started_flags_retry_broken() {
    let d = decide(FailureClass::Temp5xx, 1, true, false, true, ep());
    match d {
        MigrationDecision::RetrySame { generation_broken, .. } => assert!(generation_broken),
        other => panic!("expected RetrySame, got {other:?}"),
    }
}

// ---- backoff schedule ----

#[test]
fn backoff_schedule_is_1_2_4_seconds() {
    assert_eq!(backoff_for(0), Duration::from_secs(1));
    assert_eq!(backoff_for(1), Duration::from_secs(2));
    assert_eq!(backoff_for(2), Duration::from_secs(4));
    // Clamped at 4s for higher counts.
    assert_eq!(backoff_for(5), Duration::from_secs(4));
}

// ---- side-effect body detection ----

#[test]
fn body_with_tools_is_side_effect_risk() {
    // Anthropic shape
    assert!(body_has_side_effect_risk(br#"{"tools":[{"name":"bash"}]}"#));
    // OpenAI shape
    assert!(body_has_side_effect_risk(br#"{"tools":[{"type":"function"}]}"#));
    assert!(body_has_side_effect_risk(br#"{"functions":[{"name":"f"}]}"#));
    // Empty tools array → no risk
    assert!(!body_has_side_effect_risk(br#"{"tools":[]}"#));
    // No tools → no risk
    assert!(!body_has_side_effect_risk(br#"{"messages":[]}"#));
    // Malformed → no risk (upstream will reject)
    assert!(!body_has_side_effect_risk(b"not json"));
}

#[test]
fn migration_reason_strings_match_taxonomy() {
    // The persisted reason vocabulary must be the migratable subset of
    // FailureClass (by as_str) plus RetriesExhausted.
    assert_eq!(MigrationReason::QuotaExhausted.as_str(), FailureClass::QuotaExhausted.as_str());
    assert_eq!(MigrationReason::RateLimit.as_str(), FailureClass::RateLimit.as_str());
    assert_eq!(MigrationReason::Temp5xx.as_str(), FailureClass::Temp5xx.as_str());
    assert_eq!(MigrationReason::Timeout.as_str(), FailureClass::Timeout.as_str());
    assert_eq!(MigrationReason::RetriesExhausted.as_str(), "retries_exhausted");
    // Auth/BadRequest deliberately have NO MigrationReason — they never migrate.
}