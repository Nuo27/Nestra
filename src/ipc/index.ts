// ============================================================
// IPC barrel — re-exports every domain wrapper + type.
//
// Consumers may import from "../ipc" (barrel) or directly from a domain
// file ("../ipc/quota"). The cross-domain `ModelAbilities` type family
// stays here (imported by provider.ts and orchestration.ts); everything
// else lives in its domain file:
//
//   session.ts  · skills.ts · mcp.ts · settings.ts · palette.ts
//   diag.ts     · provider.ts (endpoint CRUD + presets) · quota.ts
//   agent.ts    · orchestration.ts (routing policy + control plane)
//   gateway.ts  (gateway service control) · autostart.ts (launch at login)
// ============================================================

/**
 * Modality token — matches the OpenCode config schema enum exactly.
 * The wire format is lowercase (`text`, `image`, `video`, `audio`, `pdf`).
 */
export type Modality = "text" | "audio" | "image" | "video" | "pdf";

/** Input/output modality matrix on a model entry. */
export interface Modalities {
  input?: Modality[];
  output?: Modality[];
}

export interface ModelAbilities {
  reasoning?: boolean;
  tool_call?: boolean;
  attachment?: boolean;
  temperature?: boolean;
  /** Optional `input` cap — OpenCode schema accepts it; rarely reported. */
  limit?: { context: number; output: number; input?: number };
  /**
   * Modalities (text/image/video/audio/pdf). Surfaced from the same merged
   * capability metadata as the other fields — models.dev cache layered with
   * bundled vendor corrections. The capability disclosure renders this
   * read-only; per-endpoint JSON overrides still flow through unchanged.
   */
  modalities?: Modalities;
  /**
   * The wire dialect this model is officially served on: "anthropic" |
   * "openai-comp" (chat completions) | "response-api".  = follows the
   * endpoint protocol. Drives the gateway's per-model wire selection and
   * the Direct-mode filters.
   */
  api?: string;
}

export * from "./session";
export * from "./skills";
export * from "./mcp";
export * from "./settings";
export * from "./palette";
export * from "./diag";
export * from "./provider";
export * from "./quota";
export * from "./agent";
export * from "./orchestration";
export * from "./gateway";
export * from "./autostart";
export * from "./updates";
