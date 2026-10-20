//! Pi MCP config: `~/.pi/agent/mcp.json` → `mcpServers` map at root.
//!
//! Pi reads MCP servers natively — stdio and streamable HTTP, user level in
//! `~/.pi/agent/mcp.json` (project level in `.pi/mcp.json`, which Nestra
//! leaves alone: a project entry overrides the user-level one with the same
//! name in pi, so Nestra's user-level write is what a project sees unless
//! the project defines its own).
//!
//! Three pi-specific rules shape this provider:
//! - **Names**: only letters, digits, `_` and `-` are legal, and pi SKIPS an
//!   invalid entry (the server silently never connects). [`pi_key`] maps a
//!   Nestra name to a legal key, so a name like "My Server" lands in pi's
//!   file as "My-Server" instead of being dropped.
//! - **Fields**: an entry carries `type` (optional — `command` selects stdio,
//!   `url` selects streamable HTTP), the transport fields, `enabled`, and
//!   pi's own `exposure`/`toolExposure`/`description`/`timeout`/`headers`/
//!   `oauth`. Nestra owns the transport fields; the rest are the user's and
//!   survive a sync ([`merge_entry`]).
//! - **No SSE**: pi dropped the legacy SSE transport, so an SSE server is
//!   written as a streamable-HTTP entry (`type: "http"`).

use crate::mcp::providers::{read_map, tree_get_mut, Provider};
use crate::mcp::{McpKind, McpTransport};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

#[cfg(test)]
mod tests;

pub struct Pi;

/// Registry constructor — see [`super::SPEC`].
pub fn new() -> Box<dyn Provider> {
    Box::new(Pi)
}

/// Pi's server-name rules: letters, digits, `_` and `-` only, and names that
/// differ only in `-`/`_` are the SAME server to pi. Illegal characters
/// become `-`; the case is kept (pi compares names case-sensitively).
/// Nestra's canonical id folds `-`/`_` the same way, so two Nestra servers
/// can never produce two names pi would reject as a duplicate.
fn pi_key(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        // A name of only illegal characters still needs a key (pi rejects
        // empty names) — a stable hash of the name keeps it unique.
        return format!("server-{:x}", stable_hash(name));
    }
    out
}

/// FNV-1a — a small hash whose output never changes (unlike
/// `DefaultHasher`, which is not guaranteed across Rust releases), for the
/// fallback key of a name that sanitizes to nothing.
fn stable_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The key an entry lives under in pi's file. Pi treats names that differ
/// only in `-`/`_` as the SAME server, so an existing entry under such a
/// variant IS the server Nestra manages: write into that key instead of
/// adding a second one (pi would reject the duplicate).
fn resolve_key(servers: &Map<String, Value>, name: &str) -> String {
    let key = pi_key(name);
    if servers.contains_key(&key) {
        return key;
    }
    let eq = |k: &str| k.replace('_', "-") == key.replace('_', "-");
    servers
        .keys()
        .find(|k| eq(k))
        .cloned()
        .unwrap_or(key)
}

/// Merge Nestra's managed fields into the entry already in pi's file.
/// Nestra owns the transport fields (`type`, `command`/`args`/`env` or `url`,
/// `enabled`); the user's own fields (`exposure`, `toolExposure`,
/// `description`, `timeout`, `cwd`, and `headers`/`oauth` on an HTTP server)
/// survive a sync. The transport kind decides which fields are stale: pi
/// rejects an entry that carries both a `command` and a `url`.
fn merge_entry(existing: &Map<String, Value>, native: &Map<String, Value>) -> Map<String, Value> {
    let mut out = existing.clone();
    let stale: &[&str] = if native.contains_key("command") {
        &["url", "headers", "oauth"]
    } else {
        &["command", "args", "env", "cwd"]
    };
    for k in stale {
        out.remove(*k);
    }
    for (k, v) in native {
        out.insert(k.clone(), v.clone());
    }
    out
}

impl Provider for Pi {
    fn agent_id(&self) -> &'static str {
        "pi-cli"
    }
    fn config_path(&self, home: &Path) -> PathBuf {
        home.join(".pi").join("agent").join("mcp.json")
    }
    /// Pi's entries carry `enabled: false` (keep the server without
    /// connecting) — so the "written but disabled" state is expressible.
    fn supports_enabled(&self) -> bool {
        true
    }
    fn read_raw(&self, raw: &str) -> AppResult<Vec<(String, Value)>> {
        read_map(raw, |o| o.get("mcpServers").and_then(|s| s.as_object()))
    }
    fn to_native(&self, s: &McpTransport, enabled: bool) -> Value {
        match s.kind {
            McpKind::Stdio => json!({
                "type": "stdio",
                "command": s.command.clone(),
                "args": s.args.clone(),
                "env": s.env.clone(),
                "enabled": enabled,
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
        let mut v: Value = serde_json::from_str(raw)
            .map_err(|e| AppError::Validation(format!("MCP config is not valid JSON: {e}")))?;
        if !v.is_object() {
            return Err(AppError::Validation(
                "MCP config root is not a JSON object".into(),
            ));
        }
        let servers = tree_get_mut(v.as_object_mut().unwrap(), &["mcpServers"]);
        for name in disabled {
            // The live key is the one pi knows this server by; the raw name
            // is removed too so a hand-written (or legacy) entry with the
            // unsanitized name doesn't linger as a second server.
            let key = resolve_key(servers, name);
            servers.remove(&key);
            if name != &key {
                servers.remove(name);
            }
        }
        for (name, native) in enabled {
            let native = native.as_object().unwrap();
            // A leftover under the raw name (hand-written, or written by an
            // older Nestra) is the SAME server — drop it so the file holds
            // exactly one key per server.
            let key = resolve_key(servers, name);
            if name != &key {
                servers.remove(name);
            }
            let merged = match servers.get(&key) {
                Some(Value::Object(existing)) => merge_entry(existing, native),
                _ => native.clone(),
            };
            servers.insert(key, Value::Object(merged));
        }
        serde_json::to_string_pretty(&v)
            .map_err(|e| AppError::Internal(format!("serialize MCP config: {e}")))
    }
}
