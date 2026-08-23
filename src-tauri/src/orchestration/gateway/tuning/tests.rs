use super::*;

fn schema_conn() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::schema::build_v1(&conn).unwrap();
    conn
}

/// Out-of-range fields (hand-edited JSON, older build) clamp into range on
/// load — every boundary from both sides.
#[test]
fn clamps_out_of_range_values() {
    let t = GatewayTuning {
        headers_timeout_secs: 0,
        first_event_timeout_secs: 999,
        stream_silence_timeout_secs: 601,
        buffered_body_timeout_secs: 0,
        request_deadline_secs: 1,
        breaker_failure_threshold: 0,
        breaker_recovery_wait_secs: 1,
        breaker_success_threshold: 0,
        breaker_error_rate_pct: 101,
        breaker_min_requests: 0,
    }
    .clamped();
    assert_eq!(t.headers_timeout_secs, 1);
    assert_eq!(t.first_event_timeout_secs, 300);
    assert_eq!(t.stream_silence_timeout_secs, 600);
    assert_eq!(t.buffered_body_timeout_secs, 1);
    assert_eq!(t.request_deadline_secs, 30);
    assert_eq!(t.breaker_failure_threshold, 1);
    assert_eq!(t.breaker_recovery_wait_secs, 5);
    assert_eq!(t.breaker_success_threshold, 1);
    assert_eq!(t.breaker_error_rate_pct, 100);
    assert_eq!(t.breaker_min_requests, 1);
    // Silence 0 (disable) and error-rate 0 (disable) are legal in-range
    // values, never clamped away.
    let off = GatewayTuning {
        stream_silence_timeout_secs: 0,
        breaker_error_rate_pct: 0,
        ..GatewayTuning::default()
    }
    .clamped();
    assert_eq!(off.stream_silence_timeout_secs, 0);
    assert_eq!(off.breaker_error_rate_pct, 0);
}

/// save → load round-trips; the stored blob is the clamped form.
#[test]
fn save_load_round_trip() {
    let conn = schema_conn();
    let t = GatewayTuning {
        headers_timeout_secs: 45,
        stream_silence_timeout_secs: 90,
        breaker_failure_threshold: 5,
        ..GatewayTuning::default()
    };
    t.save(&conn).unwrap();
    assert_eq!(GatewayTuning::load(&conn), t);
}

/// Missing key, malformed JSON, and partial JSON (a field added in a later
/// build) all resolve without error — defaults fill the gaps.
#[test]
fn load_degrades_to_defaults() {
    let conn = schema_conn();
    // Missing key.
    assert_eq!(GatewayTuning::load(&conn), GatewayTuning::default());
    // Malformed JSON.
    crate::db::set_setting(&conn, KEY, &serde_json::json!("not an object")).unwrap();
    assert_eq!(GatewayTuning::load(&conn), GatewayTuning::default());
    // Partial JSON: only one field present, the rest default.
    crate::db::set_setting(&conn, KEY, &serde_json::json!({"headers_timeout_secs": 60})).unwrap();
    let t = GatewayTuning::load(&conn);
    assert_eq!(t.headers_timeout_secs, 60);
    assert_eq!(t, GatewayTuning { headers_timeout_secs: 60, ..GatewayTuning::default() });
}
