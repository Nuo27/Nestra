import { useEffect, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";

import { handoffPreview, handoffSave } from "../../ipc";
import { extractError } from "../../ipc/errors";
import { invalidate } from "../../lib/queries";
import { useUI } from "../../stores/ui";
import { Button } from "../controls/Button";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "../ui/dialog";
import { ErrorBanner } from "../feedback/ErrorBanner";
import { Skeleton } from "../ui/skeleton";

/**
 * The "Generate handoff" flow (Context Lifecycle R1): previews the structural
 * extraction as editable markdown, then commits the (possibly edited) text as
 * the artifact. The markdown is what the next session reads; the structured
 * sections are only Nestra's index.
 */
export function HandoffDialog({
  provider,
  sessionId,
  open,
  onOpenChange,
}: {
  provider: string;
  sessionId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useUI((s) => s.pushToast);
  const [markdown, setMarkdown] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (!open) return;
    setMarkdown(null);
    setErr(null);
    handoffPreview(provider, sessionId)
      .then((p) => setMarkdown(p.markdown))
      .catch((e) => setErr(extractError(e) ?? t("sessions.handoffPreviewFailed")));
  }, [open, provider, sessionId, t]);

  const save = async () => {
    if (markdown == null) return;
    setSaving(true);
    try {
      await handoffSave(provider, sessionId, markdown);
      toast(t("sessions.handoffSaved"), "success");
      invalidate(qc, "handoff");
      onOpenChange(false);
    } catch (e) {
      toast(extractError(e) ?? t("sessions.handoffSaveFailed"), "error");
    } finally {
      setSaving(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent size="lg">
        <DialogHeader>
          <DialogTitle>{t("sessions.handoffTitle")}</DialogTitle>
          <DialogDescription>{t("sessions.handoffDialogDesc")}</DialogDescription>
        </DialogHeader>
        <DialogBody>
          {err ? (
            <ErrorBanner onDismiss={() => setErr(null)}>{err}</ErrorBanner>
          ) : markdown == null ? (
            <div className="space-y-2">
              {Array.from({ length: 6 }).map((_, i) => (
                <Skeleton key={i} className="h-5 w-full" />
              ))}
            </div>
          ) : (
            <textarea
              value={markdown}
              onChange={(e) => setMarkdown(e.target.value)}
              spellCheck={false}
              className="min-h-[45vh] w-full resize-y rounded-sm border border-border bg-inset px-2 py-1 font-mono text-xs text-fg focus-visible:outline-none focus-visible:shadow-focus"
            />
          )}
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" size="sm" onClick={() => onOpenChange(false)}>
            {t("common.cancel")}
          </Button>
          <Button
            variant="primary"
            size="sm"
            loading={saving}
            disabled={markdown == null}
            onClick={save}
          >
            {t("sessions.handoffSaveButton")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
