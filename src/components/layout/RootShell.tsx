import { Link, Outlet, useRouterState } from "@tanstack/react-router";
import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import {
  Server,
  Terminal,
  History,
  Wrench,
  Cable,
  Settings,
  Network,
  PanelLeftClose,
  PanelLeftOpen,
  type LucideIcon,
} from "lucide-react";
import { useUI } from "../../stores/ui";
import { Palette } from "./Palette";
import { ThemeToggle } from "./ThemeToggle";
import { Toaster } from "../feedback/Toaster";
import { TooltipProvider, Tip } from "../ui/tooltip";
import { Kbd } from "../ui/kbd";
import { SectionLabel } from "./PageHeader";

interface NavEntry {
  path: string;
  /** Translation key ("nav.*"), resolved at render time. */
  labelKey: string;
  icon: LucideIcon;
  match: string[];
}

/// Nav sections give the rail structural meaning instead of a flat list.
/// Matches the IA ownership map: Manage (config) → Observe (read) →
/// Extend (plugins) → System (app). Orchestration lives inside Agents —
/// per-agent Direct/Routed mode is on the agent card and its detail page.
// Labels are translation KEYS ("nav.*"), resolved with `t()` at render time —
// module-level consts can't use hooks, and a static string would freeze the
// language at load.
const NAV_SECTIONS: { labelKey: string; entries: NavEntry[] }[] = [
  {
    labelKey: "nav.manage",
    entries: [
      { path: "/providers", labelKey: "nav.providers", icon: Server, match: ["/providers"] },
      { path: "/agents", labelKey: "nav.agents", icon: Terminal, match: ["/agents"] },
    ],
  },
  {
    labelKey: "nav.observe",
    entries: [
      {
        path: "/sessions",
        labelKey: "nav.sessions",
        icon: History,
        match: ["/sessions"],
      },
    ],
  },
  {
    labelKey: "nav.extend",
    entries: [
      { path: "/skills", labelKey: "nav.skills", icon: Wrench, match: ["/skills"] },
      { path: "/mcp", labelKey: "nav.mcp", icon: Cable, match: ["/mcp"] },
    ],
  },
];

const PINNED: NavEntry[] = [
  { path: "/gateway", labelKey: "nav.gateway", icon: Network, match: ["/gateway"] },
  { path: "/settings", labelKey: "nav.settings", icon: Settings, match: ["/settings"] },
];

function NavLink({
  n,
  pathname,
  collapsed,
}: {
  n: NavEntry;
  pathname: string;
  collapsed: boolean;
  
}) {
  const { t } = useTranslation();
  const label = t(n.labelKey);
  const active = n.match.some(
    (m) => pathname === m || (m !== "/" && pathname.startsWith(m)),
  );
  const Icon = n.icon;
  // `Link` (not a raw `<a href>`), so tab switches are client-side navigations
  // instead of a full app reload + cache wipe in the Tauri webview.
  const link = (
    <Link
      to={n.path}
      aria-label={label}
      aria-current={active ? "page" : undefined}
      className={
        "relative flex h-9 items-center text-sm transition-[color,box-shadow] duration-fast focus-visible:shadow-focus " +
        (collapsed ? "mx-auto justify-center px-0 w-9 " : "gap-2 px-2 ") +
        (active ? "text-accent font-medium " : "text-muted hover:text-fg ") +
        (active ? "nav-active-bar" : "")
      }
    >
      <Icon
        data-icon
        size={15}
        strokeWidth={active ? 2 : 1.6}
        className={active ? "text-accent" : ""}
      />
      {!collapsed && <span className="truncate">{label}</span>}
    </Link>
  );
  return collapsed ? (
    <Tip content={label} side="right">
      {link}
    </Tip>
  ) : (
    link
  );
}

export function RootShell() {
  const { paletteOpen, openPalette, closePalette, sidebarCollapsed, toggleSidebar } =
    useUI();
  const { t } = useTranslation();
  const pathname = useRouterState({ select: (s) => s.location.pathname });

  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      const mod = e.metaKey || e.ctrlKey;
      if (mod && e.key.toLowerCase() === "k") {
        e.preventDefault();
        if (paletteOpen) closePalette();
        else openPalette();
      } else if (e.key === "Escape" && paletteOpen) {
        closePalette();
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [paletteOpen, openPalette, closePalette]);

  return (
    <TooltipProvider delayDuration={200}>
      <div className="flex h-full bg-canvas text-fg">
        {/* Left rail: collapsible nav */}
        <nav
          data-tauri-drag-region
          className={
            (sidebarCollapsed ? "w-14" : "w-56") +
            " flex shrink-0 flex-col gap-2 border-r border-border px-2 py-3 overflow-y-auto scroll"
          }
        >
          {NAV_SECTIONS.map((section) => (
            <div key={section.labelKey} className="flex flex-col gap-0.5">
              {!sidebarCollapsed && (
                <SectionLabel className="px-2 pb-1 pt-1">
                  {t(section.labelKey)}
                </SectionLabel>
              )}
              {section.entries.map((n) => (
                <NavLink
                  key={n.path}
                  n={n}
                  pathname={pathname}
                  collapsed={sidebarCollapsed}
                />
              ))}
            </div>
          ))}

          <div className="mt-auto flex flex-col gap-0.5">
            {!sidebarCollapsed && (
              <SectionLabel className="px-2 pb-1 pt-1">{t("nav.system")}</SectionLabel>
            )}
            {PINNED.map((n) => (
              <NavLink
                key={n.path}
                n={n}
                pathname={pathname}
                collapsed={sidebarCollapsed}
              />
            ))}

            <Tip
              content={sidebarCollapsed ? t("nav.expandSidebar") : t("nav.collapseSidebar")}
              side="right"
            >
              <button
                type="button"
                onClick={toggleSidebar}
                aria-label={sidebarCollapsed ? t("nav.expandSidebar") : t("nav.collapseSidebar")}
                // Same anatomy as the nav links above (relative + no
                // brackets-state) so the collapse control reads as one of the
                // rail's tabs, not as a separate button.
                className={
                  "relative flex h-9 items-center text-sm transition-[color,box-shadow] duration-fast focus-visible:shadow-focus " +
                  (sidebarCollapsed
                    ? "mx-auto justify-center px-0 w-9 "
                    : "gap-2 px-2 ") +
                  "text-muted hover:text-fg"
                }
              >
                {sidebarCollapsed ? (
                  <PanelLeftOpen data-icon size={15} strokeWidth={1.6} />
                ) : (
                  <>
                    <PanelLeftClose data-icon size={15} strokeWidth={1.6} />
                    <span className="truncate">{t("nav.collapseSidebar")}</span>
                  </>
                )}
              </button>
            </Tip>
          </div>
        </nav>

        {/* Main column: command bar + content */}
        <div className="flex min-w-0 flex-1 flex-col">
          <header
            data-tauri-drag-region
            className="flex h-12 shrink-0 items-center justify-between gap-3 border-b border-border pl-4 pr-3"
          >
            {/* Brand + current-page context — the full route path, live on
                navigation. The nav rail carries the section labels. */}
            <span className="flex items-center gap-2 text-sm font-semibold min-w-0">
              <span className="shrink-0">
                <span className="text-fg">NESTRA</span>
                <span className="text-accent">{">"}</span>
              </span>
              <span className="min-w-0 truncate font-mono text-xs font-normal text-muted">
                {pathname}
              </span>
            </span>
            <div className="flex items-center gap-2">
              <ThemeToggle />
              <Tip content={t("palette.placeholder")} side="bottom">
                <button
                  type="button"
                  onClick={openPalette}
                  aria-label={t("palette.placeholder")}
                  className="brackets-state flex h-8 items-center px-1.5 text-muted transition-[color,box-shadow] duration-fast hover:text-fg focus-visible:shadow-focus"
                >
                  {/* The palette binds Meta+K on macOS and Ctrl+K elsewhere —
                      show the right hint per platform. */}
                  <Kbd>{navigator.platform.toLowerCase().includes("mac") ? "⌘K" : "Ctrl+K"}</Kbd>
                </button>
              </Tip>
            </div>
          </header>

          <main className="min-h-0 flex-1 overflow-auto scroll">
            <Outlet />
          </main>
        </div>

        {paletteOpen && <Palette />}
        <Toaster />
      </div>
    </TooltipProvider>
  );
}
