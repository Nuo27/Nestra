import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { orchStatus } from "../../ipc/orchestration";
import { Badge } from "../ui/badge";
import { Skeleton } from "../ui/skeleton";

/// Compact single-line gateway status bar for the Agents page top (the
/// global orchestration surface reduced to one status line; the detail lives
/// per-agent on its card + detail page).
export function GatewayStatusBar() {
  const { t } = useTranslation();
  const q = useQuery({
    queryKey: ["orchestration", "gateway-status"],
    queryFn: orchStatus,
    // The gateway binds asynchronously at app start; refetch a few times so
    // the status flips to `up` without a manual refresh.
    refetchInterval: (query) => (query.state.data?.up ? false : 2000),
  });

  if (q.isLoading) return <Skeleton className="h-8 w-full" />;
  if (q.isError) {
    // A read failure must not render as an eternal "○ starting" that polls
    // forever — show the failure and stop the poll loop.
    return (
      <div className="flex items-center gap-2 rounded border border-danger-border bg-danger-soft px-3 py-2">
        <span className="font-mono text-2xs text-danger">
          {t("orchestration.gatewayStatusFailed")}
        </span>
      </div>
    );
  }
  const status = q.data;

  return (
    <div className="flex items-center justify-between gap-3 rounded border border-border bg-inset px-3 py-2">
      <div className="flex min-w-0 items-center gap-2">
        <span className="shrink-0 font-mono text-2xs text-subtle">{t("orchestration.gateway")}</span>
        {status?.up ? (
          <Badge tone="success" variant="soft" className="font-mono text-2xs">
            ● {t("orchestration.listening")}
          </Badge>
        ) : (
          <Badge tone="neutral" variant="soft" className="font-mono text-2xs">
            ○ {t("orchestration.starting")}
          </Badge>
        )}
        {status?.up && (
          <span className="truncate font-mono text-2xs text-subtle">
            {status.base_url}
          </span>
        )}
      </div>
      {status?.up && status.agents_enabled.length > 0 && (
        <span className="shrink-0 font-mono text-2xs text-subtle">
          {t("orchestration.routing")}: {status.agents_enabled.join(", ")}
        </span>
      )}
    </div>
  );
}
