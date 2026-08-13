import { invoke } from "@tauri-apps/api/core";

export interface HealthReport {
  ok: boolean;
  version: string;
  os: string;
  arch: string;
  data_dir: string;
  providers_detected: number;
  sessions_indexed: number;
  last_errors: string[];
}

// ---- Diagnostics ----
export const diagHealth = () => invoke<HealthReport>("diag_health");
export const diagExportLogs = (destPath: string) =>
  invoke<void>("diag_export_logs", { destPath });
export const diagOpenDataDir = () => invoke<void>("diag_open_data_dir");
