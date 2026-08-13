import { invoke } from "@tauri-apps/api/core";

// ============================================================
// CLI (agent) surface — the detected binary + its bindings.
// Provider/endpoint types live in provider.ts.
// ============================================================

export type AgentStatus =
  | "ok"
  | "outdated"
  | "missing"
  | "manual_missing"
  | "manual_ok"
  | "unsupported";

export interface AgentCapability {
  /** Nestra has a ConfigAdapter for this agent and can manage its config. */
  manageable: boolean;
  /** Agent supports binding a Provider (endpoint) to it. */
  supports_provider_configuration: boolean;
  /** true = multi-provider list, false = single-slot binding. */
  supports_multiple_providers: boolean;
  /** Agent's ConfigAdapter can inject the provider at write time. */
  supports_provider_injection: boolean;
  /** Agent supports Factory Configuration backup and restore. */
  supports_factory_restore: boolean;
  /** Agent supports session reading. */
  supports_sessions: boolean;
  /** Agent supports MCP server sync into its config file. */
  supports_mcp: boolean;
  /** Agent's MCP config format has a per-server enabled field — the MCP page
   *  offers the "written but disabled" state for it. */
  supports_mcp_enabled: boolean;
  /** Agent exposes a skills directory (drives the Skills page agent filter). */
  supports_skills: boolean;
  /** Agent can be pointed at the Nestra gateway (Routed mode). */
  supports_gateway: boolean;
}

export interface AgentProvider {
  /** The agent this binding belongs to. */
  agent_id: string;
  /** The endpoint id this binding points at (the global Provider record). */
  provider_id: string;
  display_name: string;
  protocol: string;
  base_url: string;
  has_api_key: boolean;
  status: "valid" | "invalid" | "unvalidated";
  last_validated_at: number | null;
  /** true = this binding is the agent's active one (config currently injected). */
  active: boolean;
}

/** One entry of a provider selection sent to `agent_apply_provider_selection`.
 * `protocol` is the per-binding Direct-wire override (the protocol picker):
 * `null` resolves the default (first accepted protocol row). */
export interface ProviderSelection {
  provider_id: string;
  protocol: string | null;
}

export interface AgentInfo {
  id: string;
  kind: string;
  display_name: string;
  agent_path: string | null;
  installed_version: string | null;
  status: AgentStatus;
  /** Whether detection came from automatic probes or a user-pinned path. */
  source: "auto" | "manual";
  active_provider_id: string | null;
  has_backup: boolean;
  agent_path_override: string | null;
  config_path_override: string | null;
  config_path: string | null;
  /** false = Nestra stops writing this agent's config until re-enabled. */
  enabled: boolean;
  /** UI-agnostic capability booleans — drives the per-agent configuration UI. */
  capability: AgentCapability;
  /** Wire protocols the agent's ConfigAdapter can inject (filter the provider
   *  preset list). Empty for read-only agents. */
  supported_protocols: string[];
  /** How the agent's config format surfaces model selection
   *  (`anthropic_tiers` = haiku/sonnet/opus + default; `free_form` = plain list).
   *  Drives the models-editor shape. */
  model_selection: "anthropic_tiers" | "free_form";
  /** Provider entries owned by this agent (per-agent scope, independent). */
  providers: AgentProvider[];
  /** true = Nestra has a ConfigAdapter and can manage this agent's config. */
  manageable: boolean;
  /** true = a Factory Configuration snapshot has been captured. */
  has_factory: boolean;
  /** Free-form detection hint (e.g. parsed connection status text). */
  status_detail: string | null;
}

// ---- Agent ----
export const agentList = () => invoke<AgentInfo[]>("agent_list");
export const agentDetect = () => invoke<AgentInfo[]>("agent_detect");
export const agentClearProvider = (agentId: string) =>
  invoke<void>("agent_clear_provider", { agentId });
export const agentApplyProviderSelection = (
  agentId: string,
  selected: ProviderSelection[],
  defaultProviderId: string,
) =>
  invoke<void>("agent_apply_provider_selection", {
    agentId,
    selected,
    defaultProviderId,
  });
export interface DetectedProvider {
  key: string;
  display_name: string;
  managed: boolean;
}
export interface AgentConfigContent {
  path: string | null;
  content: string | null;
  detected: DetectedProvider[];
}
export const agentReadConfig = (agentId: string) =>
  invoke<AgentConfigContent>("agent_read_config", { agentId });
export const agentRemoveDetected = (agentId: string, key: string) =>
  invoke<DetectedProvider[]>("agent_remove_detected", { agentId, key });
export const agentSetOverride = (
  agentId: string,
  agentPath: string | null,
  configPath: string | null,
) => invoke<AgentInfo>("agent_set_override", { agentId, agentPath, configPath });
export const agentClearOverride = (agentId: string) =>
  invoke<AgentInfo>("agent_clear_override", { agentId });
export const agentSetEnabled = (agentId: string, enabled: boolean) =>
  invoke<AgentInfo>("agent_set_enabled", { agentId, enabled });
