//! Migration decision engine — quota-aware fallback.
//!
//! Given an observed failure, [`decide`] returns exactly what the gateway
//! should do next: retry the same provider, migrate to a fallback, or surface
//! the error to the agent. The rules encode the generation/side-effect
//! boundary and the failure taxonomy contract (no blanket 5xx→migration;
//! Auth/BadRequest never migrate).
//!
//! This module is **pure and stateless** — it inspects no DB, no network. The
//! protocol handler calls it after each failed `attempt_request` and acts on
//! the [`MigrationDecision`]. That separation keeps the decision table
//! exhaustively unit-testable.
//!
//! ## The decision table
//!
//! | Class | gen not started, retries left | gen started | side-effect risk |
//! |-------|------------------------------|------------|------------------|
//! | Auth | Surface | Surface | Surface |
//! | BadRequest | Surface | Surface | Surface |
//! | QuotaExhausted | Migrate (if policy) else Surface | Migrate\* (gen_broken) | Surface |
//! | RateLimit/Temp5xx/Timeout, retries left | RetrySame | Migrate\* (gen_broken) | Surface |
//! | RateLimit/Temp5xx/Timeout, retries exhausted | Migrate (if policy) else Surface | Migrate\* (gen_broken) | Surface |
//!
//! \* `gen started` forces `generation_broken = true` on the next attempt
//!   (whether retry or migrate). `side_effect_risk` forces Surface to avoid
//!   double-executing a tool/external action whose success is unconfirmed.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::health::FailureClass;

/// Hard cap on same-provider retries for transient classes (RateLimit/
/// Temp5xx/Timeout). After this many attempts, the engine escalates to
/// Migrate (or Surface if policy disallows migration).
pub const MAX_RETRIES: u32 = 3;

/// Exponential-backoff schedule for `RetrySame` (1s, 2s, 4s). Indexed by
/// `attempts_so_far` (clamped). The protocol handler sleeps this before the
/// next attempt.
pub fn backoff_for(attempts_so_far: u32) -> Duration {
    let idx = attempts_so_far.min(2) as usize; // 0→1s, 1→2s, ≥2→4s
    Duration::from_secs(1u64 << idx)
}

/// Why a request migrated to a different endpoint. The vocabulary is exactly
/// the migratable subset of [`FailureClass`] (Auth/BadRequest are excluded —
/// they never migrate). Persisted as the `reason` column on `route_migration`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationReason {
    QuotaExhausted,
    RateLimit,
    /// `snake_case` would render this as `temp5xx`; the persisted `reason`
    /// column and `as_str()` use `temp_5xx` — pin the serde spelling to the
    /// same vocabulary so the two representations can never diverge.
    #[serde(rename = "temp_5xx")]
    Temp5xx,
    Timeout,
    /// Transient retries (RateLimit/Temp5xx/Timeout) were exhausted without
    /// success, so the engine escalated to a fallback endpoint.
    RetriesExhausted,
}

impl MigrationReason {
    /// The string persisted in `route_migration.reason`. Matches the
    /// `FailureClass::as_str` for the direct classes; `RetriesExhausted` is
    /// the escalation-only value.
    pub fn as_str(&self) -> &'static str {
        match self {
            MigrationReason::QuotaExhausted => "quota_exhausted",
            MigrationReason::RateLimit => "rate_limit",
            MigrationReason::Temp5xx => "temp_5xx",
            MigrationReason::Timeout => "timeout",
            MigrationReason::RetriesExhausted => "retries_exhausted",
        }
    }
}

/// What the gateway should do after a failed attempt. The protocol handler
/// acts on this; the decision itself carries no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationDecision {
    /// Retry the SAME provider (transient 5xx / rate-limit / timeout) after a
    /// backoff sleep. `attempts_so_far` is the count BEFORE this retry.
    RetrySame {
        attempts_so_far: u32,
        max: u32,
        backoff: Duration,
        /// True when the failed attempt had already streamed bytes — the new
        /// attempt's `RouteRecord` must be flagged `generation_broken`.
        generation_broken: bool,
    },
    /// Migrate to a fallback endpoint. `reason` is persisted on the
    /// `route_migration` row; `generation_broken` carries the same honesty
    /// flag as `RetrySame`.
    Migrate {
        reason: MigrationReason,
        from_endpoint_id: String,
        generation_broken: bool,
    },
    /// Surface the upstream error to the agent as-is. Used for Auth/BadRequest,
    /// side-effect-risk requests we can't confirm finalized, and migratable
    /// failures when policy disallows migration or no fallback is eligible.
    Surface,
}

/// Decide what to do after a failed attempt.
///
/// **Inputs** (all supplied by the protocol handler):
/// - `class`: the [`FailureClass`] classified from the observed status/body/timeout.
/// - `attempts_so_far`: how many attempts (including the just-failed one) have
///   been made for this task on the SAME provider. 1 = first failure.
/// - `generation_started`: **state 2** — true if ANY response bytes were
///   received before the failure (SSE stream began). Forces
///   `generation_broken` on the next attempt.
/// - `side_effect_risk`: **state 3** — true if the request body declared
///   tools/functions (may trigger tool execution upstream). Forces `Surface`
///   to avoid double-execution.
/// - `policy_allows_migrate`: from `routing_policy.migrate_on_quota` (and the
///   broader migration toggle). When false, migratable failures `Surface`.
/// - `from_endpoint_id`: the endpoint that just failed (recorded on the
///   `Migrate` decision's `route_migration` row).
pub fn decide(
    class: FailureClass,
    attempts_so_far: u32,
    generation_started: bool,
    side_effect_risk: bool,
    policy_allows_migrate: bool,
    from_endpoint_id: String,
) -> MigrationDecision {
    // State 3: side-effect risk with unconfirmed finalization → never blind-retry.
    // (The protocol handler only calls decide() on a failure, so success is by
    // definition unconfirmed here.) This wins over everything: a tool-calling
    // request that failed mid-flight is surfaced, not re-executed.
    if side_effect_risk {
        return MigrationDecision::Surface;
    }

    // Auth (401/403) and BadRequest (400/422) NEVER migrate and NEVER retry —
    // migrating/retrying an unauthorized or malformed request fails identically
    // elsewhere.
    if !class.can_migrate() {
        return MigrationDecision::Surface;
    }

    // `generation_started` does NOT change WHAT we do (retry vs migrate) — it
    // only flips the honesty flag on the next attempt. Capture it once.
    let gen_broken = generation_started;

    // QuotaExhausted migrates immediately (no same-provider retry — the window
    // is exhausted, retrying the same endpoint just burns another 429).
    if matches!(class, FailureClass::QuotaExhausted) {
        return if policy_allows_migrate {
            MigrationDecision::Migrate {
                reason: MigrationReason::QuotaExhausted,
                from_endpoint_id,
                generation_broken: gen_broken,
            }
        } else {
            MigrationDecision::Surface
        };
    }

    // RateLimit / Temp5xx / Timeout: retry same provider up to MAX_RETRIES,
    // then escalate to Migrate.
    debug_assert!(
        class.retry_same_provider(),
        "non-retryable migratable class reached the retry branch"
    );
    if attempts_so_far < MAX_RETRIES {
        return MigrationDecision::RetrySame {
            attempts_so_far,
            max: MAX_RETRIES,
            // `attempts_so_far` is ≥ 1 here (0 never reaches this branch —
            // the caller classifies before deciding), but guard anyway: a
            // debug underflow panic / release wrap-to-4s on an unexpected 0
            // is worse than a slightly-longer first backoff.
            backoff: backoff_for(attempts_so_far.saturating_sub(1)),
            generation_broken: gen_broken,
        };
    }
    // Retries exhausted → migrate (if allowed) else surface.
    if policy_allows_migrate {
        MigrationDecision::Migrate {
            reason: MigrationReason::RetriesExhausted,
            from_endpoint_id,
            generation_broken: gen_broken,
        }
    } else {
        MigrationDecision::Surface
    }
}

/// `true` when a failed attempt's body carries tool/function declarations —
/// the conservative side-effect-risk detector (state 3). Anthropic: non-empty
/// `tools` array. OpenAI: non-empty `tools` OR `functions` array. A request
/// with no tools is treated as side-effect-free (safe to retry/migrate).
///
/// This is intentionally conservative: a tool-calling request that fails
/// mid-flight might have already executed the tool upstream, so we surface
/// rather than risk a double-execution. Non-tool requests (most chat turns)
/// retry/migrate freely.
pub fn body_has_side_effect_risk(body: &[u8]) -> bool {
    // Stream-parse: we only need to know whether `tools`/`functions` is a
    // non-empty top-level array — building a full JSON tree per request on
    // the failure hot path was pure waste.
    let mut de = serde_json::Deserializer::from_slice(body).into_iter::<serde_json::Value>();
    let Some(Ok(v)) = de.next() else {
        return false; // malformed body isn't a side-effect risk (the upstream rejects it)
    };
    let tools_nonempty = |key: &str| -> bool {
        v.get(key)
            .and_then(|t| t.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    };
    tools_nonempty("tools") || tools_nonempty("functions")
}

#[cfg(test)]
mod tests {
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
}
