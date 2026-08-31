import { useEffect, useSyncExternalStore } from "react";
import { Moon, Sun, type LucideIcon } from "lucide-react";
import { useTranslation } from "react-i18next";
import { useUI, type ThemePref } from "../../stores/ui";
import { Tip } from "../ui/tooltip";

/// Resolve a ThemePref to the concrete theme that should render now.
/// `system` reads matchMedia; dark/light are literal.
function resolvePreferred(pref: ThemePref): "dark" | "light" {
  if (pref !== "system") return pref;
  return window.matchMedia("(prefers-color-scheme: light)").matches
    ? "light"
    : "dark";
}

/// Push the effective theme onto <html data-theme=…>, which the CSS keys off.
function applyTheme(pref: ThemePref) {
  document.documentElement.dataset.theme = resolvePreferred(pref);
}

/// Set the theme attribute synchronously before first paint (no FOUC).
export function initTheme(pref: ThemePref = "system") {
  applyTheme(pref);
}

const ICON: Record<"dark" | "light", LucideIcon> = {
  light: Sun,
  dark: Moon,
};

/// Reactive `prefers-color-scheme` subscription (replaces the stale
/// one-shot read: the icon/label now flips when the OS theme changes while
/// the app is on `system`).
function usePrefersLight(): boolean {
  return useSyncExternalStore(
    (onChange) => {
      const mq = window.matchMedia("(prefers-color-scheme: light)");
      // Safari <14 lacks addEventListener on MediaQueryList — feature-detect.
      if (mq.addEventListener) {
        mq.addEventListener("change", onChange);
        return () => mq.removeEventListener("change", onChange);
      }
      mq.addListener(onChange);
      return () => mq.removeListener(onChange);
    },
    () => window.matchMedia("(prefers-color-scheme: light)").matches,
  );
}

export function ThemeToggle() {
  const { t } = useTranslation();
  const theme = useUI((s) => s.theme);
  const setTheme = useUI((s) => s.setTheme);
  const prefersLight = usePrefersLight();

  // Keep <html data-theme> in sync, and re-resolve `system` when the OS
  // preference changes while the user hasn't pinned a theme. The listener
  // reads the CURRENT preference from the store each time (not the closure)
  // so an OS flip re-applies even after the user toggled away and back.
  useEffect(() => {
    applyTheme(theme);
    if (theme !== "system") return;
    const mq = window.matchMedia("(prefers-color-scheme: light)");
    const onChange = () => applyTheme(useUI.getState().theme);
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, [theme]);

  // The topbar toggle only switches between the two concrete themes, using
  // the *effective* theme as the anchor — so a click always visibly flips.
  // "Follow the system" lives on the Settings page instead.
  const effective =
    theme === "system" ? (prefersLight ? "light" : "dark") : theme;
  const next = effective === "dark" ? "light" : "dark";
  const Icon = ICON[effective];

  const label = t(`settings.theme${effective === "dark" ? "Dark" : "Light"}`);
  const nextLabel = t(`settings.theme${next === "dark" ? "Dark" : "Light"}`);

  const button = (
    <button
      type="button"
      onClick={() => setTheme(next)}
      aria-label={t("settings.themeToggleAria", { current: label, next: nextLabel })}
      className="brackets-state flex h-8 items-center px-1.5 text-muted transition-[color,box-shadow] duration-fast hover:text-fg focus-visible:shadow-focus"
    >
      <Icon data-icon size={15} strokeWidth={1.6} />
    </button>
  );

  return (
    <Tip
      content={t("settings.themeToggleTip", { current: label, next: nextLabel })}
      side="bottom"
    >
      {button}
    </Tip>
  );
}
