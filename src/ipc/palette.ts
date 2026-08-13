import { invoke } from "@tauri-apps/api/core";

export interface PaletteItem {
  kind: "provider" | "session" | "skill" | "nav";
  label: string;
  detail: string | null;
  target: string;
}

// ---- Palette ----
export const paletteSearch = (query: string) =>
  invoke<PaletteItem[]>("palette_search", { query });
