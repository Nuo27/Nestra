import { invoke } from "@tauri-apps/api/core";

// ---- Review Runtime ----

/** The gathered review input (mirrors `review::context::ContextPack`). */
export interface ContextPack {
  title: string;
  goal: string | null;
  modified_files: string[];
  failed_attempts: string[];
  diff: string | null;
}

/** One `review` row. `live_events` is present only while the review runs. */
export interface ReviewInfo {
  id: string;
  agent_id: string;
  reviewed_session_provider: string;
  reviewed_session_id: string;
  status: string; // pending|reviewing|verdict|failed|aborted
  review_role: string | null;
  verdict_summary: string | null;
  verdict_status: string | null;
  artifact_path: string | null;
  context_pack: ContextPack | null;
  created_at: number;
  finished_at: number | null;
  /** Backfilled from the review's own gateway rows (null = never routed or
   *  ambiguous window). */
  reviewer_endpoint_id: string | null;
  reviewer_model: string | null;
  task_id: string | null;
  live_events?: unknown[];
}

export const reviewCreate = (providerId: string, sessionId: string) =>
  invoke<ReviewInfo>("review_create", { providerId, sessionId });

export const reviewStart = (id: string) => invoke<ReviewInfo>("review_start", { id });

export const reviewAbort = (id: string) => invoke<void>("review_abort", { id });

export const reviewList = () => invoke<ReviewInfo[]>("review_list");

export const reviewGet = (id: string) =>
  invoke<ReviewInfo | null>("review_get", { id });
