import { useMemo, useState } from "react";
import { useMutation } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { RefreshCw } from "lucide-react";
import { endpointFetchModels, type EndpointInfo, type FetchedModels, type ModelAbilities } from "../../ipc";
import type { FormState } from "../../lib/providerForm";
import { Card } from "../controls/Card";
import { Button } from "../controls/Button";
import { Field } from "../controls/Field";
import { ErrorBanner } from "../feedback/ErrorBanner";
import { Input } from "../ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../ui/select";
import { ModelAbilitiesDisclosure } from "../controls/ModelAbilitiesDisclosure";

export function ProviderModelsCard({
  endpoint,
  form,
  onPatch,
}: {
  endpoint: EndpointInfo;
  form: FormState;
  onPatch: (patch: Partial<FormState>) => void;
}) {
  const { t } = useTranslation();
  const [fetched, setFetched] = useState<FetchedModels | null>(null);
  const fetchMut = useMutation({
    mutationFn: () => endpointFetchModels(endpoint.id),
    onSuccess: (res) => {
      setFetched(res);
      // Provider-declared hints for models the local models.dev chain can't
      // resolve: seed them into the override draft (existing user edits win)
      // so Save persists real limits instead of shipping name-only entries.
      const abilities = { ...form.model_abilities };
      for (const [id, hint] of Object.entries(res.hints)) {
        if (!abilities[id]) abilities[id] = hint;
      }
      onPatch({ models_available: res.models, model_abilities: abilities });
    },
  });

  const suggestions = fetched?.models ?? endpoint.models?.available ?? [];
  const tiers: { field: keyof FormState; labelKey: string }[] = [
    { field: "models_haiku", labelKey: "providerEdit.tierHaiku" },
    { field: "models_sonnet", labelKey: "providerEdit.tierSonnet" },
    { field: "models_opus", labelKey: "providerEdit.tierOpus" },
  ];

  const modelSelect = (
    value: string,
    onChange: (v: string) => void,
    placeholder: string,
  ) => (
    <ModelSelect
      value={value}
      onChange={onChange}
      suggestions={suggestions}
      placeholder={placeholder}
    />
  );

  // Every model id Nestra will write for this endpoint — drives the list
  // of Capabilities disclosures. We union tier + available + any model the
  // user has already saved overrides for, so an orphan override is always
  // editable.
  const allModelIds = useMemo(() => {
    const seen = new Set<string>();
    const push = (id: string) => {
      const t = id.trim();
      if (t) seen.add(t);
    };
    push(form.models_default);
    push(form.models_haiku);
    push(form.models_sonnet);
    push(form.models_opus);
    for (const id of form.models_available) push(id);
    for (const id of Object.keys(form.model_abilities)) push(id);
    return Array.from(seen);
  }, [
    form.models_default,
    form.models_haiku,
    form.models_sonnet,
    form.models_opus,
    form.models_available,
    form.model_abilities,
  ]);

  const patchAbility = (id: string, patch: ModelAbilities | undefined) => {
    const next = { ...form.model_abilities };
    if (patch === undefined) {
      // Reset the whole row: delete the key so the override disappears
      // entirely (an empty object would still count as "overridden").
      delete next[id];
    } else {
      next[id] = patch;
    }
    onPatch({ model_abilities: next });
  };
  // Fetched-but-unsaved models resolve against the fresh models.dev pull —
  // layer it under the saved defaults so the disclosure shows values for
  // them immediately (saved state wins; Save + reload re-reads it anyway).
  const defaults = { ...(fetched?.resolved ?? {}), ...(endpoint.model_abilities_defaults ?? {}) };

  return (
    <Card
      title={t("providerEdit.models")}
      hint={t("providerEdit.modelsHint")}
    >
      <div className="mb-3 flex items-center justify-between">
        <span className="text-xs text-muted">{t("providerEdit.availableFrom")}</span>
        <Button size="sm" variant="ghost" onClick={() => fetchMut.mutate()} loading={fetchMut.isPending}>
          <RefreshCw data-icon size={13} />
          {fetchMut.isPending ? t("providerEdit.fetching") : t("providerEdit.fetchModels")}
        </Button>
      </div>
      {fetchMut.isError && (
        <ErrorBanner variant="bare" className="mb-2">{t("providerEdit.fetchFailed")}</ErrorBanner>
      )}

      <div className="space-y-2">
        <Field label={t("providerEdit.defaultModel")}>
          {modelSelect(form.models_default, (v) => onPatch({ models_default: v }), t("providerEdit.modelPlaceholder"))}
        </Field>
        {tiers.map(({ field, labelKey }) => (
          <Field
            key={field}
            label={t(labelKey)}
            hint={t("providerEdit.tierHint", { model: form.models_default || t("providerEdit.default") })}
          >
            {modelSelect(
              form[field] as string,
              (v) => onPatch({ [field]: v } as Partial<FormState>),
              t("providerEdit.tierPlaceholder"),
            )}
          </Field>
        ))}
      </div>

      {allModelIds.length > 0 && (
        <div className="mt-4 border-t border-border pt-3">
          <div className="mb-2 flex items-center justify-between">
            <div>
              <div className="text-sm font-medium">{t("providerEdit.perModelCaps")}</div>
              <div className="prose text-xs text-muted leading-relaxed">
                {t("providerEdit.perModelCapsDesc")}
              </div>
            </div>
          </div>
          <div className="space-y-1">
            {allModelIds.map((id) => (
              <ModelAbilitiesDisclosure
                key={id}
                modelId={id}
                defaultAbility={defaults[id]}
                overrideAbility={form.model_abilities[id]}
                onChange={(next) => patchAbility(id, next)}
              />
            ))}
          </div>
        </div>
      )}
    </Card>
  );
}

function ModelSelect({
  value,
  onChange,
  suggestions,
  placeholder,
}: {
  value: string;
  onChange: (v: string) => void;
  suggestions: string[];
  placeholder: string;
}) {
  const { t } = useTranslation();
  // If current value isn't in suggestions, show text input. Otherwise Select.
  if (value && !suggestions.includes(value)) {
    return (
      <div className="flex gap-2">
        <Input
          value={value}
          onChange={(e) => onChange(e.target.value)}
          placeholder={placeholder}
          className="flex-1"
        />
        <Button size="sm" variant="ghost" onClick={() => suggestions.length > 0 && onChange(suggestions[0])}>{t("providerEdit.pick")}</Button>
      </div>
    );
  }
  return (
    <Select
      value={value}
      onValueChange={(v) => {
        // The "custom…" option is a MODE switch, not a model id — selecting
        // it must clear the field (which flips this component into the text
        // input above) instead of persisting the sentinel as the model.
        if (v === "__custom__") {
          onChange("");
        } else {
          onChange(v);
        }
      }}
    >
      <SelectTrigger>
        <SelectValue placeholder={placeholder} />
      </SelectTrigger>
      <SelectContent>
        <SelectItem value="__custom__">{t("providerEdit.customOption")}</SelectItem>
        {suggestions.map((m) => (
          <SelectItem key={m} value={m}>
            {m}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
