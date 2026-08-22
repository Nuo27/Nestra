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
//! gateway feeds it observed outcomes; the migration engine reads it.
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

/// Per-endpoint health summary the router reads.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EndpointHealth {
    /// Recent outcomes, oldest-first (capped at [`ProviderHealth::WINDOW`]).
    pub recent: Vec<OutcomeSnap>,
    /// Consecutive failures of any class right now. Reset to 0 on an Ok.
    pub consecutive_failures: u32,
    /// Consecutive failures of a *migratable* class. The router opens the
    /// circuit (skips the endpoint) when this crosses
    /// [`ProviderHealth::DEGRADED_THRESHOLD`].
    pub consecutive_migratable: u32,
    /// True once the circuit has opened; the router excludes the endpoint
    /// until an Ok clears it.
    pub degraded: bool,
    /// When the circuit opened (Unix millis). A degraded endpoint receives no
    /// traffic, so without a TTL it could never get the Ok that clears it —
    /// [`ProviderHealth::DEGRADED_TTL_MS`] bounds the exclusion window.
    pub degraded_at_ms: Option<i64>,
    /// Last observed failure class, when any. `None` after an Ok.
    pub last_failure: Option<FailureClass>,
}

impl EndpointHealth {
    /// Degraded AND within the TTL. This is the router's exclusion signal:
    /// an expired circuit is treated as eligible so the next request probes
    /// the endpoint (an Ok clears it; a failure re-opens it with a fresh
    /// timestamp).
    fn effectively_degraded(&self, now_ms: i64) -> bool {
        self.degraded
            && self
                .degraded_at_ms
                .is_some_and(|at| now_ms.saturating_sub(at) < DEGRADED_TTL_MS)
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
    /// Serialized last-persisted degraded set — the no-op guard that keeps
    /// `persist_degraded` off the hot path while the set is stable.
    last_persist: Mutex<Option<String>>,
}

/// How many recent outcomes to retain per endpoint.
pub const WINDOW: usize = 20;
/// Consecutive migratable failures (quota/rate/5xx/timeout) before the
/// endpoint is marked degraded and excluded from routing until an Ok clears it.
pub const DEGRADED_THRESHOLD: u32 = 3;
/// A degraded circuit is held out for at most this long (Smart Gateway fix 3):
/// the degraded endpoint gets no traffic, so without a bound a
/// restart-persisted circuit could stay open forever (stale-circuit trap).
pub const DEGRADED_TTL_MS: i64 = 10 * 60 * 1000;
/// `setting_kv` key for the persisted degraded set.
const PERSIST_KEY: &str = "provider_health";

/// One persisted degraded-circuit row. Credential-free (endpoint id +
/// timestamps + a failure-class label).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DegradedEntry {
    endpoint_id: String,
    degraded_at_ms: i64,
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
            last_persist: Mutex::new(None),
        }
    }

    /// Record an outcome for `endpoint_id`. Ok resets the consecutive counters
    /// and clears the degraded flag; failures bump the counters and push a
    /// snapshot onto the rolling window.
    pub fn record(&self, endpoint_id: &str, outcome: HealthOutcome, status: u16) {
        let mut map = self.inner.lock().expect("health lock poisoned");
        let h = map.entry(endpoint_id.to_string()).or_default();
        let now = chrono::Utc::now().timestamp_millis();
        match outcome {
            HealthOutcome::Ok => {
                h.consecutive_failures = 0;
                h.consecutive_migratable = 0;
                h.degraded = false;
                h.degraded_at_ms = None;
                h.last_failure = None;
            }
            HealthOutcome::Fail(class) => {
                h.consecutive_failures = h.consecutive_failures.saturating_add(1);
                if class.can_migrate() {
                    h.consecutive_migratable = h.consecutive_migratable.saturating_add(1);
                }
                if h.consecutive_migratable >= DEGRADED_THRESHOLD {
                    // Stamp the open time on the healthy→degraded transition
                    // only, so a still-failing endpoint doesn't reset its own
                    // TTL window.
                    if !h.degraded {
                        h.degraded_at_ms = Some(now);
                    }
                    h.degraded = true;
                }
                h.last_failure = Some(class);
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
    }

    /// Snapshot the health for one endpoint (defaulted if unseen).
    pub fn get(&self, endpoint_id: &str) -> EndpointHealth {
        let map = self.inner.lock().expect("health lock poisoned");
        map.get(endpoint_id)
            .cloned()
            .unwrap_or_default()
    }

    /// All endpoints the router should consider eligible (not degraded).
    pub fn eligible(&self, candidates: &[String]) -> Vec<String> {
        let now = chrono::Utc::now().timestamp_millis();
        let map = self.inner.lock().expect("health lock poisoned");
        candidates
            .iter()
            .filter(|id| {
                !map.get(*id)
                    .map(|h| h.effectively_degraded(now))
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    /// `true` if the endpoint is currently degraded (circuit open, within the
    /// TTL).
    pub fn is_degraded(&self, endpoint_id: &str) -> bool {
        let now = chrono::Utc::now().timestamp_millis();
        self.inner
            .lock()
            .expect("health lock poisoned")
            .get(endpoint_id)
            .map(|h| h.effectively_degraded(now))
            .unwrap_or(false)
    }

    /// Clear all health state (used by tests and a future "reset health" UI).
    pub fn clear(&self) {
        self.inner.lock().expect("health lock poisoned").clear();
    }

    /// Persist the degraded set to `setting_kv` — called from the gateway's
    /// outcome recording while it already holds the DB lock. Writes only on a
    /// degraded↔healthy TRANSITION: the cached last snapshot makes the stable
    /// case a compare-and-return, so the per-request hot path adds no write.
    /// Best-effort; a failure is logged and retried on the next transition.
    pub fn persist_degraded(&self, conn: &rusqlite::Connection) {
        let now = chrono::Utc::now().timestamp_millis();
        let entries = fresh_degraded_entries(&self.inner.lock().expect("health lock poisoned"), now);
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

    /// Restore the degraded set at startup. Entries past the TTL are dropped:
    /// a stale persisted circuit must not exclude an endpoint forever — the
    /// next request probes it. Restored entries carry the threshold already
    /// crossed so the circuit holds until an Ok or the TTL.
    pub fn load(&self, conn: &rusqlite::Connection) {
        let Ok(Some(v)) = crate::db::get_setting(conn, PERSIST_KEY) else {
            return;
        };
        let Ok(entries) = serde_json::from_value::<Vec<DegradedEntry>>(v) else {
            return;
        };
        let now = chrono::Utc::now().timestamp_millis();
        let mut map = self.inner.lock().expect("health lock poisoned");
        for e in entries {
            if now.saturating_sub(e.degraded_at_ms) >= DEGRADED_TTL_MS {
                continue;
            }
            map.insert(
                e.endpoint_id,
                EndpointHealth {
                    degraded: true,
                    degraded_at_ms: Some(e.degraded_at_ms),
                    consecutive_migratable: DEGRADED_THRESHOLD,
                    consecutive_failures: DEGRADED_THRESHOLD,
                    last_failure: Some(e.class),
                    ..Default::default()
                },
            );
        }
        // Sync the persist no-op guard so an unchanged set doesn't rewrite.
        let entries = fresh_degraded_entries(&map, now);
        if let Ok(s) = serde_json::to_string(&entries) {
            *self.last_persist.lock().expect("health persist lock poisoned") = Some(s);
        }
    }
}

/// TTL-fresh degraded entries, sorted by endpoint id for a stable comparison
/// string (HashMap order must not decide whether a transition "happened").
fn fresh_degraded_entries(
    map: &HashMap<String, EndpointHealth>,
    now_ms: i64,
) -> Vec<DegradedEntry> {
    let mut entries: Vec<DegradedEntry> = map
        .iter()
        .filter(|(_, h)| h.effectively_degraded(now_ms))
        .map(|(id, h)| DegradedEntry {
            endpoint_id: id.clone(),
            degraded_at_ms: h.degraded_at_ms.unwrap_or(now_ms),
            class: h.last_failure.unwrap_or(FailureClass::Unknown),
        })
        .collect();
    entries.sort_by(|a, b| a.endpoint_id.cmp(&b.endpoint_id));
    entries
}

#[cfg(test)]
mod tests;
