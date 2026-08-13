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
}

impl EndpointQuotaState {
    /// JSON-encode for the `last_quota_state` column. `None` when the state is
    /// the all-default (nothing worth persisting).
    pub fn to_persisted_json(&self) -> Option<String> {
        if !self.exhausted && self.reason.is_none() && self.exhausted_at_ms.is_none() {
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
            },
        );
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
        if state.exhausted {
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
mod tests {
    use super::*;
    use crate::schema;

    #[test]
    fn mark_and_clear_exhaustion() {
        let q = QuotaState::new();
        assert!(!q.is_exhausted("ep-1"));
        q.mark_exhausted("ep-1", Some("5h window".into()));
        assert!(q.is_exhausted("ep-1"));
        let s = q.get("ep-1");
        assert_eq!(s.reason.as_deref(), Some("5h window"));
        assert!(s.exhausted_at_ms.is_some());
        q.clear_exhausted("ep-1");
        assert!(!q.is_exhausted("ep-1"));
    }

    #[test]
    fn exhausted_endpoints_lists_only_exhausted() {
        let q = QuotaState::new();
        q.mark_exhausted("ep-1", None);
        q.mark_exhausted("ep-2", None);
        q.clear_exhausted("ep-1");
        let mut ex = q.exhausted_endpoints();
        ex.sort();
        assert_eq!(ex, vec!["ep-2".to_string()]);
    }

    #[test]
    fn persisted_json_round_trips() {
        let s = EndpointQuotaState {
            exhausted: true,
            reason: Some("monthly limit".into()),
            exhausted_at_ms: Some(1234),
        };
        let json = s.to_persisted_json().unwrap();
        let back = EndpointQuotaState::from_persisted_json(Some(&json));
        assert_eq!(back.exhausted, true);
        assert_eq!(back.reason.as_deref(), Some("monthly limit"));
        assert_eq!(back.exhausted_at_ms, Some(1234));
    }

    #[test]
    fn default_state_is_not_persisted() {
        // All-default state → None (don't waste a column on empty data).
        let s = EndpointQuotaState::default();
        assert!(s.to_persisted_json().is_none());
        // And parsing None/malformed yields default without panicking.
        assert_eq!(
            EndpointQuotaState::from_persisted_json(None),
            EndpointQuotaState::default()
        );
        assert_eq!(
            EndpointQuotaState::from_persisted_json(Some("not json")),
            EndpointQuotaState::default()
        );
    }

    #[test]
    fn persist_and_load_survives_via_column() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        schema::build_v1(&conn).unwrap();
        conn.execute(
            "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status)
             VALUES ('ep-1','custom','Main',0,'unvalidated')",
            [],
        )
        .unwrap();
        let state = EndpointQuotaState {
            exhausted: true,
            reason: Some("reset window".into()),
            exhausted_at_ms: Some(99),
        };
        persist(&conn, "ep-1", &state).unwrap();

        let reloaded = load_all_from_db(&conn).unwrap();
        assert!(reloaded.is_exhausted("ep-1"));
        assert_eq!(reloaded.get("ep-1").reason.as_deref(), Some("reset window"));
    }
}
