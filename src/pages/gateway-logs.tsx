import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { ScrollText } from "lucide-react";
import {
  diagLogFiles,
  diagLogFullBodiesGet,
  diagLogFullBodiesSet,
  diagLogLevelGet,
  diagLogLevelSet,
  diagReadLogs,
  type LogEntry,
  type LogFilterLevel,
  type LogLevelPreset,
} from "../ipc";
import { extractError } from "../ipc/errors";
import { qk } from "../lib/queries";
import { useUI } from "../stores/ui";
import { Page } from "../components/layout/Page";
import { PageHeader, BackLink } from "../components/layout/PageHeader";
import { Card } from "../components/controls/Card";
import { Button } from "../components/controls/Button";
import { SegmentedControl } from "../components/controls/SegmentedControl";
import { Badge } from "../components/ui/badge";
import { Input } from "../components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../components/ui/select";
import { Switch } from "../components/ui/switch";
import { EmptyState } from "../components/feedback/EmptyState";
import { ErrorBanner } from "../components/feedback/ErrorBanner";
import { Skeleton } from "../components/ui/skeleton";
import { RefreshCw } from "lucide-react";

/// Gateway log viewer (`/gateway/logs`, entered from the Activity card on
/// the Gateway page). Reads the JSON twin layer via `diag_read_logs`, with
/// the task/request correlation ids the backend lifts out of the span
/// chain — paste a task id into the search to see one request's whole
/// lifecycle. The capture level hot-switches the live filter; Debug
/// records wire evidence (truncated by default, complete with the
/// full-bodies toggle — never headers, never credentials).
export function GatewayLogsPage() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useUI((s) => s.pushToast);

  const [level, setLevel] = useState<LogFilterLevel>("all");
  const [searchInput, setSearchInput] = useState("");
  const [search, setSearch] = useState("");
  const [file, setFile] = useState<string | undefined>(undefined);
  const [limit, setLimit] = useState(500);
  // Persisted in the zustand blob — auto-refresh survives page switches and
  // relaunches, matching the Quota page's `quotaAuto` behavior.
  const auto = useUI((s) => s.logAuto);
  const setAuto = useUI((s) => s.setLogAuto);

  // Debounce the search box — the backend re-reads the file per query.
  useEffect(() => {
    const id = setTimeout(() => setSearch(searchInput.trim()), 300);
    return () => clearTimeout(id);
  }, [searchInput]);

  const filesQ = useQuery({ queryKey: qk.logFiles(), queryFn: diagLogFiles });
  const levelQ = useQuery({ queryKey: qk.logLevel(), queryFn: diagLogLevelGet });
  const fullBodiesQ = useQuery({
    queryKey: qk.logFullBodies(),
    queryFn: diagLogFullBodiesGet,
  });

  const activeFile = file && filesQ.data?.includes(file) ? file : filesQ.data?.[0];
  const logsQ = useQuery({
    queryKey: qk.logs(activeFile, level, search, limit),
    queryFn: () =>
      diagReadLogs({ file: activeFile, level, search: search || undefined, limit }),
    refetchInterval: auto ? 5000 : 0,
  });

  const setLevelPresetMut = useMutation({
    mutationFn: (preset: LogLevelPreset) => diagLogLevelSet(preset),
    onSuccess: (preset) => {
      void qc.invalidateQueries({ queryKey: qk.logLevel() });
      toast(t("gatewayLogs.levelApplied", { level: preset }), "success");
    },
    onError: (e) =>
      toast(t("gatewayLogs.levelFailed", { err: extractError(e) ?? String(e) }), "error"),
  });

  const setFullBodiesMut = useMutation({
    mutationFn: (enabled: boolean) => diagLogFullBodiesSet(enabled),
    onSuccess: () => void qc.invalidateQueries({ queryKey: qk.logFullBodies() }),
    onError: (e) =>
      toast(t("gatewayLogs.levelFailed", { err: extractError(e) ?? String(e) }), "error"),
  });

  const entries = logsQ.data ?? [];

  return (
    <Page width="wide">
      <PageHeader
        title={t("gatewayLogs.title")}
        info={t("gatewayLogs.help")}
        back={
          <BackLink to="/gateway">{t("gatewayLogs.back")}</BackLink>
        }
        action={
          <div className="flex items-center gap-3">
            <span className="text-2xs text-subtle">{t("gatewayLogs.auto")}</span>
            <Switch checked={auto} onCheckedChange={setAuto} aria-label={t("gatewayLogs.auto")} />
            <Button
              size="sm"
              variant="secondary"
              loading={logsQ.isFetching}
              onClick={() => void logsQ.refetch()}
            >
              {!logsQ.isFetching && <RefreshCw data-icon size={14} />}
              {t("common.refresh")}
            </Button>
          </div>
        }
      />

      <Card>
        <div className="flex flex-wrap items-center gap-2">
          <SegmentedControl<LogFilterLevel>
            ariaLabel={t("gatewayLogs.levelFilter")}
            size="sm"
            value={level}
            onChange={(v) => setLevel(v)}
            items={[
              { value: "all", label: t("gatewayLogs.levelAll") },
              { value: "error", label: t("gatewayLogs.levelError") },
              { value: "warn", label: t("gatewayLogs.levelWarn") },
              { value: "info", label: t("gatewayLogs.levelInfo") },
            ]}
          />
          <Input
            className="w-64"
            placeholder={t("gatewayLogs.searchHint")}
            value={searchInput}
            onChange={(e) => setSearchInput(e.target.value)}
          />
          <Select value={activeFile} onValueChange={setFile}>
            <SelectTrigger size="sm" className="w-56" aria-label={t("gatewayLogs.file")}>
              <SelectValue placeholder={t("gatewayLogs.file")} />
            </SelectTrigger>
            <SelectContent>
              {(filesQ.data ?? []).map((f) => (
                <SelectItem key={f} value={f}>
                  {f}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <div className="ms-auto flex flex-wrap items-center gap-2">
            <span
              className="text-2xs text-subtle"
              title={t("gatewayLogs.fullBodiesHint")}
            >
              {t("gatewayLogs.fullBodies")}
            </span>
            <Switch
              checked={fullBodiesQ.data ?? false}
              disabled={fullBodiesQ.isLoading || setFullBodiesMut.isPending}
              onCheckedChange={(v) => setFullBodiesMut.mutate(v)}
              aria-label={t("gatewayLogs.fullBodies")}
            />
            <span className="text-2xs text-subtle">{t("gatewayLogs.capture")}</span>
            <SegmentedControl<LogLevelPreset>
              ariaLabel={t("gatewayLogs.capture")}
              size="sm"
              value={levelQ.data ?? "info"}
              onChange={(v) => setLevelPresetMut.mutate(v)}
              items={[
                { value: "info", label: "Info", tooltip: t("gatewayLogs.presetInfo") },
                {
                  value: "debug",
                  label: "Debug",
                  tooltip: t("gatewayLogs.presetDebug"),
                },
                {
                  value: "trace",
                  label: "Trace",
                  tooltip: t("gatewayLogs.presetTrace"),
                },
              ]}
            />
          </div>
        </div>
      </Card>

      {logsQ.isError ? (
        <ErrorBanner variant="box">
          <strong>{t("gatewayLogs.errorTitle")}</strong>
          {" · "}
          {extractError(logsQ.error) ?? String(logsQ.error)}
        </ErrorBanner>
      ) : logsQ.isLoading ? (
        <div className="space-y-1.5">
          <Skeleton className="h-5 w-full" />
          <Skeleton className="h-5 w-11/12" />
          <Skeleton className="h-5 w-4/5" />
        </div>
      ) : entries.length === 0 ? (
        <EmptyState
          icon={<ScrollText />}
          title={t("gatewayLogs.empty")}
          hint={t("gatewayLogs.emptyHint")}
        />
      ) : (
        <Card>
          <div className="space-y-1 font-mono text-xs leading-relaxed">
            {entries.map((e, i) => (
              <LogRow key={`${e.timestamp}-${i}`} entry={e} />
            ))}
          </div>
          {entries.length >= limit && (
            <Button
              variant="ghost"
              size="sm"
              className="mt-3 w-full"
              onClick={() => setLimit((n) => n + 500)}
            >
              {t("gatewayLogs.loadMore")}
            </Button>
          )}
        </Card>
      )}
    </Page>
  );
}

/// One log line: time · level badge · correlation chip · message (with the
/// structured fields already rendered `k=v` by the backend).
function LogRow({ entry }: { entry: LogEntry }) {
  const time = entry.timestamp.length >= 12 ? entry.timestamp.slice(11, 23) : entry.timestamp;
  const tone =
    entry.level === "ERROR" ? "danger" : entry.level === "WARN" ? "warning" : "neutral";
  return (
    <div className="flex items-baseline gap-2">
      <span className="shrink-0 text-subtle">{time}</span>
      <Badge tone={tone} className="shrink-0 uppercase">
        {entry.level}
      </Badge>
      {(entry.task || entry.request) && (
        <span
          className="shrink-0 truncate text-accent"
          title={`${entry.task ?? ""} ${entry.request ?? ""}`}
        >
          {entry.request ?? entry.task}
        </span>
      )}
      <span className="min-w-0 break-all text-fg">{entry.message}</span>
    </div>
  );
}
