//! Codex Desktop (OpenAI): registry entry + per-agent integration modules.
//!
//! - `config`  — TOML `ConfigAdapter` writing `~/.codex/config.toml`
//!   (`[model_providers.nestra-*]` + root selection keys)
//! - `sync`    — provider-visibility sync (keeps existing conversations
//!   listed after a provider switch)
//! - `mcp`     — `[mcp_servers]` MCP provider in the same config.toml
//! - `sessions` — rollout JSONL + SQLite threads session importer (read-only)

pub mod config;
pub mod mcp;
pub mod sessions;
pub mod sync;

use crate::agents::spec::{
    AgentKind, AgentSpec, Capability, ConfigRef, DetectSpec, DetectorPath, GatewayWire, SessionRef,
};

pub static SPEC: AgentSpec = AgentSpec {
    id: "codex-desktop",
    display_name: "Codex Desktop",
    kind: "codex-desktop",
    agent_kind: AgentKind::Desktop,
    detect: DetectSpec {
        // The CLI ships bundled inside the Desktop app under a hash-named
        // `bin\<hash>\codex.exe` — unstable, not on PATH; detection is by
        // install location + the `~/.codex` config dir soft signal.
        binary_candidates: &[],
        install_paths: &[DetectorPath::PlatformLocalAppData("OpenAI/Codex")],
        config_relative: Some(".codex"),
        skip_version_probe: true,
    },
    capability: Capability {
        manageable: true,
        supports_provider_configuration: true,
        // One active `model_provider` slot, like Claude Code / ZCode.
        supports_multiple_providers: false,
        supports_provider_injection: true,
        supports_factory_restore: true,
        supports_sessions: true,
        supports_mcp: true,
        // No per-server `enabled` field documented for `[mcp_servers]`.
        supports_mcp_enabled: false,
        supports_skills: true,
        supports_gateway: true,
    },
    config: ConfigRef {
        writer: "codex",
        relative_path: ".codex/config.toml",
    },
    session: Some(SessionRef {
        reader: "codex-desktop",
        // The resumable CLI is bundled inside the app (hash-named dir, not on
        // PATH), so a copied `codex resume <id>` wouldn't run — sessions are
        // read-only, resumed from within the Codex app.
        resume_command: None,
        unsupported_reason: Some("Codex sessions are read-only — resume from within the Codex app"),
    }),
    skill_dir: Some(".codex/skills"),
    skill_name_matches_dir: false,
    // Codex speaks only the OpenAI Responses wire (`wire_api = "responses"`).
    gateway_wire: GatewayWire::Responses,
    adapter: config::new,
    importer: Some(sessions::new),
    mcp_provider: Some(mcp::new),
};
