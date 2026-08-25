import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Cable, AlertTriangle } from "lucide-react";
import { mcpImportAll, mcpImportScan, type ImportCandidate } from "../../ipc";
import { extractError } from "../../ipc/errors";
import { qk } from "../../lib/queries";
import { useUI } from "../../stores/ui";
import { Card } from "../controls/Card";
import { Button } from "../controls/Button";
import { ErrorBanner } from "../feedback/ErrorBanner";
import { EmptyState } from "../feedback/EmptyState";
import { ListSkeletonCard } from "../feedback/ListSkeletonCard";
import { Badge } from "../ui/badge";

export function McpImportSection({
  labelForAgent,
  onImport,
}: {
  labelForAgent: (id: string) => string;
  onImport: (agents: string[], name: string) => void;
}) {
  const { t } = useTranslation();
  const toast = useUI((s) => s.pushToast);
  const q = useQuery({ queryKey: qk.mcpImport(), queryFn: mcpImportScan, staleTime: 0 });
  const qc = useQueryClient();
  const items: ImportCandidate[] = q.data ?? [];

  const importAllMut = useMutation({
    mutationFn: () => mcpImportAll(),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.mcpImport() });
      qc.invalidateQueries({ queryKey: qk.mcp() });
    },
    // A failed batch import must not vanish silently.
    onError: (e) => toast(t("mcp.importFailed", { err: extractError(e) }), "error"),
  });

  if (q.isLoading) {
    return <ListSkeletonCard />;
  }

  if (q.isError) {
    // A failed scan must not masquerade as "nothing to import".
    return (
      <ErrorBanner onRetry={() => q.refetch()}>
        {t("mcp.importScanFailed")}
      </ErrorBanner>
    );
  }

  if (items.length === 0) {
    return (
      <EmptyState
        title={t("mcp.nothingToImport")}
        hint={t("mcp.importScanHint")}
        icon={<Cable data-icon size={20} />}
      />
    );
  }

  const invalidate = () => qc.invalidateQueries({ queryKey: qk.mcpImport() });

  return (
    <Card
      title={t("mcp.importableTitle", { count: items.length })}
      description={t("mcp.importableDesc")}
      action={
        <Button
          variant="primary"
          size="sm"
          disabled={importAllMut.isPending}
          onClick={() => importAllMut.mutate()}
        >
          {importAllMut.isPending ? t("mcp.importing") : t("mcp.importAll")}
        </Button>
      }
    >
      <ul className="divide-y divide-border">
        {items.map((it) => (
          <li
            key={it.id}
            className="flex items-center gap-3 py-2"
            title={it.native_paths.map(([, p]) => p).join(" · ")}
          >
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-2 truncate text-sm">
                <span className="truncate">{it.name}</span>
                <span className="text-xs text-subtle">
                  · {it.agent_ids.map(labelForAgent).join(", ")}
                </span>
                {it.disabled_in.length > 0 && (
                  <Badge tone="warning" variant="soft">
                    {t("mcp.disabledIn", { agents: it.disabled_in.map(labelForAgent).join(", ") })}
                  </Badge>
                )}
                {it.transports_conflict && (
                  <Badge tone="warning" variant="soft">
                    <AlertTriangle data-icon size={11} />
                    {t("mcp.mixedTransport")}
                  </Badge>
                )}
              </div>
            </div>
            <Button
              variant="primary"
              size="sm"
              onClick={() => {
                onImport(it.agent_ids, it.name);
                invalidate();
              }}
            >
              {t("mcp.importBtn")}
            </Button>
          </li>
        ))}
      </ul>
    </Card>
  );
}
