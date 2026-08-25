//! Pi (CLI): registry entry + per-agent integration modules.
//!
//! - `config`   — `ConfigAdapter` writing the three `~/.pi/agent/` files
//!   (`models.json`, `auth.json`, `settings.json`)
//! - `sessions` — `~/.pi/agent/sessions/*.jsonl` session importer
//! - `mcp`      — `~/.pi/agent/mcp.json` `mcpServers` MCP provider, gated
//!   on the community `pi-mcp-adapter` package being installed (pi has no
//!   native MCP — see `mcp::adapter_installed`)

pub mod config;
pub mod mcp;
pub mod sessions;

use crate::agents::spec::{
    AgentKind, AgentSpec, Capability, ConfigRef, DetectSpec, GatewayWire, SessionRef,
};

pub static SPEC: AgentSpec = AgentSpec {
    id: "pi-cli",
    display_name: "Pi CLI",
    kind: "pi-cli",
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
        writer: "pi-cli",
        relative_path: ".pi/agent/models.json",
    },
    session: Some(SessionRef {
        reader: "pi-cli",
        resume_command: Some("pi --session {id}"),
        unsupported_reason: None,
    }),
    skill_dir: Some(".agents/skills"),
    skill_name_matches_dir: false,
    gateway_wire: GatewayWire::Chat,
    adapter: config::new,
    importer: Some(sessions::new),
    mcp_provider: Some(mcp::new),
    // Pi has no native MCP — support comes from the community
    // `pi-mcp-adapter` package. Without it Nestra must neither advertise
    // MCP capability for pi nor write `mcp.json` (nothing would read it).
    mcp_available: Some(mcp::adapter_installed),
};
