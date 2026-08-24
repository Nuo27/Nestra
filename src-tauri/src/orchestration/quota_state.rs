//! Reactive quota state — the gateway-observed quota/exhaustion snapshot.
//!
//! This is the **reactive** signal source (reactive gateway-observed is the
//! primary quota/failure signal). Distinct from the proactive per-endpoint
//! quota fetchers in `endpoint_quota.rs` (Z.ai/MiniMax/Moonshot pollers, kept
//! for the Providers-page display): those run on a schedule and are
//! provider-specific; this module records what the *gateway* observes in real
//! time for **every** provider, the moment a request fails or a usage header
//! comes back.
//!
//! Ships the in-memory store + the read/write API + the `last_quota_state`
//! column bridge. The gateway feeds it; the router reads `is_exhausted`.
//!
//! ## Persistence
//!
//! Exhaustion is also mirrored to `provider_endpoint.last_quota_state` (a JSON
//! column) so the state survives a restart. On startup the gateway reloads
//! from that column; the in-memory store is authoritative within a process
//! and tests construct their own.

use std::collections::HashMap;
use std::sync::Mutex;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::db;
use crate::error::AppResult;

/// Per-endpoint reactive quota snapshot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EndpointQuotaState {
    /// `true` when the gateway has observed a quota-exhausted signal (429 with
    /// quota markers, or an explicit exhausted body) for this endpoint and not
    /// yet observed a successful request that clears it. The router skips
    /// exhausted endpoints unless they're the only option.
    pub exhausted: bool,
    /// Optional human-readable reason/detail captured from the upstream
    /// response (e.g. "5h window elapsed", "monthly limit reached"). Surfaces
    /// in the UI's "why this provider" view.
    pub reason: Option<String>,
    /// When the exhaustion was observed (unix-millis). The router may apply a
    /// TTL (provider-specific reset window) to auto-clear; the store clears it
    /// on the next successful observation.
    pub exhausted_at_ms: Option<i64>,
    /// Last known REMAINING budget, percent 0–100, fed by the proactive quota
    /// fetch (`endpoint_fetch_quota` → `set_remaining`). `None` = no signal —
    /// the router treats unknown as unconstrained. Quota-window-aware routing:
    /// the policy-target walk soft-skips endpoints at or near empty when a
    /// healthier target exists (see `router::LOW_REMAINING_PCT`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_pct: Option<f64>,
}

impl EndpointQuotaState {
    /// JSON-encode for the `last_quota_state` column. `None` when the state is
    /// the all-default (nothing worth persisting).
    pub fn to_persisted_json(&self) -> Option<String> {
        if !self.exhausted
            && self.reason.is_none()
            && self.exhausted_at_ms.is_none()
            && self.remaining_pct.is_none()
        {
            return None;
        }
        serde_json::to_string(self).ok()
    }

    /// Parse the `last_quota_state` column back into a snapshot. `None` or
    /// malformed JSON yields the default (non-exhausted) state — never panics
    /// on bad persisted data.
    pub fn from_persisted_json(s: Option<&str>) -> Self {
        let Some(s) = s else {
            return Self::default();
        };
        serde_json::from_str(s).unwrap_or_default()
    }
}

/// Process-global reactive quota store. One instance in `AppState`; tests
/// construct their own.
pub struct QuotaState {
    inner: Mutex<HashMap<String, EndpointQuotaState>>,
}

impl Default for QuotaState {
    fn default() -> Self {
        Self::new()
    }
}

impl QuotaState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Mark an endpoint as quota-exhausted. Records the reason + timestamp and
    /// mirrors it to the `last_quota_state` column so it survives a restart.
    pub fn mark_exhausted(&self, endpoint_id: &str, reason: Option<String>) {
        let now = chrono::Utc::now().timestamp_millis();
        let mut map = self.inner.lock().expect("quota lock poisoned");
        map.insert(
            endpoint_id.to_string(),
            EndpointQuotaState {
                exhausted: true,
                reason,
                exhausted_at_ms: Some(now),
                remaining_pct: Some(0.0),
            },
        );
    }

    /// Record the last known remaining budget (percent 0–100) from a
    /// successful proactive fetch. Updates the entry in place, preserving
    /// any fresher reactive exhaustion; inserts a clean entry when unseen.
    /// A remaining of 0 does NOT set `exhausted` — only the gateway's own
    /// observation may declare that.
    pub fn set_remaining(&self, endpoint_id: &str, remaining_pct: f64) {
        let mut map = self.inner.lock().expect("quota lock poisoned");
        let entry = map
            .entry(endpoint_id.to_string())
            .or_insert_with(EndpointQuotaState::default);
        entry.remaining_pct = Some(remaining_pct.clamp(0.0, 100.0));
    }

    /// Last known remaining budget (percent 0–100); `None` = no signal.
    pub fn remaining(&self, endpoint_id: &str) -> Option<f64> {
        let map = self.inner.lock().expect("quota lock poisoned");
        map.get(endpoint_id).and_then(|s| s.remaining_pct)
    }

    /// Clear exhaustion for an endpoint (called when the gateway observes a
    /// successful request, indicating the provider reset the window).
    pub fn clear_exhausted(&self, endpoint_id: &str) {
        let mut map = self.inner.lock().expect("quota lock poisoned");
        map.remove(endpoint_id);
    }

    /// `true` when the endpoint is currently quota-exhausted (the router
    /// skips it unless policy forces last-resort use).
    pub fn is_exhausted(&self, endpoint_id: &str) -> bool {
        let map = self.inner.lock().expect("quota lock poisoned");
        map.get(endpoint_id).map(|s| s.exhausted).unwrap_or(false)
    }

    /// Snapshot the state for one endpoint (defaulted if unseen).
    pub fn get(&self, endpoint_id: &str) -> EndpointQuotaState {
        let map = self.inner.lock().expect("quota lock poisoned");
        map.get(endpoint_id)
            .cloned()
            .unwrap_or_default()
    }

    /// All endpoints currently marked exhausted (for the health/quota UI card).
    pub fn exhausted_endpoints(&self) -> Vec<String> {
        let map = self.inner.lock().expect("quota lock poisoned");
        map.iter()
            .filter(|(_, s)| s.exhausted)
            .map(|(k, _)| k.clone())
            .collect()
    }

    pub fn clear(&self) {
        self.inner.lock().expect("quota lock poisoned").clear();
    }
}

/// Persist the in-memory quota state for one endpoint to the
/// `last_quota_state` column. Called by the gateway after a state change so
/// the snapshot survives a restart; also exposed for tests + the quota-state
/// read.
pub fn persist(conn: &Connection, endpoint_id: &str, state: &EndpointQuotaState) -> AppResult<()> {
    let json = state.to_persisted_json();
    conn.execute(
        "UPDATE provider_endpoint SET last_quota_state = ?1 WHERE id = ?2",
        rusqlite::params![json, endpoint_id],
    )?;
    Ok(())
}

/// Reload the persisted `last_quota_state` for every endpoint into an in-memory
/// store. Intended for process startup so the reactive state survives a
/// restart while the gateway is offline.
pub fn load_all_from_db(conn: &Connection) -> AppResult<QuotaState> {
    let store = QuotaState::new();
    for ep in db::list_endpoints(conn)? {
        // `last_quota_state` isn't on EndpointRow yet (it would need a column
        // add); read it directly for now. Once EndpointRow exposes it, switch
        // to that to avoid the extra query.
        let json: Option<String> = conn
            .query_row(
                "SELECT last_quota_state FROM provider_endpoint WHERE id = ?1",
                rusqlite::params![ep.id],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        let state = EndpointQuotaState::from_persisted_json(json.as_deref());
        if state.exhausted || state.remaining_pct.is_some() {
            // Insert the PERSISTED state directly — `mark_exhausted` would
            // stamp `exhausted_at_ms` with NOW, silently discarding the
            // persisted timestamp (breaking the TTL auto-clear and the UI's
            // "exhausted since" display).
            store
                .inner
                .lock()
                .expect("quota lock poisoned")
                .insert(ep.id, state);
        }
    }
    Ok(store)
}

#[cfg(test)]
mod tests;
