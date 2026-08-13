# Nestra

A local-only desktop control center for AI coding agents — Claude Code,
OpenCode Desktop, and Pi. Manage providers, switch between direct and routed
modes, monitor quotas, and browse sessions — all without a cloud account.

## Features

- **Provider management** — Add API endpoints (Anthropic, OpenAI, OpenRouter,
  z.ai, MiniMax, and more). Keys are validated on save and encrypted at rest
  (AES-256-GCM).
- **Agent detection & binding** — Automatically detects Claude Code, OpenCode
  Desktop, and Pi on your system. Bind each agent to a provider — Nestra
  writes the config (after backing up the original).
- **Direct / Routed modes** — Direct: each agent talks straight to its
  provider. Routed: traffic flows through Nestra's local gateway for model
  rewriting, usage observation, and quota-aware failover.
- **Quota dashboard** — Real-time quota monitoring with keep-alive support for
  z.ai and MiniMax 5-hour windows.
- **Sessions** — Browse and search session history imported from each agent's
  local logs. Resume any session via a copied command.
- **Skills & MCP** — Scan, install, and manage skills per agent. Configure MCP
  servers with per-agent enable/disable control.
- **⌘K command palette** — Quick navigation across all surfaces.

## Supported platforms

**Windows** (v0.1.0). macOS and Linux support is planned.

## Install

Download the latest installer from the GitHub Releases page:

- **NSIS** (`.exe` setup) — recommended for most users.
- **MSI** — for enterprise / managed environments.

> **Fresh data directory required.** v0.1.0 is the first public release. If
> you used a pre-release build, back up and remove the old data directory
> (`%LOCALAPPDATA%\dev.nestra.app`) before launching.

## Supported agents

| Agent            | Binary         | Config file                                         |
| ---------------- | -------------- | --------------------------------------------------- |
| Claude Code      | `claude`       | `~/.claude/settings.json`                           |
| OpenCode Desktop | `OpenCode.exe` | `~/.config/opencode/opencode.json`                  |
| Pi               | `pi`           | `~/.pi/agent/models.json` + `~/.pi/agent/auth.json` |

## Privacy

Nestra is **local-only**. No cloud, no account, no telemetry. API keys are
encrypted at rest and never leave your machine. The local gateway binds to
`127.0.0.1` — traffic never goes off-device.

## Building from source

```bash
pnpm install            # frontend dependencies
pnpm tauri dev          # run in development mode
pnpm tauri build        # build release installer

pnpm typecheck          # frontend type checking
pnpm test               # frontend tests

# Rust tests (inside src-tauri/)
cargo test
```

Requires [Node.js](https://nodejs.org/) 18+, [Rust](https://rustup.rs/),
[pnpm](https://pnpm.io/), and the
[Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/).

## License

[MIT](LICENSE)
