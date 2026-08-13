import { useTranslation } from "react-i18next";
import type { EndpointInfo, QuotaItem } from "../../ipc";
import type { QuotaRefresh } from "../../lib/quotaRefresh";
import { useUI } from "../../stores/ui";
import {
  BUILTIN_LABEL,
  BUILTIN_OPTIONS,
  planToSelectValue,
} from "../../lib/quota";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "../ui/dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../ui/select";
import { Switch } from "../ui/switch";
import { FieldRow } from "./Field";
import { SectionLabel } from "../layout/PageHeader";
import { ErrorBanner } from "../feedback/ErrorBanner";
import { KeepAliveEditor } from "./KeepAlivePopover";
import { SettingsSelectRow } from "./SettingsSelectRow";
import { QuotaExtractorFields } from "./QuotaExtractorFields";
import { OpencodeCredsFields } from "./OpencodeCredsFields";

/**
 * The quota settings dialog: query-plan picker, custom-extractor editor,
 * OpenCode Go creds, preview-window toggles, auto-refresh, and the keep-alive
 * config. All state comes from the shared `useQuotaRefresh` machine (the
 * card header + bars consume the same instance).
 */
export function QuotaSettingsDialog({
  endpoint,
  open,
  onOpenChange,
  rf,
  targetItems,
  currentPlan,
}: {
  endpoint: EndpointInfo;
  open: boolean;
  onOpenChange: (v: boolean) => void;
  rf: QuotaRefresh;
  targetItems: QuotaItem[];
  currentPlan: string | null;
}) {
  const { t } = useTranslation();
  const {
    quotaAuto: auto,
    quotaIntervalSec: intervalSec,
    setQuotaAuto,
    setQuotaIntervalSec,
  } = useUI();
  const {
    cfg,
    plan,
    planActive,
    provisioned,
    canArm,
    availableBuiltin,
    patch,
    toggle,
    setPlan,
    targetQuotaName,
    previewWindows,
    togglePreviewWindow,
  } = rf;

  // Keep-alive ping config is a small split row: protocol left, model right.
  const protocols = endpoint.protocols.map((p) => p.protocol);

  const statusText = cfg.last_status;
  const isResetting = statusText === "resetting";
  const isRetrying = statusText?.startsWith("retrying");
  const isHardError = !!statusText && statusText !== "ok" && !isResetting && !isRetrying;

  return (
    <Dialog open={open} onOpenChange={(v) => !v && onOpenChange(false)}>
      <DialogContent size="lg">
        <DialogHeader>
          <DialogTitle>{t("quota.settingsTitle")}</DialogTitle>
          <DialogDescription>
            {t("quota.settingsDesc")}
          </DialogDescription>
        </DialogHeader>
        <DialogBody className="space-y-4">
          {/* Query plan — the single "how is quota queried" choice, picked
              from a dropdown so it scales as more built-in fetchers land.
              Any built-in works for any endpoint (the fetchers use hardcoded
              URLs); the host-matched one is flagged "(recommended)". Any
              change re-locks the gate (re-verify required). */}
          <div className="space-y-2">
            <FieldRow label={<SectionLabel inline>{t("quota.queryPlan")}</SectionLabel>}>
              <Select value={planToSelectValue(plan)} onValueChange={setPlan}>
                <SelectTrigger size="sm" className="w-56 font-mono">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="none">{t("quota.planNone")}</SelectItem>
                  {BUILTIN_OPTIONS.map((o) => (
                    <SelectItem key={o.value} value={o.value}>
                      {o.label}
                      {availableBuiltin === o.value ? t("quota.recommended") : ""}
                    </SelectItem>
                  ))}
                  <SelectItem value="custom">{t("quota.planCustom")}</SelectItem>
                </SelectContent>
              </Select>
            </FieldRow>
            {plan.source === "preset" && (
              <p className="prose text-xs text-subtle leading-relaxed">
                {t("quota.usingBuiltin", { kind: BUILTIN_LABEL[plan.kind] })}
              </p>
            )}
            {plan.source === "none" && (
              <p className="prose text-xs text-subtle leading-relaxed">
                {t("quota.planNoneHint")}
              </p>
            )}
            {/* Verification state — surfaces the gate clearly. */}
            <p className="prose text-xs text-subtle leading-relaxed">
              {planActive
                ? provisioned
                  ? t("quota.verified")
                  : t("quota.notVerified")
                : null}
            </p>
          </div>

          {/* Custom extractor fields — rendered only when the plan is Custom.
              Edits go straight onto the plan and clear the verified state so
              a re-verify is required (the bars re-lock behind the dialog). */}
          {plan.source === "custom" && (
            <QuotaExtractorFields
              endpointId={endpoint.id}
              extractor={{ enabled: true, url: plan.url, headers: plan.headers, unit: plan.unit, fields: plan.fields }}
              onChange={(ex) =>
                patch({
                  query_plan: { source: "custom", ...ex },
                  provisioned: false,
                })
              }
            />
          )}

          {/* OpenCode Go dashboard credentials — the usage query scrapes the
              authenticated dashboard, so it needs a browser session cookie
              (the `auth` cookie from opencode.ai) + the workspace ID. Shown
              only when the plan is the OpenCode Go built-in. */}
          {plan.source === "preset" && plan.kind === "opencode_go" && (
            <OpencodeCredsFields endpointId={endpoint.id} />
          )}

          {currentPlan && (
            <FieldRow label={<SectionLabel inline>{t("quota.plan")}</SectionLabel>}>
              <span className="font-mono text-xs text-fg">{currentPlan}</span>
            </FieldRow>
          )}

          {/* Preview windows — which fetched windows the provider card shows.
              Multi-toggle: every fetched window gets a switch; enabled ones
              render as bars on the card. Display-only, separate from the
              keep-alive target picker below. */}
          {planActive && targetItems.length > 0 && (
            <div className="border-t border-border pt-3 space-y-2">
              <FieldRow label={<SectionLabel inline>{t("quota.previewWindows")}</SectionLabel>}>
                <span className="text-xs text-subtle">
                  {previewWindows.length === 0
                    ? t("quota.previewNone")
                    : t("quota.previewOnCard", { n: previewWindows.length })}
                </span>
              </FieldRow>
              <ul className="space-y-1">
                {targetItems.map((it) => {
                  const on = previewWindows.includes(it.name);
                  return (
                    <li key={it.name} className="flex items-center gap-2">
                      <Switch
                        checked={on}
                        onCheckedChange={(v) => togglePreviewWindow(it.name, v)}
                      />
                      <span className="font-mono text-xs text-fg">{it.name}</span>
                      {it.resets_in && (
                        <span className="text-xs text-subtle">{t("quota.resetsIn", { n: it.resets_in })}</span>
                      )}
                    </li>
                  );
                })}
              </ul>
              <p className="prose text-xs text-subtle leading-relaxed">
                {t("quota.previewHint")}
              </p>
            </div>
          )}

          <div className="space-y-2">
            <FieldRow label={t("quota.autoRefresh")}>
              <div className="flex items-center gap-3">
                <Switch
                  checked={auto}
                  onCheckedChange={setQuotaAuto}
                  disabled={!canArm}
                />
                {auto && (
                  <div className="flex items-center gap-1.5">
                    <span className="text-xs text-subtle">{t("quota.every")}</span>
                    <Select
                      value={String(intervalSec)}
                      onValueChange={(v) => setQuotaIntervalSec(Number(v))}
                    >
                      <SelectTrigger size="sm" className="w-20 font-mono">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        {[10, 20, 30, 40, 50, 60].map((s) => (
                          <SelectItem key={s} value={String(s)}>
                            {s}s
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  </div>
                )}
              </div>
            </FieldRow>
            <p className="prose text-xs text-subtle leading-relaxed">
              {canArm
                ? t("quota.autoHintOn")
                : t("quota.autoHintOff")}
            </p>
          </div>

          <div className="border-t border-border pt-3 space-y-3">
            <FieldRow label={t("quota.keepAlive")}>
              <Switch
                checked={cfg.enabled}
                onCheckedChange={toggle}
                disabled={!planActive}
              />
            </FieldRow>
            {!planActive ? (
              <p className="prose text-xs text-subtle leading-relaxed">
                {t("quota.keepAliveLocked")}
              </p>
            ) : !provisioned ? (
              <p className="prose text-xs text-subtle leading-relaxed">
                {t("quota.keepAliveUnverified")}
              </p>
            ) : cfg.enabled ? (
              <div className="space-y-2.5">
                <SettingsSelectRow
                  label={t("quota.targetQuota")}
                  value={targetQuotaName ?? "__none__"}
                  onChange={(v) =>
                    patch({ target_quota_name: v === "__none__" ? null : v })
                  }
                  disabled={targetItems.length === 0}
                  placeholder={t("quota.noQuotaItems")}
                  options={
                    targetItems.length === 0
                      ? [{ value: "__none__", label: t("quota.noQuotaItems") }]
                      : targetItems.map((it) => ({
                          value: it.name,
                          label: it.resets_in
                            ? `${it.name} · ${t("quota.resetsIn", { n: it.resets_in })}`
                            : it.name,
                        }))
                  }
                />
                <SettingsSelectRow
                  label={t("quota.protocol")}
                  value={cfg.protocol ?? (protocols[0] ?? "")}
                  onChange={(v) => patch({ protocol: v === "" ? null : v })}
                  placeholder={t("quota.protocol")}
                  options={protocols.map((p) => ({ value: p, label: p }))}
                />
                <SettingsSelectRow
                  label={t("quota.checkEvery")}
                  value={String(cfg.check_rate_secs || 180)}
                  onChange={(v) => patch({ check_rate_secs: Number(v) })}
                  options={[60, 180, 300, 600, 1800].map((s) => ({
                    value: String(s),
                    label: s < 3600 ? `${s / 60}m` : `${s / 3600}h`,
                  }))}
                />
                <SettingsSelectRow
                  label={t("quota.resetGrace")}
                  value={String(cfg.reset_grace_secs || 180)}
                  onChange={(v) => patch({ reset_grace_secs: Number(v) })}
                  options={[60, 120, 180, 300, 600].map((s) => ({
                    value: String(s),
                    label: s < 60 ? `${s}s` : `${s / 60}m`,
                  }))}
                />
                <SettingsSelectRow
                  label={t("quota.model")}
                  value={cfg.model ?? "__default__"}
                  onChange={(v) => patch({ model: v === "__default__" ? null : v })}
                  placeholder={t("quota.defaultModel")}
                  options={[
                    { value: "__default__", label: t("quota.defaultModel") },
                    ...(endpoint.models?.available ?? []).map((m) => ({
                      value: m,
                      label: m,
                    })),
                  ]}
                />
                {isHardError ? (
                  <ErrorBanner severity="error">
                    <pre className="whitespace-pre-wrap break-all font-mono">{statusText}</pre>
                  </ErrorBanner>
                ) : isRetrying ? (
                  <ErrorBanner severity="warn">
                    <pre className="whitespace-pre-wrap break-all font-mono">{statusText}</pre>
                    <span className="block text-xs">
                      {t("quota.transient")}
                    </span>
                  </ErrorBanner>
                ) : (
                  <p className="prose text-xs text-subtle leading-relaxed">
                    {isResetting
                      ? t("quota.resetting")
                      : statusText && statusText !== "ok"
                        ? statusText
                        : t("quota.armed")}
                  </p>
                )}
                {/* The request editor (redacted curl + test ping + copy)
                    lives in settings, not the popover — the popover is
                    status-only. */}
                <KeepAliveEditor endpointId={endpoint.id} />
              </div>
            ) : (
              <p className="prose text-xs text-subtle leading-relaxed">
                {t("quota.keepAliveHint")}
              </p>
            )}
          </div>
        </DialogBody>
      </DialogContent>
    </Dialog>
  );
}
