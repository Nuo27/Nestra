//! Claude Code MCP config: `~/.claude.json` → `mcpServers` map at root.

use crate::mcp::providers::{apply_at_path, read_map, stdio_or_http, Provider};
use crate::mcp::McpTransport;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::AppResult;

pub struct ClaudeCode;

/// Registry constructor — see [`super::SPEC`].
pub fn new() -> Box<dyn Provider> {
    Box::new(ClaudeCode)
}

impl Provider for ClaudeCode {
    fn agent_id(&self) -> &'static str {
        "claude-code-cli"
    }
    fn config_path(&self, home: &Path) -> PathBuf {
        home.join(".claude.json")
    }
    fn read_raw(&self, raw: &str) -> Vec<(String, Value)> {
        read_map(raw, |o| o.get("mcpServers").and_then(|s| s.as_object()))
    }
    fn to_native(&self, s: &McpTransport, _enabled: bool) -> Value {
        stdio_or_http(s)
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
