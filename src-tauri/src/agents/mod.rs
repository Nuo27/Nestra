//! Agent registry + detection + per-agent integration — the single source
//! of truth for every coding agent Nestra manages.
//!
//! One agent = one directory (`agents/<id>/`) holding its `SPEC` (the
//! registry entry), `config.rs` (ConfigAdapter), `sessions.rs`
//! (SessionImporter) and `mcp.rs` (MCP provider). [`AGENTS`] below is the
//! only central list: every subsystem (config writing, session import, MCP
//! sync, gateway dispatch) derives from it via the spec's constructor hooks.
//! Adding an agent = one new directory + one line in [`AGENTS`] (+ the
//! frontend presentation entries).
//!
//! Layout:
//! - `spec`     — describing data types (no agent data lives there)
//! - `detect`   — auto-detection algorithm (PATH, install paths, soft signals)
//! - `<agent>/` — per-agent `SPEC` + config adapter + session importer + MCP
//!   provider

pub mod spec;
pub mod detect;
pub mod claude_code;
pub mod codex;
pub mod opencode;
pub mod pi;
pub mod zcode;

pub use spec::{
    AgentSpec, Capability, ConfigRef, DetectSpec, DetectorPath, GatewayWire, SessionRef,
};

use crate::config_writer::ConfigAdapter;
use crate::error::AppError;

/// Every agent Nestra manages, in stable display order. Closed list —
/// detection requires per-agent install-path knowledge that can't be inferred,
/// so agents are declared here rather than auto-discovered.
pub static AGENTS: &[&AgentSpec] = &[
    &claude_code::SPEC,
    &opencode::SPEC,
    &pi::SPEC,
    &zcode::SPEC,
    &codex::SPEC,
];

/// Look up every agent, in display order.
pub fn agents() -> &'static [&'static AgentSpec] {
    AGENTS
}

/// Look up one agent by id.
pub fn agent_spec(id: &str) -> Option<&'static AgentSpec> {
    AGENTS.iter().find(|a| a.id == id).copied()
}

/// Wrap an external library's display-able error as an internal `AppError`.
/// Used by JSON-parsing call sites in the per-agent adapters.
pub(crate) fn internal<E: std::fmt::Display>(e: E) -> AppError {
    AppError::Internal(e.to_string())
}

/// Resolve a config adapter by writer key (matches `AgentSpec.config.writer`).
/// Derived from the `AGENTS` registry: a writer key resolves iff some agent
/// declares it, so a new agent needs no edit here —
/// `tests::every_manageable_writer_resolves` pins that every non-empty
/// `config.writer` in the registry resolves.
pub fn adapter_for(writer_key: &str) -> Option<Box<dyn ConfigAdapter>> {
    AGENTS
        .iter()
        .find(|a| a.config.writer == writer_key)
        .map(|a| (a.adapter)())
}

/// Every manageable agent's `config.writer` must resolve to an adapter —
/// a typo or a new writer key silently disables config writing for that
/// agent otherwise.
#[cfg(test)]
mod tests;

/// Reveal `path` in the OS file manager.
///
/// Windows-only: spawns `explorer.exe`. A FILE is passed with `/select,` so
/// Explorer opens the parent and selects it instead of opening the file
/// with its associated app; a directory opens
/// directly. Non-Windows builds get a stub that errors — the caller should
/// degrade (the reveal feature is inherently Windows Explorer-based).
#[cfg(target_os = "windows")]
pub fn reveal_in_explorer(path: &std::path::Path) -> std::io::Result<()> {
    if path.is_file() {
        std::process::Command::new("explorer.exe")
            .arg("/select,")
            .arg(path)
            .spawn()?;
    } else {
        std::process::Command::new("explorer.exe").arg(path).spawn()?;
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn reveal_in_explorer(_path: &std::path::Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "reveal in file manager is Windows-only",
    ))
}

/// Resolve what a session's `source_path` should reveal.
pub fn reveal_target(source_path: &str) -> std::path::PathBuf {
    let p = std::path::PathBuf::from(source_path);
    if p.is_dir() {
        p
    } else {
        // A bare filename has an empty parent — fall back to "." instead of
        // producing an empty PathBuf (explorer.exe "" is undefined behavior).
        p.parent()
            .filter(|x| !x.as_os_str().is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    }
}
