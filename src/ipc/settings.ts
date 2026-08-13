import { invoke } from "@tauri-apps/api/core";

// ---- Settings ----
export const settingGet = (key: string) =>
  invoke<unknown | null>("setting_get", { key });
export const settingSet = (key: string, value: unknown) =>
  invoke<void>("setting_set", { key, value });
export const settingDelete = (key: string) =>
  invoke<void>("setting_delete", { key });
