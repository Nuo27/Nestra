//! Failure classification + provider-health tracking.
//!
//! This module generalizes `quota_refresh::classify_status` (a private
//! function in `quota_refresh.rs`, `FailKind`-based) into the public
//! taxonomy the orchestration layer consults. Every class carries an
//! explicit retry / migrate / circuit-break decision (blanket 5xx→migration
//! is forbidden; temp-5xx retries same-provider first, migration is the
//! escalation after retries fail or the circuit opens).
//!
//! Ships the classifier + an in-memory [`ProviderHealth`] rolling window. The
//! gateway feeds it observed outcomes; the migration engine reads it. The
//! circuit breaker on top is a lazy three-state machine — see
//! [`BreakerState`] (Closed / Open / HalfOpen with probe-based recovery);
//! its parameters live in [`crate::orchestration::gateway::tuning`].
//!
//! ## The six classes (correction #3)
//!
//! | Class | Trigger | Retry same | Migrate | Circuit-break |
//! |-------|---------|-----------|---------|---------------|
//! | [`FailureClass::QuotaExhausted`] | 429 + quota/limit/exceed/throttle/余额 body, or explicit exhausted | No | Yes (prefer same model family) | TTL (reset_at if known) |
//! | [`FailureClass::RateLimit`] | 429 without quota markers (Retry-After) | Yes, honor Retry-After | Only after retries exhausted | Short cooldown |
//! | [`FailureClass::Temp5xx`] | 500/502/503/504 transient | Yes, exp backoff | **Only after retries fail** | Degraded after N/window |
//! | [`FailureClass::Timeout`] | connect/read timeout | Yes, bounded | Only after retries fail | Degraded after repeats |
//! | [`FailureClass::Auth`] | 401/403 | No | **No** (surface to agent) | Flag credential-invalid |
//! | [`FailureClass::BadRequest`] | 400/422 (non-quota 4xx) | No | **No** (surface to agent) | None |
//!
//! A clean response (2xx) is not a failure class — it resets the health
//! window for that endpoint.

use std::collections::HashMap;
use std::sync::Mutex;

/// One of the failure classes. `classify()` is the single producer; the
/// router/migration engine reads the attached decisions via the methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    QuotaExhausted,
    RateLimit,
    Temp5xx,
    Timeout,
    Auth,
    BadRequest,
    /// Ambiguous signal (0/unknown status, 3xx, 1xx) — neither retried nor
    /// migrated. Prevents a blind retry or endpoint switch on noise.
    Unknown,
}

/// Body markers that turn a generic 4xx/429 into a quota/rate-limit class.
///
/// Deliberately NARROW: ordinary rate-limit language (`rate`, `limit`) must
/// NOT classify as `QuotaExhausted` — a routine "rate limit reached" 429 is a
/// retry-same-provider signal, not a migrate signal. Only unambiguous
/// exhaustion/balance language lands on `QuotaExhausted`:
/// `quota`, `exceed` (the exceeded-token-quota shape), `throttl`, and the
/// Chinese 余额 (balance) marker. Mirrors `quota_refresh.rs:430`'s marker
/// list so behavior stays consistent with the keep-awake worker.
const QUOTA_MARKERS: [&str; 4] = ["quota", "exceed", "throttl", "余额"];

impl FailureClass {
    /// Classify an observed HTTP status + (lowercased) body fragment.
    ///
    /// `timeout = true` short-circuits to [`FailureClass::Timeout`] regardless
    /// of status — the gateway sets it when the connect/read deadline elapses
    /// before any response (so `status` is typically 0).
    pub fn classify(status: u16, body: &str, timeout: bool) -> Self {
        if timeout {
            return FailureClass::Timeout;
        }
        // Body-marker check applies to ALL 4xx (incl. 401/403): a provider that
        // reports quota exhaustion as 400/403 instead of 429 must still route
        // to QuotaExhausted so the router migrates rather than surfacing a
        // permanent error. A genuine auth failure carries no quota language.
        let body_has_quota_marker = {
            let low = body.to_ascii_lowercase();
            QUOTA_MARKERS.iter().any(|m| low.contains(m))
        };
        if status == 429 {
            // 429 with quota/rate/limit language ⇒ quota-style (long transient,
            // provider reset lag). A bare 429 with a Retry-After is a vanilla
            // rate-limit.
            return if body_has_quota_marker {
                FailureClass::QuotaExhausted
            } else {
                FailureClass::RateLimit
            };
        }
        if (500..600).contains(&status) {
            return FailureClass::Temp5xx;
        }
        if (400..500).contains(&status) {
            // Quota-marker body on any 4xx (incl. 401/403) wins over Auth /
            // BadRequest: the rare provider that reports exhaustion as 400/403
            // must migrate, not surface.
            if body_has_quota_marker {
                return FailureClass::QuotaExhausted;
            }
            if status == 401 || status == 403 {
                return FailureClass::Auth;
            }
            return FailureClass::BadRequest;
        }
        // Anything else (0/unknown, 3xx, 1xx) we can't classify positively —
        // fall back to a class that NEVER migrates and NEVER retries, so an
        // ambiguous signal can't cause a blind retry or endpoint switch. The
        // gateway should set `timeout` explicitly rather than rely on this
        // fallback.
        FailureClass::Unknown
    }

    /// Should the gateway retry the SAME provider for this class? (correction #3)
    pub fn retry_same_provider(&self) -> bool {
        matches!(
            self,
            FailureClass::RateLimit | FailureClass::Temp5xx | FailureClass::Timeout
        )
    }

    /// Should the router migrate to a fallback for this class? Auth and
    /// BadRequest NEVER migrate — migrating a malformed/unauthorized request
    /// just fails identically elsewhere. QuotaExhausted migrates immediately
    /// (prefer same model family for cache locality); the transient classes
    /// migrate only AFTER in-gateway retries are exhausted (decided, not
    /// here — this just reports the class's migratability). Unknown NEVER
    /// migrates: an ambiguous signal must not cause an endpoint switch.
    pub fn can_migrate(&self) -> bool {
        !matches!(
            self,
            FailureClass::Auth | FailureClass::BadRequest | FailureClass::Unknown
        )
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            FailureClass::QuotaExhausted => "quota_exhausted",
            FailureClass::RateLimit => "rate_limit",
            FailureClass::Temp5xx => "temp_5xx",
            FailureClass::Timeout => "timeout",
            FailureClass::Auth => "auth",
            FailureClass::BadRequest => "bad_request",
            FailureClass::Unknown => "unknown",
        }
    }
}

/// Outcome of one observed request, fed into [`ProviderHealth::record`].
#[derive(Debug, Clone, Copy)]
pub enum HealthOutcome {
    /// 2xx success — resets the failure window for this endpoint.
    Ok,
    /// A failure of the given class.
    Fail(FailureClass),
}

impl HealthOutcome {
    pub fn from_response(status: u16, body: &str, timeout: bool) -> Self {
        if (200..300).contains(&status) {
            HealthOutcome::Ok
        } else {
            HealthOutcome::Fail(FailureClass::classify(status, body, timeout))
        }
    }
}

/// The circuit-breaker state machine per endpoint (cc-switch-style
/// Closed/Open/HalfOpen). Transitions are evaluated lazily at
/// record/query time — there is no background prober task.
///
/// * `Closed → Open`: `breaker_failure_threshold` consecutive migratable
///   failures, OR the rolling-window error rate reaching
///   `breaker_error_rate_pct` over at least `breaker_min_requests` samples
///   (the flapping-endpoint backstop).
/// * `Open → HalfOpen`: after `breaker_recovery_wait_secs` the endpoint
///   becomes eligible again — the next real requests are the probes
///   (concurrent requests during half-open all count as probes).
/// * `HalfOpen → Closed`: `breaker_success_threshold` consecutive Oks.
/// * `HalfOpen → Open`: any migratable failure, with a FRESH open stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BreakerState {
    /// Healthy / eligible. Failures are counted but nothing is excluded.
    #[default]
    Closed,
    /// Circuit open: the router excludes the endpoint until the recovery
    /// wait elapses (`opened_at_ms` is Unix millis).
    Open { opened_at_ms: i64 },
    /// Recovery probes in flight: eligible; `successes` consecutive Oks so
    /// far (closes at `breaker_success_threshold`).
    HalfOpen { successes: u32 },
}

/// Per-endpoint health summary the router reads.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EndpointHealth {
    /// Recent outcomes, oldest-first (capped at [`ProviderHealth::WINDOW`]).
    pub recent: Vec<OutcomeSnap>,
    /// Consecutive failures of any class right now. Reset to 0 on an Ok.
    pub consecutive_failures: u32,
    /// Consecutive failures of a *migratable* class. Crossing
    /// `breaker_failure_threshold` opens the circuit from Closed.
    pub consecutive_migratable: u32,
    /// The breaker state. `Open` is the router's exclusion signal (lazily
    /// advanced to `HalfOpen` once the recovery wait elapses — see
    /// [`EndpointHealth::effective_state`]).
    pub breaker: BreakerState,
    /// Last observed failure class, when any. `None` after an Ok.
    pub last_failure: Option<FailureClass>,
}

impl EndpointHealth {
    /// The breaker state with the lazy `Open → HalfOpen` transition applied:
    /// an Open circuit whose recovery wait has elapsed behaves as HalfOpen
    /// (eligible — the next real request is the probe). This is what BOTH
    /// the router exclusion check and `record` consult, so the lazy
    /// transition is consistent everywhere.
    fn effective_state(&self, now_ms: i64, recovery_wait_ms: i64) -> BreakerState {
        match self.breaker {
            BreakerState::Open { opened_at_ms }
                if now_ms.saturating_sub(opened_at_ms) >= recovery_wait_ms =>
            {
                BreakerState::HalfOpen { successes: 0 }
            }
            s => s,
        }
    }
}

/// One observed outcome in the rolling window.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct OutcomeSnap {
    /// Unix-millis when the outcome was recorded.
    pub at_ms: i64,
    pub ok: bool,
    /// Failure class when `!ok`.
    pub class: Option<FailureClass>,
    /// HTTP status observed (0 on timeout / no response).
    pub status: u16,
}

/// In-memory rolling health window across all endpoints. Process-global (one
/// instance in `AppState`; tests construct their own).
///
/// Thread-safe via an internal `Mutex`. The window is bounded and cheap; the
/// gateway records one outcome per proxied request.
pub struct ProviderHealth {
    inner: Mutex<HashMap<String, EndpointHealth>>,
    /// Live breaker parameters (failure threshold / recovery wait / success
    /// threshold / error-rate backstop). Shared with `AppState` so Settings
    /// edits hot-apply. Std RwLock: brief, never-across-`.await` sections.
    tuning: crate::orchestration::gateway::tuning::SharedTuning,
    /// Serialized last-persisted degraded set — the no-op guard that keeps
    /// `persist_degraded` off the hot path while the set is stable.
    last_persist: Mutex<Option<String>>,
}

/// How many recent outcomes to retain per endpoint (the error-rate
/// backstop's sample window).
pub const WINDOW: usize = 20;
/// A persisted open circuit is dropped at load when older than this
/// (Smart Gateway fix 3, the stale-circuit trap): a restart hours later
/// must not resurrect a long-dead exclusion. The in-memory breaker itself
/// recovers via the (much shorter) `breaker_recovery_wait_secs`.
pub const PERSIST_TTL_MS: i64 = 10 * 60 * 1000;
/// `setting_kv` key for the persisted open-circuit set.
const PERSIST_KEY: &str = "provider_health";

/// One persisted open-circuit row. Credential-free (endpoint id +
/// timestamps + a failure-class label). `opened_at_ms` reads the
/// pre-three-state `degraded_at_ms` key too, so persisted rows from an
/// older build restore as `Open` unchanged.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DegradedEntry {
    endpoint_id: String,
    #[serde(alias = "degraded_at_ms")]
    opened_at_ms: i64,
    class: FailureClass,
}

impl Default for ProviderHealth {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderHealth {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            tuning: crate::orchestration::gateway::tuning::shared_default(),
            last_persist: Mutex::new(None),
        }
    }

    /// Construct sharing a live tuning slot (the `AppState` path — Settings
    /// edits apply to the breaker without a restart). `new()` owns a private
    /// default slot, which is what tests want.
    pub fn with_tuning(tuning: crate::orchestration::gateway::tuning::SharedTuning) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            tuning,
            last_persist: Mutex::new(None),
        }
    }

    /// Record an outcome for `endpoint_id` and walk the breaker state
    /// machine (see [`BreakerState`]). Parameters come from the live tuning
    /// slot; the lazy `Open → HalfOpen` transition is applied first so a
    /// probe outcome lands in the right state.
    pub fn record(&self, endpoint_id: &str, outcome: HealthOutcome, status: u16) {
        let tuning = crate::orchestration::gateway::tuning::snapshot(&self.tuning);
        let mut map = self.inner.lock().expect("health lock poisoned");
        let h = map.entry(endpoint_id.to_string()).or_default();
        let now = chrono::Utc::now().timestamp_millis();
        let recovery_wait_ms = (tuning.breaker_recovery_wait_secs as i64) * 1000;
        let state = h.effective_state(now, recovery_wait_ms);
        match outcome {
            HealthOutcome::Ok => {
                h.consecutive_failures = 0;
                h.consecutive_migratable = 0;
                h.last_failure = None;
                h.breaker = match state {
                    // A successful probe counts toward closing. A
                    // not-yet-expired Open stays Open even on an Ok: only
                    // post-wait probes may close a strict breaker — an
                    // in-flight request that started before the circuit
                    // opened must not silently resurrect the endpoint (and
                    // flapping open/close oscillation must not happen).
                    BreakerState::HalfOpen { successes } => {
                        let successes = successes + 1;
                        if successes as u64 >= tuning.breaker_success_threshold {
                            BreakerState::Closed
                        } else {
                            BreakerState::HalfOpen { successes }
                        }
                    }
                    o @ BreakerState::Open { .. } => o,
                    BreakerState::Closed => BreakerState::Closed,
                };
            }
            HealthOutcome::Fail(class) => {
                h.consecutive_failures = h.consecutive_failures.saturating_add(1);
                if class.can_migrate() {
                    h.consecutive_migratable = h.consecutive_migratable.saturating_add(1);
                }
                h.last_failure = Some(class);
                h.breaker = match state {
                    // A failed probe re-opens with a FRESH stamp so the
                    // recovery wait restarts.
                    BreakerState::HalfOpen { .. } if class.can_migrate() => {
                        BreakerState::Open { opened_at_ms: now }
                    }
                    // Still within the wait window: keep the original stamp
                    // (a still-failing endpoint must not reset its own wait).
                    o @ BreakerState::Open { .. } => o,
                    BreakerState::HalfOpen { .. } | BreakerState::Closed => {
                        if h.consecutive_migratable as u64 >= tuning.breaker_failure_threshold {
                            BreakerState::Open { opened_at_ms: now }
                        } else {
                            BreakerState::Closed
                        }
                    }
                };
            }
        }
        h.recent.push(OutcomeSnap {
            at_ms: now,
            ok: matches!(outcome, HealthOutcome::Ok),
            class: match outcome {
                HealthOutcome::Ok => None,
                HealthOutcome::Fail(c) => Some(c),
            },
            status,
        });
        if h.recent.len() > WINDOW {
            let drop_n = h.recent.len() - WINDOW;
            h.recent.drain(0..drop_n);
        }
        // Error-rate backstop (flapping endpoints whose consecutive count
        // never reaches the threshold because Oks keep resetting it). Only
        // meaningful from Closed, and only evaluated on a FAILURE — firing
        // it after an Ok would let a stale window instantly re-open a
        // circuit that half-open probes just closed (livelock: an excluded
        // endpoint gets no traffic, so its window never drains). 0 disables.
        if tuning.breaker_error_rate_pct > 0
            && matches!(outcome, HealthOutcome::Fail(_))
            && h.breaker == BreakerState::Closed
            && h.recent.len() >= tuning.breaker_min_requests as usize
        {
            let fails = h.recent.iter().filter(|o| !o.ok).count();
            let pct = fails as u64 * 100 / h.recent.len() as u64;
            if pct >= tuning.breaker_error_rate_pct {
                h.breaker = BreakerState::Open { opened_at_ms: now };
            }
        }
    }

    /// Snapshot the health for one endpoint (defaulted if unseen).
    pub fn get(&self, endpoint_id: &str) -> EndpointHealth {
        let map = self.inner.lock().expect("health lock poisoned");
        map.get(endpoint_id)
            .cloned()
            .unwrap_or_default()
    }

    /// All endpoints the router should consider eligible (breaker not Open).
    pub fn eligible(&self, candidates: &[String]) -> Vec<String> {
        let recovery_wait_ms = self.recovery_wait_ms();
        let now = chrono::Utc::now().timestamp_millis();
        let map = self.inner.lock().expect("health lock poisoned");
        candidates
            .iter()
            .filter(|id| {
                !map.get(*id)
                    .map(|h| {
                        matches!(
                            h.effective_state(now, recovery_wait_ms),
                            BreakerState::Open { .. }
                        )
                    })
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    /// `true` if the endpoint's circuit is Open (excluded). A HalfOpen
    /// endpoint (recovery wait elapsed) is eligible — the next request is
    /// the probe.
    pub fn is_degraded(&self, endpoint_id: &str) -> bool {
        let recovery_wait_ms = self.recovery_wait_ms();
        let now = chrono::Utc::now().timestamp_millis();
        self.inner
            .lock()
            .expect("health lock poisoned")
            .get(endpoint_id)
            .map(|h| {
                matches!(
                    h.effective_state(now, recovery_wait_ms),
                    BreakerState::Open { .. }
                )
            })
            .unwrap_or(false)
    }

    fn recovery_wait_ms(&self) -> i64 {
        (crate::orchestration::gateway::tuning::snapshot(&self.tuning)
            .breaker_recovery_wait_secs as i64)
            * 1000
    }

    /// Clear all health state (the Providers page "reset health" action and
    /// tests).
    pub fn clear(&self) {
        self.inner.lock().expect("health lock poisoned").clear();
    }

    /// UI-facing snapshot across every tracked endpoint: the effective
    /// breaker state (with the lazy half-open transition applied) plus the
    /// counters the health badge renders. Read by the
    /// `provider_health_snapshot` command.
    pub fn snapshot_all(&self) -> Vec<EndpointHealthSnap> {
        let recovery_wait_ms = self.recovery_wait_ms();
        let now = chrono::Utc::now().timestamp_millis();
        let map = self.inner.lock().expect("health lock poisoned");
        let mut out: Vec<EndpointHealthSnap> = map
            .iter()
            .map(|(id, h)| {
                let state = h.effective_state(now, recovery_wait_ms);
                EndpointHealthSnap {
                    endpoint_id: id.clone(),
                    state: match state {
                        BreakerState::Closed => "closed",
                        BreakerState::Open { .. } => "open",
                        BreakerState::HalfOpen { .. } => "half_open",
                    },
                    consecutive_failures: h.consecutive_failures,
                    last_failure: h.last_failure,
                    // For an Open circuit: how long until half-open probes.
                    recovery_in_ms: match state {
                        BreakerState::Open { opened_at_ms } => Some(
                            (opened_at_ms + recovery_wait_ms - now).max(0),
                        ),
                        _ => None,
                    },
                }
            })
            .collect();
        out.sort_by(|a, b| a.endpoint_id.cmp(&b.endpoint_id));
        out
    }

    /// Persist the open-circuit set to `setting_kv` — called from the gateway's
    /// outcome recording while it already holds the DB lock. Writes only on a
    /// open↔closed TRANSITION: the cached last snapshot makes the stable
    /// case a compare-and-return, so the per-request hot path adds no write.
    /// Best-effort; a failure is logged and retried on the next transition.
    pub fn persist_degraded(&self, conn: &rusqlite::Connection) {
        let now = chrono::Utc::now().timestamp_millis();
        let entries = fresh_degraded_entries(
            &self.inner.lock().expect("health lock poisoned"),
            now,
            self.recovery_wait_ms(),
        );
        let Ok(value) = serde_json::to_value(&entries) else {
            return;
        };
        let serialized = value.to_string();
        let mut last = self.last_persist.lock().expect("health persist lock poisoned");
        if last.as_deref() == Some(serialized.as_str()) {
            return;
        }
        if let Err(e) = crate::db::set_setting(conn, PERSIST_KEY, &value) {
            tracing::warn!("gateway: failed to persist provider health: {e}");
            return;
        }
        *last = Some(serialized);
    }

    /// Restore the open-circuit set at startup. Entries past the persistence
    /// TTL are dropped: a stale persisted circuit must not exclude an
    /// endpoint forever (the stale-circuit trap). A restored Open whose
    /// recovery wait already elapsed lazily becomes HalfOpen at the first
    /// query — correct breaker semantics after a long downtime. Restored
    /// entries carry the threshold already crossed so the circuit holds
    /// until probes close or re-open it.
    pub fn load(&self, conn: &rusqlite::Connection) {
        let Ok(Some(v)) = crate::db::get_setting(conn, PERSIST_KEY) else {
            return;
        };
        let Ok(entries) = serde_json::from_value::<Vec<DegradedEntry>>(v) else {
            return;
        };
        let tuning = crate::orchestration::gateway::tuning::snapshot(&self.tuning);
        let now = chrono::Utc::now().timestamp_millis();
        let mut map = self.inner.lock().expect("health lock poisoned");
        for e in entries {
            if now.saturating_sub(e.opened_at_ms) >= PERSIST_TTL_MS {
                continue;
            }
            map.insert(
                e.endpoint_id,
                EndpointHealth {
                    breaker: BreakerState::Open {
                        opened_at_ms: e.opened_at_ms,
                    },
                    consecutive_migratable: tuning.breaker_failure_threshold as u32,
                    consecutive_failures: tuning.breaker_failure_threshold as u32,
                    last_failure: Some(e.class),
                    ..Default::default()
                },
            );
        }
        // Sync the persist no-op guard so an unchanged set doesn't rewrite.
        let entries = fresh_degraded_entries(&map, now, self.recovery_wait_ms());
        if let Ok(s) = serde_json::to_string(&entries) {
            *self.last_persist.lock().expect("health persist lock poisoned") = Some(s);
        }
    }
}

/// One row of [`ProviderHealth::snapshot_all`] (credential-free).
#[derive(Debug, Clone, serde::Serialize)]
pub struct EndpointHealthSnap {
    pub endpoint_id: String,
    /// "closed" | "open" | "half_open".
    pub state: &'static str,
    pub consecutive_failures: u32,
    pub last_failure: Option<FailureClass>,
    /// Present while the circuit is Open: millis until half-open probing.
    pub recovery_in_ms: Option<i64>,
}

/// TTL-fresh Open-circuit entries, sorted by endpoint id for a stable
/// comparison string (HashMap order must not decide whether a transition
/// "happened"). An Open whose recovery wait already elapsed (lazy HalfOpen)
/// is NOT persisted — restart would immediately half-open it anyway.
fn fresh_degraded_entries(
    map: &HashMap<String, EndpointHealth>,
    now_ms: i64,
    recovery_wait_ms: i64,
) -> Vec<DegradedEntry> {
    let mut entries: Vec<DegradedEntry> = map
        .iter()
        .filter(|(_, h)| {
            matches!(
                h.effective_state(now_ms, recovery_wait_ms),
                BreakerState::Open { .. }
            )
        })
        .map(|(id, h)| DegradedEntry {
            endpoint_id: id.clone(),
            opened_at_ms: match h.breaker {
                BreakerState::Open { opened_at_ms } => opened_at_ms,
                _ => now_ms,
            },
            class: h.last_failure.unwrap_or(FailureClass::Unknown),
        })
        .collect();
    entries.sort_by(|a, b| a.endpoint_id.cmp(&b.endpoint_id));
    entries
}

#[cfg(test)]
mod tests;
