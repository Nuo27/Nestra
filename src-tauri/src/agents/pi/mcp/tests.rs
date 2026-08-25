//! `adapter_installed_at` — the pi-mcp-adapter detection seam. Pi has no
//! native MCP; the gate decides whether pi appears MCP-capable at all.

use super::adapter_installed_at;
use crate::testutil::temp_home;
use std::fs;
use std::path::PathBuf;

fn write_settings(home: &PathBuf, json: &str) {
    let agent_dir = home.join(".pi").join("agent");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(agent_dir.join("settings.json"), json).unwrap();
}

#[test]
fn adapter_listed_as_string_entry() {
    let (home, _t) = temp_home();
    write_settings(
        &home,
        r#"{"packages": ["pi-skills", "pi-mcp-adapter"]}"#,
    );
    assert!(adapter_installed_at(&home));
}

#[test]
fn adapter_listed_as_object_source() {
    let (home, _t) = temp_home();
    write_settings(
        &home,
        r#"{"packages": [{"source": "npm:pi-mcp-adapter", "extensions": []}]}"#,
    );
    assert!(adapter_installed_at(&home));
}

#[test]
fn other_packages_only_is_not_installed() {
    let (home, _t) = temp_home();
    write_settings(&home, r#"{"packages": ["pi-skills"]}"#);
    assert!(!adapter_installed_at(&home));
}

#[test]
fn npm_dir_fallback_when_settings_absent_or_unparseable() {
    // No settings.json at all — the unpacked package dir is the signal.
    let (home, _t) = temp_home();
    let pkg = home.join(".pi").join("agent").join("npm").join("pi-mcp-adapter");
    fs::create_dir_all(&pkg).unwrap();
    assert!(adapter_installed_at(&home));

    // Corrupt settings.json must not panic — falls through to the dir.
    write_settings(&home, "{not json");
    assert!(adapter_installed_at(&home));
}

#[test]
fn nothing_installed() {
    let (home, _t) = temp_home();
    write_settings(&home, r#"{"theme": "dark"}"#);
    assert!(!adapter_installed_at(&home));
}
