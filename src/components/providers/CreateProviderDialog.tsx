import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Search } from "lucide-react";
import { providerPresets, type BuiltinKind, type ProviderPreset } from "../../ipc";
import { cancellableInvoke, useGuard } from "../../lib/guard";
import { Field } from "../controls/Field";
import { Input } from "../ui/input";
import { Button } from "../controls/Button";
import { Badge } from "../ui/badge";
import { InsetBlock } from "../display/InsetBlock";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "../ui/dialog";

/// Inputs collected by the new create flow. `apiKey` is optional: present
/// for the single-step preset+key path, absent for the custom path (the
/// user finishes setup on the edit page).
export type CreateInput = {
  display_name: string;
  protocols: { protocol: string; base_url: string }[];
  apiKey?: string;
  /// Built-in quota query inherited from the chosen preset (null for custom
  /// or presets with no built-in fetcher). Stamped as the new endpoint's
  /// query plan so the Quota page + keep-alive work without extra config.
  quota_query: BuiltinKind | null;
};

export function slugify(name: string): string {
  return name
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "")
    .slice(0, 64);
}

export function CreateProviderDialog({
  onCancel,
  onSubmit,
  pending,
  error,
  existingIds,
}: {
  onCancel: () => void;
  onSubmit: (input: CreateInput) => void;
  pending: boolean;
  error: string | null;
  existingIds: string[];
}) {
  const { t } = useTranslation();
  const [displayName, setDisplayName] = useState("");
  const [presets, setPresets] = useState<ProviderPreset[]>([]);
  const [presetId, setPresetId] = useState<string | null>(null);
  const [apiKey, setApiKey] = useState("");
  const [query, setQuery] = useState("");
  /// Tracks whether the name field was last auto-filled by preset
  /// selection. Lets the user override it without being clobbered.
  const [nameAutoFilled, setNameAutoFilled] = useState(false);
  const guard = useGuard();

  // Load presets once on mount. cancellableInvoke discards the result if the
  // dialog unmounted before the fetch landed (no set-state-after-unmount) and
  // swallows any failure — presets just stays empty.
  useEffect(() => {
    cancellableInvoke(guard, providerPresets).then((res) => {
      if (!res.stale) setPresets(res.value);
    });
  }, [guard]);

  const preset = presets.find((p) => p.id === presetId) ?? null;
  const isCustom = presetId === "custom";

  // Filter presets by name or base URL. Case-insensitive substring match.
  const namedPresets = useMemo(() => {
    const q = query.trim().toLowerCase();
    const named = presets.filter((p) => p.id !== "custom");
    if (!q) return named;
    return named.filter(
      (p) =>
        p.display_name.toLowerCase().includes(q) ||
        p.protocols.some((proto) => proto.base_url.toLowerCase().includes(q)),
    );
  }, [presets, query]);

  // The custom preset always shows (filtered only by an explicit "custom"
  // search), so the manual path is reachable even with a narrow filter.
  const showCustom = !query.trim() || "custom".includes(query.trim().toLowerCase());

  const selectPreset = (p: ProviderPreset) => {
    setPresetId(p.id);
    // Auto-fill the display name from the preset when the field is empty
    // OR was previously auto-filled (so switching presets updates it, but a
    // user-typed name is preserved).
    if (!displayName.trim() || nameAutoFilled) {
      setDisplayName(p.display_name);
      setNameAutoFilled(true);
    }
  };

  const previewId = displayName.trim() ? slugify(displayName) || "provider" : null;
  const idClash = previewId ? existingIds.includes(previewId) : false;

  // Validation: a name is always required. For a preset (non-custom) the key
  // is also required (the whole point of the single-step flow). For custom,
  // no key is collected.
  const canSubmit =
    displayName.trim().length > 0 && (isCustom || apiKey.trim().length > 0);

  const submit = () => {
    if (!canSubmit || pending) return;
    onSubmit({
      display_name: displayName.trim(),
      protocols: preset?.protocols ?? [],
      apiKey: isCustom ? undefined : apiKey.trim(),
      quota_query: preset?.quota_query ?? null,
    });
  };

  return (
    <Dialog open onOpenChange={(o) => !o && onCancel()}>
      <DialogContent size="xl">
        <DialogHeader>
          <DialogTitle>
            {displayName.trim() || <span className="text-subtle">{t("providers.dialogTitleNew")}</span>}
          </DialogTitle>
          {previewId && (
            <div className="-mt-1 font-mono text-xs text-subtle">
              {previewId}
              {idClash && <span className="text-warning">{t("providers.idTaken")}</span>}
            </div>
          )}
          <DialogDescription>
            {t("providers.dialogDesc")}
          </DialogDescription>
        </DialogHeader>

        <DialogBody className="space-y-4">
          {/* Search + preset grid */}
          <Field label={t("providers.preset")}>
            <Input
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder={t("providers.searchPlaceholder")}
              prefix={<Search data-icon size={13} />}
            />
          </Field>

          <div className="grid max-h-64 grid-cols-2 gap-2 overflow-y-auto sm:grid-cols-3">
            {namedPresets.map((p) => (
              <PresetTile
                key={p.id}
                preset={p}
                selected={presetId === p.id}
                onClick={() => selectPreset(p)}
              />
            ))}
            {showCustom && (
              <PresetTile
                preset={{
                  id: "custom",
                  display_name: t("providers.customPreset"),
                  protocols: [],
                  default_model: null,
                  quota_query: null,
                }}
                selected={isCustom}
                onClick={() => {
                  setPresetId("custom");
                  // Custom has no display-name suggestion; keep whatever the
                  // user typed but clear the auto-fill flag.
                  setNameAutoFilled(false);
                }}
              />
            )}
          </div>

          {/* API key — only for a non-custom preset. */}
          {preset && !isCustom && (
            <Field
              label={t("providers.apiKey")}
              hint={
                preset.protocols.length > 0
                  ? t("providers.validatedAgainst", { url: preset.protocols[0].base_url })
                  : undefined
              }
            >
              <Input
                type="password"
                value={apiKey}
                onChange={(e) => setApiKey(e.target.value)}
                placeholder={t("providers.pasteKey")}
                autoFocus
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    submit();
                  }
                }}
              />
            </Field>
          )}

          {isCustom && (
            <InsetBlock pad="p-2.5" className="text-xs text-subtle">
              {t("providers.customHint")}
            </InsetBlock>
          )}

          {/* Display name */}
          <Field label={t("providers.displayName")}>
            <Input
              value={displayName}
              onChange={(e) => {
                setDisplayName(e.target.value);
                setNameAutoFilled(false);
              }}
              placeholder={t("providers.namePlaceholder")}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  submit();
                }
              }}
            />
          </Field>

          {error && <div className="text-xs text-danger">{error}</div>}
        </DialogBody>

        <DialogFooter>
          <Button variant="ghost" onClick={onCancel} disabled={pending}>{t("common.cancel")}</Button>
          <Button variant="primary" loading={pending} disabled={!canSubmit} onClick={submit}>
            {isCustom ? t("providers.createProvider") : t("providers.createValidate")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/// One flat selectable tile in the preset grid. Surface-field well, 0 radius,
/// no shadow — DESIGN-compliant. Selected tile lifts its border to the accent
/// pair. Shows the display name (primary) + the first base URL (mono, subtle)
/// so the user can recognise the provider without expanding a dropdown.
function PresetTile({
  preset,
  selected,
  onClick,
}: {
  preset: ProviderPreset;
  selected: boolean;
  onClick: () => void;
}) {
  const { t } = useTranslation();
  const isCustom = preset.id === "custom";
  const firstUrl = preset.protocols[0]?.base_url;
  const protoCount = preset.protocols.length;
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={selected}
      className={`surface-field flex h-full min-h-[3.5rem] flex-col items-start gap-0.5 p-2.5 text-left transition-[border-color] duration-fast hover:border-border-strong ${
        selected ? "border-accent-border" : ""
      }`}
    >
      <span className="flex w-full items-center justify-between gap-1">
        <span className="truncate text-sm font-medium">{preset.display_name}</span>
        {!isCustom && protoCount > 1 && (
          <Badge tone="neutral" variant="soft">
            {protoCount}
          </Badge>
        )}
      </span>
      <span className="w-full truncate font-mono text-2xs text-subtle">
        {isCustom ? t("providers.configureManually") : (firstUrl ?? "—")}
      </span>
    </button>
  );
}
