//! Codex MCP config: `[mcp_servers.<name>]` tables in `~/.codex/config.toml`.
//!
//! stdio servers carry `command` / `args` / `env` (optionally `type =
//! "stdio"` when written by the Desktop app itself); streamable-HTTP servers
//! carry `url`. `toml_edit` keeps every other section of the file untouched.

use crate::error::{AppError, AppResult};
use crate::mcp::providers::Provider;
use crate::mcp::{McpKind, McpTransport};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use toml_edit::{value, Item, Table};

pub struct Codex;

/// Registry constructor — see [`super::SPEC`].
pub fn new() -> Box<dyn Provider> {
    Box::new(Codex)
}

impl Provider for Codex {
    fn agent_id(&self) -> &'static str {
        "codex-desktop"
    }
    fn config_path(&self, home: &Path) -> PathBuf {
        home.join(".codex").join("config.toml")
    }
    fn read_raw(&self, raw: &str) -> AppResult<Vec<(String, Value)>> {
        let doc: toml_edit::DocumentMut = raw.parse().map_err(|e| {
            AppError::Validation(format!("MCP config is not valid TOML: {e}"))
        })?;
        let Some(providers) = doc.get("mcp_servers").and_then(Item::as_table) else {
            return Ok(Vec::new());
        };
        Ok(providers
            .iter()
            .filter_map(|(name, item)| {
                let t = item.as_table()?;
                Some((name.to_string(), table_to_json(t)))
            })
            .collect())
    }
    fn to_native(&self, s: &McpTransport, _enabled: bool) -> Value {
        match s.kind {
            McpKind::Stdio => json!({
                "command": s.command.clone(),
                "args": s.args.clone(),
                "env": s.env.clone(),
            }),
            // Known round-trip limitation: Codex's config.toml format has NO
            // transport discriminator — a bare `url` is the only HTTP shape
            // it accepts, and `from_native` reads every url entry back as
            // Http. An SSE-managed server folds into HTTP on sync; writing
            // an invented `type` field here would risk Codex rejecting the
            // file, so the fold stays until upstream grows a discriminator.
            McpKind::Http | McpKind::Sse => json!({ "url": s.url.clone() }),
        }
    }
    fn apply(
        &self,
        raw: &str,
        enabled: &BTreeMap<String, Value>,
        disabled: &[String],
    ) -> AppResult<String> {
        let mut doc: toml_edit::DocumentMut = raw
            .parse()
            .map_err(|e| AppError::Validation(format!("MCP config is not valid TOML: {e}")))?;
        let servers = doc
            .as_table_mut()
            .entry("mcp_servers")
            .or_insert_with(|| {
                let mut t = Table::new();
                t.set_implicit(true);
                Item::Table(t)
            })
            .as_table_mut()
            .ok_or_else(|| AppError::Validation("`mcp_servers` is not a table".into()))?;
        for name in disabled {
            servers.remove(name);
        }
        for (name, native) in enabled {
            servers.insert(name, Item::Table(json_to_table(native)));
        }
        let mut out = doc.to_string();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        Ok(out)
    }
}

/// A `[mcp_servers.<name>]` table → the canonical JSON shape
/// (`crate::mcp::from_native` reads `command`/`args`/`env`/`url`).
fn table_to_json(t: &Table) -> Value {
    let get_str = |k: &str| t.get(k).and_then(|v| v.as_str());
    if let Some(url) = get_str("url") {
        return json!({ "url": url });
    }
    let args = t
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let env = t
        .get("env")
        .and_then(|v| v.as_table_like())
        .map(|e| {
            serde_json::Map::from_iter(
                e.iter()
                    .filter_map(|(k, v)| Some((k.to_string(), Value::String(v.as_str()?.to_string())))),
            )
        })
        .unwrap_or_default();
    json!({
        "command": get_str("command"),
        "args": args,
        "env": env,
    })
}

/// Canonical JSON entry → a `[mcp_servers.<name>]` table.
fn json_to_table(native: &Value) -> Table {
    let mut t = Table::new();
    if let Some(url) = native.get("url").and_then(|v| v.as_str()) {
        t["url"] = value(url);
        return t;
    }
    if let Some(cmd) = native.get("command").and_then(|v| v.as_str()) {
        t["command"] = value(cmd);
    }
    if let Some(args) = native.get("args").and_then(|v| v.as_array()) {
        let arr = toml_edit::Array::from_iter(
            args.iter().filter_map(|v| v.as_str().map(str::to_string)),
        );
        t["args"] = value(arr);
    }
    if let Some(env) = native.get("env").and_then(|v| v.as_object()) {
        let mut e = Table::new();
        for (k, v) in env {
            if let Some(s) = v.as_str() {
                e.insert(k, value(s));
            }
        }
        t["env"] = Item::Table(e);
    }
    t
}

#[cfg(test)]
mod tests;
