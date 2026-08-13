import type { ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { agentGatewayEnabled } from "../../ipc/orchestration";
import { Skeleton } from "../ui/skeleton";
import { ErrorBanner } from "../feedback/ErrorBanner";
import { EmptyOrchestration } from "./EmptyOrchestration";
import { ModeSwitch } from "./ModeSwitch";
import { useTranslation } from "react-i18next";

/// Gate for Routed-mode sub-pages (the routing policy page): a Routed-mode
/// surface, so while the agent is in Direct mode we show a hint + the same
/// ModeSwitch instead of the content. The switch reads/writes the shared
/// `setting_kv` flag, so toggling here reveals the content in place.
export function RoutedGate({
  agentId,
  supportsGateway,
  title,
  hint,
  children,
}: {
  agentId: string;
  supportsGateway: boolean;
  title: string;
  hint?: ReactNode;
  children: ReactNode;
}) {
  const { t } = useTranslation();
  const q = useQuery({
    queryKey: ["orchestration", "gateway-flag", agentId],
    queryFn: () => agentGatewayEnabled(agentId),
  });
  if (q.isLoading) return <Skeleton className="h-40 w-full" />;
  if (q.isError) {
    // A flag-read failure must NOT masquerade as "Direct mode" — the user
    // would see the empty-state and could edit a config the backend can't
    // honor.
    return <ErrorBanner onRetry={() => q.refetch()}>{t("common.loadFailed")}</ErrorBanner>;
  }
  if (!(q.data ?? false)) {
    return (
      <EmptyOrchestration title={title} hint={hint}>
        <ModeSwitch agentId={agentId} supportsGateway={supportsGateway} />
      </EmptyOrchestration>
    );
  }
  return <>{children}</>;
}
