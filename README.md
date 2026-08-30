# Nestra

![License](https://img.shields.io/badge/license-MIT-blue)
![Platform](https://img.shields.io/badge/platform-Windows-informational)

**Nestra** is a local gateway and control plane for coding agents.

I built it because I was tired of manually switching between providers, models, and agent configurations. Nestra gives you one place to manage providers, API keys, models, agents, skills, MCP servers, sessions, and **routing**.

Use it as a simple provider manager, or enable the gateway for **automatic routing and failover**. Sub-agents can also have their own **role-based routing policies**.

<p align="center">
  <img src="docs/screenshots/hero.png" width="720" alt="Nestra overview dashboard">
</p>

## Install

Download the latest installer from [GitHub Releases](https://github.com/Nuo27/Nestra/releases/latest).

> **Platform:** Windows only for now.

## Get started

1. **Launch Nestra** — supported coding agents are detected automatically.
2. **Add a provider** — choose a preset, enter your API key, and select a model.
3. **Bind it to an agent** — open the agent page and choose **Direct** or **Routed** mode.
4. **Run your agent** — in Routed mode, requests flow through the local gateway and appear in **Activity** with their route, model, and token usage.

## Supported agents

| Agent       | Kind    |
| ----------- | ------- |
| Claude Code | CLI     |
| OpenCode    | Desktop |
| Pi agent    | CLI     |
| ZCode       | Desktop |
| Codex       | Desktop |

> I switched my main vibing tool to the Pi agent — it requires a community package like [pi-subagents](https://pi.dev/packages/@tintinweb/pi-subagents) for role policies to work.

## Routing

Each agent can run in one of two modes:

- **Direct** — Nestra writes the selected provider directly into the agent's configuration.
- **Routed** — the agent uses a stable local gateway endpoint at `127.0.0.1:18777`.

In Routed mode, requests are resolved through an ordered cascade — explicit pin, task affinity, role policy, fail closed. Role policies support ordered `(endpoint, model)` chains for each `(agent, role)`. When a provider hits a quota limit or a model fails, Nestra can move to the next configured target instead of interrupting the task.

## Features

- **Multi-provider management** — providers, API keys, models, and endpoints in one place
- **Gateway routing** — stable local endpoint with policy-based routing and failover
- **Role-based policies** — configure different provider/model chains for sub-agents and roles
- **Live activity logs** — inspect requests, routes, models, tokens, and task links
- **Session history** — context-pressure estimates and handoff artifacts
- **Skills & MCP sync** — manage per-agent skills and MCP configurations

## Build from source

```bash
pnpm install

# Development
pnpm tauri dev

# Release build
pnpm tauri build
```

Requires:

- Node.js 18+
- Rust
- pnpm
- [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/)

## Privacy

Nestra is designed to run locally.

- API keys are encrypted at rest using **AES-256-GCM**
- No telemetry
- No cloud backend required
- The local gateway binds to `127.0.0.1` only

## Notes

This is a personal tool for my own use, a work in progress, and might include bugs. Use at your own risk.

## License

[MIT](LICENSE)
