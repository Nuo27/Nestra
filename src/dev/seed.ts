/// Dev-only example seed: `?seed=example`.
///
/// In a plain browser tab (vite dev without the Tauri shell) there is no
/// `window.__TAURI_INTERNALS__`, so every `invoke` rejects and all pages
/// render their error states. This module installs a mock internals object
/// whose `invoke` answers from a static fixture — the real UI renders with
/// believable fake data, which is what README screenshots and demos need
/// without touching a real machine's providers or session history.
///
/// Loaded from `main.tsx` only when `import.meta.env.DEV` AND the query
/// param is present, via dynamic import — production builds never include
/// this module. Unknown commands resolve `null` and are recorded on
/// `window.__seedMisses` so a missing fixture is easy to spot from devtools
/// (or a read-only `evaluate`) while iterating on screenshots.
///
/// All names, keys, and usage numbers are invented; nothing here is secret.

// Side-effect-only module (installs the mock on import) — the export keeps
// it an ES module for the bundler and type checker.
export {};

type Unknown = Record<string, unknown>;

const now = Date.now();
const mins = (n: number) => n * 60_000;
const hours = (n: number) => n * 3_600_000;

/// Intercepted `invoke` — keys are Tauri command names, values are either a
/// plain payload (returned as-is) or a function of the call args.
const handlers: Record<string, unknown> = {
  // ---- shell / events --------------------------------------------------
  "plugin:event|listen": 1,
  "plugin:event|unlisten": undefined,

  // ---- gateway ---------------------------------------------------------
  gateway_get_status: {
    state: "running",
    enabled: true,
    configured_port: 18777,
    bound_base_url: "http://127.0.0.1:18777",
    started_at: now - hours(2.4),
    uptime_secs: 8640,
    last_error: null,
    agents_enabled: ["claude-code-cli", "opencode-desktop", "zcode-desktop"],
    has_token: true,
    stats: { total_requests: 148, last_request_at: now - mins(3), active_tasks: 2 },
  },
  gateway_recent_activity: recentActivity(),
  orch_status: {
    up: true,
    base_url: "http://127.0.0.1:18777",
    agents_enabled: ["claude-code-cli", "opencode-desktop", "zcode-desktop"],
  },

  // ---- providers -------------------------------------------------------
  endpoint_list: endpoints(),
  provider_health_snapshot: [
    {
      endpoint_id: "openrouter",
      model: "moonshotai/kimi-k2",
      state: "open",
      consecutive_failures: 5,
      last_failure: "503 upstream unavailable",
      recovery_in_ms: 42_000,
    },
  ],
  orch_usage_summary: usageRows(),

  // ---- quota -----------------------------------------------------------
  quota_refresh_get_settings: {
    endpoints: {
      "zai-glm": { enabled: true, protocol: "anthropic", model: null, target_quota_name: "5h", last_status: "ok", check_rate_secs: 900, reset_grace_secs: 180, extractor: null, query_plan: { source: "preset", kind: "zai" }, provisioned: true, preview_windows: ["5h", "weekly"] },
      openrouter: { enabled: false, protocol: "openai", model: null, target_quota_name: null, last_status: "ok", check_rate_secs: 900, reset_grace_secs: 180, extractor: null, query_plan: { source: "preset", kind: "openrouter" }, provisioned: true, preview_windows: null },
    },
  },
  endpoint_fetch_quota: ({ id }: { id: string }) => {
    const table: Record<string, Unknown> = {
      "zai-glm": {
        ok: true, plan: "preset:zai", error: null,
        items: [
          { name: "5h", pct: 62, used: 620, total: 1000, remaining: 380, resets_in: "2h 41m", resets_at_ms: now + mins(161), unit: null, is_balance: false },
          { name: "weekly", pct: 34, used: 8_421, total: 25_000, remaining: 16_579, resets_in: "3d 4h", resets_at_ms: now + hours(76), unit: null, is_balance: false },
        ],
      },
      openrouter: {
        ok: true, plan: "preset:openrouter", error: null,
        items: [
          { name: "credits", pct: 84, used: 15.82, total: 25, remaining: 9.18, resets_in: null, resets_at_ms: null, unit: "USD", is_balance: true },
        ],
      },
    };
    return table[id] ?? { ok: false, plan: null, error: "no plan", items: [] };
  },

  // ---- agents ----------------------------------------------------------
  agent_list: agents(),
  setting_get: ({ key }: { key: string }) => {
    if (key.startsWith("orchestration.gateway.")) {
      return ["claude-code-cli", "opencode-desktop", "zcode-desktop"].includes(
        key.slice("orchestration.gateway.".length),
      );
    }
    return null;
  },

  // ---- routing policy --------------------------------------------------
  routing_policy_list: () =>
    policies().map((p) => ({
      ...p,
      route_targets: JSON.stringify(p.route_targets),
    })),
  orch_model_catalog: modelCatalog(),
  orch_detected_roles: [
    { role: "claude:researcher", request_count: 24, last_seen: now - mins(6) },
    { role: "claude:reviewer", request_count: 5, last_seen: now - hours(1.2) },
  ],
  orch_resolve_preview: {
    endpoint_id: "zai-glm",
    model: "glm-4.7",
    reason: "policy",
    cache_strategy: "anthropicexplicit",
    requested_model: null,
    requested_provider: null,
    context_window: 200_000,
  },

  // ---- sessions --------------------------------------------------------
  session_list: sessions(),
  session_get: ({ id }: { id: string }) =>
    sessions().find((s) => s.id === id) ?? null,
  session_read: ({ id }: { id: string }) => {
    const win = {
      "s-gateway-retry": 12,
      "s-claude-handoff": 8,
    }[id as string] ?? 0;
    return { messages: win ? messages(id as string).slice(0, win) : [], total: win };
  },
  session_children: ({ parentId }: { parentId: string }) =>
    parentId === "s-gateway-retry"
      ? sessions().filter((s) => s.is_subagent)
      : [],
  session_context_pressure: {
    est_tokens: 94_200,
    pct: 47,
    top_consumer: "tool: Read (38 files)",
  },
  handoff_list: [
    {
      id: "h-1",
      created_at: now - hours(20),
      token_snapshot: 128_400,
      artifact_path: "C:\\repo\\.nestra\\handoffs\\h-1.md",
      target_session_id: null,
    },
  ],

  // ---- palette ---------------------------------------------------------
  palette_search: [],
};

// ---- fixtures --------------------------------------------------------------

function endpoints() {
  return [
    {
      id: "zai-glm",
      display_name: "Z.ai GLM",
      has_api_key: true,
      status: "valid",
      models: {
        haiku: "glm-4.7-air",
        sonnet: "glm-4.7",
        opus: "glm-4.7x",
        default: "glm-4.7",
        available: ["glm-4.7", "glm-4.7-air", "glm-4.7x", "glm-4.6"],
      },
      advanced_env: null,
      model_abilities: {},
      model_abilities_defaults: {
        "glm-4.7": { api: "anthropic", context_window: 200_000, max_output: 32_768, reasoning: true, tool_call: true, attachment: true, temperature: true },
      },
      last_validated_at: now - hours(5),
      models_fetched_at: now - hours(26),
      protocols: [{ protocol: "anthropic", base_url: "https://api.z.ai/api/anthropic" }],
    },
    {
      id: "openrouter",
      display_name: "OpenRouter",
      has_api_key: true,
      status: "valid",
      models: {
        default: "anthropic/claude-sonnet-4.5",
        available: ["anthropic/claude-sonnet-4.5", "moonshotai/kimi-k2", "qwen/qwen3-max"],
      },
      advanced_env: null,
      model_abilities: {},
      model_abilities_defaults: {},
      last_validated_at: now - hours(9),
      models_fetched_at: now - hours(30),
      protocols: [
        { protocol: "anthropic", base_url: "https://openrouter.ai/api/v1" },
        { protocol: "openai", base_url: "https://openrouter.ai/api/v1" },
      ],
    },
    {
      id: "moonshot-kimi",
      display_name: "Moonshot Kimi",
      has_api_key: true,
      status: "unvalidated",
      models: { default: "kimi-k2-0905-preview", available: ["kimi-k2-0905-preview"] },
      advanced_env: null,
      model_abilities: {},
      model_abilities_defaults: {},
      last_validated_at: null,
      models_fetched_at: null,
      protocols: [{ protocol: "openai", base_url: "https://api.moonshot.cn/v1" }],
    },
  ];
}

function capability(over: Partial<Record<string, boolean>> = {}) {
  return {
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
    ...over,
  };
}

function agents() {
  const base = {
    agent_path: "C:\\bin\\agent.exe",
    installed_version: "1.4.2",
    source: "auto",
    active_provider_id: null,
    has_backup: true,
    agent_path_override: null,
    config_path_override: null,
    config_path: "C:\\Users\\dev\\.agent\\config.json",
    enabled: true,
    manageable: true,
    has_factory: false,
    status_detail: null,
  };
  return [
    { ...base, id: "claude-code-cli", kind: "cli", display_name: "Claude Code CLI", status: "ok", active_provider_id: "zai-glm", capability: capability(), supported_protocols: ["anthropic", "openai"], model_selection: "anthropic_tiers", providers: [] },
    { ...base, id: "opencode-desktop", kind: "desktop", display_name: "OpenCode Desktop", status: "ok", active_provider_id: "openrouter", capability: capability({ supports_mcp_enabled: true }), supported_protocols: ["anthropic", "openai"], model_selection: "free_form", providers: [] },
    { ...base, id: "pi-cli", kind: "cli", display_name: "Pi CLI", status: "ok", active_provider_id: "zai-glm", capability: capability({ supports_mcp: false, supports_mcp_enabled: false }), supported_protocols: ["anthropic"], model_selection: "free_form", providers: [] },
    { ...base, id: "zcode-desktop", kind: "desktop", display_name: "ZCode Desktop", status: "ok", active_provider_id: "zai-glm", capability: capability({ supports_mcp_enabled: true }), supported_protocols: ["anthropic", "openai"], model_selection: "free_form", providers: [] },
    { ...base, id: "codex-desktop", kind: "desktop", display_name: "Codex Desktop", status: "ok", active_provider_id: "openrouter", capability: capability(), supported_protocols: ["openai", "responses"], model_selection: "free_form", providers: [] },
  ];
}

function policies() {
  return [
    { agent_id: "claude-code-cli", role: "main", route_targets: [{ endpoint: "zai-glm", model: "glm-4.7" }, { endpoint: "openrouter", model: "anthropic/claude-sonnet-4.5" }], migrate_on_quota: true, inject_cache_control: true, affinity_scope: "task", updated_at: now - hours(3) },
    { agent_id: "claude-code-cli", role: "claude:researcher", route_targets: [{ endpoint: "openrouter", model: "moonshotai/kimi-k2" }, { endpoint: "zai-glm", model: "glm-4.7-air" }], migrate_on_quota: true, inject_cache_control: false, affinity_scope: "task", updated_at: now - hours(26) },
    { agent_id: "claude-code-cli", role: "tier:haiku", route_targets: [{ endpoint: "zai-glm", model: "glm-4.7-air" }], migrate_on_quota: true, inject_cache_control: true, affinity_scope: "task", updated_at: now - hours(50) },
    { agent_id: "claude-code-cli", role: "*", route_targets: [{ endpoint: "zai-glm", model: "glm-4.7" }], migrate_on_quota: true, inject_cache_control: true, affinity_scope: "task", updated_at: now - hours(50) },
  ];
}

function modelCatalog() {
  return [
    { endpoint_id: "zai-glm", model_id: "glm-4.7", abilities: { api: "anthropic", context_window: 200_000, max_output: 32_768, reasoning: true, tool_call: true, attachment: true, temperature: true } },
    { endpoint_id: "zai-glm", model_id: "glm-4.7-air", abilities: { api: "anthropic", context_window: 128_000, max_output: 16_384, reasoning: true, tool_call: true, attachment: false, temperature: true } },
    { endpoint_id: "zai-glm", model_id: "glm-4.7x", abilities: { api: "anthropic", context_window: 200_000, max_output: 65_536, reasoning: true, tool_call: true, attachment: true, temperature: true } },
    { endpoint_id: "openrouter", model_id: "anthropic/claude-sonnet-4.5", abilities: { api: "anthropic", context_window: 200_000, max_output: 32_768, reasoning: true, tool_call: true, attachment: true, temperature: true } },
    { endpoint_id: "openrouter", model_id: "moonshotai/kimi-k2", abilities: { api: "openai-comp", context_window: 256_000, max_output: 16_384, reasoning: true, tool_call: true, attachment: false, temperature: true } },
    { endpoint_id: "moonshot-kimi", model_id: "kimi-k2-0905-preview", abilities: { api: "openai-comp", context_window: 256_000, max_output: 16_384, reasoning: true, tool_call: true, attachment: false, temperature: true } },
  ];
}

function sessions() {
  const mk = (s: Partial<Unknown> & { id: string; title: string }) => ({
    provider: "claude-code-cli",
    summary: "",
    project: "Nestra",
    cwd: "C:\\dev\\nestra",
    started_at: now - hours(6),
    updated_at: now - mins(12),
    ended_at: null,
    message_count: 42,
    source_path: "C:\\Users\\dev\\.claude\\projects\\nestra\\session.jsonl",
    parent_session_id: null,
    agent_id: "claude-code-cli",
    is_subagent: false,
    resume_command: "claude --resume " + s.id,
    child_count: 0,
    source_files: [],
    provider_metadata_json: "{}",
    ...s,
  });
  return [
    mk({ id: "s-gateway-retry", title: "harden the gateway retry ladder", message_count: 87, updated_at: now - mins(6), child_count: 2 }),
    mk({ id: "s-claude-handoff", title: "context handoff to fresh session", message_count: 64, updated_at: now - hours(1.4) }),
    mk({ id: "s-quota-failover", title: "quota failover across providers", message_count: 31, updated_at: now - hours(3) }),
    mk({ id: "s-reviewer-loop", title: "wire the pi reviewer verdict loop", message_count: 58, updated_at: now - hours(5) }),
    { ...mk({ id: "s-sub-researcher", title: "researcher · upstream 5xx taxonomy", is_subagent: true, parent_session_id: "s-gateway-retry", message_count: 22, provider: "claude-code-cli", updated_at: now - mins(31) }) },
    { ...mk({ id: "s-sub-planner", title: "planner · migration step order", is_subagent: true, parent_session_id: "s-gateway-retry", message_count: 14, provider: "claude-code-cli", updated_at: now - mins(52) }) },
    mk({ id: "s-pi-knowledge", title: "distill durable notes into knowledge files", provider: "pi-cli", agent_id: "pi-cli", message_count: 36, updated_at: now - hours(8) }),
    mk({ id: "s-opencode-ui", title: "opencode keybinding pass", provider: "opencode-desktop", agent_id: "opencode-desktop", message_count: 19, updated_at: now - hours(22) }),
    mk({ id: "s-codex-bridge", title: "responses bridge smoke test", provider: "codex-desktop", agent_id: "codex-desktop", message_count: 12, updated_at: now - hours(30) }),
  ];
}

function messages(sessionId: string) {
  if (sessionId === "s-claude-handoff") {
    return [
      { seq: 0, role: "user", content_text: "Summarize where we are — I want a handoff for a fresh session.", tool_name: null, tool_input: null, tool_output: null, tool_call_id: null, thinking: null, parent_message_id: null, message_id: "m-0", timestamp: now - mins(90), provider_metadata_json: "{}" },
      { seq: 1, role: "assistant", content_text: "Goal landed: the gateway retry ladder now classifies failures before migrating. Open question: whether Timeout should count against the per-request deadline once a response has started.", tool_name: null, tool_input: null, tool_output: null, tool_call_id: null, thinking: "The deadline semantics differ pre/post first byte — worth calling out in the handoff.", parent_message_id: null, message_id: "m-1", timestamp: now - mins(89), provider_metadata_json: "{}" },
    ];
  }
  return [
    { seq: 0, role: "user", content_text: "The routed ox-alpha endpoint 503s after ~60s. Find the root cause.", tool_name: null, tool_input: null, tool_output: null, tool_call_id: null, thinking: null, parent_message_id: null, message_id: "m-0", timestamp: now - mins(120), provider_metadata_json: "{}" },
    { seq: 1, role: "assistant", content_text: "I'll trace one request through the gateway logs first.", tool_name: null, tool_input: null, tool_output: null, tool_call_id: null, thinking: "Chunked upload bodies are the suspect — opencode-go's edge rejects them.", parent_message_id: null, message_id: "m-1", timestamp: now - mins(119), provider_metadata_json: "{}" },
    { seq: 2, role: "assistant", content_text: "", tool_name: "Read", tool_input: "{ \"path\": \"src-tauri/src/orchestration/gateway/forward.rs\" }", tool_output: null, tool_call_id: "t-1", thinking: null, parent_message_id: "m-1", message_id: "m-2", timestamp: now - mins(118), provider_metadata_json: "{}" },
    { seq: 3, role: "tool", content_text: "", tool_name: "Read", tool_input: null, tool_output: "112 lines · GatewayBody::size_hint present — content-length is set on every upstream request.", tool_call_id: "t-1", thinking: null, parent_message_id: null, message_id: "m-3", timestamp: now - mins(117), provider_metadata_json: "{}" },
  ];
}

function recentActivity() {
  const row = (over: Partial<Unknown>) => ({
    request_id: "r-" + Math.random().toString(36).slice(2, 8),
    task_id: "t-main",
    agent_id: "claude-code-cli",
    logical_session: "s-gateway-retry",
    subagent_role: null,
    role_source: "native",
    requested_model: "glm-4.7 [1m]",
    requested_provider: null,
    resolved_endpoint_id: "zai-glm",
    resolved_model: "glm-4.7",
    protocol: "anthropic",
    route_reason: "policy",
    http_status: 200,
    usage_input: 18_421,
    usage_output: 2_204,
    cache_creation: 1_902,
    cache_read: 96_411,
    tool_calls: 3,
    tool_names: '{"Read":2,"Grep":1}',
    generation_broken: false,
    started_at: now - mins(3),
    ended_at: now - mins(3) + 8_400,
    ...over,
  });
  return [
    row({}),
    row({ subagent_role: "claude:researcher", resolved_endpoint_id: "openrouter", resolved_model: "moonshotai/kimi-k2", protocol: "openai", usage_input: 6_120, usage_output: 1_780, started_at: now - mins(9), ended_at: now - mins(9) + 15_200 }),
    row({ agent_id: "opencode-desktop", logical_session: "s-opencode-ui", usage_input: 9_844, usage_output: 902, started_at: now - mins(24), ended_at: now - mins(24) + 6_100 }),
    row({ http_status: 429, resolved_endpoint_id: "zai-glm", route_reason: "policy", generation_broken: false, usage_input: 420, usage_output: null, started_at: now - mins(51), ended_at: now - mins(51) + 900 }),
    row({ subagent_role: "claude:reviewer", resolved_endpoint_id: "zai-glm", resolved_model: "glm-4.7x", usage_input: 31_002, usage_output: 6_441, started_at: now - hours(1.2), ended_at: now - hours(1.2) + 22_000 }),
    row({ agent_id: "zcode-desktop", logical_session: null, http_status: 200, usage_input: 4_006, usage_output: 610, started_at: now - hours(2.1), ended_at: now - hours(2.1) + 4_400 }),
  ];
}

function usageRows() {
  const day = (d: number) => new Date(now - d * 86_400_000).toISOString().slice(0, 10);
  return [
    { day: day(0), agent_id: "claude-code-cli", endpoint_id: "zai-glm", model_id: "glm-4.7", requests: 96, usage_input: 1_204_000, usage_output: 148_220, cache_creation: 82_100, cache_read: 3_902_000, cost_usd: null },
    { day: day(0), agent_id: "claude-code-cli", endpoint_id: "openrouter", model_id: "moonshotai/kimi-k2", requests: 24, usage_input: 210_400, usage_output: 61_780, cache_creation: 0, cache_read: 0, cost_usd: 1.94 },
    { day: day(1), agent_id: "claude-code-cli", endpoint_id: "zai-glm", model_id: "glm-4.7", requests: 141, usage_input: 1_880_000, usage_output: 190_100, cache_creation: 96_400, cache_read: 4_410_000, cost_usd: null },
    { day: day(1), agent_id: "opencode-desktop", endpoint_id: "openrouter", model_id: "anthropic/claude-sonnet-4.5", requests: 18, usage_input: 302_000, usage_output: 44_800, cache_creation: 12_000, cache_read: 640_000, cost_usd: 4.71 },
    { day: day(2), agent_id: "pi-cli", endpoint_id: "zai-glm", model_id: "glm-4.7x", requests: 9, usage_input: 402_000, usage_output: 88_200, cache_creation: 0, cache_read: 0, cost_usd: null },
  ];
}

// ---- install ----------------------------------------------------------------

type Internals = {
  invoke: (cmd: string, args?: Unknown, options?: Unknown) => Promise<unknown>;
  transformCallback: (cb: (...a: unknown[]) => void, once?: boolean) => number;
  removeCallback: (id: number) => void;
};

const w = window as unknown as { __TAURI_INTERNALS__?: Internals; __seedMisses?: string[] };

w.__seedMisses = [];

w.__TAURI_INTERNALS__ = {
  async invoke(cmd: string, args?: Unknown) {
    const h = handlers[cmd];
    if (h === undefined) {
      w.__seedMisses!.push(cmd + (args?.id != null ? `(${String(args.id)})` : ""));
      return null;
    }
    return typeof h === "function" ? (h as (a: Unknown) => unknown)(args ?? {}) : h;
  },
  transformCallback(cb: (...a: unknown[]) => void, once = false) {
    const id = Math.floor(Math.random() * 2 ** 31);
    if (!once) return id; // persistent callbacks are never invoked by the seed
    cb();
    return id;
  },
  removeCallback() {
    /* noop */
  },
};

console.info("[seed] example fixtures installed —", location.href);
