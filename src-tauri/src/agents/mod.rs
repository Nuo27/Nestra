//! Agent registry + detection + per-agent config adapters — the single source
//! of truth for every coding agent Nestra manages.
//!
//! One `AgentSpec` per agent carries its detection strategy, capabilities,
//! config-format reference, session reader, and skill directory. Adding a new
//! agent = adding one entry to [`AGENTS`] (plus a `ConfigAdapter` impl only if
//! its config file uses a brand-new format). No other module needs editing.
//!
//! Layout:
//! - `spec`     — the `AGENTS` static registry + describing data types
//! - `detect`   — auto-detection algorithm (PATH, install paths, soft signals)
//! - `claude_code`, `opencode`, `pi`, `zcode` — per-agent `ConfigAdapter` impls
//!
//! The detection *data* types (`DetectSpec`, `DetectorPath`) live in `spec`
//! so the dependency graph is one-way: `agents` → `config_writer` (leaf).

pub mod spec;
pub mod detect;
pub mod claude_code;
pub mod opencode;
pub mod pi;
pub mod zcode;

pub use spec::{
    agent_spec, capability_for, agents, config_ref_for, detect_spec_for, AgentKind, AgentSpec,
    Capability, ConfigRef, DetectSpec, DetectorPath, SessionRef,
};

use crate::config_writer::ConfigAdapter;
use crate::error::AppError;

/// Wrap an external library's display-able error as an internal `AppError`.
/// Used by JSON-parsing call sites in the per-agent adapters.
pub(crate) fn internal<E: std::fmt::Display>(e: E) -> AppError {
    AppError::Internal(e.to_string())
}

/// Resolve a config adapter by writer key (matches `AgentSpec.config.writer`).
/// Driven by the `AGENTS` registry: a new agent that reuses an existing
/// writer key resolves automatically, and a NEW writer key needs its adapter
/// registered here — `tests::every_manageable_writer_resolves` pins that
/// every non-empty `config.writer` in the registry resolves.
pub fn adapter_for(writer_key: &str) -> Option<Box<dyn ConfigAdapter>> {
    // Keep this match in sync with the writer keys in spec.rs AGENTS.
    match writer_key {
        "claude-code-cli" => Some(Box::new(claude_code::ClaudeCode)),
        "opencode" => Some(Box::new(opencode::OpenCode)),
        "pi-cli" => Some(Box::new(pi::Pi)),
        "zcode" => Some(Box::new(zcode::ZCode)),
        _ => None,
    }
}

/// Every manageable agent's `config.writer` must resolve to an adapter —
/// a typo or a new writer key silently disables config writing for that
/// agent otherwise.
#[cfg(test)]
mod tests;

/// Reveal `path` in the OS file manager.
///
/// Windows-only: spawns `explorer.exe`. A FILE is passed with `/select,` so
/// Explorer opens the parent and selects it (the old form opened the file
/// with its associated app instead of locating it); a directory opens
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
