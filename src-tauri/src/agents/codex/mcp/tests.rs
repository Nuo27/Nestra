use super::*;
use crate::mcp::{McpKind, McpTransport};
use serde_json::json;
use std::collections::BTreeMap;

const RAW: &str = r#"[mcp_servers.codegraph]
args = ["serve", "--mcp"]
command = "codegraph"
env = { KEY = "v" }

[mcp_servers.unity]
url = "http://127.0.0.1:8080/mcp"

[desktop]
someKey = true
"#;

#[test]
fn read_raw_parses_stdio_and_http_entries() {
    let entries = Codex.read_raw(RAW).unwrap();
    let get = |name: &str| {
        entries
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap()
    };
    let codegraph = get("codegraph");
    assert_eq!(codegraph["command"], json!("codegraph"));
    assert_eq!(codegraph["args"], json!(["serve", "--mcp"]));
    assert_eq!(codegraph["env"], json!({ "KEY": "v" }));
    let unity = get("unity");
    assert_eq!(unity["url"], json!("http://127.0.0.1:8080/mcp"));
}

#[test]
fn apply_writes_entries_and_preserves_unrelated_sections() {
    let stdio = McpTransport {
        kind: McpKind::Stdio,
        command: Some("npx".into()),
        args: vec!["-y".into(), "srv".into()],
        url: None,
        env: [("A".to_string(), "1".to_string())].into_iter().collect(),
    };
    let http = McpTransport {
        kind: McpKind::Http,
        command: None,
        args: vec![],
        url: Some("http://localhost:9/x".into()),
        env: Default::default(),
    };
    let mut enabled = BTreeMap::new();
    enabled.insert("one".to_string(), Codex.to_native(&stdio, true));
    enabled.insert("two".to_string(), Codex.to_native(&http, true));

    let out = Codex.apply(RAW, &enabled, &["codegraph".to_string()]).unwrap();

    let doc: toml_edit::DocumentMut = out.parse().unwrap();
    assert!(doc["mcp_servers"].get("codegraph").is_none(), "disabled entry dropped");
    assert_eq!(doc["mcp_servers"]["one"]["command"].as_str(), Some("npx"));
    assert_eq!(
        doc["mcp_servers"]["one"]["args"].as_array().map(|a| a.len()),
        Some(2)
    );
    assert_eq!(doc["mcp_servers"]["one"]["env"]["A"].as_str(), Some("1"));
    assert_eq!(
        doc["mcp_servers"]["two"]["url"].as_str(),
        Some("http://localhost:9/x")
    );
    // Untouched section survives.
    assert_eq!(doc["desktop"]["someKey"].as_bool(), Some(true));
    // And the written config round-trips through read_raw.
    assert_eq!(Codex.read_raw(&out).unwrap().len(), 3);
}
