import { Link, Outlet, useRouterState } from "@tanstack/react-router";
import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import { PanelLeftClose, PanelLeftOpen } from "lucide-react";
import { useUI } from "../../stores/ui";
import { Palette } from "./Palette";
import { QuotaAutoDriver } from "./QuotaAutoDriver";
import { ThemeToggle } from "./ThemeToggle";
import { Toaster } from "../feedback/Toaster";
import { TooltipProvider, Tip } from "../ui/tooltip";
import { Kbd } from "../ui/kbd";
import { SectionLabel } from "./PageHeader";
import { NAV_TOP, NAV_SECTIONS, NAV_PINNED, type NavEntry } from "./nav";
import { agentList } from "../../ipc/agent";
import { endpointList } from "../../ipc/provider";
import { settingGet, updatesCheck } from "../../ipc";
import { qk } from "../../lib/queries";

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

/// One breadcrumb segment: a translated page label, optionally an entity id
/// resolved to its display name (agents/endpoints) and/or a translated
/// sub-page suffix (routing / review / logs). Falls back to the raw id until
/// the name cache warms — the shell never blocks on it.
interface Crumb {
  labelKey: string;
  entityId?: string;
  entityKind?: "agent" | "endpoint";
  subKey?: string;
}

function crumbFor(pathname: string): Crumb {
  const seg = pathname.split("/").filter(Boolean);
  const [head, id, sub] = seg;
  switch (head) {
    case undefined:
      return { labelKey: "nav.overview" };
    case "providers":
      return id
        ? { labelKey: "nav.providers", entityId: id, entityKind: "endpoint" }
        : { labelKey: "nav.providers" };
    case "quota":
      return id
        ? { labelKey: "nav.quota", entityId: id, entityKind: "endpoint" }
        : { labelKey: "nav.quota" };
    case "agents":
      if (!id) return { labelKey: "nav.agents" };
      if (sub === "routing")
        return {
          labelKey: "nav.agents",
          entityId: id,
          entityKind: "agent",
          subKey: "agentRouting.crumb",
        };
      if (sub === "review")
        return {
          labelKey: "nav.agents",
          entityId: id,
          entityKind: "agent",
          subKey: "agentDetail.crumbReview",
        };
      return { labelKey: "nav.agents", entityId: id, entityKind: "agent" };
    case "sessions":
      return { labelKey: "nav.sessions" };
    case "skills":
      return { labelKey: "nav.skills" };
    case "mcp":
      return { labelKey: "nav.mcp" };
    case "settings":
      return { labelKey: "nav.settings" };
    case "gateway":
      // `/gateway/logs` has no `$id` level — "logs" IS the second segment.
      return id === "logs"
        ? { labelKey: "nav.gateway", subKey: "gatewayLogs.crumb" }
        : { labelKey: "nav.gateway" };
    default:
      return { labelKey: "nav.overview" };
  }
}

/// Module-level once-per-process guard for the launch update check (a `t`
/// identity change on language switch re-runs the effect; this keeps the
/// check truly one-shot).
let updateCheckRan = false;

/** Resolve an agent/endpoint id to its display name via the shared list
 * queries (SWR — warm caches answer instantly, cold ones fetch once). */
function useEntityName(kind: "agent" | "endpoint" | undefined, id: string | undefined) {
  const agentsQ = useQuery({
    queryKey: qk.agents(),
    queryFn: agentList,
    enabled: kind === "agent" && !!id,
  });
  const endpointsQ = useQuery({
    queryKey: qk.endpoints(),
    queryFn: endpointList,
    enabled: kind === "endpoint" && !!id,
  });
  if (!id || !kind) return undefined;
  if (kind === "agent")
    return agentsQ.data?.find((a) => a.id === id)?.display_name;
  return endpointsQ.data?.find((e) => e.id === id)?.display_name;
}

function Breadcrumb({ pathname }: { pathname: string }) {
  const { t } = useTranslation();
  const crumb = crumbFor(pathname);
  const entityName = useEntityName(crumb.entityKind, crumb.entityId);
  const segments = [t(crumb.labelKey)];
  if (crumb.subKey) segments.push(t(crumb.subKey));
  if (crumb.entityId) segments.push(entityName ?? crumb.entityId);
  return (
    <span className="min-w-0 truncate font-mono text-xs font-normal text-muted">
      {segments.join(" · ")}
    </span>
  );
}

export function RootShell() {
  const { paletteOpen, openPalette, closePalette, sidebarCollapsed, toggleSidebar, pushToast } =
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

  // Launch-time update check (opt-in via Settings → Updates). ONE fetch per
  // launch, never a poll; an available update surfaces as a quiet toast. A
  // failure stays silent — a launch check must never nag. The module-level
  // guard keeps this once-per-process even though `t` changes identity on
  // language switches (which would otherwise re-run the effect).
  useEffect(() => {
    if (updateCheckRan) return;
    updateCheckRan = true;
    let cancelled = false;
    void (async () => {
      try {
        const app = (await settingGet("app")) as { auto_update_check?: boolean } | null;
        if (cancelled || !app?.auto_update_check) return;
        const info = await updatesCheck();
        if (cancelled || !info.hasUpdate) return;
        pushToast(t("settings.updateToast", { version: info.latest }), "success");
      } catch {
        /* silent */
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [pushToast, t]);

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
          {/* Overview sits above the section groups — the landing page. */}
          <div className="flex flex-col gap-0.5">
            <NavLink n={NAV_TOP} pathname={pathname} collapsed={sidebarCollapsed} />
          </div>

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
            {NAV_PINNED.map((n) => (
              <NavLink
                key={n.path}
                n={n}
                pathname={pathname}
                collapsed={sidebarCollapsed}
              />
            ))}
          </div>

          {/* Collapse control — its own strip under a separator, NOT a nav
              item: it must never read as one of the rail's tabs. */}
          <div className="mt-1 border-t border-border pt-2">
            <Tip
              content={sidebarCollapsed ? t("nav.expandSidebar") : t("nav.collapseSidebar")}
              side="right"
            >
              <button
                type="button"
                onClick={toggleSidebar}
                aria-label={sidebarCollapsed ? t("nav.expandSidebar") : t("nav.collapseSidebar")}
                className={
                  "relative flex h-9 items-center text-sm text-muted transition-[color,box-shadow] duration-fast hover:text-fg focus-visible:shadow-focus " +
                  (sidebarCollapsed ? "mx-auto justify-center px-0 w-9 " : "gap-2 px-2 ")
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
            {/* Brand + breadcrumb — the current page as a human label, with
                route params resolved to entity names. The nav rail carries
                the section labels. */}
            <span className="flex items-center gap-2 text-sm font-semibold min-w-0">
              <span className="shrink-0">
                <span className="text-fg">NESTRA</span>
                <span className="text-accent">{">"}</span>
              </span>
              <Breadcrumb pathname={pathname} />
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
        <QuotaAutoDriver />
        <Toaster />
      </div>
    </TooltipProvider>
  );
}
