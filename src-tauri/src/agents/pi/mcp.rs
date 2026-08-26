//! Pi MCP config: `~/.pi/agent/mcp.json` → `mcpServers` map at root.
//!
//! Pi has no native MCP support — the `mcpServers` file is consumed by the
//! community [`pi-mcp-adapter`](https://pi.dev/packages/pi-mcp-adapter)
//! package. Every write path consults [`adapter_installed`] (via the
//! registry's `mcp_available` hook) so Nestra neither advertises pi as
//! MCP-capable nor writes the file when the adapter is absent.

use crate::mcp::providers::{apply_at_path, read_map, Provider};
use crate::mcp::{McpKind, McpTransport};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::AppResult;

#[cfg(test)]
mod tests;

pub struct Pi;

/// Registry constructor — see [`super::SPEC`].
pub fn new() -> Box<dyn Provider> {
    Box::new(Pi)
}

/// `true` when the community `pi-mcp-adapter` package is installed
/// (user scope). `pi install npm:pi-mcp-adapter` records the package in
/// `~/.pi/agent/settings.json` (`packages` array, string entries or
/// `{ "source": … }` objects) and unpacks it under `~/.pi/agent/npm/`.
/// Both are checked — settings is authoritative, the directory is the
/// fallback for hand-edited configs.
pub fn adapter_installed() -> bool {
    crate::db::home_dir()
        .map(|home| adapter_installed_at(&home))
        .unwrap_or(false)
}

/// [`adapter_installed`] against an explicit home — the test seam.
pub fn adapter_installed_at(home: &Path) -> bool {
    let agent_dir = home.join(".pi").join("agent");
    if let Ok(raw) = std::fs::read_to_string(agent_dir.join("settings.json")) {
        if let Ok(Value::Object(root)) = serde_json::from_str::<Value>(&raw) {
            if let Some(Value::Array(packages)) = root.get("packages") {
                let listed = packages.iter().any(|p| match p {
                    // String entry (`"pi-mcp-adapter"`) or object form
                    // (`{ "source": "npm:pi-mcp-adapter", … }`) — substring,
                    // so both bare and `npm:`-prefixed entries match.
                    Value::String(s) => s.contains("pi-mcp-adapter"),
                    Value::Object(o) => o
                        .get("source")
                        .and_then(|s| s.as_str())
                        .map_or(false, |s| s.contains("pi-mcp-adapter")),
                    _ => false,
                });
                if listed {
                    return true;
                }
            }
        }
    }
    agent_dir.join("npm").join("pi-mcp-adapter").exists()
}

impl Provider for Pi {
    fn agent_id(&self) -> &'static str {
        "pi-cli"
    }
    fn config_path(&self, home: &Path) -> PathBuf {
        home.join(".pi").join("agent").join("mcp.json")
    }
    fn read_raw(&self, raw: &str) -> AppResult<Vec<(String, Value)>> {
        read_map(raw, |o| o.get("mcpServers").and_then(|s| s.as_object()))
    }
    fn to_native(&self, s: &McpTransport, _enabled: bool) -> Value {
        match s.kind {
            McpKind::Stdio => json!({
                "command": s.command.clone(),
                "args": s.args.clone(),
                "env": s.env.clone(),
            }),
            McpKind::Http | McpKind::Sse => json!({ "url": s.url.clone() }),
        }
    }
    fn apply(
        &self,
        raw: &str,
        enabled: &BTreeMap<String, Value>,
        disabled: &[String],
    ) -> AppResult<String> {
        apply_at_path(raw, &["mcpServers"], enabled, disabled)
    }
}
