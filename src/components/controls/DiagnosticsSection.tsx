import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { open } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Download, FolderOpen, RefreshCw } from "lucide-react";
import {
  diagExportLogs,
  diagHealth,
  diagOpenDataDir,
  updatesCheck,
  type UpdateInfo,
} from "../../ipc";
import { qk } from "../../lib/queries";
import { Button } from "./Button";
import { ButtonGroup } from "./ButtonGroup";
import { Card } from "./Card";
import { SectionLabel } from "../layout/PageHeader";
import { FieldRow } from "./Field";
import { useUI } from "../../stores/ui";

/// Diagnostics block — rendered as the last section of the Settings page
/// (no longer a top-level route). System card (app / OS / data location with
/// open-in-explorer) + Updates card (GitHub Release check) + log export +
/// About. The former Health card was removed — its status dot was fed by a
/// hardcoded `ok: true` and isn't a real health check; `diag_health` still
/// backs the System card's rows.
export function DiagnosticsSection() {
  const { t, i18n } = useTranslation();
  const healthQ = useQuery({
    queryKey: qk.diagHealth(),
    queryFn: diagHealth,
    refetchOnMount: "always",
  });
  const toast = useUI((s) => s.pushToast);

  // Update check is on-demand (no background polling) to respect GitHub's
  // unauthenticated rate limit and the app's no-chatter stance.
  const [checking, setChecking] = useState(false);
  const [update, setUpdate] = useState<UpdateInfo | null>(null);

  async function exportLogs() {
    const picked = await open({
      title: t("settings.diagFolderTitle"),
      directory: true,
      multiple: false,
    });
    if (!picked || typeof picked !== "string") return;
    try {
      await diagExportLogs(picked);
      toast(t("settings.diagExported"), "success");
    } catch (e) {
      toast(t("settings.diagExportFailed", { err: (e as Error).message ?? e }), "error");
    }
  }

  async function openDataDir() {
    try {
      await diagOpenDataDir();
    } catch (e) {
      toast(t("settings.diagOpenFailed", { err: (e as Error).message ?? e }), "error");
    }
  }

  async function checkUpdates() {
    setChecking(true);
    try {
      const info = await updatesCheck();
      setUpdate(info);
      // Only toast on "up to date"; an available update or a missing release
      // is rendered inline on the card (no toast noise).
      if (info.found && !info.hasUpdate) {
        toast(t("settings.updatesUpToDate", { version: info.current }), "success");
      }
    } catch (e) {
      toast(t("settings.updatesCheckFailed", { err: (e as Error).message ?? e }), "error");
    } finally {
      setChecking(false);
    }
  }

  async function openReleasePage(url: string) {
    try {
      await openUrl(url);
    } catch (e) {
      toast(t("settings.updatesOpenFailed", { err: (e as Error).message ?? e }), "error");
    }
  }

  return (
    <div className="space-y-3">
      <SectionLabel className="pt-2 block">{t("settings.diagTitle")}</SectionLabel>

      <Card title={t("settings.diagSystem")} description={t("settings.diagSystemDesc")}>
        <div className="space-y-1">
          <KV k={t("settings.diagAppVersion")} v={healthQ.data?.version ?? "—"} />
          <KV
            k={t("settings.diagOs")}
            v={
              healthQ.data
                ? [healthQ.data.os, healthQ.data.arch].filter(Boolean).join(" · ") || "—"
                : "—"
            }
          />
          <FieldRow label={<span className="text-muted">{t("settings.diagDataDir")}</span>}>
            <span className="flex min-w-0 items-center gap-1.5">
              <span
                className="truncate font-mono text-sm text-fg"
                title={healthQ.data?.data_dir}
              >
                {healthQ.data?.data_dir ?? "—"}
              </span>
              {healthQ.data?.data_dir && (
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={openDataDir}
                  aria-label={t("settings.diagDataDirOpen")}
                  title={t("settings.diagDataDirOpen")}
                >
                  <FolderOpen data-icon size={14} />
                </Button>
              )}
            </span>
          </FieldRow>
        </div>
        <ButtonGroup className="mt-3" justify="end" space="loose">
          <Button size="sm" variant="ghost" onClick={exportLogs}>
            {t("common.exportLogs")}
          </Button>
        </ButtonGroup>
      </Card>

      <Card
        title={t("settings.updates")}
        description={t("settings.updatesDesc")}
        action={
          <Button size="sm" variant="secondary" loading={checking} onClick={checkUpdates}>
            {!checking && <RefreshCw data-icon size={14} />}
            {checking ? t("settings.updatesChecking") : t("settings.updatesCheckBtn")}
          </Button>
        }
      >
        <UpdateBody
          update={update}
          currentVersion={healthQ.data?.version ?? null}
          formatDate={(iso) => (iso ? new Date(iso).toLocaleDateString(i18n.language) : "")}
          t={t}
          onDownload={openReleasePage}
        />
      </Card>

      <Card title={t("settings.diagAbout")}>
        <div className="prose text-sm text-muted leading-relaxed">
          {t("settings.diagAboutBody")}
        </div>
      </Card>
    </div>
  );
}

/// Inline body of the Updates card. Renders a muted current-version line
/// until the first check, then the check result. An available version shows
/// the version, publish date, truncated notes, and a download action.
function UpdateBody({
  update,
  currentVersion,
  formatDate,
  t,
  onDownload,
}: {
  update: UpdateInfo | null;
  currentVersion: string | null;
  formatDate: (iso: string) => string;
  t: (key: string, opts?: Record<string, unknown>) => string;
  onDownload: (url: string) => void;
}) {
  if (!update) {
    return (
      <div className="text-sm text-muted">
        {currentVersion ? `${t("settings.diagAppVersion")}: ${currentVersion}` : null}
      </div>
    );
  }
  // No published release yet — GitHub 404, surfaced as `found = false`.
  if (!update.found) {
    return <div className="text-sm text-muted">{t("settings.updatesNoRelease")}</div>;
  }
  if (!update.hasUpdate) {
    return (
      <div className="text-sm text-muted">
        {t("settings.updatesUpToDate", { version: update.current })}
      </div>
    );
  }
  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
        <span className="text-sm font-medium text-fg">
          {t("settings.updatesAvailable", { version: update.latest })}
        </span>
        {update.publishedAt && (
          <span className="text-xs text-muted">
            {t("settings.updatesReleased", { date: formatDate(update.publishedAt) })}
          </span>
        )}
      </div>
      <p className="text-sm text-muted">{t("settings.updatesAvailableDesc")}</p>
      {update.notes && (
        <details className="group">
          <summary className="cursor-pointer text-xs text-muted underline-offset-2 hover:underline">
            {t("settings.updatesNotes")}
          </summary>
          <div className="mt-1 max-h-40 overflow-auto whitespace-pre-wrap rounded border border-border bg-surface px-3 py-2 text-xs text-muted">
            {update.notes}
          </div>
        </details>
      )}
      <ButtonGroup justify="end" space="loose">
        <Button size="sm" variant="primary" onClick={() => onDownload(update.releaseUrl)}>
          <Download data-icon size={14} />
          {t("settings.updatesGetBtn")}
        </Button>
      </ButtonGroup>
    </div>
  );
}

function KV({ k, v }: { k: string; v: string }) {
  return (
    <FieldRow
      label={<span className="text-muted">{k}</span>}
      align="baseline"
    >
      <span className="truncate font-mono text-sm text-fg">{v}</span>
    </FieldRow>
  );
}
