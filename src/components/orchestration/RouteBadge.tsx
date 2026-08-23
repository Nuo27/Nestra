import { useTranslation } from "react-i18next";
import { Tag } from "../controls/Tag";

/// The reason the router picked a route. Mirrors `RouteReason` in the Rust
/// identity model (identity.rs:304-329). Each reason gets a terminal glyph so
/// a route history reads like a console log without prose.
///
///   explicit     → `>`  (user/agent asked for this provider/model)
///   affinity     → `↻`  (reused a previous task-grain route — cache-friendly)
///   policy       → `◆`  (first healthy entry of the role's route-target list)
///   capability   → `◆`  (legacy pre-route-targets reason; historical rows only)
///   fallback     → `⇄`  (chosen after a migration trigger)
///   no_eligible  → `✕`  (no eligible route; request could not be served)
export type RouteReason =
  | "explicit"
  | "affinity"
  | "policy"
  | "capability"
  | "fallback"
  | "no_eligible";

const MAP: Record<RouteReason, { glyph: string; tone: "fg" | "success" | "warning" | "danger"; labelKey: string }> = {
  explicit: { glyph: ">", tone: "fg", labelKey: "route.explicit" },
  affinity: { glyph: "↻", tone: "success", labelKey: "route.affinity" },
  policy: { glyph: "◆", tone: "fg", labelKey: "route.policy" },
  capability: { glyph: "◆", tone: "fg", labelKey: "route.capability" },
  fallback: { glyph: "⇄", tone: "warning", labelKey: "route.fallback" },
  no_eligible: { glyph: "✕", tone: "danger", labelKey: "route.noEligible" },
};

export function RouteBadge({ reason }: { reason: RouteReason }) {
  const { t } = useTranslation();
  const m = MAP[reason] ?? MAP.explicit;
  const label = t(m.labelKey);
  return (
    <Tag tone={m.tone} title={`${t("route.reason")}: ${label}`}>
      <span aria-hidden>{m.glyph}</span>
      <span>{label}</span>
    </Tag>
  );
}
