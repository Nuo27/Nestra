import { useTranslation } from "react-i18next";
import type { ProbeResult } from "../../ipc";
import { StatusDot } from "../feedback/StatusDot";

/// Probe status glyph — the shared StatusDot vocabulary (● ok / ○ fail),
/// tinted by probe outcome. Tooltip carries latency + reason.
export function ProbeDot({ result }: { result: ProbeResult | undefined }) {
  const { t } = useTranslation();
  if (!result) return null;
  const latency = result.latency_ms != null ? `${result.latency_ms} ms` : "—";
  const tip = result.ok
    ? t("mcp.probeReachable", { latency })
    : t("mcp.probeUnreachable", { reason: result.reason ?? t("mcp.probeUnknown") });
  return (
    <StatusDot
      status="ok"
      color={result.ok ? "var(--success)" : "var(--danger)"}
      size={1.5}
      title={tip}
    />
  );
}
