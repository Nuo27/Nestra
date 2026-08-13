import { invoke } from "@tauri-apps/api/core";

// ---- Autostart (launch at login) ----
export const autostartIsEnabled = () => invoke<boolean>("autostart_is_enabled");
export const autostartSet = (enabled: boolean) =>
  invoke<void>("autostart_set", { enabled });
