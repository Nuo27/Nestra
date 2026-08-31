import { useQuery } from "@tanstack/react-query";
import { agentList, type AgentCapability, type AgentInfo } from "../ipc";
import { qk } from "./queries";

/// HTTP-status → tone class for task summaries. Extracted from three
/// near-identical nested ternaries (no-nested-ternary rule).
export function statusToneClass(status: number | null): string {
  if (status === null || status === undefined) return "text-subtle";
  return status < 400 ? "text-success" : "text-danger";
}

/** A capability key that gates whether an agent appears in a feature's list. */
type CapabilityKey = keyof AgentCapability;

/**
 * Lookup of display name by agent id, built from the agent list the backend
 * already returns. Unknown ids fall back to the id itself so a stale
 * reference never renders blank.
 */
export function useAgentLabels() {
  const agentQ = useQuery({ queryKey: qk.agents(), queryFn: agentList });
  const labels = new Map<string, string>();
  for (const c of agentQ.data ?? []) labels.set(c.id, c.display_name);
  return (id: string) => labels.get(id) ?? id;
}

/**
 * Unified "active agents" selector for every feature that needs to list agents
 * (Skills, MCP, anything future). Returns only agents that are:
 *   - detected + connected (status ok), AND
 *   - enabled (Nestra is allowed to touch their config), AND
 *   - optionally support the given capability flag.
 *
 * Replaces the per-page `agentList()` + hand-rolled filter that Skills and MCP
 * each duplicated with slightly different criteria.
 */
export function useActiveAgents(capability?: CapabilityKey) {
  return useQuery({
    queryKey: qk.agents(),
    queryFn: agentList,
    select: (all: AgentInfo[]) =>
      all.filter(
        (c) =>
          c.status === "ok" &&
          c.enabled &&
          (!capability || c.capability[capability]),
      ),
  });
}

/**
 * MCP-capable agents regardless of connection status. Used by the MCP page to
 * render chips for disconnected agents so users can see (and untoggle) stale
 * enablements. Backed by the same `agentList` query as `useActiveAgents` so
 * the data is shared.
 */
export function useMCPCapableAgents() {
  return useQuery({
    queryKey: qk.agents(),
    queryFn: agentList,
    select: (all: AgentInfo[]) =>
      all.filter((c) => c.capability.supports_mcp && c.enabled),
  });
}
