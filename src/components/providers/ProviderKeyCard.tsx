import { useTranslation } from "react-i18next";
import type { EndpointInfo } from "../../ipc";
import type { FormState } from "../../lib/providerForm";
import { Card } from "../controls/Card";
import { Button } from "../controls/Button";
import { FieldRow } from "../controls/Field";
import { Input } from "../ui/input";
import { Switch } from "../ui/switch";
import { StatusDot } from "../feedback/StatusDot";

export function ProviderKeyCard({
  endpoint,
  form,
  onPatch,
}: {
  endpoint: EndpointInfo;
  form: FormState;
  onPatch: (patch: Partial<FormState>) => void;
}) {
  const { t } = useTranslation();
  const showMasked = endpoint.has_api_key && !form.reveal_key && !form.clear_key;
  const status = endpoint.status;

  return (
    <Card
      title={t("providerEdit.apiKey")}
      hint={t("providerEdit.apiKeyHint")}
    >
      <div className="space-y-3">
        {showMasked ? (
          <div className="flex items-center gap-2">
            <Input
              readOnly
              className="flex-1"
              value="••••••••••••••••"
              suffix={
                <StatusDot
                  status={
                    status === "valid"
                      ? "ok"
                      : status === "invalid"
                      ? "missing"
                      : "unknown"
                  }
                  size={1.5}
                  title={t("providerEdit.statusTitle", { status })}
                />
              }
            />
            <Button size="sm" variant="secondary" onClick={() => onPatch({ reveal_key: true })}>{t("common.edit")}</Button>
          </div>
        ) : (
          <div className="flex items-center gap-2">
            <Input
              type="password"
              autoFocus
              className="flex-1"
              value={form.api_key}
              onChange={(e) => onPatch({ api_key: e.target.value })}
              placeholder={
                endpoint.has_api_key ? t("providerEdit.pasteNewKey") : t("providerEdit.pasteKey")
              }
            />
            {endpoint.has_api_key && (
              <Button
                size="sm"
                variant="ghost"
                onClick={() =>
                  onPatch({ reveal_key: false, api_key: "", clear_key: false })
                }
              >{t("common.cancel")}</Button>
            )}
          </div>
        )}
        {endpoint.has_api_key && (
          <FieldRow
            label={showMasked ? t("providerEdit.clearOnSave") : t("providerEdit.clearOnSaveAlt")}
          >
            <Switch
              checked={form.clear_key}
              onCheckedChange={(v) => onPatch({ clear_key: v })}
            />
          </FieldRow>
        )}
        <FreshnessHint endpoint={endpoint} />
      </div>
    </Card>
  );
}

function FreshnessHint({ endpoint }: { endpoint: EndpointInfo }) {
  const { t } = useTranslation();
  if (!endpoint.last_validated_at) return null;
  const ageMin = Math.floor((Date.now() - endpoint.last_validated_at) / 60_000);
  const label = ageMin < 1 ? t("providerEdit.validatedJustNow") : t("providerEdit.validatedMinAgo", { n: ageMin });
  const stale = ageMin > 5;
  return (
    <div className={`text-xs ${stale ? "text-warning" : "text-subtle"}`}>
      {stale && "⚠ "}{t("providerEdit.validatedPrefix", { label })}
    </div>
  );
}
