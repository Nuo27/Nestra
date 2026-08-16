import { invoke } from "@tauri-apps/api/core";

// ---- Handoff (Context Lifecycle R1) ----

/** Context-pressure readout for one session (estimate — see backend doc). */
export interface ContextPressure {
  est_tokens: number;
  window_tokens: number;
  /** 0–100, clamped. */
  pct: number;
  estimated: boolean;
  /** Label of the largest single part (e.g. `tool result`). */
  top_consumer: string | null;
}

/** Structured extraction persisted as `handoff.sections_json`. */
export interface HandoffSections {
  goal: string | null;
  decisions: string[];
  modified_files: string[];
  failed_attempts: string[];
  subagents: string[];
  next_steps: string | null;
}

/** One `handoff` row (+ the artifact's current content, when readable). */
export interface HandoffInfo {
  id: string;
  source_provider: string;
  source_session_id: string;
  target_session_id: string | null;
  token_snapshot: number | null;
  cost_snapshot: number | null;
  artifact_path: string;
  created_at: number;
  sections: HandoffSections;
  markdown: string | null;
}

export const sessionContextPressure = (provider: string, sessionId: string) =>
  invoke<ContextPressure>("session_context_pressure", { providerId: provider, sessionId });

export const handoffPreview = (provider: string, sessionId: string) =>
  invoke<{ markdown: string }>("handoff_preview", { providerId: provider, sessionId });

export const handoffSave = (provider: string, sessionId: string, markdown: string) =>
  invoke<HandoffInfo>("handoff_save", { providerId: provider, sessionId, markdown });

export const handoffList = (provider: string, sessionId: string) =>
  invoke<HandoffInfo[]>("handoff_list", { providerId: provider, sessionId });

export const handoffDelete = (id: string) => invoke<void>("handoff_delete", { id });

/** Supervised RPC injection: spawn a fresh Pi session seeded with this
 *  handoff (records `target_session_id` once the stream reveals it). */
export const handoffSpawn = (id: string) => invoke<HandoffInfo>("handoff_spawn", { id });

/** Copy the artifact into the session repo's `.pi/` + reference line. */
export const handoffInject = (id: string) => invoke<string>("handoff_inject", { id });

/** Remove the injected `.pi/` copy + its reference line. */
export const handoffInjectRemove = (id: string) => invoke<void>("handoff_inject_remove", { id });

/** Promote to a knowledge file (`~/.nestra/knowledge/`). Returns the path. */
export const handoffToKnowledge = (id: string, kind?: string) =>
  invoke<string>("handoff_to_knowledge", { id, kind: kind ?? null });
