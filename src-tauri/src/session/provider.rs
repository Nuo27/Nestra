//! Compatibility layer between agent-native session formats and the
//! universal session model.
//!
//! Each provider is described by one [`SessionProvider`] — a single object
//! that owns the importer (Provider → Universal), the resume command (single
//! source of truth used by both Copy button and Open-in-terminal), and the
//! reason string when the provider cannot be resumed.
//!
//! Resume is **same-provider only**: a Claude session resumes in the Claude
//! agent, a Pi session in the Pi agent. Cross-provider "translation" was
//! explored and removed — it was unverified, inherently lossy (Claude can't
//! represent someone else's subagents/MCP), and nobody does it in practice.
//! Keeping the surface small here keeps the UI honest.
//!
//! Adding a new provider = register one `SessionProvider` in
//! [`default_provider_registry`].


use crate::error::{AppError, AppResult};

/// Public face of a provider in the compatibility layer.
pub struct SessionProvider {
    pub id: &'static str,
    /// The agent registry id this provider's sessions can be resumed in
    /// (matches `agents::adapter_for(agent_id)`), or `None` if the agent has
    /// no resumable binary today.
    pub agent_id: Option<&'static str>,
    /// `Some` when this provider cannot be resumed; the string is shown in
    /// the UI. `None` means resumable via `resume_command` (when present).
    pub unsupported_reason: Option<&'static str>,
    /// Resume command with `{id}` placeholder, used by both the Copy
    /// button and Open-in-terminal. Single source of truth — kept here so
    /// the two paths cannot disagree.
    pub resume_command: Option<&'static str>,
}

/// Build the default registry. Derived from `agents::AGENTS`: every agent
/// whose `SessionRef.resume_command` is `Some` becomes a resumable
/// `SessionProvider`. Desktop variants (resume_command = None) are
/// intentionally absent — the frontend derives "delete only" from registry
/// absence and the importers still index their session files.
pub fn default_provider_registry() -> Vec<SessionProvider> {
    use crate::agents;
    agents::agents()
        .iter()
        .filter_map(|a| {
            let s = a.session?;
            let cmd = s.resume_command?;
            Some(SessionProvider {
                id: a.id,
                agent_id: Some(a.id),
                unsupported_reason: None,
                resume_command: Some(cmd),
            })
        })
        .collect()
}

/// Build a launchable resume command for `agent_id` against a session whose
/// provider is `provider_id`. Same provider — substitute `{id}` into the
/// native template. Cross-provider is refused with `Err(Validation)` so the
/// UI can show why — cross-provider resume was removed (unverified, lossy).
pub fn build_resume_command(
    registry: &[SessionProvider],
    provider_id: &str,
    agent_id: &str,
    session_id: &str,
) -> AppResult<String> {
    let target = registry
        .iter()
        .find(|p| p.agent_id == Some(agent_id))
        .ok_or_else(|| AppError::Validation(format!("'{agent_id}' is not a registered agent")))?;
    let source = registry
        .iter()
        .find(|p| p.id == provider_id)
        .ok_or_else(|| {
            AppError::Validation(format!("'{provider_id}' is not a registered provider"))
        })?;
    let template = target
        .resume_command
        .ok_or_else(|| AppError::Validation(target_reason(target)))?;
    // Same provider — same agent is the only resumable case.
    if source.id != target.id {
        return Err(AppError::Validation(format!(
            "{} (source '{}' — agent '{}')",
            target_reason(target),
            source.id,
            target.id
        )));
    }
    // The resume template is eventually launched through a shell
    // (`cmd /K …` on Windows), so the substituted id must be shell-safe.
    // Session ids originate from agent-native session files (pollutable on
    // disk) — reject anything outside [A-Za-z0-9._-] instead of quoting, so
    // a hostile id can never splice `&` / `|` / `>` into the command line.
    if !session_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(AppError::Validation(format!(
            "session id '{session_id}' is not a safe identifier"
        )));
    }
    Ok(template.replace("{id}", session_id))
}

fn target_reason(p: &SessionProvider) -> String {
    p.unsupported_reason
        .map(String::from)
        .unwrap_or_else(|| format!("'{}' has no resume command registered", p.id))
}