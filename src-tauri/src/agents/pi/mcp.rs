//! Pi MCP config: `~/.pi/agent/mcp.json` → `mcpServers` map at root.

use crate::mcp::providers::{apply_at_path, read_map, Provider};
use crate::mcp::{McpKind, McpTransport};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::AppResult;

pub struct Pi;

/// Registry constructor — see [`super::SPEC`].
pub fn new() -> Box<dyn Provider> {
    Box::new(Pi)
}

impl Provider for Pi {
    fn agent_id(&self) -> &'static str {
        "pi-cli"
    }
    fn config_path(&self, home: &Path) -> PathBuf {
        home.join(".pi").join("agent").join("mcp.json")
    }
    fn read_raw(&self, raw: &str) -> Vec<(String, Value)> {
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
