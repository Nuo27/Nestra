import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import i18n from "../../i18n";
import { Check, Copy, HeartPulse } from "lucide-react";
import {
  quotaKeepalivePreview,
  quotaKeepaliveStatus,
  quotaPingNow,
  type PingPreview,
} from "../../ipc";
import { Button } from "./Button";
import { ButtonGroup } from "./ButtonGroup";
import { useCopy } from "../../lib/useCopy";
import { qk } from "../../lib/queries";
import { extractError } from "../../ipc/errors";
import { formatTime } from "../../lib/format";
import { keepaliveMeta } from "../../lib/keepalive";

/// Keep-alive indicator + status popover for the Quota card. The trigger
/// tracks runtime state (polled every 10s) with the shared `HeartPulse` +
/// phase label; the popover is **status-only** — last success, next fire, and
/// any error. The request preview + test ping + copy live in Quota settings
/// (`KeepAliveEditor`, exported below). Phase → label / colour / pulse
/// semantics come from `lib/keepalive` so this surface, the provider-card
/// chip, and the worker all agree.
export function KeepAlivePopover({ endpointId }: { endpointId: string }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);

  const status = useQuery({
    queryKey: qk.keepaliveStatus(endpointId),
    queryFn: () => quotaKeepaliveStatus(endpointId),
    refetchInterval: 10_000,
    refetchIntervalInBackground: false,
  });
  const phase = status.data?.phase ?? "disabled";
  const meta = keepaliveMeta(phase);
  const s = status.data;

  // Close on outside click or Escape. No portal — the panel is anchored to
  // the header cluster in normal document flow, so a local listener suffices.
  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <div className="relative" ref={wrapRef}>
      <Button
        variant="ghost"
        size="sm"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        aria-label={`${t("keepalive.title")} · ${t(meta.labelKey)}`}
        title={`${t("keepalive.title")} · ${t(meta.labelKey)}`}
      >
        <HeartPulse
          data-icon
          size={14}
          className={`${meta.color}${meta.pulse ? " animate-pulse" : ""}`}
        />
        <span className={meta.color}>{t(meta.labelKey)}</span>
      </Button>

      {open && (
        <div className="absolute right-0 top-full z-50 mt-2 w-[22rem] max-w-[calc(100vw-2rem)] rounded-md border border-border bg-overlay p-3 text-xs shadow-lg">
          <div className="mb-2 flex items-center justify-between gap-2">
            <span className={`inline-flex items-center gap-1.5 font-medium ${meta.color}`}>
              <HeartPulse data-icon size={12} className={meta.pulse ? "animate-pulse" : ""} />
              {t("keepalive.title")} · {t(meta.labelKey)}
            </span>
            {s && s.attempts > 0 && (
              <span className="text-subtle">{t("keepalive.attempt", { n: s.attempts })}</span>
            )}
          </div>

          <div className="mb-2 grid grid-cols-2 gap-x-4 gap-y-0.5">
            <span className="text-subtle">{t("keepalive.lastSuccess")}</span>
            <span className="text-right font-mono">
              {s?.last_success_at ? formatTime(s.last_success_at) : "—"}
            </span>
            <span className="text-subtle">{t("keepalive.nextFire")}</span>
            <span className="text-right font-mono">
              {s?.next_fire_at ? formatUntil(s.next_fire_at) : "—"}
            </span>
          </div>

          {s?.last_error && (
            <p className="mb-2 break-all whitespace-pre-wrap text-danger">{s.last_error}</p>
          )}

          {phase === "disabled" && (
            <p className="mb-2 leading-relaxed text-subtle">
              {t("keepalive.off")}
            </p>
          )}

          {/* Status-only surface: the request preview + test + copy live in
              Quota settings (KeepAliveEditor), not here. */}
          <ButtonGroup justify="end">
            <Button variant="ghost" size="sm" onClick={() => setOpen(false)}>
              {t("common.close")}
            </Button>
          </ButtonGroup>
        </div>
      )}
    </div>
  );
}

/// The keep-alive request editor, rendered inside Quota settings when
/// keep-alive is armed: the redacted curl the worker would send, a one-shot
/// test ping, and copy. Lives here (not in the popover) per the design: the
/// popover is status-only; editing belongs in the settings dialog.
export function KeepAliveEditor({ endpointId }: { endpointId: string }) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const preview = useQuery<PingPreview>({
    queryKey: qk.keepalivePreview(endpointId),
    queryFn: () => quotaKeepalivePreview(endpointId),
    staleTime: 5_000,
  });
  const curl = useMemo(() => (preview.data ? buildCurl(preview.data) : ""), [preview.data]);

  const [testing, setTesting] = useState(false);
  const [testError, setTestError] = useState<string | null>(null);
  const test = async () => {
    setTesting(true);
    setTestError(null);
    try {
      await quotaPingNow(endpointId);
      await qc.invalidateQueries({ queryKey: qk.keepaliveStatus(endpointId) });
      await qc.invalidateQueries({ queryKey: qk.quotaRefresh() });
    } catch (e) {
      // The old try/finally had no catch: a rejected ping was an unhandled
      // rejection with zero user feedback.
      setTestError(extractError(e) ?? String(e));
    } finally {
      setTesting(false);
    }
  };

  const [copied, copy] = useCopy();

  return (
    <div className="space-y-2">
      <pre className="max-h-40 overflow-auto whitespace-pre-wrap break-all rounded-md border border-border bg-inset p-2 font-mono">
        {preview.isLoading
          ? t("keepalive.loading")
          : preview.error
            ? `${t("common.error")}: ${String(preview.error)}`
            : curl}
      </pre>
        <ButtonGroup space="loose" justify="end">
          <Button variant="ghost" size="sm" disabled={testing} loading={testing} onClick={test}>
            {testing ? t("common.testing") : t("common.test")}
          </Button>
          {preview.data && (
            <Button variant="ghost" size="sm" onClick={() => copy(curl)}>
              {copied ? <Check data-icon size={13} /> : <Copy data-icon size={13} />}
              {copied ? t("common.copied") : t("common.copy")}
            </Button>
          )}
        </ButtonGroup>
        {testError && (
          <div className="text-xs text-danger">{testError}</div>
        )}
    </div>
  );
}

function buildCurl(p: PingPreview): string {
  // Headers are single-quoted (like the body): the old double-quote form
  // left `$`, backticks and backslashes live for the shell — a key/URL
  // containing them would expand variables or inject commands when the user
  // pastes the curl. `'` inside a single-quoted shell string is escaped as
  // `'\''`.
  const shq = (s: string) => `'${s.replace(/'/g, `'\\''`)}'`;
  const headerLines = p.headers
    .map(([k, v]) => `-H ${shq(`${k}: ${v}`)}`)
    .join(" \\\n  ");
  const body = shq(p.body);
  return `curl -X ${p.method} \\\n  ${headerLines} \\\n  -d ${body} \\\n  ${shq(p.url)}`;
}

/// Relative countdown for a future fire time (formatTime would render a
/// future date as "-Nd ago"). Falls back to clock time beyond ~24h.
function formatUntil(ts: number): string {
  const delta = ts - Date.now();
  const t = i18n.t;
  if (delta <= 0) return t("keepalive.due");
  if (delta < 60_000) return t("keepalive.inSec", { n: Math.ceil(delta / 1000) });
  if (delta < 3_600_000) return t("keepalive.inMin", { n: Math.ceil(delta / 60_000) });
  if (delta < 86_400_000) return t("keepalive.inHour", { n: Math.ceil(delta / 3_600_000) });
  return formatTime(ts);
}