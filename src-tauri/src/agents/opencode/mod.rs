//! OpenCode Desktop: registry entry + per-agent integration modules.
//!
//! - `config`   — `ConfigAdapter` writing `~/.config/opencode/opencode.json`
//!   (JSONC, shared with the CLI)
//! - `sessions` — OpenCode Desktop SQLite + JSONL session importer (read-only)
//! - `mcp`      — flat `mcp.<name>` entries in `opencode.json`

pub mod config;
pub mod mcp;
pub mod sessions;

use crate::agents::spec::{
    AgentKind, AgentSpec, Capability, ConfigRef, DetectSpec, DetectorPath, GatewayWire, SessionRef,
};

pub static SPEC: AgentSpec = AgentSpec {
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
        unsupported_reason: Some(
            "OpenCode Desktop sessions are read-only — resume from within the OpenCode app",
        ),
    }),
    skill_dir: Some(".config/opencode/skills"),
    // OpenCode requires the SKILL.md frontmatter `name` to equal the skill
    // directory name (and be lowercase-alphanumeric + single hyphens), else
    // it silently drops the skill. Nestra copies land at `<dir>/<id>`, so
    // the dir name is always compliant; this flag makes the sync rewrite
    // the copied frontmatter `name` to the id. Claude Code/Pi are lenient
    // and keep the user's display name.
    skill_name_matches_dir: true,
    gateway_wire: GatewayWire::Chat,
    adapter: config::new,
    importer: Some(sessions::new),
    mcp_provider: Some(mcp::new),
    mcp_available: None,
};
