import { useTranslation } from "react-i18next";
import type { ModelAbilities, Modality } from "../../ipc";
import {
  resolveField,
  resolveLimit,
  resolveModalities,
} from "../../lib/modelAbilities";
import { Disclosure } from "./Disclosure";
import { Button } from "./Button";
import { Input } from "../ui/input";
import { Switch } from "../ui/switch";

function ModelAbilitiesDisclosure({
  modelId,
  defaultAbility,
  overrideAbility,
  onChange,
}: {
  modelId: string;
  defaultAbility?: ModelAbilities;
  overrideAbility?: ModelAbilities;
  /** `undefined` clears the whole override row (the key is deleted). */
  onChange: (next: ModelAbilities | undefined) => void;
}) {
  const { t } = useTranslation();
  const hasOverride =
    overrideAbility !== undefined && Object.keys(overrideAbility).length > 0;
  const limitRow = resolveLimit(defaultAbility, overrideAbility);

  const fields: { key: keyof Pick<ModelAbilities, "reasoning" | "tool_call" | "attachment" | "temperature">; labelKey: string }[] = [
    { key: "reasoning", labelKey: "providerEdit.reasoning" },
    { key: "tool_call", labelKey: "providerEdit.toolCalls" },
    { key: "attachment", labelKey: "providerEdit.attachments" },
    { key: "temperature", labelKey: "providerEdit.temperature" },
  ];

  // Write field = override or default; "Reset" deletes the field from the
  // override row entirely so the merge helper inherits the default.
  const setField = (key: typeof fields[number]["key"], value: boolean | undefined) => {
    const next: ModelAbilities = { ...(overrideAbility ?? {}) };
    if (value === undefined) {
      delete (next as Record<string, unknown>)[key];
    } else {
      (next as Record<string, unknown>)[key] = value;
    }
    // An override with no fields left is not an override — delete the row.
    onChange(Object.keys(next).length === 0 ? undefined : next);
  };
  const setLimit = (lim: { context: number; output: number } | undefined) => {
    const next: ModelAbilities = { ...(overrideAbility ?? {}) };
    if (lim === undefined) delete next.limit;
    else next.limit = lim;
    onChange(Object.keys(next).length === 0 ? undefined : next);
  };

  // Summarize the override status for the row header.
  const sourceLabel = hasOverride
    ? t("providerEdit.overridden")
    : defaultAbility
      ? t("providerEdit.default")
      : t("providerEdit.noDefault");

  // The wire dialect this model is officially served on (corrections +
  // overrides layer). Shown for every model with a known api — not just
  // responses-class — so the user can see why a model is Direct-excluded.
  // All labels share one quiet tone; the text distinguishes the dialect.
  const api = defaultAbility?.api;
  const apiLabel = api ? (
    <span
      className="ml-1 shrink-0 bg-border px-1 py-px text-2xs text-muted"
      title={
        api === "response-api"
          ? t("providerEdit.apiResponsesTip")
          : api === "openai-comp"
            ? t("providerEdit.apiOpenaiTip")
            : t("providerEdit.apiAnthropicTip")
      }
    >
      {api === "response-api" ? "responses-api" : api === "openai-comp" ? "openai-comp" : api}
    </span>
  ) : null;

  return (
    <Disclosure
      className="border border-border"
      header={
        <>
          <span className="min-w-0 flex-1 truncate font-mono text-xs">{modelId}</span>
          {apiLabel}
          <span className={`shrink-0 text-2xs ${hasOverride ? "text-warning" : "text-subtle"}`}>
            {sourceLabel}
          </span>
        </>
      }
    >
      <div className="space-y-3 border-t border-border p-3">
        {hasOverride && (
          <div className="flex justify-end">
            <Button
              size="sm"
              variant="ghost"
              onClick={() => onChange(undefined)}
              title={t("providerEdit.resetOverridesTip")}
            >{t("providerEdit.resetOverrides")}</Button>
          </div>
        )}
        {fields.map(({ key, labelKey }) => {
          const row = resolveField(defaultAbility, overrideAbility, key);
          return (
            <div key={key} className="flex items-center gap-3">
              <div className="flex-1">
                <div className="text-xs font-medium">{t(labelKey)}</div>
                <div className="prose text-2xs text-subtle">
                  {row.effective === undefined
                    ? t("providerEdit.noDefaultOn")
                    : row.overridden
                      ? t(
                          row.effective
                            ? "providerEdit.fieldOverriddenOn"
                            : "providerEdit.fieldOverriddenOff",
                        )
                      : t(
                          row.effective
                            ? "providerEdit.fieldDefaultOn"
                            : "providerEdit.fieldDefaultOff",
                        )}
                </div>
              </div>
              <Switch
                checked={row.effective === true}
                onCheckedChange={(v) => setField(key, v)}
              />
              {row.overridden && (
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => setField(key, undefined)}
                  title={t("providerEdit.resetToDefault")}
                >{t("common.reset")}</Button>
              )}
            </div>
          );
        })}
        <div>
          <div className="text-xs font-medium">{t("providerEdit.limits")}</div>
          <div className="prose text-2xs text-subtle">
            {limitRow.effective
              ? t("providerEdit.limitsCtxOut", {
                  ctx: (limitRow.effective.context ?? 0).toLocaleString(),
                  out: (limitRow.effective.output ?? 0).toLocaleString(),
                })
                + t(
                    limitRow.overridden
                      ? "providerEdit.limitsOverriddenSuffix"
                      : "providerEdit.limitsDefaultSuffix",
                  )
              : t("providerEdit.noDefault")}
          </div>
          <div className="mt-2 grid grid-cols-2 gap-2">
            <Input
              size="sm"
              type="number"
              placeholder={t("providerEdit.ctxPlaceholder")}
              defaultValue={limitRow.effective?.context ?? ""}
              onBlur={(e) => {
                const v = Number(e.currentTarget.value);
                if (!Number.isFinite(v) || v <= 0) return;
                setLimit({
                  context: v,
                  output: limitRow.effective?.output ?? 0,
                });
              }}
            />
            <Input
              size="sm"
              type="number"
              placeholder={t("providerEdit.outPlaceholder")}
              defaultValue={limitRow.effective?.output ?? ""}
              onBlur={(e) => {
                const v = Number(e.currentTarget.value);
                if (!Number.isFinite(v) || v <= 0) return;
                setLimit({
                  context: limitRow.effective?.context ?? 0,
                  output: v,
                });
              }}
            />
          </div>
          {limitRow.overridden && (
            <div className="mt-2 flex justify-end">
              <Button size="sm" variant="ghost" onClick={() => setLimit(undefined)}>{t("common.reset")}</Button>
            </div>
          )}
        </div>
        <ModalityRow defaultAbility={defaultAbility} overrideAbility={overrideAbility} />
      </div>
    </Disclosure>
  );
}

/**
 * Read-only display of the resolved modalities matrix. Phase-1 scope: no
 * per-modality editor — the merged metadata (models.dev + bundled vendor
 * corrections) surfaces exactly as the writer will emit it. Users who need
 * to override can still do so via the endpoint's raw `model_abilities_json`
 * field; the override path round-trips through the same `Modalities` shape.
 */
function ModalityRow({
  defaultAbility,
  overrideAbility,
}: {
  defaultAbility?: ModelAbilities;
  overrideAbility?: ModelAbilities;
}) {
  const { t } = useTranslation();
  const row = resolveModalities(defaultAbility, overrideAbility);
  const input = row.effective?.input ?? [];
  const output = row.effective?.output ?? [];
  if (input.length === 0 && output.length === 0 && !row.overridden) return null;
  return (
    <div>
      <div className="text-xs font-medium">{t("providerEdit.modalities")}</div>
      <div className="prose text-2xs text-subtle">
        {row.effective
          ? t(row.overridden ? "providerEdit.overridden" : "providerEdit.default")
          : t("providerEdit.noDefault")}
      </div>
      <div className="mt-2 space-y-1.5">
        <ModalityChipLine label={t("providerEdit.input")} tokens={input} />
        <ModalityChipLine label={t("providerEdit.output")} tokens={output} />
      </div>
    </div>
  );
}

function ModalityChipLine({ label, tokens }: { label: string; tokens: Modality[] }) {
  return (
    <div className="flex items-center gap-2">
      <span className="text-2xs w-12 shrink-0 text-subtle">{label}</span>
      {tokens.length === 0 ? (
        <span className="text-2xs text-subtle">—</span>
      ) : (
        <div className="flex flex-wrap gap-1">
          {tokens.map((t) => (
            <span
              key={t}
              className="border border-border px-1.5 py-0.5 font-mono text-2xs text-muted"
            >
              {t}
            </span>
          ))}
        </div>
      )}
    </div>
  );
}

export { ModelAbilitiesDisclosure };
