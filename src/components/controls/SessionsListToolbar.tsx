import { useTranslation } from "react-i18next";
import { RotateCw, FolderOpen, Trash2, Search, X } from "lucide-react";
import type { UseQueryResult } from "@tanstack/react-query";
import type { Session } from "../../ipc";
import type { SessionSelection } from "../../lib/sessionSelection";
import { SyncIndicator } from "../feedback/SyncIndicator";
import { Button } from "./Button";
import { ButtonGroup } from "./ButtonGroup";
import { Input } from "../ui/input";
import { Tip } from "../ui/tooltip";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../ui/select";

/**
 * The sessions-list toolbar: title + rescan, search, and the always-visible
 * row with the agents filter on the left and the selection action cluster on
 * the right (only while rows are selected). The cluster is shrink-0 so the
 * icons never overflow; on narrow panels the dropdown gives way (its label
 * truncates).
 */
export function SessionsListToolbar({
  listQuery,
  refreshMut,
  query,
  onQueryChange,
  provider,
  onProviderChange,
  connectedProviders,
  selection,
  total,
}: {
  listQuery: UseQueryResult<Session[]>;
  refreshMut: { isPending: boolean; mutate: () => void };
  query: string;
  onQueryChange: (v: string) => void;
  provider: string;
  onProviderChange: (v: string) => void;
  connectedProviders: Map<string, string>;
  selection: SessionSelection;
  total: number;
}) {
  const { t } = useTranslation();
  const { liveSelected, toggleAll, clearSelection, bulkReveal, bulkDelete } = selection;

  return (
    <div className="border-b border-border px-3 py-2">
      <div className="flex items-center justify-between">
        <div className="flex items-baseline gap-2">
          <div className="text-md font-medium tracking-[-0.01em]">{t("sessions.title")}</div>
          <SyncIndicator query={listQuery} className="hidden xl:inline-flex" />
        </div>
        <Button
          size="sm"
          variant="ghost"
          onClick={() => refreshMut.mutate()}
          disabled={refreshMut.isPending}
          loading={refreshMut.isPending}
          title={t("sessions.rescanTitle")}
          aria-label={t("sessions.rescanTitle")}
        >
          {!refreshMut.isPending && <RotateCw data-icon size={13} />}
        </Button>
      </div>
      <Input
        value={query}
        onChange={(e) => onQueryChange(e.target.value)}
        placeholder={t("sessions.searchPlaceholder")}
        size="sm"
        className="mt-2"
        prefix={<Search data-icon size={13} />}
      />
      {/* One always-visible toolbar row: the agents filter on the left,
          the selection action cluster on the right (only while rows are
          selected). The cluster is shrink-0 so the icons never overflow;
          on narrow panels the dropdown gives way (its label truncates).
          !w-28 overrides the trigger's base w-full so it stays compact. */}
      <div className="mt-2 flex h-7 items-center gap-2">
        <Select
          value={provider || "__all__"}
          onValueChange={(v) => onProviderChange(v === "__all__" ? "" : v)}
        >
          <SelectTrigger
            size="sm"
            className="h-7 min-w-0 !w-fit max-w-[10rem] border-0 bg-transparent px-1 text-xs text-muted shadow-none hover:border-0 [&_[data-icon]]:size-3"
          >
            <SelectValue placeholder={t("sessions.allAgents")} />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="__all__">{t("sessions.allAgents")}</SelectItem>
            {connectedProviders.size > 0 ? (
              [...connectedProviders].map(([providerId, label]) => (
                <SelectItem key={providerId} value={providerId}>
                  {label}
                </SelectItem>
              ))
            ) : (
              <SelectItem value="__none__" disabled>
                {t("sessions.noConnectedAgents")}
              </SelectItem>
            )}
          </SelectContent>
        </Select>
        {liveSelected.size > 0 && (
          <ButtonGroup
            className="ml-auto whitespace-nowrap text-xs"
            justify="start"
          >
            <span className="shrink-0 px-1 font-medium tabular text-muted">
              {t("sessions.selected", { n: liveSelected.size })}
            </span>
            {/* !px-0 trims the button's base px-1 — the bracket pseudo-
                elements already provide the hit-target width, and these
                are the tightest cluster in the toolbar. */}
            <Button
              size="xs"
              variant="ghost"
              onClick={toggleAll}
              className="!px-0"
            >
              {liveSelected.size === total ? t("sessions.none") : t("sessions.all")}
            </Button>
            <Tip content={t("sessions.revealSelected")}>
              <Button
                size="xs"
                variant="ghost"
                onClick={bulkReveal}
                aria-label={t("sessions.revealSelectedAria")}
                className="!px-0"
              >
                <FolderOpen data-icon size={12} />
              </Button>
            </Tip>
            <Tip content={t("sessions.deleteSelected")}>
              <Button
                size="xs"
                variant="danger"
                onClick={bulkDelete}
                aria-label={t("sessions.deleteSelectedAria")}
                className="!px-0"
              >
                <Trash2 data-icon size={12} />
              </Button>
            </Tip>
            <Button
              size="xs"
              variant="ghost"
              onClick={clearSelection}
              aria-label={t("sessions.clearSelection")}
              className="!px-0"
            >
              <X data-icon size={12} />
            </Button>
          </ButtonGroup>
        )}
      </div>
    </div>
  );
}
