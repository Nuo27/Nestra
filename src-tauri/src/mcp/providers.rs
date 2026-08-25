//! The `Provider` trait + shared machinery for per-agent MCP config files.
//! A `Provider` knows the one file shape of its agent's MCP config: how to
//! find it, read native entries into `(name, Value)` pairs, and write a
//! merged result back preserving everything Nestra doesn't manage.
//!
//! The impls live with their agents (`agents/<id>/mcp.rs`); the registry
//! (`all`/`for_agent`) is derived from the `AGENTS` agent registry via each
//! spec's `mcp_provider` constructor hook.
//!
//! Native entries are *opaque* `serde_json::Value`s decoded to canonical
//! `McpTransport` via `crate::mcp::from_native` (concept keys, not fixed
//! field-per-file names). A new agent means a new `Provider` impl in its
//! agent module.

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

pub(crate) fn stdio_or_http(s: &McpTransport) -> Value {
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
pub(crate) fn read_map(
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
pub(crate) fn apply_at_path(
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

/// Every MCP-capable agent's provider. Derived from the `AGENTS` agent
/// registry via each spec's `mcp_provider` constructor — an agent module
/// registers itself, no central list to keep in sync. Specs with an
/// `mcp_available` runtime gate (pi's community adapter) are excluded
/// while the gate returns false.
pub fn all() -> Vec<Box<dyn Provider>> {
    crate::agents::agents()
        .iter()
        .filter(|a| mcp_ready(a))
        .filter_map(|a| a.mcp_provider.map(|f| f()))
        .collect()
}

/// The provider for one agent id, when that agent supports MCP and its
/// runtime gate (if any) passes.
pub fn for_agent(id: &str) -> Option<Box<dyn Provider>> {
    crate::agents::agent_spec(id)
        .filter(|a| mcp_ready(a))
        .and_then(|a| a.mcp_provider.map(|f| f()))
}

/// `supports_mcp` AND the `mcp_available` runtime gate.
fn mcp_ready(a: &crate::agents::AgentSpec) -> bool {
    a.capability.supports_mcp && a.mcp_available.map_or(true, |f| f())
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
