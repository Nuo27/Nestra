//! The `AGENTS` static registry and the data types that describe one agent.
//!
//! `Capability` carries UI-agnostic booleans describing what an agent can do
//! (manageable, supports provider configuration, supports sessions, etc.).
//! Wire-format questions (`supported_protocols`, `model_selection`) live on
//! the `ConfigAdapter` trait, not here.

/// Every agent Nestra manages, in stable display order. Closed list —
/// detection requires per-agent install-path knowledge that can't be inferred,
/// so agents are declared here rather than auto-discovered.
pub fn agents() -> &'static [AgentSpec] {
    &AGENTS
}

/// Look up one agent by id.
pub fn agent_spec(id: &str) -> Option<&'static AgentSpec> {
    AGENTS.iter().find(|a| a.id == id)
}

/// Convenience: the declared capability for an agent id.
pub fn capability_for(id: &str) -> Option<&'static Capability> {
    agent_spec(id).map(|a| &a.capability)
}

/// Convenience: the detection spec for an agent id.
pub fn detect_spec_for(id: &str) -> Option<&'static DetectSpec> {
    agent_spec(id).map(|a| &a.detect)
}

/// Convenience: the config reference (writer key + path) for an agent id.
pub fn config_ref_for(id: &str) -> Option<&'static ConfigRef> {
    agent_spec(id).map(|a| &a.config)
}

pub static AGENTS: &[AgentSpec] = &[
    AgentSpec {
        id: "claude-code",
        display_name: "Claude Code",
        kind: "claude-code",
        agent_kind: AgentKind::Cli,
        detect: DetectSpec {
            binary_candidates: &["claude"],
            install_paths: &[],
            config_relative: Some(".claude"),
            skip_version_probe: false,
        },
        capability: Capability {
            manageable: true,
            supports_provider_configuration: true,
            supports_multiple_providers: false,
            supports_provider_injection: true,
            supports_factory_restore: true,
            supports_sessions: true,
            supports_mcp: true,
            supports_mcp_enabled: false,
            supports_skills: true,
            supports_gateway: true,
        },
        config: ConfigRef {
            writer: "claude-code",
            relative_path: ".claude/settings.json",
        },
        session: Some(SessionRef {
            reader: "claude-code",
            resume_command: Some("claude --resume {id}"),
            unsupported_reason: None,
        }),
        skill_dir: Some(".claude/skills"),
        skill_name_matches_dir: false,
    },
    AgentSpec {
        id: "opencode-desktop",
        display_name: "OpenCode Desktop",
        kind: "opencode-desktop",
        agent_kind: AgentKind::Desktop,
        detect: DetectSpec {
            binary_candidates: &[],
            install_paths: &[
                DetectorPath::PlatformLocalAppData("Programs/OpenCode/OpenCode.exe"),
                DetectorPath::PlatformAppData("OpenCode"),
                DetectorPath::HomeRelative("Applications/OpenCode.app"),
            ],
            // Shares config with the CLI; probe so a "configured" badge shows
            // even without the Desktop binary on disk.
            config_relative: Some(".config/opencode"),
            skip_version_probe: false,
        },
        capability: Capability {
            manageable: true,
            supports_provider_configuration: true,
            supports_multiple_providers: true,
            supports_provider_injection: true,
            supports_factory_restore: true,
            supports_sessions: true,
            supports_mcp: true,
            supports_mcp_enabled: true,
            supports_skills: true,
            supports_gateway: true,
        },
        config: ConfigRef {
            writer: "opencode",
            relative_path: ".config/opencode/opencode.json",
        },
        session: Some(SessionRef {
            reader: "opencode-desktop",
            resume_command: None,
            unsupported_reason: Some("OpenCode Desktop sessions are read-only — resume from within the OpenCode app"),
        }),
        skill_dir: Some(".config/opencode/skills"),
        // OpenCode requires the SKILL.md frontmatter `name` to equal the skill
        // directory name (and be lowercase-alphanumeric + single hyphens), else
        // it silently drops the skill. Nestra copies land at `<dir>/<id>`, so
        // the dir name is always compliant; this flag makes the sync rewrite
        // the copied frontmatter `name` to the id. Claude Code/Pi are lenient
        // and keep the user's display name.
        skill_name_matches_dir: true,
    },
    AgentSpec {
        id: "pi",
        display_name: "Pi",
        kind: "pi",
        agent_kind: AgentKind::Cli,
        detect: DetectSpec {
            binary_candidates: &["pi"],
            install_paths: &[],
            config_relative: Some(".pi/agent"),
            skip_version_probe: false,
        },
        capability: Capability {
            manageable: true,
            supports_provider_configuration: true,
            supports_multiple_providers: true,
            supports_provider_injection: true,
            supports_factory_restore: true,
            supports_sessions: true,
            supports_mcp: true,
            supports_mcp_enabled: false,
            supports_skills: true,
            supports_gateway: true,
        },
        config: ConfigRef {
            writer: "pi",
            relative_path: ".pi/agent/models.json",
        },
        session: Some(SessionRef {
            reader: "pi",
            resume_command: Some("pi --session {id}"),
            unsupported_reason: None,
        }),
        skill_dir: Some(".agents/skills"),
        skill_name_matches_dir: false,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Cli,
    Desktop,
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

/// One agent's full description. The unit of registration in [`AGENTS`].
#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub id: &'static str,
    pub display_name: &'static str,
    /// Stored as the `kind` column in the `cli` DB table.
    pub kind: &'static str,
    pub agent_kind: AgentKind,
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
}

impl AgentSpec {
    /// `true` when Nestra has a config adapter and can write this agent's config.
    pub fn manageable(&self) -> bool {
        !self.config.writer.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_three_agents() {
        assert_eq!(agents().len(), 3);
    }

    #[test]
    fn agent_ids_are_unique() {
        let mut ids: Vec<&str> = agents().iter().map(|a| a.id).collect();
        ids.sort();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate agent id");
    }

    #[test]
    fn manageable_agents_have_nonempty_writer_and_path() {
        for a in agents() {
            assert!(a.manageable(), "{} should be manageable", a.id);
            assert!(!a.config.relative_path.is_empty(), "{} manageable but no path", a.id);
        }
    }

    #[test]
    fn resumable_agents_have_resume_command_and_reader() {
        for a in agents() {
            if let Some(s) = a.session {
                if s.resume_command.is_some() {
                    assert!(s.unsupported_reason.is_none(), "{} has both resume and reason", a.id);
                    assert!(!s.reader.is_empty());
                }
            }
        }
    }

    #[test]
    fn known_agents_present() {
        let ids: Vec<&str> = agents().iter().map(|a| a.id).collect();
        for expected in ["claude-code", "opencode-desktop", "pi"] {
            assert!(ids.contains(&expected), "missing {expected}");
        }
    }
}
