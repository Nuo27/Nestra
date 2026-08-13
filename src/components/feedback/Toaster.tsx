import { useTranslation } from "react-i18next"
import { X } from "lucide-react"
import { useUI, type ToastTone } from "../../stores/ui"
import { ConfirmHost } from "../controls/ConfirmDialog"
import { Button } from "../controls/Button"

const TONE: Record<ToastTone, { glyph: string; accent: string }> = {
  success: { glyph: "✓", accent: "text-success" },
  error: { glyph: "!", accent: "text-danger" },
  default: { glyph: "i", accent: "text-muted" },
}

/// Renders the toast stack (bottom-right) and the singleton confirm host.
/// Mounted once at the app root. Terminal style: flat, single-glyph prefix.
export function Toaster() {
  const { t } = useTranslation()
  const toasts = useUI((s) => s.toasts)
  const dismiss = useUI((s) => s.dismissToast)

  return (
    <>
      <ConfirmHost />
      {toasts.length > 0 && (
        <div className="pointer-events-none fixed bottom-4 right-4 z-[60] flex flex-col gap-2">
          {toasts.map((toast) => {
            const { glyph, accent } = TONE[toast.tone]
            return (
              <div
                key={toast.id}
                role={toast.tone === "error" ? "alert" : "status"}
                className="pointer-events-auto flex items-start gap-2 border border-border bg-overlay px-3 py-2 text-sm text-fg animate-in slide-in-from-right-4 fade-in-0 duration-fast"
              >
                <span className={`mt-0.5 shrink-0 text-xs leading-relaxed ${accent}`}>
                  {glyph}
                </span>
                <div className="min-w-0 max-w-[20rem] break-words leading-relaxed">
                  {toast.message}
                </div>
                <Button
                  onClick={() => dismiss(toast.id)}
                  variant="ghost"
                  size="xs"
                  className="-mr-1 -mt-1 shrink-0"
                  aria-label={t("common.dismiss")}
                >
                  <X data-icon size={14} />
                </Button>
              </div>
            )
          })}
        </div>
      )}
    </>
  )
}
