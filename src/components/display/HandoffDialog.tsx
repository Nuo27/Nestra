import { useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";

import { handoffPreview, handoffSave } from "../../ipc";
import { extractError } from "../../ipc/errors";
import { invalidate } from "../../lib/queries";
import { useUI } from "../../stores/ui";
import { Button } from "../controls/Button";
import { Textarea } from "../ui/textarea";
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

  // Latest-ref for `t`: keeping the translation function OUT of the effect
  // deps stops a language switch from re-running the preview fetch (which
  // would silently discard unsaved edits under a skeleton flash).
  const tRef = useRef(t);
  tRef.current = t;

  useEffect(() => {
    if (!open) return;
    setMarkdown(null);
    setErr(null);
    // A slow response for a previous (provider, session, open) tuple must
    // never clobber the current one — guard with a cancelled flag.
    let cancelled = false;
    handoffPreview(provider, sessionId)
      .then((p) => {
        if (!cancelled) setMarkdown(p.markdown);
      })
      .catch((e) => {
        if (!cancelled) {
          setErr(extractError(e) ?? tRef.current("sessions.handoffPreviewFailed"));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [open, provider, sessionId]);

  const save = async () => {
    if (markdown === null) return;
    // An empty (whitespace-only) artifact is a destructive overwrite of the
    // existing handoff — refuse it.
    if (markdown.trim().length === 0) return;
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
    <Dialog
      open={open}
      // Esc / overlay close stays available except mid-save: the write is in
      // flight and its toast/invalidate must land on a mounted dialog.
      onOpenChange={(o) => {
        if (!o && saving) return;
        onOpenChange(o);
      }}
    >
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
            <Textarea
              value={markdown}
              onChange={(e) => setMarkdown(e.target.value)}
              disabled={saving}
              spellCheck={false}
              size="sm"
              rows={16}
              className="min-h-[45vh] font-mono"
            />
          )}
        </DialogBody>
        <DialogFooter>
          <Button
            variant="ghost"
            size="sm"
            disabled={saving}
            onClick={() => onOpenChange(false)}
          >
            {t("common.cancel")}
          </Button>
          <Button
            variant="primary"
            size="sm"
            loading={saving}
            disabled={markdown === null || markdown.trim().length === 0}
            onClick={save}
          >
            {t("sessions.handoffSaveButton")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
