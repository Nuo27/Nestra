# Nestra

![License](https://img.shields.io/badge/license-MIT-blue)
![Platform](https://img.shields.io/badge/platform-Windows-informational)
![Mode](https://img.shields.io/badge/local--only-127.0.0.1-success)

> A local-only desktop control center for AI coding agents — **Claude Code**,
> **OpenCode**, **Pi**, **ZCode**, and **Codex**.

![Nestra gateway](docs/screenshots/gateway.png)

- **Route everything** — a local gateway with per-role routing chains, quota-aware failover, and a model-grain circuit breaker.
- **Own your keys** — encrypted at rest (AES-256-GCM), no account, no telemetry, traffic never leaves `127.0.0.1`.
- **See everything** — sessions, quotas, live request logs, and per-model usage in one place.

Agents are auto-detected on launch, and every config write is backed up first.

## Install

Download from [GitHub Releases](https://github.com/Nuo27/Nestra/releases/latest):

- **NSIS** (`.exe` setup) — recommended for most users.
- **MSI** — for enterprise / managed environments.

Windows today; macOS and Linux are on the roadmap.

## Quickstart

1. **Launch** — Nestra detects installed agents automatically.
2. **Add a provider** — Providers → new: pick a preset, paste a key. Keys are validated on save.
3. **Bind & flip Routed** — on the agent's page, bind the provider and switch to
   **Routed**. Run your agent; requests land in Gateway → Activity with per-request
   route, model, and token usage.

<p>
  <img src="docs/screenshots/providers.png" width="32%" alt="Providers with quota bars">
  <img src="docs/screenshots/sessions.png" width="32%" alt="Sessions with context detail">
  <img src="docs/screenshots/routing.png" width="32%" alt="Per-role routing policy editor">
</p>

## Routing

Each agent runs in one of two modes (toggled on its page):

| Mode    | What Nestra writes     | What you get                                            |
| ------- | ---------------------- | ------------------------------------------------------- |
| Direct  | the real provider      | Nestra as configurator only                             |
| Routed  | a stable gateway alias | routing, failover, usage observation — per request      |

In Routed mode the gateway resolves every request through a fixed cascade:
**explicit** pin → **affinity** (keep a task on one provider so the prompt
cache amortizes) → **role policy** → fail closed rather than guess.

Role policies are keyed by agent *and* role — the main thread plus detected
sub-agent roles (`claude:researcher`, `pi:reviewer`, `opencode:agent`, …).
Each `(agent, role)` pair carries an ordered `(endpoint, model)` chain: the
first healthy entry serves; failures walk the list. A `*` catch-all covers
roles without their own row, and Claude Code's tier hints (`[haiku]`,
`[sonnet]`, `[opus]`) resolve through `tier:*` roles.

Resilience is honest: quota exhaustion migrates a task mid-stream to the next
target (same `task_id`, marked `generation_broken` — a retry is never claimed
as lossless continuation); a model-grain circuit breaker keeps healthy models
available when one model on an endpoint fails; agent disconnects finalize as
499 instead of dangling. Every request is observable in the live log viewer
(`/gateway/logs`), correlated by task id.

## Beyond routing

- **Quota** — 5-hour-window keep-alive for z.ai / MiniMax, reactive 429 detection, per-endpoint bars.
- **Sessions & handoff** — history imported from each agent, context-pressure estimates, one-click handoff artifacts that seed a fresh session.
- **Skills & MCP** — per-agent skill install/enable; MCP servers (stdio or HTTP) synced into agent configs.
- **Review runtime** — an isolated Pi reviewer routed via the `pi:reviewer` role, streaming live to a structured verdict.

## Building from source

```bash
pnpm install     # frontend dependencies
pnpm tauri dev   # run in development mode
pnpm tauri build # release installer

pnpm typecheck && pnpm test   # frontend checks
cd src-tauri && cargo test    # Rust suite
```

Requires [Node.js](https://nodejs.org/) 18+, [Rust](https://rustup.rs/),
[pnpm](https://pnpm.io/), and the
[Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/).

## Privacy

Nestra is local-only: no cloud, no account, no telemetry. API keys are
encrypted at rest and never leave your machine; the gateway binds to
`127.0.0.1` only.

## License

[MIT](LICENSE)
