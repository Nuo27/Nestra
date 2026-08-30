import { invoke } from "@tauri-apps/api/core";

export interface PaletteItem {
  /** "nav" items are synthesized frontend-side from the shared nav source —
   * the backend never emits them (labels must be i18n'd and route-accurate). */
  kind: "provider" | "session" | "skill" | "nav";
  label: string;
  detail: string | null;
  target: string;
}

// ---- Palette ----
export const paletteSearch = (query: string) =>
  invoke<PaletteItem[]>("palette_search", { query });
