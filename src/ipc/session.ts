import { invoke } from "@tauri-apps/api/core";

export interface SessionMessage {
  seq: number;
  role: string; // user | assistant | system | tool | provider_event
  content_text: string;
  tool_name: string | null;
  tool_input: string | null;
  tool_output: string | null;
  /** Provider-native id linking a tool_use to its tool_result (Claude
   *  `tool_use.id`, OpenAI `call_id`). Lets the UI pair a use to its result
   *  row even when an assistant turn emits multiple tool calls. */
  tool_call_id: string | null;
  /** Reasoning / chain-of-thought content (Claude `thinking`, Pi `thinking`).
   *  Rendered separately from `content_text`. */
  thinking: string | null;
  parent_message_id: string | null;
  message_id: string | null;
  timestamp: number | null;
  /** Opaque per-message metadata blob for provider-specific fields not
   *  promoted to first-class columns (attachments, is_error, model, usage,
   *  MCP provenance, raw envelope). Always a JSON object string; UI may
   *  surface known keys, unknown keys are preserved losslessly. */
  provider_metadata_json: string;
}

export interface MessageWindow {
  messages: SessionMessage[];
  total: number;
}

export interface Session {
  id: string;
  provider: string;
  title: string;
  summary: string;
  project: string | null;
  cwd: string | null;
  started_at: number;
  updated_at: number;
  ended_at: number | null;
  message_count: number;
  source_path: string;
  parent_session_id: string | null;
  agent_id: string | null;
  is_subagent: boolean;
  resume_command: string;
  child_count: number;
  source_files: string[];
  /** Opaque per-session metadata blob (model, usage, system prompt, tags,
   *  checkpoints, raw envelope). Always a JSON object string. */
  provider_metadata_json: string;
}

// ---- Session (universal, persisted store) ----
export const sessionList = (providerId?: string, search?: string, limit = 300) =>
  invoke<Session[]>("session_list", { providerId, search, limit });
export const sessionChildren = (providerId: string, parentId: string) =>
  invoke<Session[]>("session_children", { providerId, parentId });
export const sessionGet = (providerId: string, id: string) =>
  invoke<Session | null>("session_get", { providerId, id });
export const sessionRead = (providerId: string, id: string, offset = 0, limit = 0) =>
  invoke<MessageWindow>("session_read", { providerId, id, offset, limit });
export const sessionRefresh = () => invoke<void>("session_refresh");
// One-click open in a new terminal at the session's project dir, using the
// session's native resume command (e.g. `claude --resume <id>`).
export const sessionOpen = (providerId: string, id: string) =>
  invoke<void>("session_open", { provider: providerId, id });
// Reveal the session's source file in the OS file manager (File Explorer).
export const sessionReveal = (providerId: string, id: string) =>
  invoke<void>("session_reveal", { provider: providerId, id });
// Delete the session from Nestra AND remove its source files from disk.
// Destructive — the original CLI can no longer resume this session.
export const sessionDelete = (providerId: string, id: string) =>
  invoke<SessionDeleteResult>("session_delete", { provider: providerId, id });
export interface SessionDeleteResult {
  provider: string;
  id: string;
  removed_files: string[];
}
