//! The data types that describe one agent — the shape of an [`AgentSpec`]
//! and everything it references.
//!
//! `Capability` carries UI-agnostic booleans describing what an agent can do
//! (manageable, supports provider configuration, supports sessions, etc.).
//! Wire-format questions (`supported_protocols`, `model_selection`) live on
//! the `ConfigAdapter` trait, not here.
//!
//! The registry itself (`AGENTS`) is assembled in `agents/mod.rs` from each
//! agent module's `SPEC` — this file holds no agent data.

use crate::config_writer::ConfigAdapter;
use crate::mcp::providers::Provider as McpProvider;
use crate::session::SessionImporter;

/// The inbound wire protocol an agent speaks when pointed at the Nestra
/// gateway. Gateway dispatch derives from this — one enum, no per-agent
/// match arms in the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayWire {
    /// Anthropic Messages (`POST /v1/messages`).
    Anthropic,
    /// OpenAI Chat Completions (`POST /v1/chat/completions`).
    Chat,
    /// OpenAI Responses (`POST /v1/responses`) — Codex's only wire.
    Responses,
}

/// Where to look for an agent on disk. `binary_candidates` are checked via
/// `which::which` first; `install_paths` are checked in order, first hit wins;
/// if neither succeeds, the presence of `config_relative` under the user's
/// home is treated as a soft "installed" signal.
#[derive(Debug, Clone, Copy)]
pub struct DetectSpec {
    pub binary_candidates: &'static [&'static str],
    pub install_paths: &'static [DetectorPath],
    pub config_relative: Option<&'static str>,
    /// `true` for GUI apps where `<exe> --version` would hang or launch the
    /// app instead of printing a version. When set, the version probe is
    /// skipped.
    pub skip_version_probe: bool,
}

/// A single absolute or home-relative path to probe. Resolved via
/// [`crate::db::platform_dirs`].
#[derive(Debug, Clone, Copy)]
pub enum DetectorPath {
    /// `%LOCALAPPDATA%` on Windows, the `data_local_dir()` value on macOS
    /// (`~/Library/Application Support`) and Linux (`~/.local/share`).
    PlatformLocalAppData(&'static str),
    /// `%APPDATA%` on Windows, the `config_dir()` value on macOS
    /// (`~/Library/Application Support`) and Linux (`~/.config`).
    PlatformAppData(&'static str),
    /// Joined onto the user's home directory.
    HomeRelative(&'static str),
    /// MSIX/Store package: resolve `<package family prefix>*` then join
    /// `suffix`. Falls back to `Get-AppxPackage` because normal users can't
    /// enumerate `C:\Program Files\WindowsApps`.
    WindowsAppsGlob {
        prefix: &'static str,
        suffix: &'static str,
    },
    /// Hard-coded absolute path (used sparingly, mostly for tests).
    Absolute(&'static str),
}

/// UI-agnostic capability booleans. Describes what the agent can do, never
/// how the UI should render it. The wire-format questions (`supported_protocols`,
/// `model_selection`) live on the ConfigAdapter — not here.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Capability {
    /// Nestra can write this agent's config.
    pub manageable: bool,
    /// The agent supports binding a Provider (endpoint) to it.
    pub supports_provider_configuration: bool,
    /// The agent supports multiple Providers at once (`true`) versus a
    /// single-slot binding (`false`).
    pub supports_multiple_providers: bool,
    /// The agent supports runtime Provider injection — i.e. its ConfigAdapter
    /// can write the provider binding into its config file.
    pub supports_provider_injection: bool,
    /// The agent supports Factory Configuration backup and restore.
    pub supports_factory_restore: bool,
    /// The agent supports session reading.
    pub supports_sessions: bool,
    /// The agent supports MCP server sync into its config file.
    pub supports_mcp: bool,
    /// The agent's MCP config format has a per-server enabled field
    /// (`enabled: true/false`). Only such agents can express a "written but
    /// disabled" MCP entry; the UI gates the third state on this.
    pub supports_mcp_enabled: bool,
    /// The agent exposes a skills directory (`skill_dir` is set). Drives the
    /// Skills page agent filter.
    pub supports_skills: bool,
    /// The agent can be pointed at the Nestra gateway (Routed mode). Drives
    /// the Direct/Routed mode switch in the UI.
    pub supports_gateway: bool,
}

/// Reference to the agent's config format: which `ConfigAdapter` handles it
/// and where the config file lives relative to the user's home.
#[derive(Debug, Clone, Copy)]
pub struct ConfigRef {
    /// Adapter key resolved by [`crate::agents::adapter_for`]. Empty for
    /// read-only agents.
    pub writer: &'static str,
    /// Config file path relative to the user's home, e.g. `.claude/settings.json`.
    pub relative_path: &'static str,
}

/// Session integration for an agent: which reader parses its session files,
/// and the native resume command (if any). Agents whose `resume_command` is
/// `None` are read-only session sources (Desktop variants) — their sessions
/// index but can't be resumed from Nestra.
#[derive(Debug, Clone, Copy)]
pub struct SessionRef {
    /// Reader key resolved by the session importer registry.
    pub reader: &'static str,
    /// Resume command template with `{id}` placeholder, or `None` when the
    /// agent's sessions aren't resumable.
    pub resume_command: Option<&'static str>,
    /// Why resume is unavailable, shown in the UI when `resume_command` is None.
    pub unsupported_reason: Option<&'static str>,
}

/// One agent's full description. The unit of registration in
/// [`crate::agents::AGENTS`]; one `SPEC` constant per agent module.
#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub id: &'static str,
    pub display_name: &'static str,
    /// Stored as the `kind` column in the `cli` DB table.
    pub kind: &'static str,
    pub detect: DetectSpec,
    pub capability: Capability,
    pub config: ConfigRef,
    pub session: Option<SessionRef>,
    /// Skills directory relative to home, when the agent supports skills.
    pub skill_dir: Option<&'static str>,
    /// When true, the synced copy's SKILL.md frontmatter `name` is rewritten to
    /// the skill id (= dir name). Required by OpenCode, which drops skills whose
    /// frontmatter `name` ≠ directory name.
    pub skill_name_matches_dir: bool,
    /// Inbound wire this agent speaks to the gateway (see [`GatewayWire`]).
    pub gateway_wire: GatewayWire,
    /// Constructs the agent's [`ConfigAdapter`]. Present for every registry
    /// agent; `manageable()` is derived from `config.writer` being non-empty.
    pub adapter: fn() -> Box<dyn ConfigAdapter>,
    /// Session importer constructor; `Some` iff `capability.supports_sessions`.
    pub importer: Option<fn() -> Box<dyn SessionImporter>>,
    /// MCP config provider constructor; `Some` iff `capability.supports_mcp`.
    pub mcp_provider: Option<fn() -> Box<dyn McpProvider>>,
    /// Runtime gate on `capability.supports_mcp`: `None` = always available;
    /// `Some(f)` = MCP support depends on an install-time condition checked
    /// per call (pi-cli: the community `pi-mcp-adapter` package must be
    /// installed — pi has no native MCP). The reported capability, the
    /// provider registry, and MCP sync all consult it.
    pub mcp_available: Option<fn() -> bool>,
}

impl AgentSpec {
    /// The capability the UI sees: the static declaration with the
    /// [`Self::mcp_available`] runtime gate applied to `supports_mcp`.
    pub fn effective_capability(&self) -> Capability {
        let mut c = self.capability;
        if let Some(f) = self.mcp_available {
            c.supports_mcp = c.supports_mcp && f();
        }
        c
    }

    /// `true` when Nestra has a config adapter and can write this agent's config.
    pub fn manageable(&self) -> bool {
        !self.config.writer.is_empty()
    }
}
