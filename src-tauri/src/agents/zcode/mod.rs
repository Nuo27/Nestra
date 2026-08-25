//! ZCode Desktop: registry entry + per-agent integration modules.
//!
//! - `config`   — `ConfigAdapter` writing `~/.zcode/v2/config.json`
//! - `sessions` — `~/.zcode/cli/db/db.sqlite` session importer (read-only)
//! - `mcp`      — `~/.zcode/cli/config.json` `mcp.servers` MCP provider

pub mod config;
pub mod mcp;
pub mod sessions;

use crate::agents::spec::{
    AgentSpec, Capability, ConfigRef, DetectSpec, DetectorPath, GatewayWire, SessionRef,
};

pub static SPEC: AgentSpec = AgentSpec {
    id: "zcode-desktop",
    display_name: "ZCode Desktop",
    kind: "zcode-desktop",
    detect: DetectSpec {
        // The agent CLI ships bundled inside the Electron app
        // (`resources/glm/zcode.cjs`) and is not on PATH — detection is by
        // install location, like OpenCode Desktop.
        binary_candidates: &[],
        install_paths: &[
            DetectorPath::PlatformLocalAppData("Programs/ZCode/ZCode.exe"),
            DetectorPath::PlatformAppData("ZCode"),
            DetectorPath::HomeRelative("Applications/ZCode.app"),
        ],
        config_relative: Some(".zcode"),
        skip_version_probe: true,
    },
    capability: Capability {
        manageable: true,
        supports_provider_configuration: true,
        supports_multiple_providers: false,
        supports_provider_injection: true,
        supports_factory_restore: true,
        supports_sessions: true,
        supports_mcp: true,
        // `~/.zcode/cli/config.json` MCP servers carry a per-server
        // `enabled` field, so the written-but-disabled state is expressible.
        supports_mcp_enabled: true,
        supports_skills: true,
        supports_gateway: true,
    },
    config: ConfigRef {
        writer: "zcode",
        relative_path: ".zcode/v2/config.json",
    },
    session: Some(SessionRef {
        reader: "zcode-desktop",
        // The resumable CLI is bundled inside the app and not on PATH, so
        // a copied `zcode --resume <id>` wouldn't run — sessions are
        // read-only, resumed from within the ZCode app.
        resume_command: None,
        unsupported_reason: Some("ZCode sessions are read-only — resume from within the ZCode app"),
    }),
    skill_dir: Some(".zcode/skills"),
    skill_name_matches_dir: false,
    gateway_wire: GatewayWire::Anthropic,
    adapter: config::new,
    importer: Some(sessions::new),
    mcp_provider: Some(mcp::new),
    mcp_available: None,
};
