// Centralized query keys + invalidation sets.
//
// Every list/detail query in the app keys off these strings so that
// "invalidate everything a mutation touches" is one call with a prefix.
// Keep route pages reading from here — bare string keys drifted across pages
// and leave stale cache entries behind.

export const qk = {
  agents: () => ["agents"] as const,
  endpoints: () => ["endpoints"] as const,
  endpoint: (id: string) => ["endpoint", id] as const,
  providerHealth: () => ["provider-health"] as const,
  endpointQuota: (id: string) => ["endpoint-quota", id] as const,
  skills: () => ["skills"] as const,
  mcp: () => ["mcp"] as const,
  mcpImport: () => ["mcp-import"] as const,
  mcpUsage: () => ["mcp-usage"] as const,
  sessions: (provider?: string, search?: string) =>
    ["sessions", provider ?? "", search ?? ""] as const,
  session: (provider: string, id: string) => ["session", provider, id] as const,
  sessionMessages: (provider: string, id: string, shown: number) =>
    ["session-messages", provider, id, shown] as const,
  sessionChildren: (provider: string, id: string) =>
    ["session-children", provider, id] as const,
  sessionPressure: (provider: string, id: string) =>
    ["session-pressure", provider, id] as const,
  handoffs: (provider: string, id: string) => ["handoffs", provider, id] as const,
  reviews: () => ["reviews"] as const,
  review: (id: string) => ["review", id] as const,
  agentConfig: (agentId: string) => ["agent-config", agentId] as const,
  quotaRefresh: () => ["quota-refresh"] as const,
  opencodeCreds: (endpointId: string) => ["opencode-creds", endpointId] as const,
  keepalivePreview: (endpointId: string) => ["keepalive-preview", endpointId] as const,
  keepaliveStatus: (endpointId: string) => ["keepalive-status", endpointId] as const,
  settings: () => ["settings"] as const,
  autostart: () => ["autostart"] as const,
  diagHealth: () => ["diag", "health"] as const,
  // Orchestration. Routing policy, quota state, and task summaries are
  // consumed by the merged Agents surface (agent cards + /agents/$id detail).
  routingPolicies: (agentId: string) => ["routing-policy", agentId] as const,
  detectedRoles: (agentId: string) => ["detected-roles", agentId] as const,
  // Gateway Service control surface (the process, not routing policy).
  gatewayStatus: () => ["gateway", "status"] as const,
  gatewayActivity: () => ["gateway", "activity"] as const,
  // Gateway log viewer (JSON twin layer): file list, filtered entries,
  // active verbosity preset.
  logFiles: () => ["diag", "log-files"] as const,
  logs: (file: string | undefined, level: string, search: string, limit: number) =>
    ["diag", "logs", file ?? "", level, search, limit] as const,
  logLevel: () => ["diag", "log-level"] as const,
  logFullBodies: () => ["diag", "log-full-bodies"] as const,
} as const;

/// Query key prefixes mutated by each mutation family. Used by `invalidate`
/// as a prefix match so one call clears list + detail cache (e.g. a skills
/// toggle invalidates the skills list and any open agent config).
const INVALIDATION: Record<string, readonly string[]> = {
  endpoint: ["endpoint", "endpoints", "endpoint-quota"],
  agent: ["agents", "agent-config"],
  macro: ["skills", "mcp", "mcp-import", "mcp-usage"],
  session: ["sessions", "session", "session-messages", "session-children"],
  handoff: ["handoffs"],
  review: ["reviews", "review"],
  quota: ["quota-refresh", "endpoint-quota", "opencode-creds", "keepalive-preview", "keepalive-status"],
  settings: ["settings"],
  diag: ["diag"],
  // Orchestration tables — invalidate together since a policy edit can
  // change downstream route history / catalog views.
  orchestration: [
    "routing-policy",
    "logical-session",
    "route-request",
    "route-migration",
    "model-catalog",
    "detected-roles",
  ],
  // Gateway Service: the page's status/activity/token queries.
  gateway: ["gateway"],
};

/// Invalidate everything a Gateway Service mutation can touch — the page's own
/// status/activity/token queries PLUS the cross-surface caches that depend on
/// gateway liveness / agent config mode: the per-agent Routed-intent flag,
/// agent config reads, and the tasks list. Call this from every gateway
/// mutation's `onSuccess` so the Agents page never goes stale after a global
/// ON/OFF or port/token change.
export function invalidateGateway(qc: {
  invalidateQueries: (opts: { queryKey: readonly unknown[] }) => unknown;
}) {
  // Gateway page (the "gateway" prefix family covers status/activity/token).
  void qc.invalidateQueries({ queryKey: ["gateway"] });
  // Per-agent Routed-intent flag — every agent (prefix match).
  void qc.invalidateQueries({ queryKey: ["orchestration", "gateway-flag"] });
  // Agent config reads (the toggle rewrites the agent's config file).
  void qc.invalidateQueries({ queryKey: ["agent-config"] });
  // Tasks list — global OFF auto-reverts configs, so active tasks may change.
  void qc.invalidateQueries({ queryKey: ["orchestration", "tasks"] });
}

export function invalidate(
  qc: { invalidateQueries: (opts: { queryKey: readonly unknown[] }) => unknown },
  family: keyof typeof INVALIDATION,
) {
  for (const prefix of INVALIDATION[family]) {
    void qc.invalidateQueries({ queryKey: [prefix] });
  }
}