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

/// The demo cast: two agents (pi-cli, zcode-desktop) across three providers
/// (Anthropic, DeepSeek, Z.ai GLM) — the same identities the README
/// screenshots show, so captures and docs never drift apart.
const DEMO_AGENTS = ["pi-cli", "zcode-desktop"];

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
    agents_enabled: DEMO_AGENTS,
    has_token: true,
    stats: { total_requests: 148, last_request_at: now - mins(3), active_tasks: 2 },
  },
  gateway_recent_activity: recentActivity(),
  orch_status: {
    up: true,
    base_url: "http://127.0.0.1:18777",
    agents_enabled: DEMO_AGENTS,
  },

  // ---- providers -------------------------------------------------------
  endpoint_list: endpoints(),
  provider_health_snapshot: [
    {
      endpoint_id: "deepseek",
      model: "deepseek-v4-flash",
      state: "open",
      consecutive_failures: 5,
      last_failure: "503 upstream overloaded",
      recovery_in_ms: 42_000,
    },
  ],
  orch_usage_summary: usageRows(),

  // ---- quota -----------------------------------------------------------
  quota_refresh_get_settings: {
    endpoints: {
      "zai-glm": { enabled: true, protocol: "anthropic", model: null, target_quota_name: "5h", last_status: "ok", check_rate_secs: 900, reset_grace_secs: 180, extractor: null, query_plan: { source: "preset", kind: "zai" }, provisioned: true, preview_windows: ["5h", "weekly"] },
    },
  },
  endpoint_fetch_quota: ({ id }: { id: string }) => {
    if (id !== "zai-glm") return { ok: false, plan: null, error: "no plan", items: [] };
    return {
      ok: true,
      plan: "preset:zai",
      error: null,
      items: [
        { name: "5h", pct: 62, used: 620, total: 1000, remaining: 380, resets_in: "2h 41m", resets_at_ms: now + mins(161), unit: null, is_balance: false },
        { name: "weekly", pct: 34, used: 8_421, total: 25_000, remaining: 16_579, resets_in: "3d 4h", resets_at_ms: now + hours(76), unit: null, is_balance: false },
      ],
    };
  },

  // ---- agents ----------------------------------------------------------
  agent_list: agents(),
  setting_get: ({ key }: { key: string }) =>
    key.startsWith("orchestration.gateway.")
      ? DEMO_AGENTS.includes(key.slice("orchestration.gateway.".length))
      : null,

  // ---- routing policy --------------------------------------------------
  routing_policy_list: () =>
    policies().map((p) => ({
      ...p,
      route_targets: JSON.stringify(p.route_targets),
    })),
  orch_model_catalog: modelCatalog(),
  orch_detected_roles: [
    { role: "pi:reviewer", request_count: 18, last_seen: now - mins(6) },
    { role: "pi:researcher", request_count: 7, last_seen: now - hours(1.2) },
  ],
  orch_resolve_preview: {
    endpoint_id: "anthropic",
    model: "claude-opus-4.6",
    reason: "policy",
    cache_strategy: "anthropicexplicit",
    requested_model: null,
    requested_provider: null,
    context_window: 200_000,
  },

  // ---- sessions --------------------------------------------------------
  session_list: sessions(),
  // Task lineage on the session detail (SessionLineage): two observed tasks
  // for the demo parent session.
  orch_session_tasks: ({ logicalSession }: { logicalSession: string }) =>
    logicalSession === "s-pi-retry"
      ? [
          { task_id: "t-main", agent_id: "pi-cli", logical_session: "s-pi-retry", request_count: 34, latest_status: 200, generation_broken: false, first_seen: now - hours(6), last_seen: now - mins(3) },
          { task_id: "t-review", agent_id: "pi-cli", logical_session: "s-pi-retry", request_count: 12, latest_status: 200, generation_broken: false, first_seen: now - hours(2), last_seen: now - mins(9) },
        ]
      : [],
  session_get: ({ id }: { id: string }) =>
    sessions().find((s) => s.id === id) ?? null,
  session_read: ({ id }: { id: string }) => {
    const win = {
      "s-pi-handoff": 8,
      "s-pi-retry": 12,
    }[id as string] ?? 0;
    return { messages: win ? messages(id as string).slice(0, win) : [], total: win };
  },
  session_children: ({ parentId }: { parentId: string }) =>
    parentId === "s-pi-retry" ? sessions().filter((s) => s.is_subagent) : [],
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
      id: "anthropic",
      display_name: "Anthropic",
      has_api_key: true,
      status: "valid",
      models: {
        default: "claude-opus-4.6",
        available: ["claude-opus-4.6", "claude-sonnet-4.5", "claude-haiku-4.5"],
      },
      advanced_env: null,
      model_abilities: {},
      model_abilities_defaults: {
        "claude-opus-4.6": { api: "anthropic", context_window: 200_000, max_output: 32_768, reasoning: true, tool_call: true, attachment: true, temperature: true },
        "claude-sonnet-4.5": { api: "anthropic", context_window: 200_000, max_output: 64_000, reasoning: true, tool_call: true, attachment: true, temperature: true },
      },
      last_validated_at: now - hours(5),
      models_fetched_at: null,
      protocols: [{ protocol: "anthropic", base_url: "https://api.anthropic.com" }],
    },
    {
      id: "deepseek",
      display_name: "DeepSeek",
      has_api_key: true,
      status: "valid",
      models: { default: "deepseek-v4-flash", available: ["deepseek-v4-flash"] },
      advanced_env: null,
      model_abilities: {},
      model_abilities_defaults: {
        "deepseek-v4-flash": { api: "openai-comp", context_window: 128_000, max_output: 16_384, reasoning: true, tool_call: true, attachment: false, temperature: true },
      },
      last_validated_at: now - hours(9),
      models_fetched_at: null,
      protocols: [{ protocol: "openai", base_url: "https://api.deepseek.com/v1" }],
    },
    {
      id: "zai-glm",
      display_name: "Z.ai GLM",
      has_api_key: true,
      status: "valid",
      models: { default: "glm-5.3", available: ["glm-5.3", "glm-5.3-air"] },
      advanced_env: null,
      model_abilities: {},
      model_abilities_defaults: {
        "glm-5.3": { api: "anthropic", context_window: 200_000, max_output: 32_768, reasoning: true, tool_call: true, attachment: true, temperature: true },
      },
      last_validated_at: now - hours(3),
      models_fetched_at: now - hours(26),
      protocols: [{ protocol: "anthropic", base_url: "https://api.z.ai/api/anthropic" }],
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
    { ...base, id: "pi-cli", kind: "cli", display_name: "Pi CLI", status: "ok", active_provider_id: "anthropic", capability: capability({ supports_mcp: false, supports_mcp_enabled: false }), supported_protocols: ["anthropic"], model_selection: "free_form", providers: [] },
    { ...base, id: "zcode-desktop", kind: "desktop", display_name: "ZCode Desktop", status: "ok", active_provider_id: "zai-glm", capability: capability({ supports_mcp_enabled: true }), supported_protocols: ["anthropic", "openai"], model_selection: "free_form", providers: [] },
  ];
}

function policies() {
  return [
    { agent_id: "pi-cli", role: "main", route_targets: [{ endpoint: "anthropic", model: "claude-opus-4.6" }, { endpoint: "zai-glm", model: "glm-5.3" }], migrate_on_quota: true, inject_cache_control: true, affinity_scope: "task", updated_at: now - hours(3) },
    { agent_id: "pi-cli", role: "pi:reviewer", route_targets: [{ endpoint: "deepseek", model: "deepseek-v4-flash" }, { endpoint: "anthropic", model: "claude-sonnet-4.5" }], migrate_on_quota: true, inject_cache_control: false, affinity_scope: "task", updated_at: now - hours(26) },
    { agent_id: "pi-cli", role: "*", route_targets: [{ endpoint: "zai-glm", model: "glm-5.3" }], migrate_on_quota: true, inject_cache_control: true, affinity_scope: "task", updated_at: now - hours(50) },
  ];
}

function modelCatalog() {
  return [
    { endpoint_id: "anthropic", model_id: "claude-opus-4.6", abilities: { api: "anthropic", context_window: 200_000, max_output: 32_768, reasoning: true, tool_call: true, attachment: true, temperature: true } },
    { endpoint_id: "anthropic", model_id: "claude-sonnet-4.5", abilities: { api: "anthropic", context_window: 200_000, max_output: 64_000, reasoning: true, tool_call: true, attachment: true, temperature: true } },
    { endpoint_id: "deepseek", model_id: "deepseek-v4-flash", abilities: { api: "openai-comp", context_window: 128_000, max_output: 16_384, reasoning: true, tool_call: true, attachment: false, temperature: true } },
    { endpoint_id: "zai-glm", model_id: "glm-5.3", abilities: { api: "anthropic", context_window: 200_000, max_output: 32_768, reasoning: true, tool_call: true, attachment: true, temperature: true } },
    { endpoint_id: "zai-glm", model_id: "glm-5.3-air", abilities: { api: "anthropic", context_window: 128_000, max_output: 16_384, reasoning: true, tool_call: true, attachment: false, temperature: true } },
  ];
}

function sessions() {
  const mk = (s: Partial<Unknown> & { id: string; title: string }) => ({
    provider: "pi-cli",
    summary: "",
    project: "Nestra",
    cwd: "C:\\dev\\nestra",
    started_at: now - hours(6),
    updated_at: now - mins(12),
    ended_at: null,
    message_count: 42,
    source_path: "C:\\Users\\dev\\.pi\\sessions\\session.jsonl",
    parent_session_id: null,
    agent_id: "pi-cli",
    is_subagent: false,
    resume_command: "pi --resume " + s.id,
    child_count: 0,
    source_files: [],
    provider_metadata_json: "{}",
    ...s,
  });
  return [
    mk({ id: "s-pi-retry", title: "harden the gateway retry ladder", message_count: 87, updated_at: now - mins(6), child_count: 2 }),
    mk({ id: "s-pi-handoff", title: "context handoff to fresh session", message_count: 64, updated_at: now - hours(1.4) }),
    mk({ id: "s-pi-review", title: "wire the reviewer verdict loop", message_count: 58, updated_at: now - hours(5) }),
    { ...mk({ id: "s-sub-reviewer", title: "reviewer · verdict schema pass", is_subagent: true, parent_session_id: "s-pi-retry", message_count: 22, updated_at: now - mins(31) }) },
    { ...mk({ id: "s-sub-researcher", title: "researcher · upstream 5xx taxonomy", is_subagent: true, parent_session_id: "s-pi-retry", message_count: 14, updated_at: now - mins(52) }) },
    mk({ id: "s-pi-knowledge", title: "distill durable notes into knowledge files", message_count: 36, updated_at: now - hours(8) }),
    mk({ id: "s-zcode-ui", title: "keybinding + palette pass", provider: "zcode-desktop", agent_id: "zcode-desktop", message_count: 19, updated_at: now - hours(2.2), resume_command: "" }),
    mk({ id: "s-zcode-bridge", title: "responses bridge smoke test", provider: "zcode-desktop", agent_id: "zcode-desktop", message_count: 12, updated_at: now - hours(30), resume_command: "" }),
  ];
}

function messages(sessionId: string) {
  if (sessionId === "s-pi-handoff") {
    return [
      { seq: 0, role: "user", content_text: "Summarize where we are — I want a handoff for a fresh session.", tool_name: null, tool_input: null, tool_output: null, tool_call_id: null, thinking: null, parent_message_id: null, message_id: "m-0", timestamp: now - mins(90), provider_metadata_json: "{}" },
      { seq: 1, role: "assistant", content_text: "Goal landed: the gateway retry ladder now classifies failures before migrating. Open question: whether Timeout should count against the per-request deadline once a response has started.", tool_name: null, tool_input: null, tool_output: null, tool_call_id: null, thinking: "The deadline semantics differ pre/post first byte — worth calling out in the handoff.", parent_message_id: null, message_id: "m-1", timestamp: now - mins(89), provider_metadata_json: "{}" },
    ];
  }
  return [
    { seq: 0, role: "user", content_text: "The routed deepseek endpoint 503s after ~60s. Find the root cause.", tool_name: null, tool_input: null, tool_output: null, tool_call_id: null, thinking: null, parent_message_id: null, message_id: "m-0", timestamp: now - mins(120), provider_metadata_json: "{}" },
    { seq: 1, role: "assistant", content_text: "I'll trace one request through the gateway logs first.", tool_name: null, tool_input: null, tool_output: null, tool_call_id: null, thinking: "Chunked upload bodies are the suspect — the edge rejects them.", parent_message_id: null, message_id: "m-1", timestamp: now - mins(119), provider_metadata_json: "{}" },
    { seq: 2, role: "assistant", content_text: "", tool_name: "Read", tool_input: "{ \"path\": \"src-tauri/src/orchestration/gateway/forward.rs\" }", tool_output: null, tool_call_id: "t-1", thinking: null, parent_message_id: "m-1", message_id: "m-2", timestamp: now - mins(118), provider_metadata_json: "{}" },
    { seq: 3, role: "tool", content_text: "", tool_name: "Read", tool_input: null, tool_output: "112 lines · GatewayBody::size_hint present — content-length is set on every upstream request.", tool_call_id: "t-1", thinking: null, parent_message_id: null, message_id: "m-3", timestamp: now - mins(117), provider_metadata_json: "{}" },
  ];
}

function recentActivity() {
  const row = (over: Partial<Unknown>) => ({
    request_id: "r-" + Math.random().toString(36).slice(2, 8),
    task_id: "t-main",
    agent_id: "pi-cli",
    logical_session: "s-pi-retry",
    subagent_role: null,
    role_source: "native",
    requested_model: "claude-opus-4.6",
    requested_provider: null,
    resolved_endpoint_id: "anthropic",
    resolved_model: "claude-opus-4.6",
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
    row({ subagent_role: "pi:reviewer", resolved_endpoint_id: "deepseek", resolved_model: "deepseek-v4-flash", protocol: "openai", usage_input: 6_120, usage_output: 1_780, cache_read: 0, started_at: now - mins(9), ended_at: now - mins(9) + 15_200 }),
    row({ agent_id: "zcode-desktop", logical_session: "s-zcode-ui", resolved_endpoint_id: "zai-glm", resolved_model: "glm-5.3", requested_model: "glm-5.3", usage_input: 9_844, usage_output: 902, cache_read: 41_002, started_at: now - mins(24), ended_at: now - mins(24) + 6_100 }),
    row({ http_status: 429, resolved_endpoint_id: "zai-glm", resolved_model: "glm-5.3", usage_input: 420, usage_output: null, cache_read: 0, started_at: now - mins(51), ended_at: now - mins(51) + 900 }),
    row({ subagent_role: "pi:researcher", resolved_endpoint_id: "zai-glm", resolved_model: "glm-5.3-air", requested_model: null, usage_input: 31_002, usage_output: 6_441, started_at: now - hours(1.2), ended_at: now - hours(1.2) + 22_000 }),
    row({ agent_id: "zcode-desktop", logical_session: null, resolved_endpoint_id: "anthropic", resolved_model: "claude-sonnet-4.5", requested_model: "claude-sonnet-4.5", usage_input: 4_006, usage_output: 610, cache_read: 12_004, started_at: now - hours(2.1), ended_at: now - hours(2.1) + 4_400 }),
  ];
}

function usageRows() {
  const day = (d: number) => new Date(now - d * 86_400_000).toISOString().slice(0, 10);
  return [
    { day: day(0), agent_id: "pi-cli", endpoint_id: "anthropic", model_id: "claude-opus-4.6", requests: 96, usage_input: 1_204_000, usage_output: 148_220, cache_creation: 82_100, cache_read: 3_902_000, cost_usd: 41.28 },
    { day: day(0), agent_id: "pi-cli", endpoint_id: "deepseek", model_id: "deepseek-v4-flash", requests: 24, usage_input: 210_400, usage_output: 61_780, cache_creation: 0, cache_read: 0, cost_usd: 1.94 },
    { day: day(1), agent_id: "pi-cli", endpoint_id: "anthropic", model_id: "claude-sonnet-4.5", requests: 141, usage_input: 1_880_000, usage_output: 190_100, cache_creation: 96_400, cache_read: 4_410_000, cost_usd: 55.61 },
    { day: day(1), agent_id: "zcode-desktop", endpoint_id: "zai-glm", model_id: "glm-5.3", requests: 18, usage_input: 302_000, usage_output: 44_800, cache_creation: 12_000, cache_read: 640_000, cost_usd: null },
    { day: day(2), agent_id: "pi-cli", endpoint_id: "zai-glm", model_id: "glm-5.3", requests: 9, usage_input: 402_000, usage_output: 88_200, cache_creation: 0, cache_read: 0, cost_usd: null },
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
