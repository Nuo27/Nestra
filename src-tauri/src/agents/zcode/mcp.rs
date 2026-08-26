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
    fn read_raw(&self, raw: &str) -> AppResult<Vec<(String, Value)>> {
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
        // Preserve a user-set `timeoutMs`: `to_native` can't see the existing
        // file, so its hardcoded 30_000 would overwrite the user's own tuning
        // on every sync. Re-seed the live value (when numeric) before merge.
        let mut seeded = enabled.clone();
        if let Ok(doc) = serde_json::from_str::<Value>(raw) {
            if let Some(existing) = doc.pointer("/mcp/servers").and_then(Value::as_object) {
                for (name, v) in seeded.iter_mut() {
                    if let Some(user_ms) = existing
                        .get(name)
                        .and_then(|e| e.get("timeoutMs"))
                        .and_then(Value::as_i64)
                    {
                        if let Some(o) = v.as_object_mut() {
                            o.insert("timeoutMs".into(), json!(user_ms));
                        }
                    }
                }
            }
        }
        apply_at_path(raw, &["mcp", "servers"], &seeded, disabled)
    }
}
