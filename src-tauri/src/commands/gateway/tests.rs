//! The Direct restore (Routed → Direct) must put EVERY bound provider back
//! into a multi-provider agent's config file — restoring only the active one
//! silently dropped the others from the file (the bindings stayed in the DB,
//! so the agent silently lost its other providers).

use super::*;
use crate::testutil::temp_home;
use std::sync::{Arc, Mutex};

/// One custom Anthropic-compatible endpoint with a key and a model.
fn add_endpoint(conn: &rusqlite::Connection, id: &str, base_url: &str) {
    db::create_endpoint(conn, id, "custom", id).unwrap();
    db::upsert_endpoint_protocol(conn, id, "anthropic", base_url).unwrap();
    db::set_endpoint_models(
        conn,
        id,
        r#"{"default":"m1","available":["m1"],"haiku":"m1","sonnet":"m1","opus":"m1"}"#,
    )
    .unwrap();
    db::mark_endpoint_key(conn, id, true, "ok").unwrap();
    crate::secrets::set(id, "sk-test").unwrap();
}

#[test]
fn direct_restore_writes_every_bound_provider() {
    let _lock = crate::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, _home_g) = temp_home();
    std::env::set_var("NESTRA_HOME_DIR", &home);
    let (keychain, _kc_g) = temp_home();
    std::env::set_var("NESTRA_KEYCHAIN_DIR", &keychain);

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::db::migrate(&conn).unwrap();
    add_endpoint(&conn, "ep-a", "https://a.example/v1");
    add_endpoint(&conn, "ep-b", "https://b.example/v1");
    db::upsert_agent(&conn, "pi-cli", "pi-cli", "Pi CLI", None, None, "ok", None).unwrap();
    // Both providers bound, `ep-a` active.
    db::replace_bindings(
        &conn,
        "pi-cli",
        &[("ep-a".to_string(), None), ("ep-b".to_string(), None)],
        "ep-a",
    )
    .unwrap();

    let parts = AppParts {
        db: Arc::new(Mutex::new(conn)),
    };
    apply_direct_config_blocking(&parts, "pi-cli").unwrap();

    let cfg = home.join(".pi").join("agent").join("models.json");
    let text = std::fs::read_to_string(&cfg).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(
        v["providers"]["nestra-ep-a"].is_object(),
        "active provider missing: {text}"
    );
    assert!(
        v["providers"]["nestra-ep-b"].is_object(),
        "second bound provider missing: {text}"
    );
}
