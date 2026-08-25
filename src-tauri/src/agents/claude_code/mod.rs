//! Claude Code (CLI): registry entry + per-agent integration modules.
//!
//! - `config`   — `ConfigAdapter` writing `~/.claude/settings.json` env block
//! - `sessions` — `~/.claude/projects/**/*.jsonl` session importer
//! - `mcp`      — `~/.claude.json` `mcpServers` MCP provider

pub mod config;
pub mod mcp;
pub mod sessions;

use crate::agents::spec::{
    AgentKind, AgentSpec, Capability, ConfigRef, DetectSpec, GatewayWire, SessionRef,
};

pub static SPEC: AgentSpec = AgentSpec {
    id: "claude-code-cli",
    display_name: "Claude Code CLI",
    kind: "claude-code-cli",
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
        writer: "claude-code-cli",
        relative_path: ".claude/settings.json",
    },
    session: Some(SessionRef {
        reader: "claude-code-cli",
        resume_command: Some("claude --resume {id}"),
        unsupported_reason: None,
    }),
    skill_dir: Some(".claude/skills"),
    skill_name_matches_dir: false,
    gateway_wire: GatewayWire::Anthropic,
    adapter: config::new,
    importer: Some(sessions::new),
    mcp_provider: Some(mcp::new),
    mcp_available: None,
};
