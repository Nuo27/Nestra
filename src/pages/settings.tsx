import { useEffect } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { autostartIsEnabled, autostartSet, settingDelete, settingGet, settingSet } from "../ipc";
import { qk } from "../lib/queries";
import { Page } from "../components/layout/Page";
import { Card } from "../components/controls/Card";
import { PageHeader } from "../components/layout/PageHeader";
import { SegmentedControl } from "../components/controls/SegmentedControl";
import { Button } from "../components/controls/Button";
import { Switch } from "../components/ui/switch";
import { confirmDialog } from "../components/controls/ConfirmDialog";
import { ErrorBanner } from "../components/feedback/ErrorBanner";
import { Skeleton } from "../components/ui/skeleton";
import { DiagnosticsSection } from "../components/controls/DiagnosticsSection";
import { GatewayTuningSection } from "../components/controls/GatewayTuningSection";
import { useUI, type ThemePref } from "../stores/ui";

type Cadence = "on-launch" | "manual";

interface Settings {
  detection_cadence: Cadence;
  log_retention_days: 7 | 30 | 90;
}

const DEFAULTS: Settings = {
  detection_cadence: "on-launch",
  log_retention_days: 30,
};

/// localStorage key for the persisted React Query cache (mirrors app.tsx).
const QUERY_CACHE_KEY = "nestra-query-cache";

export function SettingsPage() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useUI((s) => s.pushToast);
  const theme = useUI((s) => s.theme);
  const setTheme = useUI((s) => s.setTheme);
  const language = useUI((s) => s.language);
  const setLanguage = useUI((s) => s.setLanguage);
  const persistQueryCache = useUI((s) => s.persistQueryCache);
  const setPersistQueryCache = useUI((s) => s.setPersistQueryCache);
  const q = useQuery({
    queryKey: qk.settings(),
    queryFn: () => settingGet("app"),
  });

  // Launch-at-login is OS-backed (registry/plist via tauri-plugin-autostart),
  // not a setting_kv row — a separate query + toggle keeps the two worlds
  // apart.
  const autostartQ = useQuery({
    queryKey: qk.autostart(),
    queryFn: autostartIsEnabled,
  });
  const toggleAutostart = async (v: boolean) => {
    try {
      await autostartSet(v);
      qc.invalidateQueries({ queryKey: qk.autostart() });
      toast(t("settings.savedToast"), "success");
    } catch {
      toast(t("settings.notSavedToast"), "error");
    }
  };

  // The tray can toggle autostart while this page is open — listen for the
  // backend's `autostart-changed` event and refresh the switch so it never
  // sticks on a stale state. Same cancelled-flag pattern as sessions.tsx.
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    listen("autostart-changed", () => {
      qc.invalidateQueries({ queryKey: qk.autostart() });
    })
      .then((fn) => {
        if (cancelled) {
          fn(); // resolved after unmount — release immediately
        } else {
          unlisten = fn;
        }
      })
      .catch((e) => console.error("autostart-changed listen failed", e));
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [qc]);

  const settings: Settings = { ...DEFAULTS, ...((q.data as Partial<Settings>) ?? {}) };

  async function update<K extends keyof Settings>(key: K, value: Settings[K]) {
    // Never write until the current settings have loaded: a whole-row write
    // built on an empty cache would overwrite the backend's real config
    // (e.g. log_retention_days) with DEFAULTS.
    if (!q.isSuccess) return;
    // Merge from the LATEST cache state, not this render's closure: rapid
    // consecutive edits each see the previous write instead of overwriting
    // it with a stale snapshot.
    const latest = qc.getQueryData<Partial<Settings>>(qk.settings()) ?? {};
    const prev = latest;
    const next = { ...DEFAULTS, ...latest, [key]: value };
    // Optimistic: reflect the pick in the query cache immediately so the
    // segment highlights the instant it's clicked; roll back if the write
    // fails so the cache never claims an unsaved value.
    qc.setQueryData<Partial<Settings>>(qk.settings(), next);
    try {
      await settingSet("app", next);
      qc.invalidateQueries({ queryKey: qk.settings() });
      toast(t("settings.savedToast"), "success");
    } catch {
      qc.setQueryData<Partial<Settings>>(qk.settings(), prev);
      qc.invalidateQueries({ queryKey: qk.settings() });
      toast(t("settings.notSavedToast"), "error");
    }
  }

  const setPersist = (v: boolean) => {
    setPersistQueryCache(v);
    // Turning persistence off drops the persisted copy so the next launch
    // starts clean instead of reviving stale data when it's re-enabled.
    if (!v) {
      try {
        localStorage.removeItem(QUERY_CACHE_KEY);
      } catch {
        // Storage unavailable — nothing to clear.
      }
    }
    toast(v ? t("settings.keepDataOn") : t("settings.keepDataOff"), "success");
  };

  const clearCache = async () => {
    const ok = await confirmDialog({
      title: t("settings.clearConfirmTitle"),
      body: t("settings.clearConfirmBody"),
      confirmLabel: t("settings.clearConfirmLabel"),
      tone: "danger",
    });
    if (!ok) return;
    try {
      // The only step that can FAIL goes first — aborting leaves everything
      // else untouched instead of a half-cleaned state.
      // Backend models.dev catalog cache (setting_kv entry).
      await settingDelete("models_dev_cache");
      // In-memory React Query cache + persisted copy.
      qc.clear();
      try {
        localStorage.removeItem(QUERY_CACHE_KEY);
      } catch {
        // Storage unavailable — ignore.
      }
      // In-memory quota snapshots (zustand, not persisted).
      useUI.setState({ quotaCache: {} });
      toast(t("settings.clearedToast"), "success");
    } catch {
      toast(t("settings.clearFailedToast"), "error");
    }
  };

  // First load (no cache yet): show skeletons so switches don't briefly render
  // the DEFAULTS state before the real values arrive.
  const loading = q.isLoading;

  return (
    <Page>
      <PageHeader
        title={t("settings.title")}
        info={t("settings.help")}
      />
      {q.isError && (
        <ErrorBanner onRetry={() => q.refetch()}>{t("settings.loadFailed")}</ErrorBanner>
      )}
      <Card title={t("settings.general")} description={t("settings.generalDesc")}>
        <div className="flex flex-wrap items-center justify-between gap-4">
          <div className="min-w-0">
            <div className="text-sm text-fg">{t("settings.autostart")}</div>
            <div className="prose text-xs text-muted mt-0.5">
              {t("settings.autostartDesc")}
            </div>
          </div>
          {autostartQ.isLoading ? (
            <Skeleton className="h-7 w-24" />
          ) : (
            <Switch
              className="shrink-0 ml-auto"
              checked={autostartQ.data ?? false}
              onCheckedChange={toggleAutostart}
              title={t("settings.autostart")}
            />
          )}
        </div>
      </Card>

      <Card title={t("settings.appearance")} description={t("settings.appearanceDesc")}>
        <div className="flex flex-wrap items-center justify-between gap-4">
          <div className="min-w-0">
            <div className="text-sm text-fg">{t("settings.theme")}</div>
            <div className="prose text-xs text-muted mt-0.5">
              {t("settings.themeDesc")}
            </div>
          </div>
          <SegmentedControl<ThemePref>
            className="shrink-0 ml-auto"
            value={theme}
            onChange={setTheme}
            items={[
              { value: "system", label: t("settings.themeSystem"), tooltip: t("settings.themeSystemTip") },
              { value: "light", label: t("settings.themeLight"), tooltip: t("settings.themeLightTip") },
              { value: "dark", label: t("settings.themeDark"), tooltip: t("settings.themeDarkTip") },
            ]}
          />
        </div>
        <div className="flex flex-wrap items-center justify-between gap-4 border-t border-border pt-3">
          <div className="min-w-0">
            <div className="text-sm text-fg">{t("settings.language")}</div>
            <div className="prose text-xs text-muted mt-0.5">
              {t("settings.languageDesc")}
            </div>
          </div>
          <SegmentedControl<string>
            className="shrink-0 ml-auto"
            value={language}
            onChange={setLanguage}
            items={[
              { value: "en", label: "EN" },
              { value: "zh", label: "中文" },
            ]}
          />
        </div>
      </Card>

      <Card title={t("settings.detection")} description={t("settings.detectionDesc")}>
        {loading ? (
          <div className="flex items-center justify-between gap-4">
            <Skeleton className="h-4 w-40" />
            <Skeleton className="h-7 w-56" />
          </div>
        ) : (
          <div className="flex flex-wrap items-center justify-between gap-4">
            <div className="min-w-0">
              <div className="text-sm text-fg">{t("settings.cadence")}</div>
              <div className="prose text-xs text-muted mt-0.5">
                {t("settings.cadenceDesc")}
              </div>
            </div>
            <SegmentedControl<Cadence>
              className="shrink-0 ml-auto"
              value={settings.detection_cadence}
              onChange={(v) => update("detection_cadence", v)}
              items={[
                { value: "on-launch", label: t("settings.cadenceLaunch"), tooltip: t("settings.cadenceLaunchTip") },
                { value: "manual", label: t("settings.cadenceManual"), tooltip: t("settings.cadenceManualTip") },
              ]}
            />
          </div>
        )}
      </Card>

      <Card title={t("settings.logs")} description={t("settings.logsDesc")}>
        {loading ? (
          <div className="flex items-center justify-between gap-4">
            <Skeleton className="h-4 w-32" />
            <Skeleton className="h-7 w-72" />
          </div>
        ) : (
          <div className="flex flex-wrap items-center justify-between gap-4">
            <div className="min-w-0">
              <div className="text-sm text-fg">{t("settings.retention")}</div>
              <div className="prose text-xs text-muted mt-0.5">
                {t("settings.retentionDesc")}
              </div>
            </div>
            <SegmentedControl<"7" | "30" | "90">
              className="shrink-0 ml-auto"
              value={String(settings.log_retention_days) as "7" | "30" | "90"}
              onChange={(v) => update("log_retention_days", Number(v) as 7 | 30 | 90)}
              items={[
                { value: "7", label: t("settings.retention7d"), tooltip: t("settings.retention7dTip") },
                { value: "30", label: t("settings.retention30d"), tooltip: t("settings.retention30dTip") },
                { value: "90", label: t("settings.retention90d"), tooltip: t("settings.retention90dTip") },
              ]}
            />
          </div>
        )}
      </Card>

      <Card title={t("settings.data")} description={t("settings.dataDesc")}>
        <div className="flex flex-wrap items-center justify-between gap-4">
          <div className="min-w-0">
            <div className="text-sm text-fg">{t("settings.keepData")}</div>
            <div className="prose text-xs text-muted mt-0.5">
              {t("settings.keepDataDesc")}
            </div>
          </div>
          <SegmentedControl<"on" | "off">
            className="shrink-0 ml-auto"
            value={persistQueryCache ? "on" : "off"}
            onChange={(v) => setPersist(v === "on")}
            items={[
              { value: "off", label: t("settings.off"), tooltip: t("settings.offTip") },
              { value: "on", label: t("settings.on"), tooltip: t("settings.onTip") },
            ]}
          />
        </div>
      </Card>

      <Card
        title={t("settings.cache")}
        description={t("settings.cacheDesc")}
        tone="danger"
      >
        <div className="flex flex-wrap items-center justify-between gap-4">
          <div className="min-w-0">
            <div className="text-sm text-fg">{t("settings.clearCaches")}</div>
            <div className="prose text-xs text-muted mt-0.5">
              {t("settings.clearCachesDesc")}
            </div>
          </div>
          <Button className="shrink-0 ml-auto" variant="danger" size="sm" onClick={clearCache}>
            {t("settings.clearCachesBtn")}
          </Button>
        </div>
      </Card>

      <GatewayTuningSection />

      <DiagnosticsSection />

      <Card
        title={t("settings.licensesTitle")}
        description={t("settings.licensesDesc")}
      >
        <div className="space-y-3 text-xs text-subtle">
          <p>{t("settings.licensesApp")}</p>
          <div>
            <div className="text-fg mb-1">{t("settings.licensesFonts")}</div>
<ul className="space-y-1">
              <li>{t("settings.licensesFontGeist")}</li>
              <li>{t("settings.licensesFontJetBrainsMono")}</li>
              <li>{t("settings.licensesFontSarasa")}</li>
            </ul>
          </div>
          <p>{t("settings.licensesModelsDev")}</p>
          <p>{t("settings.licensesDeps")}</p>
        </div>
      </Card>
    </Page>
  );
}
