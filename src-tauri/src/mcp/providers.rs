//! Per-CLI adapters. A `Provider` knows the one file shape of its CLI's MCP
//! config: how to find it, read native entries into `(name, Value)` pairs, and
//! write a merged result back preserving everything Nestra doesn't manage.
//!
//! Native entries are *opaque* `serde_json::Value`s decoded to canonical
//! `McpTransport` via `crate::mcp::from_native` (concept keys, not fixed
//! field-per-file names). A new CLI mostly means a new `Provider` entry.

use crate::config_writer::atomic_write;
use crate::db::home_dir;
use crate::error::{AppError, AppResult};
use crate::mcp::{McpKind, McpServer, McpTransport};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub trait Provider: Send + Sync {
    /// Registry agent id ("claude-code-cli", "pi-cli", ...) — matches the agents ids.
    fn agent_id(&self) -> &'static str;
    /// Absolute path to the agent's MCP config file under a home dir.
    fn config_path(&self, home: &Path) -> PathBuf;
    /// Read native server entries out of the raw file contents.
    fn read_raw(&self, raw: &str) -> Vec<(String, Value)>;
    /// Whether this format can express a per-server `enabled` flag (opencode).
    /// Formats without one can only write-or-drop an entry, so a "disabled"
    /// state on them degrades to absent (see `set_state`).
    fn supports_enabled(&self) -> bool {
        false
    }
    /// Canonical server -> the agent's native value for writing. `enabled` is
    /// the per-agent state flag: formats with an `enabled` field (opencode)
    /// emit it; formats without one ignore it.
    fn to_native(&self, s: &McpTransport, enabled: bool) -> Value;
    /// Build the whole native map for a set of servers (default: one
    /// `to_native` entry per server, keyed by name). `enabled` servers are
    /// written with the flag on, `disabled` ones with it off — both are
    /// *present* in the config; servers in neither set are dropped by `apply`.
    /// For this provider's agent it layers any per-agent env overrides on top
    /// of the server's base env, and on Windows wraps commands that ship as
    /// `.cmd`/`.bat` shims (`npx`, `codegraph`, …) as `cmd /c …` so the agent
    /// can spawn them.
    fn to_native_map(&self, enabled: &[McpServer], disabled: &[McpServer]) -> BTreeMap<String, Value> {
        let home = crate::db::home_dir().ok();
        let cfg_path = self.config_path(home.as_deref().unwrap_or_else(|| Path::new(".")));
        let agent_id = self.agent_id();
        let mut out = BTreeMap::new();
        let paired = enabled
            .iter()
            .map(|s| (s, true))
            .chain(disabled.iter().map(|s| (s, false)));
        for (s, on) in paired {
            let (cmd, args) =
                wrap_command_for_windows(&s.transport.command, &s.transport.args, &cfg_path);
            let mut t = s.transport.clone();
            t.command = cmd;
            t.args = args;
            // base env ∪ this agent's overrides (override wins on key clash).
            if let Some(ov) = s.env_overrides.get(agent_id) {
                for (k, v) in ov {
                    t.env.insert(k.clone(), v.clone());
                }
            }
            out.insert(s.name.clone(), self.to_native(&t, on));
        }
        out
    }
    /// Insert/refresh `enabled` (name -> native) and drop `disabled` names
    /// from the file, preserving all unrelated content. Returns new file text.
    /// Errors (rather than falling back to an empty object) when the file
    /// isn't valid JSON — an unparseable config must never be silently
    /// replaced by a write that wipes the user's whole file.
    fn apply(
        &self,
        raw: &str,
        enabled: &BTreeMap<String, Value>,
        disabled: &[String],
    ) -> AppResult<String>;
}

fn stdio_or_http(s: &McpTransport) -> Value {
    match s.kind {
        McpKind::Stdio => json!({
            "command": s.command.clone(),
            "args": s.args.clone(),
            "env": s.env.clone(),
        }),
        McpKind::Http | McpKind::Sse => json!({ "url": s.url.clone() }),
    }
}

/// Known Node-ecosystem shim launchers that always need `cmd /c` wrapping on
/// Windows — they ship as `.cmd` shims that `CreateProcess` (and Node's
/// `spawn` without a shell) can't launch directly. This is a fast-path inside
/// [`needs_cmd_wrap`]; the general rule also wraps any other command that is
/// (or resolves to) a `.cmd`/`.bat` shim, so correctness doesn't hinge on a
/// closed list — `codegraph`'s installer, `uvx`, etc. are covered too.
const WRAP_TARGETS: &[&str] = &["npx", "npm", "yarn", "pnpm", "node", "bun", "deno"];

/// On Windows, rewrite a stdio command that needs the command interpreter into
/// `cmd /d /c <command> <args…>` (as separate tokens) so the spawned CLI can
/// launch it. A command needs wrapping when it is (or resolves to) a
/// `.cmd`/`.bat` shim — Windows `CreateProcess` (and Node's `spawn` without a
/// shell) can't execute those directly, so both the probe and the synced agent
/// config would fail to launch e.g. `codegraph`, whose installer ships only
/// `codegraph.cmd`. See [`needs_cmd_wrap`] for the exact rule. `config_path` is
/// consulted to skip wrapping for configs that live under a WSL distribution
/// (`\\wsl$\…` / `\\wsl.localhost\…`), where Linux runs natively. On non-Windows
/// this is a zero-cost pass-through.
/// `pub` so the probe module can reuse it without duplicating the WSL rule.
///
/// Why separate tokens (not a single pre-quoted `/c` line): the run time that
/// later spawns this (Rust's `Command` for the probe, Node's `spawn` for the
/// agent) applies its OWN argument escaping, which would re-escape a pre-quoted
/// line into something cmd can't parse (`'\"npx\" …' is not recognized`), and
/// would quote the command token — breaking `%~dp0` inside `.cmd` shims
/// (npx.cmd, codegraph.cmd, …) so they can't locate their own launcher. Leaving
/// the tokens separate keeps the command bare (`%~dp0` resolves) and lets the
/// spawner quote each arg itself. `/d` disables the `AutoRun` registry hook (a
/// local command-injection surface). Command/args originate from user-managed
/// MCP config (not untrusted input); a single arg containing cmd metacharacters
/// but no spaces (e.g. `a&b`) is the one residual the spawner won't auto-quote.
pub fn wrap_command_for_windows(
    command: &Option<String>,
    args: &[String],
    config_path: &Path,
) -> (Option<String>, Vec<String>) {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = config_path;
        (command.clone(), args.to_vec())
    }
    #[cfg(target_os = "windows")]
    {
        let Some(cmd) = command else {
            return (None, args.to_vec());
        };
        // WSL config files target a Linux filesystem — don't wrap.
        let path_str = config_path.to_string_lossy();
        if path_str.starts_with(r"\\wsl$\") || path_str.starts_with(r"\\wsl.localhost\") {
            return (Some(cmd.clone()), args.to_vec());
        }
        if !needs_cmd_wrap(cmd) {
            return (Some(cmd.clone()), args.to_vec());
        }
        // `cmd /d /c <command> <args…>` as separate tokens — see the doc note
        // for why this isn't a single pre-quoted `/c` line.
        let mut out = Vec::with_capacity(3 + args.len());
        out.push("/d".into());
        out.push("/c".into());
        out.push(cmd.clone());
        out.extend(args.iter().cloned());
        (Some("cmd".into()), out)
    }
}

/// Decide whether a stdio command must be launched through `cmd /d /s /c` on
/// Windows. `CreateProcess` (and Node's `spawn` without a shell) can't run a
/// `.cmd`/`.bat` directly, so any command that is — or resolves to — such a
/// shim must be wrapped. Returns `true` when, in order:
///   1. the command already carries a `.cmd`/`.bat` extension (an explicit
///      path to a shim), or
///   2. its bare stem is a known Node-ecosystem shim launcher
///      ([`WRAP_TARGETS`] — wrapped even when not resolvable from *this*
///      process's PATH, since the agent's PATH may differ), or
///   3. the bare name resolves via PATH+PATHEXT (`which`) to a `.cmd`/`.bat`.
/// A real `.exe` (or a command that can't be resolved at all) is left alone so
/// the caller reports a genuine "program not found" instead of papering over
/// it. Windows-only: non-Windows builds never consult `which` (the wrapper is
/// a pass-through there).
#[cfg(target_os = "windows")]
fn needs_cmd_wrap(cmd: &str) -> bool {
    // (1) Explicit `.cmd`/`.bat` extension on the command itself — e.g.
    // `C:\nodejs\npx.cmd` or `.\tools\build.bat`.
    let explicit = Path::new(cmd)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    if matches!(explicit.as_deref(), Some("cmd") | Some("bat")) {
        return true;
    }
    // (2) Known shim launcher by bare stem (lowercased, no path/suffix). Kept
    // as an explicit list so it wraps even if not on this process's PATH.
    let stem = Path::new(cmd)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(cmd)
        .to_ascii_lowercase();
    if WRAP_TARGETS.contains(&stem.as_str()) {
        return true;
    }
    // (3) Bare name resolving to a `.cmd`/`.bat` via PATH+PATHEXT — the
    // codegraph / uvx case, where only a `.cmd` shim exists on disk.
    let resolved = which::which(cmd)
        .ok()
        .and_then(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
        });
    matches!(resolved.as_deref(), Some("cmd") | Some("bat"))
}

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

/// Read servers out of a JSON file, picking the container object via `pick`.
fn read_map(
    raw: &str,
    pick: fn(&Map<String, Value>) -> Option<&Map<String, Value>>,
) -> Vec<(String, Value)> {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let Some(map) = v.as_object().and_then(pick) else {
        return Vec::new();
    };
    map.iter()
        .map(|(k, v2)| (k.clone(), v2.clone()))
        .collect()
}

/// Mutate a (possibly nested) object path inside a JSON root, preserving all
/// other content. Fails loudly on unparseable/non-object input instead of
/// treating it as an empty object — writing that back would erase the whole
/// config file.
fn apply_at_path(
    raw: &str,
    path: &[&str],
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
    let servers = tree_get_mut(v.as_object_mut().unwrap(), path);
    for name in disabled {
        servers.remove(name);
    }
    for (name, native) in enabled {
        servers.insert(name.clone(), native.clone());
    }
    serde_json::to_string_pretty(&v)
        .map_err(|e| AppError::Internal(format!("serialize MCP config: {e}")))
}

fn tree_get_mut<'a>(
    root: &'a mut Map<String, Value>,
    path: &[&str],
) -> &'a mut Map<String, Value> {
    let mut cur = root;
    for key in path {
        let entry = cur
            .entry(key.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        cur = entry.as_object_mut().unwrap();
    }
    cur
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

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

/// ==== Claude Code ====  `~/.claude.json` -> `mcpServers` at root.
pub struct ClaudeCode;
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

/// ==== opencode ====  `~/.config/opencode/opencode.json` -> `mcp` object.
/// Keyed under `opencode-desktop`: the CLI variant is not supported
/// because it shared this file with the Desktop agent and the two fought over
/// it. opencode-desktop is the sole surviving OpenCode agent.
pub struct OpenCode;
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

/// ==== pi ====  `~/.pi/agent/mcp.json` -> `mcpServers` object at root.
pub struct Pi;
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

/// ==== zcode ====  `~/.zcode/cli/config.json` -> `mcp.servers` object.
/// The schema is strict (an unknown key silently drops the server), so
/// `to_native` writes only fields ZCode itself writes: `type`, the transport
/// fields, `enabled`, and `timeoutMs` on stdio servers.
pub struct ZCode;
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

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub fn all() -> Vec<&'static dyn Provider> {
    vec![&ClaudeCode, &OpenCode, &Pi, &ZCode]
}

pub fn for_agent(id: &str) -> Option<&'static dyn Provider> {
    all().into_iter().find(|p| p.agent_id() == id)
}

pub fn agent_exists(id: &str) -> bool {
    for_agent(id).is_some()
}

/// Remove one named entry from an agent's MCP config, preserving every other
/// entry (managed or hand-authored). No-op when the agent is unknown, the
/// config file is missing, or the named entry isn't present.
pub fn remove_server(agent: &str, name: &str) -> AppResult<()> {
    let Some(p) = for_agent(agent) else {
        return Ok(());
    };
    let Ok(home) = home_dir() else {
        return Ok(());
    };
    let path = p.config_path(&home);
    if !path.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(&path).map_err(AppError::Io)?;
    // Empty enabled map + [name] disabled = drop the one entry, write back.
    // `apply` preserves everything else verbatim (and refuses to rewrite a
    // file that isn't valid JSON).
    let out = p.apply(&raw, &BTreeMap::new(), &[name.to_string()])?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(AppError::Io)?;
    }
    atomic_write(&path, out.as_bytes())
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod registry_consistency_tests;
