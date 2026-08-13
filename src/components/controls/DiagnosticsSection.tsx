import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { open } from "@tauri-apps/plugin-dialog";
import { FolderOpen } from "lucide-react";
import { diagExportLogs, diagHealth, diagOpenDataDir } from "../../ipc";
import { qk } from "../../lib/queries";
import { Button } from "./Button";
import { ButtonGroup } from "./ButtonGroup";
import { Card } from "./Card";
import { SectionLabel } from "../layout/PageHeader";
import { FieldRow } from "./Field";
import { useUI } from "../../stores/ui";

/// Diagnostics block — rendered as the last section of the Settings page
/// (no longer a top-level route). System card (app / OS / data location with
/// open-in-explorer) + log export + About. The former Health card was
/// removed — its status dot was fed by a hardcoded `ok: true` and isn't a
/// real health check; `diag_health` still backs the System card's rows.
export function DiagnosticsSection() {
  const { t } = useTranslation();
  const healthQ = useQuery({
    queryKey: qk.diagHealth(),
    queryFn: diagHealth,
    refetchOnMount: "always",
  });
  const toast = useUI((s) => s.pushToast);

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

      <Card title={t("settings.diagAbout")}>
        <div className="prose text-sm text-muted leading-relaxed">
          {t("settings.diagAboutBody")}
        </div>
      </Card>
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
