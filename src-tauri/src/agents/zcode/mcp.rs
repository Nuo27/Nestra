//! ZCode MCP config: `~/.zcode/cli/config.json` → `mcp.servers` map.
//!
//! The schema is strict (an unknown key silently drops the server), so
//! `to_native` writes only fields ZCode itself writes: `type`, the transport
//! fields, `enabled`, and `timeoutMs` on stdio servers.

use crate::mcp::providers::{apply_at_path, read_map, Provider};
use crate::mcp::{McpKind, McpTransport};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::AppResult;

pub struct ZCode;

/// Registry constructor — see [`super::SPEC`].
pub fn new() -> Box<dyn Provider> {
    Box::new(ZCode)
}

impl Provider for ZCode {
    fn agent_id(&self) -> &'static str {
        "zcode-desktop"
    }
    fn supports_enabled(&self) -> bool {
        true
    }
    fn config_path(&self, home: &Path) -> PathBuf {
        home.join(".zcode").join("cli").join("config.json")
    }
    fn read_raw(&self, raw: &str) -> Vec<(String, Value)> {
        read_map(raw, |o| {
            o.get("mcp")
                .and_then(|m| m.get("servers"))
                .and_then(|s| s.as_object())
        })
    }
    fn to_native(&self, s: &McpTransport, enabled: bool) -> Value {
        match s.kind {
            McpKind::Stdio => json!({
                "type": "stdio",
                "command": s.command.clone(),
                "args": s.args.clone(),
                "env": s.env.clone(),
                "enabled": enabled,
                "timeoutMs": 30_000,
            }),
            McpKind::Http | McpKind::Sse => json!({
                "type": "http",
                "url": s.url.clone(),
                "enabled": enabled,
            }),
        }
    }
    fn apply(
        &self,
        raw: &str,
        enabled: &BTreeMap<String, Value>,
        disabled: &[String],
    ) -> AppResult<String> {
        apply_at_path(raw, &["mcp", "servers"], enabled, disabled)
    }
}
