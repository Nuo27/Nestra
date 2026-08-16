import { QueryClient } from "@tanstack/react-query";
import {
  PersistQueryClientProvider,
  type PersistQueryClientProviderProps,
} from "@tanstack/react-query-persist-client";
import { createSyncStoragePersister } from "@tanstack/query-sync-storage-persister";
import {
  RouterProvider,
  createRootRoute,
  createRoute,
  createRouter,
  redirect,
  useParams,
} from "@tanstack/react-router";
import { useState } from "react";
import { ErrorBoundary } from "./components/feedback/ErrorBoundary";
import { useUI } from "./stores/ui";
import { RootShell } from "./components/layout/RootShell";
import { ProvidersPage } from "./pages/providers";
import { ProviderEditPage } from "./pages/provider-edit";
import { QuotaPage } from "./pages/quota";
import { AgentsPage } from "./pages/agents";
import { AgentDetailPage } from "./pages/agent-detail";
import { AgentRoutingPage } from "./pages/agent-routing";
import { AgentReviewPage } from "./pages/agent-review";
import { SessionsPage } from "./pages/sessions";
import { SkillsPage } from "./pages/skills";
import { McpPage } from "./pages/mcp";
import { SettingsPage } from "./pages/settings";
import { GatewayPage } from "./pages/gateway";

const rootRoute = createRootRoute({
  component: RootShell,
});

// Providers lives at /providers so the top bar shows the real route; the
// root path just bounces there (opening the app lands on /providers).
const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  beforeLoad: () => {
    throw redirect({ to: "/providers" });
  },
});
const providersRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/providers",
  component: ProvidersPage,
});
const providerEditRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/providers/$id",
  component: () => <ProviderEditPage id={useParams({ from: "/providers/$id" }).id} />,
});
const quotaRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/quota/$id",
  component: () => <QuotaPage id={useParams({ from: "/quota/$id" }).id} />,
});
const agentsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/agents", component: AgentsPage });
// Agent detail — the merged orchestration surface: Direct (provider binding)
// vs Routed (routing policy + tasks + quota) per agent, with a shared
// mode switch. Orchestration lives per-agent, not on a global route.
const agentDetailRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/agents/$id",
  component: () => <AgentDetailPage id={useParams({ from: "/agents/$id" }).id} />,
});
// Routed-mode sub-page: the routing policy editor lives on its own route,
// linked from the detail page's Routed branch. Gated by RoutedGate while the
// agent is Direct.
const agentRoutingRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/agents/$id/routing",
  component: () => <AgentRoutingPage id={useParams({ from: "/agents/$id/routing" }).id} />,
});
// Review Runtime (Pi): spawn/supervise isolated review sessions on the
// reviewed work. Same shape as the routing sub-page.
const agentReviewRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/agents/$id/review",
  component: () => <AgentReviewPage id={useParams({ from: "/agents/$id/review" }).id} />,
  validateSearch: (search: Record<string, unknown>) => ({
    session: (search.session as string) || undefined,
  }),
});
const sessionsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/sessions",
  component: SessionsPage,
  validateSearch: (search: Record<string, unknown>) => ({
    id: (search.id as string) || undefined,
    provider: (search.provider as string) || undefined,
  }),
});
const skillsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/skills", component: SkillsPage });
const mcpRoute = createRoute({ getParentRoute: () => rootRoute, path: "/mcp", component: McpPage });
const settingsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/settings", component: SettingsPage });
const gatewayRoute = createRoute({ getParentRoute: () => rootRoute, path: "/gateway", component: GatewayPage });

const routeTree = rootRoute.addChildren([
  indexRoute,
  providersRoute,
  providerEditRoute,
  quotaRoute,
  agentsRoute,
  agentDetailRoute,
  agentRoutingRoute,
  agentReviewRoute,
  sessionsRoute,
  skillsRoute,
  mcpRoute,
  settingsRoute,
  gatewayRoute,
]);

const router = createRouter({ routeTree });

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}

// localStorage key for the persisted query cache. Sized for the small query
// families only (sessions list, providers, agents, skills, mcp, settings,
// routing) — session-messages / session-children are excluded below because
// full conversation bodies would blow the ~5MB localStorage budget.
const QUERY_CACHE_KEY = "nestra-query-cache";

/// Access `window.localStorage` defensively: module-level access crashes in
/// non-DOM environments (vitest node) and can throw in privacy-mode
/// WebViews. Falls back to an in-memory store so persistence is a no-op
/// instead of a crash.
function safeStorage(): Storage {
  try {
    return window.localStorage;
  } catch {
    return new MemoryStorage();
  }
}

class MemoryStorage implements Storage {
  private map = new Map<string, string>();
  get length() {
    return this.map.size;
  }
  clear() {
    this.map.clear();
  }
  getItem(k: string) {
    return this.map.get(k) ?? null;
  }
  key(i: number) {
    return [...this.map.keys()][i] ?? null;
  }
  removeItem(k: string) {
    this.map.delete(k);
  }
  setItem(k: string, v: string) {
    this.map.set(k, v);
  }
}

function persistOptionsFor(enabled: boolean): PersistQueryClientProviderProps["persistOptions"] {
  return {
    // When persistence is OFF, write to a throwaway in-memory store — the
    // provider tree stays constant (no remount) and the on-disk cache is
    // simply never touched.
    persister: createSyncStoragePersister({
      storage: enabled ? safeStorage() : new MemoryStorage(),
      key: QUERY_CACHE_KEY,
    }),
    maxAge: 1000 * 60 * 60 * 24 * 7, // 7 days
    dehydrateOptions: {
      shouldDehydrateQuery: (q) => {
        const k = q.queryKey[0];
        return (
          typeof k === "string" &&
          k !== "session-messages" &&
          k !== "session-children"
        );
      },
    },
  };
}

export function App() {
  const [qc] = useState(
    () =>
      new QueryClient({
        defaultOptions: {
          queries: {
            // SWR policy: cache-first + background refresh. staleTime 0 means
            // data is always stale, so returning to a tab re-fetches in the
            // background — but TanStack still serves the prior frame instantly
            // (no skeleton) until the fresh payload lands. gcTime keeps the
            // cache alive across tab switches so the first render is never a
            // spinner. Window-focus refetch stays off (desktop app, avoid
            // churn on every focus).
            staleTime: 0,
            gcTime: 30 * 60_000,
            refetchOnMount: "always",
            refetchOnWindowFocus: false,
            retry: 0,
          },
        },
      }),
  );
  // Persistence is opt-in (Settings → Data). When on, the last data shows
  // instantly on relaunch and refreshes in the background; when off the
  // cache stays in-memory exactly as before. ONE provider tree either way —
  // the old two-root switch remounted ErrorBoundary + RouterProvider (lost
  // form state/scroll) on every toggle.
  const persistQueryCache = useUI((s) => s.persistQueryCache);
  const persistOptions = persistOptionsFor(persistQueryCache);
  return (
    <PersistQueryClientProvider client={qc} persistOptions={persistOptions}>
      <ErrorBoundary>
        <RouterProvider router={router} />
      </ErrorBoundary>
    </PersistQueryClientProvider>
  );
}