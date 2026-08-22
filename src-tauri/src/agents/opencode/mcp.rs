//! OpenCode MCP config: `~/.config/opencode/opencode.json` → flat `mcp`
//! object. Keyed under `opencode-desktop`: the CLI variant is not supported
//! because it shared this file with the Desktop agent and the two fought over
//! it. opencode-desktop is the sole surviving OpenCode agent.

use crate::mcp::providers::Provider;
use crate::mcp::{McpKind, McpTransport};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

pub struct OpenCode;

/// Registry constructor — see [`super::SPEC`].
pub fn new() -> Box<dyn Provider> {
    Box::new(OpenCode)
}

impl Provider for OpenCode {
    fn agent_id(&self) -> &'static str {
        "opencode-desktop"
    }
    fn supports_enabled(&self) -> bool {
        true
    }
    fn config_path(&self, home: &Path) -> PathBuf {
        home.join(".config").join("opencode").join("opencode.json")
    }
    fn read_raw(&self, raw: &str) -> Vec<(String, Value)> {
        let Ok(v) = serde_json::from_str::<Value>(raw) else {
            return Vec::new();
        };
        let Some(mcp) = v.get("mcp").and_then(|m| m.as_object()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        // Flat layout: every `mcp.<name>` object is a server. Also surface
        // entries still nested under old `local`/`remote` groups (written
        // by older Nestra) so import-scanning can see and repair them.
        // A key named "local"/"remote" is treated as a legacy GROUP only
        // when it doesn't itself look like a server (has type/command/url) —
        // a real server with that name must not be misparsed.
        let looks_like_server = |v: &Value| {
            v.is_object()
                && (v.get("type").is_some() || v.get("command").is_some() || v.get("url").is_some())
        };
        for sub in ["local", "remote"] {
            if let Some(obj) = mcp.get(sub).and_then(|m| m.as_object()) {
                if looks_like_server(&serde_json::Value::Object(obj.clone())) {
                    continue; // a real server named "local"/"remote", not a group
                }
                for (k, sv) in obj {
                    out.push((k.clone(), sv.clone()));
                }
            }
        }
        for (k, sv) in mcp {
            if sv.is_object() && !matches!(k.as_str(), "local" | "remote") {
                out.push((k.clone(), sv.clone()));
            }
        }
        out
    }
    fn to_native(&self, s: &McpTransport, enabled: bool) -> Value {
        match s.kind {
            McpKind::Stdio => {
                // OpenCode wants a single array (program + args merged) — not
                // Nestra's split `command`/`args`. Also rename `env` →
                // `environment` (OpenCode's key).
                let mut cmd = Vec::with_capacity(1 + s.args.len());
                if let Some(c) = s.command.as_ref() {
                    cmd.push(c.clone());
                }
                cmd.extend(s.args.iter().cloned());
                json!({
                    "type": "local",
                    "command": cmd,
                    "environment": s.env.clone(),
                    // OpenCode's `mcp.<name>` schema accepts an explicit
                    // `enabled`; missing `type`/`command` is what fails the
                    // validator. The per-agent state drives the field: a
                    // server the user disabled for opencode stays written but
                    // off (`enabled: false`) instead of being force-enabled.
                    "enabled": enabled,
                })
            }
            McpKind::Http | McpKind::Sse => json!({
                "type": "remote",
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
        opencode_apply(raw, enabled, disabled)
    }
}

/// opencode addresses servers *directly* by name under `mcp.<name>` — the
/// server's own `type` field (`local`/`remote`) discriminates the shape.
/// Older Nestra versions wrote a nested `mcp.local.<name>` / `mcp.remote.<name>`
/// layout that OpenCode rejects (`mcp.local.enabled Missing key`); `apply`
/// repairs any such nesting on every pass so a broken file heals on the next
/// sync.
fn opencode_apply(
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
    let mcp = v
        .as_object_mut()
        .unwrap()
        .entry("mcp")
        .or_insert_with(|| Value::Object(Map::new()));
    if !mcp.is_object() {
        *mcp = Value::Object(Map::new());
    }
    repair_opencode_nested_slots(mcp.as_object_mut().unwrap());
    for name in disabled {
        remove_entry(mcp.as_object_mut().unwrap(), name);
    }
    for (name, native) in enabled {
        mcp.as_object_mut().unwrap().insert(name.clone(), native.clone());
    }
    serde_json::to_string_pretty(&v)
        .map_err(|e| AppError::Internal(format!("serialize MCP config: {e}")))
}

/// Repair pass: earlier Nestra versions (and configs synced by them) wrote
/// servers under `mcp.local.<name>` / `mcp.remote.<name>`. OpenCode's schema
/// keeps servers directly under `mcp.<name>`, so lift every entry out of the
/// `local`/`remote` groups, ensure each carries `enabled: true`, and drop the
/// now-empty slot keys. Real OpenCode never uses those keys, so there is
/// nothing old to preserve — scheduling `enabled: true` on every
/// lifted entry matches the current shape at startup.
///
/// Lifted entries are also re-shaped to opencode's canonical value (array
/// `command`, `environment` instead of `env`, no `args`) — those old split
/// keys fail the schema's `additionalProperties: false` even once flat.
///
/// Idempotent: once everything sits flat under `mcp`, this pass is a no-op.
fn repair_opencode_nested_slots(mcp: &mut Map<String, Value>) {
    for slot in ["local", "remote"] {
        let entries: Vec<(String, Value)> = match mcp.remove(slot) {
            Some(Value::Object(map)) => map.into_iter().collect(),
            _ => continue,
        };
        for (name, native) in entries {
            let mut entry = native;
            if !entry.is_object() {
                continue;
            }
            normalize_opencode_entry(&mut entry);
            if !entry.get("command").is_some() && entry.get("url").is_none() {
                // No recognizable program/URL: not an MCP server. Leave it
                // dropped rather than emitting an invalid `{enabled}` stub.
                continue;
            }
            if entry.get("enabled").is_none() {
                entry
                    .as_object_mut()
                    .unwrap()
                    .insert("enabled".into(), Value::Bool(true));
            }
            mcp.insert(name, entry);
        }
    }
}

/// Bring a lifted opencode entry into the schema's canonical value shape:
/// `command` as a JSON array (merging a split string `command` + `args`
/// tail), `environment` instead of `env`, no stray `args` key.
fn normalize_opencode_entry(entry: &mut Value) {
    let Some(o) = entry.as_object_mut() else {
        return;
    };
    let mut cmd: Vec<Value> = Vec::new();
    match o.remove("command") {
        Some(Value::Array(items)) => cmd.extend(items),
        Some(v @ Value::String(_)) => cmd.push(v),
        _ => {}
    }
    if let Some(Value::Array(args)) = o.remove("args") {
        cmd.extend(args);
    }
    if !cmd.is_empty() {
        o.insert("command".into(), Value::Array(cmd));
    }
    if let Some(Value::Object(_)) = o.get("env") {
        if let Some(env) = o.remove("env") {
            o.insert("environment".into(), env);
        }
    }
}

fn remove_entry(obj: &mut Map<String, Value>, name: &str) {
    // Primary location: flat `mcp.<name>`.
    obj.remove(name);
    // Repair remnants: old nested `local`/`remote` groups may still carry it.
    if let Some(m) = obj.get_mut("local").and_then(|x| x.as_object_mut()) {
        m.remove(name);
    }
    if let Some(m) = obj.get_mut("remote").and_then(|x| x.as_object_mut()) {
        m.remove(name);
    }
}
