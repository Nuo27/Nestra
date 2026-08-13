import { useTranslation } from "react-i18next";
import type { QuotaItem } from "../../ipc";
import { fmtMoney, fmtNum } from "../../lib/format";
import { Skeleton } from "../ui/skeleton";
import { QuotaItemRow } from "./QuotaItemRow";

export function QuotaRow({ item }: { item: QuotaItem }) {
  const { t } = useTranslation();
  const pct = Math.max(0, Math.min(100, item.pct));
  // Balance-based items (OpenRouter key limits, Moonshot balance) render as
  // a single line: the remaining amount with its currency — no percentage,
  // no bar, no reset countdown (a balance has no window to fill).
  if (item.is_balance) {
    return (
      <QuotaItemRow
        name={item.name}
        pct={pct}
        detail={item.remaining != null ? fmtMoney(item.remaining, item.unit) : null}
        isBalance
      />
    );
  }
  const detail =
    item.used != null && item.total != null
      ? `${fmtNum(item.used)} / ${fmtNum(item.total)}`
      : item.remaining != null
        ? t("quota.left", { n: fmtNum(item.remaining) })
        : null;
  return <QuotaItemRow name={item.name} pct={pct} detail={detail} resetsIn={item.resets_in} />;
}

/// Bar-shaped skeletons that match QuotaRow's layout. Shown on first quota
/// fetch (no cache yet) so the user sees structure instead of "Loading….
export function QuotaSkeletonRows() {
  // Label widths are fixed tailwind classes (JIT can't see computed widths).
  const rows = [
    { label: "w-28", fill: "w-[90%]" },
    { label: "w-24", fill: "w-[70%]" },
    { label: "w-20", fill: "w-[55%]" },
    { label: "w-24", fill: "w-[60%]" },
  ];
  return (
    <div className="space-y-3">
      {rows.map((r, i) => (
        <div key={i}>
          <div className="mb-1.5 flex items-baseline justify-between">
            <Skeleton className={`h-3 ${r.label}`} />
            <Skeleton className="h-3 w-12" />
          </div>
          <Skeleton className={`h-1.5 ${r.fill}`} />
        </div>
      ))}
    </div>
  );
}
