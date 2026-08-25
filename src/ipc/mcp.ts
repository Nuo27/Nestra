import { invoke } from "@tauri-apps/api/core";

// ---- MCP ----
export type McpKind = "stdio" | "http" | "sse";
export interface McpTransport {
  kind: McpKind;
  command: string | null;
  args: string[];
  env: Record<string, string>;
  url: string | null;
}
export interface McpServer {
  id: string;
  name: string;
  transport: McpTransport;
  /** Agents this server is written into with the native enabled flag on. */
  enabled_agents: string[];
  /** Agents this server is written into with the native enabled flag off
   *  (`enabled: false`). Only agents whose format carries the field. */
  disabled_agents: string[];
  managed: boolean;
  env_overrides: Record<string, Record<string, string>>;
}
export interface ImportCandidate {
  name: string;
  id: string;
  transport_json: string;
  agent_ids: string[];
  /** Agents where the entry was found with the enabled flag off. Importing
   *  preserves that state instead of force-enabling the server. */
  disabled_in: string[];
  transports_conflict: boolean;
  native_paths: [string, string][];
}
export type AgentMcpState = "absent" | "disabled" | "enabled";
export const mcpList = () => invoke<McpServer[]>("mcp_list");
export const mcpSave = (server: McpServer) => invoke<McpServer>("mcp_save", { server });
export const mcpSetState = (id: string, agentId: string, mcpState: AgentMcpState) =>
  invoke<McpServer>("mcp_set_state", { id, agentId, mcpState });
export const mcpDelete = (id: string) => invoke<void>("mcp_delete", { id });
export const mcpUnmanage = (id: string) => invoke<void>("mcp_unmanage", { id });
export const mcpImportScan = () => invoke<ImportCandidate[]>("mcp_import_scan");
export const mcpImportAll = () => invoke<McpServer[]>("mcp_import_all");
export const mcpImportOne = (agentId: string, name: string) =>
  invoke<McpServer>("mcp_import_one", { agentId, name });
export const mcpSyncAll = () => invoke<void>("mcp_sync_all");
/// Click-to-test a server. `latency_ms` is `null` on a hard failure
/// (spawn error, no command, …); `reason` carries the human-readable cause.
export interface ProbeResult {
  ok: boolean;
  latency_ms: number | null;
  reason: string | null;
}
export const mcpProbe = (id: string) => invoke<ProbeResult>("mcp_probe", { id });

/** Per-server gateway-observed tool usage (P1-1). Zero `total_calls` means
 *  none were observed — attribution currently covers the Claude-style
 *  `mcp__<server>__<tool>` namespace only. */
export interface McpUsageStat {
  server_id: string;
  server_name: string;
  total_calls: number;
  last_used_at: number | null;
  per_tool: Record<string, number>;
}

export const mcpUsageStats = () => invoke<McpUsageStat[]>("mcp_usage_stats");
