import { create } from "zustand";
import { persist } from "zustand/middleware";
import type { EndpointQuota } from "../ipc";
import i18n from "../i18n";

export type ToastTone = "default" | "success" | "error";
export interface Toast {
  id: number;
  message: string;
  tone: ToastTone;
}

/// Theme preference. `system` follows the OS; dark/light override.
export type ThemePref = "system" | "dark" | "light";
export type EffectiveTheme = "dark" | "light";

interface UIState {
  paletteOpen: boolean;
  paletteQuery: string;
  openPalette: () => void;
  closePalette: () => void;
  setPaletteQuery: (q: string) => void;
  /// true = icon-only rail (compact); false = expanded with labels.
  sidebarCollapsed: boolean;
  toggleSidebar: () => void;
  /// Resizable width of the sessions master list (px), persisted.
  sessionsListWidth: number;
  setSessionsListWidth: (px: number) => void;
  /// Quota page auto-refresh prefs (survive tab switches + reloads).
  quotaAuto: boolean;
  quotaIntervalSec: number;
  setQuotaAuto: (v: boolean) => void;
  setQuotaIntervalSec: (v: number) => void;
  /// Gateway log viewer auto-refresh (survives tab switches + reloads).
  logAuto: boolean;
  setLogAuto: (v: boolean) => void;
  /// In-memory (NOT persisted) quota snapshots keyed by endpoint id. Survives
  /// route unmount so switching back to the Quota tab shows the last bars
  /// instantly instead of re-fetching on mount.
  quotaCache: Record<string, EndpointQuota>;
  setQuotaCache: (id: string, data: EndpointQuota) => void;
  /// Transient toast stack. Each toast auto-dismisses after `ttlMs` (armed
  /// by pushToast itself); dismissToast is called manually on click.
  toasts: Toast[];
  pushToast: (message: string, tone?: ToastTone, ttlMs?: number) => void;
  dismissToast: (id: number) => void;
  /// Persisted theme preference; `system` = follow OS.
  theme: ThemePref;
  setTheme: (t: ThemePref) => void;
  /// Persisted UI language code ("en" | "zh"). `setLanguage` also flips the
  /// i18next locale + `<html lang>`.
  language: string;
  setLanguage: (l: string) => void;
  /// Whether the React Query cache survives relaunches (localStorage-backed).
  /// Off by default — the SWR policy refetches on mount either way, this only
  /// controls showing the last data instantly instead of a skeleton.
  persistQueryCache: boolean;
  setPersistQueryCache: (v: boolean) => void;
}

let nextToastId = 1;

export const useUI = create<UIState>()(
  persist(
    (set) => ({
      paletteOpen: false,
      paletteQuery: "",
      openPalette: () => set({ paletteOpen: true, paletteQuery: "" }),
      closePalette: () => set({ paletteOpen: false, paletteQuery: "" }),
      setPaletteQuery: (q) => set({ paletteQuery: q }),
      sidebarCollapsed: false,
      toggleSidebar: () => set((s) => ({ sidebarCollapsed: !s.sidebarCollapsed })),
      sessionsListWidth: 320,
      setSessionsListWidth: (px) => set({ sessionsListWidth: px }),
      quotaAuto: false,
      quotaIntervalSec: 10,
      setQuotaAuto: (v) => set({ quotaAuto: v }),
      setQuotaIntervalSec: (v) => set({ quotaIntervalSec: v }),
      logAuto: false,
      setLogAuto: (v) => set({ logAuto: v }),
      quotaCache: {},
      setQuotaCache: (id, data) =>
        set((s) => ({ quotaCache: { ...s.quotaCache, [id]: data } })),
      toasts: [],
      pushToast: (message, tone = "default", ttlMs = tone === "error" ? 6000 : 3200) => {
        const id = nextToastId++;
        set((s) => ({ toasts: [...s.toasts, { id, message, tone }] }));
        if (ttlMs > 0) {
          setTimeout(() => {
            set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) }));
          }, ttlMs);
        }
      },
      dismissToast: (id) =>
        set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
      theme: "system",
      setTheme: (t) => set({ theme: t }),
      language: "en",
      setLanguage: (l) => {
        const prev = useUI.getState().language;
        set({ language: l });
        // An invalid code leaves i18next on its fallback while the store
        // claims the new value — roll back so the two never diverge.
        i18n.changeLanguage(l).catch(() => {
          set({ language: prev });
          void i18n.changeLanguage(prev);
        });
      },
      persistQueryCache: false,
      setPersistQueryCache: (v) => set({ persistQueryCache: v }),
    }),
    {
      name: "nestra-ui",
      partialize: (s) => ({
        sidebarCollapsed: s.sidebarCollapsed,
        sessionsListWidth: s.sessionsListWidth,
        quotaAuto: s.quotaAuto,
        quotaIntervalSec: s.quotaIntervalSec,
        logAuto: s.logAuto,
        theme: s.theme,
        language: s.language,
        persistQueryCache: s.persistQueryCache,
      }),
    },
  ),
);
