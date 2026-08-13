# Nestra

![License](https://img.shields.io/badge/license-MIT-blue)
![Platform](https://img.shields.io/badge/platform-Windows-informational)
![Mode](https://img.shields.io/badge/local--only-127.0.0.1-success)

> A local-only desktop control center for AI coding agents — **Claude Code**,
> **OpenCode Desktop**, and **Pi**.

Nestra is one place to manage providers, bind agents, switch between direct and
routed traffic, watch quotas, and browse session history — without a cloud
account or telemetry. It's a Tauri 2 app (Rust backend + React 19 / TypeScript
frontend) that runs entirely on your machine. API keys are encrypted at rest
with AES-256-GCM, and the local gateway binds to `127.0.0.1` so traffic never
leaves the device.

**Status:** v0.1.0 — Windows. macOS and Linux are planned.

## Key features

- **Provider management** — Add API endpoints for Anthropic, OpenAI, OpenRouter,
  z.ai, MiniMax, and more, with presets or fully custom endpoints. Keys are
  validated on save and encrypted at rest (AES-256-GCM). Pick models and
  override their advertised abilities per endpoint.
- **Agent detection & binding** — Nestra auto-detects Claude Code, OpenCode
  Desktop, and Pi and binds each to a provider, writing the agent's config
  after backing up the original. Detect-on-launch plus on-demand re-detect,
  enable/disable, and config backup / restore / factory-reset.
- **Direct / Routed modes** — Per-agent switch. *Direct*: the agent talks
  straight to its provider. *Routed*: traffic flows through Nestra's local
  gateway for model rewriting, usage observation, quota-aware failover, and
  per-role routing.
- **Local gateway & role routing** — A gateway on `127.0.0.1:18777` resolves
  each request by agent and sub-agent role, retries transient failures, and
  migrates across providers on quota exhaustion. See
  [Agent role routing](#agent-role-routing).
- **Quota dashboard** — Real-time quota monitoring with keep-alive support for
  z.ai and MiniMax 5-hour windows, plus reactive detection of real 429/quota
  responses from any provider.
- **Sessions** — Browse and search session history imported from each agent's
  local logs, then resume any session via a copied command.
- **Skills & MCP** — Scan, install, and manage skills per agent. Configure MCP
  servers (stdio or HTTP) with per-agent enable/disable control.
- **Command palette & polish** — A ⌘K palette for fast navigation, light/dark
  themes, and a full English + 中文 interface.

## Supported agents

| Agent            | Binary         | Config file                                         |
| ---------------- | -------------- | --------------------------------------------------- |
| Claude Code      | `claude`       | `~/.claude/settings.json`                           |
| OpenCode Desktop | `OpenCode.exe` | `~/.config/opencode/opencode.json`                  |
| Pi               | `pi`           | `~/.pi/agent/models.json` + `~/.pi/agent/auth.json` |

## Direct vs Routed

Each agent runs in one of two modes (toggle on its detail page):

- **Direct** — Nestra writes the real provider straight into the agent's config.
  The agent connects to the provider itself; Nestra is only the configurator.
- **Routed** — Nestra writes a stable gateway alias instead. The agent sends
  traffic to the local gateway, which resolves the actual provider per request.
  This is what unlocks model rewriting, quota-aware failover, role routing, and
  usage observation.

A per-binding protocol picker controls which wire a Direct bind uses (e.g.
Anthropic Messages vs OpenAI Chat Completions on a dual-protocol endpoint like
OpenRouter).

## Agent role routing

In **Routed** mode, Nestra's gateway decides where each request goes based on
both the agent and the **role** of the request — the main thread or a named
sub-agent. Roles are detected conservatively from the request's system prompt
(never guessed) and expressed as stable policy keys:

| Role key          | Meaning                                            |
| ----------------- | -------------------------------------------------- |
| `main`            | The agent's main thread                            |
| `claude:<name>`   | A Claude Code sub-agent from `~/.claude/agents/`   |
| `pi:<role>`       | A Pi sub-agent (see [Pi subagents](#pi-subagents)) |
| `opencode:<name>` | An OpenCode agent block                            |

For each `(agent, role)` pair you can set a **routing policy**: a
preferred-endpoint chain, a fallback chain, an allowed-models glob whitelist, a
quota-migration toggle, a prompt-cache injection toggle, and an affinity scope.
A `role = "*"` catch-all covers any role without its own row, and a synthesized
default means the router always resolves.

When a request arrives, the gateway resolves its route through a fixed cascade:

1. **Explicit** — honor the task's pinned provider/model if set, healthy, and
   eligible.
2. **Affinity** — reuse the endpoint + model last used by this `task_id`,
   keeping a task on one provider so the prompt cache amortizes.
3. **Capability** — rank eligible endpoints by cost, latency, and cache
   locality.
4. **Fail closed** — if nothing is eligible, route nowhere rather than guess.

Health and quota gate every stage. On a quota or rate-limit hit, the migration
loop re-resolves a fallback while preserving the `task_id` — and never claims a
retry is a lossless continuation.

Configure policies on the per-agent **Routing policy** page (Routed mode only):
it lists the **detected roles** the gateway has actually seen in traffic, lets
you set preferred/fallback endpoints, allowed-model globs, affinity scope, and
the migration/cache toggles per role, and offers a resolve preview to dry-run a
decision.

## Pi subagents

Pi can spawn task-specialized sub-agents when the external
[`@tintinweb/pi-subagents`](https://github.com/tintinweb/pi-subagents) plugin is
installed — the Pi analog of Claude Code's `~/.claude/agents/*.md` subagents.
Nestra recognizes these sub-agents and routes each role independently, so you
can pin a role like `pi:researcher` to a different provider or model than the
main thread (see [Agent role routing](#agent-role-routing)).

- **Supported plugin:** [`@tintinweb/pi-subagents`](https://github.com/tintinweb/pi-subagents)
  (third-party, MIT). Install it through Pi's `packages` setting — e.g.
  `npm:@tintinweb/pi-subagents` — the same mechanism used for packages such as
  `npm:pi-lmstudio`. Nestra does not bundle or install it; it only reads the
  plugin's prompt marker.
- **How it's detected:** the plugin injects an `<active_agent name="<role>"/>`
  tag into the child session's system prompt (in both its *append* and
  *replace* prompt modes). Nestra's gateway reads that tag at request time,
  records the role under the agent's "detected roles", and exposes it as the
  routing-policy key `pi:<role>`. No extra configuration is needed beyond
  installing the plugin in Pi.

## Install

Download the latest installer from the GitHub Releases page:

- **NSIS** (`.exe` setup) — recommended for most users.
- **MSI** — for enterprise / managed environments.

> **Fresh data directory required.** v0.1.0 is the first public release. If you
> used a pre-release build, back up and remove the old data directory
> (`%LOCALAPPDATA%\dev.nestra.app`) before launching.

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

## Privacy

Nestra is **local-only**. No cloud, no account, no telemetry. API keys are
encrypted at rest and never leave your machine. The local gateway binds to
`127.0.0.1` — traffic never goes off-device.

## License

[MIT](LICENSE)
