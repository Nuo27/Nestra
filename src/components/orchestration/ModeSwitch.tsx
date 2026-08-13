import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { agentGatewayEnabled, agentSetGatewayEnabled } from "../../ipc/orchestration";
import { gatewayGetStatus } from "../../ipc/gateway";
import { extractError } from "../../ipc/errors";
import { useUI } from "../../stores/ui";
import { qk } from "../../lib/queries";
import { SegmentedControl } from "../controls/SegmentedControl";
import { Tip } from "../ui/tooltip";

/// Mode switch for the two per-agent config modes:
///   Direct — Nestra writes the real upstream URL + key into the agent's
///           config (the agent talks straight to the bound provider).
///   Routed — Nestra writes a stable gateway alias instead; the gateway
///           resolves provider/model per task, observes quota, migrates on
///           failure. Provider switching no longer rewrites the config.
///
/// Rendered on the shared `SegmentedControl` (the one sanctioned boxed
/// single-select, see DESIGN.md §5). Backed by the same `setting_kv` flag
/// (`orchestration.gateway.<id>`) the agent card and the detail page both
/// read, so toggling in either place is reflected everywhere instantly.
/// Hidden for agents without a gateway-capable writer.
export function ModeSwitch({
  agentId,
  supportsGateway,
}: {
  agentId: string;
  supportsGateway: boolean;
}) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useUI((s) => s.pushToast);

  const supported = supportsGateway;
  const enabledQ = useQuery({
    queryKey: ["orchestration", "gateway-flag", agentId],
    queryFn: () => agentGatewayEnabled(agentId),
    enabled: supported,
  });
  // Gateway liveness — shared cache with the /gateway page. Routed is BLOCKED
  // (disabled) while the service is down: enabling would rewrite the agent
  // config to point at a non-listening port.
  const gwQ = useQuery({ queryKey: qk.gatewayStatus(), queryFn: gatewayGetStatus });
  const gatewayRunning = gwQ.data?.state === "running";

  const toggleMut = useMutation({
    mutationFn: (next: boolean) => agentSetGatewayEnabled(agentId, next),
    onSuccess: (_data, next) => {
      qc.invalidateQueries({ queryKey: ["orchestration", "gateway-flag", agentId] });
      qc.invalidateQueries({ queryKey: qk.agentConfig(agentId) });
      qc.invalidateQueries({ queryKey: ["orchestration", "tasks"] });
      toast(
        next
          ? t("orchestration.modeRoutedToast")
          : t("orchestration.modeDirectToast"),
        "success",
      );
    },
    onError: (e: unknown) =>
      toast(t("orchestration.modeFailed", { err: extractError(e) ?? String(e) }), "error"),
  });

  if (!supported) return null;
  // A flag-read failure must not silently render "Direct" — the user would
  // toggle into a mode the backend can't honor. Disable the control instead.
  if (enabledQ.isError) {
    return (
      <Tip content={t("orchestration.modeLoadFailed")}>
        <span className="font-mono text-2xs text-warning">
          {t("orchestration.modeLoadFailed")}
        </span>
      </Tip>
    );
  }
  const routed = enabledQ.data ?? false;
  // `busy` includes isFetching: without it, a stale-window refetch after a
  // toggle lets the user double-flip while the mutation is still in flight.
  const busy = enabledQ.isLoading || enabledQ.isFetching || toggleMut.isPending;
  // Effective mode: ROUTED requires both intent AND a running gateway.
  const blocked = routed && !gatewayRunning && !gwQ.isLoading;

  return (
    <div className="flex flex-col gap-1">
      <SegmentedControl
        size="sm"
        ariaLabel={t("orchestration.modeAria")}
        value={routed ? "routed" : "direct"}
        onChange={(next) => {
          const v = next === "routed";
          if (v !== routed) toggleMut.mutate(v);
        }}
        items={[
          { value: "direct", label: t("orchestration.modeDirect"), disabled: busy },
          {
            value: "routed",
            label: t("orchestration.modeRouted"),
            disabled: busy || !gatewayRunning,
          },
        ]}
      />
      {blocked && (
        <Tip content={t("orchestration.modeBlockedHint")}>
          <span className="font-mono text-2xs text-warning">
            {t("orchestration.modeBlocked")}
          </span>
        </Tip>
      )}
    </div>
  );
}
