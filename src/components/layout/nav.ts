import {
  Server,
  Terminal,
  History,
  Wrench,
  Cable,
  Settings,
  Network,
  LayoutDashboard,
  type LucideIcon,
} from "lucide-react";

export interface NavEntry {
  path: string;
  /** Translation key ("nav.*"), resolved at render time. */
  labelKey: string;
  icon: LucideIcon;
  match: string[];
}

/// The single nav source: the rail (RootShell) renders it and the ⌘K palette
/// injects it verbatim, so the two surfaces can never drift. Overview sits
/// above the sections (the landing page); Run carries the orchestration
/// control plane (Providers feed Agents feed the Gateway); Settings is pinned
/// to the rail bottom.
// Labels are translation KEYS ("nav.*"), resolved with `t()` at render time —
// module-level consts can't use hooks, and a static string would freeze the
// language at load.
export const NAV_TOP: NavEntry = {
  path: "/",
  labelKey: "nav.overview",
  icon: LayoutDashboard,
  match: ["/"],
};

export const NAV_SECTIONS: { labelKey: string; entries: NavEntry[] }[] = [
  {
    labelKey: "nav.run",
    entries: [
      { path: "/providers", labelKey: "nav.providers", icon: Server, match: ["/providers"] },
      { path: "/agents", labelKey: "nav.agents", icon: Terminal, match: ["/agents"] },
      { path: "/gateway", labelKey: "nav.gateway", icon: Network, match: ["/gateway"] },
    ],
  },
  {
    labelKey: "nav.records",
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

export const NAV_PINNED: NavEntry[] = [
  { path: "/settings", labelKey: "nav.settings", icon: Settings, match: ["/settings"] },
];

/** Every entry in rail order — the palette's nav section. */
export const NAV_ALL: NavEntry[] = [
  NAV_TOP,
  ...NAV_SECTIONS.flatMap((s) => s.entries),
  ...NAV_PINNED,
];
