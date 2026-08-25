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
fn persisted_json_round_trips() {
    let s = EndpointQuotaState {
        exhausted: true,
        reason: Some("monthly limit".into()),
        exhausted_at_ms: Some(1234),
        remaining_pct: None,
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
        remaining_pct: None,
    };
    persist(&conn, "ep-1", &state).unwrap();

    let reloaded = load_all_from_db(&conn).unwrap();
    assert!(reloaded.is_exhausted("ep-1"));
    assert_eq!(reloaded.get("ep-1").reason.as_deref(), Some("reset window"));
}
#[test]
fn set_remaining_updates_in_place_and_survives_persist_round_trip() {
    let q = QuotaState::new();
    q.set_remaining("ep-1", 3.0);
    assert_eq!(q.remaining("ep-1"), Some(3.0));
    assert!(!q.is_exhausted("ep-1"), "remaining alone never declares exhaustion");

    // A later reactive exhaustion keeps the freshest picture: remaining → 0.
    q.mark_exhausted("ep-1", Some("429".into()));
    assert_eq!(q.remaining("ep-1"), Some(0.0));
    assert!(q.is_exhausted("ep-1"));

    // Clean recovery: set_remaining on a fresh store, persist, reload.
    let q2 = QuotaState::new();
    q2.set_remaining("ep-2", 42.5);
    let conn = Connection::open_in_memory().unwrap();
    crate::schema::build_v1(&conn).unwrap();
    conn.execute(
        "INSERT INTO provider_endpoint (id, kind, display_name, has_api_key, status)
         VALUES ('ep-2','custom','E',0,'unvalidated')",
        [],
    )
    .unwrap();
    persist(&conn, "ep-2", &q2.get("ep-2")).unwrap();
    let reloaded = load_all_from_db(&conn).unwrap();
    assert_eq!(reloaded.remaining("ep-2"), Some(42.5));
}
