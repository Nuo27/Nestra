import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import {
  agentReadConfig,
  agentRemoveDetected,
  type AgentConfigContent,
  type AgentInfo,
  type DetectedProvider,
} from "../../ipc";
import { extractError } from "../../ipc/errors";
import { qk } from "../../lib/queries";
import { useUI } from "../../stores/ui";
import { Button } from "../controls/Button";
import { Note } from "../feedback/Note";
import { CodeBlock } from "../display/CodeBlock";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "../ui/dialog";

// ============ AgentConfigDialog ============
/// Preview-only dialog: shows the live config file content + any detected
/// (non-Nestra-managed) provider entries. Opened from the card's
/// "Config preview" button. Provider selection happens inline on the card.
export function AgentConfigDialog({ agent, onClose }: { agent: AgentInfo; onClose: () => void }) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useUI((s) => s.pushToast);
  const q = useQuery({
    queryKey: qk.agentConfig(agent.id),
    queryFn: () => agentReadConfig(agent.id),
  });
  const readConfig = q.data;

  const removeDetectedMut = useMutation({
    mutationFn: (key: string) => agentRemoveDetected(agent.id, key),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.agentConfig(agent.id) });
      toast(t("agents.removedFromConfig", { name: agent.display_name }), "success");
    },
    onError: (e: unknown) => toast(t("agents.removeFailed", { err: extractError(e) ?? String(e) }), "error"),
  });

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent size="lg">
        <DialogHeader>
          <DialogTitle>{t("agents.configDialogTitle", { name: agent.display_name })}</DialogTitle>
          <DialogDescription>{t("agents.previewDialogDesc")}</DialogDescription>
        </DialogHeader>
        <DialogBody className="space-y-2">
          <ConfigPreviewSection agent={agent} readConfig={readConfig} />
          <DetectedProviders
            agent={agent}
            detected={readConfig?.detected}
            onRemove={(key) => removeDetectedMut.mutate(key)}
            removingKey={removeDetectedMut.isPending ? (removeDetectedMut.variables ?? null) : null}
          />
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>{t("common.done")}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

export function ConfigFilePreview({
  path,
  content,
}: {
  path: string | null;
  content: string | null;
}) {
  const { t } = useTranslation();
  return (
    <div className="mt-2">
      {path && <Note>{t("agents.configFile", { path })}</Note>}
      {content ? (
        <CodeBlock maxH="max-h-64" tone="subtle" className="mt-1 p-2.5">
          {content}
        </CodeBlock>
      ) : (
        <div className="prose mt-1 border border-dashed border-border p-2.5 text-xs italic text-subtle leading-relaxed">
          {t("agents.noConfigYet")}
        </div>
      )}
    </div>
  );
}

export function ConfigPreviewSection({
  agent,
  readConfig,
}: {
  agent: AgentInfo;
  readConfig?: AgentConfigContent | undefined;
}) {
  const { t } = useTranslation();
  return (
    <div className="pt-2">
      <Note>{t("agents.configPreviewTitle", { name: agent.display_name })}</Note>
      <ConfigFilePreview path={readConfig?.path ?? null} content={readConfig?.content ?? null} />
    </div>
  );
}

/// Providers that already live in the agent's config file but aren't managed by
/// Nestra (user-configured directly in the agent). Shown with a per-row delete.
/// Managed `nestra-*` entries are filtered out — those belong to the binding
/// list above.
export function DetectedProviders({
  agent,
  detected,
  onRemove,
  removingKey,
}: {
  agent: AgentInfo;
  detected?: DetectedProvider[];
  onRemove: (key: string) => void;
  removingKey: string | null;
}) {
  const { t } = useTranslation();
  const foreign = (detected ?? []).filter((d) => !d.managed);
  if (foreign.length === 0) return null;
  return (
    <div className="pt-2">
      <Note>
        {t("agents.detectedNote", { name: agent.display_name })}
      </Note>
      <div className="mt-1 space-y-1">
        {foreign.map((d) => (
          <div
            key={d.key}
            className="flex items-center gap-2 border border-border px-2.5 py-1.5 text-sm"
          >
            <span className="flex-1 truncate font-mono text-xs">{d.display_name}</span>
            <Button
              size="sm"
              variant="ghost"
              onClick={() => onRemove(d.key)}
              disabled={removingKey === d.key}
              loading={removingKey === d.key}
              title={t("agents.removeFromConfigTitle", { name: d.display_name, agent: agent.display_name })}
            >{t("common.remove")}</Button>
          </div>
        ))}
      </div>
    </div>
  );
}
