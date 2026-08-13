import { useTranslation } from "react-i18next";
import { SegmentedControl } from "../controls/SegmentedControl";

/// Affinity scope — how aggressively the router reuses a previous route.
/// Mirrors `routing_policy.affinity_scope` (schema.rs:233). Default `task`
/// because task-grain affinity protects the prompt cache. `session` reuses
/// across the whole logical session; `none` disables affinity.
export type AffinityScopeValue = "task" | "session" | "none";

export function AffinityScope({
  value,
  onChange,
}: {
  value: AffinityScopeValue;
  onChange: (next: AffinityScopeValue) => void;
}) {
  const { t } = useTranslation();
  return (
    <SegmentedControl<AffinityScopeValue>
      size="sm"
      value={value}
      onChange={onChange}
      items={[
        {
          value: "task",
          label: t("routingPolicy.affinityTask"),
          tooltip: t("routingPolicy.affinityTaskTip"),
        },
        {
          value: "session",
          label: t("routingPolicy.affinitySession"),
          tooltip: t("routingPolicy.affinitySessionTip"),
        },
        {
          value: "none",
          label: t("routingPolicy.affinityNone"),
          tooltip: t("routingPolicy.affinityNoneTip"),
        },
      ]}
    />
  );
}
