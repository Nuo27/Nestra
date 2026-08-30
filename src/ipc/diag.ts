import { invoke } from "@tauri-apps/api/core";

export interface HealthReport {
  ok: boolean;
  version: string;
  os: string;
  arch: string;
  data_dir: string;
  providers_detected: number;
  sessions_indexed: number;
  /** Recent ERROR lines from the newest JSON log generation (oldest-first). */
  last_errors: string[];
  db_path: string;
  db_size_bytes: number;
  log_dir: string;
  /** True when NESTRA_LOG overrides the persisted verbosity preset. */
  log_env_override: boolean;
}

// ---- Diagnostics ----
export const diagHealth = () => invoke<HealthReport>("diag_health");
export const diagExportLogs = (destPath: string) =>
  invoke<void>("diag_export_logs", { destPath });
export const diagExportText = (destPath: string, content: string) =>
  invoke<void>("diag_export_text", { destPath, content });
export const diagOpenDataDir = () => invoke<void>("diag_open_data_dir");

// ---- Gateway log viewer (reads the JSON twin layer) ----

export interface LogEntry {
  timestamp: string;
  level: string;
  target: string;
  /** Message plus structured fields rendered `k=v`. */
  message: string;
  task?: string;
  request?: string;
}

export type LogLevelPreset = "info" | "debug" | "trace";

/** Severity-and-above: "warn" keeps WARN + ERROR. */
export type LogFilterLevel = "all" | "error" | "warn" | "info";

export const diagLogFiles = () => invoke<string[]>("diag_log_files");
export const diagReadLogs = (opts: {
  file?: string;
  level?: LogFilterLevel;
  search?: string;
  limit?: number;
}) => invoke<LogEntry[]>("diag_read_logs", opts);
export const diagLogLevelGet = () =>
  invoke<LogLevelPreset>("diag_log_level_get");
export const diagLogLevelSet = (preset: LogLevelPreset) =>
  invoke<LogLevelPreset>("diag_log_level_set", { preset });

/** Full-body capture: debug wire evidence logs complete bodies (vs 2 KiB
 * snippets) — the "see everything forwarded" opt-in. */
export const diagLogFullBodiesGet = () =>
  invoke<boolean>("diag_log_full_bodies_get");
export const diagLogFullBodiesSet = (enabled: boolean) =>
  invoke<boolean>("diag_log_full_bodies_set", { enabled });
