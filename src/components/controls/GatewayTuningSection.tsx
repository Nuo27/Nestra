import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { gatewayTuningGet, gatewayTuningSet, type GatewayTuning } from "../../ipc";
import { Button } from "./Button";
import { Card } from "./Card";
import { Input } from "../ui/input";
import { useUI } from "../../stores/ui";

/// One tunable field: its key on `GatewayTuning`, the i18n suffix for the
/// label, and the legal range (mirrors the Rust-side clamp — the backend
/// clamps again on save, this only steers the placeholder).
interface TuningField {
  key: keyof GatewayTuning;
  suffix: string;
  min: number;
  max: number;
}

const TIMEOUT_FIELDS: TuningField[] = [
  { key: "headers_timeout_secs", suffix: "headers", min: 1, max: 300 },
  { key: "first_event_timeout_secs", suffix: "firstEvent", min: 1, max: 300 },
  { key: "stream_silence_timeout_secs", suffix: "silence", min: 0, max: 600 },
  { key: "buffered_body_timeout_secs", suffix: "buffered", min: 1, max: 1800 },
  { key: "request_deadline_secs", suffix: "deadline", min: 30, max: 3600 },
];

const BREAKER_FIELDS: TuningField[] = [
  { key: "breaker_failure_threshold", suffix: "threshold", min: 1, max: 20 },
  { key: "breaker_recovery_wait_secs", suffix: "recovery", min: 5, max: 900 },
  { key: "breaker_success_threshold", suffix: "success", min: 1, max: 10 },
  { key: "breaker_error_rate_pct", suffix: "errorRate", min: 0, max: 100 },
  { key: "breaker_min_requests", suffix: "minRequests", min: 1, max: 50 },
];

/// Gateway tuning section — the Settings surface for the gateway's timeouts
/// and circuit-breaker parameters. Saves
/// hot-apply to the next request (no gateway restart): the backend persists
/// to `setting_kv` and writes the shared in-memory slot in one command.
export function GatewayTuningSection() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useUI((s) => s.pushToast);
  const q = useQuery({ queryKey: ["gateway-tuning"], queryFn: gatewayTuningGet });
  const [draft, setDraft] = useState<GatewayTuning | null>(null);
  // Any unsaved edit parks the server-follow: after a save the backend may
  // CLAMP values, and the refetched (clamped) truth must flow back into the
  // form — otherwise the display keeps claiming an unclamped value stuck.
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (q.data && !dirty) setDraft(q.data);
  }, [q.data, dirty]);

  if (!draft) return null;

  const save = async () => {
    setSaving(true);
    try {
      await gatewayTuningSet(draft);
      setDirty(false);
      await qc.invalidateQueries({ queryKey: ["gateway-tuning"] });
      toast(t("settings.gatewayTuningSavedToast"), "success");
    } catch {
      toast(t("settings.gatewayTuningSaveFailedToast"), "error");
    } finally {
      setSaving(false);
    }
  };

  const resetDefaults = () => {
    setDirty(true);
    setDraft({
      headers_timeout_secs: 30,
      first_event_timeout_secs: 30,
      stream_silence_timeout_secs: 120,
      buffered_body_timeout_secs: 600,
      request_deadline_secs: 600,
      breaker_failure_threshold: 3,
      breaker_recovery_wait_secs: 60,
      breaker_success_threshold: 2,
      breaker_error_rate_pct: 60,
      breaker_min_requests: 10,
    });
  };

  const row = (f: TuningField) => (
    <label key={f.key} className="flex flex-col gap-1 min-w-0">
      <span className="text-xs text-fg">{t(`settings.gatewayTuning_${f.suffix}`)}</span>
      <Input
        size="sm"
        type="number"
        min={f.min}
        max={f.max}
        value={draft[f.key]}
        onChange={(e) => {
          // Number("") === 0 passes on purpose so a cleared field stays
          // editable; the blur handler below restores/clamps it.
          const v = Number(e.currentTarget.value);
          if (!Number.isFinite(v)) return;
          setDirty(true);
          setDraft({ ...draft, [f.key]: v });
        }}
        onBlur={(e) => {
          let v = Number(e.currentTarget.value);
          if (!Number.isFinite(v) || e.currentTarget.value.trim() === "") v = f.min;
          const clamped = Math.min(f.max, Math.max(f.min, Math.round(v)));
          setDirty(true);
          setDraft({ ...draft, [f.key]: clamped });
        }}
      />
      <span className="text-[11px] leading-tight text-subtle">
        {t(`settings.gatewayTuning_${f.suffix}Hint`, { min: f.min, max: f.max })}
      </span>
    </label>
  );

  return (
    <Card
      title={t("settings.gatewayTuning")}
      description={t("settings.gatewayTuningDesc")}
      info={t("settings.gatewayTuningInfo")}
    >
      <div className="space-y-4">
        <div>
          <div className="text-xs font-medium text-fg mb-2">
            {t("settings.gatewayTuningTimeoutsGroup")}
          </div>
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-3">
            {TIMEOUT_FIELDS.map(row)}
          </div>
        </div>
        <div>
          <div className="text-xs font-medium text-fg mb-2">
            {t("settings.gatewayTuningBreakerGroup")}
          </div>
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-3">
            {BREAKER_FIELDS.map(row)}
          </div>
        </div>
        <div className="flex items-center gap-2">
          <Button size="sm" onClick={save} disabled={saving}>
            {t("settings.gatewayTuningSave")}
          </Button>
          <Button size="sm" variant="secondary" onClick={resetDefaults}>
            {t("settings.gatewayTuningReset")}
          </Button>
        </div>
      </div>
    </Card>
  );
}
