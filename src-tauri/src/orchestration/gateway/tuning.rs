//! Gateway runtime tuning — the single source of truth for every
//! operational knob the gateway honors at request time.
//!
//! Two families live here:
//!
//! * **Timeouts** (three-part cover, plus a total deadline):
//!   `headers_timeout_secs` bounds the upstream dial until response headers,
//!   `first_event_timeout_secs` bounds the first complete SSE event (the
//!   in-band error probe window), `stream_silence_timeout_secs` bounds the
//!   gap between SSE frames mid-stream (0 disables), and
//!   `buffered_body_timeout_secs` bounds a non-streaming body read. The
//!   `request_deadline_secs` wall clock caps the WHOLE retry/migrate loop —
//!   the hard bound that makes an unbounded failure ladder impossible.
//! * **Circuit-breaker parameters** (see `orchestration/health.rs`):
//!   consecutive-failure threshold, recovery wait before half-open probes,
//!   successes required to close, and the error-rate backstop over the
//!   rolling window.
//!
//! Values persist in `setting_kv["gateway_tuning"]` and are shared live
//! (one `Arc<RwLock<GatewayTuning>>` in `AppState`, handed to
//! `GatewayState` and `ProviderHealth`) — a Settings edit applies to the
//! NEXT request with no gateway restart. The lock is a `std` RwLock held
//! only for brief non-`.await` sections (same discipline as
//! `ProviderHealth`'s inner Mutex; a tokio lock would make the sync
//! breaker paths either impossible or panic-prone).
//!
//! Every setter-side value is clamped on load and on save; the struct is
//! `#[serde(default)]` so stored JSON from an older build (missing fields
//! added later) still parses with the new defaults filled in.

use crate::error::AppResult;

/// `setting_kv` key for the persisted tuning blob.
pub const KEY: &str = "gateway_tuning";

/// One shared tuning slot. Brief, never-across-`.await` critical sections.
pub type SharedTuning = std::sync::Arc<std::sync::RwLock<GatewayTuning>>;

/// A fresh shared slot holding the defaults (tests + the rare constructor
/// that has nothing to load yet).
pub fn shared_default() -> SharedTuning {
    std::sync::Arc::new(std::sync::RwLock::new(GatewayTuning::default()))
}

/// Read the current tuning snapshot. A poisoned lock falls back to the
/// defaults — tuning must never be able to brick the gateway.
pub fn snapshot(shared: &SharedTuning) -> GatewayTuning {
    shared.read().map(|t| *t).unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct GatewayTuning {
    /// Upstream dial → response-headers deadline, per attempt.
    pub headers_timeout_secs: u64,
    /// Upstream → first COMPLETE SSE event deadline (the probe window), per
    /// attempt. A 2xx stream that never produces a first event is a dead
    /// upstream (the zero-byte hang shape).
    pub first_event_timeout_secs: u64,
    /// Maximum gap between SSE frames mid-stream. A healthy stream never
    /// goes quiet for minutes; a stalled one must terminate so the agent
    /// sees an honest end instead of an infinite hang. 0 disables.
    pub stream_silence_timeout_secs: u64,
    /// Total time to collect a non-streaming (buffered) body after headers
    /// arrived.
    pub buffered_body_timeout_secs: u64,
    /// Wall-clock cap on the WHOLE run-with-migration loop for one inbound
    /// request (initial attempt + retries + migrations). The last-resort
    /// bound: even a pathological policy × upstream combination cannot
    /// outlive this.
    pub request_deadline_secs: u64,
    /// Consecutive migratable failures that open the breaker.
    pub breaker_failure_threshold: u64,
    /// How long an OPEN breaker holds before allowing half-open probes.
    pub breaker_recovery_wait_secs: u64,
    /// Consecutive probe successes in half-open that close the breaker.
    pub breaker_success_threshold: u64,
    /// Error-rate backstop over the rolling window that opens the breaker
    /// even without consecutive failures (a flapping endpoint). 0 disables.
    pub breaker_error_rate_pct: u64,
    /// Minimum samples in the window before the error-rate backstop fires.
    pub breaker_min_requests: u64,
}

impl Default for GatewayTuning {
    fn default() -> Self {
        Self {
            headers_timeout_secs: 30,
            first_event_timeout_secs: 30,
            stream_silence_timeout_secs: 120,
            buffered_body_timeout_secs: 600,
            request_deadline_secs: 600,
            breaker_failure_threshold: 3,
            breaker_recovery_wait_secs: 60,
            breaker_success_threshold: 2,
            breaker_error_rate_pct: 60,
            breaker_min_requests: 10,
        }
    }
}

/// Clamp one value into `[lo, hi]` (inclusive).
fn clamp(v: u64, lo: u64, hi: u64) -> u64 {
    v.max(lo).min(hi)
}

impl GatewayTuning {
    /// Force every field into its legal range. Applied on load AND on save —
    /// the Settings UI validates, but the gateway must never trust stored
    /// JSON (hand-edited, or written by an older build) to be sane.
    pub fn clamped(mut self) -> Self {
        self.headers_timeout_secs = clamp(self.headers_timeout_secs, 1, 300);
        self.first_event_timeout_secs = clamp(self.first_event_timeout_secs, 1, 300);
        self.stream_silence_timeout_secs = clamp(self.stream_silence_timeout_secs, 0, 600);
        self.buffered_body_timeout_secs = clamp(self.buffered_body_timeout_secs, 1, 1800);
        self.request_deadline_secs = clamp(self.request_deadline_secs, 30, 3600);
        self.breaker_failure_threshold = clamp(self.breaker_failure_threshold, 1, 20);
        self.breaker_recovery_wait_secs = clamp(self.breaker_recovery_wait_secs, 5, 900);
        self.breaker_success_threshold = clamp(self.breaker_success_threshold, 1, 10);
        self.breaker_error_rate_pct = clamp(self.breaker_error_rate_pct, 0, 100);
        self.breaker_min_requests = clamp(self.breaker_min_requests, 1, 50);
        self
    }

    /// Load from `setting_kv`. Missing key, malformed JSON, or partial JSON
    /// (fields added later) all resolve to defaults — tuning must never be
    /// able to brick the gateway.
    pub fn load(conn: &rusqlite::Connection) -> Self {
        let Ok(Some(v)) = crate::db::get_setting(conn, KEY) else {
            return Self::default();
        };
        match serde_json::from_value::<GatewayTuning>(v) {
            Ok(t) => t.clamped(),
            Err(_) => Self::default(),
        }
    }

    /// Persist (clamped) to `setting_kv`.
    pub fn save(&self, conn: &rusqlite::Connection) -> AppResult<()> {
        let clamped = self.clamped();
        crate::db::set_setting(
            conn,
            KEY,
            &serde_json::to_value(clamped).map_err(|e| crate::error::AppError::Internal(e.to_string()))?,
        )
    }
}

#[cfg(test)]
mod tests;
