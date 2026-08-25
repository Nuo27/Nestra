import { useTranslation } from "react-i18next";
import { ArrowDown, ArrowUp, Trash2 } from "lucide-react";
import type { EndpointInfo } from "../../ipc";
import { Button } from "../controls/Button";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "../ui/select";
import type { RouteTarget } from "../../ipc/orchestration";

/// The ordered (provider, model) route-target list. Each row is an inline
/// provider select + a model select filtered to that provider's models,
/// with up/down/delete controls; the add row picks any provider (its
/// default model pre-fills). Order is priority — the router serves the
/// first healthy entry and failures walk down.
///
/// Not the shared `OrderedChain`: that reorders already-chosen ids, while
/// here every row hosts a live (endpoint, model) pair of selects — the
/// interaction is different even though the ▲▼✕ affordances match.
export function TargetChain({
  value,
  onChange,
  endpoints,
}: {
  value: RouteTarget[];
  onChange: (next: RouteTarget[]) => void;
  endpoints: EndpointInfo[];
}) {
  const { t } = useTranslation();
  const modelsFor = (id: string): string[] => {
    const ep = endpoints.find((e) => e.id === id);
    const list = ep?.models?.available ?? [];
    const def = ep?.models?.default;
    // Default model first so the picker opens on the provider's serving
    // default even when the list is alphabetical.
    return def && !list.includes(def) ? [def, ...list] : list;
  };
  const defaultModelFor = (id: string): string =>
    endpoints.find((e) => e.id === id)?.models?.default
      ?? endpoints.find((e) => e.id === id)?.models?.available?.[0]
      ?? "";

  const move = (from: number, to: number) => {
    if (to < 0 || to >= value.length) return;
    const next = [...value];
    const [item] = next.splice(from, 1);
    next.splice(to, 0, item);
    onChange(next);
  };
  const addChoices = endpoints.filter(
    (e) => !value.some((v) => v.endpoint === e.id),
  );

  return (
    <div className="space-y-1.5">
      {value.length === 0 && (
        <div className="border border-dashed border-border bg-inset px-3 py-2 text-2xs text-subtle">
          {t("routingPolicy.targetsEmpty")}
        </div>
      )}
      {value.map((target, i) => (
        <div
          key={`${target.endpoint}:${i}`}
          className="flex items-center gap-1.5 border border-border bg-inset px-1.5 py-1"
        >
          <span className="w-5 shrink-0 text-center font-mono text-2xs text-subtle tabular">
            {i + 1}
          </span>
          <Select
            value={target.endpoint}
            onValueChange={(ep) => {
              const next = [...value];
              // Provider switch keeps a valid model: the provider's default.
              next[i] = { endpoint: ep, model: defaultModelFor(ep) };
              onChange(next);
            }}
          >
            <SelectTrigger className="h-7 flex-1 font-mono text-xs">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {endpoints.map((e) => (
                <SelectItem key={e.id} value={e.id} className="font-mono text-xs">
                  {e.display_name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Select
            value={target.model}
            onValueChange={(model) => {
              const next = [...value];
              next[i] = { endpoint: target.endpoint, model };
              onChange(next);
            }}
          >
            <SelectTrigger className="h-7 flex-1 font-mono text-xs">
              <SelectValue placeholder={t("routingPolicy.modelPlaceholder")} />
            </SelectTrigger>
            <SelectContent>
              {modelsFor(target.endpoint).map((m) => (
                <SelectItem key={m} value={m} className="font-mono text-xs">
                  {m}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <div className="flex shrink-0 items-center">
            <Button
              variant="ghost"
              size="sm"
              disabled={i === 0}
              onClick={() => move(i, i - 1)}
              aria-label={t("routingPolicy.moveUp")}
            >
              <ArrowUp data-icon size={13} />
            </Button>
            <Button
              variant="ghost"
              size="sm"
              disabled={i === value.length - 1}
              onClick={() => move(i, i + 1)}
              aria-label={t("routingPolicy.moveDown")}
            >
              <ArrowDown data-icon size={13} />
            </Button>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => onChange(value.filter((_, j) => j !== i))}
              aria-label={t("routingPolicy.removeTarget")}
            >
              <Trash2 data-icon size={13} />
            </Button>
          </div>
        </div>
      ))}
      {addChoices.length > 0 && (
        <div className="flex items-center gap-1.5">
          <Select
            value=""
            onValueChange={(ep) => {
              if (!ep) return;
              onChange([...value, { endpoint: ep, model: defaultModelFor(ep) }]);
            }}
          >
            <SelectTrigger className="h-7 flex-1 font-mono text-xs">
              <SelectValue placeholder={t("routingPolicy.addTarget")} />
            </SelectTrigger>
            <SelectContent>
              {addChoices.map((e) => (
                <SelectItem key={e.id} value={e.id} className="font-mono text-xs">
                  {e.display_name}
                  {e.models?.default ? ` · ${e.models.default}` : ""}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      )}
    </div>
  );
}
