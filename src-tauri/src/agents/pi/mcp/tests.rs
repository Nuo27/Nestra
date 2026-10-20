//! Pi's native MCP provider: legal server names, the entry shape, the
//! per-server `enabled` flag, and the merge that keeps the user's own
//! fields across a sync.

use super::*;
use crate::mcp::McpTransport;

fn stdio() -> McpTransport {
    McpTransport {
        kind: McpKind::Stdio,
        command: Some("npx".into()),
        args: vec!["-y".into(), "@modelcontextprotocol/server-filesystem".into()],
        env: [("FOO".to_string(), "bar".to_string())].into_iter().collect(),
        url: None,
    }
}

fn http() -> McpTransport {
    McpTransport {
        kind: McpKind::Http,
        command: None,
        args: vec![],
        env: Default::default(),
        url: Some("https://example.com/mcp".into()),
    }
}

#[test]
fn pi_supports_the_enabled_flag() {
    assert!(Pi.supports_enabled());
}

#[test]
fn stdio_entry_carries_type_command_args_env_and_enabled() {
    let v = Pi.to_native(&stdio(), true);
    assert_eq!(v["type"], json!("stdio"));
    assert_eq!(v["command"], json!("npx"));
    assert_eq!(v["args"][1], json!("@modelcontextprotocol/server-filesystem"));
    assert_eq!(v["env"]["FOO"], json!("bar"));
    assert_eq!(v["enabled"], json!(true));
}

#[test]
fn disabled_entry_stays_written_with_the_flag_off() {
    let v = Pi.to_native(&stdio(), false);
    assert_eq!(v["enabled"], json!(false));
    assert_eq!(v["command"], json!("npx"));
}

#[test]
fn http_entry_carries_type_url_and_enabled() {
    let v = Pi.to_native(&http(), true);
    assert_eq!(v["type"], json!("http"));
    assert_eq!(v["url"], json!("https://example.com/mcp"));
    assert_eq!(v["enabled"], json!(true));
}

#[test]
fn server_names_are_mapped_to_pi_legal_keys() {
    // pi skips an entry whose name isn't letters/digits/`_`/`-`.
    assert_eq!(pi_key("filesystem"), "filesystem");
    assert_eq!(pi_key("My Server"), "My-Server");
    assert_eq!(pi_key("my_server"), "my_server");
    assert_eq!(pi_key("docs@v2!"), "docs-v2");
    // A name that sanitizes to nothing still gets a key.
    assert!(!pi_key("!!!").is_empty());
    // The mapping is stable — a second pass doesn't rewrite the key.
    assert_eq!(pi_key(&pi_key("My Server")), "My-Server");
}

#[test]
fn apply_writes_a_sanitized_server_under_a_legal_key() {
    let mut map = BTreeMap::new();
    map.insert("My Server".to_string(), Pi.to_native(&stdio(), true));
    let out = Pi.apply(r#"{"mcpServers":{}}"#, &map, &[]).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    let servers = v["mcpServers"].as_object().unwrap();
    assert_eq!(servers.len(), 1);
    assert!(servers.contains_key("My-Server"), "got: {servers:?}");
    assert!(!servers.contains_key("My Server"), "got: {servers:?}");
}

#[test]
fn apply_writes_under_mcp_servers_and_preserves_other_keys() {
    let mut map = BTreeMap::new();
    map.insert("filesystem".to_string(), Pi.to_native(&stdio(), true));
    let out = Pi
        .apply(r#"{"mcpServers":{},"other":1}"#, &map, &[])
        .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["other"], json!(1));
    assert_eq!(v["mcpServers"]["filesystem"]["command"], json!("npx"));
    assert_eq!(v["mcpServers"]["filesystem"]["enabled"], json!(true));
}

#[test]
fn apply_keeps_the_users_own_fields() {
    let raw = r#"{"mcpServers":{"fs":{"command":"old","exposure":"deferred","description":"mine","timeout":120}}}"#;
    let mut map = BTreeMap::new();
    map.insert("fs".to_string(), Pi.to_native(&stdio(), true));
    let out = Pi.apply(raw, &map, &[]).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    let e = &v["mcpServers"]["fs"];
    // Nestra owns the transport fields ...
    assert_eq!(e["command"], json!("npx"));
    assert_eq!(e["enabled"], json!(true));
    // ... the user's own fields survive.
    assert_eq!(e["exposure"], json!("deferred"));
    assert_eq!(e["description"], json!("mine"));
    assert_eq!(e["timeout"], json!(120));
}

#[test]
fn apply_drops_fields_of_the_other_transport_kind() {
    // pi rejects an entry that carries both a `command` and a `url`.
    let raw = r#"{"mcpServers":{"fs":{"url":"https://old/mcp","headers":{"A":"b"}}}}"#;
    let mut map = BTreeMap::new();
    map.insert("fs".to_string(), Pi.to_native(&stdio(), true));
    let out = Pi.apply(raw, &map, &[]).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    let e = &v["mcpServers"]["fs"];
    assert_eq!(e["command"], json!("npx"));
    assert!(e.get("url").is_none(), "stale url, got: {e}");
    assert!(e.get("headers").is_none(), "stale headers, got: {e}");
}

#[test]
fn apply_keeps_http_credentials_and_drops_stdio_fields() {
    let raw = r#"{"mcpServers":{"docs":{"command":"old","url":"https://x/mcp","headers":{"A":"b"},"oauth":{"clientId":"c"}}}}"#;
    let mut map = BTreeMap::new();
    map.insert("docs".to_string(), Pi.to_native(&http(), true));
    let out = Pi.apply(raw, &map, &[]).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    let e = &v["mcpServers"]["docs"];
    assert_eq!(e["url"], json!("https://example.com/mcp"));
    assert!(e.get("command").is_none(), "stale command, got: {e}");
    // The user's own HTTP credentials are preserved.
    assert_eq!(e["headers"]["A"], json!("b"));
    assert_eq!(e["oauth"]["clientId"], json!("c"));
}

/// pi treats names that differ only in `-`/`_` as the SAME server: an
/// existing entry under such a variant is the server Nestra manages, so it
/// is written into instead of adding a second (pi-rejected) key.
#[test]
fn apply_writes_into_an_equivalent_existing_key() {
    let raw = r#"{"mcpServers":{"my_server":{"command":"old"}}}"#;
    let mut map = BTreeMap::new();
    map.insert("my-server".to_string(), Pi.to_native(&stdio(), true));
    let out = Pi.apply(raw, &map, &[]).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    let servers = v["mcpServers"].as_object().unwrap();
    assert_eq!(servers.len(), 1, "one entry, got: {servers:?}");
    assert!(servers.contains_key("my_server"), "got: {servers:?}");
    assert_eq!(servers["my_server"]["command"], json!("npx"));
}

#[test]
fn apply_removes_the_sanitized_and_the_raw_name() {
    let raw = r#"{"mcpServers":{"My-Server":{"command":"npx"},"My Server":{"command":"npx"}}}"#;
    let out = Pi.apply(raw, &BTreeMap::new(), &["My Server".to_string()]).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert!(v["mcpServers"].as_object().unwrap().is_empty(), "got: {v}");
}

#[test]
fn read_raw_reads_the_mcp_servers_map() {
    let raw = r#"{"mcpServers":{"fs":{"command":"npx"}},"other":2}"#;
    let entries = Pi.read_raw(raw).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "fs");
    assert_eq!(entries[0].1["command"], json!("npx"));
}
