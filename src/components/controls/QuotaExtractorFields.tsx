import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Trans, useTranslation } from "react-i18next";
import {
  endpointFetchQuota,
  type QuotaExtractorConfig,
  type QuotaExtractorFields,
} from "../../ipc";
import { qk } from "../../lib/queries";
import { fmtMoney } from "../../lib/format";
import { FieldRow } from "./Field";
import { Input } from "../ui/input";
import { Textarea } from "../ui/textarea";
import { Button } from "./Button";

/// Custom-extractor field editor. Rendered only when the query plan source
/// is "Custom" — there is no per-section enable toggle here, being rendered
/// IS being enabled. Every edit persists immediately via `onChange` (which the
/// caller wraps onto the plan + clears the verified state), so the persisted
/// plan always matches the form — the **Test** button runs the real fetch and
/// shows a concise ✓/✗ so the user can assemble the request and confirm it
/// works without leaving the dialog. The extractor is `is_balance` shaped — no
/// reset window, keep-alive never pings it.
export function QuotaExtractorFields({
  endpointId,
  extractor,
  onChange,
}: {
  endpointId: string;
  extractor: QuotaExtractorConfig;
  onChange: (ex: QuotaExtractorConfig) => void;
}) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const ex: QuotaExtractorConfig = {
    enabled: true,
    url: extractor.url,
    headers: extractor.headers ?? {},
    unit: extractor.unit ?? null,
    fields: extractor.fields ?? {},
  };
  const patch = (p: Partial<QuotaExtractorConfig>) => onChange({ ...ex, ...p });
  const patchField = (key: keyof QuotaExtractorFields, value: string) =>
    patch({ fields: { ...ex.fields, [key]: value || undefined } });

  // Inline test result: null = not run, {ok,msg} = last outcome. A successful
  // Test also provisions the endpoint (the fetch side-effect) and refreshes
  // the settings blob so the gate reopens without a separate verify click.
  const [testing, setTesting] = useState(false);
  const [result, setResult] = useState<{ ok: boolean; msg: string } | null>(null);
  const runTest = async () => {
    setTesting(true);
    setResult(null);
    try {
      const res = await endpointFetchQuota(endpointId);
      await qc.invalidateQueries({ queryKey: qk.quotaRefresh() });
      if (res.ok && res.items.length > 0) {
        const it = res.items[0];
        const val = it.is_balance
          ? it.remaining != null
            ? fmtMoney(it.remaining, it.unit)
            : `${Math.round(it.pct)}%`
          : `${Math.round(it.pct)}%`;
        setResult({ ok: true, msg: `${it.name}: ${val}` });
      } else {
        setResult({ ok: false, msg: res.error ?? t("quota.noDataReturned") });
      }
    } catch (e) {
      setResult({ ok: false, msg: e instanceof Error ? e.message : String(e) });
    } finally {
      setTesting(false);
    }
  };

  // headers map <-> "Key: Value" textarea lines.
  const headersText = Object.entries(ex.headers ?? {})
    .map(([k, v]) => `${k}: ${v}`)
    .join("\n");
  const setHeadersText = (text: string) => {
    const headers: Record<string, string> = {};
    for (const line of text.split("\n")) {
      const idx = line.indexOf(":");
      if (idx > 0) headers[line.slice(0, idx).trim()] = line.slice(idx + 1).trim();
    }
    patch({ headers });
  };

  const field = (key: keyof QuotaExtractorFields, label: string, placeholder: string) => (
    <FieldRow label={label}>
      <Input
        size="sm"
        className="w-64 font-mono"
        value={ex.fields[key] ?? ""}
        placeholder={placeholder}
        onChange={(e) => patchField(key, e.target.value)}
      />
    </FieldRow>
  );

  return (
    <div className="border-t border-border pt-3 space-y-2.5">
      <FieldRow label={t("quota.extractorUrl")}>
        <Input
          size="sm"
          className="w-96 font-mono"
          value={ex.url}
          placeholder={t("quota.extractorUrlPlaceholder")}
          onChange={(e) => patch({ url: e.target.value })}
        />
      </FieldRow>
      <FieldRow label={t("quota.extractorHeaders")}>
<Textarea
          value={headersText}
          onChange={(e) => setHeadersText(e.target.value)}
          placeholder={t("quota.extractorHeadersPlaceholder", { apiKey: "{{apiKey}}" })}
          size="sm"
          rows={2}
          className="w-96 font-mono"
        />
      </FieldRow>
      <FieldRow label={t("quota.extractorUnit")}>
        <Input
          size="sm"
          className="w-24 font-mono"
          value={ex.unit ?? ""}
          placeholder={t("quota.extractorUnitPlaceholder")}
          onChange={(e) => patch({ unit: e.target.value || null })}
        />
      </FieldRow>
      {field("name", t("quota.extractorName"), t("quota.extractorNamePlaceholder"))}
      {field("used", t("quota.extractorUsed"), t("quota.extractorUsedPlaceholder"))}
      {field("remaining", t("quota.extractorRemaining"), t("quota.extractorRemainingPlaceholder"))}
      {field("total", t("quota.extractorTotal"), t("quota.extractorTotalPlaceholder"))}
      {field("unit", t("quota.extractorUnitField"), t("quota.extractorUnitFieldPlaceholder"))}
      <p className="prose text-xs text-subtle leading-relaxed">
        <Trans
          i18nKey="quota.jsonPathHint"
          values={{ baseUrl: "{{baseUrl}}", apiKey: "{{apiKey}}" }}
          components={{ code: <code className="font-mono" /> }}
        />
      </p>
      <div className="flex items-center justify-end gap-3">
        <Button variant="ghost" size="sm" disabled={testing} loading={testing} onClick={runTest}>
          {testing ? t("common.testing") : t("quota.testQuery")}
        </Button>
        {result && (
          <span className={`font-mono text-xs ${result.ok ? "text-success" : "text-danger"}`}>
            {result.ok ? "✓" : "✗"} {result.msg}
          </span>
        )}
      </div>
    </div>
  );
}
